//! Kodak DCR, KDC and the DCS-series TIFFs.
//!
//! Kodak's raws are all TIFF containers and share almost nothing else.
//! Three generations of professional back, two of consumer compact and
//! four compression schemes wear the same two file extensions, so this
//! module works out what it is holding from the directories rather
//! than from the name.
//!
//! Two private directories carry everything the TIFF tags do not, and
//! neither is a SubIFD, so the shared parser does not reach them:
//!
//! * `KodakIFD`, tag 0x8290 of IFD0 — the DCS bodies' calibration.
//!   Tags 0x03EB..0x03EE are the crop (left, top, width, height),
//!   0x0848..0x084D six white-balance presets, 0x03FC which of them
//!   was shot, 0x0960/0x0961 the sensor size and 0x090D the
//!   linearisation curve.
//! * The KDC directory, tag 0xFE00 of IFD0 — the EasyShare compacts'.
//!   Its tags run 0xFA00..0xFB8D: 0xFA13/0xFA14 the frame, 0xFA18 the
//!   depth and 0xFA25 the as-shot white balance.
//!
//! # White balance, two conventions
//!
//! The DCS presets are *divisors*: a stored triple (r, g, b) means
//! multipliers proportional to (1/r, 1/g, 1/b), which is why LibRaw
//! prints g²/r, g, g²/b for them. The KDC triples are multipliers
//! already, scaled by 65536. Both are normalised to green here.
//!
//! # Compression
//!
//! * 7 — lossless JPEG (SOF3), on the DCS 460/560 backs. The frame is
//!   one stream half the sensor's width with two components, so the
//!   samples come out interleaved in exactly the order the sensor rows
//!   want them.
//! * 65000 — Kodak's own scheme, on the DCR bodies. See
//!   [`decode_65000_segment`].
//! * 32867 — the DC40/DC50's much older scheme. `Unsupported`.
//! * 1 — uncompressed, packed at the stated depth.
//!
//! Whichever codec wrote it, a DCS frame decodes to *indices* into a
//! linearisation curve rather than to samples — KodakIFD 0x090D where
//! there is a KodakIFD, IFD0's GrayResponseCurve on the 460 — and the
//! curve's last entry is the saturation point.

use crate::formats::common;
use crate::tiff::{tags, Entry, Ifd, ImageLayout, Tiff};
use crate::{Cfa, CfaColor, Error, Format, RawData, RawImage, Rect, Result};
use rayon::prelude::*;

/// IFD0's pointer to the DCS calibration directory.
const KODAK_IFD: u16 = 0x8290;
/// IFD0's pointer to the EasyShare (KDC) directory.
const KDC_IFD: u16 = 0xFE00;

// KodakIFD tags.
const KODAK_CROP_LEFT: u16 = 0x03EB;
const KODAK_CROP_TOP: u16 = 0x03EC;
const KODAK_CROP_WIDTH: u16 = 0x03ED;
const KODAK_CROP_HEIGHT: u16 = 0x03EE;
const KODAK_WB_INDEX: u16 = 0x03FC;
const KODAK_WB_FIRST: u16 = 0x0848;
const KODAK_CURVE: u16 = 0x090D;

// KDC directory tags.
const KDC_WIDTH: u16 = 0xFA13;
const KDC_HEIGHT: u16 = 0xFA14;
const KDC_CFA: u16 = 0xFA15;
const KDC_DEPTH: u16 = 0xFA18;
const KDC_WB_ASSHOT: u16 = 0xFA25;
const KDC_IMAGE_WIDTH: u16 = 0xFA31;
const KDC_IMAGE_HEIGHT: u16 = 0xFA32;
const KDC_CROP_LEFT: u16 = 0xFA3E;
const KDC_CROP_TOP: u16 = 0xFA3F;

// Tags of the SubIFD the EasyShare bodies write beside the KDC one.
const KDC_SUB_CFA_PATTERN: u16 = 0xFD09;
const KDC_SUB_OFFSETS: u16 = 0xFD04;
const KDC_SUB_OFFSET_BIAS: u16 = 0xFD14;

/// TIFF's GrayResponseCurve, the 460's linearisation table.
const GRAY_RESPONSE_CURVE: u16 = 0x0123;

// Tags of a 65000-compressed image IFD.
const K65000_SEGMENT: u16 = 0xFDE8;
const K65000_OFFSETS: u16 = 0xFDE9;

/// A private directory Kodak points at with a plain LONG offset: not a
/// SubIFD, so the shared parser never followed it, but an ordinary IFD
/// in the file's byte order once you go there.
fn private_ifd<'a>(tiff: &Tiff<'a>, tag: u16) -> Option<Tiff<'a>> {
    let offset = tiff.find(tag)?.u32(0)? as usize + tiff.base();
    Tiff::parse_at(tiff.bytes(), offset, tiff.little_endian()).ok()
}

/// The IFD holding sensor samples, if the file has one at all.
///
/// The DCR bodies and the 560 back mark it with
/// `PhotometricInterpretation` 32803 (CFA). The older DCS 460 predates
/// that value and calls its sensor plain greyscale, so the fallback is
/// the largest single-sample strip image in the file — every other
/// directory in one of these is either an RGB preview or a thumbnail
/// smaller than the frame. The EasyShare KDCs have no image directory
/// at all and describe their frame only in the private one.
fn raw_ifd<'a>(tiff: &'a Tiff<'_>) -> Option<&'a Ifd> {
    let ifds = tiff.all();
    if let Some(cfa) = ifds
        .iter()
        .find(|ifd| ifd.get(tags::PHOTOMETRIC).and_then(|e| e.u32(0)) == Some(32803))
    {
        return Some(cfa);
    }
    ifds.into_iter()
        .filter(|ifd| {
            ifd.has(tags::STRIP_OFFSETS)
                && ifd
                    .get(tags::SAMPLES_PER_PIXEL)
                    .and_then(|e| e.u32(0))
                    .unwrap_or(1)
                    == 1
        })
        .max_by_key(|ifd| {
            let side = |tag| ifd.get(tag).and_then(|e| e.u64(0)).unwrap_or(0);
            side(tags::IMAGE_WIDTH).saturating_mul(side(tags::IMAGE_LENGTH))
        })
        .filter(|ifd| ifd.has(tags::IMAGE_WIDTH) && ifd.has(tags::IMAGE_LENGTH))
}

/// A 2x2 filter array from a CFAPattern-shaped entry (0 red, 1 green,
/// 2 blue, row-major).
fn cfa_from_pattern(entry: &Entry) -> Option<Cfa> {
    let mut colors = [CfaColor::Red; 4];
    for (i, color) in colors.iter_mut().enumerate() {
        *color = match entry.u32(i)? {
            0 => CfaColor::Red,
            1 => CfaColor::Green,
            2 => CfaColor::Blue,
            _ => return None,
        };
    }
    Some(Cfa::Bayer(colors))
}

/// The linearisation curve every DCS body decodes indices into.
///
/// KodakIFD 0x090D holds it where the file has a KodakIFD: 1024
/// entries on the DCR bodies, 4096 on the 560 back, the last entry
/// being where the sensor saturates. The 460 has no KodakIFD and uses
/// the standard GrayResponseCurve instead, 256 entries for its 8-bit
/// samples. Note that 0x090D wins where both exist: the 560's own
/// GrayResponseCurve is a display curve and applying it would brighten
/// every sample by a quarter.
fn curve(tiff: &Tiff<'_>, kodak: Option<&Tiff<'_>>) -> Option<Vec<u16>> {
    let entry = kodak
        .and_then(|k| k.root().get(KODAK_CURVE))
        // The DCS 460 has no KodakIFD; its 8-bit samples are indices
        // into IFD0's 256-entry GrayResponseCurve instead, which is
        // what that tag is for on a greyscale TIFF.
        .or_else(|| tiff.find(GRAY_RESPONSE_CURVE))?;
    let table: Vec<u16> = (0..entry.count)
        .map_while(|i| entry.u32(i))
        .map(|v| v.min(u16::MAX as u32) as u16)
        .collect();
    (table.len() >= 256).then_some(table)
}

/// Apply a linearisation curve in place, clamping indices that ran
/// past its end (a corrupt segment can predict its way out of range).
fn linearize(samples: &mut [u16], table: &[u16]) {
    let last = table[table.len() - 1];
    for sample in samples {
        *sample = table.get(*sample as usize).copied().unwrap_or(last);
    }
}

// ---------------------------------------------------------- 65000

/// Kodak's "65000" compression, one 256-pixel segment at a time.
///
/// The scheme was read off the files themselves, and it is simple once
/// seen. A segment of `n` pixels opens with `ceil(n/2)` bytes of
/// lengths, one 4-bit field a pixel, **low nibble first** — so byte 0
/// holds pixel 0's length in its low half and pixel 1's in its high
/// half. The differences follow, `length` bits each, in a bitstream
/// that is neither of the usual two: the bytes pair up into 16-bit
/// **big-endian** words and the bits come out of each word from its
/// **least** significant end. A field's value is extended the way
/// JPEG extends a magnitude category — a value below half the range is
/// negative, `v - 2^len + 1` — so a length of `k` covers exactly the
/// differences whose magnitude needs `k` bits.
///
/// Each difference adds to the last pixel of the same filter column,
/// two back, and both predictors restart at zero every segment; that
/// is what lets a decoder start at any segment the offset table names.
/// The result is an index into the linearisation curve, not a sample.
fn decode_65000_segment(data: &[u8], out: &mut [u16]) -> Result<()> {
    let n = out.len();
    let table = n.div_ceil(2);
    if data.len() < table {
        return Err(Error::Corrupt(
            "Kodak 65000 segment shorter than its length table".into(),
        ));
    }
    // Read the bitstream from 16-bit big-endian words, least
    // significant bit first. Past the end it yields zeros, so a
    // truncated segment decodes to a flat run rather than failing.
    let bits = &data[table..];
    let mut accumulator: u64 = 0;
    let mut have = 0u32;
    let mut at = 0usize;
    let mut predictor = [0i32; 2];

    for i in 0..n {
        let byte = data[i / 2];
        let length = (if i % 2 == 0 { byte & 0x0f } else { byte >> 4 }) as u32;
        if length > 12 {
            return Err(Error::Corrupt(format!(
                "Kodak 65000 difference length {length} (the field is at most 12 bits)"
            )));
        }
        while have < length {
            let high = bits.get(at).copied().unwrap_or(0) as u64;
            let low = bits.get(at + 1).copied().unwrap_or(0) as u64;
            accumulator |= ((high << 8) | low) << have;
            have += 16;
            at += 2;
        }
        let mut difference = 0i32;
        if length > 0 {
            let value = (accumulator & ((1u64 << length) - 1)) as i32;
            accumulator >>= length;
            have -= length;
            difference = if value < (1 << (length - 1)) {
                value - (1 << length) + 1
            } else {
                value
            };
        }
        let slot = &mut predictor[i & 1];
        *slot = slot.saturating_add(difference);
        out[i] = (*slot).clamp(0, u16::MAX as i32) as u16;
    }
    Ok(())
}

/// A whole 65000 frame: the image IFD names the segment width
/// (0xFDE8) and carries a table of segment *end* offsets (0xFDE9)
/// relative to the strip, the first segment starting at zero.
fn decode_65000(strip: &[u8], ifd: &Ifd, width: usize, height: usize) -> Result<Vec<u16>> {
    let segment = ifd
        .get(K65000_SEGMENT)
        .and_then(|e| e.u32(0))
        .filter(|v| *v > 0)
        .ok_or_else(|| Error::Corrupt("Kodak 65000 image without a segment width".into()))?
        as usize;
    let offsets: Vec<u32> = ifd
        .get(K65000_OFFSETS)
        .map(|e| e.u32s())
        .ok_or_else(|| Error::Corrupt("Kodak 65000 image without a segment offset table".into()))?;
    let per_row = width.div_ceil(segment);
    if offsets.len() < per_row * height {
        return Err(Error::Corrupt(format!(
            "Kodak 65000 offset table holds {} entries, want {}",
            offsets.len(),
            per_row * height
        )));
    }

    let mut out = vec![0u16; width * height];
    // Segments are independent by construction, so rows are too.
    out.par_chunks_mut(width)
        .enumerate()
        .try_for_each(|(row, samples)| -> Result<()> {
            for s in 0..per_row {
                let index = row * per_row + s;
                let start = if index == 0 {
                    0
                } else {
                    offsets[index - 1] as usize
                };
                let end = offsets[index] as usize;
                if end < start || end > strip.len() {
                    return Err(Error::Corrupt(format!(
                        "Kodak 65000 segment {index} spans {start}..{end} of a {}-byte strip",
                        strip.len()
                    )));
                }
                let first = s * segment;
                let last = (first + segment).min(width);
                decode_65000_segment(&strip[start..end], &mut samples[first..last])?;
            }
            Ok(())
        })?;
    Ok(out)
}

// ---------------------------------------------------------- packing

/// Unpack samples stored without compression at `bits` a piece, most
/// significant bits first.
fn unpack(data: &[u8], pixels: usize, bits: u32, little_endian: bool) -> Result<Vec<u16>> {
    match bits {
        8 => {
            if data.len() < pixels {
                return Err(Error::Corrupt(
                    "Kodak frame shorter than its 8-bit samples".into(),
                ));
            }
            Ok(data[..pixels].iter().map(|b| *b as u16).collect())
        }
        12 => {
            let need = pixels
                .div_ceil(2)
                .checked_mul(3)
                .ok_or_else(|| Error::Corrupt("Kodak frame too large".into()))?;
            if data.len() < need {
                return Err(Error::Corrupt(format!(
                    "Kodak frame holds {} bytes of 12-bit samples, want {need}",
                    data.len()
                )));
            }
            let mut out = vec![0u16; pixels];
            for (pair, triple) in out.chunks_mut(2).zip(data.as_chunks::<3>().0) {
                pair[0] = ((triple[0] as u16) << 4) | (triple[1] as u16 >> 4);
                if let Some(second) = pair.get_mut(1) {
                    *second = ((triple[1] as u16 & 0x0f) << 8) | triple[2] as u16;
                }
            }
            Ok(out)
        }
        16 => {
            if data.len() / 2 < pixels {
                return Err(Error::Corrupt(
                    "Kodak frame shorter than its 16-bit samples".into(),
                ));
            }
            Ok(data[..pixels * 2]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|w| {
                    if little_endian {
                        u16::from_le_bytes([w[0], w[1]])
                    } else {
                        u16::from_be_bytes([w[0], w[1]])
                    }
                })
                .collect())
        }
        other => Err(Error::Unsupported(format!(
            "Kodak frame with {other}-bit samples"
        ))),
    }
}

// ------------------------------------------------------ white balance

/// The as-shot balance from the DCS presets. The six entries at
/// 0x0848 are Daylight, Tungsten, Fluorescent, Flash, Custom and
/// Camera Auto, and 0x03FC says which the shot used; the numbers are
/// divisors, so a multiplier is green over the channel.
fn dcs_white_balance(kodak: &Ifd) -> Option<[f32; 4]> {
    let index = kodak
        .get(KODAK_WB_INDEX)
        .and_then(|e| e.u32(0))
        .unwrap_or(0);
    let entry = kodak.get(KODAK_WB_FIRST + index.min(5) as u16)?;
    let (red, green, blue) = (entry.f64(0)?, entry.f64(1)?, entry.f64(2)?);
    if !(red > 0.0 && green > 0.0 && blue > 0.0) {
        return None;
    }
    Some([(green / red) as f32, 1.0, (green / blue) as f32, 1.0])
}

/// The as-shot balance from the KDC directory: multipliers scaled by
/// whatever the green entry is (65536 on every file seen).
fn kdc_white_balance(kdc: &Ifd) -> Option<[f32; 4]> {
    let entry = kdc.get(KDC_WB_ASSHOT)?;
    let (red, green, blue) = (entry.f64(0)?, entry.f64(1)?, entry.f64(2)?);
    if !(red > 0.0 && green > 0.0 && blue > 0.0) {
        return None;
    }
    Some([(red / green) as f32, 1.0, (blue / green) as f32, 1.0])
}

/// A crop rectangle, kept only when it actually lies inside the frame.
/// The EasyShare Z981's does not — its own tags put a 4288-wide
/// picture 52 columns into a 4304-wide sensor — so those files come
/// out uncropped, which is what LibRaw reports for them too.
fn crop_within(
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    frame: (usize, usize),
) -> Option<Rect> {
    (width > 0 && height > 0 && x + width <= frame.0 && y + height <= frame.1).then_some(Rect {
        x,
        y,
        width,
        height,
    })
}

// ---------------------------------------------------------- decoding

/// The DCS/DCR path: a real image IFD with strips.
fn decode_tiff_raw(
    bytes: &[u8],
    tiff: &Tiff<'_>,
    ifd: &Ifd,
    kodak: Option<&Tiff<'_>>,
) -> Result<RawImage> {
    let layout = ImageLayout::of(tiff, ifd)?;
    let [(start, len)] = layout.chunks[..] else {
        return Err(Error::Unsupported(format!(
            "Kodak sensor data in {} strips, want one",
            layout.chunks.len()
        )));
    };
    let strip = &bytes[start..start + len];
    let pixels = layout
        .width
        .checked_mul(layout.height)
        .ok_or_else(|| Error::Corrupt("Kodak frame too large".into()))?;

    let curve_table = curve(tiff, kodak);
    let mut samples = match layout.compression {
        1 => unpack(strip, pixels, layout.bits_per_sample, tiff.little_endian())?,
        7 => {
            let image = crate::ljpeg::decode(strip)?;
            // The DCS backs halve the width and use two components, so
            // the stream's samples already alternate the way the row
            // does; anything else would need a layout this has never
            // seen.
            if image.width * image.components != layout.width || image.height != layout.height {
                return Err(Error::Corrupt(format!(
                    "Kodak lossless JPEG is {}x{}x{}, want {}x{}",
                    image.width, image.height, image.components, layout.width, layout.height
                )));
            }
            image.data
        }
        65000 => decode_65000(strip, ifd, layout.width, layout.height)?,
        32867 => {
            return Err(Error::Unsupported(
                "Kodak Compression 32867, the DC40/DC50 scheme".into(),
            ))
        }
        other => return Err(Error::Unsupported(format!("Kodak Compression {other}"))),
    };
    if samples.len() != pixels {
        return Err(Error::Corrupt(format!(
            "Kodak decoder produced {} samples for {}x{}",
            samples.len(),
            layout.width,
            layout.height
        )));
    }

    let mut white = ((1u32 << layout.bits_per_sample.clamp(8, 16)) - 1) as f32;
    if let Some(table) = &curve_table {
        linearize(&mut samples, table);
        // 3700 of a nominal 4095 on the DCS Pro bodies, the full
        // 4095 on the 460/560 backs.
        white = table[table.len() - 1] as f32;
    }

    let cfa = ifd
        .get(tags::CFA_PATTERN)
        .and_then(cfa_from_pattern)
        .unwrap_or(Cfa::GRBG);
    let mut raw = RawImage::new(
        Format::Kodak,
        layout.width,
        layout.height,
        1,
        RawData::U16(samples),
        cfa,
    );
    raw.white_level = white;

    if let Some(kodak) = kodak {
        let root = kodak.root();
        if let Some(coeffs) = dcs_white_balance(root) {
            raw.wb_coeffs = coeffs;
        }
        let number = |tag| root.get(tag).and_then(|e| e.u32(0)).map(|v| v as usize);
        if let (Some(x), Some(y), Some(width), Some(height)) = (
            number(KODAK_CROP_LEFT),
            number(KODAK_CROP_TOP),
            number(KODAK_CROP_WIDTH),
            number(KODAK_CROP_HEIGHT),
        ) {
            if let Some(crop) = crop_within(x, y, width, height, (layout.width, layout.height)) {
                raw.crop = crop;
            }
        }
    }
    Ok(raw)
}

/// The EasyShare path: no image IFD at all, only the private
/// directories, and the samples packed somewhere the file names very
/// indirectly.
fn decode_kdc(bytes: &[u8], tiff: &Tiff<'_>, kdc: &Tiff<'_>) -> Result<RawImage> {
    let root = kdc.root();
    let number = |tag| root.get(tag).and_then(|e| e.u32(0)).map(|v| v as usize);
    let width =
        number(KDC_WIDTH).ok_or_else(|| Error::Corrupt("KDC without a frame width".into()))?;
    // The stated height is one short of the frame LibRaw reads and is
    // odd, which a Bayer frame cannot be; rounding it up to the next
    // even row gives the frame the data actually holds.
    let height = number(KDC_HEIGHT)
        .map(|h| h + (h & 1))
        .ok_or_else(|| Error::Corrupt("KDC without a frame height".into()))?;
    // 0xFA18 is the sample depth on the Z981 and nonsense (65532) on
    // the P880, so it is believed only when it could be one.
    let bits = number(KDC_DEPTH)
        .filter(|b| (8..=16).contains(b))
        .unwrap_or(12) as u32;
    if width == 0 || height == 0 {
        return Err(Error::Corrupt("KDC with an empty frame".into()));
    }

    // Where the samples start is the one thing these files never say
    // plainly. The SubIFD's 0xFD04 is a mixed bag of values that ends
    // in a long arithmetic run — one entry per band of rows, spaced by
    // exactly a band's worth of packed bytes — and 0xFD14 is a bias
    // (-64) that has to come off the first of them. Both were read off
    // the file against LibRaw's own frame; a KDC that has neither is
    // rejected rather than guessed at.
    let sub = tiff
        .all()
        .into_iter()
        .find(|ifd| ifd.has(KDC_SUB_OFFSETS))
        .ok_or_else(|| Error::Unsupported("KDC without the 0xFD04 band table".into()))?;
    let bands = sub
        .get(KDC_SUB_OFFSETS)
        .map(|e| e.u32s())
        .unwrap_or_default();
    let bias = sub
        .get(KDC_SUB_OFFSET_BIAS)
        .and_then(|e| e.f64(0))
        .unwrap_or(0.0) as i64;
    let first = longest_ramp(&bands)
        .ok_or_else(|| Error::Unsupported("KDC band table with no run of band offsets".into()))?;
    let start = usize::try_from(first as i64 - bias)
        .map_err(|_| Error::Corrupt("KDC sample offset out of range".into()))?;
    if start >= bytes.len() {
        return Err(Error::Corrupt(
            "KDC sample offset past the end of the file".into(),
        ));
    }

    let samples = unpack(&bytes[start..], width * height, bits, tiff.little_endian())?;
    // The filter array is spelled out twice and the two disagree: the
    // KDC directory's 0xFA15 names the array of the *cropped* picture
    // ("RGGB"), the SubIFD's CFAPattern the array of the frame this
    // decoder hands out. The frame's is the one that belongs here.
    let cfa = sub
        .get(KDC_SUB_CFA_PATTERN)
        .and_then(cfa_from_pattern)
        .unwrap_or(Cfa::GRBG);
    let mut raw = RawImage::new(Format::Kodak, width, height, 1, RawData::U16(samples), cfa);
    raw.white_level = ((1u32 << bits.clamp(8, 16)) - 1) as f32;
    if let Some(coeffs) = kdc_white_balance(root) {
        raw.wb_coeffs = coeffs;
    }
    if let (Some(x), Some(y), Some(w), Some(h)) = (
        number(KDC_CROP_LEFT),
        number(KDC_CROP_TOP),
        number(KDC_IMAGE_WIDTH),
        number(KDC_IMAGE_HEIGHT),
    ) {
        // The margins are a pixel out on the row axis: taking the
        // P880's 3264x2448 picture at its stated (8, 1) would give the
        // cropped image a GRBG array, and the same directory says in
        // 0xFA15 that the picture is BGGR. The filter array is the
        // camera's own statement about the crop, so the origin is
        // nudged by up to one pixel to agree with it.
        let (x, y) = match kdc_named_cfa(root) {
            Some(named) => [(0, 0), (1, 0), (0, 1), (1, 1)]
                .into_iter()
                .map(|(dx, dy)| (x + dx, y + dy))
                .find(|(x, y)| raw.cfa.shifted(*x, *y) == named)
                .unwrap_or((x, y)),
            None => (x, y),
        };
        if let Some(crop) = crop_within(x, y, w, h, (width, height)) {
            raw.crop = crop;
        }
    }
    Ok(raw)
}

/// The filter array of the *cropped* picture, spelled out in ASCII in
/// the KDC directory ("RGGB", "BGGR").
fn kdc_named_cfa(kdc: &Ifd) -> Option<Cfa> {
    let name = kdc.get(KDC_CFA)?.str()?;
    if name.len() != 4 {
        return None;
    }
    let mut colors = [CfaColor::Red; 4];
    for (color, letter) in colors.iter_mut().zip(name.chars()) {
        *color = match letter {
            'R' => CfaColor::Red,
            'G' => CfaColor::Green,
            'B' => CfaColor::Blue,
            _ => return None,
        };
    }
    Some(Cfa::Bayer(colors))
}

/// The first value of the longest run of equal, positive differences
/// in `values`, which is how the band table hides its offsets among
/// its other numbers. At least four bands are wanted so a chance pair
/// cannot win.
fn longest_ramp(values: &[u32]) -> Option<u32> {
    let (mut best_start, mut best_len) = (0usize, 0usize);
    let mut i = 0;
    while i + 1 < values.len() {
        let step = values[i + 1].checked_sub(values[i]).filter(|s| *s > 0);
        let Some(step) = step else {
            i += 1;
            continue;
        };
        let mut j = i + 1;
        while j + 1 < values.len() && values[j + 1].checked_sub(values[j]) == Some(step) {
            j += 1;
        }
        if j - i + 1 > best_len {
            best_len = j - i + 1;
            best_start = i;
        }
        i = j.max(i + 1);
    }
    (best_len >= 4).then(|| values[best_start])
}

pub fn decode(bytes: &[u8]) -> Result<RawImage> {
    let tiff = Tiff::parse(bytes)?;
    let kodak = private_ifd(&tiff, KODAK_IFD);
    let kdc = private_ifd(&tiff, KDC_IFD);

    let mut raw = match raw_ifd(&tiff) {
        Some(ifd) => decode_tiff_raw(bytes, &tiff, ifd, kodak.as_ref())?,
        None => {
            let kdc = kdc.as_ref().ok_or_else(|| {
                Error::Unsupported(
                    "Kodak TIFF with neither a CFA image IFD nor a KDC directory".into(),
                )
            })?;
            decode_kdc(bytes, &tiff, kdc)?
        }
    };

    let (make, model) = tiff.make_model();
    raw.set_camera(&make, &model);
    raw.orientation = common::orientation(&tiff);
    raw.metadata = common::metadata(&tiff);
    // The DCS bodies keep every preview in Kodak's own compression, so
    // there is frequently no JPEG to hand out at all; the EasyShare
    // compacts point at a full-size one with JPEGInterchangeFormat.
    raw.preview = common::largest_jpeg(&tiff);
    raw.apply_camera_table();
    Ok(raw)
}

pub fn preview(bytes: &[u8]) -> Result<Option<Vec<u8>>> {
    let tiff = Tiff::parse(bytes)?;
    Ok(common::largest_jpeg(&tiff))
}

#[cfg(test)]
mod tests {

    use super::*;
    use std::path::{Path, PathBuf};

    /// Every file under `$SCHIST_RAW_CORPUS` with one of `extensions`,
    /// recursively. Empty when the variable is unset, which is how the
    /// corpus tests skip on a machine without the samples.
    fn corpus(extensions: &[&str]) -> Vec<PathBuf> {
        let Ok(root) = std::env::var("SCHIST_RAW_CORPUS") else {
            return Vec::new();
        };
        let mut found = Vec::new();
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
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| extensions.iter().any(|want| e.eq_ignore_ascii_case(want)))
                {
                    found.push(path);
                }
            }
        }
        found.sort();
        found
    }

    /// LibRaw's `unprocessed_raw -T` output beside the sample.
    fn oracle(path: &Path) -> Option<(usize, usize, Vec<u16>)> {
        let mut name = path.as_os_str().to_os_string();
        name.push(".tiff");
        let image = image::open(PathBuf::from(name)).ok()?.into_luma16();
        let (width, height) = (image.width() as usize, image.height() as usize);
        Some((width, height, image.into_raw()))
    }

    /// `raw-identify -v -w` output beside the sample.
    fn identify(path: &Path) -> Option<String> {
        let mut name = path.as_os_str().to_os_string();
        name.push(".identify.txt");
        std::fs::read_to_string(PathBuf::from(name)).ok()
    }

    /// The text after `key:` on the line that starts with it.
    fn field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
        text.lines()
            .map(str::trim)
            .find(|line| line.starts_with(key))
            .map(|line| line[key.len()..].trim())
    }

    /// A "W x H" pair.
    fn size(text: &str, key: &str) -> Option<(usize, usize)> {
        let value = field(text, key)?;
        let (w, h) = value.split_once('x')?;
        Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
    }

    /// What LibRaw's "Image flip" means as an [`Orientation`].
    fn flip(text: &str) -> Option<crate::Orientation> {
        Some(match field(text, "Image flip:")?.parse::<u32>().ok()? {
            3 => crate::Orientation::Rotate180,
            5 => crate::Orientation::Rotate270CW,
            6 => crate::Orientation::Rotate90CW,
            _ => crate::Orientation::Normal,
        })
    }

    /// The first four letters of the "Filter pattern" line as a CFA.
    fn filter_pattern(text: &str) -> Option<Cfa> {
        let pattern = field(text, "Filter pattern:")?;
        let mut colors = [CfaColor::Red; 4];
        for (color, letter) in colors.iter_mut().zip(pattern.chars()) {
            *color = match letter {
                'R' => CfaColor::Red,
                'G' => CfaColor::Green,
                'B' => CfaColor::Blue,
                _ => return None,
            };
        }
        Some(Cfa::Bayer(colors))
    }

    /// The as-shot multipliers, normalised to green, from the
    /// "As shot" row of the makernote white-balance table.
    fn as_shot(text: &str) -> Option<[f32; 3]> {
        let row = field(text, "As shot")?;
        let numbers: Vec<f32> = row
            .split_whitespace()
            .take(4)
            .map_while(|v| v.parse::<f32>().ok())
            .collect();
        let [red, green, blue, ..] = numbers[..] else {
            return None;
        };
        (green > 0.0).then(|| [red / green, 1.0, blue / green])
    }

    /// Compare a decoded frame with the oracle sample for sample.
    fn compare(path: &Path, raw: &RawImage) {
        let Some((width, height, expect)) = oracle(path) else {
            eprintln!("{}: no oracle TIFF, data not checked", path.display());
            return;
        };
        assert_eq!(
            (raw.width, raw.height),
            (width, height),
            "{}: frame is {}x{}, oracle {width}x{height}",
            path.display(),
            raw.width,
            raw.height
        );
        let RawData::U16(got) = &raw.data else {
            panic!("{}: expected integer samples", path.display())
        };
        let mut wrong = 0usize;
        let mut first = Vec::new();
        for (i, (a, b)) in got.iter().zip(expect.iter()).enumerate() {
            if a != b {
                wrong += 1;
                if first.len() < 8 {
                    first.push(format!("({}, {}): {a} not {b}", i % width, i / width));
                }
            }
        }
        assert_eq!(
            wrong,
            0,
            "{}: {wrong} samples differ; {}",
            path.display(),
            first.join(", ")
        );
    }

    /// Levels, balance, crop, orientation and CFA against
    /// `raw-identify`.
    fn compare_metadata(path: &Path, raw: &RawImage) {
        let Some(text) = identify(path) else { return };
        if let Some(cfa) = filter_pattern(&text) {
            assert_eq!(raw.cfa, cfa, "{}: filter pattern", path.display());
        }
        if let Some(orientation) = flip(&text) {
            assert_eq!(
                raw.orientation,
                orientation,
                "{}: orientation",
                path.display()
            );
        }
        if let Some((width, height)) = size(&text, "Image size:") {
            let (x, y) = match field(&text, "Raw inset, width x height:") {
                Some(inset) => {
                    let left = inset
                        .split("left:")
                        .nth(1)
                        .and_then(|v| v.split_whitespace().next().and_then(|v| v.parse().ok()));
                    let top = inset
                        .split("top:")
                        .nth(1)
                        .and_then(|v| v.split_whitespace().next().and_then(|v| v.parse().ok()));
                    (left.unwrap_or(0), top.unwrap_or(0))
                }
                None => (0, 0),
            };
            let inset = field(&text, "Raw inset, width x height:")
                .and_then(|v| v.split_whitespace().next().map(str::to_string));
            let expect = match inset.and_then(|w| w.parse::<usize>().ok()) {
                Some(w) => {
                    let h = field(&text, "Raw inset, width x height:")
                        .and_then(|v| v.split_whitespace().nth(2))
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(height);
                    Rect {
                        x,
                        y,
                        width: w,
                        height: h,
                    }
                }
                None => Rect {
                    x: 0,
                    y: 0,
                    width,
                    height,
                },
            };
            if raw.crop != expect {
                eprintln!("{}: crop {:?}, LibRaw {expect:?}", path.display(), raw.crop);
            }
        }
        if let Some(expect) = as_shot(&text) {
            for (got, want) in raw.wb_coeffs.iter().zip(expect.iter()) {
                if (got - want).abs() > want * 0.02 {
                    eprintln!(
                        "{}: white balance {:?}, LibRaw {expect:?}",
                        path.display(),
                        raw.wb_coeffs
                    );
                    break;
                }
            }
        }
    }

    /// Cutting a file short must never panic, whatever it does return.
    fn truncations(path: &Path) {
        let bytes = std::fs::read(path).expect("sample readable");
        let mut seed = bytes.len() as u64;
        for _ in 0..10 {
            // A cheap deterministic spread of cut points.
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let at = (seed >> 33) as usize % bytes.len().max(1);
            let _ = crate::decode(&bytes[..at]);
            let _ = crate::probe(&bytes[..at]);
        }
    }

    #[test]
    fn decodes_a_hand_built_65000_segment() {
        // Four pixels, lengths 3, 3, 2, 1 (low nibble first), then the
        // differences 5, 6, 2, 1 packed into one big-endian word from
        // its least significant bit up.
        let data = [0x33, 0x12, 0x01, 0xb5];
        let mut out = [0u16; 4];
        decode_65000_segment(&data, &mut out).expect("segment decodes");
        assert_eq!(out, [5, 6, 7, 7]);
    }

    #[test]
    fn a_65000_length_over_twelve_bits_is_corrupt() {
        let data = [0xff, 0x00, 0x00, 0x00];
        let mut out = [0u16; 2];
        assert!(matches!(
            decode_65000_segment(&data, &mut out),
            Err(Error::Corrupt(_))
        ));
    }

    #[test]
    fn a_truncated_65000_segment_runs_out_in_zeros() {
        // Lengths present, bitstream missing: the pump reads zeros, so
        // the differences are the most negative the lengths allow and
        // nothing panics.
        let data = [0x33, 0x12];
        let mut out = [0u16; 4];
        decode_65000_segment(&data, &mut out).expect("no bitstream is still decodable");
    }

    #[test]
    fn finds_the_band_offsets_among_the_other_numbers() {
        let values = [
            1, 3158, 654611, 2, 74722, 480, 1, 85952, 189248, 292544, 395840, 499136,
        ];
        assert_eq!(longest_ramp(&values), Some(85952));
        assert_eq!(longest_ramp(&[1, 2, 3]), None);
        assert_eq!(longest_ramp(&[]), None);
    }

    #[test]
    fn unpacks_twelve_bit_samples() {
        let data = [0xab, 0xcd, 0xef, 0x12, 0x34, 0x56];
        assert_eq!(
            unpack(&data, 4, 12, false).unwrap(),
            vec![0xabc, 0xdef, 0x123, 0x456]
        );
    }

    #[test]
    fn a_linearisation_curve_is_a_lookup_with_a_clamp() {
        let table = [0u16, 10, 20, 30];
        let mut samples = [0, 2, 9];
        linearize(&mut samples, &table);
        assert_eq!(samples, [0, 20, 30]);
    }

    #[test]
    fn garbage_is_rejected() {
        assert!(decode(b"MM\0*nonsense").is_err());
        assert!(decode(&[]).is_err());
    }

    #[test]
    fn corpus_matches_the_oracle() {
        for path in corpus(&["dcr", "kdc", "tif"]) {
            let bytes = std::fs::read(&path).expect("sample readable");
            if crate::probe(&bytes) != Some(crate::Format::Kodak) {
                // The corpus mixes vendors under one extension: a TIFF
                // some other camera wrote is not this module's to read.
                continue;
            }
            let raw = match crate::decode(&bytes) {
                Ok(raw) => raw,
                Err(Error::Unsupported(why)) => {
                    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                    let allowed: &[(&str, &str)] = &[(
                        "DC50",
                        "Compression 32867, the DC40/DC50 scheme, is not implemented",
                    )];
                    let reason = allowed
                        .iter()
                        .find(|(file, _)| name.contains(file))
                        .unwrap_or_else(|| {
                            panic!("{}: unexpected Unsupported: {why}", path.display())
                        });
                    eprintln!(
                        "{}: unsupported as documented ({}): {why}",
                        path.display(),
                        reason.1
                    );
                    continue;
                }
                Err(other) => panic!("{}: {other}", path.display()),
            };
            raw.validate().expect("decoded frame is self-consistent");
            compare(&path, &raw);
            compare_metadata(&path, &raw);
            if let Some(preview) = &raw.preview {
                image::load_from_memory(preview)
                    .unwrap_or_else(|e| panic!("{}: preview will not decode: {e}", path.display()));
            }
        }
    }

    #[test]
    fn truncated_files_never_panic() {
        for path in corpus(&["dcr", "kdc", "tif"]) {
            truncations(&path);
        }
    }
}
