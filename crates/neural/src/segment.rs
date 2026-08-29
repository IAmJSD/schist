//! Salient-object segmentation, for the Object Selection tool.
//!
//! U^2-Net answers one question: which pixels belong to the thing the
//! picture is *of*. That is not the same question Photoshop's Object
//! Selection asks -- it wants the object under the box you drew, and
//! there may be several in a picture -- but it is the same question once
//! the box is the picture, which is why the tool runs this on a crop
//! around the box rather than on the layer.
//!
//! The network is a stack of nested U-nets and emits seven maps, one per
//! depth, so it can be supervised at every scale. Only the first is the
//! answer; the rest are scaffolding from training, and they are ignored.
//!
//! One detail of its preprocessing matters and is easy to miss: the
//! picture is divided by its own maximum channel value before the
//! ImageNet normalisation, which is what it was trained with.
//!
//! What comes back is left as the probability the network emitted, and
//! deliberately *not* stretched over its own range the way the reference
//! implementation stretches it. Stretching is right when the map is going
//! to be an alpha channel and something is definitely there. It is wrong
//! here, because a selection tool has to be able to come back empty: hand
//! this a close-up of a brick wall and the honest answer is a map of
//! nothing, and a stretch turns that into a map of noise.

use anyhow::{bail, Context as _, Result};

use crate::{frame, Model};

/// How much of the object is in each pixel, 1.0 for certainly.
///
/// `rgb` is interleaved RGB in 0..=1; the map comes back at the image's
/// size.
pub fn segment(model: &Model, rgb: &[f32], width: usize, height: usize) -> Result<Vec<f32>> {
    if width == 0 || height == 0 || rgb.len() < width * height * 3 {
        bail!("image is {width}x{height} but has {} floats", rgb.len());
    }
    // Divided by its own brightest channel, which is what the reference
    // preprocessing does before normalising. On an ordinary photograph
    // that is a no-op; on a dark one it is the difference between a
    // subject and a shrug.
    let peak = rgb[..width * height * 3]
        .iter()
        .copied()
        .fold(0.0f32, f32::max);
    let scaled: Vec<f32> = match peak > 1e-4 && (peak - 1.0).abs() > 1e-3 {
        true => rgb[..width * height * 3].iter().map(|v| v / peak).collect(),
        false => rgb[..width * height * 3].to_vec(),
    };

    let (input, framing) = frame(model.spec, &scaled, width, height);
    let out = model.run(&input)?;
    let view = out[0].to_plain_array_view::<f32>()?;
    let shape: Vec<usize> = view.shape().iter().copied().filter(|d| *d != 1).collect();
    let [fh, fw] = shape[..] else {
        bail!("unexpected segmentation output shape {:?}", view.shape());
    };
    let flat = view.as_slice().context("non-contiguous output")?;
    let framing = framing.against(model.spec.input.dims(), (fw, fh));

    if !flat.iter().any(|v| v.is_finite()) {
        bail!("segmentation output has no finite values");
    }

    let (sx, sy) = framing.scale;
    let (ox, oy) = framing.offset;
    let mut map = vec![0.0f32; width * height];
    for y in 0..height {
        let fy = ((y as f32 + 0.5) * sy + oy - 0.5).clamp(0.0, fh as f32 - 1.0);
        let (y0, ty) = (fy.floor() as usize, fy - fy.floor());
        let y1 = (y0 + 1).min(fh - 1);
        for x in 0..width {
            let fx = ((x as f32 + 0.5) * sx + ox - 0.5).clamp(0.0, fw as f32 - 1.0);
            let (x0, tx) = (fx.floor() as usize, fx - fx.floor());
            let x1 = (x0 + 1).min(fw - 1);
            let at = |x: usize, y: usize| flat[y * fw + x].clamp(0.0, 1.0);
            let top = at(x0, y0) * (1.0 - tx) + at(x1, y0) * tx;
            let bot = at(x0, y1) * (1.0 - tx) + at(x1, y1) * tx;
            map[y * width + x] = top * (1.0 - ty) + bot * ty;
        }
    }
    Ok(map)
}
