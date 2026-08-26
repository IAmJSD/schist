//! Free transform (⌘T) and crop (C).
//!
//! Free transform is *modal*: activating the tool snapshots the active
//! layer, every handle drag re-renders that snapshot through an affine
//! matrix (fast nearest-neighbour while dragging), and Enter re-renders once
//! with the user's chosen filter and commits a single history entry. Escape
//! restores the snapshot.

use schist_core::{Affine, Document, Filter, IntRect, LayerId, LayerKind, TileMap};
use schist_plugin_api::{
    EditorState, OptionValue, Overlay, PluginManifest, PluginRegistry, PointerInput, ToolCtx,
    ToolOption, ToolPlugin,
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
/// Whether the tool is moving pixels or the selection outline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TransformMode {
    /// Free Transform: the layer's pixels.
    #[default]
    Layer,
    /// Select ▸ Transform Selection: the mask, leaving pixels alone.
    Selection,
}

struct Session {
    mode: TransformMode,
    layer: LayerId,
    /// Untransformed pixels (cheap: tiles are reference-counted).
    original: TileMap,
    /// Untransformed selection, for `TransformMode::Selection`.
    original_selection: schist_core::Selection,
    /// Bounds of `original`, the box the handles frame.
    base: IntRect,
    /// Scale / rotation / skew accumulated so far.
    scale: (f32, f32),
    rotation: f32,
    offset: (f32, f32),
    /// Live drag state.
    drag: Option<Drag>,
    /// Where the transform is anchored, in 0..1 of `base`.
    ///
    /// Scaling pins the handle opposite the one being dragged, so the
    /// dragged handle follows the cursor. Anchoring at the centre (which
    /// is what this always used to be) moves each edge half the drag, so
    /// the handle lagged the cursor and every scale behaved like an
    /// Alt-drag with no way to ask for the ordinary one.
    pivot_anchor: (f32, f32),
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
            self.base.left as f32 + self.base.width() as f32 * self.pivot_anchor.0,
            self.base.top as f32 + self.base.height() as f32 * self.pivot_anchor.1,
        )
    }

    /// Current matrix: scale, then rotate, both about the pivot, then
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

    /// Where a handle sits, in document space.
    ///
    /// `zoom` only matters to the rotate handle, whose standoff from the
    /// box is a fixed number of *screen* pixels: it was a fixed 24
    /// document units, so at 800% it sat three screen pixels off the box
    /// and at 10% it floated 240 screen pixels away, while `hit` had
    /// always divided its radius by the zoom.
    fn handle_pos(&self, handle: Handle, zoom: f32) -> (f32, f32) {
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
            // 24 *screen* pixels, like the hit radius below. As a flat
            // document offset the handle sat 3 px away at 800% zoom and
            // 240 px away at 10%.
            let reach = 24.0 / zoom.max(0.01);
            (top.0 + dx / len * reach, top.1 + dy / len * reach)
        } else {
            (sx, sy)
        }
    }

    fn hit(&self, x: f32, y: f32, zoom: f32) -> Option<Handle> {
        // Handles are ~9 screen pixels; convert to document units.
        let r = (9.0 / zoom.max(0.01)).max(1.0);
        for handle in [Handle::Rotate].into_iter().chain(Handle::ALL) {
            let (hx, hy) = self.handle_pos(handle, zoom);
            if (x - hx).abs() <= r && (y - hy).abs() <= r {
                return Some(handle);
            }
        }
        // Inside the transformed quad = move.
        let quad = self.corners();
        point_in_quad(x, y, &quad).then_some(Handle::Inside)
    }

    /// Re-render the layer (or the selection) from the snapshot with the
    /// current matrix.
    fn render(&self, doc: &mut Document, filter: Filter) {
        if self.mode == TransformMode::Selection {
            let before = doc.selection.bounds();
            let canvas = doc.canvas_rect();
            doc.selection = self.original_selection.transformed(&self.matrix(), canvas);
            doc.add_damage(before.union(&doc.selection.bounds()));
            return;
        }
        let clip = doc.canvas_rect().inflated(
            (self.base.width().max(self.base.height()) as f32
                * self.scale.0.abs().max(self.scale.1.abs())) as i32,
        );
        let depth = doc.depth;
        let tiles = schist_core::resample::transform_tiles(
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
        if self.mode == TransformMode::Selection {
            let before = doc.selection.bounds();
            doc.selection = self.original_selection.clone();
            doc.add_damage(before.union(&self.base));
            return;
        }
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

const INTERPOLATIONS: &[&str] = &["Nearest Neighbor", "Bilinear", "Bicubic"];

pub struct TransformTool {
    mode: TransformMode,
    session: Option<Session>,
    /// Mirrors `EditorState::resample`, which is what the commit actually
    /// reads; `options()` has no editor state to consult, so the tool
    /// keeps a copy and writes through when it changes.
    resample: Filter,
}

impl Default for TransformTool {
    fn default() -> Self {
        TransformTool::new(TransformMode::Layer)
    }
}

impl TransformTool {
    pub fn new(mode: TransformMode) -> TransformTool {
        TransformTool {
            mode,
            session: None,
            resample: Filter::Bilinear,
        }
    }
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
        let base = match self.mode {
            TransformMode::Layer => raster.tiles.content_bounds(),
            // The handles frame the selection, not the artwork.
            TransformMode::Selection => ctx.doc.selection.bounds(),
        };
        if base.is_empty() {
            return;
        }
        self.session = Some(Session {
            mode: self.mode,
            layer: id,
            original: raster.tiles.clone(),
            original_selection: ctx.doc.selection.clone(),
            base,
            scale: (1.0, 1.0),
            rotation: 0.0,
            offset: (0.0, 0.0),
            drag: None,
            pivot_anchor: (0.5, 0.5),
            dirty: false,
        });
    }
}

impl ToolPlugin for TransformTool {
    fn id(&self) -> &'static str {
        match self.mode {
            TransformMode::Layer => "transform",
            TransformMode::Selection => "transform.selection",
        }
    }

    fn name(&self) -> &'static str {
        match self.mode {
            TransformMode::Layer => "Free Transform",
            TransformMode::Selection => "Transform Selection",
        }
    }

    fn description(&self) -> &'static str {
        match self.mode {
            TransformMode::Layer => {
                "Free Transform. Activating it opens a transform box around the active \
                 layer: drag a corner to scale, drag outside one to rotate, drag an edge to \
                 skew. Nothing is written until it is committed (Enter), and cancelling \
                 (Escape) puts the layer back."
            }
            TransformMode::Selection => {
                "Transform Selection: the same box, moving the selection outline rather than \
                 the pixels inside it. Commit or cancel to finish."
            }
        }
    }

    fn icon(&self) -> &'static str {
        "transform"
    }

    fn in_toolbar(&self) -> bool {
        // Free transform is a command (⌘T), not a toolbar slot.
        false
    }

    fn on_activate(&mut self, ctx: &mut ToolCtx) {
        // Adopt whatever the document is set to, so the bar opens showing
        // the truth rather than this tool's last guess.
        self.resample = ctx.state.resample;
        self.begin(ctx);
    }

    fn options(&self) -> Vec<ToolOption> {
        vec![ToolOption::choice(
            "transform-interpolation",
            "Interpolation",
            INTERPOLATIONS,
            match self.resample {
                Filter::Nearest => 0,
                Filter::Bilinear => 1,
                Filter::Bicubic => 2,
            },
        )]
    }

    fn set_option(&mut self, key: &str, value: OptionValue) {
        if key == "transform-interpolation" {
            self.resample = match value.index() {
                0 => Filter::Nearest,
                2 => Filter::Bicubic,
                _ => Filter::Bilinear,
            };
        }
    }

    fn on_option_changed(&mut self, ctx: &mut ToolCtx, _key: &str) {
        // The commit reads it from here, not from the tool.
        ctx.state.resample = self.resample;
    }

    fn on_pointer_down(&mut self, ctx: &mut ToolCtx, input: PointerInput) {
        if self.session.is_none() {
            self.begin(ctx);
        }
        let zoom = ctx.state.zoom;
        if let Some(session) = &mut self.session {
            if let Some(handle) = session.hit(input.x, input.y, zoom) {
                // Pin the handle opposite the one being dragged, so the
                // dragged one lands under the cursor. Alt asks for the
                // centre, which is Photoshop's scale-from-centre.
                let (ax, ay) = handle.anchor();
                session.pivot_anchor = if input.modifiers.alt {
                    (0.5, 0.5)
                } else {
                    (1.0 - ax, 1.0 - ay)
                };
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
                // A handle dragged onto the pivot gives scale 0, whose
                // matrix has no inverse: `transform_tiles` then returns an
                // empty map and the layer vanishes. A later Shift-drag
                // divides by that 0 and produces NaN, which `det.abs() <
                // 1e-9` does not catch, so the NaN matrix saturates the
                // bounds to empty and the commit records the empty result.
                // Keep at least one pixel on each axis. Scale 0 has no
                // invertible matrix, so `transform_tiles` returned an
                // empty map and the layer vanished; a later Shift-drag
                // then divided by that 0 and produced NaN, which
                // `det.abs() < 1e-9` does not catch, so the NaN matrix
                // saturated the bounds to empty and the commit recorded
                // the empty result.
                let clamp = |v: f32, start: f32, extent: i32| {
                    if !v.is_finite() {
                        return start;
                    }
                    let min = 1.0 / extent.max(1) as f32;
                    if v.abs() < min {
                        min.copysign(if v == 0.0 { 1.0 } else { v })
                    } else {
                        v
                    }
                };
                session.scale = (
                    clamp(sx, drag.start_scale.0, session.base.width()),
                    clamp(sy, drag.start_scale.1, session.base.height()),
                );
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
        if session.mode == TransformMode::Selection {
            // Only the mask moves; the pixels are untouched, so this is one
            // selection edit rather than a tile rewrite. (`restore` already
            // ran above; calling it twice was harmless but obscured the
            // control flow.)
            // selection edit rather than a tile rewrite. (`restore` above
            // has already run; calling it a second time here was
            // idempotent but obscured the flow.)
            let canvas = ctx.doc.canvas_rect();
            let matrix = session.matrix();
            let base = session.original_selection.clone();
            let mut edit = ctx.doc.begin_edit("Transform Selection");
            edit.change_selection(|sel, _| *sel = base.transformed(&matrix, canvas));
            edit.commit();
            return;
        }
        // A smart object composes the transform onto its own and
        // re-renders from its untouched source, so transforming it twice
        // costs no more quality than transforming it once.
        let smart = ctx
            .doc
            .tree
            .find(session.layer)
            .and_then(|l| l.smart.as_deref())
            .map(|so| {
                let mut next = so.clone();
                next.filter = ctx.state.resample;
                next.apply(&session.matrix());
                next
            });
        let tiles = match &smart {
            Some(so) => so.render(depth, clip),
            None => schist_core::resample::transform_tiles(
                &session.original,
                &session.matrix(),
                depth,
                ctx.state.resample,
                clip,
            ),
        };
        // The mask moves with the artwork it clips. Leaving it behind cut
        // the transformed layer along the mask's old outline.
        let moved_mask = ctx
            .doc
            .tree
            .find(session.layer)
            .and_then(|l| l.mask.as_ref())
            .map(|mask| {
                let mut next = mask.clone();
                next.tiles = schist_core::resample::transform_mask(
                    &mask.tiles,
                    mask.default_value,
                    &session.matrix(),
                    clip,
                );
                next.bounds = session.matrix().transform_bounds(mask.bounds);
                next
            });
        let mut edit = ctx.doc.begin_edit("Free Transform");
        edit.replace_layer_tiles(session.layer, tiles);
        if let Some(mask) = moved_mask {
            edit.set_mask(session.layer, Some(mask));
        }
        if let Some(so) = smart {
            edit.set_smart_object(session.layer, Some(Box::new(so)));
        }
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

    fn overlays(&self, _doc: &Document, state: &EditorState) -> Vec<Overlay> {
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
            let (x, y) = session.handle_pos(handle, state.zoom);
            // `as i32` truncates toward zero, so a handle shifts by a
            // pixel once its coordinate goes negative -- on the
            // off-canvas side, where the preview then disagrees with the
            // floor/ceil used to commit the same gesture.
            out.push(Overlay::Rect(IntRect::new(
                (x - r).floor() as i32,
                (y - r).floor() as i32,
                (x + r).ceil() as i32,
                (y + r).ceil() as i32,
            )));
        }
        let (rx, ry) = session.handle_pos(Handle::Rotate, state.zoom);
        out.push(Overlay::Circle {
            cx: rx,
            cy: ry,
            r: 4.0,
        });
        out
    }
}

/// Crop: drag a rectangle, Enter trims the canvas to it.
/// Photoshop's crop ratio presets, and the width:height each locks to.
const CROP_RATIOS: &[&str] = &["Unconstrained", "1:1", "4:3", "3:2", "16:9"];
const CROP_ASPECTS: [Option<f32>; 5] = [
    None,
    Some(1.0),
    Some(4.0 / 3.0),
    Some(3.0 / 2.0),
    Some(16.0 / 9.0),
];

#[derive(Default)]
pub struct CropTool {
    anchor: Option<(f32, f32)>,
    rect: Option<IntRect>,
    /// Index into `CROP_RATIOS`.
    ratio: usize,
    /// Throw away what falls outside the crop, rather than keeping it
    /// off-canvas where a later canvas resize would bring it back.
    delete_cropped: bool,
}

/// Clear every pixel outside `keep` on every raster layer.
fn discard_outside(doc: &mut Document, keep: IntRect) {
    let mut edit = doc.begin_edit("Crop");
    for id in edit.raster_layer_ids() {
        let Some(tiles) = edit
            .doc()
            .tree
            .find(id)
            .and_then(|l| l.as_raster())
            .map(|r| r.tiles.clone())
        else {
            continue;
        };
        let mut kept = schist_core::TileMap::new();
        for (coord, buf) in tiles.iter() {
            let trect = coord.rect();
            if keep.intersect(&trect).is_empty() {
                continue;
            }
            if keep.contains(trect.left, trect.top)
                && keep.contains(trect.right - 1, trect.bottom - 1)
            {
                kept.insert(*coord, buf.clone());
                continue;
            }
            // Straddles the edge: keep the inside, blank the rest.
            let mut trimmed = (**buf).clone();
            for ly in 0..schist_core::TILE_SIZE {
                for lx in 0..schist_core::TILE_SIZE {
                    let (x, y) = (trect.left + lx, trect.top + ly);
                    if !keep.contains(x, y) {
                        let ix = (ly * schist_core::TILE_SIZE + lx) as usize;
                        trimmed.set(ix, schist_color::Rgba::TRANSPARENT);
                    }
                }
            }
            kept.insert(*coord, std::sync::Arc::new(trimmed));
        }
        kept.prune_blank();
        edit.replace_layer_tiles(id, kept);
    }
    edit.commit();
}

impl ToolPlugin for CropTool {
    fn id(&self) -> &'static str {
        "crop"
    }
    fn name(&self) -> &'static str {
        "Crop"
    }
    fn description(&self) -> &'static str {
        "Drag out the area to keep and adjust its handles; committing (Enter) trims the \
         document to it, cancelling (Escape) leaves it alone."
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
        let (mut bx, mut by) = (input.x, input.y);
        // A locked ratio follows the longer side of the drag, so the box
        // never shrinks away from the cursor.
        if let Some(aspect) = CROP_ASPECTS[self.ratio.min(CROP_ASPECTS.len() - 1)] {
            let (dx, dy) = (bx - ax, by - ay);
            if dx.abs() / aspect >= dy.abs() {
                by = ay + (dx.abs() / aspect) * if dy < 0.0 { -1.0 } else { 1.0 };
            } else {
                bx = ax + (dy.abs() * aspect) * if dx < 0.0 { -1.0 } else { 1.0 };
            }
        }
        let rect = IntRect::new(
            ax.min(bx).round() as i32,
            ay.min(by).round() as i32,
            ax.max(bx).round() as i32,
            ay.max(by).round() as i32,
        );
        self.rect = Some(rect.intersect(&ctx.doc.canvas_rect()));
    }

    fn on_pointer_up(&mut self, _ctx: &mut ToolCtx, _input: PointerInput) {
        self.anchor = None;
    }

    fn options(&self) -> Vec<ToolOption> {
        vec![
            ToolOption::choice("crop-ratio", "Ratio", CROP_RATIOS, self.ratio),
            ToolOption::toggle("crop-delete", "Delete Cropped Pixels", self.delete_cropped),
        ]
    }

    fn set_option(&mut self, key: &str, value: OptionValue) {
        match key {
            "crop-ratio" => self.ratio = value.index().min(CROP_RATIOS.len() - 1),
            "crop-delete" => self.delete_cropped = value.bool(),
            _ => {}
        }
    }

    fn on_commit(&mut self, ctx: &mut ToolCtx) {
        let Some(rect) = self.rect.take() else { return };
        if rect.is_empty() {
            return;
        }
        // Trim before the canvas moves under it, while the rect still
        // means what it says in document coordinates.
        if self.delete_cropped {
            discard_outside(ctx.doc, rect);
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
    // Guides, artboards, slices, notes, counts and stored paths move with
    // the pixels; cropping 100 px off the left used to leave every one of
    // them 100 px out of place.
    let (ox, oy) = (rect.left as f32, rect.top as f32);
    edit.map_geometry(|x, y| (x - ox, y - oy));
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
        let tiles = schist_core::resample::resize_tiles(
            &raster.tiles,
            from,
            (width, height),
            depth,
            filter,
        );
        edit.replace_layer_tiles(id, tiles);
    }
    // Masks scale with the pixels they clip. Leaving them at the old size
    // meant halving a document clipped every masked layer to a quarter of
    // its intended area.
    rescale_masks(&mut edit, from, (width, height));
    // Guides, artboards, slices, notes, counts and paths scale with the
    // canvas; halving the image used to leave them at full-size
    // coordinates.
    let sx = width as f32 / from.0.max(1) as f32;
    let sy = height as f32 / from.1.max(1) as f32;
    edit.map_geometry(|x, y| (x * sx, y * sy));
    edit.set_canvas_size(width, height);
    edit.commit();
}

/// How Image Size gets from one size to the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resample {
    /// One of the classical reconstruction filters.
    Classic(Filter),
    /// A neural x2 upscaler from the `schist_neural` catalogue, applied
    /// until the image is at or past the target, with bicubic covering
    /// whatever remainder a non-power-of-two target leaves.
    Neural(&'static str),
}

impl Resample {
    pub fn display_name(self) -> &'static str {
        match self {
            Resample::Classic(f) => f.display_name(),
            Resample::Neural(id) => schist_neural::spec(id).map_or(id, |s| s.name),
        }
    }
}

/// What a neural resample turns out to need.
///
/// Deciding is cheap and needs the document; running is expensive and must
/// not hold it, because at seconds per megapixel a resize on the UI thread
/// is a frozen window. So the two are separate, and [`NeuralResize`]
/// carries the pixels rather than borrowing them.
pub enum Plan {
    /// The model would not load. Bicubic is the honest fallback, and the
    /// caller is expected to say so rather than quietly substitute it.
    NoModel,
    /// Nothing here for a network: a downscale, or a target already
    /// reached. Bicubic, quietly.
    Classical,
    /// Run the network, then [`apply_upscaled`] what comes back.
    Neural(Box<NeuralResize>),
}

/// A neural resize with its pixels already lifted out of the document.
pub struct NeuralResize {
    model: std::sync::Arc<schist_neural::Model>,
    doublings: u32,
    from: (u32, u32),
    to: (u32, u32),
    depth: schist_color::Depth,
    doc: schist_core::DocumentId,
    /// Per layer: which one, its premultiplied RGB, and its alpha.
    layers: Vec<(LayerId, Vec<f32>, Vec<f32>)>,
}

/// The result of running one, ready to go back into a document.
pub struct Upscaled {
    to: (u32, u32),
    depth: schist_color::Depth,
    doc: schist_core::DocumentId,
    /// Per layer: which one, its straight-alpha RGBA, and the size the
    /// network actually reached for it -- per layer rather than shared,
    /// so a layer whose run stopped early still describes its own buffer.
    layers: Vec<(LayerId, Vec<f32>, (usize, usize))>,
}

impl NeuralResize {
    /// Input megapixels the network has to chew through, for a dialog that
    /// wants to say how much work it just started.
    pub fn megapixels(&self) -> f32 {
        let one = self.from.0 as f32 * self.from.1 as f32 / 1e6;
        // Each doubling quadruples what the next one is fed.
        (0..self.doublings).map(|i| one * 4f32.powi(i as i32)).sum()
    }

    /// The expensive half: no document, no UI thread, no borrow of either.
    pub fn run(self) -> Upscaled {
        let mut out = Vec::with_capacity(self.layers.len());
        for (id, mut rgb, mut alpha) in self.layers {
            let (mut w, mut h) = (self.from.0 as usize, self.from.1 as usize);
            for _ in 0..self.doublings {
                let Some(bigger) = schist_neural::run_scaled(&self.model, &rgb, w, h) else {
                    log::warn!("{}: refused {w}x{h}; bicubic finishes", self.model.spec.id);
                    break;
                };
                rgb = bigger;
                alpha = double_plane(&alpha, w, h);
                (w, h) = (w * 2, h * 2);
            }
            // Back to the straight alpha the tile maps store.
            let mut rgba = vec![0.0f32; w * h * 4];
            for i in 0..w * h {
                let a = alpha[i].clamp(0.0, 1.0);
                if a > 1e-6 {
                    rgba[i * 4] = (rgb[i * 3] / a).clamp(0.0, 1.0);
                    rgba[i * 4 + 1] = (rgb[i * 3 + 1] / a).clamp(0.0, 1.0);
                    rgba[i * 4 + 2] = (rgb[i * 3 + 2] / a).clamp(0.0, 1.0);
                }
                rgba[i * 4 + 3] = a;
            }
            out.push((id, rgba, (w, h)));
        }
        Upscaled {
            to: self.to,
            depth: self.depth,
            doc: self.doc,
            layers: out,
        }
    }
}

/// Decide what a neural resample of `doc` to `width` x `height` involves.
pub fn plan_neural(doc: &Document, width: u32, height: u32, id: &str) -> Plan {
    let Some(model) = schist_neural::get(id) else {
        return Plan::NoModel;
    };
    if width == 0 || height == 0 {
        return Plan::Classical;
    }
    // Doublings to reach or pass the target, bounded so an absurd target
    // asks for a bounded amount of inference and memory. A downscale --
    // or anything past the bound -- is bicubic's job.
    let (mut cw, mut ch) = (doc.width as usize, doc.height as usize);
    let mut doublings = 0u32;
    while (cw < width as usize || ch < height as usize)
        && doublings < 3
        && cw * 2 <= 8192
        && ch * 2 <= 8192
    {
        cw *= 2;
        ch *= 2;
        doublings += 1;
    }
    if doublings == 0 {
        return Plan::Classical;
    }

    let (w, h) = (doc.width as usize, doc.height as usize);
    let mut layers = Vec::new();
    for layer in doc.tree.iter() {
        let (id, Some(raster)) = (layer.id, layer.as_raster()) else {
            continue;
        };
        if raster.tiles.is_empty() {
            continue;
        }
        // Premultiplied, as all resampling here is: interpolating straight
        // alpha would drag the meaningless colour of fully transparent
        // pixels into every edge. The network sees the premultiplied
        // colour; alpha rides along bilinearly beside it, since a coverage
        // ramp has no detail for a network to invent.
        let mut rgb = vec![0.0f32; w * h * 3];
        let mut alpha = vec![0.0f32; w * h];
        for y in 0..h {
            for x in 0..w {
                let px = raster.tiles.pixel(x as i32, y as i32);
                let at = y * w + x;
                rgb[at * 3] = px.r * px.a;
                rgb[at * 3 + 1] = px.g * px.a;
                rgb[at * 3 + 2] = px.b * px.a;
                alpha[at] = px.a;
            }
        }
        layers.push((id, rgb, alpha));
    }
    Plan::Neural(Box::new(NeuralResize {
        model,
        doublings,
        from: (doc.width, doc.height),
        to: (width, height),
        depth: doc.depth,
        doc: doc.id,
        layers,
    }))
}

/// Put a finished upscale back, as one undoable edit.
///
/// Does nothing if the document has changed out from under it, which is
/// why the plan carried its id.
pub fn apply_upscaled(doc: &mut Document, up: Upscaled) {
    if doc.id != up.doc {
        log::warn!("upscale finished for a document that is no longer here");
        return;
    }
    let (width, height) = up.to;
    let mut edit = doc.begin_edit("Image Size");
    for (id, rgba, (w, h)) in up.layers {
        if edit
            .doc()
            .tree
            .find(id)
            .and_then(|l| l.as_raster())
            .is_none()
        {
            continue;
        }
        let mut tiles = TileMap::new();
        schist_core::blit_rgba_f32(
            &mut tiles,
            up.depth,
            IntRect::from_size(w as u32, h as u32),
            &rgba,
        );
        // Whatever the doublings overshot or fell short of.
        if (w as u32, h as u32) != (width, height) {
            tiles = schist_core::resample::resize_tiles(
                &tiles,
                (w as u32, h as u32),
                (width, height),
                up.depth,
                Filter::Bicubic,
            );
        }
        edit.replace_layer_tiles(id, tiles);
    }
    edit.set_canvas_size(width, height);
    edit.commit();
}

/// [`resize_image`], but able to resample through a neural upscaler.
///
/// Runs the network inline, so this is the right entry point for a caller
/// that is already off the UI thread (and the wrong one for a caller that
/// is not -- see [`plan_neural`]). Returns `false` when a neural resample
/// was asked for and the model would not load, in which case bicubic stood
/// in: silently substituting the thing the user specifically did not pick
/// is worse than telling them.
pub fn resize_image_with(doc: &mut Document, width: u32, height: u32, how: Resample) -> bool {
    let id = match how {
        Resample::Classic(filter) => {
            resize_image(doc, width, height, filter);
            return true;
        }
        Resample::Neural(id) => id,
    };
    if width == 0 || height == 0 || (width == doc.width && height == doc.height) {
        return true;
    }
    match plan_neural(doc, width, height, id) {
        Plan::NoModel => {
            resize_image(doc, width, height, Filter::Bicubic);
            false
        }
        Plan::Classical => {
            resize_image(doc, width, height, Filter::Bicubic);
            true
        }
        Plan::Neural(plan) => {
            apply_upscaled(doc, plan.run());
            true
        }
    }
}

/// Double a single plane bilinearly, sampling at pixel centres.
fn double_plane(src: &[f32], w: usize, h: usize) -> Vec<f32> {
    let (ow, oh) = (w * 2, h * 2);
    let mut out = vec![0.0f32; ow * oh];
    for y in 0..oh {
        let fy = (y as f32 + 0.5) / 2.0 - 0.5;
        let y0 = fy.floor().max(0.0) as usize;
        let y1 = (y0 + 1).min(h - 1);
        let ty = (fy - y0 as f32).clamp(0.0, 1.0);
        for x in 0..ow {
            let fx = (x as f32 + 0.5) / 2.0 - 0.5;
            let x0 = fx.floor().max(0.0) as usize;
            let x1 = (x0 + 1).min(w - 1);
            let tx = (fx - x0 as f32).clamp(0.0, 1.0);
            let top = src[y0 * w + x0] * (1.0 - tx) + src[y0 * w + x1] * tx;
            let bot = src[y1 * w + x0] * (1.0 - tx) + src[y1 * w + x1] * tx;
            out[y * ow + x] = top * (1.0 - ty) + bot * ty;
        }
    }
    out
}

/// Rescale every layer mask by the same factor as the canvas.
fn rescale_masks(edit: &mut schist_core::EditBuilder, from: (u32, u32), to: (u32, u32)) {
    if from.0 == 0 || from.1 == 0 {
        return;
    }
    let m = schist_core::Affine::scale(to.0 as f32 / from.0 as f32, to.1 as f32 / from.1 as f32);
    let clip = IntRect::from_size(to.0, to.1);
    let masked: Vec<_> = edit
        .doc()
        .tree
        .iter()
        .filter(|l| l.mask.is_some())
        .map(|l| l.id)
        .collect();
    for id in masked {
        let Some(mask) = edit.doc().tree.find(id).and_then(|l| l.mask.as_ref()) else {
            continue;
        };
        let tiles =
            schist_core::resample::transform_mask(&mask.tiles, mask.default_value, &m, clip);
        let mut next = mask.clone();
        next.tiles = tiles;
        next.bounds = m.transform_bounds(mask.bounds).intersect(&clip);
        edit.set_mask(id, Some(next));
    }
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
    edit.map_geometry(|x, y| (x + dx as f32, y + dy as f32));
    edit.set_canvas_size(width, height);
    edit.commit();
}

pub struct TransformToolsPlugin;

impl PluginManifest for TransformToolsPlugin {
    fn id(&self) -> &'static str {
        "schist.tools-transform"
    }

    fn register(&self, registry: &mut PluginRegistry) {
        registry.register_tool(Box::new(TransformTool::new(TransformMode::Layer)));
        registry.register_tool(Box::new(TransformTool::new(TransformMode::Selection)));
        registry.register_tool(Box::new(CropTool::default()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use schist_color::Depth;
    use schist_core::{blit_rgba8, Layer};
    use schist_plugin_api::Modifiers;

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
    fn a_locked_crop_ratio_holds_its_shape() {
        let mut doc = Document::new("t", 400, 400, Depth::Eight);
        let mut state = EditorState::default();
        let mut tool = CropTool::default();
        tool.set_option("crop-ratio", OptionValue::Choice(1)); // 1:1
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        tool.on_pointer_down(&mut ctx, input(20.0, 20.0));
        // Drag a wide, shallow box: the ratio should square it up off the
        // longer side, so it follows the cursor rather than collapsing.
        tool.on_pointer_move(&mut ctx, input(220.0, 60.0));
        let rect = tool.rect.expect("a crop rect");
        assert_eq!(
            rect.width(),
            rect.height(),
            "1:1 should be square, got {}x{}",
            rect.width(),
            rect.height()
        );
        assert_eq!(rect.width(), 200, "and should follow the longer side");

        tool.set_option("crop-ratio", OptionValue::Choice(4)); // 16:9
        tool.on_pointer_move(&mut ctx, input(180.0, 60.0));
        let rect = tool.rect.expect("a crop rect");
        let ratio = rect.width() as f32 / rect.height() as f32;
        assert!(
            (ratio - 16.0 / 9.0).abs() < 0.05,
            "expected 16:9, got {ratio}"
        );
    }

    #[test]
    fn deleting_cropped_pixels_actually_deletes_them() {
        for delete in [false, true] {
            let mut doc = Document::new("t", 200, 200, Depth::Eight);
            let mut layer = Layer::new_raster("wide");
            blit_rgba8(
                &mut layer.as_raster_mut().unwrap().tiles,
                Depth::Eight,
                IntRect::from_xywh(0, 0, 200, 200),
                &[255u8, 0, 0, 255].repeat(200 * 200),
            );
            doc.push_layer(layer);
            let id = doc.active_layer.unwrap();

            let mut state = EditorState::default();
            let mut tool = CropTool::default();
            tool.set_option("crop-delete", OptionValue::Bool(delete));
            let mut ctx = ToolCtx {
                doc: &mut doc,
                state: &mut state,
            };
            tool.on_pointer_down(&mut ctx, input(50.0, 50.0));
            tool.on_pointer_move(&mut ctx, input(150.0, 150.0));
            tool.on_pointer_up(&mut ctx, input(150.0, 150.0));
            tool.on_commit(&mut ctx);

            assert_eq!(doc.width, 100, "the canvas shrank either way");
            let tiles = &doc.tree.find(id).unwrap().as_raster().unwrap().tiles;
            // Inside the crop, the pixels survive whatever the setting.
            assert_eq!(tiles.pixel(50, 50).to_u8()[3], 255);
            // Outside it, they are kept off-canvas or thrown away.
            let outside = tiles.pixel(-10, 50).to_u8()[3];
            if delete {
                assert_eq!(outside, 0, "delete should have cleared it");
            } else {
                assert_eq!(outside, 255, "without delete it rides along off-canvas");
            }
        }
    }

    #[test]
    fn neural_resize_doubles_the_document() {
        // The model is built into the binary, so this runs the real
        // network -- over a document small enough to cost one tile.
        let mut doc = doc_with_square();
        let id = doc.active_layer.unwrap();
        let did = resize_image_with(&mut doc, 400, 400, Resample::Neural("waifu2x-photo"));
        assert!(did, "a built-in model must not fall back");
        assert_eq!((doc.width, doc.height), (400, 400));

        let tiles = &doc.tree.find(id).unwrap().as_raster().unwrap().tiles;
        // The square was at 20..60; doubled it covers 40..120. Its middle
        // must still be its colour (loosely -- the network may round) and
        // solid, and the far corner must still be empty.
        let px = tiles.pixel(80, 80).to_u8();
        assert_eq!(px[3], 255, "the square went translucent: {px:?}");
        assert!(
            px[0] < 60 && px[1] > 90 && px[1] < 170 && px[2] > 200,
            "the square changed colour: {px:?}"
        );
        assert_eq!(
            tiles.pixel(390, 390).to_u8()[3],
            0,
            "emptiness stayed empty"
        );

        assert_eq!(doc.undo().as_deref(), Some("Image Size"));
        assert_eq!((doc.width, doc.height), (200, 200), "one undoable edit");
    }

    #[test]
    fn neural_resize_reaches_a_non_power_of_two_target() {
        // 200 -> 300 is one doubling and then bicubic back down.
        let mut doc = doc_with_square();
        assert!(resize_image_with(
            &mut doc,
            300,
            300,
            Resample::Neural("waifu2x-art")
        ));
        assert_eq!((doc.width, doc.height), (300, 300));
        let id = doc.active_layer.unwrap();
        let tiles = &doc.tree.find(id).unwrap().as_raster().unwrap().tiles;
        assert_eq!(tiles.pixel(60, 60).to_u8()[3], 255);
    }

    #[test]
    fn neural_resize_downscale_is_just_bicubic() {
        let mut doc = doc_with_square();
        assert!(resize_image_with(
            &mut doc,
            100,
            100,
            Resample::Neural("waifu2x-photo")
        ));
        assert_eq!((doc.width, doc.height), (100, 100));
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
        let (hx, hy) = session.handle_pos(Handle::Rotate, 1.0);
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

    #[test]
    fn image_size_rescales_layer_masks_with_the_pixels() {
        // The mask stayed at its old size while the artwork halved, so a
        // masked layer was clipped to a quarter of its intended area.
        use schist_core::LayerMask;
        let mut doc = doc_with_square();
        let id = doc.tree.layers[0].id;
        {
            let layer = doc.tree.find_mut(id).unwrap();
            let mut mask = LayerMask::new_revealing();
            // Reveal the left half of the document.
            for coord in schist_core::TileCoord::covering(&IntRect::from_xywh(0, 0, 100, 200)) {
                let trect = coord.rect();
                let buf = mask.tiles.get_mut_or_insert(coord);
                for ly in 0..schist_core::TILE_SIZE {
                    for lx in 0..schist_core::TILE_SIZE {
                        if trect.left + lx < 100 {
                            buf[(ly * schist_core::TILE_SIZE + lx) as usize] = 255;
                        }
                    }
                }
            }
            mask.bounds = IntRect::from_xywh(0, 0, 100, 200);
            layer.mask = Some(mask);
        }

        resize_image(&mut doc, 100, 100, Filter::Bilinear);

        let mask = doc.tree.find(id).unwrap().mask.as_ref().expect("mask kept");
        // The revealed half must have halved with the canvas: covered at
        // x=40, clear at x=60.
        assert!(
            mask.tiles.value(40, 50) > 200,
            "left half should stay revealed"
        );
        assert!(
            mask.tiles.value(60, 50) < 55,
            "right half should stay hidden"
        );
        assert!(
            mask.bounds.right <= 100,
            "mask bounds must be inside the new canvas: {:?}",
            mask.bounds
        );
    }

    #[test]
    fn a_scale_handle_follows_the_cursor() {
        // Scaling pivoted on the box centre, so each edge moved half the
        // drag: the handle lagged the cursor by half, and every scale
        // behaved like an Alt-drag with no way to ask for the ordinary
        // one. Dragging the right handle should pin the left edge.
        let mut doc = doc_with_square();
        let mut state = EditorState {
            zoom: 1.0,
            ..EditorState::default()
        };
        let mut tool = TransformTool::default();
        {
            let mut ctx = ToolCtx {
                doc: &mut doc,
                state: &mut state,
            };
            tool.on_activate(&mut ctx);
            let base = tool.session.as_ref().unwrap().base;
            // Grab the right-middle handle and pull it 40 px right.
            let (hx, hy) = (
                base.right as f32,
                base.top as f32 + base.height() as f32 / 2.0,
            );
            tool.on_pointer_down(&mut ctx, input(hx, hy));
            tool.on_pointer_move(&mut ctx, input(hx + 40.0, hy));

            let session = tool.session.as_ref().unwrap();
            let m = session.matrix();
            let moved = m.transform_bounds(base);
            assert_eq!(
                moved.left, base.left,
                "the opposite edge must stay pinned: {moved:?} vs {base:?}"
            );
            assert!(
                (moved.right - (base.right + 40)).abs() <= 1,
                "the dragged edge must land under the cursor: {moved:?}"
            );
        }
    }

    #[test]
    fn a_degenerate_scale_cannot_erase_the_layer() {
        // Dragging a handle onto the pivot gave scale 0, whose matrix has
        // no inverse, so the layer vanished and the commit recorded the
        // empty result.
        let mut doc = doc_with_square();
        let mut state = EditorState {
            zoom: 1.0,
            ..EditorState::default()
        };
        let mut tool = TransformTool::default();
        {
            let mut ctx = ToolCtx {
                doc: &mut doc,
                state: &mut state,
            };
            tool.on_activate(&mut ctx);
            let base = tool.session.as_ref().unwrap().base;
            let (hx, hy) = (
                base.right as f32,
                base.top as f32 + base.height() as f32 / 2.0,
            );
            tool.on_pointer_down(&mut ctx, input(hx, hy));
            // Drag the right edge all the way onto the pinned left edge.
            tool.on_pointer_move(&mut ctx, input(base.left as f32, hy));
            tool.on_commit(&mut ctx);
        }
        let content = doc.tree.layers[0]
            .as_raster()
            .unwrap()
            .tiles
            .content_bounds();
        assert!(!content.is_empty(), "the layer must not be erased");
    }

    /// A document carrying one of everything that should move with the
    /// canvas.
    fn doc_with_geometry() -> Document {
        let mut doc = doc_with_square();
        doc.guides.push(schist_core::Guide {
            horizontal: false,
            position: 100.0,
        });
        doc.guides.push(schist_core::Guide {
            horizontal: true,
            position: 60.0,
        });
        doc.artboards.push(schist_core::annotate::Artboard {
            name: "a".into(),
            rect: IntRect::from_xywh(100, 100, 40, 40),
        });
        doc.notes.push(schist_core::annotate::Note {
            at: (100.0, 60.0),
            author: "me".into(),
            text: "here".into(),
        });
        doc
    }

    #[test]
    fn cropping_moves_guides_notes_and_artboards_with_the_pixels() {
        // Crop 40 px off the left and 20 off the top: everything
        // document-level shifts by the same amount. It used to stay put,
        // so every guide and note was that far out of place.
        let mut doc = doc_with_geometry();
        crop_to(&mut doc, IntRect::from_xywh(40, 20, 100, 100));

        assert_eq!(doc.guides[0].position, 60.0, "vertical guide");
        assert_eq!(doc.guides[1].position, 40.0, "horizontal guide");
        assert_eq!(doc.artboards[0].rect.left, 60);
        assert_eq!(doc.artboards[0].rect.top, 80);
        assert_eq!(doc.notes[0].at, (60.0, 40.0));
    }

    #[test]
    fn undo_puts_the_geometry_back_with_the_canvas() {
        // The reason this has to ride inside the edit: undoing the canvas
        // while leaving the geometry moved is worse than either state.
        let mut doc = doc_with_geometry();
        let before = doc.geometry();
        crop_to(&mut doc, IntRect::from_xywh(40, 20, 100, 100));
        assert_ne!(doc.geometry(), before, "crop moved it");

        doc.undo();
        assert_eq!(doc.geometry(), before, "undo must move it back");
    }

    #[test]
    fn image_size_scales_the_geometry_too() {
        let mut doc = doc_with_geometry();
        resize_image(&mut doc, 100, 100, Filter::Bilinear);
        // The document was 200x200, so everything halves.
        assert_eq!(doc.guides[0].position, 50.0);
        assert_eq!(doc.notes[0].at, (50.0, 30.0));
    }
    /// The rotate handle's standoff is a fixed number of *screen* pixels.
    /// It was a fixed 24 document units, so at 800% it sat three screen
    /// pixels off the box and at 10% it floated 240 away -- while `hit`
    /// had always divided its radius by the zoom.
    #[test]
    fn the_rotate_handle_keeps_its_screen_distance() {
        let mut doc = doc_with_square();
        let mut state = EditorState::default();
        let mut tool = TransformTool::default();
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        tool.on_activate(&mut ctx);
        let session = tool.session.as_ref().unwrap();
        let top = session.base.top as f32;

        for zoom in [0.1f32, 1.0, 8.0] {
            let (_, hy) = session.handle_pos(Handle::Rotate, zoom);
            let screen = (top - hy) * zoom;
            assert!(
                (screen - 24.0).abs() < 0.5,
                "at {zoom}x the handle stood {screen} screen pixels off the box"
            );
        }
    }

    /// And it stays grabbable at every zoom, which is the point.
    #[test]
    fn the_rotate_handle_is_hittable_at_any_zoom() {
        let mut doc = doc_with_square();
        let mut state = EditorState::default();
        let mut tool = TransformTool::default();
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        tool.on_activate(&mut ctx);
        let session = tool.session.as_ref().unwrap();
        for zoom in [0.1f32, 1.0, 8.0] {
            let (hx, hy) = session.handle_pos(Handle::Rotate, zoom);
            assert_eq!(
                session.hit(hx, hy, zoom),
                Some(Handle::Rotate),
                "at {zoom}x"
            );
        }
    }
}
