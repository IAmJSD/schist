//! A displacement mesh: where every pixel of the result comes from.
//!
//! Liquify, Puppet Warp and the perspective tools all end up doing the
//! same thing -- deciding, for each destination pixel, which source pixel
//! to sample -- so they share this. The displacement is stored on a coarse
//! grid and interpolated, because storing it per pixel would cost
//! 96 MB on a 12-megapixel layer for no visible gain.

use schist_color::{Depth, Rgba};
use schist_core::{IntRect, TileCoord, TileMap, TILE_SIZE};

/// Grid spacing in pixels. Small enough that a brush of any usable size
/// covers several cells, large enough to keep the mesh cheap.
pub const CELL: f32 = 4.0;

/// Per-vertex displacement over a rectangle, in source pixels.
#[derive(Debug, Clone)]
pub struct Mesh {
    /// The region the mesh covers, in document coordinates.
    pub rect: IntRect,
    pub cols: usize,
    pub rows: usize,
    /// `(dx, dy)` per vertex: where this point's colour is fetched *from*,
    /// relative to itself. Zero everywhere means the identity.
    pub offsets: Vec<(f32, f32)>,
}

impl Mesh {
    pub fn new(rect: IntRect) -> Mesh {
        let cols = (rect.width().max(0) as f32 / CELL).ceil() as usize + 1;
        let rows = (rect.height().max(0) as f32 / CELL).ceil() as usize + 1;
        Mesh {
            rect,
            cols,
            rows,
            offsets: vec![(0.0, 0.0); cols.max(1) * rows.max(1)],
        }
    }

    pub fn is_identity(&self) -> bool {
        self.offsets
            .iter()
            .all(|(x, y)| x.abs() < 1e-4 && y.abs() < 1e-4)
    }

    /// Document position of a vertex.
    #[inline]
    pub fn vertex_pos(&self, col: usize, row: usize) -> (f32, f32) {
        (
            self.rect.left as f32 + col as f32 * CELL,
            self.rect.top as f32 + row as f32 * CELL,
        )
    }

    /// Visit every vertex within `radius` of (cx, cy), with its falloff
    /// weight. The weight is a smooth bump: 1 at the centre, 0 at the rim.
    pub fn for_each_near(
        &mut self,
        cx: f32,
        cy: f32,
        radius: f32,
        mut f: impl FnMut(&mut (f32, f32), f32, f32, f32),
    ) {
        if radius <= 0.0 {
            return;
        }
        let lo_c = (((cx - radius) - self.rect.left as f32) / CELL)
            .floor()
            .max(0.0) as usize;
        let lo_r = (((cy - radius) - self.rect.top as f32) / CELL)
            .floor()
            .max(0.0) as usize;
        let hi_c = ((((cx + radius) - self.rect.left as f32) / CELL)
            .ceil()
            .max(0.0) as usize)
            .min(self.cols.saturating_sub(1));
        let hi_r = ((((cy + radius) - self.rect.top as f32) / CELL)
            .ceil()
            .max(0.0) as usize)
            .min(self.rows.saturating_sub(1));
        for row in lo_r..=hi_r {
            for col in lo_c..=hi_c {
                let (vx, vy) = self.vertex_pos(col, row);
                let d = (vx - cx).hypot(vy - cy);
                if d >= radius {
                    continue;
                }
                let t = 1.0 - d / radius;
                // Smoothstep, so a stroke has no hard rim.
                let w = t * t * (3.0 - 2.0 * t);
                let i = row * self.cols + col;
                f(&mut self.offsets[i], w, vx - cx, vy - cy);
            }
        }
    }

    /// Bilinear displacement at a document position.
    pub fn sample(&self, x: f32, y: f32) -> (f32, f32) {
        if self.cols < 2 || self.rows < 2 {
            return (0.0, 0.0);
        }
        let fx = ((x - self.rect.left as f32) / CELL).clamp(0.0, (self.cols - 1) as f32);
        let fy = ((y - self.rect.top as f32) / CELL).clamp(0.0, (self.rows - 1) as f32);
        let (c0, r0) = (fx.floor() as usize, fy.floor() as usize);
        let (c1, r1) = ((c0 + 1).min(self.cols - 1), (r0 + 1).min(self.rows - 1));
        let (tx, ty) = (fx - c0 as f32, fy - r0 as f32);
        let at = |c: usize, r: usize| self.offsets[r * self.cols + c];
        let (a, b, cc, d) = (at(c0, r0), at(c1, r0), at(c0, r1), at(c1, r1));
        let top = (a.0 + (b.0 - a.0) * tx, a.1 + (b.1 - a.1) * tx);
        let bottom = (cc.0 + (d.0 - cc.0) * tx, cc.1 + (d.1 - cc.1) * tx);
        (
            top.0 + (bottom.0 - top.0) * ty,
            top.1 + (bottom.1 - top.1) * ty,
        )
    }

    /// Pull every offset back towards zero, which is what Liquify's
    /// Reconstruct brush does.
    pub fn relax(&mut self, cx: f32, cy: f32, radius: f32, amount: f32) {
        self.for_each_near(cx, cy, radius, |off, w, _, _| {
            let k = 1.0 - (w * amount).clamp(0.0, 1.0);
            off.0 *= k;
            off.1 *= k;
        });
    }
}

/// Resample `src` through `mesh`, writing into a fresh tile map.
///
/// Sampling is bilinear on premultiplied alpha, so warping something with
/// a soft edge does not fringe it.
pub fn warp_tiles(src: &TileMap, mesh: &Mesh, depth: Depth, clip: IntRect) -> TileMap {
    let mut out = TileMap::new();
    let region = mesh.rect.intersect(&clip);
    if region.is_empty() {
        return out;
    }
    let fetch = |fx: f32, fy: f32| -> Rgba {
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
    };

    for coord in TileCoord::covering(&region) {
        let trect = coord.rect();
        let cliprect = trect.intersect(&region);
        if cliprect.is_empty() {
            continue;
        }
        let buf = out.get_mut_or_insert(coord, depth);
        for y in cliprect.top..cliprect.bottom {
            for x in cliprect.left..cliprect.right {
                let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
                let (dx, dy) = mesh.sample(fx, fy);
                let px = fetch(fx + dx - 0.5, fy + dy - 0.5);
                let ix = ((y - trect.top) * TILE_SIZE + (x - trect.left)) as usize;
                buf.set(ix, px);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect() -> IntRect {
        IntRect::new(0, 0, 64, 64)
    }

    #[test]
    fn a_fresh_mesh_is_the_identity() {
        let m = Mesh::new(rect());
        assert!(m.is_identity());
        assert_eq!(m.sample(30.0, 30.0), (0.0, 0.0));
    }

    #[test]
    fn a_push_falls_off_to_nothing_at_the_rim() {
        let mut m = Mesh::new(rect());
        m.for_each_near(32.0, 32.0, 16.0, |off, w, _, _| {
            off.0 -= 10.0 * w;
        });
        let centre = m.sample(32.0, 32.0).0;
        let edge = m.sample(48.0, 32.0).0;
        let outside = m.sample(60.0, 32.0).0;
        assert!(centre < -8.0, "centre barely moved: {centre}");
        assert!(edge.abs() < 2.0, "rim moved too much: {edge}");
        assert_eq!(outside, 0.0, "displacement leaked past the brush");
    }

    #[test]
    fn relax_undoes_a_push() {
        let mut m = Mesh::new(rect());
        m.for_each_near(32.0, 32.0, 16.0, |off, w, _, _| off.0 -= 10.0 * w);
        let before = m.sample(32.0, 32.0).0.abs();
        for _ in 0..20 {
            m.relax(32.0, 32.0, 16.0, 0.5);
        }
        assert!(
            m.sample(32.0, 32.0).0.abs() < before * 0.1,
            "reconstruct did not undo the push"
        );
    }

    #[test]
    fn sampling_outside_the_mesh_clamps_rather_than_indexing_out_of_bounds() {
        let m = Mesh::new(rect());
        let _ = m.sample(-1000.0, -1000.0);
        let _ = m.sample(1e6, 1e6);
    }

    #[test]
    fn a_degenerate_rect_makes_a_usable_mesh() {
        let m = Mesh::new(IntRect::new(0, 0, 0, 0));
        assert_eq!(m.sample(0.0, 0.0), (0.0, 0.0));
    }
}
