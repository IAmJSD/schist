//! RW2 — Panasonic, and the Leica bodies Panasonic builds (RWL, and
//! the `.RAW` of the Digilux 2).
//!
//! An RW2 is a little-endian TIFF behind the signature `IIU\0`, but
//! almost nothing in it is a TIFF tag: IFD0's low numbers are
//! Panasonic's own, and they describe the sensor rather than an image.
//! There is no ImageWidth, no BitsPerSample in the TIFF sense and no
//! usable StripOffsets — 0x0111 is `0xFFFFFFFF` on every compressed
//! body, and the sensor data is at 0x0118 instead, running to the end
//! of the file.
//!
//! Four codecs have shipped, told apart by RawFormat (0x002D):
//!
//! * no tag at all, Compression 34826 — the oldest bodies (Digilux 2,
//!   LC1) store whole 16-bit words with the sample at the top of each;
//! * 4 — the one nearly every Panasonic ever made uses: fourteen
//!   12-bit pixels to sixteen bytes, read through a stream that is
//!   shuffled in 16 KB blocks. See [`decode_v4`];
//! * 6 and 7 — the 14-bit scheme on the full-frame S bodies;
//! * 8 — the newest (GH6, G9 II), 16-bit.
//!
//! Only the first two are implemented; the others return
//! [`Error::Unsupported`] naming their version.
//!
//! The other oddity is where the metadata lives. Shutter speed,
//! aperture, focal length and the lens name are *not* in the RW2's own
//! directories: they are in the EXIF of the full-size JPEG that tag
//! 0x002E carries inline, so that JPEG is parsed as well.

use crate::formats::common;
use crate::tiff::{tags, Ifd, Tiff};
use crate::{Cfa, CfaColor, Error, Format, RawData, RawImage, Rect, Result};
use rayon::prelude::*;

/// Panasonic's own IFD0 tags, by the names ExifTool's tag
/// documentation gives them. They occupy the numbers below 0x0100
/// that TIFF leaves free.
mod tag {
    /// The full sensor frame, padding included.
    pub const SENSOR_WIDTH: u16 = 0x0002;
    pub const SENSOR_HEIGHT: u16 = 0x0003;
    /// The frame the camera means to show, as four edges.
    pub const SENSOR_TOP: u16 = 0x0004;
    pub const SENSOR_LEFT: u16 = 0x0005;
    pub const SENSOR_BOTTOM: u16 = 0x0006;
    pub const SENSOR_RIGHT: u16 = 0x0007;
    /// 1 RGGB, 2 GRBG, 3 GBRG, 4 BGGR.
    pub const CFA_PATTERN: u16 = 0x0009;
    pub const BITS_PER_SAMPLE: u16 = 0x000A;
    /// 34316 for the Panasonic codecs, 34826 for the oldest bodies.
    pub const COMPRESSION: u16 = 0x000B;
    /// The level above which the sensor stops being linear, per
    /// channel; the first is the one a developer wants.
    pub const LINEARITY_LIMIT: u16 = 0x000E;
    /// Red and blue balance x256, on the bodies too old for 0x0024.
    pub const RED_BALANCE: u16 = 0x0011;
    pub const BLUE_BALANCE: u16 = 0x0012;
    pub const BLACK_LEVEL_RED: u16 = 0x001C;
    pub const BLACK_LEVEL_GREEN: u16 = 0x001D;
    pub const BLACK_LEVEL_BLUE: u16 = 0x001E;
    pub const WB_RED_LEVEL: u16 = 0x0024;
    pub const WB_GREEN_LEVEL: u16 = 0x0025;
    pub const WB_BLUE_LEVEL: u16 = 0x0026;
    /// The aspect-ratio crop, on the bodies that offer one: a
    /// rectangle inside the sensor borders, four edges again but
    /// numbered out of order.
    pub const CROP_TOP: u16 = 0x002F;
    pub const CROP_LEFT: u16 = 0x0030;
    pub const CROP_BOTTOM: u16 = 0x0031;
    pub const CROP_RIGHT: u16 = 0x0032;
    /// Which codec the sensor data uses.
    pub const RAW_FORMAT: u16 = 0x002D;
    /// The full-size JPEG, inline.
    pub const JPEG_FROM_RAW: u16 = 0x002E;
    /// Where the sensor data starts, on every body that has a codec.
    pub const RAW_DATA_OFFSET: u16 = 0x0118;
}

/// The codec version, from RawFormat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Codec {
    /// No RawFormat and Compression 34826: whole words.
    Uncompressed,
    /// RawFormat 4: fourteen pixels to sixteen bytes.
    Blocks14,
    /// RawFormat 7: nine 14-bit samples to sixteen bytes, packed
    /// plainly with two bits left over.
    PackedBlocks,
    /// RawFormat 6: eleven 14-bit pixels to sixteen bytes, two of them
    /// whole and the other nine predicted.
    Groups11,
    /// A version this module knows of but cannot decode.
    Unsupported(u32),
}

pub fn decode(bytes: &[u8]) -> Result<RawImage> {
    let tiff = Tiff::parse(bytes)?;
    let ifd = tiff.root();
    let int = |tag: u16| ifd.get(tag).and_then(|e| e.u32(0));

    let width = int(tag::SENSOR_WIDTH).unwrap_or(0) as usize;
    let height = int(tag::SENSOR_HEIGHT).unwrap_or(0) as usize;
    if width == 0 || height == 0 || width > 1 << 16 || height > 1 << 16 {
        return Err(Error::Corrupt(format!("RW2 sensor {width}x{height}")));
    }
    let samples = crate::frame_samples(width, height, 1)?;
    // Twelve bits on everything up to the GH5, fourteen on the S
    // bodies, sixteen on the newest.
    let bits = int(tag::BITS_PER_SAMPLE)
        .filter(|b| (8..=16).contains(b))
        .unwrap_or(12);
    let compression = int(tag::COMPRESSION).unwrap_or(34316);
    let codec = match int(tag::RAW_FORMAT) {
        Some(4) => Codec::Blocks14,
        Some(6) => Codec::Groups11,
        Some(7) => Codec::PackedBlocks,
        Some(other) => Codec::Unsupported(other),
        None if compression == 34826 => Codec::Uncompressed,
        None => Codec::Unsupported(0),
    };

    // The sensor data runs from RawDataOffset to the end of the file.
    // StripOffsets is 0xFFFFFFFF on every compressed body — a
    // deliberate "do not read me as a TIFF image" — so it is only
    // trusted when RawDataOffset is missing and it looks like an
    // offset.
    let start = int(tag::RAW_DATA_OFFSET)
        .or_else(|| int(tags::STRIP_OFFSETS).filter(|o| *o != u32::MAX))
        .map(|o| o as usize)
        .filter(|o| *o < bytes.len())
        .ok_or_else(|| Error::Corrupt("RW2 with no sensor data offset".into()))?;
    let data = &bytes[start..];

    // Every codec stores at least a bit a sample, so the data bounds
    // the frame a forged header may claim; the 11-pixel groups of
    // RawFormat 6 are only known to tile rows that are whole groups.
    if data.len().saturating_mul(8) < samples {
        return Err(Error::Corrupt(format!(
            "RW2 frame of {samples} samples in {} bytes",
            data.len()
        )));
    }
    if matches!(codec, Codec::Groups11) && !width.is_multiple_of(11) {
        return Err(Error::Unsupported(format!(
            "RW2 RawFormat 6 with a width of {width}, not a whole number of 11-pixel groups"
        )));
    }
    let samples = match codec {
        Codec::Blocks14 => decode_v4(data, width, height),
        Codec::Groups11 => decode_v6(data, width, height),
        Codec::PackedBlocks => decode_packed_blocks(data, width, height, bits),
        Codec::Uncompressed => decode_words(data, width, height, bits),
        Codec::Unsupported(version) => {
            return Err(Error::Unsupported(format!(
                "RW2 RawFormat {version}: the 12-bit block codec (4), the \
                 14-bit ones (6, 7) and the uncompressed layout are implemented"
            )))
        }
    };

    let mut raw = RawImage::new(
        Format::Rw2,
        width,
        height,
        1,
        RawData::U16(samples),
        cfa(ifd),
    );
    let (make, model) = tiff.make_model();
    raw.set_camera(&make, &model);

    // Two rectangles, and the tighter one wins: the sensor borders say
    // where the active area is, and the crop tags — present only on
    // the bodies that offer 16:9, 3:2 and 1:1 in camera — say which
    // part of it the photographer framed.
    for edges in [
        (
            tag::SENSOR_TOP,
            tag::SENSOR_LEFT,
            tag::SENSOR_BOTTOM,
            tag::SENSOR_RIGHT,
        ),
        (
            tag::CROP_TOP,
            tag::CROP_LEFT,
            tag::CROP_BOTTOM,
            tag::CROP_RIGHT,
        ),
    ] {
        let (Some(top), Some(left), Some(bottom), Some(right)) =
            (int(edges.0), int(edges.1), int(edges.2), int(edges.3))
        else {
            continue;
        };
        let (x, y) = (left as usize, top as usize);
        let (right, bottom) = (right as usize, bottom as usize);
        if right > x && bottom > y && right <= width && bottom <= height {
            raw.crop = Rect {
                x,
                y,
                width: right - x,
                height: bottom - y,
            };
        }
    }

    // The codecs' zero sits fifteen counts above the level the tag
    // records — it is the bias the block coder starts each group from.
    // The 14-bit and uncompressed layouts have no such bias.
    let bias = if codec == Codec::Blocks14 { 15.0 } else { 0.0 };
    let black = |tag: u16| ifd.get(tag).and_then(|e| e.f64(0)).map(|v| v as f32 + bias);
    if let (Some(r), Some(g), Some(b)) = (
        black(tag::BLACK_LEVEL_RED),
        black(tag::BLACK_LEVEL_GREEN),
        black(tag::BLACK_LEVEL_BLUE),
    ) {
        for position in 0..4 {
            raw.black_levels[position] = match raw.cfa.color_at(position % 2, position / 2) {
                Some(CfaColor::Red) => r,
                Some(CfaColor::Blue) => b,
                _ => g,
            };
        }
    }
    raw.white_level = ifd
        .get(tag::LINEARITY_LIMIT)
        .and_then(|e| e.f64(0))
        .map(|v| v as f32)
        .filter(|v| *v > 0.0)
        .unwrap_or(((1u32 << bits) - 1) as f32);
    if raw.black_levels.iter().any(|b| *b >= raw.white_level) {
        raw.black_levels = [0.0; 4];
    }

    if let (Some(r), Some(g), Some(b)) = (
        ifd.get(tag::WB_RED_LEVEL).and_then(|e| e.f64(0)),
        ifd.get(tag::WB_GREEN_LEVEL).and_then(|e| e.f64(0)),
        ifd.get(tag::WB_BLUE_LEVEL).and_then(|e| e.f64(0)),
    ) {
        if g > 0.0 && r > 0.0 && b > 0.0 {
            raw.wb_coeffs = [(r / g) as f32, 1.0, (b / g) as f32, 1.0];
        }
    } else if let (Some(r), Some(b)) = (
        ifd.get(tag::RED_BALANCE).and_then(|e| e.f64(0)),
        ifd.get(tag::BLUE_BALANCE).and_then(|e| e.f64(0)),
    ) {
        // The oldest bodies give the two balances against a green of
        // 256 and no green level of their own.
        if r > 0.0 && b > 0.0 {
            raw.wb_coeffs = [(r / 256.0) as f32, 1.0, (b / 256.0) as f32, 1.0];
        }
    }

    raw.orientation = common::orientation(&tiff);
    raw.metadata = common::metadata(&tiff);
    raw.preview = preview_from(&tiff);
    // Shutter, aperture, focal length and lens are only in the
    // preview's EXIF; so is the orientation, on the bodies whose IFD0
    // leaves 0x0112 out.
    if let Some(jpeg) = jpeg_exif(&tiff) {
        let from_jpeg = common::metadata(&jpeg);
        let meta = &mut raw.metadata;
        meta.iso = meta.iso.or(from_jpeg.iso);
        meta.exposure_time = meta.exposure_time.or(from_jpeg.exposure_time);
        meta.f_number = meta.f_number.or(from_jpeg.f_number);
        meta.focal_length = meta.focal_length.or(from_jpeg.focal_length);
        meta.lens = meta.lens.take().or(from_jpeg.lens);
        meta.date_time = meta.date_time.take().or(from_jpeg.date_time);
        if !tiff.root().has(tags::ORIENTATION) {
            raw.orientation = common::orientation(&jpeg);
        }
    }

    raw.apply_camera_table();
    Ok(raw)
}

pub fn preview(bytes: &[u8]) -> Result<Option<Vec<u8>>> {
    let tiff = Tiff::parse(bytes)?;
    Ok(preview_from(&tiff))
}

/// The full-size JPEG tag 0x002E carries inline, or whatever
/// [`common::largest_jpeg`] can find if a body ever leaves it out.
fn preview_from(tiff: &Tiff<'_>) -> Option<Vec<u8>> {
    let inline = tiff.root().get(tag::JPEG_FROM_RAW).and_then(|entry| {
        let stream = tiff
            .bytes()
            .get(entry.offset..entry.offset.checked_add(entry.count)?)?;
        stream.starts_with(&[0xFF, 0xD8]).then(|| stream.to_vec())
    });
    match (inline, common::largest_jpeg(tiff)) {
        (Some(inline), Some(found)) if found.len() > inline.len() => Some(found),
        (Some(inline), _) => Some(inline),
        (None, found) => found,
    }
}

/// The TIFF inside the preview JPEG's APP1 segment, positioned so its
/// offsets resolve against the whole file.
fn jpeg_exif<'a>(tiff: &Tiff<'a>) -> Option<Tiff<'a>> {
    let bytes = tiff.bytes();
    let entry = tiff.root().get(tag::JPEG_FROM_RAW)?;
    let end = entry.offset.checked_add(entry.count)?.min(bytes.len());
    let jpeg = bytes.get(entry.offset..end)?;
    if !jpeg.starts_with(&[0xFF, 0xD8]) {
        return None;
    }
    // Walk marker segments only; APP1 is normally the first or second.
    let mut at = 2;
    while at + 4 <= jpeg.len() {
        if jpeg[at] != 0xFF {
            return None;
        }
        let marker = jpeg[at + 1];
        // SOS or EOI: no metadata past here.
        if marker == 0xDA || marker == 0xD9 {
            return None;
        }
        let len = u16::from_be_bytes([jpeg[at + 2], jpeg[at + 3]]) as usize;
        if len < 2 {
            return None;
        }
        if marker == 0xE1 && jpeg.get(at + 4..at + 10) == Some(b"Exif\0\0") {
            return Tiff::parse_embedded(bytes, entry.offset + at + 10).ok();
        }
        at += 2 + len;
    }
    None
}

/// The filter array, from Panasonic's own one-number code.
fn cfa(ifd: &Ifd) -> Cfa {
    match ifd.get(tag::CFA_PATTERN).and_then(|e| e.u32(0)) {
        Some(1) => Cfa::RGGB,
        Some(2) => Cfa::GRBG,
        Some(3) => Cfa::GBRG,
        Some(4) => Cfa::BGGR,
        // Every body seen says which; RGGB is the majority when one
        // does not.
        _ => Cfa::RGGB,
    }
}

/// The oldest layout: one 16-bit little-endian word a sample, with the
/// sample's bits at the *top* of the word, so a 12-bit Digilux 2 reads
/// sixteen times too high until it comes down.
fn decode_words(data: &[u8], width: usize, height: usize, bits: u32) -> Vec<u16> {
    let shift = 16 - bits.min(16);
    let mut out = vec![0u16; width * height];
    let (words, _) = data.as_chunks::<2>();
    for (sample, word) in out.iter_mut().zip(words) {
        *sample = u16::from_le_bytes(*word) >> shift;
    }
    out
}

/// The block every Panasonic codec is built on: sixteen bytes, holding
/// a whole number of pixels so that a row of them starts clean.
const BLOCK_BYTES: usize = 16;
/// How many bytes of the shuffled stream one refill takes.
const BLOCK: usize = 0x4000;
/// Where a block is cut in two before being reassembled. The bytes
/// after this point in the file arrive *first* in the buffer.
const SPLIT: usize = 0x2008;
/// Pixels to a group; the group is the unit the predictor restarts on.
const GROUP: usize = 14;
/// Bytes a group costs. Every pixel spends eight bits, each group
/// spends eight more on its four shift codes, and each of the two
/// column parities spends exactly one four-bit tail — 128 bits, always.
const GROUP_BYTES: usize = BLOCK_BYTES;

/// RawFormat 7: whole 14-bit samples, low bit first, as many as fit in
/// a sixteen-byte block — nine of them, with two bits going to waste.
/// Nothing is predicted or coded; the block is only there to keep the
/// rows byte-aligned.
fn decode_packed_blocks(data: &[u8], width: usize, height: usize, bits: u32) -> Vec<u16> {
    let bits = bits.clamp(1, 16) as usize;
    let per_block = BLOCK_BYTES * 8 / bits;
    let mask = (1u128 << bits) - 1;
    let mut out = vec![0u16; width * height];
    out.par_chunks_mut(per_block)
        .enumerate()
        .for_each(|(block, chunk)| {
            let from = block * BLOCK_BYTES;
            let mut bytes = [0u8; BLOCK_BYTES];
            if let Some(src) = data.get(from..from + BLOCK_BYTES) {
                bytes.copy_from_slice(src);
            }
            let word = u128::from_le_bytes(bytes);
            for (i, sample) in chunk.iter_mut().enumerate() {
                *sample = ((word >> (i * bits)) & mask) as u16;
            }
        });
    out
}

/// RawFormat 6: eleven 14-bit pixels to every sixteen bytes.
///
/// A block is one 128-bit word, filled from the top down: two whole
/// 14-bit pixels, then three groups of three predicted ones, then four
/// bits nobody uses. Each group opens with a two-bit code giving the
/// step its three pixels are measured in — 1, 2, 4 or 16 counts — and
/// then three ten-bit numbers, each the difference from the pixel two
/// places back (the one under the same colour) biased by 512.
///
/// The subtlety is what happens when a difference cannot reach: the
/// predictor is first lowered by half the step's range, and if that
/// takes it below zero — or if the step is at its widest, which is the
/// coder's way of saying "start again here" — it is dropped to zero
/// and the ten bits stand for the pixel outright.
///
/// Everything is coded fifteen counts high; the offset comes off at
/// the end. It is the same fifteen the 12-bit codec leaves in the
/// samples for the black level tag to carry.
fn decode_v6(data: &[u8], width: usize, height: usize) -> Vec<u16> {
    /// Two bits of step code, as a shift.
    const STEPS: [u32; 4] = [0, 1, 2, 4];
    const PIXELS: usize = 11;
    /// What every sample is coded above its true value.
    const BIAS: i32 = 15;

    let mut out = vec![0u16; width * height];
    out.par_chunks_mut(PIXELS)
        .enumerate()
        .for_each(|(block, chunk)| {
            let from = block * BLOCK_BYTES;
            let mut bytes = [0u8; BLOCK_BYTES];
            if let Some(src) = data.get(from..from + BLOCK_BYTES) {
                bytes.copy_from_slice(src);
            }
            let word = u128::from_le_bytes(bytes);
            let field = |at: u32, width: u32| ((word >> at) & ((1 << width) - 1)) as i32;

            let mut pixel = [0i32; PIXELS];
            pixel[0] = field(114, 14);
            pixel[1] = field(100, 14);
            for group in 0..3u32 {
                let step = STEPS[field(98 - group * 32, 2) as usize];
                for j in 0..3u32 {
                    let at = 2 + (group * 3 + j) as usize;
                    let mut pred = pixel[at - 2] - (512 << step);
                    if pred < 0 || step == 4 {
                        pred = 0;
                    }
                    pixel[at] = pred + (field(88 - group * 32 - j * 10, 10) << step);
                }
            }
            for (sample, value) in chunk.iter_mut().zip(pixel) {
                *sample = (value - BIAS).clamp(0, u16::MAX as i32) as u16;
            }
        });
    out
}

/// Panasonic's bit reader.
///
/// The stream is not read straight through. It arrives in 16 KB
/// blocks, and each block is cut at [`SPLIT`] and reassembled with its
/// tail first; within the reassembled buffer the bit counter runs
/// *backwards* from the top, and the byte it lands on is passed
/// through `^ 0x3FF0`, which walks the buffer forwards in groups of
/// sixteen bytes. The two inversions cancel: a reader that simply
/// followed the file would see the same bits, in a different order.
struct PanaBits<'a> {
    data: &'a [u8],
    pos: usize,
    /// The bit cursor, counted down modulo one buffer's worth of bits.
    /// Zero means "the buffer is spent, refill before reading".
    vbits: u32,
    buf: [u8; BLOCK + 1],
}

impl<'a> PanaBits<'a> {
    fn new(data: &'a [u8]) -> PanaBits<'a> {
        PanaBits {
            data,
            pos: 0,
            vbits: 0,
            buf: [0; BLOCK + 1],
        }
    }

    fn refill(&mut self) {
        let take = |data: &'a [u8], from: usize, len: usize| -> &'a [u8] {
            data.get(from..(from + len).min(data.len())).unwrap_or(&[])
        };
        self.buf = [0; BLOCK + 1];
        let tail = take(self.data, self.pos, BLOCK - SPLIT);
        self.buf[SPLIT..SPLIT + tail.len()].copy_from_slice(tail);
        let head = take(self.data, self.pos + (BLOCK - SPLIT), SPLIT);
        self.buf[..head.len()].copy_from_slice(head);
        self.pos += BLOCK;
    }

    /// The next `n` bits (at most 8, which is all this codec ever asks
    /// for). Past the end of the data the buffer is zeros, so a
    /// truncated file decodes to a short picture instead of failing.
    fn get(&mut self, n: u32) -> u32 {
        if self.vbits == 0 {
            self.refill();
        }
        // 0x1FFFF is one buffer of bits less one: the cursor wraps
        // to 0 exactly when the buffer is spent.
        self.vbits = (self.vbits.wrapping_sub(n)) & 0x1FFFF;
        let at = ((self.vbits >> 3) ^ 0x3FF0) as usize;
        let word = self.buf[at] as u32 | (self.buf[at + 1] as u32) << 8;
        (word >> (self.vbits & 7)) & ((1 << n) - 1)
    }
}

/// The predictor state, restarted every [`GROUP`] pixels. Odd and even
/// columns are predicted apart, so that each side of the filter array
/// tracks its own colour.
#[derive(Default)]
struct Group {
    pred: [i32; 2],
    nonz: [i32; 2],
    shift: u32,
}

impl Group {
    /// Decode pixel `i` of a group.
    ///
    /// A pixel is normally an eight-bit difference from the last pixel
    /// of the same parity, scaled by a shift that a two-bit code
    /// refreshes every third pixel — 0, 1, 2 or 4 bits, so the step can
    /// coarsen where the signal is loud. Until a parity has produced
    /// its first non-zero byte it is not predicting anything yet, and
    /// that first byte is instead the top eight bits of a twelve-bit
    /// absolute value; a parity that stays at zero to the end of the
    /// group is given its absolute value anyway, which is what keeps
    /// every group exactly sixteen bytes long.
    fn pixel(&mut self, bits: &mut PanaBits<'_>, i: usize) -> u16 {
        if i == 0 {
            self.pred = [0; 2];
            self.nonz = [0; 2];
        }
        if i % 3 == 2 {
            // 0, 1, 2, 4 — the code counts down from the widest.
            self.shift = 4u32.checked_shr(3 - bits.get(2)).unwrap_or(0);
        }
        let side = i & 1;
        let shift = self.shift;
        if self.nonz[side] != 0 {
            let step = bits.get(8) as i32;
            if step != 0 {
                let low = self.pred[side] - (0x80 << shift);
                // The difference is biased by half its range. Where
                // that would take the prediction below zero — or where
                // the shift is at its widest — only the bits the shift
                // leaves behind are kept, which wraps rather than
                // clips.
                self.pred[side] = if low < 0 || shift == 4 {
                    low & !(-1 << shift)
                } else {
                    low
                };
                self.pred[side] += step << shift;
            }
        } else {
            self.nonz[side] = bits.get(8) as i32;
            if self.nonz[side] != 0 || i > GROUP - 3 {
                self.pred[side] = (self.nonz[side] << 4) | bits.get(4) as i32;
            }
        }
        self.pred[side].clamp(0, u16::MAX as i32) as u16
    }
}

/// RawFormat 4: fourteen 12-bit pixels to every sixteen bytes.
///
/// Because a group is exactly sixteen bytes and a refill is exactly
/// 16 KB, one refill covers exactly 1024 groups — 14336 pixels — and
/// the predictor restarts at every group. So long as a row is a whole
/// number of groups (every body seen makes it so), the blocks are
/// independent and decode in parallel.
fn decode_v4(data: &[u8], width: usize, height: usize) -> Vec<u16> {
    let pixels = width * height;
    let mut out = vec![0u16; pixels];
    let per_block = BLOCK / GROUP_BYTES * GROUP;
    if !width.is_multiple_of(GROUP) {
        // A frame whose rows are not whole groups has to be walked in
        // order: the predictor restarts at the start of every row as
        // well, so groups and blocks stop lining up.
        let mut bits = PanaBits::new(data);
        let mut group = Group::default();
        for row in 0..height {
            for col in 0..width {
                out[row * width + col] = group.pixel(&mut bits, col % GROUP);
            }
        }
        return out;
    }
    out.par_chunks_mut(per_block)
        .enumerate()
        .for_each(|(block, chunk)| {
            let from = block * BLOCK;
            let mut bits = PanaBits::new(data.get(from..).unwrap_or(&[]));
            let mut group = Group::default();
            for (i, sample) in chunk.iter_mut().enumerate() {
                *sample = group.pixel(&mut bits, i % GROUP);
            }
        });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal RW2: the `IIU\0` signature and an IFD0 of Panasonic
    /// tags, with `data` at the end as the sensor stream.
    fn build(entries: &[(u16, u16, u32)], data: &[u8]) -> Vec<u8> {
        let mut entries = entries.to_vec();
        let ifd_at = 8usize;
        let data_at = ifd_at + 2 + (entries.len() + 1) * 12 + 4;
        entries.push((tag::RAW_DATA_OFFSET, 4, data_at as u32));
        entries.sort_by_key(|e| e.0);
        let mut out = Vec::new();
        out.extend_from_slice(b"IIU\0");
        out.extend_from_slice(&(ifd_at as u32).to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        for (tag, kind, value) in &entries {
            out.extend_from_slice(&tag.to_le_bytes());
            out.extend_from_slice(&kind.to_le_bytes());
            out.extend_from_slice(&1u32.to_le_bytes());
            out.extend_from_slice(&value.to_le_bytes());
        }
        out.extend_from_slice(&0u32.to_le_bytes());
        assert_eq!(out.len(), data_at);
        out.extend_from_slice(data);
        out
    }

    fn sensor(width: u32, height: u32, format: Option<u32>, bits: u32) -> Vec<(u16, u16, u32)> {
        let mut out = vec![
            (tag::SENSOR_WIDTH, 3, width),
            (tag::SENSOR_HEIGHT, 3, height),
            (tag::SENSOR_TOP, 3, 0),
            (tag::SENSOR_LEFT, 3, 0),
            (tag::SENSOR_BOTTOM, 3, height),
            (tag::SENSOR_RIGHT, 3, width),
            (tag::CFA_PATTERN, 3, 1),
            (tag::BITS_PER_SAMPLE, 3, bits),
            (
                tag::COMPRESSION,
                3,
                if format.is_some() { 34316 } else { 34826 },
            ),
        ];
        if let Some(format) = format {
            out.push((tag::RAW_FORMAT, 3, format));
        }
        out
    }

    #[test]
    fn the_oldest_layout_is_top_aligned_words() {
        let data = [0x00, 0x12, 0x00, 0x34, 0xf0, 0xff, 0x00, 0x00];
        let file = build(&sensor(4, 1, None, 12), &data);
        let raw = decode(&file).expect("decodes");
        let RawData::U16(samples) = &raw.data else {
            panic!("u16")
        };
        assert_eq!(samples, &[0x120, 0x340, 0xfff, 0]);
        assert_eq!(raw.cfa, Cfa::RGGB);
    }

    /// The shuffled reader. The cursor starts at the top of the
    /// buffer and runs down, and the byte it lands on goes through
    /// `^ 0x3FF0`, so the first byte out is buffer index 15 — which,
    /// because a block's tail is loaded ahead of its head, is file
    /// byte `BLOCK - SPLIT + 15`. The second byte out is index 14, and
    /// so on down each group of sixteen.
    #[test]
    fn the_reader_walks_a_shuffled_block() {
        let mut data = vec![0u8; BLOCK * 2];
        data[BLOCK - SPLIT + 15] = 0xA5;
        data[BLOCK - SPLIT + 14] = 0x5A;
        // The first byte of the file lands at buffer index SPLIT,
        // which the cursor reaches much later.
        data[0] = 0x11;
        let mut bits = PanaBits::new(&data);
        assert_eq!(bits.get(8), 0xA5);
        assert_eq!(bits.get(8), 0x5A);
        for _ in 2..16 {
            assert_eq!(bits.get(8), 0);
        }
        // Sixteen bytes in, the cursor jumps to the next group of
        // sixteen rather than continuing straight down.
        assert_eq!(bits.get(8), 0);
    }

    /// A group is always sixteen bytes: 14 pixels x 8 bits, 4 shift
    /// codes x 2 bits, and one 4-bit tail for each column parity.
    #[test]
    fn a_group_costs_exactly_sixteen_bytes() {
        let data = vec![0u8; BLOCK * 2];
        let mut bits = PanaBits::new(&data);
        let mut group = Group::default();
        let start = bits.vbits;
        for i in 0..GROUP {
            group.pixel(&mut bits, i);
        }
        // The cursor runs down, so a spent group shows as a fall of
        // 128 bits (modulo the buffer).
        assert_eq!((start.wrapping_sub(bits.vbits)) & 0x1FFFF, 128);
    }

    #[test]
    fn an_unknown_codec_says_which() {
        let file = build(&sensor(14, 1, Some(9), 14), &[0; 64]);
        let error = decode(&file).expect_err("unsupported");
        assert!(format!("{error}").contains("RawFormat 9"), "{error}");
    }

    /// The 14-bit group codec, hand-built: the two leaders sit at the
    /// top of the block and read out whole, and a group whose step
    /// code is at its widest ignores the predictor entirely.
    #[test]
    fn the_group_codec_reads_leaders_whole_and_restarts_at_the_widest_step() {
        let mut word: u128 = 0;
        word |= (1000u128 + 15) << 114; // pixel 0
        word |= (2000u128 + 15) << 100; // pixel 1
                                        // Group 0: step code 0, three differences of exactly zero.
        for j in 0..3 {
            word |= 512u128 << (88 - j * 10);
        }
        // Group 1: step code 3 (a shift of four), so its three pixels
        // are the ten-bit fields scaled up, predictor discarded.
        word |= 3u128 << 66;
        for j in 0..3 {
            word |= 100u128 << (56 - j * 10);
        }
        let data = word.to_le_bytes();
        let out = decode_v6(&data, 11, 1);
        assert_eq!(&out[..2], &[1000, 2000]);
        // Zero differences carry the leaders forward, parity by parity.
        assert_eq!(&out[2..5], &[1000, 2000, 1000]);
        assert_eq!(&out[5..8], &[100 * 16 - 15; 3]);
    }

    #[test]
    fn hostile_input_never_panics() {
        assert!(decode(&[]).is_err());
        assert!(decode(b"IIU\0").is_err());
        let file = build(&sensor(14, 4, Some(4), 12), &[0x5a; 128]);
        assert!(decode(&file).is_ok());
        for cut in 0..file.len() {
            let _ = decode(&file[..cut]);
            let _ = preview(&file[..cut]);
        }
    }
}

/// Corpus tests: every RW2, RWL and Panasonic-shaped RAW under
/// `SCHIST_RAW_CORPUS`, against the LibRaw oracle beside it. The
/// oracle helpers live in the ORF module, which shares them.
#[cfg(test)]
mod corpus {
    use super::super::orf::oracle;
    use super::*;

    /// Files this crate knowingly cannot decode yet, with the reason.
    /// A file on this list still has to probe as an RW2 and fail with
    /// `Unsupported` rather than a panic or a wrong picture.
    const UNSUPPORTED: &[&str] = &[
        // RawFormat 6 and 7: the 14-bit codec on the full-frame S
        // bodies. RawFormat 8: the 16-bit one on the GH6 and G9 II.
        "RawFormat",
    ];

    /// `.RAW` is on the list for the Digilux 2 and the LC1, whose
    /// files predate the RW2 extension; other makers use it too, so
    /// anything that does not probe as an RW2 is left to its own
    /// module.
    fn files() -> Vec<std::path::PathBuf> {
        oracle::corpus_files(&["rw2", "rwl", "raw"])
            .into_iter()
            .filter(|path| {
                path.extension()
                    .is_none_or(|e| !e.eq_ignore_ascii_case("raw"))
                    || std::fs::read(path).is_ok_and(|b| crate::probe(&b) == Some(Format::Rw2))
            })
            .collect()
    }

    #[test]
    fn every_file_matches_the_oracle() {
        for path in &files() {
            let bytes = std::fs::read(path).expect("corpus file readable");
            assert_eq!(
                crate::probe(&bytes),
                Some(Format::Rw2),
                "{} did not probe as RW2",
                path.display()
            );
            let raw = match crate::decode(&bytes) {
                Ok(raw) => raw,
                Err(Error::Unsupported(why)) if UNSUPPORTED.iter().any(|m| why.contains(m)) => {
                    eprintln!("{}: {why}", path.display());
                    continue;
                }
                Err(e) => panic!("{}: {e}", path.display()),
            };
            raw.validate().expect("valid");
            oracle::compare_samples(path, &raw);
            oracle::compare_metadata(path, &raw);
            oracle::check_preview(path, &raw);
        }
    }

    #[test]
    fn truncation_never_panics() {
        for path in &files() {
            let bytes = std::fs::read(path).expect("corpus file readable");
            oracle::truncations(bytes.len(), 10, |cut| {
                let _ = crate::decode(&bytes[..cut]);
                let _ = crate::preview(&bytes[..cut]);
            });
        }
    }
}
