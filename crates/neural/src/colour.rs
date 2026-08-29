//! Colour prediction for a greyscale photograph, for Colorize.
//!
//! The network is given luminance and predicts chroma -- the two
//! differences `R - Y` and `B - Y` -- rather than a whole image, for the
//! reason every colourisation paper gives: luminance is already correct,
//! and a network asked to reproduce it spends its capacity copying its
//! own input. Predicting only what is missing also means the result
//! cannot come back softer than the photograph went in.
//!
//! Chroma is low-frequency, so it is predicted small -- one 256x256 frame
//! for the whole picture, whatever size the picture is -- and resampled
//! up. That is not a shortcut: a colouriser has to agree with itself
//! about what a thing *is* across the whole of it, and a model run over
//! tiles cannot, because each tile is a separate opinion.
//!
//! Resampling it up is where the care goes. A plain bilinear enlargement
//! of a map that coarse smears every colour across every boundary in the
//! picture, which is the "child colouring outside the lines" look that
//! gives a colourised photograph away. So the enlargement is guided by
//! the luminance the model saw: a sample only contributes to a pixel
//! that was about as bright as it was, which puts the edges of the
//! colour back on the edges of the subject.

use anyhow::{bail, Context as _, Result};
use rayon::prelude::*;

use crate::{frame, Model};

/// Rec.601 luma, matching what the filters use.
fn luma(r: f32, g: f32, b: f32) -> f32 {
    0.299 * r + 0.587 * g + 0.114 * b
}

/// Predicted chroma for an image: two floats a pixel, `R - Y` then
/// `B - Y`, at the size of the image.
///
/// `rgb` is interleaved RGB in 0..=1. Colour already in it is ignored --
/// the model sees the luminance, which is what makes running this on a
/// colour photograph a *re*-colourisation rather than a tint.
pub fn chroma(model: &Model, rgb: &[f32], width: usize, height: usize) -> Result<Vec<f32>> {
    if width == 0 || height == 0 || rgb.len() < width * height * 3 {
        bail!("image is {width}x{height} but has {} floats", rgb.len());
    }
    let (mut input, framing) = frame(model.spec, rgb, width, height);
    // Grey after framing rather than before: luma is a weighted sum, so
    // it commutes with the resample, and this way is one pass over the
    // small buffer instead of one over the large.
    for p in input.as_chunks_mut::<3>().0.iter_mut() {
        let y = luma(p[0], p[1], p[2]);
        *p = [y; 3];
    }

    let out = model.run(&input)?;
    let view = out[0].to_plain_array_view::<f32>()?;
    let [1, 2, fh, fw] = view.shape()[..] else {
        bail!("unexpected colour output shape {:?}", view.shape());
    };
    let flat = view.as_slice().context("non-contiguous output")?;

    // Chroma comes out smaller than the frame, so the framing has to be
    // read against the map rather than against the frame.
    let framing = framing.against(model.spec.input.dims(), (fw, fh));
    // What each chroma sample was looking at, which is what makes the
    // upsample below edge-aware.
    let guide = coarse_luma(&input, model.spec.input.dims(), (fw, fh));

    let (sx, sy) = framing.scale;
    let (ox, oy) = framing.offset;
    let mut map = vec![0.0f32; width * height * 2];
    map.par_chunks_mut(width * 2)
        .enumerate()
        .for_each(|(y, row)| {
            let fy = ((y as f32 + 0.5) * sy + oy - 0.5).clamp(0.0, fh as f32 - 1.0);
            for x in 0..width {
                let fx = ((x as f32 + 0.5) * sx + ox - 0.5).clamp(0.0, fw as f32 - 1.0);
                let here = {
                    let p = &rgb[(y * width + x) * 3..];
                    luma(p[0], p[1], p[2])
                };
                let (mut acc, mut total) = ([0.0f32; 2], 0.0f32);
                for ty in taps(fy, fh) {
                    for tx in taps(fx, fw) {
                        // Spatial: a tent over one sample either side.
                        let d = (1.0 - (fx - tx as f32).abs()).max(0.0)
                            * (1.0 - (fy - ty as f32).abs()).max(0.0);
                        if d <= 0.0 {
                            continue;
                        }
                        // Range: how much this sample was looking at the
                        // same thing as this pixel. Without it a chroma
                        // map sixteen times coarser than the image bleeds
                        // the sky's blue over the roofline.
                        let step = (here - guide[ty * fw + tx]) / RANGE;
                        let w = d / (1.0 + step * step * step * step);
                        acc[0] += w * flat[ty * fw + tx];
                        acc[1] += w * flat[(fh + ty) * fw + tx];
                        total += w;
                    }
                }
                // A pixel whose luminance matches nothing nearby -- a
                // highlight in shadow -- gets the nearest sample rather
                // than nothing.
                if total <= 1e-6 {
                    let (nx, ny) = (fx.round() as usize, fy.round() as usize);
                    let (nx, ny) = (nx.min(fw - 1), ny.min(fh - 1));
                    acc = [flat[ny * fw + nx], flat[(fh + ny) * fw + nx]];
                    total = 1.0;
                }
                row[x * 2] = acc[0] / total;
                row[x * 2 + 1] = acc[1] / total;
            }
        });
    Ok(map)
}

/// How far apart two luminances have to be before they are taken to be
/// different things. A tenth of the range is about the step across the
/// edge of a subject against its background.
const RANGE: f32 = 0.1;

/// The chroma samples a position falls between, clamped to the map.
fn taps(at: f32, n: usize) -> std::ops::RangeInclusive<usize> {
    let lo = (at.floor() as isize).clamp(0, n as isize - 1) as usize;
    let hi = (lo + 1).min(n - 1);
    lo..=hi
}

/// The luminance of each chroma sample's own patch of the frame.
///
/// The network saw the frame; each of its outputs covers a block of it,
/// and the average of that block is what that output is an opinion
/// *about*. Comparing a full-resolution pixel against it is what tells
/// the upsample whether the two are the same surface.
fn coarse_luma(frame: &[f32], (fw, fh): (usize, usize), (mw, mh): (usize, usize)) -> Vec<f32> {
    let mut out = vec![0.0f32; mw * mh];
    for my in 0..mh {
        let y0 = my * fh / mh;
        let y1 = (((my + 1) * fh).div_ceil(mh)).max(y0 + 1).min(fh);
        for mx in 0..mw {
            let x0 = mx * fw / mw;
            let x1 = (((mx + 1) * fw).div_ceil(mw)).max(x0 + 1).min(fw);
            let mut sum = 0.0;
            for y in y0..y1 {
                for x in x0..x1 {
                    // The frame is already grey here, so any channel does.
                    sum += frame[(y * fw + x) * 3];
                }
            }
            out[my * mw + mx] = sum / ((y1 - y0) * (x1 - x0)) as f32;
        }
    }
    out
}

/// Rebuild a pixel from its own luminance and a predicted chroma.
///
/// Solving for green rather than predicting it is what keeps the
/// luminance exactly as the photograph had it: the red and blue offsets
/// are given, and green is whatever makes the three of them weigh what
/// they weighed before.
pub fn recolour(rgb: &[f32; 3], chroma: [f32; 2]) -> [f32; 3] {
    let y = luma(rgb[0], rgb[1], rgb[2]);
    let (r, b) = (y + chroma[0], y + chroma[1]);
    let g = (y - 0.299 * r - 0.114 * b) / 0.587;
    [r.clamp(0.0, 1.0), g.clamp(0.0, 1.0), b.clamp(0.0, 1.0)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recolour_keeps_the_luminance_it_was_given() {
        // Mid greys with chroma that fits inside the cube: nothing here
        // clips, and clipping is the only thing that can move the
        // luminance.
        for grey in [0.3f32, 0.5, 0.62, 0.75] {
            let px = [grey; 3];
            for c in [[0.0, 0.0], [0.12, -0.08], [-0.2, 0.15]] {
                let out = recolour(&px, c);
                let before = luma(px[0], px[1], px[2]);
                let after = luma(out[0], out[1], out[2]);
                assert!(
                    (before - after).abs() < 1e-5,
                    "{c:?} on {grey}: {before} -> {after}"
                );
                assert!((out[0] - out[2] - (c[0] - c[1])).abs() < 1e-5);
            }
        }
    }

    #[test]
    fn a_colour_that_does_not_fit_is_clipped_rather_than_wrapped() {
        // Ask for more red than a near-black pixel can hold. The answer
        // is the closest colour that exists, which is brighter than what
        // was asked for -- but it is still a colour, which is what
        // matters: an unclamped channel would come out as a black hole in
        // the middle of a face.
        let out = recolour(&[0.04; 3], [0.5, -0.4]);
        assert!(out.iter().all(|v| (0.0..=1.0).contains(v)), "{out:?}");
        assert!(out[0] > out[2], "the red it could fit should still lead");
    }
}
