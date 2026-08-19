//! Selection tools: rectangular & elliptical marquee, lasso, magic wand.
//!
//! Modifier convention (Photoshop): Shift = add to selection, Alt =
//! subtract, Shift+Alt = intersect, no modifier = replace.

use photoslop_core::{Document, IntRect, LayerKind, SelectOp, TileCoord, TILE_SIZE};
use photoslop_plugin_api::{
    EditorState, Modifiers, Overlay, PluginManifest, PluginRegistry, PointerInput, ToolCtx,
    ToolPlugin,
};

fn op_from(modifiers: Modifiers) -> SelectOp {
    match (modifiers.shift, modifiers.alt) {
        (true, true) => SelectOp::Intersect,
        (true, false) => SelectOp::Add,
        (false, true) => SelectOp::Subtract,
        (false, false) => SelectOp::Replace,
    }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MarqueeShape {
    Rect,
    Ellipse,
}

pub struct MarqueeTool {
    shape: MarqueeShape,
    anchor: Option<(f32, f32, Modifiers)>,
    current: Option<IntRect>,
}

impl MarqueeTool {
    fn new(shape: MarqueeShape) -> Self {
        MarqueeTool {
            shape,
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
        let op = op_from(m);
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
        let mut edit = ctx.doc.begin_edit("Select");
        edit.change_selection(|sel, _| match shape {
            MarqueeShape::Rect => sel.select_rect(rect, op),
            MarqueeShape::Ellipse => sel.select_ellipse(rect, op),
        });
        edit.commit();
    }

    fn on_cancel(&mut self, _ctx: &mut ToolCtx) {
        self.anchor = None;
        self.current = None;
    }

    fn overlays(&self, _doc: &Document, _state: &EditorState) -> Vec<Overlay> {
        match self.current {
            Some(rect) => vec![Overlay::AntsRect(rect)],
            None => Vec::new(),
        }
    }
}

pub struct LassoTool {
    points: Vec<(f32, f32)>,
    modifiers: Modifiers,
}

impl LassoTool {
    fn new() -> Self {
        LassoTool {
            points: Vec::new(),
            modifiers: Modifiers::default(),
        }
    }
}

impl ToolPlugin for LassoTool {
    fn id(&self) -> &'static str {
        "lasso"
    }
    fn name(&self) -> &'static str {
        "Lasso"
    }
    fn icon(&self) -> &'static str {
        "lasso"
    }
    fn shortcut(&self) -> Option<&'static str> {
        Some("l")
    }

    fn on_pointer_down(&mut self, _ctx: &mut ToolCtx, input: PointerInput) {
        self.modifiers = input.modifiers;
        self.points = vec![(input.x, input.y)];
    }

    fn on_pointer_move(&mut self, _ctx: &mut ToolCtx, input: PointerInput) {
        if !self.points.is_empty() {
            self.points.push((input.x, input.y));
        }
    }

    fn on_pointer_up(&mut self, ctx: &mut ToolCtx, _input: PointerInput) {
        let points = std::mem::take(&mut self.points);
        if points.len() < 3 {
            return;
        }
        let op = op_from(self.modifiers);
        let mut edit = ctx.doc.begin_edit("Lasso Select");
        edit.change_selection(|sel, _| sel.select_polygon(&points, op));
        edit.commit();
    }

    fn on_cancel(&mut self, _ctx: &mut ToolCtx) {
        self.points.clear();
    }

    fn overlays(&self, _doc: &Document, _state: &EditorState) -> Vec<Overlay> {
        if self.points.len() > 1 {
            vec![Overlay::AntsPolygon(self.points.clone())]
        } else {
            Vec::new()
        }
    }
}

/// Magic wand: contiguous flood fill on the active layer by color distance.
pub struct WandTool {
    /// 0..=255 max per-channel distance.
    pub tolerance: u8,
}

impl WandTool {
    fn new() -> Self {
        WandTool { tolerance: 32 }
    }
}

fn wand_select(doc: &Document, x: i32, y: i32, tolerance: u8) -> Option<Vec<(i32, i32)>> {
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
        let Some(pixels) = wand_select(ctx.doc, x, y, self.tolerance) else {
            return;
        };
        let op = op_from(input.modifiers);
        let mut edit = ctx.doc.begin_edit("Magic Wand");
        edit.change_selection(|sel, _| {
            if op == SelectOp::Replace {
                sel.deselect();
            }
            // Write matched pixels directly into the mask.
            let effective = if op == SelectOp::Replace {
                SelectOp::Add
            } else {
                op
            };
            for &(px, py) in &pixels {
                let coord = TileCoord::containing(px, py);
                let buf = sel.mask.get_mut_or_insert(coord);
                let lx = px.rem_euclid(TILE_SIZE) as usize;
                let ly = py.rem_euclid(TILE_SIZE) as usize;
                let ix = ly * TILE_SIZE as usize + lx;
                match effective {
                    SelectOp::Add => buf[ix] = 255,
                    SelectOp::Subtract => buf[ix] = 0,
                    SelectOp::Intersect => {} // handled below
                    SelectOp::Replace => unreachable!(),
                }
            }
            if effective == SelectOp::Intersect {
                // Intersect: keep only previously-selected wand pixels.
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
            }
            sel.activate();
            sel.recompute_bounds();
        });
        edit.commit();
    }

    fn on_pointer_move(&mut self, _ctx: &mut ToolCtx, _input: PointerInput) {}
    fn on_pointer_up(&mut self, _ctx: &mut ToolCtx, _input: PointerInput) {}
}

pub struct SelectToolsPlugin;

impl PluginManifest for SelectToolsPlugin {
    fn id(&self) -> &'static str {
        "photoslop.tools-select"
    }

    fn register(&self, registry: &mut PluginRegistry) {
        registry.register_tool(Box::new(MarqueeTool::new(MarqueeShape::Rect)));
        registry.register_tool(Box::new(MarqueeTool::new(MarqueeShape::Ellipse)));
        registry.register_tool(Box::new(LassoTool::new()));
        registry.register_tool(Box::new(WandTool::new()));
    }
}

#[allow(unused_imports)]
use photoslop_pixel_ops as _;

#[cfg(test)]
mod tests {
    use super::*;
    use photoslop_color::Depth;
    use photoslop_core::{blit_rgba8, Layer};

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
        let mut tool = LassoTool::new();
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
