//! Filter ▸ Render: filters that generate rather than transform.

use crate::util::{at, blur_plane, fbm, luma_map, put};
use crate::{choice, param, simple_filter};
use schist_plugin_api::{FilterParam, FilterPlugin, FilterValues};

simple_filter!(
    Clouds,
    "filter.clouds",
    "Clouds",
    "Render",
    [
        param("scale", "Scale", 4.0, 512.0, 96.0, " px"),
        param("detail", "Detail", 1.0, 8.0, 5.0, ""),
        param("seed", "Seed", 0.0, 999.0, 1.0, "")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // Photoshop draws clouds in the foreground and background colours;
        // filters do not see those, so this renders greyscale, which is
        // what Clouds is used as a source for anyway.
        let scale = v.get("scale").max(4.0);
        let octaves = v.get("detail").max(1.0) as u32;
        let seed = v.get("seed") as u32;
        for y in 0..h {
            for x in 0..w {
                let n = fbm(x as f32 / scale, y as f32 / scale, seed, octaves);
                let a = at(px, w, h, x as i32, y as i32)[3];
                put(px, w, x, y, [n, n, n, a.max(1.0)]);
            }
        }
    }
);

simple_filter!(
    DifferenceClouds,
    "filter.difference_clouds",
    "Difference Clouds",
    "Render",
    [
        param("scale", "Scale", 4.0, 512.0, 96.0, " px"),
        param("detail", "Detail", 1.0, 8.0, 5.0, ""),
        param("seed", "Seed", 0.0, 999.0, 1.0, "")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // Same field, differenced against what is already there, which is
        // what gives the veined look when applied repeatedly.
        let scale = v.get("scale").max(4.0);
        let octaves = v.get("detail").max(1.0) as u32;
        let seed = v.get("seed") as u32;
        for y in 0..h {
            for x in 0..w {
                let n = fbm(x as f32 / scale, y as f32 / scale, seed, octaves);
                let p = at(px, w, h, x as i32, y as i32);
                put(
                    px,
                    w,
                    x,
                    y,
                    [(p[0] - n).abs(), (p[1] - n).abs(), (p[2] - n).abs(), p[3]],
                );
            }
        }
    }
);

simple_filter!(
    Fibers,
    "filter.fibers",
    "Fibers",
    "Render",
    [
        param("variance", "Variance", 1.0, 64.0, 16.0, ""),
        param("strength", "Strength", 1.0, 64.0, 4.0, "")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // Vertical streaks: noise that varies fast across and slowly down.
        let variance = v.get("variance").max(1.0);
        let strength = v.get("strength").max(1.0);
        for y in 0..h {
            for x in 0..w {
                let n = fbm(x as f32 * variance / 16.0, y as f32 / strength, 977, 4);
                let a = at(px, w, h, x as i32, y as i32)[3];
                put(px, w, x, y, [n, n, n, a.max(1.0)]);
            }
        }
    }
);

simple_filter!(
    LensFlare,
    "filter.lens_flare",
    "Lens Flare",
    "Render",
    [
        param("x", "Centre X", 0.0, 100.0, 50.0, "%"),
        param("y", "Centre Y", 0.0, 100.0, 50.0, "%"),
        param("brightness", "Brightness", 10.0, 300.0, 100.0, "%")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        let cx = v.get("x") / 100.0 * w as f32;
        let cy = v.get("y") / 100.0 * h as f32;
        let strength = v.get("brightness") / 100.0;
        let span = (w.max(h) as f32).max(1.0);
        // The main glow, plus ghosts spaced along the line through the
        // frame's centre -- which is where real lens ghosts appear.
        let (mx, my) = (w as f32 / 2.0, h as f32 / 2.0);
        let ghosts: [(f32, f32, f32); 5] = [
            (0.35, 0.10, 0.6),
            (0.70, 0.06, 0.9),
            (1.30, 0.05, 0.8),
            (1.70, 0.08, 0.5),
            (2.10, 0.04, 1.1),
        ];
        for y in 0..h {
            for x in 0..w {
                let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
                let d = (fx - cx).hypot(fy - cy) / span;
                // Core glare falls off sharply, with a wide soft halo.
                let mut add =
                    (0.35 / (1.0 + d * d * 900.0) + 0.12 / (1.0 + d * d * 40.0)) * strength;
                for (t, size, tint) in ghosts {
                    let gx = cx + (mx - cx) * 2.0 * t;
                    let gy = cy + (my - cy) * 2.0 * t;
                    let gd = (fx - gx).hypot(fy - gy) / span;
                    add += (0.06 * tint / (1.0 + gd * gd / (size * size))) * strength;
                }
                let p = at(px, w, h, x as i32, y as i32);
                put(
                    px,
                    w,
                    x,
                    y,
                    [
                        (p[0] + add).clamp(0.0, 1.0),
                        (p[1] + add * 0.95).clamp(0.0, 1.0),
                        (p[2] + add * 0.85).clamp(0.0, 1.0),
                        p[3],
                    ],
                );
            }
        }
    }
);

/// The lights this build offers, which are Photoshop's three.
const LIGHT_TYPES: &[&str] = &["Spot", "Point", "Infinite"];

// Filter ▸ Render ▸ Lighting Effects.
//
// Photoshop's version is a room full of controls with lights you drag
// around on the canvas. What is underneath is simpler than the interface
// suggests: a light is a direction and a falloff, the image doubles as a
// bump map, and the result is Phong shading -- ambient plus diffuse plus
// a specular highlight -- multiplied back over the colour.
//
// The bump map is the interesting part and the reason this filter looks
// like nothing else: shading a photograph by *its own luminance* treated
// as a height field is what makes a flat picture look embossed and lit
// rather than merely brightened.
simple_filter!(
    LightingEffects,
    "filter.lighting_effects",
    "Lighting Effects",
    "Render",
    [
        choice("type", "Light Type", LIGHT_TYPES, 0),
        param("x", "Light X", 0.0, 100.0, 30.0, "%"),
        param("y", "Light Y", 0.0, 100.0, 25.0, "%"),
        param("angle", "Direction", 0.0, 360.0, 45.0, "\u{b0}"),
        param("intensity", "Intensity", 0.0, 300.0, 120.0, "%"),
        param("spread", "Spread", 5.0, 200.0, 60.0, "%"),
        param("ambience", "Ambience", 0.0, 100.0, 35.0, "%"),
        param("gloss", "Gloss", 0.0, 100.0, 30.0, "%"),
        param("height", "Texture Height", 0.0, 200.0, 60.0, "%")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        let kind = (v.get("type").round().max(0.0) as usize).min(2);
        let intensity = v.get("intensity") / 100.0;
        let ambience = v.get("ambience") / 100.0;
        let gloss = v.get("gloss") / 100.0;
        let bump = v.get("height") / 100.0;
        let spread = (v.get("spread") / 100.0).max(0.05);
        let angle = v.get("angle").to_radians();
        let (lx, ly) = (v.get("x") / 100.0 * w as f32, v.get("y") / 100.0 * h as f32);
        // The height field, softened: shading straight off the pixels
        // turns every speck of noise into a boulder.
        let mut height: Vec<f32> = luma_map(px, w, h);
        blur_plane(&mut height, w, h, 1.2);
        let diagonal = (w * w + h * h) as f32;
        let reach = (diagonal.sqrt() * spread).max(1.0);

        for y in 0..h {
            for x in 0..w {
                // Surface normal from the height field. The scale is
                // arbitrary and is what Texture Height sets.
                let (gx, gy) = {
                    let at = |xx: i32, yy: i32| -> f32 {
                        let xx = xx.clamp(0, w as i32 - 1) as usize;
                        let yy = yy.clamp(0, h as i32 - 1) as usize;
                        height[yy * w + xx]
                    };
                    (
                        at(x as i32 + 1, y as i32) - at(x as i32 - 1, y as i32),
                        at(x as i32, y as i32 + 1) - at(x as i32, y as i32 - 1),
                    )
                };
                let scale = 8.0 * bump;
                let n = [-gx * scale, -gy * scale, 1.0];
                let nl = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt().max(1e-6);
                let n = [n[0] / nl, n[1] / nl, n[2] / nl];

                // Direction to the light, and how much of it arrives.
                let (to_light, falloff) = match kind {
                    // Infinite: a sun. Parallel rays, no falloff.
                    2 => ([angle.cos(), angle.sin(), 0.8], 1.0),
                    _ => {
                        let (dx, dy) = (lx - x as f32, ly - y as f32);
                        let d = dx.hypot(dy);
                        // Spot: a cone that fades from the middle out.
                        // Point: the same falloff without the height, so
                        // it grazes rather than shines down.
                        let f = (1.0 - (d / reach)).clamp(0.0, 1.0);
                        let f = if kind == 0 { f * f } else { f };
                        (
                            [
                                dx / d.max(1e-6),
                                dy / d.max(1e-6),
                                if kind == 0 { 1.2 } else { 0.5 },
                            ],
                            f,
                        )
                    }
                };
                let ll = (to_light[0] * to_light[0]
                    + to_light[1] * to_light[1]
                    + to_light[2] * to_light[2])
                    .sqrt()
                    .max(1e-6);
                let l = [to_light[0] / ll, to_light[1] / ll, to_light[2] / ll];

                let diffuse = (n[0] * l[0] + n[1] * l[1] + n[2] * l[2]).max(0.0);
                // Specular against the eye, which is straight above.
                let half = [l[0], l[1], l[2] + 1.0];
                let hl = (half[0] * half[0] + half[1] * half[1] + half[2] * half[2])
                    .sqrt()
                    .max(1e-6);
                let spec = ((n[0] * half[0] + n[1] * half[1] + n[2] * half[2]) / hl)
                    .max(0.0)
                    .powf(4.0 + gloss * 60.0)
                    * gloss;

                let lit = ambience + intensity * falloff * (diffuse + spec);
                let p = at(px, w, h, x as i32, y as i32);
                put(
                    px,
                    w,
                    x,
                    y,
                    [
                        (p[0] * lit).clamp(0.0, 1.0),
                        (p[1] * lit).clamp(0.0, 1.0),
                        (p[2] * lit).clamp(0.0, 1.0),
                        p[3],
                    ],
                );
            }
        }
    }
);

pub fn register(registry: &mut schist_plugin_api::PluginRegistry) {
    registry.register_filter(Box::new(Clouds));
    registry.register_filter(Box::new(DifferenceClouds));
    registry.register_filter(Box::new(Fibers));
    registry.register_filter(Box::new(LensFlare));
    registry.register_filter(Box::new(LightingEffects));
}
