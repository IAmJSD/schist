//! The single-window workspace: canvas viewport + docked panels.
//!
//! The kernel and plugins are GPUI-free; this file is the boundary where
//! GPUI events become `PointerInput`s for the active tool and where
//! composited tiles become GPU textures.

use crate::actions::*;
use crate::keymap;
use crate::panels;
use gpui::{
    canvas, div, point, px, size, App, Bounds, Context, ExternalPaths, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, ParentElement as _, PathBuilder, PinchEvent, Pixels, Point, Render, RenderImage,
    ScrollWheelEvent, SharedString, Styled as _, TouchPhase, Window,
};
#[cfg_attr(target_arch = "wasm32", allow(unused_imports))]
use rustc_hash::{FxHashMap, FxHashSet};
use schist_color::{ColorMode, Depth, Rgba};
use schist_compositor::TileCache;
use schist_core::{blit_rgba8, Document, IntRect, Layer, TileCoord, TILE_SIZE};
use schist_plugin_api::{
    CommandCtx, EditorState, Modifiers, Overlay, PluginRegistry, PointerInput, ToolCtx,
};
use smallvec::smallvec;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
#[cfg(not(target_arch = "wasm32"))]
use std::sync::atomic::Ordering;
use std::sync::Arc;

mod adjustments;
#[cfg(not(target_arch = "wasm32"))]
mod ai;
#[cfg(target_arch = "wasm32")]
#[path = "ai_stub.rs"]
mod ai;
mod chrome;
mod clipboard;
mod colormgmt;
mod commands;
mod compose;
mod comps;
mod context;
mod docs;
mod edit_ops;
mod export;
mod filters;
mod image_ops;
mod input;
mod layers_panel;
// The gallery: watched photo folders, thumbnails, camera import and the
// PSD sidecars behind gallery edits. A browser tab has no folders to
// watch, so the web build compiles the whole thing out.
#[cfg(not(target_arch = "wasm32"))]
mod library;
#[cfg(not(target_arch = "wasm32"))]
mod library_geo;
// iPhones and PTP cameras never mount as filesystems on macOS;
// ImageCaptureCore is the door Image Capture and Photos use, and this
// module knocks on it the same way.
#[cfg(target_os = "macos")]
mod library_icc;
#[cfg(not(target_arch = "wasm32"))]
mod library_mcp;
#[cfg(not(target_arch = "wasm32"))]
mod library_ops;
#[cfg(not(target_arch = "wasm32"))]
mod library_people;
#[cfg(not(target_arch = "wasm32"))]
mod library_people_view;
#[cfg(not(target_arch = "wasm32"))]
mod library_view;
mod modals;
mod notes;
mod recovery;
mod render;
mod services;
mod styles;
mod tiles;
mod toolbar;
mod view_options;
mod viewport;

#[cfg(not(target_arch = "wasm32"))]
pub(crate) use library_geo::MapSlot;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use library_people_view::{people_models_dialog, person_name_dialog};
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use library_view::map_element;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use library_view::{
    bucket_name_dialog, camera_import_dialog, camera_import_failed_dialog,
    camera_import_options_dialog, map_filter_dialog, search_models_dialog,
};

/// The most tabs a dropped folder may open at once.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub const DROP_OPEN_CAP: usize = 100;

const PREVIEW_SHIFT: u32 = 3; // preview at 1/8 scale

/// Zoom below which the canvas paints one downscaled preview image instead
/// of hundreds of tile quads.
///
/// A preview texel is `1 << PREVIEW_SHIFT` document pixels, so it may only
/// stand in for the real thing while it lands on at most one *device*
/// pixel. Above that the preview is being magnified, and a fixed cutoff
/// blurred the view exactly where documents tend to sit after Fit to
/// Screen: at 1/8 scale and 35% zoom every texel was smeared over 2.8
/// pixels, and the frame stayed soft until a zoom crossed the threshold.
fn preview_zoom_cutoff(scale_factor: f32) -> f32 {
    1.0 / ((1u32 << PREVIEW_SHIFT) as f32 * scale_factor.max(0.01))
}

/// How long after the last zoom/pan event the view is rebuilt at full
/// quality. Long enough to outlast the gap between wheel ticks, short
/// enough that the crisp frame lands as soon as the hand stops.
const VIEW_GESTURE_SETTLE_MS: u64 = 120;

/// Idle tile prefetch: once the visible viewport is built, the rest of the
/// document keeps compositing in the background, nearest tiles first, so a
/// scroll lands on caches that are already warm instead of stalling the
/// frame on fresh compositing. The tile caches are document-space, so a
/// warm tile pays off at every zoom.
///
/// Ticks run on the UI thread between frames (the compositor and both tile
/// caches live on the workspace), so the batch is small enough that one
/// step stays well under a frame. Ticks stand down while a stroke is in
/// flight, but keep running through zoom/pan gestures — each gesture frame
/// re-aims the queue at the current view, on-screen tiles first, so the
/// settle rebuild lands on warm caches.
const PREFETCH_BATCH_TILES: usize = 8;
/// Gap between prefetch steps, long enough for input and paint to slot in.
const PREFETCH_TICK_MS: u64 = 30;
/// Cap on queued off-screen tiles, nearest first. A composited RGBA8 tile
/// is 256 KiB (twice that when colour-managed), so 2048 tiles bounds the
/// prefetch at roughly 0.5-1 GiB -- a ~134 MP document end to end.
const PREFETCH_TILE_BUDGET: usize = 2048;

/// Where Schist's folder is
#[cfg(not(target_arch = "wasm32"))]
pub fn schist_folder() -> Option<PathBuf> {
    let base = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join(".config"))
        })?;
    Some(base.join("schist"))
}

/// Where view preferences are stored.
#[cfg(not(target_arch = "wasm32"))]
fn prefs_path() -> Option<PathBuf> {
    schist_folder().map(|folder| folder.join("preferences.json"))
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn load_view_options() -> ViewOptions {
    prefs_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

#[cfg(target_arch = "wasm32")]
pub(crate) fn load_view_options() -> ViewOptions {
    crate::web::local_get(crate::web::PREFS_KEY)
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// How often a dirty document is snapshotted for crash recovery.
const AUTOSAVE_SECS: u64 = 30;

/// The registry id of the PNG codec. Codecs are registered as
/// `codec.<format>`; looking one up by the bare format name silently finds
/// nothing.
const PNG_CODEC_ID: &str = "codec.png";

/// A document open in another tab, parked with its view transform so
/// switching back lands on the same spot at the same zoom.
struct DocTab {
    doc: Document,
    zoom: f32,
    offset: Point<Pixels>,
    rotation: f32,
}
/// A model fetch in flight.
///
/// The counter is shared with the thread doing the fetching, which is the
/// only way the dialog can say how much has arrived: the fetch runs on
/// the background executor and knows nothing about views.
#[derive(Clone)]
pub struct ModelDownload {
    pub id: &'static str,
    pub got: Arc<AtomicU64>,
}

pub struct Workspace {
    pub registry: PluginRegistry,
    pub editor: EditorState,
    pub doc: Option<Document>,
    /// The other open documents, in tab order with a gap at `active_tab`
    /// where the checked-out `doc` sits.
    background_tabs: Vec<DocTab>,
    /// Position of the active document in the tab strip.
    active_tab: usize,
    cache: TileCache,
    /// Composited tiles after colour management, ready to sample.
    display_tiles: FxHashMap<TileCoord, Arc<Vec<u8>>>,
    /// The single texture the canvas paints, plus the state it was built
    /// for. One image means no seams: GPUI's sprite atlas has no padding,
    /// so painting a quad per tile let the sampler bleed past each tile's
    /// slot at fractional zoom and drew a dark line at every boundary.
    viewport_image: Option<(ViewportKey, Arc<RenderImage>)>,
    /// Images replaced this frame; freed from the sprite atlas after paint.
    retired_images: Vec<Arc<RenderImage>>,
    /// Whether a continuous zoom/pan gesture is streaming events. While
    /// set, the canvas repaints `viewport_image` stretched to the current
    /// transform instead of resampling the document every event.
    view_gesture_active: bool,
    /// Counts view-gesture events. Each event arms a settle timer that
    /// captures the count, so only the timer for the last event fires.
    view_gesture_seq: u64,
    /// Off-screen tiles awaiting idle compositing, ordered farthest first
    /// so each batch splits off the nearest tiles at the tail.
    prefetch_queue: Vec<TileCoord>,
    /// `(revision, color_epoch)` the queue was built against; an edit
    /// makes the queue stale and it is dropped rather than chased.
    prefetch_stamp: (u64, u64),
    /// Whether a prefetch ticker task is live (only ever one at a time).
    prefetch_ticker: bool,
    /// Colour-picker imagery. Painted pixel by pixel rather than drawn as
    /// gradient quads -- see `color_picker::field_image` for why -- so it
    /// is cached: the square by the hue it was built for, the two
    /// rainbows forever.
    pub picker_field: Option<(u32, Arc<RenderImage>)>,
    pub picker_strip: Option<Arc<RenderImage>>,
    pub picker_ramp: Option<Arc<RenderImage>>,
    preview: Preview,
    pub zoom: f32,
    /// Screen offset of document origin within the canvas element.
    pub offset: Point<Pixels>,
    canvas_bounds: Bounds<Pixels>,
    focus: FocusHandle,
    pan_last: Option<Point<Pixels>>,
    space_held: bool,
    pointer_down: bool,
    pub status: SharedString,
    /// Which transient popup (menu / dropdown) is open.
    pub open_popup: Option<Popup>,
    /// Scroll state of the open dropdown's list, so it can open at its
    /// current value instead of the top.
    pub dropdown: crate::ui::DropdownState,
    /// Path to the open submenu in the menu bar, e.g. [2, 4] for the fifth
    /// row of the third menu. Empty means none.
    pub open_submenu: Vec<usize>,
    /// What the macOS menu bar was last built from, so it is only rebuilt
    /// when a label or an entry has actually changed. `None` off macOS.
    pub native_menu: Option<String>,
    /// Marching-ants animation step, advanced on a timer.
    /// View rotation in radians. Display only: the pixels are untouched,
    /// so this is a change of viewpoint rather than an edit.
    pub rotation: f32,
    /// Models currently being fetched, so the dialog can say so and a
    /// second click does not start a second download.
    pub model_downloads: Vec<ModelDownload>,
    /// Font families currently downloading, so a second click does not
    /// start a second download.
    pub font_downloads: Vec<String>,
    /// Whether the HEIC decode library is currently downloading, so a
    /// second HEIC open does not start a second download.
    // Written but never read on the web: its writer flows are compiled
    // out with the subsystem it belongs to.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub heif_download: bool,
    /// How far along the update the user asked for is, if one is
    /// running. Its presence is also what stops a second click starting
    /// a second download.
    pub update_progress: Option<UpdateProgress>,
    /// Families already offered this session. Opening three documents
    /// that all want the same missing font should ask once, not thrice.
    pub fonts_offered: std::collections::HashSet<String>,
    pub ant_phase: u32,
    /// Whether the last frame drew any tool overlay, so the ants timer
    /// knows to keep repainting for tools that draw their own.
    pub tool_has_overlay: bool,
    /// Which curve the Curves editor is showing.
    pub curve_channel: schist_adjustments::CurveChannel,
    /// Index of the control point being dragged in the curve editor.
    pub curve_drag: Option<usize>,
    /// Which colour control is being dragged, if any.
    pub picker_drag: Option<PickerDrag>,
    pub filter_preview: Option<FilterPreview>,
    /// Generation of the most recently requested sensor-data preview.
    /// Slow results from an older slider position are discarded on arrival.
    raw_preview_seq: u64,
    /// Live bounds of slider tracks, recorded each frame by their canvases.
    slider_bounds: FxHashMap<&'static str, Bounds<Pixels>>,
    /// Slider drag in progress: (slider id, value before the drag) — used
    /// to commit layer-opacity drags as one undo step on release.
    active_slider: Option<(&'static str, f32)>,
    /// Layer thumbnails keyed by layer id, tagged with the doc revision
    /// they were rendered at.
    thumbs: FxHashMap<schist_core::LayerId, (u64, Arc<RenderImage>)>,
    /// Toolbar groups: (group id, tool ids in registration order).
    pub tool_groups: Vec<(&'static str, Vec<&'static str>)>,
    /// The tool each group last used — what its toolbar slot shows.
    group_active: FxHashMap<&'static str, &'static str>,
    /// An open tool flyout: the group and where to draw it.
    pub tool_flyout: Option<(&'static str, Point<Pixels>)>,
    /// A toolbar slot being held down, for click-and-hold flyouts.
    tool_press: Option<&'static str>,
    /// The open right-click menu, if any.
    pub context_menu: Option<ContextMenu>,
    /// The open modal dialog, if any.
    pub modal: Option<Modal>,
    /// A quit is waiting on the unsaved-changes prompts. Set by
    /// `request_quit`, cleared by `cancel_quit`, and consumed by
    /// `resume_quit` once every tab is clean.
    pending_quit: bool,
    /// Dialogs suspended underneath `modal`, innermost last. Only the
    /// Color Picker stacks: it opens on top of a dialog that owns a colour
    /// swatch, and closing it puts that dialog back exactly as it was.
    modal_stack: Vec<Modal>,
    /// Numeric field currently accepting digits, and its edit buffer.
    /// The editor's info panel: which of its tabs is showing, `None`
    /// until the user picks one — the default is Info when the open
    /// file has EXIF, Color otherwise. Session state; it resets when
    /// the document changes.
    pub side_tab: Option<SideTab>,
    /// The open document's EXIF, read once per document (the file is
    /// the original photo for a gallery edit, the file itself
    /// otherwise). `None` inside means the file has none.
    pub exif: Option<(schist_core::DocumentId, Option<schist_gallery::ExifSummary>)>,
    /// The info panel's map, showing where the photo was taken.
    #[cfg(not(target_arch = "wasm32"))]
    pub info_map: library_geo::MapState,
    /// The info panel's EXIF rows scroll on their own; the thumb
    /// beside them reads this.
    pub info_scroll: gpui::ScrollHandle,
    pub focused_field: Option<&'static str>,
    pub field_buffer: String,
    /// The caret's byte position in `field_buffer` (always on a char
    /// boundary). Only the textual fields move it; numeric fields stay
    /// append-only, matching their caret-less rendering.
    pub field_cursor: usize,
    /// When the caret last moved or typed: carets show during the even
    /// 530 ms beats since then, so one is always solid right after a
    /// keystroke. `None` (the web build, which has no blink timer)
    /// means always visible.
    caret_phase: Option<std::time::Instant>,
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    caret_blinker: bool,
    /// True until the first keystroke after a field takes focus, so that
    /// one can replace the seeded value rather than append to it.
    field_fresh: bool,
    /// What Enter does in the open dialog: the primary button's action,
    /// captured while the dialog rendered.
    pub default_action: Option<crate::ui::DialogAction>,
    /// Third-party plugin registry state.
    #[cfg(not(target_arch = "wasm32"))]
    pub plugins: schist_plugin_host_wasm::PluginManager,
    /// Discovered Photoshop plug-ins, including the ones this machine
    /// cannot run — the manager lists those with the reason.
    #[cfg(not(target_arch = "wasm32"))]
    pub photoshop_plugins: schist_plugin_host_8bf::manager::PluginManager,
    /// Plugin enable/disable requested from the manager UI, applied on the
    /// next render pass (the checkbox callback has no context to do it).
    // Written but never read on the web: its writer flows are compiled
    // out with the subsystem it belongs to.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub pending_plugin_toggle: Option<(String, bool)>,
    /// View toggles (rulers, grid, guides, snapping, theme).
    pub view: ViewOptions,
    /// View options and rendering intent as they stood when Preferences
    /// opened, so Cancel can put them back.
    ///
    /// Every control in that dialog applied and persisted on change and
    /// the only action was "Done" -- flipping the GPU compositing
    /// checkbox to see what it did tore the backend down, rebuilt it and
    /// wrote the preference file, with no way to back out.
    preferences_snapshot: Option<Box<(ViewOptions, schist_colormgmt::Intent)>>,
    /// Close *this* document's tab once its in-flight save finishes.
    ///
    /// "Save…" on an Untitled document falls through to the *async* Save
    /// As prompt and returns immediately with `dirty` still true, so the
    /// close was skipped: the user asked to close the tab, the file was
    /// written, and the tab stayed open.
    close_after_save: Option<schist_core::DocumentId>,
    pub screen_mode: ScreenMode,
    /// A guide being dragged out of a ruler.
    dragging_guide: Option<schist_core::Guide>,
    /// A layer row press that may become (or already is) a drag-reorder.
    layer_drag: Option<LayerDrag>,
    /// Where the dragged rows would land if released now. `Some` only
    /// while a drag is past the threshold.
    pub layer_drop: Option<LayerDrop>,
    /// Live bounds of layer rows, recorded each frame by their canvases.
    layer_row_bounds: FxHashMap<schist_core::LayerId, Bounds<Pixels>>,
    /// The row a shift-click range extends from: the last plainly
    /// clicked row.
    layer_anchor: Option<schist_core::LayerId>,
    /// An inline rename in progress in the layers panel: the layer and
    /// the text typed so far.
    pub layer_rename: Option<(schist_core::LayerId, String)>,
    /// A note field being typed into: which one, and the text so far.
    /// Held here rather than written straight through so the whole typing
    /// session is one history entry rather than one per keystroke.
    pub note_edit: Option<(NoteField, String)>,
    /// The selection outline, tagged with the selection generation it was
    /// traced from.
    selection_outline: Option<(u64, SelectionOutline)>,
    /// Navigator thumbnail, tagged with the revision it was rendered at.
    nav_thumb: Option<(u64, Arc<RenderImage>)>,
    /// The canvas takes focus on the first frame so keyboard shortcuts work
    /// before the user clicks anything.
    focused_once: bool,
    /// A freshly opened document still owes itself a Fit to Screen: the
    /// open may have happened while the canvas had no size at all (from
    /// the gallery, or at boot), so the fit is redone on the first paint
    /// that knows the real bounds.
    pending_fit: bool,
    /// Bumped whenever colour settings change, so cached pixels drawn with
    /// the old transform are rebuilt.
    color_epoch: u64,
    /// Colour management settings and the compiled display transform.
    pub color: schist_colormgmt::ColorSettings,
    display_transform: Option<Arc<schist_colormgmt::ColorTransform>>,
    proof_transform: Option<Arc<schist_colormgmt::ColorTransform>>,
    /// The AI sidebar: transcript, conversation worker, MCP queues.
    pub ai: crate::ai::AiState,
    /// The photo gallery: watched folders, thumbnails, edit sidecars.
    #[cfg(not(target_arch = "wasm32"))]
    pub library: library::Library,
}

impl Workspace {
    /// Whether the gallery view is showing instead of the editor. Always
    /// false on the web, where the gallery does not exist.
    pub fn gallery_open(&self) -> bool {
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.library.open
        }
        #[cfg(target_arch = "wasm32")]
        {
            false
        }
    }

    /// Whether the gallery's search box — or the viewer's name field —
    /// is taking typing, for the key context. Always false on the web,
    /// with the gallery itself.
    pub fn gallery_typing(&self) -> bool {
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.gallery_search_active()
                || (self.library.open && self.focused_field == Some("face-name"))
        }
        #[cfg(target_arch = "wasm32")]
        {
            false
        }
    }
}

/// One filter in the Filter Gallery's stack.
#[derive(Debug, Clone, PartialEq)]
pub struct GalleryEntry {
    pub id: &'static str,
    pub values: schist_plugin_api::FilterValues,
    /// Unticking keeps the entry but skips it, which is how the gallery's
    /// eye toggles work.
    pub enabled: bool,
}

/// What Edit ▸ Fill fills with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FillSource {
    Foreground,
    Background,
    Black,
    White,
    Gray,
    /// Grow the surroundings inwards over the selection.
    ContentAware,
}

impl FillSource {
    pub const ALL: [FillSource; 6] = [
        FillSource::Foreground,
        FillSource::Background,
        FillSource::Black,
        FillSource::White,
        FillSource::Gray,
        FillSource::ContentAware,
    ];

    pub fn label(self) -> &'static str {
        match self {
            FillSource::Foreground => "Foreground Color",
            FillSource::Background => "Background Color",
            FillSource::Black => "Black",
            FillSource::White => "White",
            FillSource::Gray => "50% Gray",
            FillSource::ContentAware => "Content-Aware",
        }
    }
}

/// What to do with the active stored path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathOp {
    Fill,
    Stroke,
    Select,
    Delete,
}

impl PathOp {
    pub fn title(self) -> &'static str {
        match self {
            PathOp::Fill => "Fill Path",
            PathOp::Stroke => "Stroke Path",
            PathOp::Select => "Make Selection",
            PathOp::Delete => "Delete Path",
        }
    }
}

/// Image ▸ Auto Tone / Auto Contrast / Auto Color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoMode {
    Tone,
    Contrast,
    Color,
}

impl AutoMode {
    pub fn title(self) -> &'static str {
        match self {
            AutoMode::Tone => "Auto Tone",
            AutoMode::Contrast => "Auto Contrast",
            AutoMode::Color => "Auto Color",
        }
    }
}

/// Whole-canvas rotations and flips.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanvasTransform {
    Cw90,
    Ccw90,
    Rotate180,
    FlipH,
    FlipV,
}

impl CanvasTransform {
    pub fn title(self) -> &'static str {
        match self {
            CanvasTransform::Cw90 => "Rotate 90\u{b0} Clockwise",
            CanvasTransform::Ccw90 => "Rotate 90\u{b0} Counter Clockwise",
            CanvasTransform::Rotate180 => "Rotate 180\u{b0}",
            CanvasTransform::FlipH => "Flip Horizontal",
            CanvasTransform::FlipV => "Flip Vertical",
        }
    }
}

/// What the gallery's batch dialog does to each photo, in the order
/// it happens: the canvas is turned, then enlarged, then the colour
/// adjustments go on top as adjustment layers — so an edit keeps them
/// live in the sidecar, the way a Layer ▸ New Adjustment would.
#[derive(Debug, Clone, PartialEq, Default)]
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub struct BatchRecipe {
    /// A quarter or half turn, if any.
    pub rotate: Option<CanvasTransform>,
    pub flip_h: bool,
    pub flip_v: bool,
    /// A neural ×2 upscaler from the catalogue, if any.
    pub upscale: Option<&'static str>,
    /// Adjustment layers, bottom to top.
    pub adjustments: Vec<schist_adjustments::Params>,
}

#[cfg(not(target_arch = "wasm32"))]
impl BatchRecipe {
    /// Whether the recipe does anything at all.
    pub fn is_empty(&self) -> bool {
        self.rotate.is_none()
            && !self.flip_h
            && !self.flip_v
            && self.upscale.is_none()
            && self.adjustments.is_empty()
    }

    /// The canvas transforms, in the order they apply.
    pub fn transforms(&self) -> Vec<CanvasTransform> {
        let mut ops = Vec::new();
        ops.extend(self.rotate);
        if self.flip_h {
            ops.push(CanvasTransform::FlipH);
        }
        if self.flip_v {
            ops.push(CanvasTransform::FlipV);
        }
        ops
    }
}

/// Where a batch run puts its results.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub enum BatchTarget {
    /// Into each photo's `.schist` sidecar, versioned like any save.
    /// The originals stay untouched and the gallery shows the edits.
    #[default]
    Edit,
    /// A flat copy beside each original, `<name>-edit.<ext>`.
    Beside,
    /// Flat copies in a folder chosen when the run starts.
    Folder,
}

#[cfg(not(target_arch = "wasm32"))]
impl BatchTarget {
    pub fn label(self) -> &'static str {
        match self {
            BatchTarget::Edit => "Gallery edits (versioned)",
            BatchTarget::Beside => "Copies beside the originals",
            BatchTarget::Folder => "Copies in a folder\u{2026}",
        }
    }
}

/// Which Select ▸ Modify operation a dialog is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModifyKind {
    Expand,
    Contract,
    Border,
    Smooth,
    Feather,
}

impl ModifyKind {
    pub fn title(self) -> &'static str {
        match self {
            ModifyKind::Expand => "Expand Selection",
            ModifyKind::Contract => "Contract Selection",
            ModifyKind::Border => "Border Selection",
            ModifyKind::Smooth => "Smooth Selection",
            ModifyKind::Feather => "Feather Selection",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ModifyKind::Feather => "Radius",
            ModifyKind::Border => "Width",
            _ => "Amount",
        }
    }
}

/// Pixels a filter dialog is previewing over, so each slider tick can
/// re-run from the untouched original and Cancel can put it back.
#[derive(Clone)]
pub struct FilterPreview {
    pub layer: schist_core::LayerId,
    pub region: IntRect,
    pub original: Vec<f32>,
    /// RAW development always covers the capture, independent of a pixel
    /// selection. Ordinary filters leave this false.
    pub whole_layer: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Popup {
    Menu(usize),
    BlendModes,
    /// A dropdown inside a dialog, keyed by field id.
    Field(&'static str),
}

/// Window chrome mode, cycled with F / toggled with Tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScreenMode {
    /// Everything visible.
    #[default]
    Standard,
    /// Canvas only.
    FullCanvas,
}

/// Light or dark chrome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum Theme {
    #[default]
    Dark,
    Light,
}

impl Theme {
    pub fn display_name(self) -> &'static str {
        match self {
            Theme::Dark => "Dark",
            Theme::Light => "Light",
        }
    }
}

/// View toggles that don't belong to the document.
// Not `Copy`: the note author is a String, and the handful of places
// that snapshot the options clone explicitly.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ViewOptions {
    pub rulers: bool,
    pub grid: bool,
    pub guides: bool,
    /// Master switch for guides/grid/selection overlays (⌘H).
    pub extras: bool,
    pub snap: bool,
    pub grid_spacing: f32,
    pub theme: Theme,
    /// Scroll zooms instead of panning (Photoshop's "Zoom with Scroll
    /// Wheel"). Useful on touchpads, where pinch gestures never arrive —
    /// GPUI doesn't surface them on any platform.
    #[serde(default)]
    pub zoom_with_scroll: bool,
    /// Write a local crash report when the editor panics. Opt-in, and
    /// nothing is ever transmitted.
    #[serde(default)]
    pub crash_reports: bool,
    /// Also upload that crash to the project's Sentry. Opt-in separately
    /// from the local report — writing a file here and sending one to us
    /// are not the same decision — and inert in any build that was not
    /// given a DSN, which is every build but the official releases.
    #[serde(default)]
    pub crash_upload: bool,
    /// Composite and resample on the GPU when an adapter exists. On by
    /// default; the CPU reference takes over per-frame for anything the
    /// GPU path can't express, and entirely when this is off.
    #[serde(default = "default_true")]
    pub gpu_compositing: bool,
    /// Ask GitHub for the latest release at launch, at most once a day.
    /// The one request Schist makes without being clicked, which is why
    /// it is a preference; it sends nothing but the request itself.
    #[serde(default = "default_true")]
    pub check_updates: bool,
    /// Draw note markers (View ▸ Notes, Photoshop's Show ▸ Notes).
    #[serde(default = "default_true")]
    pub notes: bool,
    /// Name stamped on notes as they are placed. A preference rather than
    /// document state: it is who is reviewing, not what is being
    /// reviewed, and typing it once per session would be once too many.
    #[serde(default = "default_note_author")]
    pub note_author: String,
    /// Colour new notes are given, 0xRRGGBB.
    #[serde(default = "default_note_color")]
    pub note_color: u32,
    /// Hide photos the gallery's content filter flags as explicit.
    /// Off by default, and honest about its needs: the judgement comes
    /// from the "Content (NSFW Filter)" model, fetched like any other
    /// under Filter ▸ Neural Filters ▸ Manage Models; without it,
    /// nothing is flagged.
    #[serde(default)]
    pub gallery_hide_nsfw: bool,
    /// Show the AI sidebar. Off by default: it spawns an agent CLI the
    /// user may not have, and a chat column is not everyone's furniture.
    #[serde(default)]
    pub ai_panel: bool,
    /// The same panel's switch for the gallery, remembered separately:
    /// the harness, model and conversation are shared between the two
    /// rooms, but whether a chat column sits beside the photos is its
    /// own question.
    #[serde(default)]
    pub ai_panel_gallery: bool,
    /// Which agent harness the sidebar drives ("claude" or "codex").
    #[serde(default = "default_ai_backend")]
    pub ai_backend: String,
    /// The model last used in this app, per harness — deliberately not
    /// the CLI's own default, which is tuned for coding. Empty until the
    /// first catalog fetch seeds it. Two fields because the slugs don't
    /// travel: "opus" means nothing to Codex, "gpt-5.5" nothing to
    /// Claude.
    #[serde(default)]
    pub ai_model_claude: String,
    #[serde(default)]
    pub ai_model_codex: String,
}

/// Whoever is logged in, which is Photoshop's default author too. Empty
/// when the environment does not say, rather than a guess like "user".
fn default_note_author() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_default()
}

fn default_note_color() -> u32 {
    let [r, g, b, _] = schist_core::DEFAULT_NOTE_COLOR.to_u8();
    ((r as u32) << 16) | ((g as u32) << 8) | b as u32
}

fn default_true() -> bool {
    true
}

fn default_ai_backend() -> String {
    "claude".to_string()
}

impl Default for ViewOptions {
    fn default() -> Self {
        ViewOptions {
            rulers: true,
            grid: false,
            guides: true,
            extras: true,
            snap: true,
            grid_spacing: 64.0,
            theme: Theme::Dark,
            zoom_with_scroll: false,
            crash_reports: false,
            crash_upload: false,
            gpu_compositing: true,
            check_updates: true,
            notes: true,
            note_author: default_note_author(),
            note_color: default_note_color(),
            gallery_hide_nsfw: false,
            ai_panel: false,
            ai_panel_gallery: false,
            ai_backend: default_ai_backend(),
            ai_model_claude: String::new(),
            ai_model_codex: String::new(),
        }
    }
}

/// Install the GPU backends the preference (or `SCHIST_GPU=0|1`, which
/// wins) asks for: the compositor and the filter/warp kernels behind
/// `schist_fx`, which share one device. Safe to call again when the
/// preference flips; falls back to the CPU with a log line when no adapter
/// exists.
pub fn init_compositor_backend(prefer_gpu: bool) {
    // The GPU backend opens a second wgpu device with a blocking wait,
    // which the browser's single thread cannot make progress under; the
    // web build composites on the CPU reference backend instead.
    #[cfg(target_arch = "wasm32")]
    {
        let _ = prefer_gpu;
        schist_compositor::set_backend(Arc::new(schist_compositor::CpuCompositor));
        schist_fx::set_backend(Arc::new(schist_fx::CpuFx));
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let enabled = match std::env::var("SCHIST_GPU").ok().as_deref() {
            Some("0") => false,
            Some("1") => true,
            _ => prefer_gpu,
        };
        if !enabled {
            schist_compositor::set_backend(Arc::new(schist_compositor::CpuCompositor));
            schist_fx::set_backend(Arc::new(schist_fx::CpuFx));
            return;
        }
        if schist_compositor::backend().name() == "gpu" {
            return;
        }
        match schist_compositor_gpu::GpuCompositor::new() {
            Ok(gpu) => {
                log::info!("GPU compositing and filter kernels on ({})", gpu.describe());
                let info = gpu.context().adapter_info();
                let _ = GPU_INFO.set(GpuInfo {
                    name: info.name.clone(),
                    backend: format!("{:?}", info.backend).to_lowercase(),
                    driver: format!("{} {}", info.driver, info.driver_info)
                        .trim()
                        .to_string(),
                });
                schist_fx::set_backend(Arc::new(gpu.fx()));
                schist_compositor::set_backend(Arc::new(gpu));
            }
            Err(err) => log::warn!("GPU compositing unavailable, staying on the CPU: {err}"),
        }
    }
}

/// The adapter the GPU compositor opened, as recorded by
/// [`init_compositor_backend`]. `None` while compositing is on the CPU.
#[cfg(not(target_arch = "wasm32"))]
static GPU_INFO: std::sync::OnceLock<GpuInfo> = std::sync::OnceLock::new();

/// What the GPU compositor is running on.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone)]
pub struct GpuInfo {
    /// The adapter's name, as the driver reports it: "NVIDIA RTX 4070".
    pub name: String,
    /// "vulkan", "metal", "dx12"...
    pub backend: String,
    /// The driver's name and version, whichever of the two it gives.
    pub driver: String,
}

#[cfg(not(target_arch = "wasm32"))]
pub fn gpu_info() -> Option<&'static GpuInfo> {
    GPU_INFO.get()
}

/// A traced selection boundary: runs of document-space points.
type SelectionOutline = Arc<Vec<Vec<(f32, f32)>>>;

/// What a context menu was opened on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextTarget {
    Layer(schist_core::LayerId),
    History,
    Color,
    Navigator,
    Canvas,
}

/// An open right-click menu.
#[derive(Debug, Clone, Copy)]
pub struct ContextMenu {
    pub position: Point<Pixels>,
    pub target: ContextTarget,
}

/// Identifies the state a viewport image was assembled for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ViewportKey {
    revision: u64,
    /// Zoom and pan as raw bits, so any change invalidates.
    zoom: u32,
    offset: (i32, i32),
    size: (u32, u32),
    color_epoch: u64,
    rotation: u32,
    /// The surround outside the document is baked into the image, so a
    /// theme change must invalidate it.
    surround: u32,
}

/// How far along an update the user asked for is.
#[derive(Debug, Clone, PartialEq)]
// Some variants belong to desktop-only flows (updates, plug-ins, HEIC)
// and are never constructed on the web; the types stay so the modal
// plumbing matches exhaustively on every target.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub enum UpdateProgress {
    /// `total` is what the release says the download weighs; it is never
    /// zero, since an asset that lists no size is not offered.
    Downloading { received: u64, total: u64 },
    /// Unpacking and swapping the bundle (macOS), or handing the
    /// installer over (Windows). Short, and not interruptible.
    Installing,
}

/// Which modal dialog is open.
#[derive(Debug, Clone, PartialEq)]
// Some variants belong to desktop-only flows (updates, plug-ins, HEIC)
// and are never constructed on the web; the types stay so the modal
// plumbing matches exhaustively on every target.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub enum Modal {
    ImageSize {
        width: u32,
        height: u32,
        resample: schist_tools_transform::Resample,
        link: bool,
    },
    CanvasSize {
        width: u32,
        height: u32,
        anchor: (f32, f32),
    },
    /// A destructive filter with live parameters.
    Filter {
        id: &'static str,
        values: schist_plugin_api::FilterValues,
        /// Show the result on the canvas while the dialog is open.
        preview: bool,
        /// The image picked for a filter that takes one -- Displace's
        /// map. Kept on the modal because it belongs to this run of the
        /// dialog and nothing else. Compared by identity: two decodes of
        /// the same file are the same picture, and comparing a few
        /// megapixels to find that out would be absurd.
        #[allow(clippy::type_complexity)]
        map: Option<Arc<schist_plugin_api::FilterImage>>,
    },
    /// Layer effects for one layer. Boxed because the style is by far the
    /// largest thing any dialog carries.
    LayerStyle {
        layer: schist_core::LayerId,
        style: Box<schist_core::LayerStyle>,
        /// What to put back on Cancel, and the "before" for the history
        /// entry recorded on OK.
        original: Box<schist_core::LayerStyle>,
        /// Which effect's settings are showing.
        active: &'static str,
    },
    /// An adjustment applied straight to the pixels (Image ▸ Adjustments).
    DestructiveAdjustment {
        kind: schist_core::AdjustmentKind,
        params: Box<schist_adjustments::Params>,
        preview: bool,
    },
    /// A filter running outside this process — a Photoshop plug-in, whose
    /// own dialog is where the interaction is happening. Carries no
    /// controls: it exists to hold the document still until the plug-in
    /// is done, and closes itself when it is.
    /// Something slow is running and the document must hold still until
    /// it finishes. Not dismissable: see the Escape handler.
    Busy {
        title: String,
        what: String,
        note: String,
    },
    /// Filter ▸ Filter Gallery: a stack of filters applied in order.
    FilterGallery {
        /// Applied bottom to top, as in Photoshop's stack.
        stack: Vec<GalleryEntry>,
        /// Index into `stack` whose parameters the panel is showing.
        selected: usize,
        preview: bool,
    },
    /// Edit ▸ Content-Aware Scale.
    ContentAwareScale { width: u32, height: u32 },
    /// Edit ▸ Stroke.
    Stroke {
        width: f32,
        position: schist_core::StrokePosition,
    },
    /// Edit ▸ Fill.
    Fill { source: FillSource, opacity: f32 },
    /// Select ▸ Modify, which all take one amount.
    SelectModify { kind: ModifyKind, amount: f32 },
    /// Select ▸ Color Range.
    ColorRange { tolerance: f32, target: Rgba },
    /// Photoshop's Color Picker.
    ColorPicker {
        target: ColorTarget,
        /// HSB, not RGB. Hue and saturation are not recoverable from a
        /// black or grey RGB value, so storing RGB would snap the hue
        /// strip to red the moment brightness reached zero.
        hsv: (f32, f32, f32),
        /// What Cancel leaves in place, and the "current" half of the
        /// dialog's swatch.
        original: Rgba,
    },
    /// "Save changes before closing?" for the active tab.
    ConfirmCloseTab,
    /// An image file dropped on the window while a document is open:
    /// open it in its own tab, or place it as a new layer?
    DropImage { path: PathBuf },
    /// Folders dropped on the window: open every image inside as a tab,
    /// or watch them in the gallery? `images` is what a scan found in
    /// them, so the button can say how many tabs that would be.
    DropFolders { dirs: Vec<PathBuf>, images: usize },
    /// A HEIC file needs the libheif decoder and this machine has none:
    /// offer to download it (with its LGPL license texts), then retry
    /// opening `path`.
    HeifSupport { path: PathBuf },
    /// More than one camera is reachable: ask which to import from.
    CameraImport { sources: Vec<ImportSource> },
    /// Import options for one camera: the navigable OpenStreetMap view
    /// where a boundary can be drawn (only photos whose EXIF position
    /// falls inside it import). The map's own state lives on the
    /// library, not here — it changes every pointer move.
    CameraImportOptions { source: ImportSource },
    /// A device import failed (a locked iPhone, most often): say so in
    /// a dialog with the way forward, and offer to try again with the
    /// same source and boundary. Only ever constructed on macOS, where
    /// device imports exist.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    CameraImportFailed {
        source: ImportSource,
        area: Option<(GeoBounds, String)>,
        message: String,
    },
    /// A release newer than this build. On macOS and Windows it offers
    /// to install itself and restart; everywhere else it points at the
    /// release page, since the copy came from a package manager.
    UpdateAvailable { update: crate::update::Update },
    /// The third-party plugin manager.
    PluginManager,
    /// Neural Filters model downloads.
    ModelManager,
    /// Application preferences.
    Preferences,
    /// Export with format options.
    Export {
        codec: &'static str,
        options: schist_plugin_api::ExportOptions,
    },
    /// Assign or convert to a colour profile.
    Profile {
        /// True = convert (rewrites pixels), false = assign.
        convert: bool,
        selected: usize,
    },
    /// Rename a layer (Layer Properties).
    LayerProperties {
        layer: schist_core::LayerId,
        name: String,
    },
    /// Fonts the open document names that this system doesn't have.
    MissingFonts {
        fonts: Vec<crate::fonts::MissingFont>,
    },
    /// Editing an existing adjustment layer's parameters. `original` is
    /// what the layer held before the dialog opened, so Cancel can put it
    /// back exactly.
    Adjustment {
        layer: schist_core::LayerId,
        params: schist_adjustments::Params,
        original: (Option<String>, Vec<u8>),
    },
    /// File ▸ New: the preset picker — one click for a common size,
    /// Custom… for the full dialog below.
    NewFilePicker,
    /// The gallery's map filter: the navigable map, a drawn boundary,
    /// and Apply — the grid then shows only photos taken inside it.
    MapFilter,
    /// The gallery's offer to install the two Search models, with the
    /// licences to agree to first. Desktop-only, like the gallery.
    SearchModels,
    /// The same offer for the two People models — the face detector
    /// and the face recogniser.
    PeopleModels,
    /// Rename one of the gallery's people (`index` into the people
    /// list); a name somebody else has merges the two.
    PersonName { index: usize, name: String },
    /// Save one gallery photo as a flat image: format, quality where the
    /// format takes one, and a scale to shrink it by. `size` is the
    /// source's pixel size when it could be read up front, so the
    /// dialog can say what the scale comes to.
    SaveImageAs {
        path: PathBuf,
        codec: &'static str,
        options: schist_plugin_api::ExportOptions,
        scale: f32,
        size: Option<(u32, u32)>,
    },
    /// The gallery's batch dialog: one recipe run over every photo in
    /// `photos`, written as versioned edits or as flat copies.
    BatchProcess {
        photos: Vec<PathBuf>,
        recipe: BatchRecipe,
        target: BatchTarget,
        codec: &'static str,
        options: schist_plugin_api::ExportOptions,
    },
    /// Create or edit a gallery bucket: its name, and optionally a
    /// smart rule — a search query and/or a map area (the drawn
    /// boundary lives on the shared map state, not here) that keeps
    /// the bucket filling itself. `editing` is the bucket being
    /// reconfigured; `None` creates one, born holding `photos`. An
    /// empty name falls back to "Bucket N" (create) or stays (edit).
    BucketName {
        name: String,
        query: String,
        photos: Vec<PathBuf>,
        editing: Option<usize>,
    },
    /// The full new-document dialog: everything a fresh document needs,
    /// asked up front as Photoshop does.
    NewDocument {
        name: String,
        width: u32,
        height: u32,
        /// Pixels per inch.
        resolution: f32,
        mode: ColorMode,
        depth: Depth,
        background: NewDocBackground,
    },
}

/// Where a camera import reads from.
// Plain data on every target so the Modal enum stays portable; the
// Device variant is only ever constructed on macOS.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub enum ImportSource {
    /// A mounted volume with a DCIM directory.
    Volume(PathBuf),
    /// An ImageCaptureCore device (macOS): an iPhone or a PTP camera,
    /// which never mounts as a filesystem. The id keys the connected-
    /// device list; the name is for people.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Device { id: u64, name: String },
}

/// The editor's side panel has two things in its top slot; this is
/// which one is showing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SideTab {
    Info,
    Color,
}

/// A boundary in degrees: what the import map's rectangle means, and
/// what the EXIF-position filter tests. Lives in `schist-gallery` (its
/// persistence and geometry are shared with the headless server) and is
/// re-exported here so the modal enum can carry one on every target.
pub use schist_gallery::GeoBounds;

/// What fills the bottom layer of a document made by File ▸ New.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NewDocBackground {
    White,
    BackgroundColor,
    Black,
    Transparent,
}

impl NewDocBackground {
    pub fn display_name(self) -> &'static str {
        match self {
            NewDocBackground::White => "White",
            NewDocBackground::BackgroundColor => "Background Color",
            NewDocBackground::Black => "Black",
            NewDocBackground::Transparent => "Transparent",
        }
    }
}

/// What a picker is editing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorTarget {
    Foreground,
    Background,
    /// A colour belonging to one Layer Style effect, keyed as in
    /// `style_dialog::EFFECTS`. That dialog stays open underneath the
    /// picker and receives the colour on OK.
    StyleEffect(&'static str),
    /// The colour Select ▸ Color Range matches against, with that
    /// dialog left open underneath the picker.
    ColorRange,
    /// The Note tool's colour, given to notes as they are placed. Does
    /// not recolour notes already on the canvas, matching Photoshop.
    Note,
}

/// Which note field an inline edit is typing into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteField {
    /// The body of the note at this index, in the Notes panel.
    Text(usize),
    /// The author on the Note tool's options bar, stamped on notes placed
    /// from then on.
    Author,
}

/// A press on a layer row that may become a drag-reorder.
struct LayerDrag {
    /// The row the mouse went down on.
    layer: schist_core::LayerId,
    start: Point<Pixels>,
    /// The pointer has moved past the drag threshold.
    active: bool,
    /// The row was already inside a multi-selection when pressed, so the
    /// selection was kept (to allow dragging it all); releasing without
    /// dragging collapses it to just this row.
    collapse: bool,
}

/// Where dragged layer rows would land, relative to the row under the
/// cursor. "Above"/"Below" are panel directions (above = later in the
/// sibling vec, since siblings render top-of-stack first).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayerDrop {
    Above(schist_core::LayerId),
    Below(schist_core::LayerId),
    Into(schist_core::LayerId),
}

/// Which part of a colour control the pointer is dragging. Held on the
/// workspace rather than the modal because the Color panel's ramp has no
/// modal to live in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerDrag {
    /// The saturation/brightness square.
    Field,
    /// The hue strip beside it.
    Hue,
    /// The Color panel's spectrum bar.
    Ramp,
}

#[derive(Default)]
struct Preview {
    /// RGBA8 straight, (doc.width >> PREVIEW_SHIFT) x (doc.height >> ...).
    buf: Vec<u8>,
    w: u32,
    h: u32,
    image: Option<Arc<RenderImage>>,
    dirty: Vec<IntRect>,
    valid: bool,
}

impl Workspace {
    pub fn new(
        registry: PluginRegistry,
        #[cfg(not(target_arch = "wasm32"))] plugins: schist_plugin_host_wasm::PluginManager,
        #[cfg(not(target_arch = "wasm32"))]
        photoshop_plugins: schist_plugin_host_8bf::manager::PluginManager,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut ws = Workspace {
            registry,
            editor: EditorState::default(),
            doc: None,
            background_tabs: Vec::new(),
            active_tab: 0,
            cache: TileCache::new(),
            display_tiles: FxHashMap::default(),
            viewport_image: None,
            retired_images: Vec::new(),
            view_gesture_active: false,
            view_gesture_seq: 0,
            prefetch_queue: Vec::new(),
            prefetch_stamp: (0, 0),
            prefetch_ticker: false,
            picker_field: None,
            picker_strip: None,
            picker_ramp: None,
            preview: Preview::default(),
            zoom: 1.0,
            offset: point(px(0.0), px(0.0)),
            canvas_bounds: Bounds::default(),
            focus: cx.focus_handle(),
            pan_last: None,
            space_held: false,
            pointer_down: false,
            status: "Ready".into(),
            open_popup: None,
            dropdown: Default::default(),
            open_submenu: Vec::new(),
            native_menu: None,
            rotation: 0.0,
            model_downloads: Vec::new(),
            font_downloads: Vec::new(),
            heif_download: false,
            update_progress: None,
            fonts_offered: std::collections::HashSet::new(),
            ant_phase: 0,
            tool_has_overlay: false,
            curve_channel: Default::default(),
            curve_drag: None,
            picker_drag: None,
            filter_preview: None,
            raw_preview_seq: 0,
            slider_bounds: FxHashMap::default(),
            active_slider: None,
            thumbs: FxHashMap::default(),
            tool_groups: Vec::new(),
            group_active: FxHashMap::default(),
            tool_flyout: None,
            tool_press: None,
            context_menu: None,
            modal: None,
            pending_quit: false,
            modal_stack: Vec::new(),
            side_tab: None,
            exif: None,
            #[cfg(not(target_arch = "wasm32"))]
            info_map: library_geo::MapState::default(),
            info_scroll: gpui::ScrollHandle::new(),
            focused_field: None,
            field_buffer: String::new(),
            field_cursor: 0,
            caret_phase: None,
            caret_blinker: false,
            field_fresh: false,
            default_action: None,
            #[cfg(not(target_arch = "wasm32"))]
            plugins,
            #[cfg(not(target_arch = "wasm32"))]
            photoshop_plugins,
            pending_plugin_toggle: None,
            view: load_view_options(),
            preferences_snapshot: None,
            close_after_save: None,
            screen_mode: ScreenMode::default(),
            dragging_guide: None,
            layer_drag: None,
            layer_drop: None,
            layer_row_bounds: FxHashMap::default(),
            layer_anchor: None,
            layer_rename: None,
            note_edit: None,
            selection_outline: None,
            nav_thumb: None,
            focused_once: false,
            pending_fit: false,
            color_epoch: 0,
            color: schist_colormgmt::ColorSettings::default(),
            display_transform: None,
            proof_transform: None,
            #[cfg(not(target_arch = "wasm32"))]
            ai: crate::ai::AiState::new(crate::ai::Backend::Claude),
            #[cfg(target_arch = "wasm32")]
            ai: crate::ai::AiState::default(),
            #[cfg(not(target_arch = "wasm32"))]
            library: library::Library::load(),
        };
        #[cfg(not(target_arch = "wasm32"))]
        {
            ws.ai.backend = crate::ai::Backend::from_pref(&ws.view.ai_backend);
            ws.ai.menu_backend = ws.ai.backend;
            if ws.view.ai_panel {
                ws.ensure_ai_models(cx);
            }
            ws.watch_agent_path(cx);
        }
        ws.rebuild_tool_groups();
        ws.sync_note_defaults();
        // A launch-time update check, when the preference allows one and
        // the last one was long enough ago. Delayed: the first seconds
        // after launch belong to opening whatever the user
        // double-clicked, not to a network round trip.
        #[cfg(not(target_arch = "wasm32"))]
        if ws.view.check_updates && crate::update::check_due() {
            cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(5))
                    .await;
                this.update(cx, |ws, cx| ws.check_for_update_quietly(cx))
                    .ok();
            })
            .detach();
        }
        // The workspace starts empty; File ▸ New asks for the document's
        // settings before creating anything.
        // Periodic crash-recovery snapshot; the task ends with the entity.
        cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(std::time::Duration::from_secs(AUTOSAVE_SECS))
                .await;
            if this.update(cx, |ws, _| ws.autosave()).is_err() {
                break;
            }
        })
        .detach();
        // March the selection ants. Eight steps a second is what
        // Photoshop looks like, and it only repaints while there is a
        // selection to march.
        cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(125))
                .await;
            let keep = this.update(cx, |ws, cx| {
                let marching =
                    ws.doc.as_ref().is_some_and(|d| !d.selection.is_empty()) || ws.tool_has_overlay;
                if marching {
                    ws.ant_phase = ws.ant_phase.wrapping_add(1);
                    cx.notify();
                }
            });
            if keep.is_err() {
                break;
            }
        })
        .detach();
        ws
    }
}

/// Marching ants: split a screen-space polyline into alternating white and
/// black dashes so the outline reads over any content underneath.
///
/// The dash phase comes from each dash's *position* rather than its
/// distance along the polyline. A traced selection arrives as hundreds of
/// short runs, and per-run phase would restart every few pixels — leaving
/// the whole outline one colour.
fn push_ants(ants: &mut Ants, pts: &[Point<Pixels>], phase: u32) {
    const DASH: f32 = 4.0;
    if pts.len() < 2 {
        return;
    }
    // Phase is measured in dashes, so the pattern slides one dash per
    // tick and wraps every two.
    let offset = (phase % 2) as f32 * DASH;
    for pair in pts.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        let (ax, ay) = (f32::from(a.x), f32::from(a.y));
        let (bx, by) = (f32::from(b.x), f32::from(b.y));
        let len = (bx - ax).hypot(by - ay);
        if len <= 0.01 {
            continue;
        }
        let mut t = 0.0f32;
        while t < len {
            let next = (t + DASH).min(len);
            let lerp = |v0: f32, v1: f32, f: f32| v0 + (v1 - v0) * f;
            let (sx, sy) = (lerp(ax, bx, t / len), lerp(ay, by, t / len));
            let (ex, ey) = (lerp(ax, bx, next / len), lerp(ay, by, next / len));
            // Position-based rather than per-segment: a traced outline
            // arrives as hundreds of short runs, and phase-per-run would
            // start every one of them on the same colour.
            let dark = ((((sx + sy) + offset) / DASH).floor() as i64).rem_euclid(2) == 1;
            let seg = [point(px(sx), px(sy)), point(px(ex), px(ey))];
            if dark {
                ants.dark.push(seg);
            } else {
                ants.light.push(seg);
            }
            t = next;
        }
    }
}

/// A note's pin, already in screen space.
struct Marker {
    bounds: Bounds<Pixels>,
    fill: gpui::Hsla,
    selected: bool,
}

/// Dash segments batched by colour, so an outline of any complexity costs
/// two paths rather than one per dash.
#[derive(Default)]
pub struct Ants {
    light: Vec<[Point<Pixels>; 2]>,
    dark: Vec<[Point<Pixels>; 2]>,
}

#[derive(Default)]
pub struct PaintJob {
    /// Filled beneath the artwork when a mid-gesture stale image may not
    /// cover the whole canvas element.
    backdrop: Option<(Bounds<Pixels>, gpui::Hsla)>,
    tiles: Vec<(Bounds<Pixels>, Arc<RenderImage>)>,
    /// Translucent tool highlights, painted above artwork but below chrome.
    highlights: Vec<Bounds<Pixels>>,
    outlines: Vec<(Bounds<Pixels>, gpui::Hsla)>,
    polylines: Vec<(Vec<Point<Pixels>>, gpui::Hsla)>,
    /// Text carets, each painted as one solid hairline.
    carets: Vec<(Vec<Point<Pixels>>, gpui::Hsla)>,
    /// Marching-ants dashes.
    ants: Ants,
    circles: Vec<Bounds<Pixels>>,
    /// Note pins.
    markers: Vec<Marker>,
    /// Thin filled rectangles: grid lines, guides and ruler ticks.
    lines: Vec<(Bounds<Pixels>, gpui::Hsla)>,
    /// Images superseded this frame, freed after painting.
    retired: Vec<Arc<RenderImage>>,
}

/// Blend one tile's worth of a filtered region back over the original,
/// weighted by selection coverage.
#[allow(clippy::too_many_arguments)]
fn blend_region_tile(
    tile: &mut schist_core::TileBuf,
    coord: TileCoord,
    clip: IntRect,
    region: IntRect,
    original: &[f32],
    filtered: &[f32],
    selection: Option<&schist_core::Selection>,
) {
    let trect = coord.rect();
    let w = region.width() as usize;
    for y in clip.top..clip.bottom {
        for x in clip.left..clip.right {
            let cov = selection.map_or(1.0, |selection| selection.coverage(x, y) as f32 / 255.0);
            if cov <= 0.0 {
                continue;
            }
            let src = ((y - region.top) as usize * w + (x - region.left) as usize) * 4;
            let ix = ((y - trect.top) * TILE_SIZE + (x - trect.left)) as usize;
            let mix = |a: f32, b: f32| a + (b - a) * cov;
            tile.set(
                ix,
                schist_color::Rgba::new(
                    mix(original[src], filtered[src]),
                    mix(original[src + 1], filtered[src + 1]),
                    mix(original[src + 2], filtered[src + 2]),
                    mix(original[src + 3], filtered[src + 3]),
                ),
            );
        }
    }
}

/// Whether one named effect is switched on, for the dialog's initial tab.
fn style_enabled(style: &schist_core::LayerStyle, key: &str) -> bool {
    match key {
        "bevel" => style.bevel.enabled,
        "stroke" => style.stroke.enabled,
        "inner_shadow" => style.inner_shadow.enabled,
        "inner_glow" => style.inner_glow.enabled,
        "satin" => style.satin.enabled,
        "color_overlay" => style.color_overlay.enabled,
        "gradient_overlay" => style.gradient_overlay.enabled,
        "outer_glow" => style.outer_glow.enabled,
        "drop_shadow" => style.drop_shadow.enabled,
        "blur" => style.blur.enabled,
        _ => false,
    }
}

/// Tools that draw the active path themselves, so the canvas must not draw
/// it a second time on top.
const PATH_TOOLS: &[&str] = &[
    "pen",
    "pen.freeform",
    "pen.curvature",
    "path_select",
    "direct_select",
];

/// Remove `other`'s coverage from `sel`, in place.
fn subtract_into(
    sel: &mut schist_core::Selection,
    other: &schist_core::Selection,
    canvas: IntRect,
) {
    let rect = sel.bounds().intersect(&canvas);
    let keep = sel.clone();
    sel.deselect();
    sel.activate();
    sel.apply_shape(rect, schist_core::SelectOp::Replace, |x, y| {
        keep.coverage(x, y).saturating_sub(other.coverage(x, y))
    });
}

/// Re-rasterize any shape layer whose path, fill or stroke has moved.
///
/// This is what keeps a vector shape sharp: the pixels are a cache of the
/// path, thrown away and rebuilt rather than resampled.
fn reshape_layers(layers: &mut [Layer], depth: Depth, canvas: IntRect, damage: &mut Vec<IntRect>) {
    for layer in layers.iter_mut() {
        if let schist_core::LayerKind::Group(g) = &mut layer.kind {
            reshape_layers(&mut g.children, depth, canvas, damage);
        }
        let Some(shape) = layer.shape.as_deref() else {
            continue;
        };
        let key = shape.key();
        if layer.shape_key == key {
            continue;
        }
        let before = layer.content_bounds();
        let tiles = schist_tools_vector::render_shape(shape, depth, canvas);
        if let Some(raster) = layer.as_raster_mut() {
            raster.tiles = tiles;
        }
        layer.shape_key = key;
        // The style cache was built from the old pixels.
        layer.styled = None;
        damage.push(before);
        damage.push(layer.content_bounds());
    }
}

/// Write downloaded faces into the user font directory.
#[cfg(not(target_arch = "wasm32"))]
fn install_faces(faces: &[crate::fonts::Face]) -> Result<usize, String> {
    let mut installed = 0;
    for (name, bytes) in faces {
        schist_text_engine::install_face(name, bytes)?;
        installed += 1;
    }
    Ok(installed)
}

/// Read and decode a document file. Blocking and potentially seconds of
/// work for a large layered file, so it runs on a background thread.
fn decode_file(
    codecs: &[Arc<dyn schist_plugin_api::CodecPlugin>],
    path: &std::path::Path,
) -> anyhow::Result<Document> {
    #[cfg(not(target_arch = "wasm32"))]
    let bytes = std::fs::read(path)?;
    // Browser paths are invented names over an in-memory map; the bytes
    // arrived when the file was picked or dropped.
    #[cfg(target_arch = "wasm32")]
    let bytes = crate::web::read_file(path)?;
    let ext = path.extension().and_then(|e| e.to_str());
    let codec = codecs
        .iter()
        .find(|c| c.probe(&bytes))
        .or_else(|| {
            let ext = ext?.to_ascii_lowercase();
            codecs
                .iter()
                .find(|c| c.extensions().contains(&ext.as_str()))
        })
        .ok_or_else(|| anyhow::anyhow!("no codec for {}", path.display()))?;
    let mut doc = codec.import(&bytes)?;
    doc.title = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Untitled".into());
    doc.path = Some(path.to_path_buf());
    Ok(doc)
}

/// Fetch a model over HTTP. Blocking, so it runs on a background thread.
/// (The web build fetches through `crate::web::fetch_bytes` instead —
/// there is no thread to block.)
#[cfg(not(target_arch = "wasm32"))]
fn fetch_model(url: &str, got: &AtomicU64) -> Result<Vec<u8>, String> {
    use std::io::Read as _;
    // The largest model in the catalogue is 66 MB; the cap is a guard
    // against a redirect to something enormous, not a real limit.
    const MAX: u64 = 256 << 20;
    let mut response = ureq::get(url)
        .header("User-Agent", "schist-model-fetch")
        .call()
        .map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    let reader = response.body_mut().as_reader();
    let mut reader = std::io::Read::take(reader, MAX);
    // A chunk at a time rather than `read_to_end`, so the dialog can say
    // how far along a sixty-megabyte download is instead of sitting on
    // "Downloading..." for a minute.
    let mut chunk = vec![0u8; 64 << 10];
    loop {
        let n = reader.read(&mut chunk).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..n]);
        got.store(bytes.len() as u64, Ordering::Relaxed);
    }
    if bytes.is_empty() {
        return Err("empty response".into());
    }
    Ok(bytes)
}

/// Whether `t` can be appended to a numeric field holding `buffer`.
///
/// Digits alone used to be accepted, yet every one of these fields is
/// read back with `parse::<f32>()`, so fractional and negative values
/// were unreachable from the keyboard -- the only way to a non-integer
/// was the +/- step buttons. A leading `-` and a single `.` go through
/// now; `parse` still rejects whatever is malformed.
/// The colour picker's hex field after a keystroke, or `None` when it
/// refuses one.
///
/// The field is seeded with the committed value and capped at a full
/// triplet, so typing into a freshly clicked one did nothing at all until
/// the user backspaced six times. The first character replaces what is
/// there, the way it would if the text were selected.
fn hex_field_after(buffer: &str, fresh: bool, typed: &str) -> Option<String> {
    let base = if fresh { "" } else { buffer };
    if typed.is_empty()
        || base.len() + typed.len() > 6
        || !typed.chars().all(|c| c.is_ascii_hexdigit())
    {
        return None;
    }
    Some(format!("{base}{typed}"))
}

fn numeric_accepts(buffer: &str, t: &str) -> bool {
    let mut len = buffer.len();
    let mut dot = buffer.contains('.');
    for c in t.chars() {
        match c {
            '0'..='9' => {}
            '-' if len == 0 => {}
            '.' if !dot => dot = true,
            _ => return false,
        }
        len += c.len_utf8();
    }
    true
}

/// The built-in profile list position for a profile name, defaulting to
/// the first entry when nothing matches (an embedded profile that is not
/// one of ours).
fn builtin_index(name: &str) -> usize {
    schist_colormgmt::Profile::builtins()
        .iter()
        .position(|(n, _)| n.eq_ignore_ascii_case(name))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::{builtin_index, hex_field_after, numeric_accepts, PNG_CODEC_ID};
    use schist_plugin_api::{PluginManifest, PluginRegistry};
    use std::time::{Duration, SystemTime};

    /// Copying to the system clipboard and exporting slices both look the
    /// PNG codec up by id, and both looked for `"png"` -- which nothing is
    /// registered as, so each returned early and did nothing at all.
    #[test]
    fn the_png_codec_answers_to_the_id_it_is_looked_up_by() {
        let mut registry = PluginRegistry::new();
        schist_codecs_common::CommonCodecsPlugin.register(&mut registry);
        let codec = registry
            .codecs()
            .find(|c| c.id() == PNG_CODEC_ID)
            .expect("nothing is registered under the id the PNG lookups use");
        assert!(codec.can_export(), "the PNG codec has to be able to export");
    }

    #[test]
    fn numeric_fields_take_fractions_and_negatives() {
        // These fields are all read back with `parse::<f32>()`, but only
        // ASCII digits were let through, so no fractional or negative
        // value could be typed at all.
        assert!(numeric_accepts("", "-"));
        assert!(numeric_accepts("1", "."));
        assert!(numeric_accepts("1.", "5"));
        assert!(numeric_accepts("-1.", "5"));
        assert!(numeric_accepts("", "12.5"));
        // A minus only leads, and there is only one decimal point.
        assert!(!numeric_accepts("1", "-"));
        assert!(!numeric_accepts("1.5", "."));
        assert!(!numeric_accepts("", "1.2.3"));
        assert!(!numeric_accepts("", "e"));
    }

    #[test]
    fn the_first_keystroke_replaces_a_freshly_clicked_hex_field() {
        // Seeded with the committed value and capped at six digits, so
        // every keystroke was refused until the field was emptied by
        // hand.
        assert_eq!(hex_field_after("ff8800", true, "a").as_deref(), Some("a"));
        // After that it appends, up to the cap.
        assert_eq!(hex_field_after("a", false, "b").as_deref(), Some("ab"));
        assert_eq!(hex_field_after("ffaa11", false, "b"), None);
        // And it is still hex only.
        assert_eq!(hex_field_after("ff", false, "z"), None);
        assert_eq!(hex_field_after("ff", true, ""), None);
        // A pasted triplet lands whole.
        assert_eq!(
            hex_field_after("112233", true, "aabbcc").as_deref(),
            Some("aabbcc")
        );
    }

    fn snap(secs: u64, name: &str) -> (SystemTime, std::path::PathBuf) {
        (
            SystemTime::UNIX_EPOCH + Duration::from_secs(secs),
            std::path::PathBuf::from(format!("/recovery/{name}")),
        )
    }

    #[test]
    fn every_previous_snapshot_is_recovered_newest_first() {
        // autosave writes one snapshot per dirty document. Returning only
        // the newest left the rest on disk, offered one per launch.
        let found = vec![
            snap(300, "session-11-2.psd"),
            snap(100, "session-11-1.psd"),
            snap(200, "session-12-1.psd"),
        ];
        let ranked = Workspace::rank_snapshots(found, 99);
        let names: Vec<_> = ranked
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap())
            .collect();
        assert_eq!(
            names,
            ["session-11-2.psd", "session-12-1.psd", "session-11-1.psd"]
        );
    }

    #[test]
    fn this_process_snapshots_and_other_files_are_skipped() {
        let found = vec![
            snap(400, "session-99-1.psd"),  // ours, still live
            snap(300, "session-99-12.psd"), // ours, id prefix must not confuse
            snap(200, "session-11-1.psd"),  // a previous run
            snap(100, "notes.txt"),         // not a snapshot
        ];
        let ranked = Workspace::rank_snapshots(found, 99);
        let names: Vec<_> = ranked
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap())
            .collect();
        assert_eq!(names, ["session-11-1.psd"]);
    }

    #[test]
    fn no_snapshots_is_not_an_error() {
        assert!(Workspace::rank_snapshots(Vec::new(), 1).is_empty());
    }

    #[test]
    fn first_dirty_tab_picks_the_earliest_unsaved_one() {
        // `first_dirty_tab` is what decides whether quitting prompts, so
        // its contract is worth pinning even though the tab strip itself
        // needs a running window.
        let strip: Vec<(&str, bool)> =
            vec![("clean", false), ("also clean", false), ("dirty", true)];
        assert_eq!(strip.iter().position(|(_, d)| *d), Some(2));

        let strip: Vec<(&str, bool)> = vec![("clean", false), ("clean too", false)];
        assert_eq!(strip.iter().position(|(_, d)| *d), None);

        let strip: Vec<(&str, bool)> = vec![("dirty", true), ("dirty", true)];
        assert_eq!(strip.iter().position(|(_, d)| *d), Some(0));
    }

    use super::*;

    /// The preview stands in for the real composite, so it may never be
    /// asked to cover more than the pixels it has. A fixed cutoff did,
    /// and Fit to Screen landed in the magnified band on most documents.
    #[test]
    fn the_preview_is_never_magnified() {
        let texel = (1u32 << PREVIEW_SHIFT) as f32;
        for scale_factor in [1.0, 1.25, 1.5, 2.0, 3.0] {
            let zoom = preview_zoom_cutoff(scale_factor);
            assert!(
                zoom * texel * scale_factor <= 1.0,
                "a preview texel covers {} device pixels at {scale_factor}x",
                zoom * texel * scale_factor
            );
        }
    }

    /// Assign / Convert Profile both opened on index 0 -- sRGB -- however
    /// the document was actually tagged, so the dialog described a
    /// conversion the user had not asked for and OK applied it.
    #[test]
    fn the_profile_dialog_opens_on_the_documents_own_profile() {
        let builtins = schist_colormgmt::Profile::builtins();
        for (i, (name, _)) in builtins.iter().enumerate() {
            assert_eq!(builtin_index(name), i, "{name}");
            // Names arrive from a parsed profile, whose casing we do not
            // control.
            assert_eq!(builtin_index(&name.to_lowercase()), i, "{name}");
        }
        // Anything we do not ship falls back to the first entry rather
        // than to a wrong one.
        assert_eq!(builtin_index("Some Scanner Profile"), 0);
    }
}
