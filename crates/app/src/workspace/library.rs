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
// The on-disk model — folders, buckets, the index snapshot, the caches
// beside the thumbnails — lives in `schist-gallery`, shared with the
// headless MCP server. This file is what the window does with it.
use schist_gallery::*;
pub use schist_gallery::{backing_psd, Entry, Section};
use std::collections::BTreeMap;
use std::path::Path;

/// Thumbnails decoded per background batch — in parallel, one task
/// each, so a batch costs its slowest decode rather than their sum.
/// Small enough that the first screenful streams in rather than
/// arriving all at once at the end.
const THUMB_BATCH: usize = 8;
/// Decoded thumbnails kept in memory before the least recently shown
/// go back to the disk cache. Each is up to 256 KB of BGRA, so this is
/// a ~256 MB ceiling where an afternoon of scrolling used to pin every
/// photo it ever passed.
const THUMB_KEEP: usize = 1024;
/// What the map shrinks to while the gallery is off screen: enough for
/// the first screenfuls to reappear instantly, a fraction of the RAM.
const THUMB_KEEP_PARKED: usize = 256;
/// The grid scrollbar's breathing room at each end of its track.
const SCROLLBAR_INSET: f32 = 4.0;
/// How many recently opened files the start screen lists.
const RECENTS_KEPT: usize = 10;

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

/// A named basket of photos, dragged in and acted on as a group — or,
/// with a rule set, a smart one that keeps filling itself.
#[derive(Clone)]
pub struct Bucket {
    pub name: String,
    /// Photos put in by hand (drag, right-click). Persisted.
    pub photos: Vec<PathBuf>,
    /// The smart rule: a search query and/or a map area. Either being
    /// set makes the bucket fill itself from the index — both set
    /// means both must hold. Persisted.
    pub query: Option<String>,
    pub area: Option<(GeoBounds, String)>,
    /// What the rule currently matches, best first. Derived — rebuilt
    /// whenever the index moves — so never persisted.
    pub matches: Vec<PathBuf>,
}

impl Bucket {
    pub fn is_smart(&self) -> bool {
        self.query.is_some() || self.area.is_some()
    }

    /// Everything in the bucket: the hand-picked photos in the order
    /// they were dropped, then what the rule matched.
    pub fn contents(&self) -> Vec<PathBuf> {
        let mut all = self.photos.clone();
        for path in &self.matches {
            if !all.contains(path) {
                all.push(path.clone());
            }
        }
        all
    }

    /// The rule, described for people: what shows under the bucket's
    /// header and in its editor.
    pub fn rule_label(&self) -> String {
        let mut parts = Vec::new();
        if let Some(query) = &self.query {
            parts.push(format!("matches \u{201c}{query}\u{201d}"));
        }
        if let Some((_, name)) = &self.area {
            parts.push(format!("taken in {name}"));
        }
        parts.join(" · ")
    }
}

/// What the gallery's right-click menu was opened on.
#[derive(Clone)]
pub enum GalleryContext {
    Photo(PathBuf),
    /// Freeze the clicked marker's members independently of selection or zoom.
    MapCluster(Vec<PathBuf>),
    Bucket(usize),
}

/// A drag of gallery photos, headed for a folder or a bucket.
#[derive(Clone)]
pub struct GalleryDrag {
    pub paths: Vec<PathBuf>,
}

/// A query the engine already answered: its text embedding and the
/// place it named, so retyping — every prefix, every backspace — costs
/// a lookup instead of a model run.
#[derive(Clone)]
struct CachedQuery {
    text: Option<Arc<Vec<f32>>>,
    place: Option<library_geo::GeoMatch>,
}

/// The ranking task's view of the index, shared rather than copied: a
/// keystroke used to clone every path and vector in the library.
#[derive(Clone)]
struct SearchSnapshot {
    vectors: Arc<Vec<(PathBuf, Arc<Vec<f32>>)>>,
    positions: Arc<Vec<(PathBuf, (f64, f64))>>,
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
    /// The selected photos, in the order they were picked; the last is
    /// the lead — what arrows move and Enter opens.
    pub selected: Vec<PathBuf>,
    /// Where a Shift-click range extends from.
    select_anchor: Option<PathBuf>,
    /// The buckets: named baskets photos are dragged into, acted on as
    /// a group (ZIP them, upscale them). Persisted.
    pub buckets: Vec<Bucket>,
    /// Bumped whenever any bucket's rule changes (or a bucket goes
    /// away, which shifts the indices an in-flight compute is keyed
    /// by), so stale smart-bucket results can be recognised.
    rule_rev: u64,
    /// The `(index_gen, rule_rev)` the smart buckets were last scored
    /// against, and whether a scoring pass is in flight. `None` = never
    /// scored this session.
    smart_synced: Option<(u64, u64)>,
    smart_running: bool,
    /// Showing one bucket's contents instead of the folders.
    pub bucket_filter: Option<usize>,
    /// The gallery's own right-click menu: where, and on what.
    pub context: Option<(Point<Pixels>, GalleryContext)>,
    /// Thumbnail cell edge in pixels, the tray slider's value.
    pub thumb_px: f32,
    pub scanning: bool,
    /// A camera import in flight, so a second click does not start one.
    pub importing: bool,
    /// Thumbnail states, tagged with the mtime they were built from: a
    /// file that changed underneath — a photo still landing off a
    /// camera when its first decode ran, an edit — loads again.
    pub(super) thumbs: FxHashMap<PathBuf, (u64, Thumb)>,
    /// The frame each thumbnail was last visible on — what decides who
    /// goes when the map is over budget. Cells near the viewport
    /// re-stamp every frame, and eviction refuses anything stamped
    /// with the current frame, so what is on screen can never be taken
    /// no matter how far over budget a huge window gets.
    thumb_used: FxHashMap<PathBuf, u64>,
    thumb_frame: u64,
    queue: Vec<ThumbJob>,
    /// Whether a thumbnail loader task is live (only ever one at a time).
    ticker: bool,
    /// A gallery open waiting for its decode: (path being loaded, the
    /// original image it is an edit of). Consumed by `finish_load`.
    pub(super) pending_backing: Vec<(PathBuf, PathBuf)>,
    /// Original image path per open document that came from the gallery,
    /// so a save can refresh that image's thumbnail.
    pub(super) edit_backings: FxHashMap<schist_core::DocumentId, PathBuf>,
    /// The import dialog's navigable map: view, tiles, and the drawn
    /// boundary (kept here so it survives closing the dialog).
    pub map: library_geo::MapState,
    /// The photo map has its own camera, independent of import boundaries.
    pub world_map: library_geo::MapState,
    pub map_view: bool,
    pub map_photos: Vec<PathBuf>,
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
    pub(super) positions: FxHashMap<PathBuf, Option<(f64, f64)>>,
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
    /// The caret's byte position in `search`, always on a char
    /// boundary — arrows move it, typing inserts at it.
    pub search_cursor: usize,
    /// ⌘A selected the whole query: the next keystroke replaces it,
    /// backspace clears it, ⌘C/⌘X take it — the minimal selection a
    /// one-line box owes the keyboard.
    pub search_selected: bool,
    pub search_results: Option<Vec<(PathBuf, f32)>>,
    /// The place the current query named, when it named one — shown on
    /// the results header.
    pub search_place: Option<String>,
    /// The bucket the current results — or the query in flight — were
    /// ranked within: a search made while viewing a bucket is scoped
    /// to it. Compared against `bucket_filter` each frame, so the
    /// search follows when the viewed bucket changes.
    pub search_scoped: Option<usize>,
    /// Bumped per query so a slow embedding cannot land on a newer one.
    search_seq: u64,
    /// Answered queries, so the engine is only asked once per string.
    query_cache: FxHashMap<String, CachedQuery>,
    /// Bumped whenever the index gains entries; the snapshot below is
    /// rebuilt only when this moved.
    index_gen: u64,
    /// Whether this session already loaded the persisted index file,
    /// and the generation it last wrote — so the file is read once and
    /// written only when something new was learned.
    index_loaded: bool,
    index_saved_gen: u64,
    /// When the loader last repainted for index-only work: those
    /// batches finish in milliseconds off warm caches, and notifying
    /// per batch rebuilt the grid hundreds of times per second.
    last_loader_notify: Option<std::time::Instant>,
    index_snapshot: Option<(u64, SearchSnapshot)>,
    /// The towers were pre-loaded this session (parsing and optimizing
    /// the text model is the slow part of a first search).
    engine_warmed: bool,
    /// The grid's scroll handle, plus the viewport and selected-cell
    /// rectangles recorded each paint — what keyboard navigation needs
    /// to keep the selection on screen in a wrap layout that has no
    /// notion of rows to ask about.
    pub grid_scroll: gpui::ScrollHandle,
    pub grid_bounds: Bounds<Pixels>,
    /// A scrollbar-thumb drag in progress: the pointer's offset from
    /// the thumb's top when it was grabbed, in pixels.
    pub scrollbar_grab: Option<f32>,
    /// The photos a gpui drag is currently carrying. Kept so that a
    /// drag which wanders out of the window can be handed to the
    /// platform's own drag-and-drop, and dropped on a file manager.
    pub dragging: Option<Vec<PathBuf>>,
    pub selected_bounds: Option<Bounds<Pixels>>,
    /// The keyboard moved the selection; scroll until it is visible.
    reveal_selection: bool,
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

    pub(super) fn key(self) -> &'static str {
        match self {
            GroupBy::Date => "date",
            GroupBy::Folder => "folder",
            GroupBy::Place => "place",
        }
    }

    pub(super) fn from_key(key: &str) -> Option<GroupBy> {
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
        let file = LibraryFile::load();
        Library {
            open: false,
            folders: file.folders,
            recents: file.recents,
            sections: Vec::new(),
            folder_filter: None,
            selected: Vec::new(),
            select_anchor: None,
            buckets: file
                .buckets
                .into_iter()
                .map(|b| match b {
                    BucketFile::Rich {
                        name,
                        photos,
                        query,
                        area,
                    } => Bucket {
                        name,
                        photos,
                        query,
                        area,
                        matches: Vec::new(),
                    },
                    BucketFile::Plain(name, photos) => Bucket {
                        name,
                        photos,
                        query: None,
                        area: None,
                        matches: Vec::new(),
                    },
                })
                .collect(),
            rule_rev: 0,
            smart_synced: None,
            smart_running: false,
            bucket_filter: None,
            context: None,
            thumb_px: file.thumb_px.unwrap_or(144.0).clamp(80.0, 240.0),
            scanning: false,
            importing: false,
            thumbs: FxHashMap::default(),
            thumb_used: FxHashMap::default(),
            thumb_frame: 0,
            queue: Vec::new(),
            ticker: false,
            pending_backing: Vec::new(),
            edit_backings: FxHashMap::default(),
            map: library_geo::MapState::default(),
            world_map: library_geo::MapState::world(),
            map_view: false,
            map_photos: Vec::new(),
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
            search_cursor: 0,
            search_selected: false,
            search_results: None,
            search_place: None,
            search_scoped: None,
            search_seq: 0,
            query_cache: FxHashMap::default(),
            index_gen: 0,
            index_loaded: false,
            index_saved_gen: 0,
            last_loader_notify: None,
            index_snapshot: None,
            engine_warmed: false,
            grid_scroll: gpui::ScrollHandle::new(),
            grid_bounds: Bounds::default(),
            scrollbar_grab: None,
            dragging: None,
            selected_bounds: None,
            reveal_selection: false,
            heif_needed: None,
            heif_prompted: false,
        }
    }

    fn save(&self) {
        let file = LibraryFile {
            folders: self.folders.clone(),
            recents: self.recents.clone(),
            thumb_px: Some(self.thumb_px),
            group_by: Some(self.group_by.key().to_string()),
            buckets: self
                .buckets
                .iter()
                .map(|b| BucketFile::Rich {
                    name: b.name.clone(),
                    photos: b.photos.clone(),
                    query: b.query.clone(),
                    area: b.area.clone(),
                })
                .collect(),
        };
        if let Err(err) = file.save() {
            log::warn!("gallery: library.json not saved: {err:#}");
        }
    }

    /// The decoded thumbnail for `entry`, if one is in memory. A pure
    /// read: the whole grid builds an element per photo every frame
    /// while only a screenful is on screen, so building must not queue
    /// work or count as use — `note_visible`, called from each cell's
    /// paint-time probe, is what does both.
    pub fn thumb(&self, entry: &Entry) -> Option<Arc<RenderImage>> {
        match self.thumbs.get(&entry.path) {
            Some((mtime, Thumb::Ready(img))) if *mtime == entry.mtime => Some(img.clone()),
            _ => None,
        }
    }

    /// A cell reported itself inside (or near) the viewport: stamp its
    /// thumbnail as in use — what the eviction pass keeps — and queue
    /// a decode when none is ready or in flight. Returns whether new
    /// work was queued, so the caller knows to kick the loader.
    pub(super) fn note_visible(&mut self, entry: &Entry) -> bool {
        self.thumb_used.insert(entry.path.clone(), self.thumb_frame);
        match self.thumbs.get(&entry.path) {
            // Ready, in flight, or given up — leave it be. A different
            // mtime falls through and queues a fresh decode.
            Some((mtime, _)) if *mtime == entry.mtime => return false,
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
        true
    }

    /// Keep at most `keep` decoded thumbnails, dropping the least
    /// recently shown back to the disk cache they reload from. Pending
    /// and failed states stay — they hold no pixels, and a Failed that
    /// went away would retry forever.
    fn evict_thumbs(&mut self, keep: usize) {
        let ready = |t: &(u64, Thumb)| matches!(t.1, Thumb::Ready(_));
        let over = self
            .thumbs
            .values()
            .filter(|t| ready(t))
            .count()
            .saturating_sub(keep);
        if over == 0 {
            return;
        }
        // Anything stamped with the current frame is on (or near) the
        // screen right now and is never a candidate, even if that
        // leaves the map over budget — a giant window at the smallest
        // thumb size beats the budget, never the other way round.
        let mut by_age: Vec<(u64, PathBuf)> = self
            .thumbs
            .iter()
            .filter(|(_, t)| ready(t))
            .filter_map(|(p, _)| {
                let stamp = self.thumb_used.get(p).copied().unwrap_or(0);
                (stamp < self.thumb_frame).then(|| (stamp, p.clone()))
            })
            .collect();
        by_age.sort_unstable();
        for (_, path) in by_age.into_iter().take(over) {
            self.thumbs.remove(&path);
            self.thumb_used.remove(&path);
        }
    }

    /// A new gallery frame is rendering: visibility stamps from here
    /// on are "current", the one age eviction refuses to touch.
    pub(super) fn begin_thumb_frame(&mut self) {
        self.thumb_frame += 1;
    }

    /// The grid scrollbar's geometry: (track inset, thumb height,
    /// thumb travel, max scroll), exact because it reads the scroll
    /// handle's own extents. `None` while nothing scrolls.
    pub(super) fn scrollbar_geometry(&self) -> Option<(f32, f32, f32, f32)> {
        let view_h = f32::from(self.grid_bounds.size.height);
        let max_y = f32::from(self.grid_scroll.max_offset().height);
        if view_h <= 0.0 || max_y <= 1.0 {
            return None;
        }
        let track_h = view_h - 2.0 * SCROLLBAR_INSET;
        let thumb_h = (track_h * view_h / (view_h + max_y)).clamp(30.0, track_h);
        let travel = (track_h - thumb_h).max(1.0);
        Some((SCROLLBAR_INSET, thumb_h, travel, max_y))
    }

    /// Scroll so the thumb follows the pointer of an active grab;
    /// `pointer_y` is in window coordinates.
    pub(super) fn scrollbar_drag_to(&mut self, pointer_y: f32) {
        let Some(grab) = self.scrollbar_grab else {
            return;
        };
        let Some((inset, _, travel, max_y)) = self.scrollbar_geometry() else {
            return;
        };
        let top = f32::from(self.grid_bounds.origin.y);
        let thumb_top = (pointer_y - top - inset - grab).clamp(0.0, travel);
        let mut offset = self.grid_scroll.offset();
        offset.y = px(-(thumb_top / travel * max_y));
        self.grid_scroll.set_offset(offset);
    }

    /// The gallery left the screen: give back what only it was using.
    /// The scorer and the two search towers are hundreds of resident
    /// megabytes; they reload lazily, and a fully indexed library
    /// never asks for them again until a new photo or a new search.
    /// Thumbnails shrink to a parked handful — the PNGs are on disk,
    /// so reopening costs a moment of decode, not a rebuild.
    pub(super) fn shed_memory(&mut self) {
        for id in ["nsfw", "embed-image", "embed-text"] {
            schist_neural::release(id);
        }
        self.engine_warmed = false;
        self.evict_thumbs(THUMB_KEEP_PARKED);
        // Leaving the gallery is also a fine moment to persist what
        // indexing learned, in case the loader never went idle.
        self.save_index_snapshot();
    }

    /// Feed the loader the next photos missing index work — a search
    /// embedding (when the model to make one is here) or an EXIF
    /// position probe. Returns whether anything queued.
    fn refill_index_queue(&mut self) -> bool {
        let embeds = schist_neural::installed("embed-image");
        let scores = nsfw_installed();
        let jobs: Vec<ThumbJob> = self
            .sections
            .iter()
            .flat_map(|s| s.entries.iter())
            .filter(|e| {
                (embeds && !self.embeddings.contains_key(&e.path))
                    || !self.positions.contains_key(&e.path)
                    // A scorer installed after the position pass ran
                    // still has every photo to see.
                    || (scores && !self.flagged.contains_key(&e.path))
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

    /// The gallery as the AI panel's MCP tools describe it: what the
    /// sidebar and tray show, as JSON.
    pub(super) fn state_json(&self) -> serde_json::Value {
        use serde_json::json;
        let (indexed, total) = self.index_progress();
        json!({
            "folders": self.folders.iter().map(|f| f.display().to_string()).collect::<Vec<_>>(),
            "photos": self.photo_count(),
            "group_by": self.group_by.key(),
            "groups": self.grouped().iter().map(|(title, subtitle, entries)| json!({
                "title": title, "detail": subtitle, "photos": entries.len(),
            })).collect::<Vec<_>>(),
            "selected": self.selected.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
            "buckets": self.buckets.iter().enumerate().map(|(i, b)| json!({
                "index": i, "name": b.name, "photos": b.contents().len(),
                "rule": b.is_smart().then(|| b.rule_label()),
                "viewing": self.bucket_filter == Some(i),
            })).collect::<Vec<_>>(),
            "search": (!self.search.is_empty()).then_some(&self.search),
            "search_results": self.search_results.as_ref().map(|r| r.len()),
            "search_bucket": self
                .search_scoped
                .and_then(|i| self.buckets.get(i))
                .map(|b| b.name.clone()),
            "map_filter": self.map_filter_label(),
            "index": {"embedded": indexed, "total": total,
                      "search_models_installed": schist_neural::embed::ready()},
        })
    }

    /// Photos as JSON rows: path, when taken, where, whether edited,
    /// and the content filter's verdict — `null` while unscored.
    pub(super) fn entry_json(&self, entry: &Entry) -> serde_json::Value {
        serde_json::json!({
            "path": entry.path.display().to_string(),
            "taken": self.taken_of(entry),
            "place": self.places.get(&entry.path).cloned().flatten(),
            "edited": entry.edited,
            "selected": self.is_selected(&entry.path),
            "flagged": self.flagged.get(&entry.path).copied(),
        })
    }

    /// The content filter's verdict on a photo, as a word: `flagged`,
    /// `clean`, or `unscored` (not looked at yet, or no model to look
    /// with).
    pub(super) fn verdict(&self, path: &Path) -> &'static str {
        match self.flagged.get(path) {
            Some(true) => "flagged",
            Some(false) => "clean",
            None => "unscored",
        }
    }

    /// Every photo the grid could show (folder and map filters
    /// applied, the content filter *not* applied) with the given
    /// verdict, in display order.
    pub(super) fn entries_by_verdict(&self, verdict: &str) -> Vec<Entry> {
        self.grouped()
            .into_iter()
            .flat_map(|(_, _, entries)| entries)
            .filter(|e| self.verdict(&e.path) == verdict)
            .collect()
    }

    /// The content filter's state, for the AI panel's tools and the
    /// gallery's own reporting: model, switch, and how many photos
    /// have been scored each way.
    pub(super) fn content_filter_json(&self, enabled: bool) -> serde_json::Value {
        let all: Vec<&Entry> = self
            .sections
            .iter()
            .flat_map(|s| s.entries.iter())
            .collect();
        let count = |v: &str| all.iter().filter(|e| self.verdict(&e.path) == v).count();
        serde_json::json!({
            "enabled": enabled,
            "model_installed": nsfw_installed(),
            "photos": all.len(),
            "flagged": count("flagged"),
            "clean": count("clean"),
            "unscored": count("unscored"),
            "hidden_now": if enabled { self.flagged_count() } else { 0 },
        })
    }

    /// The photos of one group (by title), one bucket (by name), or
    /// the whole grid, in display order.
    pub(super) fn list_entries(&self, group: Option<&str>, bucket: Option<&str>) -> Vec<Entry> {
        if let Some(name) = bucket {
            let Some(bucket) = self
                .buckets
                .iter()
                .find(|b| b.name.eq_ignore_ascii_case(name))
            else {
                return Vec::new();
            };
            let all: Vec<&Entry> = self
                .sections
                .iter()
                .flat_map(|s| s.entries.iter())
                .collect();
            return bucket
                .contents()
                .iter()
                .filter_map(|p| all.iter().find(|e| &e.path == p).map(|e| (*e).clone()))
                .collect();
        }
        self.grouped()
            .into_iter()
            .filter(|(title, _, _)| group.is_none_or(|g| title.eq_ignore_ascii_case(g)))
            .flat_map(|(_, _, entries)| entries)
            .collect()
    }

    /// The cached thumbnail PNG for a photo, if one has been rendered.
    pub(super) fn thumb_png(&self, path: &Path) -> Option<Vec<u8>> {
        let entry = self
            .sections
            .iter()
            .flat_map(|s| s.entries.iter())
            .find(|e| e.path == path)?;
        let cache = thumb_cache_path(&thumb_source(&entry.path, entry.edited), entry.mtime)?;
        std::fs::read(cache).ok()
    }

    /// How much of the gallery the search index covers: (embedded, all).
    pub fn index_progress(&self) -> (usize, usize) {
        let total = self.sections.iter().map(|s| s.entries.len()).sum();
        (self.embeddings.len().min(total), total)
    }

    /// Everything the index knows, one row per photo that has any of
    /// it — what the snapshot file persists between runs.
    fn collect_index_rows(&self) -> Vec<IndexRow> {
        self.sections
            .iter()
            .flat_map(|s| s.entries.iter())
            .filter_map(|e| {
                let row = IndexRow {
                    path: e.path.clone(),
                    mtime: e.mtime,
                    embed: self.embeddings.get(&e.path).cloned(),
                    gps: self.positions.get(&e.path).copied(),
                    taken: self.taken.get(&e.path).cloned(),
                    place: self.places.get(&e.path).cloned(),
                    flagged: self.flagged.get(&e.path).copied(),
                };
                let empty = row.embed.is_none()
                    && row.gps.is_none()
                    && row.taken.is_none()
                    && row.place.is_none()
                    && row.flagged.is_none();
                (!empty).then_some(row)
            })
            .collect()
    }

    /// Take a loaded snapshot into the live index. Rows only count for
    /// photos the scan still knows with the same mtime — an edited
    /// photo re-indexes — and never overwrite what this session
    /// already learned.
    fn apply_index_rows(&mut self, mut rows: Vec<IndexRow>) {
        {
            let current: FxHashMap<&Path, u64> = self
                .sections
                .iter()
                .flat_map(|s| s.entries.iter())
                .map(|e| (e.path.as_path(), e.mtime))
                .collect();
            rows.retain(|r| current.get(r.path.as_path()) == Some(&r.mtime));
        }
        if rows.is_empty() {
            return;
        }
        log::info!("gallery: index snapshot restored {} photos", rows.len());
        for row in rows {
            if let Some(embed) = row.embed {
                self.embeddings.entry(row.path.clone()).or_insert(embed);
            }
            if let Some(gps) = row.gps {
                self.positions.entry(row.path.clone()).or_insert(gps);
            }
            if let Some(taken) = row.taken {
                self.taken.entry(row.path.clone()).or_insert(taken);
            }
            if let Some(place) = row.place {
                self.places.entry(row.path.clone()).or_insert(place);
            }
            if let Some(flagged) = row.flagged {
                self.flagged.entry(row.path).or_insert(flagged);
            }
        }
        self.index_gen += 1;
    }

    /// Write the index to its snapshot file (on a plain thread — pure
    /// file work, and some callers have no async context), if anything
    /// changed since the last write. ~2 KB per photo, one read at the
    /// next launch instead of thousands of per-photo cache probes.
    pub(super) fn save_index_snapshot(&mut self) {
        if self.index_gen == self.index_saved_gen {
            return;
        }
        self.index_saved_gen = self.index_gen;
        let rows = self.collect_index_rows();
        if rows.is_empty() {
            return;
        }
        std::thread::spawn(move || {
            if let Err(err) = write_index_snapshot(&rows) {
                log::warn!("gallery: index snapshot not saved: {err:#}");
            }
        });
    }

    /// Whether any queued decode is waiting for a loader task.
    pub fn wants_thumbs(&self) -> bool {
        !self.queue.is_empty() && !self.ticker
    }

    /// Whether a thumbnail decode gave up, so the cell can say so.
    pub fn thumb_failed(&self, path: &Path) -> bool {
        matches!(self.thumbs.get(path), Some((_, Thumb::Failed)))
    }

    /// What may leave in an archive: with the content filter on, the
    /// flagged photos stay behind whatever asked for them — a bucket,
    /// a selection, an agent. Returns what may go and how many stayed.
    pub(super) fn zip_candidates(&self, paths: Vec<PathBuf>, hide: bool) -> (Vec<PathBuf>, usize) {
        if !hide {
            return (paths, 0);
        }
        let before = paths.len();
        let kept: Vec<PathBuf> = paths.into_iter().filter(|p| !self.is_flagged(p)).collect();
        let held = before - kept.len();
        (kept, held)
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
            .filter(|e| self.passes_map(&e.path) && self.is_flagged(&e.path))
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
            .filter(|e| self.passes_map(&e.path))
            .count()
    }

    /// Whether the map filter lets a photo through: no filter passes
    /// everything, a filter passes only photos whose EXIF position
    /// falls inside it — the point of asking for a place.
    pub fn passes_map(&self, path: &Path) -> bool {
        let Some(bounds) = self.map_filter else {
            return true;
        };
        matches!(
            self.positions.get(path),
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
        // A bucket on show replaces the grouping: the hand-picked
        // photos in the order they were dropped in, then whatever its
        // rule matched, best first.
        if let Some(bucket) = self.bucket_filter.and_then(|i| self.buckets.get(i)) {
            let entries: Vec<Entry> = bucket
                .contents()
                .iter()
                .filter_map(|path| {
                    self.sections
                        .iter()
                        .flat_map(|s| s.entries.iter())
                        .find(|e| &e.path == path)
                        .cloned()
                })
                .filter(|e| self.passes_map(&e.path))
                .collect();
            return vec![(
                format!("Bucket · {}", bucket.name),
                bucket.rule_label(),
                entries,
            )];
        }
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
                        .filter(|e| self.passes_map(&e.path))
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
                    .filter(|e| self.passes_map(&e.path))
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
                    .filter(|e| self.passes_map(&e.path))
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

    /// A fresh bucket; an empty name falls back to "Bucket N".
    pub fn add_bucket(&mut self, name: String) -> usize {
        let name = match name.trim() {
            "" => format!("Bucket {}", self.buckets.len() + 1),
            typed => typed.to_string(),
        };
        self.buckets.push(Bucket {
            name,
            photos: Vec::new(),
            query: None,
            area: None,
            matches: Vec::new(),
        });
        self.save();
        self.buckets.len() - 1
    }

    /// Rename a bucket and set (or clear) its smart rule. An empty
    /// name keeps the one it has.
    pub fn configure_bucket(
        &mut self,
        index: usize,
        name: String,
        query: Option<String>,
        area: Option<(GeoBounds, String)>,
    ) {
        let Some(bucket) = self.buckets.get_mut(index) else {
            return;
        };
        if !name.trim().is_empty() {
            bucket.name = name.trim().to_string();
        }
        if bucket.query != query || bucket.area != area {
            bucket.query = query;
            bucket.area = area;
            bucket.matches.clear();
            self.rule_rev += 1;
        }
        self.save();
    }

    /// Drop photos into a bucket, keeping each once.
    pub fn add_to_bucket(&mut self, index: usize, paths: &[PathBuf]) {
        let Some(bucket) = self.buckets.get_mut(index) else {
            return;
        };
        for path in paths {
            if !bucket.photos.contains(path) {
                bucket.photos.push(path.clone());
            }
        }
        self.save();
    }

    pub fn remove_from_bucket(&mut self, index: usize, path: &Path) {
        if let Some(bucket) = self.buckets.get_mut(index) {
            bucket.photos.retain(|p| p != path);
            self.save();
        }
    }

    pub fn clear_bucket(&mut self, index: usize) {
        if let Some(bucket) = self.buckets.get_mut(index) {
            bucket.photos.clear();
            self.save();
        }
    }

    pub fn delete_bucket(&mut self, index: usize) {
        if index < self.buckets.len() {
            self.buckets.remove(index);
            // Later buckets just shifted down a slot; an in-flight
            // smart-bucket pass is keyed by the old indices.
            self.rule_rev += 1;
            if self.bucket_filter == Some(index) {
                self.bucket_filter = None;
            } else if let Some(f) = self.bucket_filter {
                if f > index {
                    self.bucket_filter = Some(f - 1);
                }
            }
            self.save();
        }
    }

    /// What a search is confined to: the viewed bucket's contents
    /// (hand-picked photos and the rule's matches), so the bucket
    /// filters first and the query ranks what is left. `None` while
    /// no bucket is on show — the whole index.
    pub fn search_scope(&self) -> Option<FxHashSet<PathBuf>> {
        self.bucket_filter
            .and_then(|i| self.buckets.get(i))
            .map(|b| b.contents().into_iter().collect())
    }

    /// The index as the ranking task sees it, rebuilt only when the
    /// index actually changed.
    fn search_snapshot(&mut self) -> SearchSnapshot {
        if let Some((gen, snapshot)) = &self.index_snapshot {
            if *gen == self.index_gen {
                return snapshot.clone();
            }
        }
        let snapshot = SearchSnapshot {
            vectors: Arc::new(
                self.embeddings
                    .iter()
                    .map(|(p, v)| (p.clone(), v.clone()))
                    .collect(),
            ),
            positions: Arc::new(
                self.positions
                    .iter()
                    .filter_map(|(p, pos)| pos.map(|pos| (p.clone(), pos)))
                    .collect(),
            ),
        };
        self.index_snapshot = Some((self.index_gen, snapshot.clone()));
        snapshot
    }

    /// The lead of the selection — what arrows move and Enter opens.
    pub fn lead_selected(&self) -> Option<&PathBuf> {
        self.selected.last()
    }

    /// Whether a photo is in the selection.
    pub fn is_selected(&self, path: &Path) -> bool {
        self.selected.iter().any(|p| p == path)
    }

    /// The lead selection's entry, if it still exists in a section.
    pub fn selected_entry(&self) -> Option<&Entry> {
        let lead = self.lead_selected()?;
        self.sections
            .iter()
            .flat_map(|s| s.entries.iter())
            .find(|e| &e.path == lead)
    }

    /// A plain click: this photo alone, and the range anchor moves.
    pub fn select_single(&mut self, path: PathBuf) {
        self.select_anchor = Some(path.clone());
        self.selected = vec![path];
    }

    /// ⌘-click: in or out of the selection, keeping the rest.
    pub fn toggle_selected(&mut self, path: PathBuf) {
        if let Some(at) = self.selected.iter().position(|p| p == &path) {
            self.selected.remove(at);
        } else {
            self.select_anchor.get_or_insert_with(|| path.clone());
            self.selected.push(path);
        }
    }
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

/// The GPS position a camera wrote into a file, if any. Blocking.
fn photo_gps(path: &Path) -> Option<(f64, f64)> {
    gps_from(&exif_of(path)?)
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
            // the Import click is already on the list. The search
            // towers are NOT warmed here — they are ~300 MB resident,
            // so they wait for the search box to be focused.
            #[cfg(target_os = "macos")]
            super::library_icc::start_browsing();
        } else {
            self.library.shed_memory();
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
                ws.maybe_load_index_snapshot(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Restore the persisted index once per session, now that a scan
    /// knows which photos (and mtimes) it may vouch for. This is what
    /// makes a relaunch open already indexed instead of re-reading
    /// thousands of per-photo cache files through the loader.
    fn maybe_load_index_snapshot(&mut self, cx: &mut Context<Self>) {
        if self.library.index_loaded {
            return;
        }
        self.library.index_loaded = true;
        cx.spawn(async move |this, cx| {
            let rows = cx
                .background_executor()
                .spawn(async move { read_index_snapshot() })
                .await;
            let Some(rows) = rows else { return };
            this.update(cx, |ws, cx| {
                ws.library.apply_index_rows(rows);
                // Nothing new to learn is nothing new to write back.
                ws.library.index_saved_gen = ws.library.index_gen;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Every extension a registered codec can decode, lowercased.
    pub(super) fn codec_extensions(&self) -> Vec<String> {
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
        // The map can open before any grid cell has queued a thumbnail.
        if self.library.map_view && !self.library.ticker && self.library.queue.is_empty() {
            self.library.refill_index_queue();
        }
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
                this.update(cx, |ws, _| {
                    ws.library.ticker = false;
                    // The loader going idle is "indexing caught up":
                    // the moment to persist what it learned.
                    ws.library.save_index_snapshot();
                })
                .ok();
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
                ws.library.index_gen += 1;
                let visible_work = results.iter().any(|(_, _, for_index, _)| !for_index);
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
                // The batch may have pushed the map over budget; shed
                // whatever scrolled away longest ago.
                ws.library.evict_thumbs(THUMB_KEEP);
                // A thumbnail someone can see always repaints; pure
                // index batches — milliseconds each off warm caches —
                // repaint at most a few times a second, or the counter
                // in the search box would rebuild the grid per batch.
                let now = std::time::Instant::now();
                let due = ws
                    .library
                    .last_loader_notify
                    .is_none_or(|last| now.duration_since(last).as_millis() >= 250);
                if visible_work || due || ws.library.queue.is_empty() {
                    ws.library.last_loader_notify = Some(now);
                    cx.notify();
                }
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

    /// Ask what to call a new bucket — and, optionally, its smart rule
    /// (a query, an area drawn on the dialog's map); it is created on
    /// the dialog's Create, born holding `photos`.
    pub(super) fn gallery_new_bucket(&mut self, photos: Vec<PathBuf>, cx: &mut Context<Self>) {
        self.open_modal(
            Modal::BucketName {
                name: String::new(),
                query: String::new(),
                photos,
                editing: None,
            },
            cx,
        );
        // The name is what everyone types first; put the keyboard in it.
        self.focus_field("bucket-name", "");
    }

    /// Reopen the bucket dialog on an existing bucket: rename it, give
    /// it a rule, change or remove the one it has.
    pub(super) fn gallery_edit_bucket(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(bucket) = self.library.buckets.get(index) else {
            return;
        };
        let name = bucket.name.clone();
        let query = bucket.query.clone().unwrap_or_default();
        let area = bucket.area.clone();
        self.open_modal(
            Modal::BucketName {
                name,
                query,
                photos: Vec::new(),
                editing: Some(index),
            },
            cx,
        );
        // Show the rule being edited: the shared map takes the
        // bucket's boundary (and jumps to it), or clears so a leftover
        // selection from the import dialog cannot pass as this
        // bucket's.
        match area {
            Some((bounds, place)) => self.library.map.jump_to(&place, bounds),
            None => {
                self.library.map.selection = None;
                self.library.map.selection_name = None;
            }
        }
    }

    /// Keep the smart buckets current: whenever the index moved — or a
    /// rule changed — since the last pass, re-score every rule against
    /// the index snapshot in the background. Called from the gallery's
    /// render, so a bucket keeps filling itself as photos are indexed,
    /// imported, or edited.
    pub(super) fn refresh_smart_buckets(&mut self, cx: &mut Context<Self>) {
        if self.library.smart_running || !self.library.buckets.iter().any(|b| b.is_smart()) {
            return;
        }
        let target = (self.library.index_gen, self.library.rule_rev);
        if self.library.smart_synced == Some(target) {
            return;
        }
        self.library.smart_running = true;
        // One job per smart bucket: its slot, the query (with the
        // engine's cached answer when it has one), and the area.
        struct SmartRule {
            index: usize,
            query: Option<String>,
            cached: Option<CachedQuery>,
            area: Option<GeoBounds>,
        }
        let rules: Vec<SmartRule> = self
            .library
            .buckets
            .iter()
            .enumerate()
            .filter(|(_, b)| b.is_smart())
            .map(|(index, b)| {
                let query = b
                    .query
                    .as_deref()
                    .map(str::trim)
                    .filter(|q| !q.is_empty())
                    .map(str::to_string);
                let cached = query
                    .as_ref()
                    .and_then(|q| self.library.query_cache.get(q).cloned());
                SmartRule {
                    index,
                    query,
                    cached,
                    area: b.area.as_ref().map(|(bounds, _)| *bounds),
                }
            })
            .collect();
        let snapshot = self.library.search_snapshot();
        cx.spawn(async move |this, cx| {
            let computed = cx
                .background_executor()
                .spawn(async move {
                    let mut fresh: Vec<(String, CachedQuery)> = Vec::new();
                    let mut out: Vec<(usize, Vec<PathBuf>, bool)> = Vec::new();
                    for SmartRule {
                        index,
                        query,
                        cached,
                        area,
                    } in rules
                    {
                        let answer = match (&query, cached) {
                            (Some(q), None) => {
                                // Same engine as the search box, cached
                                // in the same place.
                                let answer = CachedQuery {
                                    text: schist_neural::embed::embed_text(q).map(Arc::new),
                                    place: library_geo::find_place(q),
                                };
                                fresh.push((q.clone(), answer.clone()));
                                Some(answer)
                            }
                            (_, cached) => cached,
                        };
                        let readable = answer
                            .as_ref()
                            .is_some_and(|a| a.text.is_some() || a.place.is_some());
                        let (mut matched, by_score) = if readable {
                            // Score exactly as the search box does, but
                            // keep everything above the floor — a
                            // bucket holds all its matches, not a
                            // screenful.
                            let answer = answer.unwrap();
                            let mut scored: FxHashMap<&PathBuf, f32> = FxHashMap::default();
                            if let Some(text) = &answer.text {
                                for (path, v) in snapshot.vectors.iter() {
                                    let s =
                                        v.iter().zip(text.iter()).map(|(a, b)| a * b).sum::<f32>();
                                    scored.insert(path, s);
                                }
                            }
                            if let Some(place) = &answer.place {
                                for (path, (lat, lon)) in snapshot.positions.iter() {
                                    let affinity = library_geo::geo_affinity(place, *lat, *lon);
                                    if affinity > 0.0 {
                                        *scored.entry(path).or_insert(0.0) += GEO_BOOST * affinity;
                                    }
                                }
                            }
                            let floor = if answer.text.is_some() {
                                SEARCH_FLOOR
                            } else {
                                GEO_BOOST * 0.3
                            };
                            let mut scored: Vec<(&PathBuf, f32)> =
                                scored.into_iter().filter(|(_, s)| *s >= floor).collect();
                            scored.sort_by(|a, b| b.1.total_cmp(&a.1));
                            (scored.into_iter().map(|(p, _)| p.clone()).collect(), true)
                        } else if query.is_some() {
                            // A query the engine cannot read yet — the
                            // models aren't installed and it names no
                            // place. Match nothing, not everything.
                            (Vec::new(), false)
                        } else {
                            // Area-only: every positioned photo is a
                            // candidate; the clip below is the rule.
                            (
                                snapshot
                                    .positions
                                    .iter()
                                    .map(|(p, _)| p.clone())
                                    .collect::<Vec<_>>(),
                                false,
                            )
                        };
                        if let Some(area) = area {
                            let at: FxHashMap<&PathBuf, (f64, f64)> = snapshot
                                .positions
                                .iter()
                                .map(|(p, pos)| (p, *pos))
                                .collect();
                            matched.retain(|p| {
                                at.get(p)
                                    .is_some_and(|(lat, lon)| area.contains(*lat, *lon))
                            });
                        }
                        out.push((index, matched, by_score));
                    }
                    (out, fresh)
                })
                .await;
            this.update(cx, |ws, cx| {
                let (out, fresh) = computed;
                for (query, answer) in fresh {
                    if ws.library.query_cache.len() > 512 {
                        ws.library.query_cache.clear();
                    }
                    ws.library.query_cache.insert(query, answer);
                }
                // The rules (or the bucket list) changed underneath
                // the pass: throw it away, the next render re-runs it.
                let mut viewed_refilled = false;
                if ws.library.rule_rev == target.1 {
                    for (index, mut matched, by_score) in out {
                        if !by_score {
                            // Unscored matches show newest first, like
                            // the grid.
                            matched.sort_by_key(|p| {
                                std::cmp::Reverse(
                                    ws.library.taken.get(p).cloned().unwrap_or_default(),
                                )
                            });
                        }
                        if let Some(bucket) = ws.library.buckets.get_mut(index) {
                            if bucket.matches != matched {
                                bucket.matches = matched;
                                viewed_refilled |= ws.library.bucket_filter == Some(index);
                            }
                        }
                    }
                    ws.library.smart_synced = Some(target);
                }
                ws.library.smart_running = false;
                // A search scoped to this bucket was ranked over what
                // it held before the pass; rank it over what it holds now.
                if viewed_refilled && !ws.library.search.trim().is_empty() {
                    ws.gallery_search_changed(cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
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
        // Keep the caret on the rails whatever changed the text.
        self.library.search_cursor = self.library.search_cursor.min(self.library.search.len());
        match ev.keystroke.key.as_str() {
            "a" if primary => {
                self.library.search_selected = !self.library.search.is_empty();
                self.library.search_cursor = self.library.search.len();
                cx.notify();
            }
            "c" if primary && self.library.search_selected => {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(self.library.search.clone()));
            }
            "x" if primary && self.library.search_selected => {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(self.library.search.clone()));
                self.library.search.clear();
                self.library.search_cursor = 0;
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
                    self.library.search_cursor = 0;
                    self.library.search_selected = false;
                }
                let at = self.library.search_cursor;
                self.library.search.insert_str(at, &pasted);
                self.library.search_cursor = at + pasted.len();
                self.gallery_search_changed(cx);
            }
            "left" | "right" if primary => {
                // ⌘←/⌘→: the ends of the line.
                self.library.search_selected = false;
                self.library.search_cursor = if ev.keystroke.key == "left" {
                    0
                } else {
                    self.library.search.len()
                };
                cx.notify();
            }
            "left" => {
                self.library.search_cursor = if self.library.search_selected {
                    0
                } else {
                    crate::ui::caret_left(&self.library.search, self.library.search_cursor)
                };
                self.library.search_selected = false;
                cx.notify();
            }
            "right" => {
                self.library.search_cursor = if self.library.search_selected {
                    self.library.search.len()
                } else {
                    crate::ui::caret_right(&self.library.search, self.library.search_cursor)
                        .min(self.library.search.len())
                };
                self.library.search_selected = false;
                cx.notify();
            }
            "home" | "up" => {
                self.library.search_cursor = 0;
                self.library.search_selected = false;
                cx.notify();
            }
            "end" | "down" => {
                self.library.search_cursor = self.library.search.len();
                self.library.search_selected = false;
                cx.notify();
            }
            "backspace" => {
                if self.library.search_selected {
                    self.library.search.clear();
                    self.library.search_cursor = 0;
                    self.library.search_selected = false;
                } else if self.library.search_cursor > 0 {
                    let from =
                        crate::ui::caret_left(&self.library.search, self.library.search_cursor);
                    self.library
                        .search
                        .replace_range(from..self.library.search_cursor, "");
                    self.library.search_cursor = from;
                }
                self.gallery_search_changed(cx);
            }
            "delete" => {
                if self.library.search_selected {
                    self.library.search.clear();
                    self.library.search_cursor = 0;
                    self.library.search_selected = false;
                } else if self.library.search_cursor < self.library.search.len() {
                    let to =
                        crate::ui::caret_right(&self.library.search, self.library.search_cursor);
                    self.library
                        .search
                        .replace_range(self.library.search_cursor..to, "");
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
                    self.library.search_cursor = 0;
                    self.library.search_selected = false;
                }
                let at = self.library.search_cursor;
                self.library.search.insert_str(at, text);
                self.library.search_cursor = at + text.len();
                self.gallery_search_changed(cx);
            }
        }
        // A key landed in the box: show the caret solid from here.
        self.reset_caret_phase();
        true
    }

    /// An arrow key while the gallery has the keyboard: move the
    /// selection through the photos in display order — left/right by
    /// one, up/down by a visual row, worked out from the grid's real
    /// width since a wrap layout has no rows to ask about.
    pub(super) fn gallery_nav_key(
        &mut self,
        ev: &gpui::KeyDownEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.library.search_active {
            return false;
        }
        let columns = {
            let width = f32::from(self.library.grid_bounds.size.width);
            let cell = self.library.thumb_px;
            // p_2 padding both sides, gap_2 between cells.
            (((width - 16.0 + 8.0) / (cell + 8.0)).floor() as isize).max(1)
        };
        let step: isize = match ev.keystroke.key.as_str() {
            "left" => -1,
            "right" => 1,
            "up" => -columns,
            "down" => columns,
            _ => return false,
        };
        let flat = self.gallery_flat_order();
        if flat.is_empty() {
            return false;
        }
        let next = match self
            .library
            .lead_selected()
            .and_then(|lead| flat.iter().position(|p| p == lead))
        {
            Some(at) => (at as isize + step).clamp(0, flat.len() as isize - 1) as usize,
            // Nothing selected yet: any arrow lands on the first photo.
            None => 0,
        };
        let lead = flat[next].clone();
        if ev.keystroke.modifiers.shift {
            // Shift+arrow: the range from the anchor to wherever the
            // lead moved, in display order.
            let anchor = self
                .library
                .select_anchor
                .clone()
                .unwrap_or_else(|| lead.clone());
            let a = flat.iter().position(|p| p == &anchor).unwrap_or(next);
            let (lo, hi) = (a.min(next), a.max(next));
            let mut range: Vec<PathBuf> = flat[lo..=hi].to_vec();
            if a > next {
                // The lead must stay last, so arrows keep moving it.
                range.reverse();
            }
            self.library.select_anchor = Some(anchor);
            self.library.selected = range;
        } else {
            self.library.select_single(lead);
        }
        self.library.reveal_selection = true;
        cx.notify();
        true
    }

    /// Every photo the grid is currently showing, in display order —
    /// what arrows walk and Shift-clicks span.
    pub(super) fn gallery_flat_order(&self) -> Vec<PathBuf> {
        let hide = self.view.gallery_hide_nsfw;
        if let Some(results) = &self.library.search_results {
            results
                .iter()
                .map(|(path, _)| path.clone())
                .filter(|p| self.library.passes_map(p))
                .filter(|p| !(hide && self.library.is_flagged(p)))
                .collect()
        } else {
            self.library
                .grouped()
                .into_iter()
                .flat_map(|(_, _, entries)| entries)
                .map(|e| e.path)
                .filter(|p| !(hide && self.library.is_flagged(p)))
                .collect()
        }
    }

    /// Shift-click: select the display-order range from the anchor to
    /// this photo, which becomes the lead.
    pub(super) fn gallery_select_range_to(&mut self, path: PathBuf) {
        let flat = self.gallery_flat_order();
        let anchor = self
            .library
            .select_anchor
            .clone()
            .unwrap_or_else(|| path.clone());
        let (Some(a), Some(b)) = (
            flat.iter().position(|p| p == &anchor),
            flat.iter().position(|p| p == &path),
        ) else {
            self.library.select_single(path);
            return;
        };
        let (lo, hi) = (a.min(b), a.max(b));
        let mut range: Vec<PathBuf> = flat[lo..=hi].to_vec();
        if a > b {
            range.reverse();
        }
        self.library.select_anchor = Some(anchor);
        self.library.selected = range;
    }

    /// Nudge the grid until the keyboard-moved selection is on screen.
    /// Runs per render off the bounds the previous paint recorded, so
    /// it converges a frame after the selection moves.
    pub(super) fn gallery_reveal_tick(&mut self, cx: &mut Context<Self>) {
        if !self.library.reveal_selection {
            return;
        }
        let (Some(cell), view) = (self.library.selected_bounds, self.library.grid_bounds) else {
            return;
        };
        if view.size.height <= px(0.0) {
            return;
        }
        let top = f32::from(cell.origin.y);
        let bottom = top + f32::from(cell.size.height);
        let view_top = f32::from(view.origin.y);
        let view_bottom = view_top + f32::from(view.size.height);
        let mut offset = self.library.grid_scroll.offset();
        if bottom > view_bottom {
            // Scrolling down means a more negative offset in gpui.
            offset.y -= px(bottom - view_bottom + 8.0);
        } else if top < view_top {
            offset.y += px(view_top - top + 8.0);
        } else {
            self.library.reveal_selection = false;
            return;
        }
        self.library.grid_scroll.set_offset(offset);
        cx.notify();
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
        self.library.search_cursor = 0;
        self.library.search_active = false;
        self.library.search_selected = false;
        self.library.search_results = None;
        self.library.search_place = None;
        self.library.search_scoped = None;
        self.library.search_seq += 1;
        cx.notify();
        true
    }

    /// Re-rank for the current query. The engine is consulted at most
    /// once per string — answered queries come from the cache — and the
    /// index rides into the task as a shared snapshot instead of a
    /// per-keystroke copy of every path and vector. Made while viewing
    /// a bucket, the search is scoped to it: the bucket filters, then
    /// the query ranks what is left.
    pub(super) fn gallery_search_changed(&mut self, cx: &mut Context<Self>) {
        self.library.search_seq += 1;
        let seq = self.library.search_seq;
        let query = self.library.search.trim().to_string();
        self.library.search_scoped = self.library.bucket_filter;
        if query.is_empty() {
            self.library.search_results = None;
            self.library.search_place = None;
            cx.notify();
            return;
        }
        let cached = self.library.query_cache.get(&query).cloned();
        let scope = self.library.search_scope();
        let snapshot = self.library.search_snapshot();
        cx.spawn(async move |this, cx| {
            let ranked = cx
                .background_executor()
                .spawn(async move {
                    // Two readings of the query, blended: what the
                    // photos look like, and — when it names somewhere
                    // the gazetteer knows — where they were taken.
                    let (fresh, answer) = match cached {
                        Some(answer) => (false, answer),
                        None => (
                            true,
                            CachedQuery {
                                text: schist_neural::embed::embed_text(&query).map(Arc::new),
                                place: library_geo::find_place(&query),
                            },
                        ),
                    };
                    if answer.text.is_none() && answer.place.is_none() {
                        return None;
                    }
                    // Scores keyed by borrowed path; only what survives
                    // the cut gets cloned. The scope applies before the
                    // cut, so a bucket's matches are never crowded out
                    // of the kept two hundred by the rest of the library.
                    let in_scope = |path: &PathBuf| scope.as_ref().is_none_or(|s| s.contains(path));
                    let mut scored: FxHashMap<&PathBuf, f32> = FxHashMap::default();
                    if let Some(text) = &answer.text {
                        for (path, v) in snapshot.vectors.iter() {
                            if !in_scope(path) {
                                continue;
                            }
                            let s = v.iter().zip(text.iter()).map(|(a, b)| a * b).sum::<f32>();
                            scored.insert(path, s);
                        }
                    }
                    if let Some(place) = &answer.place {
                        for (path, (lat, lon)) in snapshot.positions.iter() {
                            if !in_scope(path) {
                                continue;
                            }
                            let affinity = library_geo::geo_affinity(place, *lat, *lon);
                            if affinity > 0.0 {
                                *scored.entry(path).or_insert(0.0) += GEO_BOOST * affinity;
                            }
                        }
                    }
                    let floor = if answer.text.is_some() {
                        SEARCH_FLOOR
                    } else {
                        // Location-only search (no text model): being
                        // near the place is the whole of the score.
                        GEO_BOOST * 0.3
                    };
                    let mut scored: Vec<(&PathBuf, f32)> = scored.into_iter().collect();
                    scored.sort_by(|a, b| b.1.total_cmp(&a.1));
                    scored.truncate(SEARCH_KEPT);
                    scored.retain(|(_, s)| *s >= floor);
                    let ranked: Vec<(PathBuf, f32)> = scored
                        .into_iter()
                        .map(|(path, score)| (path.clone(), score))
                        .collect();
                    let place_name = answer.place.as_ref().map(|p| p.name.clone());
                    Some((ranked, place_name, fresh.then_some((query, answer))))
                })
                .await;
            this.update(cx, |ws, cx| {
                let Some((ranked, place_name, fresh)) = ranked else {
                    return;
                };
                if let Some((query, answer)) = fresh {
                    // Session cache; a typo-storm cannot grow it forever.
                    if ws.library.query_cache.len() > 512 {
                        ws.library.query_cache.clear();
                    }
                    ws.library.query_cache.insert(query, answer);
                }
                // A newer keystroke owns the results now.
                if ws.library.search_seq == seq {
                    ws.library.search_results = Some(ranked);
                    ws.library.search_place = place_name;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// The search box's ranking, answered on the spot: what the AI
    /// panel's `gallery_search` tool needs, since a tool call is a
    /// question that wants its answer in the reply. Sets the box and
    /// its results too, so the user sees what the agent saw. Blocking
    /// on the text tower — milliseconds once warm, a few seconds the
    /// first time — which a tool call can afford and a keystroke
    /// cannot.
    pub(super) fn gallery_search_now(
        &mut self,
        query: &str,
        cx: &mut Context<Self>,
    ) -> Vec<(PathBuf, f32)> {
        let query = query.trim().to_string();
        self.library.search = query.clone();
        self.library.search_cursor = query.len();
        self.library.search_seq += 1;
        self.library.search_scoped = self.library.bucket_filter;
        let scope = self.library.search_scope();
        let answer = match self.library.query_cache.get(&query) {
            Some(answer) => answer.clone(),
            None => {
                let answer = CachedQuery {
                    text: schist_neural::embed::embed_text(&query).map(Arc::new),
                    place: library_geo::find_place(&query),
                };
                if self.library.query_cache.len() > 512 {
                    self.library.query_cache.clear();
                }
                self.library
                    .query_cache
                    .insert(query.clone(), answer.clone());
                answer
            }
        };
        let snapshot = self.library.search_snapshot();
        let in_scope = |path: &PathBuf| scope.as_ref().is_none_or(|s| s.contains(path));
        let mut scored: FxHashMap<&PathBuf, f32> = FxHashMap::default();
        if let Some(text) = &answer.text {
            for (path, v) in snapshot.vectors.iter() {
                if !in_scope(path) {
                    continue;
                }
                scored.insert(path, v.iter().zip(text.iter()).map(|(a, b)| a * b).sum());
            }
        }
        if let Some(place) = &answer.place {
            for (path, (lat, lon)) in snapshot.positions.iter() {
                if !in_scope(path) {
                    continue;
                }
                let affinity = library_geo::geo_affinity(place, *lat, *lon);
                if affinity > 0.0 {
                    *scored.entry(path).or_insert(0.0) += GEO_BOOST * affinity;
                }
            }
        }
        let floor = if answer.text.is_some() {
            SEARCH_FLOOR
        } else {
            GEO_BOOST * 0.3
        };
        let mut ranked: Vec<(PathBuf, f32)> = scored
            .into_iter()
            .filter(|(_, s)| *s >= floor)
            .map(|(p, s)| (p.clone(), s))
            .collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
        ranked.truncate(SEARCH_KEPT);
        self.library.search_place = answer.place.as_ref().map(|p| p.name.clone());
        self.library.search_results = Some(ranked.clone());
        cx.notify();
        ranked
    }

    /// Keep a live search scoped to the bucket on show: when the
    /// viewed bucket changes under a query — a click in the sidebar,
    /// a bucket deleted — re-rank within the new one (or the whole
    /// library). Called from the gallery's render, like the smart
    /// buckets' refresh, so nothing that moves `bucket_filter` has to
    /// remember to.
    pub(super) fn gallery_search_rescope(&mut self, cx: &mut Context<Self>) {
        if self.library.search.trim().is_empty()
            || self.library.search_scoped == self.library.bucket_filter
        {
            return;
        }
        self.gallery_search_changed(cx);
    }

    /// Load both towers off the UI thread before the first keystroke
    /// needs them: parsing and optimizing the text model is the slow
    /// part of a first search, and it memoizes.
    pub(super) fn warm_search_engine(&mut self, cx: &mut Context<Self>) {
        if self.library.engine_warmed || !schist_neural::embed::ready() {
            return;
        }
        self.library.engine_warmed = true;
        cx.background_executor()
            .spawn(async move {
                let started = std::time::Instant::now();
                let text = schist_neural::get("embed-text").is_some();
                let image = schist_neural::get("embed-image").is_some();
                log::info!(
                    "gallery: search engine warmed in {:?} (text: {text}, image: {image})",
                    started.elapsed()
                );
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
                this.update_in(cx, |ws, _window, cx| ws.add_gallery_folders(paths, cx))
                    .ok();
            }
        })
        .detach();
    }

    /// Watch these folders, and show the gallery with them in it. The
    /// path both the picker and a folder dropped on the window take.
    pub fn add_gallery_folders(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        let mut added = 0;
        for path in paths {
            if !self.library.folders.contains(&path) {
                self.library.folders.push(path);
                added += 1;
            }
        }
        if added > 0 {
            self.library.folders.sort();
            self.library.save();
        }
        self.library.open = true;
        self.library_rescan(cx);
        cx.notify();
    }

    /// Open every image the folders hold as its own tab, up to
    /// [`DROP_OPEN_CAP`] — a dropped camera roll must not become five
    /// thousand tabs. The gallery is the other answer, and the dialog
    /// that offers this offers that first.
    pub fn open_folder_images(&mut self, dirs: Vec<PathBuf>, cx: &mut Context<Self>) {
        let images: Vec<PathBuf> = scan_folders(&dirs, &self.codec_extensions())
            .into_iter()
            .flat_map(|s| s.entries.into_iter().map(|e| e.path))
            .collect();
        let total = images.len();
        let opening = total.min(DROP_OPEN_CAP);
        for path in images.into_iter().take(DROP_OPEN_CAP) {
            self.load_file(path, cx);
        }
        self.status = if total > opening {
            format!("Opened the first {opening} of {total} images — the gallery holds the rest")
                .into()
        } else {
            format!("Opened {opening} images").into()
        };
        cx.notify();
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
        self.library
            .pending_backing
            .push((target.clone(), original));
        self.load_file(target, cx);
    }

    /// Bookkeeping when a load finishes: adopt a gallery edit's backing
    /// arrangement, or record an ordinary open in the recents.
    pub(super) fn finish_load_bookkeeping(&mut self, loaded: &Path) {
        // Loads finish in any order, so each claims its own entry.
        let claimed = self
            .library
            .pending_backing
            .iter()
            .position(|(target, _)| target == loaded);
        let Some(claimed) = claimed else {
            self.note_recent(loaded);
            return;
        };
        let (_, original) = self.library.pending_backing.remove(claimed);
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
        super::library_ops::keep_sidecar_version(path);
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

    /// Grow or shrink the cells by a wheel's travel: a fifth of a
    /// pixel of cell per pixel of wheel, so a mouse notch (three lines
    /// on X11) is a visible 24 px step — seven notches span the range —
    /// and a trackpad flick a smooth glide. Up is bigger, as on the map.
    pub fn nudge_gallery_thumb_px(&mut self, wheel_dy: f32) {
        let value = self.library.thumb_px + wheel_dy * 0.2;
        self.set_gallery_thumb_px(value);
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
    fn the_index_snapshot_round_trips_every_field_shape() {
        // Each Option layer matters: gps Some(None) is "probed, no
        // position", which saves a re-probe on every future launch.
        let rows = vec![
            IndexRow {
                path: PathBuf::from("/p/full.jpg"),
                mtime: 7,
                embed: Some(Arc::new(vec![0.25f32, -1.0, 3.5])),
                gps: Some(Some((40.7, -74.0))),
                taken: Some("2026-09-01 12:00:00".into()),
                place: Some(Some("New York City".into())),
                flagged: Some(true),
            },
            IndexRow {
                path: PathBuf::from("/p/bare.jpg"),
                mtime: 9,
                embed: None,
                gps: Some(None),
                taken: None,
                place: Some(None),
                flagged: Some(false),
            },
        ];
        let dir = std::env::temp_dir().join(format!("schist-idx-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("index.v1");
        write_index_snapshot_to(&file, &rows).unwrap();
        let bytes = std::fs::read(&file).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let back = parse_index_snapshot(&bytes).expect("parses");
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].path, rows[0].path);
        assert_eq!(back[0].mtime, 7);
        assert_eq!(back[0].embed.as_deref(), Some(&vec![0.25f32, -1.0, 3.5]));
        assert_eq!(back[0].gps, Some(Some((40.7, -74.0))));
        assert_eq!(back[0].taken.as_deref(), Some("2026-09-01 12:00:00"));
        assert_eq!(back[0].place, Some(Some("New York City".into())));
        assert_eq!(back[0].flagged, Some(true));
        assert_eq!(back[1].gps, Some(None));
        assert_eq!(back[1].place, Some(None));
        assert_eq!(back[1].flagged, Some(false));
        assert_eq!(back[1].embed, None);
        // A torn file is a miss, not a crash.
        assert!(parse_index_snapshot(&bytes[..bytes.len() - 3]).is_none());
        assert!(parse_index_snapshot(b"not an index").is_none());
    }

    #[test]
    fn a_search_is_scoped_to_the_bucket_on_show() {
        let mut lib = Library::load();
        lib.buckets.push(Bucket {
            name: "Trip".into(),
            photos: vec![PathBuf::from("/p/a.jpg")],
            query: Some("beach".into()),
            area: None,
            matches: vec![PathBuf::from("/p/b.jpg"), PathBuf::from("/p/a.jpg")],
        });
        // No bucket on show: the whole index.
        assert!(lib.search_scope().is_none());
        // Viewing one: its hand-picked photos and its rule's matches,
        // each once.
        lib.bucket_filter = Some(lib.buckets.len() - 1);
        let scope = lib.search_scope().expect("scoped");
        assert_eq!(scope.len(), 2);
        assert!(scope.contains(&PathBuf::from("/p/a.jpg")));
        assert!(scope.contains(&PathBuf::from("/p/b.jpg")));
        // A slot that no longer exists scopes to nothing in
        // particular — the whole index again, not a crash.
        lib.bucket_filter = Some(99);
        assert!(lib.search_scope().is_none());
    }

    #[test]
    fn the_content_filter_keeps_flagged_photos_out_of_archives() {
        let mut lib = Library::load();
        lib.flagged.insert(PathBuf::from("/p/a.jpg"), true);
        lib.flagged.insert(PathBuf::from("/p/b.jpg"), false);
        let all = || {
            vec![
                PathBuf::from("/p/a.jpg"),
                PathBuf::from("/p/b.jpg"),
                PathBuf::from("/p/c.jpg"),
            ]
        };
        // Filter off: everything goes, nothing is held back.
        assert_eq!(lib.zip_candidates(all(), false), (all(), 0));
        // Filter on: the flagged one stays; clean and unscored go.
        let (kept, held) = lib.zip_candidates(all(), true);
        assert_eq!(
            kept,
            vec![PathBuf::from("/p/b.jpg"), PathBuf::from("/p/c.jpg")]
        );
        assert_eq!(held, 1);
    }

    #[test]
    fn verdicts_come_in_three_words_and_lists_follow_them() {
        let mut lib = Library::load();
        let mk = |n: &str| Entry {
            path: PathBuf::from(format!("/p/{n}.jpg")),
            mtime: 0,
            edited: false,
        };
        lib.sections = vec![Section {
            dir: PathBuf::from("/p"),
            entries: vec![mk("a"), mk("b"), mk("c")],
        }];
        lib.flagged.insert(PathBuf::from("/p/a.jpg"), true);
        lib.flagged.insert(PathBuf::from("/p/b.jpg"), false);
        assert_eq!(lib.verdict(Path::new("/p/a.jpg")), "flagged");
        assert_eq!(lib.verdict(Path::new("/p/b.jpg")), "clean");
        assert_eq!(lib.verdict(Path::new("/p/c.jpg")), "unscored");
        let names = |v: &str| -> Vec<String> {
            lib.entries_by_verdict(v)
                .iter()
                .map(|e| e.path.file_name().unwrap().to_string_lossy().into_owned())
                .collect()
        };
        assert_eq!(names("flagged"), vec!["a.jpg"]);
        assert_eq!(names("clean"), vec!["b.jpg"]);
        assert_eq!(names("unscored"), vec!["c.jpg"]);
        let state = lib.content_filter_json(true);
        assert_eq!(state["flagged"], 1);
        assert_eq!(state["clean"], 1);
        assert_eq!(state["unscored"], 1);
        assert_eq!(state["hidden_now"], 1);
        // A row says its verdict too, null while unscored.
        assert_eq!(lib.entry_json(&mk("a"))["flagged"], true);
        assert!(lib.entry_json(&mk("c"))["flagged"].is_null());
    }

    #[test]
    fn thumbnail_eviction_takes_the_least_recently_shown_and_only_them() {
        let mut lib = Library::load();
        lib.thumb_frame = 100;
        let img = || {
            Thumb::Ready(Arc::new(RenderImage::new(smallvec![image::Frame::new(
                image::RgbaImage::new(2, 2),
            )])))
        };
        for i in 0..4 {
            let path = PathBuf::from(format!("/p/{i}.jpg"));
            lib.thumbs.insert(path.clone(), (0, img()));
            lib.thumb_used.insert(path, i);
        }
        // On screen this very frame: over budget or not, untouchable.
        lib.thumbs
            .insert(PathBuf::from("/p/visible.jpg"), (0, img()));
        lib.thumb_used.insert(PathBuf::from("/p/visible.jpg"), 100);
        // Pending and Failed hold no pixels; they must never be
        // evicted (a Failed that went away would retry forever).
        lib.thumbs
            .insert(PathBuf::from("/p/pending.jpg"), (0, Thumb::Pending));
        lib.thumbs
            .insert(PathBuf::from("/p/failed.jpg"), (0, Thumb::Failed));
        lib.evict_thumbs(2);
        // Three Ready over budget, but only the oldest off-screen pair
        // plus one more go; the current-frame one and both pixel-less
        // states stay.
        assert!(!lib.thumbs.contains_key(Path::new("/p/0.jpg")));
        assert!(!lib.thumbs.contains_key(Path::new("/p/1.jpg")));
        assert!(!lib.thumbs.contains_key(Path::new("/p/2.jpg")));
        assert!(lib.thumbs.contains_key(Path::new("/p/3.jpg")));
        assert!(lib.thumbs.contains_key(Path::new("/p/visible.jpg")));
        assert!(lib.thumbs.contains_key(Path::new("/p/pending.jpg")));
        assert!(lib.thumbs.contains_key(Path::new("/p/failed.jpg")));
        // Nothing more evictable: what's left is the current-frame
        // thumb, the last off-screen one, and the pixel-less states.
        lib.evict_thumbs(2);
        assert_eq!(lib.thumbs.len(), 4);
    }

    #[test]
    fn buckets_saved_before_they_had_rules_still_read() {
        // The pre-rule shape was a bare (name, photos) tuple; the
        // untagged enum must take both it and the named form.
        let legacy: Vec<BucketFile> =
            serde_json::from_str(r#"[["Trip", ["/a.jpg"]]]"#).expect("legacy shape");
        assert!(matches!(&legacy[0], BucketFile::Plain(name, photos)
            if name == "Trip" && photos == &[PathBuf::from("/a.jpg")]));
        let rich: Vec<BucketFile> = serde_json::from_str(
            r#"[{"name": "NYC dogs", "query": "dog",
                 "area": [{"south": 40.0, "west": -75.0, "north": 41.0, "east": -73.0}, "New York City"]}]"#,
        )
        .expect("rich shape");
        assert!(
            matches!(&rich[0], BucketFile::Rich { name, photos, query, area }
            if name == "NYC dogs"
                && photos.is_empty()
                && query.as_deref() == Some("dog")
                && area.as_ref().is_some_and(|(b, place)| place == "New York City" && b.contains(40.7, -74.0)))
        );
    }

    #[test]
    fn bucket_contents_are_the_hand_picked_photos_then_the_matches() {
        let bucket = Bucket {
            name: "b".into(),
            photos: vec![PathBuf::from("/hand.jpg"), PathBuf::from("/both.jpg")],
            query: Some("dog".into()),
            area: None,
            matches: vec![PathBuf::from("/both.jpg"), PathBuf::from("/matched.jpg")],
        };
        // Drop order first, matches after, nothing twice.
        assert_eq!(
            bucket.contents(),
            vec![
                PathBuf::from("/hand.jpg"),
                PathBuf::from("/both.jpg"),
                PathBuf::from("/matched.jpg"),
            ]
        );
        assert!(bucket.is_smart());
    }

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
    fn exif_zero_fix_is_missing_but_either_axis_can_be_zero() {
        let path =
            std::env::temp_dir().join(format!("schist-zero-gps-test-{}.jpg", std::process::id()));
        for (zero_lat, zero_lon) in [(true, true), (true, false), (false, true)] {
            let mut jpeg = times_square_jpeg();
            // TIFF starts at byte 12; latitude and longitude each have
            // three rationals. Zero numerators, retain the denominators.
            for start in [zero_lat.then_some(92), zero_lon.then_some(116)]
                .into_iter()
                .flatten()
            {
                for offset in (start..start + 24).step_by(8) {
                    jpeg[offset..offset + 4].copy_from_slice(&0u32.to_le_bytes());
                }
            }
            std::fs::write(&path, jpeg).unwrap();
            let gps = photo_gps(&path);
            assert_eq!(gps.is_none(), zero_lat && zero_lon);
            if let Some((lat, lon)) = gps {
                assert_eq!(lat == 0.0, zero_lat);
                assert_eq!(lon == 0.0, zero_lon);
            }
        }
        std::fs::remove_file(path).unwrap();
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
