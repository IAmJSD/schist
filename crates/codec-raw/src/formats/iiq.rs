//! Phase One IIQ, the raw of the P, IQ and iXU backs.
//!
//! The container is a little-endian TIFF whose IFD0 is only the RGB
//! thumbnail; everything that matters hangs off a private structure
//! that starts at byte 8 with the signature `IIIICwaR`. Byte 16 holds
//! the offset of a tag directory, and **every offset inside that
//! structure — the directory's own, and each entry's — is relative to
//! byte 8**, not to the file. (Files with the extension `.IIQ` and
//! files a capture session writes as `.TIF` are the same format; the
//! extension says nothing.)
//!
//! The directory is a count, a reserved word, then 16-byte entries of
//! four little-endian `u32`s: tag, type, length in bytes, and either
//! the value itself when it fits in four bytes or the offset of the
//! value when it does not. Tag 0x010F is the sensor data: its length
//! field is the byte count and its value field the offset.
//!
//! Four compressions live under tag 0x010E:
//!
//!  * **0** — a plain 16-bit raster.
//!  * **3** — "IIQ L", lossless: 14-bit samples, one bit stream a
//!    row, addressed through the row-offset table in tag 0x021C.
//!    Every group of eight columns opens with two length selectors,
//!    one for the even columns and one for the odd, and each pixel is
//!    then a difference against the previous pixel of its own parity.
//!    The bits are read most-significant-first out of 32-bit
//!    little-endian words — the same reader Hasselblad's lossless
//!    JPEG needs, which is no coincidence: the two formats share an
//!    ancestor.
//!  * **5** and **6** — "IIQ S", lossy. Neither is a
//!    predictor-plus-residual code (the samples land on a coarse,
//!    non-linear grid that no arrangement of the format-3 machinery
//!    reproduces), and this module does not decode them.
//!
//! The 14-bit formats are shifted up by two on the way out, so that
//! every Phase One frame — 16-bit raster or compressed — shares one
//! scale and one white level. That is also what LibRaw does, so the
//! oracle frames compare directly.

use crate::bits::{BitPump, BitPumpMsb32};
use crate::formats::common;
use crate::tiff::Tiff;
use crate::{Cfa, Error, Format, RawData, RawImage, Rect, Result};
use rayon::prelude::*;

/// Everything in the Phase One structure is addressed from byte 8,
/// where its signature sits.
const BASE: usize = 8;
const SIGNATURE: &[u8; 8] = b"IIIICwaR";

/// Tags of the Phase One directory that this decoder reads.
mod p1 {
    /// Three floats: the as-shot R, G and B multipliers, green 1.0.
    /// (Tag 0x0106 beside it holds nine floats, but they are Phase
    /// One's own camera matrix, not DNG's XYZ-to-camera one, so the
    /// colour matrix is left to the camera table.)
    pub const WHITE_BALANCE: u32 = 0x0107;
    pub const RAW_WIDTH: u32 = 0x0108;
    pub const RAW_HEIGHT: u32 = 0x0109;
    pub const CROP_LEFT: u32 = 0x010A;
    pub const CROP_TOP: u32 = 0x010B;
    pub const CROP_WIDTH: u32 = 0x010C;
    pub const CROP_HEIGHT: u32 = 0x010D;
    pub const FORMAT: u32 = 0x010E;
    /// The sensor data: length in the length field, offset in the
    /// value field.
    pub const RAW_DATA: u32 = 0x010F;
    /// One `u32` a row: where that row's bits start, counted from the
    /// beginning of the sensor data.
    pub const ROW_OFFSETS: u32 = 0x021C;
    /// The back's name and firmware, ASCII.
    pub const MODEL_FIRMWARE: u32 = 0x0301;
}

/// The difference lengths a selector can choose between.
///
/// The selector is unary: up to five zero bits, then (unless five
/// were read) a one, and finally a single bit that picks between the
/// pair the run length reached. A run of zero — a leading one bit —
/// means "keep the length in force", which is what a flat row spends
/// most of its selectors on and why the format costs so little more
/// than its samples. Entry 14 is not a length but the escape: the
/// sample is sixteen raw bits, absolute rather than a difference.
const LENGTHS: [u32; 10] = [8, 7, 6, 9, 11, 10, 5, 12, 14, 13];
const ESCAPE: u32 = 14;

/// One entry of the Phase One directory.
#[derive(Debug, Clone, Copy)]
struct Entry {
    tag: u32,
    /// Bytes of value, whether inline or out of line.
    length: u32,
    /// The value itself when `length <= 4`, else its offset from
    /// [`BASE`].
    value: u32,
}

/// The Phase One tag directory, and the file it points into.
struct Directory<'a> {
    bytes: &'a [u8],
    entries: Vec<Entry>,
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

impl<'a> Directory<'a> {
    fn parse(bytes: &'a [u8]) -> Result<Directory<'a>> {
        if bytes.get(BASE..BASE + 8) != Some(SIGNATURE) {
            return Err(Error::Corrupt(
                "no IIIICwaR signature at byte 8: not a Phase One raw".into(),
            ));
        }
        let start = u32_at(bytes, 16)
            .and_then(|o| (o as usize).checked_add(BASE))
            .ok_or_else(|| Error::Corrupt("truncated Phase One header".into()))?;
        let count = u32_at(bytes, start)
            .ok_or_else(|| Error::Corrupt("Phase One directory outside the file".into()))?;
        // A directory is sixteen bytes an entry; anything claiming
        // more entries than the file could hold is corrupt, and
        // capping keeps the allocation honest.
        let count = count.min((bytes.len() / 16) as u32) as usize;
        let mut entries = Vec::with_capacity(count);
        for i in 0..count {
            let at = start + 8 + i * 16;
            let (Some(tag), Some(length), Some(value)) = (
                u32_at(bytes, at),
                u32_at(bytes, at + 8),
                u32_at(bytes, at + 12),
            ) else {
                break;
            };
            entries.push(Entry { tag, length, value });
        }
        if entries.is_empty() {
            return Err(Error::Corrupt("empty Phase One directory".into()));
        }
        Ok(Directory { bytes, entries })
    }

    fn entry(&self, tag: u32) -> Option<Entry> {
        self.entries.iter().find(|e| e.tag == tag).copied()
    }

    /// A tag whose value fits in the entry.
    fn int(&self, tag: u32) -> Option<u32> {
        self.entry(tag).filter(|e| e.length <= 4).map(|e| e.value)
    }

    /// The bytes a tag points at, `None` when they are inline or fall
    /// outside the file.
    fn blob(&self, tag: u32) -> Option<&'a [u8]> {
        let entry = self.entry(tag).filter(|e| e.length > 4)?;
        let start = (entry.value as usize).checked_add(BASE)?;
        let end = start.checked_add(entry.length as usize)?;
        self.bytes.get(start..end)
    }

    fn floats(&self, tag: u32, count: usize) -> Option<Vec<f32>> {
        let blob = self.blob(tag)?;
        (blob.len() >= count * 4).then(|| {
            blob[..count * 4]
                .as_chunks::<4>()
                .0
                .iter()
                .map(|b| f32::from_le_bytes(*b))
                .collect()
        })
    }

    fn text(&self, tag: u32) -> Option<String> {
        let blob = self.blob(tag)?;
        let end = blob.iter().position(|b| *b == 0).unwrap_or(blob.len());
        let text = String::from_utf8_lossy(&blob[..end]).trim().to_string();
        (!text.is_empty()).then_some(text)
    }
}

/// One row of an IIQ L stream.
///
/// `pred` holds the previous sample of each column parity and `len`
/// the difference length in force for each; both are per row, because
/// every row starts at its own offset in the table and its own bit
/// boundary.
fn decode_row(bytes: &[u8], out: &mut [u16]) {
    let width = out.len();
    let mut pump = BitPumpMsb32::new(bytes);
    let mut pred = [0i32; 2];
    // The escape until a selector says otherwise: a row whose first
    // group declines to choose a length then reads whole samples
    // rather than an arbitrary number of bits.
    let mut len = [ESCAPE; 2];
    // The columns past the last whole group of eight are always
    // escapes; the encoder has no group header left to describe them.
    let tail = width & !7;
    for (col, sample) in out.iter_mut().enumerate() {
        if col >= tail {
            len = [ESCAPE; 2];
        } else if col % 8 == 0 {
            for side in &mut len {
                let mut zeros = 0;
                while zeros < 5 && pump.get(1) == 0 {
                    zeros += 1;
                }
                if zeros > 0 {
                    let index = (zeros - 1) * 2 + pump.get(1) as usize;
                    *side = LENGTHS[index.min(LENGTHS.len() - 1)];
                }
            }
        }
        let parity = col & 1;
        let bits = len[parity];
        if bits == ESCAPE {
            pred[parity] = pump.get(16) as i32;
        } else {
            // The value bits are the difference biased into the
            // unsigned range: the low half of the range is negative.
            let value = pump.get(bits) as i32;
            pred[parity] += value + 1 - (1 << (bits - 1));
        }
        // 14-bit samples, shifted to share the 16-bit raster's scale.
        *sample = (pred[parity].clamp(0, 0x3FFF) as u16) << 2;
    }
}

/// IIQ L: one independent bit stream a row, found through the
/// row-offset table.
fn decompress(data: &[u8], offsets: &[u8], width: usize, height: usize) -> Result<Vec<u16>> {
    if offsets.len() < height * 4 {
        return Err(Error::Corrupt(format!(
            "Phase One row table holds {} bytes for {height} rows",
            offsets.len()
        )));
    }
    // A compressed row cannot be shorter than a bit a sample, so the
    // data length bounds the frame a forged header may claim.
    let samples = crate::frame_samples(width, height, 1)?;
    if data.len().saturating_mul(8) < samples {
        return Err(Error::Corrupt(format!(
            "Phase One frame of {samples} samples in {} bytes",
            data.len()
        )));
    }
    let mut out = vec![0u16; samples];
    out.par_chunks_exact_mut(width)
        .enumerate()
        .for_each(|(row, line)| {
            let start = u32::from_le_bytes([
                offsets[row * 4],
                offsets[row * 4 + 1],
                offsets[row * 4 + 2],
                offsets[row * 4 + 3],
            ]) as usize;
            // A row pointing outside the data is left black rather
            // than failing the whole frame: the rest of the file is
            // still worth having, and the bit reader would only see
            // zeros anyway.
            if let Some(bits) = data.get(start..) {
                decode_row(bits, line);
            }
        });
    Ok(out)
}

/// The uncompressed format: little-endian 16-bit, no padding.
fn unpack(data: &[u8], width: usize, height: usize) -> Result<Vec<u16>> {
    let samples = width * height;
    if data.len() < samples * 2 {
        return Err(Error::Corrupt(format!(
            "Phase One raster holds {} bytes for {samples} samples",
            data.len()
        )));
    }
    Ok(data[..samples * 2]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| u16::from_le_bytes(*b))
        .collect())
}

pub fn decode(bytes: &[u8]) -> Result<RawImage> {
    let dir = Directory::parse(bytes)?;
    let width = dir.int(p1::RAW_WIDTH).unwrap_or(0) as usize;
    let height = dir.int(p1::RAW_HEIGHT).unwrap_or(0) as usize;
    if width == 0 || height == 0 || width > 1 << 16 || height > 1 << 16 {
        return Err(Error::Corrupt(format!(
            "Phase One frame of {width}x{height}"
        )));
    }
    let raw = dir
        .entry(p1::RAW_DATA)
        .ok_or_else(|| Error::Corrupt("Phase One file with no sensor data".into()))?;
    let start = (raw.value as usize)
        .checked_add(BASE)
        .ok_or_else(|| Error::Corrupt("Phase One data offset out of range".into()))?;
    let data = bytes
        .get(start..)
        .ok_or_else(|| Error::Corrupt("Phase One data starts past the end of the file".into()))?;
    let data = &data[..(raw.length as usize).min(data.len())];

    let format = dir.int(p1::FORMAT).unwrap_or(0);
    let (samples, white) = match format {
        0 => (unpack(data, width, height)?, 65535.0),
        3 => {
            let offsets = dir
                .blob(p1::ROW_OFFSETS)
                .ok_or_else(|| Error::Corrupt("IIQ L with no row-offset table".into()))?;
            // 14 bits shifted up by two: the largest sample a row can
            // carry is 0x3FFF, so saturation lands here.
            (decompress(data, offsets, width, height)?, 65532.0)
        }
        5 | 6 => {
            return Err(Error::Unsupported(
                "Phase One IIQ S (compression 5 and 6): a lossy code this decoder cannot read"
                    .into(),
            ))
        }
        other => return Err(Error::Unsupported(format!("Phase One compression {other}"))),
    };

    // The crop's top-left is where the sensor's active area begins,
    // and that area always reads RGGB; the masked border in front of
    // it moves the pattern's phase for the full frame.
    let left = dir.int(p1::CROP_LEFT).unwrap_or(0) as usize;
    let top = dir.int(p1::CROP_TOP).unwrap_or(0) as usize;
    let cfa = Cfa::RGGB.shifted(left % 2, top % 2);
    let mut image = RawImage::new(Format::Iiq, width, height, 1, RawData::U16(samples), cfa);
    image.white_level = white;

    let crop_width = dir.int(p1::CROP_WIDTH).unwrap_or(0) as usize;
    let crop_height = dir.int(p1::CROP_HEIGHT).unwrap_or(0) as usize;
    if crop_width > 0
        && crop_height > 0
        && left + crop_width <= width
        && top + crop_height <= height
    {
        image.crop = Rect {
            x: left,
            y: top,
            width: crop_width,
            height: crop_height,
        };
    }
    if let Some(wb) = dir.floats(p1::WHITE_BALANCE, 3) {
        if wb.iter().all(|v| v.is_finite() && *v > 0.0) {
            image.wb_coeffs = [wb[0] / wb[1], 1.0, wb[2] / wb[1], 1.0];
        }
    }

    // The TIFF around the private structure carries the maker, the
    // model, the Exif IFD and the thumbnail. It is an ordinary TIFF,
    // so a failure to parse it costs only the metadata.
    if let Ok(tiff) = Tiff::parse(bytes) {
        let (make, model) = tiff.make_model();
        image.set_camera(&make, &model);
        image.metadata = common::metadata(&tiff);
        image.orientation = common::orientation(&tiff);
        // Phase One thumbnails are uncompressed RGB, so this is
        // almost always None; it costs nothing to look.
        image.preview = common::largest_jpeg(&tiff);
    }
    if image.model.is_empty() {
        // The back names itself in the private structure too, ahead
        // of its firmware versions: "IQ140, User Firmware: 8.00.30".
        if let Some(text) = dir.text(p1::MODEL_FIRMWARE) {
            let model = text.split(',').next().unwrap_or(&text).trim().to_string();
            image.set_camera("Phase One", &model);
        }
    }
    image.apply_camera_table();
    Ok(image)
}

pub fn preview(bytes: &[u8]) -> Result<Option<Vec<u8>>> {
    Ok(common::largest_jpeg(&Tiff::parse(bytes)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formats::hasselblad::corpus;

    /// Files whose compression this module knowingly declines, with
    /// the reason. Everything else in the corpus must decode.
    const UNSUPPORTED: &[(&str, &str)] = &[
        ("H_25-H25_Outdoor_.IIQ", "IIQ S, compression 5"),
        ("P25+-CF028662.IIQ", "IIQ S, compression 5"),
        ("P45-230215_3810.TIF", "IIQ S, compression 5"),
        (
            "IQ140-Phase_One_IQ140_sample_files_04_Diciembre_2017_005_IIQ_S.TIF",
            "IIQ S, compression 6",
        ),
        (
            "IQ140-Phase_One_IQ140_sample_files_04_Diciembre_2017_011_Sensor+_IIQ_S.TIF",
            "IIQ S, compression 6",
        ),
        ("iXU180-cap_22908.IIQ", "IIQ S, compression 6"),
    ];

    /// A Phase One structure around one directory, for the tests that
    /// do not need a whole file.
    fn build(entries: &[(u32, u32, u32)], trailer: &[u8]) -> Vec<u8> {
        let mut out = b"II*\0".to_vec();
        out.extend_from_slice(&0u32.to_le_bytes()); // no IFD0
        out.extend_from_slice(SIGNATURE);
        let directory = 4096u32;
        out.extend_from_slice(&directory.to_le_bytes());
        out.resize(BASE + directory as usize, 0);
        out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        for (tag, length, value) in entries {
            out.extend_from_slice(&tag.to_le_bytes());
            out.extend_from_slice(&4u32.to_le_bytes());
            out.extend_from_slice(&length.to_le_bytes());
            out.extend_from_slice(&value.to_le_bytes());
        }
        out.extend_from_slice(trailer);
        out
    }

    #[test]
    fn offsets_are_relative_to_the_signature() {
        // The blob's value field is 0, which is byte 8 of the file:
        // the signature itself. Reading it from byte 0 would give
        // "II*\0".
        let bytes = build(&[(p1::MODEL_FIRMWARE, 8, 0)], &[]);
        assert_eq!(
            Directory::parse(&bytes)
                .unwrap()
                .text(p1::MODEL_FIRMWARE)
                .as_deref(),
            Some("IIIICwaR")
        );
    }

    #[test]
    fn short_values_live_in_the_entry() {
        let bytes = build(&[(p1::RAW_WIDTH, 4, 4134), (p1::FORMAT, 4, 3)], &[]);
        let dir = Directory::parse(&bytes).unwrap();
        assert_eq!(dir.int(p1::RAW_WIDTH), Some(4134));
        assert_eq!(dir.int(p1::FORMAT), Some(3));
        assert_eq!(dir.blob(p1::RAW_WIDTH), None);
    }

    /// Writes bits MSB-first into 32-bit little-endian words.
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
        /// The unary selector for a length, or a bare 1 for "keep".
        fn selector(&mut self, length: Option<u32>) {
            match length {
                None => self.put(1, 1),
                Some(length) => {
                    let index = LENGTHS.iter().position(|l| *l == length).expect("a length");
                    let zeros = index as u32 / 2 + 1;
                    // Five zeros need no terminating one: the run is
                    // already as long as the code allows.
                    if zeros < 5 {
                        self.put(1, zeros + 1);
                    } else {
                        self.put(0, zeros);
                    }
                    self.put(index as u32 % 2, 1);
                }
            }
        }
        fn finish(mut self) -> Vec<u8> {
            if self.bits > 0 {
                self.put(0, 32 - self.bits);
            }
            self.words.iter().flat_map(|w| w.to_le_bytes()).collect()
        }
    }

    #[test]
    fn a_group_of_eight_carries_two_selectors() {
        let mut writer = Writer::default();
        // Even columns escape to whole samples, odd columns use
        // six-bit differences from zero.
        writer.selector(Some(ESCAPE));
        writer.selector(Some(6));
        for i in 0..4 {
            writer.put(1000 + i, 16);
            // 6-bit differences of +1: value 1 - 1 + 32 = 32.
            writer.put(32, 6);
        }
        let bytes = writer.finish();
        let mut out = vec![0u16; 8];
        decode_row(&bytes, &mut out);
        assert_eq!(
            out,
            vec![
                1000 << 2,
                1 << 2,
                1001 << 2,
                2 << 2,
                1002 << 2,
                3 << 2,
                1003 << 2,
                4 << 2
            ]
        );
    }

    #[test]
    fn a_leading_one_keeps_the_length_in_force() {
        let mut writer = Writer::default();
        writer.selector(Some(5));
        writer.selector(Some(5));
        for _ in 0..8 {
            // 5 bits: value 16 is the difference 16 + 1 - 16 = +1.
            writer.put(16, 5);
        }
        writer.selector(None);
        writer.selector(None);
        for _ in 0..8 {
            writer.put(16, 5);
        }
        let bytes = writer.finish();
        let mut out = vec![0u16; 16];
        decode_row(&bytes, &mut out);
        // Each parity climbs by one a step, right through the second
        // group's "keep" selectors.
        let want: Vec<u16> = (0..16).map(|i| ((i / 2 + 1) << 2) as u16).collect();
        assert_eq!(out, want);
    }

    #[test]
    fn the_tail_past_the_last_whole_group_is_absolute() {
        let mut writer = Writer::default();
        writer.selector(Some(5));
        writer.selector(Some(5));
        for _ in 0..8 {
            writer.put(16, 5);
        }
        // Ten columns: the last two have no group header and are
        // whole samples.
        writer.put(4000, 16);
        writer.put(4001, 16);
        let bytes = writer.finish();
        let mut out = vec![0u16; 10];
        decode_row(&bytes, &mut out);
        assert_eq!(&out[8..], &[4000 << 2, 4001 << 2]);
    }

    #[test]
    fn a_short_row_table_is_corrupt() {
        assert!(matches!(
            decompress(&[0; 64], &[0; 4], 8, 4),
            Err(Error::Corrupt(_))
        ));
    }

    #[test]
    fn rows_pointing_outside_the_data_stay_black() {
        // Two rows, the second addressed past the end.
        let mut offsets = Vec::new();
        offsets.extend_from_slice(&0u32.to_le_bytes());
        offsets.extend_from_slice(&u32::MAX.to_le_bytes());
        let out = decompress(&[0; 64], &offsets, 8, 2).unwrap();
        assert_eq!(&out[8..], &[0; 8]);
    }

    #[test]
    fn garbage_is_not_a_phase_one() {
        assert!(decode(&[0u8; 64]).is_err());
        assert!(decode(b"II*\0\x08\0\0\0IIIICwaR").is_err());
        for cut in 0..200 {
            let bytes = build(&[(p1::RAW_WIDTH, 4, 8), (p1::RAW_HEIGHT, 4, 8)], &[]);
            let _ = decode(&bytes[..cut.min(bytes.len())]);
        }
    }

    #[test]
    fn corpus_matches_the_oracle() {
        let files = corpus::files(&["iiq", "tif"]);
        let mut checked = 0;
        for path in &files {
            let bytes = std::fs::read(path).unwrap();
            let name = corpus::name(path);
            // The corpus holds converters' TIFFs too; only the Phase
            // One ones are this module's.
            if crate::probe(&bytes) != Some(Format::Iiq) {
                continue;
            }
            checked += 1;
            if let Some((_, reason)) = UNSUPPORTED.iter().find(|(f, _)| *f == name) {
                match decode(&bytes) {
                    Err(Error::Unsupported(_)) => eprintln!("{name}: unsupported ({reason})"),
                    other => panic!("{name}: expected Unsupported for {reason}, got {other:?}"),
                }
                continue;
            }
            let raw = decode(&bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
            raw.validate().unwrap_or_else(|e| panic!("{name}: {e}"));
            corpus::check_against_oracle(path, &raw);
            // LibRaw shaves a pixel off some Phase One crops and
            // rotates portrait backs itself, neither of which the
            // file says to do; the CFA check below covers what the
            // crop is really for.
            corpus::check_against_identify(path, &raw, &["Image size", "Image flip"]);
            corpus::check_cfa(path, &raw);
            corpus::check_preview(path, &raw);
        }
        eprintln!("iiq: {checked} corpus files checked");
        assert!(files.is_empty() || checked > 0);
    }

    #[test]
    fn corpus_truncations_do_not_panic() {
        for path in corpus::files(&["iiq", "tif"]) {
            corpus::check_truncations(&path, decode);
        }
    }
}
