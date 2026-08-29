//! Monocular depth estimation, for Depth Blur.
//!
//! MiDaS predicts *relative inverse* depth: a number per pixel that is
//! larger for nearer things, on a scale that means nothing between one
//! image and the next. That is all a lens-blur effect needs -- what is in
//! front of what -- so the map is normalised to 0..=1 over its own range
//! and handed back at the size of the image.
//!
//! The network sees a 256x256 frame however big the image is, which is
//! not a compromise so much as the point: depth is about what the objects
//! in a scene *are*, and that survives a downscale. The map is smooth by
//! nature, so putting it back at full size is an ordinary resample.

use anyhow::{bail, Context as _, Result};

use crate::{frame, Model};

/// Relative depth for an image, one value per pixel, 1.0 nearest.
///
/// `rgb` is interleaved RGB in 0..=1.
pub fn depth_map(model: &Model, rgb: &[f32], width: usize, height: usize) -> Result<Vec<f32>> {
    if width == 0 || height == 0 || rgb.len() < width * height * 3 {
        bail!("image is {width}x{height} but has {} floats", rgb.len());
    }
    let (input, framing) = frame(model.spec, rgb, width, height);
    let out = model.run(&input)?;
    let view = out[0].to_plain_array_view::<f32>()?;
    // The graph emits [1, h, w]; be tolerant of the [1, 1, h, w] a
    // re-export could produce instead.
    let shape: Vec<usize> = view.shape().iter().copied().filter(|d| *d != 1).collect();
    let [fh, fw] = shape[..] else {
        bail!("unexpected depth output shape {:?}", view.shape());
    };
    let flat = view.as_slice().context("non-contiguous output")?;
    // A no-op for a network that predicts at its input's resolution, as
    // this one does, but the map is what the coordinates below mean.
    let framing = framing.against(model.spec.input.dims(), (fw, fh));

    // The scale is arbitrary, so it has to come from the picture. Ignore
    // whatever landed in the letterbox: it is grey the network invented a
    // distance for, and it is often the extreme at both ends.
    let inside = |fx: usize, fy: usize| -> bool {
        let (sx, sy) = framing.scale;
        let (ox, oy) = framing.offset;
        let x = (fx as f32 + 0.5 - ox) / sx;
        let y = (fy as f32 + 0.5 - oy) / sy;
        x >= 0.0 && y >= 0.0 && x < width as f32 && y < height as f32
    };
    let (mut lo, mut hi) = (f32::INFINITY, f32::NEG_INFINITY);
    for fy in 0..fh {
        for fx in 0..fw {
            if !inside(fx, fy) {
                continue;
            }
            let v = flat[fy * fw + fx];
            if v.is_finite() {
                lo = lo.min(v);
                hi = hi.max(v);
            }
        }
    }
    if !lo.is_finite() || !hi.is_finite() {
        bail!("depth output has no finite values");
    }
    let span = (hi - lo).max(1e-6);

    let (sx, sy) = framing.scale;
    let (ox, oy) = framing.offset;
    let mut map = vec![0.0f32; width * height];
    for y in 0..height {
        let fy = ((y as f32 + 0.5) * sy + oy - 0.5).clamp(0.0, fh as f32 - 1.0);
        let (y0, ty) = (fy.floor(), fy - fy.floor());
        let (y0, y1) = (y0 as usize, (y0 as usize + 1).min(fh - 1));
        for x in 0..width {
            let fx = ((x as f32 + 0.5) * sx + ox - 0.5).clamp(0.0, fw as f32 - 1.0);
            let (x0, tx) = (fx.floor(), fx - fx.floor());
            let (x0, x1) = (x0 as usize, (x0 as usize + 1).min(fw - 1));
            let at = |x: usize, y: usize| ((flat[y * fw + x] - lo) / span).clamp(0.0, 1.0);
            let top = at(x0, y0) * (1.0 - tx) + at(x1, y0) * tx;
            let bot = at(x0, y1) * (1.0 - tx) + at(x1, y1) * tx;
            map[y * width + x] = top * (1.0 - ty) + bot * ty;
        }
    }
    Ok(map)
}
