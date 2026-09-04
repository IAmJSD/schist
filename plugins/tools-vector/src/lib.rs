//! Shape tools (U) and the pen tool (P).
//!
//! Shapes and pen paths are rasterized onto their own layer through
//! `schist-vector`. They are *not* PSD vector shape layers: we render
//! pixels and keep no editable vector data, which is why re-opening a saved
//! file gives raster layers (a deliberate v1 scope cut).

pub mod paths;

use schist_color::Rgba;
use schist_core::{Document, IntRect, Layer, LayerId, LayerPath, TileCoord, TILE_SIZE};
use schist_plugin_api::{
    EditorState, OptionValue, Overlay, PluginManifest, PluginRegistry, PointerInput, ToolCtx,
    ToolOption, ToolPlugin,
};
use schist_vector::{FillRule, Path, PathBuilder};

/// Paint a coverage mask onto a fresh layer above the active one, honouring
/// the current selection, as a single undoable edit.
/// Rasterize a path onto a new layer, clipped to the selection.
///
/// Public so the shell can drive Fill Path and Stroke Path from the menu.
pub fn fill_path(doc: &mut Document, path: &Path, color: Rgba, rule: FillRule, name: &str) {
    commit_shape(doc, path, color, rule, name)
}

/// Rasterize a vector shape: fill, then stroke on top.
///
/// The single implementation, used both when a shape layer is created and
/// whenever its path is edited afterwards, so the two can never drift.
pub fn render_shape(
    shape: &schist_core::VectorShape,
    depth: schist_color::Depth,
    canvas: IntRect,
) -> schist_core::TileMap {
    let flat = crate::paths::flatten(&shape.path);
    let rule = if shape.even_odd {
        FillRule::EvenOdd
    } else {
        FillRule::NonZero
    };
    let mut tiles = schist_core::TileMap::new();
    rasterize_into(&mut tiles, &flat, rule, shape.fill, depth, canvas);
    if let Some((colour, width)) = shape.stroke {
        let stroked = schist_vector::stroke_path(
            &flat,
            schist_vector::StrokeStyle::new(width)
                .with_cap(schist_vector::LineCap::Round)
                .with_join(schist_vector::LineJoin::Round),
        );
        rasterize_into(
            &mut tiles,
            &stroked,
            FillRule::NonZero,
            colour,
            depth,
            canvas,
        );
    }
    tiles
}

/// Composite a filled path onto a tile map.
fn rasterize_into(
    tiles: &mut schist_core::TileMap,
    path: &Path,
    rule: FillRule,
    colour: Rgba,
    depth: schist_color::Depth,
    canvas: IntRect,
) {
    let bounds = path.bounds().intersect(&canvas);
    if bounds.is_empty() {
        return;
    }
    let mask = schist_vector::rasterize(path, bounds, rule);
    let w = bounds.width() as usize;
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
                let ix = ((y - trect.top) * TILE_SIZE + (x - trect.left)) as usize;
                let under = buf.get(ix);
                let a = colour.a * cov as f32 / 255.0;
                buf.set(ix, Rgba { a, ..colour }.over(under));
            }
        }
    }
}

/// A live shape layer, rasterized straight away rather than waiting for
/// the next frame's refresh, so the shape appears the moment it is drawn.
pub(crate) fn shape_layer(doc: &Document, shape: schist_core::VectorShape, name: &str) -> Layer {
    let mut layer = Layer::new_raster(name);
    let key = shape.key();
    if let Some(raster) = layer.as_raster_mut() {
        raster.tiles = render_shape(&shape, doc.depth, doc.canvas_rect());
    }
    layer.shape_key = key;
    layer.shape = Some(Box::new(shape));
    layer
}

/// Where a new shape layer goes: at the top of the stack for a live shape
/// (`above_active` false), or just above the active layer for pixels.
fn insert_path(doc: &Document, above_active: bool) -> LayerPath {
    let above = above_active
        .then(|| doc.active_layer.and_then(|a| doc.tree.path_of(a)))
        .flatten();
    match above {
        Some(mut p) => {
            *p.0.last_mut().unwrap() += 1;
            p
        }
        None => LayerPath(vec![doc.tree.layers.len()]),
    }
}

/// Insert a finished layer as a single undoable edit and make it active.
pub(crate) fn commit_layer(doc: &mut Document, layer: Layer, path: LayerPath, edit: &str) {
    let id = layer.id;
    let mut edit = doc.begin_edit(edit.to_string());
    edit.insert_layer(path, layer);
    edit.commit();
    doc.active_layer = Some(id);
}

/// Insert a live shape layer.
pub(crate) fn commit_shape_layer(doc: &mut Document, shape: schist_core::VectorShape, name: &str) {
    let layer = shape_layer(doc, shape, name);
    // Keep a new shape beside the active layer, including inside its group.
    // Sending live shapes to the root top made drawing inside a group jump
    // the result out of that group and above the rest of the document.
    let path = insert_path(doc, true);
    commit_layer(doc, layer, path, &format!("{name} Layer"));
}

/// A path rasterized onto a fresh layer, clipped to the selection. `None`
/// when nothing would be painted (off canvas, or entirely deselected).
pub(crate) fn rasterized_layer(
    doc: &Document,
    path: &Path,
    color: Rgba,
    rule: FillRule,
    name: &str,
) -> Option<Layer> {
    let bounds = path.bounds().intersect(&doc.canvas_rect());
    if bounds.is_empty() {
        return None;
    }
    let mask = schist_vector::rasterize(path, bounds, rule);
    let w = bounds.width() as usize;
    let selection = &doc.selection;
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
    (!layer.as_raster().unwrap().tiles.is_empty()).then_some(layer)
}

pub(crate) fn commit_shape(
    doc: &mut Document,
    path: &Path,
    color: Rgba,
    rule: FillRule,
    name: &str,
) {
    let Some(layer) = rasterized_layer(doc, path, color, rule, name) else {
        return;
    };
    let path = insert_path(doc, true);
    commit_layer(doc, layer, path, name);
}

/// The layer a shape tool shows while its shape is being dragged out.
///
/// Affinity and Photoshop draw the actual shape under the cursor, fill
/// and all, not a marching outline of where it will go. The preview is
/// the very layer the drag would commit, rebuilt on every move and kept
/// outside history: it sits in the tree so the compositor paints it like
/// anything else, and on release it is lifted out and re-inserted as the
/// one undoable edit. Cancelling just drops it.
#[derive(Default)]
pub(crate) struct DragPreview {
    layer: Option<LayerId>,
}

impl DragPreview {
    pub(crate) fn is_showing(&self) -> bool {
        self.layer.is_some()
    }

    /// Replace the preview with `fresh`, or remove it when the drag would
    /// commit nothing. The layer keeps its id across updates so the layers
    /// panel sees one layer growing, not a new one every frame.
    pub(crate) fn show(&mut self, doc: &mut Document, fresh: Option<Layer>, above_active: bool) {
        let (path, id) = match self.discard(doc) {
            Some((path, old)) => (path, Some(old.id)),
            None => (insert_path(doc, above_active), None),
        };
        let Some(mut layer) = fresh else {
            return;
        };
        if let Some(id) = id {
            layer.id = id;
        }
        doc.add_damage(layer.content_bounds());
        self.layer = Some(layer.id);
        doc.tree.insert_at(&path, layer);
    }

    /// Lift the preview out of the tree, undamaging its pixels, and hand
    /// it back with the position it occupied.
    pub(crate) fn discard(&mut self, doc: &mut Document) -> Option<(LayerPath, Layer)> {
        let id = self.layer.take()?;
        let (path, layer) = doc.tree.remove(id)?;
        doc.add_damage(layer.content_bounds());
        Some((path, layer))
    }

    /// Turn the preview into the committed layer.
    pub(crate) fn commit(&mut self, doc: &mut Document, edit: &str) {
        if let Some((path, layer)) = self.discard(doc) {
            commit_layer(doc, layer, path, edit);
        }
    }
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
    /// Line thickness, Photoshop's "Weight". Independent of the brush.
    weight: f32,
    /// Photoshop's tool mode: a live shape layer, or plain pixels.
    vector: bool,
    arrow_start: bool,
    arrow_end: bool,
    anchor: Option<(f32, f32)>,
    current: Option<(f32, f32)>,
    square: bool,
    preview: DragPreview,
}

impl ShapeTool {
    fn new(kind: ShapeKind) -> ShapeTool {
        ShapeTool {
            kind,
            sides: 5,
            weight: 1.0,
            vector: true,
            arrow_start: false,
            arrow_end: false,
            anchor: None,
            current: None,
            square: false,
            preview: DragPreview::default(),
        }
    }

    /// The layer this drag would commit: a live shape layer in Shape mode,
    /// plain selection-clipped pixels otherwise.
    fn build_layer(
        &self,
        doc: &Document,
        from: (f32, f32),
        to: (f32, f32),
        colour: Rgba,
    ) -> Option<Layer> {
        if self.vector {
            // A live shape layer: the path is kept and the pixels are
            // derived from it, so it stays editable and stays sharp.
            if let Some(shape) = self.vector_shape(from, to, colour) {
                return Some(shape_layer(doc, shape, self.kind.label()));
            }
        }
        let path = self.path_for(from, to);
        // Stroke outlines self-overlap at joins, so every shape fills with
        // the nonzero rule.
        rasterized_layer(doc, &path, colour, FillRule::NonZero, self.kind.label())
    }

    /// Show the shape as it stands at `to`, once the drag is a drag.
    fn update_preview(&mut self, ctx: &mut ToolCtx, to: (f32, f32)) {
        let Some(anchor) = self.anchor else {
            return;
        };
        if (to.0 - anchor.0).abs() < 0.5 && (to.1 - anchor.1).abs() < 0.5 {
            // Still a click; a click commits nothing, so it previews
            // nothing either.
            self.preview.show(ctx.doc, None, !self.vector);
            return;
        }
        let fresh = self.build_layer(ctx.doc, anchor, to, ctx.state.foreground);
        self.preview.show(ctx.doc, fresh, !self.vector);
    }

    /// Where the drag actually ends, after Shift constrains it. Rectangles,
    /// ellipses and polygons constrain to a square bounding box; a line
    /// constrains its angle to the nearest 45 degrees, like Photoshop.
    fn constrained(&self, from: (f32, f32), to: (f32, f32)) -> (f32, f32) {
        if !self.square || self.kind != ShapeKind::Line {
            return to;
        }
        let (dx, dy) = (to.0 - from.0, to.1 - from.1);
        let len = dx.hypot(dy);
        if len < 1e-6 {
            return to;
        }
        let step = std::f32::consts::FRAC_PI_4;
        let angle = (dy.atan2(dx) / step).round() * step;
        (from.0 + len * angle.cos(), from.1 + len * angle.sin())
    }

    /// Arrowhead as a triangle at `tip`, pointing away from `from`.
    fn arrow_head(&self, from: (f32, f32), tip: (f32, f32)) -> Option<Vec<(f32, f32)>> {
        let (dx, dy) = (tip.0 - from.0, tip.1 - from.1);
        let len = dx.hypot(dy);
        if len < 1e-6 {
            return None;
        }
        let (ux, uy) = (dx / len, dy / len);
        // Photoshop's defaults: length 300% and width 500% of the weight.
        let along = (self.weight * 3.0).max(3.0);
        let across = (self.weight * 2.5).max(2.5);
        let base = (tip.0 - ux * along, tip.1 - uy * along);
        let (nx, ny) = (-uy * across, ux * across);
        Some(vec![
            tip,
            (base.0 + nx, base.1 + ny),
            (base.0 - nx, base.1 - ny),
        ])
    }

    /// The shape as vector anchors rather than a flattened path, for the
    /// Shape mode. Returns `None` for shapes with no vector form here.
    fn vector_shape(
        &self,
        from: (f32, f32),
        to: (f32, f32),
        colour: Rgba,
    ) -> Option<schist_core::VectorShape> {
        let to = self.constrained(from, to);
        let r = drag_rect(from.0, from.1, to.0, to.1, self.square);
        let (l, t, rr, b) = (r.left as f32, r.top as f32, r.right as f32, r.bottom as f32);
        let mut path = schist_core::VectorPath::new(self.kind.label());
        match self.kind {
            ShapeKind::Rectangle => {
                path.subpaths.push(schist_core::SubPath {
                    anchors: vec![
                        schist_core::Anchor::corner(l, t),
                        schist_core::Anchor::corner(rr, t),
                        schist_core::Anchor::corner(rr, b),
                        schist_core::Anchor::corner(l, b),
                    ],
                    closed: true,
                });
            }
            ShapeKind::Ellipse => {
                // Four cubic arcs, the standard circle-to-Bezier fit.
                const K: f32 = 0.552_284_8;
                let (cx, cy) = ((l + rr) / 2.0, (t + b) / 2.0);
                let (rx, ry) = ((rr - l) / 2.0, (b - t) / 2.0);
                let (ox, oy) = (rx * K, ry * K);
                path.subpaths.push(schist_core::SubPath {
                    anchors: vec![
                        schist_core::Anchor::smooth(cx, t, ox, 0.0),
                        schist_core::Anchor::smooth(rr, cy, 0.0, oy),
                        schist_core::Anchor::smooth(cx, b, -ox, 0.0),
                        schist_core::Anchor::smooth(l, cy, 0.0, -oy),
                    ],
                    closed: true,
                });
            }
            ShapeKind::Polygon => {
                let n = self.sides.max(3) as usize;
                let (cx, cy) = ((l + rr) / 2.0, (t + b) / 2.0);
                let (rx, ry) = ((rr - l) / 2.0, (b - t) / 2.0);
                path.subpaths.push(schist_core::SubPath {
                    anchors: (0..n)
                        .map(|i| {
                            // Start at the top, as Photoshop's polygon does.
                            let a = -std::f32::consts::FRAC_PI_2
                                + i as f32 * std::f32::consts::TAU / n as f32;
                            schist_core::Anchor::corner(cx + rx * a.cos(), cy + ry * a.sin())
                        })
                        .collect(),
                    closed: true,
                });
            }
            ShapeKind::Line => {
                // The line itself is an open subpath, so it contributes
                // nothing to the fill (a two-point ring has no area) and
                // everything to the stroke. Arrowheads are closed
                // subpaths, so they are the other way round.
                path.push_open_anchors(vec![
                    schist_core::Anchor::corner(from.0, from.1),
                    schist_core::Anchor::corner(to.0, to.1),
                ]);
                if self.arrow_start {
                    if let Some(head) = self.arrow_head(to, from) {
                        path.subpaths.push(schist_core::SubPath {
                            anchors: head
                                .iter()
                                .map(|(x, y)| schist_core::Anchor::corner(*x, *y))
                                .collect(),
                            closed: true,
                        });
                    }
                }
                if self.arrow_end {
                    if let Some(head) = self.arrow_head(from, to) {
                        path.subpaths.push(schist_core::SubPath {
                            anchors: head
                                .iter()
                                .map(|(x, y)| schist_core::Anchor::corner(*x, *y))
                                .collect(),
                            closed: true,
                        });
                    }
                }
                let mut shape = schist_core::VectorShape::new(path, colour);
                shape.stroke = Some((colour, self.weight));
                return Some(shape);
            }
        }
        if path.is_empty() {
            return None;
        }
        Some(schist_core::VectorShape::new(path, colour))
    }

    fn path_for(&self, from: (f32, f32), to: (f32, f32)) -> Path {
        let to = self.constrained(from, to);
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
                // Butt caps: a line should end exactly where the drag did.
                let mut path = schist_vector::stroke_path(
                    &b.build(0.25),
                    schist_vector::StrokeStyle::new(self.weight)
                        .with_cap(schist_vector::LineCap::Butt),
                );
                if self.arrow_start {
                    if let Some(head) = self.arrow_head(to, from) {
                        path.push_closed(head);
                    }
                }
                if self.arrow_end {
                    if let Some(head) = self.arrow_head(from, to) {
                        path.push_closed(head);
                    }
                }
                return path;
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

    fn description(&self) -> &'static str {
        match self.kind {
            ShapeKind::Rectangle => {
                "Drag out a rectangle shape layer in the foreground colour; it stays vector \
                 and re-rasterizes when resized."
            }
            ShapeKind::Ellipse => "Drag out an ellipse shape layer in the foreground colour.",
            ShapeKind::Line => "Drag out a straight line shape layer in the foreground colour.",
            ShapeKind::Polygon => {
                "Drag out a regular polygon shape layer in the foreground colour, with the \
                 number of sides from the options."
            }
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

    fn group(&self) -> &'static str {
        "shape"
    }

    fn on_pointer_down(&mut self, _ctx: &mut ToolCtx, input: PointerInput) {
        self.anchor = Some((input.x, input.y));
        self.current = Some((input.x, input.y));
        self.square = input.modifiers.shift;
    }

    fn on_pointer_move(&mut self, ctx: &mut ToolCtx, input: PointerInput) {
        if self.anchor.is_none() {
            return;
        }
        let to = (input.x, input.y);
        let unchanged = self.current == Some(to) && self.square == input.modifiers.shift;
        self.current = Some(to);
        self.square = input.modifiers.shift;
        if !unchanged || !self.preview.is_showing() {
            self.update_preview(ctx, to);
        }
    }

    fn on_pointer_up(&mut self, ctx: &mut ToolCtx, input: PointerInput) {
        if self.anchor.is_none() {
            return;
        }
        // The release usually lands where the last move did, in which case
        // the preview already is the layer to commit.
        self.on_pointer_move(ctx, input);
        self.anchor = None;
        self.current = None;
        let name = self.kind.label();
        let edit = if self.vector {
            format!("{name} Layer")
        } else {
            name.to_string()
        };
        self.preview.commit(ctx.doc, &edit);
    }

    fn on_cancel(&mut self, ctx: &mut ToolCtx) {
        self.anchor = None;
        self.current = None;
        self.preview.discard(ctx.doc);
    }

    fn on_deactivate(&mut self, ctx: &mut ToolCtx) {
        self.on_cancel(ctx);
    }

    fn options(&self) -> Vec<ToolOption> {
        let mut out = vec![ToolOption::choice(
            "shape-mode",
            "Mode",
            &["Shape", "Pixels"],
            (!self.vector) as usize,
        )];
        out.extend(match self.kind {
            ShapeKind::Line => vec![
                ToolOption::slider("shape-weight", "Weight", self.weight, 1.0, 100.0, " px"),
                ToolOption::toggle("shape-arrow-start", "Start", self.arrow_start),
                ToolOption::toggle("shape-arrow-end", "End", self.arrow_end),
            ],
            ShapeKind::Polygon => vec![ToolOption::slider(
                "shape-sides",
                "Sides",
                self.sides as f32,
                3.0,
                24.0,
                "",
            )],
            _ => Vec::new(),
        });
        out
    }

    fn set_option(&mut self, key: &str, value: OptionValue) {
        match key {
            "shape-mode" => self.vector = value.index() == 0,
            "shape-weight" => self.weight = value.num().clamp(1.0, 100.0),
            "shape-arrow-start" => self.arrow_start = value.bool(),
            "shape-arrow-end" => self.arrow_end = value.bool(),
            "shape-sides" => self.sides = value.num().round().clamp(3.0, 24.0) as u32,
            _ => {}
        }
    }

    fn overlays(&self, _doc: &Document, _state: &EditorState) -> Vec<Overlay> {
        // The shape itself is on the canvas while it is dragged; the
        // overlay is just the box it is being fitted into, the way
        // Affinity outlines a shape's bounds mid-drag. A line has no
        // useful box.
        let (Some(a), Some(c)) = (self.anchor, self.current) else {
            return Vec::new();
        };
        if self.kind == ShapeKind::Line {
            return Vec::new();
        }
        vec![Overlay::Rect(drag_rect(a.0, a.1, c.0, c.1, self.square))]
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
    /// Commit as a live shape layer rather than painting the fill in.
    vector: bool,
    /// Preview the segment from the last anchor to the cursor.
    rubber_band: bool,
}

impl PenTool {
    /// The anchors as an editable path, for the shape-layer commit. The
    /// pen already stores a point and an outgoing handle per anchor, which
    /// is exactly what a smooth anchor is.
    fn vector_shape(&self, colour: Rgba) -> Option<schist_core::VectorShape> {
        if self.anchors.len() < 3 {
            return None;
        }
        let mut path = schist_core::VectorPath::new("Path");
        path.subpaths.push(schist_core::SubPath {
            anchors: self
                .anchors
                .iter()
                .map(|((x, y), (hx, hy))| schist_core::Anchor::smooth(*x, *y, *hx, *hy))
                .collect(),
            closed: true,
        });
        Some(schist_core::VectorShape::new(path, colour))
    }

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
    fn description(&self) -> &'static str {
        "Build a path: click for a corner point, drag to pull curve handles out of it, and \
         click the first point to close. Commit turns the path into a shape layer."
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
        let color = ctx.state.foreground;
        if self.vector {
            if let Some(shape) = self.vector_shape(color) {
                self.anchors.clear();
                self.dragging = false;
                commit_shape_layer(ctx.doc, shape, "Path");
                return;
            }
        }
        let path = self.build_path(true);
        self.anchors.clear();
        self.dragging = false;
        commit_shape(ctx.doc, &path, color, FillRule::NonZero, "Path Fill");
    }

    fn options(&self) -> Vec<ToolOption> {
        vec![
            ToolOption::choice(
                "pen-mode",
                "Mode",
                &["Shape", "Pixels"],
                usize::from(!self.vector),
            ),
            ToolOption::toggle("pen-rubber-band", "Rubber Band", self.rubber_band),
        ]
    }

    fn set_option(&mut self, key: &str, value: OptionValue) {
        match key {
            "pen-mode" => self.vector = value.index() == 0,
            "pen-rubber-band" => self.rubber_band = value.bool(),
            _ => {}
        }
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
        if !self.rubber_band {
            return out;
        }
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
        "schist.tools-vector"
    }

    fn register(&self, registry: &mut PluginRegistry) {
        registry.register_tool(Box::new(PenTool::default()));
        registry.register_tool(Box::new(paths::FreeformPenTool::new(false)));
        registry.register_tool(Box::new(paths::FreeformPenTool::new(true)));
        registry.register_tool(Box::new(paths::PathSelectTool::new(paths::ArrowKind::Path)));
        registry.register_tool(Box::new(paths::PathSelectTool::new(
            paths::ArrowKind::Direct,
        )));
        registry.register_tool(Box::new(ShapeTool::new(ShapeKind::Rectangle)));
        registry.register_tool(Box::new(ShapeTool::new(ShapeKind::Ellipse)));
        registry.register_tool(Box::new(ShapeTool::new(ShapeKind::Line)));
        registry.register_tool(Box::new(ShapeTool::new(ShapeKind::Polygon)));
        registry.register_tool(Box::new(paths::CustomShapeTool::new()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use schist_color::Depth;
    use schist_core::SelectOp;
    use schist_plugin_api::Modifiers;

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
    fn shape_shows_on_the_canvas_while_it_is_dragged() {
        let mut doc = doc();
        let mut state = red();
        let mut tool = ShapeTool::new(ShapeKind::Ellipse);
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        tool.on_pointer_down(&mut ctx, input(10.0, 10.0));
        assert_eq!(ctx.doc.tree.layers.len(), 1, "a click previews nothing");
        tool.on_pointer_move(&mut ctx, input(50.0, 50.0));
        assert_eq!(ctx.doc.tree.layers.len(), 2, "the drag is on the canvas");
        assert_eq!(top_px(ctx.doc, 30, 30), [255, 0, 0, 255]);
        assert_eq!(top_px(ctx.doc, 12, 12)[3], 0, "an ellipse, not its box");
        assert!(!ctx.doc.take_damage().is_empty(), "the preview repaints");

        // Dragging on rebuilds the same layer rather than piling up new ones.
        let id = ctx.doc.tree.layers[1].id;
        tool.on_pointer_move(&mut ctx, input(90.0, 90.0));
        assert_eq!(ctx.doc.tree.layers.len(), 2);
        assert_eq!(ctx.doc.tree.layers[1].id, id);
        assert_eq!(top_px(ctx.doc, 50, 50), [255, 0, 0, 255]);

        tool.on_pointer_up(&mut ctx, input(90.0, 90.0));
        assert_eq!(doc.tree.layers.len(), 2);
        assert!(
            doc.tree.layers[1].shape.is_some(),
            "committed as a live shape"
        );
        assert_eq!(doc.undo().as_deref(), Some("Ellipse Layer"));
        assert_eq!(doc.tree.layers.len(), 1, "one edit for the whole drag");
        assert_eq!(doc.undo(), None, "the preview left nothing in history");
    }

    #[test]
    fn cancelling_a_drag_removes_the_preview() {
        let mut doc = doc();
        let mut state = red();
        let mut tool = ShapeTool::new(ShapeKind::Rectangle);
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        tool.on_pointer_down(&mut ctx, input(10.0, 10.0));
        tool.on_pointer_move(&mut ctx, input(50.0, 50.0));
        assert_eq!(ctx.doc.tree.layers.len(), 2);
        tool.on_cancel(&mut ctx);
        assert_eq!(ctx.doc.tree.layers.len(), 1);
        assert_eq!(doc.undo(), None);
        // Nor does a drag that ends back where it started leave anything.
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        tool.on_pointer_down(&mut ctx, input(10.0, 10.0));
        tool.on_pointer_move(&mut ctx, input(50.0, 50.0));
        tool.on_pointer_up(&mut ctx, input(10.0, 10.0));
        assert_eq!(doc.tree.layers.len(), 1);
    }

    #[test]
    fn pixel_mode_previews_the_clipped_pixels() {
        let mut doc = doc();
        doc.selection.activate();
        doc.selection
            .apply_shape(IntRect::new(0, 0, 50, 100), SelectOp::Replace, |_, _| 255);
        let mut state = red();
        let mut tool = ShapeTool::new(ShapeKind::Rectangle);
        tool.set_option("shape-mode", OptionValue::Choice(1));
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        tool.on_pointer_down(&mut ctx, input(10.0, 10.0));
        tool.on_pointer_move(&mut ctx, input(90.0, 90.0));
        assert_eq!(ctx.doc.tree.layers.len(), 2);
        assert_eq!(top_px(ctx.doc, 30, 30), [255, 0, 0, 255]);
        assert_eq!(top_px(ctx.doc, 70, 70)[3], 0, "outside the selection");
        assert!(ctx.doc.tree.layers[1].shape.is_none());
        tool.on_pointer_up(&mut ctx, input(90.0, 90.0));
        assert_eq!(doc.undo().as_deref(), Some("Rectangle"));
        assert_eq!(doc.tree.layers.len(), 1);
    }

    #[test]
    fn custom_shape_previews_while_dragged() {
        let mut doc = doc();
        let mut state = red();
        let mut tool = paths::CustomShapeTool::new();
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        tool.on_pointer_down(&mut ctx, input(10.0, 10.0));
        tool.on_pointer_move(&mut ctx, input(90.0, 90.0));
        assert_eq!(ctx.doc.tree.layers.len(), 2);
        assert_eq!(top_px(ctx.doc, 50, 50), [255, 0, 0, 255]);
        tool.on_cancel(&mut ctx);
        assert_eq!(doc.tree.layers.len(), 1);
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
    fn line_tool_strokes_at_its_own_weight() {
        let mut doc = doc();
        // Weight is the line tool's own setting; the brush size is a red
        // herring and must not affect it.
        let mut state = EditorState {
            brush_size: 60.0,
            ..red()
        };
        let mut tool = ShapeTool::new(ShapeKind::Line);
        tool.set_option("shape-weight", OptionValue::Num(6.0));
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
    fn line_tool_has_no_hole_at_the_start() {
        // The stroker used to cancel the start cap against the shaft.
        let mut doc = doc();
        let mut state = red();
        let mut tool = ShapeTool::new(ShapeKind::Line);
        tool.set_option("shape-weight", OptionValue::Num(10.0));
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        tool.on_pointer_down(&mut ctx, input(20.0, 50.0));
        tool.on_pointer_move(&mut ctx, input(80.0, 50.0));
        tool.on_pointer_up(&mut ctx, input(80.0, 50.0));
        assert_eq!(top_px(&doc, 22, 50)[3], 255, "just inside the start");
        assert_eq!(top_px(&doc, 50, 50)[3], 255, "middle of the shaft");
        assert_eq!(top_px(&doc, 78, 50)[3], 255, "just inside the end");
    }

    #[test]
    fn shift_constrains_a_line_to_45_degrees() {
        let mut doc = doc();
        let mut state = red();
        let mut tool = ShapeTool::new(ShapeKind::Line);
        tool.set_option("shape-weight", OptionValue::Num(4.0));
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        // A shallow drag: 60 across, 8 down. With shift it must flatten.
        let shift = Modifiers {
            shift: true,
            ..Default::default()
        };
        tool.on_pointer_down(
            &mut ctx,
            PointerInput {
                modifiers: shift,
                ..input(10.0, 50.0)
            },
        );
        tool.on_pointer_up(
            &mut ctx,
            PointerInput {
                modifiers: shift,
                ..input(70.0, 58.0)
            },
        );
        // Snapped to horizontal, so the far end sits at y = 50, not 58.
        assert!(top_px(&doc, 68, 50)[3] > 0, "line snapped to horizontal");
        assert_eq!(top_px(&doc, 68, 58)[3], 0, "not left on the raw angle");
    }

    #[test]
    fn line_tool_can_draw_an_arrowhead() {
        fn draw(arrow: bool) -> Document {
            let mut doc = doc();
            let mut state = red();
            let mut tool = ShapeTool::new(ShapeKind::Line);
            tool.set_option("shape-weight", OptionValue::Num(4.0));
            if arrow {
                tool.set_option("shape-arrow-end", OptionValue::Bool(true));
            }
            let mut ctx = ToolCtx {
                doc: &mut doc,
                state: &mut state,
            };
            tool.on_pointer_down(&mut ctx, input(20.0, 50.0));
            tool.on_pointer_up(&mut ctx, input(80.0, 50.0));
            doc
        }
        // The bare line is 4px wide, so nothing reaches this far off-axis.
        assert_eq!(top_px(&draw(false), 74, 54)[3], 0, "no arrowhead");
        assert!(top_px(&draw(true), 74, 54)[3] > 0, "arrowhead flares out");
    }

    #[test]
    fn shapes_in_pixels_mode_respect_the_selection() {
        let mut doc = doc();
        doc.selection
            .select_rect(IntRect::from_xywh(0, 0, 30, 100), SelectOp::Replace);
        let mut state = red();
        let mut tool = ShapeTool::new(ShapeKind::Rectangle);
        // Pixels mode is clipped by the selection, as painting is.
        tool.set_option("shape-mode", OptionValue::Choice(1));
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
    fn a_shape_layer_is_not_clipped_by_the_selection() {
        // Photoshop's shape layers carry their own vector mask and ignore
        // the selection; only Pixels mode is clipped by it. Both are
        // tested so the difference is deliberate rather than accidental.
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
        tool.on_pointer_up(&mut ctx, input(60.0, 60.0));
        assert_eq!(top_px(&doc, 20, 20)[3], 255, "inside the selection");
        assert_eq!(
            top_px(&doc, 45, 20)[3],
            255,
            "a shape layer should not be clipped by the selection"
        );
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
