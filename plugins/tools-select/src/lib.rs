//! Selection tools: marquees, the three lassos, magic wand, quick
//! selection and object selection.
//!
//! Modifier convention (Photoshop): Shift = add to selection, Alt =
//! subtract, Shift+Alt = intersect, no modifier = replace.

use schist_core::{
    Document, IntRect, LayerKind, SelectOp, Selection, TileCoord, TileMap, TILE_SIZE,
};
use schist_plugin_api::{
    EditorState, Modifiers, OptionValue, Overlay, PluginManifest, PluginRegistry, PointerInput,
    ToolCtx, ToolOption, ToolPlugin,
};

/// Write a set of pixel coordinates into a selection under `op`.
///
/// Shared by every tool that produces a pixel set rather than a shape:
/// the wand, quick selection and object selection.
fn commit_pixels(ctx: &mut ToolCtx, pixels: &[(i32, i32)], op: SelectOp, name: &str) {
    if pixels.is_empty() {
        return;
    }
    let mut edit = ctx.doc.begin_edit(name.to_string());
    edit.change_selection(|sel, _| {
        if op == SelectOp::Replace {
            sel.deselect();
        }
        let effective = if op == SelectOp::Replace {
            SelectOp::Add
        } else {
            op
        };
        if effective == SelectOp::Intersect {
            let keep: std::collections::HashSet<(i32, i32)> = pixels.iter().copied().collect();
            let coords: Vec<_> = sel.mask.iter().map(|(c, _)| *c).collect();
            for coord in coords {
                let rect = coord.rect();
                let buf = sel.mask.get_mut_or_insert(coord);
                for ly in 0..TILE_SIZE {
                    for lx in 0..TILE_SIZE {
                        let ix = (ly * TILE_SIZE + lx) as usize;
                        if buf[ix] > 0 && !keep.contains(&(rect.left + lx, rect.top + ly)) {
                            buf[ix] = 0;
                        }
                    }
                }
            }
        } else {
            for &(px, py) in pixels {
                let coord = TileCoord::containing(px, py);
                let buf = sel.mask.get_mut_or_insert(coord);
                let lx = px.rem_euclid(TILE_SIZE) as usize;
                let ly = py.rem_euclid(TILE_SIZE) as usize;
                let ix = ly * TILE_SIZE as usize + lx;
                match effective {
                    SelectOp::Add => buf[ix] = 255,
                    SelectOp::Subtract => buf[ix] = 0,
                    _ => {}
                }
            }
        }
        sel.activate();
        sel.recompute_bounds();
    });
    edit.commit();
}

/// The active layer's pixels, if it has any.
fn active_raster(doc: &Document) -> Option<&schist_core::RasterLayer> {
    let layer = doc.active_layer.and_then(|id| doc.tree.find(id))?;
    match &layer.kind {
        LayerKind::Raster(r) => Some(r),
        _ => None,
    }
}

/// The four ways a new shape can meet the existing selection, in the order
/// Photoshop's buttons sit in.
const SELECT_MODES: &[&str] = &["New", "Add", "Subtract", "Intersect"];

fn mode_from_index(i: usize) -> SelectOp {
    match i {
        1 => SelectOp::Add,
        2 => SelectOp::Subtract,
        3 => SelectOp::Intersect,
        _ => SelectOp::Replace,
    }
}

fn mode_index(op: SelectOp) -> usize {
    match op {
        SelectOp::Replace => 0,
        SelectOp::Add => 1,
        SelectOp::Subtract => 2,
        SelectOp::Intersect => 3,
    }
}

/// Modifiers win over the options bar, and the bar decides what an
/// unmodified drag does -- which is how Photoshop's selection tools work.
fn op_from(modifiers: Modifiers, base: SelectOp) -> SelectOp {
    match (modifiers.shift, modifiers.alt) {
        (true, true) => SelectOp::Intersect,
        (true, false) => SelectOp::Add,
        (false, true) => SelectOp::Subtract,
        (false, false) => base,
    }
}

/// Draw a shape into `sel`, softening the shape's own edge first.
///
/// Feather belongs to the shape you just drew, not to the result. The
/// difference shows the moment you add to an existing selection:
/// feathering afterwards would soften the boundary of the whole thing,
/// including edges you had already settled.
fn commit_shape(
    sel: &mut Selection,
    feather: f32,
    op: SelectOp,
    draw: impl Fn(&mut Selection, SelectOp),
) {
    if feather < 0.5 {
        draw(sel, op);
        return;
    }
    let mut shape = Selection::new();
    draw(&mut shape, SelectOp::Replace);
    shape.feather(feather);
    if shape.is_empty() {
        return;
    }
    let bounds = shape.bounds();
    sel.apply_shape(bounds, op, |x, y| shape.coverage(x, y));
}

fn drag_rect(ax: f32, ay: f32, bx: f32, by: f32, square: bool) -> IntRect {
    let (mut w, mut h) = (bx - ax, by - ay);
    if square {
        let m = w.abs().max(h.abs());
        w = m * w.signum();
        h = m * h.signum();
    }
    let (x1, x2) = if w < 0.0 { (ax + w, ax) } else { (ax, ax + w) };
    let (y1, y2) = if h < 0.0 { (ay + h, ay) } else { (ay, ay + h) };
    IntRect::new(
        x1.round() as i32,
        y1.round() as i32,
        x2.round() as i32,
        y2.round() as i32,
    )
}

/// The ellipse inscribed in `rect`, as a closed polygon in document
/// space, for the drag overlay to trace with marching ants.
///
/// Segment count follows the rect's size so a small ellipse costs little
/// and a large one still reads as a curve rather than a polygon.
fn ellipse_points(rect: IntRect) -> Vec<(f32, f32)> {
    let rx = rect.width() as f32 / 2.0;
    let ry = rect.height() as f32 / 2.0;
    let cx = rect.left as f32 + rx;
    let cy = rect.top as f32 + ry;
    let steps = ((rx + ry) as usize).clamp(24, 256);
    (0..=steps)
        .map(|i| {
            let a = i as f32 / steps as f32 * std::f32::consts::TAU;
            (cx + rx * a.cos(), cy + ry * a.sin())
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MarqueeShape {
    Rect,
    Ellipse,
}

pub struct MarqueeTool {
    shape: MarqueeShape,
    /// What an unmodified drag does.
    mode: SelectOp,
    /// Radius the new shape's edge is softened by, in pixels.
    feather: f32,
    anchor: Option<(f32, f32, Modifiers)>,
    current: Option<IntRect>,
}

impl MarqueeTool {
    fn new(shape: MarqueeShape) -> Self {
        MarqueeTool {
            shape,
            mode: SelectOp::Replace,
            feather: 0.0,
            anchor: None,
            current: None,
        }
    }
}

impl ToolPlugin for MarqueeTool {
    fn id(&self) -> &'static str {
        match self.shape {
            MarqueeShape::Rect => "marquee.rect",
            MarqueeShape::Ellipse => "marquee.ellipse",
        }
    }

    fn name(&self) -> &'static str {
        match self.shape {
            MarqueeShape::Rect => "Rectangular Marquee",
            MarqueeShape::Ellipse => "Elliptical Marquee",
        }
    }

    fn icon(&self) -> &'static str {
        match self.shape {
            MarqueeShape::Rect => "marquee-rect",
            MarqueeShape::Ellipse => "marquee-ellipse",
        }
    }

    fn shortcut(&self) -> Option<&'static str> {
        match self.shape {
            MarqueeShape::Rect => Some("m"),
            MarqueeShape::Ellipse => None,
        }
    }

    fn group(&self) -> &'static str {
        "marquee"
    }

    fn on_pointer_down(&mut self, _ctx: &mut ToolCtx, input: PointerInput) {
        self.anchor = Some((input.x, input.y, input.modifiers));
        self.current = None;
    }

    fn on_pointer_move(&mut self, _ctx: &mut ToolCtx, input: PointerInput) {
        if let Some((ax, ay, m)) = self.anchor {
            // Shift is overloaded (add-to-selection at press time, square
            // constraint during drag) — use press-time modifiers for the op
            // and live shift for the constraint, like Photoshop.
            let square = input.modifiers.shift && !m.shift;
            self.current = Some(drag_rect(ax, ay, input.x, input.y, square));
        }
    }

    fn on_pointer_up(&mut self, ctx: &mut ToolCtx, input: PointerInput) {
        let Some((ax, ay, m)) = self.anchor.take() else {
            return;
        };
        let rect = self
            .current
            .take()
            .unwrap_or_else(|| drag_rect(ax, ay, input.x, input.y, false));
        let op = op_from(m, self.mode);
        if rect.is_empty() {
            // Click without drag: deselect (Photoshop behavior).
            if op == SelectOp::Replace {
                let mut edit = ctx.doc.begin_edit("Deselect");
                edit.change_selection(|sel, _| sel.deselect());
                edit.commit();
            }
            return;
        }
        let shape = self.shape;
        let feather = self.feather;
        let mut edit = ctx.doc.begin_edit("Select");
        edit.change_selection(|sel, _| {
            commit_shape(sel, feather, op, |target, op| match shape {
                MarqueeShape::Rect => target.select_rect(rect, op),
                MarqueeShape::Ellipse => target.select_ellipse(rect, op),
            })
        });
        edit.commit();
    }

    fn on_cancel(&mut self, _ctx: &mut ToolCtx) {
        self.anchor = None;
        self.current = None;
    }

    fn options(&self) -> Vec<ToolOption> {
        vec![
            ToolOption::choice("marquee-mode", "Mode", SELECT_MODES, mode_index(self.mode)),
            ToolOption::slider(
                "marquee-feather",
                "Feather",
                self.feather,
                0.0,
                250.0,
                " px",
            ),
        ]
    }

    fn set_option(&mut self, key: &str, value: OptionValue) {
        match key {
            "marquee-mode" => self.mode = mode_from_index(value.index()),
            "marquee-feather" => self.feather = value.num().max(0.0),
            _ => {}
        }
    }

    fn overlays(&self, _doc: &Document, _state: &EditorState) -> Vec<Overlay> {
        // The overlay shows the shape that will be committed, so the
        // elliptical marquee traces its ellipse rather than the rect it
        // was dragged out of.
        match (self.current, self.shape) {
            (Some(rect), MarqueeShape::Rect) => vec![Overlay::AntsRect(rect)],
            (Some(rect), MarqueeShape::Ellipse) if !rect.is_empty() => {
                vec![Overlay::AntsPolygon(ellipse_points(rect))]
            }
            _ => Vec::new(),
        }
    }
}

/// Which of Photoshop's three lassos this instance is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LassoKind {
    /// Drag to trace freehand.
    Free,
    /// Click to drop corners; click the first point, double-click or press
    /// Enter to close.
    Polygonal,
    /// Drag near an edge and the path snaps to it.
    Magnetic,
}

pub struct LassoTool {
    kind: LassoKind,
    points: Vec<(f32, f32)>,
    /// Where the cursor is, so an in-progress polygon can show its next
    /// edge before it is committed.
    cursor: Option<(f32, f32)>,
    modifiers: Modifiers,
    /// Magnetic: how far either side of the path to look for an edge.
    width: f32,
    /// Magnetic: how strong an edge has to be to attract the path.
    contrast: f32,
    /// Magnetic: pixels between automatically dropped anchor points.
    frequency: f32,
    /// What an unmodified drag does.
    mode: SelectOp,
    /// Radius the new shape's edge is softened by, in pixels.
    feather: f32,
}

impl LassoTool {
    fn new(kind: LassoKind) -> Self {
        LassoTool {
            kind,
            points: Vec::new(),
            cursor: None,
            modifiers: Modifiers::default(),
            width: 10.0,
            contrast: 10.0,
            frequency: 8.0,
            mode: SelectOp::Replace,
            feather: 0.0,
        }
    }

    fn commit(&mut self, ctx: &mut ToolCtx) {
        let points = std::mem::take(&mut self.points);
        self.cursor = None;
        if points.len() < 3 {
            return;
        }
        let op = op_from(self.modifiers, self.mode);
        let name = match self.kind {
            LassoKind::Free => "Lasso Select",
            LassoKind::Polygonal => "Polygonal Lasso",
            LassoKind::Magnetic => "Magnetic Lasso",
        };
        let mut edit = ctx.doc.begin_edit(name);
        let feather = self.feather;
        edit.change_selection(|sel, _| {
            commit_shape(sel, feather, op, |target, op| {
                target.select_polygon(&points, op)
            })
        });
        edit.commit();
    }

    /// True when `p` is close enough to the first anchor to close the path.
    fn closes_at(&self, p: (f32, f32)) -> bool {
        match self.points.first() {
            Some(first) if self.points.len() >= 3 => (first.0 - p.0).hypot(first.1 - p.1) <= 6.0,
            _ => false,
        }
    }

    /// Slide `to` onto the strongest edge near the straight line from the
    /// last anchor, which is what makes the magnetic lasso feel magnetic.
    fn snap_to_edge(&self, doc: &Document, to: (f32, f32)) -> (f32, f32) {
        let Some(raster) = active_raster(doc) else {
            return to;
        };
        let Some(&from) = self.points.last() else {
            return to;
        };
        let (dx, dy) = (to.0 - from.0, to.1 - from.1);
        let len = dx.hypot(dy);
        if len < 1e-3 {
            return to;
        }
        // Search perpendicular to the direction of travel.
        let (nx, ny) = (-dy / len, dx / len);
        let reach = self.width.max(1.0);
        let mut best = (to, -1.0f32);
        let mut t = -reach;
        while t <= reach {
            let p = (to.0 + nx * t, to.1 + ny * t);
            let g = gradient_at(raster, p.0 as i32, p.1 as i32);
            // Prefer strong edges, but break ties towards the cursor.
            let score = g - (t.abs() / reach) * self.contrast * 0.5;
            if g >= self.contrast && score > best.1 {
                best = (p, score);
            }
            t += 1.0;
        }
        best.0
    }
}

/// Sobel-ish edge strength at a pixel, 0..=255-ish.
fn gradient_at(raster: &schist_core::RasterLayer, x: i32, y: i32) -> f32 {
    let lum = |x: i32, y: i32| {
        let p = raster.tiles.pixel(x, y);
        (0.299 * p.r + 0.587 * p.g + 0.114 * p.b) * p.a * 255.0
    };
    let gx = lum(x + 1, y) - lum(x - 1, y);
    let gy = lum(x, y + 1) - lum(x, y - 1);
    gx.hypot(gy)
}

impl ToolPlugin for LassoTool {
    fn id(&self) -> &'static str {
        match self.kind {
            LassoKind::Free => "lasso",
            LassoKind::Polygonal => "lasso.polygonal",
            LassoKind::Magnetic => "lasso.magnetic",
        }
    }
    fn name(&self) -> &'static str {
        match self.kind {
            LassoKind::Free => "Lasso",
            LassoKind::Polygonal => "Polygonal Lasso",
            LassoKind::Magnetic => "Magnetic Lasso",
        }
    }
    fn icon(&self) -> &'static str {
        match self.kind {
            LassoKind::Free => "lasso",
            LassoKind::Polygonal => "lasso-poly",
            LassoKind::Magnetic => "lasso-magnetic",
        }
    }
    fn shortcut(&self) -> Option<&'static str> {
        matches!(self.kind, LassoKind::Free).then_some("l")
    }
    fn group(&self) -> &'static str {
        "lasso"
    }

    fn options(&self) -> Vec<ToolOption> {
        let mut opts = vec![
            ToolOption::choice("lasso-mode", "Mode", SELECT_MODES, mode_index(self.mode)),
            ToolOption::slider("lasso-feather", "Feather", self.feather, 0.0, 250.0, " px"),
        ];
        if self.kind == LassoKind::Magnetic {
            opts.extend([
                ToolOption::slider("lasso-width", "Width", self.width, 1.0, 40.0, " px"),
                ToolOption::slider("lasso-contrast", "Contrast", self.contrast, 1.0, 100.0, ""),
                ToolOption::slider(
                    "lasso-frequency",
                    "Frequency",
                    self.frequency,
                    1.0,
                    100.0,
                    "",
                ),
            ]);
        }
        opts
    }

    fn set_option(&mut self, key: &str, value: OptionValue) {
        match key {
            "lasso-mode" => self.mode = mode_from_index(value.index()),
            "lasso-feather" => self.feather = value.num().max(0.0),
            "lasso-width" => self.width = value.num(),
            "lasso-contrast" => self.contrast = value.num(),
            "lasso-frequency" => self.frequency = value.num(),
            _ => {}
        }
    }

    fn on_pointer_down(&mut self, ctx: &mut ToolCtx, input: PointerInput) {
        let p = (input.x, input.y);
        match self.kind {
            LassoKind::Free => {
                self.modifiers = input.modifiers;
                self.points = vec![p];
            }
            LassoKind::Polygonal | LassoKind::Magnetic => {
                if self.points.is_empty() {
                    // First click of a new path: the modifiers that were
                    // held then decide how it combines.
                    self.modifiers = input.modifiers;
                    self.points.push(p);
                } else if self.closes_at(p) {
                    self.commit(ctx);
                } else {
                    self.points.push(p);
                }
            }
        }
    }

    fn on_pointer_move(&mut self, ctx: &mut ToolCtx, input: PointerInput) {
        let p = (input.x, input.y);
        self.cursor = Some(p);
        match self.kind {
            LassoKind::Free => {
                if !self.points.is_empty() {
                    self.points.push(p);
                }
            }
            LassoKind::Polygonal => {}
            LassoKind::Magnetic => {
                if self.points.is_empty() {
                    return;
                }
                // Drop an anchor every `frequency` pixels, snapped to the
                // nearest strong edge.
                let last = *self.points.last().unwrap();
                if (last.0 - p.0).hypot(last.1 - p.1) >= self.frequency.max(1.0) {
                    let snapped = self.snap_to_edge(ctx.doc, p);
                    self.points.push(snapped);
                }
            }
        }
    }

    fn on_pointer_up(&mut self, ctx: &mut ToolCtx, _input: PointerInput) {
        // Only the freehand lasso finishes on release; the others keep
        // collecting anchors until the path is closed or committed.
        if self.kind == LassoKind::Free {
            self.commit(ctx);
        }
    }

    fn on_commit(&mut self, ctx: &mut ToolCtx) {
        self.commit(ctx);
    }

    fn on_cancel(&mut self, _ctx: &mut ToolCtx) {
        self.points.clear();
        self.cursor = None;
    }

    fn on_deactivate(&mut self, _ctx: &mut ToolCtx) {
        self.points.clear();
        self.cursor = None;
    }

    fn overlays(&self, _doc: &Document, _state: &EditorState) -> Vec<Overlay> {
        if self.points.len() < 2 && self.cursor.is_none() {
            return Vec::new();
        }
        let mut pts = self.points.clone();
        // Show the edge that would be added by clicking where the cursor is.
        if self.kind != LassoKind::Free {
            if let Some(c) = self.cursor {
                if !pts.is_empty() {
                    pts.push(c);
                }
            }
        }
        if pts.len() < 2 {
            return Vec::new();
        }
        vec![Overlay::AntsPolygon(pts)]
    }
}

/// Magic wand: contiguous flood fill on the active layer by color distance.
pub struct WandTool {
    /// 0..=255 max per-channel distance.
    pub tolerance: u8,
    /// Off selects every matching pixel, not just the connected blob.
    pub contiguous: bool,
    /// What an unmodified click does.
    mode: SelectOp,
}

impl WandTool {
    fn new() -> Self {
        WandTool {
            tolerance: 32,
            contiguous: true,
            mode: SelectOp::Replace,
        }
    }
}

fn wand_select(
    doc: &Document,
    x: i32,
    y: i32,
    tolerance: u8,
    contiguous: bool,
) -> Option<Vec<(i32, i32)>> {
    let layer = doc.active_layer.and_then(|id| doc.tree.find(id))?;
    let LayerKind::Raster(raster) = &layer.kind else {
        return None;
    };
    let canvas = doc.canvas_rect();
    if !canvas.contains(x, y) {
        return None;
    }
    let target = raster.tiles.pixel(x, y).to_u8();
    let tol = tolerance as i32;
    let matches = |px: [u8; 4]| -> bool {
        px.iter()
            .zip(target.iter())
            .all(|(&a, &b)| (a as i32 - b as i32).abs() <= tol)
    };
    if !contiguous {
        // Global match: every pixel within tolerance, connected or not.
        let mut out = Vec::new();
        for py in canvas.top..canvas.bottom {
            for px in canvas.left..canvas.right {
                if matches(raster.tiles.pixel(px, py).to_u8()) {
                    out.push((px, py));
                }
            }
        }
        return Some(out);
    }
    let w = canvas.width() as usize;
    let mut visited = vec![false; w * canvas.height() as usize];
    let mut out = Vec::new();
    let mut stack = vec![(x, y)];
    visited[y as usize * w + x as usize] = true;
    while let Some((cx, cy)) = stack.pop() {
        out.push((cx, cy));
        for (nx, ny) in [(cx - 1, cy), (cx + 1, cy), (cx, cy - 1), (cx, cy + 1)] {
            if !canvas.contains(nx, ny) {
                continue;
            }
            let ix = ny as usize * w + nx as usize;
            if visited[ix] {
                continue;
            }
            visited[ix] = true;
            if matches(raster.tiles.pixel(nx, ny).to_u8()) {
                stack.push((nx, ny));
            }
        }
    }
    Some(out)
}

impl ToolPlugin for WandTool {
    fn id(&self) -> &'static str {
        "wand"
    }
    fn name(&self) -> &'static str {
        "Magic Wand"
    }
    fn icon(&self) -> &'static str {
        "wand"
    }
    fn shortcut(&self) -> Option<&'static str> {
        Some("w")
    }

    fn on_pointer_down(&mut self, ctx: &mut ToolCtx, input: PointerInput) {
        let x = input.x.floor() as i32;
        let y = input.y.floor() as i32;
        let Some(pixels) = wand_select(ctx.doc, x, y, self.tolerance, self.contiguous) else {
            return;
        };
        let op = op_from(input.modifiers, self.mode);
        commit_pixels(ctx, &pixels, op, "Magic Wand");
    }

    fn options(&self) -> Vec<ToolOption> {
        vec![
            ToolOption::choice("wand-mode", "Mode", SELECT_MODES, mode_index(self.mode)),
            ToolOption::slider(
                "wand-tolerance",
                "Tolerance",
                self.tolerance as f32,
                0.0,
                255.0,
                "",
            ),
            ToolOption::toggle("wand-contiguous", "Contiguous", self.contiguous),
        ]
    }

    fn set_option(&mut self, key: &str, value: OptionValue) {
        match key {
            "wand-mode" => self.mode = mode_from_index(value.index()),
            "wand-tolerance" => self.tolerance = value.num().round().clamp(0.0, 255.0) as u8,
            "wand-contiguous" => self.contiguous = value.bool(),
            _ => {}
        }
    }

    fn on_pointer_move(&mut self, _ctx: &mut ToolCtx, _input: PointerInput) {}
    fn on_pointer_up(&mut self, _ctx: &mut ToolCtx, _input: PointerInput) {}
}

/// Quick Selection: paint over a region and it grows to fill whatever
/// looks like the same material.
///
/// Photoshop's version is a graph cut. This is a flood fill seeded by the
/// brush stroke that matches against the running mean colour of everything
/// already selected, which handles gradients and texture far better than
/// the wand's fixed reference pixel while staying honest about what it is.
pub struct QuickSelectTool {
    radius: f32,
    tolerance: f32,
    /// Alt-dragging removes from the selection, as in Photoshop.
    subtract: bool,
    dragging: bool,
    /// Running mean of the colours the stroke has accepted, and how many.
    seed: [f64; 3],
    seen: u32,
    /// Everything the current stroke has selected, so each dab extends the
    /// same region rather than restarting.
    stroke: std::collections::HashSet<(i32, i32)>,
}

impl QuickSelectTool {
    fn new() -> Self {
        QuickSelectTool {
            radius: 20.0,
            tolerance: 28.0,
            subtract: false,
            dragging: false,
            seed: [0.0; 3],
            seen: 0,
            stroke: std::collections::HashSet::new(),
        }
    }

    /// Grow the region from a dab centred on (x, y).
    fn grow(&mut self, doc: &Document, x: i32, y: i32) -> Vec<(i32, i32)> {
        let Some(raster) = active_raster(doc) else {
            return Vec::new();
        };
        let canvas = doc.canvas_rect();
        let r = self.radius.max(1.0) as i32;
        let mut stack = Vec::new();
        // Seed with the dab itself, learning its colours as we go.
        for dy in -r..=r {
            for dx in -r..=r {
                if dx * dx + dy * dy > r * r {
                    continue;
                }
                let (px, py) = (x + dx, y + dy);
                if !canvas.contains(px, py) {
                    continue;
                }
                let c = raster.tiles.pixel(px, py);
                if c.a <= 0.0 {
                    continue;
                }
                self.seed[0] += c.r as f64;
                self.seed[1] += c.g as f64;
                self.seed[2] += c.b as f64;
                self.seen += 1;
                if self.stroke.insert((px, py)) {
                    stack.push((px, py));
                }
            }
        }
        if self.seen == 0 {
            return Vec::new();
        }
        let mean = [
            (self.seed[0] / self.seen as f64) as f32,
            (self.seed[1] / self.seen as f64) as f32,
            (self.seed[2] / self.seen as f64) as f32,
        ];
        let tol = self.tolerance / 255.0;
        // Flood outwards while pixels still look like the mean.
        let mut added: Vec<(i32, i32)> = stack.clone();
        while let Some((cx, cy)) = stack.pop() {
            for (nx, ny) in [(cx - 1, cy), (cx + 1, cy), (cx, cy - 1), (cx, cy + 1)] {
                if !canvas.contains(nx, ny) || self.stroke.contains(&(nx, ny)) {
                    continue;
                }
                let c = raster.tiles.pixel(nx, ny);
                let d = (c.r - mean[0])
                    .abs()
                    .max((c.g - mean[1]).abs())
                    .max((c.b - mean[2]).abs());
                if d <= tol && c.a > 0.0 {
                    self.stroke.insert((nx, ny));
                    added.push((nx, ny));
                    stack.push((nx, ny));
                }
            }
        }
        added
    }
}

impl ToolPlugin for QuickSelectTool {
    fn id(&self) -> &'static str {
        "quick_select"
    }
    fn name(&self) -> &'static str {
        "Quick Selection"
    }
    fn icon(&self) -> &'static str {
        "quick-select"
    }
    fn shortcut(&self) -> Option<&'static str> {
        None
    }
    fn group(&self) -> &'static str {
        "wand"
    }

    fn options(&self) -> Vec<ToolOption> {
        vec![
            ToolOption::slider("qs-size", "Size", self.radius, 1.0, 120.0, " px"),
            ToolOption::slider("qs-tolerance", "Tolerance", self.tolerance, 1.0, 128.0, ""),
        ]
    }

    fn set_option(&mut self, key: &str, value: OptionValue) {
        match key {
            "qs-size" => self.radius = value.num(),
            "qs-tolerance" => self.tolerance = value.num(),
            _ => {}
        }
    }

    fn on_pointer_down(&mut self, ctx: &mut ToolCtx, input: PointerInput) {
        self.dragging = true;
        self.subtract = input.modifiers.alt;
        self.seed = [0.0; 3];
        self.seen = 0;
        self.stroke.clear();
        let added = self.grow(ctx.doc, input.x as i32, input.y as i32);
        // A plain drag starts a new selection; shift extends the old one.
        let op = if self.subtract {
            SelectOp::Subtract
        } else if input.modifiers.shift {
            SelectOp::Add
        } else {
            SelectOp::Replace
        };
        commit_pixels(ctx, &added, op, "Quick Selection");
    }

    fn on_pointer_move(&mut self, ctx: &mut ToolCtx, input: PointerInput) {
        if !self.dragging {
            return;
        }
        let added = self.grow(ctx.doc, input.x as i32, input.y as i32);
        let op = if self.subtract {
            SelectOp::Subtract
        } else {
            SelectOp::Add
        };
        commit_pixels(ctx, &added, op, "Quick Selection");
    }

    fn on_pointer_up(&mut self, _ctx: &mut ToolCtx, _input: PointerInput) {
        self.dragging = false;
        self.stroke.clear();
    }

    fn on_cancel(&mut self, _ctx: &mut ToolCtx) {
        self.dragging = false;
        self.stroke.clear();
    }
}

/// Object Selection: drag a box around something and it finds the object
/// inside it.
///
/// Photoshop CC 2020 does this with a segmentation model, and so does
/// this, when one is installed: U^2-Net separates the subject of a
/// picture from its background, and a box around an object is a picture
/// whose subject is that object -- which is why the network is given a
/// crop rather than the layer. Its answer comes back soft and at the
/// resolution it thinks in, so the boundary is settled against the
/// picture's own colours before it becomes a selection.
///
/// Without the model it falls back to the older reading: the border of
/// the drawn box is a sample of the background, and what is kept is
/// whatever inside does not look like it. That handles a subject against
/// a distinguishable background, which is most of what the tool is used
/// for, and it is also what runs when the network looks at the box and
/// finds nothing in it.
pub struct ObjectSelectTool {
    anchor: Option<(f32, f32)>,
    current: Option<(f32, f32)>,
    modifiers: Modifiers,
    tolerance: f32,
    /// What an unmodified drag does.
    mode: SelectOp,
}

impl ObjectSelectTool {
    fn new() -> Self {
        ObjectSelectTool {
            anchor: None,
            current: None,
            modifiers: Modifiers::default(),
            mode: SelectOp::Replace,
            tolerance: 24.0,
        }
    }

    fn find_object(&self, doc: &Document, rect: IntRect) -> Vec<(i32, i32)> {
        let Some(raster) = active_raster(doc) else {
            return Vec::new();
        };
        let canvas = doc.canvas_rect();
        let rect = rect.intersect(&canvas);
        if rect.width() < 3 || rect.height() < 3 {
            return Vec::new();
        }
        let mask = object_by_model(&raster.tiles, rect, canvas)
            .unwrap_or_else(|| self.object_by_colour(raster, rect));
        let w = rect.width() as usize;
        let mut out = Vec::new();
        for y in 0..rect.height() as usize {
            for x in 0..w {
                if mask[y * w + x] {
                    out.push((rect.left + x as i32, rect.top + y as i32));
                }
            }
        }
        out
    }

    /// The older reading of a box: everything inside it that does not
    /// look like its border. A mask over `rect`.
    fn object_by_colour(&self, raster: &schist_core::RasterLayer, rect: IntRect) -> Vec<bool> {
        let (w, h) = (rect.width() as usize, rect.height() as usize);
        // Sample the box's border as "background".
        let mut bg: Vec<[f32; 3]> = Vec::new();
        for x in rect.left..rect.right {
            for y in [rect.top, rect.bottom - 1] {
                let c = raster.tiles.pixel(x, y);
                bg.push([c.r, c.g, c.b]);
            }
        }
        for y in rect.top..rect.bottom {
            for x in [rect.left, rect.right - 1] {
                let c = raster.tiles.pixel(x, y);
                bg.push([c.r, c.g, c.b]);
            }
        }
        if bg.is_empty() {
            return vec![false; w * h];
        }
        let tol = self.tolerance / 255.0;
        let looks_like_bg = |c: [f32; 3]| {
            bg.iter().any(|b| {
                (c[0] - b[0])
                    .abs()
                    .max((c[1] - b[1]).abs())
                    .max((c[2] - b[2]).abs())
                    <= tol
            })
        };

        let mut fg = vec![false; w * h];
        for y in 0..h {
            for x in 0..w {
                let c = raster
                    .tiles
                    .pixel(rect.left + x as i32, rect.top + y as i32);
                fg[y * w + x] = c.a > 0.0 && !looks_like_bg([c.r, c.g, c.b]);
            }
        }
        despeckle(&mut fg, w, h);
        fg
    }
}

/// Fills pinholes and drops isolated speckle: a pixel joins the majority
/// of the nine it sits in the middle of.
fn despeckle(mask: &mut [bool], w: usize, h: usize) {
    let was = mask.to_vec();
    for y in 1..h.saturating_sub(1) {
        for x in 1..w.saturating_sub(1) {
            let mut on = 0;
            for dy in -1i32..=1 {
                for dx in -1i32..=1 {
                    let sx = (x as i32 + dx) as usize;
                    let sy = (y as i32 + dy) as usize;
                    on += was[sy * w + sx] as u32;
                }
            }
            mask[y * w + x] = on >= 5;
        }
    }
}

/// How much bigger than the drawn box the network is shown.
///
/// A box is a promise that the object is inside it and says nothing
/// about what is outside -- and "what is outside" is exactly what a
/// segmentation network separates an object from, so it is given some.
const OBJECT_MARGIN: f32 = 0.3;

/// A network's answer for the object inside `rect`, as a mask over
/// `rect`.
///
/// `None` means "no answer": there is no model installed, it failed, or
/// it looked at the box and found nothing in it. All three want the
/// colour path instead, and none of them want an empty selection.
fn object_by_model(tiles: &TileMap, rect: IntRect, canvas: IntRect) -> Option<Vec<bool>> {
    let model = schist_neural::get("segment")?;
    let grow = (rect.width().max(rect.height()) as f32 * OBJECT_MARGIN) as i32;
    let win = IntRect::new(
        rect.left - grow,
        rect.top - grow,
        rect.right + grow,
        rect.bottom + grow,
    )
    .intersect(&canvas);
    let (ww, wh) = (win.width().max(0) as usize, win.height().max(0) as usize);
    if ww < 8 || wh < 8 {
        return None;
    }
    let mut rgb = Vec::with_capacity(ww * wh * 3);
    for y in 0..wh {
        for x in 0..ww {
            let c = tiles.pixel(win.left + x as i32, win.top + y as i32);
            rgb.extend_from_slice(&[c.r, c.g, c.b]);
        }
    }
    let map = schist_neural::segment(&model, &rgb, ww, wh)
        .map_err(|e| log::warn!("object selection: {e:#}"))
        .ok()?;

    // Confidence is read inside the drawn box only. The margin was for
    // the network's benefit and may well contain the rest of a subject
    // the user deliberately did not put in the box.
    let (w, h) = (rect.width() as usize, rect.height() as usize);
    let (ox, oy) = (
        (rect.left - win.left) as usize,
        (rect.top - win.top) as usize,
    );
    let at = |x: usize, y: usize| map[(oy + y) * ww + ox + x];
    let peak = (0..h)
        .flat_map(|y| (0..w).map(move |x| (x, y)))
        .fold(0.0f32, |m, (x, y)| m.max(at(x, y)));
    if peak < 0.35 {
        return None;
    }
    // Half of what it was willing to commit to, so a confident answer is
    // cut at the usual half-probability and a hesitant one is still cut
    // somewhere down its own slope rather than everywhere or nowhere.
    let cut = (peak * 0.5).clamp(0.2, 0.5);

    // The map is a resample of a 320-pixel grid, so its boundary is
    // several pixels of ramp wherever the box was bigger than that.
    // Settle that band against the picture: the two sides of a real
    // edge are two colours, and each uncertain pixel belongs to the one
    // it is nearer.
    let band = 0.2f32;
    let colour = |x: usize, y: usize| {
        let c = tiles.pixel(rect.left + x as i32, rect.top + y as i32);
        [c.r, c.g, c.b]
    };
    let (mut inside, mut outside, mut n_in, mut n_out) = ([0f32; 3], [0f32; 3], 0f32, 0f32);
    for y in 0..h {
        for x in 0..w {
            let v = at(x, y);
            let c = colour(x, y);
            if v > cut + band {
                for i in 0..3 {
                    inside[i] += c[i];
                }
                n_in += 1.0;
            } else if v < cut - band {
                for i in 0..3 {
                    outside[i] += c[i];
                }
                n_out += 1.0;
            }
        }
    }
    let refine = n_in > 0.0 && n_out > 0.0;
    let far =
        |c: [f32; 3], m: [f32; 3], n: f32| (0..3).map(|i| (c[i] - m[i] / n).powi(2)).sum::<f32>();

    let mut mask = vec![false; w * h];
    for y in 0..h {
        for x in 0..w {
            let v = at(x, y);
            // Nothing is not an object, whatever the network makes of
            // the colour it reads under a transparent pixel.
            let there = tiles.pixel(rect.left + x as i32, rect.top + y as i32).a > 0.0;
            mask[y * w + x] = there
                && match refine && (v - cut).abs() <= band {
                    true => far(colour(x, y), inside, n_in) < far(colour(x, y), outside, n_out),
                    false => v > cut,
                };
        }
    }
    despeckle(&mut mask, w, h);

    // A handful of pixels is not an object; it is the network hedging,
    // and the colour path will do better with the same box.
    let found = mask.iter().filter(|&&v| v).count();
    match found * 200 >= w * h {
        true => Some(mask),
        false => None,
    }
}

impl ToolPlugin for ObjectSelectTool {
    fn id(&self) -> &'static str {
        "object_select"
    }
    fn name(&self) -> &'static str {
        "Object Selection"
    }
    fn icon(&self) -> &'static str {
        "object-select"
    }
    fn group(&self) -> &'static str {
        "wand"
    }

    fn options(&self) -> Vec<ToolOption> {
        vec![
            ToolOption::choice("os-mode", "Mode", SELECT_MODES, mode_index(self.mode)),
            ToolOption::slider("os-tolerance", "Tolerance", self.tolerance, 1.0, 128.0, ""),
        ]
    }

    fn set_option(&mut self, key: &str, value: OptionValue) {
        match key {
            "os-mode" => self.mode = mode_from_index(value.index()),
            "os-tolerance" => self.tolerance = value.num(),
            _ => {}
        }
    }

    fn on_pointer_down(&mut self, _ctx: &mut ToolCtx, input: PointerInput) {
        self.anchor = Some((input.x, input.y));
        self.current = Some((input.x, input.y));
        self.modifiers = input.modifiers;
    }

    fn on_pointer_move(&mut self, _ctx: &mut ToolCtx, input: PointerInput) {
        if self.anchor.is_some() {
            self.current = Some((input.x, input.y));
        }
    }

    fn on_pointer_up(&mut self, ctx: &mut ToolCtx, input: PointerInput) {
        let Some(a) = self.anchor.take() else { return };
        self.current = None;
        let rect = IntRect::new(
            a.0.min(input.x).floor() as i32,
            a.1.min(input.y).floor() as i32,
            a.0.max(input.x).ceil() as i32,
            a.1.max(input.y).ceil() as i32,
        );
        let pixels = self.find_object(ctx.doc, rect);
        commit_pixels(
            ctx,
            &pixels,
            op_from(self.modifiers, self.mode),
            "Object Selection",
        );
    }

    fn on_cancel(&mut self, _ctx: &mut ToolCtx) {
        self.anchor = None;
        self.current = None;
    }

    fn overlays(&self, _doc: &Document, _state: &EditorState) -> Vec<Overlay> {
        match (self.anchor, self.current) {
            (Some(a), Some(c)) => vec![Overlay::AntsRect(IntRect::new(
                a.0.min(c.0) as i32,
                a.1.min(c.1) as i32,
                a.0.max(c.0) as i32,
                a.1.max(c.1) as i32,
            ))],
            _ => Vec::new(),
        }
    }
}

pub struct SelectToolsPlugin;

impl PluginManifest for SelectToolsPlugin {
    fn id(&self) -> &'static str {
        "schist.tools-select"
    }

    fn register(&self, registry: &mut PluginRegistry) {
        registry.register_tool(Box::new(MarqueeTool::new(MarqueeShape::Rect)));
        registry.register_tool(Box::new(MarqueeTool::new(MarqueeShape::Ellipse)));
        registry.register_tool(Box::new(LassoTool::new(LassoKind::Free)));
        registry.register_tool(Box::new(LassoTool::new(LassoKind::Polygonal)));
        registry.register_tool(Box::new(LassoTool::new(LassoKind::Magnetic)));
        registry.register_tool(Box::new(WandTool::new()));
        registry.register_tool(Box::new(QuickSelectTool::new()));
        registry.register_tool(Box::new(ObjectSelectTool::new()));
    }
}

#[allow(unused_imports)]
use schist_pixel_ops as _;

#[cfg(test)]
mod tests {
    use super::*;
    use schist_color::Depth;
    use schist_core::{blit_rgba8, Layer};

    fn input(x: f32, y: f32, m: Modifiers) -> PointerInput {
        PointerInput {
            x,
            y,
            pressure: 1.0,
            modifiers: m,
        }
    }

    fn drag(
        tool: &mut dyn ToolPlugin,
        ctx: &mut ToolCtx,
        from: (f32, f32),
        to: (f32, f32),
        m: Modifiers,
    ) {
        tool.on_pointer_down(ctx, input(from.0, from.1, m));
        tool.on_pointer_move(ctx, input(to.0, to.1, m));
        tool.on_pointer_up(ctx, input(to.0, to.1, m));
    }

    #[test]
    fn the_mode_option_decides_what_an_unmodified_drag_does() {
        let mut doc = Document::new("t", 200, 200, Depth::Eight);
        let mut state = EditorState::default();
        let mut tool = MarqueeTool::new(MarqueeShape::Rect);
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        let plain = Modifiers::default();

        drag(&mut tool, &mut ctx, (10.0, 10.0), (50.0, 50.0), plain);
        // Add: the first selection survives the second drag.
        tool.set_option("marquee-mode", OptionValue::Choice(1));
        drag(&mut tool, &mut ctx, (100.0, 100.0), (140.0, 140.0), plain);
        assert_eq!(ctx.doc.selection.coverage(20, 20), 255, "the first stays");
        assert_eq!(
            ctx.doc.selection.coverage(120, 120),
            255,
            "the second joins"
        );

        // Back to New: the next drag is on its own again.
        tool.set_option("marquee-mode", OptionValue::Choice(0));
        drag(&mut tool, &mut ctx, (150.0, 10.0), (190.0, 50.0), plain);
        assert_eq!(ctx.doc.selection.coverage(20, 20), 0, "the old one is gone");
        assert_eq!(ctx.doc.selection.coverage(170, 30), 255);
    }

    #[test]
    fn a_modifier_still_overrides_the_mode() {
        let mut doc = Document::new("t", 200, 200, Depth::Eight);
        let mut state = EditorState::default();
        let mut tool = MarqueeTool::new(MarqueeShape::Rect);
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        drag(
            &mut tool,
            &mut ctx,
            (10.0, 10.0),
            (50.0, 50.0),
            Modifiers::default(),
        );
        // Mode says Add, but alt still means subtract.
        tool.set_option("marquee-mode", OptionValue::Choice(1));
        let alt = Modifiers {
            alt: true,
            ..Modifiers::default()
        };
        drag(&mut tool, &mut ctx, (20.0, 20.0), (60.0, 60.0), alt);
        assert_eq!(ctx.doc.selection.coverage(30, 30), 0, "alt subtracted");
        assert_eq!(ctx.doc.selection.coverage(12, 12), 255, "the rest stayed");
    }

    #[test]
    fn feather_softens_the_new_shape_not_the_whole_selection() {
        let plain = Modifiers::default();
        let mut doc = Document::new("t", 200, 200, Depth::Eight);
        let mut state = EditorState::default();
        let mut tool = MarqueeTool::new(MarqueeShape::Rect);
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };

        // A hard rectangle first, with no feather set.
        drag(&mut tool, &mut ctx, (10.0, 10.0), (60.0, 60.0), plain);
        assert_eq!(ctx.doc.selection.coverage(11, 35), 255, "hard edge");

        // Now add a feathered one elsewhere. Its own edge softens...
        tool.set_option("marquee-feather", OptionValue::Num(8.0));
        tool.set_option("marquee-mode", OptionValue::Choice(1));
        drag(&mut tool, &mut ctx, (100.0, 100.0), (160.0, 160.0), plain);
        let edge = ctx.doc.selection.coverage(101, 130);
        assert!(
            edge > 0 && edge < 255,
            "the feathered edge should be partial, got {edge}"
        );
        assert_eq!(
            ctx.doc.selection.coverage(130, 130),
            255,
            "centre stays full"
        );

        // ...and the rectangle that was already there keeps its hard edge.
        assert_eq!(
            ctx.doc.selection.coverage(11, 35),
            255,
            "feather must not reach back into what was already selected"
        );
    }

    #[test]
    fn marquee_replace_add_subtract() {
        let mut doc = Document::new("t", 200, 200, Depth::Eight);
        let mut state = EditorState::default();
        let mut tool = MarqueeTool::new(MarqueeShape::Rect);
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };

        drag(
            &mut tool,
            &mut ctx,
            (10.0, 10.0),
            (50.0, 50.0),
            Modifiers::default(),
        );
        assert_eq!(ctx.doc.selection.coverage(20, 20), 255);
        assert_eq!(ctx.doc.selection.coverage(60, 60), 0);

        // Shift-drag adds.
        drag(
            &mut tool,
            &mut ctx,
            (100.0, 100.0),
            (150.0, 150.0),
            Modifiers {
                shift: true,
                ..Default::default()
            },
        );
        assert_eq!(ctx.doc.selection.coverage(20, 20), 255, "kept");
        assert_eq!(ctx.doc.selection.coverage(120, 120), 255, "added");

        // Alt-drag subtracts.
        drag(
            &mut tool,
            &mut ctx,
            (10.0, 10.0),
            (30.0, 30.0),
            Modifiers {
                alt: true,
                ..Default::default()
            },
        );
        assert_eq!(ctx.doc.selection.coverage(20, 20), 0, "subtracted");
        assert_eq!(ctx.doc.selection.coverage(40, 40), 255, "rest kept");

        // Selections are undoable.
        ctx.doc.undo();
        assert_eq!(ctx.doc.selection.coverage(20, 20), 255);
    }

    #[test]
    fn marquee_click_deselects() {
        let mut doc = Document::new("t", 100, 100, Depth::Eight);
        let mut state = EditorState::default();
        let mut tool = MarqueeTool::new(MarqueeShape::Rect);
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        drag(
            &mut tool,
            &mut ctx,
            (10.0, 10.0),
            (50.0, 50.0),
            Modifiers::default(),
        );
        assert!(!ctx.doc.selection.is_empty());
        // Plain click.
        tool.on_pointer_down(&mut ctx, input(80.0, 80.0, Modifiers::default()));
        tool.on_pointer_up(&mut ctx, input(80.0, 80.0, Modifiers::default()));
        assert!(ctx.doc.selection.is_empty());
    }

    #[test]
    fn lasso_selects_polygon() {
        let mut doc = Document::new("t", 100, 100, Depth::Eight);
        let mut state = EditorState::default();
        let mut tool = LassoTool::new(LassoKind::Free);
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        tool.on_pointer_down(&mut ctx, input(10.0, 10.0, Modifiers::default()));
        for p in [(90.0, 10.0), (90.0, 90.0), (10.0, 90.0)] {
            tool.on_pointer_move(&mut ctx, input(p.0, p.1, Modifiers::default()));
        }
        tool.on_pointer_up(&mut ctx, input(10.0, 90.0, Modifiers::default()));
        assert_eq!(ctx.doc.selection.coverage(50, 50), 255);
        assert_eq!(ctx.doc.selection.coverage(5, 5), 0);
    }

    #[test]
    fn wand_selects_contiguous_color() {
        let mut doc = Document::new("t", 64, 64, Depth::Eight);
        let mut layer = Layer::new_raster("bg");
        // Left half red, right half blue.
        let mut buf = vec![0u8; 64 * 64 * 4];
        for y in 0..64 {
            for x in 0..64 {
                let i = (y * 64 + x) * 4;
                if x < 32 {
                    buf[i..i + 4].copy_from_slice(&[255, 0, 0, 255]);
                } else {
                    buf[i..i + 4].copy_from_slice(&[0, 0, 255, 255]);
                }
            }
        }
        blit_rgba8(
            &mut layer.as_raster_mut().unwrap().tiles,
            Depth::Eight,
            IntRect::from_size(64, 64),
            &buf,
        );
        doc.push_layer(layer);

        let mut state = EditorState::default();
        let mut tool = WandTool::new();
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        tool.on_pointer_down(&mut ctx, input(10.0, 10.0, Modifiers::default()));
        assert_eq!(ctx.doc.selection.coverage(20, 30), 255, "red side selected");
        assert_eq!(
            ctx.doc.selection.coverage(40, 30),
            0,
            "blue side not selected"
        );
    }
}

#[cfg(test)]
mod new_tool_tests {
    use super::*;
    use schist_color::{Depth, Rgba};
    use schist_core::{Layer, TileCoord};

    fn input(x: f32, y: f32, m: Modifiers) -> PointerInput {
        PointerInput {
            x,
            y,
            pressure: 1.0,
            modifiers: m,
        }
    }

    /// 200x200 white, with a solid blue disc of radius 30 at (100,100).
    fn doc_with_disc() -> Document {
        let mut doc = Document::new("t", 200, 200, Depth::Eight);
        let mut layer = Layer::new_raster("bg");
        {
            let raster = layer.as_raster_mut().unwrap();
            for y in 0..200i32 {
                for x in 0..200i32 {
                    let inside = (x - 100).pow(2) + (y - 100).pow(2) <= 30 * 30;
                    let c = if inside {
                        Rgba::new(0.1, 0.2, 0.9, 1.0)
                    } else {
                        Rgba::new(1.0, 1.0, 1.0, 1.0)
                    };
                    let coord = TileCoord::containing(x, y);
                    let trect = coord.rect();
                    let buf = raster.tiles.get_mut_or_insert(coord, Depth::Eight);
                    let ix = ((y - trect.top) * TILE_SIZE + (x - trect.left)) as usize;
                    buf.set(ix, c);
                }
            }
        }
        doc.push_layer(layer);
        doc
    }

    #[test]
    fn polygonal_lasso_closes_on_the_first_point() {
        let mut doc = Document::new("t", 200, 200, Depth::Eight);
        let mut state = EditorState::default();
        let mut tool = LassoTool::new(LassoKind::Polygonal);
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        let m = Modifiers::default();
        // Three corners, then click back on the first to close.
        for p in [(20.0, 20.0), (80.0, 20.0), (80.0, 80.0)] {
            tool.on_pointer_down(&mut ctx, input(p.0, p.1, m));
            tool.on_pointer_up(&mut ctx, input(p.0, p.1, m));
        }
        assert!(ctx.doc.selection.is_empty(), "closed too early");
        tool.on_pointer_down(&mut ctx, input(21.0, 21.0, m));
        assert_eq!(
            ctx.doc.selection.coverage(50, 30),
            255,
            "polygon did not commit on closing"
        );
        assert_eq!(ctx.doc.selection.coverage(150, 150), 0, "leaked outside");
    }

    #[test]
    fn polygonal_lasso_does_not_finish_on_a_plain_release() {
        let mut doc = Document::new("t", 200, 200, Depth::Eight);
        let mut state = EditorState::default();
        let mut tool = LassoTool::new(LassoKind::Polygonal);
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        let m = Modifiers::default();
        tool.on_pointer_down(&mut ctx, input(20.0, 20.0, m));
        tool.on_pointer_up(&mut ctx, input(20.0, 20.0, m));
        tool.on_pointer_down(&mut ctx, input(80.0, 20.0, m));
        tool.on_pointer_up(&mut ctx, input(80.0, 20.0, m));
        assert!(
            ctx.doc.selection.is_empty(),
            "a two-point path should not have committed"
        );
    }

    #[test]
    fn enter_commits_an_open_polygon() {
        let mut doc = Document::new("t", 200, 200, Depth::Eight);
        let mut state = EditorState::default();
        let mut tool = LassoTool::new(LassoKind::Polygonal);
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        let m = Modifiers::default();
        for p in [(20.0, 20.0), (80.0, 20.0), (80.0, 80.0)] {
            tool.on_pointer_down(&mut ctx, input(p.0, p.1, m));
            tool.on_pointer_up(&mut ctx, input(p.0, p.1, m));
        }
        tool.on_commit(&mut ctx);
        assert_eq!(ctx.doc.selection.coverage(60, 30), 255);
    }

    #[test]
    fn magnetic_lasso_pulls_anchors_onto_the_edge() {
        let mut doc = doc_with_disc();
        let mut state = EditorState::default();
        let mut tool = LassoTool::new(LassoKind::Magnetic);
        tool.set_option("lasso-width", OptionValue::Num(12.0));
        tool.set_option("lasso-contrast", OptionValue::Num(10.0));
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        let m = Modifiers::default();
        // Start on the disc's left edge, then drag along it but 6px off.
        tool.on_pointer_down(&mut ctx, input(70.0, 100.0, m));
        tool.on_pointer_move(&mut ctx, input(74.0, 76.0, m));
        let anchors = tool.points.clone();
        assert!(anchors.len() >= 2, "no anchor was dropped");
        let last = *anchors.last().unwrap();
        // The disc's edge near y=76 is about 24px from the centre in x.
        let dist = ((last.0 - 100.0).hypot(last.1 - 100.0) - 30.0).abs();
        assert!(
            dist < 6.0,
            "anchor at {last:?} is {dist} px off the edge, so it did not snap"
        );
    }

    #[test]
    fn quick_selection_grows_to_fill_the_disc() {
        let mut doc = doc_with_disc();
        let mut state = EditorState::default();
        let mut tool = QuickSelectTool::new();
        tool.set_option("qs-size", OptionValue::Num(4.0));
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        tool.on_pointer_down(&mut ctx, input(100.0, 100.0, Modifiers::default()));
        tool.on_pointer_up(&mut ctx, input(100.0, 100.0, Modifiers::default()));

        assert_eq!(ctx.doc.selection.coverage(100, 100), 255, "centre missed");
        assert_eq!(
            ctx.doc.selection.coverage(100, 75),
            255,
            "did not reach the top of the disc"
        );
        assert_eq!(
            ctx.doc.selection.coverage(100, 60),
            0,
            "spilled past the disc into the background"
        );
    }

    #[test]
    fn the_segmentation_model_cuts_round_the_object_in_the_box() {
        let Some(_) = schist_neural::get("segment") else {
            eprintln!("skipping: the segmentation model is not installed");
            return;
        };
        let doc = doc_with_disc();
        let tiles = &active_raster(&doc).unwrap().tiles;
        let rect = IntRect::new(55, 55, 145, 145);
        let mask = object_by_model(tiles, rect, doc.canvas_rect())
            .expect("the model had something to say about a disc on a field");
        let w = rect.width() as usize;
        let (mut hit, mut miss, mut area) = (0usize, 0usize, 0usize);
        for y in 0..rect.height() as usize {
            for x in 0..w {
                let (gx, gy) = (rect.left + x as i32 - 100, rect.top + y as i32 - 100);
                let inside = gx * gx + gy * gy <= 30 * 30;
                area += inside as usize;
                hit += (inside && mask[y * w + x]) as usize;
                miss += (!inside && mask[y * w + x]) as usize;
            }
        }
        assert!(
            hit * 10 >= area * 9 && miss * 4 < area,
            "cut {hit} of {area} disc pixels and {miss} outside it"
        );
    }

    #[test]
    fn object_selection_falls_back_when_the_model_sees_no_object() {
        // Fine grain with no subject in it. The network has nothing to
        // say, and what has to happen then is the colour path -- not an
        // empty selection, and not a selection of noise.
        let mut doc = Document::new("t", 200, 200, Depth::Eight);
        let mut layer = Layer::new_raster("bg");
        {
            let raster = layer.as_raster_mut().unwrap();
            let mut seed = 0x1234_5678u32;
            for y in 0..200i32 {
                for x in 0..200i32 {
                    seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    let v = 0.5 + (seed >> 8) as f32 / u32::MAX as f32 * 0.06;
                    let coord = TileCoord::containing(x, y);
                    let trect = coord.rect();
                    let buf = raster.tiles.get_mut_or_insert(coord, Depth::Eight);
                    let ix = ((y - trect.top) * TILE_SIZE + (x - trect.left)) as usize;
                    buf.set(ix, Rgba::new(v, v, v, 1.0));
                }
            }
        }
        doc.push_layer(layer);
        let tiles = &active_raster(&doc).unwrap().tiles;
        assert!(
            object_by_model(tiles, IntRect::new(40, 40, 160, 160), doc.canvas_rect()).is_none(),
            "the model claimed to find an object in grain"
        );
    }

    #[test]
    fn object_selection_finds_the_subject_inside_the_box() {
        let mut doc = doc_with_disc();
        let mut state = EditorState::default();
        let mut tool = ObjectSelectTool::new();
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        let m = Modifiers::default();
        // A box with a comfortable margin of background around the disc.
        tool.on_pointer_down(&mut ctx, input(55.0, 55.0, m));
        tool.on_pointer_up(&mut ctx, input(145.0, 145.0, m));

        assert_eq!(ctx.doc.selection.coverage(100, 100), 255, "missed the disc");
        assert_eq!(
            ctx.doc.selection.coverage(60, 60),
            0,
            "selected the background corner too"
        );
        // The edge of the disc should be roughly where the selection ends.
        let b = ctx.doc.selection.bounds();
        assert!(
            b.width() > 50 && b.width() < 70,
            "selection bounds {b:?} do not match the disc"
        );
    }

    #[test]
    fn non_contiguous_wand_matches_across_the_canvas() {
        let mut doc = Document::new("t", 100, 100, Depth::Eight);
        let mut layer = Layer::new_raster("bg");
        {
            let raster = layer.as_raster_mut().unwrap();
            for y in 0..100i32 {
                for x in 0..100i32 {
                    // Two separate red squares on white.
                    let red = (10..30).contains(&x) && (10..30).contains(&y)
                        || (70..90).contains(&x) && (70..90).contains(&y);
                    let c = if red {
                        Rgba::new(1.0, 0.0, 0.0, 1.0)
                    } else {
                        Rgba::new(1.0, 1.0, 1.0, 1.0)
                    };
                    let coord = TileCoord::containing(x, y);
                    let trect = coord.rect();
                    let buf = raster.tiles.get_mut_or_insert(coord, Depth::Eight);
                    let ix = ((y - trect.top) * TILE_SIZE + (x - trect.left)) as usize;
                    buf.set(ix, c);
                }
            }
        }
        doc.push_layer(layer);
        let mut state = EditorState::default();
        let mut tool = WandTool::new();
        tool.set_option("wand-tolerance", OptionValue::Num(10.0));

        // Contiguous: only the square that was clicked.
        {
            let mut ctx = ToolCtx {
                doc: &mut doc,
                state: &mut state,
            };
            tool.on_pointer_down(&mut ctx, input(20.0, 20.0, Modifiers::default()));
        }
        assert_eq!(doc.selection.coverage(20, 20), 255);
        assert_eq!(doc.selection.coverage(80, 80), 0, "should not have reached");

        // Non-contiguous: both.
        tool.set_option("wand-contiguous", OptionValue::Bool(false));
        {
            let mut ctx = ToolCtx {
                doc: &mut doc,
                state: &mut state,
            };
            tool.on_pointer_down(&mut ctx, input(20.0, 20.0, Modifiers::default()));
        }
        assert_eq!(doc.selection.coverage(80, 80), 255, "missed the far square");
    }
}
