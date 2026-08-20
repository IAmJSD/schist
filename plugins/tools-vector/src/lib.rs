//! Shape tools (U) and the pen tool (P).
//!
//! Shapes and pen paths are rasterized onto their own layer through
//! `photoslop-vector`. They are *not* PSD vector shape layers: we render
//! pixels and keep no editable vector data, which is why re-opening a saved
//! file gives raster layers (noted in PLAN.md §7 as an M7 scope cut).

use photoslop_color::Rgba;
use photoslop_core::{Document, IntRect, Layer, LayerPath, TileCoord, TILE_SIZE};
use photoslop_plugin_api::{
    EditorState, Overlay, PluginManifest, PluginRegistry, PointerInput, ToolCtx, ToolPlugin,
};
use photoslop_vector::{FillRule, Path, PathBuilder};

/// Paint a coverage mask onto a fresh layer above the active one, honouring
/// the current selection, as a single undoable edit.
fn commit_shape(doc: &mut Document, path: &Path, color: Rgba, rule: FillRule, name: &str) {
    let bounds = path.bounds().intersect(&doc.canvas_rect());
    if bounds.is_empty() {
        return;
    }
    let mask = photoslop_vector::rasterize(path, bounds, rule);
    let w = bounds.width() as usize;
    let selection = doc.selection.clone();
    let depth = doc.depth;

    let mut layer = Layer::new_raster(name);
    {
        let tiles = &mut layer.as_raster_mut().unwrap().tiles;
        for coord in TileCoord::covering(&bounds) {
            let trect = coord.rect();
            let clip = trect.intersect(&bounds);
            if clip.is_empty() {
                continue;
            }
            let buf = tiles.get_mut_or_insert(coord, depth);
            for y in clip.top..clip.bottom {
                for x in clip.left..clip.right {
                    let cov = mask[(y - bounds.top) as usize * w + (x - bounds.left) as usize];
                    if cov == 0 {
                        continue;
                    }
                    let sel = selection.coverage(x, y) as f32 / 255.0;
                    let a = color.a * (cov as f32 / 255.0) * sel;
                    if a <= 0.0 {
                        continue;
                    }
                    let ix = ((y - trect.top) * TILE_SIZE + (x - trect.left)) as usize;
                    buf.set(ix, Rgba { a, ..color });
                }
            }
        }
        tiles.prune_blank();
    }
    if layer.as_raster().unwrap().tiles.is_empty() {
        return;
    }

    let id = layer.id;
    let path_index = match doc.active_layer.and_then(|a| doc.tree.path_of(a)) {
        Some(mut p) => {
            *p.0.last_mut().unwrap() += 1;
            p
        }
        None => LayerPath(vec![doc.tree.layers.len()]),
    };
    let mut edit = doc.begin_edit(name.to_string());
    edit.insert_layer(path_index, layer);
    edit.commit();
    doc.active_layer = Some(id);
}

fn drag_rect(ax: f32, ay: f32, bx: f32, by: f32, square: bool) -> IntRect {
    let (mut w, mut h) = (bx - ax, by - ay);
    if square {
        let m = w.abs().max(h.abs());
        w = m * w.signum();
        h = m * h.signum();
    }
    let (x0, x1) = if w < 0.0 { (ax + w, ax) } else { (ax, ax + w) };
    let (y0, y1) = if h < 0.0 { (ay + h, ay) } else { (ay, ay + h) };
    IntRect::new(
        x0.round() as i32,
        y0.round() as i32,
        x1.round() as i32,
        y1.round() as i32,
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShapeKind {
    Rectangle,
    Ellipse,
    Line,
    Polygon,
}

impl ShapeKind {
    fn label(self) -> &'static str {
        match self {
            ShapeKind::Rectangle => "Rectangle",
            ShapeKind::Ellipse => "Ellipse",
            ShapeKind::Line => "Line",
            ShapeKind::Polygon => "Polygon",
        }
    }
}

pub struct ShapeTool {
    kind: ShapeKind,
    /// Number of sides for the polygon shape.
    pub sides: u32,
    anchor: Option<(f32, f32)>,
    current: Option<(f32, f32)>,
    square: bool,
}

impl ShapeTool {
    fn new(kind: ShapeKind) -> ShapeTool {
        ShapeTool {
            kind,
            sides: 5,
            anchor: None,
            current: None,
            square: false,
        }
    }

    fn path_for(&self, from: (f32, f32), to: (f32, f32), stroke_width: f32) -> Path {
        let mut b = PathBuilder::new();
        match self.kind {
            ShapeKind::Rectangle => {
                b.rect(drag_rect(from.0, from.1, to.0, to.1, self.square));
            }
            ShapeKind::Ellipse => {
                b.ellipse(drag_rect(from.0, from.1, to.0, to.1, self.square));
            }
            ShapeKind::Polygon => {
                b.polygon(
                    drag_rect(from.0, from.1, to.0, to.1, self.square),
                    self.sides,
                );
            }
            ShapeKind::Line => {
                b.move_to(from.0, from.1).line_to(to.0, to.1);
                return photoslop_vector::stroke_to_path(&b.build(0.25), stroke_width);
            }
        }
        b.build(0.25)
    }
}

impl ToolPlugin for ShapeTool {
    fn id(&self) -> &'static str {
        match self.kind {
            ShapeKind::Rectangle => "shape.rect",
            ShapeKind::Ellipse => "shape.ellipse",
            ShapeKind::Line => "shape.line",
            ShapeKind::Polygon => "shape.polygon",
        }
    }

    fn name(&self) -> &'static str {
        match self.kind {
            ShapeKind::Rectangle => "Rectangle Tool",
            ShapeKind::Ellipse => "Ellipse Tool",
            ShapeKind::Line => "Line Tool",
            ShapeKind::Polygon => "Polygon Tool",
        }
    }

    fn icon(&self) -> &'static str {
        match self.kind {
            ShapeKind::Rectangle => "shape-rect",
            ShapeKind::Ellipse => "shape-ellipse",
            ShapeKind::Line => "shape-line",
            ShapeKind::Polygon => "shape-polygon",
        }
    }

    fn shortcut(&self) -> Option<&'static str> {
        matches!(self.kind, ShapeKind::Rectangle).then_some("u")
    }

    fn on_pointer_down(&mut self, _ctx: &mut ToolCtx, input: PointerInput) {
        self.anchor = Some((input.x, input.y));
        self.current = Some((input.x, input.y));
        self.square = input.modifiers.shift;
    }

    fn on_pointer_move(&mut self, _ctx: &mut ToolCtx, input: PointerInput) {
        if self.anchor.is_some() {
            self.current = Some((input.x, input.y));
            self.square = input.modifiers.shift;
        }
    }

    fn on_pointer_up(&mut self, ctx: &mut ToolCtx, input: PointerInput) {
        let Some(anchor) = self.anchor.take() else {
            return;
        };
        self.current = None;
        let to = (input.x, input.y);
        if (to.0 - anchor.0).abs() < 0.5 && (to.1 - anchor.1).abs() < 0.5 {
            return; // a click, not a drag
        }
        let path = self.path_for(anchor, to, ctx.state.brush_size);
        // Stroke outlines self-overlap at joins, so every shape fills with
        // the nonzero rule.
        let color = ctx.state.foreground;
        commit_shape(ctx.doc, &path, color, FillRule::NonZero, self.kind.label());
    }

    fn on_cancel(&mut self, _ctx: &mut ToolCtx) {
        self.anchor = None;
        self.current = None;
    }

    fn overlays(&self, _doc: &Document, state: &EditorState) -> Vec<Overlay> {
        let (Some(a), Some(c)) = (self.anchor, self.current) else {
            return Vec::new();
        };
        match self.kind {
            ShapeKind::Line => vec![Overlay::Line {
                x1: a.0,
                y1: a.1,
                x2: c.0,
                y2: c.1,
            }],
            _ => {
                let r = drag_rect(a.0, a.1, c.0, c.1, self.square);
                let _ = state;
                vec![Overlay::Rect(r)]
            }
        }
    }
}

/// Pen: click for corner points, drag for smooth Bézier handles, Enter (or
/// clicking the first point) fills the path.
#[derive(Default)]
pub struct PenTool {
    /// Anchor points with their outgoing control handle offsets.
    anchors: Vec<((f32, f32), (f32, f32))>,
    dragging: bool,
    cursor: Option<(f32, f32)>,
}

impl PenTool {
    fn build_path(&self, close: bool) -> Path {
        let mut b = PathBuilder::new();
        if self.anchors.is_empty() {
            return b.build(0.25);
        }
        b.move_to(self.anchors[0].0 .0, self.anchors[0].0 .1);
        for i in 1..self.anchors.len() {
            self.segment(&mut b, i - 1, i);
        }
        if close && self.anchors.len() > 2 {
            self.segment(&mut b, self.anchors.len() - 1, 0);
            b.close();
        }
        b.build(0.25)
    }

    /// Emit the curve (or line) between two anchors, mirroring the incoming
    /// handle so joins stay smooth.
    fn segment(&self, b: &mut PathBuilder, from: usize, to: usize) {
        let (p0, h0) = self.anchors[from];
        let (p1, h1) = self.anchors[to];
        if h0 == (0.0, 0.0) && h1 == (0.0, 0.0) {
            b.line_to(p1.0, p1.1);
        } else {
            b.cubic_to(
                p0.0 + h0.0,
                p0.1 + h0.1,
                p1.0 - h1.0,
                p1.1 - h1.1,
                p1.0,
                p1.1,
            );
        }
    }
}

impl ToolPlugin for PenTool {
    fn id(&self) -> &'static str {
        "pen"
    }
    fn name(&self) -> &'static str {
        "Pen"
    }
    fn icon(&self) -> &'static str {
        "pen"
    }
    fn shortcut(&self) -> Option<&'static str> {
        Some("p")
    }

    fn on_pointer_down(&mut self, ctx: &mut ToolCtx, input: PointerInput) {
        // Clicking the first anchor closes and fills the path.
        if let Some(((fx, fy), _)) = self.anchors.first() {
            let r = 6.0 / ctx.state.zoom.max(0.01);
            if self.anchors.len() > 2 && (input.x - fx).abs() < r && (input.y - fy).abs() < r {
                self.on_commit(ctx);
                return;
            }
        }
        self.anchors.push(((input.x, input.y), (0.0, 0.0)));
        self.dragging = true;
    }

    fn on_pointer_move(&mut self, _ctx: &mut ToolCtx, input: PointerInput) {
        self.cursor = Some((input.x, input.y));
        if self.dragging {
            if let Some(last) = self.anchors.last_mut() {
                last.1 = (input.x - last.0 .0, input.y - last.0 .1);
            }
        }
    }

    fn on_pointer_up(&mut self, _ctx: &mut ToolCtx, _input: PointerInput) {
        self.dragging = false;
    }

    fn on_commit(&mut self, ctx: &mut ToolCtx) {
        if self.anchors.len() < 3 {
            self.anchors.clear();
            return;
        }
        let path = self.build_path(true);
        self.anchors.clear();
        self.dragging = false;
        let color = ctx.state.foreground;
        commit_shape(ctx.doc, &path, color, FillRule::NonZero, "Path Fill");
    }

    fn on_cancel(&mut self, _ctx: &mut ToolCtx) {
        self.anchors.clear();
        self.dragging = false;
    }

    fn overlays(&self, _doc: &Document, _state: &EditorState) -> Vec<Overlay> {
        let mut out = Vec::new();
        if self.anchors.is_empty() {
            return out;
        }
        // Flattened preview of the path so far.
        let path = self.build_path(false);
        for sub in &path.subpaths {
            if sub.len() >= 2 {
                out.push(Overlay::AntsPolygon(sub.clone()));
            }
        }
        for ((x, y), h) in &self.anchors {
            out.push(Overlay::Rect(IntRect::new(
                *x as i32 - 2,
                *y as i32 - 2,
                *x as i32 + 2,
                *y as i32 + 2,
            )));
            if *h != (0.0, 0.0) {
                out.push(Overlay::Line {
                    x1: *x,
                    y1: *y,
                    x2: x + h.0,
                    y2: y + h.1,
                });
            }
        }
        // Rubber band to the cursor.
        if let (Some(((lx, ly), _)), Some((cx, cy))) = (self.anchors.last(), self.cursor) {
            out.push(Overlay::Line {
                x1: *lx,
                y1: *ly,
                x2: cx,
                y2: cy,
            });
        }
        out
    }
}

pub struct VectorToolsPlugin;

impl PluginManifest for VectorToolsPlugin {
    fn id(&self) -> &'static str {
        "photoslop.tools-vector"
    }

    fn register(&self, registry: &mut PluginRegistry) {
        registry.register_tool(Box::new(PenTool::default()));
        registry.register_tool(Box::new(ShapeTool::new(ShapeKind::Rectangle)));
        registry.register_tool(Box::new(ShapeTool::new(ShapeKind::Ellipse)));
        registry.register_tool(Box::new(ShapeTool::new(ShapeKind::Line)));
        registry.register_tool(Box::new(ShapeTool::new(ShapeKind::Polygon)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use photoslop_color::Depth;
    use photoslop_core::SelectOp;
    use photoslop_plugin_api::Modifiers;

    fn doc() -> Document {
        let mut d = Document::new("t", 100, 100, Depth::Eight);
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

    fn top_px(doc: &Document, x: i32, y: i32) -> [u8; 4] {
        doc.tree
            .layers
            .last()
            .unwrap()
            .as_raster()
            .unwrap()
            .tiles
            .pixel(x, y)
            .to_u8()
    }

    fn red() -> EditorState {
        EditorState {
            foreground: Rgba::new(1.0, 0.0, 0.0, 1.0),
            ..Default::default()
        }
    }

    #[test]
    fn rectangle_tool_creates_a_filled_layer() {
        let mut doc = doc();
        let mut state = red();
        let mut tool = ShapeTool::new(ShapeKind::Rectangle);
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        tool.on_pointer_down(&mut ctx, input(10.0, 10.0));
        tool.on_pointer_move(&mut ctx, input(40.0, 30.0));
        tool.on_pointer_up(&mut ctx, input(40.0, 30.0));

        assert_eq!(doc.tree.layers.len(), 2, "shape went on its own layer");
        assert_eq!(doc.tree.layers[1].name, "Rectangle");
        assert_eq!(top_px(&doc, 20, 20), [255, 0, 0, 255]);
        assert_eq!(top_px(&doc, 60, 60)[3], 0);
        doc.undo();
        assert_eq!(doc.tree.layers.len(), 1, "undo removes the shape layer");
    }

    #[test]
    fn ellipse_tool_is_round() {
        let mut doc = doc();
        let mut state = red();
        let mut tool = ShapeTool::new(ShapeKind::Ellipse);
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        tool.on_pointer_down(&mut ctx, input(10.0, 10.0));
        tool.on_pointer_move(&mut ctx, input(50.0, 50.0));
        tool.on_pointer_up(&mut ctx, input(50.0, 50.0));
        assert_eq!(top_px(&doc, 30, 30), [255, 0, 0, 255], "centre filled");
        assert_eq!(top_px(&doc, 11, 11)[3], 0, "corner outside the ellipse");
    }

    #[test]
    fn shift_constrains_to_a_square() {
        let mut doc = doc();
        let mut state = red();
        let mut tool = ShapeTool::new(ShapeKind::Rectangle);
        let shift = PointerInput {
            modifiers: Modifiers {
                shift: true,
                ..Default::default()
            },
            ..input(10.0, 10.0)
        };
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        tool.on_pointer_down(&mut ctx, shift);
        tool.on_pointer_move(
            &mut ctx,
            PointerInput {
                x: 50.0,
                y: 20.0,
                ..shift
            },
        );
        tool.on_pointer_up(
            &mut ctx,
            PointerInput {
                x: 50.0,
                y: 20.0,
                ..shift
            },
        );
        // Constrained to 40x40, not 40x10.
        assert_eq!(top_px(&doc, 45, 45), [255, 0, 0, 255]);
    }

    #[test]
    fn line_tool_strokes_with_brush_width() {
        let mut doc = doc();
        let mut state = EditorState {
            brush_size: 6.0,
            ..red()
        };
        let mut tool = ShapeTool::new(ShapeKind::Line);
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        tool.on_pointer_down(&mut ctx, input(10.0, 50.0));
        tool.on_pointer_move(&mut ctx, input(80.0, 50.0));
        tool.on_pointer_up(&mut ctx, input(80.0, 50.0));
        assert_eq!(top_px(&doc, 40, 50), [255, 0, 0, 255], "on the line");
        assert!(top_px(&doc, 40, 52)[3] > 0, "within the stroke width");
        assert_eq!(top_px(&doc, 40, 60)[3], 0, "outside it");
    }

    #[test]
    fn shapes_respect_the_selection() {
        let mut doc = doc();
        doc.selection
            .select_rect(IntRect::from_xywh(0, 0, 30, 100), SelectOp::Replace);
        let mut state = red();
        let mut tool = ShapeTool::new(ShapeKind::Rectangle);
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        tool.on_pointer_down(&mut ctx, input(10.0, 10.0));
        tool.on_pointer_move(&mut ctx, input(60.0, 60.0));
        tool.on_pointer_up(&mut ctx, input(60.0, 60.0));
        assert_eq!(top_px(&doc, 20, 20), [255, 0, 0, 255], "inside selection");
        assert_eq!(top_px(&doc, 45, 20)[3], 0, "clipped outside selection");
    }

    #[test]
    fn pen_tool_fills_a_closed_polygon() {
        let mut doc = doc();
        let mut state = red();
        let mut tool = PenTool::default();
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        for (x, y) in [(10.0, 10.0), (60.0, 10.0), (60.0, 60.0), (10.0, 60.0)] {
            tool.on_pointer_down(&mut ctx, input(x, y));
            tool.on_pointer_up(&mut ctx, input(x, y));
        }
        tool.on_commit(&mut ctx);
        assert_eq!(doc.tree.layers.len(), 2);
        assert_eq!(top_px(&doc, 35, 35), [255, 0, 0, 255]);
        assert_eq!(top_px(&doc, 80, 80)[3], 0);
    }

    #[test]
    fn pen_tool_cancel_discards_anchors() {
        let mut doc = doc();
        let mut state = red();
        let mut tool = PenTool::default();
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        tool.on_pointer_down(&mut ctx, input(10.0, 10.0));
        tool.on_pointer_up(&mut ctx, input(10.0, 10.0));
        tool.on_cancel(&mut ctx);
        assert!(tool.anchors.is_empty());
        assert_eq!(doc.tree.layers.len(), 1, "nothing committed");
    }

    #[test]
    fn tiny_drag_creates_nothing() {
        let mut doc = doc();
        let mut state = red();
        let mut tool = ShapeTool::new(ShapeKind::Rectangle);
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        tool.on_pointer_down(&mut ctx, input(10.0, 10.0));
        tool.on_pointer_up(&mut ctx, input(10.2, 10.1));
        assert_eq!(doc.tree.layers.len(), 1);
    }
}
