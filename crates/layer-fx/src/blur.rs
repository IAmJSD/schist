//! Separable box blur, three passes, on a single-channel buffer.
//!
//! Three box passes approximate a Gaussian closely enough that the
//! difference is invisible, at a fraction of the cost -- the same trick
//! the core filters use, specialised here to one channel because every
//! effect blurs alpha rather than colour.

/// Blur `a` in place. `radius` is the Gaussian radius in pixels.
pub fn gaussian_alpha(a: &mut [f32], w: usize, h: usize, radius: f32) {
    if radius < 0.5 || w == 0 || h == 0 {
        return;
    }
    let r = ((radius / 3.0f32.sqrt()).round() as usize).max(1);
    let mut tmp = vec![0.0f32; a.len()];
    for _ in 0..3 {
        box_pass(a, &mut tmp, w, h, r, false);
        box_pass(&tmp, a, w, h, r, true);
    }
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
