//! Destructive filters: blur, sharpen, noise, median.
//!
//! Every filter works on a straight-alpha f32 RGBA buffer. Blurs run in
//! premultiplied space (otherwise transparent pixels bleed their colour
//! into the result) and convert back afterwards.
//!
//! The large-kernel sweeps — the box passes behind every Gaussian, and the
//! lens blur's disc — go through `schist_fx`, which runs them on the GPU
//! when one is installed and on its own CPU reference otherwise. Anything
//! whose cost is a couple of taps per pixel stays here.

use schist_plugin_api::{FilterParam, FilterPlugin, FilterValues, PluginManifest, PluginRegistry};

pub mod camera_raw;
pub mod distort;
pub mod neural;
pub mod other;
pub mod pixelate;
pub mod render;
pub mod stylize;
pub mod util;

/// A `FilterParam` without the struct-literal noise.
pub const fn param(
    key: &'static str,
    label: &'static str,
    min: f32,
    max: f32,
    default: f32,
    suffix: &'static str,
) -> FilterParam {
    FilterParam {
        key,
        label,
        min,
        max,
        default,
        suffix,
        choices: &[],
    }
}

/// A parameter that picks from a list. The value is the index.
pub const fn choice(
    key: &'static str,
    label: &'static str,
    choices: &'static [&'static str],
    default: usize,
) -> FilterParam {
    FilterParam {
        key,
        label,
        min: 0.0,
        max: (choices.len() - 1) as f32,
        default: default as f32,
        suffix: "",
        choices,
    }
}

/// Declare a filter whose whole implementation is one closure.
///
/// Most of the set is exactly that: a name, a couple of sliders, and a
/// function over the pixel buffer. Spelling out the trait impl for each
/// would be forty lines of boilerplate apiece.
#[macro_export]
macro_rules! simple_filter {
    ($ty:ident, $id:expr, $name:expr, $category:expr, [$($param:expr),* $(,)?], $body:expr) => {
        pub struct $ty;

        impl FilterPlugin for $ty {
            fn id(&self) -> &'static str {
                $id
            }
            fn name(&self) -> &'static str {
                $name
            }
            fn category(&self) -> &'static str {
                $category
            }
            fn params(&self) -> Vec<FilterParam> {
                vec![$($param),*]
            }
            /// A filter with a spatial parameter reads that far outside
            /// the pixel it writes, so the shell knows to hand it that
            /// much surrounding image.
            fn context(&self, values: &FilterValues) -> u32 {
                let params = self.params();
                // Only genuinely spatial parameters. "amount" is
                // intensity in most filters (Add Noise, Unsharp), and
                // treating it as reach grew the buffer by up to its whole
                // slider range for no benefit.
                ["radius", "size", "distance"]
                    .iter()
                    .filter(|key| params.iter().any(|p| p.key == **key))
                    .map(|key| values.get(key).ceil().max(0.0) as u32)
                    .max()
                    .unwrap_or(0)
            }
            fn apply(
                &self,
                pixels: &mut [f32],
                width: usize,
                height: usize,
                values: &FilterValues,
            ) {
                if width == 0 || height == 0 {
                    return;
                }
                #[allow(clippy::redundant_closure_call)]
                ($body)(pixels, width, height, values)
            }
        }
    };
}

use schist_fx::{gaussian_rgba as gaussian_blur, premultiply, unpremultiply};

pub struct GaussianBlur;

impl FilterPlugin for GaussianBlur {
    /// Reads `radius` pixels outside what it writes, so a
    /// selection blur can be handed the surrounding image
    /// instead of clamping at the selection edge.
    fn context(&self, values: &FilterValues) -> u32 {
        values.get("radius").ceil().max(0.0) as u32
    }
    fn id(&self) -> &'static str {
        "filter.gaussian_blur"
    }
    fn name(&self) -> &'static str {
        "Gaussian Blur"
    }
    fn category(&self) -> &'static str {
        "Blur"
    }
    fn params(&self) -> Vec<FilterParam> {
        vec![FilterParam {
            key: "radius",
            label: "Radius",
            min: 0.0,
            max: 100.0,
            default: 4.0,
            suffix: " px",
            choices: &[],
        }]
    }
    fn apply(&self, pixels: &mut [f32], width: usize, height: usize, values: &FilterValues) {
        gaussian_blur(pixels, width, height, values.get("radius"));
    }
}

pub struct BoxBlur;

impl FilterPlugin for BoxBlur {
    /// Reads `radius` pixels outside what it writes, so a
    /// selection blur can be handed the surrounding image
    /// instead of clamping at the selection edge.
    fn context(&self, values: &FilterValues) -> u32 {
        values.get("radius").ceil().max(0.0) as u32
    }
    fn id(&self) -> &'static str {
        "filter.box_blur"
    }
    fn name(&self) -> &'static str {
        "Box Blur"
    }
    fn category(&self) -> &'static str {
        "Blur"
    }
    fn params(&self) -> Vec<FilterParam> {
        vec![FilterParam {
            key: "radius",
            label: "Radius",
            min: 0.0,
            max: 100.0,
            default: 4.0,
            suffix: " px",
            choices: &[],
        }]
    }
    fn apply(&self, pixels: &mut [f32], width: usize, height: usize, values: &FilterValues) {
        schist_fx::box_blur_rgba(pixels, width, height, values.get("radius").round() as usize);
    }
}

pub struct MotionBlur;

impl FilterPlugin for MotionBlur {
    fn id(&self) -> &'static str {
        "filter.motion_blur"
    }
    fn name(&self) -> &'static str {
        "Motion Blur"
    }
    fn category(&self) -> &'static str {
        "Blur"
    }
    fn params(&self) -> Vec<FilterParam> {
        vec![
            FilterParam {
                key: "distance",
                label: "Distance",
                min: 1.0,
                max: 200.0,
                default: 12.0,
                suffix: " px",
                choices: &[],
            },
            FilterParam {
                key: "angle",
                label: "Angle",
                min: -180.0,
                max: 180.0,
                default: 0.0,
                suffix: "°",
                choices: &[],
            },
        ]
    }
    fn apply(&self, pixels: &mut [f32], width: usize, height: usize, values: &FilterValues) {
        let distance = values.get("distance");
        if distance < 1.0 || width == 0 || height == 0 {
            return;
        }
        let angle = values.get("angle").to_radians();
        let (dx, dy) = (angle.cos(), angle.sin());
        let steps = distance.round().max(1.0) as i32;
        premultiply(pixels);
        let src = pixels.to_vec();
        for y in 0..height as i32 {
            for x in 0..width as i32 {
                let mut acc = [0.0f32; 4];
                let mut n = 0.0f32;
                for s in -steps / 2..=steps / 2 {
                    let sx = (x as f32 + dx * s as f32).round() as i32;
                    let sy = (y as f32 + dy * s as f32).round() as i32;
                    if sx < 0 || sy < 0 || sx >= width as i32 || sy >= height as i32 {
                        continue;
                    }
                    let at = (sy as usize * width + sx as usize) * 4;
                    for c in 0..4 {
                        acc[c] += src[at + c];
                    }
                    n += 1.0;
                }
                if n == 0.0 {
                    continue;
                }
                let at = (y as usize * width + x as usize) * 4;
                for c in 0..4 {
                    pixels[at + c] = acc[c] / n;
                }
            }
        }
        unpremultiply(pixels);
    }
}

pub struct Sharpen;

impl FilterPlugin for Sharpen {
    fn id(&self) -> &'static str {
        "filter.sharpen"
    }
    fn name(&self) -> &'static str {
        "Sharpen"
    }
    fn category(&self) -> &'static str {
        "Sharpen"
    }
    fn params(&self) -> Vec<FilterParam> {
        vec![FilterParam {
            key: "amount",
            label: "Amount",
            min: 0.0,
            max: 300.0,
            default: 100.0,
            suffix: "%",
            choices: &[],
        }]
    }
    fn apply(&self, pixels: &mut [f32], width: usize, height: usize, values: &FilterValues) {
        unsharp_mask(
            pixels,
            width,
            height,
            1.0,
            values.get("amount") / 100.0,
            0.0,
        );
    }
}

pub struct UnsharpMask;

impl FilterPlugin for UnsharpMask {
    fn id(&self) -> &'static str {
        "filter.unsharp_mask"
    }
    fn name(&self) -> &'static str {
        "Unsharp Mask"
    }
    fn category(&self) -> &'static str {
        "Sharpen"
    }
    fn params(&self) -> Vec<FilterParam> {
        vec![
            FilterParam {
                key: "amount",
                label: "Amount",
                min: 0.0,
                max: 500.0,
                default: 100.0,
                suffix: "%",
                choices: &[],
            },
            FilterParam {
                key: "radius",
                label: "Radius",
                min: 0.1,
                max: 50.0,
                default: 2.0,
                suffix: " px",
                choices: &[],
            },
            FilterParam {
                key: "threshold",
                label: "Threshold",
                min: 0.0,
                max: 255.0,
                default: 0.0,
                suffix: "",
                choices: &[],
            },
        ]
    }
    fn apply(&self, pixels: &mut [f32], width: usize, height: usize, values: &FilterValues) {
        unsharp_mask(
            pixels,
            width,
            height,
            values.get("radius"),
            values.get("amount") / 100.0,
            values.get("threshold") / 255.0,
        );
    }
}

/// `result = original + amount * (original - blurred)`, skipping pixels
/// whose local contrast is below `threshold`.
fn unsharp_mask(
    pixels: &mut [f32],
    width: usize,
    height: usize,
    radius: f32,
    amount: f32,
    threshold: f32,
) {
    if amount <= 0.0 || width == 0 || height == 0 {
        return;
    }
    let mut blurred = pixels.to_vec();
    if radius >= 0.5 {
        gaussian_blur(&mut blurred, width, height, radius);
    } else {
        // Sub-pixel radius: a 3x3 box stands in for the blurred reference.
        schist_fx::box_blur_rgba(&mut blurred, width, height, 1);
    }
    for (px, bl) in pixels
        .as_chunks_mut::<4>()
        .0
        .iter_mut()
        .zip(blurred.as_chunks::<4>().0.iter())
    {
        for c in 0..3 {
            let diff = px[c] - bl[c];
            if diff.abs() >= threshold {
                px[c] = (px[c] + diff * amount).clamp(0.0, 1.0);
            }
        }
    }
}

pub struct AddNoise;

impl FilterPlugin for AddNoise {
    fn id(&self) -> &'static str {
        "filter.add_noise"
    }
    fn name(&self) -> &'static str {
        "Add Noise"
    }
    fn category(&self) -> &'static str {
        "Noise"
    }
    fn params(&self) -> Vec<FilterParam> {
        vec![
            FilterParam {
                key: "amount",
                label: "Amount",
                min: 0.0,
                max: 100.0,
                default: 10.0,
                suffix: "%",
                choices: &[],
            },
            FilterParam {
                key: "monochrome",
                label: "Monochrome",
                min: 0.0,
                max: 1.0,
                default: 1.0,
                suffix: "",
                choices: &[],
            },
        ]
    }
    fn apply(&self, pixels: &mut [f32], width: usize, height: usize, values: &FilterValues) {
        let amount = values.get("amount") / 100.0;
        if amount <= 0.0 {
            return;
        }
        let mono = values.get("monochrome") >= 0.5;
        // Deterministic hash noise: same input, same output, so undo/redo
        // and re-runs are reproducible.
        let hash = |x: u32, y: u32, c: u32| -> f32 {
            let mut h = x.wrapping_mul(0x9E37_79B9)
                ^ y.wrapping_mul(0x85EB_CA6B)
                ^ c.wrapping_mul(0xC2B2_AE35);
            h ^= h >> 15;
            h = h.wrapping_mul(0x2C1B_3C6D);
            h ^= h >> 12;
            h = h.wrapping_mul(0x297A_2D39);
            h ^= h >> 15;
            (h as f32 / u32::MAX as f32) * 2.0 - 1.0
        };
        for y in 0..height {
            for x in 0..width {
                let at = (y * width + x) * 4;
                if mono {
                    let n = hash(x as u32, y as u32, 0) * amount;
                    for c in 0..3 {
                        pixels[at + c] = (pixels[at + c] + n).clamp(0.0, 1.0);
                    }
                } else {
                    for c in 0..3 {
                        let n = hash(x as u32, y as u32, c as u32) * amount;
                        pixels[at + c] = (pixels[at + c] + n).clamp(0.0, 1.0);
                    }
                }
            }
        }
    }
}

pub struct Median;

impl FilterPlugin for Median {
    fn id(&self) -> &'static str {
        "filter.median"
    }
    fn name(&self) -> &'static str {
        "Median"
    }
    fn category(&self) -> &'static str {
        "Noise"
    }
    fn params(&self) -> Vec<FilterParam> {
        vec![FilterParam {
            key: "radius",
            label: "Radius",
            min: 1.0,
            max: 10.0,
            default: 2.0,
            suffix: " px",
            choices: &[],
        }]
    }
    fn apply(&self, pixels: &mut [f32], width: usize, height: usize, values: &FilterValues) {
        let r = values.get("radius").round().clamp(1.0, 10.0) as i32;
        if width == 0 || height == 0 {
            return;
        }
        let src = pixels.to_vec();
        let mut window: Vec<f32> = Vec::with_capacity(((r * 2 + 1) * (r * 2 + 1)) as usize);
        for y in 0..height as i32 {
            for x in 0..width as i32 {
                for c in 0..4 {
                    window.clear();
                    for dy in -r..=r {
                        for dx in -r..=r {
                            let sx = (x + dx).clamp(0, width as i32 - 1) as usize;
                            let sy = (y + dy).clamp(0, height as i32 - 1) as usize;
                            window.push(src[(sy * width + sx) * 4 + c]);
                        }
                    }
                    window.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                    pixels[(y as usize * width + x as usize) * 4 + c] = window[window.len() / 2];
                }
            }
        }
    }
}

pub struct CoreFiltersPlugin;

impl PluginManifest for CoreFiltersPlugin {
    fn id(&self) -> &'static str {
        "schist.filters-core"
    }

    fn register(&self, registry: &mut PluginRegistry) {
        registry.register_filter(Box::new(GaussianBlur));
        registry.register_filter(Box::new(BoxBlur));
        registry.register_filter(Box::new(MotionBlur));
        registry.register_filter(Box::new(Sharpen));
        registry.register_filter(Box::new(UnsharpMask));
        registry.register_filter(Box::new(AddNoise));
        registry.register_filter(Box::new(Median));
        camera_raw::register(registry);
        neural::register(registry);
        distort::register(registry);
        pixelate::register(registry);
        render::register(registry);
        stylize::register(registry);
        other::register(registry);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `w`x`h` buffer with one white opaque pixel in the middle.
    fn impulse(w: usize, h: usize) -> Vec<f32> {
        let mut buf = vec![0.0f32; w * h * 4];
        let at = ((h / 2) * w + w / 2) * 4;
        buf[at..at + 4].copy_from_slice(&[1.0, 1.0, 1.0, 1.0]);
        buf
    }

    fn flat(w: usize, h: usize, v: f32) -> Vec<f32> {
        let mut buf = vec![0.0f32; w * h * 4];
        for px in buf.as_chunks_mut::<4>().0.iter_mut() {
            px.copy_from_slice(&[v, v, v, 1.0]);
        }
        buf
    }

    fn at(buf: &[f32], w: usize, x: usize, y: usize) -> [f32; 4] {
        let i = (y * w + x) * 4;
        [buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]
    }

    fn values(pairs: &[(&'static str, f32)]) -> FilterValues {
        FilterValues(pairs.to_vec())
    }

    #[test]
    fn gaussian_blur_spreads_an_impulse_and_conserves_energy() {
        let (w, h) = (33, 33);
        let mut buf = impulse(w, h);
        let before: f32 = buf.iter().skip(3).step_by(4).sum();
        GaussianBlur.apply(&mut buf, w, h, &values(&[("radius", 4.0)]));
        let after: f32 = buf.iter().skip(3).step_by(4).sum();
        assert!(at(&buf, w, 16, 16)[3] < 1.0, "centre spread out");
        assert!(
            at(&buf, w, 18, 16)[3] > 0.0,
            "energy reached the neighbours"
        );
        assert!(
            (after - before).abs() < before * 0.05,
            "alpha conserved: {before} -> {after}"
        );
    }

    #[test]
    fn zero_radius_blur_is_a_no_op() {
        let (w, h) = (8, 8);
        let mut buf = impulse(w, h);
        let original = buf.clone();
        GaussianBlur.apply(&mut buf, w, h, &values(&[("radius", 0.0)]));
        assert_eq!(buf, original);
    }

    #[test]
    fn blur_does_not_darken_edges_of_transparent_regions() {
        // A white opaque square on transparent black: blurring in straight
        // alpha would pull black into the edge.
        let (w, h) = (16, 16);
        let mut buf = vec![0.0f32; w * h * 4];
        for y in 4..12 {
            for x in 4..12 {
                let at = (y * w + x) * 4;
                buf[at..at + 4].copy_from_slice(&[1.0, 1.0, 1.0, 1.0]);
            }
        }
        GaussianBlur.apply(&mut buf, w, h, &values(&[("radius", 2.0)]));
        let edge = at(&buf, w, 3, 8);
        assert!(edge[3] > 0.0 && edge[3] < 1.0, "edge is partially covered");
        assert!(edge[0] > 0.9, "edge colour stays white, not grey: {edge:?}");
    }

    #[test]
    fn box_blur_averages_a_step_edge() {
        let (w, h) = (16, 4);
        let mut buf = vec![0.0f32; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let v = if x < 8 { 0.0 } else { 1.0 };
                let at = (y * w + x) * 4;
                buf[at..at + 4].copy_from_slice(&[v, v, v, 1.0]);
            }
        }
        BoxBlur.apply(&mut buf, w, h, &values(&[("radius", 2.0)]));
        let mid = at(&buf, w, 8, 2)[0];
        assert!(mid > 0.4 && mid < 0.9, "edge softened: {mid}");
        assert!(at(&buf, w, 0, 2)[0] < 0.05, "far side untouched");
    }

    #[test]
    fn motion_blur_smears_along_the_angle_only() {
        let (w, h) = (33, 33);
        let mut buf = impulse(w, h);
        MotionBlur.apply(
            &mut buf,
            w,
            h,
            &values(&[("distance", 10.0), ("angle", 0.0)]),
        );
        assert!(at(&buf, w, 19, 16)[3] > 0.0, "smeared horizontally");
        assert_eq!(at(&buf, w, 16, 19)[3], 0.0, "not vertically");
    }

    #[test]
    fn sharpen_increases_local_contrast() {
        let (w, h) = (16, 4);
        let mut buf = vec![0.0f32; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                // A soft ramp the filter can bite into.
                let v = (x as f32 / (w - 1) as f32).clamp(0.0, 1.0);
                let at = (y * w + x) * 4;
                buf[at..at + 4].copy_from_slice(&[v, v, v, 1.0]);
            }
        }
        let before = at(&buf, w, 2, 2)[0];
        Sharpen.apply(&mut buf, w, h, &values(&[("amount", 200.0)]));
        let after = at(&buf, w, 2, 2)[0];
        assert!(after < before, "dark side of the ramp got darker");
        assert!(
            at(&buf, w, 13, 2)[0] > 13.0 / 15.0,
            "light side got lighter"
        );
    }

    #[test]
    fn unsharp_threshold_protects_flat_areas() {
        let (w, h) = (8, 8);
        let mut buf = flat(w, h, 0.5);
        let original = buf.clone();
        UnsharpMask.apply(
            &mut buf,
            w,
            h,
            &values(&[("amount", 300.0), ("radius", 2.0), ("threshold", 20.0)]),
        );
        assert_eq!(buf, original, "flat region below threshold is untouched");
    }

    #[test]
    fn add_noise_is_deterministic_and_bounded() {
        let (w, h) = (16, 16);
        let mut a = flat(w, h, 0.5);
        let mut b = a.clone();
        let v = values(&[("amount", 20.0), ("monochrome", 1.0)]);
        AddNoise.apply(&mut a, w, h, &v);
        AddNoise.apply(&mut b, w, h, &v);
        assert_eq!(a, b, "same input gives the same noise");
        assert_ne!(a, flat(w, h, 0.5), "something changed");
        for px in a.as_chunks::<4>().0.iter() {
            assert!(px[0] >= 0.0 && px[0] <= 1.0);
            assert!((px[0] - 0.5).abs() <= 0.21, "within amount: {}", px[0]);
        }
    }

    #[test]
    fn monochrome_noise_keeps_channels_equal() {
        let (w, h) = (8, 8);
        let mut buf = flat(w, h, 0.5);
        AddNoise.apply(
            &mut buf,
            w,
            h,
            &values(&[("amount", 30.0), ("monochrome", 1.0)]),
        );
        for px in buf.as_chunks::<4>().0.iter() {
            assert!((px[0] - px[1]).abs() < 1e-6 && (px[1] - px[2]).abs() < 1e-6);
        }
    }

    #[test]
    fn median_removes_salt_and_pepper() {
        let (w, h) = (9, 9);
        let mut buf = flat(w, h, 0.5);
        // A single white speck the median should swallow.
        let at_i = ((h / 2) * w + w / 2) * 4;
        buf[at_i..at_i + 3].copy_from_slice(&[1.0, 1.0, 1.0]);
        Median.apply(&mut buf, w, h, &values(&[("radius", 1.0)]));
        assert!(
            (at(&buf, w, 4, 4)[0] - 0.5).abs() < 1e-5,
            "speck removed: {}",
            at(&buf, w, 4, 4)[0]
        );
    }

    #[test]
    fn filters_handle_tiny_and_empty_buffers() {
        let v = values(&[
            ("radius", 5.0),
            ("amount", 100.0),
            ("distance", 10.0),
            ("angle", 45.0),
            ("threshold", 0.0),
            ("monochrome", 1.0),
        ]);
        let filters: Vec<Box<dyn FilterPlugin>> = vec![
            Box::new(GaussianBlur),
            Box::new(BoxBlur),
            Box::new(MotionBlur),
            Box::new(Sharpen),
            Box::new(UnsharpMask),
            Box::new(AddNoise),
            Box::new(Median),
        ];
        for f in filters {
            let mut empty: Vec<f32> = Vec::new();
            f.apply(&mut empty, 0, 0, &v);
            let mut one = vec![0.5f32, 0.5, 0.5, 1.0];
            f.apply(&mut one, 1, 1, &v);
            assert!(
                one.iter().all(|c| c.is_finite()),
                "{} produced NaN",
                f.name()
            );
        }
    }

    #[test]
    fn every_filter_declares_usable_parameters() {
        let filters: Vec<Box<dyn FilterPlugin>> = vec![
            Box::new(GaussianBlur),
            Box::new(BoxBlur),
            Box::new(MotionBlur),
            Box::new(Sharpen),
            Box::new(UnsharpMask),
            Box::new(AddNoise),
            Box::new(Median),
        ];
        for f in filters {
            for p in f.params() {
                assert!(
                    p.min <= p.default && p.default <= p.max,
                    "{} / {}",
                    f.name(),
                    p.key
                );
                assert!(!p.label.is_empty());
            }
            assert!(!f.category().is_empty());
        }
    }
}

#[cfg(test)]
mod context_tests {
    use super::*;

    /// The buffer a filter is handed is exactly the region being
    /// filtered and the kernels clamp at its edge, so a selection blur
    /// repeated its boundary row outward instead of pulling in the real
    /// pixels just outside it. A filter that reads its neighbours has to
    /// say how far.
    #[test]
    fn filters_that_read_neighbours_advertise_their_reach() {
        let blur = GaussianBlur;
        let mut values = FilterValues::defaults(&blur.params());
        values.set("radius", 12.0);
        assert_eq!(blur.context(&values), 12);
        values.set("radius", 0.0);
        assert_eq!(blur.context(&values), 0);
    }

    /// The macro-built filters pick their reach up from whichever sizing
    /// parameter they declare.
    #[test]
    fn a_radius_parameter_implies_the_reach() {
        let registry = {
            let mut r = schist_plugin_api::PluginRegistry::new();
            CoreFiltersPlugin.register(&mut r);
            r
        };
        // Maximum takes a radius and reads that far.
        let max = registry
            .filters()
            .find(|f| f.id() == "filter.maximum")
            .expect("maximum");
        let mut values = FilterValues::defaults(&max.params());
        values.set("radius", 7.0);
        assert_eq!(max.context(&values), 7);

        // A per-pixel filter reads nothing around it.
        let invert = registry
            .filters()
            .find(|f| f.id() == "filter.invert" || f.category() == "Adjust")
            .or_else(|| registry.filters().find(|f| f.params().is_empty()));
        if let Some(f) = invert {
            let values = FilterValues::defaults(&f.params());
            assert_eq!(f.context(&values), 0, "{} should read nothing", f.id());
        }
    }
}
