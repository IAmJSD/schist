//! Type tool (T): editable text layers.
//!
//! A text layer is a raster layer plus a `PsTx` block in its preserved-PSD
//! extras holding the JSON [`TextSpec`] it was rendered from. That block
//! rides through save/load untouched (the PSD writer re-emits unknown
//! blocks verbatim), so text stays re-editable across sessions while
//! Photoshop still sees ordinary pixels.

use schist_color::Rgba;
use schist_core::{
    Document, IntRect, Layer, LayerId, LayerPath, RawBlock, TileCoord, TileMap, TILE_SIZE,
};
use schist_plugin_api::{
    EditorState, Modifiers, OptionValue, Overlay, PluginManifest, PluginRegistry, PointerInput,
    ToolCtx, ToolOption, ToolPlugin,
};
use schist_text_engine::{rasterize, Align, TextSpec};

/// Additional-layer-info key under which the text spec is preserved.
pub const TEXT_BLOCK_KEY: [u8; 4] = *b"PsTx";

/// What a text layer stores alongside the spec.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct StoredText {
    spec: TextSpec,
    /// Document-space origin of the layout box.
    origin: (i32, i32),
    /// Fill colour as RGBA bytes.
    color: [u8; 4],
}

fn read_stored(layer: &Layer) -> Option<StoredText> {
    let block = layer.extras.iter().find(|b| b.key == TEXT_BLOCK_KEY)?;
    match serde_json::from_slice(&block.data) {
        Ok(v) => Some(v),
        Err(err) => {
            log::warn!("text layer {:?} has unreadable spec: {err}", layer.name);
            None
        }
    }
}

fn write_stored(layer: &mut Layer, stored: &StoredText) {
    let data = match serde_json::to_vec(stored) {
        Ok(d) => d,
        Err(err) => {
            log::error!("cannot serialize text spec: {err}");
            return;
        }
    };
    layer.extras.retain(|b| b.key != TEXT_BLOCK_KEY);
    layer.extras.push(RawBlock {
        key: TEXT_BLOCK_KEY,
        data,
    });
}

/// Render a text spec into a fresh tile map at `origin`.
fn render_tiles(doc: &Document, stored: &StoredText) -> (TileMap, IntRect) {
    let mut tiles = TileMap::new();
    let Some(raster) = rasterize(&stored.spec) else {
        return (tiles, IntRect::EMPTY);
    };
    if raster.is_empty() {
        return (tiles, IntRect::EMPTY);
    }
    let bounds = raster.bounds.translated(stored.origin.0, stored.origin.1);
    let w = raster.bounds.width() as usize;
    let color = Rgba::from_u8(
        stored.color[0],
        stored.color[1],
        stored.color[2],
        stored.color[3],
    );
    let depth = doc.depth;
    for coord in TileCoord::covering(&bounds) {
        let trect = coord.rect();
        let clip = trect.intersect(&bounds);
        if clip.is_empty() {
            continue;
        }
        let buf = tiles.get_mut_or_insert(coord, depth);
        for y in clip.top..clip.bottom {
            for x in clip.left..clip.right {
                let cov =
                    raster.coverage[(y - bounds.top) as usize * w + (x - bounds.left) as usize];
                if cov == 0 {
                    continue;
                }
                let ix = ((y - trect.top) * TILE_SIZE + (x - trect.left)) as usize;
                buf.set(
                    ix,
                    Rgba {
                        a: color.a * (cov as f32 / 255.0),
                        ..color
                    },
                );
            }
        }
    }
    tiles.prune_blank();
    (tiles, bounds)
}

/// Every font family the document's text layers ask for, in the order
/// first seen and without repeats.
pub fn families_used(doc: &Document) -> Vec<String> {
    fn walk(layers: &[Layer], out: &mut Vec<String>) {
        for layer in layers {
            if let Some(children) = layer.children() {
                walk(children, out);
            }
            let Some(stored) = read_stored(layer) else {
                continue;
            };
            let family = stored.spec.family.trim();
            if !family.is_empty() && !out.iter().any(|f| f == family) {
                out.push(family.to_string());
            }
        }
    }
    let mut out = Vec::new();
    walk(&doc.tree.layers, &mut out);
    out
}

/// Re-set every text layer in `family` and repaint it.
///
/// A layer set in a font that was missing was rasterized in whatever the
/// engine substituted; once the real font arrives those pixels are stale
/// and only a re-render fixes them. Returns how many layers changed.
pub fn rerender_family(doc: &mut Document, family: &str) -> usize {
    fn collect(layers: &[Layer], family: &str, out: &mut Vec<schist_core::LayerId>) {
        for layer in layers {
            if let Some(children) = layer.children() {
                collect(children, family, out);
            }
            if read_stored(layer).is_some_and(|s| s.spec.family.trim().eq_ignore_ascii_case(family))
            {
                out.push(layer.id);
            }
        }
    }
    let mut ids = Vec::new();
    collect(&doc.tree.layers, family, &mut ids);
    if ids.is_empty() {
        return 0;
    }
    // One undoable edit for the lot. These pixels used to be assigned
    // straight onto the raster with nothing but an `add_damage` call --
    // no `begin_edit`, so "Installed Inter (2 faces) - re-set 7 text
    // layer(s)" changed seven layers with no way to undo it, and the
    // document was not even marked dirty, so it could be closed without
    // a save prompt.
    let mut rendered = Vec::new();
    for id in &ids {
        let Some(stored) = doc.tree.find(*id).and_then(read_stored) else {
            continue;
        };
        let (tiles, _bounds) = render_tiles(doc, &stored);
        rendered.push((*id, tiles));
    }
    if rendered.is_empty() {
        return 0;
    }
    let changed = rendered.len();
    let mut edit = doc.begin_edit("Update Fonts");
    for (id, tiles) in rendered {
        edit.replace_layer_tiles(id, tiles);
    }
    edit.commit();
    // The style caches were built from the old glyphs.
    for id in ids {
        if let Some(layer) = doc.tree.find_mut(id) {
            layer.styled = None;
        }
    }
    changed
}

/// An editing session over one text layer.
struct Editing {
    layer: LayerId,
    stored: StoredText,
    /// Pixels before this session, for undo capture on commit.
    original: TileMap,
    /// True once the layer was created by this session (so cancelling
    /// removes it entirely).
    created: bool,
    dirty: bool,
    /// Byte offset of the caret in `stored.spec.text`.
    caret: usize,
    /// The other end of the selection. Equal to `caret` when nothing is
    /// selected, so the two together describe both states.
    anchor: usize,
    /// True while the layer's name is still the auto-generated one, so
    /// typing may keep updating it.
    name_is_auto: bool,
}

/// Does this char hang off the one before it, rather than standing alone?
///
/// Combining marks, zero-width joiners, variation selectors and skin-tone
/// modifiers all render as part of the previous character, so a caret step
/// has to cross them together with it.
fn is_continuation(ch: char) -> bool {
    matches!(ch as u32,
        0x0300..=0x036F      // combining diacritics
        | 0x200D             // zero-width joiner
        | 0xFE00..=0xFE0F    // variation selectors
        | 0x1F3FB..=0x1F3FF  // skin tone modifiers
    ) || matches!(ch as u32, 0x1AB0..=0x1AFF | 0x20D0..=0x20FF)
}

/// The styles the options bar offers, in the order Photoshop lists them.
const STYLES: &[&str] = &["Regular", "Bold", "Italic", "Bold Italic"];
const ALIGNMENTS: &[&str] = &["Left", "Center", "Right"];

/// Narrower than this and a drag was meant as a click.
const MIN_AREA_WIDTH: f32 = 8.0;

/// The widest wrap the options slider offers.
const MAX_AREA_WIDTH: f32 = 4000.0;

pub struct TypeTool {
    editing: Option<Editing>,
    /// What new text starts as, and what the options bar shows when
    /// nothing is being edited. Editing a layer adopts its spec, so the
    /// bar always describes the text you are looking at.
    spec: TextSpec,
    /// Whether the text being edited tracks the foreground swatch.
    ///
    /// `StoredText.color` was written once in `start_new` and never
    /// again -- `TextSpec` has no colour field, so nothing in the options
    /// bar could reach it. Set the foreground to red, place text, switch
    /// to blue and click back in: it stayed red, and the only way to
    /// change it was to delete and retype. On (the default) the edited
    /// text follows the swatch; off keeps whatever colour the layer
    /// already had, for editing wording without restyling it.
    follow_foreground: bool,
    /// A press that has not been released yet, which will make point
    /// text if it was a click and an area-text box if it was a drag.
    ///
    /// `TextSpec::wrap_width` existed and the engine honoured it, but
    /// nothing in the ui could set it, so every layer was point text and
    /// paragraph text was unreachable.
    pending: Option<(f32, f32)>,
    /// The Wrap slider's own value, and the only thing a fresh click
    /// inherits.
    ///
    /// `spec.wrap_width` also picks up the width of whatever layer is
    /// being edited, so it cannot double as the default for the next
    /// one: after a single area-text drag -- or just clicking into an
    /// existing box -- every later click made a box of that width.
    wrap_default: Option<f32>,
}

impl Default for TypeTool {
    fn default() -> Self {
        TypeTool {
            editing: None,
            spec: TextSpec::default(),
            follow_foreground: true,
            pending: None,
            wrap_default: None,
        }
    }
}

impl TypeTool {
    /// Take the current foreground swatch, if the text is set to follow
    /// it.
    ///
    /// This used to run only from `on_option_changed`, so the swatch
    /// could be changed and the text kept its old colour until some
    /// unrelated control was nudged -- and nudging one then recoloured a
    /// layer the user had only meant to resize. Every moment the tool
    /// touches a layer goes through here instead.
    fn adopt_foreground(&mut self, ctx: &mut ToolCtx) {
        if !self.follow_foreground {
            return;
        }
        let fg = ctx.state.foreground.to_u8();
        let Some(session) = &mut self.editing else {
            return;
        };
        if session.stored.color == fg {
            return;
        }
        session.stored.color = fg;
        session.dirty = true;
        self.refresh(ctx.doc);
    }

    /// Live-render the session's text into its layer without touching
    /// history (the whole session commits as one edit).
    fn refresh(&mut self, doc: &mut Document) {
        let Some(session) = &mut self.editing else {
            return;
        };
        let (tiles, bounds) = render_tiles(doc, &session.stored);
        let before = doc
            .tree
            .find(session.layer)
            .map(|l| l.content_bounds())
            .unwrap_or(IntRect::EMPTY);
        if let Some(layer) = doc.tree.find_mut(session.layer) {
            if let Some(raster) = layer.as_raster_mut() {
                raster.tiles = tiles;
            }
            // Only auto-name while the name still looks auto-generated.
            // Renaming a text layer to "Headline" and then editing its
            // text reverted the name on the next keystroke, and no
            // `LayerProps` op was recorded, so undo could not bring it
            // back either.
            if session.name_is_auto {
                layer.name = display_name(&session.stored.spec.text);
            }
            write_stored(layer, &session.stored);
        }
        doc.add_damage(before.union(&bounds));
    }

    fn start_new(&mut self, ctx: &mut ToolCtx, x: f32, y: f32) {
        let stored = StoredText {
            spec: TextSpec {
                text: String::new(),
                wrap_width: self.wrap_default,
                ..self.spec.clone()
            },
            origin: (x.round() as i32, y.round() as i32),
            color: ctx.state.foreground.to_u8(),
        };
        let mut layer = Layer::new_raster("Text");
        write_stored(&mut layer, &stored);
        let id = layer.id;
        let path = match ctx.doc.active_layer.and_then(|a| ctx.doc.tree.path_of(a)) {
            Some(mut p) => {
                *p.0.last_mut().unwrap() += 1;
                p
            }
            None => LayerPath(vec![ctx.doc.tree.layers.len()]),
        };
        let mut edit = ctx.doc.begin_edit("New Text Layer");
        edit.insert_layer(path, layer);
        edit.commit();
        ctx.doc.active_layer = Some(id);
        self.editing = Some(Editing {
            layer: id,
            stored,
            original: TileMap::new(),
            created: true,
            dirty: false,
            caret: 0,
            anchor: 0,
            name_is_auto: true,
        });
    }

    /// Pick an existing text layer under the cursor, if any.
    fn text_layer_at(doc: &Document, x: f32, y: f32) -> Option<(LayerId, StoredText)> {
        let (px, py) = (x.round() as i32, y.round() as i32);
        let mut hit = None;
        for layer in doc.tree.iter() {
            // Every other tool filters these; the type tool did not, so
            // clicking where a hidden text layer used to be silently
            // started editing it (caret over nothing, keystrokes changing
            // an invisible layer), and a locked one could be edited
            // despite the lock.
            if !layer.visible || layer.locked {
                continue;
            }
            let Some(stored) = read_stored(layer) else {
                continue;
            };
            if layer.tight_bounds().inflated(4).contains(px, py) {
                hit = Some((layer.id, stored));
            }
        }
        hit
    }
}

fn display_name(text: &str) -> String {
    let first: String = text.lines().next().unwrap_or("").chars().take(24).collect();
    if first.trim().is_empty() {
        "Text".to_string()
    } else {
        first
    }
}

impl ToolPlugin for TypeTool {
    fn id(&self) -> &'static str {
        "type"
    }

    fn editing_text(&self) -> Option<&str> {
        self.editing.as_ref().map(|s| s.stored.spec.text.as_str())
    }

    fn insert_text(&mut self, ctx: &mut ToolCtx, text: &str) -> bool {
        insert_text(self, ctx.doc, text)
    }

    fn take_text(&mut self, ctx: &mut ToolCtx) -> Option<String> {
        let taken = self.editing.as_ref()?.stored.spec.text.clone();
        if taken.is_empty() {
            return None;
        }
        clear_text(self, ctx.doc).then_some(taken)
    }
    fn name(&self) -> &'static str {
        "Type"
    }
    fn icon(&self) -> &'static str {
        "type"
    }
    fn shortcut(&self) -> Option<&'static str> {
        Some("t")
    }

    fn captures_keys(&self) -> bool {
        self.editing.is_some()
    }

    fn on_pointer_down(&mut self, ctx: &mut ToolCtx, input: PointerInput) {
        // Clicking away from the current text commits it first.
        if self.editing.is_some() {
            self.on_commit(ctx);
        }
        match Self::text_layer_at(ctx.doc, input.x, input.y) {
            Some((layer, stored)) => {
                let original = ctx
                    .doc
                    .tree
                    .find(layer)
                    .and_then(|l| l.as_raster())
                    .map(|r| r.tiles.clone())
                    .unwrap_or_default();
                ctx.doc.active_layer = Some(layer);
                // Show this layer's own type settings in the bar.
                self.spec = TextSpec {
                    text: String::new(),
                    ..stored.spec.clone()
                };
                let end = stored.spec.text.len();
                let name_is_auto = ctx
                    .doc
                    .tree
                    .find(layer)
                    .is_some_and(|l| l.name == display_name(&stored.spec.text));
                self.editing = Some(Editing {
                    layer,
                    stored,
                    original,
                    created: false,
                    dirty: false,
                    caret: end,
                    anchor: end,
                    name_is_auto,
                });
            }
            None => {
                // The layer is created here as it always was, so a click
                // and a keystroke still works with no release in
                // between. A drag turns it into an area-text box on
                // release.
                self.pending = Some((input.x, input.y));
                self.start_new(ctx, input.x, input.y);
            }
        }
        // Clicking into a layer is when the swatch takes effect: set the
        // foreground to red, place text, switch to blue and click back
        // in, and it turns blue.
        self.adopt_foreground(ctx);
    }

    fn on_pointer_move(&mut self, _ctx: &mut ToolCtx, _input: PointerInput) {}

    fn on_pointer_up(&mut self, ctx: &mut ToolCtx, input: PointerInput) {
        let Some((ax, _ay)) = self.pending.take() else {
            return;
        };
        // A drag defines the box paragraph text wraps inside; a click
        // leaves point text, which is all this could make before.
        let width = (input.x - ax).abs();
        if width < MIN_AREA_WIDTH {
            return;
        }
        let wrap = width.min(MAX_AREA_WIDTH);
        // The bar shows the box that was just drawn, but this width
        // belongs to this layer: it is not what the next click makes.
        self.spec.wrap_width = Some(wrap);
        if let Some(session) = &mut self.editing {
            session.stored.spec.wrap_width = Some(wrap);
            session.dirty = true;
            self.refresh(ctx.doc);
        }
    }

    fn on_key(
        &mut self,
        ctx: &mut ToolCtx,
        key: &str,
        text: Option<&str>,
        modifiers: Modifiers,
    ) -> bool {
        if self.editing.is_none() {
            return false;
        }
        let shift = modifiers.shift;
        let word = modifiers.ctrl_or_cmd;
        // What is typed next comes out in the current colour. Only for
        // keystrokes that actually put something in: an arrow key has no
        // business recolouring the layer.
        let inserts = !modifiers.ctrl_or_cmd
            && (matches!(key, "enter" | "tab" | "space")
                || text.is_some_and(|t| !t.is_empty() && !t.chars().any(char::is_control)));
        if inserts {
            self.adopt_foreground(ctx);
        }
        let mut changed = false;
        let mut handled = true;
        {
            let Some(session) = &mut self.editing else {
                return false;
            };
            match key {
                // Ctrl+A selects the text being edited, not the canvas.
                "a" if modifiers.ctrl_or_cmd => {
                    session.anchor = 0;
                    session.caret = session.stored.spec.text.len();
                }
                "left" | "right" | "up" | "down" | "home" | "end" => {
                    let at = session.caret;
                    let to = match key {
                        "left" if word => session.prev_word(at),
                        "right" if word => session.next_word(at),
                        // An unshifted arrow with a selection collapses to
                        // its edge rather than stepping past it.
                        "left" if session.has_selection() && !shift => session.selection().start,
                        "right" if session.has_selection() && !shift => session.selection().end,
                        "left" => session.prev_boundary(at),
                        "right" => session.next_boundary(at),
                        "up" => session.vertical(at, false),
                        "down" => session.vertical(at, true),
                        "home" => session.line_start(at),
                        _ => session.line_end(at),
                    };
                    session.caret = to;
                    if !shift {
                        session.anchor = to;
                    }
                }
                "backspace" => {
                    if !session.delete_selection() {
                        let to = if word {
                            session.prev_word(session.caret)
                        } else {
                            session.prev_boundary(session.caret)
                        };
                        if to != session.caret {
                            session
                                .stored
                                .spec
                                .text
                                .replace_range(to..session.caret, "");
                            session.caret = to;
                            session.anchor = to;
                        }
                    }
                    changed = true;
                }
                "delete" => {
                    if !session.delete_selection() {
                        let to = if word {
                            session.next_word(session.caret)
                        } else {
                            session.next_boundary(session.caret)
                        };
                        if to != session.caret {
                            session
                                .stored
                                .spec
                                .text
                                .replace_range(session.caret..to, "");
                        }
                    }
                    changed = true;
                }
                // Anything else with a modifier is a shortcut, not typing.
                _ if modifiers.ctrl_or_cmd => return false,
                "enter" => {
                    session.insert("\n");
                    changed = true;
                }
                "tab" => {
                    session.insert("    ");
                    changed = true;
                }
                "space" => {
                    session.insert(" ");
                    changed = true;
                }
                _ => match text {
                    Some(t) if !t.is_empty() && !t.chars().any(|c| c.is_control()) => {
                        session.insert(t);
                        changed = true;
                    }
                    _ => handled = false,
                },
            }
            if changed {
                session.dirty = true;
            }
        }
        if changed {
            self.refresh(ctx.doc);
        }
        if !handled {
            return false;
        }
        // Swallow every plain keystroke while typing so letters don't
        // switch tools mid-word.
        true
    }

    fn options(&self) -> Vec<ToolOption> {
        let families = schist_text_engine::family_names();
        let family = families
            .iter()
            .position(|f| *f == self.spec.family)
            .unwrap_or(0);
        vec![
            ToolOption::choice("type-family", "Font", families, family),
            ToolOption::choice(
                "type-style",
                "Style",
                STYLES,
                usize::from(self.spec.bold) | (usize::from(self.spec.italic) << 1),
            ),
            ToolOption::slider("type-size", "Size", self.spec.size, 6.0, 400.0, " px"),
            ToolOption::choice(
                "type-align",
                "Align",
                ALIGNMENTS,
                match self.spec.align {
                    Align::Left => 0,
                    Align::Center => 1,
                    Align::Right => 2,
                },
            ),
            ToolOption::slider(
                "type-leading",
                "Leading",
                self.spec.line_height,
                0.5,
                3.0,
                "\u{d7}",
            ),
            ToolOption::slider(
                "type-tracking",
                "Tracking",
                self.spec.tracking,
                -20.0,
                80.0,
                " px",
            ),
            ToolOption::toggle(
                "type-follow-fg",
                "Foreground Colour",
                self.follow_foreground,
            ),
            ToolOption::slider(
                "type-wrap",
                "Wrap",
                self.spec.wrap_width.unwrap_or(0.0),
                0.0,
                MAX_AREA_WIDTH,
                " px",
            ),
        ]
    }

    fn set_option(&mut self, key: &str, value: OptionValue) {
        match key {
            "type-family" => {
                if let Some(name) = schist_text_engine::family_names().get(value.index()) {
                    self.spec.family = (*name).to_string();
                }
            }
            "type-style" => {
                let i = value.index();
                self.spec.bold = i & 1 != 0;
                self.spec.italic = i & 2 != 0;
            }
            "type-size" => self.spec.size = value.num().clamp(6.0, 400.0),
            "type-align" => {
                self.spec.align = match value.index() {
                    1 => Align::Center,
                    2 => Align::Right,
                    _ => Align::Left,
                }
            }
            "type-leading" => self.spec.line_height = value.num().clamp(0.5, 3.0),
            "type-tracking" => self.spec.tracking = value.num(),
            "type-follow-fg" => self.follow_foreground = value.bool(),
            // Zero means "no box": point text, which is what every layer
            // was before there was any way to set this.
            "type-wrap" => {
                let w = value.num();
                self.spec.wrap_width = (w >= MIN_AREA_WIDTH).then_some(w.min(MAX_AREA_WIDTH));
                // Asked for by hand, so it is what the next new layer
                // starts as too.
                self.wrap_default = self.spec.wrap_width;
            }
            _ => {}
        }
    }

    /// Push the bar's settings onto the text being edited, so a font or
    /// size change shows up immediately rather than on the next click.
    fn on_option_changed(&mut self, ctx: &mut ToolCtx, _key: &str) {
        {
            let Some(session) = &mut self.editing else {
                return;
            };
            let text = std::mem::take(&mut session.stored.spec.text);
            session.stored.spec = TextSpec {
                text,
                ..self.spec.clone()
            };
            session.dirty = true;
        }
        self.refresh(ctx.doc);
        self.adopt_foreground(ctx);
    }

    fn on_commit(&mut self, ctx: &mut ToolCtx) {
        let Some(session) = self.editing.take() else {
            return;
        };
        if !session.dirty {
            // Nothing typed: drop an empty layer we created.
            if session.created {
                let mut edit = ctx.doc.begin_edit("Discard Empty Text");
                edit.remove_layer(session.layer);
                edit.commit();
                // The insert and this removal cancel out, so collapse the
                // pair rather than leaving two no-op steps in the panel.
                //
                // This used to call `undo()` twice and then `pop_redo()`
                // twice. Both were wrong: `undo()` unwinds whatever is on
                // top, which is only this layer's insert when nothing else
                // was committed in between, and `pop_redo()` is the redo
                // primitive, so it pushed the junk straight back onto the
                // undo stack. Clicking with the type tool and not typing
                // therefore reverted the user's last two real edits, with
                // the History panel still listing them as applied.
                ctx.doc
                    .history
                    .drop_cancelling_pair("Discard Empty Text", "New Text Layer");
            }
            return;
        }
        // Re-apply through the edit builder so undo restores the pre-edit
        // pixels in one step.
        let (tiles, _) = render_tiles(ctx.doc, &session.stored);
        if let Some(raster) = ctx
            .doc
            .tree
            .find_mut(session.layer)
            .and_then(|l| l.as_raster_mut())
        {
            raster.tiles = session.original.clone();
        }
        let mut edit = ctx.doc.begin_edit("Edit Text");
        edit.replace_layer_tiles(session.layer, tiles);
        edit.commit();
    }

    fn on_cancel(&mut self, ctx: &mut ToolCtx) {
        let Some(session) = self.editing.take() else {
            return;
        };
        let before = ctx
            .doc
            .tree
            .find(session.layer)
            .map(|l| l.content_bounds())
            .unwrap_or(IntRect::EMPTY);
        if let Some(raster) = ctx
            .doc
            .tree
            .find_mut(session.layer)
            .and_then(|l| l.as_raster_mut())
        {
            raster.tiles = session.original.clone();
        }
        ctx.doc.add_damage(before);
        if session.created {
            let mut edit = ctx.doc.begin_edit("Discard Text");
            edit.remove_layer(session.layer);
            edit.commit();
        }
    }

    fn on_deactivate(&mut self, ctx: &mut ToolCtx) {
        self.on_commit(ctx);
    }

    fn overlays(&self, doc: &Document, _state: &EditorState) -> Vec<Overlay> {
        let Some(session) = &self.editing else {
            return Vec::new();
        };
        let bounds = doc
            .tree
            .find(session.layer)
            .map(|l| l.tight_bounds())
            .unwrap_or(IntRect::EMPTY);
        // Layout coordinates are relative to the layout box's top-left,
        // which `render_tiles` places at `origin`, so the same offset maps
        // a caret onto the canvas. The old overlay measured x from the ink
        // bounds and stepped y by `size * line_height`, neither of which
        // is what the engine actually laid out.
        let (ox, oy) = session.origin_f32();
        let spec = &session.stored.spec;
        let mut out = Vec::new();
        if !bounds.is_empty() {
            out.push(Overlay::Rect(bounds.inflated(2)));
        }

        if session.has_selection() {
            let range = session.selection();
            for span in schist_text_engine::line_spans(spec) {
                let from = range.start.max(span.start);
                let to = range.end.min(span.end);
                if from >= to {
                    continue;
                }
                let left = schist_text_engine::caret_at(spec, from).map(|c| c.x);
                // At a line's end, measure the line rather than asking for
                // a caret there: on a wrapped line that offset also starts
                // the next line.
                let right = if to >= span.end {
                    Some(span.x + span.width)
                } else {
                    schist_text_engine::caret_at(spec, to).map(|c| c.x)
                };
                if let (Some(l), Some(r)) = (left, right) {
                    if r > l {
                        out.push(Overlay::Rect(IntRect::new(
                            (ox + l).floor() as i32,
                            (oy + span.top).floor() as i32,
                            (ox + r).ceil() as i32,
                            (oy + span.top + span.height).ceil() as i32,
                        )));
                    }
                }
            }
        }

        if let Some(caret) = schist_text_engine::caret_at(spec, session.caret) {
            let x = ox + caret.x;
            let y = oy + caret.top;
            out.push(Overlay::Line {
                x1: x,
                y1: y,
                x2: x,
                y2: y + caret.height,
            });
        }
        out
    }
}

impl Editing {
    /// The selected byte range, empty when the caret is a plain insertion
    /// point.
    fn selection(&self) -> std::ops::Range<usize> {
        let (a, b) = (self.caret.min(self.anchor), self.caret.max(self.anchor));
        a..b
    }

    fn has_selection(&self) -> bool {
        self.caret != self.anchor
    }

    fn text(&self) -> &str {
        &self.stored.spec.text
    }

    /// Collapse the selection, returning true if anything was removed.
    fn delete_selection(&mut self) -> bool {
        let range = self.selection();
        if range.is_empty() {
            return false;
        }
        self.stored.spec.text.replace_range(range.clone(), "");
        self.caret = range.start;
        self.anchor = range.start;
        true
    }

    /// Insert at the caret, replacing the selection first.
    fn insert(&mut self, s: &str) {
        self.delete_selection();
        let at = self.caret.min(self.stored.spec.text.len());
        self.stored.spec.text.insert_str(at, s);
        self.caret = at + s.len();
        self.anchor = self.caret;
    }

    /// Byte offset one grapheme before `at`.
    ///
    /// Whole-grapheme rather than whole-`char`, so backspacing an accented
    /// letter or an emoji removes what looks like one character instead of
    /// peeling off a combining mark at a time.
    fn prev_boundary(&self, at: usize) -> usize {
        let text = self.text();
        if at == 0 {
            return 0;
        }
        let mut i = at - 1;
        while i > 0 && !text.is_char_boundary(i) {
            i -= 1;
        }
        // Absorb any combining marks, zero-width joiners and variation
        // selectors that hang off the character before.
        while i > 0 {
            let Some(ch) = text[i..].chars().next() else {
                break;
            };
            if !is_continuation(ch) {
                break;
            }
            let mut j = i - 1;
            while j > 0 && !text.is_char_boundary(j) {
                j -= 1;
            }
            i = j;
        }
        i
    }

    /// Byte offset one grapheme after `at`.
    fn next_boundary(&self, at: usize) -> usize {
        let text = self.text();
        if at >= text.len() {
            return text.len();
        }
        let mut i = at + 1;
        while i < text.len() && !text.is_char_boundary(i) {
            i += 1;
        }
        while i < text.len() {
            let Some(ch) = text[i..].chars().next() else {
                break;
            };
            if !is_continuation(ch) {
                break;
            }
            i += ch.len_utf8();
        }
        i
    }

    /// Start of the word at or before `at`.
    fn prev_word(&self, at: usize) -> usize {
        let text = self.text();
        let mut i = at;
        while i > 0 {
            let p = self.prev_boundary(i);
            let ch = text[p..].chars().next().unwrap_or(' ');
            if !ch.is_whitespace() {
                break;
            }
            i = p;
        }
        while i > 0 {
            let p = self.prev_boundary(i);
            let ch = text[p..].chars().next().unwrap_or(' ');
            if ch.is_whitespace() {
                break;
            }
            i = p;
        }
        i
    }

    /// End of the word at or after `at`.
    fn next_word(&self, at: usize) -> usize {
        let text = self.text();
        let mut i = at;
        while i < text.len() {
            let ch = text[i..].chars().next().unwrap_or(' ');
            if !ch.is_whitespace() {
                break;
            }
            i = self.next_boundary(i);
        }
        while i < text.len() {
            let ch = text[i..].chars().next().unwrap_or(' ');
            if ch.is_whitespace() {
                break;
            }
            i = self.next_boundary(i);
        }
        i
    }

    /// Start of the source line containing `at`.
    fn line_start(&self, at: usize) -> usize {
        self.text()[..at].rfind('\n').map(|i| i + 1).unwrap_or(0)
    }

    /// End of the source line containing `at`.
    fn line_end(&self, at: usize) -> usize {
        self.text()[at..]
            .find('\n')
            .map(|i| at + i)
            .unwrap_or(self.text().len())
    }

    /// Move to the same column one line up or down, clamped to that
    /// line's length.
    fn vertical(&self, at: usize, down: bool) -> usize {
        let column = at - self.line_start(at);
        if down {
            let end = self.line_end(at);
            if end >= self.text().len() {
                return at;
            }
            let next_start = end + 1;
            let next_end = self.line_end(next_start);
            let mut target = next_start + column;
            if target > next_end {
                target = next_end;
            }
            while target > next_start && !self.text().is_char_boundary(target) {
                target -= 1;
            }
            target
        } else {
            let start = self.line_start(at);
            if start == 0 {
                return at;
            }
            let prev_start = self.line_start(start - 1);
            let prev_end = start - 1;
            let mut target = prev_start + column;
            if target > prev_end {
                target = prev_end;
            }
            while target > prev_start && !self.text().is_char_boundary(target) {
                target -= 1;
            }
            target
        }
    }

    fn origin_f32(&self) -> (f32, f32) {
        (self.stored.origin.0 as f32, self.stored.origin.1 as f32)
    }
}

/// Set the alignment of the layer currently being edited (used by the tool
/// options bar).
pub fn set_align(tool: &mut TypeTool, doc: &mut Document, align: Align) {
    if let Some(session) = &mut tool.editing {
        session.stored.spec.align = align;
        session.dirty = true;
    }
    tool.refresh(doc);
}

/// Set the font size of the layer currently being edited.
pub fn set_size(tool: &mut TypeTool, doc: &mut Document, size: f32) {
    if let Some(session) = &mut tool.editing {
        session.stored.spec.size = size.clamp(4.0, 800.0);
        session.dirty = true;
    }
    tool.refresh(doc);
}

/// Family of the text layer being edited, if any.
pub fn editing_family(tool: &TypeTool) -> Option<String> {
    tool.editing.as_ref().map(|e| e.stored.spec.family.clone())
}

/// Set the font family of the layer currently being edited.
pub fn set_family(tool: &mut TypeTool, doc: &mut Document, family: String) {
    if let Some(session) = &mut tool.editing {
        session.stored.spec.family = family;
        session.dirty = true;
    }
    tool.refresh(doc);
}

/// True while a text layer is open for editing.
pub fn is_editing(tool: &TypeTool) -> bool {
    tool.editing.is_some()
}

/// The text of the layer being edited, if one is.
pub fn current_text(tool: &TypeTool) -> Option<&str> {
    tool.editing.as_ref().map(|s| s.stored.spec.text.as_str())
}

/// Append text to the layer being edited and re-render.
///
/// The clipboard held pixels and nothing else, so ctrl-V while typing
/// pasted a "Pasted Layer" of pixels on top of the text instead of the
/// string that had been copied.
pub fn insert_text(tool: &mut TypeTool, doc: &mut Document, text: &str) -> bool {
    let Some(session) = &mut tool.editing else {
        return false;
    };
    if text.is_empty() {
        return false;
    }
    // Newlines and tabs are meaningful -- tabs advance to the next stop,
    // so stripping them collapsed the columns of anything pasted out of
    // a PSD-imported layer. Other control characters are not.
    let cleaned: String = text
        .chars()
        .filter(|c| *c == '\n' || *c == '\t' || !c.is_control())
        .collect();
    if cleaned.is_empty() {
        return false;
    }
    session.stored.spec.text.push_str(&cleaned);
    session.dirty = true;
    tool.refresh(doc);
    true
}

/// Replace the edited layer's text, for a cut.
pub fn clear_text(tool: &mut TypeTool, doc: &mut Document) -> bool {
    let Some(session) = &mut tool.editing else {
        return false;
    };
    if session.stored.spec.text.is_empty() {
        return false;
    }
    session.stored.spec.text.clear();
    session.dirty = true;
    tool.refresh(doc);
    true
}

pub struct TypeToolsPlugin;

impl PluginManifest for TypeToolsPlugin {
    fn id(&self) -> &'static str {
        "schist.tools-type"
    }

    fn register(&self, registry: &mut PluginRegistry) {
        registry.register_tool(Box::new(TypeTool::default()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use schist_color::Depth;

    fn doc() -> Document {
        let mut d = Document::new("t", 300, 200, Depth::Eight);
        d.push_layer(Layer::new_raster("bg"));
        d
    }

    fn input(x: f32, y: f32) -> PointerInput {
        PointerInput {
            x,
            y,
            pressure: 1.0,
            modifiers: Modifiers::default(),
        }
    }

    fn type_text(tool: &mut TypeTool, ctx: &mut ToolCtx, s: &str) {
        for ch in s.chars() {
            let text = ch.to_string();
            let key = if ch == ' ' { "space" } else { &text };
            tool.on_key(ctx, key, Some(&text), Modifiers::default());
        }
    }

    #[test]
    fn the_options_bar_settings_reach_the_text() {
        let mut d = doc();
        let mut state = schist_plugin_api::EditorState::default();
        let mut tool = TypeTool::default();
        let mut ctx = ToolCtx {
            doc: &mut d,
            state: &mut state,
        };
        tool.on_pointer_down(&mut ctx, input(20.0, 60.0));
        type_text(&mut tool, &mut ctx, "Hi");

        // Size, alignment and style all land on the live session.
        tool.set_option("type-size", OptionValue::Num(96.0));
        tool.set_option("type-align", OptionValue::Choice(2));
        tool.set_option("type-style", OptionValue::Choice(3));
        tool.on_option_changed(&mut ctx, "type-size");

        let session = tool.editing.as_ref().expect("still editing");
        assert_eq!(session.stored.spec.size, 96.0);
        assert_eq!(session.stored.spec.align, Align::Right);
        assert!(session.stored.spec.bold && session.stored.spec.italic);
        assert_eq!(session.stored.spec.text, "Hi", "the typing survives");
    }

    #[test]
    fn a_bigger_size_renders_bigger_text() {
        let render = |size: f32| {
            let mut d = doc();
            let mut state = schist_plugin_api::EditorState::default();
            let mut tool = TypeTool::default();
            tool.set_option("type-size", OptionValue::Num(size));
            let mut ctx = ToolCtx {
                doc: &mut d,
                state: &mut state,
            };
            tool.on_pointer_down(&mut ctx, input(20.0, 120.0));
            type_text(&mut tool, &mut ctx, "AB");
            let layer = tool.editing.as_ref().unwrap().layer;
            d.tree.find(layer).unwrap().tight_bounds().width()
        };
        let small = render(16.0);
        let large = render(64.0);
        assert!(small > 0, "the small text drew something");
        assert!(
            large > small * 2,
            "64px text should be far wider than 16px: {small} vs {large}"
        );
    }

    #[test]
    fn editing_an_existing_layer_adopts_its_settings() {
        let mut d = doc();
        let mut state = schist_plugin_api::EditorState::default();
        let mut tool = TypeTool::default();
        {
            let mut ctx = ToolCtx {
                doc: &mut d,
                state: &mut state,
            };
            tool.set_option("type-size", OptionValue::Num(72.0));
            tool.on_pointer_down(&mut ctx, input(20.0, 60.0));
            type_text(&mut tool, &mut ctx, "X");
            tool.on_commit(&mut ctx);

            // A new layer somewhere else, at a different size.
            tool.set_option("type-size", OptionValue::Num(20.0));
            tool.on_pointer_down(&mut ctx, input(200.0, 160.0));
            type_text(&mut tool, &mut ctx, "Y");
            tool.on_commit(&mut ctx);

            // Clicking back into the first one should show 72 again.
            // Aim at the glyph itself: hit-testing uses the inked bounds,
            // which sit below the origin you clicked to place the text.
            tool.on_pointer_down(&mut ctx, input(40.0, 100.0));
        }
        let shown = tool
            .options()
            .into_iter()
            .find(|o| o.key == "type-size")
            .expect("a size option")
            .value
            .num();
        assert_eq!(shown, 72.0, "the bar should describe the text you clicked");
    }

    fn ink(doc: &Document) -> usize {
        doc.tree
            .layers
            .last()
            .unwrap()
            .as_raster()
            .unwrap()
            .tiles
            .iter()
            .map(|(_, buf)| {
                (0..schist_core::TILE_PIXELS)
                    .filter(|&i| buf.get(i).a > 0.0)
                    .count()
            })
            .sum()
    }

    #[test]
    fn typing_creates_a_text_layer_with_pixels() {
        let mut doc = doc();
        let mut state = EditorState::default();
        let mut tool = TypeTool::default();
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        tool.on_pointer_down(&mut ctx, input(20.0, 60.0));
        type_text(&mut tool, &mut ctx, "Hello");
        tool.on_commit(&mut ctx);

        assert_eq!(doc.tree.layers.len(), 2);
        assert!(ink(&doc) > 20, "text drew pixels");
        assert_eq!(doc.tree.layers[1].name, "Hello");
    }

    #[test]
    fn text_spec_is_preserved_on_the_layer() {
        let mut doc = doc();
        let mut state = EditorState::default();
        let mut tool = TypeTool::default();
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        tool.on_pointer_down(&mut ctx, input(10.0, 40.0));
        type_text(&mut tool, &mut ctx, "Hi");
        tool.on_commit(&mut ctx);

        let stored = read_stored(&doc.tree.layers[1]).expect("spec stored in extras");
        assert_eq!(stored.spec.text, "Hi");
        assert_eq!(stored.origin, (10, 40));
    }

    #[test]
    fn clicking_an_existing_text_layer_resumes_editing() {
        let mut doc = doc();
        let mut state = EditorState::default();
        let mut tool = TypeTool::default();
        {
            let mut ctx = ToolCtx {
                doc: &mut doc,
                state: &mut state,
            };
            tool.on_pointer_down(&mut ctx, input(20.0, 60.0));
            type_text(&mut tool, &mut ctx, "AB");
            tool.on_commit(&mut ctx);
        }
        let layers_before = doc.tree.layers.len();
        let bounds = doc.tree.layers[1].tight_bounds();

        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        tool.on_pointer_down(
            &mut ctx,
            input(bounds.left as f32 + 2.0, bounds.top as f32 + 2.0),
        );
        assert!(is_editing(&tool), "resumed editing the existing layer");
        type_text(&mut tool, &mut ctx, "C");
        tool.on_commit(&mut ctx);

        assert_eq!(doc.tree.layers.len(), layers_before, "no new layer");
        assert_eq!(read_stored(&doc.tree.layers[1]).unwrap().spec.text, "ABC");
    }

    #[test]
    fn backspace_deletes_and_undo_restores_previous_text() {
        let mut doc = doc();
        let mut state = EditorState::default();
        let mut tool = TypeTool::default();
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        tool.on_pointer_down(&mut ctx, input(20.0, 60.0));
        type_text(&mut tool, &mut ctx, "Hey");
        tool.on_key(&mut ctx, "backspace", None, Modifiers::default());
        tool.on_commit(&mut ctx);
        assert_eq!(read_stored(&doc.tree.layers[1]).unwrap().spec.text, "He");

        let with_text = ink(&doc);
        doc.undo(); // "Edit Text"
        assert!(ink(&doc) < with_text, "undo removed the rendered glyphs");
    }

    #[test]
    fn escape_discards_a_new_empty_layer() {
        let mut doc = doc();
        let mut state = EditorState::default();
        let mut tool = TypeTool::default();
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        tool.on_pointer_down(&mut ctx, input(20.0, 60.0));
        type_text(&mut tool, &mut ctx, "oops");
        tool.on_cancel(&mut ctx);
        assert_eq!(doc.tree.layers.len(), 1, "cancelled layer is gone");
    }

    #[test]
    fn modifier_keys_are_not_swallowed() {
        let mut doc = doc();
        let mut state = EditorState::default();
        let mut tool = TypeTool::default();
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        tool.on_pointer_down(&mut ctx, input(20.0, 60.0));
        let consumed = tool.on_key(
            &mut ctx,
            "z",
            Some("z"),
            Modifiers {
                ctrl_or_cmd: true,
                ..Default::default()
            },
        );
        assert!(!consumed, "ctrl-z must reach the keymap");
    }

    #[test]
    fn newline_grows_the_layer_downwards() {
        let mut doc = doc();
        let mut state = EditorState::default();
        let mut tool = TypeTool::default();
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        tool.on_pointer_down(&mut ctx, input(20.0, 40.0));
        type_text(&mut tool, &mut ctx, "A");
        let one_line = doc.tree.layers[1].tight_bounds().height();
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        tool.on_key(&mut ctx, "enter", None, Modifiers::default());
        type_text(&mut tool, &mut ctx, "B");
        let two_lines = doc.tree.layers[1].tight_bounds().height();
        assert!(two_lines > one_line + 10, "{two_lines} vs {one_line}");
    }
    #[test]
    fn clicking_without_typing_leaves_earlier_edits_alone() {
        // The data loss. `on_commit` on an untouched session called
        // `undo()` twice, which unwinds whatever is on top rather than
        // this layer's own insert. The two only line up when nothing was
        // committed in between; a command run mid-session (which is
        // exactly what a shortcut does, since running one does not commit
        // the pending tool session) puts a real edit on top instead.
        let mut d = doc();
        let mut state = schist_plugin_api::EditorState::default();
        let mut tool = TypeTool::default();

        {
            let mut ctx = ToolCtx {
                doc: &mut d,
                state: &mut state,
            };
            tool.on_pointer_down(&mut ctx, input(20.0, 60.0));
        }

        // A real edit committed while the empty session is still open.
        {
            let mut edit = d.begin_edit("Important Edit");
            edit.insert_layer(LayerPath(vec![0]), Layer::new_raster("important"));
            edit.commit();
        }
        assert!(d.tree.iter().any(|l| l.name == "important"));

        // Now click away without having typed anything.
        {
            let mut ctx = ToolCtx {
                doc: &mut d,
                state: &mut state,
            };
            tool.on_commit(&mut ctx);
        }

        let after: Vec<String> = d.tree.iter().map(|l| l.name.clone()).collect();
        assert!(
            after.iter().any(|n| n == "important"),
            "the edit made during the session must survive: {after:?}"
        );
        assert!(
            d.history
                .entries()
                .iter()
                .any(|e| e.name == "Important Edit"),
            "and it must still be in history"
        );
    }

    #[test]
    fn a_user_set_layer_name_survives_editing_its_text() {
        // `refresh` reset the name to the first line of the text on every
        // keystroke, outside any edit, so a name the user had set was
        // reverted by typing and undo could not bring it back: no
        // `LayerProps` op was ever recorded for it.
        let mut d = doc();
        let mut state = schist_plugin_api::EditorState::default();
        let mut tool = TypeTool::default();
        {
            let mut ctx = ToolCtx {
                doc: &mut d,
                state: &mut state,
            };
            tool.on_pointer_down(&mut ctx, input(20.0, 60.0));
            type_text(&mut tool, &mut ctx, "Hi");
        }
        let id = tool.editing.as_ref().unwrap().layer;

        // The user renames the layer, which makes the name no longer the
        // auto-generated one.
        d.tree.find_mut(id).unwrap().name = "Headline".into();
        tool.editing.as_mut().unwrap().name_is_auto = false;

        {
            let mut ctx = ToolCtx {
                doc: &mut d,
                state: &mut state,
            };
            type_text(&mut tool, &mut ctx, "!");
        }
        assert_eq!(
            d.tree.find(id).unwrap().name,
            "Headline",
            "typing must not overwrite a name the user set"
        );
    }

    #[test]
    fn discarding_an_empty_layer_leaves_no_junk_history() {
        let mut d = doc();
        let mut state = schist_plugin_api::EditorState::default();
        let mut tool = TypeTool::default();
        let before = d.history.entries().len();
        {
            let mut ctx = ToolCtx {
                doc: &mut d,
                state: &mut state,
            };
            tool.on_pointer_down(&mut ctx, input(20.0, 60.0));
            tool.on_commit(&mut ctx);
        }
        assert_eq!(
            d.history.entries().len(),
            before,
            "the insert and its removal should collapse"
        );
    }

    fn ctrl() -> Modifiers {
        Modifiers {
            ctrl_or_cmd: true,
            ..Modifiers::default()
        }
    }

    fn shift() -> Modifiers {
        Modifiers {
            shift: true,
            ..Modifiers::default()
        }
    }

    /// Type `s`, sending Enter for newlines the way a keyboard does.
    fn type_lines(tool: &mut TypeTool, ctx: &mut ToolCtx, s: &str) {
        for ch in s.chars() {
            let text = ch.to_string();
            match ch {
                '\n' => {
                    tool.on_key(ctx, "enter", None, Modifiers::default());
                }
                ' ' => {
                    tool.on_key(ctx, "space", Some(" "), Modifiers::default());
                }
                _ => {
                    tool.on_key(ctx, &text, Some(&text), Modifiers::default());
                }
            }
        }
    }

    /// A tool mid-session with `s` typed into it.
    fn editing(d: &mut Document, s: &str) -> TypeTool {
        let mut state = schist_plugin_api::EditorState::default();
        let mut tool = TypeTool::default();
        let mut ctx = ToolCtx {
            doc: d,
            state: &mut state,
        };
        tool.on_pointer_down(&mut ctx, input(20.0, 60.0));
        type_lines(&mut tool, &mut ctx, s);
        tool
    }

    fn key(tool: &mut TypeTool, d: &mut Document, k: &str, m: Modifiers) -> bool {
        let mut state = schist_plugin_api::EditorState::default();
        let mut ctx = ToolCtx {
            doc: d,
            state: &mut state,
        };
        tool.on_key(&mut ctx, k, None, m)
    }

    #[test]
    fn ctrl_a_selects_the_text_not_the_canvas() {
        // The reported bug. The binding change stops the canvas Select All
        // running; this is the half that makes the keystroke do the right
        // thing once it arrives.
        let mut d = doc();
        let mut tool = editing(&mut d, "hello");
        assert!(key(&mut tool, &mut d, "a", ctrl()), "must be consumed");
        let s = tool.editing.as_ref().unwrap();
        assert_eq!(s.selection(), 0..5);
        assert!(s.has_selection());
    }

    #[test]
    fn typing_over_a_selection_replaces_it() {
        let mut d = doc();
        let mut tool = editing(&mut d, "hello");
        key(&mut tool, &mut d, "a", ctrl());
        let mut state = schist_plugin_api::EditorState::default();
        let mut ctx = ToolCtx {
            doc: &mut d,
            state: &mut state,
        };
        tool.on_key(&mut ctx, "x", Some("x"), Modifiers::default());
        assert_eq!(tool.editing.as_ref().unwrap().text(), "x");
    }

    #[test]
    fn the_caret_moves_and_inserts_in_the_middle() {
        let mut d = doc();
        let mut tool = editing(&mut d, "ac");
        key(&mut tool, &mut d, "left", Modifiers::default());
        let mut state = schist_plugin_api::EditorState::default();
        let mut ctx = ToolCtx {
            doc: &mut d,
            state: &mut state,
        };
        tool.on_key(&mut ctx, "b", Some("b"), Modifiers::default());
        assert_eq!(tool.editing.as_ref().unwrap().text(), "abc");
    }

    #[test]
    fn home_and_end_reach_the_line_edges() {
        let mut d = doc();
        let mut tool = editing(&mut d, "one\ntwo");
        key(&mut tool, &mut d, "home", Modifiers::default());
        assert_eq!(tool.editing.as_ref().unwrap().caret, 4);
        key(&mut tool, &mut d, "end", Modifiers::default());
        assert_eq!(tool.editing.as_ref().unwrap().caret, 7);
    }

    #[test]
    fn shift_arrow_extends_a_selection_and_a_plain_arrow_collapses_it() {
        let mut d = doc();
        let mut tool = editing(&mut d, "abcd");
        key(&mut tool, &mut d, "left", shift());
        key(&mut tool, &mut d, "left", shift());
        assert_eq!(tool.editing.as_ref().unwrap().selection(), 2..4);
        key(&mut tool, &mut d, "left", Modifiers::default());
        let s = tool.editing.as_ref().unwrap();
        assert!(!s.has_selection());
        assert_eq!(s.caret, 2, "collapses to the selection's near edge");
    }

    #[test]
    fn delete_forward_and_backspace_remove_one_character_each() {
        let mut d = doc();
        let mut tool = editing(&mut d, "abc");
        key(&mut tool, &mut d, "left", Modifiers::default());
        key(&mut tool, &mut d, "backspace", Modifiers::default());
        assert_eq!(tool.editing.as_ref().unwrap().text(), "ac");
        key(&mut tool, &mut d, "delete", Modifiers::default());
        assert_eq!(tool.editing.as_ref().unwrap().text(), "a");
    }

    #[test]
    fn backspace_removes_a_whole_grapheme() {
        // "e" + combining acute looks like one letter, so one backspace
        // should remove it, not peel off the accent.
        let mut d = doc();
        let mut tool = editing(&mut d, "x");
        {
            let s = tool.editing.as_mut().unwrap();
            s.stored.spec.text = "e\u{301}".into();
            s.caret = s.stored.spec.text.len();
            s.anchor = s.caret;
        }
        key(&mut tool, &mut d, "backspace", Modifiers::default());
        assert_eq!(tool.editing.as_ref().unwrap().text(), "");
    }

    #[test]
    fn ctrl_arrow_jumps_by_word() {
        let mut d = doc();
        let mut tool = editing(&mut d, "one two three");
        key(&mut tool, &mut d, "left", ctrl());
        assert_eq!(tool.editing.as_ref().unwrap().caret, 8, "start of 'three'");
        key(&mut tool, &mut d, "left", ctrl());
        assert_eq!(tool.editing.as_ref().unwrap().caret, 4, "start of 'two'");
    }

    #[test]
    fn up_and_down_keep_the_column() {
        let mut d = doc();
        let mut tool = editing(&mut d, "long line\nab");
        // Caret is at the end of "ab" (column 2).
        key(&mut tool, &mut d, "up", Modifiers::default());
        assert_eq!(
            tool.editing.as_ref().unwrap().caret,
            2,
            "column 2 of line 1"
        );
        key(&mut tool, &mut d, "down", Modifiers::default());
        assert_eq!(tool.editing.as_ref().unwrap().caret, 12, "back to column 2");
    }

    #[test]
    fn an_unhandled_shortcut_is_not_swallowed() {
        // ctrl+s must still reach the app; only ctrl+a is ours.
        let mut d = doc();
        let mut tool = editing(&mut d, "hi");
        assert!(!key(&mut tool, &mut d, "s", ctrl()));
    }

    #[test]
    fn an_auto_named_layer_still_follows_its_text() {
        // The other direction: while the name is still the generated one,
        // it should keep tracking what is typed.
        let mut d = doc();
        let mut state = schist_plugin_api::EditorState::default();
        let mut tool = TypeTool::default();
        let mut ctx = ToolCtx {
            doc: &mut d,
            state: &mut state,
        };
        tool.on_pointer_down(&mut ctx, input(20.0, 60.0));
        type_text(&mut tool, &mut ctx, "Hi");
        let id = tool.editing.as_ref().unwrap().layer;
        assert_eq!(ctx.doc.tree.find(id).unwrap().name, "Hi");
    }

    #[test]
    fn a_hidden_or_locked_text_layer_is_not_entered() {
        // Every other tool filters these. Clicking where a hidden text
        // layer used to be silently started editing it.
        let mut d = doc();
        let mut state = schist_plugin_api::EditorState::default();
        let mut tool = TypeTool::default();
        {
            let mut ctx = ToolCtx {
                doc: &mut d,
                state: &mut state,
            };
            tool.on_pointer_down(&mut ctx, input(20.0, 60.0));
            type_text(&mut tool, &mut ctx, "Hi");
            tool.on_commit(&mut ctx);
        }
        let id = d.tree.iter().find(|l| l.name == "Hi").unwrap().id;
        let layers_before = d.tree.len();
        let b = d.tree.find(id).unwrap().tight_bounds();
        d.tree.find_mut(id).unwrap().visible = false;

        {
            let mut ctx = ToolCtx {
                doc: &mut d,
                state: &mut state,
            };
            // Clicking the hidden layer must start a *new* layer instead
            // of resuming the invisible one.
            tool.on_pointer_down(&mut ctx, input(b.left as f32 + 2.0, b.top as f32 + 2.0));
        }
        let session = tool.editing.as_ref().expect("a session started");
        assert_ne!(session.layer, id, "must not resume the hidden layer");
        assert!(d.tree.len() > layers_before, "a new layer was created");
    }

    /// `StoredText.color` was written once in `start_new` and never
    /// again: `TextSpec` has no colour field, so nothing in the options
    /// bar could reach it. Set the foreground to red, place text, switch
    /// to blue and click back in — it stayed red, and the only way to
    /// change it was to delete and retype.
    #[test]
    fn the_edited_text_follows_the_foreground_swatch() {
        let mut d = doc();
        let mut state = schist_plugin_api::EditorState {
            foreground: schist_color::Rgba::new(1.0, 0.0, 0.0, 1.0),
            ..Default::default()
        };
        let mut tool = TypeTool::default();
        {
            let mut ctx = ToolCtx {
                doc: &mut d,
                state: &mut state,
            };
            tool.on_pointer_down(&mut ctx, input(20.0, 60.0));
            tool.on_pointer_up(&mut ctx, input(20.0, 60.0));
            type_text(&mut tool, &mut ctx, "Hi");
            assert_eq!(
                tool.editing.as_ref().unwrap().stored.color,
                [255, 0, 0, 255]
            );
        }

        state.foreground = schist_color::Rgba::new(0.0, 0.0, 1.0, 1.0);
        let mut ctx = ToolCtx {
            doc: &mut d,
            state: &mut state,
        };
        tool.set_option("type-size", OptionValue::Num(40.0));
        tool.on_option_changed(&mut ctx, "type-size");
        assert_eq!(
            tool.editing.as_ref().unwrap().stored.color,
            [0, 0, 255, 255],
            "the text did not pick up the new foreground"
        );
    }

    /// And turning the toggle off keeps the layer's own colour, for
    /// editing the wording without restyling it.
    #[test]
    fn the_colour_can_be_pinned_to_the_layer() {
        let mut d = doc();
        let mut state = schist_plugin_api::EditorState {
            foreground: schist_color::Rgba::new(1.0, 0.0, 0.0, 1.0),
            ..Default::default()
        };
        let mut tool = TypeTool::default();
        let mut ctx = ToolCtx {
            doc: &mut d,
            state: &mut state,
        };
        tool.on_pointer_down(&mut ctx, input(20.0, 60.0));
        tool.on_pointer_up(&mut ctx, input(20.0, 60.0));
        type_text(&mut tool, &mut ctx, "Hi");

        tool.set_option("type-follow-fg", OptionValue::Bool(false));
        ctx.state.foreground = schist_color::Rgba::new(0.0, 0.0, 1.0, 1.0);
        tool.set_option("type-size", OptionValue::Num(40.0));
        tool.on_option_changed(&mut ctx, "type-size");
        assert_eq!(
            tool.editing.as_ref().unwrap().stored.color,
            [255, 0, 0, 255]
        );
    }

    /// Installing a font re-rendered every layer set in it by assigning
    /// straight onto the raster — no `begin_edit`, so the change was not
    /// undoable and the document was not even marked dirty, meaning it
    /// could be closed without a save prompt.
    #[test]
    fn a_font_re_render_is_one_undoable_edit() {
        let mut d = doc();
        let mut state = schist_plugin_api::EditorState::default();
        let mut tool = TypeTool::default();
        let family = {
            let mut ctx = ToolCtx {
                doc: &mut d,
                state: &mut state,
            };
            tool.on_pointer_down(&mut ctx, input(20.0, 60.0));
            tool.on_pointer_up(&mut ctx, input(20.0, 60.0));
            type_text(&mut tool, &mut ctx, "Hi");
            let family = tool.editing.as_ref().unwrap().stored.spec.family.clone();
            tool.on_commit(&mut ctx);
            family
        };
        d.dirty = false;
        let steps_before = d.history.undo_name().map(String::from);

        let changed = rerender_family(&mut d, &family);
        assert_eq!(changed, 1, "the text layer should have been re-rendered");
        assert_eq!(d.history.undo_name(), Some("Update Fonts"));
        assert!(d.dirty, "a re-render is an unsaved change");

        d.undo();
        assert_eq!(
            d.history.undo_name().map(String::from),
            steps_before,
            "the re-render left more than one entry"
        );
    }

    /// A family nothing is set in changes nothing, and records nothing.
    #[test]
    fn a_font_nothing_uses_records_no_edit() {
        let mut d = doc();
        assert_eq!(rerender_family(&mut d, "Definitely Not Installed"), 0);
        assert!(!d.history.can_undo());
    }

    /// Copy, cut and paste meant pixels and nothing else: ctrl-V while
    /// typing into a text layer pasted a "Pasted Layer" of pixels on top
    /// of the text rather than the string that had been copied.
    #[test]
    fn the_type_tool_is_a_text_sink() {
        let mut d = doc();
        let mut state = schist_plugin_api::EditorState::default();
        let mut tool = TypeTool::default();
        let mut ctx = ToolCtx {
            doc: &mut d,
            state: &mut state,
        };

        // Not editing: the tool is not a text sink, so the pixel
        // clipboard keeps working.
        assert_eq!(tool.editing_text(), None);
        assert!(!tool.insert_text(&mut ctx, "hello"));
        assert_eq!(tool.take_text(&mut ctx), None);

        tool.on_pointer_down(&mut ctx, input(20.0, 60.0));
        tool.on_pointer_up(&mut ctx, input(20.0, 60.0));
        type_text(&mut tool, &mut ctx, "Hi");
        assert_eq!(tool.editing_text(), Some("Hi"));

        assert!(tool.insert_text(&mut ctx, " there"));
        assert_eq!(tool.editing_text(), Some("Hi there"));

        // Newlines survive a paste; other control characters do not.
        assert!(tool.insert_text(&mut ctx, "\nline\u{7}two"));
        assert_eq!(tool.editing_text(), Some("Hi there\nlinetwo"));

        assert_eq!(
            tool.take_text(&mut ctx).as_deref(),
            Some("Hi there\nlinetwo")
        );
        assert_eq!(tool.editing_text(), Some(""));
        // Nothing left to cut.
        assert_eq!(tool.take_text(&mut ctx), None);
    }

    /// Pasting nothing leaves the text alone rather than marking the
    /// session dirty for no reason.
    #[test]
    fn pasting_nothing_changes_nothing() {
        let mut d = doc();
        let mut state = schist_plugin_api::EditorState::default();
        let mut tool = TypeTool::default();
        let mut ctx = ToolCtx {
            doc: &mut d,
            state: &mut state,
        };
        tool.on_pointer_down(&mut ctx, input(20.0, 60.0));
        tool.on_pointer_up(&mut ctx, input(20.0, 60.0));
        type_text(&mut tool, &mut ctx, "Hi");

        assert!(!tool.insert_text(&mut ctx, ""));
        assert!(!tool.insert_text(&mut ctx, "\u{7}\u{1b}"));
        assert_eq!(tool.editing_text(), Some("Hi"));
    }

    /// `TextSpec::wrap_width` existed and the engine honoured it, but
    /// nothing in the ui could set it, so every layer was point text and
    /// paragraph text was unreachable.
    #[test]
    fn dragging_the_type_tool_makes_an_area_text_box() {
        let mut d = doc();
        let mut state = schist_plugin_api::EditorState::default();
        let mut tool = TypeTool::default();
        let mut ctx = ToolCtx {
            doc: &mut d,
            state: &mut state,
        };

        tool.on_pointer_down(&mut ctx, input(20.0, 60.0));
        tool.on_pointer_up(&mut ctx, input(140.0, 100.0));
        let wrap = tool.editing.as_ref().unwrap().stored.spec.wrap_width;
        assert_eq!(wrap, Some(120.0), "the drag did not set a wrap box");

        // And the text wraps inside it: a long line becomes several.
        type_text(&mut tool, &mut ctx, "wrapping text needs several words");
        let tall = layer_height(ctx.doc);
        tool.set_option("type-wrap", OptionValue::Num(0.0));
        tool.on_option_changed(&mut ctx, "type-wrap");
        let flat = layer_height(ctx.doc);
        assert!(
            tall > flat,
            "wrapped text should be taller than one line: {tall} vs {flat}"
        );
    }

    /// A click still makes point text, and does so on the press, so a
    /// click and a keystroke with no release in between still works.
    #[test]
    fn clicking_still_makes_point_text() {
        let mut d = doc();
        let mut state = schist_plugin_api::EditorState::default();
        let mut tool = TypeTool::default();
        let mut ctx = ToolCtx {
            doc: &mut d,
            state: &mut state,
        };
        tool.on_pointer_down(&mut ctx, input(20.0, 60.0));
        assert!(tool.editing.is_some(), "no layer on press");
        // A jitter of a pixel or two is a click, not a drag.
        tool.on_pointer_up(&mut ctx, input(22.0, 61.0));
        assert_eq!(tool.editing.as_ref().unwrap().stored.spec.wrap_width, None);
    }

    /// The width a drag defined belongs to that layer, not to the tool:
    /// after one area-text box, every later click made a box of the same
    /// width and there was no way back to point text.
    #[test]
    fn a_drag_does_not_make_every_later_click_a_box() {
        let mut d = doc();
        let mut state = schist_plugin_api::EditorState::default();
        let mut tool = TypeTool::default();
        let mut ctx = ToolCtx {
            doc: &mut d,
            state: &mut state,
        };

        tool.on_pointer_down(&mut ctx, input(20.0, 60.0));
        tool.on_pointer_up(&mut ctx, input(140.0, 100.0));
        type_text(&mut tool, &mut ctx, "box");
        assert_eq!(
            tool.editing.as_ref().unwrap().stored.spec.wrap_width,
            Some(120.0)
        );

        // A plain click somewhere else is point text again.
        tool.on_pointer_down(&mut ctx, input(20.0, 160.0));
        tool.on_pointer_up(&mut ctx, input(20.0, 160.0));
        assert_eq!(tool.editing.as_ref().unwrap().stored.spec.wrap_width, None);

        // And so is a click after editing an existing box.
        tool.on_pointer_down(&mut ctx, input(30.0, 65.0));
        assert_eq!(
            tool.editing.as_ref().unwrap().stored.spec.wrap_width,
            Some(120.0),
            "clicking into the box should re-open it as a box"
        );
        tool.on_pointer_up(&mut ctx, input(30.0, 65.0));
        tool.on_pointer_down(&mut ctx, input(250.0, 180.0));
        tool.on_pointer_up(&mut ctx, input(250.0, 180.0));
        assert_eq!(tool.editing.as_ref().unwrap().stored.spec.wrap_width, None);

        // Dialling the Wrap slider in by hand is the way to ask for a box
        // up front, and that does carry to the next click.
        tool.set_option("type-wrap", OptionValue::Num(90.0));
        tool.on_option_changed(&mut ctx, "type-wrap");
        tool.on_pointer_down(&mut ctx, input(40.0, 20.0));
        tool.on_pointer_up(&mut ctx, input(40.0, 20.0));
        assert_eq!(
            tool.editing.as_ref().unwrap().stored.spec.wrap_width,
            Some(90.0)
        );
    }

    /// The swatch reaches the text when the tool touches a layer, not
    /// only when an options-bar control happens to move.
    #[test]
    fn clicking_back_in_takes_the_current_foreground() {
        let mut d = doc();
        let mut state = schist_plugin_api::EditorState {
            foreground: schist_color::Rgba::new(1.0, 0.0, 0.0, 1.0),
            ..Default::default()
        };
        let mut tool = TypeTool::default();
        {
            let mut ctx = ToolCtx {
                doc: &mut d,
                state: &mut state,
            };
            tool.on_pointer_down(&mut ctx, input(20.0, 60.0));
            tool.on_pointer_up(&mut ctx, input(20.0, 60.0));
            type_text(&mut tool, &mut ctx, "Hi");
            assert_eq!(tool.editing.as_ref().unwrap().stored.color[0], 255);
            tool.on_commit(&mut ctx);
        }

        // Switch to blue and click back into the same text.
        state.foreground = schist_color::Rgba::new(0.0, 0.0, 1.0, 1.0);
        let mut ctx = ToolCtx {
            doc: &mut d,
            state: &mut state,
        };
        tool.on_pointer_down(&mut ctx, input(22.0, 55.0));
        let color = tool.editing.as_ref().expect("re-opened").stored.color;
        assert_eq!(color[2], 255, "the text stayed its old colour: {color:?}");
        assert_eq!(color[0], 0);
    }

    /// Tabs are layout characters here, so pasting a tabbed table keeps
    /// its columns.
    #[test]
    fn pasting_keeps_tabs() {
        let mut d = doc();
        let mut state = schist_plugin_api::EditorState::default();
        let mut tool = TypeTool::default();
        let mut ctx = ToolCtx {
            doc: &mut d,
            state: &mut state,
        };
        tool.on_pointer_down(&mut ctx, input(20.0, 60.0));
        tool.on_pointer_up(&mut ctx, input(20.0, 60.0));
        assert!(tool.insert_text(&mut ctx, "a\tb\nc\td"));
        assert_eq!(tool.editing_text(), Some("a\tb\nc\td"));
    }

    fn layer_height(doc: &Document) -> i32 {
        doc.tree
            .iter()
            .last()
            .map(|l| l.content_bounds().height())
            .unwrap_or(0)
    }
}
