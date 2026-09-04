//! Colour filter array interpolation.
//!
//! Input is one plane of normalised, white-balanced samples (black
//! subtracted, white at 1.0) with the CFA that describes it; output is
//! three floats a pixel.
//!
//! Because the plane arrives white balanced, a neutral subject gives
//! *equal* numbers under every filter colour. That is what makes the
//! mosaic itself usable as a luminance signal: neighbouring samples of
//! different colours can be compared directly, so gradients measured
//! across the mosaic mean something, and the colour-difference planes
//! (R-G, B-G) really are the slowly varying signals every demosaic
//! algorithm assumes they are. Demosaicing before white balance would
//! break both assumptions.
//!
//! Three families live here:
//!
//! * Bayer, [`Quality::Fast`]: bilinear.
//! * Bayer, [`Quality::Best`]: the Hamilton-Adams gradient-corrected
//!   method (Adams & Hamilton, *Adaptive colour plane interpolation*,
//!   1997), written from the published description: green is
//!   interpolated along whichever of the horizontal and vertical
//!   directions has the smaller second-order gradient, with a Laplacian
//!   correction from the centre pixel's own colour; red and blue then
//!   follow from the colour-difference planes, choosing the quieter
//!   diagonal.
//! * Everything else (X-Trans and arbitrary `Pattern`s): a generic
//!   per-colour interpolation. `Fast` is an inverse-square-distance
//!   weighted average of each colour over a 5x5 window. `Best` is a
//!   gradient-weighted, three-pass version of the same idea: a green
//!   lowpass to steer the weights with, then green everywhere from
//!   the colour-difference model, then red and blue as green plus an
//!   interpolated colour difference. It is directional in the sense
//!   that matters — a neighbour across an edge barely counts — while
//!   staying agnostic about where the pattern puts its colours,
//!   which is what lets one routine serve X-Trans and whatever a
//!   vendor invents next.
//!
//! No part of this is derived from another decoder's source: the
//! algorithms come from their published descriptions and the code is
//! this crate's own.

use crate::{frame_samples, Cfa, CfaColor, Error, Result};
use rayon::prelude::*;

/// How much work to spend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Quality {
    /// Bilinear: previews, and the fallback for any pattern.
    Fast,
    /// The best available for the pattern: an adaptive homogeneity or
    /// gradient-directed method for Bayer, a directional method for
    /// X-Trans.
    #[default]
    Best,
}

/// Internal colour codes: the index of the output channel a sample
/// belongs to. A second green is just green — the develop stage has
/// already balanced it against the first.
const RED: u8 = 0;
const GRN: u8 = 1;
const BLU: u8 = 2;

/// How far the working plane is extended past the frame. Every
/// algorithm here reaches at most two pixels, and the widening search
/// for a missing colour at most six.
const PAD: usize = 6;

/// Inverse-square-distance weights over a 5x5 window, indexed
/// `(dy + 2) * 5 + (dx + 2)`. The centre is never used (it is the
/// pixel's own sample), so its weight is zero.
#[rustfmt::skip]
const SPATIAL: [f32; 25] = [
    0.125, 0.2, 0.25, 0.2, 0.125,
    0.2,   0.5, 1.0,  0.5, 0.2,
    0.25,  1.0, 0.0,  1.0, 0.25,
    0.2,   0.5, 1.0,  0.5, 0.2,
    0.125, 0.2, 0.25, 0.2, 0.125,
];

/// How much of the centre's own detail the generic `Best` model
/// carries across to the other colours.
///
/// The colour-difference model says green equals the centre's sample
/// plus the local mean of green minus the local mean of the centre's
/// colour. Taken literally (a factor of one) it over-sharpens, and
/// badly: the centre's colour is sampled every second pixel at best,
/// so `centre - mean` is a second difference over twice the distance
/// the green mean spans, and the correction comes out roughly twice
/// the detail green actually has. Fitting the two transfer functions
/// over the frequencies a filter array can carry puts the factor
/// between 0.4 and 0.6 — the same reason Hamilton-Adams weights its
/// own Laplacian term by a half.
const CORRECTION: f32 = 0.5;

/// Strength of the edge-sensing term in the generic `Best` weights.
/// Samples are nominally 0..1, so a difference of an eighth of full
/// scale roughly halves a neighbour's weight.
const RANGE: f32 = 8.0;

/// Interpolate `data` (`width * height`, one sample a pixel) under
/// `cfa` into RGB (`width * height * 3`). Any `Cfa` variant: Bayer and
/// X-Trans get their proper algorithms, `Pattern` gets a generic
/// per-colour interpolation (CMYG sensors are converted to RGB by the
/// develop matrix, so they come out as four "colours" here — see
/// `develop`).
pub fn demosaic(
    data: &[f32],
    width: usize,
    height: usize,
    cfa: &Cfa,
    quality: Quality,
) -> Result<Vec<f32>> {
    let samples = frame_samples(width, height, 1)?;
    if data.len() != samples {
        return Err(Error::Corrupt(format!(
            "demosaic: {} samples for a {}x{} plane",
            data.len(),
            width,
            height
        )));
    }
    if width == 0 || height == 0 {
        return Ok(Vec::new());
    }
    if matches!(cfa, Cfa::None) {
        return Err(Error::Unsupported(
            "demosaic: Cfa::None is three samples a pixel already".into(),
        ));
    }
    // Every path below allocates three floats per input pixel.
    frame_samples(width, height, 3)?;
    let mosaic = Mosaic::build(data, width, height, cfa)?;
    // A Bayer array that is not actually Bayer (a vendor's odd
    // four-colour layout squeezed into the variant) falls through to
    // the generic path rather than being interpolated as if the
    // neighbours were the colours the algorithm expects.
    let bayer = matches!(cfa, Cfa::Bayer(_)) && mosaic.is_true_bayer();
    Ok(match (bayer, quality) {
        (true, Quality::Fast) => bayer_bilinear(&mosaic, width, height),
        (true, Quality::Best) => bayer_hamilton_adams(&mosaic, width, height),
        (false, Quality::Fast) => generic_weighted(&mosaic, width, height),
        (false, Quality::Best) => generic_directional(&mosaic, width, height),
    })
}

/// The sample plane extended by [`PAD`] pixels on every side, with a
/// colour code for every position.
///
/// The border is filled by copying from one CFA period further in
/// (`x - PAD` becomes `x - PAD + period` when it falls off the left,
/// and so on). Plain mirroring would be wrong for anything but a 2x2
/// array: moving by a whole period is the only extension that keeps
/// every padded position under the same filter colour it would have
/// had if the sensor went on for ever, which is what lets the
/// algorithms run unbranched right up to the frame edge.
struct Mosaic {
    plane: Vec<f32>,
    codes: Vec<u8>,
    pw: usize,
    ph: usize,
}

impl Mosaic {
    #[inline(always)]
    fn at(&self, x: usize, y: usize) -> f32 {
        self.plane[y * self.pw + x]
    }

    #[inline(always)]
    fn code(&self, x: usize, y: usize) -> u8 {
        self.codes[y * self.pw + x]
    }

    fn build(data: &[f32], width: usize, height: usize, cfa: &Cfa) -> Result<Mosaic> {
        let (cw, ch) = match cfa {
            Cfa::None => return Err(Error::Unsupported("demosaic: no filter array".into())),
            Cfa::Bayer(_) => (2, 2),
            Cfa::XTrans(_) => (6, 6),
            Cfa::Pattern {
                width: pw,
                height: ph,
                colors,
            } => {
                let cells = pw.checked_mul(*ph).filter(|n| *n <= 4096).ok_or_else(|| {
                    Error::Corrupt(format!("demosaic: filter pattern of {pw}x{ph}"))
                })?;
                if *pw == 0 || *ph == 0 || colors.len() < cells {
                    return Err(Error::Corrupt(format!(
                        "demosaic: {}x{} filter pattern with {} colours",
                        pw,
                        ph,
                        colors.len()
                    )));
                }
                (*pw, *ph)
            }
            // The stored rectangle interpolated as it lies: the colours
            // are right, the geometry is not — the picture comes out
            // sheared by 45 degrees. `develop` re-indexes the
            // photosites into a Bayer frame before it gets here; this
            // arm only serves a caller who wants the stored rectangle.
            Cfa::SuperCcd { row_staggered, .. } => Cfa::super_ccd_period(*row_staggered),
        };
        // One period of colour codes, laid out for *padded*
        // coordinates: padded x corresponds to sensor x - PAD, so the
        // period is rotated by PAD (mod the period).
        let period_len = cw
            .checked_mul(ch)
            .filter(|n| *n <= 4096)
            .ok_or_else(|| Error::Corrupt(format!("demosaic: filter pattern of {cw}x{ch}")))?;
        let mut period = vec![0u8; period_len];
        for py in 0..ch {
            for px in 0..cw {
                let ox = (px + cw * (PAD / cw + 1) - PAD) % cw;
                let oy = (py + ch * (PAD / ch + 1) - PAD) % ch;
                let color = cfa
                    .color_at(ox, oy)
                    .ok_or_else(|| Error::Corrupt("demosaic: filter pattern is short".into()))?;
                period[py * cw + px] = code_of(color)?;
            }
        }
        let (pw, ph) = (
            width
                .checked_add(2 * PAD)
                .ok_or_else(|| Error::Corrupt("demosaic: padded width overflow".into()))?,
            height
                .checked_add(2 * PAD)
                .ok_or_else(|| Error::Corrupt("demosaic: padded height overflow".into()))?,
        );
        let padded = frame_samples(pw, ph, 1)?;
        let mut plane = vec![0f32; padded];
        plane.par_chunks_mut(pw).enumerate().for_each(|(py, row)| {
            let sy = wrap(py as isize - PAD as isize, height, ch);
            let src = &data[sy * width..sy * width + width];
            row[PAD..PAD + width].copy_from_slice(src);
            for px in 0..PAD {
                row[px] = src[wrap(px as isize - PAD as isize, width, cw)];
            }
            for px in PAD + width..pw {
                row[px] = src[wrap(px as isize - PAD as isize, width, cw)];
            }
        });
        let mut codes = vec![0u8; padded];
        codes.par_chunks_mut(pw).enumerate().for_each(|(py, row)| {
            let base = (py % ch) * cw;
            let mut i = 0;
            for c in row.iter_mut() {
                *c = period[base + i];
                i += 1;
                if i == cw {
                    i = 0;
                }
            }
        });
        Ok(Mosaic {
            plane,
            codes,
            pw,
            ph,
        })
    }

    /// Whether the 2x2 array really is a Bayer one: one red, one blue
    /// and two greens on a diagonal. The Hamilton-Adams code assumes
    /// exactly that.
    fn is_true_bayer(&self) -> bool {
        let c = [
            self.code(0, 0),
            self.code(1, 0),
            self.code(0, 1),
            self.code(1, 1),
        ];
        let greens = c.iter().filter(|v| **v == GRN).count();
        let diagonal = (c[0] == GRN && c[3] == GRN) || (c[1] == GRN && c[2] == GRN);
        greens == 2
            && diagonal
            && c.iter().filter(|v| **v == RED).count() == 1
            && c.iter().filter(|v| **v == BLU).count() == 1
    }
}

fn code_of(color: CfaColor) -> Result<u8> {
    match color {
        CfaColor::Red => Ok(RED),
        // A separately balanced second green is plain green by the
        // time it reaches here: develop has already applied its own
        // multiplier.
        CfaColor::Green | CfaColor::Green2 => Ok(GRN),
        CfaColor::Blue => Ok(BLU),
        // CMYG and four-colour (emerald) arrays need a 4x3 sensor-to-
        // XYZ matrix rather than the 3x3 the rest of this crate
        // carries, so there is nothing sensible to interpolate them
        // into yet.
        other => Err(Error::Unsupported(format!(
            "demosaic: {other:?} filter array (CMYG and four-colour sensors)"
        ))),
    }
}

/// Fold `i` into `0..n` by whole CFA periods, so the filter colour of
/// the result is the filter colour `i` would have had. Falls back to
/// clamping for frames narrower than one period, which no real sensor
/// is.
#[inline]
fn wrap(i: isize, n: usize, period: usize) -> usize {
    let (n, p) = (n as isize, period.max(1) as isize);
    let mut i = i;
    while i < 0 {
        i += p;
    }
    while i >= n {
        i -= p;
    }
    i.clamp(0, n - 1) as usize
}

/// Bilinear: the missing green at a red or blue site is the average of
/// its four green neighbours, the missing opposite colour the average
/// of the four diagonals, and at a green site each of red and blue
/// comes from the two neighbours that carry it (one axis each).
fn bayer_bilinear(m: &Mosaic, width: usize, height: usize) -> Vec<f32> {
    let mut out = vec![0f32; width * height * 3];
    out.par_chunks_mut(width * 3)
        .enumerate()
        .for_each(|(y, row)| {
            let py = y + PAD;
            for x in 0..width {
                let px = x + PAD;
                let v = m.at(px, py);
                let (n, s) = (m.at(px, py - 1), m.at(px, py + 1));
                let (w, e) = (m.at(px - 1, py), m.at(px + 1, py));
                let o = &mut row[x * 3..x * 3 + 3];
                match m.code(px, py) {
                    GRN => {
                        let horizontal = (w + e) * 0.5;
                        let vertical = (n + s) * 0.5;
                        let (r, b) = if m.code(px - 1, py) == RED {
                            (horizontal, vertical)
                        } else {
                            (vertical, horizontal)
                        };
                        o[0] = r;
                        o[1] = v;
                        o[2] = b;
                    }
                    code => {
                        let g = (n + s + w + e) * 0.25;
                        let d = (m.at(px - 1, py - 1)
                            + m.at(px + 1, py - 1)
                            + m.at(px - 1, py + 1)
                            + m.at(px + 1, py + 1))
                            * 0.25;
                        if code == RED {
                            o[0] = v;
                            o[1] = g;
                            o[2] = d;
                        } else {
                            o[0] = d;
                            o[1] = g;
                            o[2] = v;
                        }
                    }
                }
            }
        });
    out
}

/// Hamilton-Adams pass one: green over the whole padded plane.
///
/// At a red or blue site the two candidate estimates are the average
/// of the two green neighbours along an axis plus a quarter of the
/// second derivative of the *centre's own* colour along the same axis
/// (`2c - c_-2 - c_+2`). That correction is what carries detail finer
/// than the green sampling grid across from the red/blue grid. The
/// direction is chosen by the smaller of the two classifiers
/// `|g_- - g_+| + |2c - c_-2 - c_+2|`, i.e. interpolate along the edge,
/// never across it; a tie averages both.
///
/// Only the interior of the padded plane is filled: that covers the
/// two-pixel margin the second pass reads around any real pixel.
fn bayer_green(m: &Mosaic) -> Vec<f32> {
    let mut green = vec![0f32; m.pw * m.ph];
    green
        .par_chunks_mut(m.pw)
        .enumerate()
        .for_each(|(py, row)| {
            if py < 2 || py + 2 >= m.ph {
                return;
            }
            for (px, out) in row.iter_mut().enumerate().take(m.pw - 2).skip(2) {
                let v = m.at(px, py);
                if m.code(px, py) == GRN {
                    *out = v;
                    continue;
                }
                let (w, e) = (m.at(px - 1, py), m.at(px + 1, py));
                let (n, s) = (m.at(px, py - 1), m.at(px, py + 1));
                let lh = 2.0 * v - m.at(px - 2, py) - m.at(px + 2, py);
                let lv = 2.0 * v - m.at(px, py - 2) - m.at(px, py + 2);
                let dh = (w - e).abs() + lh.abs();
                let dv = (n - s).abs() + lv.abs();
                let gh = 0.5 * (w + e) + 0.25 * lh;
                let gv = 0.5 * (n + s) + 0.25 * lv;
                *out = if dh < dv {
                    gh
                } else if dv < dh {
                    gv
                } else {
                    0.5 * (gh + gv)
                };
            }
        });
    green
}

/// Hamilton-Adams pass two: red and blue from the colour differences.
///
/// With green known everywhere, `R - G` and `B - G` are smooth enough
/// to interpolate linearly. At a green site the missing colour has two
/// neighbours on one axis; at a red site blue (and vice versa) has
/// four diagonal ones, and the quieter diagonal — the one whose two
/// colour differences agree — is used alone.
fn bayer_hamilton_adams(m: &Mosaic, width: usize, height: usize) -> Vec<f32> {
    let green = bayer_green(m);
    let mut out = vec![0f32; width * height * 3];
    out.par_chunks_mut(width * 3)
        .enumerate()
        .for_each(|(y, row)| {
            let py = y + PAD;
            let diff = |x: usize, y: usize| m.at(x, y) - green[y * m.pw + x];
            for x in 0..width {
                let px = x + PAD;
                let v = m.at(px, py);
                let g = green[py * m.pw + px];
                let o = &mut row[x * 3..x * 3 + 3];
                match m.code(px, py) {
                    GRN => {
                        let horizontal = 0.5 * (diff(px - 1, py) + diff(px + 1, py));
                        let vertical = 0.5 * (diff(px, py - 1) + diff(px, py + 1));
                        let (r, b) = if m.code(px - 1, py) == RED {
                            (horizontal, vertical)
                        } else {
                            (vertical, horizontal)
                        };
                        o[0] = g + r;
                        o[1] = v;
                        o[2] = g + b;
                    }
                    code => {
                        let nw = diff(px - 1, py - 1);
                        let ne = diff(px + 1, py - 1);
                        let sw = diff(px - 1, py + 1);
                        let se = diff(px + 1, py + 1);
                        let down = (nw - se).abs();
                        let up = (ne - sw).abs();
                        let other = g + if down < up {
                            0.5 * (nw + se)
                        } else if up < down {
                            0.5 * (ne + sw)
                        } else {
                            0.25 * (nw + ne + sw + se)
                        };
                        if code == RED {
                            o[0] = v;
                            o[1] = g;
                            o[2] = other;
                        } else {
                            o[0] = other;
                            o[1] = g;
                            o[2] = v;
                        }
                    }
                }
            }
        });
    out
}

/// The last resort when a colour is missing from the 5x5 window (a
/// pathological pattern, never a real sensor): widen ring by ring.
fn wide_average(m: &Mosaic, px: usize, py: usize, want: u8) -> Option<f32> {
    for r in 3..=PAD as isize {
        let mut sum = 0.0;
        let mut weight = 0.0;
        for dy in -r..=r {
            for dx in -r..=r {
                if dx.abs() != r && dy.abs() != r {
                    continue;
                }
                let (qx, qy) = ((px as isize + dx) as usize, (py as isize + dy) as usize);
                if m.code(qx, qy) == want {
                    let w = 1.0 / (dx * dx + dy * dy) as f32;
                    sum += w * m.at(qx, qy);
                    weight += w;
                }
            }
        }
        if weight > 0.0 {
            return Some(sum / weight);
        }
    }
    None
}

/// The generic `Fast` path: every missing colour is the inverse-square
/// distance weighted average of that colour over a 5x5 window. It
/// makes no assumption at all about the layout, so it works for
/// X-Trans and for any `Cfa::Pattern`; it is simply soft, since each
/// channel is reconstructed only from its own sparse samples.
fn generic_weighted(m: &Mosaic, width: usize, height: usize) -> Vec<f32> {
    let mut out = vec![0f32; width * height * 3];
    out.par_chunks_mut(width * 3)
        .enumerate()
        .for_each(|(y, row)| {
            let py = y + PAD;
            for x in 0..width {
                let px = x + PAD;
                let own = m.code(px, py);
                let mut sum = [0f32; 3];
                let mut weight = [0f32; 3];
                for dy in -2isize..=2 {
                    let qy = (py as isize + dy) as usize;
                    for dx in -2isize..=2 {
                        if dx == 0 && dy == 0 {
                            continue;
                        }
                        let qx = (px as isize + dx) as usize;
                        let k = m.code(qx, qy) as usize;
                        let w = SPATIAL[(dy + 2) as usize * 5 + (dx + 2) as usize];
                        sum[k] += w * m.at(qx, qy);
                        weight[k] += w;
                    }
                }
                let o = &mut row[x * 3..x * 3 + 3];
                for k in 0..3 {
                    o[k] = if k as u8 == own {
                        m.at(px, py)
                    } else if weight[k] > 0.0 {
                        sum[k] / weight[k]
                    } else {
                        wide_average(m, px, py, k as u8).unwrap_or_else(|| m.at(px, py))
                    };
                }
            }
        });
    out
}

/// A dense but blurry green: the distance-weighted mean of the green
/// samples in a 5x5 window, at *every* position.
///
/// This exists only as a guide for the edge-sensing weights below. The
/// obvious thing — weighting a neighbour by how far its sample is from
/// the centre's — is wrong across a filter array, because two samples
/// of different colours differ by the subject's colour as well as by
/// any edge between them. On a smooth gradient that turns into a
/// systematic bias: the greens on the side whose values happen to sit
/// nearer the centre's colour get the larger weights, and the mean
/// comes out shifted. Comparing two green estimates instead compares
/// like with like, so the weights stay symmetric where the image is
/// smooth and only really change at edges.
fn green_lowpass(m: &Mosaic) -> Vec<f32> {
    let mut out = vec![0f32; m.pw * m.ph];
    out.par_chunks_mut(m.pw).enumerate().for_each(|(py, row)| {
        if py < 2 || py + 2 >= m.ph {
            return;
        }
        for (px, out) in row.iter_mut().enumerate().take(m.pw - 2).skip(2) {
            let mut sum = 0f32;
            let mut weight = 0f32;
            for dy in -2isize..=2 {
                let qy = (py as isize + dy) as usize;
                for dx in -2isize..=2 {
                    let qx = (px as isize + dx) as usize;
                    if (dx == 0 && dy == 0) || m.code(qx, qy) != GRN {
                        continue;
                    }
                    let w = SPATIAL[(dy + 2) as usize * 5 + (dx + 2) as usize];
                    sum += w * m.at(qx, qy);
                    weight += w;
                }
            }
            *out = if weight > 0.0 {
                sum / weight
            } else {
                m.at(px, py)
            };
        }
    });
    out
}

/// Generic `Best`, pass one: green over the padded plane.
///
/// The model is the standard one — colour differences vary slowly — but
/// applied without any assumption about where the colours sit. At a
/// non-green site, green is the centre's own sample plus the local
/// mean of green minus the local mean of the centre's colour (the
/// centre itself excluded, so the sample's own detail survives instead
/// of being averaged away). On a smooth gradient both means come out
/// at the centre's position, so the estimate reduces to "the same
/// colour difference as the neighbourhood", which is exact.
///
/// The weights are edge sensing: a neighbour counts less the further
/// the *green lowpass* at its position is from the centre's, which on
/// a white-balanced mosaic means "less across an edge" and nothing at
/// all on smooth ground.
fn generic_green(m: &Mosaic, guide: &[f32]) -> Vec<f32> {
    let mut green = vec![0f32; m.pw * m.ph];
    green
        .par_chunks_mut(m.pw)
        .enumerate()
        .for_each(|(py, row)| {
            if py < 4 || py + 4 >= m.ph {
                return;
            }
            for (px, out) in row.iter_mut().enumerate().take(m.pw - 4).skip(4) {
                let own = m.code(px, py);
                let centre = m.at(px, py);
                if own == GRN {
                    *out = centre;
                    continue;
                }
                let here = guide[py * m.pw + px];
                let (mut sum_g, mut weight_g) = (0f32, 0f32);
                let (mut sum_c, mut weight_c) = (0f32, 0f32);
                for dy in -2isize..=2 {
                    let qy = (py as isize + dy) as usize;
                    for dx in -2isize..=2 {
                        if dx == 0 && dy == 0 {
                            continue;
                        }
                        let qx = (px as isize + dx) as usize;
                        let k = m.code(qx, qy);
                        if k != GRN && k != own {
                            continue;
                        }
                        let w = SPATIAL[(dy + 2) as usize * 5 + (dx + 2) as usize]
                            / (1.0 + RANGE * (guide[qy * m.pw + qx] - here).abs());
                        if k == GRN {
                            sum_g += w * m.at(qx, qy);
                            weight_g += w;
                        } else {
                            sum_c += w * m.at(qx, qy);
                            weight_c += w;
                        }
                    }
                }
                *out = if weight_g <= 0.0 {
                    centre
                } else if weight_c <= 0.0 {
                    sum_g / weight_g
                } else {
                    sum_g / weight_g + CORRECTION * (centre - sum_c / weight_c)
                };
            }
        });
    green
}

/// Generic `Best`, pass two: red and blue as green plus an
/// interpolated colour difference, with the edge-sensing weights now
/// measured on the dense green plane.
fn generic_directional(m: &Mosaic, width: usize, height: usize) -> Vec<f32> {
    let green = generic_green(m, &green_lowpass(m));
    let mut out = vec![0f32; width * height * 3];
    out.par_chunks_mut(width * 3)
        .enumerate()
        .for_each(|(y, row)| {
            let py = y + PAD;
            for x in 0..width {
                let px = x + PAD;
                let own = m.code(px, py);
                let centre = m.at(px, py);
                let g = green[py * m.pw + px];
                let mut sum = [0f32; 3];
                let mut weight = [0f32; 3];
                for dy in -2isize..=2 {
                    let qy = (py as isize + dy) as usize;
                    for dx in -2isize..=2 {
                        if dx == 0 && dy == 0 {
                            continue;
                        }
                        let qx = (px as isize + dx) as usize;
                        let k = m.code(qx, qy) as usize;
                        if k == GRN as usize {
                            continue;
                        }
                        let qg = green[qy * m.pw + qx];
                        let w = SPATIAL[(dy + 2) as usize * 5 + (dx + 2) as usize]
                            / (1.0 + RANGE * (qg - g).abs());
                        sum[k] += w * (m.at(qx, qy) - qg);
                        weight[k] += w;
                    }
                }
                let o = &mut row[x * 3..x * 3 + 3];
                o[1] = g;
                for k in [RED as usize, BLU as usize] {
                    o[k] = if k as u8 == own {
                        centre
                    } else if weight[k] > 0.0 {
                        g + sum[k] / weight[k]
                    } else {
                        wide_average(m, px, py, k as u8).unwrap_or(g)
                    };
                }
            }
        });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A synthetic scene, built the way photographs are: one
    /// luminance field carrying all the detail, modulated by a
    /// chromaticity that varies slowly across the frame, plus a
    /// couple of genuinely coloured patches.
    ///
    /// That correlation is not decoration. Every demosaic algorithm
    /// worth the name works by assuming the colour *differences* are
    /// the smooth part of the picture and the luminance carries the
    /// detail, which is true of light through any real lens and any
    /// real subject. A test image whose three channels moved
    /// independently would punish exactly the assumption that makes
    /// these algorithms good, and would measure nothing a sensor ever
    /// sees.
    ///
    /// `detail` adds what separates the methods: hard edges at four
    /// orientations, a fine near-Nyquist texture, and a zone plate
    /// whose frequency climbs towards the corner.
    fn scene(width: usize, height: usize, detail: bool) -> Vec<f32> {
        let mut rgb = vec![0f32; width * height * 3];
        for y in 0..height {
            for x in 0..width {
                let (fx, fy) = (x as f32 / width as f32, y as f32 / height as f32);
                let mut l = 0.45 + 0.20 * fx - 0.10 * fy
                    + 0.10 * (std::f32::consts::TAU * (fx + 0.5 * fy)).sin();
                if detail {
                    l += 0.05 * (x as f32 * 0.9).sin() * (y as f32 * 1.1).sin();
                    if x < width / 2 && y < height / 2 {
                        let (dx, dy) = (x as f32, y as f32);
                        l = 0.5 + 0.32 * (0.006 * (dx * dx + dy * dy)).sin();
                    }
                    if (width / 2..width * 3 / 4).contains(&x)
                        && (height / 8..height * 3 / 8).contains(&y)
                    {
                        l = 0.88;
                    }
                    if (x as f32) > 0.35 * width as f32 + 0.7 * y as f32 {
                        l = 0.12;
                    }
                    if y > height * 7 / 8 {
                        l = 0.75;
                    }
                }
                // Slowly varying chromaticity, and — in the detailed
                // scene — two patches where the colour itself changes.
                // A demosaic has to survive those too, they are just
                // rarer in photographs than luminance edges.
                let (mut cr, mut cb) = (0.85 + 0.35 * fx, 1.15 - 0.35 * fy);
                if detail {
                    if (width / 8..width / 4).contains(&x)
                        && (height / 2..height * 3 / 4).contains(&y)
                    {
                        cr = 1.6;
                        cb = 0.4;
                    }
                    if (width / 3..width / 2).contains(&x)
                        && (height * 5 / 8..height * 7 / 8).contains(&y)
                    {
                        cr = 0.4;
                        cb = 1.7;
                    }
                }
                let pixel = [
                    (l * cr).clamp(0.0, 1.0),
                    l.clamp(0.0, 1.0),
                    (l * cb).clamp(0.0, 1.0),
                ];
                rgb[(y * width + x) * 3..(y * width + x) * 3 + 3].copy_from_slice(&pixel);
            }
        }
        rgb
    }

    /// Sample a full-colour image through a filter array.
    fn mosaic(rgb: &[f32], width: usize, height: usize, cfa: &Cfa) -> Vec<f32> {
        (0..width * height)
            .map(|i| {
                let color = cfa.color_at(i % width, i / width).expect("test pattern");
                rgb[i * 3 + code_of(color).expect("test colour") as usize]
            })
            .collect()
    }

    /// Peak signal to noise over the interior, ignoring `margin`
    /// pixels of frame edge (every algorithm is degraded there and the
    /// numbers would say more about the border policy than the
    /// interpolation).
    fn psnr(truth: &[f32], test: &[f32], width: usize, height: usize, margin: usize) -> f32 {
        let mut sum = 0f64;
        let mut n = 0u64;
        for y in margin..height - margin {
            for x in margin..width - margin {
                for c in 0..3 {
                    let d = (truth[(y * width + x) * 3 + c] - test[(y * width + x) * 3 + c]) as f64;
                    sum += d * d;
                    n += 1;
                }
            }
        }
        (10.0 * (1.0 / (sum / n as f64)).log10()) as f32
    }

    /// The Fujifilm X-Trans array, taken from the 36-byte CFA table in
    /// the header of a real X-T10 RAF (metadata tag 0x131, 0 = red,
    /// 1 = green, 2 = blue): twenty greens, eight each of red and
    /// blue, with the 2x2 green blocks that give the array its name.
    fn xtrans() -> Cfa {
        #[rustfmt::skip]
        const CODES: [[u8; 6]; 6] = [
            [2, 1, 1, 0, 1, 1],
            [0, 1, 1, 2, 1, 1],
            [1, 2, 0, 1, 0, 2],
            [0, 1, 1, 2, 1, 1],
            [2, 1, 1, 0, 1, 1],
            [1, 0, 2, 1, 2, 0],
        ];
        Cfa::XTrans(std::array::from_fn(|y| {
            std::array::from_fn(|x| match CODES[y][x] {
                0 => CfaColor::Red,
                1 => CfaColor::Green,
                _ => CfaColor::Blue,
            })
        }))
    }

    fn report(name: &str, cfa: &Cfa, edges: bool) -> (f32, f32) {
        let (w, h) = (192, 160);
        let truth = scene(w, h, edges);
        let plane = mosaic(&truth, w, h, cfa);
        let fast = demosaic(&plane, w, h, cfa, Quality::Fast).expect("fast");
        let best = demosaic(&plane, w, h, cfa, Quality::Best).expect("best");
        let (a, b) = (psnr(&truth, &fast, w, h, 6), psnr(&truth, &best, w, h, 6));
        println!("{name}: fast {a:.2} dB, best {b:.2} dB");
        (a, b)
    }

    #[test]
    fn bayer_smooth_scene() {
        for (name, cfa) in [
            ("RGGB", Cfa::RGGB),
            ("BGGR", Cfa::BGGR),
            ("GRBG", Cfa::GRBG),
            ("GBRG", Cfa::GBRG),
        ] {
            let (fast, best) = report(name, &cfa, false);
            assert!(fast > 80.0, "{name} bilinear on a smooth scene: {fast} dB");
            assert!(
                best > 88.0,
                "{name} Hamilton-Adams on a smooth scene: {best} dB"
            );
            assert!(
                best > fast,
                "{name}: best {best} dB is no better than fast {fast} dB"
            );
        }
    }

    #[test]
    fn bayer_edges() {
        for (name, cfa) in [
            ("RGGB", Cfa::RGGB),
            ("BGGR", Cfa::BGGR),
            ("GRBG", Cfa::GRBG),
            ("GBRG", Cfa::GBRG),
        ] {
            let (fast, best) = report(name, &cfa, true);
            assert!(fast > 25.0, "{name} bilinear on edges: {fast} dB");
            // The whole point of a directional method: several
            // decibels of it, exactly where bilinear smears across
            // the edge instead of along it.
            assert!(
                best > fast + 4.0,
                "{name}: best {best} dB over fast {fast} dB"
            );
        }
    }

    #[test]
    fn xtrans_scene() {
        let (fast, best) = report("x-trans smooth", &xtrans(), false);
        assert!(fast > 60.0, "x-trans weighted on a smooth scene: {fast} dB");
        assert!(
            best > fast + 5.0,
            "x-trans: best {best} dB is no better than fast {fast} dB"
        );
        let (fast, best) = report("x-trans edges", &xtrans(), true);
        assert!(fast > 22.0, "x-trans weighted on edges: {fast} dB");
        assert!(
            best > fast + 5.0,
            "x-trans edges: best {best} dB is no better than fast {fast} dB"
        );
    }

    /// An RGB `Cfa::Pattern` goes down the generic path and must still
    /// reconstruct the scene; here the very same 2x2 array as
    /// `Cfa::RGGB`, so the two paths are directly comparable.
    #[test]
    fn generic_pattern() {
        let cfa = Cfa::Pattern {
            width: 2,
            height: 2,
            colors: vec![
                CfaColor::Red,
                CfaColor::Green,
                CfaColor::Green,
                CfaColor::Blue,
            ],
        };
        let (fast, best) = report("pattern RGGB", &cfa, false);
        assert!(fast > 80.0, "generic weighted: {fast} dB");
        assert!(
            best > fast + 5.0,
            "generic: best {best} dB is no better than fast {fast} dB"
        );
    }

    /// A Bayer array that is not one — the four phases the algorithm
    /// knows are the only ones it may assume — falls back to the
    /// generic path instead of interpolating nonsense.
    #[test]
    fn odd_bayer_falls_back() {
        let cfa = Cfa::Bayer([
            CfaColor::Red,
            CfaColor::Green,
            CfaColor::Blue,
            CfaColor::Green,
        ]);
        let (w, h) = (32, 32);
        let plane = vec![0.5f32; w * h];
        let out = demosaic(&plane, w, h, &cfa, Quality::Best).expect("odd bayer");
        assert!(out.iter().all(|v| (*v - 0.5).abs() < 1e-5));
    }

    /// A flat field must come out flat under every path: any
    /// interpolation of a constant is that constant.
    #[test]
    fn flat_field_stays_flat() {
        for cfa in [Cfa::RGGB, Cfa::GBRG, xtrans()] {
            for quality in [Quality::Fast, Quality::Best] {
                let (w, h) = (37, 41);
                let plane = vec![0.25f32; w * h];
                let out = demosaic(&plane, w, h, &cfa, quality).expect("flat");
                let worst = out.iter().map(|v| (*v - 0.25).abs()).fold(0.0f32, f32::max);
                assert!(worst < 1e-5, "{cfa:?} {quality:?} drifted by {worst}");
            }
        }
    }

    /// Sizes smaller than the working border, odd sizes, and single
    /// rows: the padded plane is built by whole CFA periods, so none
    /// of these may panic.
    #[test]
    fn small_frames_do_not_panic() {
        for (w, h) in [(1, 1), (2, 2), (1, 9), (9, 1), (3, 5), (6, 6), (13, 7)] {
            for cfa in [Cfa::RGGB, xtrans()] {
                for quality in [Quality::Fast, Quality::Best] {
                    let plane: Vec<f32> = (0..w * h).map(|i| (i % 17) as f32 / 17.0).collect();
                    let out = demosaic(&plane, w, h, &cfa, quality).expect("small frame");
                    assert_eq!(out.len(), w * h * 3);
                    assert!(out.iter().all(|v| v.is_finite()));
                }
            }
        }
    }

    #[test]
    fn rejects_bad_input() {
        assert!(matches!(
            demosaic(&[0.0; 3], 2, 2, &Cfa::RGGB, Quality::Fast),
            Err(crate::Error::Corrupt(_))
        ));
        assert!(matches!(
            demosaic(&[0.0; 4], 2, 2, &Cfa::None, Quality::Fast),
            Err(crate::Error::Unsupported(_))
        ));
        // CMYG: no 3x3 matrix can describe it, so it is refused rather
        // than guessed at.
        let cmyg = Cfa::Pattern {
            width: 2,
            height: 2,
            colors: vec![
                CfaColor::Cyan,
                CfaColor::Magenta,
                CfaColor::Yellow,
                CfaColor::Green,
            ],
        };
        assert!(matches!(
            demosaic(&[0.0; 4], 2, 2, &cmyg, Quality::Fast),
            Err(crate::Error::Unsupported(_))
        ));
    }
}
