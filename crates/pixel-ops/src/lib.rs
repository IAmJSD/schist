//! CPU reference implementation of pixel math: PSD blend modes and
//! compositing. This is the semantic contract — the future GPU path must
//! match it tile-for-tile (PLAN.md §2).
//!
//! Separable modes follow the W3C compositing spec formulas, which match
//! Photoshop for 8/16-bit documents. Non-separable modes (Hue/Saturation/
//! Color/Luminosity, Darker/Lighter Color) operate on the full color triple.
//! All math is straight-alpha f32; the general compositing equation is
//!   co·αo = Cs·αs·(1−αb) + B(Cb,Cs)·αs·αb + Cb·αb·(1−αs)

use schist_color::Rgba;
use schist_core::BlendMode;

#[inline]
fn mul(b: f32, s: f32) -> f32 {
    b * s
}

#[inline]
fn screen(b: f32, s: f32) -> f32 {
    b + s - b * s
}

#[inline]
fn hard_light(b: f32, s: f32) -> f32 {
    if s <= 0.5 {
        mul(b, 2.0 * s)
    } else {
        screen(b, 2.0 * s - 1.0)
    }
}

#[inline]
fn color_dodge(b: f32, s: f32) -> f32 {
    if b <= 0.0 {
        0.0
    } else if s >= 1.0 {
        1.0
    } else {
        (b / (1.0 - s)).min(1.0)
    }
}

#[inline]
fn color_burn(b: f32, s: f32) -> f32 {
    if b >= 1.0 {
        1.0
    } else if s <= 0.0 {
        0.0
    } else {
        1.0 - ((1.0 - b) / s).min(1.0)
    }
}

#[inline]
fn soft_light(b: f32, s: f32) -> f32 {
    if s <= 0.5 {
        b - (1.0 - 2.0 * s) * b * (1.0 - b)
    } else {
        let d = if b <= 0.25 {
            ((16.0 * b - 12.0) * b + 4.0) * b
        } else {
            b.sqrt()
        };
        b + (2.0 * s - 1.0) * (d - b)
    }
}

/// Separable blend function for one channel; `b` = backdrop, `s` = source.
#[inline]
fn separable(mode: BlendMode, b: f32, s: f32) -> f32 {
    use BlendMode::*;
    let v = match mode {
        Normal | PassThrough | Dissolve => s,
        Darken => b.min(s),
        Multiply => mul(b, s),
        ColorBurn => color_burn(b, s),
        LinearBurn => b + s - 1.0,
        Lighten => b.max(s),
        Screen => screen(b, s),
        ColorDodge => color_dodge(b, s),
        LinearDodge => b + s,
        Overlay => hard_light(s, b),
        SoftLight => soft_light(b, s),
        HardLight => hard_light(b, s),
        VividLight => {
            if s <= 0.5 {
                color_burn(b, 2.0 * s)
            } else {
                color_dodge(b, 2.0 * s - 1.0)
            }
        }
        LinearLight => b + 2.0 * s - 1.0,
        PinLight => {
            if s <= 0.5 {
                b.min(2.0 * s)
            } else {
                b.max(2.0 * s - 1.0)
            }
        }
        HardMix => {
            if b + s >= 1.0 {
                1.0
            } else {
                0.0
            }
        }
        Difference => (b - s).abs(),
        Exclusion => b + s - 2.0 * b * s,
        Subtract => b - s,
        Divide => {
            if s <= 0.0 {
                1.0
            } else {
                b / s
            }
        }
        // handled by `blend_color`
        Hue | Saturation | Color | Luminosity | DarkerColor | LighterColor => s,
    };
    v.clamp(0.0, 1.0)
}

// --- Non-separable (HSL) helpers, W3C compositing spec ---

#[inline]
fn lum(c: [f32; 3]) -> f32 {
    0.3 * c[0] + 0.59 * c[1] + 0.11 * c[2]
}

fn clip_color(c: [f32; 3]) -> [f32; 3] {
    let l = lum(c);
    let n = c[0].min(c[1]).min(c[2]);
    let x = c[0].max(c[1]).max(c[2]);
    let mut out = c;
    if n < 0.0 {
        for v in &mut out {
            *v = l + (*v - l) * l / (l - n);
        }
    }
    if x > 1.0 {
        for v in &mut out {
            *v = l + (*v - l) * (1.0 - l) / (x - l);
        }
    }
    out
}

fn set_lum(c: [f32; 3], l: f32) -> [f32; 3] {
    let d = l - lum(c);
    clip_color([c[0] + d, c[1] + d, c[2] + d])
}

fn sat(c: [f32; 3]) -> f32 {
    c[0].max(c[1]).max(c[2]) - c[0].min(c[1]).min(c[2])
}

fn set_sat(c: [f32; 3], s: f32) -> [f32; 3] {
    // Order the three channels; set max-min to s preserving mid ratio.
    let mut idx = [0usize, 1, 2];
    idx.sort_by(|&a, &b| c[a].partial_cmp(&c[b]).unwrap());
    let (imin, imid, imax) = (idx[0], idx[1], idx[2]);
    let mut out = [0.0f32; 3];
    if c[imax] > c[imin] {
        out[imid] = (c[imid] - c[imin]) * s / (c[imax] - c[imin]);
        out[imax] = s;
    }
    out
}

/// Full-color blend `B(Cb, Cs)` for any mode.
fn blend_color(mode: BlendMode, cb: [f32; 3], cs: [f32; 3]) -> [f32; 3] {
    use BlendMode::*;
    match mode {
        Hue => set_lum(set_sat(cs, sat(cb)), lum(cb)),
        Saturation => set_lum(set_sat(cb, sat(cs)), lum(cb)),
        Color => set_lum(cs, lum(cb)),
        Luminosity => set_lum(cb, lum(cs)),
        DarkerColor => {
            if lum(cs) < lum(cb) {
                cs
            } else {
                cb
            }
        }
        LighterColor => {
            if lum(cs) > lum(cb) {
                cs
            } else {
                cb
            }
        }
        _ => [
            separable(mode, cb[0], cs[0]),
            separable(mode, cb[1], cs[1]),
            separable(mode, cb[2], cs[2]),
        ],
    }
}

/// Deterministic per-pixel hash in [0,1) for Dissolve.
#[inline]
fn dissolve_hash(x: i32, y: i32) -> f32 {
    let mut h = (x as u32).wrapping_mul(0x9E37_79B9) ^ (y as u32).wrapping_mul(0x85EB_CA6B);
    h ^= h >> 16;
    h = h.wrapping_mul(0x7FEB_352D);
    h ^= h >> 15;
    h = h.wrapping_mul(0x846C_A68B);
    h ^= h >> 16;
    (h >> 8) as f32 / ((1u32 << 24) as f32)
}

/// Composite `top` over `bottom` with the given blend mode.
/// `x`, `y` are document coordinates (used only by Dissolve).
pub fn blend_pixel(mode: BlendMode, top: Rgba, bottom: Rgba, x: i32, y: i32) -> Rgba {
    if mode == BlendMode::Dissolve {
        // Dissolve: source shown opaque with probability = source alpha.
        return if top.a > dissolve_hash(x, y) {
            Rgba::new(top.r, top.g, top.b, 1.0).over(bottom)
        } else {
            bottom
        };
    }
    let a_s = top.a;
    let a_b = bottom.a;
    if a_s <= 0.0 {
        return bottom;
    }
    let cs = [top.r, top.g, top.b];
    let cb = [bottom.r, bottom.g, bottom.b];
    let bl = blend_color(mode, cb, cs);
    let a_o = a_s + a_b * (1.0 - a_s);
    if a_o <= 0.0 {
        return Rgba::TRANSPARENT;
    }
    let comp = |cs_c: f32, cb_c: f32, bl_c: f32| {
        (cs_c * a_s * (1.0 - a_b) + bl_c * a_s * a_b + cb_c * a_b * (1.0 - a_s)) / a_o
    };
    Rgba {
        r: comp(cs[0], cb[0], bl[0]),
        g: comp(cs[1], cb[1], bl[1]),
        b: comp(cs[2], cb[2], bl[2]),
        a: a_o,
    }
}

/// Blend a whole span with the mode chosen *once*, so the per-pixel loop
/// monomorphizes over a single blend function instead of re-matching an
/// enum 65,536 times. This is the compositor's hot path.
///
/// `opacity` scales the source alpha. Returns false for modes that need
/// per-pixel context (Dissolve) or full-colour math (the HSL family), which
/// the caller then handles with [`blend_pixel`].
pub fn blend_span(mode: BlendMode, top: &[f32], bottom: &mut [f32], opacity: f32) -> bool {
    use BlendMode::*;

    #[inline(always)]
    fn run<F: Fn(f32, f32) -> f32>(top: &[f32], bottom: &mut [f32], opacity: f32, blend: F) {
        for (s, d) in top.chunks_exact(4).zip(bottom.chunks_exact_mut(4)) {
            let a_s = s[3] * opacity;
            if a_s <= 0.0 {
                continue;
            }
            let a_b = d[3];
            let a_o = a_s + a_b * (1.0 - a_s);
            if a_o <= 0.0 {
                continue;
            }
            let inv = 1.0 / a_o;
            for c in 0..3 {
                let cs = s[c];
                let cb = d[c];
                let bl = blend(cb, cs).clamp(0.0, 1.0);
                d[c] = (cs * a_s * (1.0 - a_b) + bl * a_s * a_b + cb * a_b * (1.0 - a_s)) * inv;
            }
            d[3] = a_o;
        }
    }

    match mode {
        Normal | PassThrough => run(top, bottom, opacity, |_, s| s),
        Darken => run(top, bottom, opacity, |b, s| b.min(s)),
        Multiply => run(top, bottom, opacity, mul),
        ColorBurn => run(top, bottom, opacity, color_burn),
        LinearBurn => run(top, bottom, opacity, |b, s| b + s - 1.0),
        Lighten => run(top, bottom, opacity, |b, s| b.max(s)),
        Screen => run(top, bottom, opacity, screen),
        ColorDodge => run(top, bottom, opacity, color_dodge),
        LinearDodge => run(top, bottom, opacity, |b, s| b + s),
        Overlay => run(top, bottom, opacity, |b, s| hard_light(s, b)),
        SoftLight => run(top, bottom, opacity, soft_light),
        HardLight => run(top, bottom, opacity, hard_light),
        VividLight => run(top, bottom, opacity, |b, s| {
            if s <= 0.5 {
                color_burn(b, 2.0 * s)
            } else {
                color_dodge(b, 2.0 * s - 1.0)
            }
        }),
        LinearLight => run(top, bottom, opacity, |b, s| b + 2.0 * s - 1.0),
        PinLight => run(top, bottom, opacity, |b, s| {
            if s <= 0.5 {
                b.min(2.0 * s)
            } else {
                b.max(2.0 * s - 1.0)
            }
        }),
        HardMix => run(
            top,
            bottom,
            opacity,
            |b, s| if b + s >= 1.0 { 1.0 } else { 0.0 },
        ),
        Difference => run(top, bottom, opacity, |b, s| (b - s).abs()),
        Exclusion => run(top, bottom, opacity, |b, s| b + s - 2.0 * b * s),
        Subtract => run(top, bottom, opacity, |b, s| b - s),
        Divide => run(
            top,
            bottom,
            opacity,
            |b, s| if s <= 0.0 { 1.0 } else { b / s },
        ),
        // Dissolve needs pixel coordinates; the HSL modes and
        // Darker/Lighter Color need all three channels at once.
        Dissolve | Hue | Saturation | Color | Luminosity | DarkerColor | LighterColor => {
            return false
        }
    }
    true
}

/// Blend two equally-sized straight-alpha f32 RGBA buffers in place:
/// `bottom = blend(top over bottom)`. `origin` is the document-space
/// position of the buffer's first pixel (for Dissolve), `width` its row
/// length in pixels.
pub fn blend_buffer(
    mode: BlendMode,
    top: &[f32],
    bottom: &mut [f32],
    width: usize,
    origin: (i32, i32),
) {
    debug_assert_eq!(top.len(), bottom.len());
    for (i, (t, b)) in top
        .chunks_exact(4)
        .zip(bottom.chunks_exact_mut(4))
        .enumerate()
    {
        let x = origin.0 + (i % width) as i32;
        let y = origin.1 + (i / width) as i32;
        let out = blend_pixel(
            mode,
            Rgba::new(t[0], t[1], t[2], t[3]),
            Rgba::new(b[0], b[1], b[2], b[3]),
            x,
            y,
        );
        b[0] = out.r;
        b[1] = out.g;
        b[2] = out.b;
        b[3] = out.a;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use BlendMode::*;

    fn px(r: f32, g: f32, b: f32, a: f32) -> Rgba {
        Rgba::new(r, g, b, a)
    }

    fn assert_close(a: Rgba, b: Rgba, tol: f32, ctx: &str) {
        for (x, y) in [(a.r, b.r), (a.g, b.g), (a.b, b.b), (a.a, b.a)] {
            assert!((x - y).abs() <= tol, "{ctx}: {a:?} vs {b:?}");
        }
    }

    #[test]
    fn normal_opaque_replaces() {
        let out = blend_pixel(Normal, px(0.2, 0.4, 0.6, 1.0), px(0.9, 0.9, 0.9, 1.0), 0, 0);
        assert_close(out, px(0.2, 0.4, 0.6, 1.0), 1e-6, "normal");
    }

    #[test]
    fn multiply_of_halves() {
        let out = blend_pixel(
            Multiply,
            px(0.5, 0.5, 0.5, 1.0),
            px(0.5, 0.5, 0.5, 1.0),
            0,
            0,
        );
        assert_close(out, px(0.25, 0.25, 0.25, 1.0), 1e-6, "multiply");
    }

    #[test]
    fn screen_of_halves() {
        let out = blend_pixel(Screen, px(0.5, 0.5, 0.5, 1.0), px(0.5, 0.5, 0.5, 1.0), 0, 0);
        assert_close(out, px(0.75, 0.75, 0.75, 1.0), 1e-6, "screen");
    }

    #[test]
    fn blend_over_transparent_backdrop_is_source() {
        // With αb = 0 every mode degenerates to plain source-over.
        for &mode in BlendMode::layer_modes() {
            if mode == Dissolve {
                continue;
            }
            let top = px(0.3, 0.7, 0.2, 0.5);
            let out = blend_pixel(mode, top, Rgba::TRANSPARENT, 3, 5);
            assert_close(out, top, 1e-5, mode.display_name());
        }
    }

    #[test]
    fn transparent_source_is_identity() {
        for &mode in BlendMode::layer_modes() {
            let bottom = px(0.3, 0.7, 0.2, 0.8);
            let out = blend_pixel(mode, Rgba::TRANSPARENT, bottom, 1, 1);
            assert_close(out, bottom, 1e-5, mode.display_name());
        }
    }

    #[test]
    fn difference_symmetric() {
        let a = px(0.8, 0.2, 0.5, 1.0);
        let b = px(0.3, 0.6, 0.5, 1.0);
        let ab = blend_pixel(Difference, a, b, 0, 0);
        let ba = blend_pixel(Difference, b, a, 0, 0);
        assert_close(ab, ba, 1e-6, "difference symmetry");
        assert_close(ab, px(0.5, 0.4, 0.0, 1.0), 1e-6, "difference value");
    }

    #[test]
    fn hsl_color_mode_takes_hue_sat_keeps_lum() {
        let top = px(1.0, 0.0, 0.0, 1.0); // red
        let bottom = px(0.5, 0.5, 0.5, 1.0); // gray
        let out = blend_pixel(Color, top, bottom, 0, 0);
        // Luminosity preserved from backdrop.
        assert!((lum([out.r, out.g, out.b]) - 0.5).abs() < 1e-3, "{out:?}");
        assert!(out.r > out.g && out.r > out.b, "hue applied: {out:?}");
    }

    #[test]
    fn hard_mix_posterizes() {
        let out = blend_pixel(
            HardMix,
            px(0.6, 0.1, 0.5, 1.0),
            px(0.6, 0.1, 0.5, 1.0),
            0,
            0,
        );
        assert_close(out, px(1.0, 0.0, 1.0, 1.0), 1e-6, "hard mix");
    }

    #[test]
    fn dissolve_deterministic_and_binary() {
        let top = px(1.0, 0.0, 0.0, 0.5);
        let bottom = px(0.0, 0.0, 1.0, 1.0);
        let a = blend_pixel(Dissolve, top, bottom, 10, 20);
        let b = blend_pixel(Dissolve, top, bottom, 10, 20);
        assert_eq!(a, b, "deterministic");
        assert!(
            a == px(1.0, 0.0, 0.0, 1.0) || a == bottom,
            "binary outcome: {a:?}"
        );
        // Roughly half of a large region should show the source.
        let mut hits = 0;
        for y in 0..100 {
            for x in 0..100 {
                if blend_pixel(Dissolve, top, bottom, x, y).r > 0.5 {
                    hits += 1;
                }
            }
        }
        assert!((3500..6500).contains(&hits), "dissolve density {hits}");
    }

    #[test]
    fn color_dodge_burn_extremes() {
        assert_eq!(separable(ColorDodge, 0.5, 1.0), 1.0);
        assert_eq!(separable(ColorDodge, 0.0, 0.7), 0.0);
        assert_eq!(separable(ColorBurn, 1.0, 0.3), 1.0);
        assert_eq!(separable(ColorBurn, 0.5, 0.0), 0.0);
    }

    #[test]
    fn buffer_blend_matches_pixel_blend() {
        let top = [0.5f32, 0.2, 0.8, 0.9, 0.1, 0.9, 0.3, 0.4];
        let mut bottom = [0.3f32, 0.3, 0.3, 1.0, 0.6, 0.6, 0.6, 0.5];
        let expect0 = blend_pixel(
            Overlay,
            px(0.5, 0.2, 0.8, 0.9),
            px(0.3, 0.3, 0.3, 1.0),
            0,
            0,
        );
        let expect1 = blend_pixel(
            Overlay,
            px(0.1, 0.9, 0.3, 0.4),
            px(0.6, 0.6, 0.6, 0.5),
            1,
            0,
        );
        blend_buffer(Overlay, &top, &mut bottom, 2, (0, 0));
        assert_close(
            px(bottom[0], bottom[1], bottom[2], bottom[3]),
            expect0,
            1e-6,
            "px0",
        );
        assert_close(
            px(bottom[4], bottom[5], bottom[6], bottom[7]),
            expect1,
            1e-6,
            "px1",
        );
    }
}

#[cfg(test)]
mod span_tests {
    use super::*;
    use BlendMode::*;

    /// The span path must agree with the per-pixel reference for every
    /// mode it claims to handle — it is an optimization, not a variant.
    #[test]
    fn span_matches_reference_for_every_supported_mode() {
        let samples = [
            ([0.2f32, 0.7, 0.4, 1.0], [0.8f32, 0.1, 0.6, 1.0]),
            ([0.0, 0.0, 0.0, 0.5], [1.0, 1.0, 1.0, 1.0]),
            ([1.0, 0.5, 0.25, 0.75], [0.3, 0.9, 0.1, 0.4]),
            ([0.5, 0.5, 0.5, 0.0], [0.5, 0.5, 0.5, 1.0]),
            ([0.9, 0.2, 0.5, 1.0], [0.0, 0.0, 0.0, 0.0]),
        ];
        for &mode in BlendMode::layer_modes() {
            for opacity in [1.0f32, 0.5, 0.0] {
                for (top, bottom) in samples {
                    let mut span_out = bottom.to_vec();
                    let handled = blend_span(mode, &top, &mut span_out, opacity);
                    if !handled {
                        continue;
                    }
                    let expected = blend_pixel(
                        mode,
                        Rgba::new(top[0], top[1], top[2], top[3] * opacity),
                        Rgba::new(bottom[0], bottom[1], bottom[2], bottom[3]),
                        0,
                        0,
                    );
                    for (i, want) in [expected.r, expected.g, expected.b, expected.a]
                        .into_iter()
                        .enumerate()
                    {
                        assert!(
                            (span_out[i] - want).abs() < 1e-4,
                            "{} opacity={opacity} chan {i}: {} vs {want}",
                            mode.display_name(),
                            span_out[i]
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn context_dependent_modes_are_declined() {
        let top = [0.5f32, 0.5, 0.5, 1.0];
        for mode in [
            Dissolve,
            Hue,
            Saturation,
            Color,
            Luminosity,
            DarkerColor,
            LighterColor,
        ] {
            let mut out = vec![0.2f32, 0.2, 0.2, 1.0];
            let before = out.clone();
            assert!(!blend_span(mode, &top, &mut out, 1.0), "{mode:?}");
            assert_eq!(out, before, "declined modes must not touch the buffer");
        }
    }
}
