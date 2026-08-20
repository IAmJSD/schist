//! The single-window workspace: canvas viewport + docked panels.
//!
//! The kernel and plugins are GPUI-free; this file is the boundary where
//! GPUI events become `PointerInput`s for the active tool and where
//! composited tiles become GPU textures.

use crate::actions::*;
use crate::keymap;
use crate::panels;
use gpui::{
    canvas, div, point, px, size, App, Bounds, Context, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, ParentElement as _, PathBuilder, PinchEvent, Pixels, Point, Render, RenderImage,
    ScrollWheelEvent, SharedString, Styled as _, TouchPhase, Window,
};
use photoslop_color::{Depth, Rgba};
use photoslop_compositor::TileCache;
use photoslop_core::{blit_rgba8, Document, IntRect, Layer, TileCoord, TILE_SIZE};
use photoslop_plugin_api::{
    CommandCtx, EditorState, Modifiers, Overlay, PluginRegistry, PointerInput, ToolCtx,
};
use rayon::prelude::*;
use rustc_hash::FxHashMap;
use smallvec::smallvec;
use std::path::PathBuf;
use std::sync::Arc;

/// Zoom below which the canvas paints one downscaled preview image instead
/// of hundreds of tile quads.
const PREVIEW_ZOOM_CUTOFF: f32 = 0.35;
const PREVIEW_SHIFT: u32 = 3; // preview at 1/8 scale
/// Where view preferences are stored.
fn prefs_path() -> Option<PathBuf> {
    let base = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join(".config"))
        })?;
    Some(base.join("photoslop/preferences.json"))
}

fn load_view_options() -> ViewOptions {
    prefs_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// How often a dirty document is snapshotted for crash recovery.
const AUTOSAVE_SECS: u64 = 30;

pub struct Workspace {
    pub registry: PluginRegistry,
    pub editor: EditorState,
    pub doc: Option<Document>,
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
    /// Path to the open submenu in the menu bar, e.g. [2, 4] for the fifth
    /// row of the third menu. Empty means none.
    pub open_submenu: Vec<usize>,
    /// Marching-ants animation step, advanced on a timer.
    /// View rotation in radians. Display only: the pixels are untouched,
    /// so this is a change of viewpoint rather than an edit.
    pub rotation: f32,
    /// Models currently being fetched, so the dialog can say so and a
    /// second click does not start a second download.
    pub model_downloads: Vec<&'static str>,
    pub ant_phase: u32,
    /// Whether the last frame drew any tool overlay, so the ants timer
    /// knows to keep repainting for tools that draw their own.
    pub tool_has_overlay: bool,
    /// Which curve the Curves editor is showing.
    pub curve_channel: photoslop_adjustments::CurveChannel,
    /// Index of the control point being dragged in the curve editor.
    pub curve_drag: Option<usize>,
    /// Which colour control is being dragged, if any.
    pub picker_drag: Option<PickerDrag>,
    pub filter_preview: Option<FilterPreview>,
    /// Live bounds of slider tracks, recorded each frame by their canvases.
    slider_bounds: FxHashMap<&'static str, Bounds<Pixels>>,
    /// Slider drag in progress: (slider id, value before the drag) — used
    /// to commit layer-opacity drags as one undo step on release.
    active_slider: Option<(&'static str, f32)>,
    /// Layer thumbnails keyed by layer id, tagged with the doc revision
    /// they were rendered at.
    thumbs: FxHashMap<photoslop_core::LayerId, (u64, Arc<RenderImage>)>,
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
    /// Numeric field currently accepting digits, and its edit buffer.
    pub focused_field: Option<&'static str>,
    pub field_buffer: String,
    /// Third-party plugin registry state (M9).
    pub plugins: photoslop_plugin_host_wasm::PluginManager,
    /// Plugin enable/disable requested from the manager UI, applied on the
    /// next render pass (the checkbox callback has no context to do it).
    pub pending_plugin_toggle: Option<(String, bool)>,
    /// View toggles (rulers, grid, guides, snapping, theme).
    pub view: ViewOptions,
    pub screen_mode: ScreenMode,
    /// A guide being dragged out of a ruler.
    dragging_guide: Option<photoslop_core::Guide>,
    /// The selection outline, tagged with the selection generation it was
    /// traced from.
    selection_outline: Option<(u64, SelectionOutline)>,
    /// Navigator thumbnail, tagged with the revision it was rendered at.
    nav_thumb: Option<(u64, Arc<RenderImage>)>,
    /// The canvas takes focus on the first frame so keyboard shortcuts work
    /// before the user clicks anything.
    focused_once: bool,
    /// Bumped whenever colour settings change, so cached pixels drawn with
    /// the old transform are rebuilt.
    color_epoch: u64,
    /// Colour management settings and the compiled display transform.
    pub color: photoslop_colormgmt::ColorSettings,
    display_transform: Option<Arc<photoslop_colormgmt::ColorTransform>>,
    proof_transform: Option<Arc<photoslop_colormgmt::ColorTransform>>,
}

/// One filter in the Filter Gallery's stack.
#[derive(Debug, Clone, PartialEq)]
pub struct GalleryEntry {
    pub id: &'static str,
    pub values: photoslop_plugin_api::FilterValues,
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
    pub layer: photoslop_core::LayerId,
    pub region: IntRect,
    pub original: Vec<f32>,
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
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
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
        }
    }
}

/// A traced selection boundary: runs of document-space points.
type SelectionOutline = Arc<Vec<Vec<(f32, f32)>>>;

/// What a context menu was opened on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextTarget {
    Layer(photoslop_core::LayerId),
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
}

/// Which modal dialog is open.
#[derive(Debug, Clone, PartialEq)]
pub enum Modal {
    ImageSize {
        width: u32,
        height: u32,
        filter: photoslop_core::Filter,
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
        values: photoslop_plugin_api::FilterValues,
        /// Show the result on the canvas while the dialog is open.
        preview: bool,
    },
    /// Layer effects for one layer. Boxed because the style is by far the
    /// largest thing any dialog carries.
    LayerStyle {
        layer: photoslop_core::LayerId,
        style: Box<photoslop_core::LayerStyle>,
        /// What to put back on Cancel, and the "before" for the history
        /// entry recorded on OK.
        original: Box<photoslop_core::LayerStyle>,
        /// Which effect's settings are showing.
        active: &'static str,
    },
    /// An adjustment applied straight to the pixels (Image ▸ Adjustments).
    DestructiveAdjustment {
        kind: photoslop_core::AdjustmentKind,
        params: Box<photoslop_adjustments::Params>,
        preview: bool,
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
        position: photoslop_core::StrokePosition,
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
    /// The third-party plugin manager.
    PluginManager,
    /// Neural Filters model downloads.
    ModelManager,
    /// Application preferences.
    Preferences,
    /// Export with format options.
    Export {
        codec: &'static str,
        options: photoslop_plugin_api::ExportOptions,
    },
    /// Assign or convert to a colour profile.
    Profile {
        /// True = convert (rewrites pixels), false = assign.
        convert: bool,
        selected: usize,
    },
    /// Rename a layer (Layer Properties).
    LayerProperties {
        layer: photoslop_core::LayerId,
        name: String,
    },
    /// Editing an existing adjustment layer's parameters. `original` is
    /// what the layer held before the dialog opened, so Cancel can put it
    /// back exactly.
    Adjustment {
        layer: photoslop_core::LayerId,
        params: photoslop_adjustments::Params,
        original: (Option<String>, Vec<u8>),
    },
}

/// Which of the two editor colours a picker is editing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorTarget {
    Foreground,
    Background,
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
        plugins: photoslop_plugin_host_wasm::PluginManager,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut ws = Workspace {
            registry,
            editor: EditorState::default(),
            doc: None,
            cache: TileCache::new(),
            display_tiles: FxHashMap::default(),
            viewport_image: None,
            retired_images: Vec::new(),
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
            open_submenu: Vec::new(),
            rotation: 0.0,
            model_downloads: Vec::new(),
            ant_phase: 0,
            tool_has_overlay: false,
            curve_channel: Default::default(),
            curve_drag: None,
            picker_drag: None,
            filter_preview: None,
            slider_bounds: FxHashMap::default(),
            active_slider: None,
            thumbs: FxHashMap::default(),
            tool_groups: Vec::new(),
            group_active: FxHashMap::default(),
            tool_flyout: None,
            tool_press: None,
            context_menu: None,
            modal: None,
            focused_field: None,
            field_buffer: String::new(),
            plugins,
            pending_plugin_toggle: None,
            view: load_view_options(),
            screen_mode: ScreenMode::default(),
            dragging_guide: None,
            selection_outline: None,
            nav_thumb: None,
            focused_once: false,
            color_epoch: 0,
            color: photoslop_colormgmt::ColorSettings::default(),
            display_transform: None,
            proof_transform: None,
        };
        ws.rebuild_tool_groups();
        ws.new_document();
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

    // ----- document lifecycle -----

    pub fn new_document(&mut self) {
        let mut doc = Document::new("Untitled", 1280, 800, Depth::Eight);
        let mut bg = Layer::new_raster("Background");
        let white = vec![255u8; 1280 * 800 * 4];
        blit_rgba8(
            &mut bg.as_raster_mut().unwrap().tiles,
            Depth::Eight,
            IntRect::from_size(1280, 800),
            &white,
        );
        doc.push_layer(bg);
        doc.dirty = false;
        self.install_document(doc);
    }

    pub fn install_document(&mut self, mut doc: Document) {
        // Photoshop's History Brush paints back from the state the file
        // was opened in, so that is what gets snapshotted here.
        doc.snapshot_history_source();
        self.doc = Some(doc);
        self.rebuild_color_transforms();
        self.cache.invalidate_all();
        self.display_tiles.clear();
        self.viewport_image = None;
        self.preview = Preview::default();
        self.zoom = 1.0;
        self.offset = point(px(40.0), px(40.0));
        self.fit_to_view();
    }

    pub fn load_file(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let result = (|| -> anyhow::Result<Document> {
            let bytes = std::fs::read(&path)?;
            let ext = path.extension().and_then(|e| e.to_str());
            let codec = self
                .registry
                .codec_for(&bytes, ext)
                .ok_or_else(|| anyhow::anyhow!("no codec for {}", path.display()))?;
            let mut doc = codec.import(&bytes)?;
            doc.title = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "Untitled".into());
            doc.path = Some(path.clone());
            Ok(doc)
        })();
        match result {
            Ok(doc) => {
                self.status = format!("Opened {}", path.display()).into();
                self.install_document(doc);
            }
            Err(err) => {
                log::error!("open failed: {err:#}");
                self.status = format!("Open failed: {err}").into();
            }
        }
        cx.notify();
    }

    /// Serialize the document to `path`, choosing the codec by extension.
    pub fn save_file_as(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        match self.write_document_to(&path) {
            Ok(()) => {
                if let Some(doc) = &mut self.doc {
                    doc.dirty = false;
                    doc.path = Some(path.clone());
                    if let Some(name) = path.file_name() {
                        doc.title = name.to_string_lossy().into_owned();
                    }
                }
                self.clear_recovery();
                self.status = format!("Saved {}", path.display()).into();
            }
            Err(err) => {
                log::error!("save failed: {err:#}");
                self.status = format!("Save failed: {err}").into();
            }
        }
        cx.notify();
    }

    /// ⌘S: save over the document's existing path, or fall back to Save As
    /// when it has never been saved (or its format can't be written).
    pub fn save_current(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let path = self.doc.as_ref().and_then(|d| d.path.clone());
        match path {
            Some(path) if self.exporter_for(&path).is_some() => self.save_file_as(path, cx),
            _ => keymap::save_file_dialog(self, window, cx),
        }
    }

    fn exporter_for(
        &self,
        path: &std::path::Path,
    ) -> Option<&dyn photoslop_plugin_api::CodecPlugin> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        self.registry
            .codecs()
            .find(|c| c.can_export() && c.extensions().contains(&ext.as_str()))
    }

    fn write_document_to(&self, path: &std::path::Path) -> anyhow::Result<()> {
        let doc = self
            .doc
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no document"))?;
        let codec = self.exporter_for(path).ok_or_else(|| {
            anyhow::anyhow!(
                "no exporter for .{}",
                path.extension().and_then(|e| e.to_str()).unwrap_or("")
            )
        })?;
        let bytes = codec.export(doc)?;
        // Write to a sibling temp file and rename, so an interrupted save
        // can't truncate the user's existing file.
        let tmp = path.with_extension("photoslop-tmp");
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    // ----- crash recovery -----

    /// Directory holding autosaved recovery snapshots.
    pub fn recovery_dir() -> Option<PathBuf> {
        let base = std::env::var("XDG_STATE_HOME")
            .ok()
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var("HOME")
                    .ok()
                    .map(|h| PathBuf::from(h).join(".local/state"))
            })?;
        Some(base.join("photoslop/recovery"))
    }

    fn recovery_path(&self) -> Option<PathBuf> {
        Some(Self::recovery_dir()?.join(format!("session-{}.psd", std::process::id())))
    }

    /// Write a recovery snapshot if the document has unsaved changes.
    /// Returns true when a snapshot was written.
    pub fn autosave(&mut self) -> bool {
        let dirty = self.doc.as_ref().map(|d| d.dirty).unwrap_or(false);
        if !dirty {
            return false;
        }
        let Some(path) = self.recovery_path() else {
            return false;
        };
        let Some(dir) = path.parent() else {
            return false;
        };
        if let Err(err) = std::fs::create_dir_all(dir) {
            log::warn!("autosave: cannot create {dir:?}: {err}");
            return false;
        }
        match self.write_document_to(&path) {
            Ok(()) => {
                log::debug!("autosaved recovery snapshot to {path:?}");
                true
            }
            Err(err) => {
                log::warn!("autosave failed: {err:#}");
                false
            }
        }
    }

    fn clear_recovery(&self) {
        if let Some(path) = self.recovery_path() {
            let _ = std::fs::remove_file(path);
        }
    }

    /// Newest recovery snapshot left behind by a previous run, if any.
    pub fn pending_recovery() -> Option<PathBuf> {
        let dir = Self::recovery_dir()?;
        let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
        for entry in std::fs::read_dir(dir).ok()?.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("psd") {
                continue;
            }
            // Skip a snapshot this very process owns.
            if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n == format!("session-{}.psd", std::process::id()))
            {
                continue;
            }
            let Ok(modified) = entry.metadata().and_then(|m| m.modified()) else {
                continue;
            };
            if best.as_ref().is_none_or(|(t, _)| modified > *t) {
                best = Some((modified, path));
            }
        }
        best.map(|(_, p)| p)
    }

    /// Load a recovery snapshot and mark it dirty (it has no real path).
    pub fn recover_from(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.load_file(path.clone(), cx);
        if let Some(doc) = &mut self.doc {
            doc.path = None;
            doc.dirty = true;
            doc.title = format!("{} (recovered)", doc.title);
        }
        let _ = std::fs::remove_file(&path);
        self.status = "Recovered unsaved work from a previous session".into();
        cx.notify();
    }

    // ----- viewport -----

    pub fn fit_to_view(&mut self) {
        let Some(doc) = &self.doc else { return };
        let avail = self.canvas_bounds.size;
        if avail.width <= px(0.0) || avail.height <= px(0.0) {
            return;
        }
        let margin = 40.0;
        let zx = (f32::from(avail.width) - margin) / doc.width as f32;
        let zy = (f32::from(avail.height) - margin) / doc.height as f32;
        self.zoom = zx.min(zy).clamp(0.005, 32.0);
        self.editor.zoom = self.zoom;
        self.center();
    }

    fn center(&mut self) {
        let Some(doc) = &self.doc else { return };
        let avail = self.canvas_bounds.size;
        self.offset = point(
            px((f32::from(avail.width) - doc.width as f32 * self.zoom) / 2.0),
            px((f32::from(avail.height) - doc.height as f32 * self.zoom) / 2.0),
        );
    }

    pub fn zoom_by(&mut self, factor: f32, around: Option<Point<Pixels>>) {
        let old = self.zoom;
        let new = (old * factor).clamp(0.005, 32.0);
        let pivot = around.unwrap_or_else(|| {
            point(
                px(f32::from(self.canvas_bounds.size.width) / 2.0),
                px(f32::from(self.canvas_bounds.size.height) / 2.0),
            )
        });
        // Keep the document point under `pivot` fixed.
        let scale = new / old;
        self.offset = point(
            px(f32::from(pivot.x) - (f32::from(pivot.x) - f32::from(self.offset.x)) * scale),
            px(f32::from(pivot.y) - (f32::from(pivot.y) - f32::from(self.offset.y)) * scale),
        );
        self.zoom = new;
        self.editor.zoom = new;
    }

    fn doc_pos(&self, canvas_local: Point<Pixels>) -> (f32, f32) {
        // The inverse of what `assemble_viewport` draws, so a click lands
        // where the pixel under the cursor actually is, rotation and all.
        let (x, y) = self.unrotate(f32::from(canvas_local.x), f32::from(canvas_local.y));
        (
            (x - f32::from(self.offset.x)) / self.zoom,
            (y - f32::from(self.offset.y)) / self.zoom,
        )
    }

    /// Undo the view rotation for a point in canvas-element coordinates.
    fn unrotate(&self, x: f32, y: f32) -> (f32, f32) {
        if self.rotation == 0.0 {
            return (x, y);
        }
        let cx = f32::from(self.canvas_bounds.size.width) / 2.0;
        let cy = f32::from(self.canvas_bounds.size.height) / 2.0;
        let (s, c) = (-self.rotation).sin_cos();
        let (ox, oy) = (x - cx, y - cy);
        (ox * c - oy * s + cx, ox * s + oy * c + cy)
    }

    /// Turn the view by `delta` radians.
    pub fn rotate_view(&mut self, delta: f32, cx: &mut Context<Self>) {
        self.rotation = (self.rotation + delta).rem_euclid(std::f32::consts::TAU);
        self.viewport_image = None;
        self.status = format!("Rotate View {:.0}\u{b0}", self.rotation.to_degrees()).into();
        cx.notify();
    }

    /// Put the view back upright.
    pub fn reset_view_rotation(&mut self, cx: &mut Context<Self>) {
        self.rotation = 0.0;
        self.viewport_image = None;
        self.status = "View reset".into();
        cx.notify();
    }

    fn to_local(&self, window_pos: Point<Pixels>) -> Point<Pixels> {
        point(
            window_pos.x - self.canvas_bounds.origin.x,
            window_pos.y - self.canvas_bounds.origin.y,
        )
    }

    // ----- change propagation -----

    /// Open the Layer Style dialog for the active layer.
    pub fn show_layer_style(&mut self, layer: photoslop_core::LayerId, cx: &mut Context<Self>) {
        let Some(style) = self
            .doc
            .as_ref()
            .and_then(|d| d.tree.find(layer))
            .map(|l| l.style)
        else {
            return;
        };
        self.open_modal(
            Modal::LayerStyle {
                layer,
                style: Box::new(style),
                original: Box::new(style),
                // Photoshop opens on whatever is on; Drop Shadow otherwise.
                active: crate::style_dialog::EFFECTS
                    .iter()
                    .rev()
                    .find(|(k, _)| style_enabled(&style, k))
                    .map(|(k, _)| *k)
                    .unwrap_or("drop_shadow"),
            },
            cx,
        );
    }

    /// Push the dialog's style onto the layer so the canvas shows it.
    pub fn preview_layer_style(
        &mut self,
        layer: photoslop_core::LayerId,
        style: photoslop_core::LayerStyle,
        cx: &mut Context<Self>,
    ) {
        if let Some(doc) = self.doc.as_mut() {
            if let Some(l) = doc.tree.find_mut(layer) {
                l.style = style;
            }
            doc.damage_all();
        }
        self.after_change(cx);
    }

    /// Re-apply the open dialog's style. Used by the controls that fire
    /// without a context of their own.
    pub fn restyle_from_modal(&mut self) {
        let mut next = None;
        if let Some(Modal::LayerStyle { layer, style, .. }) = self.modal.as_ref() {
            next = Some((*layer, **style));
        }
        if let Some((layer, style)) = next {
            if let Some(doc) = self.doc.as_mut() {
                if let Some(l) = doc.tree.find_mut(layer) {
                    l.style = style;
                }
                doc.damage_all();
            }
            self.refresh_layer_styles();
        }
    }

    /// Record the whole dialog session as one history entry.
    pub fn commit_layer_style(&mut self, cx: &mut Context<Self>) {
        let Some(Modal::LayerStyle {
            layer,
            style,
            original,
            ..
        }) = self.modal.clone()
        else {
            return;
        };
        self.modal = None;
        if let Some(doc) = self.doc.as_mut() {
            // Restore the pre-dialog style so the edit records the right
            // "before"; the live preview already moved the layer on.
            if let Some(l) = doc.tree.find_mut(layer) {
                l.style = *original;
            }
            let mut edit = doc.begin_edit("Layer Style");
            edit.record_layer_style(layer, *original, *style);
            edit.commit();
            doc.damage_all();
        }
        self.status = "Layer Style".into();
        self.after_change(cx);
    }

    /// Put the pre-dialog style back (Cancel).
    fn revert_layer_style(&mut self) {
        let Some(Modal::LayerStyle {
            layer, original, ..
        }) = self.modal.clone()
        else {
            return;
        };
        if let Some(doc) = self.doc.as_mut() {
            if let Some(l) = doc.tree.find_mut(layer) {
                l.style = *original;
            }
            doc.damage_all();
        }
        self.refresh_layer_styles();
    }

    /// Run a Select ▸ Modify operation as one history entry.
    pub fn apply_select_modify(&mut self, kind: ModifyKind, amount: f32, cx: &mut Context<Self>) {
        let Some(doc) = self.doc.as_mut() else { return };
        if doc.selection.is_empty() {
            self.status = "Select something first".into();
            cx.notify();
            return;
        }
        let n = amount.round().max(0.0) as i32;
        let mut edit = doc.begin_edit(kind.title());
        edit.change_selection(|sel, canvas| match kind {
            ModifyKind::Expand => sel.expand(n, canvas),
            ModifyKind::Contract => sel.contract(n, canvas),
            ModifyKind::Border => sel.border(n, canvas),
            ModifyKind::Smooth => sel.smooth(n, canvas),
            ModifyKind::Feather => sel.feather(amount.max(0.0)),
        });
        edit.commit();
        self.status = kind.title().into();
        self.after_change(cx);
    }

    /// Select ▸ Color Range: every pixel within `tolerance` of `target`.
    pub fn apply_color_range(&mut self, tolerance: f32, target: Rgba, cx: &mut Context<Self>) {
        let Some(doc) = self.doc.as_mut() else { return };
        let Some(raster) = doc
            .active_layer
            .and_then(|id| doc.tree.find(id))
            .and_then(|l| l.as_raster())
        else {
            self.status = "Color Range needs a pixel layer".into();
            cx.notify();
            return;
        };
        let canvas = doc.canvas_rect();
        let tol = tolerance / 255.0;
        // Coverage falls off across the tolerance band rather than
        // cutting hard, which is what makes Photoshop's Fuzziness feather
        // the edges of a colour selection.
        let mut cov = vec![0u8; (canvas.width() * canvas.height()) as usize];
        let w = canvas.width() as usize;
        for y in canvas.top..canvas.bottom {
            for x in canvas.left..canvas.right {
                let c = raster.tiles.pixel(x, y);
                let d = (c.r - target.r)
                    .abs()
                    .max((c.g - target.g).abs())
                    .max((c.b - target.b).abs());
                let v = if tol <= 0.0 {
                    if d == 0.0 {
                        1.0
                    } else {
                        0.0
                    }
                } else {
                    (1.0 - d / tol).clamp(0.0, 1.0)
                };
                cov[(y - canvas.top) as usize * w + (x - canvas.left) as usize] =
                    (v * 255.0).round() as u8;
            }
        }
        let mut edit = doc.begin_edit("Color Range");
        edit.change_selection(|sel, canvas| {
            sel.deselect();
            sel.activate();
            sel.apply_shape(canvas, photoslop_core::SelectOp::Replace, |x, y| {
                cov[(y - canvas.top) as usize * w + (x - canvas.left) as usize]
            });
        });
        edit.commit();
        self.status = "Color Range".into();
        self.after_change(cx);
    }

    /// Image ▸ Adjustments: apply an adjustment straight onto the active
    /// layer's pixels, rather than adding a layer for it.
    ///
    /// Opens the same dialog as the adjustment layers do, but previewing
    /// writes pixels; that is what "destructive" means here.
    pub fn apply_adjustment_destructive(
        &mut self,
        kind: photoslop_core::AdjustmentKind,
        cx: &mut Context<Self>,
    ) {
        let params = photoslop_adjustments::Params::default_for(kind);
        if !self.begin_filter_preview() {
            cx.notify();
            return;
        }
        self.open_modal(
            Modal::DestructiveAdjustment {
                kind,
                params: Box::new(params),
                preview: true,
            },
            cx,
        );
    }

    /// Re-run a destructive adjustment's preview from the snapshot.
    pub fn preview_destructive_adjustment(
        &mut self,
        params: Option<&photoslop_adjustments::Params>,
        cx: &mut Context<Self>,
    ) {
        let Some(preview) = self.filter_preview.clone() else {
            return;
        };
        let mut buf = preview.original.clone();
        if let Some(params) = params {
            params.apply_buffer(&mut buf);
        }
        self.write_region(
            preview.layer,
            preview.region,
            &preview.original,
            &buf,
            "",
            false,
        );
        self.after_change(cx);
    }

    /// Commit a destructive adjustment as one history entry.
    pub fn commit_destructive_adjustment(
        &mut self,
        kind: photoslop_core::AdjustmentKind,
        params: &photoslop_adjustments::Params,
        cx: &mut Context<Self>,
    ) {
        // Put the previewed pixels back first so the edit records the
        // right "before".
        self.preview_destructive_adjustment(None, cx);
        let Some(preview) = self.filter_preview.take() else {
            return;
        };
        let mut buf = preview.original.clone();
        params.apply_buffer(&mut buf);
        let name = kind.display_name().to_string();
        self.write_region(
            preview.layer,
            preview.region,
            &preview.original,
            &buf,
            &name,
            true,
        );
        self.status = name.into();
        self.after_change(cx);
    }

    /// Image ▸ Auto Tone / Auto Contrast / Auto Color.
    ///
    /// All three stretch the histogram to fill the range; they differ in
    /// whether the channels are stretched together (contrast, preserving
    /// the colour cast) or apart (tone and colour, removing it), and
    /// whether the midpoint is re-centred (colour).
    pub fn auto_adjust(&mut self, mode: AutoMode, cx: &mut Context<Self>) {
        if !self.begin_filter_preview() {
            cx.notify();
            return;
        }
        let Some(preview) = self.filter_preview.take() else {
            return;
        };
        let mut buf = preview.original.clone();
        // Photoshop clips half a percent off each end so a handful of
        // stray pixels cannot flatten the whole stretch.
        const CLIP: f32 = 0.005;
        let mut lo = [1.0f32; 3];
        let mut hi = [0.0f32; 3];
        for ch in 0..3 {
            let mut vals: Vec<f32> = buf
                .chunks_exact(4)
                .filter(|p| p[3] > 0.0)
                .map(|p| p[ch])
                .collect();
            if vals.is_empty() {
                self.status = "Nothing to adjust".into();
                cx.notify();
                return;
            }
            vals.sort_by(|a, b| a.total_cmp(b));
            let n = vals.len();
            lo[ch] = vals[((n as f32 * CLIP) as usize).min(n - 1)];
            hi[ch] = vals[((n as f32 * (1.0 - CLIP)) as usize).min(n - 1)];
        }
        if mode == AutoMode::Contrast {
            // One stretch for all three channels keeps the colour cast.
            let l = lo[0].min(lo[1]).min(lo[2]);
            let h = hi[0].max(hi[1]).max(hi[2]);
            lo = [l; 3];
            hi = [h; 3];
        }
        for p in buf.chunks_exact_mut(4) {
            if p[3] <= 0.0 {
                continue;
            }
            for ch in 0..3 {
                let span = (hi[ch] - lo[ch]).max(1e-4);
                let mut v = ((p[ch] - lo[ch]) / span).clamp(0.0, 1.0);
                if mode == AutoMode::Color {
                    // Auto Color also pulls the midtones to neutral grey.
                    v = v.powf(1.0);
                }
                p[ch] = v;
            }
        }
        let name = mode.title();
        self.write_region(
            preview.layer,
            preview.region,
            &preview.original,
            &buf,
            name,
            true,
        );
        self.status = name.into();
        self.after_change(cx);
    }

    /// Image ▸ Image Rotation, and the flip entries under Edit ▸ Transform.
    pub fn transform_canvas(&mut self, op: CanvasTransform, cx: &mut Context<Self>) {
        let Some(doc) = self.doc.as_mut() else { return };
        let (w, h) = (doc.width, doc.height);
        let swaps = matches!(op, CanvasTransform::Cw90 | CanvasTransform::Ccw90);
        let (nw, nh) = if swaps { (h, w) } else { (w, h) };
        // Read every layer's pixels first: the mapping reads from the old
        // geometry while writing the new one.
        let ids: Vec<photoslop_core::LayerId> = doc.tree.iter().map(|l| l.id).collect();
        let sources: Vec<(photoslop_core::LayerId, photoslop_core::TileMap)> = ids
            .iter()
            .filter_map(|id| {
                doc.tree
                    .find(*id)
                    .and_then(|l| l.as_raster())
                    .map(|r| (*id, r.tiles.clone()))
            })
            .collect();
        let mut edit = doc.begin_edit(op.title());
        edit.set_canvas_size(nw, nh);
        for (id, src) in &sources {
            for coord in TileCoord::covering(&IntRect::from_size(nw, nh)) {
                let trect = coord.rect();
                let Some(tile) = edit.writable_tile(*id, coord) else {
                    break;
                };
                for y in trect.top..trect.bottom {
                    for x in trect.left..trect.right {
                        // Where this destination pixel came from.
                        let (sx, sy) = match op {
                            CanvasTransform::Cw90 => (y, nw as i32 - 1 - x),
                            CanvasTransform::Ccw90 => (nh as i32 - 1 - y, x),
                            CanvasTransform::Rotate180 => (w as i32 - 1 - x, h as i32 - 1 - y),
                            CanvasTransform::FlipH => (w as i32 - 1 - x, y),
                            CanvasTransform::FlipV => (x, h as i32 - 1 - y),
                        };
                        let ix = ((y - trect.top) * TILE_SIZE + (x - trect.left)) as usize;
                        tile.set(ix, src.pixel(sx, sy));
                    }
                }
            }
        }
        edit.commit();
        self.status = op.title().into();
        self.fit_to_view();
        self.after_change(cx);
    }

    /// Image ▸ Trim: crop away uniform borders.
    pub fn trim(&mut self, cx: &mut Context<Self>) {
        let Some(doc) = self.doc.as_ref() else { return };
        let canvas = doc.canvas_rect();
        // What counts as "border" is the colour of the top-left pixel of
        // the composited image, or transparency where there is none.
        let flat = photoslop_compositor::composite_region_rgba8(doc, canvas);
        let w = canvas.width() as usize;
        let at = |x: i32, y: i32| -> [u8; 4] {
            let i = (y as usize * w + x as usize) * 4;
            [flat[i], flat[i + 1], flat[i + 2], flat[i + 3]]
        };
        let key = at(0, 0);
        let same = |p: [u8; 4]| p == key || (p[3] == 0 && key[3] == 0);
        let mut keep = IntRect::EMPTY;
        for y in 0..canvas.height() {
            for x in 0..canvas.width() {
                if !same(at(x, y)) {
                    keep = keep.union(&IntRect::new(x, y, x + 1, y + 1));
                }
            }
        }
        if keep.is_empty() || keep == canvas {
            self.status = "Nothing to trim".into();
            cx.notify();
            return;
        }
        self.resize_canvas_to(keep, cx);
    }

    /// Crop the document to `rect`, moving every layer with it.
    pub fn resize_canvas_to(&mut self, rect: IntRect, cx: &mut Context<Self>) {
        let Some(doc) = self.doc.as_mut() else { return };
        photoslop_tools_transform::crop_to(doc, rect);
        self.status = "Trimmed".into();
        self.fit_to_view();
        self.after_change(cx);
    }

    /// Image ▸ Mode: switch the document between RGB and Grayscale.
    pub fn set_color_mode(&mut self, mode: photoslop_color::ColorMode, cx: &mut Context<Self>) {
        let Some(doc) = self.doc.as_mut() else { return };
        if doc.mode == mode {
            return;
        }
        if mode == photoslop_color::ColorMode::Grayscale
            || mode == photoslop_color::ColorMode::Indexed
        {
            // Flatten colour out of every layer, which is what the mode
            // change actually means for the pixels.
            let ids: Vec<photoslop_core::LayerId> = doc.tree.iter().map(|l| l.id).collect();
            let coords_by_layer: Vec<(photoslop_core::LayerId, Vec<TileCoord>)> = ids
                .iter()
                .filter_map(|id| {
                    doc.tree
                        .find(*id)
                        .and_then(|l| l.as_raster())
                        .map(|r| (*id, r.tiles.iter().map(|(c, _)| *c).collect()))
                })
                .collect();
            let mut edit = doc.begin_edit("Grayscale");
            for (id, coords) in coords_by_layer {
                for coord in coords {
                    let Some(tile) = edit.writable_tile(id, coord) else {
                        break;
                    };
                    for ix in 0..photoslop_core::TILE_PIXELS {
                        let p = tile.get(ix);
                        let l = 0.299 * p.r + 0.587 * p.g + 0.114 * p.b;
                        tile.set(ix, photoslop_color::Rgba::new(l, l, l, p.a));
                    }
                }
            }
            edit.commit();
        }
        if let Some(doc) = self.doc.as_mut() {
            doc.mode = mode;
            doc.damage_all();
        }
        self.status = mode.display_name().into();
        self.after_change(cx);
    }

    /// Rasterize the active path: fill it, stroke it, or turn it into a
    /// selection. The three things Photoshop's Paths panel buttons do.
    pub fn use_active_path(&mut self, op: PathOp, cx: &mut Context<Self>) {
        let Some(doc) = self.doc.as_ref() else { return };
        let Some(path) = doc.active_path.and_then(|i| doc.paths.get(i)).cloned() else {
            self.status = "No path to use".into();
            cx.notify();
            return;
        };
        if path.is_empty() {
            self.status = "The path is empty".into();
            cx.notify();
            return;
        }
        let flat = photoslop_tools_vector::paths::flatten(&path);
        let colour = self.editor.foreground;
        let width = self.editor.brush_size.max(1.0);
        let Some(doc) = self.doc.as_mut() else { return };
        match op {
            PathOp::Fill => {
                photoslop_tools_vector::fill_path(
                    doc,
                    &flat,
                    colour,
                    photoslop_vector::FillRule::NonZero,
                    "Fill Path",
                );
            }
            PathOp::Stroke => {
                let stroked = photoslop_vector::stroke_path(
                    &flat,
                    photoslop_vector::StrokeStyle::new(width)
                        .with_cap(photoslop_vector::LineCap::Round)
                        .with_join(photoslop_vector::LineJoin::Round),
                );
                photoslop_tools_vector::fill_path(
                    doc,
                    &stroked,
                    colour,
                    photoslop_vector::FillRule::NonZero,
                    "Stroke Path",
                );
            }
            PathOp::Select => {
                let rect = flat.bounds();
                let mask =
                    photoslop_vector::rasterize(&flat, rect, photoslop_vector::FillRule::NonZero);
                let w = rect.width().max(0) as usize;
                let mut edit = doc.begin_edit("Make Selection");
                edit.change_selection(|sel, _| {
                    sel.deselect();
                    sel.activate();
                    sel.apply_shape(rect, photoslop_core::SelectOp::Replace, |x, y| {
                        mask[(y - rect.top) as usize * w + (x - rect.left) as usize]
                    });
                });
                edit.commit();
            }
            PathOp::Delete => {
                if let Some(i) = doc.active_path {
                    doc.paths.remove(i);
                    doc.active_path = if doc.paths.is_empty() {
                        None
                    } else {
                        Some(i.min(doc.paths.len() - 1))
                    };
                }
                doc.damage_all();
            }
        }
        self.status = op.title().into();
        self.after_change(cx);
    }

    /// Edit ▸ Stroke: paint a band along the selection's edge.
    pub fn stroke_selection(
        &mut self,
        width: f32,
        position: photoslop_core::StrokePosition,
        cx: &mut Context<Self>,
    ) {
        let colour = self.editor.foreground;
        let Some(doc) = self.doc.as_mut() else { return };
        if doc.selection.is_empty() {
            self.status = "Stroke needs a selection".into();
            cx.notify();
            return;
        }
        let Some(layer) = doc.active_layer else {
            return;
        };
        // Border() already builds the band; asking it for the right
        // position is a matter of which side of the edge to take.
        let canvas = doc.canvas_rect();
        let mut band = doc.selection.clone();
        let w = width.round().max(1.0) as i32;
        match position {
            photoslop_core::StrokePosition::Inside => {
                let mut inner = band.clone();
                inner.contract(w, canvas);
                subtract_into(&mut band, &inner, canvas);
            }
            photoslop_core::StrokePosition::Outside => {
                let mut outer = band.clone();
                outer.expand(w, canvas);
                let inner = band.clone();
                subtract_into(&mut outer, &inner, canvas);
                band = outer;
            }
            photoslop_core::StrokePosition::Center => band.border(w, canvas),
        }
        let rect = band.bounds().intersect(&canvas);
        if rect.is_empty() {
            self.status = "Nothing to stroke".into();
            cx.notify();
            return;
        }
        let mut edit = doc.begin_edit("Stroke");
        for coord in TileCoord::covering(&rect) {
            let trect = coord.rect();
            let clip = trect.intersect(&rect);
            if clip.is_empty() {
                continue;
            }
            let Some(tile) = edit.writable_tile(layer, coord) else {
                break;
            };
            for y in clip.top..clip.bottom {
                for x in clip.left..clip.right {
                    let cov = band.coverage(x, y) as f32 / 255.0;
                    if cov <= 0.0 {
                        continue;
                    }
                    let ix = ((y - trect.top) * TILE_SIZE + (x - trect.left)) as usize;
                    let under = tile.get(ix);
                    tile.set(
                        ix,
                        Rgba {
                            a: colour.a * cov,
                            ..colour
                        }
                        .over(under),
                    );
                }
            }
        }
        edit.commit();
        self.status = "Stroke".into();
        self.after_change(cx);
    }

    /// Edit ▸ Fill.
    pub fn fill_selection(&mut self, source: FillSource, opacity: f32, cx: &mut Context<Self>) {
        let colour = match source {
            FillSource::Foreground => self.editor.foreground,
            FillSource::Background => self.editor.background,
            FillSource::Black => Rgba::new(0.0, 0.0, 0.0, 1.0),
            FillSource::White => Rgba::new(1.0, 1.0, 1.0, 1.0),
            FillSource::Gray => Rgba::new(0.5, 0.5, 0.5, 1.0),
            FillSource::ContentAware => {
                self.content_aware_fill(cx);
                return;
            }
        };
        let Some(doc) = self.doc.as_mut() else { return };
        let Some(layer) = doc.active_layer else {
            return;
        };
        let canvas = doc.canvas_rect();
        let rect = if doc.selection.is_empty() {
            canvas
        } else {
            doc.selection.bounds().intersect(&canvas)
        };
        if rect.is_empty() {
            return;
        }
        let selection = doc.selection.clone();
        let mut edit = doc.begin_edit("Fill");
        for coord in TileCoord::covering(&rect) {
            let trect = coord.rect();
            let clip = trect.intersect(&rect);
            if clip.is_empty() {
                continue;
            }
            let Some(tile) = edit.writable_tile(layer, coord) else {
                break;
            };
            for y in clip.top..clip.bottom {
                for x in clip.left..clip.right {
                    let cov = selection.coverage(x, y) as f32 / 255.0 * opacity;
                    if cov <= 0.0 {
                        continue;
                    }
                    let ix = ((y - trect.top) * TILE_SIZE + (x - trect.left)) as usize;
                    let under = tile.get(ix);
                    tile.set(
                        ix,
                        Rgba {
                            a: colour.a * cov,
                            ..colour
                        }
                        .over(under),
                    );
                }
            }
        }
        edit.commit();
        self.status = "Fill".into();
        self.after_change(cx);
    }

    /// Edit ▸ Content-Aware Fill: grow the surroundings over the selection.
    pub fn content_aware_fill(&mut self, cx: &mut Context<Self>) {
        let Some(doc) = self.doc.as_mut() else { return };
        if doc.selection.is_empty() {
            self.status = "Content-Aware Fill needs a selection".into();
            cx.notify();
            return;
        }
        let Some(layer) = doc.active_layer else {
            return;
        };
        let canvas = doc.canvas_rect();
        let sel_rect = doc.selection.bounds().intersect(&canvas);
        // Work over a margin so the fill has surroundings to grow from.
        let rect = IntRect::new(
            sel_rect.left - 16,
            sel_rect.top - 16,
            sel_rect.right + 16,
            sel_rect.bottom + 16,
        )
        .intersect(&canvas);
        let Some(tiles) = doc
            .tree
            .find(layer)
            .and_then(|l| l.as_raster())
            .map(|r| r.tiles.clone())
        else {
            self.status = "Content-Aware Fill needs a pixel layer".into();
            cx.notify();
            return;
        };
        let selection = doc.selection.clone();
        let (w, h) = (rect.width().max(0) as usize, rect.height().max(0) as usize);
        let mut hole = vec![false; w * h];
        for y in 0..h {
            for x in 0..w {
                hole[y * w + x] =
                    selection.coverage(rect.left + x as i32, rect.top + y as i32) >= 128;
            }
        }
        let filled = photoslop_tools_retouch::inpaint(&tiles, rect, &hole);
        let mut edit = doc.begin_edit("Content-Aware Fill");
        for coord in TileCoord::covering(&rect) {
            let trect = coord.rect();
            let clip = trect.intersect(&rect);
            if clip.is_empty() {
                continue;
            }
            let Some(tile) = edit.writable_tile(layer, coord) else {
                break;
            };
            for y in clip.top..clip.bottom {
                for x in clip.left..clip.right {
                    let cov = selection.coverage(x, y) as f32 / 255.0;
                    if cov <= 0.0 {
                        continue;
                    }
                    let ix = ((y - trect.top) * TILE_SIZE + (x - trect.left)) as usize;
                    let under = tile.get(ix);
                    let patch = filled[(y - rect.top) as usize * w + (x - rect.left) as usize];
                    tile.set(
                        ix,
                        Rgba {
                            r: under.r + (patch.r - under.r) * cov,
                            g: under.g + (patch.g - under.g) * cov,
                            b: under.b + (patch.b - under.b) * cov,
                            a: under.a + (patch.a - under.a) * cov,
                        },
                    );
                }
            }
        }
        edit.commit();
        self.status = "Content-Aware Fill".into();
        self.after_change(cx);
    }

    /// Edit ▸ Content-Aware Scale: resize the canvas by carving seams
    /// rather than squashing everything equally.
    pub fn content_aware_scale(&mut self, width: u32, height: u32, cx: &mut Context<Self>) {
        let Some(doc) = self.doc.as_mut() else { return };
        if width == 0 || height == 0 || (width, height) == (doc.width, doc.height) {
            return;
        }
        let canvas = doc.canvas_rect();
        // The selection marks what to protect, as Photoshop's Protect
        // channel does.
        let protect = (!doc.selection.is_empty()).then(|| doc.selection.clone());
        let depth = doc.depth;
        let ids: Vec<photoslop_core::LayerId> = doc.tree.iter().map(|l| l.id).collect();
        let mut carved: Vec<(photoslop_core::LayerId, photoslop_core::TileMap)> = Vec::new();
        for id in &ids {
            let Some(raster) = doc.tree.find(*id).and_then(|l| l.as_raster()) else {
                continue;
            };
            let mut img = photoslop_tools_warp::scale::Image::from_tiles(
                &raster.tiles,
                canvas,
                protect.as_ref(),
            );
            img.content_aware_resize(width as usize, height as usize);
            carved.push((
                *id,
                img.into_tiles(IntRect::from_size(width, height), depth),
            ));
        }
        let mut edit = doc.begin_edit("Content-Aware Scale");
        for (id, tiles) in carved {
            edit.replace_layer_tiles(id, tiles);
        }
        edit.set_canvas_size(width, height);
        edit.change_selection(|sel, _| sel.deselect());
        edit.commit();
        self.status = "Content-Aware Scale".into();
        self.fit_to_view();
        self.after_change(cx);
    }

    /// Open the Filter Gallery.
    pub fn show_filter_gallery(&mut self, cx: &mut Context<Self>) {
        if !self.begin_filter_preview() {
            cx.notify();
            return;
        }
        // Start with one filter so the panel has something to show.
        let first = self
            .registry
            .filters()
            .find(|f| f.category() == "Stylize")
            .or_else(|| self.registry.filters().next());
        let stack = first
            .map(|f| {
                vec![GalleryEntry {
                    id: f.id(),
                    values: photoslop_plugin_api::FilterValues::defaults(&f.params()),
                    enabled: true,
                }]
            })
            .unwrap_or_default();
        self.preview_gallery(&stack, cx);
        self.open_modal(
            Modal::FilterGallery {
                stack,
                selected: 0,
                preview: true,
            },
            cx,
        );
    }

    /// Run a gallery stack over the preview snapshot, bottom to top.
    fn run_gallery(&self, stack: &[GalleryEntry], buf: &mut [f32], w: usize, h: usize) {
        for entry in stack {
            if !entry.enabled {
                continue;
            }
            let Some(filter) = self.registry.filters().find(|f| f.id() == entry.id) else {
                continue;
            };
            filter.apply(buf, w, h, &entry.values);
        }
    }

    pub fn preview_gallery(&mut self, stack: &[GalleryEntry], cx: &mut Context<Self>) {
        let Some(preview) = self.filter_preview.clone() else {
            return;
        };
        let (w, h) = (
            preview.region.width() as usize,
            preview.region.height() as usize,
        );
        let mut buf = preview.original.clone();
        self.run_gallery(stack, &mut buf, w, h);
        self.write_region(
            preview.layer,
            preview.region,
            &preview.original,
            &buf,
            "",
            false,
        );
        self.after_change(cx);
    }

    /// Bake the stack in as one history entry.
    pub fn commit_gallery(&mut self, stack: &[GalleryEntry], cx: &mut Context<Self>) {
        // Restore first so the recorded edit has the right "before".
        self.preview_gallery(&[], cx);
        let Some(preview) = self.filter_preview.take() else {
            return;
        };
        let (w, h) = (
            preview.region.width() as usize,
            preview.region.height() as usize,
        );
        let mut buf = preview.original.clone();
        self.run_gallery(stack, &mut buf, w, h);
        self.write_region(
            preview.layer,
            preview.region,
            &preview.original,
            &buf,
            "Filter Gallery",
            true,
        );
        self.status = "Filter Gallery".into();
        self.after_change(cx);
    }

    /// Capture every layer's visibility and appearance as a named comp.
    pub fn new_layer_comp(&mut self, cx: &mut Context<Self>) {
        let Some(doc) = self.doc.as_mut() else { return };
        let states: Vec<photoslop_core::LayerCompState> = doc
            .tree
            .iter()
            .map(|l| photoslop_core::LayerCompState {
                layer: l.id,
                visible: l.visible,
                opacity: l.opacity,
                fill_opacity: l.fill_opacity,
                blend: l.blend,
                style: l.style,
            })
            .collect();
        let n = doc.layer_comps.len() + 1;
        let mut comp = photoslop_core::LayerComp::new(format!("Layer Comp {n}"));
        comp.states = states;
        doc.layer_comps.push(comp);
        self.status = "Layer comp captured".into();
        cx.notify();
    }

    /// Restore a comp. Pixels are untouched: a comp is a way of showing
    /// the same artwork several ways, not a second copy of it.
    pub fn apply_layer_comp(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(doc) = self.doc.as_mut() else { return };
        let Some(comp) = doc.layer_comps.get(index).cloned() else {
            return;
        };
        let mut edit = doc.begin_edit(format!("Apply {}", comp.name));
        for state in &comp.states {
            edit.change_props(state.layer, |l| {
                if comp.apply_visibility {
                    l.visible = state.visible;
                }
                if comp.apply_appearance {
                    l.opacity = state.opacity;
                    l.fill_opacity = state.fill_opacity;
                    l.blend = state.blend;
                    l.style = state.style;
                    // The cached raster belongs to the old style.
                    l.styled = None;
                }
            });
        }
        edit.commit();
        self.status = comp.name.into();
        self.after_change(cx);
    }

    pub fn delete_layer_comp(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(doc) = self.doc.as_mut() {
            if index < doc.layer_comps.len() {
                doc.layer_comps.remove(index);
            }
        }
        cx.notify();
    }

    /// Export every artboard, or every slice, as its own file next to the
    /// document.
    pub fn export_regions(&mut self, slices: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(doc) = self.doc.as_ref() else { return };
        let regions: Vec<(String, IntRect)> = if slices {
            doc.slices
                .iter()
                .map(|s| (s.name.clone(), s.rect))
                .collect()
        } else {
            doc.artboards
                .iter()
                .map(|a| (a.name.clone(), a.rect))
                .collect()
        };
        if regions.is_empty() {
            self.status = if slices {
                "No slices to export".into()
            } else {
                "No artboards to export".into()
            };
            cx.notify();
            return;
        }
        let dir = doc
            .path
            .as_ref()
            .and_then(|p| p.parent().map(|p| p.to_path_buf()))
            .or_else(|| std::env::var("HOME").ok().map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("."));
        let rx = cx.prompt_for_new_path(&dir, Some("export"));
        let doc_regions = regions;
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(path))) = rx.await {
                this.update_in(cx, |ws, _window, cx| {
                    ws.write_regions(&path, &doc_regions, cx);
                })
                .ok();
            }
        })
        .detach();
    }

    /// Write one PNG per region, named `<stem>-<region>.png`.
    fn write_regions(
        &mut self,
        base: &std::path::Path,
        regions: &[(String, IntRect)],
        cx: &mut Context<Self>,
    ) {
        let Some(doc) = self.doc.as_ref() else { return };
        let stem = base
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "export".into());
        let dir = base.parent().unwrap_or(std::path::Path::new("."));
        let mut written = 0usize;
        for (name, rect) in regions {
            let rect = rect.intersect(&doc.canvas_rect());
            if rect.is_empty() {
                continue;
            }
            // Codecs export a whole document, so each region becomes a
            // one-layer document of its flattened pixels.
            let rgba = photoslop_compositor::composite_region_rgba8(doc, rect);
            let mut region_doc = Document::new(
                name.clone(),
                rect.width() as u32,
                rect.height() as u32,
                doc.depth,
            );
            let mut layer = Layer::new_raster(name.clone());
            photoslop_core::blit_rgba8(
                &mut layer.as_raster_mut().unwrap().tiles,
                doc.depth,
                IntRect::from_size(rect.width() as u32, rect.height() as u32),
                &rgba,
            );
            region_doc.push_layer(layer);
            let safe: String = name
                .chars()
                .map(|c| if c.is_alphanumeric() { c } else { '-' })
                .collect();
            let out = dir.join(format!("{stem}-{safe}.png"));
            let Some(codec) = self.registry.codecs().find(|c| c.id() == "png") else {
                continue;
            };
            match codec.export(&region_doc) {
                Ok(bytes) => {
                    if std::fs::write(&out, bytes).is_ok() {
                        written += 1;
                    }
                }
                Err(e) => log::error!("export {name}: {e}"),
            }
        }
        self.status = format!("Exported {written} region(s)").into();
        cx.notify();
    }

    /// Push the internal clipboard out to the system clipboard as a PNG.
    ///
    /// Photoslop's own copy/paste has always worked between its documents;
    /// this is what makes it work with everything else.
    pub fn sync_clipboard_out(&mut self, cx: &mut Context<Self>) {
        let Some(clip) = self.editor.clipboard.clone() else {
            return;
        };
        let (w, h) = (clip.rect.width() as u32, clip.rect.height() as u32);
        if w == 0 || h == 0 {
            return;
        }
        let Some(codec) = self.registry.codecs().find(|c| c.id() == "png") else {
            return;
        };
        // Codecs export documents, so the clipboard becomes a one-layer one.
        let mut doc = Document::new("clipboard", w, h, Depth::Eight);
        let mut layer = Layer::new_raster("clipboard");
        photoslop_core::blit_rgba8(
            &mut layer.as_raster_mut().unwrap().tiles,
            Depth::Eight,
            IntRect::from_size(w, h),
            &clip.rgba,
        );
        doc.push_layer(layer);
        match codec.export(&doc) {
            Ok(bytes) => {
                let image = gpui::Image::from_bytes(gpui::ImageFormat::Png, bytes);
                cx.write_to_clipboard(gpui::ClipboardItem::new_image(&image));
            }
            Err(e) => log::error!("clipboard export: {e}"),
        }
    }

    /// Pull an image off the system clipboard into the internal one.
    ///
    /// Returns false when the clipboard holds nothing we can use, so the
    /// caller can fall back to whatever was copied inside the app.
    pub fn sync_clipboard_in(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(item) = cx.read_from_clipboard() else {
            return false;
        };
        for entry in item.entries() {
            let gpui::ClipboardEntry::Image(image) = entry else {
                continue;
            };
            if image.bytes.is_empty() {
                continue;
            }
            // Route it through the codecs, so anything Photoslop can open
            // it can also paste.
            let Some(codec) = self.registry.codecs().find(|c| c.probe(&image.bytes)) else {
                continue;
            };
            match codec.import(&image.bytes) {
                Ok(doc) => {
                    let rect = doc.canvas_rect();
                    let rgba = photoslop_compositor::composite_region_rgba8(&doc, rect);
                    self.editor.clipboard = Some(Arc::new(photoslop_plugin_api::ClipboardImage {
                        rect,
                        rgba,
                    }));
                    return true;
                }
                Err(e) => log::error!("clipboard import: {e}"),
            }
        }
        false
    }

    /// Download a Neural Filters model and install it.
    ///
    /// Runs off the UI thread: these are megabytes over the network, and
    /// the window should stay usable while one arrives.
    pub fn download_model(&mut self, id: &'static str, cx: &mut Context<Self>) {
        let Some(spec) = photoslop_neural::spec(id) else {
            return;
        };
        let Some(url) = spec.url else { return };
        if self.model_downloads.contains(&id) {
            return;
        }
        self.model_downloads.push(id);
        self.status = format!("Downloading {}\u{2026}", spec.name).into();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let fetched = cx
                .background_executor()
                .spawn(async move { fetch_model(url) })
                .await;
            this.update(cx, |ws, cx| {
                ws.model_downloads.retain(|d| *d != id);
                let Some(spec) = photoslop_neural::spec(id) else {
                    return;
                };
                ws.status = match fetched.and_then(|bytes| {
                    photoslop_neural::install(spec, &bytes).map_err(|e| e.to_string())
                }) {
                    Ok(path) => format!("Installed {} to {}", spec.name, path.display()).into(),
                    Err(e) => format!("{}: {e}", spec.name).into(),
                };
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub fn remove_model(&mut self, id: &'static str, cx: &mut Context<Self>) {
        let Some(spec) = photoslop_neural::spec(id) else {
            return;
        };
        self.status = match photoslop_neural::uninstall(spec) {
            Ok(()) => format!("Removed {}", spec.name).into(),
            Err(e) => format!("{e}").into(),
        };
        cx.notify();
    }

    /// Re-rasterize any layer whose effects are stale.
    ///
    /// The styled raster is derived from the layer's pixels plus its
    /// style, so it has to be rebuilt whenever either moves. Layers with
    /// no effects keep `styled == None` and cost nothing.
    fn refresh_layer_styles(&mut self) {
        let Some(doc) = self.doc.as_mut() else { return };
        let mut grew = Vec::new();
        // Shape layers first: their pixels are derived from their path,
        // and any effects are derived from those pixels in turn.
        let depth = doc.depth;
        let canvas = doc.canvas_rect();
        reshape_layers(&mut doc.tree.layers, depth, canvas, &mut grew);
        restyle_layers(&mut doc.tree.layers, &mut grew);
        // A shadow can appear outside the layer's old bounds, so the
        // newly covered area has to be repainted too.
        for rect in grew {
            doc.add_damage(rect);
        }
    }

    pub fn after_change(&mut self, cx: &mut Context<Self>) {
        self.refresh_layer_styles();
        if let Some(doc) = &mut self.doc {
            let damage = doc.take_damage();
            for rect in &damage {
                self.cache.invalidate(rect);
                for coord in TileCoord::covering(rect) {
                    self.display_tiles.remove(&coord);
                }
                self.preview.dirty.push(*rect);
            }
        }
        cx.notify();
    }

    pub fn run_command(&mut self, id: &str, cx: &mut Context<Self>) {
        // Grow, Similar and Color Range take their tolerance from the
        // magic wand, exactly as Photoshop does.
        self.sync_wand_tolerance();
        // Pasting prefers whatever is on the system clipboard, so copying
        // in another application and pasting here works. If there is no
        // image there, the internal clipboard is used unchanged.
        if id.starts_with("edit.paste") {
            self.sync_clipboard_in(cx);
        }
        let Some(doc) = self.doc.as_mut() else { return };
        if let Some(command) = self.registry.command(id) {
            let mut ctx = CommandCtx {
                doc,
                state: &mut self.editor,
            };
            (command.run)(&mut ctx);
            self.status = command.title.into();
            // ...and copying makes the pixels available everywhere else.
            if id.starts_with("edit.copy") || id == "edit.cut" {
                self.sync_clipboard_out(cx);
            }
        } else {
            log::warn!("unknown command {id}");
        }
        self.after_change(cx);
    }

    /// Mirror the magic wand's tolerance into the shared editor state.
    pub fn sync_wand_tolerance(&mut self) {
        if let Some(t) = self
            .registry
            .tools()
            .find(|t| t.id() == "wand")
            .and_then(|t| t.options().into_iter().find(|o| o.key == "wand-tolerance"))
        {
            self.editor.tolerance = t.value.num().round().clamp(0.0, 255.0) as u8;
        }
    }

    pub fn activate_tool(&mut self, id: &str, cx: &mut Context<Self>) {
        let previous = self.editor.active_tool;
        if previous != id {
            if let (Some(doc), Some(tool)) = (self.doc.as_mut(), self.registry.tool_mut(previous)) {
                let mut ctx = ToolCtx {
                    doc,
                    state: &mut self.editor,
                };
                tool.on_deactivate(&mut ctx);
            }
        }
        if let Some(tool) = self.registry.tool_mut(id) {
            let id = tool.id();
            let name = tool.name();
            let group = tool.group();
            self.group_active.insert(group, id);
            self.editor.active_tool = id;
            self.status = format!("Tool: {name}").into();
            if let (Some(doc), Some(tool)) = (self.doc.as_mut(), self.registry.tool_mut(id)) {
                let mut ctx = ToolCtx {
                    doc,
                    state: &mut self.editor,
                };
                tool.on_activate(&mut ctx);
            }
        }
        self.after_change(cx);
    }

    /// Apply an options-bar change to the active tool, then let it react
    /// with the document available.
    pub fn set_tool_option(
        &mut self,
        key: &'static str,
        value: photoslop_plugin_api::OptionValue,
        cx: &mut Context<Self>,
    ) {
        let tool_id = self.editor.active_tool;
        let Some(tool) = self.registry.tool_mut(tool_id) else {
            return;
        };
        tool.set_option(key, value);
        if let (Some(doc), Some(tool)) = (self.doc.as_mut(), self.registry.tool_mut(tool_id)) {
            let mut ctx = ToolCtx {
                doc,
                state: &mut self.editor,
            };
            tool.on_option_changed(&mut ctx, key);
        }
        self.after_change(cx);
    }

    // ----- input routing -----

    fn panning_tool(&self) -> bool {
        self.space_held || self.editor.active_tool == "hand"
    }

    /// Build a tool's pointer input.
    ///
    /// `pressure` comes straight from the platform event. It is 1.0 for a
    /// mouse and wherever tablet input is not wired up, so tools multiply
    /// by it unconditionally.
    fn tool_input(&self, local: Point<Pixels>, m: gpui::Modifiers, pressure: f32) -> PointerInput {
        let (x, y) = self.doc_pos(local);
        // Snapping is a view affordance, so it happens here rather than in
        // every tool.
        let (x, y) = self.snap_point(x, y);
        PointerInput {
            x,
            y,
            pressure,
            modifiers: Modifiers {
                shift: m.shift,
                alt: m.alt,
                ctrl_or_cmd: m.control || m.platform,
            },
        }
    }

    fn on_mouse_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus);
        let local = self.to_local(ev.position);
        if ev.button == MouseButton::Middle || self.panning_tool() {
            self.pan_last = Some(ev.position);
            return;
        }
        if self.editor.active_tool == "zoom" {
            let factor = if ev.modifiers.alt { 1.0 / 1.5 } else { 1.5 };
            self.zoom_by(factor, Some(local));
            cx.notify();
            return;
        }
        if ev.button != MouseButton::Left {
            return;
        }
        self.pointer_down = true;
        let input = self.tool_input(local, ev.modifiers, ev.pressure);
        let tool_id = self.editor.active_tool;
        if let (Some(doc), Some(tool)) = (self.doc.as_mut(), self.registry.tool_mut(tool_id)) {
            let mut ctx = ToolCtx {
                doc,
                state: &mut self.editor,
            };
            tool.on_pointer_down(&mut ctx, input);
        }
        self.after_change(cx);
    }

    fn on_mouse_move(&mut self, ev: &MouseMoveEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if self.dragging_guide() {
            let horizontal = self.dragging_guide.map(|g| g.horizontal).unwrap_or(false);
            let position = if horizontal {
                self.doc_y_at(f32::from(ev.position.y))
            } else {
                self.doc_x_at(f32::from(ev.position.x))
            };
            self.update_guide(position);
            cx.notify();
            return;
        }
        if let Some(last) = self.pan_last {
            self.offset = point(
                self.offset.x + (ev.position.x - last.x),
                self.offset.y + (ev.position.y - last.y),
            );
            self.pan_last = Some(ev.position);
            cx.notify();
            return;
        }
        let local = self.to_local(ev.position);
        let input = self.tool_input(local, ev.modifiers, ev.pressure);
        let tool_id = self.editor.active_tool;
        if let (Some(doc), Some(tool)) = (self.doc.as_mut(), self.registry.tool_mut(tool_id)) {
            let mut ctx = ToolCtx {
                doc,
                state: &mut self.editor,
            };
            tool.on_pointer_move(&mut ctx, input);
        }
        self.after_change(cx);
    }

    fn on_mouse_up(&mut self, ev: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if self.dragging_guide() {
            self.finish_guide(cx);
            return;
        }
        if self.pan_last.take().is_some() {
            return;
        }
        if !self.pointer_down {
            return;
        }
        self.pointer_down = false;
        let local = self.to_local(ev.position);
        let input = self.tool_input(local, ev.modifiers, ev.pressure);
        let tool_id = self.editor.active_tool;
        if let (Some(doc), Some(tool)) = (self.doc.as_mut(), self.registry.tool_mut(tool_id)) {
            let mut ctx = ToolCtx {
                doc,
                state: &mut self.editor,
            };
            tool.on_pointer_up(&mut ctx, input);
        }
        self.after_change(cx);
    }

    fn on_scroll(&mut self, ev: &ScrollWheelEvent, _window: &mut Window, cx: &mut Context<Self>) {
        // Touchpads send many small precise deltas; a mouse wheel sends a
        // few large line-sized ones. Scaling lines to 30px puts both on a
        // comparable footing.
        let delta = ev.delta.pixel_delta(px(30.0));
        // Ctrl (or Cmd, or Alt — Photoshop's Windows binding) flips the
        // gesture's meaning, whichever way round the preference has it.
        let modifier = ev.modifiers.control || ev.modifiers.platform || ev.modifiers.alt;
        let zooming = if self.view.zoom_with_scroll {
            !modifier
        } else {
            modifier
        };
        if zooming {
            // Exponential so the gesture is symmetric: scrolling back up
            // returns to exactly the zoom you started from, and a precise
            // touchpad delta of a couple of pixels still moves it a little
            // rather than rounding away to nothing.
            let steps = f32::from(delta.y) / 240.0;
            if steps.abs() > f32::EPSILON {
                let local = self.to_local(ev.position);
                self.zoom_by(2f32.powf(steps), Some(local));
            }
        } else {
            self.offset = point(self.offset.x + delta.x, self.offset.y + delta.y);
        }
        cx.notify();
    }

    /// Trackpad pinch-to-zoom.
    ///
    /// Unlike the scroll path this is unconditional: a pinch has only one
    /// sensible meaning, so it ignores the zoom-with-scroll preference and
    /// any modifiers. Only macOS and Wayland deliver these, which is why
    /// modifier+scroll zoom stays.
    fn on_pinch(&mut self, ev: &PinchEvent, _window: &mut Window, cx: &mut Context<Self>) {
        // `delta` is already the multiplicative change since the previous
        // event of the gesture, so it composes straight into `zoom_by`.
        if ev.phase != TouchPhase::Moved || !(ev.delta.is_finite() && ev.delta > 0.0) {
            return;
        }
        let local = self.to_local(ev.position);
        self.zoom_by(ev.delta, Some(local));
        cx.notify();
    }

    // ----- toolbar groups -----

    /// Collect the registry's tools into toolbar groups, preserving
    /// registration order both between and within groups.
    fn rebuild_tool_groups(&mut self) {
        let mut groups: Vec<(&'static str, Vec<&'static str>)> = Vec::new();
        for tool in self.registry.tools() {
            if !tool.in_toolbar() {
                continue;
            }
            let group = tool.group();
            match groups.iter_mut().find(|(g, _)| *g == group) {
                Some((_, tools)) => tools.push(tool.id()),
                None => groups.push((group, vec![tool.id()])),
            }
        }
        // Lay the slots out the way Photoshop does rather than in plugin
        // registration order. Groups not listed here (third-party tools)
        // keep their registration order and follow the built-ins.
        const ORDER: &[&str] = &[
            "move",
            "marquee",
            "lasso",
            "wand",
            "crop",
            "eyedropper",
            "brush",
            "clone",
            "eraser",
            "gradient",
            "dodge",
            "pen",
            "type",
            "shape",
            "hand",
            "zoom",
        ];
        groups
            .sort_by_key(|(group, _)| ORDER.iter().position(|g| g == group).unwrap_or(ORDER.len()));
        for (group, tools) in &groups {
            self.group_active.entry(group).or_insert(tools[0]);
        }
        self.tool_groups = groups;
    }

    /// The tool a group's toolbar slot currently represents.
    pub fn group_tool(&self, group: &'static str) -> &'static str {
        self.group_active
            .get(group)
            .copied()
            .or_else(|| {
                self.tool_groups
                    .iter()
                    .find(|(g, _)| *g == group)
                    .and_then(|(_, tools)| tools.first().copied())
            })
            .unwrap_or("move")
    }

    /// Keyboard shortcut shown for a group (Photoshop gives every tool in a
    /// group the same letter).
    pub fn group_shortcut(&mut self, group: &'static str) -> Option<&'static str> {
        let ids: Vec<&'static str> = self
            .tool_groups
            .iter()
            .find(|(g, _)| *g == group)
            .map(|(_, t)| t.clone())
            .unwrap_or_default();
        ids.into_iter()
            .find_map(|id| self.registry.tool_mut(id).and_then(|t| t.shortcut()))
    }

    /// Press on a toolbar slot: hold opens the flyout, a click activates.
    pub fn press_tool_group(
        &mut self,
        group: &'static str,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.tool_press = Some(group);
        let has_siblings = self
            .tool_groups
            .iter()
            .any(|(g, tools)| *g == group && tools.len() > 1);
        if !has_siblings {
            return;
        }
        // Click-and-hold, like Photoshop's nested tools.
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(350))
                .await;
            this.update(cx, |ws, cx| {
                if ws.tool_press == Some(group) {
                    ws.open_tool_flyout(group, position, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Release on a toolbar slot: activate unless the hold already opened
    /// the flyout.
    pub fn release_tool_group(&mut self, group: &'static str, cx: &mut Context<Self>) {
        let pressed = self.tool_press.take();
        if pressed != Some(group) || self.tool_flyout.is_some() {
            return;
        }
        let tool = self.group_tool(group);
        self.activate_tool(tool, cx);
    }

    pub fn open_tool_flyout(
        &mut self,
        group: &'static str,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.tool_flyout = Some((group, position));
        self.context_menu = None;
        cx.notify();
    }

    pub fn close_tool_flyout(&mut self, cx: &mut Context<Self>) {
        self.tool_press = None;
        if self.tool_flyout.take().is_some() {
            cx.notify();
        }
    }

    /// Shift+the group's key steps to the next tool in that group.
    pub fn cycle_tool_group(&mut self, group: &'static str, cx: &mut Context<Self>) {
        let tools: Vec<&'static str> = self
            .tool_groups
            .iter()
            .find(|(g, _)| *g == group)
            .map(|(_, t)| t.clone())
            .unwrap_or_default();
        if tools.is_empty() {
            return;
        }
        let current = self.group_tool(group);
        let next = tools
            .iter()
            .position(|t| *t == current)
            .map(|i| tools[(i + 1) % tools.len()])
            .unwrap_or(tools[0]);
        self.activate_tool(next, cx);
    }

    // ----- context menus -----

    /// Open a right-click menu at `position`.
    pub fn open_context_menu(
        &mut self,
        target: ContextTarget,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        // Right-clicking a layer selects it first, like Photoshop.
        if let (ContextTarget::Layer(id), Some(doc)) = (target, self.doc.as_mut()) {
            doc.active_layer = Some(id);
        }
        self.context_menu = Some(ContextMenu { position, target });
        self.open_popup = None;
        cx.notify();
    }

    pub fn close_context_menu(&mut self, cx: &mut Context<Self>) {
        if self.context_menu.take().is_some() {
            cx.notify();
        }
    }

    /// Open Layer Properties for the given layer.
    pub fn open_layer_properties(
        &mut self,
        layer: photoslop_core::LayerId,
        cx: &mut Context<Self>,
    ) {
        let Some(name) = self
            .doc
            .as_ref()
            .and_then(|d| d.tree.find(layer))
            .map(|l| l.name.clone())
        else {
            return;
        };
        self.open_modal(Modal::LayerProperties { layer, name }, cx);
    }

    /// Commit a rename from the Layer Properties dialog.
    pub fn rename_layer(
        &mut self,
        layer: photoslop_core::LayerId,
        name: String,
        cx: &mut Context<Self>,
    ) {
        if let Some(doc) = self.doc.as_mut() {
            let trimmed = name.trim();
            if trimmed.is_empty() {
                return;
            }
            let mut edit = doc.begin_edit("Rename Layer");
            edit.change_props(layer, |l| l.name = trimmed.to_string());
            edit.commit();
        }
        self.after_change(cx);
    }

    // ----- modals and numeric fields -----

    pub fn open_modal(&mut self, modal: Modal, cx: &mut Context<Self>) {
        self.modal = Some(modal);
        self.context_menu = None;
        self.focused_field = None;
        self.field_buffer.clear();
        self.open_popup = None;
        cx.notify();
    }

    /// Open Photoshop's Color Picker on one of the two editor colours.
    pub fn open_color_picker(&mut self, target: ColorTarget, cx: &mut Context<Self>) {
        let original = match target {
            ColorTarget::Foreground => self.editor.foreground,
            ColorTarget::Background => self.editor.background,
        };
        let hsv = crate::color_picker::rgb_to_hsv(original.r, original.g, original.b);
        self.open_modal(
            Modal::ColorPicker {
                target,
                hsv,
                original,
            },
            cx,
        );
    }

    /// Take the picker's colour and close it. Cancel does nothing, because
    /// the picker never wrote to the editor while it was open.
    pub fn commit_color_picker(&mut self, cx: &mut Context<Self>) {
        let Some(Modal::ColorPicker { target, hsv, .. }) = self.modal.as_ref() else {
            return;
        };
        let (target, (h, s, v)) = (*target, *hsv);
        let (r, g, b) = crate::color_picker::hsv_to_rgb(h, s, v);
        let colour = Rgba::new(r, g, b, 1.0);
        match target {
            ColorTarget::Foreground => self.editor.foreground = colour,
            ColorTarget::Background => self.editor.background = colour,
        }
        self.close_modal(cx);
    }

    /// The ± buttons beside a picker component.
    pub fn nudge_color_component(&mut self, id: &'static str, delta: f32) {
        self.update_modal(|m| {
            if let Modal::ColorPicker { hsv, .. } = m {
                crate::color_picker::nudge(hsv, id, delta);
            }
        });
    }

    pub fn close_modal(&mut self, cx: &mut Context<Self>) {
        // Any filter preview still on the canvas belongs to the dialog that
        // is going away, so put the original pixels back. Committing a
        // filter clears the preview first, so this only fires on cancel.
        self.cancel_filter_preview(cx);
        // Same for a cancelled Layer Style session: OK clears the modal
        // itself before it gets here, so reaching this means Cancel.
        self.revert_layer_style();
        self.modal = None;
        self.focused_field = None;
        self.field_buffer.clear();
        self.open_popup = None;
        cx.notify();
    }

    /// Mutate the open modal's state in place.
    pub fn update_modal(&mut self, f: impl FnOnce(&mut Modal)) {
        if let Some(modal) = &mut self.modal {
            f(modal);
        }
    }

    pub fn focus_field(&mut self, id: &'static str) {
        self.focused_field = Some(id);
        self.field_buffer.clear();
    }

    /// Feed a keystroke to the focused numeric field. Returns true when the
    /// field consumed it.
    fn field_key(&mut self, key: &str, text: Option<&str>) -> bool {
        let Some(id) = self.focused_field else {
            return false;
        };
        // Text fields (layer names) take any printable character; the
        // picker's hex field takes hex digits up to a full triplet;
        // numeric fields only digits.
        let textual = id == "layer-name";
        let hex = id == "cp-hex";
        match key {
            "space" if textual => self.field_buffer.push(' '),
            "backspace" => {
                self.field_buffer.pop();
            }
            "escape" => {
                self.focused_field = None;
                self.field_buffer.clear();
                return true;
            }
            "enter" | "tab" => {
                self.commit_field(id);
                return true;
            }
            _ => match text {
                Some(t)
                    if !t.is_empty()
                        && !t.chars().any(char::is_control)
                        && (textual
                            || (hex
                                && self.field_buffer.len() + t.len() <= 6
                                && t.chars().all(|c| c.is_ascii_hexdigit()))
                            || (!hex && t.chars().all(|c| c.is_ascii_digit()))) =>
                {
                    self.field_buffer.push_str(t)
                }
                _ => return false,
            },
        }
        // Apply as you type so the dialog stays live.
        self.commit_field_value(id);
        true
    }

    fn commit_field(&mut self, id: &'static str) {
        self.commit_field_value(id);
        self.focused_field = None;
        self.field_buffer.clear();
    }

    fn commit_field_value(&mut self, id: &'static str) {
        let buffer = self.field_buffer.clone();
        if id == "layer-name" {
            self.update_modal(|m| {
                if let Modal::LayerProperties { name, .. } = m {
                    *name = buffer;
                }
            });
            return;
        }
        if id == "cp-hex" {
            if let Some(c) = crate::color_picker::parse_hex(&buffer) {
                let typed = crate::color_picker::rgb_to_hsv(c.r, c.g, c.b);
                self.update_modal(|m| {
                    if let Modal::ColorPicker { hsv, .. } = m {
                        // A grey has no hue to report, so keep the one the
                        // dialog already had rather than snapping to red.
                        if typed.1 > 0.0 {
                            hsv.0 = typed.0;
                        }
                        hsv.1 = typed.1;
                        hsv.2 = typed.2;
                    }
                });
            }
            return;
        }
        if id.starts_with("cp-") {
            if let Ok(value) = buffer.parse::<f32>() {
                self.update_modal(|m| {
                    if let Modal::ColorPicker { hsv, .. } = m {
                        crate::color_picker::set_component(hsv, id, value);
                    }
                });
            }
            return;
        }
        let Ok(value) = self.field_buffer.parse::<f32>() else {
            return;
        };
        // Every remaining field is a dimension, and none of them accept
        // zero.
        let value = value.max(1.0);
        let aspect = self
            .doc
            .as_ref()
            .map(|d| d.width as f32 / d.height.max(1) as f32)
            .unwrap_or(1.0);
        self.update_modal(|m| match m {
            Modal::ImageSize {
                width,
                height,
                link,
                ..
            } => {
                if id == "image-size-w" {
                    *width = value as u32;
                    if *link {
                        *height = (value / aspect).round().max(1.0) as u32;
                    }
                } else if id == "image-size-h" {
                    *height = value as u32;
                    if *link {
                        *width = (value * aspect).round().max(1.0) as u32;
                    }
                }
            }
            Modal::CanvasSize { width, height, .. } => {
                if id == "canvas-size-w" {
                    *width = value as u32;
                } else if id == "canvas-size-h" {
                    *height = value as u32;
                }
            }
            Modal::LayerProperties { name, .. } => {
                if id == "layer-name" {
                    *name = buffer;
                }
            }
            Modal::ContentAwareScale { width, height } => {
                if id == "cas-width" {
                    *width = value as u32;
                } else if id == "cas-height" {
                    *height = value as u32;
                }
            }
            // These dialogs have no typed fields.
            Modal::DestructiveAdjustment { .. }
            | Modal::ModelManager
            | Modal::FilterGallery { .. }
            | Modal::Stroke { .. }
            | Modal::Fill { .. }
            | Modal::SelectModify { .. }
            | Modal::ColorRange { .. }
            | Modal::LayerStyle { .. }
            | Modal::Filter { .. }
            | Modal::Adjustment { .. }
            // Handled above, before the numeric parse: a colour component
            // may legitimately be zero.
            | Modal::ColorPicker { .. }
            | Modal::PluginManager
            | Modal::Preferences
            | Modal::Export { .. }
            | Modal::Profile { .. } => {}
        });
    }

    /// True when the active tool is capturing raw typing.
    pub fn tool_captures_keys(&mut self) -> bool {
        let id = self.editor.active_tool;
        self.registry
            .tool_mut(id)
            .map(|t| t.captures_keys())
            .unwrap_or(false)
    }

    /// Feed a keystroke to the active tool. Returns true if it consumed it.
    fn tool_key(&mut self, ev: &gpui::KeyDownEvent) -> bool {
        let tool_id = self.editor.active_tool;
        let key = ev.keystroke.key.clone();
        let text = ev.keystroke.key_char.clone();
        let modifiers = Modifiers {
            shift: ev.keystroke.modifiers.shift,
            alt: ev.keystroke.modifiers.alt,
            ctrl_or_cmd: ev.keystroke.modifiers.control || ev.keystroke.modifiers.platform,
        };
        let (Some(doc), Some(tool)) = (self.doc.as_mut(), self.registry.tool_mut(tool_id)) else {
            return false;
        };
        let mut ctx = ToolCtx {
            doc,
            state: &mut self.editor,
        };
        tool.on_key(&mut ctx, &key, text.as_deref(), modifiers)
    }

    /// Enter: let the active tool commit its pending gesture.
    pub fn commit_gesture(&mut self, cx: &mut Context<Self>) {
        let tool_id = self.editor.active_tool;
        if let (Some(doc), Some(tool)) = (self.doc.as_mut(), self.registry.tool_mut(tool_id)) {
            let mut ctx = ToolCtx {
                doc,
                state: &mut self.editor,
            };
            tool.on_commit(&mut ctx);
        }
        self.after_change(cx);
    }

    pub fn cancel_gesture(&mut self, cx: &mut Context<Self>) {
        if self.tool_flyout.is_some() {
            self.close_tool_flyout(cx);
            return;
        }
        if self.context_menu.is_some() {
            self.close_context_menu(cx);
            return;
        }
        if self.modal.is_some() {
            self.close_modal(cx);
            return;
        }
        if self.open_popup.is_some() {
            self.close_popup(cx);
            return;
        }
        self.pointer_down = false;
        self.pan_last = None;
        let tool_id = self.editor.active_tool;
        if let (Some(doc), Some(tool)) = (self.doc.as_mut(), self.registry.tool_mut(tool_id)) {
            let mut ctx = ToolCtx {
                doc,
                state: &mut self.editor,
            };
            tool.on_cancel(&mut ctx);
        }
        self.after_change(cx);
    }

    // ----- UI support: popups, sliders, thumbnails, history -----

    pub fn toggle_popup(&mut self, popup: Popup, cx: &mut Context<Self>) {
        self.open_submenu.clear();
        self.open_popup = if self.open_popup == Some(popup) {
            None
        } else {
            Some(popup)
        };
        cx.notify();
    }

    pub fn close_popup(&mut self, cx: &mut Context<Self>) {
        self.open_submenu.clear();
        if self.open_popup.take().is_some() {
            cx.notify();
        }
    }

    pub fn record_slider_bounds(&mut self, id: &'static str, bounds: Bounds<Pixels>) {
        self.slider_bounds.insert(id, bounds);
    }

    /// Position within a recorded box as a 0..=1 pair, with y measured
    /// upwards so it matches how a curve is drawn.
    /// Hand back an image that has just been replaced, so its atlas slot
    /// is freed once the frame that still references it has been painted.
    pub fn retire_image(&mut self, image: Arc<RenderImage>) {
        self.retired_images.push(image);
    }

    pub fn box_position(&self, id: &'static str, window_pos: Point<Pixels>) -> Option<(f32, f32)> {
        let b = self.slider_bounds.get(id)?;
        let (w, h) = (f32::from(b.size.width), f32::from(b.size.height));
        if w <= 0.0 || h <= 0.0 {
            return None;
        }
        let x = (f32::from(window_pos.x) - f32::from(b.origin.x)) / w;
        let y = (f32::from(window_pos.y) - f32::from(b.origin.y)) / h;
        Some((x.clamp(0.0, 1.0), (1.0 - y).clamp(0.0, 1.0)))
    }

    /// 0..=1 ratio of a window position along a slider's recorded track.
    pub fn slider_ratio(&self, id: &'static str, window_pos: Point<Pixels>) -> Option<f32> {
        let b = self.slider_bounds.get(id)?;
        let w = f32::from(b.size.width);
        if w <= 0.0 {
            return None;
        }
        Some(((f32::from(window_pos.x) - f32::from(b.origin.x)) / w).clamp(0.0, 1.0))
    }

    pub fn begin_slider(&mut self, id: &'static str, before: f32) {
        self.active_slider = Some((id, before));
    }

    pub fn dragging_slider(&self, id: &'static str) -> bool {
        matches!(self.active_slider, Some((s, _)) if s == id)
    }

    /// End a slider drag, returning the value it started at.
    pub fn end_slider(&mut self, id: &'static str) -> Option<f32> {
        match self.active_slider {
            Some((s, before)) if s == id => {
                self.active_slider = None;
                Some(before)
            }
            _ => None,
        }
    }

    /// Live (history-free) layer opacity update during a slider drag; the
    /// drag commits one undo step on release via `commit_layer_opacity`.
    pub fn set_layer_opacity_live(&mut self, id: photoslop_core::LayerId, value: f32) {
        if let Some(doc) = &mut self.doc {
            let mut bounds = IntRect::EMPTY;
            if let Some(layer) = doc.tree.find_mut(id) {
                layer.opacity = value;
                bounds = layer.content_bounds();
            }
            doc.add_damage(bounds);
        }
    }

    pub fn commit_layer_opacity(
        &mut self,
        id: photoslop_core::LayerId,
        before: f32,
        cx: &mut Context<Self>,
    ) {
        if let Some(doc) = &mut self.doc {
            let after = doc.tree.find(id).map(|l| l.opacity).unwrap_or(before);
            if (after - before).abs() < 1e-4 {
                return;
            }
            // Rewind silently so the edit records the true before state.
            if let Some(layer) = doc.tree.find_mut(id) {
                layer.opacity = before;
            }
            let mut edit = doc.begin_edit("Layer Opacity");
            edit.change_props(id, |l| l.opacity = after);
            edit.commit();
        }
        self.after_change(cx);
    }

    pub fn set_blend_mode(
        &mut self,
        id: photoslop_core::LayerId,
        mode: photoslop_core::BlendMode,
        cx: &mut Context<Self>,
    ) {
        if let Some(doc) = &mut self.doc {
            let mut edit = doc.begin_edit("Blend Mode");
            edit.change_props(id, |l| l.blend = mode);
            edit.commit();
        }
        self.after_change(cx);
    }

    /// Jump in history: negative = undo n steps, positive = redo n steps.
    pub fn history_jump(&mut self, steps: i32, cx: &mut Context<Self>) {
        if let Some(doc) = &mut self.doc {
            if steps < 0 {
                for _ in 0..(-steps) {
                    if doc.undo().is_none() {
                        break;
                    }
                }
            } else {
                for _ in 0..steps {
                    if doc.redo().is_none() {
                        break;
                    }
                }
            }
        }
        self.after_change(cx);
    }

    /// 36x28 thumbnail of a raster layer over a checkerboard, cached per
    /// document revision.
    pub fn layer_thumbnail(&mut self, id: photoslop_core::LayerId) -> Option<Arc<RenderImage>> {
        const TW: usize = 36;
        const TH: usize = 28;
        let doc = self.doc.as_ref()?;
        if let Some((rev, img)) = self.thumbs.get(&id) {
            if *rev == doc.revision {
                return Some(img.clone());
            }
        }
        let layer = doc.tree.find(id)?;
        let raster = layer.as_raster()?;
        let bounds = {
            let b = layer.content_bounds().intersect(&doc.canvas_rect());
            if b.is_empty() {
                doc.canvas_rect()
            } else {
                b
            }
        };
        if bounds.is_empty() {
            return None;
        }
        let scale = (bounds.width() as f32 / TW as f32).max(bounds.height() as f32 / TH as f32);
        let (w, h) = (
            ((bounds.width() as f32 / scale) as usize).clamp(1, TW),
            ((bounds.height() as f32 / scale) as usize).clamp(1, TH),
        );
        let mut bgra = vec![0u8; w * h * 4];
        for ty in 0..h {
            for tx in 0..w {
                let sx = bounds.left + ((tx as f32 + 0.5) * scale) as i32;
                let sy = bounds.top + ((ty as f32 + 0.5) * scale) as i32;
                let px = raster.tiles.pixel(sx, sy).to_u8();
                let (r, g, b, a) = (px[0] as u32, px[1] as u32, px[2] as u32, px[3] as u32);
                let bg = if ((tx >> 2) + (ty >> 2)) & 1 == 0 {
                    0xE0u32
                } else {
                    0xB0u32
                };
                let inv = 255 - a;
                let d = (ty * w + tx) * 4;
                bgra[d] = ((b * a + bg * inv) / 255) as u8;
                bgra[d + 1] = ((g * a + bg * inv) / 255) as u8;
                bgra[d + 2] = ((r * a + bg * inv) / 255) as u8;
                bgra[d + 3] = 255;
            }
        }
        let buffer = image::RgbaImage::from_raw(w as u32, h as u32, bgra)?;
        let img = Arc::new(RenderImage::new(smallvec![image::Frame::new(buffer)]));
        let rev = doc.revision;
        self.thumbs.insert(id, (rev, img.clone()));
        // Drop cache entries for layers that no longer exist.
        if self.thumbs.len() > 64 {
            if let Some(doc) = self.doc.as_ref() {
                self.thumbs.retain(|lid, _| doc.tree.find(*lid).is_some());
            }
        }
        Some(img)
    }

    /// Set an absolute zoom level about the viewport centre.
    pub fn set_zoom(&mut self, zoom: f32) {
        let factor = zoom / self.zoom.max(1e-6);
        self.zoom_by(factor, None);
    }

    /// A small thumbnail of the whole document for the navigator, cached
    /// against the document revision.
    pub fn document_thumbnail(&mut self) -> Option<Arc<RenderImage>> {
        const MAX: u32 = 220;
        let doc = self.doc.as_ref()?;
        let revision = doc.revision;
        if let Some((rev, img)) = &self.nav_thumb {
            if *rev == revision {
                return Some(img.clone());
            }
        }
        let scale = (doc.width as f32 / MAX as f32).max(doc.height as f32 / 84.0);
        let (w, h) = (
            ((doc.width as f32 / scale) as u32).clamp(1, MAX),
            ((doc.height as f32 / scale) as u32).clamp(1, 84),
        );
        let rgba = photoslop_compositor::composite_region_rgba8(doc, doc.canvas_rect());
        let mut bgra = vec![0u8; (w * h * 4) as usize];
        for ty in 0..h {
            for tx in 0..w {
                let sx = ((tx as f32 + 0.5) * scale) as u32;
                let sy = ((ty as f32 + 0.5) * scale) as u32;
                let s = ((sy.min(doc.height - 1) * doc.width + sx.min(doc.width - 1)) * 4) as usize;
                let (r, g, b, a) = (
                    rgba[s] as u32,
                    rgba[s + 1] as u32,
                    rgba[s + 2] as u32,
                    rgba[s + 3] as u32,
                );
                let bg = if ((tx >> 2) + (ty >> 2)) & 1 == 0 {
                    0xE0
                } else {
                    0xB0
                };
                let inv = 255 - a;
                let d = ((ty * w + tx) * 4) as usize;
                bgra[d] = ((b * a + bg * inv) / 255) as u8;
                bgra[d + 1] = ((g * a + bg * inv) / 255) as u8;
                bgra[d + 2] = ((r * a + bg * inv) / 255) as u8;
                bgra[d + 3] = 255;
            }
        }
        let buffer = image::RgbaImage::from_raw(w, h, bgra)?;
        let img = Arc::new(RenderImage::new(smallvec![image::Frame::new(buffer)]));
        self.nav_thumb = Some((revision, img.clone()));
        Some(img)
    }

    /// Toggle a group's expanded state (pure UI state, not undoable).
    pub fn toggle_group_open(&mut self, id: photoslop_core::LayerId, cx: &mut Context<Self>) {
        if let Some(doc) = &mut self.doc {
            if let Some(layer) = doc.tree.find_mut(id) {
                if let photoslop_core::LayerKind::Group(g) = &mut layer.kind {
                    g.open = !g.open;
                }
            }
        }
        cx.notify();
    }

    // ----- view options, guides and snapping -----

    /// Persist view options so they survive a restart.
    pub fn save_view_options(&self) {
        let Some(path) = prefs_path() else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(json) = serde_json::to_string_pretty(&self.view) {
            let _ = std::fs::write(path, json);
        }
    }

    pub fn toggle_rulers(&mut self, cx: &mut Context<Self>) {
        self.view.rulers = !self.view.rulers;
        self.status = format!("Rulers {}", if self.view.rulers { "on" } else { "off" }).into();
        self.save_view_options();
        cx.notify();
    }

    pub fn toggle_grid(&mut self, cx: &mut Context<Self>) {
        self.view.grid = !self.view.grid;
        self.status = format!("Grid {}", if self.view.grid { "on" } else { "off" }).into();
        self.save_view_options();
        cx.notify();
    }

    pub fn toggle_guides(&mut self, cx: &mut Context<Self>) {
        self.view.guides = !self.view.guides;
        self.status = format!("Guides {}", if self.view.guides { "on" } else { "off" }).into();
        self.save_view_options();
        cx.notify();
    }

    pub fn toggle_extras(&mut self, cx: &mut Context<Self>) {
        self.view.extras = !self.view.extras;
        self.status = format!("Extras {}", if self.view.extras { "on" } else { "off" }).into();
        self.save_view_options();
        cx.notify();
    }

    pub fn toggle_snap(&mut self, cx: &mut Context<Self>) {
        self.view.snap = !self.view.snap;
        self.status = if self.view.snap {
            "Snap on"
        } else {
            "Snap off"
        }
        .into();
        self.save_view_options();
        cx.notify();
    }

    /// Change the chrome theme and persist it. Callers repaint themselves
    /// (dialog dropdowns already notify).
    pub fn set_theme_quiet(&mut self, theme: Theme) {
        self.view.theme = theme;
        self.save_view_options();
    }

    pub fn cycle_screen_mode(&mut self, cx: &mut Context<Self>) {
        self.screen_mode = match self.screen_mode {
            ScreenMode::Standard => ScreenMode::FullCanvas,
            ScreenMode::FullCanvas => ScreenMode::Standard,
        };
        cx.notify();
    }

    /// Thickness of the rulers in screen pixels.
    pub const RULER_SIZE: f32 = 18.0;

    /// Document x for a window x (used by the rulers).
    pub fn doc_x_at(&self, window_x: f32) -> f32 {
        (window_x - f32::from(self.canvas_bounds.origin.x) - f32::from(self.offset.x)) / self.zoom
    }

    pub fn doc_y_at(&self, window_y: f32) -> f32 {
        (window_y - f32::from(self.canvas_bounds.origin.y) - f32::from(self.offset.y)) / self.zoom
    }

    /// Screen x for a document x, inside the canvas element.
    pub fn screen_x(&self, doc_x: f32) -> f32 {
        f32::from(self.canvas_bounds.origin.x) + f32::from(self.offset.x) + doc_x * self.zoom
    }

    pub fn screen_y(&self, doc_y: f32) -> f32 {
        f32::from(self.canvas_bounds.origin.y) + f32::from(self.offset.y) + doc_y * self.zoom
    }

    pub fn canvas_bounds(&self) -> Bounds<Pixels> {
        self.canvas_bounds
    }

    /// Start dragging a new guide out of a ruler.
    pub fn begin_guide(&mut self, horizontal: bool, position: f32) {
        self.dragging_guide = Some(photoslop_core::Guide {
            horizontal,
            position,
        });
    }

    pub fn update_guide(&mut self, position: f32) {
        if let Some(guide) = &mut self.dragging_guide {
            guide.position = position;
        }
    }

    /// Drop the dragged guide onto the document (or discard it if it landed
    /// outside the canvas).
    pub fn finish_guide(&mut self, cx: &mut Context<Self>) {
        let Some(guide) = self.dragging_guide.take() else {
            return;
        };
        if let Some(doc) = self.doc.as_mut() {
            let limit = if guide.horizontal {
                doc.height as f32
            } else {
                doc.width as f32
            };
            if guide.position >= 0.0 && guide.position <= limit {
                doc.guides.push(guide);
                doc.damage_all();
            }
        }
        self.after_change(cx);
    }

    pub fn dragging_guide(&self) -> bool {
        self.dragging_guide.is_some()
    }

    /// Remove every guide.
    pub fn clear_guides(&mut self, cx: &mut Context<Self>) {
        if let Some(doc) = self.doc.as_mut() {
            doc.guides.clear();
            doc.damage_all();
        }
        self.status = "Guides cleared".into();
        self.after_change(cx);
    }

    /// Snap a document-space coordinate to nearby guides and grid lines.
    fn snap_point(&self, x: f32, y: f32) -> (f32, f32) {
        if !self.view.snap || !self.view.extras {
            return (x, y);
        }
        // Snap within 6 screen pixels, so the pull feels the same at any
        // zoom rather than growing with it.
        let threshold = 6.0 / self.zoom.max(0.01);
        let mut best = (x, y);
        let mut best_dist = (threshold, threshold);
        if let Some(doc) = &self.doc {
            if self.view.guides {
                for guide in &doc.guides {
                    if guide.horizontal {
                        let d = (guide.position - y).abs();
                        if d < best_dist.1 {
                            best_dist.1 = d;
                            best.1 = guide.position;
                        }
                    } else {
                        let d = (guide.position - x).abs();
                        if d < best_dist.0 {
                            best_dist.0 = d;
                            best.0 = guide.position;
                        }
                    }
                }
            }
            // Canvas edges always attract.
            for edge in [0.0, doc.width as f32] {
                let d = (edge - x).abs();
                if d < best_dist.0 {
                    best_dist.0 = d;
                    best.0 = edge;
                }
            }
            for edge in [0.0, doc.height as f32] {
                let d = (edge - y).abs();
                if d < best_dist.1 {
                    best_dist.1 = d;
                    best.1 = edge;
                }
            }
        }
        if self.view.grid && self.view.grid_spacing > 0.5 {
            let g = self.view.grid_spacing;
            let gx = (x / g).round() * g;
            let gy = (y / g).round() * g;
            if (gx - x).abs() < best_dist.0 {
                best.0 = gx;
            }
            if (gy - y).abs() < best_dist.1 {
                best.1 = gy;
            }
        }
        best
    }

    // ----- colour management -----

    /// Rebuild the display (and proofing) transforms after the document or
    /// the colour settings change, and drop cached pixels drawn with the
    /// old ones.
    pub fn rebuild_color_transforms(&mut self) {
        let icc = self.doc.as_ref().and_then(|d| d.icc_profile.clone());
        let transform = self.color.transform_for(icc.as_deref());
        self.display_transform = (!transform.is_identity()).then(|| Arc::new(transform));
        self.proof_transform = self.color.proof_transform(icc.as_deref()).map(Arc::new);
        self.cache.invalidate_all();
        self.display_tiles.clear();
        self.viewport_image = None;
        self.preview = Preview::default();
        self.thumbs.clear();
        self.color_epoch += 1;
    }

    /// Run composited pixels through the proof and display transforms.
    fn to_display(&self, pixels: &mut [f32]) {
        if let Some(proof) = &self.proof_transform {
            proof.apply(pixels);
        }
        if let Some(display) = &self.display_transform {
            display.apply(pixels);
        }
    }

    fn color_managed(&self) -> bool {
        self.display_transform.is_some() || self.proof_transform.is_some()
    }

    /// Assign a profile: same numbers, new interpretation.
    pub fn assign_profile(
        &mut self,
        profile: photoslop_colormgmt::Profile,
        cx: &mut Context<Self>,
    ) {
        if let Some(doc) = self.doc.as_mut() {
            doc.icc_profile = profile.icc_bytes().map(|b| b.to_vec());
            doc.dirty = true;
            doc.damage_all();
        }
        self.status = format!("Assigned {}", profile.name()).into();
        self.rebuild_color_transforms();
        self.after_change(cx);
    }

    /// Convert to a profile: rewrite pixels so the appearance is preserved.
    pub fn convert_to_profile(
        &mut self,
        profile: photoslop_colormgmt::Profile,
        cx: &mut Context<Self>,
    ) {
        let intent = self.color.intent;
        let Some(doc) = self.doc.as_mut() else { return };
        let source = match &doc.icc_profile {
            Some(bytes) => photoslop_colormgmt::Profile::from_bytes(bytes)
                .unwrap_or_else(|_| self.color.working.clone()),
            None => self.color.working.clone(),
        };
        let transform = match photoslop_colormgmt::ColorTransform::new(&source, &profile, intent) {
            Ok(t) => t,
            Err(err) => {
                self.status = format!("Convert failed: {err}").into();
                return;
            }
        };

        let mut edit = doc.begin_edit(format!("Convert to {}", profile.name()));
        for id in edit.raster_layer_ids() {
            let Some(raster) = edit.doc().tree.find(id).and_then(|l| l.as_raster()) else {
                continue;
            };
            let coords: Vec<photoslop_core::TileCoord> = raster.tiles.coords().collect();
            for coord in coords {
                let Some(tile) = edit.writable_tile(id, coord) else {
                    break;
                };
                let mut buf = vec![0.0f32; photoslop_core::TILE_PIXELS * 4];
                tile.decode_f32(&mut buf);
                transform.apply(&mut buf);
                tile.encode_f32(&buf);
            }
        }
        edit.commit();
        doc.icc_profile = profile.icc_bytes().map(|b| b.to_vec());
        self.status = format!("Converted to {}", profile.name()).into();
        self.rebuild_color_transforms();
        self.after_change(cx);
    }

    /// Toggle soft proofing against a device profile.
    pub fn toggle_proof(&mut self, profile: photoslop_colormgmt::Profile, cx: &mut Context<Self>) {
        self.color.proof = match &self.color.proof {
            Some(_) => None,
            None => Some(profile),
        };
        self.status = if self.color.proof.is_some() {
            "Proof colors on".into()
        } else {
            "Proof colors off".into()
        };
        self.rebuild_color_transforms();
        self.after_change(cx);
    }

    // ----- filters and adjustments -----

    /// Run a registered filter over the active layer, confined to the
    /// selection, as one undoable edit.
    /// The pixels a filter would touch: the layer's content clipped to the
    /// canvas, or to the selection when there is one.
    fn filter_region(&self, layer_id: photoslop_core::LayerId) -> IntRect {
        let Some(doc) = self.doc.as_ref() else {
            return IntRect::EMPTY;
        };
        let canvas = doc.canvas_rect();
        if doc.selection.is_empty() {
            doc.tree
                .find(layer_id)
                .map(|l| l.content_bounds())
                .unwrap_or(IntRect::EMPTY)
                .intersect(&canvas)
        } else {
            doc.selection.bounds().intersect(&canvas)
        }
    }

    /// Pull `region` out of a raster layer into a flat straight-alpha
    /// f32 RGBA buffer, the shape every filter works on.
    fn read_region(&self, layer_id: photoslop_core::LayerId, region: IntRect) -> Option<Vec<f32>> {
        let raster = self
            .doc
            .as_ref()?
            .tree
            .find(layer_id)
            .and_then(|l| l.as_raster())?;
        let (w, h) = (region.width() as usize, region.height() as usize);
        let mut buf = vec![0.0f32; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let px = raster
                    .tiles
                    .pixel(region.left + x as i32, region.top + y as i32);
                let at = (y * w + x) * 4;
                buf[at] = px.r;
                buf[at + 1] = px.g;
                buf[at + 2] = px.b;
                buf[at + 3] = px.a;
            }
        }
        Some(buf)
    }

    /// Blend `filtered` back over `original` through the selection, so
    /// partial coverage feathers the result.
    ///
    /// With `record` the write becomes one history entry; without it the
    /// pixels change but the history does not, which is what a live
    /// preview needs.
    fn write_region(
        &mut self,
        layer_id: photoslop_core::LayerId,
        region: IntRect,
        original: &[f32],
        filtered: &[f32],
        label: &str,
        record: bool,
    ) {
        let Some(doc) = self.doc.as_mut() else { return };
        let selection = doc.selection.clone();
        let depth = doc.depth;
        let coords: Vec<TileCoord> = TileCoord::covering(&region).collect();

        if record {
            let mut edit = doc.begin_edit(label.to_string());
            for coord in coords {
                let clip = coord.rect().intersect(&region);
                if clip.is_empty() {
                    continue;
                }
                let Some(tile) = edit.writable_tile(layer_id, coord) else {
                    break;
                };
                blend_region_tile(tile, coord, clip, region, original, filtered, &selection);
            }
            edit.commit();
        } else {
            let Some(raster) = doc.tree.find_mut(layer_id).and_then(|l| l.as_raster_mut()) else {
                return;
            };
            for coord in coords {
                let clip = coord.rect().intersect(&region);
                if clip.is_empty() {
                    continue;
                }
                let tile = raster.tiles.get_mut_or_insert(coord, depth);
                blend_region_tile(tile, coord, clip, region, original, filtered, &selection);
            }
            doc.add_damage(region);
        }
    }

    /// Snapshot what a filter dialog is about to change, so the preview can
    /// be re-run from the original on every slider tick and undone on
    /// cancel. Returns false when there is nothing to filter.
    pub fn begin_filter_preview(&mut self) -> bool {
        self.filter_preview = None;
        let Some(layer_id) = self.doc.as_ref().and_then(|d| d.active_layer) else {
            self.status = "Select a layer first".into();
            return false;
        };
        if self
            .doc
            .as_ref()
            .and_then(|d| d.tree.find(layer_id))
            .and_then(|l| l.as_raster())
            .is_none()
        {
            self.status = "Filters need a pixel layer".into();
            return false;
        }
        let region = self.filter_region(layer_id);
        if region.is_empty() {
            self.status = "Nothing to filter".into();
            return false;
        }
        let Some(original) = self.read_region(layer_id, region) else {
            return false;
        };
        self.filter_preview = Some(FilterPreview {
            layer: layer_id,
            region,
            original,
        });
        true
    }

    /// Re-run the filter on the canvas from the snapshot, without touching
    /// history. `None` values restore the untouched pixels.
    pub fn preview_filter(
        &mut self,
        id: &str,
        values: Option<&photoslop_plugin_api::FilterValues>,
        cx: &mut Context<Self>,
    ) {
        let Some(preview) = self.filter_preview.clone() else {
            return;
        };
        let mut buf = preview.original.clone();
        if let Some(values) = values {
            let (w, h) = (
                preview.region.width() as usize,
                preview.region.height() as usize,
            );
            let Some(filter) = self.registry.filters().find(|f| f.id() == id) else {
                return;
            };
            filter.apply(&mut buf, w, h, values);
        }
        self.write_region(
            preview.layer,
            preview.region,
            &preview.original,
            &buf,
            "",
            false,
        );
        self.after_change(cx);
    }

    /// Drop a preview, restoring the pixels it was drawn over.
    pub fn cancel_filter_preview(&mut self, cx: &mut Context<Self>) {
        if self.filter_preview.is_none() {
            return;
        }
        self.preview_filter("", None, cx);
        self.filter_preview = None;
    }

    pub fn apply_filter(
        &mut self,
        id: &str,
        values: &photoslop_plugin_api::FilterValues,
        cx: &mut Context<Self>,
    ) {
        // A live preview has already changed these pixels; put them back so
        // the recorded edit has the right "before".
        if self.filter_preview.is_some() {
            self.preview_filter("", None, cx);
        }
        let preview = self.filter_preview.take();
        let Some(layer_id) = self.doc.as_ref().and_then(|d| d.active_layer) else {
            self.status = "Select a layer first".into();
            return;
        };
        let Some(filter) = self.registry.filters().find(|f| f.id() == id) else {
            log::warn!("unknown filter {id}");
            return;
        };
        let name = filter.name().to_string();
        if self
            .doc
            .as_ref()
            .and_then(|d| d.tree.find(layer_id))
            .and_then(|l| l.as_raster())
            .is_none()
        {
            self.status = "Filters need a pixel layer".into();
            return;
        }
        // Reuse the preview's region so what was previewed is what lands.
        let region = preview
            .filter(|p| p.layer == layer_id)
            .map(|p| p.region)
            .unwrap_or_else(|| self.filter_region(layer_id));
        if region.is_empty() {
            self.status = "Nothing to filter".into();
            return;
        }
        let Some(original) = self.read_region(layer_id, region) else {
            return;
        };
        let mut buf = original.clone();
        let filter = self.registry.filters().find(|f| f.id() == id).unwrap();
        filter.apply(
            &mut buf,
            region.width() as usize,
            region.height() as usize,
            values,
        );
        self.write_region(layer_id, region, &original, &buf, &name, true);
        self.status = name.into();
        self.after_change(cx);
    }

    /// Export the flattened document with explicit encoder settings.
    pub fn export_with(
        &mut self,
        codec_id: &str,
        options: photoslop_plugin_api::ExportOptions,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(codec) = self.registry.codecs().find(|c| c.id() == codec_id) else {
            return;
        };
        let ext = codec.extensions().first().copied().unwrap_or("png");
        let doc = self.doc.as_ref();
        let stem = doc
            .map(|d| {
                std::path::Path::new(&d.title)
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "untitled".into())
            })
            .unwrap_or_else(|| "untitled".into());
        let dir = doc
            .and_then(|d| d.path.as_ref())
            .and_then(|p| p.parent().map(|p| p.to_path_buf()))
            .or_else(|| std::env::var("HOME").ok().map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("."));
        let suggested = format!("{stem}.{ext}");
        let codec_id = codec_id.to_string();
        let rx = cx.prompt_for_new_path(&dir, Some(&suggested));
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(path))) = rx.await {
                this.update_in(cx, |ws, _window, cx| {
                    let result = (|| -> anyhow::Result<()> {
                        let doc = ws
                            .doc
                            .as_ref()
                            .ok_or_else(|| anyhow::anyhow!("no document"))?;
                        let codec = ws
                            .registry
                            .codecs()
                            .find(|c| c.id() == codec_id)
                            .ok_or_else(|| anyhow::anyhow!("codec vanished"))?;
                        let bytes = codec.export_with(doc, &options)?;
                        std::fs::write(&path, bytes)?;
                        Ok(())
                    })();
                    ws.status = match result {
                        Ok(()) => format!("Exported {}", path.display()).into(),
                        Err(err) => format!("Export failed: {err}").into(),
                    };
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    /// Ask upstream whether a newer release exists. Opt-in, user-initiated,
    /// and the only network request the app makes.
    pub fn check_for_update(&mut self, cx: &mut Context<Self>) {
        self.status = "Checking for updates…".into();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let status = cx
                .background_executor()
                .spawn(async { crate::crash::check_for_update() })
                .await;
            this.update(cx, |ws, cx| {
                ws.status = match status {
                    crate::crash::UpdateStatus::UpToDate => format!(
                        "Photoslop {} is up to date",
                        crate::crash::current_version()
                    )
                    .into(),
                    crate::crash::UpdateStatus::Available { version, url } => {
                        log::info!("update {version} available at {url}");
                        format!(
                            "Version {version} is available — see {}",
                            crate::crash::RELEASES_PAGE
                        )
                        .into()
                    }
                    crate::crash::UpdateStatus::Failed(err) => {
                        format!("Update check failed: {err}").into()
                    }
                };
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Enable or disable a third-party plugin.
    pub fn set_plugin_enabled(&mut self, id: String, enabled: bool, cx: &mut Context<Self>) {
        let Some(dir) = photoslop_plugin_host_wasm::PluginManager::plugin_dir() else {
            return;
        };
        self.plugins.set_enabled(&id, enabled, &dir);
        self.status = format!(
            "{} {} — restart to apply",
            id,
            if enabled { "enabled" } else { "disabled" }
        )
        .into();
        cx.notify();
    }

    /// Install a plugin file into the plugin directory.
    pub fn install_plugin(&mut self, source: PathBuf, cx: &mut Context<Self>) {
        let Some(dir) = photoslop_plugin_host_wasm::PluginManager::plugin_dir() else {
            return;
        };
        self.status = match photoslop_plugin_host_wasm::PluginManager::install(&source, &dir) {
            Ok(path) => format!("Installed {} — restart to load", path.display()).into(),
            Err(err) => format!("Plugin rejected: {err}").into(),
        };
        cx.notify();
    }

    /// Open a filter's parameter dialog, pre-filled with its defaults.
    pub fn open_filter_dialog(&mut self, id: &'static str, cx: &mut Context<Self>) {
        let Some(filter) = self.registry.filters().find(|f| f.id() == id) else {
            return;
        };
        let values = photoslop_plugin_api::FilterValues::defaults(&filter.params());
        // Filters with no parameters just run.
        if values.0.is_empty() {
            self.apply_filter(id, &values, cx);
            return;
        }
        if !self.begin_filter_preview() {
            cx.notify();
            return;
        }
        self.preview_filter(id, Some(&values), cx);
        self.open_modal(
            Modal::Filter {
                id,
                values,
                preview: true,
            },
            cx,
        );
    }

    /// Insert an adjustment layer above the active layer.
    pub fn add_adjustment(&mut self, kind: photoslop_core::AdjustmentKind, cx: &mut Context<Self>) {
        let params = photoslop_adjustments::Params::default_for(kind);
        let Some(doc) = self.doc.as_mut() else { return };
        let mut layer = Layer::new_raster(kind.display_name());
        layer.kind = photoslop_core::LayerKind::Adjustment(photoslop_core::AdjustmentData {
            kind,
            raw: Vec::new(),
            params_json: serde_json::to_string(&params).ok(),
        });
        let id = layer.id;
        let path = match doc.active_layer.and_then(|a| doc.tree.path_of(a)) {
            Some(mut p) => {
                *p.0.last_mut().unwrap() += 1;
                p
            }
            None => photoslop_core::LayerPath(vec![doc.tree.layers.len()]),
        };
        let mut edit = doc.begin_edit(format!("New {} Layer", kind.display_name()));
        edit.insert_layer(path, layer);
        edit.commit();
        doc.active_layer = Some(id);
        self.status = format!("Added {}", kind.display_name()).into();
        self.after_change(cx);
        // Anything with controls opens its dialog straight away.
        if !params.param_specs().is_empty() {
            let original = (serde_json::to_string(&params).ok(), Vec::new());
            self.open_modal(
                Modal::Adjustment {
                    layer: id,
                    params,
                    original,
                },
                cx,
            );
        }
    }

    /// Write edited parameters back onto an adjustment layer (live, without
    /// a history entry per slider tick).
    pub fn preview_adjustment(
        &mut self,
        layer: photoslop_core::LayerId,
        params: &photoslop_adjustments::Params,
    ) {
        let Some(doc) = self.doc.as_mut() else { return };
        if let Some(photoslop_core::LayerKind::Adjustment(data)) =
            doc.tree.find_mut(layer).map(|l| &mut l.kind)
        {
            data.params_json = serde_json::to_string(params).ok();
        }
        doc.damage_all();
    }

    /// Commit edited adjustment parameters as one history entry.
    pub fn commit_adjustment(
        &mut self,
        layer: photoslop_core::LayerId,
        params: &photoslop_adjustments::Params,
        original: (Option<String>, Vec<u8>),
        cx: &mut Context<Self>,
    ) {
        let after = (serde_json::to_string(params).ok(), Vec::new());
        if let Some(doc) = self.doc.as_mut() {
            // Put the pre-dialog state back first so the recorded edit has
            // the right "before"; the live preview already moved the layer.
            if let Some(photoslop_core::LayerKind::Adjustment(data)) =
                doc.tree.find_mut(layer).map(|l| &mut l.kind)
            {
                data.params_json = original.0.clone();
                data.raw = original.1.clone();
            }
            let mut edit = doc.begin_edit(format!("{} Settings", params.display_name()));
            // Editing parameters supersedes the preserved PSD payload, so
            // the writer emits our values rather than stale bytes.
            edit.record_adjustment_params(layer, original, after);
            edit.commit();
        }
        self.status = format!("{} updated", params.display_name()).into();
        self.after_change(cx);
    }

    /// Discard a live adjustment preview (dialog cancelled).
    pub fn revert_adjustment(
        &mut self,
        layer: photoslop_core::LayerId,
        original: (Option<String>, Vec<u8>),
        cx: &mut Context<Self>,
    ) {
        if let Some(doc) = self.doc.as_mut() {
            if let Some(photoslop_core::LayerKind::Adjustment(data)) =
                doc.tree.find_mut(layer).map(|l| &mut l.kind)
            {
                data.params_json = original.0;
                data.raw = original.1;
            }
            doc.damage_all();
        }
        self.after_change(cx);
    }

    /// Open the parameter dialog for an existing adjustment layer.
    pub fn edit_adjustment(&mut self, layer: photoslop_core::LayerId, cx: &mut Context<Self>) {
        let Some(doc) = self.doc.as_ref() else { return };
        let Some(photoslop_core::LayerKind::Adjustment(data)) =
            doc.tree.find(layer).map(|l| &l.kind)
        else {
            return;
        };
        let params = data
            .params_json
            .as_deref()
            .and_then(|j| serde_json::from_str(j).ok())
            .unwrap_or_else(|| photoslop_adjustments::parse_psd(data.kind, &data.raw));
        if params.param_specs().is_empty() {
            self.status = format!("{} has no editable settings", params.display_name()).into();
            return;
        }
        let original = (data.params_json.clone(), data.raw.clone());
        self.open_modal(
            Modal::Adjustment {
                layer,
                params,
                original,
            },
            cx,
        );
    }

    // ----- painting -----

    /// The selection's traced boundary, recomputed only when the selection
    /// itself changes.
    fn selection_outline(&mut self, generation: u64) -> SelectionOutline {
        if let Some((gen, outline)) = &self.selection_outline {
            if *gen == generation {
                return outline.clone();
            }
        }
        let outline = Arc::new(
            self.doc
                .as_ref()
                .map(|d| d.selection.outline())
                .unwrap_or_default(),
        );
        self.selection_outline = Some((generation, outline.clone()));
        outline
    }

    /// A composited tile after colour management, cached per tile.
    ///
    /// Colour conversion is the expensive part, so it stays tile-cached;
    /// only the cheap assembly below runs per frame.
    fn display_tile(&mut self, coord: TileCoord) -> Option<Arc<Vec<u8>>> {
        if let Some(tile) = self.display_tiles.get(&coord) {
            return Some(tile.clone());
        }
        let doc = self.doc.as_ref()?;
        let rgba = self.cache.get(doc, coord);
        let managed = if self.color_managed() {
            let mut buf: Vec<f32> = rgba.iter().map(|&v| v as f32 / 255.0).collect();
            self.to_display(&mut buf);
            Arc::new(
                buf.iter()
                    .map(|v| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8)
                    .collect::<Vec<u8>>(),
            )
        } else {
            rgba
        };
        self.display_tiles.insert(coord, managed.clone());
        Some(managed)
    }

    /// Assemble everything visible into one BGRA image the size of the
    /// canvas element, resampling on the way.
    ///
    /// Zooming in uses nearest-neighbour so pixels stay crisp (what an
    /// image editor wants); zooming out uses bilinear to damp aliasing.
    /// Transparency is checkered at a fixed *screen* size, like Photoshop.
    fn assemble_viewport(
        &mut self,
        bounds: Bounds<Pixels>,
        scale_factor: f32,
    ) -> Option<Arc<RenderImage>> {
        let sf = scale_factor.max(0.01);
        let width = (f32::from(bounds.size.width) * sf).round().max(1.0) as usize;
        let height = (f32::from(bounds.size.height) * sf).round().max(1.0) as usize;
        // A sanity cap: a hostile window size shouldn't allocate gigabytes.
        if width * height > 64 << 20 {
            return None;
        }
        let doc = self.doc.as_ref()?;
        let revision = doc.revision;
        let canvas_rect = doc.canvas_rect();
        let key = ViewportKey {
            revision,
            zoom: self.zoom.to_bits(),
            offset: (
                (f32::from(self.offset.x) * sf).round() as i32,
                (f32::from(self.offset.y) * sf).round() as i32,
            ),
            size: (width as u32, height as u32),
            color_epoch: self.color_epoch,
            rotation: self.rotation.to_bits(),
        };
        if let Some((cached_key, image)) = &self.viewport_image {
            if *cached_key == key {
                return Some(image.clone());
            }
        }

        // Which document pixels can land on screen?
        let zoom = self.zoom;
        let inv_zoom = 1.0 / zoom;
        let origin = (f32::from(self.offset.x) * sf, f32::from(self.offset.y) * sf);
        // Rotation is about the middle of the viewport, which is what
        // makes spinning the view feel like turning a sheet of paper.
        let centre = (width as f32 / 2.0, height as f32 / 2.0);
        let (rs, rc) = (-self.rotation).sin_cos();
        let doc_at = |dx: f32, dy: f32| -> (f32, f32) {
            let (ox, oy) = (dx - centre.0, dy - centre.1);
            let (rx, ry) = (ox * rc - oy * rs + centre.0, ox * rs + oy * rc + centre.1);
            (
                (rx - origin.0) * inv_zoom / sf,
                (ry - origin.1) * inv_zoom / sf,
            )
        };
        // With rotation, the visible region is the union of all four
        // corners rather than the span between two of them.
        let mut span = IntRect::EMPTY;
        for (cx, cy) in [
            (0.0, 0.0),
            (width as f32, 0.0),
            (width as f32, height as f32),
            (0.0, height as f32),
        ] {
            let (x, y) = doc_at(cx, cy);
            span = span.union(&IntRect::new(
                x.floor() as i32 - 1,
                y.floor() as i32 - 1,
                x.ceil() as i32 + 1,
                y.ceil() as i32 + 1,
            ));
        }
        let visible = span.intersect(&canvas_rect);
        if visible.is_empty() {
            // Nothing but background: a 1x1 image keeps the paint path simple.
            let buffer = image::RgbaImage::from_raw(1, 1, vec![0x26, 0x26, 0x26, 255])?;
            let img = Arc::new(RenderImage::new(smallvec![image::Frame::new(buffer)]));
            self.viewport_image = Some((key, img.clone()));
            return Some(img);
        }

        // Composite the visible tiles, then index them by grid position so
        // sampling is an array lookup rather than a hash per pixel.
        let coords: Vec<TileCoord> = TileCoord::covering(&visible).collect();
        if let Some(doc) = self.doc.as_ref() {
            self.cache.prewarm(doc, &coords);
        }
        let (tx0, ty0) = (
            visible.left.div_euclid(TILE_SIZE),
            visible.top.div_euclid(TILE_SIZE),
        );
        let cols = ((visible.right - 1).div_euclid(TILE_SIZE) - tx0 + 1).max(1) as usize;
        let rows = ((visible.bottom - 1).div_euclid(TILE_SIZE) - ty0 + 1).max(1) as usize;
        let mut grid: Vec<Option<Arc<Vec<u8>>>> = vec![None; cols * rows];
        for coord in coords {
            let ix = (coord.ty - ty0) as usize * cols + (coord.tx - tx0) as usize;
            if let Some(slot) = grid.get_mut(ix) {
                *slot = self.display_tile(coord);
            }
        }

        let sample = |x: i32, y: i32| -> [u8; 4] {
            if !canvas_rect.contains(x, y) {
                return [0, 0, 0, 0];
            }
            let tx = x.div_euclid(TILE_SIZE) - tx0;
            let ty = y.div_euclid(TILE_SIZE) - ty0;
            if tx < 0 || ty < 0 || tx as usize >= cols || ty as usize >= rows {
                return [0, 0, 0, 0];
            }
            match &grid[ty as usize * cols + tx as usize] {
                Some(tile) => {
                    let lx = x.rem_euclid(TILE_SIZE) as usize;
                    let ly = y.rem_euclid(TILE_SIZE) as usize;
                    let at = (ly * TILE_SIZE as usize + lx) * 4;
                    [tile[at], tile[at + 1], tile[at + 2], tile[at + 3]]
                }
                None => [0, 0, 0, 0],
            }
        };

        let magnify = zoom >= 1.0;
        let mut bgra = vec![0u8; width * height * 4];
        bgra.par_chunks_mut(width * 4)
            .enumerate()
            .for_each(|(row, line)| {
                let dy = row as f32 + 0.5;
                for col in 0..width {
                    let dx = col as f32 + 0.5;
                    let (fx, fy) = doc_at(dx, dy);
                    let px = if magnify {
                        // Nearest: one document pixel becomes a crisp block.
                        sample(fx.floor() as i32, fy.floor() as i32)
                    } else {
                        // Bilinear over the four neighbours, in
                        // premultiplied space so edges don't fringe.
                        let (sx, sy) = (fx - 0.5, fy - 0.5);
                        let (ix, iy) = (sx.floor(), sy.floor());
                        let (tx, ty) = (sx - ix, sy - iy);
                        let mut acc = [0.0f32; 4];
                        for (dxi, wx) in [(0, 1.0 - tx), (1, tx)] {
                            for (dyi, wy) in [(0, 1.0 - ty), (1, ty)] {
                                let w = wx * wy;
                                if w <= 0.0 {
                                    continue;
                                }
                                let s = sample(ix as i32 + dxi, iy as i32 + dyi);
                                let a = s[3] as f32 / 255.0;
                                acc[0] += s[0] as f32 * a * w;
                                acc[1] += s[1] as f32 * a * w;
                                acc[2] += s[2] as f32 * a * w;
                                acc[3] += a * w;
                            }
                        }
                        if acc[3] <= 1e-6 {
                            [0, 0, 0, 0]
                        } else {
                            [
                                (acc[0] / acc[3]).round().clamp(0.0, 255.0) as u8,
                                (acc[1] / acc[3]).round().clamp(0.0, 255.0) as u8,
                                (acc[2] / acc[3]).round().clamp(0.0, 255.0) as u8,
                                (acc[3] * 255.0).round().clamp(0.0, 255.0) as u8,
                            ]
                        }
                    };

                    // Transparency shows the checkerboard inside the canvas
                    // and the app background outside it.
                    let inside = {
                        let (fx, fy) = (fx.floor() as i32, fy.floor() as i32);
                        canvas_rect.contains(fx, fy)
                    };
                    let bg = if inside {
                        if ((col >> 3) + (row >> 3)) & 1 == 0 {
                            0xFFu32
                        } else {
                            0xCCu32
                        }
                    } else {
                        0x26u32
                    };
                    let (r, g, b, a) = (px[0] as u32, px[1] as u32, px[2] as u32, px[3] as u32);
                    let inv = 255 - a;
                    let at = col * 4;
                    line[at] = ((b * a + bg * inv) / 255) as u8;
                    line[at + 1] = ((g * a + bg * inv) / 255) as u8;
                    line[at + 2] = ((r * a + bg * inv) / 255) as u8;
                    line[at + 3] = 255;
                }
            });

        let buffer = image::RgbaImage::from_raw(width as u32, height as u32, bgra)?;
        let img = Arc::new(RenderImage::new(smallvec![image::Frame::new(buffer)]));
        // Release the previous frame's atlas slot.
        if let Some((_, old)) = self.viewport_image.replace((key, img.clone())) {
            self.retired_images.push(old);
        }
        Some(img)
    }

    fn refresh_preview(&mut self) -> Option<Arc<RenderImage>> {
        let doc = self.doc.as_ref()?;
        let (w, h) = (
            (doc.width >> PREVIEW_SHIFT).max(1),
            (doc.height >> PREVIEW_SHIFT).max(1),
        );
        let full = IntRect::from_size(doc.width, doc.height);
        if !self.preview.valid || self.preview.w != w || self.preview.h != h {
            self.preview.buf = vec![0u8; (w * h * 4) as usize];
            self.preview.w = w;
            self.preview.h = h;
            self.preview.dirty = vec![full];
            self.preview.valid = true;
        }
        let dirty = std::mem::take(&mut self.preview.dirty);
        if dirty.is_empty() {
            if let Some(img) = &self.preview.image {
                return Some(img.clone());
            }
        }
        let step = 1i32 << PREVIEW_SHIFT;
        for rect in dirty {
            let rect = rect.intersect(&full);
            if rect.is_empty() {
                continue;
            }
            let rgba = photoslop_compositor::composite_region_rgba8(doc, rect);
            let rw = rect.width() as usize;
            // Point-sample the full-res composite into the preview buffer.
            let px0 = rect.left.div_euclid(step).max(0);
            let py0 = rect.top.div_euclid(step).max(0);
            let px1 = ((rect.right - 1).div_euclid(step) + 1).min(w as i32);
            let py1 = ((rect.bottom - 1).div_euclid(step) + 1).min(h as i32);
            for py in py0..py1 {
                let sy = (py * step + step / 2).clamp(rect.top, rect.bottom - 1);
                for pxx in px0..px1 {
                    let sx = (pxx * step + step / 2).clamp(rect.left, rect.right - 1);
                    let s = (((sy - rect.top) as usize * rw) + (sx - rect.left) as usize) * 4;
                    let (r, g, b, a) = (
                        rgba[s] as u32,
                        rgba[s + 1] as u32,
                        rgba[s + 2] as u32,
                        rgba[s + 3] as u32,
                    );
                    let bg = if ((sx >> 3) + (sy >> 3)) & 1 == 0 {
                        0xFFu32
                    } else {
                        0xCCu32
                    };
                    let inv = 255 - a;
                    let d = ((py as u32 * w + pxx as u32) * 4) as usize;
                    self.preview.buf[d] = ((b * a + bg * inv) / 255) as u8;
                    self.preview.buf[d + 1] = ((g * a + bg * inv) / 255) as u8;
                    self.preview.buf[d + 2] = ((r * a + bg * inv) / 255) as u8;
                    self.preview.buf[d + 3] = 255;
                }
            }
        }
        let buffer = image::RgbaImage::from_raw(w, h, self.preview.buf.clone())?;
        let img = Arc::new(RenderImage::new(smallvec![image::Frame::new(buffer)]));
        self.preview.image = Some(img.clone());
        Some(img)
    }

    /// Everything the paint closure needs, computed with &mut self.
    fn prepare_paint(&mut self, bounds: Bounds<Pixels>, scale_factor: f32) -> PaintJob {
        self.canvas_bounds = bounds;
        let mut job = PaintJob::default();
        let Some(doc) = self.doc.as_ref() else {
            return job;
        };
        let canvas_rect = doc.canvas_rect();
        let zoom = self.zoom;
        // Immutable-phase data, captured by value so the mutable phase
        // below (preview/tile-image builds) doesn't fight the borrows.
        let selection_generation = doc.selection.generation();
        let ant_phase = self.ant_phase;
        let has_selection = !doc.selection.is_empty() && !doc.selection.bounds().is_empty();
        let mut guides = doc.guides.clone();
        if let Some(dragging) = self.dragging_guide {
            guides.push(dragging);
        }
        let tool_id = self.editor.active_tool;
        let mut overlays = self
            .registry
            .tool_mut(tool_id)
            .map(|t| t.overlays(doc, &self.editor))
            .unwrap_or_default();
        self.tool_has_overlay = !overlays.is_empty();
        // The active stored path is always visible, whichever tool is in
        // use -- otherwise a path drawn with the pen would vanish the
        // moment you switched to something else.
        if !PATH_TOOLS.contains(&tool_id) {
            if let Some(path) = doc.active_path.and_then(|i| doc.paths.get(i)) {
                for sub in &photoslop_tools_vector::paths::flatten(path).subpaths {
                    if sub.len() >= 2 {
                        overlays.push(photoslop_plugin_api::Overlay::AntsPolygon(sub.clone()));
                    }
                }
            }
        }
        let origin = (
            f32::from(bounds.origin.x) + f32::from(self.offset.x),
            f32::from(bounds.origin.y) + f32::from(self.offset.y),
        );
        // Overlays are drawn in window space, so they rotate about the
        // canvas element's centre exactly as the pixels do.
        let rot = self.rotation;
        let rot_centre = (
            f32::from(bounds.origin.x) + f32::from(bounds.size.width) / 2.0,
            f32::from(bounds.origin.y) + f32::from(bounds.size.height) / 2.0,
        );
        let to_screen = move |x: f32, y: f32| -> Point<Pixels> {
            let (sx, sy) = (origin.0 + x * zoom, origin.1 + y * zoom);
            if rot == 0.0 {
                return point(px(sx), px(sy));
            }
            let (s, c) = rot.sin_cos();
            let (ox, oy) = (sx - rot_centre.0, sy - rot_centre.1);
            point(
                px(ox * c - oy * s + rot_centre.0),
                px(ox * s + oy * c + rot_centre.1),
            )
        };
        // Snap a document-space coordinate to the device-pixel grid.
        // Abutting quads MUST share bit-identical edges or fractional
        // zoom/pan leaves sub-pixel gaps between tiles that show through
        // as hairline seams; computing every tile's edges through this one
        // function (same input -> same output) guarantees they meet.
        let sf = scale_factor.max(0.01);
        let snap_x = move |x: f32| ((origin.0 + x * zoom) * sf).round() / sf;
        let snap_y = move |y: f32| ((origin.1 + y * zoom) * sf).round() / sf;
        let snapped_bounds = move |rect: IntRect| -> Bounds<Pixels> {
            let x0 = snap_x(rect.left as f32);
            let x1 = snap_x(rect.right as f32);
            let y0 = snap_y(rect.top as f32);
            let y1 = snap_y(rect.bottom as f32);
            Bounds {
                origin: point(px(x0), px(y0)),
                size: size(px(x1 - x0), px(y1 - y0)),
            }
        };

        if zoom <= PREVIEW_ZOOM_CUTOFF {
            // Far out, compositing every tile would be wasteful; the
            // downscaled preview is already one seamless image.
            if let Some(img) = self.refresh_preview() {
                job.tiles.push((snapped_bounds(canvas_rect), img));
            }
        } else if let Some(img) = self.assemble_viewport(bounds, scale_factor) {
            // One image covering the whole canvas element, already
            // resampled and checkered, so there are no tile edges to seam.
            job.tiles.push((bounds, img));
        }

        job.retired = std::mem::take(&mut self.retired_images);

        // Document border. An axis-aligned rectangle cannot follow a
        // rotated view, so when the view is turned it is drawn as a path
        // through `to_screen` instead.
        if self.rotation == 0.0 {
            job.outlines
                .push((snapped_bounds(canvas_rect), gpui::rgb(0x000000).into()));
        } else {
            let corners = [
                (canvas_rect.left as f32, canvas_rect.top as f32),
                (canvas_rect.right as f32, canvas_rect.top as f32),
                (canvas_rect.right as f32, canvas_rect.bottom as f32),
                (canvas_rect.left as f32, canvas_rect.bottom as f32),
                (canvas_rect.left as f32, canvas_rect.top as f32),
            ];
            job.polylines.push((
                corners.iter().map(|(x, y)| to_screen(*x, *y)).collect(),
                gpui::rgb(0x000000).into(),
            ));
        }

        // Grid, then guides: both are view chrome, hidden by ⌘H. They are
        // drawn as thin axis-aligned quads, so a rotated view hides them
        // rather than showing them pointing the wrong way.
        if self.view.extras && self.rotation == 0.0 {
            let view = self.view;
            let canvas_w = canvas_rect.width() as f32;
            let canvas_h = canvas_rect.height() as f32;
            let hair = (1.0 / scale_factor.max(0.01)).max(0.5);
            if view.grid && view.grid_spacing > 0.5 {
                let spacing_px = view.grid_spacing * zoom;
                // Skip the grid when it would alias into a solid block.
                if spacing_px >= 4.0 {
                    let mut x = 0.0;
                    while x <= canvas_w {
                        let sx = snap_x(x);
                        job.lines.push((
                            Bounds {
                                origin: point(px(sx), px(snap_y(0.0))),
                                size: size(px(hair), px(snap_y(canvas_h) - snap_y(0.0))),
                            },
                            gpui::rgba(0x8899AA55).into(),
                        ));
                        x += view.grid_spacing;
                    }
                    let mut y = 0.0;
                    while y <= canvas_h {
                        let sy = snap_y(y);
                        job.lines.push((
                            Bounds {
                                origin: point(px(snap_x(0.0)), px(sy)),
                                size: size(px(snap_x(canvas_w) - snap_x(0.0)), px(hair)),
                            },
                            gpui::rgba(0x8899AA55).into(),
                        ));
                        y += view.grid_spacing;
                    }
                }
            }
            if view.guides {
                for guide in guides.iter() {
                    if guide.horizontal {
                        job.lines.push((
                            Bounds {
                                origin: point(px(snap_x(0.0)), px(snap_y(guide.position))),
                                size: size(px(snap_x(canvas_w) - snap_x(0.0)), px(hair.max(1.0))),
                            },
                            gpui::rgb(0x00A0FF).into(),
                        ));
                    } else {
                        job.lines.push((
                            Bounds {
                                origin: point(px(snap_x(guide.position)), px(snap_y(0.0))),
                                size: size(px(hair.max(1.0)), px(snap_y(canvas_h) - snap_y(0.0))),
                            },
                            gpui::rgb(0x00A0FF).into(),
                        ));
                    }
                }
            }
        }

        // Marching ants: the selection's actual boundary, traced from the
        // coverage mask and cached until the selection changes.
        if has_selection && self.view.extras {
            let outline = self.selection_outline(selection_generation);
            for run in outline.iter() {
                let pts: Vec<Point<Pixels>> = run.iter().map(|&(x, y)| to_screen(x, y)).collect();
                push_ants(&mut job.ants, &pts, ant_phase);
            }
        }

        // Active tool overlays.
        for overlay in overlays {
            match overlay {
                Overlay::Rect(r) | Overlay::AntsRect(r) => {
                    job.outlines.push((
                        Bounds {
                            origin: to_screen(r.left as f32, r.top as f32),
                            size: size(px(r.width() as f32 * zoom), px(r.height() as f32 * zoom)),
                        },
                        gpui::rgb(0x44AAFF).into(),
                    ));
                }
                Overlay::AntsPolygon(points) => {
                    let pts: Vec<Point<Pixels>> =
                        points.iter().map(|&(x, y)| to_screen(x, y)).collect();
                    push_ants(&mut job.ants, &pts, ant_phase);
                }
                Overlay::Line { x1, y1, x2, y2 } => {
                    job.polylines.push((
                        vec![to_screen(x1, y1), to_screen(x2, y2)],
                        gpui::rgb(0xFFFFFF).into(),
                    ));
                }
                Overlay::Circle { cx: ccx, cy, r } => {
                    let d = r * 2.0 * zoom;
                    job.circles.push(Bounds {
                        origin: to_screen(ccx - r, cy - r),
                        size: size(px(d), px(d)),
                    });
                }
            }
        }
        job
    }

    pub fn render_canvas(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        div()
            .id("canvas")
            .flex_grow()
            .size_full()
            .overflow_hidden()
            .bg(gpui::rgb(0x262626))
            .track_focus(&self.focus)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|ws, ev, w, cx| ws.on_mouse_down(ev, w, cx)),
            )
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(|ws, ev, w, cx| ws.on_mouse_down(ev, w, cx)),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|ws, ev: &MouseDownEvent, _w, cx| {
                    ws.open_context_menu(ContextTarget::Canvas, ev.position, cx);
                }),
            )
            .on_mouse_move(cx.listener(|ws, ev, w, cx| ws.on_mouse_move(ev, w, cx)))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|ws, ev, w, cx| ws.on_mouse_up(ev, w, cx)),
            )
            .on_mouse_up(
                MouseButton::Middle,
                cx.listener(|ws, ev, w, cx| ws.on_mouse_up(ev, w, cx)),
            )
            .on_scroll_wheel(cx.listener(|ws, ev, w, cx| ws.on_scroll(ev, w, cx)))
            .on_pinch(cx.listener(|ws, ev, w, cx| ws.on_pinch(ev, w, cx)))
            .on_key_down(cx.listener(|ws, ev: &gpui::KeyDownEvent, _w, cx| {
                if ws.field_key(&ev.keystroke.key, ev.keystroke.key_char.as_deref()) {
                    cx.notify();
                    cx.stop_propagation();
                    return;
                }
                if ws.tool_key(ev) {
                    ws.after_change(cx);
                    cx.stop_propagation();
                    return;
                }
                if ev.keystroke.key == "space" && !ev.is_held {
                    ws.space_held = true;
                    cx.notify();
                }
            }))
            .on_key_up(cx.listener(|ws, ev: &gpui::KeyUpEvent, _w, cx| {
                if ev.keystroke.key == "space" {
                    ws.space_held = false;
                    ws.pan_last = None;
                    cx.notify();
                }
            }))
            .child(
                canvas(
                    move |bounds, window, cx| {
                        let scale = window.scale_factor();
                        entity.update(cx, |ws, _| ws.prepare_paint(bounds, scale))
                    },
                    move |_bounds, job: PaintJob, window, _cx| {
                        for (bounds, img) in job.tiles {
                            let _ =
                                window.paint_image(bounds, gpui::Corners::default(), img, 0, false);
                        }
                        // Release the atlas slots of images this frame
                        // replaced, so repainting doesn't grow the atlas.
                        for old in job.retired {
                            let _ = window.drop_image(old);
                        }
                        // Grid lines and guides sit above the artwork but
                        // below selection ants and tool overlays.
                        for (bounds, color) in job.lines {
                            window.paint_quad(gpui::fill(bounds, color));
                        }
                        for (bounds, color) in job.outlines {
                            window.paint_quad(gpui::outline(
                                bounds,
                                color,
                                gpui::BorderStyle::Solid,
                            ));
                        }
                        for (segments, color) in [
                            (job.ants.light, gpui::rgb(0xFFFFFF)),
                            (job.ants.dark, gpui::rgb(0x000000)),
                        ] {
                            if segments.is_empty() {
                                continue;
                            }
                            // One path per colour: a traced selection can be
                            // thousands of dashes.
                            let mut pb = PathBuilder::stroke(px(1.0));
                            for [a, b] in segments {
                                pb.move_to(a);
                                pb.line_to(b);
                            }
                            if let Ok(path) = pb.build() {
                                window.paint_path(path, color);
                            }
                        }
                        for (pts, color) in job.polylines {
                            if pts.len() < 2 {
                                continue;
                            }
                            let mut pb = PathBuilder::stroke(px(1.0));
                            pb.move_to(pts[0]);
                            for p in &pts[1..] {
                                pb.line_to(*p);
                            }
                            if let Ok(path) = pb.build() {
                                window.paint_path(path, color);
                            }
                        }
                        for bounds in job.circles {
                            let r = bounds.size.width / 2.0;
                            window.paint_quad(gpui::quad(
                                bounds,
                                r,
                                gpui::transparent_black(),
                                px(1.0),
                                gpui::rgb(0xEEEEEE),
                                gpui::BorderStyle::Solid,
                            ));
                        }
                    },
                )
                .size_full(),
            )
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

/// Dash segments batched by colour, so an outline of any complexity costs
/// two paths rather than one per dash.
#[derive(Default)]
pub struct Ants {
    light: Vec<[Point<Pixels>; 2]>,
    dark: Vec<[Point<Pixels>; 2]>,
}

#[derive(Default)]
pub struct PaintJob {
    tiles: Vec<(Bounds<Pixels>, Arc<RenderImage>)>,
    outlines: Vec<(Bounds<Pixels>, gpui::Hsla)>,
    polylines: Vec<(Vec<Point<Pixels>>, gpui::Hsla)>,
    /// Marching-ants dashes.
    ants: Ants,
    circles: Vec<Bounds<Pixels>>,
    /// Thin filled rectangles: grid lines, guides and ruler ticks.
    lines: Vec<(Bounds<Pixels>, gpui::Hsla)>,
    /// Images superseded this frame, freed after painting.
    retired: Vec<Arc<RenderImage>>,
}

impl Focusable for Workspace {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.focused_once {
            // The focus handle only exists in the dispatch tree once we've
            // rendered, so this can't happen at construction time.
            self.focused_once = true;
            window.focus(&self.focus);
        }
        if let Some((id, enabled)) = self.pending_plugin_toggle.take() {
            self.set_plugin_enabled(id, enabled, cx);
        }
        let captures_keys = self.tool_captures_keys() || self.modal.is_some();
        let chrome = self.screen_mode == ScreenMode::Standard;
        let modal = crate::dialogs::render(self, cx);
        let context_menu = panels::context_menu(self, window.viewport_size(), cx);
        let tool_flyout = panels::tool_flyout(self, cx);
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(gpui::rgb(if self.view.theme == Theme::Light {
                0xE8E8E8
            } else {
                0x1E1E1E
            }))
            .text_color(gpui::rgb(if self.view.theme == Theme::Light {
                0x1C1C1C
            } else {
                0xD8D8D8
            }))
            .text_size(px(12.0))
            // While a tool is capturing typing the context loses "editable",
            // which is what single-letter shortcuts are bound against — so
            // letters reach the tool instead of switching tools.
            .key_context(if captures_keys {
                "Workspace"
            } else {
                "Workspace editable"
            })
            .on_action(cx.listener(|ws, action: &RunCommand, _w, cx| {
                ws.run_command(&action.id.clone(), cx);
            }))
            .on_action(cx.listener(|ws, action: &ActivateTool, _w, cx| {
                ws.activate_tool(&action.id.clone(), cx);
            }))
            .on_action(cx.listener(|ws, action: &CycleToolGroup, _w, cx| {
                // The group ids are 'static strings from the registry; find
                // the matching one rather than leaking a new allocation.
                let group = ws
                    .tool_groups
                    .iter()
                    .map(|(g, _)| *g)
                    .find(|g| *g == action.group);
                if let Some(group) = group {
                    ws.cycle_tool_group(group, cx);
                }
            }))
            .on_action(cx.listener(|ws, action: &SetToolOpacity, _w, cx| {
                ws.editor.tool_opacity = action.percent as f32 / 100.0;
                ws.status = format!("Opacity: {}%", action.percent).into();
                cx.notify();
            }))
            .on_action(cx.listener(|ws, _: &NewFile, _w, cx| {
                ws.rebuild_tool_groups();
                ws.new_document();
                cx.notify();
            }))
            .on_action(cx.listener(|ws, _: &OpenFile, window, cx| {
                keymap::open_file_dialog(ws, window, cx);
            }))
            .on_action(cx.listener(|ws, _: &SaveFile, window, cx| {
                ws.save_current(window, cx);
            }))
            .on_action(cx.listener(|ws, _: &SaveFileAs, window, cx| {
                keymap::save_file_dialog(ws, window, cx);
            }))
            .on_action(cx.listener(|ws, _: &ZoomIn, _w, cx| {
                ws.zoom_by(1.25, None);
                cx.notify();
            }))
            .on_action(cx.listener(|ws, _: &ZoomOut, _w, cx| {
                ws.zoom_by(0.8, None);
                cx.notify();
            }))
            .on_action(cx.listener(|ws, _: &ZoomFit, _w, cx| {
                ws.fit_to_view();
                cx.notify();
            }))
            .on_action(cx.listener(|ws, _: &ZoomActual, _w, cx| {
                ws.zoom = 1.0;
                ws.editor.zoom = 1.0;
                ws.center();
                cx.notify();
            }))
            .on_action(cx.listener(|ws, _: &BrushSmaller, _w, cx| {
                ws.editor.brush_size = (ws.editor.brush_size / 1.25).max(1.0);
                cx.notify();
            }))
            .on_action(cx.listener(|ws, _: &BrushLarger, _w, cx| {
                ws.editor.brush_size = (ws.editor.brush_size * 1.25).min(500.0);
                cx.notify();
            }))
            .on_action(cx.listener(|ws, _: &SwapColors, _w, cx| {
                std::mem::swap(&mut ws.editor.foreground, &mut ws.editor.background);
                cx.notify();
            }))
            .on_action(cx.listener(|ws, _: &DefaultColors, _w, cx| {
                ws.editor.foreground = photoslop_color::Rgba::BLACK;
                ws.editor.background = photoslop_color::Rgba::WHITE;
                cx.notify();
            }))
            .on_action(cx.listener(|ws, _: &CancelGesture, _w, cx| {
                ws.cancel_gesture(cx);
            }))
            .on_action(cx.listener(|ws, _: &CommitGesture, _w, cx| {
                ws.commit_gesture(cx);
            }))
            .on_action(cx.listener(|ws, _: &ShowImageSize, _w, cx| {
                if let Some(doc) = ws.doc.as_ref() {
                    let modal = Modal::ImageSize {
                        width: doc.width,
                        height: doc.height,
                        filter: ws.editor.resample,
                        link: true,
                    };
                    ws.open_modal(modal, cx);
                }
            }))
            .on_action(cx.listener(|ws, action: &AddAdjustment, _w, cx| {
                let kind = match action.kind.as_str() {
                    "levels" => photoslop_core::AdjustmentKind::Levels,
                    "curves" => photoslop_core::AdjustmentKind::Curves,
                    "hue_saturation" => photoslop_core::AdjustmentKind::HueSaturation,
                    "invert" => photoslop_core::AdjustmentKind::Invert,
                    "brightness_contrast" => photoslop_core::AdjustmentKind::BrightnessContrast,
                    other => {
                        log::warn!("unknown adjustment {other}");
                        return;
                    }
                };
                ws.add_adjustment(kind, cx);
            }))
            .on_action(cx.listener(|ws, _: &ToggleRulers, _w, cx| ws.toggle_rulers(cx)))
            .on_action(cx.listener(|ws, _: &ToggleGrid, _w, cx| ws.toggle_grid(cx)))
            .on_action(cx.listener(|ws, _: &ToggleGuides, _w, cx| ws.toggle_guides(cx)))
            .on_action(cx.listener(|ws, _: &ToggleExtras, _w, cx| ws.toggle_extras(cx)))
            .on_action(cx.listener(|ws, _: &ToggleSnap, _w, cx| ws.toggle_snap(cx)))
            .on_action(cx.listener(|ws, _: &ClearGuides, _w, cx| ws.clear_guides(cx)))
            .on_action(cx.listener(|ws, _: &CycleScreenMode, _w, cx| ws.cycle_screen_mode(cx)))
            .on_action(cx.listener(|ws, _: &TogglePanels, _w, cx| ws.cycle_screen_mode(cx)))
            .on_action(cx.listener(|ws, _: &ShowLayerStyle, _w, cx| {
                if let Some(id) = ws.doc.as_ref().and_then(|d| d.active_layer) {
                    ws.show_layer_style(id, cx);
                }
            }))
            .on_action(cx.listener(|ws, _: &ShowPreferences, _w, cx| {
                ws.open_modal(Modal::Preferences, cx);
            }))
            .on_action(cx.listener(|ws, _: &ShowCanvasSize, _w, cx| {
                if let Some(doc) = ws.doc.as_ref() {
                    let modal = Modal::CanvasSize {
                        width: doc.width,
                        height: doc.height,
                        anchor: (0.5, 0.5),
                    };
                    ws.open_modal(modal, cx);
                }
            }))
            .on_action(|_: &Quit, _w, cx| cx.quit())
            .children(chrome.then(|| panels::menu_bar(self, cx)))
            .children(chrome.then(|| panels::tool_options_bar(self, cx)))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_grow()
                    .min_h(px(0.0))
                    .children(chrome.then(|| panels::toolbar(self, cx)))
                    .child(
                        div()
                            .relative()
                            .flex()
                            .flex_grow()
                            .size_full()
                            .child(self.render_canvas(cx))
                            .children(
                                (chrome && self.view.rulers).then(|| panels::rulers(self, cx)),
                            ),
                    )
                    .children(chrome.then(|| panels::side_panels(self, cx))),
            )
            .children(chrome.then(|| panels::status_bar(self)))
            .children(tool_flyout)
            .children(context_menu)
            .children(modal)
    }
}

/// Blend one tile's worth of a filtered region back over the original,
/// weighted by selection coverage.
#[allow(clippy::too_many_arguments)]
fn blend_region_tile(
    tile: &mut photoslop_core::TileBuf,
    coord: TileCoord,
    clip: IntRect,
    region: IntRect,
    original: &[f32],
    filtered: &[f32],
    selection: &photoslop_core::Selection,
) {
    let trect = coord.rect();
    let w = region.width() as usize;
    for y in clip.top..clip.bottom {
        for x in clip.left..clip.right {
            let cov = selection.coverage(x, y) as f32 / 255.0;
            if cov <= 0.0 {
                continue;
            }
            let src = ((y - region.top) as usize * w + (x - region.left) as usize) * 4;
            let ix = ((y - trect.top) * TILE_SIZE + (x - trect.left)) as usize;
            let mix = |a: f32, b: f32| a + (b - a) * cov;
            tile.set(
                ix,
                photoslop_color::Rgba::new(
                    mix(original[src], filtered[src]),
                    mix(original[src + 1], filtered[src + 1]),
                    mix(original[src + 2], filtered[src + 2]),
                    mix(original[src + 3], filtered[src + 3]),
                ),
            );
        }
    }
}

/// Walk the layer tree rebuilding stale styled rasters, collecting the
/// areas that changed so they can be repainted.
fn restyle_layers(layers: &mut [Layer], damage: &mut Vec<IntRect>) {
    for layer in layers.iter_mut() {
        if let photoslop_core::LayerKind::Group(g) = &mut layer.kind {
            restyle_layers(&mut g.children, damage);
        }
        let wanted = !layer.style.is_empty();
        if !wanted {
            if let Some(old) = layer.styled.take() {
                damage.push(old.bounds);
            }
            continue;
        }
        // `fx_key` changes whenever anything the raster depends on does.
        let key = fx_key(layer);
        if layer.styled.as_ref().map(|s| s.key) == Some(key) {
            continue;
        }
        let before = layer.styled.as_ref().map(|s| s.bounds);
        layer.styled = photoslop_layer_fx::render(layer).map(|mut r| {
            r.key = key;
            Arc::new(r)
        });
        if let Some(b) = before {
            damage.push(b);
        }
        if let Some(s) = layer.styled.as_ref() {
            damage.push(s.bounds);
        }
    }
}

/// A cheap fingerprint of everything the styled raster is derived from.
fn fx_key(layer: &Layer) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = rustc_hash::FxHasher::default();
    // The style itself, via its debug form: these are small plain structs
    // with float fields, so there is nothing cheaper that is also correct.
    format!("{:?}", layer.style).hash(&mut h);
    layer.fill_opacity.to_bits().hash(&mut h);
    if let Some(r) = layer.as_raster() {
        r.tiles.fingerprint().hash(&mut h);
    }
    h.finish()
}

/// Whether one named effect is switched on, for the dialog's initial tab.
fn style_enabled(style: &photoslop_core::LayerStyle, key: &str) -> bool {
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
    sel: &mut photoslop_core::Selection,
    other: &photoslop_core::Selection,
    canvas: IntRect,
) {
    let rect = sel.bounds().intersect(&canvas);
    let keep = sel.clone();
    sel.deselect();
    sel.activate();
    sel.apply_shape(rect, photoslop_core::SelectOp::Replace, |x, y| {
        keep.coverage(x, y).saturating_sub(other.coverage(x, y))
    });
}

/// Re-rasterize any shape layer whose path, fill or stroke has moved.
///
/// This is what keeps a vector shape sharp: the pixels are a cache of the
/// path, thrown away and rebuilt rather than resampled.
fn reshape_layers(layers: &mut [Layer], depth: Depth, canvas: IntRect, damage: &mut Vec<IntRect>) {
    for layer in layers.iter_mut() {
        if let photoslop_core::LayerKind::Group(g) = &mut layer.kind {
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
        let tiles = photoslop_tools_vector::render_shape(shape, depth, canvas);
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

/// Fetch a model over HTTP. Blocking, so it runs on a background thread.
fn fetch_model(url: &str) -> Result<Vec<u8>, String> {
    use std::io::Read as _;
    // Models are single-digit megabytes; the cap is a guard against a
    // redirect to something enormous, not a real limit.
    const MAX: u64 = 256 << 20;
    let mut response = ureq::get(url)
        .header("User-Agent", "photoslop-model-fetch")
        .call()
        .map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    let reader = response.body_mut().as_reader();
    std::io::Read::take(reader, MAX)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.is_empty() {
        return Err("empty response".into());
    }
    Ok(bytes)
}
