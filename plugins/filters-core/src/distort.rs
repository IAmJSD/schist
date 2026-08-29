//! Filter ▸ Distort. Every one of these is a coordinate remap through
//! [`warp`], so they differ only in the mapping.

use crate::util::{blur_plane, fbm, luma, surface, value_noise, warp};
use crate::{choice, param, simple_filter};
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
        param("generators", "Number of Generators", 1.0, 8.0, 1.0, ""),
        param("wavelength", "Wavelength", 1.0, 400.0, 60.0, " px"),
        param("amplitude", "Amplitude", 0.0, 200.0, 15.0, " px"),
        param("horizontal", "Horizontal Scale", 0.0, 100.0, 100.0, "%"),
        param("vertical", "Vertical Scale", 0.0, 100.0, 100.0, "%"),
        choice("type", "Type", &["Sine", "Triangle", "Square"], 0),
        param("seed", "Randomness", 0.0, 999.0, 1.0, "")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // Photoshop's Wave is several wave generators added together,
        // each with its own wavelength and phase inside the ranges the
        // dialog gives -- which is why one generator looks like a ripple
        // and five look like water.
        let generators = v.get("generators").round().max(1.0) as usize;
        let len = v.get("wavelength").max(1.0);
        let amp = v.get("amplitude");
        let hscale = v.get("horizontal") / 100.0;
        let vscale = v.get("vertical") / 100.0;
        let kind = (v.get("type").round().max(0.0) as usize).min(2);
        let seed = v.get("seed") as u32;
        // Square waves displace by a constant either way, which is what
        // gives Wave its torn-paper look; triangles ramp between.
        let shape = move |phase: f32| -> f32 {
            let t = phase.rem_euclid(std::f32::consts::TAU) / std::f32::consts::TAU;
            match kind {
                1 => 4.0 * (t - 0.5).abs() - 1.0,
                2 => {
                    if t < 0.5 {
                        1.0
                    } else {
                        -1.0
                    }
                }
                _ => phase.sin(),
            }
        };
        warp(px, w, h, move |x, y| {
            let (mut ox, mut oy) = (0.0f32, 0.0f32);
            for g in 0..generators {
                // Each generator gets its own wavelength and phase, from
                // the seed, so Randomness rearranges the water without
                // changing how much of it there is.
                let jitter = 0.5 + value_noise(g as f32 * 13.0, 0.0, seed);
                let k = std::f32::consts::TAU / (len * jitter);
                let phase = value_noise(0.0, g as f32 * 7.0, seed) * std::f32::consts::TAU;
                ox += shape(y * k + phase) * amp / generators as f32;
                oy += shape(x * k + phase) * amp / generators as f32;
            }
            (x + ox * hscale, y + oy * vscale)
        });
    }
);

/// Photoshop's three ZigZags, which are three directions to push in.
const ZIGZAG_STYLES: &[&str] = &["Around Center", "Out From Center", "Pond Ripples"];

simple_filter!(
    ZigZag,
    "filter.zigzag",
    "ZigZag",
    "Distort",
    [
        param("amount", "Amount", -100.0, 100.0, 30.0, ""),
        param("ridges", "Ridges", 1.0, 20.0, 5.0, ""),
        choice("style", "Style", ZIGZAG_STYLES, 2)
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        let amount = v.get("amount") / 100.0;
        let ridges = v.get("ridges").max(1.0);
        let style = (v.get("style").round().max(0.0) as usize).min(2);
        let (cx, cy) = (w as f32 / 2.0, h as f32 / 2.0);
        let radius = cx.hypot(cy).max(1.0);
        warp(px, w, h, |x, y| {
            let (dx, dy) = (x - cx, y - cy);
            let d = dx.hypot(dy);
            if d < 1e-3 {
                return (x, y);
            }
            let phase = d / radius * ridges * std::f32::consts::TAU;
            // The three styles differ in *which way* the ripple pushes.
            match style {
                // Around Center: tangentially, so the picture twists back
                // and forth as the rings go out.
                0 => {
                    let twist = phase.sin() * amount * 0.6 * (1.0 - d / radius).max(0.0);
                    let (s, c) = twist.sin_cos();
                    (cx + dx * c - dy * s, cy + dx * s + dy * c)
                }
                // Out From Center: radially, and always outwards, which
                // reads as a starburst rather than as water.
                1 => {
                    let push = phase.sin().abs() * amount * radius * 0.1;
                    (x + dx / d * push, y + dy / d * push)
                }
                // Pond Ripples: radially, signed, and fading out.
                _ => {
                    let push = phase.sin() * amount * radius * 0.1 * (1.0 - d / radius).max(0.0);
                    (x + dx / d * push, y + dy / d * push)
                }
            }
        });
    }
);

simple_filter!(
    Spherize,
    "filter.spherize",
    "Spherize",
    "Distort",
    [
        param("amount", "Amount", -100.0, 100.0, 50.0, "%"),
        choice(
            "mode",
            "Mode",
            &["Normal", "Horizontal Only", "Vertical Only"],
            0
        )
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        let amount = v.get("amount") / 100.0;
        // The two axis modes bulge a cylinder rather than a sphere, which
        // is what you want for wrapping a label round a bottle.
        let mode = (v.get("mode").round().max(0.0) as usize).min(2);
        let (cx, cy) = (w as f32 / 2.0, h as f32 / 2.0);
        let radius = cx.min(cy).max(1.0);
        warp(px, w, h, |x, y| {
            let (dx, dy) = match mode {
                1 => (x - cx, 0.0),
                2 => (0.0, y - cy),
                _ => (x - cx, y - cy),
            };
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
    [
        param("amount", "Amount", -100.0, 100.0, 50.0, "%"),
        choice(
            "mode",
            "Mode",
            &["Normal", "Horizontal Only", "Vertical Only"],
            0
        )
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        let amount = v.get("amount") / 100.0;
        // The two axis modes bulge a cylinder rather than a sphere, which
        // is what you want for wrapping a label round a bottle.
        let mode = (v.get("mode").round().max(0.0) as usize).min(2);
        let (cx, cy) = (w as f32 / 2.0, h as f32 / 2.0);
        let radius = cx.min(cy).max(1.0);
        warp(px, w, h, |x, y| {
            let (dx, dy) = match mode {
                1 => (x - cx, 0.0),
                2 => (0.0, y - cy),
                _ => (x - cx, y - cy),
            };
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
    [choice(
        "to_polar",
        "Convert",
        &["Polar to Rectangular", "Rectangular to Polar"],
        1
    )],
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
    [
        param("amount", "Amount", -200.0, 200.0, 40.0, " px"),
        choice("curve", "Curve", &["Bow", "S-Curve", "Ramp"], 0),
        choice(
            "undefined",
            "Undefined Areas",
            &["Repeat Edge Pixels", "Wrap Around"],
            0
        )
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // Photoshop draws the shear as a curve you drag; the three shapes
        // here are the ones anybody actually drags it into.
        let amount = v.get("amount");
        let curve = (v.get("curve").round().max(0.0) as usize).min(2);
        let wrap = v.get("undefined") >= 0.5;
        let hh = h as f32;
        let ww = w as f32;
        warp(px, w, h, move |x, y| {
            let t = y / hh;
            let shift = match curve {
                1 => (t * std::f32::consts::TAU).sin(),
                2 => t * 2.0 - 1.0,
                _ => (t * std::f32::consts::PI).sin(),
            } * amount;
            let sx = x + shift;
            // What happens at the sides: repeat the edge, which `warp`
            // does by clamping, or bring the other side round.
            (if wrap { sx.rem_euclid(ww) } else { sx }, y)
        });
    }
);

simple_filter!(
    Displace,
    "filter.displace",
    "Displace",
    "Distort",
    [
        param("scale", "Horizontal Scale", 0.0, 200.0, 20.0, " px"),
        param("vscale", "Vertical Scale", 0.0, 200.0, 20.0, " px"),
        param("detail", "Detail", 1.0, 64.0, 16.0, " px"),
        param("seed", "Randomness", 0.0, 999.0, 1.0, ""),
        choice(
            "undefined",
            "Undefined Areas",
            &["Repeat Edge Pixels", "Wrap Around"],
            0
        )
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // Photoshop displaces through a separate map file, chosen from a
        // file picker a filter does not have. This uses its own noise
        // field instead, with the same two scales and the same choice
        // about what happens at the edges.
        let scale = v.get("scale");
        let vscale = v.get("vscale");
        let detail = v.get("detail").max(1.0);
        let seed = v.get("seed") as u32;
        let wrap = v.get("undefined") >= 0.5;
        let (ww, hh) = (w as f32, h as f32);
        warp(px, w, h, move |x, y| {
            let u = fbm(x / detail, y / detail, 11 + seed, 3) - 0.5;
            let vv = fbm(x / detail + 37.0, y / detail - 19.0, 23 + seed, 3) - 0.5;
            let (sx, sy) = (x + u * scale * 2.0, y + vv * vscale * 2.0);
            if wrap {
                (sx.rem_euclid(ww), sy.rem_euclid(hh))
            } else {
                (sx, sy)
            }
        });
    }
);

// The three Distort effects that live in the Filter Gallery rather than
// in the Filter menu. They are distortions in the same sense as the rest
// of this module -- Glass and Ocean Ripple are coordinate remaps -- with
// the exception of Diffuse Glow, which Adobe filed here because it is
// what happens when you distort *light* instead of position.

simple_filter!(
    DiffuseGlow,
    "filter.diffuse_glow",
    "Diffuse Glow",
    "Distort",
    [
        param("graininess", "Graininess", 0.0, 10.0, 6.0, ""),
        param("glow", "Glow Amount", 0.0, 20.0, 10.0, ""),
        param("clear", "Clear Amount", 0.0, 20.0, 15.0, "")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // Light bleeding out of the highlights through a grainy diffusion
        // filter, which is what a stocking over the lens does. Clear
        // Amount is the threshold: below it nothing glows, which is what
        // keeps the shadows from fogging.
        let graininess = v.get("graininess") / 10.0;
        let glow = v.get("glow") / 20.0;
        let clear = v.get("clear") / 20.0;
        let mut bright: Vec<f32> = px
            .as_chunks::<4>()
            .0
            .iter()
            .map(|p| (luma(p) - clear).max(0.0) / (1.0 - clear).max(1e-3))
            .collect();
        blur_plane(&mut bright, w, h, 4.0 + glow * 12.0);
        for (i, p) in px.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let (x, y) = ((i % w) as f32, (i / w) as f32);
            let grain = (value_noise(x, y, 5237) - 0.5) * graininess * 0.35;
            let lift = (bright[i] * (0.6 + glow * 2.0) + grain).max(0.0);
            for v in p.iter_mut().take(3) {
                // Screened towards white rather than added, so the glow
                // saturates the way light does instead of clipping.
                *v = (1.0 - (1.0 - *v) * (1.0 - lift.min(1.0))).clamp(0.0, 1.0);
            }
        }
    }
);

simple_filter!(
    Glass,
    "filter.glass",
    "Glass",
    "Distort",
    [
        param("distortion", "Distortion", 0.0, 20.0, 5.0, ""),
        param("smoothness", "Smoothness", 1.0, 15.0, 3.0, ""),
        choice("texture", "Texture", GLASS_TEXTURES, 0),
        param("scaling", "Scaling", 50.0, 200.0, 100.0, "%")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // Seen through a sheet of textured glass: the texture is a height
        // field, and every pixel is fetched from wherever that height
        // field's slope bends the light. Smoothness is the polish on the
        // glass.
        let distortion = v.get("distortion");
        let smoothness = v.get("smoothness");
        let kind = v.get("texture").round().max(0.0) as u32;
        let scaling = (v.get("scaling") / 100.0 * 10.0).max(1.0);
        let height = |x: f32, y: f32| -> f32 {
            match kind {
                // Frosted: fine noise, softened by Smoothness.
                0 => fbm(
                    x / (smoothness * 2.0).max(1.0),
                    y / (smoothness * 2.0).max(1.0),
                    337,
                    3,
                ),
                // Blocks: a grid of panes.
                1 => {
                    let (u, vv) = ((x / scaling).fract(), (y / scaling).fract());
                    ((u - 0.5).abs() + (vv - 0.5).abs()) * 0.7
                }
                // Canvas and tiny lens use the shared surface generator.
                _ => surface(kind - 2, x, y, scaling, 149),
            }
        };
        warp(px, w, h, |x, y| {
            let gx = height(x + 1.0, y) - height(x - 1.0, y);
            let gy = height(x, y + 1.0) - height(x, y - 1.0);
            (x + gx * distortion * 1.2, y + gy * distortion * 1.2)
        });
    }
);

/// Glass has its own texture list: the first two are its own, the rest
/// are the surfaces the Texture group uses.
const GLASS_TEXTURES: &[&str] = &[
    "Frosted",
    "Blocks",
    "Canvas",
    "Sandstone",
    "Burlap",
    "Brick",
];

simple_filter!(
    OceanRipple,
    "filter.ocean_ripple",
    "Ocean Ripple",
    "Distort",
    [
        param("size", "Ripple Size", 1.0, 15.0, 9.0, ""),
        param("magnitude", "Ripple Magnitude", 0.0, 20.0, 9.0, "")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // Randomly spaced ripples, as though the image were under moving
        // water. Distort ▸ Ripple is regular and periodic; this one takes
        // its offsets from a noise field, which is why it looks wet
        // rather than corrugated.
        let size = v.get("size").max(1.0) * 6.0;
        let magnitude = v.get("magnitude");
        warp(px, w, h, |x, y| {
            let a = fbm(x / size, y / size, 1049, 2) - 0.5;
            let b = fbm(x / size + 31.0, y / size + 17.0, 1049, 2) - 0.5;
            (x + a * magnitude * 2.0, y + b * magnitude * 2.0)
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
    registry.register_filter(Box::new(DiffuseGlow));
    registry.register_filter(Box::new(Glass));
    registry.register_filter(Box::new(OceanRipple));
}
