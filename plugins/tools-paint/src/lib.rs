//! Painting tools: brush, pencil, eraser — plus the shared stroke engine.
//!
//! Photoshop opacity semantics: within one stroke, overlapping dabs
//! accumulate *coverage* (max, not sum), and the stroke composites onto the
//! pre-stroke pixels at `coverage × tool opacity`. So scribbling over the
//! same spot at 50% opacity stays 50%, but two separate strokes darken.

use photoslop_color::Rgba;
use photoslop_core::{Document, IntRect, LayerId, LayerKind, StrokeEdit, TileCoord, TILE_SIZE};
use photoslop_plugin_api::{
    EditorState, Overlay, PluginManifest, PluginRegistry, PointerInput, ToolCtx, ToolPlugin,
};
use rustc_hash::FxHashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaintMode {
    Brush,
    Pencil,
    Eraser,
}

/// One in-progress stroke.
struct Stroke {
    edit: StrokeEdit,
    layer: LayerId,
    /// Accumulated dab coverage (0..=1) per touched pixel, keyed by tile.
    coverage: FxHashMap<TileCoord, Box<[f32]>>,
    last: (f32, f32),
    /// Leftover distance to the next dab from the previous segment.
    spacing_debt: f32,
    color: Rgba,
    opacity: f32,
    size: f32,
    hardness: f32,
    mode: PaintMode,
}

impl Stroke {
    fn begin(ctx: &mut ToolCtx, input: PointerInput, mode: PaintMode) -> Option<Stroke> {
        let layer = paintable_layer(ctx.doc)?;
        let color = ctx.state.foreground;
        let mut stroke = Stroke {
            edit: StrokeEdit::new(match mode {
                PaintMode::Brush => "Brush",
                PaintMode::Pencil => "Pencil",
                PaintMode::Eraser => "Eraser",
            }),
            layer,
            coverage: FxHashMap::default(),
            last: (input.x, input.y),
            spacing_debt: 0.0,
            color,
            opacity: ctx.state.tool_opacity,
            size: ctx.state.brush_size,
            hardness: if mode == PaintMode::Pencil {
                1.0
            } else {
                ctx.state.brush_hardness
            },
            mode,
        };
        stroke.dab(ctx.doc, input.x, input.y, input.pressure);
        Some(stroke)
    }

    fn spacing(&self) -> f32 {
        (self.size * 0.15).max(1.0)
    }

    fn extend(&mut self, doc: &mut Document, x: f32, y: f32, pressure: f32) {
        let (lx, ly) = self.last;
        let dx = x - lx;
        let dy = y - ly;
        let dist = (dx * dx + dy * dy).sqrt();
        if dist <= f32::EPSILON {
            return;
        }
        let spacing = self.spacing();
        let mut t = self.spacing_debt;
        while t <= dist {
            let f = t / dist;
            self.dab(doc, lx + dx * f, ly + dy * f, pressure);
            t += spacing;
        }
        self.spacing_debt = t - dist;
        self.last = (x, y);
    }

    /// Stamp one dab: raise coverage, then re-composite affected pixels
    /// from their pre-stroke values.
    fn dab(&mut self, doc: &mut Document, cx: f32, cy: f32, pressure: f32) {
        let radius = (self.size / 2.0 * pressure.max(0.05)).max(0.5);
        let bounds = IntRect::new(
            (cx - radius).floor() as i32,
            (cy - radius).floor() as i32,
            (cx + radius).ceil() as i32 + 1,
            (cy + radius).ceil() as i32 + 1,
        );
        // Hard inner radius; anti-aliased single-pixel rim even at
        // hardness 1 so pencil still gets its crisp-but-not-jagged edge.
        let inner = radius * self.hardness.clamp(0.0, 0.99);
        let selection = doc.selection.clone();
        for coord in TileCoord::covering(&bounds) {
            let trect = coord.rect();
            let clip = trect.intersect(&bounds);
            if clip.is_empty() {
                continue;
            }
            // Coverage accumulation buffer for this tile.
            let cov = self
                .coverage
                .entry(coord)
                .or_insert_with(|| vec![0f32; (TILE_SIZE * TILE_SIZE) as usize].into_boxed_slice());
            let mut touched = false;
            for y in clip.top..clip.bottom {
                for x in clip.left..clip.right {
                    let d = ((x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2)).sqrt();
                    if d >= radius {
                        continue;
                    }
                    let mut a = if d <= inner {
                        1.0
                    } else {
                        1.0 - (d - inner) / (radius - inner)
                    };
                    if self.mode == PaintMode::Pencil {
                        // Pencil: binary coverage.
                        a = if a >= 0.5 { 1.0 } else { 0.0 };
                    }
                    a *= selection.coverage(x, y) as f32 / 255.0;
                    if a <= 0.0 {
                        continue;
                    }
                    let ix = ((y - trect.top) * TILE_SIZE + (x - trect.left)) as usize;
                    if a > cov[ix] {
                        cov[ix] = a;
                        touched = true;
                    }
                }
            }
            if !touched {
                continue;
            }
            // Re-composite this tile's touched pixels from pre-stroke state.
            let original = self.edit.pre_stroke_tile(doc, self.layer, coord);
            // Ensure before-capture happens (and get write access).
            let cov = self.coverage.get(&coord).unwrap();
            let (mode, color, opacity) = (self.mode, self.color, self.opacity);
            let Some(tile) = self.edit.writable_tile(doc, self.layer, coord) else {
                continue;
            };
            for y in clip.top..clip.bottom {
                for x in clip.left..clip.right {
                    let ix = ((y - trect.top) * TILE_SIZE + (x - trect.left)) as usize;
                    let c = cov[ix];
                    if c <= 0.0 {
                        continue;
                    }
                    let orig = match &original {
                        Some(t) => t.get(ix),
                        None => Rgba::TRANSPARENT,
                    };
                    let out = match mode {
                        PaintMode::Eraser => Rgba {
                            a: orig.a * (1.0 - c * opacity),
                            ..orig
                        },
                        _ => Rgba {
                            a: c * opacity,
                            ..color
                        }
                        .over(orig),
                    };
                    tile.set(ix, out);
                }
            }
            doc.add_damage(clip);
        }
    }

    fn finish(self, doc: &mut Document) {
        if let Some(layer) = doc.tree.find_mut(self.layer) {
            if let LayerKind::Raster(r) = &mut layer.kind {
                r.tiles.prune_blank();
            }
        }
        self.edit.commit(doc);
    }
}

/// Topmost paintable (raster, unlocked, visible) layer if the active layer
/// isn't paintable.
fn paintable_layer(doc: &Document) -> Option<LayerId> {
    if let Some(id) = doc.active_layer {
        if let Some(l) = doc.tree.find(id) {
            if matches!(l.kind, LayerKind::Raster(_)) && !l.locked {
                return Some(id);
            }
        }
    }
    doc.tree
        .iter()
        .filter(|l| matches!(l.kind, LayerKind::Raster(_)) && !l.locked)
        .map(|l| l.id)
        .last()
}

pub struct PaintTool {
    mode: PaintMode,
    stroke: Option<Stroke>,
    cursor: Option<(f32, f32)>,
}

impl PaintTool {
    fn new(mode: PaintMode) -> Self {
        PaintTool {
            mode,
            stroke: None,
            cursor: None,
        }
    }
}

impl ToolPlugin for PaintTool {
    fn id(&self) -> &'static str {
        match self.mode {
            PaintMode::Brush => "brush",
            PaintMode::Pencil => "pencil",
            PaintMode::Eraser => "eraser",
        }
    }

    fn name(&self) -> &'static str {
        match self.mode {
            PaintMode::Brush => "Brush",
            PaintMode::Pencil => "Pencil",
            PaintMode::Eraser => "Eraser",
        }
    }

    fn icon(&self) -> &'static str {
        match self.mode {
            PaintMode::Brush => "brush",
            PaintMode::Pencil => "pencil",
            PaintMode::Eraser => "eraser",
        }
    }

    fn shortcut(&self) -> Option<&'static str> {
        match self.mode {
            PaintMode::Brush => Some("b"),
            PaintMode::Pencil => None, // shift-b cycle lands in M11
            PaintMode::Eraser => Some("e"),
        }
    }

    fn on_pointer_down(&mut self, ctx: &mut ToolCtx, input: PointerInput) {
        self.cursor = Some((input.x, input.y));
        self.stroke = Stroke::begin(ctx, input, self.mode);
    }

    fn on_pointer_move(&mut self, ctx: &mut ToolCtx, input: PointerInput) {
        self.cursor = Some((input.x, input.y));
        if let Some(stroke) = &mut self.stroke {
            stroke.extend(ctx.doc, input.x, input.y, input.pressure);
        }
    }

    fn on_pointer_up(&mut self, ctx: &mut ToolCtx, _input: PointerInput) {
        if let Some(stroke) = self.stroke.take() {
            stroke.finish(ctx.doc);
        }
    }

    fn on_cancel(&mut self, ctx: &mut ToolCtx) {
        if let Some(stroke) = self.stroke.take() {
            stroke.edit.cancel(ctx.doc);
        }
    }

    fn overlays(&self, _doc: &Document, state: &EditorState) -> Vec<Overlay> {
        match self.cursor {
            Some((cx, cy)) => {
                vec![Overlay::Circle {
                    cx,
                    cy,
                    r: state.brush_size / 2.0,
                }]
            }
            None => Vec::new(),
        }
    }
}

pub struct PaintToolsPlugin;

impl PluginManifest for PaintToolsPlugin {
    fn id(&self) -> &'static str {
        "photoslop.tools-paint"
    }

    fn register(&self, registry: &mut PluginRegistry) {
        registry.register_tool(Box::new(PaintTool::new(PaintMode::Brush)));
        registry.register_tool(Box::new(PaintTool::new(PaintMode::Pencil)));
        registry.register_tool(Box::new(PaintTool::new(PaintMode::Eraser)));
    }
}

// Silence unused-dep warning: pixel-ops is used by future paint modes
// (brush blend modes); keep the wiring alive.
#[allow(unused_imports)]
use photoslop_pixel_ops as _;

#[cfg(test)]
mod tests {
    use super::*;
    use photoslop_color::Depth;
    use photoslop_core::Layer;
    use photoslop_plugin_api::Modifiers;

    fn doc_with_layer() -> Document {
        let mut doc = Document::new("t", 128, 128, Depth::Eight);
        doc.push_layer(Layer::new_raster("paint"));
        doc
    }

    fn input(x: f32, y: f32) -> PointerInput {
        PointerInput {
            x,
            y,
            pressure: 1.0,
            modifiers: Modifiers::default(),
        }
    }

    fn pixel(doc: &Document, x: i32, y: i32) -> [u8; 4] {
        doc.tree.layers[0]
            .as_raster()
            .unwrap()
            .tiles
            .pixel(x, y)
            .to_u8()
    }

    #[test]
    fn brush_paints_and_undoes() {
        let mut doc = doc_with_layer();
        let mut state = EditorState {
            foreground: Rgba::new(1.0, 0.0, 0.0, 1.0),
            ..Default::default()
        };
        let mut tool = PaintTool::new(PaintMode::Brush);
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        tool.on_pointer_down(&mut ctx, input(30.0, 30.0));
        tool.on_pointer_move(&mut ctx, input(70.0, 30.0));
        tool.on_pointer_up(&mut ctx, input(70.0, 30.0));

        let p = pixel(&doc, 50, 30);
        assert_eq!(p[0], 255, "stroke center painted: {p:?}");
        assert_eq!(p[3], 255);
        assert_eq!(pixel(&doc, 50, 100)[3], 0, "far pixel untouched");

        assert_eq!(doc.undo().as_deref(), Some("Brush"));
        assert_eq!(pixel(&doc, 50, 30)[3], 0, "undo clears stroke");
        doc.redo();
        assert_eq!(pixel(&doc, 50, 30)[0], 255, "redo restores");
    }

    #[test]
    fn stroke_opacity_does_not_compound_within_stroke() {
        let mut doc = doc_with_layer();
        let mut state = EditorState {
            foreground: Rgba::BLACK,
            tool_opacity: 0.5,
            ..Default::default()
        };
        let mut tool = PaintTool::new(PaintMode::Brush);
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        tool.on_pointer_down(&mut ctx, input(40.0, 40.0));
        // Scribble back and forth over the same spot.
        for _ in 0..5 {
            tool.on_pointer_move(&mut ctx, input(60.0, 40.0));
            tool.on_pointer_move(&mut ctx, input(40.0, 40.0));
        }
        tool.on_pointer_up(&mut ctx, input(40.0, 40.0));
        let a = pixel(&doc, 50, 40)[3];
        assert!((a as i32 - 128).abs() <= 2, "opacity stayed ~50%: {a}");
    }

    #[test]
    fn separate_strokes_compound() {
        let mut doc = doc_with_layer();
        let mut state = EditorState {
            foreground: Rgba::BLACK,
            tool_opacity: 0.5,
            ..Default::default()
        };
        let mut tool = PaintTool::new(PaintMode::Brush);
        for _ in 0..2 {
            let mut ctx = ToolCtx {
                doc: &mut doc,
                state: &mut state,
            };
            tool.on_pointer_down(&mut ctx, input(50.0, 40.0));
            tool.on_pointer_up(&mut ctx, input(50.0, 40.0));
        }
        let a = pixel(&doc, 50, 40)[3];
        assert!(a > 170 && a < 210, "two 50% strokes ≈ 75%: {a}");
    }

    #[test]
    fn eraser_clears() {
        let mut doc = doc_with_layer();
        let mut state = EditorState {
            foreground: Rgba::BLACK,
            ..Default::default()
        };
        let mut brush = PaintTool::new(PaintMode::Brush);
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        brush.on_pointer_down(&mut ctx, input(50.0, 50.0));
        brush.on_pointer_up(&mut ctx, input(50.0, 50.0));
        assert!(pixel(&doc, 50, 50)[3] > 0);

        let mut eraser = PaintTool::new(PaintMode::Eraser);
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        eraser.on_pointer_down(&mut ctx, input(50.0, 50.0));
        eraser.on_pointer_up(&mut ctx, input(50.0, 50.0));
        assert_eq!(pixel(&doc, 50, 50)[3], 0, "erased");
    }

    #[test]
    fn selection_confines_painting() {
        use photoslop_core::SelectOp;
        let mut doc = doc_with_layer();
        doc.selection
            .select_rect(IntRect::from_xywh(0, 0, 45, 128), SelectOp::Replace);
        let mut state = EditorState {
            foreground: Rgba::BLACK,
            ..Default::default()
        };
        let mut tool = PaintTool::new(PaintMode::Brush);
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        tool.on_pointer_down(&mut ctx, input(30.0, 30.0));
        tool.on_pointer_move(&mut ctx, input(80.0, 30.0));
        tool.on_pointer_up(&mut ctx, input(80.0, 30.0));
        assert!(pixel(&doc, 40, 30)[3] > 0, "inside selection painted");
        assert_eq!(pixel(&doc, 60, 30)[3], 0, "outside selection untouched");
    }

    #[test]
    fn cancel_rolls_back() {
        let mut doc = doc_with_layer();
        let mut state = EditorState::default();
        let mut tool = PaintTool::new(PaintMode::Brush);
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        tool.on_pointer_down(&mut ctx, input(50.0, 50.0));
        tool.on_cancel(&mut ctx);
        assert_eq!(pixel(&doc, 50, 50)[3], 0);
        assert!(!doc.history.can_undo());
    }
}
