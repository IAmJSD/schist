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
    /// Author stamped on notes as they are placed. Mirrored from the Note
    /// tool's options bar, and persisted across sessions by the shell --
    /// a reviewer types their name once, not once per note.
    pub note_author: String,
    /// Colour new notes are given, likewise from the options bar.
    pub note_color: Rgba,
    /// Index into `Document::notes` of the note the Notes panel is
    /// showing. Set by the Note tool when a marker is clicked, and by the
    /// panel's own navigation.
    ///
    /// An index rather than an id because notes carry none; anything that
    /// shortens the list has to clamp or clear this, which is why every
    /// read goes through `Document::notes.get()`.
    pub active_note: Option<usize>,
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
            note_author: String::new(),
            note_color: schist_core::DEFAULT_NOTE_COLOR,
            active_note: None,
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
    /// Translucent selection highlight, drawn above artwork and below
    /// outline/caret chrome.
    Highlight(IntRect),
    /// Circle outline (brush cursor), document-space center and radius.
    Circle { cx: f32, cy: f32, r: f32 },
    /// Straight line segment.
    Line { x1: f32, y1: f32, x2: f32, y2: f32 },
    /// A note's pin, filled in the note's own colour. Drawn at a fixed
    /// *screen* size like Photoshop's, so a note stays findable and
    /// clickable whether the document is at 5% or 1600%.
    NoteMarker {
        x: f32,
        y: f32,
        color: Rgba,
        /// The note showing in the Notes panel, outlined so the panel and
        /// the canvas agree on which one is being read.
        selected: bool,
    },
}

/// A canvas tool. One tool is active at a time; the canvas routes pointer
/// events (already in document space) to it.
pub trait ToolPlugin: Send {
    /// Stable identifier, e.g. "brush".
    fn id(&self) -> &'static str;
    fn name(&self) -> &'static str;
    /// A sentence saying what the tool does and how it is driven, for
    /// hosts that have no toolbar to point at: the MCP server publishes
    /// one tool per canvas tool and this is its description. Defaulted
    /// so a third-party plugin still compiles; it then falls back to
    /// the name.
    fn description(&self) -> &'static str {
        ""
    }
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
    /// What the command does, in a sentence. The title is a menu label
    /// ("Grow", "Similar") and says nothing on its own to a caller that
    /// cannot see the menu it sits in -- the MCP server publishes every
    /// command as its own tool and this is its description.
    pub description: &'static str,
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

/// An image handed to a filter alongside the layer it is working on.
///
/// Straight-alpha RGBA, the same layout as the buffer a filter is given,
/// but any size: a displacement map is whatever file somebody picked.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FilterImage {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<f32>,
}

impl FilterImage {
    /// Sample at a fraction of the image, stretching it to whatever it is
    /// being used over. Photoshop calls this Stretch To Fit.
    pub fn stretched(&self, u: f32, v: f32) -> [f32; 4] {
        self.texel(u * self.width as f32, v * self.height as f32)
    }

    /// Sample in the map's own pixels, repeating it. Photoshop calls this
    /// Tile.
    pub fn tiled(&self, x: f32, y: f32) -> [f32; 4] {
        self.texel(
            x.rem_euclid(self.width.max(1) as f32),
            y.rem_euclid(self.height.max(1) as f32),
        )
    }

    fn texel(&self, x: f32, y: f32) -> [f32; 4] {
        if self.width == 0 || self.height == 0 {
            return [0.0; 4];
        }
        let x = (x as usize).min(self.width - 1);
        let y = (y as usize).min(self.height - 1);
        let i = (y * self.width + x) * 4;
        [
            self.pixels[i],
            self.pixels[i + 1],
            self.pixels[i + 2],
            self.pixels[i + 3],
        ]
    }
}

/// Everything a filter can be given beyond its own pixels and numbers.
///
/// Photoshop's filters read the toolbox and the open document as freely
/// as they read the layer: the Sketch group paints in the foreground and
/// background colours, Clouds renders in them, Displace warps through a
/// file somebody picked, Flame follows the current path. A filter here is
/// a plug-in with none of that in reach, so the host gathers whatever
/// each one asks for and hands it over.
#[derive(Clone, Copy)]
pub struct FilterContext<'a> {
    /// What the document composites to under the layer being filtered.
    pub backdrop: Option<&'a [f32]>,
    /// The toolbox's two colours.
    pub foreground: Rgba,
    pub background: Rgba,
    /// The image picked for this run, for filters that take one.
    pub map: Option<&'a FilterImage>,
    /// The document's active path, flattened into the filter's own
    /// coordinates.
    pub path: Option<&'a [(f32, f32)]>,
}

impl Default for FilterContext<'_> {
    fn default() -> Self {
        FilterContext {
            backdrop: None,
            // The swatches a fresh Photoshop opens with, and what every
            // one of these filters is pictured with.
            foreground: Rgba::BLACK,
            background: Rgba::WHITE,
            map: None,
            path: None,
        }
    }
}

impl FilterContext<'_> {
    /// The foreground as a plain array, which is what a filter mixing
    /// colours wants.
    pub fn fg(&self) -> [f32; 3] {
        [self.foreground.r, self.foreground.g, self.foreground.b]
    }

    /// The background, likewise.
    pub fn bg(&self) -> [f32; 3] {
        [self.background.r, self.background.g, self.background.b]
    }
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
    /// Transfer -- need something to match *to*, and the nearest thing
    /// to hand is what the document composites to below. Photoshop's
    /// Harmonization asks for a layer the same way.
    fn wants_backdrop(&self) -> bool {
        false
    }

    /// The label of the image this filter takes, if it takes one.
    ///
    /// Displace warps through a map somebody chose from a file, and
    /// Photoshop asks for it with a file dialog before the filter runs.
    /// Returning a label here is what makes the host offer one.
    fn wants_map(&self) -> Option<&'static str> {
        None
    }

    /// Whether this filter wants the document's active path.
    ///
    /// Flame draws along one. A filter cannot reach into the document,
    /// so the host flattens the path and passes the points.
    fn wants_path(&self) -> bool {
        false
    }

    /// Apply with everything the host was able to gather.
    ///
    /// The default ignores the context, which is right for every filter
    /// that asked for nothing -- most of them.
    fn apply_with(
        &self,
        pixels: &mut [f32],
        width: usize,
        height: usize,
        values: &FilterValues,
        context: &FilterContext,
    ) {
        let _ = context;
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
