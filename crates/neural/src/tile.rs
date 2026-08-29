//! Running a fixed-size model over an arbitrary image.
//!
//! Models here take a fixed square tile, so an image is cut into
//! overlapping ones. The overlap is the point: a convolutional network's
//! output near a tile's edge is wrong, because the context it needed was
//! outside the tile. Trimming the overlap off before writing the result
//! back is what stops a grid of seams appearing.
//!
//! Edges of the *image* are extended by mirroring rather than padded with
//! black, for the same reason -- a network fed a black border will draw
//! one.

use rayon::prelude::*;

use crate::{Input, Model};

/// The tile grid a run over a `width` x `height` image uses.
struct Grid {
    t: usize,
    overlap: usize,
    /// How much new ground each tile covers.
    step: usize,
    cols: usize,
    rows: usize,
}

fn grid(model: &Model, width: usize, height: usize) -> Option<Grid> {
    let Input::Tiles {
        size: t, overlap, ..
    } = model.spec.input
    else {
        log::warn!("{} is not a tiled model", model.spec.id);
        return None;
    };
    let overlap = overlap.min(t / 4);
    if width == 0 || height == 0 || t == 0 {
        return None;
    }
    let step = t - overlap * 2;
    if step == 0 {
        return None;
    }
    Some(Grid {
        t,
        overlap,
        step,
        cols: width.div_ceil(step),
        rows: height.div_ceil(step),
    })
}

/// Mirror a coordinate back inside the image.
fn reflect(v: i64, n: usize) -> usize {
    let n = n as i64;
    if n == 1 {
        return 0;
    }
    let period = 2 * (n - 1);
    let mut m = v.rem_euclid(period);
    if m >= n {
        m = period - m;
    }
    m as usize
}

/// Cut every tile, run each through the model, and hand back the results
/// with the grid position each belongs at. Collected rather than written
/// as they finish so the tiles can run in parallel without writing over
/// each other's overlap regions.
fn infer(
    model: &Model,
    source: &[f32],
    width: usize,
    height: usize,
    g: &Grid,
) -> Vec<(usize, usize, Vec<f32>)> {
    let t = g.t;
    (0..g.rows * g.cols)
        .into_par_iter()
        .filter_map(|i| {
            let (cy, cx) = (i / g.cols, i % g.cols);
            let ox = (cx * g.step) as i64 - g.overlap as i64;
            let oy = (cy * g.step) as i64 - g.overlap as i64;
            let mut patch = vec![0.0f32; t * t * 3];
            for y in 0..t {
                let sy = reflect(oy + y as i64, height);
                for x in 0..t {
                    let sx = reflect(ox + x as i64, width);
                    let at = (sy * width + sx) * 3;
                    let to = (y * t + x) * 3;
                    patch[to..to + 3].copy_from_slice(&source[at..at + 3]);
                }
            }
            match model.run_tile(&patch) {
                Ok(out) => Some((cx, cy, out)),
                Err(e) => {
                    log::warn!("neural tile failed: {e:#}");
                    None
                }
            }
        })
        .collect()
}

/// Apply `model` across an interleaved RGB f32 image, in place.
///
/// `blend` scales how much of the result replaces the original, so a
/// filter can offer a strength slider without running the network twice.
pub fn run_tiled(model: &Model, rgb: &mut [f32], width: usize, height: usize, blend: f32) {
    if model.spec.input.scale() != 1 {
        log::warn!("{} resizes; run_scaled is its driver", model.spec.id);
        return;
    }
    if rgb.len() < width * height * 3 {
        return;
    }
    let Some(g) = grid(model, width, height) else {
        return;
    };
    let source = rgb.to_vec();
    let done = infer(model, &source, width, height, &g);

    let (t, overlap) = (g.t, g.overlap);
    let blend = blend.clamp(0.0, 1.0);
    for (cx, cy, out) in done {
        let ox = (cx * g.step) as i64 - overlap as i64;
        let oy = (cy * g.step) as i64 - overlap as i64;
        // Write back only the middle: the overlap was context, not output.
        for y in overlap..t - overlap {
            let dy = oy + y as i64;
            if dy < 0 || dy as usize >= height {
                continue;
            }
            for x in overlap..t - overlap {
                let dx = ox + x as i64;
                if dx < 0 || dx as usize >= width {
                    continue;
                }
                let to = (dy as usize * width + dx as usize) * 3;
                let from = (y * t + x) * 3;
                for c in 0..3 {
                    let orig = source[to + c];
                    rgb[to + c] = orig + (out[from + c] - orig) * blend;
                }
            }
        }
    }
}

/// Apply an upscaling `model` across an interleaved RGB f32 image,
/// returning one `scale` times as large on each side.
///
/// Same tiling as [`run_tiled`], with every write-back coordinate
/// multiplied by the model's scale. `None` when the model is not a tiled
/// one or the image is degenerate.
pub fn run_scaled(model: &Model, rgb: &[f32], width: usize, height: usize) -> Option<Vec<f32>> {
    let scale = model.spec.input.scale();
    if rgb.len() < width * height * 3 {
        return None;
    }
    let g = grid(model, width, height)?;

    // Start from a nearest-neighbour enlargement, so a tile that fails
    // (and is logged) degrades to soft pixels rather than a black square.
    let (ow, oh) = (width * scale, height * scale);
    let mut out = vec![0.0f32; ow * oh * 3];
    for y in 0..oh {
        let sy = y / scale;
        for x in 0..ow {
            let at = (sy * width + x / scale) * 3;
            out[(y * ow + x) * 3..][..3].copy_from_slice(&rgb[at..at + 3]);
        }
    }

    let done = infer(model, rgb, width, height, &g);
    let (t, overlap) = (g.t, g.overlap);
    for (cx, cy, tile) in done {
        let ox = ((cx * g.step) as i64 - overlap as i64) * scale as i64;
        let oy = ((cy * g.step) as i64 - overlap as i64) * scale as i64;
        for y in overlap * scale..(t - overlap) * scale {
            let dy = oy + y as i64;
            if dy < 0 || dy as usize >= oh {
                continue;
            }
            for x in overlap * scale..(t - overlap) * scale {
                let dx = ox + x as i64;
                if dx < 0 || dx as usize >= ow {
                    continue;
                }
                let to = (dy as usize * ow + dx as usize) * 3;
                let from = (y * t * scale + x) * 3;
                out[to..to + 3].copy_from_slice(&tile[from..from + 3]);
            }
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    /// Every tile position a run would visit must, between them, cover
    /// every pixel -- otherwise the filter would leave untouched bands.
    /// With a scale the same walk covers the enlarged image, because every
    /// destination coordinate is a source one times the scale.
    #[test]
    fn tiles_cover_the_whole_image() {
        for (w, h, t, overlap, scale) in [
            (100usize, 60usize, 32usize, 4usize, 1usize),
            (33, 129, 64, 8, 1),
            (7, 7, 128, 8, 1),
            (100, 60, 32, 4, 2),
            (33, 129, 64, 8, 2),
            (7, 7, 128, 8, 2),
        ] {
            let step = t - overlap * 2;
            let cols = w.div_ceil(step);
            let rows = h.div_ceil(step);
            let (ow, oh) = (w * scale, h * scale);
            let mut covered = vec![false; ow * oh];
            for cy in 0..rows {
                for cx in 0..cols {
                    let ox = ((cx * step) as i64 - overlap as i64) * scale as i64;
                    let oy = ((cy * step) as i64 - overlap as i64) * scale as i64;
                    for y in overlap * scale..(t - overlap) * scale {
                        let dy = oy + y as i64;
                        if dy < 0 || dy as usize >= oh {
                            continue;
                        }
                        for x in overlap * scale..(t - overlap) * scale {
                            let dx = ox + x as i64;
                            if dx < 0 || dx as usize >= ow {
                                continue;
                            }
                            covered[dy as usize * ow + dx as usize] = true;
                        }
                    }
                }
            }
            let missed = covered.iter().filter(|c| !**c).count();
            assert_eq!(missed, 0, "{w}x{h} tile {t} x{scale}: {missed} uncovered");
        }
    }
}
