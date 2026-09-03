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
//! * 8 — the newest (GH6, G9 II): genuinely 16-bit, and the only one
//!   that is entropy coded. See [`decode_v8`].
//!
//! All of them are implemented; a RawFormat this module does not know
//! returns [`Error::Unsupported`] naming its version.
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
    /// RawFormat 8's output curve: six 32-bit segment mode words, and
    /// six 32-bit segment descriptors (input threshold in the low
    /// half, output base in the high half).
    pub const CURVE_MODES: u16 = 0x0039;
    pub const CURVE_SEGMENTS: u16 = 0x003A;
    /// The ceiling every reconstructed RawFormat 8 sample is clipped
    /// to.
    pub const DATA_MAX: u16 = 0x003B;
    /// The four initial predictors, one per position in the Bayer
    /// quad, in the order top-left, top-right, bottom-left,
    /// bottom-right.
    pub const INITIAL_PREDICTORS: u16 = 0x003C;
    /// Seventeen `(code length, code)` pairs, one per magnitude class.
    pub const HUFFMAN_CODES: u16 = 0x0040;
    /// Seventeen quantiser shifts, one per magnitude class.
    pub const HUFFMAN_SHIFTS: u16 = 0x0041;
    /// How many vertical stripes the frame is cut into, and then one
    /// entry each of: absolute file offset, left column, compressed
    /// size *in bits*, width, height.
    pub const STRIPE_COUNT: u16 = 0x0042;
    pub const STRIPE_OFFSETS: u16 = 0x0044;
    pub const STRIPE_LEFTS: u16 = 0x0045;
    pub const STRIPE_BITS: u16 = 0x0046;
    pub const STRIPE_WIDTHS: u16 = 0x0047;
    pub const STRIPE_HEIGHTS: u16 = 0x0048;
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
    /// RawFormat 8: Huffman-coded DPCM over 2x2 Bayer quads, 16-bit,
    /// in independently coded vertical stripes.
    Quads16,
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
        Some(8) => Codec::Quads16,
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
        // The only codec that does not read from `data`: its stripes
        // carry their own absolute file offsets.
        Codec::Quads16 => decode_v8(bytes, ifd, width, height)?,
        Codec::Unsupported(version) => {
            return Err(Error::Unsupported(format!(
                "RW2 RawFormat {version}: the 12-bit block codec (4), the \
                 14-bit ones (6, 7), the 16-bit one (8) and the uncompressed \
                 layout are implemented"
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

// -------------------------------------------------------- RawFormat 8

/// Magnitude classes in RawFormat 8's Huffman code, `0..=16`.
const V8_CLASSES: usize = 17;
/// The most stripes the format allows.
const V8_MAX_STRIPES: usize = 5;

/// RawFormat 8's bit reader.
///
/// The rule is simply "the stripe's bytes in file order, least
/// significant bit first within each byte". A window of the next 64
/// bits is kept most-significant-bit aligned, so a code is matched
/// against the top of it; nothing is ever realigned after the start of
/// a stripe, and there are no restart markers.
struct V8Bits<'a> {
    bytes: &'a [u8],
    /// Next byte to feed the window.
    pos: usize,
    /// Live bits, left-aligned: the next bit out is bit 63.
    cache: u64,
    nbits: u32,
}

impl<'a> V8Bits<'a> {
    fn new(bytes: &'a [u8]) -> V8Bits<'a> {
        V8Bits {
            bytes,
            pos: 0,
            cache: 0,
            nbits: 0,
        }
    }

    /// Top the window up to at least 57 live bits, reversing each
    /// byte as it goes in — that reversal *is* the LSB-first rule.
    /// Past the end of the stripe it feeds zeros, so a truncated file
    /// decodes to a short picture rather than failing.
    #[inline]
    fn fill(&mut self) {
        while self.nbits <= 56 {
            let byte = self.bytes.get(self.pos).copied().unwrap_or(0);
            self.pos += 1;
            self.cache |= (byte.reverse_bits() as u64) << (56 - self.nbits);
            self.nbits += 8;
        }
    }

    #[inline(always)]
    fn peek(&mut self, n: u32) -> u32 {
        debug_assert!((1..=32).contains(&n));
        if self.nbits < n {
            self.fill();
        }
        ((self.cache >> 32) as u32) >> (32 - n)
    }

    #[inline(always)]
    fn consume(&mut self, n: u32) {
        self.cache <<= n;
        self.nbits -= n;
    }

    #[inline(always)]
    fn get(&mut self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        let value = self.peek(n);
        self.consume(n);
        value
    }
}

/// RawFormat 8's Huffman code.
///
/// Seventeen magnitude classes, each with an explicit `(length, code)`
/// pair in tag `0x0040`. This is a plain prefix code written out in
/// the file: there is no length-count array and no canonical
/// reconstruction to do, and the codes arrive in symbol order rather
/// than in code order.
struct V8Huffman {
    /// Bits of the widest code, and so of the lookup index.
    bits: u32,
    /// `(length << 8) | class` per index; 0 where no code matches.
    lookup: Vec<u16>,
}

impl V8Huffman {
    fn new(pairs: &[(u32, u32)]) -> Result<V8Huffman> {
        let bits = pairs.iter().map(|(length, _)| *length).max().unwrap_or(0);
        if !(1..=16).contains(&bits) {
            return Err(Error::Corrupt(format!(
                "RW2 RawFormat 8 Huffman code with a longest code of {bits} bits"
            )));
        }
        let mut lookup = vec![0u16; 1usize << bits];
        for (class, (length, code)) in pairs.iter().enumerate() {
            if *length == 0 || *length > bits || *code >= (1u32 << length) {
                return Err(Error::Corrupt(format!(
                    "RW2 RawFormat 8 class {class} has a {length}-bit code of {code:#x}"
                )));
            }
            let shift = bits - length;
            let base = (*code as usize) << shift;
            for slot in &mut lookup[base..base + (1usize << shift)] {
                // Two classes claiming one bit pattern would make the
                // stream ambiguous, so the code must be prefix-free.
                if *slot != 0 {
                    return Err(Error::Corrupt(
                        "RW2 RawFormat 8 Huffman code is not prefix-free".into(),
                    ));
                }
                *slot = ((*length as u16) << 8) | class as u16;
            }
        }
        Ok(V8Huffman { bits, lookup })
    }

    /// The next magnitude class, or `None` when the window matches no
    /// code (which only an incomplete table can produce).
    #[inline(always)]
    fn decode(&self, pump: &mut V8Bits<'_>) -> Option<u32> {
        let entry = self.lookup[pump.peek(self.bits) as usize];
        if entry == 0 {
            return None;
        }
        pump.consume((entry >> 8) as u32);
        Some((entry & 0xFF) as u32)
    }
}

/// One signed difference: the mantissa of a magnitude class, in the
/// lossless-JPEG sign convention with the format's quantiser folded in.
///
/// The difference has `class` bits of dynamic range; the low `shift`
/// of them were thrown away by the encoder, so only `class - shift`
/// are transmitted and the reconstruction puts them back at the top
/// and adds half a step. With `shift` zero — which is what every file
/// seen carries — this is exactly lossless JPEG: a value whose top bit
/// is set is itself, one whose top bit is clear is `m - (2^class - 1)`.
#[inline(always)]
fn v8_difference(pump: &mut V8Bits<'_>, class: u32, shift: u32) -> i32 {
    let take = class.saturating_sub(shift);
    let transmitted = pump.get(take) as i32;
    let magnitude = transmitted << shift;
    // The sign is the top bit of the full magnitude, which is the
    // first mantissa bit read.
    let negative = take == 0 || (transmitted >> (take - 1)) & 1 == 0;
    let mut delta = if class == 0 {
        0
    } else if !negative {
        magnitude
    } else if shift == 0 {
        magnitude - (1 << class) + 1
    } else {
        magnitude - (1 << class)
    };
    if shift > 0 {
        // The mid-point of the interval the quantiser threw away.
        delta += 1 << (shift - 1);
    }
    delta
}

/// A codec tag's payload: a 16-bit count, then that many little-endian
/// integers `width` bytes wide. `per` values are read for each counted
/// entry (tag `0x0040` counts pairs).
fn v8_counted(ifd: &Ifd, tag: u16, width: usize, per: usize) -> Result<Vec<u32>> {
    let bytes = ifd
        .get(tag)
        .and_then(|entry| entry.bytes())
        .ok_or_else(|| Error::Corrupt(format!("RW2 RawFormat 8 has no tag {tag:#06x}")))?;
    let count = bytes
        .get(..2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]) as usize)
        .ok_or_else(|| Error::Corrupt(format!("RW2 tag {tag:#06x} is empty")))?;
    let mut out = Vec::with_capacity(count * per);
    for i in 0..count * per {
        let at = 2 + i * width;
        let slice = bytes.get(at..at + width).ok_or_else(|| {
            Error::Corrupt(format!(
                "RW2 tag {tag:#06x} promises {count} entries but holds {} bytes",
                bytes.len()
            ))
        })?;
        out.push(slice.iter().rev().fold(0u32, |v, b| (v << 8) | *b as u32));
    }
    Ok(out)
}

/// The output curve as a 65536-entry lookup, or `None` when it is the
/// identity and can be skipped.
///
/// Six linear segments, each with an input threshold, an output base
/// and a power-of-two slope — a right shift compresses, a left shift
/// expands — plus two encodings for a flat segment. On both bodies in
/// the corpus every descriptor is zero and every mode word is
/// `0x00010000`, which comes out as the identity; the rest of this is
/// therefore written from the format's description and unverified
/// against a body that ships a real curve.
fn v8_curve(modes: &[u32], segments: &[u32], datamax: u32) -> Option<Vec<u16>> {
    let entry = |table: &[u32], i: usize| table.get(i).copied().unwrap_or(0);
    let threshold = |i: usize| entry(segments, i) & 0xFFFF;
    let base = |i: usize| (entry(segments, i) >> 16) & 0xFFFF;
    let mut lookup = vec![0u16; 1 << 16];
    let mut identity = true;
    for (x, slot) in lookup.iter_mut().enumerate() {
        let x = x as u32;
        let mut k = 0usize;
        for j in 1..6 {
            if x >= threshold(j) {
                k = j;
            }
        }
        let mode = entry(modes, k);
        let n = mode & 0x1F;
        // `threshold(0)` need not be zero in a forged table; below it
        // the distance is simply nothing.
        let mut d = x.saturating_sub(threshold(k));
        let value = if n == 31 {
            // A flat segment reading the *next* segment's base.
            if k == 5 {
                0xFFFF
            } else {
                base(k + 1)
            }
        } else {
            if mode & 0x10 == 0 {
                if n == 15 {
                    // The other flat encoding: this segment's base.
                    d = 0;
                } else if n > 0 {
                    d = (d + (1 << (n - 1))) >> n;
                }
            } else {
                d <<= mode & 0x0F;
            }
            d + base(k)
        };
        let value = value.min(datamax).min(0xFFFF) as u16;
        identity &= value as u32 == x;
        *slot = value;
    }
    (!identity).then_some(lookup)
}

/// One stripe: its own bit stream, its own predictors, no prediction
/// across its edges.
///
/// A stripe is decoded as `height / 2` row-pairs of `width / 2` 2x2
/// Bayer quads. The four samples of a quad arrive top-left,
/// bottom-left, top-right, bottom-right, and each is predicted from
/// the same position of the quad immediately to its left — a four-way
/// interleaved DPCM. What starts a row-pair is the *first* quad of the
/// row-pair above, not its last and not the sample directly overhead,
/// which is the one thing in this codec easy to get wrong.
#[allow(clippy::too_many_arguments)]
fn decode_v8_stripe(
    stream: &[u8],
    huffman: &V8Huffman,
    shifts: &[u32; V8_CLASSES],
    initial: [i32; 4],
    width: usize,
    height: usize,
    datamax: i32,
    curve: Option<&[u16]>,
) -> Result<Vec<u16>> {
    let mut out = vec![0u16; width * height];
    let mut pump = V8Bits::new(stream);
    let mut predictor = initial;
    for pair in 0..height / 2 {
        let mut carry = predictor;
        for quad in 0..width / 2 {
            let mut values = [0i32; 4];
            for (position, value) in values.iter_mut().enumerate() {
                let class = huffman.decode(&mut pump).ok_or_else(|| {
                    Error::Corrupt("RW2 RawFormat 8 stream holds no valid code".into())
                })?;
                let delta = v8_difference(&mut pump, class, shifts[class as usize]);
                *value = (predictor[position] + delta).clamp(0, datamax);
            }
            predictor = values;
            if quad == 0 {
                carry = values;
            }
            let store = |value: i32| -> u16 {
                let value = value as u16;
                curve.map_or(value, |curve| curve[value as usize])
            };
            let top = 2 * pair * width + 2 * quad;
            out[top] = store(values[0]);
            out[top + 1] = store(values[2]);
            out[top + width] = store(values[1]);
            out[top + width + 1] = store(values[3]);
        }
        predictor = carry;
    }
    Ok(out)
}

/// One stripe's place in the frame and in the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct V8Stripe {
    offset: usize,
    /// Bytes of coded data: the tag counts *bits*, and the reader
    /// takes whole 64-bit words.
    bytes: usize,
    left: usize,
    width: usize,
    height: usize,
}

/// The stripe table, checked against the frame and the file.
///
/// The five parallel tags are read together because only together do
/// they say anything: the widths must tile the frame exactly, every
/// stripe must be the frame's full height, and every byte range must
/// be inside the file. A stripe table that fails any of those would
/// otherwise decode into a frame with a seam or a hole in it.
fn v8_stripes(ifd: &Ifd, file_len: usize, width: usize, height: usize) -> Result<Vec<V8Stripe>> {
    let count = ifd
        .get(tag::STRIPE_COUNT)
        .and_then(|e| e.u32(0))
        .unwrap_or(0) as usize;
    if count == 0 || count > V8_MAX_STRIPES {
        return Err(Error::Corrupt(format!(
            "RW2 RawFormat 8 in {count} stripes"
        )));
    }
    let offsets = v8_counted(ifd, tag::STRIPE_OFFSETS, 4, 1)?;
    let lefts = v8_counted(ifd, tag::STRIPE_LEFTS, 4, 1)?;
    let sizes = v8_counted(ifd, tag::STRIPE_BITS, 4, 1)?;
    let widths = v8_counted(ifd, tag::STRIPE_WIDTHS, 2, 1)?;
    let heights = v8_counted(ifd, tag::STRIPE_HEIGHTS, 2, 1)?;
    if [&offsets, &lefts, &sizes, &widths, &heights]
        .iter()
        .any(|table| table.len() < count)
    {
        return Err(Error::Corrupt(
            "RW2 RawFormat 8 stripe tables are shorter than the stripe count".into(),
        ));
    }

    let mut stripes = Vec::with_capacity(count);
    let mut covered = 0usize;
    for i in 0..count {
        let stripe = V8Stripe {
            offset: offsets[i] as usize,
            // The tag counts bits; the reader takes whole 64-bit
            // words, so round up twice.
            bytes: (sizes[i] as usize).div_ceil(8).next_multiple_of(8),
            left: lefts[i] as usize,
            width: widths[i] as usize,
            height: heights[i] as usize,
        };
        if stripe.height != height
            || stripe.width == 0
            || stripe.left + stripe.width > width
            || stripe
                .offset
                .checked_add(stripe.bytes)
                .is_none_or(|end| end > file_len)
        {
            return Err(Error::Corrupt(format!(
                "RW2 RawFormat 8 stripe {i} is {}x{} at column {} offset {}, outside a \
                 {width}x{height} frame or the file",
                stripe.width, stripe.height, stripe.left, stripe.offset
            )));
        }
        covered += stripe.width;
        stripes.push(stripe);
    }
    if covered != width {
        return Err(Error::Corrupt(format!(
            "RW2 RawFormat 8 stripes cover {covered} of {width} columns"
        )));
    }
    Ok(stripes)
}

/// RawFormat 8: Huffman-coded DPCM over Bayer quads, in independently
/// coded vertical stripes.
fn decode_v8(bytes: &[u8], ifd: &Ifd, width: usize, height: usize) -> Result<Vec<u16>> {
    let short = |tag: u16| ifd.get(tag).and_then(|e| e.u32(0));
    let datamax = short(tag::DATA_MAX).unwrap_or(u16::MAX as u32).min(0xFFFF);

    let codes = v8_counted(ifd, tag::HUFFMAN_CODES, 2, 2)?;
    if codes.len() != V8_CLASSES * 2 {
        return Err(Error::Corrupt(format!(
            "RW2 RawFormat 8 Huffman table holds {} values, not {}",
            codes.len(),
            V8_CLASSES * 2
        )));
    }
    let pairs: Vec<(u32, u32)> = codes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|p| (p[0], p[1]))
        .collect();
    let huffman = V8Huffman::new(&pairs)?;

    let raw_shifts = v8_counted(ifd, tag::HUFFMAN_SHIFTS, 2, 1)?;
    if raw_shifts.len() != V8_CLASSES {
        return Err(Error::Corrupt(format!(
            "RW2 RawFormat 8 shift table holds {} values, not {V8_CLASSES}",
            raw_shifts.len()
        )));
    }
    let shifts: [u32; V8_CLASSES] = std::array::from_fn(|i| raw_shifts[i] & 0x1F);
    // A class whose quantiser is as wide as the class itself would
    // transmit no mantissa at all and leave the sign bit undefined.
    // No file seen has a non-zero shift; refusing is better than
    // guessing what one would mean.
    if shifts
        .iter()
        .enumerate()
        .any(|(class, shift)| *shift >= (class as u32).max(1))
    {
        return Err(Error::Unsupported(
            "RW2 RawFormat 8 with a quantiser shift at or above its magnitude class".into(),
        ));
    }

    // Tag order is top-left, top-right, bottom-left, bottom-right; the
    // stream's order is top-left, bottom-left, top-right,
    // bottom-right. Both bodies ship zeros, so the mapping is an
    // assumption — but it is the one the tags' own numbering makes.
    let initial = [
        short(tag::INITIAL_PREDICTORS).unwrap_or(0) as i32,
        short(tag::INITIAL_PREDICTORS + 2).unwrap_or(0) as i32,
        short(tag::INITIAL_PREDICTORS + 1).unwrap_or(0) as i32,
        short(tag::INITIAL_PREDICTORS + 3).unwrap_or(0) as i32,
    ];

    let curve = v8_curve(
        &v8_counted(ifd, tag::CURVE_MODES, 4, 1)?,
        &v8_counted(ifd, tag::CURVE_SEGMENTS, 4, 1)?,
        datamax,
    );

    let stripes = v8_stripes(ifd, bytes.len(), width, height)?;

    // Stripes share nothing — separate streams, separate predictors —
    // so they decode in parallel into their own buffers and are
    // stitched afterwards.
    let planes: Result<Vec<Vec<u16>>> = stripes
        .par_iter()
        .map(|stripe| {
            decode_v8_stripe(
                &bytes[stripe.offset..stripe.offset + stripe.bytes],
                &huffman,
                &shifts,
                initial,
                stripe.width,
                stripe.height,
                datamax as i32,
                curve.as_deref(),
            )
        })
        .collect();
    let planes = planes?;

    let mut out = vec![0u16; crate::frame_samples(width, height, 1)?];
    for (stripe, plane) in stripes.iter().zip(&planes) {
        for (row, line) in plane.chunks_exact(stripe.width).enumerate() {
            let at = row * width + stripe.left;
            out[at..at + stripe.width].copy_from_slice(line);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tiff::Entry;

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

    // ---------------------------------------------------- RawFormat 8

    /// The seventeen `(code length, code)` pairs both bodies ship, in
    /// symbol order 0..=16.
    pub(super) const V8_TABLE: [(u32, u32); 17] = [
        (10, 0x3FE),
        (11, 0x7FE),
        (8, 0x0FE),
        (9, 0x1FE),
        (7, 0x07E),
        (4, 0x00E),
        (4, 0x00C),
        (3, 0x004),
        (3, 0x002),
        (2, 0x000),
        (3, 0x003),
        (3, 0x005),
        (4, 0x00D),
        (5, 0x01E),
        (6, 0x03E),
        (12, 0xFFE),
        (12, 0xFFF),
    ];

    /// A RawFormat 8 bit stream: the fields most-significant bit
    /// first, packed into bytes least-significant bit first.
    fn v8_stream(fields: &[(u32, u32)]) -> Vec<u8> {
        let mut bits = Vec::new();
        for (value, length) in fields {
            for i in (0..*length).rev() {
                bits.push(((value >> i) & 1) as u8);
            }
        }
        // Eight bytes of slack so the reader's window can always fill.
        let mut out = vec![0u8; bits.len().div_ceil(8) + 8];
        for (i, bit) in bits.iter().enumerate() {
            out[i / 8] |= bit << (i % 8);
        }
        out
    }

    /// Bytes in file order, least significant bit first within each.
    /// Reading the bytes the other way up fails here rather than three
    /// megapixels later.
    #[test]
    fn the_v8_reader_takes_the_low_bit_of_each_byte_first() {
        // 0b1011_0001 low bit first is 1, 0, 0, 0, 1, 1, 0, 1.
        let mut pump = V8Bits::new(&[0b1011_0001, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(pump.get(4), 0b1000);
        assert_eq!(pump.get(4), 0b1101);
        // A field may straddle a byte boundary without realignment.
        let stream = v8_stream(&[(0x2A5, 12), (7, 3)]);
        let mut pump = V8Bits::new(&stream);
        assert_eq!(pump.get(12), 0x2A5);
        assert_eq!(pump.get(3), 7);
        // Past the end the window feeds zeros rather than failing.
        let mut pump = V8Bits::new(&[]);
        assert_eq!(pump.get(16), 0);
    }

    /// The Huffman table is a plain prefix code written out in the
    /// file, in symbol order rather than code order.
    #[test]
    fn the_v8_huffman_table_is_a_complete_prefix_code() {
        let huffman = V8Huffman::new(&V8_TABLE).expect("builds");
        assert_eq!(huffman.bits, 12);
        // Complete: the seventeen codes tile the twelve-bit window
        // exactly, so no window is left undecodable.
        let total: u32 = V8_TABLE
            .iter()
            .map(|(length, _)| 1u32 << (12 - length))
            .sum();
        assert_eq!(total, 1 << 12);
        assert!(huffman.lookup.iter().all(|entry| *entry != 0));
        // Every code decodes back to its own class.
        for (class, (length, code)) in V8_TABLE.iter().enumerate() {
            let stream = v8_stream(&[(*code, *length), (0, 16)]);
            let mut pump = V8Bits::new(&stream);
            assert_eq!(huffman.decode(&mut pump), Some(class as u32));
        }
        // A table two classes share a code with is refused.
        let mut clash = V8_TABLE;
        clash[0] = (2, 0);
        assert!(V8Huffman::new(&clash).is_err());
    }

    /// The lossless-JPEG sign rule the differences use, and the lossy
    /// reconstruction the format defines but no file in the corpus
    /// exercises.
    #[test]
    fn v8_differences_follow_the_lossless_jpeg_sign_rule() {
        let difference = |class: u32, mantissa: u32, shift: u32| {
            let stream = v8_stream(&[(mantissa, class - shift), (0, 16)]);
            let mut pump = V8Bits::new(&stream);
            v8_difference(&mut pump, class, shift)
        };
        // Class 12, a top bit set: the value is itself. This is the
        // GH6's very first sample.
        assert_eq!(difference(12, 2229, 0), 2229);
        // Class 9 with the top bit clear: 223 - 511, the G9M2's
        // quad 1 bottom-left.
        assert_eq!(difference(9, 223, 0), -288);
        // Class 6 either way.
        assert_eq!(difference(6, 0b100011, 0), 35);
        assert_eq!(difference(6, 0b001110, 0), -49);
        // Class 0 is a difference of zero and reads no mantissa.
        let mut pump = V8Bits::new(&[0xFF; 16]);
        assert_eq!(v8_difference(&mut pump, 0, 0), 0);
        // With a quantiser the thrown-away low bits come back as a
        // half-step: two transmitted bits stand for four, and the
        // rounding term is 2^(shift-1).
        assert_eq!(difference(4, 0b11, 2), (0b11 << 2) + 2);
        assert_eq!(difference(4, 0b01, 2), (0b01 << 2) - 16 + 2);
    }

    /// The output curve. Both bodies ship the identity, which is
    /// detected and skipped; the rest is the format's description,
    /// exercised here on a curve built by hand.
    #[test]
    fn the_v8_output_curve_is_six_shifted_segments() {
        // What the corpus carries: every descriptor zero, every mode
        // word 0x00010000.
        assert!(v8_curve(&[0x0001_0000; 6], &[0; 6], 65535).is_none());
        // A forged table whose first threshold is above zero: every
        // input below it is simply at distance zero, never an
        // underflow.
        let _ = v8_curve(&[0x0001_0000; 6], &[1; 6], 65535);

        // Six segments 256 apart, each with its own base.
        let segments: [u32; 6] = std::array::from_fn(|k| {
            let edge = (k as u32) * 256;
            (edge << 16) | edge
        });
        let modes: [u32; 6] = [
            0x0001_0000, // slope 1
            0x0001_0001, // right shift 1, rounded
            0x0001_0012, // bit 4 set: left shift 2
            0x0001_001F, // flat, taking the next segment's base
            0x0001_000F, // flat, taking this segment's base
            0x0001_0000,
        ];
        let curve = v8_curve(&modes, &segments, 65535).expect("not the identity");
        assert_eq!(curve[100], 100);
        // Segment 1: ((300 - 256) + 1) >> 1, then base 256.
        assert_eq!(curve[300], 278);
        // Segment 2: (600 - 512) << 2, then base 512.
        assert_eq!(curve[600], 864);
        // Segment 3 is flat at segment 4's base.
        assert_eq!(curve[800], 1024);
        // Segment 4 is flat at its own base.
        assert_eq!(curve[1100], 1024);
        // Segment 5 has slope 1 again.
        assert_eq!(curve[2000], 2000);
        // Nothing leaves `datamax`.
        let clipped = v8_curve(&modes, &segments, 1000).expect("not the identity");
        assert!(clipped.iter().all(|v| *v <= 1000));
    }

    /// A stripe table that does not tile the frame, or that points
    /// outside the file, is refused rather than decoded into a frame
    /// with a hole in it.
    #[test]
    fn v8_stripe_tables_must_tile_the_frame() {
        fn counted(tag: u16, width: usize, values: &[u32]) -> Entry {
            let mut bytes = (values.len() as u16).to_le_bytes().to_vec();
            for value in values {
                bytes.extend_from_slice(&value.to_le_bytes()[..width]);
            }
            Entry {
                tag,
                kind: crate::tiff::Kind::Undefined,
                count: bytes.len(),
                offset: 0,
                value: crate::tiff::Value::Undefined(bytes),
            }
        }
        let ifd = |widths: &[u32], lefts: &[u32]| Ifd {
            offset: 0,
            entries: vec![
                Entry {
                    tag: tag::STRIPE_COUNT,
                    kind: crate::tiff::Kind::Short,
                    count: 1,
                    offset: 0,
                    value: crate::tiff::Value::Short(vec![widths.len() as u16]),
                },
                counted(tag::STRIPE_OFFSETS, 4, &vec![0; widths.len()]),
                counted(tag::STRIPE_LEFTS, 4, lefts),
                counted(tag::STRIPE_BITS, 4, &vec![64; widths.len()]),
                counted(tag::STRIPE_WIDTHS, 2, widths),
                counted(tag::STRIPE_HEIGHTS, 2, &vec![4; widths.len()]),
            ],
            ..Ifd::default()
        };
        let stripes = v8_stripes(&ifd(&[8, 8], &[0, 8]), 1024, 16, 4).expect("tiles");
        assert_eq!(stripes.len(), 2);
        assert_eq!(stripes[1].left, 8);
        // 64 bits of stream is eight bytes, and the reader wants whole
        // 64-bit words, so that is what a stripe reserves.
        assert_eq!(stripes[0].bytes, 8);
        // Widths that do not sum to the frame's.
        assert!(v8_stripes(&ifd(&[8, 4], &[0, 8]), 1024, 16, 4).is_err());
        // A stripe running past the end of the file.
        assert!(v8_stripes(&ifd(&[8, 8], &[0, 8]), 4, 16, 4).is_err());
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
    use super::tests::V8_TABLE;
    use super::*;
    use crate::RawImage;

    /// Files this crate knowingly cannot decode yet, with the reason.
    /// A file on this list still has to probe as an RW2 and fail with
    /// `Unsupported` rather than a panic or a wrong picture.
    /// Empty: every RawFormat in the corpus decodes. The list stays
    /// for the codec a future body brings.
    const UNSUPPORTED: &[&str] = &[];

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

    /// A corpus file by name.
    fn sample(name: &str) -> Option<Vec<u8>> {
        let path = files()
            .into_iter()
            .find(|p| p.file_name().is_some_and(|f| f == name))?;
        std::fs::read(path).ok()
    }

    fn frame(raw: &RawImage) -> &[u16] {
        let RawData::U16(data) = &raw.data else {
            panic!("RW2 frames are integers")
        };
        data
    }

    /// The GH6's stripe tables, exactly as tag `0x0044`..`0x0048`
    /// carry them, including that `0x0046` counts bits and not bytes.
    #[test]
    fn the_v8_stripe_tables_parse_as_the_bodies_write_them() {
        for (name, width, height, first_class, want) in [
            (
                "DC-GH6-3-2.RW2",
                5792usize,
                4352usize,
                12u32,
                [
                    (6_926_848usize, 149_867_038usize, 0usize, 2848usize),
                    (25_660_256, 154_571_946, 2848, 2944),
                ],
            ),
            (
                "DC-G9M2-P1000019.RW2",
                5784,
                4344,
                14,
                [
                    (6_683_648, 137_423_474, 0, 2880),
                    (23_861_600, 139_106_557, 2880, 2904),
                ],
            ),
        ] {
            let Some(bytes) = sample(name) else { return };
            let tiff = crate::tiff::Tiff::parse(&bytes).expect("RW2 parses");
            let ifd = tiff.root();
            assert_eq!(ifd.get(tag::RAW_FORMAT).and_then(|e| e.u32(0)), Some(8));
            // The Huffman table both bodies ship is the same one.
            let codes = v8_counted(ifd, tag::HUFFMAN_CODES, 2, 2).expect("code table");
            let pairs: Vec<(u32, u32)> = codes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|p| (p[0], p[1]))
                .collect();
            assert_eq!(pairs, V8_TABLE.to_vec(), "{name} Huffman table");
            // Every quantiser shift is zero: these files are lossless.
            assert_eq!(
                v8_counted(ifd, tag::HUFFMAN_SHIFTS, 2, 1).expect("shift table"),
                vec![0u32; 17],
                "{name} shifts"
            );
            // And the curve is the identity, so it is never applied.
            assert!(v8_curve(
                &v8_counted(ifd, tag::CURVE_MODES, 4, 1).unwrap(),
                &v8_counted(ifd, tag::CURVE_SEGMENTS, 4, 1).unwrap(),
                65535,
            )
            .is_none());

            let stripes = v8_stripes(ifd, bytes.len(), width, height).expect("stripe table");
            assert_eq!(stripes.len(), 2, "{name} stripe count");
            for (stripe, (offset, bits, left, stripe_width)) in stripes.iter().zip(want) {
                assert_eq!(stripe.offset, offset, "{name} stripe offset");
                assert_eq!(stripe.left, left, "{name} stripe left");
                assert_eq!(stripe.width, stripe_width, "{name} stripe width");
                assert_eq!(stripe.height, height, "{name} stripe height");
                // The tag is a bit count; the reader wants whole
                // 64-bit words.
                assert_eq!(
                    stripe.bytes,
                    bits.div_ceil(8).next_multiple_of(8),
                    "{name} stripe bytes"
                );
            }
            assert_eq!(
                stripes.iter().map(|s| s.width).sum::<usize>(),
                width,
                "{name} stripes tile the frame"
            );

            // The very first code of stripe 0: the four-bit one for
            // magnitude class 12 on the GH6, the six-bit one for
            // class 14 on the G9M2. Reading the bytes the wrong way
            // up fails right here rather than a megapixel later.
            let huffman = V8Huffman::new(&pairs).expect("builds");
            let stripe = stripes[0];
            let mut pump = V8Bits::new(&bytes[stripe.offset..stripe.offset + stripe.bytes]);
            assert_eq!(
                huffman.decode(&mut pump),
                Some(first_class),
                "{name} first symbol"
            );
        }
    }

    /// The GH6's first two row-pairs, quad by quad. The second pair is
    /// the decisive test of the vertical carry: it predicts from the
    /// *first* quad of the pair above, not the last one and not the
    /// sample directly overhead.
    #[test]
    fn the_gh6_decodes_its_first_row_pairs_sample_for_sample() {
        let Some(bytes) = sample("DC-GH6-3-2.RW2") else {
            return;
        };
        let raw = crate::decode(&bytes).expect("decodes");
        assert_eq!((raw.width, raw.height), (5792, 4352));
        assert_eq!(raw.cfa, Cfa::RGGB);
        assert_eq!(raw.white_level, 65535.0);
        assert_eq!(raw.black_levels, [2048.0; 4]);
        let frame = frame(&raw);
        let width = raw.width;
        let row = |r: usize| &frame[r * width..(r + 1) * width];
        // Row-pair 0, quads 0..=3 and the two that follow.
        assert_eq!(
            &row(0)[..12],
            &[2229, 2452, 2264, 2532, 2094, 2493, 2099, 2495, 2189, 2390, 2136, 2428]
        );
        assert_eq!(
            &row(1)[..12],
            &[2663, 2224, 2614, 2151, 2523, 2156, 2388, 2225, 2551, 2188, 2416, 2117]
        );
        // Row-pair 1: its predictors are row-pair 0's *first* quad —
        // 2229, 2452, 2663, 2224 — so quad 0 lands on these values.
        assert_eq!(
            &row(2)[..12],
            &[2426, 3009, 2405, 2930, 2274, 2729, 2391, 2590, 2206, 2483, 2188, 2502]
        );
        assert_eq!(
            &row(3)[..12],
            &[4245, 2540, 3573, 2458, 3100, 2340, 2914, 2261, 2690, 2195, 2535, 2186]
        );
        // Stripe 1 restarts from the initial predictors at column
        // 2848, with no prediction across the seam.
        assert_eq!(&row(0)[2848..2852], &[2405, 3292, 2488, 3462]);
        assert_eq!(&row(1)[2848..2852], &[3355, 2334, 3473, 2307]);
    }

    /// The G9M2 is the better fixture for the sign rule: its first
    /// quad spends the two twelve-bit codes and its second exercises
    /// the negative branch at magnitude class 9.
    #[test]
    fn the_g9m2_decodes_its_first_row_pair_sample_for_sample() {
        let Some(bytes) = sample("DC-G9M2-P1000019.RW2") else {
            return;
        };
        let raw = crate::decode(&bytes).expect("decodes");
        assert_eq!((raw.width, raw.height), (5784, 4344));
        let frame = frame(&raw);
        let width = raw.width;
        assert_eq!(
            &frame[..12],
            &[10899, 26184, 11028, 26263, 10600, 26069, 10617, 26639, 10501, 26508, 10791, 25734]
        );
        assert_eq!(
            &frame[width..width + 12],
            &[26010, 19649, 25722, 19679, 26256, 19991, 26058, 20038, 26055, 19969, 25871, 20057]
        );
    }

    #[test]
    fn every_file_matches_the_oracle() {
        let mut checked = 0;
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
            checked += 1;
        }
        eprintln!("rw2: {checked} corpus files matched the oracle");
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
