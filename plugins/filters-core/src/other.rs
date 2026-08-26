//! Filter ▸ Other, plus the extra Blur, Sharpen and Noise entries that did
//! not exist yet.

use crate::util::{at, gaussian_rgba, premultiply, put, unpremultiply, value_noise};
use crate::{param, simple_filter};
use rayon::prelude::*;
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
///
/// The structuring element is a disc, which is not separable -- but it
/// decomposes into one horizontal line segment per row, and a 1-D
/// morphological pass over a line is O(1) per pixel with a monotonic
/// deque. So instead of scanning the whole disc per pixel (about 5000
/// taps at radius 40, single-threaded, on every slider tick of a live
/// preview) this runs one horizontal pass per distinct row half-width
/// and folds the results together: 81 passes at radius 40 rather than
/// 5000 taps per pixel, and the rows within a pass go out to rayon.
fn morph(px: &mut [f32], w: usize, h: usize, radius: i32, take_max: bool) {
    if w == 0 || h == 0 || radius <= 0 {
        return;
    }
    let src = px.to_vec();
    let pick = |a: f32, b: f32| if take_max { a.max(b) } else { a.min(b) };
    // Start from the disc's own centre row, which every pixel is in.
    let mut out = horizontal_morph(&src, w, h, radius, take_max);

    let r2 = radius * radius;
    for dy in 1..=radius {
        // The disc's half-width at this row offset.
        let hw = ((r2 - dy * dy) as f32).sqrt().floor() as i32;
        if hw < 0 {
            continue;
        }
        let band = horizontal_morph(&src, w, h, hw, take_max);
        // The same band serves +dy and -dy: the disc is symmetric.
        out.par_chunks_mut(w * 4).enumerate().for_each(|(y, row)| {
            let up = (y as i32 - dy).clamp(0, h as i32 - 1) as usize;
            let down = (y as i32 + dy).clamp(0, h as i32 - 1) as usize;
            for (x, v) in row.iter_mut().enumerate() {
                let i = x;
                *v = pick(*v, pick(band[up * w * 4 + i], band[down * w * 4 + i]));
            }
        });
    }
    px.copy_from_slice(&out);
}

/// One row-wise morphological pass with a `2 * half + 1` wide window.
///
/// A monotonic deque per channel keeps this O(1) per pixel however wide
/// the window is.
fn horizontal_morph(src: &[f32], w: usize, h: usize, half: i32, take_max: bool) -> Vec<f32> {
    let mut out = vec![0f32; w * h * 4];
    let half = half.max(0) as usize;
    out.par_chunks_mut(w * 4).enumerate().for_each(|(y, row)| {
        let base = y * w * 4;
        // Indices into the row, kept so their values are monotonic.
        let mut deque: Vec<usize> = Vec::with_capacity(2 * half + 2);
        for c in 0..4 {
            deque.clear();
            let value = |x: usize| src[base + x * 4 + c];
            let better = |a: f32, b: f32| if take_max { a >= b } else { a <= b };
            // Prime the window with everything left of the first
            // output pixel's right edge.
            let mut next = 0usize;
            for x in 0..w {
                let right = (x + half).min(w - 1);
                while next <= right {
                    while deque
                        .last()
                        .is_some_and(|&i| !better(value(i), value(next)))
                    {
                        deque.pop();
                    }
                    deque.push(next);
                    next += 1;
                }
                // Drop anything that has fallen off the left edge.
                let left = x.saturating_sub(half);
                while deque.first().is_some_and(|&i| i < left) {
                    deque.remove(0);
                }
                row[x * 4 + c] = value(deque[0]);
            }
        }
    });
    out
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
    registry.register_filter(Box::new(ReduceNoise));
}

#[cfg(test)]
mod morph_tests {
    use super::{horizontal_morph, morph};
    use crate::util::{at, put};

    /// `morph` as it was written before the decomposition: a full disc
    /// scan per pixel, about 5000 taps at radius 40.
    fn brute_force(px: &mut [f32], w: usize, h: usize, radius: i32, take_max: bool) {
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

    /// A field with isolated bright and dark specks, so a dilation and an
    /// erosion both have something to spread.
    fn field(w: usize, h: usize) -> Vec<f32> {
        let mut px = vec![0.5f32; w * h * 4];
        for (i, v) in px.iter_mut().enumerate() {
            let p = i / 4;
            let (x, y) = (p % w, p / w);
            *v = match (x * 7 + y * 13) % 11 {
                0 => 0.95,
                3 => 0.05,
                _ => 0.4 + ((x + y) % 5) as f32 * 0.05,
            };
        }
        px
    }

    #[test]
    fn the_decomposed_disc_matches_a_full_disc_scan() {
        let (w, h) = (37usize, 23usize);
        for radius in [1, 2, 3, 5, 9] {
            for take_max in [true, false] {
                let mut fast = field(w, h);
                let mut slow = fast.clone();
                morph(&mut fast, w, h, radius, take_max);
                brute_force(&mut slow, w, h, radius, take_max);
                for i in 0..fast.len() {
                    assert!(
                        (fast[i] - slow[i]).abs() < 1e-6,
                        "radius {radius}, max {take_max}, sample {i}: {} != {}",
                        fast[i],
                        slow[i]
                    );
                }
            }
        }
    }

    /// A radius wider than the image still clamps at the edges rather
    /// than reading out of bounds.
    #[test]
    fn a_radius_larger_than_the_image_is_fine() {
        let (w, h) = (5usize, 4usize);
        let mut fast = field(w, h);
        let mut slow = fast.clone();
        morph(&mut fast, w, h, 12, true);
        brute_force(&mut slow, w, h, 12, true);
        for i in 0..fast.len() {
            assert!((fast[i] - slow[i]).abs() < 1e-6, "sample {i}");
        }
    }

    /// The 1-D pass is the piece everything else is built from.
    #[test]
    fn the_row_pass_is_a_sliding_window_extreme() {
        let w = 8usize;
        let mut src = vec![0f32; w * 4];
        for (x, v) in [0.1f32, 0.9, 0.2, 0.3, 0.05, 0.7, 0.4, 0.6]
            .iter()
            .enumerate()
        {
            for c in 0..4 {
                src[x * 4 + c] = *v;
            }
        }
        let out = horizontal_morph(&src, w, 1, 1, true);
        // Each output is the max of the pixel and its neighbours,
        // clamped at the ends.
        let want = [0.9f32, 0.9, 0.9, 0.3, 0.7, 0.7, 0.7, 0.6];
        for (x, w_) in want.iter().enumerate() {
            assert!((out[x * 4] - w_).abs() < 1e-6, "at {x}: {}", out[x * 4]);
        }
    }
}
