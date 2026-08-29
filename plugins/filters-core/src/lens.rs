//! Filter ▸ Lens Correction and Adaptive Wide Angle.
//!
//! Both undo what a lens did to a picture, and both are the same shape of
//! operation as [`crate::distort`]: a coordinate remap. What makes them
//! their own module is that the remap is a *model of an optical system*
//! rather than an effect -- barrel distortion, the difference in
//! magnification between red and blue, the falloff at the corners, and
//! the projection a very wide lens uses.
//!
//! Photoshop reads the lens out of the file's EXIF and looks it up in a
//! profile database. There is no database here, so the sliders are the
//! profile: point them at a straight line that came out bent and stop
//! when it is straight.

use crate::util::{luma, premultiply, put, sample, unpremultiply};
use crate::{choice, param, simple_filter};
use schist_plugin_api::{FilterParam, FilterPlugin, FilterValues};

/// Sample one channel through its own coordinate map.
///
/// Chromatic aberration is a *difference in magnification* between the
/// wavelengths, so undoing it means scaling the red and blue channels
/// about the centre by slightly different amounts -- which is why this
/// cannot go through the usual whole-pixel warp.
fn remap_channels(
    px: &mut [f32],
    w: usize,
    h: usize,
    scales: [f32; 3],
    map: impl Fn(f32, f32) -> (f32, f32),
) {
    premultiply(px);
    let src = px.to_vec();
    let (cx, cy) = (w as f32 / 2.0, h as f32 / 2.0);
    for y in 0..h {
        for x in 0..w {
            let (mx, my) = map(x as f32 + 0.5, y as f32 + 0.5);
            let mut out = [0.0f32; 4];
            for (c, scale) in scales.iter().enumerate() {
                let sx = cx + (mx - cx) * scale;
                let sy = cy + (my - cy) * scale;
                let p = sample(&src, w, h, sx - 0.5, sy - 0.5);
                out[c] = p[c];
                // Alpha follows green, which is the channel that is not
                // being moved for the fringes.
                if c == 1 {
                    out[3] = p[3];
                }
            }
            put(px, w, x, y, out);
        }
    }
    unpremultiply(px);
}

simple_filter!(
    LensCorrection,
    "filter.lens_correction",
    "Lens Correction",
    "Other",
    [
        param("distortion", "Remove Distortion", -100.0, 100.0, 0.0, ""),
        param("red", "Fix Red/Cyan Fringe", -50.0, 50.0, 0.0, ""),
        param("blue", "Fix Blue/Yellow Fringe", -50.0, 50.0, 0.0, ""),
        param("vignette", "Vignette Amount", -100.0, 100.0, 0.0, ""),
        param("midpoint", "Vignette Midpoint", 0.0, 100.0, 50.0, ""),
        param("vertical", "Vertical Perspective", -100.0, 100.0, 0.0, ""),
        param(
            "horizontal",
            "Horizontal Perspective",
            -100.0,
            100.0,
            0.0,
            ""
        ),
        param("angle", "Angle", -180.0, 180.0, 0.0, "\u{b0}"),
        param("scale", "Scale", 50.0, 200.0, 100.0, "%")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // Everything a lens profile carries, as sliders. Positive
        // Distortion pulls the corners in, which straightens the barrel
        // a wide lens leaves; negative pushes them out for pincushion.
        let distortion = v.get("distortion") / 100.0;
        let red = 1.0 + v.get("red") / 50.0 * 0.006;
        let blue = 1.0 + v.get("blue") / 50.0 * 0.006;
        let vignette = v.get("vignette") / 100.0;
        let midpoint = (v.get("midpoint") / 100.0).clamp(0.05, 1.0);
        let vertical = v.get("vertical") / 100.0;
        let horizontal = v.get("horizontal") / 100.0;
        let angle = v.get("angle").to_radians();
        let scale = (v.get("scale") / 100.0).max(0.05);
        let (cx, cy) = (w as f32 / 2.0, h as f32 / 2.0);
        let norm = cx.hypot(cy).max(1.0);

        // With every correction at zero the map is the identity, and
        // resampling an image through the identity is not free: it costs
        // a premultiply round trip, which flattens the colour under
        // fully transparent pixels. Leave it alone instead.
        let corrected = distortion != 0.0
            || red != 1.0
            || blue != 1.0
            || vertical != 0.0
            || horizontal != 0.0
            || angle != 0.0
            || (scale - 1.0).abs() > 1e-6;
        if corrected {
            remap_channels(px, w, h, [red, 1.0, blue], |x, y| {
                // Work in a square-ish space centred on the frame, so the
                // corrections do not depend on the aspect ratio.
                let (mut u, mut v) = ((x - cx) / norm, (y - cy) / norm);
                // Rotation and scale first: they are what the corrections
                // below are measured against.
                let (s, c) = angle.sin_cos();
                let (ru, rv) = (u * c - v * s, u * s + v * c);
                u = ru / scale;
                v = rv / scale;
                // Perspective: divide by a plane tilted away from the
                // viewer, which is what a keystone correction is.
                let denom = (1.0 + vertical * v + horizontal * u).max(0.05);
                u /= denom;
                v /= denom;
                // Radial distortion, the usual quadratic term.
                let r2 = u * u + v * v;
                let k = 1.0 - distortion * r2 * 0.5;
                (cx + u * k * norm, cy + v * k * norm)
            });
        }

        if vignette != 0.0 {
            for (i, p) in px.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                let (x, y) = ((i % w) as f32 + 0.5, (i / w) as f32 + 0.5);
                let r = ((x - cx) / norm).hypot((y - cy) / norm);
                // Nothing happens inside the midpoint; beyond it the
                // correction ramps up with the square of the distance,
                // which is roughly how the falloff arrives.
                let t = ((r - midpoint) / (1.0 - midpoint).max(1e-3)).clamp(0.0, 1.0);
                let gain = 1.0 + vignette * t * t;
                for c in p.iter_mut().take(3) {
                    *c = (*c * gain).clamp(0.0, 1.0);
                }
            }
        }
    }
);

/// The projections Adaptive Wide Angle knows how to undo.
const PROJECTIONS: &[&str] = &["Fisheye", "Perspective", "Full Spherical"];

simple_filter!(
    AdaptiveWideAngle,
    "filter.adaptive_wide_angle",
    "Adaptive Wide Angle",
    "Other",
    [
        choice("projection", "Correction", PROJECTIONS, 0),
        param("focal", "Focal Length", 4.0, 60.0, 14.0, " mm"),
        param("crop", "Crop Factor", 0.5, 3.0, 1.0, "\u{d7}"),
        param("scale", "Scale", 50.0, 200.0, 100.0, "%")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // A very wide lens does not project the world the way a normal
        // one does, so straightening it is a change of projection rather
        // than a polynomial: map each pixel back to the angle it came
        // from, then re-project that angle rectilinearly.
        //
        // Photoshop gets the angle from the lens profile. Here it comes
        // from the focal length and the crop factor, which is the same
        // arithmetic every field-of-view calculator does.
        let projection = (v.get("projection").round().max(0.0) as usize).min(2);
        let focal = v.get("focal").max(1.0);
        let crop = v.get("crop").max(0.1);
        let scale = (v.get("scale") / 100.0).max(0.05);
        // Half the diagonal field of view, from a 43.3 mm full-frame
        // diagonal divided by the crop factor.
        let half_fov = ((43.27 / crop) / (2.0 * focal)).atan();
        let (cx, cy) = (w as f32 / 2.0, h as f32 / 2.0);
        let norm = cx.hypot(cy).max(1.0);
        // How far a ray at the edge of the frame lands under each
        // projection, so the corrected picture still fills the frame.
        let edge = match projection {
            0 => half_fov,
            1 => half_fov.tan(),
            _ => 2.0 * (half_fov / 2.0).sin(),
        };
        crate::util::warp(px, w, h, |x, y| {
            let (u, vv) = ((x - cx) / norm / scale, (y - cy) / norm / scale);
            let r = u.hypot(vv);
            if r < 1e-6 {
                return (x, y);
            }
            // The angle this output pixel is asking about, rectilinear.
            let theta = (r * half_fov.tan()).atan();
            // Where the lens actually put that angle.
            let rs = match projection {
                // Equidistant: radius proportional to angle.
                0 => theta,
                // Rectilinear already: only the scale changes.
                1 => theta.tan(),
                // Equisolid, which is what most fisheyes really are.
                _ => 2.0 * (theta / 2.0).sin(),
            } / edge;
            (cx + u / r * rs * norm, cy + vv / r * rs * norm)
        });
    }
);

/// A quick brightness reading, for the frame and flame filters that want
/// to sit against the picture rather than on top of it.
pub fn mean_luma(px: &[f32]) -> f32 {
    let n = (px.len() / 4).max(1) as f32;
    px.as_chunks::<4>().0.iter().map(|p| luma(p) / n).sum()
}

pub fn register(registry: &mut schist_plugin_api::PluginRegistry) {
    registry.register_filter(Box::new(LensCorrection));
    registry.register_filter(Box::new(AdaptiveWideAngle));
}
