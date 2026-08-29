//! Filter ▸ 3D: Generate Bump Map and Generate Normal Map.
//!
//! The two filters Adobe left behind when they took 3D out of Photoshop,
//! because texture artists kept using them: they turn a photograph of a
//! surface into the maps a renderer wants. A bump map is *how high* each
//! point is; a normal map is *which way it faces*, which is the same
//! information differentiated and packed into red, green and blue.
//!
//! Both start from the same place -- luminance is a decent guess at
//! height, because a photograph of a rough surface is mostly its own
//! shading -- and both spend their controls on the same problem, which is
//! that a photograph carries detail at every scale and a bump map wants
//! only some of them.

use crate::util::{blur_plane, luma_map, put};
use crate::{choice, param, simple_filter};
use schist_plugin_api::{FilterParam, FilterPlugin, FilterValues};

/// How much of the picture's own detail survives into the map.
const BLUR_DETAIL: &[&str] = &["High", "Medium", "Low"];

/// The height field both filters work from.
fn height_field(
    px: &[f32],
    w: usize,
    h: usize,
    blur: usize,
    contrast: f32,
    invert: bool,
) -> Vec<f32> {
    let mut plane = luma_map(px, w, h);
    // High detail keeps the fine grain, Low throws it away and leaves the
    // form. This is Photoshop's Blur Detail, which is a blur radius named
    // for what it preserves rather than for what it does.
    let sigma = [0.4f32, 1.6, 4.0][blur.min(2)];
    blur_plane(&mut plane, w, h, sigma);
    for v in plane.iter_mut() {
        let c = ((*v - 0.5) * (1.0 + contrast * 3.0) + 0.5).clamp(0.0, 1.0);
        *v = if invert { 1.0 - c } else { c };
    }
    plane
}

simple_filter!(
    GenerateBumpMap,
    "filter.bump_map",
    "Generate Bump Map",
    "3D",
    [
        choice("blur", "Blur Detail", BLUR_DETAIL, 1),
        param("contrast", "Contrast", 0.0, 100.0, 30.0, ""),
        param("invert", "Invert Height", 0.0, 1.0, 0.0, "")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // A greyscale height field: white is high, black is low, which is
        // the convention every renderer expects.
        let blur = v.get("blur").round().max(0.0) as usize;
        let contrast = v.get("contrast") / 100.0;
        let invert = v.get("invert") >= 0.5;
        let plane = height_field(px, w, h, blur, contrast, invert);
        for (i, p) in px.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let v = plane[i];
            p[0] = v;
            p[1] = v;
            p[2] = v;
        }
    }
);

simple_filter!(
    GenerateNormalMap,
    "filter.normal_map",
    "Generate Normal Map",
    "3D",
    [
        choice("blur", "Blur Detail", BLUR_DETAIL, 1),
        param("contrast", "Contrast", 0.0, 100.0, 30.0, ""),
        param("strength", "Height Scale", 1.0, 100.0, 30.0, ""),
        param("invert", "Invert Height", 0.0, 1.0, 0.0, "")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // Tangent-space normals: the surface's slope at each point,
        // packed as a unit vector into the three channels with zero at
        // mid grey. A flat surface therefore comes out the familiar
        // lavender, which is (0, 0, 1) written down.
        let blur = v.get("blur").round().max(0.0) as usize;
        let contrast = v.get("contrast") / 100.0;
        let strength = v.get("strength") / 100.0 * 20.0;
        let invert = v.get("invert") >= 0.5;
        let plane = height_field(px, w, h, blur, contrast, invert);
        let at = |x: i32, y: i32| -> f32 {
            let x = x.clamp(0, w as i32 - 1) as usize;
            let y = y.clamp(0, h as i32 - 1) as usize;
            plane[y * w + x]
        };
        for y in 0..h {
            for x in 0..w {
                let (ix, iy) = (x as i32, y as i32);
                // Sobel rather than a plain difference: a normal map made
                // from two-tap gradients is visibly stair-stepped along
                // any edge that is not axis-aligned.
                let gx = at(ix + 1, iy - 1) + 2.0 * at(ix + 1, iy) + at(ix + 1, iy + 1)
                    - at(ix - 1, iy - 1)
                    - 2.0 * at(ix - 1, iy)
                    - at(ix - 1, iy + 1);
                let gy = at(ix - 1, iy + 1) + 2.0 * at(ix, iy + 1) + at(ix + 1, iy + 1)
                    - at(ix - 1, iy - 1)
                    - 2.0 * at(ix, iy - 1)
                    - at(ix + 1, iy - 1);
                let n = [-gx * strength, -gy * strength, 1.0];
                let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt().max(1e-6);
                let a = px[(y * w + x) * 4 + 3];
                put(
                    px,
                    w,
                    x,
                    y,
                    [
                        (n[0] / len * 0.5 + 0.5).clamp(0.0, 1.0),
                        (n[1] / len * 0.5 + 0.5).clamp(0.0, 1.0),
                        (n[2] / len * 0.5 + 0.5).clamp(0.0, 1.0),
                        a,
                    ],
                );
            }
        }
    }
);

pub fn register(registry: &mut schist_plugin_api::PluginRegistry) {
    registry.register_filter(Box::new(GenerateBumpMap));
    registry.register_filter(Box::new(GenerateNormalMap));
}
