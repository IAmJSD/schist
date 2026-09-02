//! Bulk actions on gallery photos: move them to a folder (sidecars in
//! tow — the versioning invariant is that edits live beside their
//! photo), pack them into a ZIP, revert them to their originals, and
//! run the batch dialog's recipe (turn, upscale, adjust) over them as
//! versioned edits or flat copies. Everything here runs on the
//! background executor with the tray narrating.

use super::library::backing_psd;
use super::*;
use std::path::Path;

impl Workspace {
    /// Move photos into a folder, each with its edit sidecar and
    /// versions, then rescan. The gallery's drag-to-folder.
    pub(super) fn move_photos_to(
        &mut self,
        paths: Vec<PathBuf>,
        dest: PathBuf,
        cx: &mut Context<Self>,
    ) {
        if paths.is_empty() {
            return;
        }
        self.status = format!(
            "Moving {} photos to {}\u{2026}",
            paths.len(),
            dest.display()
        )
        .into();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let mut moved = 0usize;
                    for path in &paths {
                        match move_photo(path, &dest) {
                            Ok(()) => moved += 1,
                            Err(err) => {
                                log::error!("move failed for {}: {err:#}", path.display())
                            }
                        }
                    }
                    (moved, paths.len(), dest)
                })
                .await;
            this.update(cx, |ws, cx| {
                let (moved, asked, dest) = result;
                ws.status = if moved == asked {
                    format!("Moved {moved} photos to {}", dest.display()).into()
                } else {
                    format!(
                        "Moved {moved} of {asked} photos to {} — the log has the rest",
                        dest.display()
                    )
                    .into()
                };
                // The moved paths are gone; so is their selection.
                ws.library.selected.clear();
                ws.library_rescan(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Pack photos into a ZIP wherever the save prompt says.
    pub(super) fn save_photos_zip(
        &mut self,
        paths: Vec<PathBuf>,
        suggested: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The content filter holds at the door too: with it on, flagged
        // photos never leave in an archive, whoever asked.
        let (paths, held) = self
            .library
            .zip_candidates(paths, self.view.gallery_hide_nsfw);
        if paths.is_empty() {
            if held > 0 {
                self.status = format!(
                    "Nothing to zip: the content filter keeps all {held} of those photos out"
                )
                .into();
                cx.notify();
            }
            return;
        }
        let dir = std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."));
        let rx = cx.prompt_for_new_path(&dir, Some(&suggested));
        let codecs = self.registry.shared_codecs();
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(out))) = rx.await else { return };
            let total = paths.len();
            // One photo at a time, rendered then written straight into
            // the archive: a camera roll's worth of PNGs would not fit
            // in memory at once, and rendering is slow enough that the
            // tray owes the count.
            let target = out.clone();
            let opened = cx
                .background_executor()
                .spawn(async move { ZipWriter::create(&target) })
                .await;
            let mut writer = match opened {
                Ok(writer) => writer,
                Err(err) => {
                    log::error!("zip failed: {err:#}");
                    this.update(cx, |ws, cx| {
                        ws.status = format!("ZIP failed: {err}").into();
                        cx.notify();
                    })
                    .ok();
                    return;
                }
            };
            let mut written = 0usize;
            for (done, path) in paths.into_iter().enumerate() {
                let job_codecs = codecs.clone();
                let job_path = path.clone();
                let (returned, result) = cx
                    .background_executor()
                    .spawn(async move {
                        let mut writer = writer;
                        let result = zip_entry(&job_codecs, &job_path)
                            .and_then(|(name, bytes)| writer.add(&name, &bytes));
                        (writer, result)
                    })
                    .await;
                writer = returned;
                match result {
                    Ok(()) => written += 1,
                    Err(err) => log::warn!("zip: skipping {}: {err:#}", path.display()),
                }
                let keep = this.update(cx, |ws, cx| {
                    ws.status = format!("Zipping {}/{total}\u{2026}", done + 1).into();
                    cx.notify();
                });
                if keep.is_err() {
                    return;
                }
            }
            let finished = cx
                .background_executor()
                .spawn(async move { writer.finish() })
                .await;
            this.update(cx, |ws, cx| {
                let held_note = match held {
                    0 => String::new(),
                    1 => " (1 left out by the content filter)".into(),
                    n => format!(" ({n} left out by the content filter)"),
                };
                ws.status = match finished {
                    Ok(()) if written == total => {
                        format!("Zipped {written} photos to {}{held_note}", out.display()).into()
                    }
                    Ok(()) => format!(
                        "Zipped {written} of {total} photos to {}{held_note} \u{2014} the log has the rest",
                        out.display()
                    )
                    .into(),
                    Err(err) => {
                        log::error!("zip failed: {err:#}");
                        format!("ZIP failed: {err}").into()
                    }
                };
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Open the Save Image As dialog for one photo. PNG by default (the
    /// one format every build exports), the photo's own pixel size
    /// read up front so the scale slider can speak in pixels.
    pub(super) fn open_save_image_as(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let codec = self
            .registry
            .codecs()
            .find(|c| c.can_export() && c.extensions().contains(&"png"))
            .or_else(|| self.registry.codecs().find(|c| c.can_export()))
            .map(|c| c.id());
        let Some(codec) = codec else {
            self.status = "No format here can save an image".into();
            cx.notify();
            return;
        };
        // The original's size stands in for the edit's: an edit keeps
        // its photo's canvas in all but the rarest cases, and reading
        // a header beats decoding a sidecar just to label a slider.
        let size = image::image_dimensions(&path).ok();
        self.open_modal(
            Modal::SaveImageAs {
                path,
                codec,
                options: schist_plugin_api::ExportOptions::default(),
                scale: 1.0,
                size,
            },
            cx,
        );
    }

    /// Save one photo — its edit when it has one — as a flat image in
    /// the chosen format, shrunk by `scale`.
    pub fn save_photo_as(
        &mut self,
        path: PathBuf,
        codec_id: &'static str,
        options: schist_plugin_api::ExportOptions,
        scale: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let codecs = self.registry.shared_codecs();
        let Some(codec) = codecs.iter().find(|c| c.id() == codec_id).cloned() else {
            return;
        };
        let ext = codec.extensions().first().copied().unwrap_or("png");
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "photo".into());
        let suggested = if scale < 0.999 {
            format!("{stem}@{}%.{ext}", (scale * 100.0).round() as u32)
        } else {
            format!("{stem}.{ext}")
        };
        let dir = path
            .parent()
            .map(|p| p.to_path_buf())
            .or_else(|| std::env::var("HOME").ok().map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("."));
        let rx = cx.prompt_for_new_path(&dir, Some(&suggested));
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(out))) = rx.await else { return };
            let target = out.clone();
            let result =
                cx.background_executor()
                    .spawn(async move {
                        render_photo_as(&codecs, &codec, &path, &options, scale, &target)
                    })
                    .await;
            this.update_in(cx, |ws, _window, cx| {
                ws.status = match result {
                    Ok((w, h)) => format!("Saved {} ({w} \u{d7} {h})", out.display()).into(),
                    Err(err) => {
                        log::error!("save image as failed: {err:#}");
                        format!("Save failed: {err}").into()
                    }
                };
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Open several photos as editor tabs, each backed by its sidecar
    /// like a double-click would, up to [`DROP_OPEN_CAP`].
    pub(super) fn open_photos_in_tabs(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        let total = paths.len();
        let opening = total.min(DROP_OPEN_CAP);
        for path in paths.into_iter().take(DROP_OPEN_CAP) {
            self.open_from_gallery(path, cx);
        }
        if total > opening {
            self.status = format!(
                "Opening the first {opening} of {total} photos — the gallery holds the rest"
            )
            .into();
            cx.notify();
        }
    }

    /// Ask for a folder, then move the photos there — the menu's route
    /// to what dragging onto a folder row does.
    pub(super) fn move_photos_prompt(
        &mut self,
        paths: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if paths.is_empty() {
            return;
        }
        let rx = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Move Here".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(mut dirs))) = rx.await else {
                return;
            };
            let Some(dest) = dirs.pop() else { return };
            this.update_in(cx, |ws, _window, cx| ws.move_photos_to(paths, dest, cx))
                .ok();
        })
        .detach();
    }

    /// Put photos back to their originals: each edit sidecar goes into
    /// `versions/` (nothing is lost — it is one more version) and the
    /// gallery shows the untouched photo again.
    pub(super) fn revert_photos(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        let mut reverted = 0usize;
        for path in &paths {
            let Some(psd) = backing_psd(path).filter(|p| p.exists()) else {
                continue;
            };
            keep_sidecar_version(&psd);
            match std::fs::remove_file(&psd) {
                Ok(()) => {
                    reverted += 1;
                    self.library.thumbs.remove(path);
                }
                Err(err) => log::error!("revert failed for {}: {err:#}", path.display()),
            }
        }
        self.status = match reverted {
            0 => "Nothing to revert: none of those photos has an edit".into(),
            1 => "Reverted 1 photo to its original — the edit is kept under versions/".into(),
            n => format!(
                "Reverted {n} photos to their originals — the edits are kept under versions/"
            )
            .into(),
        };
        self.library_rescan(cx);
        cx.notify();
    }

    /// Right-click ▸ Process…: the batch dialog over these photos.
    pub(super) fn open_batch_process(&mut self, photos: Vec<PathBuf>, cx: &mut Context<Self>) {
        if photos.is_empty() {
            return;
        }
        let codec = self
            .registry
            .codecs()
            .find(|c| c.can_export() && c.extensions().contains(&"jpg"))
            .or_else(|| {
                self.registry
                    .codecs()
                    .find(|c| c.can_export() && c.extensions().contains(&"png"))
            })
            .or_else(|| self.registry.codecs().find(|c| c.can_export()))
            .map(|c| c.id());
        let Some(codec) = codec else {
            self.status = "No format here can save an image".into();
            cx.notify();
            return;
        };
        self.open_modal(
            Modal::BatchProcess {
                photos,
                recipe: BatchRecipe::default(),
                target: BatchTarget::Edit,
                codec,
                options: schist_plugin_api::ExportOptions {
                    quality: 92,
                    ..Default::default()
                },
            },
            cx,
        );
    }

    /// The dialog's Run: settle where the results go, then start.
    #[allow(clippy::too_many_arguments)]
    pub fn run_batch(
        &mut self,
        photos: Vec<PathBuf>,
        recipe: BatchRecipe,
        target: BatchTarget,
        codec_id: &'static str,
        options: schist_plugin_api::ExportOptions,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if recipe.is_empty() {
            self.status = "Nothing to do: pick a turn, an upscale or an adjustment first".into();
            cx.notify();
            return;
        }
        let codecs = self.registry.shared_codecs();
        let flat = codecs.iter().find(|c| c.id() == codec_id).cloned();
        match target {
            BatchTarget::Edit => self.batch_photos(photos, recipe, BatchSink::Edit, cx),
            BatchTarget::Beside => {
                let Some(codec) = flat else { return };
                self.batch_photos(
                    photos,
                    recipe,
                    BatchSink::Copies {
                        dir: None,
                        codec,
                        options,
                    },
                    cx,
                )
            }
            BatchTarget::Folder => {
                let Some(codec) = flat else { return };
                let rx = cx.prompt_for_paths(gpui::PathPromptOptions {
                    files: false,
                    directories: true,
                    multiple: false,
                    prompt: Some("Save Copies Here".into()),
                });
                cx.spawn_in(window, async move |this, cx| {
                    let Ok(Ok(Some(mut dirs))) = rx.await else {
                        return;
                    };
                    let Some(dir) = dirs.pop() else { return };
                    this.update_in(cx, |ws, _window, cx| {
                        ws.batch_photos(
                            photos,
                            recipe,
                            BatchSink::Copies {
                                dir: Some(dir),
                                codec,
                                options,
                            },
                            cx,
                        )
                    })
                    .ok();
                })
                .detach();
            }
        }
    }

    /// Run the recipe over every photo, one at a time on the background
    /// executor — a neural upscale is seconds per megapixel, and a camera
    /// roll's worth of documents would not fit in memory together.
    fn batch_photos(
        &mut self,
        photos: Vec<PathBuf>,
        recipe: BatchRecipe,
        sink: BatchSink,
        cx: &mut Context<Self>,
    ) {
        if photos.is_empty() {
            return;
        }
        let total = photos.len();
        self.status = format!("Processing {total} photos\u{2026}").into();
        cx.notify();
        let codecs = self.registry.shared_codecs();
        let recipe = Arc::new(recipe);
        let sink = Arc::new(sink);
        cx.spawn(async move |this, cx| {
            let mut failed = 0usize;
            for (done, path) in photos.into_iter().enumerate() {
                let job_codecs = codecs.clone();
                let job_recipe = recipe.clone();
                let job_sink = sink.clone();
                let job_path = path.clone();
                let result =
                    cx.background_executor()
                        .spawn(async move {
                            process_photo(&job_codecs, &job_path, &job_recipe, &job_sink)
                        })
                        .await;
                let keep = this.update(cx, |ws, cx| {
                    match result {
                        Ok(out) => {
                            // The grid renders from the sidecar once it
                            // exists; the cached thumbnail is the old one.
                            ws.library.thumbs.remove(&path);
                            ws.status = format!(
                                "Processed {}/{total} \u{2014} {}",
                                done + 1,
                                out.display()
                            )
                            .into();
                        }
                        Err(err) => {
                            failed += 1;
                            log::error!("batch failed for {}: {err:#}", path.display());
                            ws.status = format!("Processing failed: {err}").into();
                        }
                    }
                    cx.notify();
                });
                if keep.is_err() {
                    return;
                }
            }
            this.update(cx, |ws, cx| {
                if failed == 0 {
                    ws.status = format!("Processed {total} photos").into();
                } else {
                    ws.status = format!(
                        "Processed {} of {total} photos \u{2014} the log has the rest",
                        total - failed
                    )
                    .into();
                }
                ws.library_rescan(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}

/// Where a batch run writes.
enum BatchSink {
    /// Each photo's sidecar, versioned.
    Edit,
    /// Flat copies: beside the originals when `dir` is None.
    Copies {
        dir: Option<PathBuf>,
        codec: Arc<dyn schist_plugin_api::CodecPlugin>,
        options: schist_plugin_api::ExportOptions,
    },
}

/// Copy a sidecar into its `versions/` directory, stamped, before it
/// is replaced — every save of an edit is a version, automatically.
pub(super) fn keep_sidecar_version(path: &Path) {
    let Some(dir) = path.parent() else { return };
    let _ = std::fs::create_dir_all(dir);
    if !path.exists() {
        return;
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "backing.psd".into());
    let versions = dir.join("versions");
    let _ = std::fs::create_dir_all(&versions);
    if let Err(err) = std::fs::copy(path, versions.join(format!("{stamp}-{name}"))) {
        log::warn!("could not keep a version of {}: {err}", path.display());
    }
}

/// A path that nothing occupies yet: `name.ext`, else `name-2.ext`,
/// `name-3.ext`… Batch output never overwrites a file it did not
/// just write.
fn unclaimed(dir: &Path, stem: &str, ext: &str) -> PathBuf {
    let first = dir.join(format!("{stem}.{ext}"));
    if !first.exists() {
        return first;
    }
    (2..)
        .map(|n| dir.join(format!("{stem}-{n}.{ext}")))
        .find(|p| !p.exists())
        .expect("the counter is unbounded")
}

/// Run the recipe over one photo and write the result where the sink
/// says. Blocking and, with an upscale in the recipe, heavy.
fn process_photo(
    codecs: &[Arc<dyn schist_plugin_api::CodecPlugin>],
    path: &Path,
    recipe: &BatchRecipe,
    sink: &BatchSink,
) -> anyhow::Result<PathBuf> {
    // The edit is the picture the gallery shows, so a recipe applies on
    // top of it; the original stands in when there is none.
    let sidecar = backing_psd(path).ok_or_else(|| anyhow::anyhow!("no file name"))?;
    let source = if sidecar.exists() {
        sidecar.clone()
    } else {
        path.to_path_buf()
    };
    let mut doc = super::decode_file(codecs, &source)?;
    for op in recipe.transforms() {
        super::image_ops::transform_document(&mut doc, op);
    }
    if let Some(id) = recipe.upscale {
        let (w, h) = (doc.width * 2, doc.height * 2);
        match schist_tools_transform::plan_neural(&doc, w, h, id) {
            schist_tools_transform::Plan::NoModel => {
                anyhow::bail!(
                    "the {} model would not load",
                    schist_tools_transform::Resample::Neural(id).display_name()
                )
            }
            schist_tools_transform::Plan::Classical => {
                schist_tools_transform::resize_image(&mut doc, w, h, schist_core::Filter::Bicubic)
            }
            schist_tools_transform::Plan::Neural(plan) => {
                let up = plan.run();
                schist_tools_transform::apply_upscaled(&mut doc, up);
            }
        }
    }
    for params in &recipe.adjustments {
        let kind = params.kind();
        let mut layer = schist_core::Layer::new_raster(kind.display_name());
        layer.kind = schist_core::LayerKind::Adjustment(schist_core::AdjustmentData {
            kind,
            raw: Vec::new(),
            params_json: serde_json::to_string(params).ok(),
        });
        doc.push_layer(layer);
    }
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "photo".into());
    match sink {
        BatchSink::Edit => {
            let psd = codecs
                .iter()
                .find(|c| c.can_export() && c.extensions().contains(&"psd"))
                .ok_or_else(|| anyhow::anyhow!("no PSD writer is loaded"))?;
            if let Some(name) = path.file_name() {
                doc.title = name.to_string_lossy().into_owned();
            }
            let bytes = psd.export(&doc)?;
            keep_sidecar_version(&sidecar);
            if let Some(dir) = sidecar.parent() {
                std::fs::create_dir_all(dir)?;
            }
            // A sibling temp file and a rename, so an interrupted run
            // cannot leave a truncated edit behind.
            let tmp = sidecar.with_extension("schist-tmp");
            std::fs::write(&tmp, bytes)?;
            std::fs::rename(&tmp, &sidecar)?;
            Ok(sidecar)
        }
        BatchSink::Copies {
            dir,
            codec,
            options,
        } => {
            let img = flatten(&doc)?;
            let (bytes, _, _) = flat_export(img, codec, options, 1.0)?;
            let ext = codec.extensions().first().copied().unwrap_or("png");
            let out = match dir {
                Some(dir) => unclaimed(dir, &stem, ext),
                None => {
                    let beside = path.parent().unwrap_or(Path::new("."));
                    unclaimed(beside, &format!("{stem}-edit"), ext)
                }
            };
            std::fs::write(&out, bytes)?;
            Ok(out)
        }
    }
}

/// Show a photo in the platform's file manager.
pub(super) fn reveal_in_file_manager(path: &Path) {
    #[cfg(target_os = "macos")]
    let spawned = std::process::Command::new("open")
        .arg("-R")
        .arg(path)
        .spawn();
    #[cfg(target_os = "windows")]
    let spawned = std::process::Command::new("explorer")
        .arg(format!("/select,{}", path.display()))
        .spawn();
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let spawned = std::process::Command::new("xdg-open")
        .arg(path.parent().unwrap_or(path))
        .spawn();
    if let Err(err) = spawned {
        log::warn!("could not open the file manager: {err}");
    }
}

/// One photo to `dest`, with its `.schist` sidecar and versions.
fn move_photo(path: &Path, dest: &Path) -> anyhow::Result<()> {
    let name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("no file name"))?;
    move_file(path, &dest.join(name))?;
    if let (Some(old_psd), Some(new_psd)) = (backing_psd(path), backing_psd(&dest.join(name))) {
        if old_psd.exists() {
            if let Some(dir) = new_psd.parent() {
                std::fs::create_dir_all(dir)?;
            }
            move_file(&old_psd, &new_psd)?;
        }
        // The versions of this photo, matched by the sidecar's name
        // that every stamp ends with.
        if let (Some(old_versions), Some(sidecar_name)) = (
            old_psd.parent().map(|d| d.join("versions")),
            old_psd.file_name().and_then(|n| n.to_str()),
        ) {
            if let Ok(read) = std::fs::read_dir(&old_versions) {
                for item in read.flatten() {
                    let is_ours = item
                        .file_name()
                        .to_str()
                        .is_some_and(|n| n.ends_with(sidecar_name));
                    if !is_ours {
                        continue;
                    }
                    if let Some(new_dir) = new_psd.parent().map(|d| d.join("versions")) {
                        let _ = std::fs::create_dir_all(&new_dir);
                        let _ = move_file(&item.path(), &new_dir.join(item.file_name()));
                    }
                }
            }
        }
    }
    Ok(())
}

/// Rename, or copy-and-delete across filesystems.
fn move_file(from: &Path, to: &Path) -> anyhow::Result<()> {
    if to.exists() {
        anyhow::bail!("{} already exists", to.display());
    }
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    std::fs::copy(from, to)?;
    std::fs::remove_file(from)?;
    Ok(())
}

/// Formats that already threw pixels away. An archive keeps a photo in
/// one of these rather than turning it into a bigger PNG of exactly
/// the same (already lossy) picture; anything else becomes a PNG.
const LOSSY_EXTS: &[&str] = &["jpg", "jpeg", "jpe", "jfif", "heic", "heif", "avif", "webp"];

/// The formats we can *write* back out of that list. An edited HEIC
/// has to land as a PNG: nothing here encodes HEIC.
const LOSSY_WRITABLE: &[&str] = &["jpg", "jpeg", "jpe", "jfif"];

/// How a photo goes into an archive: which file it comes from, whether
/// those bytes can go in untouched, and the extension the entry ends
/// up with.
#[derive(Debug, PartialEq)]
struct ZipPlan {
    source: PathBuf,
    verbatim: bool,
    ext: String,
}

/// Work out that plan. An edited photo contributes its edit — the
/// point of the archive is the picture you see in the gallery — and
/// the entry keeps a lossy photo's own format (re-encoded from the
/// edit when there is one, byte-for-byte when there is not), while
/// everything else becomes a PNG.
fn zip_plan(path: &Path) -> ZipPlan {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let lossy = LOSSY_EXTS.contains(&ext.as_str());
    match backing_psd(path).filter(|psd| psd.exists()) {
        // Edited: the sidecar is the picture. Keep a lossy photo's
        // format when we have an encoder for it, PNG otherwise.
        Some(edit) => {
            let keep = lossy && LOSSY_WRITABLE.contains(&ext.as_str());
            ZipPlan {
                source: edit,
                verbatim: false,
                ext: if keep { ext } else { "png".into() },
            }
        }
        // Unedited: a lossy photo (or a PNG) is already exactly what
        // the archive wants, so its bytes go in as they are —
        // re-encoding a JPEG would only lose a second generation.
        None if lossy || ext == "png" => ZipPlan {
            source: path.to_path_buf(),
            verbatim: true,
            ext,
        },
        None => ZipPlan {
            source: path.to_path_buf(),
            verbatim: false,
            ext: "png".into(),
        },
    }
}

/// The name and bytes a photo contributes to an archive. Blocking:
/// this decodes and re-encodes whole images.
fn zip_entry(
    codecs: &[Arc<dyn schist_plugin_api::CodecPlugin>],
    path: &Path,
) -> anyhow::Result<(String, Vec<u8>)> {
    let plan = zip_plan(path);
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "photo".into());
    // Named for the photo, never for its sidecar: "holiday.jpg", not
    // "holiday.jpg.psd".
    let name = format!("{stem}.{}", plan.ext);
    if plan.verbatim {
        return Ok((name, std::fs::read(&plan.source)?));
    }
    let doc = super::decode_file(codecs, &plan.source)?;
    let rect = doc.canvas_rect();
    let (w, h) = (rect.width() as u32, rect.height() as u32);
    let rgba = schist_compositor::composite_region_rgba8(&doc, rect);
    let mut out = std::io::Cursor::new(Vec::new());
    if LOSSY_WRITABLE.contains(&plan.ext.as_str()) {
        // JPEG has no alpha: lay the picture on white, as every
        // "export as JPEG" does.
        let mut rgb = Vec::with_capacity((w * h) as usize * 3);
        for px in rgba.as_chunks::<4>().0 {
            let a = px[3] as u32;
            for c in &px[..3] {
                rgb.push(((*c as u32 * a + 255 * (255 - a)) / 255) as u8);
            }
        }
        let img: image::RgbImage = image::ImageBuffer::from_raw(w, h, rgb)
            .ok_or_else(|| anyhow::anyhow!("composited buffer had the wrong size"))?;
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, ZIP_JPEG_QUALITY)
            .encode_image(&img)?;
    } else {
        let img: image::RgbaImage = image::ImageBuffer::from_raw(w, h, rgba)
            .ok_or_else(|| anyhow::anyhow!("composited buffer had the wrong size"))?;
        img.write_to(&mut out, image::ImageFormat::Png)?;
    }
    Ok((name, out.into_inner()))
}

/// What an edit re-encoded back to JPEG is saved at: high enough that
/// the second generation is not what anyone notices.
const ZIP_JPEG_QUALITY: u8 = 92;

/// A ZIP written as its entries arrive, each deflated — and stored
/// instead when deflate would not have made it smaller, which is what
/// an already-compressed JPEG or PNG usually comes to. The container
/// is a hundred honest lines by hand; the compression is flate2's.
/// Streaming rather than buffering because a whole gallery's worth of
/// rendered images does not fit in memory.
/// ZIP compression methods.
const ZIP_METHOD_STORE: u16 = 0;
const ZIP_METHOD_DEFLATE: u16 = 8;

/// Raw deflate, as ZIP wants it (no zlib framing), at flate2's default
/// level: a PNG's own filters have done the hard part already, and a
/// slower level buys a few percent on the metadata-heavy cases only.
fn deflate(bytes: &[u8]) -> Vec<u8> {
    use std::io::Write as _;
    let mut encoder =
        flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
    // Writing into a Vec cannot fail; the unwrap_or keeps the type
    // honest about it rather than the caller.
    encoder.write_all(bytes).ok();
    encoder.finish().unwrap_or_default()
}

struct ZipWriter {
    file: std::io::BufWriter<std::fs::File>,
    /// Where the archive is being built, and where it lands on
    /// `finish` — a half-written ZIP never takes the real name.
    tmp: PathBuf,
    out: PathBuf,
    offset: u32,
    central: Vec<u8>,
    entries: u16,
    used: std::collections::HashSet<String>,
}

impl ZipWriter {
    fn create(out: &Path) -> anyhow::Result<ZipWriter> {
        let tmp = out.with_extension("schist-tmp");
        Ok(ZipWriter {
            file: std::io::BufWriter::new(std::fs::File::create(&tmp)?),
            tmp,
            out: out.to_path_buf(),
            offset: 0,
            central: Vec::new(),
            entries: 0,
            used: std::collections::HashSet::new(),
        })
    }

    fn add(&mut self, name: &str, bytes: &[u8]) -> anyhow::Result<()> {
        use std::io::Write as _;
        if bytes.len() as u64 > u32::MAX as u64 {
            anyhow::bail!("too large for a zip without zip64");
        }
        // Two folders can hold the same file name; a ZIP cannot.
        let mut name = name.to_string();
        while !self.used.insert(name.clone()) {
            name = format!("{}-{name}", self.entries);
        }
        let crc = crc32fast::hash(bytes);
        let offset = self.offset;
        // Deflate, then keep whichever is smaller: a photo that is
        // already compressed does not get bigger for having been tried.
        let deflated = deflate(bytes);
        let (method, payload): (u16, &[u8]) = if deflated.len() < bytes.len() {
            (ZIP_METHOD_DEFLATE, &deflated)
        } else {
            (ZIP_METHOD_STORE, bytes)
        };
        let header = |v: &mut Vec<u8>, central_dir: bool| {
            if central_dir {
                v.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
                v.extend_from_slice(&20u16.to_le_bytes()); // made by
            } else {
                v.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
            }
            v.extend_from_slice(&20u16.to_le_bytes()); // version needed
            v.extend_from_slice(&0u16.to_le_bytes()); // flags
            v.extend_from_slice(&method.to_le_bytes());
            v.extend_from_slice(&0u32.to_le_bytes()); // dos time/date
            v.extend_from_slice(&crc.to_le_bytes()); // of the uncompressed bytes
            v.extend_from_slice(&(payload.len() as u32).to_le_bytes()); // compressed
            v.extend_from_slice(&(bytes.len() as u32).to_le_bytes()); // uncompressed
            v.extend_from_slice(&(name.len() as u16).to_le_bytes());
            v.extend_from_slice(&0u16.to_le_bytes()); // extra len
            if central_dir {
                v.extend_from_slice(&0u16.to_le_bytes()); // comment
                v.extend_from_slice(&0u16.to_le_bytes()); // disk
                v.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
                v.extend_from_slice(&0u32.to_le_bytes()); // external attrs
                v.extend_from_slice(&offset.to_le_bytes());
            }
            v.extend_from_slice(name.as_bytes());
        };
        let mut local = Vec::with_capacity(30 + name.len());
        header(&mut local, false);
        self.file.write_all(&local)?;
        self.file.write_all(payload)?;
        header(&mut self.central, true);
        self.offset += (local.len() + payload.len()) as u32;
        self.entries += 1;
        Ok(())
    }

    fn finish(mut self) -> anyhow::Result<()> {
        use std::io::Write as _;
        if self.entries == 0 {
            let _ = std::fs::remove_file(&self.tmp);
            anyhow::bail!("nothing could be read to zip");
        }
        let central_offset = self.offset;
        let central_len = self.central.len() as u32;
        self.file.write_all(&self.central)?;
        // End of central directory.
        let mut eocd = Vec::with_capacity(22);
        eocd.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        eocd.extend_from_slice(&0u16.to_le_bytes()); // this disk
        eocd.extend_from_slice(&0u16.to_le_bytes()); // central dir disk
        eocd.extend_from_slice(&self.entries.to_le_bytes());
        eocd.extend_from_slice(&self.entries.to_le_bytes());
        eocd.extend_from_slice(&central_len.to_le_bytes());
        eocd.extend_from_slice(&central_offset.to_le_bytes());
        eocd.extend_from_slice(&0u16.to_le_bytes()); // comment
        self.file.write_all(&eocd)?;
        self.file.into_inner()?.sync_all()?;
        std::fs::rename(&self.tmp, &self.out)?;
        Ok(())
    }
}

/// Decode a photo (its edit when it has one), flatten it, shrink it by
/// `scale`, and write it through `codec`. Returns the written size.
fn render_photo_as(
    codecs: &[Arc<dyn schist_plugin_api::CodecPlugin>],
    codec: &Arc<dyn schist_plugin_api::CodecPlugin>,
    path: &Path,
    options: &schist_plugin_api::ExportOptions,
    scale: f32,
    out: &Path,
) -> anyhow::Result<(u32, u32)> {
    let source = zip_plan(path).source;
    let doc = super::decode_file(codecs, &source)?;
    let img = flatten(&doc)?;
    let (bytes, w, h) = flat_export(img, codec, options, scale)?;
    std::fs::write(out, bytes)?;
    Ok((w, h))
}

/// Flatten a document to 8-bit RGBA.
fn flatten(doc: &schist_core::Document) -> anyhow::Result<image::RgbaImage> {
    let rect = doc.canvas_rect();
    let (w, h) = (rect.width() as u32, rect.height() as u32);
    let rgba = schist_compositor::composite_region_rgba8(doc, rect);
    image::ImageBuffer::from_raw(w, h, rgba)
        .ok_or_else(|| anyhow::anyhow!("composited buffer had the wrong size"))
}

/// Shrink a flat picture by `scale` and encode it through `codec`.
/// Returns the bytes and the size they hold.
fn flat_export(
    img: image::RgbaImage,
    codec: &Arc<dyn schist_plugin_api::CodecPlugin>,
    options: &schist_plugin_api::ExportOptions,
    scale: f32,
) -> anyhow::Result<(Vec<u8>, u32, u32)> {
    let (w, h) = img.dimensions();
    let (img, w, h) = if scale < 0.999 {
        let nw = ((w as f32 * scale).round() as u32).max(1);
        let nh = ((h as f32 * scale).round() as u32).max(1);
        (
            image::imageops::resize(&img, nw, nh, image::imageops::FilterType::Lanczos3),
            nw,
            nh,
        )
    } else {
        (img, w, h)
    };
    // Codecs write documents, so the flat picture becomes a one-layer
    // one — the same road the clipboard takes out of the app.
    let mut flat = schist_core::Document::new("save", w, h, schist_color::Depth::Eight);
    let mut layer = schist_core::Layer::new_raster("photo");
    schist_core::blit_rgba8(
        &mut layer.as_raster_mut().unwrap().tiles,
        schist_color::Depth::Eight,
        schist_core::IntRect::from_size(w, h),
        &img.into_raw(),
    );
    flat.push_layer(layer);
    let bytes = codec.export_with(&flat, options)?;
    Ok((bytes, w, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The archive our writer emits, read back with a bare parser: the
    /// signatures, counts and stored bytes all land where the spec puts
    /// them, which is what any unzip checks first.
    #[test]
    fn the_zip_writer_round_trips() {
        let dir = std::env::temp_dir().join(format!("schist-zip-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("out.zip");
        let mut zip = ZipWriter::create(&out).unwrap();
        zip.add("a.png", b"first").unwrap();
        zip.add("b.png", b"second!").unwrap();
        // The same name twice must not collide.
        zip.add("a.png", b"third").unwrap();
        zip.finish().unwrap();
        let zip = std::fs::read(&out).unwrap();
        // Local header, then the stored bytes right after the name.
        assert_eq!(&zip[0..4], &0x0403_4b50u32.to_le_bytes());
        let name_len = u16::from_le_bytes([zip[26], zip[27]]) as usize;
        assert_eq!(&zip[30..30 + name_len], b"a.png");
        assert_eq!(&zip[30 + name_len..30 + name_len + 5], b"first");
        // End-of-central-directory says three entries.
        let eocd = zip.len() - 22;
        assert_eq!(&zip[eocd..eocd + 4], &0x0605_4b50u32.to_le_bytes());
        assert_eq!(u16::from_le_bytes([zip[eocd + 10], zip[eocd + 11]]), 3);
        // CRC of "first" as any table has it.
        assert_eq!(
            u32::from_le_bytes([zip[14], zip[15], zip[16], zip[17]]),
            crc32fast::hash(b"first")
        );
        // The central directory starts where the header says, and its
        // first entry points back at offset zero.
        let central_offset =
            u32::from_le_bytes(zip[eocd + 16..eocd + 20].try_into().unwrap()) as usize;
        assert_eq!(
            &zip[central_offset..central_offset + 4],
            &0x0201_4b50u32.to_le_bytes()
        );
        assert_eq!(
            u32::from_le_bytes(
                zip[central_offset + 42..central_offset + 46]
                    .try_into()
                    .unwrap()
            ),
            0
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Compressible bytes come out deflated — smaller, method 8, and
    /// inflating back to the original — while bytes deflate cannot
    /// shrink are stored as they are.
    #[test]
    fn the_zip_writer_deflates_what_it_can_and_stores_the_rest() {
        use std::io::Read as _;
        let dir = std::env::temp_dir().join(format!("schist-zipdef-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("out.zip");
        let text: Vec<u8> = b"the same line over and over\n".repeat(200);
        let mut zip = ZipWriter::create(&out).unwrap();
        zip.add("text.txt", &text).unwrap();
        // Five bytes: deflate can only make them longer.
        zip.add("tiny.bin", b"first").unwrap();
        zip.finish().unwrap();
        let zip = std::fs::read(&out).unwrap();

        // First local header: method 8, compressed well under the
        // original, CRC of the *uncompressed* bytes.
        assert_eq!(u16::from_le_bytes([zip[8], zip[9]]), ZIP_METHOD_DEFLATE);
        let compressed = u32::from_le_bytes(zip[18..22].try_into().unwrap()) as usize;
        let uncompressed = u32::from_le_bytes(zip[22..26].try_into().unwrap()) as usize;
        assert_eq!(uncompressed, text.len());
        assert!(
            compressed < text.len() / 10,
            "{compressed} vs {}",
            text.len()
        );
        assert_eq!(
            u32::from_le_bytes(zip[14..18].try_into().unwrap()),
            crc32fast::hash(&text)
        );
        let name_len = u16::from_le_bytes([zip[26], zip[27]]) as usize;
        let data = &zip[30 + name_len..30 + name_len + compressed];
        let mut back = Vec::new();
        flate2::read::DeflateDecoder::new(data)
            .read_to_end(&mut back)
            .unwrap();
        assert_eq!(back, text);

        // Second local header: stored, five bytes both ways.
        let second = 30 + name_len + compressed;
        assert_eq!(&zip[second..second + 4], &0x0403_4b50u32.to_le_bytes());
        assert_eq!(
            u16::from_le_bytes([zip[second + 8], zip[second + 9]]),
            ZIP_METHOD_STORE
        );
        assert_eq!(
            u32::from_le_bytes(zip[second + 18..second + 22].try_into().unwrap()),
            5
        );
        // For checking the archive with a real unzip by hand:
        // SCHIST_KEEP_ZIP=/some/path.zip cargo test ...
        if let Ok(keep) = std::env::var("SCHIST_KEEP_ZIP") {
            std::fs::copy(&out, keep).unwrap();
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The archive's format rule: a lossy photo stays in its own
    /// format, everything else becomes a PNG, and an edited photo
    /// contributes its edit rather than the untouched original.
    #[test]
    fn archives_keep_lossy_photos_lossy_and_make_everything_else_png() {
        let dir = std::env::temp_dir().join(format!("schist-zipsrc-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".schist")).unwrap();
        for name in [
            "plain.jpg",
            "plain.png",
            "plain.tif",
            "shot.HEIC",
            "edited.jpg",
        ] {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        std::fs::write(dir.join(".schist/edited.jpg.psd"), b"edit").unwrap();
        let plan = |name: &str| zip_plan(&dir.join(name));

        // Unedited and lossy: its own bytes, its own format. Nothing is
        // gained by re-encoding a JPEG, and a generation is lost.
        assert_eq!(
            plan("plain.jpg"),
            ZipPlan {
                source: dir.join("plain.jpg"),
                verbatim: true,
                ext: "jpg".into()
            }
        );
        // Extensions are matched however they are spelled.
        assert_eq!(
            plan("shot.HEIC"),
            ZipPlan {
                source: dir.join("shot.HEIC"),
                verbatim: true,
                ext: "heic".into()
            }
        );
        // A PNG is already what the archive wants.
        assert_eq!(plan("plain.png").ext, "png");
        assert!(plan("plain.png").verbatim);
        // Lossless but not a PNG: re-encoded as one.
        assert_eq!(
            plan("plain.tif"),
            ZipPlan {
                source: dir.join("plain.tif"),
                verbatim: false,
                ext: "png".into()
            }
        );
        // Edited: the sidecar's pixels, back in the photo's own lossy
        // format — never the sidecar verbatim.
        assert_eq!(
            plan("edited.jpg"),
            ZipPlan {
                source: dir.join(".schist/edited.jpg.psd"),
                verbatim: false,
                ext: "jpg".into()
            }
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// End to end on real bytes: an edited photo contributes its
    /// edit's pixels under the photo's own name — as a JPEG when the
    /// photo was one, as a PNG when its format cannot be written.
    #[test]
    fn an_edited_photo_zips_as_its_edit() {
        use schist_color::Depth;
        use schist_core::{Document, IntRect, Layer};
        let dir = std::env::temp_dir().join(format!("schist-zippng-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".schist")).unwrap();
        // The "originals" are never read here, and are not even
        // decodable — proof that the edit is what got picked up.
        std::fs::write(dir.join("photo.jpg"), b"not a jpeg at all").unwrap();
        std::fs::write(dir.join("photo.heic"), b"not a heic at all").unwrap();
        // The edit: 8x4 of solid magenta.
        let (w, h) = (8u32, 4u32);
        let mut doc = Document::new("edit", w, h, Depth::Eight);
        let mut layer = Layer::new_raster("edit");
        let rgba: Vec<u8> = (0..w * h).flat_map(|_| [255u8, 0, 255, 255]).collect();
        schist_core::blit_rgba8(
            &mut layer.as_raster_mut().unwrap().tiles,
            Depth::Eight,
            IntRect::from_size(w, h),
            &rgba,
        );
        doc.push_layer(layer);
        let psd = schist_codec_psd::write_psd(&doc).unwrap();
        std::fs::write(dir.join(".schist/photo.jpg.psd"), &psd).unwrap();
        std::fs::write(dir.join(".schist/photo.heic.psd"), &psd).unwrap();

        let codecs: Vec<Arc<dyn schist_plugin_api::CodecPlugin>> = vec![Arc::new(crate::PsdCodec)];

        // The JPEG keeps its extension, and decodes to the edit.
        let (name, bytes) = zip_entry(&codecs, &dir.join("photo.jpg")).unwrap();
        assert_eq!(name, "photo.jpg");
        assert_eq!(&bytes[..3], b"\xff\xd8\xff");
        let decoded = image::load_from_memory(&bytes).unwrap().to_rgba8();
        assert_eq!(decoded.dimensions(), (w, h));
        // JPEG is lossy, so magenta comes back as nearly magenta.
        assert!(decoded
            .pixels()
            .all(|p| { p.0[0] > 200 && p.0[1] < 60 && p.0[2] > 200 }));

        // The HEIC has no encoder here, so it lands as a PNG — exactly.
        let (name, bytes) = zip_entry(&codecs, &dir.join("photo.heic")).unwrap();
        assert_eq!(name, "photo.png");
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
        let decoded = image::load_from_memory(&bytes).unwrap().to_rgba8();
        assert_eq!(decoded.dimensions(), (w, h));
        assert!(decoded.pixels().all(|p| p.0 == [255, 0, 255, 255]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A 6x2 document, magenta on the left half and black on the right,
    /// as a PSD: the batch test's "photo" and, once written, its edit.
    fn half_magenta(dir: &std::path::Path, name: &str) -> PathBuf {
        use schist_color::Depth;
        use schist_core::{Document, IntRect, Layer};
        let (w, h) = (6u32, 2u32);
        let mut doc = Document::new("photo", w, h, Depth::Eight);
        let mut layer = Layer::new_raster("photo");
        let rgba: Vec<u8> = (0..w * h)
            .flat_map(|i| {
                if i % w < 3 {
                    [255u8, 0, 255, 255]
                } else {
                    [0, 0, 0, 255]
                }
            })
            .collect();
        schist_core::blit_rgba8(
            &mut layer.as_raster_mut().unwrap().tiles,
            Depth::Eight,
            IntRect::from_size(w, h),
            &rgba,
        );
        doc.push_layer(layer);
        let path = dir.join(name);
        std::fs::write(&path, schist_codec_psd::write_psd(&doc).unwrap()).unwrap();
        path
    }

    /// The batch run, end to end: a turn and an adjustment land in the
    /// sidecar as a rotated canvas plus a live adjustment layer, the
    /// original is untouched, a second run versions the first edit, and
    /// the copies target writes a flat file beside the photo without
    /// overwriting anything.
    #[test]
    fn batch_process_writes_versioned_edits_and_flat_copies() {
        let dir = std::env::temp_dir().join(format!("schist-batch-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let photo = half_magenta(&dir, "photo.psd");
        let original_bytes = std::fs::read(&photo).unwrap();
        let codecs: Vec<Arc<dyn schist_plugin_api::CodecPlugin>> = vec![Arc::new(crate::PsdCodec)];

        let recipe = BatchRecipe {
            rotate: Some(CanvasTransform::Cw90),
            adjustments: vec![schist_adjustments::Params::Invert],
            ..Default::default()
        };
        let out = process_photo(&codecs, &photo, &recipe, &BatchSink::Edit).unwrap();
        assert_eq!(out, dir.join(".schist/photo.psd.psd"));
        assert_eq!(std::fs::read(&photo).unwrap(), original_bytes);
        let edit = schist_codec_psd::read_psd(&std::fs::read(&out).unwrap()).unwrap();
        let rect = edit.canvas_rect();
        // Turned on its side, with the adjustment kept live on top.
        assert_eq!((rect.width(), rect.height()), (2, 6));
        assert!(edit.tree.iter().any(|l| matches!(
            &l.kind,
            schist_core::LayerKind::Adjustment(a) if a.kind == schist_core::AdjustmentKind::Invert
        )));
        // Clockwise, the magenta left half becomes the top half; inverted
        // it is green, and the black bottom half is white.
        let pixels = schist_compositor::composite_region_rgba8(&edit, rect);
        let px = pixels.as_chunks::<4>().0;
        assert!(px[..6].iter().all(|p| p[0] < 16 && p[1] > 240 && p[2] < 16));
        assert!(px[6..]
            .iter()
            .all(|p| p[0] > 240 && p[1] > 240 && p[2] > 240));

        // Running again works on the edit and keeps the first as a version.
        let again = BatchRecipe {
            flip_v: true,
            ..Default::default()
        };
        process_photo(&codecs, &photo, &again, &BatchSink::Edit).unwrap();
        let versions: Vec<_> = std::fs::read_dir(dir.join(".schist/versions"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(versions.len(), 1);
        assert!(versions[0].ends_with("-photo.psd.psd"));
        let edit = schist_codec_psd::read_psd(&std::fs::read(&out).unwrap()).unwrap();
        let rect = edit.canvas_rect();
        assert_eq!((rect.width(), rect.height()), (2, 6));
        let pixels = schist_compositor::composite_region_rgba8(&edit, rect);
        let px = pixels.as_chunks::<4>().0;
        // Flipped: white on top now, green below.
        assert!(px[..6]
            .iter()
            .all(|p| p[0] > 240 && p[1] > 240 && p[2] > 240));
        assert!(px[6..].iter().all(|p| p[0] < 16 && p[1] > 240 && p[2] < 16));

        // Copies go beside the photo, from the edit, and never overwrite.
        let sink = BatchSink::Copies {
            dir: None,
            codec: codecs[0].clone(),
            options: schist_plugin_api::ExportOptions::default(),
        };
        let first = process_photo(&codecs, &photo, &again, &sink).unwrap();
        assert_eq!(first, dir.join("photo-edit.psd"));
        let second = process_photo(&codecs, &photo, &again, &sink).unwrap();
        assert_eq!(second, dir.join("photo-edit-2.psd"));
        let flat = schist_codec_psd::read_psd(&std::fs::read(&first).unwrap()).unwrap();
        assert_eq!(flat.tree.layers.len(), 1);
        let rect = flat.canvas_rect();
        let pixels = schist_compositor::composite_region_rgba8(&flat, rect);
        let px = pixels.as_chunks::<4>().0;
        // The edit was white-over-green; flipped once more it is green
        // on top again.
        assert!(px[..6].iter().all(|p| p[0] < 16 && p[1] > 240 && p[2] < 16));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Save Image As, end to end on real bytes: the edit is what gets
    /// saved, scaled as asked, through whichever codec was chosen.
    #[test]
    fn save_image_as_renders_the_edit_at_the_asked_scale() {
        use schist_color::Depth;
        use schist_core::{Document, IntRect, Layer};
        let dir = std::env::temp_dir().join(format!("schist-saveas-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".schist")).unwrap();
        // An undecodable "original" with a real 8x4 magenta edit beside it.
        std::fs::write(dir.join("photo.jpg"), b"not a jpeg").unwrap();
        let (w, h) = (8u32, 4u32);
        let mut doc = Document::new("edit", w, h, Depth::Eight);
        let mut layer = Layer::new_raster("edit");
        let rgba: Vec<u8> = (0..w * h).flat_map(|_| [255u8, 0, 255, 255]).collect();
        schist_core::blit_rgba8(
            &mut layer.as_raster_mut().unwrap().tiles,
            Depth::Eight,
            IntRect::from_size(w, h),
            &rgba,
        );
        doc.push_layer(layer);
        std::fs::write(
            dir.join(".schist/photo.jpg.psd"),
            schist_codec_psd::write_psd(&doc).unwrap(),
        )
        .unwrap();

        let codecs: Vec<Arc<dyn schist_plugin_api::CodecPlugin>> = vec![Arc::new(crate::PsdCodec)];
        let out = dir.join("out.psd");
        let size = render_photo_as(
            &codecs,
            &codecs[0],
            &dir.join("photo.jpg"),
            &schist_plugin_api::ExportOptions::default(),
            0.5,
            &out,
        )
        .unwrap();
        assert_eq!(size, (4, 2));
        // Read it back through the codec: half the size, still magenta.
        let back = schist_codec_psd::read_psd(&std::fs::read(&out).unwrap()).unwrap();
        let rect = back.canvas_rect();
        assert_eq!((rect.width(), rect.height()), (4, 2));
        let pixels = schist_compositor::composite_region_rgba8(&back, rect);
        assert!(pixels
            .as_chunks::<4>()
            .0
            .iter()
            .all(|p| p[0] > 240 && p[1] < 16 && p[2] > 240));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn moving_a_photo_brings_its_sidecar_and_versions() {
        let dir = std::env::temp_dir().join(format!("schist-move-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (from, to) = (dir.join("from"), dir.join("to"));
        std::fs::create_dir_all(from.join(".schist/versions")).unwrap();
        std::fs::create_dir_all(&to).unwrap();
        std::fs::write(from.join("a.jpg"), b"photo").unwrap();
        std::fs::write(from.join(".schist/a.jpg.psd"), b"edit").unwrap();
        std::fs::write(from.join(".schist/versions/123-a.jpg.psd"), b"v1").unwrap();
        // A neighbour's version must stay behind.
        std::fs::write(from.join(".schist/versions/123-b.jpg.psd"), b"other").unwrap();
        move_photo(&from.join("a.jpg"), &to).unwrap();
        assert!(to.join("a.jpg").exists());
        assert!(to.join(".schist/a.jpg.psd").exists());
        assert!(to.join(".schist/versions/123-a.jpg.psd").exists());
        assert!(!from.join("a.jpg").exists());
        assert!(!from.join(".schist/a.jpg.psd").exists());
        assert!(from.join(".schist/versions/123-b.jpg.psd").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
