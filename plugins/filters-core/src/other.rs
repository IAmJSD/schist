//! Filter ▸ Other, plus the extra Blur, Sharpen and Noise entries that did
//! not exist yet.

use crate::util::{
    at, convolve3, gaussian_rgba, luma, premultiply, put, sample, unpremultiply, value_noise,
};
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

/// What Offset leaves where the picture used to be.
const OFFSET_UNDEFINED: &[&str] = &["Set to Transparent", "Repeat Edge Pixels", "Wrap Around"];

simple_filter!(
    Offset,
    "filter.offset",
    "Offset",
    "Other",
    [
        param("x", "Horizontal", -2000.0, 2000.0, 0.0, " px"),
        param("y", "Vertical", -2000.0, 2000.0, 0.0, " px"),
        choice("undefined", "Undefined Areas", OFFSET_UNDEFINED, 2)
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        let dx = v.get("x").round() as i32;
        let dy = v.get("y").round() as i32;
        let undefined = (v.get("undefined").round().max(0.0) as usize).min(2);
        let src = px.to_vec();
        for y in 0..h as i32 {
            for x in 0..w as i32 {
                let (mut sx, mut sy) = (x - dx, y - dy);
                let outside = sx < 0 || sy < 0 || sx >= w as i32 || sy >= h as i32;
                match undefined {
                    // Transparent: the vacated strip is a hole.
                    0 if outside => {
                        put(px, w, x as usize, y as usize, [0.0; 4]);
                        continue;
                    }
                    // Repeat Edge Pixels: `at` clamps, so smearing the
                    // edge is what happens if nothing else is done.
                    1 => {}
                    // Wrap Around: the other side comes into view, which
                    // is what makes Offset the tool for checking whether
                    // a texture tiles.
                    2 => {
                        sx = sx.rem_euclid(w as i32);
                        sy = sy.rem_euclid(h as i32);
                    }
                    _ => {}
                }
                put(px, w, x as usize, y as usize, at(&src, w, h, sx, sy));
            }
        }
    }
);

/// Grey-level morphology: dilate (`max`) grows light areas, erode grows
/// dark ones. Photoshop calls them Maximum and Minimum.
///
/// `round` picks the shape of the structuring element, which is
/// Photoshop's Preserve option: a disc keeps curves curved, and a square
/// keeps corners square -- which matters, because these two filters are
/// mostly used to grow and shrink masks, and a mask of a building should
/// not come back with rounded corners.
fn morph(px: &mut [f32], w: usize, h: usize, radius: i32, take_max: bool, round: bool) {
    let src = px.to_vec();
    for y in 0..h as i32 {
        for x in 0..w as i32 {
            let mut acc = if take_max { [0.0f32; 4] } else { [1.0f32; 4] };
            for dy in -radius..=radius {
                for dx in -radius..=radius {
                    if round && dx * dx + dy * dy > radius * radius {
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
    [
        param("radius", "Radius", 1.0, 40.0, 2.0, " px"),
        choice("preserve", "Preserve", &["Squareness", "Roundness"], 1)
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        morph(
            px,
            w,
            h,
            v.get("radius").round().max(1.0) as i32,
            true,
            v.get("preserve") >= 0.5,
        );
    }
);

simple_filter!(
    Minimum,
    "filter.minimum",
    "Minimum",
    "Other",
    [
        param("radius", "Radius", 1.0, 40.0, 2.0, " px"),
        choice("preserve", "Preserve", &["Squareness", "Roundness"], 1)
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        morph(
            px,
            w,
            h,
            v.get("radius").round().max(1.0) as i32,
            false,
            v.get("preserve") >= 0.5,
        );
    }
);

simple_filter!(
    RadialBlur,
    "filter.radial_blur",
    "Radial Blur",
    "Blur",
    [
        param("amount", "Amount", 1.0, 100.0, 10.0, ""),
        choice("method", "Blur Method", &["Spin", "Zoom"], 0),
        choice("quality", "Quality", &["Draft", "Good", "Best"], 1),
        param("x", "Centre X", 0.0, 100.0, 50.0, "%"),
        param("y", "Centre Y", 0.0, 100.0, 50.0, "%")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // Average along an arc (spin) or a ray (zoom) through the centre.
        // Quality is how many samples that average takes: Photoshop's
        // Draft is visibly banded and exists because this filter used to
        // take a minute.
        let amount = v.get("amount") / 100.0;
        let spin = v.get("method") < 0.5;
        let steps = match (v.get("quality").round().max(0.0) as usize).min(2) {
            0 => 6,
            1 => 16,
            _ => 48,
        };
        let (cx, cy) = (v.get("x") / 100.0 * w as f32, v.get("y") / 100.0 * h as f32);
        premultiply(px);
        let src = px.to_vec();
        for y in 0..h {
            for x in 0..w {
                let (fx, fy) = (x as f32 + 0.5 - cx, y as f32 + 0.5 - cy);
                let r = fx.hypot(fy);
                let theta = fy.atan2(fx);
                let mut acc = [0.0f32; 4];
                for s in 0..steps {
                    let t = s as f32 / (steps - 1) as f32 - 0.5;
                    let (sx, sy) = if spin {
                        let a = theta + t * amount;
                        (cx + r * a.cos(), cy + r * a.sin())
                    } else {
                        let k = 1.0 + t * amount;
                        (cx + fx * k, cy + fy * k)
                    };
                    let p = crate::util::sample(&src, w, h, sx - 0.5, sy - 0.5);
                    for c in 0..4 {
                        acc[c] += p[c] / steps as f32;
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

/// The apertures Lens Blur can be given, as the number of blades.
///
/// A lens's iris is a ring of straight blades, and an out-of-focus
/// highlight comes out the shape of the hole they leave -- which is why
/// bokeh is hexagonal on one lens and round on another.
const IRIS_SHAPES: &[&str] = &[
    "Circle", "Triangle", "Square", "Pentagon", "Hexagon", "Heptagon", "Octagon",
];

/// The specular threshold `schist_fx`'s disc blur bakes into its own
/// highlight weighting, and therefore the default here: move the slider
/// off it and the filter takes its own path.
const FX_THRESHOLD: f32 = 0.75;

simple_filter!(
    LensBlur,
    "filter.lens_blur",
    "Lens Blur",
    "Blur",
    [
        param("radius", "Radius", 1.0, 60.0, 8.0, " px"),
        choice("shape", "Iris Shape", IRIS_SHAPES, 0),
        param("curvature", "Blade Curvature", 0.0, 100.0, 0.0, ""),
        param("rotation", "Rotation", 0.0, 360.0, 0.0, "\u{b0}"),
        param("brightness", "Specular Brightness", 0.0, 100.0, 0.0, ""),
        param("threshold", "Specular Threshold", 0.0, 100.0, 75.0, ""),
        param("noise", "Noise", 0.0, 100.0, 0.0, "")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // A flat kernel rather than a Gaussian, which is what makes
        // out-of-focus highlights come out as discs instead of smears --
        // and, at radius 60, eleven thousand taps a pixel.
        let radius = v.get("radius").round().max(1.0) as i32;
        let blades = (v.get("shape").round().max(0.0) as usize).min(IRIS_SHAPES.len() - 1);
        let curvature = v.get("curvature") / 100.0;
        let rotation = v.get("rotation").to_radians();
        let boost = v.get("brightness") / 100.0;
        let threshold = v.get("threshold") / 100.0;
        let noise = v.get("noise") / 100.0;

        // The plain round iris at the standard threshold is the one the
        // shared blur implements -- and the one the GPU knows -- so the
        // default settings stay on the fast path. Anything else is a
        // different kernel and runs here.
        if blades == 0 && curvature <= 0.0 && (threshold - FX_THRESHOLD).abs() < 0.01 {
            schist_fx::lens_blur_rgba(px, w, h, radius, boost);
        } else {
            let sides = blades + 2;
            premultiply(px);
            let src = px.to_vec();
            // The polygon, as the radius it allows at each angle: a
            // straight blade is a cosine of the angle to the nearest
            // corner, and curvature bows it back out towards a circle.
            let reach = |dx: f32, dy: f32| -> f32 {
                if blades == 0 {
                    return radius as f32;
                }
                let a = dy.atan2(dx) - rotation;
                let step = std::f32::consts::TAU / sides as f32;
                let inner = (a.rem_euclid(step) - step / 2.0).cos() / (step / 2.0).cos();
                let flat = radius as f32 / inner.max(1e-3);
                flat + (radius as f32 - flat) * curvature
            };
            for y in 0..h as i32 {
                for x in 0..w as i32 {
                    let mut acc = [0.0f32; 4];
                    let mut n = 0.0f32;
                    for dy in -radius..=radius {
                        for dx in -radius..=radius {
                            let d = (dx as f32).hypot(dy as f32);
                            if d > reach(dx as f32, dy as f32) {
                                continue;
                            }
                            let p = at(&src, w, h, x + dx, y + dy);
                            // Only what is brighter than the threshold
                            // blooms, which is what keeps the whole
                            // picture from lifting.
                            let l = luma(&p);
                            let k = if l > threshold {
                                1.0 + (l - threshold).powi(2) * boost * 60.0
                            } else {
                                1.0
                            };
                            for c in 0..4 {
                                acc[c] += p[c] * k;
                            }
                            n += k;
                        }
                    }
                    if n > 0.0 {
                        for a in acc.iter_mut() {
                            *a /= n;
                        }
                        put(px, w, x as usize, y as usize, acc);
                    }
                }
            }
            unpremultiply(px);
        }

        // Grain, added last: a lens blur is the one place a picture ends
        // up *too* clean, and Photoshop offers this for the same reason.
        if noise > 0.0 {
            for (i, p) in px.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                let (x, y) = ((i % w) as f32, (i / w) as f32);
                let n = (value_noise(x, y, 9173) - 0.5) * noise * 0.35;
                for c in p.iter_mut().take(3) {
                    *c = (*c + n).clamp(0.0, 1.0);
                }
            }
        }
    }
);

/// What Smart Sharpen is trying to undo, which decides what it
/// subtracts.
const SHARPEN_REMOVE: &[&str] = &["Gaussian Blur", "Lens Blur", "Motion Blur"];

simple_filter!(
    SmartSharpen,
    "filter.smart_sharpen",
    "Smart Sharpen",
    "Sharpen",
    [
        param("amount", "Amount", 1.0, 500.0, 100.0, "%"),
        param("radius", "Radius", 0.1, 64.0, 1.5, " px"),
        param("noise", "Reduce Noise", 0.0, 100.0, 10.0, "%"),
        choice("remove", "Remove", SHARPEN_REMOVE, 1),
        param("angle", "Angle", 0.0, 360.0, 0.0, "\u{b0}")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // Unsharp mask that leaves low-contrast detail alone, which is
        // what stops it amplifying grain -- and which subtracts the blur
        // it is actually trying to undo. Sharpening against a Gaussian
        // when the softness came from motion puts halos across the
        // direction of travel; sharpening against the *motion* does not.
        let amount = v.get("amount") / 100.0;
        let floor = v.get("noise") / 100.0 * 0.25;
        let remove = (v.get("remove").round().max(0.0) as usize).min(2);
        let angle = v.get("angle").to_radians();
        let radius = v.get("radius");
        let mut low = px.to_vec();
        match remove {
            // Gaussian: an ordinary soft focus.
            0 => gaussian_rgba(&mut low, w, h, radius),
            // Lens Blur: a flat disc, which is what defocus really is,
            // and the reason this is Photoshop's default.
            1 => schist_fx::lens_blur_rgba(&mut low, w, h, radius.round().max(1.0) as i32, 0.0),
            // Motion Blur: a line at the given angle.
            _ => {
                let steps = radius.round().max(1.0) as i32;
                let src = low.clone();
                let (dx, dy) = (angle.cos(), angle.sin());
                for y in 0..h {
                    for x in 0..w {
                        let mut acc = [0.0f32; 4];
                        let mut n = 0.0;
                        for t in -steps..=steps {
                            let p = sample(
                                &src,
                                w,
                                h,
                                x as f32 + dx * t as f32,
                                y as f32 + dy * t as f32,
                            );
                            for c in 0..4 {
                                acc[c] += p[c];
                            }
                            n += 1.0;
                        }
                        for a in acc.iter_mut() {
                            *a /= n;
                        }
                        put(&mut low, w, x, y, acc);
                    }
                }
            }
        }
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
        param("detail", "Preserve Details", 0.0, 100.0, 60.0, "%"),
        param("colour", "Reduce Colour Noise", 0.0, 100.0, 50.0, "%"),
        param("sharpen", "Sharpen Details", 0.0, 100.0, 25.0, "%"),
        param("jpeg", "Remove JPEG Artifact", 0.0, 1.0, 0.0, "")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // Bilateral again, but tuned the other way round: a small spatial
        // radius with a threshold set by how much detail to keep.
        let strength = v.get("strength");
        let detail = v.get("detail") / 100.0;
        let colour = v.get("colour") / 100.0;
        let sharpen = v.get("sharpen") / 100.0;
        let jpeg = v.get("jpeg") >= 0.5;
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

        // Colour noise is a different animal from luminance noise: the
        // eye barely resolves chroma, so it can be blurred hard without
        // the picture going soft. Doing it separately is why a photograph
        // can lose its purple speckle and keep its detail.
        if colour > 0.0 {
            let luminance: Vec<f32> = px.as_chunks::<4>().0.iter().map(|p| luma(p)).collect();
            let mut smooth = px.to_vec();
            gaussian_rgba(&mut smooth, w, h, 1.0 + colour * 4.0);
            for (i, p) in px.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                let sl = luma(&smooth[i * 4..i * 4 + 4]);
                for c in 0..3 {
                    // Take the smoothed pixel's chroma against this
                    // pixel's own luminance.
                    let chroma = smooth[i * 4 + c] - sl;
                    let target = luminance[i] + chroma;
                    p[c] = (p[c] + (target - p[c]) * colour).clamp(0.0, 1.0);
                }
            }
        }

        // JPEG leaves its damage on the 8-pixel grid and nowhere else, so
        // the check box smooths across that grid specifically. It is the
        // same trick JPEG Artifact Removal falls back to without its
        // model, at a fraction of the strength: this is a tick box on a
        // noise filter, not the filter for the job.
        if jpeg {
            let src = px.to_vec();
            for y in 0..h {
                for x in 0..w {
                    if !(x % 8 == 0 && x > 0 || y % 8 == 0 && y > 0) {
                        continue;
                    }
                    let here = at(&src, w, h, x as i32, y as i32);
                    let left = at(&src, w, h, x as i32 - 1, y as i32);
                    let above = at(&src, w, h, x as i32, y as i32 - 1);
                    let mut out = here;
                    for c in 0..3 {
                        let mean = (here[c] + left[c] + above[c]) / 3.0;
                        if (here[c] - mean).abs() < 0.1 {
                            out[c] = here[c] + (mean - here[c]) * 0.7;
                        }
                    }
                    put(px, w, x, y, out);
                }
            }
        }

        // Sharpen Details puts back what the smoothing cost, and only
        // where there was an edge to begin with -- otherwise it would
        // sharpen the noise straight back in.
        if sharpen > 0.0 {
            let mut low = px.to_vec();
            gaussian_rgba(&mut low, w, h, 1.0);
            for (p, l) in px
                .as_chunks_mut::<4>()
                .0
                .iter_mut()
                .zip(low.as_chunks::<4>().0.iter())
            {
                for c in 0..3 {
                    let d = p[c] - l[c];
                    if d.abs() > 0.02 {
                        p[c] = (p[c] + d * sharpen * 1.5).clamp(0.0, 1.0);
                    }
                }
            }
        }
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
