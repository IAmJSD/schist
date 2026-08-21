//! Shared helpers for the filter set.
//!
//! Everything here works on the same straight-alpha f32 RGBA buffer the
//! `FilterPlugin` trait hands out: `width * height * 4` floats, row major.

/// Premultiply alpha, so blurs and resamples do not pull colour out of
/// transparent pixels.
pub fn premultiply(px: &mut [f32]) {
    for p in px.as_chunks_mut::<4>().0.iter_mut() {
        p[0] *= p[3];
        p[1] *= p[3];
        p[2] *= p[3];
    }
}

pub fn unpremultiply(px: &mut [f32]) {
    for p in px.as_chunks_mut::<4>().0.iter_mut() {
        if p[3] > 1e-6 {
            p[0] /= p[3];
            p[1] /= p[3];
            p[2] /= p[3];
        } else {
            p[0] = 0.0;
            p[1] = 0.0;
            p[2] = 0.0;
        }
    }
}

#[inline]
pub fn luma(p: &[f32]) -> f32 {
    0.299 * p[0] + 0.587 * p[1] + 0.114 * p[2]
}

/// Read a pixel, clamping to the edge.
#[inline]
pub fn at(px: &[f32], w: usize, h: usize, x: i32, y: i32) -> [f32; 4] {
    let x = x.clamp(0, w as i32 - 1) as usize;
    let y = y.clamp(0, h as i32 - 1) as usize;
    let i = (y * w + x) * 4;
    [px[i], px[i + 1], px[i + 2], px[i + 3]]
}

/// Bilinear sample at a fractional position, clamping to the edge.
pub fn sample(px: &[f32], w: usize, h: usize, fx: f32, fy: f32) -> [f32; 4] {
    let x0 = fx.floor();
    let y0 = fy.floor();
    let tx = fx - x0;
    let ty = fy - y0;
    let (x0, y0) = (x0 as i32, y0 as i32);
    let mut out = [0.0f32; 4];
    for (c, o) in out.iter_mut().enumerate() {
        let a = at(px, w, h, x0, y0)[c];
        let b = at(px, w, h, x0 + 1, y0)[c];
        let d = at(px, w, h, x0, y0 + 1)[c];
        let e = at(px, w, h, x0 + 1, y0 + 1)[c];
        *o = a * (1.0 - tx) * (1.0 - ty) + b * tx * (1.0 - ty) + d * (1.0 - tx) * ty + e * tx * ty;
    }
    out
}

#[inline]
pub fn put(px: &mut [f32], w: usize, x: usize, y: usize, v: [f32; 4]) {
    let i = (y * w + x) * 4;
    px[i] = v[0];
    px[i + 1] = v[1];
    px[i + 2] = v[2];
    px[i + 3] = v[3];
}

/// Remap every pixel by where it came from, given in source coordinates.
///
/// The workhorse for the distort filters: they differ only in the mapping.
/// Sampling is done on premultiplied alpha so edges do not fringe.
pub fn warp(px: &mut [f32], w: usize, h: usize, map: impl Fn(f32, f32) -> (f32, f32)) {
    if w == 0 || h == 0 {
        return;
    }
    premultiply(px);
    let src = px.to_vec();
    for y in 0..h {
        for x in 0..w {
            let (sx, sy) = map(x as f32 + 0.5, y as f32 + 0.5);
            put(px, w, x, y, sample(&src, w, h, sx - 0.5, sy - 0.5));
        }
    }
    unpremultiply(px);
}

/// Convolve with a 3x3 kernel, leaving alpha alone.
pub fn convolve3(px: &mut [f32], w: usize, h: usize, k: [f32; 9], bias: f32) {
    if w == 0 || h == 0 {
        return;
    }
    let src = px.to_vec();
    for y in 0..h as i32 {
        for x in 0..w as i32 {
            let mut acc = [0.0f32; 3];
            for (i, weight) in k.iter().enumerate() {
                let p = at(&src, w, h, x + (i % 3) as i32 - 1, y + (i / 3) as i32 - 1);
                for c in 0..3 {
                    acc[c] += p[c] * weight;
                }
            }
            let a = at(&src, w, h, x, y)[3];
            put(
                px,
                w,
                x as usize,
                y as usize,
                [
                    (acc[0] + bias).clamp(0.0, 1.0),
                    (acc[1] + bias).clamp(0.0, 1.0),
                    (acc[2] + bias).clamp(0.0, 1.0),
                    a,
                ],
            );
        }
    }
}

/// A cheap, repeatable value-noise field.
///
/// Filters must be deterministic -- the same document and settings have to
/// give the same result twice -- so this hashes the coordinate instead of
/// drawing from a random source.
pub fn value_noise(x: f32, y: f32, seed: u32) -> f32 {
    let hash = |xi: i32, yi: i32| -> f32 {
        let mut n = (xi as u32)
            .wrapping_mul(0x9E37_79B1)
            .wrapping_add((yi as u32).wrapping_mul(0x85EB_CA6B))
            .wrapping_add(seed.wrapping_mul(0xC2B2_AE35));
        n ^= n >> 15;
        n = n.wrapping_mul(0x2545_F491);
        n ^= n >> 13;
        (n & 0xFFFF) as f32 / 65535.0
    };
    let (x0, y0) = (x.floor(), y.floor());
    let (tx, ty) = (x - x0, y - y0);
    // Smoothstep so the lattice does not show through as diamonds.
    let sx = tx * tx * (3.0 - 2.0 * tx);
    let sy = ty * ty * (3.0 - 2.0 * ty);
    let (xi, yi) = (x0 as i32, y0 as i32);
    let a = hash(xi, yi);
    let b = hash(xi + 1, yi);
    let c = hash(xi, yi + 1);
    let d = hash(xi + 1, yi + 1);
    let top = a + (b - a) * sx;
    let bottom = c + (d - c) * sx;
    top + (bottom - top) * sy
}

/// Sum of octaves of [`value_noise`], which is what makes clouds look like
/// clouds rather than blobs.
pub fn fbm(x: f32, y: f32, seed: u32, octaves: u32) -> f32 {
    let mut sum = 0.0;
    let mut amp = 0.5;
    let mut freq = 1.0;
    let mut norm = 0.0;
    for o in 0..octaves.max(1) {
        sum += value_noise(x * freq, y * freq, seed.wrapping_add(o * 7919)) * amp;
        norm += amp;
        amp *= 0.5;
        freq *= 2.0;
    }
    sum / norm.max(1e-6)
}

/// Separable box blur on premultiplied RGBA, three passes for a Gaussian.
pub fn gaussian_rgba(px: &mut [f32], w: usize, h: usize, radius: f32) {
    if radius < 0.5 || w == 0 || h == 0 {
        return;
    }
    let r = ((radius / 3.0f32.sqrt()).round() as usize).max(1);
    premultiply(px);
    let mut tmp = vec![0.0f32; px.len()];
    for _ in 0..3 {
        box_pass(px, &mut tmp, w, h, r, false);
        box_pass(&tmp, px, w, h, r, true);
    }
    unpremultiply(px);
}

fn box_pass(src: &[f32], dst: &mut [f32], w: usize, h: usize, r: usize, vertical: bool) {
    let (outer, inner) = if vertical { (w, h) } else { (h, w) };
    let stride = if vertical { w * 4 } else { 4 };
    let step = if vertical { 4 } else { w * 4 };
    let window = (r * 2 + 1) as f32;
    for o in 0..outer {
        let base = o * step;
        for i in 0..inner {
            let mut acc = [0.0f32; 4];
            for k in 0..=(r * 2) {
                let s = (i + k).saturating_sub(r).min(inner - 1);
                let at = base + s * stride;
                for c in 0..4 {
                    acc[c] += src[at + c];
                }
            }
            let at = base + i * stride;
            for c in 0..4 {
                dst[at + c] = acc[c] / window;
            }
        }
    }
}
