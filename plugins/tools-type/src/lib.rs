//! Type tool (T): editable text layers.
//!
//! A text layer is a raster layer plus a `PsTx` block in its preserved-PSD
//! extras holding the JSON [`TextSpec`] it was rendered from. That block
//! rides through save/load untouched (the PSD writer re-emits unknown
//! blocks verbatim), so text stays re-editable across sessions while
//! Photoshop still sees ordinary pixels.

use photoslop_color::Rgba;
use photoslop_core::{
    Document, IntRect, Layer, LayerId, LayerPath, RawBlock, TileCoord, TileMap, TILE_SIZE,
};
use photoslop_plugin_api::{
    EditorState, Modifiers, Overlay, PluginManifest, PluginRegistry, PointerInput, ToolCtx,
    ToolPlugin,
};
use photoslop_text_engine::{rasterize, Align, TextSpec};

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
}

#[derive(Default)]
pub struct TypeTool {
    editing: Option<Editing>,
}

impl TypeTool {
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
            layer.name = display_name(&session.stored.spec.text);
            write_stored(layer, &session.stored);
        }
        doc.add_damage(before.union(&bounds));
    }

    fn start_new(&mut self, ctx: &mut ToolCtx, x: f32, y: f32) {
        let stored = StoredText {
            spec: TextSpec {
                size: (ctx.state.brush_size * 2.0).clamp(8.0, 400.0),
                ..Default::default()
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
        });
    }

    /// Pick an existing text layer under the cursor, if any.
    fn text_layer_at(doc: &Document, x: f32, y: f32) -> Option<(LayerId, StoredText)> {
        let (px, py) = (x.round() as i32, y.round() as i32);
        let mut hit = None;
        for layer in doc.tree.iter() {
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
                self.editing = Some(Editing {
                    layer,
                    stored,
                    original,
                    created: false,
                    dirty: false,
                });
            }
            None => self.start_new(ctx, input.x, input.y),
        }
    }

    fn on_pointer_move(&mut self, _ctx: &mut ToolCtx, _input: PointerInput) {}
    fn on_pointer_up(&mut self, _ctx: &mut ToolCtx, _input: PointerInput) {}

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
        // Let shortcuts through; we only capture plain typing.
        if modifiers.ctrl_or_cmd {
            return false;
        }
        let mut changed = true;
        {
            let Some(session) = &mut self.editing else {
                return false;
            };
            match key {
                "backspace" => {
                    session.stored.spec.text.pop();
                }
                "enter" => session.stored.spec.text.push('\n'),
                "tab" => session.stored.spec.text.push_str("    "),
                "space" => session.stored.spec.text.push(' '),
                _ => match text {
                    Some(t) if !t.is_empty() && !t.chars().any(|c| c.is_control()) => {
                        session.stored.spec.text.push_str(t)
                    }
                    _ => changed = false,
                },
            }
            if changed {
                session.dirty = true;
            }
        }
        if changed {
            self.refresh(ctx.doc);
        }
        // Swallow every plain keystroke while typing so letters don't
        // switch tools mid-word.
        true
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
                // The insert and this removal cancel out; drop both.
                ctx.doc.undo();
                ctx.doc.undo();
                ctx.doc.history.pop_redo();
                ctx.doc.history.pop_redo();
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
        let (ox, oy) = session.origin_f32();
        let size = session.stored.spec.size;
        // Box around the text plus a caret at the end of the last line.
        let mut out = Vec::new();
        if !bounds.is_empty() {
            out.push(Overlay::Rect(bounds.inflated(2)));
        }
        let lines = session.stored.spec.text.lines().count().max(1) as f32;
        let caret_x = if bounds.is_empty() {
            ox
        } else {
            bounds.right as f32
        };
        let caret_y = oy + (lines - 1.0) * size * session.stored.spec.line_height;
        out.push(Overlay::Line {
            x1: caret_x,
            y1: caret_y,
            x2: caret_x,
            y2: caret_y + size,
        });
        out
    }
}

impl Editing {
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

pub struct TypeToolsPlugin;

impl PluginManifest for TypeToolsPlugin {
    fn id(&self) -> &'static str {
        "photoslop.tools-type"
    }

    fn register(&self, registry: &mut PluginRegistry) {
        registry.register_tool(Box::new(TypeTool::default()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use photoslop_color::Depth;

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
                (0..photoslop_core::TILE_PIXELS)
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
}
