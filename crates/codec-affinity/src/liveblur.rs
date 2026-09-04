//! The live blur filters — `FlRN` nodes under Pixel ▸ New Live Filter
//! Layer ▸ Blur, which soften a layer's own pixels in place.
//!
//! Every size convention here was fitted against Affinity's own render
//! of the RGB-cube card, over the card's interior so the canvas edge
//! stays out of it (`fixtures/affinity-probe/lf_*.af`; the derivations
//! are in the "Live filters" section of `docs/affinity-format.md`).
//! The residuals below are whole-card RMS against Affinity's render:
//!
//! | filter | convention | RMS |
//! | --- | --- | --- |
//! | `RMBC` maximum | square window half-width `Radi` | 0.00 |
//! | `RMeB` median | square window half-width `Radi` | 0.00 |
//! | `D&SC` dust | that median, gated by `Tole` | 0.00 |
//! | `RUSC` unsharp | `Fact` × the detail a σ = `Radi`/3 blur drops | 0.08–2.7 |
//! | `RHPC` high pass | mid grey plus half that same detail | 0.59 |
//! | `RGBC` gaussian | three box passes, σ = `Radi`/3 | 0.19–0.28 |
//! | `RBBC` box | window half-width is `Radi` | 0.22 |
//! | `RMoB` motion | a line 2·`Radi` long at −`Angl` | 0.40 |
//! | `RRaB` radial | a sweep of ±`Angl` about `Cent` | 0.56 |
//!
//! All of them blur premultiplied and treat everything off the layer as
//! transparent, which is what Affinity does: a blurred 512² card comes
//! back with alpha 132 down its edge and 68 in its corners, the fraction
//! of each kernel that was still on the page.

use rayon::prelude::*;

/// One live blur, its sizes already in the layer's own pixels.
#[derive(Debug, Clone, Copy)]
pub(crate) enum LiveBlur {
    /// `RGBC`. `Radi` is three times the panel's radius in pixels, and
    /// the standard deviation is that panel figure — a third of it.
    Gaussian { radius: f64 },
    /// `RBBC`, a plain square average.
    Box { radius: f64 },
    /// `RMoB`. `Angl` is in radians here — unlike the twirl's, which is
    /// in degrees — and turns anticlockwise on screen.
    Motion { radius: f64, angle_rad: f64 },
    /// `RRaB`, a spin about `Cent`. `Angl` is the half-sweep in degrees.
    Radial { cx: f64, cy: f64, angle_deg: f64 },
    /// `RMBC`, a dilation. `Circ` swaps the square window for a disc;
    /// only the square is measured.
    Maximum { radius: f64, circular: bool },
    /// `RMeB`, a square-window median — the one blur that keeps edges.
    Median { radius: f64 },
    /// `D&SC`: the median again, but only where the pixel is further
    /// from it than `Tole`. At `Tole` 0 it *is* the median — the two
    /// probes come back byte-identical — and `Chan` decides whether the
    /// test is per channel or on the pixel as a whole.
    DustAndScratches {
        radius: f64,
        tolerance: f64,
        per_channel: bool,
    },
    /// `RHPC`: mid grey plus half of what the same blur threw away.
    /// `Mono` is false in the only probe; the usual reading, that it
    /// takes the detail off the luminosity alone, is what runs.
    HighPass { radius: f64, mono: bool },
    /// `RUSC`, the one that sharpens: the layer plus `Fact` times what
    /// blurring it by `Radi` threw away. `Thrs` is a fraction of full
    /// scale below which a difference is left alone; every probe has it
    /// at zero, so that reading is the standard one and untested.
    Unsharp {
        radius: f64,
        factor: f64,
        threshold: f64,
    },
}

/// How Affinity's live Gaussian `Radi` converts to a standard deviation.
///
/// Affinity's is not a true Gaussian: three box passes fit the `Radi` 30
/// probe (0.33 RMS) where the best-fitting Gaussian manages only 0.84,
/// and once the model is right the constant is exactly a third — the
/// panel's radius in pixels, since `Radi` is three times what the box
/// says. Three passes of a box 2σ wide have standard deviation σ. The
/// larger probe wants 0.29 rather than a third, which is Affinity
/// working on something coarser than full resolution at radius 90;
/// over the card's interior a third costs it 0.47 RMS instead of 0.31.
///
/// This is *not* the layer-effect blur's [`crate::BLUR_RADI`]: a live
/// blur and a `Gaus` effect scale `Radi` differently.
const GAUSSIAN_RADI: f64 = 1.0 / 3.0;

/// The unsharp mask's own radius convention, which turns out to be the
/// same third: `RUSC` `Radi` 5, 10 and 20 fit box widths 3.3, 6.7 and
/// 12.0, so σ = `Radi`/3 with the same drift at the top.
const UNSHARP_RADI: f64 = 1.0 / 3.0;

pub(crate) fn apply(width: u32, height: u32, pixels: &[u8], blur: &LiveBlur) -> Vec<u8> {
    let (w, h) = (width as usize, height as usize);
    if w == 0 || h == 0 {
        return pixels.to_vec();
    }
    match *blur {
        LiveBlur::Gaussian { radius } => {
            if radius < 1.0 {
                return pixels.to_vec();
            }
            separable(w, h, pixels, &box3_kernel(2.0 * radius * GAUSSIAN_RADI))
        }
        LiveBlur::Box { radius } => {
            let r = radius.round().max(0.0) as usize;
            if r == 0 {
                return pixels.to_vec();
            }
            separable(w, h, pixels, &vec![1.0 / (2 * r + 1) as f32; 2 * r + 1])
        }
        LiveBlur::Unsharp {
            radius,
            factor,
            threshold,
        } => unsharp(w, h, pixels, radius, factor, threshold),
        LiveBlur::HighPass { radius, mono } => high_pass(w, h, pixels, radius, mono),
        LiveBlur::Motion { radius, angle_rad } => {
            let length = 2.0 * radius;
            if length < 1.0 {
                return pixels.to_vec();
            }
            // Screen y runs down, so the panel's anticlockwise angle is
            // a clockwise one here.
            let (dx, dy) = ((-angle_rad).cos(), (-angle_rad).sin());
            let taps = (length.round() as usize).max(1) | 1;
            let offsets: Vec<(f64, f64)> = (0..taps)
                .map(|i| {
                    let t = -length / 2.0 + length * i as f64 / (taps - 1).max(1) as f64;
                    (t * dx, t * dy)
                })
                .collect();
            gather(w, h, pixels, |x, y| {
                offsets.iter().map(|(ox, oy)| (x + ox, y + oy)).collect()
            })
        }
        LiveBlur::Radial { cx, cy, angle_deg } => {
            if angle_deg.abs() < 0.01 {
                return pixels.to_vec();
            }
            let sweep = angle_deg.to_radians();
            // One tap per half pixel of arc at the far corner, so the
            // longest streak in the layer is still sampled densely.
            let far = [
                (0.0, 0.0),
                (w as f64, 0.0),
                (0.0, h as f64),
                (w as f64, h as f64),
            ]
            .iter()
            .map(|(x, y)| (x - cx).hypot(y - cy))
            .fold(0.0, f64::max);
            let taps = ((sweep.abs() * far * 4.0).ceil() as usize).clamp(3, 2048) | 1;
            gather(w, h, pixels, |x, y| {
                let (dx, dy) = (x - cx, y - cy);
                let r = dx.hypot(dy);
                let th = dy.atan2(dx);
                (0..taps)
                    .map(|i| {
                        let a = th - sweep + 2.0 * sweep * i as f64 / (taps - 1) as f64;
                        (cx + r * a.cos(), cy + r * a.sin())
                    })
                    .collect()
            })
        }
        LiveBlur::Maximum { radius, circular } => {
            let r = radius.round().max(0.0) as i64;
            if r == 0 {
                return pixels.to_vec();
            }
            if circular {
                window_max_disc(w, h, pixels, r)
            } else {
                // A square window's maximum is separable.
                let rows = window_max_1d(w, h, pixels, r, true);
                window_max_1d(w, h, &rows, r, false)
            }
        }
        LiveBlur::DustAndScratches {
            radius,
            tolerance,
            per_channel,
        } => {
            let r = radius.round().max(0.0) as i64;
            if r == 0 {
                return pixels.to_vec();
            }
            let med = median(w, h, pixels, r);
            let cut = (tolerance * 255.0) as i32;
            let mut out = pixels.to_vec();
            for ((px, base), m) in out
                .as_chunks_mut::<4>()
                .0
                .iter_mut()
                .zip(pixels.as_chunks::<4>().0)
                .zip(med.as_chunks::<4>().0)
            {
                let apart = |c: usize| (base[c] as i32 - m[c] as i32).abs();
                if per_channel {
                    for (c, v) in px.iter_mut().enumerate() {
                        if apart(c) > cut {
                            *v = m[c];
                        }
                    }
                } else if (0..4).map(apart).max().unwrap_or(0) > cut {
                    *px = *m;
                }
            }
            out
        }
        LiveBlur::Median { radius } => {
            let r = radius.round().max(0.0) as i64;
            if r == 0 {
                return pixels.to_vec();
            }
            median(w, h, pixels, r)
        }
    }
}

/// A box `width` wide, with the two end taps carrying the fraction that
/// makes it exactly that wide.
fn box_kernel(width: f64) -> Vec<f32> {
    let half = (width / 2.0).max(0.0);
    let n = (half.floor() as usize).min(1 << 16);
    let frac = (half - n as f64) as f32;
    let mut k = vec![1.0f32; 2 * n + 3];
    k[0] = frac;
    k[2 * n + 2] = frac;
    let sum: f32 = k.iter().sum();
    for v in &mut k {
        *v /= sum;
    }
    k
}

/// Three of those boxes convolved into one kernel — the quadratic
/// B-spline Affinity's Gaussian actually is.
///
/// Convolving them first and running one pass is not the same as
/// running three: a pass only writes pixels that are on the layer, so
/// three of them throw away the mass that spilled over the edge and
/// leave the border darker than it should be. Affinity's does not —
/// a blurred full-page card comes back at alpha 132 down its edge,
/// the half of a symmetric kernel that is still on the page — so the
/// composite is the one to run.
fn box3_kernel(width: f64) -> Vec<f32> {
    let k = box_kernel(width);
    let convolve = |a: &[f32], b: &[f32]| -> Vec<f32> {
        let mut out = vec![0.0f32; a.len() + b.len() - 1];
        for (i, x) in a.iter().enumerate() {
            for (j, y) in b.iter().enumerate() {
                out[i + j] += x * y;
            }
        }
        out
    };
    let twice = convolve(&k, &k);
    convolve(&twice, &k)
}

/// Premultiplied RGBA as floats, so several passes can run without
/// rounding to bytes in between.
fn to_premultiplied(pixels: &[u8]) -> Vec<f32> {
    let mut out = vec![0.0f32; pixels.len()];
    for (px, o) in pixels
        .as_chunks::<4>()
        .0
        .iter()
        .zip(out.as_chunks_mut::<4>().0)
    {
        let a = px[3] as f32;
        *o = [px[0] as f32 * a, px[1] as f32 * a, px[2] as f32 * a, a];
    }
    out
}

fn from_premultiplied(buf: &[f32]) -> Vec<u8> {
    let mut out = vec![0u8; buf.len()];
    for (acc, px) in buf
        .as_chunks::<4>()
        .0
        .iter()
        .zip(out.as_chunks_mut::<4>().0)
    {
        write_premultiplied(px, *acc);
    }
    out
}

/// One 1-D pass of `kernel` over premultiplied floats, treating
/// everything off the layer as transparent.
fn pass(w: usize, h: usize, src: &[f32], kernel: &[f32], horizontal: bool) -> Vec<f32> {
    let n = (kernel.len() / 2) as i64;
    let mut out = vec![0.0f32; src.len()];
    out.par_chunks_exact_mut(w * 4)
        .enumerate()
        .for_each(|(y, row)| {
            for (x, px) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                let mut acc = [0.0f32; 4];
                for (i, k) in kernel.iter().enumerate() {
                    let d = i as i64 - n;
                    let (sx, sy) = if horizontal {
                        (x as i64 + d, y as i64)
                    } else {
                        (x as i64, y as i64 + d)
                    };
                    if sx < 0 || sy < 0 || sx >= w as i64 || sy >= h as i64 {
                        continue;
                    }
                    let at = (sy as usize * w + sx as usize) * 4;
                    for (c, a) in acc.iter_mut().enumerate() {
                        *a += src[at + c] * k;
                    }
                }
                *px = acc;
            }
        });
    out
}

/// Convolve premultiplied RGBA with the same 1-D kernel down each axis.
fn separable(w: usize, h: usize, pixels: &[u8], kernel: &[f32]) -> Vec<u8> {
    let buf = to_premultiplied(pixels);
    let buf = pass(w, h, &buf, kernel, true);
    let buf = pass(w, h, &buf, kernel, false);
    from_premultiplied(&buf)
}

/// Average the premultiplied samples a filter gathers for each pixel.
fn gather<F>(w: usize, h: usize, pixels: &[u8], taps: F) -> Vec<u8>
where
    F: Fn(f64, f64) -> Vec<(f64, f64)> + Sync,
{
    let mut out = vec![0u8; w * h * 4];
    out.par_chunks_exact_mut(w * 4)
        .enumerate()
        .for_each(|(y, row)| {
            for (x, px) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                let points = taps(x as f64 + 0.5, y as f64 + 0.5);
                let mut acc = [0.0f32; 4];
                for (sx, sy) in &points {
                    let s = sample(w, h, pixels, *sx, *sy);
                    for (c, a) in acc.iter_mut().enumerate() {
                        *a += s[c];
                    }
                }
                let n = points.len().max(1) as f32;
                for a in &mut acc {
                    *a /= n;
                }
                write_premultiplied(px, acc);
            }
        });
    out
}

/// One bilinear premultiplied tap, transparent off the layer.
fn sample(w: usize, h: usize, pixels: &[u8], sx: f64, sy: f64) -> [f32; 4] {
    let fetch = |x: i64, y: i64| -> [f32; 4] {
        if x < 0 || y < 0 || x >= w as i64 || y >= h as i64 {
            return [0.0; 4];
        }
        let at = (y as usize * w + x as usize) * 4;
        let a = pixels[at + 3] as f32;
        [
            pixels[at] as f32 * a,
            pixels[at + 1] as f32 * a,
            pixels[at + 2] as f32 * a,
            a,
        ]
    };
    let (fx, fy) = (sx - 0.5, sy - 0.5);
    let (x0, y0) = (fx.floor(), fy.floor());
    let (tx, ty) = ((fx - x0) as f32, (fy - y0) as f32);
    let (x0, y0) = (x0 as i64, y0 as i64);
    let (p00, p10) = (fetch(x0, y0), fetch(x0 + 1, y0));
    let (p01, p11) = (fetch(x0, y0 + 1), fetch(x0 + 1, y0 + 1));
    let mut out = [0.0f32; 4];
    for (c, o) in out.iter_mut().enumerate() {
        let top = p00[c] + (p10[c] - p00[c]) * tx;
        let bot = p01[c] + (p11[c] - p01[c]) * tx;
        *o = top + (bot - top) * ty;
    }
    out
}

fn write_premultiplied(px: &mut [u8], acc: [f32; 4]) {
    let a = acc[3];
    px[3] = (a + 0.5).clamp(0.0, 255.0) as u8;
    if a > 0.0 {
        for c in 0..3 {
            px[c] = (acc[c] / a + 0.5).clamp(0.0, 255.0) as u8;
        }
    }
}

/// `RUSC`: put back `factor` times the detail the blur removed.
///
/// The comparison is on the encoded (sRGB) values, not linear light —
/// working in linear costs 4 RMS against Affinity's render where sRGB
/// costs 0.4 — and alpha is left alone.
fn unsharp(w: usize, h: usize, pixels: &[u8], radius: f64, factor: f64, threshold: f64) -> Vec<u8> {
    if radius < 1.0 || factor == 0.0 {
        return pixels.to_vec();
    }
    let blurred = separable(w, h, pixels, &box3_kernel(2.0 * radius * UNSHARP_RADI));
    let cut = (threshold * 255.0) as f32;
    let mut out = pixels.to_vec();
    for ((px, base), blur) in out
        .as_chunks_mut::<4>()
        .0
        .iter_mut()
        .zip(pixels.as_chunks::<4>().0)
        .zip(blurred.as_chunks::<4>().0)
    {
        for c in 0..3 {
            let detail = base[c] as f32 - blur[c] as f32;
            if detail.abs() <= cut {
                continue;
            }
            px[c] = (base[c] as f32 + factor as f32 * detail + 0.5).clamp(0.0, 255.0) as u8;
        }
    }
    out
}

/// `RHPC`: what the blur threw away, halved and hung off mid grey.
///
/// The half is measured, not assumed — fitting a free gain against the
/// probe gives 0.5004 — and the radius is the same third as the
/// Gaussian's and the unsharp mask's, which is what pins the model at
/// 0.40 RMS while a width either side of it costs 5.
fn high_pass(w: usize, h: usize, pixels: &[u8], radius: f64, mono: bool) -> Vec<u8> {
    if radius < 1.0 {
        return pixels.to_vec();
    }
    let blurred = separable(w, h, pixels, &box3_kernel(2.0 * radius * GAUSSIAN_RADI));
    let mut out = pixels.to_vec();
    for ((px, base), blur) in out
        .as_chunks_mut::<4>()
        .0
        .iter_mut()
        .zip(pixels.as_chunks::<4>().0)
        .zip(blurred.as_chunks::<4>().0)
    {
        let detail = |c: usize| base[c] as f32 - blur[c] as f32;
        let flat = (detail(0) + detail(1) + detail(2)) / 3.0;
        for (c, v) in px[..3].iter_mut().enumerate() {
            let d = if mono { flat } else { detail(c) };
            *v = (127.5 + d * 0.5).round_ties_even().clamp(0.0, 255.0) as u8;
        }
    }
    out
}

/// One pass of a square-window maximum, along rows or down columns.
fn window_max_1d(w: usize, h: usize, pixels: &[u8], r: i64, horizontal: bool) -> Vec<u8> {
    let mut out = vec![0u8; w * h * 4];
    out.par_chunks_exact_mut(w * 4)
        .enumerate()
        .for_each(|(y, row)| {
            for (x, px) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                let mut best = [0u8; 4];
                for d in -r..=r {
                    let (sx, sy) = if horizontal {
                        (x as i64 + d, y as i64)
                    } else {
                        (x as i64, y as i64 + d)
                    };
                    if sx < 0 || sy < 0 || sx >= w as i64 || sy >= h as i64 {
                        continue;
                    }
                    let at = (sy as usize * w + sx as usize) * 4;
                    for c in 0..4 {
                        best[c] = best[c].max(pixels[at + c]);
                    }
                }
                *px = best;
            }
        });
    out
}

/// The `Circ` variant: a disc window, which does not separate.
fn window_max_disc(w: usize, h: usize, pixels: &[u8], r: i64) -> Vec<u8> {
    let offsets: Vec<(i64, i64)> = (-r..=r)
        .flat_map(|dy| (-r..=r).map(move |dx| (dx, dy)))
        .filter(|(dx, dy)| dx * dx + dy * dy <= r * r)
        .collect();
    let mut out = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let mut best = [0u8; 4];
            for (dx, dy) in &offsets {
                let (sx, sy) = (x as i64 + dx, y as i64 + dy);
                if sx < 0 || sy < 0 || sx >= w as i64 || sy >= h as i64 {
                    continue;
                }
                let at = (sy as usize * w + sx as usize) * 4;
                for c in 0..4 {
                    best[c] = best[c].max(pixels[at + c]);
                }
            }
            out[(y * w + x) * 4..][..4].copy_from_slice(&best);
        }
    }
    out
}

/// Square-window median, per channel.
///
/// Off the layer every channel counts as zero rather than dropping out,
/// which is why Affinity leaves a darker band down the border: at radius
/// 8 the top row's window is 136 phantom zeros out of 289, so the answer
/// there is the ninth smallest real value rather than the middle one,
/// and in a corner the phantoms are the majority and the pixel comes
/// back transparent. Counting them keeps the window a fixed 289 samples,
/// which is also what makes the sliding histogram below simple.
///
/// That histogram is Huang's: rebuilt once per row, then slid along it a
/// column at a time while the median index walks to its new place, so
/// the cost per pixel is the window's width and not its area.
fn median(w: usize, h: usize, pixels: &[u8], r: i64) -> Vec<u8> {
    let mut out = vec![0u8; w * h * 4];
    let full = ((2 * r + 1) * (2 * r + 1)) as u32;
    // The rank we want, 1-based: for an odd window the middle sample,
    // for an even one (which only the phantoms can make) the lower of
    // the two middles.
    let rank = full.div_ceil(2);
    let at = |x: i64, y: i64, c: usize| -> usize {
        if x < 0 || y < 0 || x >= w as i64 || y >= h as i64 {
            0
        } else {
            pixels[(y as usize * w + x as usize) * 4 + c] as usize
        }
    };
    out.par_chunks_exact_mut(w * 4)
        .enumerate()
        .for_each(|(row_y, row)| {
            let y = row_y as i64;
            for c in 0..4 {
                let mut hist = [0u32; 256];
                for dy in -r..=r {
                    for dx in -r..=r {
                        hist[at(dx, y + dy, c)] += 1;
                    }
                }
                // `below` counts the samples strictly under `med`, so
                // the median is where the running total first reaches
                // `rank`.
                let (mut med, mut below) = (0usize, 0u32);
                while below + hist[med] < rank {
                    below += hist[med];
                    med += 1;
                }
                row[c] = med as u8;
                for x in 1..w as i64 {
                    for dy in -r..=r {
                        let leave = at(x - r - 1, y + dy, c);
                        hist[leave] -= 1;
                        if leave < med {
                            below -= 1;
                        }
                        let enter = at(x + r, y + dy, c);
                        hist[enter] += 1;
                        if enter < med {
                            below += 1;
                        }
                    }
                    while below >= rank {
                        med -= 1;
                        below -= hist[med];
                    }
                    while below + hist[med] < rank {
                        below += hist[med];
                        med += 1;
                    }
                    row[x as usize * 4 + c] = med as u8;
                }
            }
        });
    out
}
