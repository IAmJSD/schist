//! Filter ▸ Render: filters that generate rather than transform.

use crate::util::{at, fbm, put};
use crate::{param, simple_filter};
use photoslop_plugin_api::{FilterParam, FilterPlugin, FilterValues};

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

pub fn register(registry: &mut photoslop_plugin_api::PluginRegistry) {
    registry.register_filter(Box::new(Clouds));
    registry.register_filter(Box::new(DifferenceClouds));
    registry.register_filter(Box::new(Fibers));
    registry.register_filter(Box::new(LensFlare));
}
