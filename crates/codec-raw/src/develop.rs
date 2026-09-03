//! From sensor data to linear sRGB.
//!
//! The classic pipeline: subtract the black level, scale so the white
//! level is 1.0, apply the white balance multipliers, interpolate the
//! CFA, convert camera colour to sRGB through the camera matrix (with
//! the white balance folded in the way DNG describes, so a neutral
//! stays neutral), clip, then crop and orient. Output is linear light;
//! the caller applies a tone curve and encoding.

use crate::demosaic::demosaic;
use crate::{Cfa, CfaColor, Error, Orientation, RawData, RawImage, Rect, Result};
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
        let mut plane = vec![0f32; width * height];
        match &raw.data {
            RawData::U16(v) => plane
                .par_chunks_mut(width)
                .enumerate()
                .for_each(|(y, row)| {
                    normalise_row(row, &v[y * width..(y + 1) * width], &levels, y)
                }),
            RawData::F32(v) => plane
                .par_chunks_mut(width)
                .enumerate()
                .for_each(|(y, row)| {
                    normalise_row(row, &v[y * width..(y + 1) * width], &levels, y)
                }),
        }
        // Step (c).
        demosaic(&plane, width, height, &raw.cfa, options.quality)?
    };

    // Step (d): camera RGB to linear sRGB. No matrix (or the caller
    // asked for camera RGB) leaves the numbers in the camera's own
    // space, which is still white balanced and so still neutral-
    // correct, just not colour-correct.
    let matrix = if options.camera_rgb {
        None
    } else {
        raw.color_matrix.as_ref().map(camera_to_srgb)
    };

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
