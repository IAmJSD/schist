//! Vanishing Point: define a plane in the scene's perspective, then clone
//! along it so the copied pixels take on the plane's foreshortening.
//!
//! The plane is four corners; mapping between the image and the plane's
//! own flat coordinates is the homography those four corners define.
//! Cloning then happens in *plane* space, which is what makes a patch
//! copied along a receding wall get smaller as it recedes.

use photoslop_color::Rgba;
use photoslop_core::{Document, IntRect, LayerId, TileCoord, TileMap, TILE_SIZE};
use photoslop_plugin_api::{
    EditorState, OptionValue, Overlay, PointerInput, ToolCtx, ToolOption, ToolPlugin,
};

/// A 3x3 projective transform.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Homography(pub [f32; 9]);

impl Homography {
    pub const IDENTITY: Homography = Homography([1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0]);

    pub fn apply(&self, x: f32, y: f32) -> (f32, f32) {
        let m = &self.0;
        let w = m[6] * x + m[7] * y + m[8];
        if w.abs() < 1e-9 {
            return (x, y);
        }
        (
            (m[0] * x + m[1] * y + m[2]) / w,
            (m[3] * x + m[4] * y + m[5]) / w,
        )
    }

    pub fn invert(&self) -> Option<Homography> {
        let m = &self.0;
        // Adjugate over determinant; a 3x3 inverse written out.
        let c = [
            m[4] * m[8] - m[5] * m[7],
            m[2] * m[7] - m[1] * m[8],
            m[1] * m[5] - m[2] * m[4],
            m[5] * m[6] - m[3] * m[8],
            m[0] * m[8] - m[2] * m[6],
            m[2] * m[3] - m[0] * m[5],
            m[3] * m[7] - m[4] * m[6],
            m[1] * m[6] - m[0] * m[7],
            m[0] * m[4] - m[1] * m[3],
        ];
        let det = m[0] * c[0] + m[1] * c[3] + m[2] * c[6];
        if det.abs() < 1e-12 || !det.is_finite() {
            return None;
        }
        let mut out = [0.0f32; 9];
        for i in 0..9 {
            out[i] = c[i] / det;
        }
        Some(Homography(out))
    }
}

/// The homography taking the unit square onto four corners, in the order
/// top-left, top-right, bottom-right, bottom-left.
///
/// Solved in closed form rather than by a general linear solve: the unit
/// square is a special enough case that the answer is three subtractions
/// and two 2x2 systems.
pub fn unit_square_to_quad(q: &[(f32, f32); 4]) -> Option<Homography> {
    let (x0, y0) = q[0];
    let (x1, y1) = q[1];
    let (x2, y2) = q[2];
    let (x3, y3) = q[3];
    let dx1 = x1 - x2;
    let dy1 = y1 - y2;
    let dx2 = x3 - x2;
    let dy2 = y3 - y2;
    let sx = x0 - x1 + x2 - x3;
    let sy = y0 - y1 + y2 - y3;

    let (g, h);
    if sx.abs() < 1e-6 && sy.abs() < 1e-6 {
        // An affine quad: no perspective term.
        g = 0.0;
        h = 0.0;
    } else {
        let det = dx1 * dy2 - dx2 * dy1;
        if det.abs() < 1e-9 {
            return None;
        }
        g = (sx * dy2 - dx2 * sy) / det;
        h = (dx1 * sy - sx * dy1) / det;
    }
    let a = x1 - x0 + g * x1;
    let b = x3 - x0 + h * x3;
    let cc = x0;
    let d = y1 - y0 + g * y1;
    let e = y3 - y0 + h * y3;
    let f = y0;
    let m = Homography([a, b, cc, d, e, f, g, h, 1.0]);
    // Reject degenerate quads rather than producing NaNs downstream.
    m.invert().map(|_| m)
}

/// Sample a tile map bilinearly on premultiplied alpha.
fn sample(src: &TileMap, fx: f32, fy: f32) -> Rgba {
    let (x0, y0) = (fx.floor(), fy.floor());
    let (tx, ty) = (fx - x0, fy - y0);
    let (x0, y0) = (x0 as i32, y0 as i32);
    let mut acc = [0.0f32; 4];
    for (dx, dy, w) in [
        (0, 0, (1.0 - tx) * (1.0 - ty)),
        (1, 0, tx * (1.0 - ty)),
        (0, 1, (1.0 - tx) * ty),
        (1, 1, tx * ty),
    ] {
        if w <= 0.0 {
            continue;
        }
        let p = src.pixel(x0 + dx, y0 + dy);
        acc[0] += p.r * p.a * w;
        acc[1] += p.g * p.a * w;
        acc[2] += p.b * p.a * w;
        acc[3] += p.a * w;
    }
    if acc[3] <= 1e-6 {
        return Rgba::TRANSPARENT;
    }
    Rgba::new(acc[0] / acc[3], acc[1] / acc[3], acc[2] / acc[3], acc[3])
}

/// Clone through a plane: for every pixel in `region`, work out where it
/// sits on the plane, step back by `offset` in *plane* coordinates, and
/// fetch whatever is there.
///
/// That step in plane space is the whole trick -- an offset of half a unit
/// along a receding wall is a shorter distance in pixels at the far end
/// than at the near end, so the copy foreshortens with the wall.
#[allow(clippy::too_many_arguments)]
pub fn perspective_clone(
    src: &TileMap,
    dst: &mut TileMap,
    plane: &Homography,
    offset: (f32, f32),
    region: IntRect,
    radius: f32,
    centre: (f32, f32),
    depth: photoslop_color::Depth,
) {
    let Some(inv) = plane.invert() else { return };
    for coord in TileCoord::covering(&region) {
        let trect = coord.rect();
        let clip = trect.intersect(&region);
        if clip.is_empty() {
            continue;
        }
        let buf = dst.get_mut_or_insert(coord, depth);
        for y in clip.top..clip.bottom {
            for x in clip.left..clip.right {
                let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
                let d = (fx - centre.0).hypot(fy - centre.1);
                if d >= radius {
                    continue;
                }
                let t = 1.0 - d / radius;
                let w = t * t * (3.0 - 2.0 * t);
                // Image -> plane -> shifted -> image.
                let (u, v) = inv.apply(fx, fy);
                let (sx, sy) = plane.apply(u + offset.0, v + offset.1);
                let picked = sample(src, sx - 0.5, sy - 0.5);
                let ix = ((y - trect.top) * TILE_SIZE + (x - trect.left)) as usize;
                let under = buf.get(ix);
                buf.set(
                    ix,
                    Rgba {
                        r: under.r + (picked.r - under.r) * w,
                        g: under.g + (picked.g - under.g) * w,
                        b: under.b + (picked.b - under.b) * w,
                        a: under.a + (picked.a - under.a) * w,
                    },
                );
            }
        }
    }
}

/// What the tool is doing right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Dragging the plane's corners into place.
    DefinePlane,
    /// Cloning along it.
    Clone,
}

pub struct VanishingPointTool {
    /// Plane corners in image space: TL, TR, BR, BL.
    corners: [(f32, f32); 4],
    /// True once the plane has been placed at least once.
    placed: bool,
    phase: Phase,
    grabbed: Option<usize>,
    size: f32,
    /// Alt-clicked source point, and the plane-space offset it implies.
    source: Option<(f32, f32)>,
    offset: Option<(f32, f32)>,
    stroke: Option<(LayerId, TileMap)>,
}

impl VanishingPointTool {
    pub fn new() -> Self {
        VanishingPointTool {
            corners: [(0.0, 0.0); 4],
            placed: false,
            phase: Phase::DefinePlane,
            grabbed: None,
            size: 80.0,
            source: None,
            offset: None,
            stroke: None,
        }
    }

    fn homography(&self) -> Option<Homography> {
        unit_square_to_quad(&self.corners)
    }

    /// Put a default plane in the middle of the canvas.
    fn default_plane(&mut self, doc: &Document) {
        let r = doc.canvas_rect();
        let (w, h) = (r.width() as f32, r.height() as f32);
        let (ix, iy) = (w * 0.2, h * 0.2);
        self.corners = [
            (r.left as f32 + ix, r.top as f32 + iy),
            (r.right as f32 - ix, r.top as f32 + iy),
            (r.right as f32 - ix, r.bottom as f32 - iy),
            (r.left as f32 + ix, r.bottom as f32 - iy),
        ];
        self.placed = true;
    }
}

impl Default for VanishingPointTool {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolPlugin for VanishingPointTool {
    fn id(&self) -> &'static str {
        "vanishing_point"
    }
    fn name(&self) -> &'static str {
        "Vanishing Point"
    }
    fn icon(&self) -> &'static str {
        "vanishing-point"
    }
    fn group(&self) -> &'static str {
        "warp"
    }

    fn options(&self) -> Vec<ToolOption> {
        vec![
            ToolOption::choice(
                "vp-phase",
                "Mode",
                &["Edit Plane", "Clone"],
                match self.phase {
                    Phase::DefinePlane => 0,
                    Phase::Clone => 1,
                },
            ),
            ToolOption::slider("vp-size", "Size", self.size, 10.0, 400.0, " px"),
        ]
    }

    fn set_option(&mut self, key: &str, value: OptionValue) {
        match key {
            "vp-phase" => {
                self.phase = if value.index() == 1 {
                    Phase::Clone
                } else {
                    Phase::DefinePlane
                }
            }
            "vp-size" => self.size = value.num(),
            _ => {}
        }
    }

    fn on_activate(&mut self, ctx: &mut ToolCtx) {
        if !self.placed {
            self.default_plane(ctx.doc);
        }
    }

    fn on_pointer_down(&mut self, ctx: &mut ToolCtx, input: PointerInput) {
        if !self.placed {
            self.default_plane(ctx.doc);
        }
        if self.phase == Phase::DefinePlane {
            let r = 12.0 / ctx.state.zoom.max(0.01);
            self.grabbed = self
                .corners
                .iter()
                .position(|c| (c.0 - input.x).hypot(c.1 - input.y) <= r);
            return;
        }
        // Clone mode. Alt-click sets the source point.
        let Some(h) = self.homography() else { return };
        let Some(inv) = h.invert() else { return };
        if input.modifiers.alt {
            self.source = Some((input.x, input.y));
            self.offset = None;
            return;
        }
        let Some(src_pt) = self.source else { return };
        if self.offset.is_none() {
            // Lock the offset in plane coordinates, not pixels.
            let a = inv.apply(src_pt.0, src_pt.1);
            let b = inv.apply(input.x, input.y);
            self.offset = Some((a.0 - b.0, a.1 - b.1));
        }
        let Some(id) = ctx.doc.active_layer else {
            return;
        };
        let Some(raster) = ctx.doc.tree.find(id).and_then(|l| l.as_raster()) else {
            return;
        };
        self.stroke = Some((id, raster.tiles.clone()));
        self.on_pointer_move(ctx, input);
    }

    fn on_pointer_move(&mut self, ctx: &mut ToolCtx, input: PointerInput) {
        if self.phase == Phase::DefinePlane {
            if let Some(i) = self.grabbed {
                self.corners[i] = (input.x, input.y);
                ctx.doc.add_damage(ctx.doc.canvas_rect());
            }
            return;
        }
        let (Some(h), Some(offset), Some((id, snapshot))) =
            (self.homography(), self.offset, self.stroke.clone())
        else {
            return;
        };
        let depth = ctx.doc.depth;
        let radius = self.size / 2.0;
        let region = IntRect::new(
            (input.x - radius).floor() as i32,
            (input.y - radius).floor() as i32,
            (input.x + radius).ceil() as i32 + 1,
            (input.y + radius).ceil() as i32 + 1,
        )
        .intersect(&ctx.doc.canvas_rect());
        let Some(raster) = ctx.doc.tree.find_mut(id).and_then(|l| l.as_raster_mut()) else {
            return;
        };
        let mut tiles = raster.tiles.clone();
        perspective_clone(
            &snapshot,
            &mut tiles,
            &h,
            offset,
            region,
            radius,
            (input.x, input.y),
            depth,
        );
        raster.tiles = tiles;
        ctx.doc.add_damage(region);
    }

    fn on_pointer_up(&mut self, ctx: &mut ToolCtx, _input: PointerInput) {
        self.grabbed = None;
        // Record the whole stroke as one entry.
        if let Some((id, snapshot)) = self.stroke.take() {
            let after = ctx
                .doc
                .tree
                .find(id)
                .and_then(|l| l.as_raster())
                .map(|r| r.tiles.clone());
            if let Some(after) = after {
                if let Some(raster) = ctx.doc.tree.find_mut(id).and_then(|l| l.as_raster_mut()) {
                    raster.tiles = snapshot;
                }
                let mut edit = ctx.doc.begin_edit("Vanishing Point");
                edit.replace_layer_tiles(id, after);
                edit.commit();
            }
        }
    }

    fn on_cancel(&mut self, _ctx: &mut ToolCtx) {
        self.grabbed = None;
        self.stroke = None;
        self.source = None;
        self.offset = None;
    }

    fn overlays(&self, _doc: &Document, _state: &EditorState) -> Vec<Overlay> {
        if !self.placed {
            return Vec::new();
        }
        let mut out = vec![Overlay::AntsPolygon(self.corners.to_vec())];
        // The plane's own grid, drawn through the homography so it shows
        // the perspective the clone will follow.
        if let Some(h) = self.homography() {
            for i in 1..4 {
                let t = i as f32 / 4.0;
                let a = h.apply(t, 0.0);
                let b = h.apply(t, 1.0);
                out.push(Overlay::Line {
                    x1: a.0,
                    y1: a.1,
                    x2: b.0,
                    y2: b.1,
                });
                let c = h.apply(0.0, t);
                let d = h.apply(1.0, t);
                out.push(Overlay::Line {
                    x1: c.0,
                    y1: c.1,
                    x2: d.0,
                    y2: d.1,
                });
            }
        }
        for c in &self.corners {
            out.push(Overlay::Rect(IntRect::new(
                c.0 as i32 - 4,
                c.1 as i32 - 4,
                c.0 as i32 + 4,
                c.1 as i32 + 4,
            )));
        }
        out
    }
}
