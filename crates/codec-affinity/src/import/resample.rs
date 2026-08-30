//! Resampling images: affine and projective maps, mirroring, and
//! plain scaling.

use super::*;

/// Resample an image through a full affine map (bitmap pixel space →
/// canvas space): the destination is the transformed rect's bounding
/// box; every destination pixel centre inverse-maps into the source and
/// samples bilinearly, transparent outside it.
pub(super) fn affine_resample(img: &RgbaImage, map: &Mat) -> Option<(IntRect, RgbaImage)> {
    let inv = map.invert()?;
    let (sw, sh) = (img.width as f64, img.height as f64);
    let corners = [
        map.apply(0.0, 0.0),
        map.apply(sw, 0.0),
        map.apply(0.0, sh),
        map.apply(sw, sh),
    ];
    let mut lo = (f64::INFINITY, f64::INFINITY);
    let mut hi = (f64::NEG_INFINITY, f64::NEG_INFINITY);
    for (x, y) in corners {
        if !x.is_finite() || !y.is_finite() {
            return None;
        }
        lo = (lo.0.min(x), lo.1.min(y));
        hi = (hi.0.max(x), hi.1.max(y));
    }
    if lo.0.abs().max(lo.1.abs()).max(hi.0.abs()).max(hi.1.abs()) > (1 << 24) as f64 {
        return None;
    }
    let rect = IntRect::new(
        lo.0.floor() as i32,
        lo.1.floor() as i32,
        hi.0.ceil() as i32,
        hi.1.ceil() as i32,
    );
    let (dw, dh) = (rect.width() as usize, rect.height() as usize);
    if rect.is_empty() || dw * dh > (1 << 28) {
        return None;
    }

    let (iw, ih) = (img.width as i64, img.height as i64);
    // Taps are premultiplied (so transparent neighbours don't drag
    // colour in) but kept on the 0–255 scale; the unpremultiply ratio
    // and the alpha write-out below are scale-invariant.
    let fetch = |x: i64, y: i64| -> [f32; 4] {
        if x < 0 || y < 0 || x >= iw || y >= ih {
            return [0.0; 4];
        }
        let at = ((y as usize * iw as usize) + x as usize) * 4;
        let p = &img.pixels[at..at + 4];
        let a = p[3] as f32;
        [p[0] as f32 * a, p[1] as f32 * a, p[2] as f32 * a, a]
    };
    let mut pixels = vec![0u8; dw * dh * 4];
    let m = &inv.0;
    // Fully opaque sources (photos, most pasted images) need no
    // premultiply/unpremultiply when the whole 2×2 neighbourhood is
    // inside the image: the taps are opaque, so a straight lerp of the
    // raw channels gives the same result without twelve multiplies and
    // a divide per pixel.
    let opaque = img
        .pixels
        .as_chunks::<4>()
        .0
        .par_iter()
        .all(|p| p[3] == 0xFF);
    pixels
        .par_chunks_exact_mut(dw * 4)
        .enumerate()
        .for_each(|(y, row)| {
            // The inverse map is affine, so along a row the source point
            // advances by a constant (m[0], m[3]) per pixel.
            let py = rect.top as f64 + y as f64 + 0.5;
            let (row_sx, row_sy) = (
                m[1] * py + m[2] + m[0] * (rect.left as f64 + 0.5),
                m[4] * py + m[5] + m[3] * (rect.left as f64 + 0.5),
            );
            for (x, out) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                let (sx, sy) = (row_sx + m[0] * x as f64, row_sy + m[3] * x as f64);
                let (fx, fy) = (sx - 0.5, sy - 0.5);
                if fx < -1.0 || fy < -1.0 || fx > sw || fy > sh {
                    continue;
                }
                // Branch-free floor: `as` truncates toward zero, so
                // adjust down for negative non-integers. (Landing one
                // texel low on a negative integer just moves all the
                // weight to the other tap — same sample.)
                let (tx, ty) = (fx as i64, fy as i64);
                let x0 = tx - (tx as f64 > fx) as i64;
                let y0 = ty - (ty as f64 > fy) as i64;
                let (wx, wy) = ((fx - x0 as f64) as f32, (fy - y0 as f64) as f32);
                if opaque && x0 >= 0 && y0 >= 0 && x0 + 1 < iw && y0 + 1 < ih {
                    let row0 = &img.pixels[(y0 as usize * iw as usize + x0 as usize) * 4..][..8];
                    let row1 =
                        &img.pixels[((y0 + 1) as usize * iw as usize + x0 as usize) * 4..][..8];
                    for c in 0..3 {
                        let top = row0[c] as f32 * (1.0 - wx) + row0[c + 4] as f32 * wx;
                        let bot = row1[c] as f32 * (1.0 - wx) + row1[c + 4] as f32 * wx;
                        out[c] = (top * (1.0 - wy) + bot * wy + 0.5) as u8;
                    }
                    out[3] = 0xFF;
                    continue;
                }
                let acc = if x0 >= 0 && y0 >= 0 && x0 + 1 < iw && y0 + 1 < ih {
                    // Whole 2×2 neighbourhood inside: read the two row
                    // pairs directly, skipping the per-tap bounds test.
                    let at = |px: i64, py: i64| -> [f32; 4] {
                        let p = &img.pixels[((py as usize * iw as usize) + px as usize) * 4..][..4];
                        let a = p[3] as f32;
                        [p[0] as f32 * a, p[1] as f32 * a, p[2] as f32 * a, a]
                    };
                    let (p00, p10) = (at(x0, y0), at(x0 + 1, y0));
                    let (p01, p11) = (at(x0, y0 + 1), at(x0 + 1, y0 + 1));
                    let mut acc = [0.0f32; 4];
                    for c in 0..4 {
                        let top = p00[c] * (1.0 - wx) + p10[c] * wx;
                        let bot = p01[c] * (1.0 - wx) + p11[c] * wx;
                        acc[c] = top * (1.0 - wy) + bot * wy;
                    }
                    acc
                } else {
                    let mut acc = [0.0f32; 4];
                    for (dxy, wgt) in [
                        ((0, 0), (1.0 - wx) * (1.0 - wy)),
                        ((1, 0), wx * (1.0 - wy)),
                        ((0, 1), (1.0 - wx) * wy),
                        ((1, 1), wx * wy),
                    ] {
                        let p = fetch(x0 + dxy.0, y0 + dxy.1);
                        for (a, v) in acc.iter_mut().zip(p) {
                            *a += v * wgt;
                        }
                    }
                    acc
                };
                if acc[3] > f32::EPSILON {
                    let unpremul = 1.0 / acc[3];
                    for i in 0..3 {
                        out[i] = (acc[i] * unpremul + 0.5).clamp(0.0, 255.0) as u8;
                    }
                    out[3] = (acc[3] + 0.5).clamp(0.0, 255.0) as u8;
                }
            }
        });
    Some((
        rect,
        RgbaImage {
            width: dw as u32,
            height: dh as u32,
            pixels,
        },
    ))
}

impl Homography {
    /// The map taking `src[i]` to `dst[i]`, by the usual eight-equation
    /// direct linear solve. `None` if the quads are degenerate.
    pub(super) fn from_quads(src: &[(f64, f64); 4], dst: &[(f64, f64); 4]) -> Option<Homography> {
        // Two rows per corner: x' (h6 h7 scaled by -x') and y'.
        let mut a = [[0.0f64; 9]; 8];
        for i in 0..4 {
            let (x, y) = src[i];
            let (u, v) = dst[i];
            a[2 * i] = [x, y, 1.0, 0.0, 0.0, 0.0, -u * x, -u * y, u];
            a[2 * i + 1] = [0.0, 0.0, 0.0, x, y, 1.0, -v * x, -v * y, v];
        }
        // Gaussian elimination with partial pivoting.
        for col in 0..8 {
            let mut pivot = col;
            for row in col + 1..8 {
                if a[row][col].abs() > a[pivot][col].abs() {
                    pivot = row;
                }
            }
            if a[pivot][col].abs() < 1e-12 {
                return None;
            }
            a.swap(col, pivot);
            let d = a[col][col];
            for v in a[col].iter_mut() {
                *v /= d;
            }
            let pivot = a[col];
            for (row, cells) in a.iter_mut().enumerate() {
                if row == col {
                    continue;
                }
                let f = cells[col];
                if f == 0.0 {
                    continue;
                }
                for (k, v) in cells.iter_mut().enumerate().skip(col) {
                    *v -= f * pivot[k];
                }
            }
        }
        let mut h = [0.0f64; 9];
        for (i, row) in a.iter().enumerate() {
            h[i] = row[8];
            if !h[i].is_finite() {
                return None;
            }
        }
        h[8] = 1.0;
        Some(Homography(h))
    }

    fn apply(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        let h = &self.0;
        let w = h[6] * x + h[7] * y + h[8];
        if w.abs() < 1e-12 {
            return None;
        }
        let p = (
            (h[0] * x + h[1] * y + h[2]) / w,
            (h[3] * x + h[4] * y + h[5]) / w,
        );
        (p.0.is_finite() && p.1.is_finite()).then_some(p)
    }

    /// The inverse map, from the adjugate of the 3x3 (scale is free, so
    /// no determinant division is needed beyond renormalising `h8`).
    fn invert(&self) -> Option<Homography> {
        let h = &self.0;
        let mut inv = [
            h[4] * h[8] - h[5] * h[7],
            h[2] * h[7] - h[1] * h[8],
            h[1] * h[5] - h[2] * h[4],
            h[5] * h[6] - h[3] * h[8],
            h[0] * h[8] - h[2] * h[6],
            h[2] * h[3] - h[0] * h[5],
            h[3] * h[7] - h[4] * h[6],
            h[1] * h[6] - h[0] * h[7],
            h[0] * h[4] - h[1] * h[3],
        ];
        if inv[8].abs() < 1e-12 || !inv.iter().all(|v| v.is_finite()) {
            return None;
        }
        let s = inv[8];
        for v in inv.iter_mut() {
            *v /= s;
        }
        Some(Homography(inv))
    }

    /// `self` applied after `other` (the matrix product self * other).
    pub(super) fn compose(&self, other: &Homography) -> Homography {
        let (a, b) = (&self.0, &other.0);
        let mut out = [0.0f64; 9];
        for r in 0..3 {
            for c in 0..3 {
                out[r * 3 + c] =
                    a[r * 3] * b[c] + a[r * 3 + 1] * b[3 + c] + a[r * 3 + 2] * b[6 + c];
            }
        }
        if out[8].abs() > 1e-12 {
            let s = out[8];
            for v in out.iter_mut() {
                *v /= s;
            }
        }
        Homography(out)
    }

    pub(super) fn is_identity(&self) -> bool {
        const I: [f64; 9] = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];
        self.0.iter().zip(I).all(|(a, b)| (a - b).abs() < 1e-9)
    }
}

/// Resample an image through a projective map, in the image's own pixel
/// space. Returns the integer offset of the result within that space
/// (the destination quad can leave the source's box in any direction)
/// alongside the warped pixels; the caller folds the offset into the
/// layer transform.
pub(super) fn perspective_resample(
    img: &RgbaImage,
    h: &Homography,
) -> Option<(i32, i32, RgbaImage)> {
    let inv = h.invert()?;
    let (sw, sh) = (img.width as f64, img.height as f64);
    let mut lo = (f64::INFINITY, f64::INFINITY);
    let mut hi = (f64::NEG_INFINITY, f64::NEG_INFINITY);
    for (x, y) in [(0.0, 0.0), (sw, 0.0), (0.0, sh), (sw, sh)] {
        let (x, y) = h.apply(x, y)?;
        lo = (lo.0.min(x), lo.1.min(y));
        hi = (hi.0.max(x), hi.1.max(y));
    }
    if lo.0.abs().max(lo.1.abs()).max(hi.0.abs()).max(hi.1.abs()) > (1 << 24) as f64 {
        return None;
    }
    let rect = IntRect::new(
        lo.0.floor() as i32,
        lo.1.floor() as i32,
        hi.0.ceil() as i32,
        hi.1.ceil() as i32,
    );
    let (dw, dh) = (rect.width() as usize, rect.height() as usize);
    if rect.is_empty() || dw * dh > (1 << 28) {
        return None;
    }

    let (iw, ih) = (img.width as i64, img.height as i64);
    let fetch = |x: i64, y: i64| -> [f32; 4] {
        if x < 0 || y < 0 || x >= iw || y >= ih {
            return [0.0; 4];
        }
        let at = ((y as usize * iw as usize) + x as usize) * 4;
        let p = &img.pixels[at..at + 4];
        let a = p[3] as f32;
        [p[0] as f32 * a, p[1] as f32 * a, p[2] as f32 * a, a]
    };
    let m = &inv.0;
    let mut pixels = vec![0u8; dw * dh * 4];
    pixels
        .par_chunks_exact_mut(dw * 4)
        .enumerate()
        .for_each(|(y, row)| {
            // Unlike an affine map the source point does not advance by
            // a constant along the row, but the three linear forms do,
            // so only the divide is per-pixel.
            let py = rect.top as f64 + y as f64 + 0.5;
            let px0 = rect.left as f64 + 0.5;
            let (mut nx, mut ny, mut nw) = (
                m[0] * px0 + m[1] * py + m[2],
                m[3] * px0 + m[4] * py + m[5],
                m[6] * px0 + m[7] * py + m[8],
            );
            for out in row.as_chunks_mut::<4>().0.iter_mut() {
                let (cx, cy, cw) = (nx, ny, nw);
                nx += m[0];
                ny += m[3];
                nw += m[6];
                if cw.abs() < 1e-12 {
                    continue;
                }
                let (sx, sy) = (cx / cw, cy / cw);
                let (fx, fy) = (sx - 0.5, sy - 0.5);
                if !(fx >= -1.0 && fy >= -1.0 && fx <= sw && fy <= sh) {
                    continue;
                }
                let (tx, ty) = (fx as i64, fy as i64);
                let x0 = tx - (tx as f64 > fx) as i64;
                let y0 = ty - (ty as f64 > fy) as i64;
                let (wx, wy) = ((fx - x0 as f64) as f32, (fy - y0 as f64) as f32);
                let mut acc = [0.0f32; 4];
                for (dxy, wgt) in [
                    ((0, 0), (1.0 - wx) * (1.0 - wy)),
                    ((1, 0), wx * (1.0 - wy)),
                    ((0, 1), (1.0 - wx) * wy),
                    ((1, 1), wx * wy),
                ] {
                    let p = fetch(x0 + dxy.0, y0 + dxy.1);
                    for (a, v) in acc.iter_mut().zip(p) {
                        *a += v * wgt;
                    }
                }
                if acc[3] > f32::EPSILON {
                    let unpremul = 1.0 / acc[3];
                    for i in 0..3 {
                        out[i] = (acc[i] * unpremul + 0.5).clamp(0.0, 255.0) as u8;
                    }
                    out[3] = (acc[3] + 0.5).clamp(0.0, 255.0) as u8;
                }
            }
        });
    Some((
        rect.left,
        rect.top,
        RgbaImage {
            width: dw as u32,
            height: dh as u32,
            pixels,
        },
    ))
}

pub(super) fn mirror(img: &mut RgbaImage, horizontal: bool, vertical: bool) {
    let (w, h) = (img.width as usize, img.height as usize);
    if horizontal {
        for row in img.pixels.chunks_exact_mut(w * 4) {
            let (mut a, mut b) = (0, w - 1);
            while a < b {
                for c in 0..4 {
                    row.swap(a * 4 + c, b * 4 + c);
                }
                a += 1;
                b -= 1;
            }
        }
    }
    if vertical {
        let stride = w * 4;
        let (mut top, mut bottom) = (0, h - 1);
        while top < bottom {
            for i in 0..stride {
                img.pixels.swap(top * stride + i, bottom * stride + i);
            }
            top += 1;
            bottom -= 1;
        }
    }
}

pub(super) fn resample_to(img: RgbaImage, dw: u32, dh: u32) -> RgbaImage {
    if (img.width, img.height) == (dw, dh) || dw == 0 || dh == 0 {
        return img;
    }
    let (sw, sh) = (img.width as usize, img.height as usize);
    let (dw, dh) = (dw as usize, dh as usize);
    // The horizontal taps are the same for every row; compute them once
    // as byte offsets into a source row.
    let xtaps: Vec<(usize, usize, f32)> = (0..dw)
        .map(|x| {
            let fx = (x as f32 + 0.5) * sw as f32 / dw as f32 - 0.5;
            let x0 = (fx.floor().max(0.0) as usize).min(sw - 1);
            let x1 = (x0 + 1).min(sw - 1);
            let wx = (fx - x0 as f32).clamp(0.0, 1.0);
            (x0 * 4, x1 * 4, wx)
        })
        .collect();
    let mut pixels = vec![0u8; dw * dh * 4];
    pixels
        .par_chunks_exact_mut(dw * 4)
        .enumerate()
        .for_each(|(y, out_row)| {
            let fy = (y as f32 + 0.5) * sh as f32 / dh as f32 - 0.5;
            let y0 = (fy.floor().max(0.0) as usize).min(sh - 1);
            let y1 = (y0 + 1).min(sh - 1);
            let wy = (fy - y0 as f32).clamp(0.0, 1.0);
            let row0 = &img.pixels[y0 * sw * 4..][..sw * 4];
            let row1 = &img.pixels[y1 * sw * 4..][..sw * 4];
            for (out, &(x0, x1, wx)) in out_row.as_chunks_mut::<4>().0.iter_mut().zip(&xtaps) {
                for c in 0..4 {
                    let top = row0[x0 + c] as f32 * (1.0 - wx) + row0[x1 + c] as f32 * wx;
                    let bot = row1[x0 + c] as f32 * (1.0 - wx) + row1[x1 + c] as f32 * wx;
                    out[c] = (top * (1.0 - wy) + bot * wy + 0.5) as u8;
                }
            }
        });
    RgbaImage {
        width: dw as u32,
        height: dh as u32,
        pixels,
    }
}
