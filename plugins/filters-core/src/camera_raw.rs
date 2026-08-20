//! Camera Raw.
//!
//! Photoshop's Camera Raw is a whole raw-development pipeline; this is its
//! Basic and Detail panels applied to already-developed pixels, which is
//! also what "Filter ▸ Camera Raw Filter" does to a normal layer.
//!
//! The order matters and is the same one Adobe uses: white balance, then
//! exposure and the tone controls, then presence (clarity, vibrance,
//! saturation), then detail (sharpening, noise reduction), then the
//! vignette last so it darkens the finished image.

use crate::util::{at, gaussian_rgba, luma, put};
use crate::{param, simple_filter};
use photoslop_plugin_api::{FilterParam, FilterPlugin, FilterValues};

/// Smooth weighting of a value's membership of a tonal band.
///
/// Highlights and Shadows have to act on their own end of the range and
/// fade out before they reach the other, or they just become Exposure.
fn band(l: f32, centre: f32, width: f32) -> f32 {
    let t = ((l - centre) / width).clamp(-1.0, 1.0);
    let s = 1.0 - t * t;
    s * s
}

simple_filter!(
    CameraRaw,
    "filter.camera_raw",
    "Camera Raw Filter",
    "Camera Raw",
    [
        param("temperature", "Temperature", -100.0, 100.0, 0.0, ""),
        param("tint", "Tint", -100.0, 100.0, 0.0, ""),
        param("exposure", "Exposure", -5.0, 5.0, 0.0, " EV"),
        param("contrast", "Contrast", -100.0, 100.0, 0.0, ""),
        param("highlights", "Highlights", -100.0, 100.0, 0.0, ""),
        param("shadows", "Shadows", -100.0, 100.0, 0.0, ""),
        param("whites", "Whites", -100.0, 100.0, 0.0, ""),
        param("blacks", "Blacks", -100.0, 100.0, 0.0, ""),
        param("clarity", "Clarity", -100.0, 100.0, 0.0, ""),
        param("dehaze", "Dehaze", -100.0, 100.0, 0.0, ""),
        param("vibrance", "Vibrance", -100.0, 100.0, 0.0, ""),
        param("saturation", "Saturation", -100.0, 100.0, 0.0, ""),
        param("sharpening", "Sharpening", 0.0, 150.0, 0.0, ""),
        param("noise", "Noise Reduction", 0.0, 100.0, 0.0, ""),
        param("vignette", "Vignette", -100.0, 100.0, 0.0, ""),
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // ---- white balance ----
        let temp = v.get("temperature") / 100.0;
        let tint = v.get("tint") / 100.0;
        if temp != 0.0 || tint != 0.0 {
            for p in px.chunks_exact_mut(4) {
                // Warmer lifts red and drops blue; tint trades green
                // against magenta, which is the other axis of the
                // correction.
                p[0] = (p[0] * (1.0 + temp * 0.35)).clamp(0.0, 1.0);
                p[2] = (p[2] * (1.0 - temp * 0.35)).clamp(0.0, 1.0);
                p[1] = (p[1] * (1.0 - tint * 0.25)).clamp(0.0, 1.0);
                p[0] = (p[0] * (1.0 + tint * 0.12)).clamp(0.0, 1.0);
                p[2] = (p[2] * (1.0 + tint * 0.12)).clamp(0.0, 1.0);
            }
        }

        // ---- exposure and tone ----
        let exposure = 2f32.powf(v.get("exposure"));
        let contrast = v.get("contrast") / 100.0;
        let highlights = v.get("highlights") / 100.0;
        let shadows = v.get("shadows") / 100.0;
        let whites = v.get("whites") / 100.0;
        let blacks = v.get("blacks") / 100.0;
        for p in px.chunks_exact_mut(4) {
            for c in p.iter_mut().take(3) {
                *c = (*c * exposure).clamp(0.0, 1.0);
            }
            let l = luma(p);
            // Each control is a gain applied through its own band, so
            // Shadows leaves the highlights where they are and vice versa.
            let mut gain = 0.0;
            if highlights != 0.0 {
                gain += highlights * 0.5 * band(l, 0.8, 0.45);
            }
            if shadows != 0.0 {
                gain += shadows * 0.5 * band(l, 0.2, 0.45);
            }
            if whites != 0.0 {
                gain += whites * 0.35 * band(l, 1.0, 0.4);
            }
            if blacks != 0.0 {
                gain += blacks * 0.35 * band(l, 0.0, 0.4);
            }
            if gain != 0.0 {
                for c in p.iter_mut().take(3) {
                    *c = (*c + gain * (1.0 - *c).max(0.05)).clamp(0.0, 1.0);
                }
            }
            if contrast != 0.0 {
                // S-curve about mid grey.
                let k = 1.0 + contrast;
                for c in p.iter_mut().take(3) {
                    *c = ((*c - 0.5) * k + 0.5).clamp(0.0, 1.0);
                }
            }
        }

        // ---- clarity and dehaze: local contrast at two scales ----
        let clarity = v.get("clarity") / 100.0;
        let dehaze = v.get("dehaze") / 100.0;
        for (amount, radius) in [(clarity, 12.0f32), (dehaze, 48.0f32)] {
            if amount == 0.0 {
                continue;
            }
            let mut low = px.to_vec();
            gaussian_rgba(&mut low, w, h, radius);
            for (p, l) in px.chunks_exact_mut(4).zip(low.chunks_exact(4)) {
                for c in 0..3 {
                    // Midtone-weighted so clarity does not blow the
                    // highlights or crush the shadows.
                    let weight = band(l[c], 0.5, 0.75);
                    p[c] = (p[c] + (p[c] - l[c]) * amount * 1.5 * weight).clamp(0.0, 1.0);
                }
            }
        }

        // ---- presence ----
        let vibrance = v.get("vibrance") / 100.0;
        let saturation = v.get("saturation") / 100.0;
        if vibrance != 0.0 || saturation != 0.0 {
            for p in px.chunks_exact_mut(4) {
                let l = luma(p);
                let max = p[0].max(p[1]).max(p[2]);
                let min = p[0].min(p[1]).min(p[2]);
                let sat = max - min;
                // Vibrance leans on the least saturated pixels.
                let amount = saturation + vibrance * (1.0 - sat);
                let k = 1.0 + amount;
                for c in p.iter_mut().take(3) {
                    *c = (l + (*c - l) * k).clamp(0.0, 1.0);
                }
            }
        }

        // ---- detail ----
        let noise = v.get("noise") / 100.0;
        if noise > 0.0 {
            // Edge-preserving average, so grain goes and detail stays.
            let src = px.to_vec();
            let t = (0.06 + 0.12 * (1.0 - noise)).max(1e-3);
            for y in 0..h as i32 {
                for x in 0..w as i32 {
                    let centre = at(&src, w, h, x, y);
                    let mut acc = [0.0f32; 4];
                    let mut wsum = 0.0;
                    for dy in -2..=2 {
                        for dx in -2..=2 {
                            let q = at(&src, w, h, x + dx, y + dy);
                            let d = (q[0] - centre[0])
                                .abs()
                                .max((q[1] - centre[1]).abs())
                                .max((q[2] - centre[2]).abs());
                            let k = (1.0 - d / t).max(0.0);
                            for c in 0..4 {
                                acc[c] += q[c] * k;
                            }
                            wsum += k;
                        }
                    }
                    if wsum > 0.0 {
                        let mut out = centre;
                        for c in 0..3 {
                            out[c] = centre[c] + (acc[c] / wsum - centre[c]) * noise;
                        }
                        put(px, w, x as usize, y as usize, out);
                    }
                }
            }
        }
        let sharpening = v.get("sharpening") / 100.0;
        if sharpening > 0.0 {
            let mut low = px.to_vec();
            gaussian_rgba(&mut low, w, h, 1.0);
            for (p, l) in px.chunks_exact_mut(4).zip(low.chunks_exact(4)) {
                for c in 0..3 {
                    p[c] = (p[c] + (p[c] - l[c]) * sharpening).clamp(0.0, 1.0);
                }
            }
        }

        // ---- vignette, last so it darkens the finished image ----
        let vignette = v.get("vignette") / 100.0;
        if vignette != 0.0 {
            let (cx, cy) = (w as f32 / 2.0, h as f32 / 2.0);
            let max_r = cx.hypot(cy).max(1.0);
            for y in 0..h {
                for x in 0..w {
                    let d = (x as f32 + 0.5 - cx).hypot(y as f32 + 0.5 - cy) / max_r;
                    // Flat in the middle, falling off towards the corners.
                    let falloff = (d * d * d).clamp(0.0, 1.0);
                    let k = 1.0 - vignette * falloff;
                    let i = (y * w + x) * 4;
                    for c in 0..3 {
                        px[i + c] = (px[i + c] * k).clamp(0.0, 1.0);
                    }
                }
            }
        }
    }
);

pub fn register(registry: &mut photoslop_plugin_api::PluginRegistry) {
    registry.register_filter(Box::new(CameraRaw));
}
