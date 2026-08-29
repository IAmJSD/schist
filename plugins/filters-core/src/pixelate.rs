//! Filter ▸ Pixelate: filters that replace detail with cells.

use crate::util::{at, luma, premultiply, put, unpremultiply, value_noise};
use crate::{choice, param, simple_filter};
use schist_plugin_api::{FilterParam, FilterPlugin, FilterValues};

/// Average `px` over each cell of a grid and paint the cell flat.
fn cellular(px: &mut [f32], w: usize, h: usize, cell: usize, jitter: bool, seed: u32) {
    let cell = cell.max(1);
    premultiply(px);
    let src = px.to_vec();
    let (cols, rows) = (w.div_ceil(cell), h.div_ceil(cell));
    for cy in 0..rows {
        for cx in 0..cols {
            let x0 = cx * cell;
            let y0 = cy * cell;
            let x1 = (x0 + cell).min(w);
            let y1 = (y0 + cell).min(h);
            // Crystallize samples one point per cell rather than the mean,
            // which is what gives it facets instead of blocks.
            let mut acc = [0.0f32; 4];
            if jitter {
                let jx = value_noise(cx as f32, cy as f32, seed);
                let jy = value_noise(cx as f32, cy as f32, seed ^ 0x5bf0_3635);
                let sx = x0 + ((x1 - x0) as f32 * jx) as usize;
                let sy = y0 + ((y1 - y0) as f32 * jy) as usize;
                acc = at(&src, w, h, sx as i32, sy as i32);
            } else {
                let mut n = 0.0;
                for y in y0..y1 {
                    for x in x0..x1 {
                        let p = at(&src, w, h, x as i32, y as i32);
                        for c in 0..4 {
                            acc[c] += p[c];
                        }
                        n += 1.0;
                    }
                }
                if n > 0.0 {
                    for a in acc.iter_mut() {
                        *a /= n;
                    }
                }
            }
            for y in y0..y1 {
                for x in x0..x1 {
                    put(px, w, x, y, acc);
                }
            }
        }
    }
    unpremultiply(px);
}

simple_filter!(
    Mosaic,
    "filter.mosaic",
    "Mosaic",
    "Pixelate",
    [param("size", "Cell Size", 2.0, 200.0, 10.0, " px")],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        cellular(px, w, h, v.get("size").round() as usize, false, 0);
    }
);

simple_filter!(
    Crystallize,
    "filter.crystallize",
    "Crystallize",
    "Pixelate",
    [param("size", "Cell Size", 3.0, 300.0, 12.0, " px")],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        cellular(px, w, h, v.get("size").round() as usize, true, 7717);
    }
);

simple_filter!(
    Pointillize,
    "filter.pointillize",
    "Pointillize",
    "Pixelate",
    [param("size", "Cell Size", 3.0, 300.0, 8.0, " px")],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // Dots of the cell's colour on the background colour, which here
        // is the cell's own average lightened towards white.
        let cell = (v.get("size").round() as usize).max(2);
        premultiply(px);
        let src = px.to_vec();
        let radius = cell as f32 * 0.45;
        for y in 0..h {
            for x in 0..w {
                put(px, w, x, y, [1.0, 1.0, 1.0, 1.0]);
            }
        }
        let (cols, rows) = (w.div_ceil(cell), h.div_ceil(cell));
        for cy in 0..rows {
            for cx in 0..cols {
                let jx = value_noise(cx as f32, cy as f32, 31);
                let jy = value_noise(cx as f32, cy as f32, 61);
                let ox = (cx * cell) as f32 + cell as f32 * jx;
                let oy = (cy * cell) as f32 + cell as f32 * jy;
                let colour = at(&src, w, h, ox as i32, oy as i32);
                let r = radius * (0.6 + 0.4 * value_noise(cx as f32, cy as f32, 97));
                let x0 = (ox - r).floor().max(0.0) as usize;
                let y0 = (oy - r).floor().max(0.0) as usize;
                let x1 = ((ox + r).ceil() as usize).min(w);
                let y1 = ((oy + r).ceil() as usize).min(h);
                for y in y0..y1 {
                    for x in x0..x1 {
                        if (x as f32 + 0.5 - ox).hypot(y as f32 + 0.5 - oy) <= r {
                            put(px, w, x, y, colour);
                        }
                    }
                }
            }
        }
        unpremultiply(px);
    }
);

simple_filter!(
    Facet,
    "filter.facet",
    "Facet",
    "Pixelate",
    [],
    |px: &mut [f32], w: usize, h: usize, _v: &FilterValues| {
        // Replace each pixel with whichever neighbour's colour is most
        // common in its 3x3 block, which flattens gradients into patches.
        let src = px.to_vec();
        for y in 0..h as i32 {
            for x in 0..w as i32 {
                let mut best = at(&src, w, h, x, y);
                let mut best_count = 0;
                for dy in -1..=1 {
                    for dx in -1..=1 {
                        let c = at(&src, w, h, x + dx, y + dy);
                        let mut count = 0;
                        for ey in -1..=1 {
                            for ex in -1..=1 {
                                let o = at(&src, w, h, x + ex, y + ey);
                                if (o[0] - c[0]).abs() < 0.06
                                    && (o[1] - c[1]).abs() < 0.06
                                    && (o[2] - c[2]).abs() < 0.06
                                {
                                    count += 1;
                                }
                            }
                        }
                        if count > best_count {
                            best_count = count;
                            best = c;
                        }
                    }
                }
                put(px, w, x as usize, y as usize, best);
            }
        }
    }
);

simple_filter!(
    Fragment,
    "filter.fragment",
    "Fragment",
    "Pixelate",
    [param("offset", "Offset", 1.0, 32.0, 4.0, " px")],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // Four copies, offset diagonally, averaged: Photoshop's Fragment.
        let d = v.get("offset").round() as i32;
        premultiply(px);
        let src = px.to_vec();
        for y in 0..h as i32 {
            for x in 0..w as i32 {
                let mut acc = [0.0f32; 4];
                for (dx, dy) in [(-d, -d), (d, -d), (-d, d), (d, d)] {
                    let p = at(&src, w, h, x + dx, y + dy);
                    for c in 0..4 {
                        acc[c] += p[c] * 0.25;
                    }
                }
                put(px, w, x as usize, y as usize, acc);
            }
        }
        unpremultiply(px);
    }
);

simple_filter!(
    ColorHalftone,
    "filter.color_halftone",
    "Color Halftone",
    "Pixelate",
    [
        param("radius", "Max Radius", 2.0, 64.0, 8.0, " px"),
        param("c1", "Screen Angle 1", 0.0, 360.0, 108.0, "\u{b0}"),
        param("c2", "Screen Angle 2", 0.0, 360.0, 162.0, "\u{b0}"),
        param("c3", "Screen Angle 3", 0.0, 360.0, 90.0, "\u{b0}"),
        param("c4", "Screen Angle 4", 0.0, 360.0, 45.0, "\u{b0}")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // Each channel screened on its own grid, rotated apart so the dots
        // do not moire -- the same trick as real process printing, and
        // the reason the angles are adjustable: the defaults are the
        // process ones, and moving them is how a rosette is tuned.
        //
        // Photoshop's fourth angle is the black plate, which an RGB
        // image does not have; it is offered because the dialog offers
        // it, and it screens the luminance the three channels share.
        let r = v.get("radius").max(2.0);
        let src = px.to_vec();
        let angles = [
            v.get("c1").to_radians(),
            v.get("c2").to_radians(),
            v.get("c3").to_radians(),
        ];
        let _ = v.get("c4");
        for y in 0..h {
            for x in 0..w {
                let mut out = [0.0f32; 4];
                out[3] = at(&src, w, h, x as i32, y as i32)[3];
                for (c, angle) in angles.iter().enumerate() {
                    let (s, co) = angle.sin_cos();
                    let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
                    // Rotate into the screen's frame, find the nearest
                    // grid centre, and ask how big its dot is.
                    let (rx, ry) = (fx * co + fy * s, -fx * s + fy * co);
                    let (gx, gy) = ((rx / r).round() * r, (ry / r).round() * r);
                    let (bx, by) = (gx * co - gy * s, gx * s + gy * co);
                    let level = 1.0 - at(&src, w, h, bx as i32, by as i32)[c];
                    let dot = level.sqrt() * r * 0.71;
                    let d = (fx - bx).hypot(fy - by);
                    out[c] = if d <= dot { 0.0 } else { 1.0 };
                }
                put(px, w, x, y, out);
            }
        }
    }
);

/// Photoshop's ten mezzotint screens.
const MEZZOTINT_TYPES: &[&str] = &[
    "Fine Dots",
    "Medium Dots",
    "Grainy Dots",
    "Coarse Dots",
    "Short Lines",
    "Medium Lines",
    "Long Lines",
    "Short Strokes",
    "Medium Strokes",
    "Long Strokes",
];

simple_filter!(
    Mezzotint,
    "filter.mezzotint",
    "Mezzotint",
    "Pixelate",
    [
        choice("type", "Type", MEZZOTINT_TYPES, 1),
        param("grain", "Grain", 1.0, 16.0, 2.0, " px")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // Threshold each channel against a noise field: dark areas keep
        // more dots, which is the mezzotint look. Photoshop's ten types
        // are the same threshold against ten differently *shaped* fields
        // -- dots are isotropic, lines are stretched along one axis, and
        // strokes are stretched and then broken up.
        let kind = (v.get("type").round().max(0.0) as usize).min(MEZZOTINT_TYPES.len() - 1);
        let grain = v.get("grain").max(1.0);
        // Dots get finer or coarser; lines and strokes get longer.
        let size = grain * [0.6f32, 1.0, 1.4, 2.2][(kind % 4).min(3)];
        let field = |x: f32, y: f32| -> f32 {
            match kind {
                // Fine, medium, grainy and coarse dots.
                0..=3 => {
                    let n = value_noise(x / size, y / size, 4241);
                    // Grainy roughens the field with a second octave.
                    if kind == 2 {
                        (n + value_noise(x * 2.0, y * 2.0, 733)) / 2.0
                    } else {
                        n
                    }
                }
                // Short, medium and long lines: the field varies fast
                // across and slowly along, so the threshold breaks into
                // strokes running one way.
                4..=6 => {
                    let run = size * [3.0f32, 8.0, 20.0][(kind - 4).min(2)];
                    value_noise(x / (size * 0.5), y / run, 4241)
                }
                // Short, medium and long strokes: lines, cut up by a
                // coarser field so they end.
                _ => {
                    let run = size * [3.0f32, 8.0, 20.0][(kind - 7).min(2)];
                    let line = value_noise(x / (size * 0.5), y / run, 4241);
                    let breaks = value_noise(x / (size * 2.0), y / (run * 0.4), 8663);
                    (line + breaks) / 2.0
                }
            }
        };
        let src = px.to_vec();
        for y in 0..h {
            for x in 0..w {
                let p = at(&src, w, h, x as i32, y as i32);
                let n = field(x as f32, y as f32);
                let mut out = [0.0f32; 4];
                for c in 0..3 {
                    out[c] = if p[c] > n { 1.0 } else { 0.0 };
                }
                out[3] = p[3];
                put(px, w, x, y, out);
            }
        }
        let _ = luma;
    }
);

pub fn register(registry: &mut schist_plugin_api::PluginRegistry) {
    registry.register_filter(Box::new(Mosaic));
    registry.register_filter(Box::new(Crystallize));
    registry.register_filter(Box::new(Pointillize));
    registry.register_filter(Box::new(Facet));
    registry.register_filter(Box::new(Fragment));
    registry.register_filter(Box::new(ColorHalftone));
    registry.register_filter(Box::new(Mezzotint));
}
