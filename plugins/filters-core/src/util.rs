//! Shared helpers for the filter set.
//!
//! Everything here works on the same straight-alpha f32 RGBA buffer the
//! `FilterPlugin` trait hands out: `width * height * 4` floats, row major.

/// Premultiplied-alpha conversion and the separable blur live in
/// `schist_fx`, which is where the GPU seam is; re-exported so the filter
/// modules keep a single import.
pub use schist_fx::{gaussian_rgba, premultiply, unpremultiply};

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

/// The luminance of every pixel, which is what most of the Filter Gallery
/// works on: the gallery's effects are about *drawing* an image again --
/// in charcoal, in ink, in torn paper -- and a drawing is a tone map with
/// a technique applied to it.
pub fn luma_map(px: &[f32], w: usize, h: usize) -> Vec<f32> {
    let _ = (w, h);
    px.as_chunks::<4>().0.iter().map(|p| luma(p)).collect()
}

/// Write a single-channel result back over the colour, keeping alpha.
///
/// `tint` mixes the original colour back in: 0 leaves the result grey,
/// which is what the Sketch filters want, and 1 keeps the hue of the
/// photograph under the new tone, which is what the Artistic ones do.
pub fn from_luma(px: &mut [f32], plane: &[f32], tint: f32) {
    for (i, p) in px.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let v = plane[i].clamp(0.0, 1.0);
        if tint <= 0.0 {
            p[0] = v;
            p[1] = v;
            p[2] = v;
            continue;
        }
        // Keep the pixel's own chroma against the new luminance, so a
        // yellow flower stays yellow while its tone is redrawn.
        let l = luma(p).max(1e-4);
        for channel in p.iter_mut().take(3) {
            let coloured = (*channel / l * v).clamp(0.0, 1.0);
            *channel = v + (coloured - v) * tint;
        }
    }
}

/// Blur a single-channel plane. The colour blur premultiplies and works
/// on four channels; a tone map needs neither.
pub fn blur_plane(plane: &mut [f32], w: usize, h: usize, sigma: f32) {
    let mut rgba: Vec<f32> = plane.iter().flat_map(|v| [*v, *v, *v, 1.0]).collect();
    gaussian_rgba(&mut rgba, w, h, sigma);
    for (i, p) in rgba.as_chunks::<4>().0.iter().enumerate() {
        plane[i] = p[0];
    }
}

/// Sobel gradient of a plane: (dx, dy) per pixel.
pub fn gradient(plane: &[f32], w: usize, h: usize) -> Vec<(f32, f32)> {
    let get = |x: i32, y: i32| -> f32 {
        let x = x.clamp(0, w as i32 - 1) as usize;
        let y = y.clamp(0, h as i32 - 1) as usize;
        plane[y * w + x]
    };
    let mut out = Vec::with_capacity(w * h);
    for y in 0..h as i32 {
        for x in 0..w as i32 {
            let gx = get(x + 1, y - 1) + 2.0 * get(x + 1, y) + get(x + 1, y + 1)
                - get(x - 1, y - 1)
                - 2.0 * get(x - 1, y)
                - get(x - 1, y + 1);
            let gy = get(x - 1, y + 1) + 2.0 * get(x, y + 1) + get(x + 1, y + 1)
                - get(x - 1, y - 1)
                - 2.0 * get(x, y - 1)
                - get(x + 1, y - 1);
            out.push((gx / 4.0, gy / 4.0));
        }
    }
    out
}

/// Edge strength, 0..=1, from the Sobel gradient.
pub fn edges(plane: &[f32], w: usize, h: usize) -> Vec<f32> {
    gradient(plane, w, h)
        .iter()
        .map(|(gx, gy)| gx.hypot(*gy).min(1.0))
        .collect()
}

/// Sample a plane bilinearly, clamping to the edge.
pub fn plane_at(plane: &[f32], w: usize, h: usize, fx: f32, fy: f32) -> f32 {
    let x0 = fx.floor();
    let y0 = fy.floor();
    let (tx, ty) = (fx - x0, fy - y0);
    let get = |x: f32, y: f32| -> f32 {
        let x = (x as i32).clamp(0, w as i32 - 1) as usize;
        let y = (y as i32).clamp(0, h as i32 - 1) as usize;
        plane[y * w + x]
    };
    let top = get(x0, y0) * (1.0 - tx) + get(x0 + 1.0, y0) * tx;
    let bot = get(x0, y0 + 1.0) * (1.0 - tx) + get(x0 + 1.0, y0 + 1.0) * tx;
    top * (1.0 - ty) + bot * ty
}

/// Average a plane along a line through each pixel, in a direction the
/// caller chooses per pixel.
///
/// This is the whole of the Brush Strokes group in one function: a stroke
/// is what you get when you smear a picture *along* something -- the edge
/// direction, a fixed angle, the gradient -- rather than in a circle.
pub fn streak(
    plane: &[f32],
    w: usize,
    h: usize,
    length: f32,
    direction: impl Fn(usize, usize) -> (f32, f32),
) -> Vec<f32> {
    let steps = (length.max(1.0)).round() as i32;
    let mut out = vec![0.0f32; w * h];
    for y in 0..h {
        for x in 0..w {
            let (dx, dy) = direction(x, y);
            let n = dx.hypot(dy).max(1e-6);
            let (dx, dy) = (dx / n, dy / n);
            let mut sum = 0.0;
            let mut count = 0.0;
            for t in -steps..=steps {
                let fx = x as f32 + dx * t as f32;
                let fy = y as f32 + dy * t as f32;
                sum += plane_at(plane, w, h, fx, fy);
                count += 1.0;
            }
            out[y * w + x] = sum / count;
        }
    }
    out
}

/// Quantise to `levels` steps, the posterisation every "drawn in N tones"
/// effect is built on.
#[inline]
pub fn posterize(v: f32, levels: f32) -> f32 {
    let n = levels.max(2.0) - 1.0;
    (v * n).round() / n
}

/// A repeatable texture field, for the surfaces the Texture and Artistic
/// groups draw onto: canvas weave, sandstone grain, burlap, brick.
///
/// Photoshop loads these from a texture file; generating them means the
/// filter has no assets to ship and no file to be missing.
pub fn surface(kind: u32, x: f32, y: f32, scale: f32, seed: u32) -> f32 {
    let (u, v) = (x / scale.max(1.0), y / scale.max(1.0));
    match kind {
        // Canvas: two crossed sine weaves with noise in the fibres. The
        // noise carries most of it -- a pure weave reads as a screen
        // door rather than as cloth.
        0 => {
            let weave = ((u * std::f32::consts::TAU).sin() + (v * std::f32::consts::TAU).sin())
                * 0.25
                + 0.5;
            weave * 0.35 + fbm(u * 2.0, v * 2.0, seed, 3) * 0.45 + value_noise(x, y, seed) * 0.2
        }
        // Sandstone: fine noise over a soft one.
        1 => fbm(u, v, seed, 3) * 0.6 + value_noise(x, y, seed ^ 0x5bd1) * 0.4,
        // Burlap: coarse crossed fibres, one direction stronger.
        2 => {
            let warp = ((u * std::f32::consts::TAU).sin().abs() * 0.6
                + (v * std::f32::consts::TAU * 0.5).sin().abs() * 0.4)
                .clamp(0.0, 1.0);
            warp * 0.65 + value_noise(x * 1.3, y * 0.9, seed) * 0.35
        }
        // Brick: a running bond, mortar dark.
        _ => {
            let row = (v).floor();
            let shift = if (row as i32) % 2 == 0 { 0.0 } else { 0.5 };
            let cx = (u + shift).fract();
            let cy = v.fract();
            let mortar = if cx < 0.06 || cy < 0.12 { 0.25 } else { 0.85 };
            mortar * 0.8 + value_noise(x * 0.5, y * 0.5, seed) * 0.2
        }
    }
}

/// Quantise a pixel to a flat tone and a coarse colour.
///
/// Posterising each of red, green and blue on its own -- which is what
/// Image ▸ Adjustments ▸ Posterize does -- crosses the channels at
/// different places and turns a smooth sky into bands of unrelated hues.
/// The cut-paper effects want flat *areas*, not accidental colours, so
/// the tone is quantised and the colour with it, coarsely and together.
pub fn flatten_colour(p: &mut [f32], levels: f32, chroma_step: f32) {
    let l = luma(p);
    let tone = posterize(l, levels);
    let step = chroma_step.max(1e-3);
    let a = ((p[0] - l) / step).round() * step;
    let b = ((p[2] - l) / step).round() * step;
    let (r, bl) = (tone + a, tone + b);
    let g = (tone - 0.299 * r - 0.114 * bl) / 0.587;
    p[0] = r.clamp(0.0, 1.0);
    p[1] = g.clamp(0.0, 1.0);
    p[2] = bl.clamp(0.0, 1.0);
}
