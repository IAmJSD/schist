//! Running an image-to-image model that wants the whole picture at once.
//!
//! [`crate::run_tiled`] is for models that work on pixels: it cuts the
//! image into tiles at full resolution because the answer is local. This
//! is for the other kind, where the answer is about *the whole subject* --
//! filling in a drawing of a face means knowing it is a face, and filling
//! in a hole means knowing what is on the other side of it -- so the
//! picture goes in as one frame at whatever size the model was trained
//! at, and the result is resampled back out.
//!
//! That resample is the cost, and it is why only models that have to work
//! this way do: the result cannot carry more detail than the frame held.

use anyhow::{bail, Context as _, Result};
use tract_onnx::prelude::*;

use crate::{frame, Framing, Model};

/// Run a whole-image model and return its result at the image's size.
///
/// `rgb` is interleaved RGB in 0..=1, and so is what comes back.
pub fn run_framed(model: &Model, rgb: &[f32], width: usize, height: usize) -> Result<Vec<f32>> {
    if width == 0 || height == 0 || rgb.len() < width * height * 3 {
        bail!("image is {width}x{height} but has {} floats", rgb.len());
    }
    let (input, framing) = frame(model.spec, rgb, width, height);
    let out = model.run(&input)?;
    unframe(&out[0], model, framing, width, height)
}

/// Resample a model's `[1, 3, H, W]` output back to the image it came
/// from, given how the image was fitted into the frame.
pub(crate) fn unframe(
    out: &TValue,
    model: &Model,
    framing: Framing,
    width: usize,
    height: usize,
) -> Result<Vec<f32>> {
    let view = out.to_plain_array_view::<f32>()?;
    let [1, 3, fh, fw] = view.shape()[..] else {
        bail!("expected a 1x3xHxW image out, got {:?}", view.shape());
    };
    let flat = view.as_slice().context("non-contiguous output")?;
    let framing = framing.against(model.spec.input.dims(), (fw, fh));
    let (sx, sy) = framing.scale;
    let (ox, oy) = framing.offset;

    let mut result = vec![0.0f32; width * height * 3];
    for y in 0..height {
        let fy = ((y as f32 + 0.5) * sy + oy - 0.5).clamp(0.0, fh as f32 - 1.0);
        let (y0, ty) = (fy.floor() as usize, fy - fy.floor());
        let y1 = (y0 + 1).min(fh - 1);
        for x in 0..width {
            let fx = ((x as f32 + 0.5) * sx + ox - 0.5).clamp(0.0, fw as f32 - 1.0);
            let (x0, tx) = (fx.floor() as usize, fx - fx.floor());
            let x1 = (x0 + 1).min(fw - 1);
            for c in 0..3 {
                let at = |x: usize, y: usize| flat[(c * fh + y) * fw + x];
                let top = at(x0, y0) * (1.0 - tx) + at(x1, y0) * tx;
                let bot = at(x0, y1) * (1.0 - tx) + at(x1, y1) * tx;
                result[(y * width + x) * 3 + c] = (top * (1.0 - ty) + bot * ty).clamp(0.0, 1.0);
            }
        }
    }
    Ok(result)
}
