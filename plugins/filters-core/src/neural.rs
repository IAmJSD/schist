//! Photoshop's Neural Filters.
//!
//! Two of these run a real network. Super Zoom uses `detail.onnx`, a small
//! residual CNN trained for this application (`tools/train/detail.py`)
//! and shipped inside the binary; Style Transfer uses the fast
//! neural-style networks from the ONNX Model Zoo, downloaded on demand.
//! Both run through `photoslop-neural`, which is `tract` -- pure Rust, so
//! there is no runtime to install.
//!
//! The rest are signal processing, and the distinction is worth stating
//! plainly rather than leaving to be discovered: Skin Smoothing does not
//! know what a face is, it does frequency separation on pixels whose
//! colour falls in the skin-tone range; Colorize does not know that grass
//! is green, it maps luminance through a ramp. On the material each is
//! aimed at they do a recognisable version of the job, and they fail
//! predictably rather than surprisingly.
//!
//! Every model-backed filter also works without its model, falling back to
//! the classical path and saying so in its dialog. Nothing here is a stub
//! that stops working when a download fails.

/// Copy the RGB of a filter buffer out, run `f` on it, and blend the
/// result back. Models work on RGB; the filter buffer is RGBA.
fn through_rgb(px: &mut [f32], f: impl FnOnce(&mut Vec<f32>)) {
    let n = px.len() / 4;
    let mut rgb = Vec::with_capacity(n * 3);
    for p in px.chunks_exact(4) {
        rgb.extend_from_slice(&p[..3]);
    }
    f(&mut rgb);
    for (i, p) in px.chunks_exact_mut(4).enumerate() {
        p[..3].copy_from_slice(&rgb[i * 3..i * 3 + 3]);
    }
}

/// The note a model-backed filter shows in its dialog.
fn model_note(id: &str, fallback: &str) -> Option<String> {
    match photoslop_neural::spec(id) {
        Some(spec) if photoslop_neural::installed(id) => {
            Some(format!("Using {} ({}).", spec.name, spec.license))
        }
        Some(spec) => Some(format!(
            "{} is not installed \u{2014} {fallback} Get it from \
             Filter \u{25b8} Neural Filters \u{25b8} Manage Models.",
            spec.name
        )),
        None => None,
    }
}

use crate::util::{at, gaussian_rgba, luma, put, sample};
use crate::{choice, param, simple_filter};
use photoslop_plugin_api::{FilterParam, FilterPlugin, FilterValues};

/// How much a colour looks like skin, 0..=1.
///
/// Skin tones sit in a narrow wedge: red leads, green follows, blue
/// trails, and the whole thing is reasonably bright and not very
/// saturated. That is enough to separate a face from a blue shirt, which
/// is the separation this filter needs.
fn skinness(p: &[f32]) -> f32 {
    let (r, g, b) = (p[0], p[1], p[2]);
    if r <= g || g < b || r < 0.2 {
        return 0.0;
    }
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let sat = if max > 0.0 { (max - min) / max } else { 0.0 };
    // Within the wedge, prefer moderate saturation and mid-to-high
    // brightness, falling off smoothly at the edges so there is no seam.
    let sat_fit = 1.0 - ((sat - 0.3).abs() / 0.35).clamp(0.0, 1.0);
    let lum_fit = 1.0 - ((luma(p) - 0.55).abs() / 0.45).clamp(0.0, 1.0);
    let hue_fit = ((r - b) / r.max(1e-4)).clamp(0.0, 1.0);
    (sat_fit * lum_fit * hue_fit).clamp(0.0, 1.0)
}

simple_filter!(
    SkinSmoothing,
    "filter.neural.skin_smoothing",
    "Skin Smoothing",
    "Neural Filters",
    [
        param("blur", "Smoothness", 0.0, 100.0, 50.0, ""),
        param("detail", "Keep Detail", 0.0, 100.0, 40.0, "")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // Frequency separation: smooth the colour, keep the texture, and
        // mix the texture back at "Keep Detail". Blended only where the
        // pixel looks like skin, so eyes and lips stay sharp.
        let amount = v.get("blur") / 100.0;
        let detail = v.get("detail") / 100.0;
        if amount <= 0.0 {
            return;
        }
        let src = px.to_vec();
        let mut low = px.to_vec();
        gaussian_rgba(&mut low, w, h, 3.0 + amount * 9.0);
        for i in 0..px.len() / 4 {
            let p = &src[i * 4..i * 4 + 4];
            let s = skinness(p) * amount;
            if s <= 0.0 {
                continue;
            }
            for c in 0..3 {
                let smoothed = low[i * 4 + c];
                let texture = p[c] - smoothed;
                let target = smoothed + texture * detail;
                px[i * 4 + c] = (p[c] + (target - p[c]) * s).clamp(0.0, 1.0);
            }
        }
    }
);

simple_filter!(
    JpegArtifactRemoval,
    "filter.neural.jpeg_artifacts",
    "JPEG Artifact Removal",
    "Neural Filters",
    [param("strength", "Strength", 0.0, 100.0, 60.0, "")],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // JPEG artefacts are blocky: discontinuities that land on the 8px
        // grid and nowhere else. So smooth *across the block boundaries*
        // specifically, which leaves real edges alone wherever they
        // happen to fall.
        let strength = v.get("strength") / 100.0;
        if strength <= 0.0 {
            return;
        }
        let src = px.to_vec();
        for y in 0..h {
            for x in 0..w {
                let on_v = x % 8 == 0 && x > 0;
                let on_h = y % 8 == 0 && y > 0;
                if !on_v && !on_h {
                    continue;
                }
                let here = at(&src, w, h, x as i32, y as i32);
                let left = at(&src, w, h, x as i32 - 1, y as i32);
                let above = at(&src, w, h, x as i32, y as i32 - 1);
                let mut out = here;
                for (c, o) in out.iter_mut().enumerate().take(3) {
                    let mut acc = 0.0;
                    let mut n = 0.0;
                    if on_v {
                        acc += left[c] + here[c];
                        n += 2.0;
                    }
                    if on_h {
                        acc += above[c] + here[c];
                        n += 2.0;
                    }
                    let mean = acc / n;
                    // Only pull towards the mean when the step is small
                    // enough to be an artefact rather than an edge.
                    if (*o - mean).abs() < 0.12 {
                        *o += (mean - *o) * strength;
                    }
                }
                put(px, w, x, y, out);
            }
        }
        // A gentle ringing clean-up inside the blocks.
        let mut low = px.to_vec();
        gaussian_rgba(&mut low, w, h, 0.8);
        for (p, l) in px.chunks_exact_mut(4).zip(low.chunks_exact(4)) {
            for c in 0..3 {
                if (p[c] - l[c]).abs() < 0.05 {
                    p[c] += (l[c] - p[c]) * strength * 0.5;
                }
            }
        }
    }
);

simple_filter!(
    Colorize,
    "filter.neural.colorize",
    "Colorize",
    "Neural Filters",
    [
        param("warmth", "Warmth", -100.0, 100.0, 20.0, ""),
        param("strength", "Strength", 0.0, 100.0, 70.0, "")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // Maps luminance through a ramp: cool in the shadows, warm in the
        // highlights, which is what most natural scenes actually do. It
        // does not recognise objects, so it will not make grass green.
        let _ = (w, h);
        let warmth = v.get("warmth") / 100.0;
        let strength = v.get("strength") / 100.0;
        for p in px.chunks_exact_mut(4) {
            let l = luma(p);
            // Shadows towards blue, highlights towards amber.
            let t = l * 2.0 - 1.0;
            let target = [
                (l + t * 0.10 * (1.0 + warmth)).clamp(0.0, 1.0),
                (l + t * 0.03).clamp(0.0, 1.0),
                (l - t * 0.10 * (1.0 + warmth)).clamp(0.0, 1.0),
            ];
            for c in 0..3 {
                p[c] = (p[c] + (target[c] - p[c]) * strength).clamp(0.0, 1.0);
            }
        }
    }
);

/// Super Zoom: restore the detail an enlargement loses.
///
/// The one filter here whose network ships with the application. A filter
/// cannot resize its own buffer, so this is the second half of an
/// upscale -- enlarge with Image Size, then run this to put the high
/// frequencies back.
pub struct SuperZoom;

impl FilterPlugin for SuperZoom {
    fn id(&self) -> &'static str {
        "filter.neural.super_zoom"
    }
    fn name(&self) -> &'static str {
        "Super Zoom"
    }
    fn category(&self) -> &'static str {
        "Neural Filters"
    }
    fn params(&self) -> Vec<FilterParam> {
        vec![param("detail", "Detail", 0.0, 100.0, 60.0, "")]
    }

    fn info(&self) -> Option<String> {
        model_note("detail", "using edge-directed sharpening instead.")
    }

    fn apply(&self, px: &mut [f32], width: usize, height: usize, values: &FilterValues) {
        if width == 0 || height == 0 {
            return;
        }
        let detail = (values.get("detail") / 100.0).clamp(0.0, 1.0);
        if detail <= 0.0 {
            return;
        }
        if let Some(model) = photoslop_neural::get("detail") {
            through_rgb(px, |rgb| {
                photoslop_neural::run_tiled(&model, rgb, width, height, detail);
            });
            return;
        }
        edge_directed_sharpen(px, width, height, detail);
    }
}

/// The fallback: sharpen along the gradient rather than across it, which
/// avoids the halos plain sharpening leaves on an already-soft image.
fn edge_directed_sharpen(px: &mut [f32], w: usize, h: usize, detail: f32) {
    let src = px.to_vec();
    for y in 0..h as i32 {
        for x in 0..w as i32 {
            let c0 = at(&src, w, h, x, y);
            let gx = luma(&at(&src, w, h, x + 1, y)) - luma(&at(&src, w, h, x - 1, y));
            let gy = luma(&at(&src, w, h, x, y + 1)) - luma(&at(&src, w, h, x, y - 1));
            let mag = gx.hypot(gy);
            if mag < 1e-4 {
                continue;
            }
            let (ux, uy) = (gx / mag, gy / mag);
            let a = sample(&src, w, h, x as f32 - ux, y as f32 - uy);
            let b = sample(&src, w, h, x as f32 + ux, y as f32 + uy);
            let mut out = c0;
            for c in 0..3 {
                let mid = (a[c] + b[c]) / 2.0;
                out[c] = (c0[c] + (c0[c] - mid) * detail * 1.5).clamp(0.0, 1.0);
            }
            put(px, w, x as usize, y as usize, out);
        }
    }
}

/// The styles this build knows about, in catalogue order.
const STYLES: &[&str] = &["Mosaic", "Candy", "Udnie"];
const STYLE_IDS: &[&str] = &["style-mosaic", "style-candy", "style-udnie"];

/// Style Transfer: repaint the image in a learned style.
///
/// This is the one that genuinely needs a network -- there is no
/// signal-processing stand-in for a brushstroke. Without the model it does
/// the colour half only, which is honest but is not the same thing.
pub struct StyleTransfer;

impl FilterPlugin for StyleTransfer {
    fn id(&self) -> &'static str {
        "filter.neural.style_transfer"
    }
    fn name(&self) -> &'static str {
        "Style Transfer"
    }
    fn category(&self) -> &'static str {
        "Neural Filters"
    }
    fn params(&self) -> Vec<FilterParam> {
        vec![
            choice("style", "Style", STYLES, 0),
            param("strength", "Strength", 0.0, 100.0, 100.0, ""),
        ]
    }

    fn info(&self) -> Option<String> {
        // Report on whichever styles are present, since they install
        // separately.
        let ready: Vec<&str> = STYLE_IDS
            .iter()
            .enumerate()
            .filter(|(_, id)| photoslop_neural::installed(id))
            .map(|(i, _)| STYLES[i])
            .collect();
        Some(if ready.is_empty() {
            "No style models installed \u{2014} transferring colour only. \
             Get them from Filter \u{25b8} Neural Filters \u{25b8} Manage Models."
                .to_string()
        } else {
            format!(
                "Installed: {} (ONNX Model Zoo, Apache-2.0).",
                ready.join(", ")
            )
        })
    }

    fn apply(&self, px: &mut [f32], width: usize, height: usize, values: &FilterValues) {
        if width == 0 || height == 0 {
            return;
        }
        let strength = (values.get("strength") / 100.0).clamp(0.0, 1.0);
        if strength <= 0.0 {
            return;
        }
        let pick = (values.get("style").round().max(0.0) as usize).min(STYLE_IDS.len() - 1);
        if let Some(model) = photoslop_neural::get(STYLE_IDS[pick]) {
            through_rgb(px, |rgb| {
                photoslop_neural::run_tiled(&model, rgb, width, height, strength);
            });
            return;
        }
        // Colour-only fallback: push the image towards the style's
        // dominant hue. It is not style transfer and does not pretend to
        // be; `info` says so.
        let hue = [30.0f32, 340.0, 210.0][pick.min(2)].to_radians();
        colour_shift(px, hue, strength * 0.6);
    }
}

/// Push an image's mean chroma towards a hue.
fn colour_shift(px: &mut [f32], hue: f32, strength: f32) {
    let n = (px.len() / 4).max(1) as f32;
    let (mut ma, mut mb) = (0.0f32, 0.0f32);
    for p in px.chunks_exact(4) {
        let l = luma(p);
        ma += p[0] - l;
        mb += p[2] - l;
    }
    ma /= n;
    mb /= n;
    let (ta, tb) = (hue.cos() * 0.18, hue.sin() * 0.18);
    for p in px.chunks_exact_mut(4) {
        let l = luma(p);
        let (ca, cb) = (p[0] - l, p[2] - l);
        let target = [
            (l + ca - ma + ta).clamp(0.0, 1.0),
            (l - ((ca - ma + ta) + (cb - mb + tb)) * 0.3).clamp(0.0, 1.0),
            (l + cb - mb + tb).clamp(0.0, 1.0),
        ];
        for c in 0..3 {
            p[c] += (target[c] - p[c]) * strength;
        }
    }
}

simple_filter!(
    ColorTransfer,
    "filter.neural.color_transfer",
    "Color Transfer",
    "Neural Filters",
    [
        param("hue", "Target Hue", 0.0, 360.0, 30.0, "\u{b0}"),
        param("strength", "Strength", 0.0, 100.0, 60.0, ""),
        param("contrast", "Match Contrast", 0.0, 100.0, 50.0, "")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // Reinhard-style colour transfer, aimed at a hue rather than a
        // reference image: shift the image's mean chroma towards the
        // target and optionally normalise its spread. This is what Style
        // Transfer's colour half does; the brushstroke half needs a model.
        let _ = (w, h);
        let strength = v.get("strength") / 100.0;
        let match_contrast = v.get("contrast") / 100.0;
        let hue = v.get("hue").to_radians();
        if strength <= 0.0 {
            return;
        }
        // Mean and spread of luminance, and mean chroma.
        let n = (px.len() / 4).max(1) as f32;
        let (mut mean_l, mut mean_a, mut mean_b) = (0.0f32, 0.0f32, 0.0f32);
        for p in px.chunks_exact(4) {
            let l = luma(p);
            mean_l += l;
            mean_a += p[0] - l;
            mean_b += p[2] - l;
        }
        mean_l /= n;
        mean_a /= n;
        mean_b /= n;
        let mut var_l = 0.0f32;
        for p in px.chunks_exact(4) {
            var_l += (luma(p) - mean_l).powi(2);
        }
        let sd_l = (var_l / n).sqrt().max(1e-4);
        // The target chroma direction.
        let (ta, tb) = (hue.cos() * 0.18, hue.sin() * 0.18);
        let gain = 1.0 + match_contrast * (0.25 / sd_l - 1.0).clamp(-0.5, 0.5);
        for p in px.chunks_exact_mut(4) {
            let l = luma(p);
            let (ca, cb) = (p[0] - l, p[2] - l);
            let l2 = mean_l + (l - mean_l) * gain;
            let na = ca - mean_a + ta;
            let nb = cb - mean_b + tb;
            let target = [
                (l2 + na).clamp(0.0, 1.0),
                (l2 - (na + nb) * 0.3).clamp(0.0, 1.0),
                (l2 + nb).clamp(0.0, 1.0),
            ];
            for c in 0..3 {
                p[c] = (p[c] + (target[c] - p[c]) * strength).clamp(0.0, 1.0);
            }
        }
    }
);

simple_filter!(
    DepthBlur,
    "filter.neural.depth_blur",
    "Depth Blur",
    "Neural Filters",
    [
        param("focus", "Focal Distance", 0.0, 100.0, 50.0, ""),
        param("blur", "Blur Strength", 0.0, 100.0, 50.0, "")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // Without depth estimation, the stand-in is local sharpness:
        // areas that are already detailed read as near, flat areas as
        // far. Blur is applied by how far a region's own acuity sits from
        // the focal setting. It is a photographic effect rather than a
        // depth map, and it behaves like one.
        let focus = v.get("focus") / 100.0;
        let strength = v.get("blur") / 100.0;
        if strength <= 0.0 {
            return;
        }
        let src = px.to_vec();
        // Local detail: how far each pixel is from a blurred copy.
        let mut low = px.to_vec();
        gaussian_rgba(&mut low, w, h, 6.0);
        let mut acuity = vec![0.0f32; w * h];
        for i in 0..w * h {
            let d = (0..3)
                .map(|c| (src[i * 4 + c] - low[i * 4 + c]).abs())
                .fold(0.0f32, f32::max);
            acuity[i] = (d * 12.0).clamp(0.0, 1.0);
        }
        // Smooth it, so the blur varies over regions rather than pixels.
        let mut plane: Vec<f32> = acuity.iter().flat_map(|a| [*a, *a, *a, 1.0]).collect();
        gaussian_rgba(&mut plane, w, h, 20.0);
        let mut blurred = src.clone();
        gaussian_rgba(&mut blurred, w, h, 2.0 + strength * 14.0);
        for i in 0..w * h {
            let near = plane[i * 4];
            // How out of focus this region is.
            let k = ((near - focus).abs() * 2.0).clamp(0.0, 1.0) * strength;
            for c in 0..3 {
                px[i * 4 + c] = src[i * 4 + c] + (blurred[i * 4 + c] - src[i * 4 + c]) * k;
            }
        }
    }
);

pub fn register(registry: &mut photoslop_plugin_api::PluginRegistry) {
    registry.register_filter(Box::new(SkinSmoothing));
    registry.register_filter(Box::new(JpegArtifactRemoval));
    registry.register_filter(Box::new(Colorize));
    registry.register_filter(Box::new(SuperZoom));
    registry.register_filter(Box::new(StyleTransfer));
    registry.register_filter(Box::new(ColorTransfer));
    registry.register_filter(Box::new(DepthBlur));
}
