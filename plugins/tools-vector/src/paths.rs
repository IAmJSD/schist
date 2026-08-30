//! The path tools: the two selection arrows, the freeform and curvature
//! pens, and custom shapes.
//!
//! These edit `Document::paths` rather than pixels. A path is only
//! rasterized when the user asks -- Fill Path, Stroke Path, or turning it
//! into a selection -- which is the whole reason paths are stored rather
//! than committed on the spot.

use schist_core::{Anchor, Document, IntRect, SubPath, VectorPath};
use schist_plugin_api::{
    EditorState, OptionValue, Overlay, PointerInput, ToolCtx, ToolOption, ToolPlugin,
};
use schist_vector::{Path, PathBuilder};

/// Flatten a stored path for drawing or rasterizing.
pub fn flatten(path: &VectorPath) -> Path {
    let mut b = PathBuilder::new();
    for sub in &path.subpaths {
        if sub.anchors.is_empty() {
            continue;
        }
        let first = sub.anchors[0];
        b.move_to(first.point.0, first.point.1);
        let n = sub.anchors.len();
        let last = if sub.closed { n } else { n - 1 };
        for i in 0..last {
            let a = sub.anchors[i];
            let c = sub.anchors[(i + 1) % n];
            if a.handle_out == (0.0, 0.0) && c.handle_in == (0.0, 0.0) {
                b.line_to(c.point.0, c.point.1);
            } else {
                b.cubic_to(
                    a.point.0 + a.handle_out.0,
                    a.point.1 + a.handle_out.1,
                    c.point.0 + c.handle_in.0,
                    c.point.1 + c.handle_in.1,
                    c.point.0,
                    c.point.1,
                );
            }
        }
        if sub.closed {
            b.close();
        }
    }
    b.build(0.25)
}

/// Overlays for a stored path: its outline, plus handles when `detail`.
fn path_overlays(path: &VectorPath, detail: bool) -> Vec<Overlay> {
    let mut out = Vec::new();
    for sub in &flatten(path).subpaths {
        if sub.len() >= 2 {
            out.push(Overlay::AntsPolygon(sub.clone()));
        }
    }
    if !detail {
        return out;
    }
    for (_, _, a) in path.anchors() {
        let (x, y) = a.point;
        out.push(Overlay::Rect(IntRect::new(
            x as i32 - 3,
            y as i32 - 3,
            x as i32 + 3,
            y as i32 + 3,
        )));
        for h in [a.handle_in, a.handle_out] {
            if h != (0.0, 0.0) {
                out.push(Overlay::Line {
                    x1: x,
                    y1: y,
                    x2: x + h.0,
                    y2: y + h.1,
                });
            }
        }
    }
    out
}

/// The path the arrows edit.
///
/// A shape layer's own path takes precedence over the Paths panel's
/// selection, so clicking a shape and dragging its anchors edits the
/// shape rather than some unrelated stored path.
fn active(doc: &Document) -> Option<&VectorPath> {
    if let Some(shape) = doc
        .active_layer
        .and_then(|id| doc.tree.find(id))
        .and_then(|l| l.shape.as_deref())
    {
        return Some(&shape.path);
    }
    doc.active_path.and_then(|i| doc.paths.get(i))
}

fn active_mut(doc: &mut Document) -> Option<&mut VectorPath> {
    if let Some(id) = doc.active_layer {
        let is_shape = doc.tree.find(id).is_some_and(|l| l.shape.is_some());
        if is_shape {
            return doc
                .tree
                .find_mut(id)
                .and_then(|l| l.shape.as_deref_mut())
                .map(|s| &mut s.path);
        }
    }
    let i = doc.active_path?;
    doc.paths.get_mut(i)
}

/// Add a path to the document and make it the active one.
pub fn store(doc: &mut Document, path: VectorPath) {
    doc.paths.push(path);
    doc.active_path = Some(doc.paths.len() - 1);
}

// ------------------------------------------------------ path selection

/// Whether an arrow moves whole paths or individual anchors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArrowKind {
    /// The black arrow: drags the whole path.
    Path,
    /// The white arrow: drags one anchor or handle.
    Direct,
}

pub struct PathSelectTool {
    kind: ArrowKind,
    last: Option<(f32, f32)>,
    /// Direct selection: which anchor, and whether a handle was grabbed.
    grabbed: Option<(usize, usize, Grab)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Grab {
    Point,
    HandleIn,
    HandleOut,
}

impl PathSelectTool {
    pub fn new(kind: ArrowKind) -> Self {
        PathSelectTool {
            kind,
            last: None,
            grabbed: None,
        }
    }
}

impl ToolPlugin for PathSelectTool {
    fn id(&self) -> &'static str {
        match self.kind {
            ArrowKind::Path => "path_select",
            ArrowKind::Direct => "direct_select",
        }
    }
    fn name(&self) -> &'static str {
        match self.kind {
            ArrowKind::Path => "Path Selection",
            ArrowKind::Direct => "Direct Selection",
        }
    }
    fn description(&self) -> &'static str {
        match self.kind {
            ArrowKind::Path => "Click a path to select it, and drag to move the whole path.",
            ArrowKind::Direct => {
                "Drag an individual anchor point or its handles to reshape the path around it."
            }
        }
    }
    fn icon(&self) -> &'static str {
        match self.kind {
            ArrowKind::Path => "path-select",
            ArrowKind::Direct => "direct-select",
        }
    }
    fn shortcut(&self) -> Option<&'static str> {
        matches!(self.kind, ArrowKind::Path).then_some("a")
    }
    fn group(&self) -> &'static str {
        "path_select"
    }

    fn on_pointer_down(&mut self, ctx: &mut ToolCtx, input: PointerInput) {
        self.last = Some((input.x, input.y));
        let r = 8.0 / ctx.state.zoom.max(0.01);
        if self.kind == ArrowKind::Path {
            // Pick whichever path has an anchor near the click.
            let hit = ctx
                .doc
                .paths
                .iter()
                .position(|p| p.hit_anchor(input.x, input.y, r * 4.0).is_some());
            if hit.is_some() {
                ctx.doc.active_path = hit;
            }
            return;
        }
        let Some(path) = active(ctx.doc) else { return };
        // Handles take precedence: they sit outside the anchor.
        let mut best: Option<(usize, usize, Grab, f32)> = None;
        for (s, a, an) in path.anchors() {
            for (grab, h) in [
                (Grab::HandleIn, an.handle_in),
                (Grab::HandleOut, an.handle_out),
            ] {
                if h == (0.0, 0.0) {
                    continue;
                }
                let (hx, hy) = (an.point.0 + h.0, an.point.1 + h.1);
                let d = (hx - input.x).hypot(hy - input.y);
                if d <= r && best.is_none_or(|(_, _, _, bd)| d < bd) {
                    best = Some((s, a, grab, d));
                }
            }
            let d = (an.point.0 - input.x).hypot(an.point.1 - input.y);
            if d <= r && best.is_none_or(|(_, _, _, bd)| d < bd) {
                best = Some((s, a, Grab::Point, d));
            }
        }
        self.grabbed = best.map(|(s, a, g, _)| (s, a, g));
    }

    fn on_pointer_move(&mut self, ctx: &mut ToolCtx, input: PointerInput) {
        let Some((lx, ly)) = self.last else { return };
        let (dx, dy) = (input.x - lx, input.y - ly);
        self.last = Some((input.x, input.y));
        if dx == 0.0 && dy == 0.0 {
            return;
        }
        match self.kind {
            ArrowKind::Path => {
                if let Some(path) = active_mut(ctx.doc) {
                    path.translate(dx, dy);
                }
            }
            ArrowKind::Direct => {
                let Some((s, a, grab)) = self.grabbed else {
                    return;
                };
                if let Some(path) = active_mut(ctx.doc) {
                    if let Some(an) = path
                        .subpaths
                        .get_mut(s)
                        .and_then(|sub| sub.anchors.get_mut(a))
                    {
                        match grab {
                            Grab::Point => *an = an.translated(dx, dy),
                            // Dragging one handle swings the other with it,
                            // which is what keeps a smooth point smooth.
                            Grab::HandleOut => {
                                an.handle_out = (an.handle_out.0 + dx, an.handle_out.1 + dy);
                                if an.handle_in != (0.0, 0.0) {
                                    an.handle_in = (-an.handle_out.0, -an.handle_out.1);
                                }
                            }
                            Grab::HandleIn => {
                                an.handle_in = (an.handle_in.0 + dx, an.handle_in.1 + dy);
                                if an.handle_out != (0.0, 0.0) {
                                    an.handle_out = (-an.handle_in.0, -an.handle_in.1);
                                }
                            }
                        }
                    }
                }
            }
        }
        ctx.doc.add_damage(ctx.doc.canvas_rect());
    }

    fn on_pointer_up(&mut self, _ctx: &mut ToolCtx, _input: PointerInput) {
        self.last = None;
        self.grabbed = None;
    }

    fn overlays(&self, doc: &Document, _state: &EditorState) -> Vec<Overlay> {
        match active(doc) {
            Some(p) => path_overlays(p, self.kind == ArrowKind::Direct),
            None => Vec::new(),
        }
    }
}

// ------------------------------------------------------------ freeform

/// Drag to draw; the trace is thinned into anchors afterwards.
pub struct FreeformPenTool {
    points: Vec<(f32, f32)>,
    /// Larger values drop more of the trace, so the path is smoother.
    fit: f32,
    /// Curvature pen: click points and the curve is fitted through them.
    curvature: bool,
    drawing: bool,
}

impl FreeformPenTool {
    pub fn new(curvature: bool) -> Self {
        FreeformPenTool {
            points: Vec::new(),
            fit: 2.0,
            curvature,
            drawing: false,
        }
    }

    /// Ramer-Douglas-Peucker: keep only the points that carry the shape.
    fn simplify(points: &[(f32, f32)], tolerance: f32) -> Vec<(f32, f32)> {
        if points.len() < 3 {
            return points.to_vec();
        }
        let (first, last) = (points[0], points[points.len() - 1]);
        let mut worst = (0usize, 0.0f32);
        for (i, p) in points.iter().enumerate().skip(1).take(points.len() - 2) {
            let d = perpendicular_distance(*p, first, last);
            if d > worst.1 {
                worst = (i, d);
            }
        }
        if worst.1 <= tolerance {
            return vec![first, last];
        }
        let mut left = Self::simplify(&points[..=worst.0], tolerance);
        let right = Self::simplify(&points[worst.0..], tolerance);
        left.pop();
        left.extend(right);
        left
    }

    fn commit(&mut self, ctx: &mut ToolCtx) {
        let points = std::mem::take(&mut self.points);
        if points.len() < 2 {
            return;
        }
        let kept = if self.curvature {
            points
        } else {
            Self::simplify(&points, self.fit.max(0.5))
        };
        let mut path = VectorPath::new(format!("Path {}", ctx.doc.paths.len() + 1));
        path.subpaths.push(SubPath {
            anchors: kept.iter().map(|(x, y)| Anchor::corner(*x, *y)).collect(),
            closed: false,
        });
        // Both pens produce smooth curves through their points.
        path.smooth_all();
        store(ctx.doc, path);
        ctx.doc.add_damage(ctx.doc.canvas_rect());
    }
}

fn perpendicular_distance(p: (f32, f32), a: (f32, f32), b: (f32, f32)) -> f32 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len = dx.hypot(dy);
    if len < 1e-6 {
        return (p.0 - a.0).hypot(p.1 - a.1);
    }
    ((p.0 - a.0) * dy - (p.1 - a.1) * dx).abs() / len
}

impl ToolPlugin for FreeformPenTool {
    fn id(&self) -> &'static str {
        if self.curvature {
            "pen.curvature"
        } else {
            "pen.freeform"
        }
    }
    fn name(&self) -> &'static str {
        if self.curvature {
            "Curvature Pen"
        } else {
            "Freeform Pen"
        }
    }
    fn description(&self) -> &'static str {
        if self.curvature {
            "Click a series of points and the path is curved smoothly through them."
        } else {
            "Drag a freehand line and it is fitted to a path, as loosely as the Fit option says."
        }
    }
    fn icon(&self) -> &'static str {
        if self.curvature {
            "pen-curvature"
        } else {
            "pen-freeform"
        }
    }
    fn group(&self) -> &'static str {
        "pen"
    }

    fn options(&self) -> Vec<ToolOption> {
        if self.curvature {
            Vec::new()
        } else {
            vec![ToolOption::slider(
                "freeform-fit",
                "Fit",
                self.fit,
                0.5,
                10.0,
                " px",
            )]
        }
    }

    fn set_option(&mut self, key: &str, value: OptionValue) {
        if key == "freeform-fit" {
            self.fit = value.num();
        }
    }

    fn on_pointer_down(&mut self, ctx: &mut ToolCtx, input: PointerInput) {
        if self.curvature {
            // Click to add a point; clicking near the first closes it.
            let r = 8.0 / ctx.state.zoom.max(0.01);
            if let Some(first) = self.points.first() {
                if self.points.len() > 2 && (first.0 - input.x).hypot(first.1 - input.y) <= r {
                    self.commit(ctx);
                    return;
                }
            }
            self.points.push((input.x, input.y));
            return;
        }
        self.drawing = true;
        self.points = vec![(input.x, input.y)];
    }

    fn on_pointer_move(&mut self, _ctx: &mut ToolCtx, input: PointerInput) {
        if self.drawing {
            self.points.push((input.x, input.y));
        }
    }

    fn on_pointer_up(&mut self, ctx: &mut ToolCtx, _input: PointerInput) {
        if self.drawing {
            self.drawing = false;
            self.commit(ctx);
        }
    }

    fn on_commit(&mut self, ctx: &mut ToolCtx) {
        self.commit(ctx);
    }

    fn on_cancel(&mut self, _ctx: &mut ToolCtx) {
        self.points.clear();
        self.drawing = false;
    }

    fn overlays(&self, _doc: &Document, _state: &EditorState) -> Vec<Overlay> {
        if self.points.len() < 2 {
            return Vec::new();
        }
        vec![Overlay::AntsPolygon(self.points.clone())]
    }
}

// -------------------------------------------------------- custom shape

/// The preset shapes, as unit outlines in 0..=1 that get scaled to the
/// drag rectangle.
const PRESETS: &[(&str, &[(f32, f32)])] = &[
    (
        "Heart",
        &[
            (0.50, 1.00),
            (0.10, 0.60),
            (0.02, 0.38),
            (0.10, 0.16),
            (0.30, 0.10),
            (0.50, 0.26),
            (0.70, 0.10),
            (0.90, 0.16),
            (0.98, 0.38),
            (0.90, 0.60),
        ],
    ),
    (
        "Star",
        &[
            (0.50, 0.00),
            (0.62, 0.35),
            (1.00, 0.35),
            (0.69, 0.57),
            (0.81, 0.94),
            (0.50, 0.71),
            (0.19, 0.94),
            (0.31, 0.57),
            (0.00, 0.35),
            (0.38, 0.35),
        ],
    ),
    (
        "Arrow",
        &[
            (0.00, 0.35),
            (0.60, 0.35),
            (0.60, 0.12),
            (1.00, 0.50),
            (0.60, 0.88),
            (0.60, 0.65),
            (0.00, 0.65),
        ],
    ),
    (
        "Lightning",
        &[
            (0.55, 0.00),
            (0.15, 0.55),
            (0.42, 0.55),
            (0.30, 1.00),
            (0.80, 0.42),
            (0.50, 0.42),
        ],
    ),
    (
        "Cross",
        &[
            (0.35, 0.00),
            (0.65, 0.00),
            (0.65, 0.35),
            (1.00, 0.35),
            (1.00, 0.65),
            (0.65, 0.65),
            (0.65, 1.00),
            (0.35, 1.00),
            (0.35, 0.65),
            (0.00, 0.65),
            (0.00, 0.35),
            (0.35, 0.35),
        ],
    ),
    (
        "Speech Bubble",
        &[
            (0.05, 0.05),
            (0.95, 0.05),
            (0.95, 0.65),
            (0.45, 0.65),
            (0.25, 0.95),
            (0.25, 0.65),
            (0.05, 0.65),
        ],
    ),
];

const PRESET_NAMES: &[&str] = &[
    "Heart",
    "Star",
    "Arrow",
    "Lightning",
    "Cross",
    "Speech Bubble",
];

pub struct CustomShapeTool {
    shape: usize,
    anchor: Option<(f32, f32)>,
    current: Option<(f32, f32)>,
    keep_ratio: bool,
}

impl CustomShapeTool {
    pub fn new() -> Self {
        CustomShapeTool {
            shape: 0,
            anchor: None,
            current: None,
            keep_ratio: false,
        }
    }

    fn outline(&self, from: (f32, f32), to: (f32, f32)) -> Vec<(f32, f32)> {
        let (_, pts) = PRESETS[self.shape.min(PRESETS.len() - 1)];
        let (x0, y0) = (from.0.min(to.0), from.1.min(to.1));
        let (mut w, mut h) = ((to.0 - from.0).abs(), (to.1 - from.1).abs());
        if self.keep_ratio {
            let s = w.min(h);
            w = s;
            h = s;
        }
        pts.iter().map(|(u, v)| (x0 + u * w, y0 + v * h)).collect()
    }
}

impl Default for CustomShapeTool {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolPlugin for CustomShapeTool {
    fn id(&self) -> &'static str {
        "shape.custom"
    }
    fn name(&self) -> &'static str {
        "Custom Shape"
    }
    fn description(&self) -> &'static str {
        "Drag out one of the built-in preset shapes, picked with the Shape option."
    }
    fn icon(&self) -> &'static str {
        "shape-custom"
    }
    fn group(&self) -> &'static str {
        "shape"
    }

    fn options(&self) -> Vec<ToolOption> {
        vec![ToolOption::choice(
            "custom-shape",
            "Shape",
            PRESET_NAMES,
            self.shape,
        )]
    }

    fn set_option(&mut self, key: &str, value: OptionValue) {
        if key == "custom-shape" {
            self.shape = value.index().min(PRESETS.len() - 1);
        }
    }

    fn on_pointer_down(&mut self, _ctx: &mut ToolCtx, input: PointerInput) {
        self.anchor = Some((input.x, input.y));
        self.current = Some((input.x, input.y));
        self.keep_ratio = input.modifiers.shift;
    }

    fn on_pointer_move(&mut self, _ctx: &mut ToolCtx, input: PointerInput) {
        if self.anchor.is_some() {
            self.current = Some((input.x, input.y));
            self.keep_ratio = input.modifiers.shift;
        }
    }

    fn on_pointer_up(&mut self, ctx: &mut ToolCtx, input: PointerInput) {
        let Some(from) = self.anchor.take() else {
            return;
        };
        self.current = None;
        let to = (input.x, input.y);
        if (to.0 - from.0).abs() < 2.0 && (to.1 - from.1).abs() < 2.0 {
            return;
        }
        let outline = self.outline(from, to);
        let mut b = PathBuilder::new();
        b.move_to(outline[0].0, outline[0].1);
        for p in &outline[1..] {
            b.line_to(p.0, p.1);
        }
        b.close();
        let color = ctx.state.foreground;
        crate::commit_shape(
            ctx.doc,
            &b.build(0.25),
            color,
            schist_vector::FillRule::NonZero,
            "Custom Shape",
        );
    }

    fn on_cancel(&mut self, _ctx: &mut ToolCtx) {
        self.anchor = None;
        self.current = None;
    }

    fn overlays(&self, _doc: &Document, _state: &EditorState) -> Vec<Overlay> {
        match (self.anchor, self.current) {
            (Some(a), Some(c)) => vec![Overlay::AntsPolygon(self.outline(a, c))],
            _ => Vec::new(),
        }
    }
}
