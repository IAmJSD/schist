//! From sensor data to linear sRGB.
//!
//! The classic pipeline: subtract the black level, scale so the white
//! level is 1.0, apply the white balance multipliers, interpolate the
//! CFA, convert camera colour to sRGB through the camera matrix (with
//! the white balance folded in the way DNG describes, so a neutral
//! stays neutral), clip, then crop and orient. Output is linear light;
//! the caller applies a tone curve and encoding.

use crate::demosaic::demosaic;
use crate::{frame_samples, Cfa, CfaColor, Error, Orientation, RawData, RawImage, Rect, Result};
use rayon::prelude::*;

#[derive(Debug, Clone, PartialEq)]
pub struct DevelopOptions {
    pub quality: crate::demosaic::Quality,
    /// Multipliers to use instead of the file's as-shot ones.
    pub white_balance: Option<[f32; 4]>,
    /// Apply `raw.crop`. Off yields the whole frame, masked borders and
    /// all — for tests against an oracle's uncropped output.
    pub crop: bool,
    /// Apply `raw.orientation`.
    pub orient: bool,
    /// Skip the matrix and return white-balanced camera RGB, for
    /// cameras with no matrix (the caller decides what to do).
    pub camera_rgb: bool,
}

impl Default for DevelopOptions {
    fn default() -> Self {
        DevelopOptions {
            quality: Default::default(),
            white_balance: None,
            crop: true,
            orient: true,
            camera_rgb: false,
        }
    }
}

/// Linear sRGB, three floats a pixel, nominally 0..=1 (specular
/// highlights and out-of-gamut colours may exceed it).
#[derive(Debug, Clone, PartialEq)]
pub struct Developed {
    pub width: usize,
    pub height: usize,
    pub rgb: Vec<f32>,
}

pub fn develop(raw: &RawImage, options: &DevelopOptions) -> Result<Developed> {
    // Everything below indexes `data` from the declared dimensions and
    // the crop, so the invariants have to hold before any of it runs.
    raw.validate()?;
    // `validate` pairs one sample a pixel with a filter array and
    // three with none, but says nothing about the counts in between;
    // nothing here knows what a two- or four-sample frame would mean.
    if raw.cpp != 1 && raw.cpp != 3 {
        return Err(Error::Unsupported(format!(
            "develop: {} samples a pixel",
            raw.cpp
        )));
    }
    let (width, height) = (raw.width, raw.height);
    let multipliers = white_balance(raw, options);

    // Steps (a) and (b): one pass turning sensor units into
    // white-balanced 0..1 scene-linear numbers. `Levels` holds the
    // per-CFA-position black and the combined 1/(white-black) and
    // white balance factor, so this costs one subtract and one
    // multiply a sample.
    let mut rgb = if raw.cpp == 3 {
        // Linear DNG, Foveon and Canon sRAW arrive with three samples
        // a pixel: no interpolation, levels and multipliers per
        // channel.
        let levels = Levels::per_channel(raw, multipliers)?;
        let mut rgb = vec![0f32; width * height * 3];
        let stride = width * 3;
        match &raw.data {
            RawData::U16(v) => rgb.par_chunks_mut(stride).enumerate().for_each(|(y, row)| {
                normalise_pixels(row, &v[y * stride..(y + 1) * stride], &levels)
            }),
            RawData::F32(v) => rgb.par_chunks_mut(stride).enumerate().for_each(|(y, row)| {
                normalise_pixels(row, &v[y * stride..(y + 1) * stride], &levels)
            }),
        }
        rgb
    } else {
        let levels = Levels::per_position(raw, multipliers)?;
        let plane = normalised_plane(raw, &levels);
        if let Cfa::SuperCcd {
            row_staggered,
            fuji_width,
            ..
        } = &raw.cfa
        {
            // A SuperCCD's stored rectangle is not the picture; the
            // photosites have to be re-indexed before interpolation
            // and rotated after it, and the crop is consumed on the
            // way. Its own pipeline from here on.
            return develop_super_ccd(raw, options, &plane, *row_staggered, *fuji_width);
        }
        // Step (c).
        demosaic(&plane, width, height, &raw.cfa, options.quality)?
    };

    let matrix = srgb_matrix(raw, options);

    // Steps (e) and (f). The crop happens after the interpolation, so
    // the CFA phase never has to be adjusted: the demosaic saw the
    // whole frame with the pattern anchored where the decoder said it
    // was, and what comes out is plain RGB that can be cut anywhere.
    // It also means the crop's edge pixels have real neighbours
    // instead of the frame's own extended border.
    let crop = if options.crop {
        raw.crop
    } else {
        Rect {
            x: 0,
            y: 0,
            width,
            height,
        }
    };
    let (mut out_w, mut out_h) = (crop.width, crop.height);
    if crop.x == 0 && crop.y == 0 && crop.width == width && crop.height == height {
        rgb.par_chunks_mut(width * 3)
            .for_each(|row| finish_row(row, matrix.as_ref()));
    } else {
        let mut cropped = vec![0f32; crop.width * crop.height * 3];
        cropped
            .par_chunks_mut(crop.width * 3)
            .enumerate()
            .for_each(|(y, row)| {
                let start = ((y + crop.y) * width + crop.x) * 3;
                row.copy_from_slice(&rgb[start..start + crop.width * 3]);
                finish_row(row, matrix.as_ref());
            });
        rgb = cropped;
    }

    // Step (g).
    if options.orient && raw.orientation != Orientation::Normal {
        rgb = orient(&rgb, out_w, out_h, raw.orientation);
        if raw.orientation.transposes() {
            std::mem::swap(&mut out_w, &mut out_h);
        }
    }
    Ok(Developed {
        width: out_w,
        height: out_h,
        rgb,
    })
}

/// Step (d)'s matrix: camera RGB to linear sRGB. No matrix (or the
/// caller asked for camera RGB) leaves the numbers in the camera's own
/// space, which is still white balanced and so still neutral-correct,
/// just not colour-correct.
fn srgb_matrix(raw: &RawImage, options: &DevelopOptions) -> Option<[[f32; 3]; 3]> {
    if options.camera_rgb {
        None
    } else {
        raw.color_matrix.as_ref().map(camera_to_srgb)
    }
}

/// Steps (a) and (b) for one sample a pixel: the whole frame as
/// white-balanced 0..1 scene-linear numbers, still under the filter
/// array.
fn normalised_plane(raw: &RawImage, levels: &Levels) -> Vec<f32> {
    let (width, height) = (raw.width, raw.height);
    let mut plane = vec![0f32; width * height];
    match &raw.data {
        RawData::U16(v) => plane
            .par_chunks_mut(width)
            .enumerate()
            .for_each(|(y, row)| normalise_row(row, &v[y * width..(y + 1) * width], levels, y)),
        RawData::F32(v) => plane
            .par_chunks_mut(width)
            .enumerate()
            .for_each(|(y, row)| normalise_row(row, &v[y * width..(y + 1) * width], levels, y)),
    }
    plane
}

/// Steps (c) to (g) for a SuperCCD frame (see [`Cfa::SuperCcd`]):
/// shear the active photosites into a square Bayer frame, interpolate
/// that, rotate it back by 45 degrees, then the matrix and the
/// orientation as for everything else.
///
/// The crop is always applied. The stored rectangle's padding (the
/// FinePix bodies fill it with a constant, the DBP back with masked
/// black) has no place on the lattice: the shear is defined on the
/// active rectangle alone, and the rotation's output covers exactly
/// the photosites, so there is no "uncropped" picture to offer.
///
/// White level: the file's (or the table's) value is used as it is.
/// The reference developer lowers it to the frame's brightest sample
/// when that sits between three quarters of the level and the level —
/// an empirical exposure policy, not a property of the format — and
/// this crate does not adopt it: a highlight that fell short of
/// saturation stays short, as it does for every other camera here.
/// Against the reference's output that is a uniform 1.7 % on the
/// FinePix S9600 sample (white 15872 against its brightest sample
/// 15604) and nothing on the DBP, whose frame saturates.
fn develop_super_ccd(
    raw: &RawImage,
    options: &DevelopOptions,
    plane: &[f32],
    row_staggered: bool,
    fuji_width: usize,
) -> Result<Developed> {
    let crop = raw.crop;
    let bayer = raw.cfa.super_ccd_bayer((crop.x, crop.y)).ok_or_else(|| {
        Error::Corrupt("develop: SuperCCD colours do not form a Bayer lattice".into())
    })?;
    let sheared = super_ccd_shear(plane, raw.width, crop, row_staggered, fuji_width)?;
    let rgb = demosaic(
        &sheared.plane,
        sheared.width,
        sheared.height,
        &Cfa::Bayer(bayer),
        options.quality,
    )?;
    drop(sheared.plane);
    let (mut rgb, mut out_w, mut out_h) =
        super_ccd_rotate(&rgb, sheared.width, sheared.height, fuji_width)?;
    let matrix = srgb_matrix(raw, options);
    rgb.par_chunks_mut(out_w * 3)
        .for_each(|row| finish_row(row, matrix.as_ref()));
    if options.orient && raw.orientation != Orientation::Normal {
        rgb = orient(&rgb, out_w, out_h, raw.orientation);
        if raw.orientation.transposes() {
            std::mem::swap(&mut out_w, &mut out_h);
        }
    }
    Ok(Developed {
        width: out_w,
        height: out_h,
        rgb,
    })
}

/// A SuperCCD's photosites re-indexed into the sheared frame.
pub(crate) struct Sheared {
    /// `width * height` cells; the ones no photosite maps to are zero.
    pub plane: Vec<f32>,
    pub width: usize,
    pub height: usize,
    /// How many cells received a photosite — the active rectangle's
    /// area when the geometry is consistent. Checked by the tests.
    #[allow(dead_code)]
    pub written: usize,
}

/// The sheared frame's dimensions for an active rectangle `height`
/// rows tall: one row of cells per anti-diagonal, and `fuji_width`
/// cells across each, so `(height >> stagger) + fuji_width` wide and
/// one less tall.
pub(crate) fn super_ccd_sheared_size(
    row_staggered: bool,
    fuji_width: usize,
    height: usize,
) -> Result<(usize, usize)> {
    let width = (height >> usize::from(row_staggered))
        .checked_add(fuji_width)
        .ok_or_else(|| Error::Corrupt("develop: SuperCCD frame too large".into()))?;
    if fuji_width == 0 || width < 2 {
        return Err(Error::Corrupt(format!(
            "develop: SuperCCD fuji width {fuji_width} over {height} rows"
        )));
    }
    let sheared_height = width - 1;
    let cells = frame_samples(width, sheared_height, 1)?;
    // A real SuperCCD rectangle occupies almost exactly half of its
    // sheared bounding square. Allow considerably more slack for odd
    // dimensions, but reject forged aspect ratios that would turn a
    // modest stored frame into a multi-gigabyte intermediate.
    let active_width = fuji_width
        .checked_shl(u32::from(!row_staggered))
        .ok_or_else(|| Error::Corrupt("develop: SuperCCD active width overflow".into()))?;
    let active = frame_samples(active_width, height, 1)?;
    if cells > active.saturating_mul(3) {
        return Err(Error::Corrupt(format!(
            "develop: SuperCCD {active_width}x{height} lattice needs an implausible \
             {width}x{sheared_height} sheared frame"
        )));
    }
    // Demosaicing this plane is the next step, so validate that output
    // before either the sheared plane or its RGB expansion is allocated.
    frame_samples(width, sheared_height, 3)?;
    Ok((width, sheared_height))
}

/// Scatter the active rectangle of `plane` (`stride` samples a row)
/// into the sheared frame with [`super_ccd_cell`]. Written as a gather
/// — every cell inverts the map and asks whether a photosite lands on
/// it — so rows are independent and run in parallel.
///
/// [`super_ccd_cell`]: crate::super_ccd_cell
pub(crate) fn super_ccd_shear(
    plane: &[f32],
    stride: usize,
    crop: Rect,
    row_staggered: bool,
    fuji_width: usize,
) -> Result<Sheared> {
    // The active rectangle is exactly one stagger line wide: a stored
    // row of `fuji_width` photosites, or `2 * fuji_width` columns whose
    // pairs each hold one lattice column. Anything else is a decoder
    // that disagrees with itself about the geometry.
    let line = fuji_width
        .checked_shl(u32::from(!row_staggered))
        .filter(|w| *w == crop.width)
        .ok_or_else(|| {
            Error::Corrupt(format!(
                "develop: SuperCCD crop {} wide for a fuji width of {fuji_width}",
                crop.width
            ))
        })?;
    if crop.x.checked_add(line).is_none_or(|right| right > stride)
        || crop
            .y
            .checked_add(crop.height)
            .and_then(|bottom| bottom.checked_mul(stride))
            .is_none_or(|end| end > plane.len())
    {
        return Err(Error::Corrupt(
            "develop: SuperCCD crop outside the frame".into(),
        ));
    }
    let (width, height) = super_ccd_sheared_size(row_staggered, fuji_width, crop.height)?;
    let mut out = vec![0f32; frame_samples(width, height, 1)?];
    let fw = fuji_width as i64;
    let (active_w, active_h) = (crop.width as i64, crop.height as i64);
    let written = out
        .par_chunks_mut(width)
        .enumerate()
        .map(|(r, row)| {
            let r = r as i64;
            let mut n = 0usize;
            for (c, cell) in row.iter_mut().enumerate() {
                let c = c as i64;
                // Invert the forward map. The parity test is what
                // tells a cell with a photosite from an empty one: the
                // two interleave like a checkerboard along every
                // anti-diagonal.
                let (x, y) = if row_staggered {
                    let y = r + c - (fw - 1);
                    if y < 0 || y >= active_h {
                        continue;
                    }
                    let t = c - r + fw - 1 - (y & 1);
                    if t < 0 || t & 1 != 0 {
                        continue;
                    }
                    (t / 2, y)
                } else {
                    let x = c - r + fw - 1;
                    if x < 0 || x >= active_w {
                        continue;
                    }
                    let t = r + c - fw + 1 - (x & 1);
                    if t < 0 || t & 1 != 0 {
                        continue;
                    }
                    (x, t / 2)
                };
                if x >= active_w || y >= active_h {
                    continue;
                }
                *cell = plane[(crop.y + y as usize) * stride + crop.x + x as usize];
                n += 1;
            }
            n
        })
        .sum();
    Ok(Sheared {
        plane: out,
        width,
        height,
        written,
    })
}

/// The rotated picture's size: the sheared grid's cell diagonal is one
/// output pixel, so the lattice's `fuji_width - 1` cell widths span
/// `(fuji_width - 1) * sqrt 2` output columns, and the rows beyond the
/// first `fuji_width - 1` span the output rows likewise. Truncated,
/// as the reference truncates.
pub(crate) fn super_ccd_output_size(
    fuji_width: usize,
    sheared_height: usize,
) -> Result<(usize, usize)> {
    let s = 0.5f64.sqrt();
    let span = fuji_width
        .checked_sub(1)
        .ok_or_else(|| Error::Corrupt("develop: SuperCCD fuji width of zero".into()))?;
    let rows = sheared_height.checked_sub(span).ok_or_else(|| {
        Error::Corrupt("develop: SuperCCD sheared frame shorter than its fuji width".into())
    })?;
    let wide = (span as f64 / s) as usize;
    let high = (rows as f64 / s) as usize;
    if wide == 0 || high == 0 {
        return Err(Error::Corrupt(format!(
            "develop: SuperCCD picture of {wide}x{high}"
        )));
    }
    Ok((wide, high))
}

/// Where output pixel (`row`, `col`) samples the sheared frame, as
/// (row, column) in cells: the sheared frame's axes are the lattice's
/// diagonals, so a rigid 45-degree turn with unit scale, offset so the
/// first output row starts at cell row `fuji_width - 1`. Computed in
/// double and rounded to single, the fractions then taken in single,
/// which is the reference's arithmetic.
pub(crate) fn super_ccd_source(fuji_width: usize, row: usize, col: usize) -> (f32, f32) {
    let s = 0.5f64.sqrt();
    let span = fuji_width.saturating_sub(1) as f64;
    let (row, col) = (row as f64, col as f64);
    ((span + (row - col) * s) as f32, ((row + col) * s) as f32)
}

/// The interpolated sheared frame (`width * height * 3`) rotated back
/// by 45 degrees with bilinear sampling. An output pixel whose cell
/// has no neighbour to its right or below (past the frame's last row
/// or column) is left black; nothing else is masked, so the outer few
/// pixels — which sampled cells that were interpolated against the
/// empty region outside the parallelogram — are as the reference
/// leaves them.
pub(crate) fn super_ccd_rotate(
    rgb: &[f32],
    width: usize,
    height: usize,
    fuji_width: usize,
) -> Result<(Vec<f32>, usize, usize)> {
    if rgb.len() != frame_samples(width, height, 3)? {
        return Err(Error::Corrupt(
            "develop: sheared frame size mismatch".into(),
        ));
    }
    let (wide, high) = super_ccd_output_size(fuji_width, height)?;
    let mut out = vec![0f32; frame_samples(wide, high, 3)?];
    out.par_chunks_mut(wide * 3)
        .enumerate()
        .for_each(|(row, line)| {
            for (col, pixel) in line.as_chunks_mut::<3>().0.iter_mut().enumerate() {
                let (r, c) = super_ccd_source(fuji_width, row, col);
                if r < 0.0 || c < 0.0 {
                    continue;
                }
                let (ur, uc) = (r as usize, c as usize);
                if ur + 2 > height || uc + 2 > width {
                    continue;
                }
                let (fr, fc) = (r - ur as f32, c - uc as f32);
                let at = |rr: usize, cc: usize| &rgb[(rr * width + cc) * 3..][..3];
                let (p00, p01) = (at(ur, uc), at(ur, uc + 1));
                let (p10, p11) = (at(ur + 1, uc), at(ur + 1, uc + 1));
                for i in 0..3 {
                    let top = p00[i] * (1.0 - fc) + p01[i] * fc;
                    let bottom = p10[i] * (1.0 - fc) + p11[i] * fc;
                    pixel[i] = top * (1.0 - fr) + bottom * fr;
                }
            }
        });
    Ok((out, wide, high))
}

/// Sensor units to white-balanced scene-linear, one CFA row.
///
/// `black` and `gain` repeat with the filter pattern, so the position
/// index cycles along the row rather than costing a division a sample.
#[inline]
fn normalise_row<T: Copy + Into<f32>>(row: &mut [f32], src: &[T], levels: &Levels, y: usize) {
    let base = (y % levels.height) * levels.width;
    let mut i = 0;
    for (out, sample) in row.iter_mut().zip(src.iter()) {
        *out = ((*sample).into() - levels.black[base + i]) * levels.gain[base + i];
        i += 1;
        if i == levels.width {
            i = 0;
        }
    }
}

/// The same for data that is already three samples a pixel, where the
/// levels are per channel rather than per filter position.
#[inline]
fn normalise_pixels<T: Copy + Into<f32>>(row: &mut [f32], src: &[T], levels: &Levels) {
    for (pixel, sample) in row
        .as_chunks_mut::<3>()
        .0
        .iter_mut()
        .zip(src.as_chunks::<3>().0)
    {
        for (c, out) in pixel.iter_mut().enumerate() {
            *out = (sample[c].into() - levels.black[c]) * levels.gain[c];
        }
    }
}

/// Apply the colour matrix, then clip negatives. Values above 1 are
/// left alone: they are real light (specular highlights, and colours
/// outside sRGB's gamut after the matrix), and the caller's tone curve
/// is what should decide their fate.
#[inline]
fn finish_row(row: &mut [f32], matrix: Option<&[[f32; 3]; 3]>) {
    match matrix {
        Some(m) => {
            for pixel in row.as_chunks_mut::<3>().0 {
                let (r, g, b) = (pixel[0], pixel[1], pixel[2]);
                for (c, out) in pixel.iter_mut().enumerate() {
                    // NaN loses to 0.0 here, which is what we want for
                    // a sample that arrived broken.
                    *out = (m[c][0] * r + m[c][1] * g + m[c][2] * b).max(0.0);
                }
            }
        }
        None => {
            for v in row.iter_mut() {
                *v = v.max(0.0);
            }
        }
    }
}

/// The multipliers to use: the caller's override, else the file's
/// as-shot ones, with anything unusable replaced by 1.0. A zero or
/// missing second green means "the sensor has one green", so it takes
/// the first green's multiplier.
fn white_balance(raw: &RawImage, options: &DevelopOptions) -> [f32; 4] {
    let source = options.white_balance.unwrap_or(raw.wb_coeffs);
    let usable = |m: f32| m.is_finite() && m > 0.0;
    let mut wb = [1.0f32; 4];
    for (out, m) in wb.iter_mut().zip(source.iter()) {
        if usable(*m) {
            *out = *m;
        }
    }
    if !usable(source[3]) {
        wb[3] = wb[1];
    }
    // The contract puts green at exactly 1.0; a decoder that forgot
    // would otherwise scale the whole picture.
    if wb[1] != 1.0 {
        let g = wb[1];
        for m in wb.iter_mut() {
            *m /= g;
        }
    }
    wb
}

/// Black level and combined scale for every position of the CFA
/// period (or, for three-sample data, for every channel).
struct Levels {
    width: usize,
    height: usize,
    black: Vec<f32>,
    gain: Vec<f32>,
}

impl Levels {
    fn per_channel(raw: &RawImage, wb: [f32; 4]) -> Result<Levels> {
        let mut levels = Levels {
            width: 1,
            height: 1,
            black: Vec::with_capacity(3),
            gain: Vec::with_capacity(3),
        };
        for (black, multiplier) in raw.black_levels.iter().zip(wb.iter()).take(3) {
            levels.black.push(*black);
            levels
                .gain
                .push(scale(*black, raw.white_level)? * multiplier);
        }
        Ok(levels)
    }

    fn per_position(raw: &RawImage, wb: [f32; 4]) -> Result<Levels> {
        // For Bayer the four black levels *are* the four positions of
        // the 2x2 array, in the same row-major order, so they index
        // directly. Sensors that need this are the ones whose green
        // rows sit at different offsets.
        //
        // For X-Trans and arbitrary patterns there is no such
        // correspondence — a 6x6 array has 36 positions — so a single
        // level is used unless the file gave four genuinely different
        // ones, in which case they can only have meant one per colour
        // (R, G, B, second G).
        let (pw, ph) = match &raw.cfa {
            Cfa::None => {
                return Err(Error::Corrupt(
                    "develop: one sample a pixel with no filter array".into(),
                ))
            }
            Cfa::Bayer(_) => (2, 2),
            Cfa::XTrans(_) => (6, 6),
            Cfa::Pattern { width, height, .. } => (*width, *height),
            // The stored rectangle's period; its black levels are per
            // colour (R, G, B, G2), like a pattern's.
            Cfa::SuperCcd { row_staggered, .. } => Cfa::super_ccd_period(*row_staggered),
        };
        if pw == 0 || ph == 0 {
            return Err(Error::Corrupt("develop: empty filter pattern".into()));
        }
        // A period is a handful of pixels; a pattern claiming millions
        // is a forged header, and must not size an allocation.
        let cells = pw
            .checked_mul(ph)
            .filter(|n| *n <= 4096)
            .ok_or_else(|| Error::Corrupt(format!("develop: filter pattern of {pw}x{ph}")))?;
        if let Cfa::Pattern { colors, .. } = &raw.cfa {
            if colors.len() != cells {
                return Err(Error::Corrupt(format!(
                    "develop: pattern of {pw}x{ph} with {} colours",
                    colors.len()
                )));
            }
        }
        let bayer = matches!(raw.cfa, Cfa::Bayer(_));
        let uniform = raw.black_levels.iter().all(|b| *b == raw.black_levels[0]);
        let mut levels = Levels {
            width: pw,
            height: ph,
            black: vec![0.0; pw * ph],
            gain: vec![0.0; pw * ph],
        };
        for y in 0..ph {
            for x in 0..pw {
                let color = raw
                    .cfa
                    .color_at(x, y)
                    .ok_or_else(|| Error::Corrupt("develop: filter pattern is short".into()))?;
                let channel = match color {
                    CfaColor::Red => 0,
                    CfaColor::Green => 1,
                    CfaColor::Blue => 2,
                    CfaColor::Green2 => 3,
                    other => {
                        return Err(Error::Unsupported(format!(
                            "develop: {other:?} filter array (CMYG and four-colour sensors)"
                        )))
                    }
                };
                let black = if bayer {
                    raw.black_levels[y * 2 + x]
                } else if uniform {
                    raw.black_levels[0]
                } else {
                    raw.black_levels[channel]
                };
                let multiplier = if channel == 3 { wb[3] } else { wb[channel] };
                levels.black[y * pw + x] = black;
                levels.gain[y * pw + x] = scale(black, raw.white_level)? * multiplier;
            }
        }
        Ok(levels)
    }
}

/// 1/(white - black) for one position. `validate` has already checked
/// every recorded black level against white, but a pattern position
/// can pick up a level `validate` never looked at, so check again.
fn scale(black: f32, white: f32) -> Result<f32> {
    let range = white - black;
    // Written this way round on purpose: a NaN level has to fail, and
    // `range <= 0.0` alone would let it through.
    if range.is_nan() || range <= 0.0 {
        return Err(Error::Corrupt(format!(
            "develop: black {black} is not below white {white}"
        )));
    }
    Ok(1.0 / range)
}

/// Rearrange `src` for `orientation`. Written as a gather over
/// destination pixels — every thread only writes its own rows — and
/// tiled, because the four transposing orientations otherwise walk a
/// column of the source per destination row and miss the cache on
/// every pixel.
fn orient(src: &[f32], width: usize, height: usize, orientation: Orientation) -> Vec<f32> {
    const TILE: usize = 32;
    let (dw, dh) = if orientation.transposes() {
        (height, width)
    } else {
        (width, height)
    };
    let mut out = vec![0f32; dw * dh * 3];
    out.par_chunks_mut(dw * 3 * TILE)
        .enumerate()
        .for_each(|(band, rows)| {
            let y0 = band * TILE;
            for x0 in (0..dw).step_by(TILE) {
                let x1 = (x0 + TILE).min(dw);
                for (j, row) in rows.chunks_exact_mut(dw * 3).enumerate() {
                    let dy = y0 + j;
                    for dx in x0..x1 {
                        // The eight EXIF orientations, as the inverse map
                        // from the displayed pixel back to the stored one.
                        // 5 and 7 are the two transposes; 6 and 8 the two
                        // rotations.
                        let (sx, sy) = match orientation {
                            Orientation::Normal => (dx, dy),
                            Orientation::MirrorHorizontal => (width - 1 - dx, dy),
                            Orientation::Rotate180 => (width - 1 - dx, height - 1 - dy),
                            Orientation::MirrorVertical => (dx, height - 1 - dy),
                            Orientation::Transpose => (dy, dx),
                            Orientation::Rotate90CW => (dy, height - 1 - dx),
                            Orientation::Transverse => (width - 1 - dy, height - 1 - dx),
                            Orientation::Rotate270CW => (width - 1 - dy, dx),
                        };
                        let s = (sy * width + sx) * 3;
                        row[dx * 3..dx * 3 + 3].copy_from_slice(&src[s..s + 3]);
                    }
                }
            }
        });
    out
}

/// The camera-to-linear-sRGB matrix for a raw, from its XYZ→camera
/// matrix: invert, normalise each camera row so the matrix maps D65
/// white to (1,1,1) in camera space (which is what makes the as-shot
/// multipliers do their job), compose with XYZ→sRGB. Public so tests
/// and the camera table can check it.
///
/// Why the normalisation: DNG's ColorMatrix takes XYZ to the camera's
/// *unbalanced* space, where a white subject gives whatever three
/// numbers the filters and the sensor happen to produce. The as-shot
/// multipliers have already been applied by the time this matrix is
/// used, so by then white *is* (1,1,1) in camera space. Dividing row
/// `i` of M by `(M · XYZ_D65)[i]` builds exactly that convention into
/// the matrix, and inverting it then gives a camera→XYZ that sends
/// (1,1,1) to D65 white. Composing with XYZ→sRGB (also D65) therefore
/// sends camera white to sRGB white, and every row of the result sums
/// to one.
pub fn camera_to_srgb(xyz_to_camera: &[[f32; 3]; 3]) -> [[f32; 3]; 3] {
    /// CIE XYZ of the D65 white point, normalised to Y = 1.
    const XYZ_D65: [f64; 3] = [0.9505, 1.0, 1.0890];
    /// Linear sRGB primaries with a D65 white point, the standard
    /// (IEC 61966-2-1) matrix.
    #[rustfmt::skip]
    const XYZ_TO_SRGB: [[f64; 3]; 3] = [
        [ 3.2406, -1.5372, -0.4986],
        [-0.9689,  1.8758,  0.0415],
        [ 0.0557, -0.2040,  1.0570],
    ];

    let mut m = [[0f64; 3]; 3];
    for i in 0..3 {
        for j in 0..3 {
            m[i][j] = xyz_to_camera[i][j] as f64;
        }
    }
    for row in m.iter_mut() {
        let white = row[0] * XYZ_D65[0] + row[1] * XYZ_D65[1] + row[2] * XYZ_D65[2];
        if !(white.is_finite() && white.abs() > 1e-9) {
            // A row that gives white no response is not a camera
            // matrix; camera RGB is a better answer than NaNs.
            log::warn!("raw: colour matrix {xyz_to_camera:?} has a dead row, leaving camera RGB");
            return [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
        }
        for v in row.iter_mut() {
            *v /= white;
        }
    }
    let Some(camera_to_xyz) = invert(&m) else {
        // A singular matrix means the camera table or the file is
        // wrong; camera RGB is a better answer than NaNs.
        log::warn!("raw: colour matrix {xyz_to_camera:?} is singular, leaving camera RGB");
        return [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
    };
    let mut out = [[0f32; 3]; 3];
    for i in 0..3 {
        for j in 0..3 {
            let mut sum = 0.0;
            for k in 0..3 {
                sum += XYZ_TO_SRGB[i][k] * camera_to_xyz[k][j];
            }
            out[i][j] = sum as f32;
        }
    }
    out
}

/// 3x3 inverse by the adjugate, in double precision: camera matrices
/// are near-singular often enough that the f32 determinant is not to
/// be trusted.
fn invert(m: &[[f64; 3]; 3]) -> Option<[[f64; 3]; 3]> {
    let mut adj = [[0f64; 3]; 3];
    // The indices are the point here: `adj[i][j]` is the *transposed*
    // cofactor, built from rows j+1, j+2 and columns i+1, i+2, so
    // iterating the destination by value would lose the relationship.
    #[allow(clippy::needless_range_loop)]
    for i in 0..3 {
        for j in 0..3 {
            let (r0, r1) = ((j + 1) % 3, (j + 2) % 3);
            let (c0, c1) = ((i + 1) % 3, (i + 2) % 3);
            adj[i][j] = m[r0][c0] * m[r1][c1] - m[r0][c1] * m[r1][c0];
        }
    }
    let det = m[0][0] * adj[0][0] + m[0][1] * adj[1][0] + m[0][2] * adj[2][0];
    if !det.is_finite() || det.abs() < 1e-12 {
        return None;
    }
    let mut out = [[0f64; 3]; 3];
    for i in 0..3 {
        for j in 0..3 {
            out[i][j] = adj[i][j] / det;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demosaic::Quality;
    use crate::Format;

    /// A plausible XYZ→camera matrix of the shape DNG files carry:
    /// not any particular camera's, just an invertible one with the
    /// right sign pattern.
    const CAMERA: [[f32; 3]; 3] = [
        [0.6844, -0.0996, -0.0856],
        [-0.3876, 1.1761, 0.2396],
        [-0.0593, 0.1772, 0.6198],
    ];

    /// The sRGB primaries as an XYZ→RGB matrix, i.e. what a camera
    /// whose filters were exactly sRGB's would record.
    const SRGB: [[f32; 3]; 3] = [
        [3.2406, -1.5372, -0.4986],
        [-0.9689, 1.8758, 0.0415],
        [0.0557, -0.2040, 1.0570],
    ];

    /// Mosaic a full-colour image (in normalised, white-balanced
    /// units) back into sensor samples, inverting exactly what
    /// `develop` will do: undo the multiplier, scale by the position's
    /// own range, add its black level.
    fn synthetic(
        width: usize,
        height: usize,
        black: [f32; 4],
        white: f32,
        wb: [f32; 4],
        pixel: impl Fn(usize, usize) -> [f32; 3],
    ) -> RawImage {
        let cfa = Cfa::RGGB;
        let mut data = vec![0u16; width * height];
        for y in 0..height {
            for x in 0..width {
                let (channel, multiplier) = match cfa.color_at(x, y).expect("bayer") {
                    CfaColor::Red => (0, wb[0]),
                    CfaColor::Green => (1, wb[1]),
                    CfaColor::Blue => (2, wb[2]),
                    CfaColor::Green2 => (1, wb[3]),
                    other => panic!("{other:?}"),
                };
                let b = black[(y % 2) * 2 + x % 2];
                let v = pixel(x, y)[channel] / multiplier * (white - b) + b;
                data[y * width + x] = v.round().clamp(0.0, 65535.0) as u16;
            }
        }
        let mut raw = RawImage::new(Format::Dng, width, height, 1, RawData::U16(data), cfa);
        raw.black_levels = black;
        raw.white_level = white;
        raw.wb_coeffs = wb;
        raw
    }

    fn close(a: &[f32], b: &[f32], tolerance: f32) {
        assert_eq!(a.len(), b.len(), "lengths");
        for (i, (x, y)) in a.iter().zip(b.iter()).enumerate() {
            assert!((x - y).abs() <= tolerance, "sample {i}: {x} vs {y}");
        }
    }

    /// A camera whose XYZ→RGB matrix *is* sRGB's needs no colour
    /// conversion at all, so the derivation must give the identity.
    #[test]
    fn srgb_camera_is_the_identity() {
        let m = camera_to_srgb(&SRGB);
        for i in 0..3 {
            for j in 0..3 {
                let want = if i == j { 1.0 } else { 0.0 };
                assert!((m[i][j] - want).abs() < 2e-3, "{m:?}");
            }
        }
    }

    /// The whole point of the row normalisation: white balanced camera
    /// (1,1,1) has to land on sRGB (1,1,1), so a neutral subject stays
    /// neutral. Equivalently, every row sums to one.
    #[test]
    fn white_is_preserved() {
        for matrix in [
            SRGB,
            CAMERA,
            [[1.0, 0.2, 0.1], [0.05, 0.9, 0.3], [0.2, 0.1, 1.4]],
        ] {
            let m = camera_to_srgb(&matrix);
            for row in m {
                let sum = row[0] + row[1] + row[2];
                assert!((sum - 1.0).abs() < 1e-3, "row of {m:?} sums to {sum}");
            }
        }
    }

    /// A singular matrix cannot be inverted; camera RGB is the answer
    /// rather than a plane of NaNs.
    #[test]
    fn singular_matrix_falls_back() {
        let m = camera_to_srgb(&[[1.0, 1.0, 1.0], [2.0, 2.0, 2.0], [3.0, 3.0, 3.0]]);
        assert_eq!(m, [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]);
    }

    /// Different black levels per Bayer position, a real white balance
    /// and a real matrix: a subject that is neutral after balancing
    /// has to develop to equal R, G and B.
    #[test]
    fn neutral_stays_neutral() {
        let mut raw = synthetic(
            24,
            16,
            [64.0, 68.0, 68.0, 72.0],
            1024.0,
            [2.0, 1.0, 1.5, 1.0],
            |_, _| [0.5, 0.5, 0.5],
        );
        raw.color_matrix = Some(CAMERA);
        let out = develop(&raw, &DevelopOptions::default()).expect("develop");
        assert_eq!((out.width, out.height), (24, 16));
        close(&out.rgb, &vec![0.5; 24 * 16 * 3], 3e-3);
    }

    /// A flat coloured patch interpolates exactly, so the developed
    /// pixel is the matrix applied to the balanced camera triple and
    /// nothing else.
    #[test]
    fn red_patch_goes_through_the_matrix() {
        let camera = [0.8f32, 0.1, 0.1];
        let mut raw = synthetic(20, 20, [0.0; 4], 4095.0, [1.7, 1.0, 1.9, 1.0], |_, _| {
            camera
        });
        raw.color_matrix = Some(CAMERA);
        let m = camera_to_srgb(&CAMERA);
        let want: Vec<f32> = (0..3)
            .map(|i| (m[i][0] * camera[0] + m[i][1] * camera[1] + m[i][2] * camera[2]).max(0.0))
            .collect();
        let out = develop(&raw, &DevelopOptions::default()).expect("develop");
        for pixel in out.rgb.as_chunks::<3>().0 {
            close(pixel, &want, 3e-3);
        }
        // And `camera_rgb` skips the matrix entirely.
        let plain = develop(
            &raw,
            &DevelopOptions {
                camera_rgb: true,
                ..Default::default()
            },
        )
        .expect("develop");
        for pixel in plain.rgb.as_chunks::<3>().0 {
            close(pixel, &camera, 3e-3);
        }
    }

    /// The caller's multipliers replace the file's, and a zero second
    /// green means "use the first".
    #[test]
    fn white_balance_override() {
        let raw = synthetic(8, 8, [0.0; 4], 1023.0, [1.0, 1.0, 1.0, 1.0], |_, _| {
            [0.25, 0.25, 0.25]
        });
        let out = develop(
            &raw,
            &DevelopOptions {
                white_balance: Some([2.0, 1.0, 3.0, 0.0]),
                camera_rgb: true,
                ..Default::default()
            },
        )
        .expect("develop");
        for pixel in out.rgb.as_chunks::<3>().0 {
            close(pixel, &[0.5, 0.25, 0.75], 3e-3);
        }
    }

    /// Cropping after the interpolation means the cropped frame is
    /// exactly the window of the uncropped one — no re-phasing of the
    /// filter array, and no border artefacts pulled inside.
    #[test]
    fn crop_is_a_window_on_the_full_frame() {
        let scene = |x: usize, y: usize| {
            [
                0.2 + 0.02 * (x % 7) as f32,
                0.3 + 0.02 * (y % 5) as f32,
                0.4 + 0.01 * ((x + y) % 11) as f32,
            ]
        };
        let mut raw = synthetic(32, 24, [16.0; 4], 1023.0, [1.4, 1.0, 1.8, 1.0], scene);
        raw.crop = Rect {
            x: 3,
            y: 5,
            width: 20,
            height: 14,
        };
        raw.color_matrix = Some(CAMERA);
        let whole = develop(
            &raw,
            &DevelopOptions {
                crop: false,
                ..Default::default()
            },
        )
        .expect("develop");
        let cropped = develop(&raw, &DevelopOptions::default()).expect("develop");
        assert_eq!((whole.width, whole.height), (32, 24));
        assert_eq!((cropped.width, cropped.height), (20, 14));
        for y in 0..14 {
            let from = ((y + 5) * 32 + 3) * 3;
            close(
                &cropped.rgb[y * 20 * 3..(y + 1) * 20 * 3],
                &whole.rgb[from..from + 20 * 3],
                0.0,
            );
        }
    }

    /// Three samples a pixel: no interpolation, levels and
    /// multipliers per channel. Values are `index / 10` so the eight
    /// orientation cases below can be read off by eye.
    fn linear_raw(orientation: Orientation) -> RawImage {
        let (width, height) = (2, 3);
        let data: Vec<u16> = (0..width * height)
            .flat_map(|i| [i as u16 * 10; 3])
            .collect();
        let mut raw = RawImage::new(Format::Dng, width, height, 3, RawData::U16(data), Cfa::None);
        raw.white_level = 100.0;
        raw.orientation = orientation;
        raw
    }

    #[test]
    fn three_samples_a_pixel() {
        let raw = linear_raw(Orientation::Normal);
        let out = develop(&raw, &DevelopOptions::default()).expect("develop");
        assert_eq!((out.width, out.height), (2, 3));
        let want: Vec<f32> = (0..6).flat_map(|i| [i as f32 / 10.0; 3]).collect();
        close(&out.rgb, &want, 1e-6);
    }

    /// All eight EXIF orientations against hand-written expectations.
    /// The source is
    ///
    /// ```text
    /// 0 1
    /// 2 3
    /// 4 5
    /// ```
    #[test]
    fn every_orientation() {
        #[rustfmt::skip]
        let cases: [(Orientation, usize, usize, [usize; 6]); 8] = [
            (Orientation::Normal,           2, 3, [0, 1, 2, 3, 4, 5]),
            (Orientation::MirrorHorizontal, 2, 3, [1, 0, 3, 2, 5, 4]),
            (Orientation::Rotate180,        2, 3, [5, 4, 3, 2, 1, 0]),
            (Orientation::MirrorVertical,   2, 3, [4, 5, 2, 3, 0, 1]),
            (Orientation::Transpose,        3, 2, [0, 2, 4, 1, 3, 5]),
            (Orientation::Rotate90CW,       3, 2, [4, 2, 0, 5, 3, 1]),
            (Orientation::Transverse,       3, 2, [5, 3, 1, 4, 2, 0]),
            (Orientation::Rotate270CW,      3, 2, [1, 3, 5, 0, 2, 4]),
        ];
        for (orientation, width, height, want) in cases {
            let raw = linear_raw(orientation);
            let out = develop(&raw, &DevelopOptions::default()).expect("develop");
            assert_eq!((out.width, out.height), (width, height), "{orientation:?}");
            let expect: Vec<f32> = want.iter().flat_map(|i| [*i as f32 / 10.0; 3]).collect();
            close(&out.rgb, &expect, 1e-6);
            // Off by request, the stored orientation is left alone.
            let stored = develop(
                &raw,
                &DevelopOptions {
                    orient: false,
                    ..Default::default()
                },
            )
            .expect("develop");
            assert_eq!((stored.width, stored.height), (2, 3), "{orientation:?}");
        }
    }

    /// Orientation is applied to the *cropped* frame, and the crop is
    /// in unrotated sensor coordinates.
    #[test]
    fn crop_then_orient() {
        let mut raw = synthetic(16, 12, [0.0; 4], 1023.0, [1.0; 4], |_, _| [0.4, 0.4, 0.4]);
        raw.crop = Rect {
            x: 2,
            y: 2,
            width: 10,
            height: 8,
        };
        raw.orientation = Orientation::Rotate90CW;
        let out = develop(&raw, &DevelopOptions::default()).expect("develop");
        assert_eq!((out.width, out.height), (8, 10));
    }

    /// Floating-point DNG data is already linear, and takes the same
    /// levels as everything else.
    #[test]
    fn float_data() {
        let (width, height) = (16, 16);
        let data: Vec<f32> = (0..width * height)
            .map(|i| 0.25 + (i % 4) as f32 * 0.05)
            .collect();
        let mut raw = RawImage::new(
            Format::Dng,
            width,
            height,
            1,
            RawData::F32(data.clone()),
            Cfa::RGGB,
        );
        raw.black_levels = [0.0; 4];
        raw.white_level = 1.0;
        let out = develop(
            &raw,
            &DevelopOptions {
                camera_rgb: true,
                quality: Quality::Fast,
                ..Default::default()
            },
        )
        .expect("develop");
        // Every sample survives in its own colour channel, wherever
        // the interpolation put the other two.
        for y in 0..height {
            for x in 0..width {
                let channel = match raw.cfa.color_at(x, y).expect("bayer") {
                    CfaColor::Red => 0,
                    CfaColor::Blue => 2,
                    _ => 1,
                };
                let got = out.rgb[(y * width + x) * 3 + channel];
                assert!((got - data[y * width + x]).abs() < 1e-6, "{x},{y}: {got}");
            }
        }
        // A float frame with a black level scales the same way.
        let mut offset = raw.clone();
        offset.data = RawData::F32(data.iter().map(|v| v * 0.5 + 0.25).collect());
        offset.black_levels = [0.25; 4];
        offset.white_level = 0.75;
        let out = develop(
            &offset,
            &DevelopOptions {
                camera_rgb: true,
                quality: Quality::Fast,
                ..Default::default()
            },
        )
        .expect("develop");
        for y in 0..height {
            for x in 0..width {
                let channel = match raw.cfa.color_at(x, y).expect("bayer") {
                    CfaColor::Red => 0,
                    CfaColor::Blue => 2,
                    _ => 1,
                };
                let got = out.rgb[(y * width + x) * 3 + channel];
                assert!((got - data[y * width + x]).abs() < 1e-5, "{x},{y}: {got}");
            }
        }
    }

    /// Negatives are clipped, values above one are not: they are real
    /// light, and the caller's tone curve decides what to do with them.
    #[test]
    fn clips_below_zero_only() {
        let mut raw = synthetic(8, 8, [0.0; 4], 100.0, [1.0; 4], |_, _| [1.5, 1.5, 1.5]);
        raw.black_levels = [200.0; 4];
        raw.white_level = 400.0;
        let out = develop(
            &raw,
            &DevelopOptions {
                camera_rgb: true,
                ..Default::default()
            },
        )
        .expect("develop");
        assert!(out.rgb.iter().all(|v| *v == 0.0), "negatives should clip");

        let raw = synthetic(8, 8, [0.0; 4], 1000.0, [1.0; 4], |_, _| [2.0, 2.0, 2.0]);
        let out = develop(
            &raw,
            &DevelopOptions {
                camera_rgb: true,
                ..Default::default()
            },
        )
        .expect("develop");
        assert!(
            out.rgb.iter().all(|v| *v > 1.5),
            "highlights should survive"
        );
    }

    /// Inconsistent images are refused before anything indexes them.
    #[test]
    fn rejects_bad_images() {
        let mut raw = synthetic(8, 8, [0.0; 4], 1023.0, [1.0; 4], |_, _| [0.5; 3]);
        raw.width = 9;
        assert!(develop(&raw, &DevelopOptions::default()).is_err());
    }

    /// Sample counts the pipeline has no meaning for are refused
    /// rather than half-read.
    #[test]
    fn rejects_odd_sample_counts() {
        let mut raw = synthetic(8, 8, [0.0; 4], 1023.0, [1.0; 4], |_, _| [0.5; 3]);
        raw.cpp = 4;
        raw.data = RawData::U16(vec![100u16; 8 * 8 * 4]);
        assert!(matches!(
            develop(&raw, &DevelopOptions::default()),
            Err(Error::Unsupported(_))
        ));
    }

    /// A SuperCCD raw with every photosite at the same scene value, in
    /// either stagger direction. The padding outside the active
    /// rectangle is filled with a marker that must never reach the
    /// picture.
    fn super_ccd_raw(row_staggered: bool, fuji_width: usize, rows: usize) -> RawImage {
        let (left, top) = (4, 2);
        let line = fuji_width << usize::from(!row_staggered);
        let (width, height) = (line + 2 * left, rows + 2 * top);
        let bayer = [
            CfaColor::Green,
            CfaColor::Blue,
            CfaColor::Red,
            CfaColor::Green,
        ];
        let cfa = Cfa::super_ccd(row_staggered, fuji_width, bayer, (left, top));
        let data = vec![7777u16; width * height];
        let mut raw = RawImage::new(Format::Raf, width, height, 1, RawData::U16(data), cfa);
        raw.white_level = 10000.0;
        raw.crop = Rect {
            x: left,
            y: top,
            width: line,
            height: rows,
        };
        raw
    }

    /// The dimensions the SuperCCD note derives for both corpus bodies.
    #[test]
    fn super_ccd_sizes_of_both_bodies() {
        assert_eq!(
            super_ccd_sheared_size(true, 2448, 3688).unwrap(),
            (4292, 4291)
        );
        assert_eq!(super_ccd_output_size(2448, 4291).unwrap(), (3460, 2607));
        assert_eq!(
            super_ccd_sheared_size(false, 2720, 3840).unwrap(),
            (6560, 6559)
        );
        assert_eq!(super_ccd_output_size(2720, 6559).unwrap(), (3845, 5430));
        assert!(super_ccd_sheared_size(true, 0, 100).is_err());
        assert!(super_ccd_output_size(0, 100).is_err());
        assert!(super_ccd_output_size(200, 100).is_err());
        // Dimension-preserving hostile RAF mutations from the review:
        // their stored frames remain only 9/21 MP, but their extreme
        // aspect ratios used to request 1.7--2.7 GB RGB intermediates.
        assert!(super_ccd_sheared_size(true, 14752, 628).is_err());
        assert!(super_ccd_sheared_size(false, 10976, 948).is_err());
    }

    /// The note's worked sampling positions for the S9600 and the
    /// GX680 (rotated-frame coordinates, before the GX680's turn).
    #[test]
    fn super_ccd_sampling_positions() {
        let (r, c) = super_ccd_source(2448, 707, 1416);
        assert_eq!((r as usize, c as usize), (1945, 1501));
        assert!(
            (r - 1945.6613).abs() < 2e-4 && (c - 1501.1877).abs() < 2e-4,
            "{r} {c}"
        );
        let (r, c) = super_ccd_source(2448, 708, 1416);
        assert!(
            (r - 1946.3684).abs() < 2e-4 && (c - 1501.8948).abs() < 2e-4,
            "{r} {c}"
        );
        let (r, c) = super_ccd_source(2720, 1416, 1414);
        assert!(
            (r - 2720.4143).abs() < 2e-4 && (c - 2001.1122).abs() < 2e-4,
            "{r} {c}"
        );
    }

    /// Every photosite lands on exactly one cell, at the cell the
    /// forward map names, and the cells in between stay empty.
    #[test]
    fn super_ccd_shear_places_every_photosite_once() {
        for (row_staggered, fuji_width, rows) in
            [(true, 6, 8), (false, 4, 6), (true, 5, 7), (false, 3, 5)]
        {
            let line = fuji_width << usize::from(!row_staggered);
            let crop = Rect {
                x: 3,
                y: 1,
                width: line,
                height: rows,
            };
            let (stride, height) = (line + 5, rows + 3);
            // Unique, non-zero values so a cell can be traced back.
            let plane: Vec<f32> = (0..stride * height).map(|i| i as f32 + 1.0).collect();
            let sheared = super_ccd_shear(&plane, stride, crop, row_staggered, fuji_width).unwrap();
            let mut seen = vec![false; sheared.width * sheared.height];
            let mut inside = 0;
            for y in 0..rows {
                for x in 0..line {
                    let (r, c) = crate::super_ccd_cell(row_staggered, fuji_width, x, y);
                    let (r, c) = (r as usize, c as usize);
                    if r >= sheared.height || c >= sheared.width {
                        // A row-staggered frame of odd height has no
                        // cell row for the last row's first photosite:
                        // the frame is sized by `height >> 1`, as the
                        // reference sizes it. Nothing else may fall out.
                        assert!(
                            row_staggered && rows % 2 == 1 && y == rows - 1 && x == 0,
                            "({x}, {y}) -> ({r}, {c}) outside"
                        );
                        continue;
                    }
                    inside += 1;
                    assert!(!seen[r * sheared.width + c], "cell ({r}, {c}) hit twice");
                    seen[r * sheared.width + c] = true;
                    assert_eq!(
                        sheared.plane[r * sheared.width + c],
                        plane[(crop.y + y) * stride + crop.x + x]
                    );
                }
            }
            assert_eq!(
                sheared.written, inside,
                "{row_staggered} {fuji_width} {rows}"
            );
            if rows % 2 == 0 {
                assert_eq!(inside, line * rows);
            }
            let nonzero = sheared.plane.iter().filter(|v| **v != 0.0).count();
            assert_eq!(nonzero, inside, "empty cells must stay zero");
        }
        // A crop that is not one stagger line wide is a decoder bug.
        let plane = vec![1f32; 100];
        let crop = Rect {
            x: 0,
            y: 0,
            width: 7,
            height: 5,
        };
        assert!(matches!(
            super_ccd_shear(&plane, 10, crop, true, 6),
            Err(Error::Corrupt(_))
        ));
        assert!(super_ccd_shear(&plane, 10, Rect { width: 6, ..crop }, true, 6).is_ok());
        assert!(matches!(
            super_ccd_shear(
                &plane,
                10,
                Rect {
                    x: 5,
                    width: 6,
                    ..crop
                },
                true,
                6
            ),
            Err(Error::Corrupt(_))
        ));
    }

    /// The rotation of a constant frame is that constant, apart from
    /// the pixels the edge rule leaves black.
    #[test]
    fn super_ccd_rotation_of_a_flat_frame() {
        let (ws, hs) = super_ccd_sheared_size(true, 20, 30).unwrap();
        let rgb: Vec<f32> = (0..ws * hs).flat_map(|_| [0.25, 0.5, 0.75]).collect();
        let (out, wide, high) = super_ccd_rotate(&rgb, ws, hs, 20).unwrap();
        assert_eq!((wide, high), super_ccd_output_size(20, hs).unwrap());
        assert_eq!(out.len(), wide * high * 3);
        let mut black = 0;
        for pixel in out.as_chunks::<3>().0 {
            if pixel == &[0.0, 0.0, 0.0] {
                black += 1;
            } else {
                close(pixel, &[0.25, 0.5, 0.75], 1e-5);
            }
        }
        assert!(black < wide * 2, "{black} black pixels");
        assert!(super_ccd_rotate(&rgb[..30], ws, hs, 20).is_err());
    }

    /// End to end: a flat SuperCCD field develops flat away from the
    /// edge, for both stagger directions and both qualities, sized as
    /// the geometry says, and the padding marker never shows.
    #[test]
    fn super_ccd_flat_field_develops_flat() {
        for (row_staggered, fuji_width, rows) in [(true, 40, 60), (false, 30, 50)] {
            let raw = super_ccd_raw(row_staggered, fuji_width, rows);
            for quality in [Quality::Fast, Quality::Best] {
                let out = develop(
                    &raw,
                    &DevelopOptions {
                        quality,
                        camera_rgb: true,
                        ..Default::default()
                    },
                )
                .expect("develop");
                let (_, hs) = super_ccd_sheared_size(row_staggered, fuji_width, rows).unwrap();
                assert_eq!(
                    (out.width, out.height),
                    super_ccd_output_size(fuji_width, hs).unwrap()
                );
                let margin = 8;
                for y in margin..out.height - margin {
                    for x in margin..out.width - margin {
                        let p = &out.rgb[(y * out.width + x) * 3..][..3];
                        close(p, &[0.7777; 3], 2e-3);
                    }
                }
            }
        }
    }

    /// The crop is consumed by the shear: asking for no crop makes no
    /// difference, and the orientation applies to the rotated picture.
    #[test]
    fn super_ccd_crop_and_orientation() {
        let mut raw = super_ccd_raw(false, 30, 50);
        raw.orientation = Orientation::Rotate90CW;
        let turned = develop(&raw, &DevelopOptions::default()).expect("develop");
        let upright = develop(
            &raw,
            &DevelopOptions {
                orient: false,
                crop: false,
                ..Default::default()
            },
        )
        .expect("develop");
        assert_eq!(
            (turned.width, turned.height),
            (upright.height, upright.width)
        );
        // Inconsistent geometry is refused before anything indexes.
        raw.crop.width += 1;
        raw.width += 1;
        raw.data = RawData::U16(vec![7777; raw.width * raw.height]);
        assert!(matches!(
            develop(&raw, &DevelopOptions::default()),
            Err(Error::Corrupt(_))
        ));
    }

    /// Release-mode timing for a 24 megapixel Bayer frame, the size
    /// this pipeline is meant to keep under a couple of seconds.
    /// Ignored by default: `SCHIST_RAW_BENCH=1 cargo test --release -p
    /// schist-codec-raw -- --ignored --nocapture`.
    #[test]
    #[ignore = "timing only; needs --release and SCHIST_RAW_BENCH"]
    fn timing_24_megapixels() {
        if std::env::var("SCHIST_RAW_BENCH").is_err() {
            println!("set SCHIST_RAW_BENCH=1 to run the timing test");
            return;
        }
        let (width, height) = (6000usize, 4000usize);
        let data: Vec<u16> = (0..width * height)
            .map(|i| (512 + (i * 2654435761usize) % 3000) as u16)
            .collect();
        let mut raw = RawImage::new(Format::Dng, width, height, 1, RawData::U16(data), Cfa::RGGB);
        raw.black_levels = [512.0; 4];
        raw.white_level = 4095.0;
        raw.wb_coeffs = [2.1, 1.0, 1.5, 1.0];
        raw.color_matrix = Some(CAMERA);
        raw.crop = Rect {
            x: 8,
            y: 8,
            width: width - 16,
            height: height - 16,
        };
        for quality in [Quality::Fast, Quality::Best] {
            let start = std::time::Instant::now();
            let out = develop(
                &raw,
                &DevelopOptions {
                    quality,
                    ..Default::default()
                },
            )
            .expect("develop");
            println!(
                "develop {width}x{height} {quality:?}: {:?}",
                start.elapsed()
            );
            assert_eq!(out.rgb.len(), out.width * out.height * 3);
        }
    }
}

/// Corpus check of the whole SuperCCD pipeline against the reference
/// developer's output: every `<raw>.developed.tiff` under
/// `SCHIST_RAW_CORPUS` (16-bit linear RGB, camera white balance, matrix
/// applied, oriented, no crop), compared over the interior.
#[cfg(test)]
mod corpus {
    use super::*;
    use std::path::{Path, PathBuf};

    fn developed_oracles() -> Vec<PathBuf> {
        let Ok(root) = std::env::var("SCHIST_RAW_CORPUS") else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut stack = vec![PathBuf::from(root)];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path
                    .to_str()
                    .is_some_and(|p| p.ends_with(".developed.tiff"))
                {
                    out.push(path);
                }
            }
        }
        out.sort();
        out
    }

    /// The numbers on the line after `key`, and on the `n` lines after
    /// that.
    fn lines_after<'a>(text: &'a str, key: &str, n: usize) -> Option<Vec<&'a str>> {
        let mut lines = text.lines();
        lines.find(|line| line.trim_start().starts_with(key))?;
        let out: Vec<&str> = lines.by_ref().take(n).collect();
        (out.len() == n).then_some(out)
    }

    fn floats(text: &str) -> Vec<f32> {
        text.split_whitespace()
            .filter_map(|t| t.parse().ok())
            .collect()
    }

    /// What the reference's identify output says about the raw beside
    /// the oracle: its XYZ->camera matrix and its as-shot multipliers.
    fn identify(raw: &Path) -> Option<([[f32; 3]; 3], [f32; 4])> {
        let mut sidecar = raw.as_os_str().to_os_string();
        sidecar.push(".identify.txt");
        let text = std::fs::read_to_string(sidecar).ok()?;
        let rows = lines_after(&text, "XYZ->CamRGB matrix:", 3)?;
        let mut matrix = [[0f32; 3]; 3];
        for (row, line) in matrix.iter_mut().zip(rows) {
            let v = floats(line);
            *row = [*v.first()?, *v.get(1)?, *v.get(2)?];
        }
        let shot = text
            .lines()
            .find(|line| line.trim_start().starts_with("As shot"))?;
        let v: Vec<f32> = shot
            .split_whitespace()
            .filter_map(|t| t.parse::<u32>().ok())
            .map(|t| t as f32)
            .collect();
        let wb = [*v.first()?, *v.get(1)?, *v.get(2)?, *v.get(3)?];
        Some((matrix, [wb[0] / wb[1], 1.0, wb[2] / wb[1], wb[3] / wb[1]]))
    }

    #[test]
    fn matches_the_developed_oracles() {
        let oracles = developed_oracles();
        if oracles.is_empty() {
            return;
        }
        let mut failures = Vec::new();
        for oracle in &oracles {
            let raw_path =
                PathBuf::from(oracle.to_str().unwrap().trim_end_matches(".developed.tiff"));
            let bytes = std::fs::read(&raw_path).expect("corpus file");
            let mut raw =
                crate::decode(&bytes).unwrap_or_else(|e| panic!("{}: {e}", raw_path.display()));
            let (matrix, as_shot) = identify(&raw_path)
                .unwrap_or_else(|| panic!("{}: no identify sidecar", raw_path.display()));
            // The reference's own matrix and multipliers: the camera
            // table has no entry for these bodies, and the DBP's
            // as-shot multipliers are not the file's (see the RAF
            // corpus test), so the comparison isolates the geometry
            // and the interpolation.
            raw.color_matrix = Some(matrix);

            // The reference lowers the white level to the frame's
            // brightest black-subtracted sample when that lies within
            // the top quarter below the level; this crate keeps the
            // level, so its output is scaled by the ratio to compare.
            let RawData::U16(data) = &raw.data else {
                panic!("{}: not 16-bit", raw_path.display())
            };
            let mut peak = 0f32;
            for (i, v) in data.iter().enumerate() {
                let (x, y) = (i % raw.width, i / raw.width);
                if x < raw.crop.x
                    || x >= raw.crop.x + raw.crop.width
                    || y < raw.crop.y
                    || y >= raw.crop.y + raw.crop.height
                {
                    continue;
                }
                let black = match raw.cfa.color_at(x, y) {
                    Some(CfaColor::Red) => raw.black_levels[0],
                    Some(CfaColor::Blue) => raw.black_levels[2],
                    Some(CfaColor::Green2) => raw.black_levels[3],
                    _ => raw.black_levels[1],
                };
                peak = peak.max(*v as f32 - black);
            }
            let common_black = raw.black_levels.iter().copied().fold(f32::MAX, f32::min);
            let white = raw.white_level - common_black;
            let effective = if peak < white && peak > 0.75 * white {
                peak
            } else {
                white
            };
            let scale = white / effective;
            println!(
                "{}: white {} effective {effective} (scale {scale:.5})",
                raw_path.display(),
                raw.white_level
            );

            let out = develop(
                &raw,
                &DevelopOptions {
                    white_balance: Some(as_shot),
                    ..Default::default()
                },
            )
            .unwrap_or_else(|e| panic!("{}: {e}", raw_path.display()));

            let want = {
                let mut reader = image::ImageReader::open(oracle)
                    .unwrap_or_else(|e| panic!("{}: {e}", oracle.display()));
                reader.limits(image::Limits::no_limits());
                reader
                    .decode()
                    .unwrap_or_else(|e| panic!("{}: {e}", oracle.display()))
                    .into_rgb16()
            };
            assert_eq!(
                (want.width() as usize, want.height() as usize),
                (out.width, out.height),
                "{}: developed size",
                raw_path.display()
            );
            let (w, h) = (out.width, out.height);
            let want = want.as_raw();
            // Interior only: the reference interpolates the sheared
            // frame straight through the empty region around the
            // photosites, which pollutes the outer few pixels in a way
            // nothing should try to match.
            let margin = 8;
            // Clipped highlights are also left out, with a 4-pixel
            // halo: the reference clips the white-balanced plane at
            // the white level *before* interpolating, so a blown
            // highlight is neutral there, while this crate keeps the
            // headroom above 1.0 (as it does for every camera; the
            // tone stage decides) and the red and blue excess spreads
            // through the interpolation and the rotation. The DBP
            // frame has 1.5 % of its pixels clipped and they alone
            // cost it 14 dB.
            let mine16 = |i: usize| (out.rgb[i] * scale * 65535.0).clamp(0.0, 65535.0);
            let clipped: Vec<bool> = (0..w * h)
                .map(|p| (0..3).any(|c| want[p * 3 + c] == 65535 || mine16(p * 3 + c) >= 65535.0))
                .collect();
            let halo = 4;
            let mut wide = vec![false; w * h];
            for y in 0..h {
                for x in 0..w {
                    if clipped[y * w + x] {
                        for dx in x.saturating_sub(halo)..(x + halo + 1).min(w) {
                            wide[y * w + dx] = true;
                        }
                    }
                }
            }
            let mut excluded = vec![false; w * h];
            for y in 0..h {
                for x in 0..w {
                    if wide[y * w + x] {
                        for dy in y.saturating_sub(halo)..(y + halo + 1).min(h) {
                            excluded[dy * w + x] = true;
                        }
                    }
                }
            }
            let mut sum = [0f64; 2];
            let mut n = [0u64; 2];
            let mut worst = 0f32;
            let mut errors = Vec::with_capacity((w - 2 * margin) * (h - 2 * margin) * 3);
            for y in margin..h - margin {
                for x in margin..w - margin {
                    let keep = !excluded[y * w + x];
                    for c in 0..3 {
                        let i = (y * w + x) * 3 + c;
                        let d = mine16(i) - want[i] as f32;
                        sum[0] += (d as f64) * (d as f64);
                        n[0] += 1;
                        if keep {
                            sum[1] += (d as f64) * (d as f64);
                            n[1] += 1;
                            worst = worst.max(d.abs());
                            errors.push(d.abs());
                        }
                    }
                }
            }
            // `SCHIST_RAW_DUMP=1` writes this crate's picture beside
            // the oracle, for looking at where they differ.
            if std::env::var("SCHIST_RAW_DUMP").is_ok() {
                let pixels: Vec<u16> = (0..w * h * 3).map(|i| mine16(i) as u16).collect();
                let buffer =
                    image::ImageBuffer::<image::Rgb<u16>, _>::from_raw(w as u32, h as u32, pixels)
                        .expect("buffer");
                let mut dump = oracle.as_os_str().to_os_string();
                dump.push(".mine.tiff");
                buffer.save(PathBuf::from(dump)).expect("dump");
            }
            let psnr = |k: usize| 10.0 * ((65535.0f64 * 65535.0) / (sum[k] / n[k] as f64)).log10();
            let (all, unclipped) = (psnr(0), psnr(1));
            errors.sort_by(|a, b| a.total_cmp(b));
            let median = errors[errors.len() / 2];
            let p99 = errors[errors.len() * 99 / 100];
            println!(
                "{}: {w}x{h}, PSNR {all:.2} dB over the interior, {unclipped:.2} dB away from clipped highlights ({:.1} % of pixels excluded); there median |err| {median:.1}, p99 {p99:.1}, max {worst:.0} (16-bit scale)",
                raw_path.display(),
                100.0 * (1.0 - n[1] as f64 / n[0] as f64)
            );
            if unclipped <= 40.0 {
                failures.push(format!("{}: PSNR {unclipped:.2} dB", raw_path.display()));
            }
        }
        assert!(failures.is_empty(), "{failures:?}");
    }
}
