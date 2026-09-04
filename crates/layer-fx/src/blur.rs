//! Separable box blur, three passes, on a single-channel buffer.
//!
//! Three box passes approximate a Gaussian closely enough that the
//! difference is invisible, at a fraction of the cost -- the same trick
//! the core filters use, specialised here to one channel because every
//! effect blurs alpha rather than colour.

/// Blur `a` in place. `radius` is the Gaussian radius in pixels, on the
/// convention that the standard deviation is `radius / sqrt(3)`.
pub fn gaussian_alpha(a: &mut [f32], w: usize, h: usize, radius: f32) {
    if radius < 0.5 || w == 0 || h == 0 {
        return;
    }
    let sigma = radius / 3.0f32.sqrt();
    let mut tmp = vec![0.0f32; a.len()];
    for r in box_radii(sigma) {
        box_pass(a, &mut tmp, w, h, r, false);
        box_pass(&tmp, a, w, h, r, true);
    }
}

/// The three box radii whose passes come closest to a Gaussian of
/// `sigma`.
///
/// One radius for all three passes quantises badly — the achievable
/// sigmas are sqrt((2r+1)^2 - 1)/2, which near r = 8 step by a whole
/// pixel, so a nominal radius can land several percent wide or narrow
/// and two probes of the same effect then disagree about the scale
/// factor. Mixing two adjacent widths across the passes (the standard
/// construction) hits the target sigma to well under a percent instead.
fn box_radii(sigma: f32) -> [usize; 3] {
    const N: f32 = 3.0;
    let ideal = (12.0 * sigma * sigma / N + 1.0).sqrt();
    let mut lower = ideal.floor();
    if (lower as i32) % 2 == 0 {
        lower -= 1.0;
    }
    let lower = lower.max(1.0);
    let upper = lower + 2.0;
    // How many passes take the narrower box, from matching variances.
    let m = ((12.0 * sigma * sigma - N * lower * lower - 4.0 * N * lower - 3.0 * N)
        / (-4.0 * lower - 4.0))
        .round()
        .clamp(0.0, N) as usize;
    let (lo, hi) = (
        ((lower - 1.0) / 2.0) as usize,
        ((upper - 1.0) / 2.0) as usize,
    );
    let mut out = [hi; 3];
    for v in out.iter_mut().take(m) {
        *v = lo;
    }
    out
}

fn box_pass(src: &[f32], dst: &mut [f32], w: usize, h: usize, r: usize, vertical: bool) {
    let (outer, inner) = if vertical { (w, h) } else { (h, w) };
    let (stride, step) = if vertical { (w, 1) } else { (1, w) };
    let window = (r * 2 + 1) as f32;
    for o in 0..outer {
        let base = o * step;
        // Running sum: the window only gains and loses one sample a step.
        let mut acc = 0.0f32;
        for k in 0..=r {
            acc += src[base + k.min(inner - 1) * stride];
        }
        acc += src[base] * r as f32;
        for i in 0..inner {
            dst[base + i * stride] = acc / window;
            let add = src[base + (i + r + 1).min(inner - 1) * stride];
            let sub = src[base + i.saturating_sub(r) * stride];
            acc += add - sub;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blur_preserves_a_constant_field() {
        let mut a = vec![1.0f32; 32 * 32];
        gaussian_alpha(&mut a, 32, 32, 4.0);
        for (i, v) in a.iter().enumerate() {
            assert!((v - 1.0).abs() < 1e-3, "pixel {i} drifted to {v}");
        }
    }

    #[test]
    fn blur_spreads_a_point() {
        let (w, h) = (33usize, 33usize);
        let mut a = vec![0.0f32; w * h];
        a[16 * w + 16] = 1.0;
        gaussian_alpha(&mut a, w, h, 4.0);
        assert!(a[16 * w + 16] < 1.0, "peak should fall");
        assert!(a[16 * w + 18] > 0.0, "energy should spread sideways");
        // Total energy is conserved by a normalised blur.
        let sum: f32 = a.iter().sum();
        assert!((sum - 1.0).abs() < 0.05, "energy {sum} not conserved");
    }
}
