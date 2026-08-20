//! The plugin contract surface (PLAN.md §3.2).
//!
//! Every user-facing feature implements one of these traits and registers
//! through a `PluginManifest`. First-party plugins are workspace crates
//! compiled in; the same conceptual API is projected to sandboxed WASM
//! plugins in M9. Tool/command plugins are deliberately GPUI-free: input
//! arrives as our own event types and overlays are returned as primitive
//! shapes the canvas draws — which also makes tools unit-testable headless.

use photoslop_color::Rgba;
use photoslop_core::{Document, IntRect};

pub use registry::{PluginManifest, PluginRegistry};

pub mod registry;

/// Keyboard modifiers accompanying a pointer event.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Modifiers {
    pub shift: bool,
    pub alt: bool,
    pub ctrl_or_cmd: bool,
}

/// A pointer event, already transformed into document space.
#[derive(Debug, Clone, Copy)]
pub struct PointerInput {
    /// Document-space position (fractional: zoomed-in clicks land between
    /// pixels).
    pub x: f32,
    pub y: f32,
    /// 0.0..=1.0; mouse reports 1.0.
    pub pressure: f32,
    pub modifiers: Modifiers,
}

/// Pixels on the internal clipboard (system-clipboard PNG interop is
/// handled by the app shell where the platform allows it).
#[derive(Debug, Clone)]
pub struct ClipboardImage {
    /// Where the pixels came from, so paste-in-place works.
    pub rect: IntRect,
    /// Straight-alpha RGBA8, rect.width() * rect.height() * 4.
    pub rgba: Vec<u8>,
}

/// Shared editing state that tools and commands read and write.
#[derive(Debug, Clone)]
pub struct EditorState {
    pub foreground: Rgba,
    pub background: Rgba,
    pub brush_size: f32,
    /// 0.0 = maximally soft edge, 1.0 = hard edge.
    pub brush_hardness: f32,
    /// Tool opacity (digit keys), 0.0..=1.0.
    pub tool_opacity: f32,
    pub active_tool: &'static str,
    pub clipboard: Option<std::sync::Arc<ClipboardImage>>,
    /// Current canvas zoom, mirrored from the viewport so tools can size
    /// hit targets and handles in *screen* pixels.
    pub zoom: f32,
    /// Resampling filter used when committing transforms.
    pub resample: photoslop_core::Filter,
}

impl Default for EditorState {
    fn default() -> Self {
        EditorState {
            foreground: Rgba::BLACK,
            background: Rgba::WHITE,
            brush_size: 24.0,
            brush_hardness: 0.5,
            tool_opacity: 1.0,
            active_tool: "move",
            clipboard: None,
            zoom: 1.0,
            resample: photoslop_core::Filter::Bicubic,
        }
    }
}

/// Everything a tool may touch while handling input.
pub struct ToolCtx<'a> {
    pub doc: &'a mut Document,
    pub state: &'a mut EditorState,
}

/// Overlay primitives a tool asks the canvas to draw (marching ants,
/// transform handles, brush cursor…). Coordinates are document-space.
#[derive(Debug, Clone)]
pub enum Overlay {
    /// Dashed "marching ants" rectangle.
    AntsRect(IntRect),
    /// Dashed polygon outline.
    AntsPolygon(Vec<(f32, f32)>),
    /// Solid outline rect (e.g. layer bounds during move).
    Rect(IntRect),
    /// Circle outline (brush cursor), document-space center and radius.
    Circle { cx: f32, cy: f32, r: f32 },
    /// Straight line segment.
    Line { x1: f32, y1: f32, x2: f32, y2: f32 },
}

/// A canvas tool. One tool is active at a time; the canvas routes pointer
/// events (already in document space) to it.
pub trait ToolPlugin: Send {
    /// Stable identifier, e.g. "brush".
    fn id(&self) -> &'static str;
    fn name(&self) -> &'static str;
    /// Icon asset name (the app shell renders `icons/<name>.svg`).
    fn icon(&self) -> &'static str;
    /// Default activation key, e.g. "b". Registered into the keymap.
    fn shortcut(&self) -> Option<&'static str> {
        None
    }

    fn on_pointer_down(&mut self, ctx: &mut ToolCtx, input: PointerInput);
    fn on_pointer_move(&mut self, ctx: &mut ToolCtx, input: PointerInput);
    fn on_pointer_up(&mut self, ctx: &mut ToolCtx, input: PointerInput);
    /// True while the tool wants raw typing (the type tool with an open
    /// text layer). The shell switches the keymap context so single-letter
    /// shortcuts stop matching, and routes keys to `on_key` instead.
    fn captures_keys(&self) -> bool {
        false
    }

    /// Raw key input, delivered before the keymap so a tool that is
    /// capturing text (the type tool) can swallow plain letters instead of
    /// letting them switch tools. `key` is the physical key name
    /// ("a", "enter", "backspace"); `text` is the character it would type,
    /// when it types one. Return true to consume the event.
    fn on_key(
        &mut self,
        _ctx: &mut ToolCtx,
        _key: &str,
        _text: Option<&str>,
        _modifiers: Modifiers,
    ) -> bool {
        false
    }

    /// The user switched to this tool. Modal tools (free transform, crop)
    /// start their session here.
    fn on_activate(&mut self, _ctx: &mut ToolCtx) {}
    /// Enter pressed: commit whatever the tool has pending.
    fn on_commit(&mut self, _ctx: &mut ToolCtx) {}
    /// Escape pressed while the tool is mid-gesture.
    fn on_cancel(&mut self, _ctx: &mut ToolCtx) {}
    /// Called when the user switches away from this tool.
    fn on_deactivate(&mut self, _ctx: &mut ToolCtx) {}

    /// Overlay shapes to draw this frame (in addition to the persistent
    /// selection ants the canvas draws itself).
    fn overlays(&self, _doc: &Document, _state: &EditorState) -> Vec<Overlay> {
        Vec::new()
    }
}

/// Encoder settings chosen in the export dialog.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExportOptions {
    /// 1..=100 for lossy formats; ignored by lossless ones.
    pub quality: u8,
    /// Bits per channel to write, where the format supports a choice.
    pub bit_depth: u8,
    /// Dither when reducing depth, to avoid banding.
    pub dither: bool,
}

impl Default for ExportOptions {
    fn default() -> Self {
        ExportOptions {
            quality: 90,
            bit_depth: 8,
            dither: true,
        }
    }
}

/// An image format importer/exporter.
pub trait CodecPlugin: Send + Sync {
    fn id(&self) -> &'static str;
    fn name(&self) -> &'static str;
    /// Lowercase extensions without dot, e.g. ["png"].
    fn extensions(&self) -> &'static [&'static str];
    /// Cheap signature sniff.
    fn probe(&self, bytes: &[u8]) -> bool;
    fn import(&self, bytes: &[u8]) -> anyhow::Result<Document>;
    fn can_export(&self) -> bool {
        false
    }
    fn export(&self, _doc: &Document) -> anyhow::Result<Vec<u8>> {
        anyhow::bail!("{} cannot export", self.name())
    }
    /// Export with explicit settings. Defaults to the plain export for
    /// formats that have nothing to tune.
    fn export_with(&self, doc: &Document, _options: &ExportOptions) -> anyhow::Result<Vec<u8>> {
        self.export(doc)
    }
    /// Whether the export dialog should offer a quality slider.
    fn supports_quality(&self) -> bool {
        false
    }
}

/// Context handed to commands (menu items / keybound actions).
pub struct CommandCtx<'a> {
    pub doc: &'a mut Document,
    pub state: &'a mut EditorState,
}

/// A named, keybindable command. `id` is namespaced like "layer.duplicate".
pub struct Command {
    pub id: &'static str,
    pub title: &'static str,
    /// Default keybinding in GPUI keystroke syntax, e.g. "cmd-shift-e"
    /// ("cmd" is mapped to ctrl on Linux/Windows by the app shell).
    pub keybind: Option<&'static str>,
    pub run: Box<dyn Fn(&mut CommandCtx) + Send>,
}

/// A bag of commands contributed by a plugin.
pub trait CommandPlugin: Send {
    fn commands(&self) -> Vec<Command>;
}

/// One tunable of a filter. The shell turns these into dialog sliders, so
/// a filter never has to know anything about the UI toolkit.
#[derive(Debug, Clone, Copy)]
pub struct FilterParam {
    pub key: &'static str,
    pub label: &'static str,
    pub min: f32,
    pub max: f32,
    pub default: f32,
    /// Displayed after the value, e.g. " px" or "%".
    pub suffix: &'static str,
}

/// Parameter values keyed by [`FilterParam::key`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FilterValues(pub Vec<(&'static str, f32)>);

impl FilterValues {
    pub fn defaults(params: &[FilterParam]) -> FilterValues {
        FilterValues(params.iter().map(|p| (p.key, p.default)).collect())
    }

    pub fn get(&self, key: &str) -> f32 {
        self.0
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| *v)
            .unwrap_or_default()
    }

    pub fn set(&mut self, key: &'static str, value: f32) {
        match self.0.iter_mut().find(|(k, _)| *k == key) {
            Some(slot) => slot.1 = value,
            None => self.0.push((key, value)),
        }
    }
}

/// A destructive pixel filter.
pub trait FilterPlugin: Send + Sync {
    fn id(&self) -> &'static str;
    fn name(&self) -> &'static str;
    /// Menu grouping, e.g. "Blur" or "Sharpen".
    fn category(&self) -> &'static str {
        "Other"
    }
    fn params(&self) -> Vec<FilterParam> {
        Vec::new()
    }
    /// Apply in place to a straight-alpha f32 RGBA buffer of
    /// `width * height` pixels.
    fn apply(&self, pixels: &mut [f32], width: usize, height: usize, values: &FilterValues);
}
