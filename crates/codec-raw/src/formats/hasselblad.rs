//! Hasselblad 3FR and FFF: the raw files of the H, X and CFV backs,
//! and of the Imacon Ixpress backs Hasselblad bought.
//!
//! Both extensions hold the same thing — an ordinary TIFF (little
//! endian for 3FR, big endian for the FFF a Phocus session writes)
//! whose sensor IFD is marked `PhotometricInterpretation = 32803`
//! (CFA) and carries the DNG level and crop tags. Only the
//! compression differs: `1` is a plain 16-bit raster, `7` (and the
//! `9` a few writers use) is Hasselblad's own lossless JPEG.
//!
//! That JPEG is where the format earns its module. The stream is a
//! real SOF3 frame — 16-bit precision, one component, the sensor's
//! dimensions, a single 17-symbol Huffman table — but the scan under
//! it is not T.81's:
//!
//!  * the entropy data is **not** byte-stuffed. `0xFF` bytes appear
//!    raw, so a JPEG-aware bit reader would stop at the first one it
//!    mistook for a marker.
//!  * bits are read most-significant-first out of **32-bit
//!    little-endian words**, not out of the byte stream
//!    ([`BitPumpMsb32`]).
//!  * pixels are coded in pairs sharing one code group: both
//!    difference *lengths* first, then both difference *values*.
//!  * the two pixels of a pair belong to two independent prediction
//!    chains (even and odd columns), each reset to 32768 at the start
//!    of every row. The scan itself runs on without a break, so rows
//!    are not byte-aligned.
//!  * a length of 16 is followed by sixteen value bits rather than
//!    standing for 32768 on its own, and the value 65535 in those
//!    bits is an escape for -32768 (the one difference the ordinary
//!    sign rule cannot spell).
//!
//! `SOS`'s `Ss` byte, which would be T.81's predictor selector, is 8
//! — outside the standard's 1..=7 — which is the format admitting it
//! is not doing T.81 prediction. That is why this module decodes the
//! scan itself instead of calling [`crate::ljpeg`].

use crate::bits::{BitPump, BitPumpMsb32, HuffTable};
use crate::formats::common;
use crate::tiff::{tags, Ifd, Tiff};
use crate::{Cfa, Error, Format, RawData, RawImage, Rect, Result};

/// DNG tags Hasselblad writes in its sensor IFD and IFD0.
mod hb_tags {
    pub const BLACK_LEVEL: u16 = 0xC61A;
    pub const WHITE_LEVEL: u16 = 0xC61D;
    pub const DEFAULT_CROP_ORIGIN: u16 = 0xC61F;
    pub const DEFAULT_CROP_SIZE: u16 = 0xC620;
    pub const COLOR_MATRIX_1: u16 = 0xC621;
    pub const COLOR_MATRIX_2: u16 = 0xC622;
    pub const AS_SHOT_NEUTRAL: u16 = 0xC628;
    pub const UNIQUE_CAMERA_MODEL: u16 = 0xC614;
}

/// The sensor IFD: the one that says it holds a colour filter array.
fn raw_ifd<'a>(tiff: &'a Tiff<'_>) -> Result<&'a Ifd> {
    tiff.all()
        .into_iter()
        .find(|ifd| {
            matches!(
                ifd.get(tags::PHOTOMETRIC).and_then(|e| e.u32(0)),
                Some(32803 | 34892)
            )
        })
        .ok_or_else(|| Error::Corrupt("no CFA IFD in this Hasselblad file".into()))
}

/// The sensor data's byte range.
///
/// Not [`crate::tiff::ImageLayout`], because Hasselblad's
/// `StripByteCounts` cannot be trusted: the Ixpress CF132 sample
/// stores the *uncompressed* size there, 44,695,552 bytes in a
/// 33,503,232-byte file, and means "to the end". Clamping is the only
/// reading that decodes it, and it costs nothing for the files whose
/// count is right.
fn strip(tiff: &Tiff<'_>, ifd: &Ifd) -> Result<(usize, usize)> {
    let base = tiff.base();
    let offset = ifd
        .get(tags::STRIP_OFFSETS)
        .and_then(|e| e.u64(0))
        .and_then(|o| usize::try_from(o).ok())
        .and_then(|o| o.checked_add(base))
        .ok_or_else(|| Error::Corrupt("Hasselblad CFA IFD without StripOffsets".into()))?;
    if offset >= tiff.bytes().len() {
        return Err(Error::Corrupt(
            "Hasselblad strip starts past the end of the file".into(),
        ));
    }
    let available = tiff.bytes().len() - offset;
    let count = ifd
        .get(tags::STRIP_BYTE_COUNTS)
        .and_then(|e| e.u64(0))
        .and_then(|c| usize::try_from(c).ok())
        .unwrap_or(available)
        .min(available);
    Ok((offset, count))
}

/// What the markers before the scan say.
struct Frame {
    width: usize,
    height: usize,
    huffman: HuffTable,
    /// Offset of the first entropy-coded byte within the stream.
    scan: usize,
}

/// Parse SOI/SOF3/DHT/SOS. Only the pieces this scan needs are kept:
/// the frame size to cross-check the IFD with, and the single
/// difference-length table. Unknown segments are skipped by length,
/// and `0xFF` fill bytes between segments are allowed (Hasselblad
/// writes one before its DHT).
fn frame(stream: &[u8]) -> Result<Frame> {
    if stream.get(0..2) != Some(&[0xFF, 0xD8]) {
        return Err(Error::Corrupt("Hasselblad raw strip is not a JPEG".into()));
    }
    let (mut width, mut height, mut huffman) = (0usize, 0usize, None);
    let mut at = 2;
    loop {
        // Fill bytes: any number of 0xFF may precede a marker.
        while stream.get(at) == Some(&0xFF) && stream.get(at + 1) == Some(&0xFF) {
            at += 1;
        }
        let (Some(0xFF), Some(&marker)) = (stream.get(at), stream.get(at + 1)) else {
            return Err(Error::Corrupt(format!(
                "Hasselblad JPEG: no marker at byte {at}"
            )));
        };
        let length = stream
            .get(at + 2..at + 4)
            .map(|b| u16::from_be_bytes([b[0], b[1]]) as usize)
            .filter(|l| *l >= 2)
            .ok_or_else(|| Error::Corrupt("Hasselblad JPEG: truncated segment".into()))?;
        let body = stream
            .get(at + 4..at + 2 + length)
            .ok_or_else(|| Error::Corrupt("Hasselblad JPEG: segment past the end".into()))?;
        match marker {
            // SOF3, lossless. Anything else that starts a frame is a
            // stream this module has never seen and cannot check.
            0xC3 => {
                if body.len() < 6 {
                    return Err(Error::Corrupt("Hasselblad JPEG: short SOF".into()));
                }
                height = u16::from_be_bytes([body[1], body[2]]) as usize;
                width = u16::from_be_bytes([body[3], body[4]]) as usize;
                if body[5] != 1 {
                    return Err(Error::Unsupported(format!(
                        "Hasselblad JPEG with {} components",
                        body[5]
                    )));
                }
            }
            0xC4 => {
                // One table, always id 0; the scan has a single
                // component so there is nothing to select between.
                let counts = body
                    .get(1..17)
                    .ok_or_else(|| Error::Corrupt("Hasselblad JPEG: short DHT".into()))?;
                let symbols = &body[17..];
                huffman = Some(HuffTable::new(counts, symbols)?);
            }
            0xDA => {
                let huffman = huffman.ok_or_else(|| {
                    Error::Corrupt("Hasselblad JPEG: scan with no Huffman table".into())
                })?;
                if width == 0 || height == 0 {
                    return Err(Error::Corrupt("Hasselblad JPEG: scan with no frame".into()));
                }
                return Ok(Frame {
                    width,
                    height,
                    huffman,
                    scan: at + 2 + length,
                });
            }
            0xD9 => {
                return Err(Error::Corrupt(
                    "Hasselblad JPEG ends before its scan".into(),
                ))
            }
            _ => {}
        }
        at += 2 + length;
    }
}

/// One difference: the length symbol has already been read, so this
/// only takes the value bits.
///
/// The sign rule is T.81's — a leading zero bit means the negative
/// half of the category — with Hasselblad's escape on top: sixteen
/// value bits reading 65535 mean -32768. Under the ordinary rule
/// those bits would mean +65535, which as a 16-bit wrap is -1, a
/// difference the shorter categories already spell; -32768 has no
/// other spelling, so the encoder borrows the redundant code for it.
#[inline]
fn difference(pump: &mut BitPumpMsb32<'_>, length: u32) -> i32 {
    if length == 0 || length > 16 {
        return 0;
    }
    let value = pump.get(length);
    if length == 16 && value == 0xFFFF {
        return -32768;
    }
    if value < 1 << (length - 1) {
        value as i32 - (1 << length) + 1
    } else {
        value as i32
    }
}

/// Decode the scan into `width * height` samples.
fn decompress(stream: &[u8], width: usize, height: usize) -> Result<Vec<u16>> {
    let frame = frame(stream)?;
    if frame.width != width || frame.height != height {
        return Err(Error::Corrupt(format!(
            "Hasselblad JPEG is {}x{} but its IFD says {width}x{height}",
            frame.width, frame.height
        )));
    }
    // At least a bit a sample: bounds a forged frame by the data.
    let samples = crate::frame_samples(width, height, 1)?;
    if stream.len().saturating_mul(8) < samples {
        return Err(Error::Corrupt(format!(
            "Hasselblad frame of {samples} samples in {} bytes",
            stream.len()
        )));
    }
    let mut out = vec![0u16; samples];
    let mut pump = BitPumpMsb32::new(&stream[frame.scan..]);
    for row in out.chunks_exact_mut(width) {
        // Both chains start at the middle of the range, as T.81 has
        // the first sample of a frame do; here it is every row.
        let mut pred = [0x8000i32; 2];
        let mut col = 0;
        while col < width {
            let len0 = frame.huffman.decode(&mut pump) as u32;
            let len1 = frame.huffman.decode(&mut pump) as u32;
            let diff0 = difference(&mut pump, len0);
            let diff1 = difference(&mut pump, len1);
            pred[0] = pred[0].wrapping_add(diff0);
            pred[1] = pred[1].wrapping_add(diff1);
            row[col] = pred[0] as u16;
            // An odd width would leave the second half of the last
            // pair with nowhere to go; no Hasselblad has one, but the
            // bits are still consumed above so the stream stays in
            // step.
            if col + 1 < width {
                row[col + 1] = pred[1] as u16;
            }
            col += 2;
        }
    }
    Ok(out)
}

/// The 16-bit raster of the uncompressed backs, in the file's byte
/// order.
fn unpack(data: &[u8], width: usize, height: usize, little_endian: bool) -> Result<Vec<u16>> {
    let samples = crate::frame_samples(width, height, 1)?;
    if data.len() / 2 < samples {
        return Err(Error::Corrupt(format!(
            "Hasselblad strip holds {} bytes for {samples} 16-bit samples",
            data.len()
        )));
    }
    Ok(data[..samples * 2]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| {
            if little_endian {
                u16::from_le_bytes([b[0], b[1]])
            } else {
                u16::from_be_bytes([b[0], b[1]])
            }
        })
        .collect())
}

pub fn decode(bytes: &[u8]) -> Result<RawImage> {
    let tiff = Tiff::parse(bytes)?;
    let ifd = raw_ifd(&tiff)?;
    let int = |tag: u16| ifd.get(tag).and_then(|e| e.u32(0));
    let width = int(tags::IMAGE_WIDTH).unwrap_or(0) as usize;
    let height = int(tags::IMAGE_LENGTH).unwrap_or(0) as usize;
    if width == 0 || height == 0 {
        return Err(Error::Corrupt("Hasselblad CFA IFD with no size".into()));
    }
    let bits = int(tags::BITS_PER_SAMPLE).unwrap_or(16);
    if bits != 16 {
        return Err(Error::Unsupported(format!(
            "Hasselblad sensor data {bits} bits deep"
        )));
    }
    let (offset, count) = strip(&tiff, ifd)?;
    let data = &bytes[offset..offset + count];
    let compression = int(tags::COMPRESSION).unwrap_or(1);
    let samples = match compression {
        1 => unpack(data, width, height, tiff.little_endian())?,
        // 7 is TIFF's "new-style JPEG"; 9 is the private value some
        // Phocus versions write for the very same stream.
        7 | 9 => decompress(data, width, height)?,
        other => {
            return Err(Error::Unsupported(format!(
                "Hasselblad compression {other}"
            )))
        }
    };

    let mut raw = RawImage::new(
        Format::Hasselblad,
        width,
        height,
        1,
        RawData::U16(samples),
        // Every Hasselblad back this decoder has met reads RGGB from
        // the top-left of the full frame; none of them carry a
        // CFAPattern tag to say otherwise.
        Cfa::RGGB,
    );

    let root = tiff.root();
    let (make, model) = tiff.make_model();
    // Model carries the body on the modern backs ("X2D 100C",
    // "Hasselblad H5D-50c") and the shutter mode on some others
    // ("CFV 100C/Electronic Shutter", which the camera table splits).
    // Where it is missing altogether, UniqueCameraModel names the back.
    let model = if model.trim().is_empty() {
        root.get(hb_tags::UNIQUE_CAMERA_MODEL)
            .and_then(|e| e.str())
            .unwrap_or("")
            .to_string()
    } else {
        model
    };
    raw.set_camera(&make, &model);

    if let Some(black) = ifd.get(hb_tags::BLACK_LEVEL).and_then(|e| e.f64(0)) {
        raw.black_levels = [black as f32; 4];
    }
    if let Some(white) = ifd.get(hb_tags::WHITE_LEVEL).and_then(|e| e.f64(0)) {
        if white > 0.0 {
            raw.white_level = white as f32;
        }
    }
    // AsShotNeutral is the camera-space colour of white, so the
    // multipliers that make it grey are its reciprocals; green is
    // already 1 in every Hasselblad file, but normalising costs
    // nothing and keeps the promise `wb_coeffs` makes.
    if let Some(neutral) = root.get(hb_tags::AS_SHOT_NEUTRAL) {
        let value = |i: usize| neutral.f64(i).filter(|v| *v > 0.0).map(|v| 1.0 / v as f32);
        if let (Some(r), Some(g), Some(b)) = (value(0), value(1), value(2)) {
            raw.wb_coeffs = [r / g, 1.0, b / g, 1.0];
        }
    }
    // ColorMatrix2 belongs to the second calibration illuminant,
    // which is the daylight one where a file carries both.
    for tag in [hb_tags::COLOR_MATRIX_2, hb_tags::COLOR_MATRIX_1] {
        if let Some(entry) = root.get(tag) {
            if let Some(matrix) = (0..9)
                .map(|i| entry.f64(i).map(|v| v as f32))
                .collect::<Option<Vec<f32>>>()
            {
                raw.color_matrix = Some([
                    [matrix[0], matrix[1], matrix[2]],
                    [matrix[3], matrix[4], matrix[5]],
                    [matrix[6], matrix[7], matrix[8]],
                ]);
                break;
            }
        }
    }

    // DefaultCropOrigin/Size mark the frame's active area; the rest
    // is the masked border the back uses for its own black reference.
    let pair = |tag: u16| -> Option<(usize, usize)> {
        let entry = ifd.get(tag)?;
        Some((entry.f64(0)? as usize, entry.f64(1)? as usize))
    };
    if let (Some((x, y)), Some((w, h))) = (
        pair(hb_tags::DEFAULT_CROP_ORIGIN),
        pair(hb_tags::DEFAULT_CROP_SIZE),
    ) {
        let inside = x.checked_add(w).is_some_and(|r| r <= width)
            && y.checked_add(h).is_some_and(|b| b <= height);
        if inside && w > 0 && h > 0 {
            raw.crop = Rect {
                x,
                y,
                width: w,
                height: h,
            };
        }
    }
    raw.orientation = common::orientation(&tiff);
    raw.metadata = common::metadata(&tiff);
    raw.preview = common::largest_jpeg(&tiff);
    raw.apply_camera_table();
    Ok(raw)
}

pub fn preview(bytes: &[u8]) -> Result<Option<Vec<u8>>> {
    Ok(common::largest_jpeg(&Tiff::parse(bytes)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hasselblad's fixed table: seventeen difference lengths, 0..=16.
    const COUNTS: [u8; 16] = [0, 2, 1, 4, 2, 3, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0];
    const SYMBOLS: [u8; 17] = [6, 7, 5, 4, 8, 9, 10, 3, 11, 1, 2, 12, 0, 13, 14, 15, 16];

    /// Writes bits MSB-first into 32-bit little-endian words, the
    /// mirror of what [`BitPumpMsb32`] reads.
    #[derive(Default)]
    struct Writer {
        words: Vec<u32>,
        acc: u64,
        bits: u32,
    }

    impl Writer {
        fn put(&mut self, value: u32, len: u32) {
            self.acc = (self.acc << len) | (value & ((1u64 << len) - 1) as u32) as u64;
            self.bits += len;
            while self.bits >= 32 {
                self.words.push((self.acc >> (self.bits - 32)) as u32);
                self.bits -= 32;
            }
        }
        fn finish(mut self) -> Vec<u8> {
            if self.bits > 0 {
                self.put(0, 32 - self.bits);
            }
            self.words.iter().flat_map(|w| w.to_le_bytes()).collect()
        }
    }

    /// The canonical code for a symbol in the table above.
    fn code(symbol: u8) -> (u32, u32) {
        let (mut value, mut index) = (0u32, 0usize);
        for len in 1..=16u32 {
            for _ in 0..COUNTS[len as usize - 1] {
                if SYMBOLS[index] == symbol {
                    return (value, len);
                }
                value += 1;
                index += 1;
            }
            value <<= 1;
        }
        panic!("symbol {symbol} is not in the table");
    }

    /// The category and value bits for a difference, T.81's rule.
    fn encode_diff(diff: i32) -> (u8, u32, u32) {
        if diff == 0 {
            return (0, 0, 0);
        }
        let magnitude = diff.unsigned_abs();
        let len = 32 - magnitude.leading_zeros();
        let bits = if diff > 0 {
            diff as u32
        } else {
            (diff + (1 << len) - 1) as u32
        };
        (len as u8, bits, len)
    }

    /// A whole Hasselblad stream around a frame of differences.
    fn stream(width: usize, height: usize, diffs: &[(i32, i32)]) -> Vec<u8> {
        let mut out = vec![0xFF, 0xD8, 0xFF, 0xC3, 0, 11, 16];
        out.extend_from_slice(&(height as u16).to_be_bytes());
        out.extend_from_slice(&(width as u16).to_be_bytes());
        out.extend_from_slice(&[1, 0, 0x11, 0]);
        // A fill byte before the DHT, exactly as the cameras write it.
        out.push(0xFF);
        out.extend_from_slice(&[0xFF, 0xC4, 0, 36, 0]);
        out.extend_from_slice(&COUNTS);
        out.extend_from_slice(&SYMBOLS);
        out.extend_from_slice(&[0xFF, 0xDA, 0, 8, 1, 0, 0, 8, 0, 0]);
        let mut writer = Writer::default();
        for (a, b) in diffs {
            let (sym_a, bits_a, len_a) = encode_diff(*a);
            let (sym_b, bits_b, len_b) = encode_diff(*b);
            for symbol in [sym_a, sym_b] {
                let (value, len) = code(symbol);
                writer.put(value, len);
            }
            writer.put(bits_a, len_a);
            writer.put(bits_b, len_b);
        }
        out.extend_from_slice(&writer.finish());
        out
    }

    #[test]
    fn pairs_run_two_prediction_chains() {
        // Two rows of four: the chains must restart at 32768 on the
        // second row rather than carry the first row's values on.
        let diffs = [(10, -20), (3, 4), (-8, 30), (0, 1)];
        let bytes = stream(4, 2, &diffs);
        let out = decompress(&bytes, 4, 2).unwrap();
        assert_eq!(
            out,
            vec![
                32778, 32748, 32781, 32752, // row 0
                32760, 32798, 32760, 32799, // row 1
            ]
        );
    }

    #[test]
    fn sixteen_bit_escape_is_minus_32768() {
        // 65535 in sixteen value bits is Hasselblad's spelling of
        // -32768, which the ordinary rule cannot reach.
        let mut bytes = vec![0xFF, 0xD8, 0xFF, 0xC3, 0, 11, 16, 0, 1, 0, 2, 1, 0, 0x11, 0];
        bytes.extend_from_slice(&[0xFF, 0xC4, 0, 36, 0]);
        bytes.extend_from_slice(&COUNTS);
        bytes.extend_from_slice(&SYMBOLS);
        bytes.extend_from_slice(&[0xFF, 0xDA, 0, 8, 1, 0, 0, 8, 0, 0]);
        let mut writer = Writer::default();
        let (value, len) = code(16);
        writer.put(value, len);
        let (zero, zero_len) = code(0);
        writer.put(zero, zero_len);
        writer.put(0xFFFF, 16);
        bytes.extend_from_slice(&writer.finish());
        let out = decompress(&bytes, 2, 1).unwrap();
        assert_eq!(out, vec![0x8000u16.wrapping_sub(32768), 0x8000]);
    }

    #[test]
    fn a_scan_may_hold_ff_bytes() {
        // 0xFF is ordinary data here: a stuffing-aware reader would
        // stop dead at one. A run of large positive differences fills
        // the value bits with ones and so writes 0xFF bytes.
        let diffs = [(4095, 4095); 8];
        let bytes = stream(4, 2, &diffs);
        let scan = frame(&bytes).unwrap().scan;
        assert!(
            bytes[scan..].contains(&0xFF),
            "test stream has no 0xFF byte"
        );
        let out = decompress(&bytes, 4, 2).unwrap();
        assert_eq!(
            out,
            vec![36863, 36863, 40958, 40958, 36863, 36863, 40958, 40958]
        );
    }

    #[test]
    fn truncated_streams_do_not_panic() {
        let bytes = stream(4, 2, &[(10, -20), (3, 4), (-8, 30), (0, 1)]);
        for cut in 0..bytes.len() {
            // Either an error or a short frame; never a panic.
            let _ = decompress(&bytes[..cut], 4, 2);
        }
    }

    #[test]
    fn a_frame_smaller_than_its_ifd_is_corrupt() {
        let bytes = stream(4, 2, &[(1, 1), (1, 1)]);
        assert!(matches!(decompress(&bytes, 8, 2), Err(Error::Corrupt(_))));
    }

    #[test]
    fn corpus_matches_the_oracle() {
        let files = corpus::files(&["3fr", "fff"]);
        for path in &files {
            let bytes = std::fs::read(path).unwrap();
            let name = corpus::name(path);
            assert_eq!(
                crate::probe(&bytes),
                Some(Format::Hasselblad),
                "{name} did not probe as Hasselblad"
            );
            let raw = decode(&bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
            raw.validate().unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(raw.cfa, Cfa::RGGB, "{name}");
            corpus::check_against_oracle(path, &raw);
            corpus::check_against_identify(path, &raw, &["Image size"]);
            corpus::check_preview(path, &raw);
        }
        eprintln!("hasselblad: {} corpus files checked", files.len());
    }

    #[test]
    fn corpus_truncations_do_not_panic() {
        for path in corpus::files(&["3fr", "fff"]) {
            corpus::check_truncations(&path, decode);
        }
    }

    #[test]
    fn garbage_is_not_a_hasselblad() {
        assert!(decode(&[0u8; 64]).is_err());
        assert!(decode(b"II*\0\x08\0\0\0").is_err());
    }
}

/// Shared support for the corpus tests of the medium-format and
/// Foveon modules (this one, `iiq`, `mos` and `x3f`).
///
/// Every check is driven by the sidecars the fetch script leaves
/// beside a sample: `<file>.tiff` is LibRaw's `unprocessed_raw -T`
/// frame, `<file>.identify.txt` is `raw-identify -v -w`. A sample
/// with no sidecar is still decoded and validated; only the
/// comparisons are skipped.
#[cfg(test)]
pub(crate) mod corpus {
    use crate::{Orientation, RawData, RawImage};
    use std::path::{Path, PathBuf};

    /// Every file under `SCHIST_RAW_CORPUS` with one of `extensions`
    /// (compared case-insensitively). Empty when the variable is
    /// unset, which is how these tests skip themselves.
    pub fn files(extensions: &[&str]) -> Vec<PathBuf> {
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
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| extensions.iter().any(|w| w.eq_ignore_ascii_case(e)))
                {
                    out.push(path);
                }
            }
        }
        out.sort();
        out
    }

    pub fn name(path: &Path) -> String {
        path.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    }

    /// `<file><suffix>`, appended to the whole name so the sample's
    /// own extension stays in it.
    fn sidecar(path: &Path, suffix: &str) -> PathBuf {
        let mut name = path.as_os_str().to_os_string();
        name.push(suffix);
        PathBuf::from(name)
    }

    /// LibRaw's unpacked frame: 16-bit grey, the full sensor,
    /// black not subtracted.
    ///
    /// Read with this crate's own TIFF parser rather than the `image`
    /// crate, whose default allocation limit refuses the 200 MB
    /// frames a 100 MP back unpacks to.
    pub fn oracle(path: &Path) -> Option<(usize, usize, Vec<u16>)> {
        let file = sidecar(path, ".tiff");
        let bytes = std::fs::read(&file).ok()?;
        let read = || -> crate::Result<(usize, usize, Vec<u16>)> {
            let tiff = crate::tiff::Tiff::parse(&bytes)?;
            let layout = crate::tiff::ImageLayout::of(&tiff, tiff.root())?;
            if layout.bits_per_sample != 16 || layout.samples_per_pixel != 1 {
                return Err(crate::Error::Unsupported(format!(
                    "oracle frame is {} bits x {} samples",
                    layout.bits_per_sample, layout.samples_per_pixel
                )));
            }
            let little_endian = tiff.little_endian();
            let mut out = Vec::with_capacity(layout.width * layout.height);
            for (start, len) in &layout.chunks {
                out.extend(
                    bytes[*start..*start + *len]
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|b| {
                            if little_endian {
                                u16::from_le_bytes([b[0], b[1]])
                            } else {
                                u16::from_be_bytes([b[0], b[1]])
                            }
                        }),
                );
            }
            out.truncate(layout.width * layout.height);
            Ok((layout.width, layout.height, out))
        };
        // A sidecar that exists but will not read is a broken oracle,
        // not a missing one, and must not pass silently.
        Some(read().unwrap_or_else(|e| panic!("{}: {e}", file.display())))
    }

    pub fn identify(path: &Path) -> Option<String> {
        std::fs::read_to_string(sidecar(path, ".identify.txt")).ok()
    }

    /// Samples must equal the oracle exactly, at the oracle's size.
    pub fn check_against_oracle(path: &Path, raw: &RawImage) {
        let file = name(path);
        let Some((width, height, want)) = oracle(path) else {
            eprintln!("{file}: no oracle frame, decode only");
            return;
        };
        assert_eq!(
            (raw.width, raw.height),
            (width, height),
            "{file}: decoded {}x{} but LibRaw unpacked {width}x{height}",
            raw.width,
            raw.height
        );
        let RawData::U16(got) = &raw.data else {
            panic!("{file}: float samples from an integer sensor");
        };
        let mut wrong = 0usize;
        let mut first = Vec::new();
        for (i, (a, b)) in got.iter().zip(want.iter()).enumerate() {
            if a != b {
                wrong += 1;
                if first.len() < 8 {
                    first.push(format!("({},{}) got {a} want {b}", i % width, i / width));
                }
            }
        }
        assert_eq!(
            wrong,
            0,
            "{file}: {wrong} of {} samples differ from the oracle: {}",
            got.len(),
            first.join(", ")
        );
    }

    /// Compare what `raw-identify -v -w` printed with what the
    /// decoder produced. `allow` names the checks a caller knows
    /// LibRaw and this decoder legitimately disagree on.
    pub fn check_against_identify(path: &Path, raw: &RawImage, allow: &[&str]) {
        let file = name(path);
        let Some(text) = identify(path) else { return };
        let field = |key: &str| -> Option<String> {
            text.lines()
                .find(|l| l.trim_start().starts_with(key))
                .and_then(|l| l.split_once(':'))
                .map(|(_, v)| v.trim().to_string())
        };
        let size = |key: &str| -> Option<(usize, usize)> {
            let value = field(key)?;
            let (w, h) = value.split_once('x')?;
            Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
        };
        if let Some((width, height)) = size("Full size") {
            assert_eq!(
                (raw.width, raw.height),
                (width, height),
                "{file}: full size"
            );
        }
        if !allow.contains(&"Image size") {
            if let Some((width, height)) = size("Image size") {
                assert_eq!(
                    (raw.crop.width, raw.crop.height),
                    (width, height),
                    "{file}: crop size"
                );
            }
        }
        if !allow.contains(&"Image flip") {
            if let Some(flip) = field("Image flip").and_then(|v| v.parse::<u32>().ok()) {
                // LibRaw's flip is the EXIF orientation in its own
                // spelling: 0 none, 3 half turn, 5 and 6 the quarters.
                let want = match flip {
                    3 => Orientation::Rotate180,
                    5 => Orientation::Rotate270CW,
                    6 => Orientation::Rotate90CW,
                    _ => Orientation::Normal,
                };
                assert_eq!(raw.orientation, want, "{file}: orientation");
            }
        }
        if !allow.contains(&"As shot") {
            if let Some(line) = text.lines().find(|l| l.trim_start().starts_with("As shot")) {
                let numbers: Vec<f32> = line
                    .split_whitespace()
                    .skip(2)
                    .filter_map(|t| t.parse().ok())
                    .collect();
                if numbers.len() >= 3 && numbers[1] > 0.0 {
                    for (i, want) in numbers[..3].iter().enumerate() {
                        let want = want / numbers[1];
                        let got = raw.wb_coeffs[i];
                        assert!(
                            (got - want).abs() <= 1e-3 * want.max(1.0),
                            "{file}: white balance {i}: got {got} want {want}"
                        );
                    }
                }
            }
        }
    }

    /// LibRaw prints the filter array as sixteen letters, a 4x4
    /// expansion of the pattern at the frame's origin.
    pub fn check_cfa(path: &Path, raw: &RawImage) {
        use crate::{Cfa, CfaColor};
        let file = name(path);
        let Some(text) = identify(path) else { return };
        let Some(want) = text
            .lines()
            .find(|l| l.trim_start().starts_with("Filter pattern"))
            .and_then(|l| l.split_once(':'))
            .map(|(_, v)| v.trim().to_string())
        else {
            return;
        };
        if want.len() < 4 {
            return;
        }
        let letter = |c: CfaColor| match c {
            CfaColor::Red => 'R',
            CfaColor::Green | CfaColor::Green2 => 'G',
            CfaColor::Blue => 'B',
            CfaColor::Cyan => 'C',
            CfaColor::Magenta => 'M',
            CfaColor::Yellow => 'Y',
            CfaColor::Emerald => 'E',
        };
        let Cfa::Bayer(_) = &raw.cfa else { return };
        let got: String = [(0, 0), (1, 0), (0, 1), (1, 1)]
            .iter()
            .map(|(x, y)| raw.cfa.color_at(*x, *y).map(letter).unwrap_or('?'))
            .collect();
        // LibRaw prints the 2x2 four times over, in the same
        // top-left, top-right, bottom-left, bottom-right order
        // [`Cfa::Bayer`] uses, so the first four letters are it.
        assert_eq!(got, want[..4], "{file}: filter pattern");
    }

    /// The preview must be a JPEG a viewer can actually show.
    pub fn check_preview(path: &Path, raw: &RawImage) {
        let file = name(path);
        match &raw.preview {
            Some(jpeg) => {
                let decoded = image::load_from_memory(jpeg)
                    .unwrap_or_else(|e| panic!("{file}: preview will not decode: {e}"));
                assert!(decoded.width() > 0 && decoded.height() > 0, "{file}");
            }
            None => eprintln!("{file}: no embedded preview"),
        }
    }

    /// A truncated file must be an error, never a panic. The cuts are
    /// spread over the file with a fixed sequence so a failure is
    /// reproducible.
    pub fn check_truncations(path: &Path, decode: fn(&[u8]) -> crate::Result<RawImage>) {
        let Ok(bytes) = std::fs::read(path) else {
            return;
        };
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        for _ in 0..10 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let cut = (state % bytes.len() as u64) as usize;
            let _ = decode(&bytes[..cut]);
        }
        // The empty file and a bare header, too.
        let _ = decode(&[]);
        let _ = decode(&bytes[..bytes.len().min(64)]);
    }
}
