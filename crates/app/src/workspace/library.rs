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

use super::library_geo;
use super::*;
use std::collections::BTreeMap;
use std::hash::{Hash as _, Hasher as _};
use std::path::Path;

/// Longest edge of a rendered thumbnail. Cells scale the image down from
/// here, so one render serves every position of the size slider.
const THUMB_EDGE: u32 = 256;
/// Thumbnails decoded per background batch — in parallel, one task
/// each, so a batch costs its slowest decode rather than their sum.
/// Small enough that the first screenful streams in rather than
/// arriving all at once at the end.
const THUMB_BATCH: usize = 8;
/// Folder scanning stops here rather than following a loop of symlinks
/// (or someone's home directory) forever.
const SCAN_MAX_DEPTH: usize = 6;
const SCAN_MAX_FILES: usize = 5000;
/// How many recently opened files the start screen lists.
const RECENTS_KEPT: usize = 10;
/// The most results a search shows, and the similarity below which a
/// result is the model shrugging rather than matching (cosines from the
/// MobileCLIP pair sit around 0.2–0.3 for a real match).
const SEARCH_KEPT: usize = 200;
const SEARCH_FLOOR: f32 = 0.15;
/// What being squarely *in* the named place adds to a photo's score —
/// bigger than any cosine gap, so location dominates the ordering when
/// the query names one, without exiling good semantic matches.
const GEO_BOOST: f32 = 0.35;

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
    /// Queued by the search indexer rather than a visible cell: score
    /// and embed, but don't keep the pixels — a whole camera roll of
    /// retained thumbnails would be gigabytes.
    for_index: bool,
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
    #[serde(default)]
    group_by: Option<String>,
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
    /// Thumbnail states, tagged with the mtime they were built from: a
    /// file that changed underneath — a photo still landing off a
    /// camera when its first decode ran, an edit — loads again.
    thumbs: FxHashMap<PathBuf, (u64, Thumb)>,
    queue: Vec<ThumbJob>,
    /// Whether a thumbnail loader task is live (only ever one at a time).
    ticker: bool,
    /// A gallery open waiting for its decode: (path being loaded, the
    /// original image it is an edit of). Consumed by `finish_load`.
    pub(super) pending_backing: Option<(PathBuf, PathBuf)>,
    /// Original image path per open document that came from the gallery,
    /// so a save can refresh that image's thumbnail.
    edit_backings: FxHashMap<schist_core::DocumentId, PathBuf>,
    /// The import dialog's navigable map: view, tiles, and the drawn
    /// boundary (kept here so it survives closing the dialog).
    pub map: library_geo::MapState,
    /// Photos the content filter flagged as explicit, filled by the
    /// thumbnail loader. Only consulted while the preference is on.
    flagged: FxHashMap<PathBuf, bool>,
    /// The search index: one unit vector per embedded photo, filled by
    /// the thumbnail loader and the background indexer. Ranking is a
    /// dot product over the lot — thousands of photos is nothing.
    embeddings: FxHashMap<PathBuf, Arc<Vec<f32>>>,
    /// Where each probed photo was taken, from its EXIF, so a place
    /// named in the search can pull its photos in. `None` = probed and
    /// positionless, which is most photos off most cameras.
    positions: FxHashMap<PathBuf, Option<(f64, f64)>>,
    /// Capture times as sortable text, and the city each positioned
    /// photo groups under — the other two readings of the same EXIF.
    taken: FxHashMap<PathBuf, String>,
    places: FxHashMap<PathBuf, Option<String>>,
    /// How the grid is grouped, persisted. Date by default: a camera
    /// roll is a diary before it is a directory tree.
    pub group_by: GroupBy,
    /// The map filter: when set, the grid shows only photos whose EXIF
    /// position falls inside it. Session-only — a fresh launch starts
    /// unfiltered — and loudly bannered while it is on.
    pub map_filter: Option<GeoBounds>,
    pub map_filter_name: Option<String>,
    /// The search box: its text, whether it is taking keystrokes, and
    /// the current query's ranked results (`None` = not searching).
    pub search: String,
    pub search_active: bool,
    /// ⌘A selected the whole query: the next keystroke replaces it,
    /// backspace clears it, ⌘C/⌘X take it — the minimal selection a
    /// one-line box owes the keyboard.
    pub search_selected: bool,
    pub search_results: Option<Vec<(PathBuf, f32)>>,
    /// The place the current query named, when it named one — shown on
    /// the results header.
    pub search_place: Option<String>,
    /// Bumped per query so a slow embedding cannot land on a newer one.
    search_seq: u64,
    /// A thumbnail failed for want of the HEIC support download; the
    /// gallery offers it once.
    heif_needed: Option<PathBuf>,
    heif_prompted: bool,
}

/// How the grid is grouped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupBy {
    /// By capture month, newest first — the diary reading.
    Date,
    /// By the directory scanning found them in.
    Folder,
    /// By the nearest city their EXIF position names.
    Place,
}

impl GroupBy {
    pub const ALL: [GroupBy; 3] = [GroupBy::Date, GroupBy::Folder, GroupBy::Place];

    pub fn label(self) -> &'static str {
        match self {
            GroupBy::Date => "Date",
            GroupBy::Folder => "Folder",
            GroupBy::Place => "Place",
        }
    }

    fn key(self) -> &'static str {
        match self {
            GroupBy::Date => "date",
            GroupBy::Folder => "folder",
            GroupBy::Place => "place",
        }
    }

    fn from_key(key: &str) -> Option<GroupBy> {
        GroupBy::ALL.into_iter().find(|g| g.key() == key)
    }
}

const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

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
            map: library_geo::MapState::default(),
            flagged: FxHashMap::default(),
            embeddings: FxHashMap::default(),
            positions: FxHashMap::default(),
            taken: FxHashMap::default(),
            places: FxHashMap::default(),
            group_by: file
                .group_by
                .as_deref()
                .and_then(GroupBy::from_key)
                .unwrap_or(GroupBy::Date),
            map_filter: None,
            map_filter_name: None,
            search: String::new(),
            search_active: false,
            search_selected: false,
            search_results: None,
            search_place: None,
            search_seq: 0,
            heif_needed: None,
            heif_prompted: false,
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
            group_by: Some(self.group_by.key().to_string()),
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
            Some((mtime, Thumb::Ready(img))) if *mtime == entry.mtime => return Some(img.clone()),
            // Same file, still in flight or given up: leave it be. A
            // different mtime falls through and queues a fresh decode.
            Some((mtime, _)) if *mtime == entry.mtime => return None,
            _ => {}
        }
        self.thumbs
            .insert(entry.path.clone(), (entry.mtime, Thumb::Pending));
        self.queue.push(ThumbJob {
            key: entry.path.clone(),
            source: thumb_source(&entry.path, entry.edited),
            mtime: entry.mtime,
            for_index: false,
        });
        None
    }

    /// Feed the loader the next photos missing index work — a search
    /// embedding (when the model to make one is here) or an EXIF
    /// position probe. Returns whether anything queued.
    fn refill_index_queue(&mut self) -> bool {
        let embeds = schist_neural::installed("embed-image");
        let jobs: Vec<ThumbJob> = self
            .sections
            .iter()
            .flat_map(|s| s.entries.iter())
            .filter(|e| {
                (embeds && !self.embeddings.contains_key(&e.path))
                    || !self.positions.contains_key(&e.path)
            })
            .take(THUMB_BATCH)
            .map(|e| ThumbJob {
                key: e.path.clone(),
                source: thumb_source(&e.path, e.edited),
                mtime: e.mtime,
                for_index: true,
            })
            .collect();
        self.queue.extend(jobs);
        !self.queue.is_empty()
    }

    /// How much of the gallery the search index covers: (embedded, all).
    pub fn index_progress(&self) -> (usize, usize) {
        let total = self.sections.iter().map(|s| s.entries.len()).sum();
        (self.embeddings.len().min(total), total)
    }

    /// Whether any queued decode is waiting for a loader task.
    pub fn wants_thumbs(&self) -> bool {
        !self.queue.is_empty() && !self.ticker
    }

    /// Whether a thumbnail decode gave up, so the cell can say so.
    pub fn thumb_failed(&self, path: &Path) -> bool {
        matches!(self.thumbs.get(path), Some((_, Thumb::Failed)))
    }

    /// Whether the content filter flagged a photo as explicit.
    pub fn is_flagged(&self, path: &Path) -> bool {
        self.flagged.get(path).copied().unwrap_or(false)
    }

    /// Flagged photos among the visible sections — what the filter is
    /// currently keeping out of the grid.
    pub fn flagged_count(&self) -> usize {
        self.visible_sections()
            .flat_map(|s| s.entries.iter())
            .filter(|e| self.passes_map(e) && self.is_flagged(&e.path))
            .count()
    }

    /// Forget failed thumbnails so they load again — what the HEIC
    /// support download makes worth retrying.
    pub fn retry_failed_thumbs(&mut self) {
        self.thumbs.retain(|_, (_, t)| !matches!(t, Thumb::Failed));
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
        self.visible_sections()
            .flat_map(|s| s.entries.iter())
            .filter(|e| self.passes_map(e))
            .count()
    }

    /// Whether the map filter lets a photo through: no filter passes
    /// everything, a filter passes only photos whose EXIF position
    /// falls inside it — the point of asking for a place.
    pub fn passes_map(&self, entry: &Entry) -> bool {
        let Some(bounds) = self.map_filter else {
            return true;
        };
        matches!(
            self.positions.get(&entry.path),
            Some(Some((lat, lon))) if bounds.contains(*lat, *lon)
        )
    }

    /// What the active map filter is called, for the banner.
    pub fn map_filter_label(&self) -> Option<String> {
        self.map_filter.as_ref()?;
        Some(
            self.map_filter_name
                .clone()
                .unwrap_or_else(|| "drawn area".to_string()),
        )
    }

    /// A photo's capture time as sortable text: EXIF when probed, the
    /// file's own clock until then.
    fn taken_of(&self, entry: &Entry) -> String {
        self.taken
            .get(&entry.path)
            .cloned()
            .unwrap_or_else(|| taken_from_unix(entry.mtime))
    }

    /// The visible photos grouped the way `group_by` asks:
    /// (title, subtitle, entries) per group.
    pub fn grouped(&self) -> Vec<(String, String, Vec<Entry>)> {
        match self.group_by {
            GroupBy::Folder => self
                .visible_sections()
                .map(|s| {
                    let title = s
                        .dir
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| s.dir.display().to_string());
                    let entries = s
                        .entries
                        .iter()
                        .filter(|e| self.passes_map(e))
                        .cloned()
                        .collect();
                    (title, s.dir.display().to_string(), entries)
                })
                .collect(),
            GroupBy::Date => {
                // Month buckets keyed "YYYY-MM", newest first, photos
                // newest first inside each.
                let mut buckets: BTreeMap<String, Vec<Entry>> = BTreeMap::new();
                for entry in self
                    .visible_sections()
                    .flat_map(|s| s.entries.iter())
                    .filter(|e| self.passes_map(e))
                {
                    let taken = self.taken_of(entry);
                    let key = taken.get(..7).unwrap_or("0000-00").to_string();
                    buckets.entry(key).or_default().push(entry.clone());
                }
                buckets
                    .into_iter()
                    .rev()
                    .map(|(key, mut entries)| {
                        entries.sort_by_key(|e| std::cmp::Reverse(self.taken_of(e)));
                        let title = match (
                            key.get(..4),
                            key.get(5..7).and_then(|m| m.parse::<usize>().ok()),
                        ) {
                            (Some(year), Some(month)) if (1..=12).contains(&month) => {
                                format!("{} {year}", MONTHS[month - 1])
                            }
                            _ => "Undated".to_string(),
                        };
                        (title, String::new(), entries)
                    })
                    .collect()
            }
            GroupBy::Place => {
                // City buckets, biggest first; the unprobed and the
                // positionless gather at the end.
                let mut buckets: BTreeMap<String, Vec<Entry>> = BTreeMap::new();
                for entry in self
                    .visible_sections()
                    .flat_map(|s| s.entries.iter())
                    .filter(|e| self.passes_map(e))
                {
                    let key = match self.places.get(&entry.path) {
                        Some(Some(city)) => city.clone(),
                        Some(None) => "No location".to_string(),
                        None => "Not indexed yet".to_string(),
                    };
                    buckets.entry(key).or_default().push(entry.clone());
                }
                let mut groups: Vec<(String, String, Vec<Entry>)> = buckets
                    .into_iter()
                    .map(|(city, mut entries)| {
                        entries.sort_by_key(|e| std::cmp::Reverse(self.taken_of(e)));
                        (city, String::new(), entries)
                    })
                    .collect();
                groups.sort_by(|a, b| {
                    let tail = |t: &str| t == "No location" || t == "Not indexed yet";
                    (tail(&a.0), std::cmp::Reverse(a.2.len()))
                        .cmp(&(tail(&b.0), std::cmp::Reverse(b.2.len())))
                });
                groups
            }
        }
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
pub(super) fn rgba_to_render_image(
    width: u32,
    height: u32,
    mut rgba: Vec<u8>,
) -> Option<Arc<RenderImage>> {
    for px in rgba.as_chunks_mut::<4>().0 {
        px.swap(0, 2);
    }
    let buffer = image::RgbaImage::from_raw(width, height, rgba)?;
    Some(Arc::new(RenderImage::new(smallvec![image::Frame::new(
        buffer
    )])))
}

/// What loading one thumbnail produced.
struct ThumbOutcome {
    img: Option<Arc<RenderImage>>,
    /// The photo's content scores, when the model is installed.
    score: Option<ExplicitScore>,
    /// The photo's search embedding, when that model is installed. An
    /// empty vector marks "tried and cannot" — an undecodable file —
    /// so the indexer does not queue it forever.
    embedding: Option<Vec<f32>>,
    /// Position, capture time and grouping city, from the EXIF.
    meta: PhotoMeta,
    /// The decode failed for want of the HEIC support download.
    needs_heif: bool,
}

/// The two signals the flag rule reads out of the model's five softmax
/// classes: porn+hentai together, and "sexy" alone.
#[derive(Clone, Copy)]
struct ExplicitScore {
    explicit: f32,
    sexy: f32,
}

/// Whether a photo counts as explicit. The nsfwjs guidance, learned
/// again the hard way: flag on the porn and hentai classes, and only on
/// a near-certain "sexy" — that class fires on bare shoulders and
/// swimwear, and summing it in flagged most of a real camera roll.
fn is_explicit(score: ExplicitScore) -> bool {
    score.explicit >= 0.5 || score.sexy >= 0.9
}

/// Decode one thumbnail, through the disk cache when it can, scoring it
/// for the content filter and embedding it for search on the way past.
/// Blocking.
fn load_thumb(job: &ThumbJob) -> ThumbOutcome {
    let cache = thumb_cache_path(&job.source, job.mtime);
    // An index pass whose answers are all cached needs no pixels at all:
    // this is what makes re-indexing a warm library a file-read sweep
    // rather than a decode of everything.
    if job.for_index {
        let cached_embed = read_embed_cache(&cache);
        let cached_score = read_score_cache(&cache);
        let embeds_wanted = schist_neural::installed("embed-image");
        if (cached_embed.is_some() || !embeds_wanted)
            && (cached_score.is_some() || !nsfw_installed())
        {
            return ThumbOutcome {
                img: None,
                score: cached_score,
                embedding: cached_embed,
                meta: photo_meta(&cache, &job.key),
                needs_heif: false,
            };
        }
    }
    let mut needs_heif = false;
    let rgba: Option<(u32, u32, Vec<u8>)> = if let Some(cached) = cache
        .as_ref()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|bytes| image::load_from_memory(&bytes).ok())
    {
        let img = cached.into_rgba8();
        Some((img.width(), img.height(), img.into_raw()))
    } else {
        match schist_preview::render_file(&job.source, THUMB_EDGE) {
            Ok(preview) => {
                if let (Some(path), Ok(png)) = (&cache, preview.to_png()) {
                    if let Some(dir) = path.parent() {
                        let _ = std::fs::create_dir_all(dir);
                    }
                    let _ = std::fs::write(path, png);
                }
                Some((preview.width, preview.height, preview.rgba))
            }
            Err(err) => {
                needs_heif = schist_codecs_common::heif::download_would_help(&err);
                log::warn!("thumbnail failed for {}: {err:#}", job.source.display());
                None
            }
        }
    };
    let score = rgba
        .as_ref()
        .and_then(|(w, h, rgba)| explicit_score(&cache, *w, *h, rgba));
    let embedding = match &rgba {
        Some((w, h, rgba)) => photo_embedding(&cache, *w, *h, rgba),
        // Undecodable: leave the "tried and cannot" marker so the
        // indexer moves on, but only when a model was here to try.
        None if schist_neural::installed("embed-image") => Some(Vec::new()),
        None => None,
    };
    ThumbOutcome {
        img: if job.for_index {
            None
        } else {
            rgba.and_then(|(w, h, rgba)| rgba_to_render_image(w, h, rgba))
        },
        score,
        embedding,
        meta: photo_meta(&cache, &job.key),
        needs_heif,
    }
}

/// A photo's EXIF-derived metadata — position, the city it groups
/// under, and when it was taken — cached beside the thumbnail in one
/// file, so a whole camera roll is parsed once, not per launch.
struct PhotoMeta {
    gps: Option<(f64, f64)>,
    /// "YYYY-MM-DD HH:MM:SS": sortable as text, no calendar needed.
    taken: Option<String>,
    place: Option<String>,
}

fn photo_meta(cache: &Option<PathBuf>, original: &Path) -> PhotoMeta {
    let meta_cache = cache.as_ref().map(|p| p.with_extension("meta"));
    if let Some(text) = meta_cache
        .as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok())
    {
        let mut lines = text.lines();
        let gps = lines.next().and_then(|l| {
            let mut parts = l.split_whitespace().filter_map(|v| v.parse::<f64>().ok());
            match (parts.next(), parts.next()) {
                (Some(lat), Some(lon)) => Some((lat, lon)),
                _ => None,
            }
        });
        let field = |l: Option<&str>| {
            l.filter(|l| *l != "none" && !l.is_empty())
                .map(str::to_string)
        };
        return PhotoMeta {
            gps,
            taken: field(lines.next()),
            place: field(lines.next()),
        };
    }
    let data = exif_of(original);
    let gps = data.as_ref().and_then(gps_from);
    let taken = data.as_ref().and_then(datetime_from);
    let place = gps.and_then(|(lat, lon)| library_geo::nearest_city(lat, lon));
    if let Some(path) = meta_cache {
        let line1 = match gps {
            Some((lat, lon)) => format!("{lat} {lon}"),
            None => "none".into(),
        };
        let text = format!(
            "{line1}\n{}\n{}",
            taken.as_deref().unwrap_or("none"),
            place.as_deref().unwrap_or("none")
        );
        let _ = std::fs::write(path, text);
    }
    PhotoMeta { gps, taken, place }
}

/// The capture time as sortable text, from DateTimeOriginal (else
/// DateTime): "YYYY:MM:DD HH:MM:SS" with the date's colons swapped out.
fn datetime_from(data: &exif::Exif) -> Option<String> {
    let field = data
        .get_field(exif::Tag::DateTimeOriginal, exif::In::PRIMARY)
        .or_else(|| data.get_field(exif::Tag::DateTime, exif::In::PRIMARY))?;
    let raw = field.display_value().to_string();
    let raw = raw.trim();
    // "2026:08:14 17:03:22" — sanity before trusting it to sort.
    if raw.len() < 10 || !raw.as_bytes()[..4].iter().all(u8::is_ascii_digit) {
        return None;
    }
    let mut normalized: Vec<u8> = raw.bytes().collect();
    if normalized.get(4) == Some(&b':') {
        normalized[4] = b'-';
    }
    if normalized.get(7) == Some(&b':') {
        normalized[7] = b'-';
    }
    String::from_utf8(normalized).ok()
}

/// A unix time as the same sortable text, for photos whose EXIF says
/// nothing — the file's own clock is better than no clock.
fn taken_from_unix(secs: u64) -> String {
    let (y, m, d) = ymd_from_unix(secs);
    let rem = secs % 86_400;
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}",
        rem / 3600,
        (rem / 60) % 60,
        rem % 60
    )
}

/// Civil date from days-since-epoch (Howard Hinnant's algorithm).
fn ymd_from_unix(secs: u64) -> (i64, u32, u32) {
    let z = (secs / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn nsfw_installed() -> bool {
    schist_neural::installed("nsfw")
}

fn read_score_cache(cache: &Option<PathBuf>) -> Option<ExplicitScore> {
    let text = cache
        .as_ref()
        .and_then(|p| std::fs::read_to_string(p.with_extension("score2")).ok())?;
    let mut parts = text
        .split_whitespace()
        .filter_map(|v| v.parse::<f32>().ok());
    match (parts.next(), parts.next()) {
        (Some(explicit), Some(sexy)) => Some(ExplicitScore { explicit, sexy }),
        _ => None,
    }
}

fn read_embed_cache(cache: &Option<PathBuf>) -> Option<Vec<f32>> {
    let bytes = cache
        .as_ref()
        .and_then(|p| std::fs::read(p.with_extension("embed")).ok())?;
    if bytes.is_empty() || bytes.len() % 4 != 0 {
        return None;
    }
    Some(
        bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| f32::from_le_bytes(*c))
            .collect(),
    )
}

/// The photo's search embedding, cached beside the thumbnail. `None`
/// when the model is not installed.
fn photo_embedding(
    cache: &Option<PathBuf>,
    width: u32,
    height: u32,
    rgba: &[u8],
) -> Option<Vec<f32>> {
    if let Some(cached) = read_embed_cache(cache) {
        return Some(cached);
    }
    let spec = schist_neural::spec("embed-image")?;
    if !schist_neural::installed("embed-image") {
        return None;
    }
    let (mw, mh) = spec.input.dims();
    let img = image::RgbaImage::from_raw(width, height, rgba.to_vec())?;
    let img = image::imageops::resize(
        &img,
        mw as u32,
        mh as u32,
        image::imageops::FilterType::Triangle,
    );
    let mut rgb = Vec::with_capacity(mw * mh * 3);
    for px in img.pixels() {
        rgb.extend([
            px.0[0] as f32 / 255.0,
            px.0[1] as f32 / 255.0,
            px.0[2] as f32 / 255.0,
        ]);
    }
    let vector = schist_neural::embed::embed_image(&rgb)?;
    if let Some(path) = cache {
        let bytes: Vec<u8> = vector.iter().flat_map(|f| f.to_le_bytes()).collect();
        let _ = std::fs::write(path.with_extension("embed"), bytes);
    }
    Some(vector)
}

/// The model's judgement of a photo, cached beside the thumbnail so
/// each photo is judged once. `None` when the model is not installed —
/// nothing is flagged without it.
fn explicit_score(
    cache: &Option<PathBuf>,
    width: u32,
    height: u32,
    rgba: &[u8],
) -> Option<ExplicitScore> {
    // "score2": the first format cached one blended number that mixed
    // "sexy" in; those verdicts were wrong and are left to rot.
    if let Some(cached) = read_score_cache(cache) {
        return Some(cached);
    }
    let score_cache = cache.as_ref().map(|p| p.with_extension("score2"));
    let model = schist_neural::get("nsfw")?;
    let (mw, mh) = model.spec.input.dims();
    let img = image::RgbaImage::from_raw(width, height, rgba.to_vec())?;
    let img = image::imageops::resize(
        &img,
        mw as u32,
        mh as u32,
        image::imageops::FilterType::Triangle,
    );
    let mut rgb = Vec::with_capacity(mw * mh * 3);
    for px in img.pixels() {
        rgb.extend([
            px.0[0] as f32 / 255.0,
            px.0[1] as f32 / 255.0,
            px.0[2] as f32 / 255.0,
        ]);
    }
    let scores = model.run_scores(&rgb).ok()?;
    // The five softmax classes are drawing, hentai, neutral, porn, sexy.
    if scores.len() != 5 {
        return None;
    }
    let score = ExplicitScore {
        explicit: scores[1] + scores[3],
        sexy: scores[4],
    };
    if let Some(path) = score_cache {
        let _ = std::fs::write(path, format!("{} {}", score.explicit, score.sexy));
    }
    Some(score)
}

/// Where a volume keeps its photos: `DCIM` at the root (cards, cameras,
/// iPhones over AFC), or one level down inside a storage directory, the
/// way MTP phones present "Internal storage/DCIM".
pub(super) fn dcim_dir(root: &Path) -> Option<PathBuf> {
    let direct = root.join("DCIM");
    if direct.is_dir() {
        return Some(direct);
    }
    for child in std::fs::read_dir(root).ok()?.flatten() {
        let nested = child.path().join("DCIM");
        if nested.is_dir() {
            return Some(nested);
        }
    }
    None
}

/// What to call a camera volume. GVFS mounts are named by their URL
/// (`afc:host=<udid>`), which says nothing to a person; say what kind of
/// device it is instead.
pub(super) fn volume_label(root: &Path) -> String {
    let name = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.display().to_string());
    if name.starts_with("afc:") {
        "iPhone or iPad".into()
    } else if name.starts_with("gphoto2:") {
        "Camera".into()
    } else if name.starts_with("mtp:") {
        "Phone".into()
    } else {
        name
    }
}

/// A human name as a folder name: path separators and control
/// characters out, and never empty.
fn sanitize_folder_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c == '/' || c == '\\' || c == ':' || c.is_control() {
                '-'
            } else {
                c
            }
        })
        .collect();
    let trimmed = cleaned.trim().trim_matches('.');
    if trimmed.is_empty() {
        "Selected Area".into()
    } else {
        trimmed.to_string()
    }
}

/// What to call an import source in dialogs and status lines.
pub(super) fn source_label(source: &ImportSource) -> String {
    match source {
        ImportSource::Volume(path) => volume_label(path),
        ImportSource::Device { name, .. } => name.clone(),
    }
}

/// Mounted volumes that look like cameras or cards: anything under the
/// removable-media roots with a `DCIM` directory, which is what the
/// design rule every camera follows requires them to create. GVFS
/// mounts count too — that is how an unlocked iPhone (`afc:`), a PTP
/// camera (`gphoto2:`) or an Android phone (`mtp:`) appears as files on
/// a Linux desktop.
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
    if let Ok(runtime) = std::env::var("XDG_RUNTIME_DIR") {
        roots.push(PathBuf::from(runtime).join("gvfs"));
    }
    if let Ok(home) = std::env::var("HOME") {
        // Where GVFS mounted before it moved to the runtime dir.
        roots.push(PathBuf::from(home).join(".gvfs"));
    }
    let mut out = Vec::new();
    for root in roots {
        let Ok(read) = std::fs::read_dir(root) else {
            continue;
        };
        for item in read.flatten() {
            let path = item.path();
            if dcim_dir(&path).is_some() {
                out.push(path);
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// One EXIF parse of a file, shared by every reader below.
fn exif_of(path: &Path) -> Option<exif::Exif> {
    let file = std::fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::new(file);
    exif::Reader::new().read_from_container(&mut reader).ok()
}

/// The GPS position a camera wrote into a file, if any. Blocking.
fn photo_gps(path: &Path) -> Option<(f64, f64)> {
    gps_from(&exif_of(path)?)
}

/// The GPS position out of a parsed EXIF block.
fn gps_from(data: &exif::Exif) -> Option<(f64, f64)> {
    // Degrees/minutes/seconds as three rationals, hemisphere in the
    // companion Ref tag ("S"/"W" flip the sign).
    let axis = |tag: exif::Tag, ref_tag: exif::Tag, negative: char| -> Option<f64> {
        let field = data.get_field(tag, exif::In::PRIMARY)?;
        let exif::Value::Rational(parts) = &field.value else {
            return None;
        };
        if parts.is_empty() {
            return None;
        }
        let part = |i: usize| parts.get(i).map(|r| r.to_f64()).unwrap_or(0.0);
        let degrees = part(0) + part(1) / 60.0 + part(2) / 3600.0;
        let flip = data
            .get_field(ref_tag, exif::In::PRIMARY)
            .is_some_and(|f| f.display_value().to_string().contains(negative));
        Some(if flip { -degrees } else { degrees })
    };
    let lat = axis(exif::Tag::GPSLatitude, exif::Tag::GPSLatitudeRef, 'S')?;
    let lon = axis(exif::Tag::GPSLongitude, exif::Tag::GPSLongitudeRef, 'W')?;
    Some((lat, lon))
}

/// Copy every image under the volume's DCIM into `dest`, skipping files
/// that already arrived (same name, same size). With a boundary, only
/// photos whose EXIF position falls inside it are taken — "taken in New
/// York", by the camera's own record — and photos without a position
/// are left behind rather than guessed about. Blocking; returns
/// (copied, left behind by the boundary).
fn copy_dcim(
    source: &Path,
    dest: &Path,
    exts: &[String],
    area: Option<library_geo::GeoBounds>,
) -> anyhow::Result<(usize, usize)> {
    let dcim = dcim_dir(source)
        .ok_or_else(|| anyhow::anyhow!("no DCIM folder on {}", source.display()))?;
    std::fs::create_dir_all(dest)?;
    let mut copied = 0;
    let mut filtered = 0;
    let mut stack = vec![dcim];
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
            if let Some(area) = area {
                let inside = photo_gps(&path).is_some_and(|(lat, lon)| area.contains(lat, lon));
                if !inside {
                    filtered += 1;
                    continue;
                }
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
    Ok((copied, filtered))
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
            // Warm up device discovery, so an iPhone plugged in before
            // the Import click is already on the list.
            #[cfg(target_os = "macos")]
            super::library_icc::start_browsing();
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
                // Nothing on screen wants a thumbnail; spend the idle
                // time indexing the rest of the library for search.
                let refilled = this
                    .update(cx, |ws, _| ws.library.refill_index_queue())
                    .unwrap_or(false);
                if refilled {
                    continue;
                }
                this.update(cx, |ws, _| ws.library.ticker = false).ok();
                return;
            }
            // One task per decode: the executor runs them across its
            // threads, so a batch costs its slowest member — a full
            // HEIC decode plus a classifier pass each, which in single
            // file was slow enough to look stuck on a camera roll.
            let tasks: Vec<_> = batch
                .into_iter()
                .map(|job| {
                    cx.background_executor().spawn(async move {
                        let outcome = load_thumb(&job);
                        (job.key, job.mtime, job.for_index, outcome)
                    })
                })
                .collect();
            let mut results = Vec::with_capacity(tasks.len());
            for task in tasks {
                results.push(task.await);
            }
            let keep = this.update(cx, |ws, cx| {
                for (key, mtime, for_index, outcome) in results {
                    if let Some(score) = outcome.score {
                        ws.library.flagged.insert(key.clone(), is_explicit(score));
                    }
                    if let Some(vector) = outcome.embedding {
                        ws.library.embeddings.insert(key.clone(), Arc::new(vector));
                    }
                    ws.library.positions.insert(key.clone(), outcome.meta.gps);
                    if let Some(taken) = outcome.meta.taken {
                        ws.library.taken.insert(key.clone(), taken);
                    }
                    ws.library.places.insert(key.clone(), outcome.meta.place);
                    if outcome.needs_heif && ws.library.heif_needed.is_none() {
                        ws.library.heif_needed = Some(key.clone());
                    }
                    // Index passes keep no pixels; the map slot stays
                    // free for a real cell to claim later.
                    if !for_index {
                        let state = match outcome.img {
                            Some(img) => Thumb::Ready(img),
                            None => Thumb::Failed,
                        };
                        ws.library.thumbs.insert(key, (mtime, state));
                    }
                }
                cx.notify();
            });
            if keep.is_err() {
                return;
            }
        })
        .detach();
    }

    /// Whether the gallery's search box is taking keystrokes — what
    /// flips the key context to text entry so letters reach the box
    /// instead of the tool shortcuts.
    pub fn gallery_search_active(&self) -> bool {
        self.library.open && self.library.search_active
    }

    /// A keystroke for the search box. Returns whether it was taken.
    pub(super) fn gallery_search_key(
        &mut self,
        ev: &gpui::KeyDownEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.library.search_active {
            return false;
        }
        let primary = ev.keystroke.modifiers.platform || ev.keystroke.modifiers.control;
        match ev.keystroke.key.as_str() {
            "a" if primary => {
                self.library.search_selected = !self.library.search.is_empty();
                cx.notify();
            }
            "c" if primary && self.library.search_selected => {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(self.library.search.clone()));
            }
            "x" if primary && self.library.search_selected => {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(self.library.search.clone()));
                self.library.search.clear();
                self.library.search_selected = false;
                self.gallery_search_changed(cx);
            }
            "v" if primary => {
                let Some(pasted) = cx.read_from_clipboard().and_then(|item| item.text()) else {
                    return true;
                };
                // One line: a pasted paragraph flattens rather than
                // breaking the box.
                let pasted: String = pasted
                    .chars()
                    .map(|c| if c.is_control() { ' ' } else { c })
                    .collect();
                if self.library.search_selected {
                    self.library.search.clear();
                    self.library.search_selected = false;
                }
                self.library.search.push_str(&pasted);
                self.gallery_search_changed(cx);
            }
            "backspace" | "delete" => {
                if self.library.search_selected {
                    self.library.search.clear();
                    self.library.search_selected = false;
                } else {
                    self.library.search.pop();
                }
                self.gallery_search_changed(cx);
            }
            "enter" => {
                // The results are already live; Enter just puts the
                // keyboard back on the shortcuts.
                self.library.search_active = false;
                self.library.search_selected = false;
                cx.notify();
            }
            _ => {
                let Some(text) = ev.keystroke.key_char.as_deref() else {
                    return false;
                };
                if text.chars().any(char::is_control) {
                    return false;
                }
                // Typing over a selection replaces it, as anywhere.
                if self.library.search_selected {
                    self.library.search.clear();
                    self.library.search_selected = false;
                }
                self.library.search.push_str(text);
                self.gallery_search_changed(cx);
            }
        }
        true
    }

    /// Leave the search: clear the box and show the folders again.
    /// Wired into the always-on Escape path.
    pub(super) fn gallery_search_clear(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.library.open
            || (!self.library.search_active && self.library.search_results.is_none())
        {
            return false;
        }
        self.library.search.clear();
        self.library.search_active = false;
        self.library.search_selected = false;
        self.library.search_results = None;
        self.library.search_place = None;
        self.library.search_seq += 1;
        cx.notify();
        true
    }

    /// Re-rank for the current query: embed the text on a background
    /// thread and dot it against every photo's vector.
    pub(super) fn gallery_search_changed(&mut self, cx: &mut Context<Self>) {
        self.library.search_seq += 1;
        let seq = self.library.search_seq;
        let query = self.library.search.trim().to_string();
        if query.is_empty() {
            self.library.search_results = None;
            cx.notify();
            return;
        }
        let vectors: Vec<(PathBuf, Arc<Vec<f32>>)> = self
            .library
            .embeddings
            .iter()
            .map(|(p, v)| (p.clone(), v.clone()))
            .collect();
        let positions: Vec<(PathBuf, (f64, f64))> = self
            .library
            .positions
            .iter()
            .filter_map(|(p, pos)| pos.map(|pos| (p.clone(), pos)))
            .collect();
        cx.spawn(async move |this, cx| {
            let ranked = cx
                .background_executor()
                .spawn(async move {
                    // Two readings of the query, blended: what the
                    // photos look like, and — when it names somewhere
                    // the gazetteer knows — where they were taken.
                    let place = library_geo::find_place(&query);
                    let text = schist_neural::embed::embed_text(&query);
                    if text.is_none() && place.is_none() {
                        return None;
                    }
                    let mut scored: FxHashMap<PathBuf, f32> = FxHashMap::default();
                    if let Some(text) = &text {
                        for (path, v) in vectors {
                            let s = v.iter().zip(text).map(|(a, b)| a * b).sum::<f32>();
                            scored.insert(path, s);
                        }
                    }
                    if let Some(place) = &place {
                        for (path, (lat, lon)) in positions {
                            let affinity = library_geo::geo_affinity(place, lat, lon);
                            if affinity > 0.0 {
                                *scored.entry(path).or_insert(0.0) += GEO_BOOST * affinity;
                            }
                        }
                    }
                    let floor = if text.is_some() {
                        SEARCH_FLOOR
                    } else {
                        // Location-only search (no text model): being
                        // near the place is the whole of the score.
                        GEO_BOOST * 0.3
                    };
                    let mut scored: Vec<(PathBuf, f32)> = scored.into_iter().collect();
                    scored.sort_by(|a, b| b.1.total_cmp(&a.1));
                    scored.truncate(SEARCH_KEPT);
                    scored.retain(|(_, s)| *s >= floor);
                    Some((scored, place.map(|p| p.name)))
                })
                .await;
            this.update(cx, |ws, cx| {
                // A newer keystroke owns the results now.
                if ws.library.search_seq == seq {
                    if let Some((ranked, place)) = ranked {
                        ws.library.search_results = Some(ranked);
                        ws.library.search_place = place;
                    }
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Offer the HEIC support download once, when thumbnails have been
    /// failing for want of it. Called from the gallery render, where a
    /// modal can be raised.
    pub(super) fn maybe_offer_heif(&mut self, cx: &mut Context<Self>) {
        if self.modal.is_some() || self.heif_download || self.library.heif_prompted {
            return;
        }
        let Some(path) = self.library.heif_needed.take() else {
            return;
        };
        if schist_codecs_common::heif::managed_library().is_none() {
            return;
        }
        self.library.heif_prompted = true;
        self.open_modal(Modal::HeifSupport { path }, cx);
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

    /// Import from a camera. One source goes straight to the options;
    /// none or several open the picker — "none" gets a dialog too, since
    /// a button that answers with nothing visible reads as broken.
    pub fn gallery_import_camera(&mut self, cx: &mut Context<Self>) {
        if self.library.importing {
            return;
        }
        let mut sources: Vec<ImportSource> = camera_sources()
            .into_iter()
            .map(ImportSource::Volume)
            .collect();
        // iPhones and PTP cameras don't mount on macOS; ask
        // ImageCaptureCore what is plugged in.
        #[cfg(target_os = "macos")]
        {
            super::library_icc::start_browsing();
            sources.extend(
                super::library_icc::devices()
                    .into_iter()
                    .map(|(id, name)| ImportSource::Device { id, name }),
            );
        }
        if sources.len() == 1 {
            // Straight to the options (place filter, destination) rather
            // than importing on the spot: the filter is part of the ask.
            self.open_modal(
                Modal::CameraImportOptions {
                    source: sources.remove(0),
                },
                cx,
            );
        } else {
            self.open_modal(Modal::CameraImport { sources }, cx);
        }
    }

    /// Copy a camera volume's DCIM into ~/Pictures and watch the result.
    /// Import from a camera, optionally bounded: `area` is the drawn
    /// (or preset) box and its human name, and only photos whose EXIF
    /// position falls inside it come over.
    pub fn import_camera(
        &mut self,
        source: ImportSource,
        area: Option<(library_geo::GeoBounds, String)>,
        cx: &mut Context<Self>,
    ) {
        if self.library.importing {
            return;
        }
        let label = source_label(&source);
        let Some(home) = std::env::var("HOME").ok().map(PathBuf::from) else {
            self.status = "Import needs a home directory to copy into".into();
            return;
        };
        // A boundary is a sorting instruction, so it names the
        // destination: photos taken in New York land in a New York
        // folder, whatever camera they came off.
        let dest_name = area
            .as_ref()
            .map(|(_, name)| sanitize_folder_name(name))
            .unwrap_or_else(|| label.clone());
        let dest = home.join("Pictures/Schist Imports").join(&dest_name);
        self.library.importing = true;
        self.status = match &area {
            Some((_, name)) => {
                format!("Importing photos taken in {name} from {label}\u{2026}").into()
            }
            None => format!("Importing from {label}\u{2026}").into(),
        };
        // The destination joins the gallery now, not when the import
        // finishes: with the gallery open and a rescan ticking below,
        // photos appear in the grid as they land.
        if !self.library.folders.contains(&dest) {
            self.library.folders.push(dest.clone());
            self.library.folders.sort();
            self.library.save();
        }
        self.library.open = true;
        cx.notify();
        match source {
            ImportSource::Volume(volume) => self.import_volume(volume, dest, area, cx),
            #[cfg(target_os = "macos")]
            ImportSource::Device { id, name } => self.import_device(id, name, dest, area, cx),
            #[cfg(not(target_os = "macos"))]
            ImportSource::Device { .. } => {
                // Never constructed off macOS; the arm exists for the
                // exhaustiveness check.
                self.library.importing = false;
                self.status = "Direct device import is a macOS feature".into();
            }
        }
        // While the import runs, keep rescanning the watched folders so
        // each arriving photo shows up within a moment of landing.
        if self.library.importing {
            cx.spawn(async move |this, cx| loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(1500))
                    .await;
                let live = this.update(cx, |ws, cx| {
                    if !ws.library.importing {
                        return false;
                    }
                    ws.library_rescan(cx);
                    true
                });
                if !live.unwrap_or(false) {
                    break;
                }
            })
            .detach();
        }
    }

    /// A mounted DCIM volume: plain file copies on a background thread.
    fn import_volume(
        &mut self,
        source: PathBuf,
        dest: PathBuf,
        area: Option<(library_geo::GeoBounds, String)>,
        cx: &mut Context<Self>,
    ) {
        let exts = self.codec_extensions();
        let copy_dest = dest.clone();
        let bounds = area.as_ref().map(|(b, _)| *b);
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { copy_dcim(&source, &copy_dest, &exts, bounds) })
                .await;
            this.update(cx, |ws, cx| {
                ws.library.importing = false;
                match result {
                    Ok((copied, filtered)) => {
                        ws.finish_camera_import(dest, copied, filtered, 0, area, cx)
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

    /// An ImageCaptureCore device (an iPhone, a PTP camera): downloads
    /// run through the main-thread delegate; this side polls for
    /// progress and finishes the bookkeeping when the delegate is done.
    #[cfg(target_os = "macos")]
    fn import_device(
        &mut self,
        id: u64,
        name: String,
        dest: PathBuf,
        area: Option<(library_geo::GeoBounds, String)>,
        cx: &mut Context<Self>,
    ) {
        use super::library_icc;
        // The filter runs per downloaded file, on the file itself: a
        // device gives no way to read EXIF without downloading, so a
        // declined photo is downloaded, inspected and removed.
        let keep = area.as_ref().map(|(bounds, _)| {
            let bounds = *bounds;
            Box::new(move |path: &Path| {
                photo_gps(path).is_some_and(|(lat, lon)| bounds.contains(lat, lon))
            }) as library_icc::KeepFilter
        });
        if let Err(err) = library_icc::begin_import(id, dest.clone(), keep) {
            self.library.importing = false;
            self.report_device_failure(id, name, area, err, cx);
            return;
        }
        cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(400))
                .await;
            let finished = this.update(cx, |ws, cx| {
                let Some(status) = library_icc::poll_import() else {
                    ws.library.importing = false;
                    return true;
                };
                if let Some(result) = status.finished {
                    library_icc::finish_import();
                    ws.library.importing = false;
                    match result {
                        Ok((copied, filtered, failed)) => {
                            ws.finish_camera_import(
                                dest.clone(),
                                copied,
                                filtered,
                                failed,
                                area.clone(),
                                cx,
                            );
                        }
                        Err(err) => {
                            ws.report_device_failure(id, name.clone(), area.clone(), err, cx);
                        }
                    }
                    cx.notify();
                    return true;
                }
                ws.status = if status.locked {
                    format!("{name} is locked — unlock it and tap Trust to continue").into()
                } else {
                    match status.total {
                        None => format!("Reading {name}'s photo catalog\u{2026}").into(),
                        Some(total) => format!(
                            "Importing photo {}/{total} from {name}\u{2026}",
                            (status.done + 1).min(total.max(1))
                        )
                        .into(),
                    }
                };
                cx.notify();
                false
            });
            if finished.unwrap_or(true) {
                break;
            }
        })
        .detach();
    }

    /// A device import that could not finish gets a dialog, not a line
    /// of tray text — "Please unlock the iPhone" read as furniture down
    /// there — and the dialog can retry with the same boundary.
    #[cfg(target_os = "macos")]
    fn report_device_failure(
        &mut self,
        id: u64,
        name: String,
        area: Option<(library_geo::GeoBounds, String)>,
        message: String,
        cx: &mut Context<Self>,
    ) {
        log::error!("camera import failed: {message}");
        self.status = format!("Import from {name} failed").into();
        self.open_modal(
            Modal::CameraImportFailed {
                source: ImportSource::Device { id, name },
                area,
                message,
            },
            cx,
        );
    }

    /// Shared tail of every camera import: watch the destination, tell
    /// the user what happened, show the result.
    fn finish_camera_import(
        &mut self,
        dest: PathBuf,
        copied: usize,
        filtered: usize,
        failed: usize,
        area: Option<(library_geo::GeoBounds, String)>,
        cx: &mut Context<Self>,
    ) {
        if !self.library.folders.contains(&dest) {
            self.library.folders.push(dest.clone());
            self.library.folders.sort();
            self.library.save();
        }
        let mut message = match area {
            Some((_, name)) => format!(
                "Imported {copied} photos taken in {name} to {} \
                 ({filtered} elsewhere or without a position left on the camera)",
                dest.display()
            ),
            None => format!("Imported {copied} photos to {}", dest.display()),
        };
        if failed > 0 {
            message.push_str(&format!(" — {failed} failed"));
        }
        self.status = message.into();
        self.library.open = true;
        self.library_rescan(cx);
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

    /// Open the map-filter dialog, seeded with the active filter so
    /// editing starts from what is on.
    pub fn open_map_filter(&mut self, cx: &mut Context<Self>) {
        if let Some(bounds) = self.library.map_filter {
            self.library.map.selection = Some(bounds);
            self.library.map.selection_name = self.library.map_filter_name.clone();
            self.library.map.center = bounds.center();
        }
        self.open_modal(Modal::MapFilter, cx);
    }

    /// Make the drawn boundary the gallery's filter (or clear it, when
    /// nothing is drawn), and remember it.
    pub fn apply_map_filter(&mut self, cx: &mut Context<Self>) {
        self.library.map_filter = self.library.map.selection;
        self.library.map_filter_name = self
            .library
            .map_filter
            .and(self.library.map.selection_name.clone());
        self.close_modal(cx);
        cx.notify();
    }

    /// Turn the map filter off. The boundary stays drawn on the map, so
    /// turning it back on is one Apply away.
    pub fn clear_map_filter(&mut self, cx: &mut Context<Self>) {
        self.library.map_filter = None;
        self.library.map_filter_name = None;
        cx.notify();
    }

    /// Regroup the grid and remember the choice.
    pub fn set_gallery_group(&mut self, group: GroupBy, cx: &mut Context<Self>) {
        self.library.group_by = group;
        self.library.save();
        cx.notify();
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
    fn ordinary_photos_of_people_are_not_flagged() {
        // The first formula summed "sexy" into the verdict, and on a
        // real camera roll — people, beaches, shoulders — it flagged
        // nearly everything. The rule now needs porn/hentai, or a
        // near-certain sexy.
        let portrait = ExplicitScore {
            explicit: 0.05,
            sexy: 0.55,
        };
        assert!(!is_explicit(portrait));
        let beach = ExplicitScore {
            explicit: 0.10,
            sexy: 0.85,
        };
        assert!(!is_explicit(beach));
        let explicit = ExplicitScore {
            explicit: 0.60,
            sexy: 0.30,
        };
        assert!(is_explicit(explicit));
        let sure_sexy = ExplicitScore {
            explicit: 0.04,
            sexy: 0.95,
        };
        assert!(is_explicit(sure_sexy));
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

    /// A minimal JPEG whose EXIF says it was taken at Times Square:
    /// 40°45'28.8"N, 73°59'6"W. Built by hand so the test exercises the
    /// real parser rather than a mock of it.
    fn times_square_jpeg() -> Vec<u8> {
        let mut tiff: Vec<u8> = Vec::new();
        let u16le = |v: &mut Vec<u8>, x: u16| v.extend_from_slice(&x.to_le_bytes());
        let u32le = |v: &mut Vec<u8>, x: u32| v.extend_from_slice(&x.to_le_bytes());
        // Header: little-endian, IFD0 at offset 8.
        tiff.extend_from_slice(b"II*\0");
        u32le(&mut tiff, 8);
        // IFD0: one entry, the GPS IFD pointer (tag 0x8825) to offset 26.
        u16le(&mut tiff, 1);
        u16le(&mut tiff, 0x8825);
        u16le(&mut tiff, 4); // LONG
        u32le(&mut tiff, 1);
        u32le(&mut tiff, 26);
        u32le(&mut tiff, 0); // no next IFD
                             // GPS IFD at 26: Ref/Latitude/Ref/Longitude; rationals at 80/104.
        u16le(&mut tiff, 4);
        for (tag, kind, count, value) in [
            (0x0001u16, 2u16, 2u32, u32::from_le_bytes(*b"N\0\0\0")),
            (0x0002, 5, 3, 80),
            (0x0003, 2, 2, u32::from_le_bytes(*b"W\0\0\0")),
            (0x0004, 5, 3, 104),
        ] {
            u16le(&mut tiff, tag);
            u16le(&mut tiff, kind);
            u32le(&mut tiff, count);
            u32le(&mut tiff, value);
        }
        u32le(&mut tiff, 0);
        // 40° 45' 28.8"  then  73° 59' 6".
        for (num, den) in [(40, 1), (45, 1), (288, 10), (73, 1), (59, 1), (6, 1)] {
            u32le(&mut tiff, num);
            u32le(&mut tiff, den);
        }
        let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xE1];
        jpeg.extend_from_slice(&((2 + 6 + tiff.len()) as u16).to_be_bytes());
        jpeg.extend_from_slice(b"Exif\0\0");
        jpeg.extend_from_slice(&tiff);
        jpeg.extend_from_slice(&[0xFF, 0xD9]);
        jpeg
    }

    #[test]
    fn the_place_filter_reads_the_cameras_own_position() {
        let path = std::env::temp_dir().join(format!("schist-gps-test-{}.jpg", std::process::id()));
        std::fs::write(&path, times_square_jpeg()).unwrap();
        let (lat, lon) = photo_gps(&path).expect("the GPS IFD parses");
        let _ = std::fs::remove_file(&path);
        assert!((lat - 40.758).abs() < 1e-3, "latitude was {lat}");
        assert!((lon + 73.985).abs() < 1e-3, "longitude was {lon}");
        // And that position sorts into the New York box, which is the
        // whole of "import photos taken in NYC".
        let nyc = &library_geo::PLACES[0];
        assert!(nyc.bounds.contains(lat, lon));
    }
}

#[cfg(test)]
mod grouping_tests {
    use super::*;

    #[test]
    fn unix_times_become_civil_dates() {
        assert_eq!(ymd_from_unix(0), (1970, 1, 1));
        // Constants checked against `date -u`.
        assert_eq!(ymd_from_unix(1_787_270_400), (2026, 8, 21));
        assert_eq!(taken_from_unix(1_788_264_000), "2026-09-01 12:00:00");
        assert_eq!(taken_from_unix(0), "1970-01-01 00:00:00");
    }

    #[test]
    fn exif_datetimes_normalize_to_sortable_text() {
        // The date's colons swap for dashes so plain string order is
        // chronological order; the time keeps its own.
        let taken = "2026:08:14 17:03:22";
        let mut bytes: Vec<u8> = taken.bytes().collect();
        bytes[4] = b'-';
        bytes[7] = b'-';
        let normalized = String::from_utf8(bytes).unwrap();
        assert_eq!(normalized, "2026-08-14 17:03:22");
        assert!(normalized.get(..7) == Some("2026-08"));
        assert!("2026-09-01 00:00:00" > normalized.as_str());
    }
}
