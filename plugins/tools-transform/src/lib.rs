//! Free transform (⌘T) and crop (C).
//!
//! Free transform is *modal*: activating the tool snapshots the active
//! layer, every handle drag re-renders that snapshot through an affine
//! matrix (fast nearest-neighbour while dragging), and Enter re-renders once
//! with the user's chosen filter and commits a single history entry. Escape
//! restores the snapshot.

use photoslop_core::{Affine, Document, Filter, IntRect, LayerId, LayerKind, TileMap};
use photoslop_plugin_api::{
    EditorState, Overlay, PluginManifest, PluginRegistry, PointerInput, ToolCtx, ToolPlugin,
};

/// Which handle of the transform box is being dragged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Handle {
    TopLeft,
    Top,
    TopRight,
    Right,
    BottomRight,
    Bottom,
    BottomLeft,
    Left,
    Rotate,
    Inside,
}

impl Handle {
    /// Unit position of the handle within the box (0..1 in each axis).
    fn anchor(self) -> (f32, f32) {
        match self {
            Handle::TopLeft => (0.0, 0.0),
            Handle::Top => (0.5, 0.0),
            Handle::TopRight => (1.0, 0.0),
            Handle::Right => (1.0, 0.5),
            Handle::BottomRight => (1.0, 1.0),
            Handle::Bottom => (0.5, 1.0),
            Handle::BottomLeft => (0.0, 1.0),
            Handle::Left => (0.0, 0.5),
            Handle::Rotate | Handle::Inside => (0.5, 0.5),
        }
    }

    fn scales_x(self) -> bool {
        matches!(
            self,
            Handle::TopLeft
                | Handle::TopRight
                | Handle::BottomRight
                | Handle::BottomLeft
                | Handle::Left
                | Handle::Right
        )
    }

    fn scales_y(self) -> bool {
        matches!(
            self,
            Handle::TopLeft
                | Handle::TopRight
                | Handle::BottomRight
                | Handle::BottomLeft
                | Handle::Top
                | Handle::Bottom
        )
    }

    const ALL: [Handle; 8] = [
        Handle::TopLeft,
        Handle::Top,
        Handle::TopRight,
        Handle::Right,
        Handle::BottomRight,
        Handle::Bottom,
        Handle::BottomLeft,
        Handle::Left,
    ];
}

/// A transform in progress.
struct Session {
    layer: LayerId,
    /// Untransformed pixels (cheap: tiles are reference-counted).
    original: TileMap,
    /// Bounds of `original`, the box the handles frame.
    base: IntRect,
    /// Scale / rotation / skew accumulated so far.
    scale: (f32, f32),
    rotation: f32,
    offset: (f32, f32),
    /// Live drag state.
    drag: Option<Drag>,
    dirty: bool,
}

struct Drag {
    handle: Handle,
    start: (f32, f32),
    start_scale: (f32, f32),
    start_rotation: f32,
    start_offset: (f32, f32),
}

impl Session {
    fn pivot(&self) -> (f32, f32) {
        (
            self.base.left as f32 + self.base.width() as f32 / 2.0,
            self.base.top as f32 + self.base.height() as f32 / 2.0,
        )
    }

    /// Current matrix: scale, then rotate, both about the box centre, then
    /// translate.
    fn matrix(&self) -> Affine {
        let (px, py) = self.pivot();
        Affine::scale(self.scale.0, self.scale.1)
            .then(&Affine::rotate(self.rotation))
            .around(px, py)
            .then(&Affine::translate(self.offset.0, self.offset.1))
    }

    /// The four corners of the transformed box, clockwise from top-left.
    fn corners(&self) -> [(f32, f32); 4] {
        let m = self.matrix();
        let r = self.base;
        [
            m.apply(r.left as f32, r.top as f32),
            m.apply(r.right as f32, r.top as f32),
            m.apply(r.right as f32, r.bottom as f32),
            m.apply(r.left as f32, r.bottom as f32),
        ]
    }

    fn handle_pos(&self, handle: Handle) -> (f32, f32) {
        let (ux, uy) = handle.anchor();
        let m = self.matrix();
        let r = self.base;
        let (x, y) = (
            r.left as f32 + r.width() as f32 * ux,
            r.top as f32 + r.height() as f32 * uy,
        );
        let (sx, sy) = m.apply(x, y);
        if handle == Handle::Rotate {
            // Sits above the top edge, along the box's own "up" direction.
            let top = m.apply(r.left as f32 + r.width() as f32 * 0.5, r.top as f32);
            let (dx, dy) = (top.0 - sx, top.1 - sy);
            let len = (dx * dx + dy * dy).sqrt().max(1e-3);
            (top.0 + dx / len * 24.0, top.1 + dy / len * 24.0)
        } else {
            (sx, sy)
        }
    }

    fn hit(&self, x: f32, y: f32, zoom: f32) -> Option<Handle> {
        // Handles are ~9 screen pixels; convert to document units.
        let r = (9.0 / zoom.max(0.01)).max(1.0);
        for handle in [Handle::Rotate].into_iter().chain(Handle::ALL) {
            let (hx, hy) = self.handle_pos(handle);
            if (x - hx).abs() <= r && (y - hy).abs() <= r {
                return Some(handle);
            }
        }
        // Inside the transformed quad = move.
        let quad = self.corners();
        point_in_quad(x, y, &quad).then_some(Handle::Inside)
    }

    /// Re-render the layer from the snapshot with the current matrix.
    fn render(&self, doc: &mut Document, filter: Filter) {
        let clip = doc.canvas_rect().inflated(
            (self.base.width().max(self.base.height()) as f32
                * self.scale.0.abs().max(self.scale.1.abs())) as i32,
        );
        let depth = doc.depth;
        let tiles = photoslop_core::resample::transform_tiles(
            &self.original,
            &self.matrix(),
            depth,
            filter,
            clip,
        );
        let before = doc
            .tree
            .find(self.layer)
            .map(|l| l.content_bounds())
            .unwrap_or(IntRect::EMPTY);
        if let Some(raster) = doc
            .tree
            .find_mut(self.layer)
            .and_then(|l| l.as_raster_mut())
        {
            raster.tiles = tiles;
        }
        let after = doc
            .tree
            .find(self.layer)
            .map(|l| l.content_bounds())
            .unwrap_or(IntRect::EMPTY);
        doc.add_damage(before.union(&after));
    }

    fn restore(&self, doc: &mut Document) {
        let before = doc
            .tree
            .find(self.layer)
            .map(|l| l.content_bounds())
            .unwrap_or(IntRect::EMPTY);
        if let Some(raster) = doc
            .tree
            .find_mut(self.layer)
            .and_then(|l| l.as_raster_mut())
        {
            raster.tiles = self.original.clone();
        }
        doc.add_damage(before.union(&self.base));
    }
}

fn point_in_quad(x: f32, y: f32, quad: &[(f32, f32); 4]) -> bool {
    let mut inside = false;
    for i in 0..4 {
        let (x1, y1) = quad[i];
        let (x2, y2) = quad[(i + 1) % 4];
        if (y1 > y) != (y2 > y) {
            let xint = x1 + (y - y1) / (y2 - y1) * (x2 - x1);
            if x < xint {
                inside = !inside;
            }
        }
    }
    inside
}

#[derive(Default)]
pub struct TransformTool {
    session: Option<Session>,
}

impl TransformTool {
    fn begin(&mut self, ctx: &mut ToolCtx) {
        let Some(id) = ctx.doc.active_layer else {
            return;
        };
        let Some(layer) = ctx.doc.tree.find(id) else {
            return;
        };
        if layer.locked {
            return;
        }
        let LayerKind::Raster(raster) = &layer.kind else {
            return;
        };
        let base = raster.tiles.content_bounds();
        if base.is_empty() {
            return;
        }
        self.session = Some(Session {
            layer: id,
            original: raster.tiles.clone(),
            base,
            scale: (1.0, 1.0),
            rotation: 0.0,
            offset: (0.0, 0.0),
            drag: None,
            dirty: false,
        });
    }
}

impl ToolPlugin for TransformTool {
    fn id(&self) -> &'static str {
        "transform"
    }
    fn name(&self) -> &'static str {
        "Free Transform"
    }
    fn icon(&self) -> &'static str {
        "transform"
    }

    fn in_toolbar(&self) -> bool {
        // Free transform is a command (⌘T), not a toolbar slot.
        false
    }

    fn on_activate(&mut self, ctx: &mut ToolCtx) {
        self.begin(ctx);
    }

    fn on_pointer_down(&mut self, ctx: &mut ToolCtx, input: PointerInput) {
        if self.session.is_none() {
            self.begin(ctx);
        }
        let zoom = ctx.state.zoom;
        if let Some(session) = &mut self.session {
            if let Some(handle) = session.hit(input.x, input.y, zoom) {
                session.drag = Some(Drag {
                    handle,
                    start: (input.x, input.y),
                    start_scale: session.scale,
                    start_rotation: session.rotation,
                    start_offset: session.offset,
                });
            }
        }
    }

    fn on_pointer_move(&mut self, ctx: &mut ToolCtx, input: PointerInput) {
        let Some(session) = &mut self.session else {
            return;
        };
        let Some(drag) = &session.drag else { return };
        let (px, py) = session.pivot();
        let handle = drag.handle;
        let (dx, dy) = (input.x - drag.start.0, input.y - drag.start.1);

        match handle {
            Handle::Inside => {
                session.offset = (drag.start_offset.0 + dx, drag.start_offset.1 + dy);
            }
            Handle::Rotate => {
                let a0 = (drag.start.1 - py).atan2(drag.start.0 - px);
                let a1 = (input.y - py).atan2(input.x - px);
                let mut delta = a1 - a0;
                if input.modifiers.shift {
                    // Snap to 15° increments.
                    let step = std::f32::consts::FRAC_PI_2 / 6.0;
                    delta = (delta / step).round() * step;
                }
                session.rotation = drag.start_rotation + delta;
            }
            _ => {
                // Scale about the opposite edge/corner: distance from the
                // pivot along each axis grows with the drag.
                let half_w = (session.base.width() as f32 / 2.0).max(1.0);
                let half_h = (session.base.height() as f32 / 2.0).max(1.0);
                let (ax, ay) = handle.anchor();
                let dir_x = (ax - 0.5) * 2.0;
                let dir_y = (ay - 0.5) * 2.0;
                let mut sx = drag.start_scale.0;
                let mut sy = drag.start_scale.1;
                if handle.scales_x() && dir_x != 0.0 {
                    sx = drag.start_scale.0 + dx * dir_x / half_w / 2.0;
                }
                if handle.scales_y() && dir_y != 0.0 {
                    sy = drag.start_scale.1 + dy * dir_y / half_h / 2.0;
                }
                if input.modifiers.shift && handle.scales_x() && handle.scales_y() {
                    // Constrain proportions from the larger change.
                    let f = if (sx / drag.start_scale.0).abs() > (sy / drag.start_scale.1).abs() {
                        sx / drag.start_scale.0
                    } else {
                        sy / drag.start_scale.1
                    };
                    sx = drag.start_scale.0 * f;
                    sy = drag.start_scale.1 * f;
                }
                session.scale = (sx, sy);
            }
        }
        session.dirty = true;
        // Nearest-neighbour keeps the drag interactive; the committed
        // render uses the user's filter.
        session.render(ctx.doc, Filter::Nearest);
    }

    fn on_pointer_up(&mut self, _ctx: &mut ToolCtx, _input: PointerInput) {
        if let Some(session) = &mut self.session {
            session.drag = None;
        }
    }

    fn on_commit(&mut self, ctx: &mut ToolCtx) {
        let Some(session) = self.session.take() else {
            return;
        };
        if !session.dirty {
            return;
        }
        // Rebuild from the snapshot at full quality, then record the change
        // against the *original* pixels so undo restores them exactly.
        session.restore(ctx.doc);
        let depth = ctx.doc.depth;
        let clip = ctx
            .doc
            .canvas_rect()
            .inflated(session.base.width().max(session.base.height()));
        let tiles = photoslop_core::resample::transform_tiles(
            &session.original,
            &session.matrix(),
            depth,
            ctx.state.resample,
            clip,
        );
        let mut edit = ctx.doc.begin_edit("Free Transform");
        edit.replace_layer_tiles(session.layer, tiles);
        edit.commit();
    }

    fn on_cancel(&mut self, ctx: &mut ToolCtx) {
        if let Some(session) = self.session.take() {
            session.restore(ctx.doc);
        }
    }

    fn on_deactivate(&mut self, ctx: &mut ToolCtx) {
        // Leaving the tool commits, matching Photoshop.
        self.on_commit(ctx);
    }

    fn overlays(&self, _doc: &Document, _state: &EditorState) -> Vec<Overlay> {
        let Some(session) = &self.session else {
            return Vec::new();
        };
        let quad = session.corners();
        let mut out = Vec::with_capacity(14);
        for i in 0..4 {
            let (x1, y1) = quad[i];
            let (x2, y2) = quad[(i + 1) % 4];
            out.push(Overlay::Line { x1, y1, x2, y2 });
        }
        let r = 3.0;
        for handle in Handle::ALL {
            let (x, y) = session.handle_pos(handle);
            out.push(Overlay::Rect(IntRect::new(
                (x - r) as i32,
                (y - r) as i32,
                (x + r) as i32,
                (y + r) as i32,
            )));
        }
        let (rx, ry) = session.handle_pos(Handle::Rotate);
        out.push(Overlay::Circle {
            cx: rx,
            cy: ry,
            r: 4.0,
        });
        out
    }
}

/// Crop: drag a rectangle, Enter trims the canvas to it.
#[derive(Default)]
pub struct CropTool {
    anchor: Option<(f32, f32)>,
    rect: Option<IntRect>,
}

impl ToolPlugin for CropTool {
    fn id(&self) -> &'static str {
        "crop"
    }
    fn name(&self) -> &'static str {
        "Crop"
    }
    fn icon(&self) -> &'static str {
        "crop"
    }
    fn shortcut(&self) -> Option<&'static str> {
        Some("c")
    }

    fn on_pointer_down(&mut self, _ctx: &mut ToolCtx, input: PointerInput) {
        self.anchor = Some((input.x, input.y));
        self.rect = None;
    }

    fn on_pointer_move(&mut self, ctx: &mut ToolCtx, input: PointerInput) {
        let Some((ax, ay)) = self.anchor else { return };
        let rect = IntRect::new(
            ax.min(input.x).round() as i32,
            ay.min(input.y).round() as i32,
            ax.max(input.x).round() as i32,
            ay.max(input.y).round() as i32,
        );
        self.rect = Some(rect.intersect(&ctx.doc.canvas_rect()));
    }

    fn on_pointer_up(&mut self, _ctx: &mut ToolCtx, _input: PointerInput) {
        self.anchor = None;
    }

    fn on_commit(&mut self, ctx: &mut ToolCtx) {
        let Some(rect) = self.rect.take() else { return };
        if rect.is_empty() {
            return;
        }
        crop_to(ctx.doc, rect);
    }

    fn on_cancel(&mut self, _ctx: &mut ToolCtx) {
        self.anchor = None;
        self.rect = None;
    }

    fn overlays(&self, _doc: &Document, _state: &EditorState) -> Vec<Overlay> {
        match self.rect {
            Some(rect) if !rect.is_empty() => vec![Overlay::AntsRect(rect)],
            _ => Vec::new(),
        }
    }
}

/// Trim the canvas to `rect`, moving every layer so the crop origin becomes
/// (0, 0). One undoable edit.
pub fn crop_to(doc: &mut Document, rect: IntRect) {
    let mut edit = doc.begin_edit("Crop");
    let ids = edit.raster_layer_ids();
    for id in ids {
        edit.translate_layer(id, -rect.left, -rect.top);
    }
    edit.set_canvas_size(rect.width() as u32, rect.height() as u32);
    edit.change_selection(|sel, _| sel.deselect());
    edit.commit();
}

/// Rescale the whole document (Image Size).
pub fn resize_image(doc: &mut Document, width: u32, height: u32, filter: Filter) {
    if width == 0 || height == 0 || (width == doc.width && height == doc.height) {
        return;
    }
    let from = (doc.width, doc.height);
    let depth = doc.depth;
    let mut edit = doc.begin_edit("Image Size");
    let ids = edit.raster_layer_ids();
    for id in ids {
        let Some(raster) = edit.doc().tree.find(id).and_then(|l| l.as_raster()) else {
            continue;
        };
        let tiles = photoslop_core::resample::resize_tiles(
            &raster.tiles,
            from,
            (width, height),
            depth,
            filter,
        );
        edit.replace_layer_tiles(id, tiles);
    }
    edit.set_canvas_size(width, height);
    edit.commit();
}

/// Change the canvas without rescaling pixels (Canvas Size). `anchor` is the
/// content's relative position in the new canvas, 0..1 per axis.
pub fn resize_canvas(doc: &mut Document, width: u32, height: u32, anchor: (f32, f32)) {
    if width == 0 || height == 0 || (width == doc.width && height == doc.height) {
        return;
    }
    let dx = ((width as f32 - doc.width as f32) * anchor.0).round() as i32;
    let dy = ((height as f32 - doc.height as f32) * anchor.1).round() as i32;
    let mut edit = doc.begin_edit("Canvas Size");
    let ids = edit.raster_layer_ids();
    for id in ids {
        edit.translate_layer(id, dx, dy);
    }
    edit.set_canvas_size(width, height);
    edit.commit();
}

pub struct TransformToolsPlugin;

impl PluginManifest for TransformToolsPlugin {
    fn id(&self) -> &'static str {
        "photoslop.tools-transform"
    }

    fn register(&self, registry: &mut PluginRegistry) {
        registry.register_tool(Box::new(TransformTool::default()));
        registry.register_tool(Box::new(CropTool::default()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use photoslop_color::Depth;
    use photoslop_core::{blit_rgba8, Layer};
    use photoslop_plugin_api::Modifiers;

    fn doc_with_square() -> Document {
        let mut doc = Document::new("t", 200, 200, Depth::Eight);
        let mut layer = Layer::new_raster("sq");
        let buf = [0u8, 128, 255, 255].repeat(40 * 40);
        blit_rgba8(
            &mut layer.as_raster_mut().unwrap().tiles,
            Depth::Eight,
            IntRect::from_xywh(20, 20, 40, 40),
            &buf,
        );
        doc.push_layer(layer);
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

    fn px(doc: &Document, x: i32, y: i32) -> [u8; 4] {
        doc.tree.layers[0]
            .as_raster()
            .unwrap()
            .tiles
            .pixel(x, y)
            .to_u8()
    }

    #[test]
    fn transform_scales_layer_and_undoes() {
        let mut doc = doc_with_square();
        let mut state = EditorState::default();
        let mut tool = TransformTool::default();
        {
            let mut ctx = ToolCtx {
                doc: &mut doc,
                state: &mut state,
            };
            tool.on_activate(&mut ctx);
            // Grab the bottom-right handle (box is 20..60) and drag out.
            tool.on_pointer_down(&mut ctx, input(60.0, 60.0));
            tool.on_pointer_move(&mut ctx, input(100.0, 100.0));
            tool.on_pointer_up(&mut ctx, input(100.0, 100.0));
            tool.on_commit(&mut ctx);
        }
        // The square grew about its centre: pixels now reach further out.
        assert_eq!(px(&doc, 40, 40)[3], 255, "centre still covered");
        assert!(px(&doc, 75, 75)[3] > 0, "grew past the original edge");
        assert_eq!(doc.undo().as_deref(), Some("Free Transform"));
        assert_eq!(px(&doc, 75, 75)[3], 0, "undo restores original extent");
        assert_eq!(px(&doc, 30, 30), [0, 128, 255, 255]);
    }

    #[test]
    fn transform_cancel_restores_pixels() {
        let mut doc = doc_with_square();
        let mut state = EditorState::default();
        let mut tool = TransformTool::default();
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        tool.on_activate(&mut ctx);
        tool.on_pointer_down(&mut ctx, input(60.0, 60.0));
        tool.on_pointer_move(&mut ctx, input(120.0, 120.0));
        tool.on_cancel(&mut ctx);
        assert_eq!(px(&doc, 30, 30), [0, 128, 255, 255]);
        assert_eq!(px(&doc, 80, 80)[3], 0);
        assert!(!doc.history.can_undo(), "cancelling records nothing");
    }

    #[test]
    fn rotation_handle_turns_the_layer() {
        let mut doc = Document::new("t", 200, 200, Depth::Eight);
        let mut layer = Layer::new_raster("bar");
        // A wide, short bar: after a 90° turn it should be tall and narrow.
        let buf = [255u8, 0, 0, 255].repeat(40 * 8);
        blit_rgba8(
            &mut layer.as_raster_mut().unwrap().tiles,
            Depth::Eight,
            IntRect::from_xywh(50, 96, 40, 8),
            &buf,
        );
        doc.push_layer(layer);

        let mut state = EditorState::default();
        let mut tool = TransformTool::default();
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        tool.on_activate(&mut ctx);
        let session = tool.session.as_ref().unwrap();
        let (hx, hy) = session.handle_pos(Handle::Rotate);
        let (cx, cy) = session.pivot();
        tool.on_pointer_down(&mut ctx, input(hx, hy));
        // Drag the rotate handle a quarter turn around the centre.
        let radius = ((hx - cx).powi(2) + (hy - cy).powi(2)).sqrt();
        tool.on_pointer_move(&mut ctx, input(cx + radius, cy));
        tool.on_commit(&mut ctx);

        assert!(px(&doc, 70, 85)[3] > 0, "bar now extends vertically");
        assert_eq!(px(&doc, 55, 100)[3], 0, "and no longer horizontally");
    }

    #[test]
    fn crop_trims_canvas_and_moves_content() {
        let mut doc = doc_with_square();
        let mut state = EditorState::default();
        let mut tool = CropTool::default();
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        tool.on_pointer_down(&mut ctx, input(20.0, 20.0));
        tool.on_pointer_move(&mut ctx, input(60.0, 60.0));
        tool.on_pointer_up(&mut ctx, input(60.0, 60.0));
        tool.on_commit(&mut ctx);

        assert_eq!((doc.width, doc.height), (40, 40));
        assert_eq!(
            px(&doc, 0, 0),
            [0, 128, 255, 255],
            "content shifted to origin"
        );
        doc.undo();
        assert_eq!((doc.width, doc.height), (200, 200));
        assert_eq!(px(&doc, 20, 20), [0, 128, 255, 255]);
    }

    #[test]
    fn image_size_rescales_all_layers() {
        let mut doc = doc_with_square();
        resize_image(&mut doc, 100, 100, Filter::Bilinear);
        assert_eq!((doc.width, doc.height), (100, 100));
        // The square was at 20..60 of 200; at half scale it is 10..30.
        assert!(px(&doc, 15, 15)[3] > 0, "content scaled with the canvas");
        assert_eq!(px(&doc, 50, 50)[3], 0);
        doc.undo();
        assert_eq!((doc.width, doc.height), (200, 200));
        assert_eq!(px(&doc, 30, 30), [0, 128, 255, 255]);
    }

    #[test]
    fn canvas_size_keeps_pixel_scale() {
        let mut doc = doc_with_square();
        resize_canvas(&mut doc, 400, 400, (0.5, 0.5));
        assert_eq!((doc.width, doc.height), (400, 400));
        // Centred: content moved by (400-200)/2 = 100.
        assert_eq!(px(&doc, 130, 130), [0, 128, 255, 255]);
        doc.undo();
        assert_eq!(px(&doc, 30, 30), [0, 128, 255, 255]);
    }
}
