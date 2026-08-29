//! The plugin contract surface.
//!
//! Every user-facing feature implements one of these traits and registers
//! through a `PluginManifest`. First-party plugins are workspace crates
//! compiled in; the same conceptual API is projected to sandboxed WASM
//! plugins. Tool/command plugins are deliberately GPUI-free: input
//! arrives as our own event types and overlays are returned as primitive
//! shapes the canvas draws — which also makes tools unit-testable headless.

use schist_color::Rgba;
use schist_core::{Document, IntRect};

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
    pub resample: schist_core::Filter,
    /// Colour tolerance, 0..=255. Mirrored from the magic wand, which is
    /// where Photoshop's Grow, Similar and Color Range take theirs from.
    pub tolerance: u8,
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
            resample: schist_core::Filter::Bicubic,
            tolerance: 32,
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

    /// Tools sharing a group id occupy one toolbar slot, like Photoshop's
    /// nested tools: the slot shows whichever one was last used, a flyout
    /// lists the rest, and Shift+the group's key cycles them. Defaults to
    /// the tool's own id, i.e. a group of one.
    fn group(&self) -> &'static str {
        self.id()
    }

    /// False for tools that are reachable only by command or shortcut and
    /// shouldn't take a toolbar slot (free transform, for instance).
    fn in_toolbar(&self) -> bool {
        true
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

    /// Controls this tool wants in the options bar, left to right. The
    /// shell renders them generically and calls `set_option` on change, so
    /// a tool can carry settings without the shell knowing about it.
    fn options(&self) -> Vec<ToolOption> {
        Vec::new()
    }

    /// Apply a change made in the options bar. `key` is the one given in
    /// [`ToolOption`].
    fn set_option(&mut self, _key: &str, _value: OptionValue) {}

    /// Called straight after `set_option`, with the document in hand.
    ///
    /// Most tools only need `set_option`, which stores the value for the
    /// next gesture. A tool holding a live session -- the type tool with
    /// an open text layer -- needs to re-render it here, or changing the
    /// font size would do nothing visible until the next click.
    fn on_option_changed(&mut self, _ctx: &mut ToolCtx, _key: &str) {}
}

/// A control the options bar should show for the active tool.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolOption {
    pub key: &'static str,
    pub label: &'static str,
    pub kind: OptionKind,
    pub value: OptionValue,
}

impl ToolOption {
    pub fn slider(
        key: &'static str,
        label: &'static str,
        value: f32,
        min: f32,
        max: f32,
        suffix: &'static str,
    ) -> ToolOption {
        ToolOption {
            key,
            label,
            kind: OptionKind::Slider { min, max, suffix },
            value: OptionValue::Num(value),
        }
    }

    pub fn toggle(key: &'static str, label: &'static str, value: bool) -> ToolOption {
        ToolOption {
            key,
            label,
            kind: OptionKind::Toggle,
            value: OptionValue::Bool(value),
        }
    }

    pub fn choice(
        key: &'static str,
        label: &'static str,
        options: &'static [&'static str],
        value: usize,
    ) -> ToolOption {
        ToolOption {
            key,
            label,
            kind: OptionKind::Choice(options),
            value: OptionValue::Choice(value),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum OptionKind {
    /// Continuous value shown as a drag track.
    Slider {
        min: f32,
        max: f32,
        /// Appended to the readout, e.g. " px" or "%".
        suffix: &'static str,
    },
    /// On/off checkbox.
    Toggle,
    /// One of a fixed list, shown as a dropdown.
    Choice(&'static [&'static str]),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum OptionValue {
    Num(f32),
    Bool(bool),
    Choice(usize),
}

impl OptionValue {
    pub fn num(self) -> f32 {
        match self {
            OptionValue::Num(v) => v,
            OptionValue::Bool(b) => b as u8 as f32,
            OptionValue::Choice(i) => i as f32,
        }
    }

    pub fn bool(self) -> bool {
        match self {
            OptionValue::Bool(b) => b,
            OptionValue::Num(v) => v != 0.0,
            OptionValue::Choice(i) => i != 0,
        }
    }

    pub fn index(self) -> usize {
        match self {
            OptionValue::Choice(i) => i,
            OptionValue::Num(v) => v.max(0.0) as usize,
            OptionValue::Bool(b) => b as usize,
        }
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
    /// Why the command did nothing, when it did nothing.
    ///
    /// The command layer had no way to say anything: it is full of bare
    /// `return`s and the shell then set the status line to the command's
    /// own title regardless, so Grow with no selection, Clipping Mask on
    /// the bottom layer, Paste with an empty clipboard and a dozen others
    /// all reported themselves as having worked.
    pub refusal: Option<String>,
}

impl CommandCtx<'_> {
    /// Decline to act, with a reason the user will see.
    pub fn refuse(&mut self, why: impl Into<String>) {
        self.refusal = Some(why.into());
    }
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
    /// Names for a parameter whose values are a list rather than a
    /// quantity. When this is non-empty the value is an index into it and
    /// the host shows the name instead of the number, which is the
    /// difference between a control that reads "Mosaic" and one that
    /// reads "0.00".
    pub choices: &'static [&'static str],
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

    /// Whether this filter wants to see what is underneath the layer.
    ///
    /// Most do not: a filter is a function of its own pixels. The ones
    /// that match one image to another -- Harmonization, Colour
    /// Transfer -- need something to match *to*, and the only second
    /// image a filter dialog can offer without a file picker is the one
    /// already in the document. Photoshop's Harmonization asks for a
    /// layer the same way.
    fn wants_backdrop(&self) -> bool {
        false
    }

    /// Apply with the pixels underneath the layer, when the host has
    /// them.
    ///
    /// `backdrop` is the same size as `pixels` and is what the document
    /// composites to under this layer, or `None` when there is nothing
    /// below or the host cannot produce one. The default ignores it,
    /// which is right for every filter that did not ask.
    fn apply_over(
        &self,
        pixels: &mut [f32],
        backdrop: Option<&[f32]>,
        width: usize,
        height: usize,
        values: &FilterValues,
    ) {
        let _ = backdrop;
        self.apply(pixels, width, height, values);
    }

    /// A line shown in the filter's dialog, for anything the user should
    /// know before running it -- which is mostly whether a neural filter
    /// found its model or is about to use its fallback.
    fn info(&self) -> Option<String> {
        None
    }

    /// Whether `apply` blocks on something outside this process — a
    /// helper, a dialog someone has to answer, a network call. A host
    /// with a UI thread should run these off it; one without can ignore
    /// this. Filters that own their pixels return `false` and are run
    /// inline, which is both faster and what every caller already
    /// expected.
    fn runs_out_of_process(&self) -> bool {
        false
    }

    /// Why the last [`FilterPlugin::apply`] did nothing, if it did
    /// nothing. `apply` has no return value because a filter that owns
    /// its pixels cannot fail -- but one that hands them to a separate
    /// process can, and a host that recorded "applied" for a run the
    /// user cancelled would be lying to them.
    ///
    /// Checked immediately after `apply`, so an implementation only has
    /// to remember the most recent run.
    fn last_error(&self) -> Option<String> {
        None
    }
}
