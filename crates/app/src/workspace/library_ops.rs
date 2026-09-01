//! Bulk actions on gallery photos: move them to a folder (sidecars in
//! tow — the versioning invariant is that edits live beside their
//! photo), pack them into a ZIP, upscale them with the built-in
//! waifu2x. Everything here runs on the background executor with the
//! tray narrating.

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
        if paths.is_empty() {
            return;
        }
        let dir = std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."));
        let rx = cx.prompt_for_new_path(&dir, Some(&suggested));
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(out))) = rx.await else { return };
            let count = paths.len();
            let result = cx
                .background_executor()
                .spawn(async move { write_zip(&out, &paths).map(|()| out) })
                .await;
            this.update_in(cx, |ws, _window, cx| {
                match result {
                    Ok(out) => {
                        ws.status = format!("Zipped {count} photos to {}", out.display()).into()
                    }
                    Err(err) => {
                        log::error!("zip failed: {err:#}");
                        ws.status = format!("ZIP failed: {err}").into();
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Double every photo with the built-in waifu2x, writing
    /// `<name>@2x.png` beside each original. The model ships in the
    /// binary, so this needs no download.
    pub(super) fn upscale_photos(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        if paths.is_empty() {
            return;
        }
        let total = paths.len();
        self.status = format!("Upscaling {total} photos\u{2026}").into();
        cx.notify();
        let codecs = self.registry.shared_codecs();
        cx.spawn(async move |this, cx| {
            for (done, path) in paths.into_iter().enumerate() {
                let job_codecs = codecs.clone();
                let result = cx
                    .background_executor()
                    .spawn(async move { upscale_photo(&job_codecs, &path) })
                    .await;
                let keep = this.update(cx, |ws, cx| {
                    match result {
                        Ok(out) => {
                            ws.status =
                                format!("Upscaled {}/{total} — {}", done + 1, out.display()).into()
                        }
                        Err(err) => {
                            log::error!("upscale failed: {err:#}");
                            ws.status = format!("Upscale failed: {err}").into();
                        }
                    }
                    cx.notify();
                });
                if keep.is_err() {
                    return;
                }
            }
            this.update(cx, |ws, cx| {
                ws.library_rescan(cx);
            })
            .ok();
        })
        .detach();
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

/// A stored (uncompressed) ZIP of the given files. Photos are already
/// compressed, so store beats deflate here — and a store-only writer is
/// a hundred honest lines instead of a dependency.
fn write_zip(out: &Path, paths: &[PathBuf]) -> anyhow::Result<()> {
    let mut body: Vec<u8> = Vec::new();
    let mut central: Vec<u8> = Vec::new();
    let mut entries = 0u16;
    let mut used: std::collections::HashSet<String> = std::collections::HashSet::new();
    for path in paths {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(err) => {
                log::warn!("zip: skipping {}: {err}", path.display());
                continue;
            }
        };
        if bytes.len() as u64 > u32::MAX as u64 {
            log::warn!("zip: skipping {} (zip64 not written)", path.display());
            continue;
        }
        let mut name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| format!("photo-{entries}"));
        // Two folders can hold the same file name; a ZIP cannot.
        while !used.insert(name.clone()) {
            name = format!("{entries}-{name}");
        }
        let crc = crc32fast::hash(&bytes);
        let offset = body.len() as u32;
        let header = |v: &mut Vec<u8>, central_dir: bool| {
            if central_dir {
                v.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
                v.extend_from_slice(&20u16.to_le_bytes()); // made by
            } else {
                v.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
            }
            v.extend_from_slice(&20u16.to_le_bytes()); // version needed
            v.extend_from_slice(&0u16.to_le_bytes()); // flags
            v.extend_from_slice(&0u16.to_le_bytes()); // method: store
            v.extend_from_slice(&0u32.to_le_bytes()); // dos time/date
            v.extend_from_slice(&crc.to_le_bytes());
            v.extend_from_slice(&(bytes.len() as u32).to_le_bytes()); // compressed
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
        header(&mut body, false);
        body.extend_from_slice(&bytes);
        header(&mut central, true);
        entries += 1;
    }
    if entries == 0 {
        anyhow::bail!("nothing could be read to zip");
    }
    let central_offset = body.len() as u32;
    let central_len = central.len() as u32;
    body.extend_from_slice(&central);
    // End of central directory.
    body.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    body.extend_from_slice(&0u16.to_le_bytes()); // this disk
    body.extend_from_slice(&0u16.to_le_bytes()); // central dir disk
    body.extend_from_slice(&entries.to_le_bytes());
    body.extend_from_slice(&entries.to_le_bytes());
    body.extend_from_slice(&central_len.to_le_bytes());
    body.extend_from_slice(&central_offset.to_le_bytes());
    body.extend_from_slice(&0u16.to_le_bytes()); // comment
    let tmp = out.with_extension("schist-tmp");
    std::fs::write(&tmp, &body)?;
    std::fs::rename(&tmp, out)?;
    Ok(())
}

/// Decode a photo whole, run the built-in waifu2x over it, and write
/// the double-size PNG beside the original. Blocking and heavy.
fn upscale_photo(
    codecs: &[Arc<dyn schist_plugin_api::CodecPlugin>],
    path: &Path,
) -> anyhow::Result<PathBuf> {
    let model = schist_neural::get("waifu2x-photo")
        .ok_or_else(|| anyhow::anyhow!("the waifu2x model failed to load"))?;
    let doc = super::decode_file(codecs, path)?;
    let rect = doc.canvas_rect();
    let (w, h) = (rect.width() as usize, rect.height() as usize);
    let rgba = schist_compositor::composite_region_rgba8(&doc, rect);
    let mut rgb = Vec::with_capacity(w * h * 3);
    for px in rgba.as_chunks::<4>().0 {
        rgb.extend([
            px[0] as f32 / 255.0,
            px[1] as f32 / 255.0,
            px[2] as f32 / 255.0,
        ]);
    }
    let out = schist_neural::run_scaled(&model, &rgb, w, h)
        .ok_or_else(|| anyhow::anyhow!("the model declined the image"))?;
    let (ow, oh) = (w * 2, h * 2);
    let mut pixels = Vec::with_capacity(ow * oh * 3);
    for v in &out {
        pixels.push((v.clamp(0.0, 1.0) * 255.0).round() as u8);
    }
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "photo".into());
    let target = path.with_file_name(format!("{stem}@2x.png"));
    let img: image::RgbImage = image::ImageBuffer::from_raw(ow as u32, oh as u32, pixels)
        .ok_or_else(|| anyhow::anyhow!("upscaled buffer had the wrong size"))?;
    img.save(&target)?;
    Ok(target)
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
        std::fs::write(dir.join("a.jpg"), b"first").unwrap();
        std::fs::write(dir.join("b.jpg"), b"second!").unwrap();
        let out = dir.join("out.zip");
        write_zip(&out, &[dir.join("a.jpg"), dir.join("b.jpg")]).unwrap();
        let zip = std::fs::read(&out).unwrap();
        // Local header, then the stored bytes right after the name.
        assert_eq!(&zip[0..4], &0x0403_4b50u32.to_le_bytes());
        let name_len = u16::from_le_bytes([zip[26], zip[27]]) as usize;
        assert_eq!(&zip[30..30 + name_len], b"a.jpg");
        assert_eq!(&zip[30 + name_len..30 + name_len + 5], b"first");
        // End-of-central-directory says two entries.
        let eocd = zip.len() - 22;
        assert_eq!(&zip[eocd..eocd + 4], &0x0605_4b50u32.to_le_bytes());
        assert_eq!(u16::from_le_bytes([zip[eocd + 10], zip[eocd + 11]]), 2);
        // CRC of "first" as any table has it.
        assert_eq!(
            u32::from_le_bytes([zip[14], zip[15], zip[16], zip[17]]),
            crc32fast::hash(b"first")
        );
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
