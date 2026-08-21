//! Filter ▸ Distort. Every one of these is a coordinate remap through
//! [`warp`], so they differ only in the mapping.

use crate::util::{fbm, warp};
use crate::{param, simple_filter};
use schist_plugin_api::{FilterParam, FilterPlugin, FilterValues};

simple_filter!(
    Twirl,
    "filter.twirl",
    "Twirl",
    "Distort",
    [param("angle", "Angle", -999.0, 999.0, 50.0, "\u{b0}")],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        let angle = v.get("angle").to_radians();
        let (cx, cy) = (w as f32 / 2.0, h as f32 / 2.0);
        let radius = cx.hypot(cy);
        warp(px, w, h, |x, y| {
            let (dx, dy) = (x - cx, y - cy);
            let d = dx.hypot(dy);
            if d >= radius {
                return (x, y);
            }
            // Rotation falls off to nothing at the edge of the circle.
            let t = angle * (1.0 - d / radius).powi(2);
            let (s, c) = t.sin_cos();
            (cx + dx * c - dy * s, cy + dx * s + dy * c)
        });
    }
);

simple_filter!(
    Ripple,
    "filter.ripple",
    "Ripple",
    "Distort",
    [
        param("amount", "Amount", -999.0, 999.0, 100.0, ""),
        param("size", "Size", 1.0, 64.0, 12.0, " px")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        let amount = v.get("amount") / 100.0;
        let size = v.get("size").max(1.0);
        warp(px, w, h, |x, y| {
            (
                x + (y / size).sin() * amount * size * 0.25,
                y + (x / size).sin() * amount * size * 0.25,
            )
        });
    }
);

simple_filter!(
    Wave,
    "filter.wave",
    "Wave",
    "Distort",
    [
        param("wavelength", "Wavelength", 1.0, 400.0, 60.0, " px"),
        param("amplitude", "Amplitude", 0.0, 200.0, 15.0, " px")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        let len = v.get("wavelength").max(1.0);
        let amp = v.get("amplitude");
        let k = std::f32::consts::TAU / len;
        warp(px, w, h, |x, y| {
            (x + (y * k).sin() * amp, y + (x * k).sin() * amp)
        });
    }
);

simple_filter!(
    ZigZag,
    "filter.zigzag",
    "ZigZag",
    "Distort",
    [
        param("amount", "Amount", -100.0, 100.0, 30.0, ""),
        param("ridges", "Ridges", 1.0, 20.0, 5.0, "")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        let amount = v.get("amount") / 100.0;
        let ridges = v.get("ridges").max(1.0);
        let (cx, cy) = (w as f32 / 2.0, h as f32 / 2.0);
        let radius = cx.hypot(cy).max(1.0);
        warp(px, w, h, |x, y| {
            let (dx, dy) = (x - cx, y - cy);
            let d = dx.hypot(dy);
            if d < 1e-3 {
                return (x, y);
            }
            // Pond ripples: displacement along the radius, fading out.
            let phase = d / radius * ridges * std::f32::consts::TAU;
            let push = phase.sin() * amount * radius * 0.1 * (1.0 - d / radius).max(0.0);
            (x + dx / d * push, y + dy / d * push)
        });
    }
);

simple_filter!(
    Spherize,
    "filter.spherize",
    "Spherize",
    "Distort",
    [param("amount", "Amount", -100.0, 100.0, 50.0, "%")],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        let amount = v.get("amount") / 100.0;
        let (cx, cy) = (w as f32 / 2.0, h as f32 / 2.0);
        let radius = cx.min(cy).max(1.0);
        warp(px, w, h, |x, y| {
            let (dx, dy) = (x - cx, y - cy);
            let d = dx.hypot(dy);
            if d >= radius || d < 1e-3 {
                return (x, y);
            }
            let t = d / radius;
            // asin gives the bulge of a hemisphere seen head on.
            let bulged = (t.asin() / (std::f32::consts::FRAC_PI_2)).clamp(0.0, 1.0);
            let scale = 1.0 + (bulged / t - 1.0) * amount;
            (cx + dx * scale, cy + dy * scale)
        });
    }
);

simple_filter!(
    Pinch,
    "filter.pinch",
    "Pinch",
    "Distort",
    [param("amount", "Amount", -100.0, 100.0, 50.0, "%")],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        let amount = v.get("amount") / 100.0;
        let (cx, cy) = (w as f32 / 2.0, h as f32 / 2.0);
        let radius = cx.min(cy).max(1.0);
        warp(px, w, h, |x, y| {
            let (dx, dy) = (x - cx, y - cy);
            let d = dx.hypot(dy);
            if d >= radius || d < 1e-3 {
                return (x, y);
            }
            let t = d / radius;
            let scale = t.powf(1.0 + amount) / t;
            (cx + dx * scale, cy + dy * scale)
        });
    }
);

simple_filter!(
    PolarCoordinates,
    "filter.polar",
    "Polar Coordinates",
    "Distort",
    [param("to_polar", "Rectangular to Polar", 0.0, 1.0, 1.0, "")],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        let to_polar = v.get("to_polar") >= 0.5;
        let (cx, cy) = (w as f32 / 2.0, h as f32 / 2.0);
        let radius = cx.hypot(cy).max(1.0);
        warp(px, w, h, move |x, y| {
            if to_polar {
                // Destination is polar: angle across, radius down.
                let (dx, dy) = (x - cx, y - cy);
                let theta = dy.atan2(dx) + std::f32::consts::PI;
                let r = dx.hypot(dy);
                (
                    theta / std::f32::consts::TAU * w as f32,
                    r / radius * h as f32,
                )
            } else {
                let theta = x / w as f32 * std::f32::consts::TAU - std::f32::consts::PI;
                let r = y / h as f32 * radius;
                (cx + r * theta.cos(), cy + r * theta.sin())
            }
        });
    }
);

simple_filter!(
    Shear,
    "filter.shear",
    "Shear",
    "Distort",
    [param("amount", "Amount", -200.0, 200.0, 40.0, " px")],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        let amount = v.get("amount");
        let hh = h as f32;
        warp(px, w, h, |x, y| {
            // A single bow across the height, which is what Shear's
            // default curve does.
            (x + (y / hh * std::f32::consts::PI).sin() * amount, y)
        });
    }
);

simple_filter!(
    Displace,
    "filter.displace",
    "Displace",
    "Distort",
    [
        param("scale", "Scale", 0.0, 200.0, 20.0, " px"),
        param("detail", "Detail", 1.0, 64.0, 16.0, " px")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // Photoshop displaces through a separate map file; with nowhere to
        // pick one, this uses its own noise field, which is what the
        // filter is mostly used for anyway.
        let scale = v.get("scale");
        let detail = v.get("detail").max(1.0);
        warp(px, w, h, |x, y| {
            let u = fbm(x / detail, y / detail, 11, 3) - 0.5;
            let vv = fbm(x / detail + 37.0, y / detail - 19.0, 23, 3) - 0.5;
            (x + u * scale * 2.0, y + vv * scale * 2.0)
        });
    }
);

pub fn register(registry: &mut schist_plugin_api::PluginRegistry) {
    registry.register_filter(Box::new(Twirl));
    registry.register_filter(Box::new(Ripple));
    registry.register_filter(Box::new(Wave));
    registry.register_filter(Box::new(ZigZag));
    registry.register_filter(Box::new(Spherize));
    registry.register_filter(Box::new(Pinch));
    registry.register_filter(Box::new(PolarCoordinates));
    registry.register_filter(Box::new(Shear));
    registry.register_filter(Box::new(Displace));
}
