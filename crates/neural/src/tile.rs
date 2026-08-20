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

use crate::Model;

/// Apply `model` across an interleaved RGB f32 image, in place.
///
/// `blend` scales how much of the result replaces the original, so a
/// filter can offer a strength slider without running the network twice.
pub fn run_tiled(model: &Model, rgb: &mut [f32], width: usize, height: usize, blend: f32) {
    let t = model.spec.tile;
    let overlap = model.spec.overlap.min(t / 4);
    if width == 0 || height == 0 || rgb.len() < width * height * 3 || t == 0 {
        return;
    }
    // How much new ground each tile covers.
    let step = t - overlap * 2;
    if step == 0 {
        return;
    }
    let cols = width.div_ceil(step);
    let rows = height.div_ceil(step);

    // Mirror a coordinate back inside the image.
    let reflect = |v: i64, n: usize| -> usize {
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
    };

    let source = rgb.to_vec();
    // Each tile's result, with the position it belongs at. Collected
    // first so the tiles can run in parallel without writing over each
    // other's overlap regions.
    let done: Vec<(usize, usize, Vec<f32>)> = (0..rows * cols)
        .into_par_iter()
        .filter_map(|i| {
            let (cy, cx) = (i / cols, i % cols);
            let ox = (cx * step) as i64 - overlap as i64;
            let oy = (cy * step) as i64 - overlap as i64;
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
        .collect();

    let blend = blend.clamp(0.0, 1.0);
    for (cx, cy, out) in done {
        let ox = (cx * step) as i64 - overlap as i64;
        let oy = (cy * step) as i64 - overlap as i64;
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

#[cfg(test)]
mod tests {
    /// Every tile position a run would visit must, between them, cover
    /// every pixel -- otherwise the filter would leave untouched bands.
    #[test]
    fn tiles_cover_the_whole_image() {
        for (w, h, t, overlap) in [
            (100usize, 60usize, 32usize, 4usize),
            (33, 129, 64, 8),
            (7, 7, 128, 8),
        ] {
            let step = t - overlap * 2;
            let cols = w.div_ceil(step);
            let rows = h.div_ceil(step);
            let mut covered = vec![false; w * h];
            for cy in 0..rows {
                for cx in 0..cols {
                    let ox = (cx * step) as i64 - overlap as i64;
                    let oy = (cy * step) as i64 - overlap as i64;
                    for y in overlap..t - overlap {
                        let dy = oy + y as i64;
                        if dy < 0 || dy as usize >= h {
                            continue;
                        }
                        for x in overlap..t - overlap {
                            let dx = ox + x as i64;
                            if dx < 0 || dx as usize >= w {
                                continue;
                            }
                            covered[dy as usize * w + dx as usize] = true;
                        }
                    }
                }
            }
            let missed = covered.iter().filter(|c| !**c).count();
            assert_eq!(missed, 0, "{w}x{h} tile {t}: {missed} pixels uncovered");
        }
    }
}
