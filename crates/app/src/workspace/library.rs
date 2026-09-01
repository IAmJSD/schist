//! The photo gallery: watched folders, thumbnails, camera import, and
//! the PSD backing files behind gallery edits.
//!
//! Half Picasa, half Lightroom: folders are watched in place rather than
//! copied into a catalogue, and edits never touch the original file.
//! Opening a photo from the gallery gives the document a hidden sidecar
//! (`<folder>/.schist/<name>.psd`) as its save path, so ⌘S writes the
//! layered edit there — with the previous state copied into
//! `.schist/versions/` first — and the gallery thumbnails render from
//! the sidecar once one exists. Desktop only: a browser tab has no
//! folders to watch and no cameras to mount, so the whole module is
//! compiled out of the web build.

use super::*;
use std::collections::BTreeMap;
use std::hash::{Hash as _, Hasher as _};
use std::path::Path;

/// Longest edge of a rendered thumbnail. Cells scale the image down from
/// here, so one render serves every position of the size slider.
const THUMB_EDGE: u32 = 256;
/// Thumbnails decoded per background batch. Small enough that the first
/// screenful streams in rather than arriving all at once at the end.
const THUMB_BATCH: usize = 4;
/// Folder scanning stops here rather than following a loop of symlinks
/// (or someone's home directory) forever.
const SCAN_MAX_DEPTH: usize = 6;
const SCAN_MAX_FILES: usize = 5000;
/// How many recently opened files the start screen lists.
const RECENTS_KEPT: usize = 10;

/// One scanned directory and the images in it, a section of the grid.
pub struct Section {
    pub dir: PathBuf,
    pub entries: Vec<Entry>,
}

/// One image in the gallery.
#[derive(Clone)]
pub struct Entry {
    pub path: PathBuf,
    /// Modification seconds of whichever file the thumbnail renders from,
    /// part of the disk-cache key.
    pub mtime: u64,
    /// A PSD sidecar exists: the thumbnail shows the edit, and the cell
    /// wears a badge.
    pub edited: bool,
}

/// A thumbnail's place in the pipeline.
pub enum Thumb {
    /// Queued or decoding.
    Pending,
    Ready(Arc<RenderImage>),
    /// Decode failed; the cell shows a placeholder and nothing retries.
    Failed,
}

/// One queued thumbnail decode.
#[derive(Clone)]
struct ThumbJob {
    /// The original image path, which is what the grid keys cells by.
    key: PathBuf,
    /// What actually gets rendered: the PSD sidecar when one exists.
    source: PathBuf,
    mtime: u64,
}

/// What `library.json` persists: the watched folders and the recents.
/// Everything else — sections, thumbnails — is derived from the disk.
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct LibraryFile {
    folders: Vec<PathBuf>,
    #[serde(default)]
    recents: Vec<PathBuf>,
    #[serde(default)]
    thumb_px: Option<f32>,
}

pub struct Library {
    /// Whether the gallery view is showing instead of the editor.
    pub open: bool,
    /// The watched folder roots, persisted.
    pub folders: Vec<PathBuf>,
    /// Recently opened files, newest first, persisted.
    pub recents: Vec<PathBuf>,
    /// Scan result, grouped by directory.
    pub sections: Vec<Section>,
    /// Sidebar filter: show only sections under this root. `None` = all.
    pub folder_filter: Option<PathBuf>,
    /// The clicked image, if any.
    pub selected: Option<PathBuf>,
    /// Thumbnail cell edge in pixels, the tray slider's value.
    pub thumb_px: f32,
    pub scanning: bool,
    /// A camera import in flight, so a second click does not start one.
    pub importing: bool,
    thumbs: FxHashMap<PathBuf, Thumb>,
    queue: Vec<ThumbJob>,
    /// Whether a thumbnail loader task is live (only ever one at a time).
    ticker: bool,
    /// A gallery open waiting for its decode: (path being loaded, the
    /// original image it is an edit of). Consumed by `finish_load`.
    pub(super) pending_backing: Option<(PathBuf, PathBuf)>,
    /// Original image path per open document that came from the gallery,
    /// so a save can refresh that image's thumbnail.
    edit_backings: FxHashMap<schist_core::DocumentId, PathBuf>,
}

impl Library {
    /// Load the persisted folder list and recents.
    pub fn load() -> Library {
        let file: LibraryFile = library_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        Library {
            open: false,
            folders: file.folders,
            recents: file.recents,
            sections: Vec::new(),
            folder_filter: None,
            selected: None,
            thumb_px: file.thumb_px.unwrap_or(144.0).clamp(80.0, 240.0),
            scanning: false,
            importing: false,
            thumbs: FxHashMap::default(),
            queue: Vec::new(),
            ticker: false,
            pending_backing: None,
            edit_backings: FxHashMap::default(),
        }
    }

    fn save(&self) {
        let Some(path) = library_path() else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let file = LibraryFile {
            folders: self.folders.clone(),
            recents: self.recents.clone(),
            thumb_px: Some(self.thumb_px),
        };
        if let Ok(json) = serde_json::to_string_pretty(&file) {
            let _ = std::fs::write(path, json);
        }
    }

    /// The thumbnail for `entry`, queueing a decode the first time a cell
    /// asks. Rendering is what drives loading, so only images that have
    /// been on screen ever cost a decode.
    pub fn thumb(&mut self, entry: &Entry) -> Option<Arc<RenderImage>> {
        match self.thumbs.get(&entry.path) {
            Some(Thumb::Ready(img)) => return Some(img.clone()),
            Some(_) => return None,
            None => {}
        }
        self.thumbs.insert(entry.path.clone(), Thumb::Pending);
        self.queue.push(ThumbJob {
            key: entry.path.clone(),
            source: thumb_source(&entry.path, entry.edited),
            mtime: entry.mtime,
        });
        None
    }

    /// Whether any queued decode is waiting for a loader task.
    pub fn wants_thumbs(&self) -> bool {
        !self.queue.is_empty() && !self.ticker
    }

    /// Whether a thumbnail decode gave up, so the cell can say so.
    pub fn thumb_failed(&self, path: &Path) -> bool {
        matches!(self.thumbs.get(path), Some(Thumb::Failed))
    }

    /// Sections after the sidebar filter.
    pub fn visible_sections(&self) -> impl Iterator<Item = &Section> {
        let filter = self.folder_filter.clone();
        self.sections
            .iter()
            .filter(move |s| match &filter {
                Some(root) => s.dir.starts_with(root),
                None => true,
            })
            .filter(|s| !s.entries.is_empty())
    }

    pub fn photo_count(&self) -> usize {
        self.visible_sections().map(|s| s.entries.len()).sum()
    }

    /// The selected entry, if it still exists in a section.
    pub fn selected_entry(&self) -> Option<&Entry> {
        let sel = self.selected.as_ref()?;
        self.sections
            .iter()
            .flat_map(|s| s.entries.iter())
            .find(|e| &e.path == sel)
    }
}

/// Where the folder list is persisted, beside the other config files.
fn library_path() -> Option<PathBuf> {
    let base = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join(".config"))
        })?;
    Some(base.join("schist/library.json"))
}

/// The PSD sidecar an edit of `original` saves into.
pub(super) fn backing_psd(original: &Path) -> Option<PathBuf> {
    let dir = original.parent()?;
    let name = original.file_name()?.to_string_lossy();
    Some(dir.join(".schist").join(format!("{name}.psd")))
}

/// What a thumbnail renders from: the sidecar once one exists, so the
/// gallery shows the edit, as Picasa does.
fn thumb_source(original: &Path, edited: bool) -> PathBuf {
    if edited {
        if let Some(psd) = backing_psd(original) {
            return psd;
        }
    }
    original.to_path_buf()
}

fn mtime_secs(path: &Path) -> u64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Walk the watched folders and group every decodable image by directory.
/// Blocking, so it runs on a background thread.
fn scan_folders(roots: &[PathBuf], exts: &[String]) -> Vec<Section> {
    let mut by_dir: BTreeMap<PathBuf, Vec<Entry>> = BTreeMap::new();
    let mut budget = SCAN_MAX_FILES;
    for root in roots {
        walk(root, 0, exts, &mut by_dir, &mut budget);
    }
    by_dir
        .into_iter()
        .map(|(dir, mut entries)| {
            entries.sort_by(|a, b| a.path.cmp(&b.path));
            Section { dir, entries }
        })
        .collect()
}

fn walk(
    dir: &Path,
    depth: usize,
    exts: &[String],
    out: &mut BTreeMap<PathBuf, Vec<Entry>>,
    budget: &mut usize,
) {
    if depth > SCAN_MAX_DEPTH || *budget == 0 {
        return;
    }
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    for item in read.flatten() {
        if *budget == 0 {
            return;
        }
        let path = item.path();
        let hidden = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with('.'));
        // Dot-directories include the `.schist` sidecars, which must not
        // list as photos of their own.
        if hidden {
            continue;
        }
        if path.is_dir() {
            walk(&path, depth + 1, exts, out, budget);
            continue;
        }
        let known = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .is_some_and(|e| exts.iter().any(|x| x == &e));
        if !known {
            continue;
        }
        let edited = backing_psd(&path).is_some_and(|p| p.exists());
        let mtime = mtime_secs(&thumb_source(&path, edited));
        *budget -= 1;
        out.entry(dir.to_path_buf()).or_default().push(Entry {
            path,
            mtime,
            edited,
        });
    }
}

/// Where rendered thumbnails are cached between runs, keyed by source
/// path, mtime and render size — a re-edited photo gets a fresh entry and
/// the stale one ages out with the directory.
fn thumb_cache_path(source: &Path, mtime: u64) -> Option<PathBuf> {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    source.hash(&mut hasher);
    mtime.hash(&mut hasher);
    THUMB_EDGE.hash(&mut hasher);
    let dir = crate::crash::state_dir()?.join("schist/thumbs");
    Some(dir.join(format!("{:016x}.png", hasher.finish())))
}

/// RGBA straight bytes as the BGRA frame `RenderImage` wants.
fn rgba_to_render_image(width: u32, height: u32, mut rgba: Vec<u8>) -> Option<Arc<RenderImage>> {
    for px in rgba.as_chunks_mut::<4>().0 {
        px.swap(0, 2);
    }
    let buffer = image::RgbaImage::from_raw(width, height, rgba)?;
    Some(Arc::new(RenderImage::new(smallvec![image::Frame::new(
        buffer
    )])))
}

/// Decode one thumbnail, through the disk cache when it can. Blocking.
fn load_thumb(job: &ThumbJob) -> Option<Arc<RenderImage>> {
    let cache = thumb_cache_path(&job.source, job.mtime);
    if let Some(cached) = cache.as_ref().and_then(|p| std::fs::read(p).ok()) {
        if let Ok(img) = image::load_from_memory(&cached) {
            let img = img.into_rgba8();
            return rgba_to_render_image(img.width(), img.height(), img.into_raw());
        }
    }
    let preview = match schist_preview::render_file(&job.source, THUMB_EDGE) {
        Ok(p) => p,
        Err(err) => {
            log::warn!("thumbnail failed for {}: {err:#}", job.source.display());
            return None;
        }
    };
    if let (Some(path), Ok(png)) = (&cache, preview.to_png()) {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, png);
    }
    rgba_to_render_image(preview.width, preview.height, preview.rgba)
}

/// Mounted volumes that look like cameras or cards: anything under the
/// removable-media roots with a `DCIM` directory, which is what the
/// design rule every camera follows requires them to create.
pub(crate) fn camera_sources() -> Vec<PathBuf> {
    let mut roots = vec![
        PathBuf::from("/Volumes"),
        PathBuf::from("/media"),
        PathBuf::from("/mnt"),
    ];
    if let Ok(user) = std::env::var("USER") {
        roots.push(PathBuf::from(format!("/media/{user}")));
        roots.push(PathBuf::from(format!("/run/media/{user}")));
    }
    let mut out = Vec::new();
    for root in roots {
        let Ok(read) = std::fs::read_dir(root) else {
            continue;
        };
        for item in read.flatten() {
            let path = item.path();
            if path.join("DCIM").is_dir() {
                out.push(path);
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Copy every image under `source/DCIM` into `dest`, skipping files that
/// already arrived (same name, same size). Blocking; returns how many
/// files were copied.
fn copy_dcim(source: &Path, dest: &Path, exts: &[String]) -> anyhow::Result<usize> {
    std::fs::create_dir_all(dest)?;
    let mut copied = 0;
    let mut stack = vec![source.join("DCIM")];
    while let Some(dir) = stack.pop() {
        let Ok(read) = std::fs::read_dir(&dir) else {
            continue;
        };
        for item in read.flatten() {
            let path = item.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let known = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_ascii_lowercase())
                .is_some_and(|e| exts.iter().any(|x| x == &e));
            let Some(name) = path.file_name() else {
                continue;
            };
            if !known {
                continue;
            }
            let target = dest.join(name);
            let same = match (std::fs::metadata(&path), std::fs::metadata(&target)) {
                (Ok(a), Ok(b)) => a.len() == b.len(),
                _ => false,
            };
            if same {
                continue;
            }
            std::fs::copy(&path, &target)?;
            copied += 1;
        }
    }
    Ok(copied)
}

impl Workspace {
    /// Show or hide the gallery. Opening rescans the watched folders, so
    /// files added outside Schist appear without a manual refresh.
    pub fn toggle_gallery(&mut self, cx: &mut Context<Self>) {
        self.library.open = !self.library.open;
        self.open_popup = None;
        self.open_submenu.clear();
        if self.library.open {
            self.library_rescan(cx);
        }
        cx.notify();
    }

    /// Re-walk the watched folders on a background thread.
    pub fn library_rescan(&mut self, cx: &mut Context<Self>) {
        if self.library.folders.is_empty() {
            self.library.sections = Vec::new();
            return;
        }
        if self.library.scanning {
            return;
        }
        self.library.scanning = true;
        let folders = self.library.folders.clone();
        let exts = self.codec_extensions();
        cx.spawn(async move |this, cx| {
            let sections = cx
                .background_executor()
                .spawn(async move { scan_folders(&folders, &exts) })
                .await;
            this.update(cx, |ws, cx| {
                ws.library.scanning = false;
                ws.library.sections = sections;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Every extension a registered codec can decode, lowercased.
    fn codec_extensions(&self) -> Vec<String> {
        self.registry
            .codecs()
            .flat_map(|c| c.extensions())
            .map(|e| e.to_string())
            .collect()
    }

    /// Start the thumbnail loader if decodes are queued and none is
    /// running. Called from the gallery render, the same way the canvas
    /// kicks tile prefetch from paint.
    pub(super) fn kick_thumb_loader(&mut self, cx: &mut Context<Self>) {
        if !self.library.wants_thumbs() {
            return;
        }
        self.library.ticker = true;
        cx.spawn(async move |this, cx| loop {
            let batch: Vec<ThumbJob> = match this.update(cx, |ws, _| {
                let queue = &mut ws.library.queue;
                let n = queue.len().min(THUMB_BATCH);
                queue.drain(..n).collect()
            }) {
                Ok(batch) => batch,
                Err(_) => return,
            };
            if batch.is_empty() {
                this.update(cx, |ws, _| ws.library.ticker = false).ok();
                return;
            }
            let results = cx
                .background_executor()
                .spawn(async move {
                    batch
                        .into_iter()
                        .map(|job| {
                            let img = load_thumb(&job);
                            (job.key, img)
                        })
                        .collect::<Vec<_>>()
                })
                .await;
            let keep = this.update(cx, |ws, cx| {
                for (key, img) in results {
                    let state = match img {
                        Some(img) => Thumb::Ready(img),
                        None => Thumb::Failed,
                    };
                    ws.library.thumbs.insert(key, state);
                }
                cx.notify();
            });
            if keep.is_err() {
                return;
            }
        })
        .detach();
    }

    /// Ask for folders and watch them. Multiple selection: adding a
    /// year's worth of albums should not take a dialog each.
    pub fn gallery_add_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: true,
            prompt: Some("Add to Gallery".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                this.update_in(cx, |ws, _window, cx| {
                    let mut added = 0;
                    for path in paths {
                        if !ws.library.folders.contains(&path) {
                            ws.library.folders.push(path);
                            added += 1;
                        }
                    }
                    if added > 0 {
                        ws.library.folders.sort();
                        ws.library.save();
                    }
                    ws.library.open = true;
                    ws.library_rescan(cx);
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    /// Stop watching a folder. The photos and any `.schist` sidecars stay
    /// on disk untouched — this only forgets the folder.
    pub fn gallery_remove_folder(&mut self, folder: &Path, cx: &mut Context<Self>) {
        self.library.folders.retain(|f| f != folder);
        if self.library.folder_filter.as_deref() == Some(folder) {
            self.library.folder_filter = None;
        }
        self.library.save();
        self.library_rescan(cx);
        cx.notify();
    }

    /// Import from a mounted camera. One source imports straight away; a
    /// choice of several opens the picker dialog.
    pub fn gallery_import_camera(&mut self, cx: &mut Context<Self>) {
        if self.library.importing {
            return;
        }
        let mut sources = camera_sources();
        match sources.len() {
            0 => {
                self.status =
                    "No camera found — mount one and look for a volume with a DCIM folder".into();
                cx.notify();
            }
            1 => self.import_camera(sources.remove(0), cx),
            _ => self.open_modal(Modal::CameraImport { sources }, cx),
        }
    }

    /// Copy a camera volume's DCIM into ~/Pictures and watch the result.
    pub fn import_camera(&mut self, source: PathBuf, cx: &mut Context<Self>) {
        if self.library.importing {
            return;
        }
        let volume = source
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Camera".into());
        let Some(home) = std::env::var("HOME").ok().map(PathBuf::from) else {
            self.status = "Import needs a home directory to copy into".into();
            return;
        };
        let dest = home.join("Pictures/Schist Imports").join(&volume);
        self.library.importing = true;
        self.status = format!("Importing from {volume}\u{2026}").into();
        cx.notify();
        let exts = self.codec_extensions();
        let copy_dest = dest.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { copy_dcim(&source, &copy_dest, &exts) })
                .await;
            this.update(cx, |ws, cx| {
                ws.library.importing = false;
                match result {
                    Ok(copied) => {
                        if !ws.library.folders.contains(&dest) {
                            ws.library.folders.push(dest.clone());
                            ws.library.folders.sort();
                            ws.library.save();
                        }
                        ws.status =
                            format!("Imported {copied} photos to {}", dest.display()).into();
                        ws.library.open = true;
                        ws.library_rescan(cx);
                    }
                    Err(err) => {
                        log::error!("camera import failed: {err:#}");
                        ws.status = format!("Import failed: {err}").into();
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Open a gallery photo for editing. The PSD sidecar is what opens
    /// when one exists — that is where the layers of the last edit live —
    /// and either way the document saves to the sidecar, never over the
    /// original.
    pub fn open_from_gallery(&mut self, original: PathBuf, cx: &mut Context<Self>) {
        let Some(psd) = backing_psd(&original) else {
            return;
        };
        let target = if psd.exists() { psd } else { original.clone() };
        self.library.pending_backing = Some((target.clone(), original));
        self.load_file(target, cx);
    }

    /// Bookkeeping when a load finishes: adopt a gallery edit's backing
    /// arrangement, or record an ordinary open in the recents.
    pub(super) fn finish_load_bookkeeping(&mut self, loaded: &Path) {
        let claimed = self
            .library
            .pending_backing
            .as_ref()
            .is_some_and(|(target, _)| target == loaded);
        if !claimed {
            self.note_recent(loaded);
            return;
        }
        let Some((_, original)) = self.library.pending_backing.take() else {
            return;
        };
        let Some(doc) = self.doc.as_mut() else {
            return;
        };
        // ⌘S goes to the sidecar from the first save, and the title stays
        // the photo's own name rather than the sidecar's.
        doc.path = backing_psd(&original);
        if let Some(name) = original.file_name() {
            doc.title = name.to_string_lossy().into_owned();
        }
        self.library.edit_backings.insert(doc.id, original);
    }

    /// Before a save lands on a gallery sidecar: make sure its hidden
    /// directory exists, and copy the previous sidecar into `versions/`
    /// so every save is a version, automatically.
    pub(super) fn pre_save_backing(&mut self, path: &Path) {
        let backed = self
            .doc
            .as_ref()
            .and_then(|d| self.library.edit_backings.get(&d.id))
            .and_then(|original| backing_psd(original))
            .is_some_and(|psd| psd == path);
        if !backed {
            return;
        }
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

    /// After a save landed on a gallery sidecar: drop the photo's cached
    /// thumbnail so the gallery shows the edit, and mark it edited.
    /// Returns whether the path was a sidecar (which stays out of the
    /// recents — the original is what the user thinks of as the file).
    pub(super) fn post_save_backing(&mut self, path: &Path) -> bool {
        let original = self
            .doc
            .as_ref()
            .and_then(|d| self.library.edit_backings.get(&d.id))
            .filter(|original| backing_psd(original).as_deref() == Some(path))
            .cloned();
        let Some(original) = original else {
            return false;
        };
        self.library.thumbs.remove(&original);
        let mtime = mtime_secs(path);
        for section in &mut self.library.sections {
            for entry in &mut section.entries {
                if entry.path == original {
                    entry.edited = true;
                    entry.mtime = mtime;
                }
            }
        }
        true
    }

    /// Forget the gallery backing of a closed document.
    pub(super) fn forget_backing(&mut self, id: schist_core::DocumentId) {
        self.library.edit_backings.remove(&id);
    }

    /// Record a file in the recents list, newest first.
    pub(super) fn note_recent(&mut self, path: &Path) {
        self.library.recents.retain(|p| p != path);
        self.library.recents.insert(0, path.to_path_buf());
        self.library.recents.truncate(RECENTS_KEPT);
        self.library.save();
    }

    /// Set the tray slider's cell size.
    pub fn set_gallery_thumb_px(&mut self, value: f32) {
        self.library.thumb_px = value.clamp(80.0, 240.0);
        self.library.save();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sidecar_lives_in_a_hidden_directory_beside_the_photo() {
        // The sidecar carries the extension of the original in its name,
        // so `a.jpg` and `a.png` in one folder never share an edit.
        assert_eq!(
            backing_psd(Path::new("/photos/trip/a.jpg")),
            Some(PathBuf::from("/photos/trip/.schist/a.jpg.psd"))
        );
        assert_eq!(
            backing_psd(Path::new("/photos/trip/a.png")),
            Some(PathBuf::from("/photos/trip/.schist/a.png.psd"))
        );
    }

    #[test]
    fn scanning_skips_hidden_directories_and_unknown_files() {
        let root = std::env::temp_dir().join(format!("schist-lib-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("album/.schist")).unwrap();
        std::fs::write(root.join("album/one.png"), b"x").unwrap();
        std::fs::write(root.join("album/two.txt"), b"x").unwrap();
        // A sidecar PSD must not list as a photo of its own.
        std::fs::write(root.join("album/.schist/one.png.psd"), b"x").unwrap();
        let sections = scan_folders(
            std::slice::from_ref(&root),
            &["png".to_string(), "psd".to_string()],
        );
        let all: Vec<_> = sections
            .iter()
            .flat_map(|s| s.entries.iter().map(|e| e.path.clone()))
            .collect();
        assert_eq!(all, vec![root.join("album/one.png")]);
        // And the photo with a sidecar knows it has been edited.
        assert!(sections[0].entries[0].edited);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn thumb_cache_keys_change_with_the_file() {
        let a = thumb_cache_path(Path::new("/p/a.jpg"), 100).unwrap();
        let same = thumb_cache_path(Path::new("/p/a.jpg"), 100).unwrap();
        let touched = thumb_cache_path(Path::new("/p/a.jpg"), 101).unwrap();
        let other = thumb_cache_path(Path::new("/p/b.jpg"), 100).unwrap();
        assert_eq!(a, same);
        assert_ne!(a, touched);
        assert_ne!(a, other);
    }
}
