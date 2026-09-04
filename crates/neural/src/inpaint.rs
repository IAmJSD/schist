//! Filling a hole in a photograph, for Content-Aware Fill.
//!
//! The network takes four planes -- the picture with the hole punched
//! out, and a plane saying where the hole is -- and predicts the whole
//! picture back. The fourth plane is not a convenience: a network handed
//! only the punched-out picture cannot tell a hole from something that
//! happened to be black, and those are the same pixels.
//!
//! What comes back is a *layout*, not a finish. It sees the region at
//! the size it was trained on, so the answer is right about where the
//! horizon goes and vague about what the grass under it looks like; the
//! caller follows it with a patch-synthesis pass that gets the texture
//! from the picture itself. Asking one small network to do both is how
//! you get a blur, which is what this replaced.

use anyhow::{bail, Result};

use crate::{frame, framed::unframe, Model};

/// Predict the whole region back, hole included.
///
/// `rgb` is interleaved RGB in 0..=1 and `hole` is one flag per pixel,
/// true where the picture is missing. The result is interleaved RGB at
/// the same size: only the hole is worth taking, but the rest is what
/// says whether the network understood the region at all.
pub fn inpaint(
    model: &Model,
    rgb: &[f32],
    width: usize,
    height: usize,
    hole: &[bool],
) -> Result<Vec<f32>> {
    if width == 0 || height == 0 || rgb.len() < width * height * 3 {
        bail!("image is {width}x{height} but has {} floats", rgb.len());
    }
    if hole.len() != width * height {
        bail!("hole is {} flags for {} pixels", hole.len(), width * height);
    }
    if model.channels() != 4 {
        bail!(
            "inpainting wants a four-plane model, not {}",
            model.channels()
        );
    }

    // Punched out before framing, so the resample never carries a
    // missing pixel's colour into a kept one.
    let mut holed = rgb[..width * height * 3].to_vec();
    for (i, &gone) in hole.iter().enumerate() {
        if gone {
            holed[i * 3..i * 3 + 3].fill(0.0);
        }
    }
    // The mask goes through the same resample as the picture, as three
    // identical planes, so the two land on exactly the same grid -- the
    // one thing that would quietly break this is a mask half a pixel off
    // the hole it describes.
    let spread: Vec<f32> = hole
        .iter()
        .flat_map(|&g| [g as u8 as f32; 3])
        .collect::<Vec<_>>();
    let (framed_rgb, framing) = frame(model.spec, &holed, width, height);
    let (framed_mask, _) = frame(model.spec, &spread, width, height);

    let (fw, fh) = model.spec.input.dims();
    let mut planes = vec![vec![0.0f32; fw * fh]; 4];
    for i in 0..fw * fh {
        // Back to a hard edge: the network was trained on masks that are
        // one or zero, and a ramp along the boundary is a shape it has
        // never seen.
        let gone = framed_mask[i * 3] > 0.5;
        planes[3][i] = gone as u8 as f32;
        for c in 0..3 {
            planes[c][i] = match gone {
                true => 0.0,
                false => framed_rgb[i * 3 + c],
            };
        }
    }

    let refs: Vec<&[f32]> = planes.iter().map(|p| p.as_slice()).collect();
    let out = model.run_planes(&refs)?;
    unframe(&out[0], model, framing, width, height)
}
