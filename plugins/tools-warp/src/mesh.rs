//! A displacement mesh: where every pixel of the result comes from.
//!
//! Liquify, Puppet Warp and the perspective tools all end up doing the
//! same thing -- deciding, for each destination pixel, which source pixel
//! to sample -- so they share this. The displacement is stored on a coarse
//! grid and interpolated, because storing it per pixel would cost
//! 96 MB on a 12-megapixel layer for no visible gain.

use rayon::prelude::*;
use schist_color::{Depth, Rgba};
use schist_core::{IntRect, TileCoord, TileMap, TILE_SIZE};
use std::sync::atomic::{AtomicU64, Ordering};

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

/// A rectangular slice of a [`Mesh`], in the flat form the kernel takes.
pub struct Subgrid {
    /// `(dx, dy)` per vertex, interleaved, row major.
    pub offsets: Vec<f32>,
    pub cols: usize,
    pub rows: usize,
    /// Document position of this slice's vertex (0, 0).
    pub origin: (i32, i32),
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

    /// The destination pixels a dab at `(cx, cy)` can change.
    ///
    /// A dab only moves vertices inside its radius, and a destination
    /// pixel interpolates the four vertices around it, so the influence
    /// stops one cell past the outermost vertex the brush reached. This is
    /// what lets a stroke re-render its own footprint instead of the whole
    /// layer.
    pub fn dab_rect(&self, cx: f32, cy: f32, radius: f32) -> IntRect {
        let reach = radius + CELL;
        IntRect::new(
            (cx - reach).floor() as i32,
            (cy - reach).floor() as i32,
            (cx + reach).ceil() as i32 + 1,
            (cy + reach).ceil() as i32 + 1,
        )
        .intersect(&self.rect)
    }

    /// The vertices `rect` samples, interleaved `(dx, dy)`, with the grid
    /// shape and origin that go with them.
    ///
    /// A dab re-renders a disc, and handing the kernel the whole grid for
    /// it would copy far more offsets than the disc reads — a megabyte and
    /// a half per pointer move on a 12-megapixel layer. The slice keeps a
    /// cell of margin on every side, so a pixel inside `rect` interpolates
    /// the same four vertices it would have in the full grid, and where the
    /// margin runs out it is because the full grid ends there too, which
    /// makes clamping agree as well.
    pub fn subgrid(&self, rect: IntRect) -> Subgrid {
        if self.cols < 2 || self.rows < 2 || rect.is_empty() {
            return Subgrid {
                offsets: self.offsets.iter().flat_map(|(x, y)| [*x, *y]).collect(),
                cols: self.cols,
                rows: self.rows,
                origin: (self.rect.left, self.rect.top),
            };
        }
        let lo = |v: i32, origin: i32| {
            (((v - origin) as f32 / CELL).floor() as i64 - 1).clamp(0, i64::MAX) as usize
        };
        let hi = |v: i32, origin: i32, n: usize| {
            (((v - origin) as f32 / CELL).ceil() as i64 + 1).clamp(0, n as i64 - 1) as usize
        };
        let lo_c = lo(rect.left, self.rect.left).min(self.cols - 1);
        let lo_r = lo(rect.top, self.rect.top).min(self.rows - 1);
        let hi_c = hi(rect.right, self.rect.left, self.cols).max(lo_c);
        let hi_r = hi(rect.bottom, self.rect.top, self.rows).max(lo_r);
        let (cols, rows) = (hi_c - lo_c + 1, hi_r - lo_r + 1);
        let mut offsets = Vec::with_capacity(cols * rows * 2);
        for row in lo_r..=hi_r {
            let base = row * self.cols;
            for &(dx, dy) in &self.offsets[base + lo_c..=base + hi_c] {
                offsets.push(dx);
                offsets.push(dy);
            }
        }
        Subgrid {
            offsets,
            cols,
            rows,
            origin: (
                self.rect.left + (lo_c as f32 * CELL) as i32,
                self.rect.top + (lo_r as f32 * CELL) as i32,
            ),
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

/// Identify one warp source for the life of a drag.
///
/// Puppet Warp re-renders everything its mesh covers from a snapshot taken
/// when the gesture started, so the pixels never change while the mesh
/// does — which is exactly what lets an accelerated backend keep the source
/// plane resident and pay only for the result coming back. Anything that
/// hands out a token is promising the pixels behind it are frozen. (Liquify
/// wants no token: a dab redoes its own footprint, which is small enough
/// that the transfer would cost more than the work.)
pub fn next_source_token() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Resample `src` through `mesh`, writing into a fresh tile map.
///
/// Sampling is bilinear on premultiplied alpha, so warping something with
/// a soft edge does not fringe it. `src_token` names the snapshot for a
/// backend that can cache it (see [`next_source_token`]); 0 disables that.
pub fn warp_tiles(
    src: &TileMap,
    mesh: &Mesh,
    depth: Depth,
    clip: IntRect,
    src_token: u64,
) -> TileMap {
    let mut out = TileMap::new();
    warp_into(&mut out, src, mesh, depth, clip, src_token);
    out
}

/// Resample `src` through `mesh` into `dst`, touching only pixels inside
/// `clip`.
///
/// Splitting this out is what lets a Liquify stroke re-render just the
/// footprint of the dab it has applied: everything outside `clip` in `dst`
/// was warped through the same mesh values by an earlier call and is still
/// current, because a dab only moves the vertices under the brush.
pub fn warp_into(
    dst: &mut TileMap,
    src: &TileMap,
    mesh: &Mesh,
    depth: Depth,
    clip: IntRect,
    src_token: u64,
) {
    let region = mesh.rect.intersect(&clip);
    if region.is_empty() {
        return;
    }
    let stored = src.tile_bounds();
    let grid = mesh.subgrid(region);
    // Which source pixels to put in front of the kernel, and it follows
    // from the token. A token promises a fixed plane across a drag, so a
    // backend can keep it resident and a sweep of the whole layer pays for
    // the transfer once; the plane is then the entire snapshot, because
    // cropping it would make the token name different pixels on different
    // calls. No token means a one-off — in practice the footprint of a
    // single dab — and flattening megabytes of layer to resample a disc of
    // it costs far more than the resample, so the plane shrinks to what the
    // displacement over this region can actually reach.
    let plane = if src_token != 0 {
        stored
    } else {
        let reach = grid.offsets.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        region.inflated(reach.ceil() as i32 + 2).intersect(&stored)
    };
    let params = schist_fx::WarpParams {
        src_width: plane.width().max(0) as usize,
        src_height: plane.height().max(0) as usize,
        src_origin: (plane.left, plane.top),
        dst_origin: (region.left, region.top),
        dst_width: region.width().max(0) as usize,
        dst_height: region.height().max(0) as usize,
        mesh: &grid.offsets,
        mesh_cols: grid.cols,
        mesh_rows: grid.rows,
        cell: CELL,
        mesh_origin: grid.origin,
        src_token,
    };
    let warped = schist_fx::warp(&params, || flatten(src, plane));

    let stride = params.dst_width;
    for coord in TileCoord::covering(&region) {
        let trect = coord.rect();
        let cliprect = trect.intersect(&region);
        if cliprect.is_empty() {
            continue;
        }
        let buf = dst.get_mut_or_insert(coord, depth);
        for y in cliprect.top..cliprect.bottom {
            for x in cliprect.left..cliprect.right {
                let i = ((y - region.top) as usize * stride + (x - region.left) as usize) * 4;
                let ix = ((y - trect.top) * TILE_SIZE + (x - trect.left)) as usize;
                buf.set(
                    ix,
                    Rgba::new(warped[i], warped[i + 1], warped[i + 2], warped[i + 3]),
                );
            }
        }
    }
}

/// Copy a tile map's stored region out into one flat straight-alpha plane.
///
/// A row at a time across the cores, and within a row a tile at a time: a
/// map lookup per pixel costs about as much as the resample this is
/// feeding. A tile the map does not store is transparent, which the buffer
/// already is.
fn flatten(src: &TileMap, rect: IntRect) -> Vec<f32> {
    let (w, h) = (rect.width().max(0) as usize, rect.height().max(0) as usize);
    let mut out = vec![0.0f32; w * h * 4];
    if w == 0 || h == 0 {
        return out;
    }
    out.par_chunks_mut(w * 4)
        .enumerate()
        .for_each(|(row, dst)| {
            let y = rect.top + row as i32;
            let ty = y.div_euclid(TILE_SIZE);
            let ly = y.rem_euclid(TILE_SIZE) * TILE_SIZE;
            let mut x = rect.left;
            while x < rect.right {
                let tx = x.div_euclid(TILE_SIZE);
                let end = ((tx + 1) * TILE_SIZE).min(rect.right);
                if let Some(tile) = src.get(TileCoord { tx, ty }) {
                    for x in x..end {
                        let p = tile.get((ly + x.rem_euclid(TILE_SIZE)) as usize);
                        let i = (x - rect.left) as usize * 4;
                        dst[i..i + 4].copy_from_slice(&[p.r, p.g, p.b, p.a]);
                    }
                }
                x = end;
            }
        });
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
