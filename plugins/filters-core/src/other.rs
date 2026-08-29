//! Filter ▸ Other, plus the extra Blur, Sharpen and Noise entries that did
//! not exist yet.

use crate::util::{at, convolve3, gaussian_rgba, premultiply, put, unpremultiply, value_noise};
use crate::{choice, param, simple_filter};
use schist_plugin_api::{FilterParam, FilterPlugin, FilterValues};

simple_filter!(
    HighPass,
    "filter.high_pass",
    "High Pass",
    "Other",
    [param("radius", "Radius", 0.1, 250.0, 3.0, " px")],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // What is left after the low frequencies are taken away, centred
        // on mid grey. The classic sharpening pre-pass.
        let mut low = px.to_vec();
        gaussian_rgba(&mut low, w, h, v.get("radius"));
        for (p, l) in px
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .zip(low.as_chunks::<4>().0.iter())
        {
            for c in 0..3 {
                p[c] = (p[c] - l[c] + 0.5).clamp(0.0, 1.0);
            }
        }
    }
);

simple_filter!(
    Offset,
    "filter.offset",
    "Offset",
    "Other",
    [
        param("x", "Horizontal", -2000.0, 2000.0, 0.0, " px"),
        param("y", "Vertical", -2000.0, 2000.0, 0.0, " px"),
        param("wrap", "Wrap Around", 0.0, 1.0, 1.0, "")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        let dx = v.get("x").round() as i32;
        let dy = v.get("y").round() as i32;
        let wrap = v.get("wrap") >= 0.5;
        let src = px.to_vec();
        for y in 0..h as i32 {
            for x in 0..w as i32 {
                let (mut sx, mut sy) = (x - dx, y - dy);
                if wrap {
                    sx = sx.rem_euclid(w as i32);
                    sy = sy.rem_euclid(h as i32);
                } else if sx < 0 || sy < 0 || sx >= w as i32 || sy >= h as i32 {
                    put(px, w, x as usize, y as usize, [0.0; 4]);
                    continue;
                }
                put(px, w, x as usize, y as usize, at(&src, w, h, sx, sy));
            }
        }
    }
);

/// Grey-level morphology: dilate (`max`) grows light areas, erode grows
/// dark ones. Photoshop calls them Maximum and Minimum.
fn morph(px: &mut [f32], w: usize, h: usize, radius: i32, take_max: bool) {
    let src = px.to_vec();
    for y in 0..h as i32 {
        for x in 0..w as i32 {
            let mut acc = if take_max { [0.0f32; 4] } else { [1.0f32; 4] };
            for dy in -radius..=radius {
                for dx in -radius..=radius {
                    if dx * dx + dy * dy > radius * radius {
                        continue;
                    }
                    let p = at(&src, w, h, x + dx, y + dy);
                    for c in 0..4 {
                        acc[c] = if take_max {
                            acc[c].max(p[c])
                        } else {
                            acc[c].min(p[c])
                        };
                    }
                }
            }
            put(px, w, x as usize, y as usize, acc);
        }
    }
}

simple_filter!(
    Maximum,
    "filter.maximum",
    "Maximum",
    "Other",
    [param("radius", "Radius", 1.0, 40.0, 2.0, " px")],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        morph(px, w, h, v.get("radius").round().max(1.0) as i32, true);
    }
);

simple_filter!(
    Minimum,
    "filter.minimum",
    "Minimum",
    "Other",
    [param("radius", "Radius", 1.0, 40.0, 2.0, " px")],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        morph(px, w, h, v.get("radius").round().max(1.0) as i32, false);
    }
);

simple_filter!(
    RadialBlur,
    "filter.radial_blur",
    "Radial Blur",
    "Blur",
    [
        param("amount", "Amount", 1.0, 100.0, 10.0, ""),
        param("spin", "Spin (0 = Zoom)", 0.0, 1.0, 1.0, "")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // Average along an arc (spin) or a ray (zoom) through the centre.
        let amount = v.get("amount") / 100.0;
        let spin = v.get("spin") >= 0.5;
        let (cx, cy) = (w as f32 / 2.0, h as f32 / 2.0);
        premultiply(px);
        let src = px.to_vec();
        const STEPS: usize = 16;
        for y in 0..h {
            for x in 0..w {
                let (fx, fy) = (x as f32 + 0.5 - cx, y as f32 + 0.5 - cy);
                let r = fx.hypot(fy);
                let theta = fy.atan2(fx);
                let mut acc = [0.0f32; 4];
                for s in 0..STEPS {
                    let t = s as f32 / (STEPS - 1) as f32 - 0.5;
                    let (sx, sy) = if spin {
                        let a = theta + t * amount;
                        (cx + r * a.cos(), cy + r * a.sin())
                    } else {
                        let k = 1.0 + t * amount;
                        (cx + fx * k, cy + fy * k)
                    };
                    let p = crate::util::sample(&src, w, h, sx - 0.5, sy - 0.5);
                    for c in 0..4 {
                        acc[c] += p[c] / STEPS as f32;
                    }
                }
                put(px, w, x, y, acc);
            }
        }
        unpremultiply(px);
    }
);

simple_filter!(
    SurfaceBlur,
    "filter.surface_blur",
    "Surface Blur",
    "Blur",
    [
        param("radius", "Radius", 1.0, 40.0, 5.0, " px"),
        param("threshold", "Threshold", 1.0, 255.0, 15.0, "")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // Bilateral: average only over neighbours that look similar, so
        // flat areas smooth and edges stay put.
        let r = v.get("radius").round().max(1.0) as i32;
        let t = (v.get("threshold") / 255.0).max(1e-3);
        let src = px.to_vec();
        for y in 0..h as i32 {
            for x in 0..w as i32 {
                let centre = at(&src, w, h, x, y);
                let mut acc = [0.0f32; 4];
                let mut wsum = 0.0f32;
                for dy in -r..=r {
                    for dx in -r..=r {
                        if dx * dx + dy * dy > r * r {
                            continue;
                        }
                        let p = at(&src, w, h, x + dx, y + dy);
                        let d = (p[0] - centre[0])
                            .abs()
                            .max((p[1] - centre[1]).abs())
                            .max((p[2] - centre[2]).abs());
                        if d > t {
                            continue;
                        }
                        let k = 1.0 - d / t;
                        for c in 0..4 {
                            acc[c] += p[c] * k;
                        }
                        wsum += k;
                    }
                }
                if wsum > 0.0 {
                    for a in acc.iter_mut() {
                        *a /= wsum;
                    }
                    put(px, w, x as usize, y as usize, acc);
                }
            }
        }
    }
);

simple_filter!(
    AverageBlur,
    "filter.average",
    "Average",
    "Blur",
    [],
    |px: &mut [f32], w: usize, h: usize, _v: &FilterValues| {
        let _ = (w, h);
        let mut acc = [0.0f64; 4];
        let n = (px.len() / 4) as f64;
        for p in px.as_chunks::<4>().0.iter() {
            for c in 0..4 {
                acc[c] += p[c] as f64;
            }
        }
        if n == 0.0 {
            return;
        }
        let mean = [
            (acc[0] / n) as f32,
            (acc[1] / n) as f32,
            (acc[2] / n) as f32,
            (acc[3] / n) as f32,
        ];
        for p in px.as_chunks_mut::<4>().0.iter_mut() {
            p[0] = mean[0];
            p[1] = mean[1];
            p[2] = mean[2];
        }
    }
);

simple_filter!(
    LensBlur,
    "filter.lens_blur",
    "Lens Blur",
    "Blur",
    [
        param("radius", "Radius", 1.0, 60.0, 8.0, " px"),
        param("brightness", "Specular Brightness", 0.0, 100.0, 0.0, "")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // A disc-shaped kernel rather than a Gaussian, which is what makes
        // out-of-focus highlights come out as bokeh circles — and, at
        // radius 60, eleven thousand taps a pixel, so this is the filter
        // with most to gain from the GPU seam.
        schist_fx::lens_blur_rgba(
            px,
            w,
            h,
            v.get("radius").round().max(1.0) as i32,
            v.get("brightness") / 100.0,
        );
    }
);

simple_filter!(
    SmartSharpen,
    "filter.smart_sharpen",
    "Smart Sharpen",
    "Sharpen",
    [
        param("amount", "Amount", 1.0, 500.0, 100.0, "%"),
        param("radius", "Radius", 0.1, 64.0, 1.5, " px"),
        param("noise", "Reduce Noise", 0.0, 100.0, 10.0, "%")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // Unsharp mask that leaves low-contrast detail alone, which is
        // what stops it amplifying grain.
        let amount = v.get("amount") / 100.0;
        let floor = v.get("noise") / 100.0 * 0.25;
        let mut low = px.to_vec();
        gaussian_rgba(&mut low, w, h, v.get("radius"));
        for (p, l) in px
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .zip(low.as_chunks::<4>().0.iter())
        {
            for c in 0..3 {
                let d = p[c] - l[c];
                if d.abs() <= floor {
                    continue;
                }
                p[c] = (p[c] + d * amount).clamp(0.0, 1.0);
            }
        }
    }
);

simple_filter!(
    SharpenEdges,
    "filter.sharpen_edges",
    "Sharpen Edges",
    "Sharpen",
    [],
    |px: &mut [f32], w: usize, h: usize, _v: &FilterValues| {
        // Sharpen only where there is already an edge.
        let mut low = px.to_vec();
        gaussian_rgba(&mut low, w, h, 1.5);
        for (p, l) in px
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .zip(low.as_chunks::<4>().0.iter())
        {
            let contrast = (p[0] - l[0])
                .abs()
                .max((p[1] - l[1]).abs())
                .max((p[2] - l[2]).abs());
            if contrast < 0.03 {
                continue;
            }
            for c in 0..3 {
                p[c] = (p[c] + (p[c] - l[c]) * 1.5).clamp(0.0, 1.0);
            }
        }
    }
);

simple_filter!(
    Despeckle,
    "filter.despeckle",
    "Despeckle",
    "Noise",
    [],
    |px: &mut [f32], w: usize, h: usize, _v: &FilterValues| {
        // Median of 3x3, but only away from edges, so detail survives.
        let src = px.to_vec();
        for y in 0..h as i32 {
            for x in 0..w as i32 {
                let mut vals: Vec<[f32; 4]> = Vec::with_capacity(9);
                for dy in -1..=1 {
                    for dx in -1..=1 {
                        vals.push(at(&src, w, h, x + dx, y + dy));
                    }
                }
                let centre = at(&src, w, h, x, y);
                let mut out = centre;
                for c in 0..3 {
                    let mut ch: Vec<f32> = vals.iter().map(|p| p[c]).collect();
                    ch.sort_by(|a, b| a.total_cmp(b));
                    let median = ch[4];
                    // Only replace where the pixel is an outlier.
                    if (centre[c] - median).abs() > 0.04 {
                        out[c] = median;
                    }
                }
                put(px, w, x as usize, y as usize, out);
            }
        }
    }
);

simple_filter!(
    DustAndScratches,
    "filter.dust_scratches",
    "Dust & Scratches",
    "Noise",
    [
        param("radius", "Radius", 1.0, 16.0, 2.0, " px"),
        param("threshold", "Threshold", 0.0, 255.0, 20.0, "")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        let r = v.get("radius").round().max(1.0) as i32;
        let t = v.get("threshold") / 255.0;
        let src = px.to_vec();
        for y in 0..h as i32 {
            for x in 0..w as i32 {
                let centre = at(&src, w, h, x, y);
                let mut out = centre;
                for c in 0..3 {
                    let mut ch = Vec::new();
                    for dy in -r..=r {
                        for dx in -r..=r {
                            if dx * dx + dy * dy > r * r {
                                continue;
                            }
                            ch.push(at(&src, w, h, x + dx, y + dy)[c]);
                        }
                    }
                    ch.sort_by(|a, b| a.total_cmp(b));
                    let median = ch[ch.len() / 2];
                    // Threshold spares detail that differs by less.
                    if (centre[c] - median).abs() > t {
                        out[c] = median;
                    }
                }
                put(px, w, x as usize, y as usize, out);
            }
        }
    }
);

simple_filter!(
    ReduceNoise,
    "filter.reduce_noise",
    "Reduce Noise",
    "Noise",
    [
        param("strength", "Strength", 0.0, 10.0, 5.0, ""),
        param("detail", "Preserve Details", 0.0, 100.0, 60.0, "%")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // Bilateral again, but tuned the other way round: a small spatial
        // radius with a threshold set by how much detail to keep.
        let strength = v.get("strength");
        let detail = v.get("detail") / 100.0;
        let r = (strength.max(0.5)).round() as i32;
        let t = (0.25 * (1.0 - detail)).max(1e-3);
        let src = px.to_vec();
        for y in 0..h as i32 {
            for x in 0..w as i32 {
                let centre = at(&src, w, h, x, y);
                let mut acc = [0.0f32; 4];
                let mut wsum = 0.0f32;
                for dy in -r..=r {
                    for dx in -r..=r {
                        let p = at(&src, w, h, x + dx, y + dy);
                        let d = (p[0] - centre[0])
                            .abs()
                            .max((p[1] - centre[1]).abs())
                            .max((p[2] - centre[2]).abs());
                        let k = (1.0 - d / t).max(0.0);
                        for c in 0..4 {
                            acc[c] += p[c] * k;
                        }
                        wsum += k;
                    }
                }
                if wsum > 0.0 {
                    for a in acc.iter_mut() {
                        *a /= wsum;
                    }
                    put(px, w, x as usize, y as usize, acc);
                }
            }
        }
        let _ = value_noise;
    }
);

/// Photoshop's one-shot blurs: no dialog, a fixed small radius each.
///
/// They are the oldest filters in the program and they survive because
/// "a bit softer" is a thing people want without deciding how much.
/// Blur More is Blur about four times over.
fn fixed_blur(px: &mut [f32], w: usize, h: usize, sigma: f32) {
    // `gaussian_rgba` premultiplies for itself, as the other blurs here
    // rely on; doing it again outside would divide the edges twice.
    gaussian_rgba(px, w, h, sigma);
}

simple_filter!(
    Blur,
    "filter.blur",
    "Blur",
    "Blur",
    [],
    |px: &mut [f32], w: usize, h: usize, _v: &FilterValues| {
        fixed_blur(px, w, h, 0.6);
    }
);

simple_filter!(
    BlurMore,
    "filter.blur_more",
    "Blur More",
    "Blur",
    [],
    |px: &mut [f32], w: usize, h: usize, _v: &FilterValues| {
        fixed_blur(px, w, h, 1.7);
    }
);

simple_filter!(
    SharpenMore,
    "filter.sharpen_more",
    "Sharpen More",
    "Sharpen",
    [],
    |px: &mut [f32], w: usize, h: usize, _v: &FilterValues| {
        // The same 3x3 Laplacian as Sharpen with the centre weighted
        // harder, which is exactly what Photoshop's More variants are.
        convolve3(
            px,
            w,
            h,
            [0.0, -1.0, 0.0, -1.0, 5.0, -1.0, 0.0, -1.0, 0.0],
            0.0,
        );
        convolve3(
            px,
            w,
            h,
            [0.0, -0.5, 0.0, -0.5, 3.0, -0.5, 0.0, -0.5, 0.0],
            0.0,
        );
    }
);

/// Filter ▸ Other ▸ Custom: a 5x5 convolution the user writes themselves.
///
/// Twenty-five weights, a scale to divide the sum by and an offset to add
/// afterwards, which between them cover blur, sharpen, emboss, edge
/// detection and every hand-rolled kernel that has been passed around on
/// forums since Photoshop 3. It starts as the identity -- centre one,
/// everything else zero -- so opening it and touching nothing does
/// nothing, as it does in Photoshop.
pub struct Custom;

/// The keys of the kernel, row by row. Static so `params` can hand out
/// `&'static str` keys.
const CUSTOM_KEYS: [&str; 25] = [
    "k00", "k01", "k02", "k03", "k04", "k10", "k11", "k12", "k13", "k14", "k20", "k21", "k22",
    "k23", "k24", "k30", "k31", "k32", "k33", "k34", "k40", "k41", "k42", "k43", "k44",
];
const CUSTOM_LABELS: [&str; 25] = [
    "1,1", "1,2", "1,3", "1,4", "1,5", "2,1", "2,2", "2,3", "2,4", "2,5", "3,1", "3,2", "3,3",
    "3,4", "3,5", "4,1", "4,2", "4,3", "4,4", "4,5", "5,1", "5,2", "5,3", "5,4", "5,5",
];

impl FilterPlugin for Custom {
    fn id(&self) -> &'static str {
        "filter.custom"
    }
    fn name(&self) -> &'static str {
        "Custom"
    }
    fn category(&self) -> &'static str {
        "Other"
    }
    fn params(&self) -> Vec<FilterParam> {
        let mut out: Vec<FilterParam> = (0..25)
            .map(|i| {
                param(
                    CUSTOM_KEYS[i],
                    CUSTOM_LABELS[i],
                    -999.0,
                    999.0,
                    // The centre tap is the identity; the rest are silent.
                    if i == 12 { 1.0 } else { 0.0 },
                    "",
                )
            })
            .collect();
        out.push(param("scale", "Scale", 1.0, 999.0, 1.0, ""));
        // In Photoshop's 0..255 units, because that is what every kernel
        // anyone has ever written down assumes.
        out.push(param("offset", "Offset", -255.0, 255.0, 0.0, ""));
        out
    }

    fn apply(&self, px: &mut [f32], width: usize, height: usize, values: &FilterValues) {
        if width == 0 || height == 0 {
            return;
        }
        let mut k = [0.0f32; 25];
        for (i, key) in CUSTOM_KEYS.iter().enumerate() {
            k[i] = values.get(key);
        }
        let scale = values.get("scale").abs().max(1e-3);
        let offset = values.get("offset") / 255.0;
        let src = px.to_vec();
        for y in 0..height as i32 {
            for x in 0..width as i32 {
                let mut acc = [0.0f32; 3];
                for (i, weight) in k.iter().enumerate() {
                    if *weight == 0.0 {
                        continue;
                    }
                    let p = at(
                        &src,
                        width,
                        height,
                        x + (i % 5) as i32 - 2,
                        y + (i / 5) as i32 - 2,
                    );
                    for c in 0..3 {
                        acc[c] += p[c] * weight;
                    }
                }
                let a = at(&src, width, height, x, y)[3];
                put(
                    px,
                    width,
                    x as usize,
                    y as usize,
                    [
                        (acc[0] / scale + offset).clamp(0.0, 1.0),
                        (acc[1] / scale + offset).clamp(0.0, 1.0),
                        (acc[2] / scale + offset).clamp(0.0, 1.0),
                        a,
                    ],
                );
            }
        }
    }
}

/// Hue, saturation and brightness *as channels*.
///
/// Not an adjustment -- it changes nothing about how the image looks
/// except by lying about what its channels mean. It is a utility for
/// channel work: convert to HSB, edit the resulting "red" channel, which
/// is now hue, and convert back. Photoshop has shipped it as a plug-in
/// with no dialog since version 3; the round trip is why it has two
/// directions.
const HSB_MODES: &[&str] = &["RGB to HSB", "RGB to HSL", "HSB to RGB", "HSL to RGB"];

fn to_hs(r: f32, g: f32, b: f32, lightness: bool) -> [f32; 3] {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let c = max - min;
    let h = if c <= 1e-6 {
        0.0
    } else if max == r {
        (((g - b) / c) % 6.0) / 6.0
    } else if max == g {
        ((b - r) / c + 2.0) / 6.0
    } else {
        ((r - g) / c + 4.0) / 6.0
    };
    let h = if h < 0.0 { h + 1.0 } else { h };
    if lightness {
        let l = (max + min) / 2.0;
        let s = if l <= 0.0 || l >= 1.0 {
            0.0
        } else {
            c / (1.0 - (2.0 * l - 1.0).abs())
        };
        [h, s.clamp(0.0, 1.0), l]
    } else {
        let s = if max <= 0.0 { 0.0 } else { c / max };
        [h, s, max]
    }
}

fn from_hs(h: f32, s: f32, v: f32, lightness: bool) -> [f32; 3] {
    let (c, m) = if lightness {
        let c = (1.0 - (2.0 * v - 1.0).abs()) * s;
        (c, v - c / 2.0)
    } else {
        let c = v * s;
        (c, v - c)
    };
    let hp = (h.rem_euclid(1.0)) * 6.0;
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r, g, b) = match hp as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    [
        (r + m).clamp(0.0, 1.0),
        (g + m).clamp(0.0, 1.0),
        (b + m).clamp(0.0, 1.0),
    ]
}

simple_filter!(
    HsbHsl,
    "filter.hsb_hsl",
    "HSB/HSL",
    "Other",
    [choice("mode", "Mode", HSB_MODES, 0)],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        let _ = (w, h);
        let mode = (v.get("mode").round().max(0.0) as usize).min(3);
        let lightness = mode == 1 || mode == 3;
        let forward = mode < 2;
        for p in px.as_chunks_mut::<4>().0.iter_mut() {
            let out = if forward {
                to_hs(p[0], p[1], p[2], lightness)
            } else {
                from_hs(p[0], p[1], p[2], lightness)
            };
            p[..3].copy_from_slice(&out);
        }
    }
);

pub fn register(registry: &mut schist_plugin_api::PluginRegistry) {
    registry.register_filter(Box::new(HighPass));
    registry.register_filter(Box::new(Offset));
    registry.register_filter(Box::new(Maximum));
    registry.register_filter(Box::new(Minimum));
    registry.register_filter(Box::new(RadialBlur));
    registry.register_filter(Box::new(SurfaceBlur));
    registry.register_filter(Box::new(AverageBlur));
    registry.register_filter(Box::new(LensBlur));
    registry.register_filter(Box::new(SmartSharpen));
    registry.register_filter(Box::new(SharpenEdges));
    registry.register_filter(Box::new(Despeckle));
    registry.register_filter(Box::new(DustAndScratches));
    registry.register_filter(Box::new(Blur));
    registry.register_filter(Box::new(BlurMore));
    registry.register_filter(Box::new(SharpenMore));
    registry.register_filter(Box::new(Custom));
    registry.register_filter(Box::new(HsbHsl));
    registry.register_filter(Box::new(ReduceNoise));
}
