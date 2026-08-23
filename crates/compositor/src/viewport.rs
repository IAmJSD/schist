//! Viewport resampling: turn a grid of composited display tiles into the
//! single BGRA image the canvas paints.
//!
//! Extracted from the canvas view so the CPU reference and the GPU
//! backend implement one contract. Integer zooms stay nearest-neighbour
//! so pixels stay crisp; fractional and rotated views interpolate; zooming
//! out box-averages the whole pixel footprint to damp aliasing.
//! Transparency is checkered at a fixed *screen* size, the app background
//! fills the surround, and the output is BGRA with opaque alpha.

use rayon::prelude::*;
use schist_core::{IntRect, TILE_PIXELS, TILE_SIZE};
use std::sync::Arc;

/// Everything that determines how document pixels land on screen.
///
/// `grid` (passed alongside) is row-major `grid_cols * grid_rows` slots of
/// RGBA8 display tiles covering the visible tile span; `None` means
/// transparent. Coordinates are *device* pixels.
#[derive(Debug, Clone, Copy)]
pub struct ViewportParams {
    /// Output size in device pixels.
    pub width: usize,
    pub height: usize,
    /// Pan offset in device pixels.
    pub origin: (f32, f32),
    pub zoom: f32,
    pub scale_factor: f32,
    /// View rotation in radians, about the viewport centre.
    pub rotation: f32,
    /// The document's canvas rect; outside it the surround colour shows.
    pub canvas: IntRect,
    /// Tile coordinate of the grid's top-left slot.
    pub grid_origin: (i32, i32),
    pub grid_cols: usize,
    pub grid_rows: usize,
    /// Surround grey level (low byte used), from the app palette.
    pub surround: u32,
}

impl ViewportParams {
    /// Map a device pixel to document coordinates.
    #[inline]
    pub fn doc_at(&self, dx: f32, dy: f32) -> (f32, f32) {
        let sf = self.scale_factor;
        let inv_zoom = 1.0 / self.zoom;
        let centre = (self.width as f32 / 2.0, self.height as f32 / 2.0);
        let (rs, rc) = (-self.rotation).sin_cos();
        let (ox, oy) = (dx - centre.0, dy - centre.1);
        let (rx, ry) = (ox * rc - oy * rs + centre.0, ox * rs + oy * rc + centre.1);
        (
            (rx - self.origin.0) * inv_zoom / sf,
            (ry - self.origin.1) * inv_zoom / sf,
        )
    }

    /// Nearest-neighbour is only exact for unrotated integer device zooms.
    #[inline]
    pub fn crisp(&self) -> bool {
        let dev_zoom = self.zoom * self.scale_factor;
        self.rotation == 0.0 && dev_zoom >= 1.0 && dev_zoom.fract() == 0.0
    }

    /// Box-filter taps per axis when minifying, 0 when magnifying.
    #[inline]
    pub fn box_taps(&self) -> usize {
        let dev_zoom = self.zoom * self.scale_factor;
        if dev_zoom < 1.0 {
            // Tap spacing stays under one document pixel up to 8x
            // minification; past that the cost stops being worth it.
            ((1.0 / dev_zoom).ceil() as usize).clamp(2, 8)
        } else {
            0
        }
    }
}

/// CPU reference: resample the tile grid into a BGRA image
/// (`width * height * 4` bytes, opaque).
pub fn render_viewport_cpu(p: &ViewportParams, grid: &[Option<Arc<Vec<u8>>>]) -> Vec<u8> {
    let (tx0, ty0) = p.grid_origin;
    let (cols, rows) = (p.grid_cols, p.grid_rows);
    let canvas_rect = p.canvas;
    let sample = |x: i32, y: i32| -> [u8; 4] {
        if !canvas_rect.contains(x, y) {
            return [0, 0, 0, 0];
        }
        let tx = x.div_euclid(TILE_SIZE) - tx0;
        let ty = y.div_euclid(TILE_SIZE) - ty0;
        if tx < 0 || ty < 0 || tx as usize >= cols || ty as usize >= rows {
            return [0, 0, 0, 0];
        }
        match &grid[ty as usize * cols + tx as usize] {
            Some(tile) => {
                let lx = x.rem_euclid(TILE_SIZE) as usize;
                let ly = y.rem_euclid(TILE_SIZE) as usize;
                let at = (ly * TILE_SIZE as usize + lx) * 4;
                [tile[at], tile[at + 1], tile[at + 2], tile[at + 3]]
            }
            None => [0, 0, 0, 0],
        }
    };

    // How a screen pixel samples the document:
    //  - Integer zoom, unrotated: nearest. One document pixel maps to
    //    an exact block, so this is crisp and alias-free.
    //  - Other magnification (fractional zoom, or any rotation):
    //    bilinear. Nearest here duplicates rows and columns unevenly
    //    and staircases every antialiased edge.
    //  - Minification: several document pixels land inside each screen
    //    pixel, so average an n x n grid spanning the pixel's whole
    //    footprint. Bilinear reads only the four nearest and skips
    //    pixels outright below 50% zoom, which is where the shimmer
    //    and jagged edges came from.
    let footprint = 1.0 / (p.zoom * p.scale_factor); // document px per device px
    let crisp = p.crisp();
    let box_taps = p.box_taps();
    // Straight-alpha result from a premultiplied accumulator, so
    // averaging across an edge doesn't fringe.
    let resolve = |acc: [f32; 4]| -> [u8; 4] {
        if acc[3] <= 1e-6 {
            [0, 0, 0, 0]
        } else {
            [
                (acc[0] / acc[3]).round().clamp(0.0, 255.0) as u8,
                (acc[1] / acc[3]).round().clamp(0.0, 255.0) as u8,
                (acc[2] / acc[3]).round().clamp(0.0, 255.0) as u8,
                (acc[3] * 255.0).round().clamp(0.0, 255.0) as u8,
            ]
        }
    };
    let surround = p.surround & 0xFF;
    let width = p.width;
    let mut bgra = vec![0u8; p.width * p.height * 4];
    bgra.par_chunks_mut(width * 4)
        .enumerate()
        .for_each(|(row, line)| {
            let dy = row as f32 + 0.5;
            for col in 0..width {
                let dx = col as f32 + 0.5;
                let (fx, fy) = p.doc_at(dx, dy);
                let px = if crisp {
                    sample(fx.floor() as i32, fy.floor() as i32)
                } else if box_taps == 0 {
                    // Bilinear over the four neighbours.
                    let (sx, sy) = (fx - 0.5, fy - 0.5);
                    let (ix, iy) = (sx.floor(), sy.floor());
                    let (tx, ty) = (sx - ix, sy - iy);
                    let mut acc = [0.0f32; 4];
                    for (dxi, wx) in [(0, 1.0 - tx), (1, tx)] {
                        for (dyi, wy) in [(0, 1.0 - ty), (1, ty)] {
                            let w = wx * wy;
                            if w <= 0.0 {
                                continue;
                            }
                            let s = sample(ix as i32 + dxi, iy as i32 + dyi);
                            let a = s[3] as f32 / 255.0 * w;
                            acc[0] += s[0] as f32 * a;
                            acc[1] += s[1] as f32 * a;
                            acc[2] += s[2] as f32 * a;
                            acc[3] += a;
                        }
                    }
                    resolve(acc)
                } else {
                    // Box average over the footprint. The box is kept
                    // axis-aligned even for rotated views: a square is
                    // near enough rotation-invariant at this size.
                    let n = box_taps;
                    let mut acc = [0.0f32; 4];
                    for sy in 0..n {
                        let oy = ((sy as f32 + 0.5) / n as f32 - 0.5) * footprint;
                        for sx in 0..n {
                            let ox = ((sx as f32 + 0.5) / n as f32 - 0.5) * footprint;
                            let s = sample((fx + ox).floor() as i32, (fy + oy).floor() as i32);
                            let a = s[3] as f32 / 255.0;
                            acc[0] += s[0] as f32 * a;
                            acc[1] += s[1] as f32 * a;
                            acc[2] += s[2] as f32 * a;
                            acc[3] += a;
                        }
                    }
                    resolve(acc)
                };

                // Transparency shows the checkerboard inside the canvas
                // and the app background outside it.
                let inside = {
                    let (fx, fy) = (fx.floor() as i32, fy.floor() as i32);
                    canvas_rect.contains(fx, fy)
                };
                let bg = if inside {
                    if ((col >> 3) + (row >> 3)) & 1 == 0 {
                        0xFFu32
                    } else {
                        0xCCu32
                    }
                } else {
                    surround
                };
                let (r, g, b, a) = (px[0] as u32, px[1] as u32, px[2] as u32, px[3] as u32);
                let inv = 255 - a;
                let at = col * 4;
                line[at] = ((b * a + bg * inv) / 255) as u8;
                line[at + 1] = ((g * a + bg * inv) / 255) as u8;
                line[at + 2] = ((r * a + bg * inv) / 255) as u8;
                line[at + 3] = 255;
            }
        });
    bgra
}

/// Sanity check the grid a caller hands to a renderer.
pub fn grid_len_ok(p: &ViewportParams, grid: &[Option<Arc<Vec<u8>>>]) -> bool {
    grid.len() == p.grid_cols * p.grid_rows
        && grid
            .iter()
            .flatten()
            .all(|t| t.len() == TILE_PIXELS * 4)
}
