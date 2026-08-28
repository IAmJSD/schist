//! The geometric live filters — `FlRN` nodes whose `Filt` moves pixels
//! about rather than recolouring them.
//!
//! Every mapping here was read straight off a render: the probe card is
//! the 64³ RGB cube, whose every pixel carries a unique colour, so a
//! 512×512 document's byte-exact thumbnail *is* the filter's
//! displacement field — the colour that lands at a destination pixel
//! says which source pixel it came from, to a fraction of a pixel
//! wherever the resample interpolated. The fixtures are
//! `fixtures/affinity-probe/lf_*.af` and the derivations are in the
//! "Live filters" section of `docs/affinity-format.md`.
//!
//! All of them are inverse maps in the layer's own pixel space: given a
//! destination pixel centre, they say where to sample.

use rayon::prelude::*;

/// One geometric live filter, with its parameters already in pixels and
/// the layer's own coordinates.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Distort {
    /// `RTwC`: rotate by `angle_deg` at the centre, easing to nothing at
    /// `radius` as the square of the distance left to go.
    Twirl {
        cx: f64,
        cy: f64,
        radius: f64,
        angle_deg: f64,
    },
    /// `RPPC`: pull the disc's contents in (positive) or push them out
    /// (negative), on the same squared ease as the twirl.
    Pinch {
        cx: f64,
        cy: f64,
        radius: f64,
        amount: f64,
    },
    /// `RSpC`: bulge the disc as if seen through a sphere. The two
    /// directions are each other's inverse — arcsine outwards, sine
    /// inwards — mixed with the identity by `amount`.
    Spherical {
        cx: f64,
        cy: f64,
        radius: f64,
        amount: f64,
    },
    /// `RRiC`: concentric rings of rotation, a sine in the radius.
    Ripple { cx: f64, cy: f64, intensity: f64 },
    /// `RLdC`: barrel/pincushion about the centre, pinned at the corner.
    Lens {
        cx: f64,
        cy: f64,
        rad_x: f64,
        rad_y: f64,
        amount: f64,
    },
    /// `RPxC`: square blocks of the average colour.
    Pixelate { size: f64 },
}

/// The ripple's angular amplitude in degrees at `Inte` = 100, and the
/// exponent by which it follows the slider.
///
/// Measured at `Inte` 25, 50 and 100 (`lf_ripple*.af`): the wavelength
/// is exactly 1440/`Inte` pixels, but the amplitude is not quite linear
/// in the slider — this power law fits the three probes to within 3%,
/// which is the whole of this filter's disagreement with Affinity.
const RIPPLE_AMPLITUDE: f64 = 2.6224;
const RIPPLE_EXPONENT: f64 = 0.895;

impl Distort {
    /// Where a destination pixel centre samples from, in the same
    /// space. `Pixelate` is not a resample and never asks.
    fn source(&self, x: f64, y: f64) -> (f64, f64) {
        match *self {
            Distort::Twirl {
                cx,
                cy,
                radius,
                angle_deg,
            } => {
                let (dx, dy) = (x - cx, y - cy);
                let r = dx.hypot(dy);
                if r >= radius || radius <= 0.0 {
                    return (x, y);
                }
                let ease = 1.0 - r / radius;
                let turn = (angle_deg * ease * ease).to_radians();
                let a = dy.atan2(dx) - turn;
                (cx + r * a.cos(), cy + r * a.sin())
            }
            Distort::Pinch {
                cx,
                cy,
                radius,
                amount,
            } => {
                let (dx, dy) = (x - cx, y - cy);
                let r = dx.hypot(dy);
                if r >= radius || radius <= 0.0 || r == 0.0 {
                    return (x, y);
                }
                let ease = 1.0 - r / radius;
                let scale = 1.0 - amount * ease * ease;
                (cx + dx * scale, cy + dy * scale)
            }
            Distort::Spherical {
                cx,
                cy,
                radius,
                amount,
            } => {
                let (dx, dy) = (x - cx, y - cy);
                let r = dx.hypot(dy);
                if r >= radius || radius <= 0.0 || r == 0.0 {
                    return (x, y);
                }
                let t = r / radius;
                // Outwards the disc samples along the arc of a
                // hemisphere (arcsine); inwards it samples the sine,
                // which is the same map run backwards.
                let full = if amount >= 0.0 {
                    radius * std::f64::consts::FRAC_2_PI * t.asin()
                } else {
                    radius * (std::f64::consts::FRAC_PI_2 * t).sin()
                };
                let sr = r + amount.abs() * (full - r);
                let scale = sr / r;
                (cx + dx * scale, cy + dy * scale)
            }
            Distort::Ripple { cx, cy, intensity } => {
                let (dx, dy) = (x - cx, y - cy);
                let r = dx.hypot(dy);
                if intensity <= 0.0 || r == 0.0 {
                    return (x, y);
                }
                let wavelength = 1440.0 / intensity;
                let amp = RIPPLE_AMPLITUDE * (intensity / 100.0).powf(RIPPLE_EXPONENT);
                let turn = (amp * (std::f64::consts::TAU * r / wavelength).sin()).to_radians();
                let a = dy.atan2(dx) + turn;
                (cx + r * a.cos(), cy + r * a.sin())
            }
            Distort::Lens {
                cx,
                cy,
                rad_x,
                rad_y,
                amount,
            } => {
                if rad_x <= 0.0 || rad_y <= 0.0 {
                    return (x, y);
                }
                let (dx, dy) = (x - cx, y - cy);
                // Normalised so that the corner — where the elliptical
                // radius is sqrt(2) — is the one point that stays put.
                let rho = (dx / rad_x).hypot(dy / rad_y);
                let scale = 1.0 - amount * (1.0 - rho / std::f64::consts::SQRT_2);
                (cx + dx * scale, cy + dy * scale)
            }
            Distort::Pixelate { .. } => (x, y),
        }
    }
}

/// Run one filter over a layer's pixels, in place of them.
///
/// The result is the same size as the input: none of these grow the
/// layer. Taps are premultiplied so a transparent neighbour cannot drag
/// its colour in, and off-page samples are transparent rather than
/// clamped — the ripple turns a 512² card's corners over the page edge
/// and Affinity hands back transparency there, not a smeared edge.
pub(crate) fn apply(width: u32, height: u32, pixels: &[u8], filter: &Distort) -> Vec<u8> {
    if let Distort::Pixelate { size } = *filter {
        return pixelate(width, height, pixels, size);
    }
    let (w, h) = (width as usize, height as usize);
    let (fw, fh) = (width as f64, height as f64);
    let fetch = |x: i64, y: i64| -> [f32; 4] {
        if x < 0 || y < 0 || x >= w as i64 || y >= h as i64 {
            return [0.0; 4];
        }
        let at = (y as usize * w + x as usize) * 4;
        let p = &pixels[at..at + 4];
        let a = p[3] as f32;
        [p[0] as f32 * a, p[1] as f32 * a, p[2] as f32 * a, a]
    };
    let mut out = vec![0u8; w * h * 4];
    out.par_chunks_exact_mut(w * 4)
        .enumerate()
        .for_each(|(y, row)| {
            for (x, px) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                let (sx, sy) = filter.source(x as f64 + 0.5, y as f64 + 0.5);
                if !(sx.is_finite() && sy.is_finite())
                    || sx < -1.0
                    || sy < -1.0
                    || sx > fw + 1.0
                    || sy > fh + 1.0
                {
                    continue;
                }
                let (fx, fy) = (sx - 0.5, sy - 0.5);
                let (x0, y0) = (fx.floor(), fy.floor());
                let (tx, ty) = ((fx - x0) as f32, (fy - y0) as f32);
                let (x0, y0) = (x0 as i64, y0 as i64);
                let (p00, p10) = (fetch(x0, y0), fetch(x0 + 1, y0));
                let (p01, p11) = (fetch(x0, y0 + 1), fetch(x0 + 1, y0 + 1));
                let mut acc = [0.0f32; 4];
                for (c, a) in acc.iter_mut().enumerate() {
                    let top = p00[c] + (p10[c] - p00[c]) * tx;
                    let bot = p01[c] + (p11[c] - p01[c]) * tx;
                    *a = top + (bot - top) * ty;
                }
                let a = acc[3];
                px[3] = (a + 0.5).clamp(0.0, 255.0) as u8;
                if a > 0.0 {
                    for c in 0..3 {
                        px[c] = (acc[c] / a + 0.5).clamp(0.0, 255.0) as u8;
                    }
                }
            }
        });
    out
}

/// `RPxC`: blocks `size` wide, centred on multiples of `size` measured
/// from the layer's own origin — with `Quan` 16 the first boundary is at
/// 8, not 16, so the two outermost bands are half blocks.
///
/// Those half blocks are where the filter shows its hand, and both
/// probes agree at both block sizes: the missing area counts as
/// transparent, so the band comes out at exactly the fraction of the
/// block that is on the page (alpha 128 down an edge, 64 in a corner),
/// and the colour along a clipped axis is the edge row or column alone
/// rather than the average of the part inside. Ties round to even, which
/// is what makes a block straddling two of the cube card's tiles come
/// back 32 rather than 33.
fn pixelate(width: u32, height: u32, pixels: &[u8], size: f64) -> Vec<u8> {
    let (w, h) = (width as usize, height as usize);
    let q = size.round().clamp(1.0, u32::MAX as f64) as i64;
    if q <= 1 {
        return pixels.to_vec();
    }
    let half = q / 2;
    let block = |i: i64| -> (i64, i64) {
        let k = (i + half).div_euclid(q);
        (k * q - half, k * q - half + q)
    };
    // A block that runs off the page keeps only its edge line, but its
    // coverage — and so the alpha it comes back with — is still the
    // fraction of the whole block that was on the page.
    let span = |lo: i64, hi: i64, n: i64| -> (i64, i64, f64) {
        let inside = (hi.min(n) - lo.max(0)).max(0);
        if lo < 0 {
            (0, 1, inside as f64 / (hi - lo) as f64)
        } else if hi > n {
            (n - 1, n, inside as f64 / (hi - lo) as f64)
        } else {
            (lo, hi, 1.0)
        }
    };
    let row_spans: Vec<(i64, i64, f64)> = (0..w as i64)
        .map(|x| {
            let (lo, hi) = block(x);
            span(lo, hi, w as i64)
        })
        .collect();
    let mut out = vec![0u8; w * h * 4];
    for (y, row) in out.chunks_exact_mut(w * 4).enumerate() {
        let (ly0, ly1) = block(y as i64);
        let (by0, by1, ycov) = span(ly0, ly1, h as i64);
        for (x, px) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let (bx0, bx1, xcov) = row_spans[x];
            let mut acc = [0.0f64; 4];
            for sy in by0..by1 {
                for sx in bx0..bx1 {
                    let src = (sy as usize * w + sx as usize) * 4;
                    let a = pixels[src + 3] as f64;
                    acc[0] += pixels[src] as f64 * a;
                    acc[1] += pixels[src + 1] as f64 * a;
                    acc[2] += pixels[src + 2] as f64 * a;
                    acc[3] += a;
                }
            }
            let n = ((bx1 - bx0) * (by1 - by0)) as f64;
            px[3] = (acc[3] / n * xcov * ycov)
                .round_ties_even()
                .clamp(0.0, 255.0) as u8;
            if acc[3] > 0.0 {
                for c in 0..3 {
                    px[c] = (acc[c] / acc[3]).round_ties_even().clamp(0.0, 255.0) as u8;
                }
            }
        }
    }
    out
}
