//! Samsung SRW: a TIFF whose sensor frame hides in a SubIFD with no
//! PhotometricInterpretation, under one of five compressions.
//!
//! Samsung's SubIFD claims `SamplesPerPixel` 3 on most bodies even
//! though a CFA frame has one sample a pixel, and never says the strip
//! is CFA data. What it does carry is `CFARepeatPatternDim` and
//! `CFAPattern`, so the filter array is read from the file rather than
//! looked up, and the strip's own byte count says whether the data is
//! compressed at all. Nothing here is decided by camera model.
//!
//! The compressions, all private numbers:
//!
//! * `32769`, and `32770` on the 2010 bodies — not compressed. Either
//!   one 16-bit little-endian word a pixel or samples packed at
//!   `BitsPerSample`; the strip's size tells which.
//! * `32770` with a row-pointer table — the first lossless codec (the
//!   NX300 generation). See [`decompress_v1`].
//! * `32772` — a Huffman-coded difference per pixel against a table
//!   built into the firmware (NX mini, NX3000). See [`decompress_v2`].
//! * `32773` — the last codec, on the NX1 and NX500: sixteen-column
//!   blocks predicted from the two rows above with a one-dimensional
//!   motion vector, over an optional quantiser. See [`decompress_v3`].
//!
//! Written from observation of the files and their LibRaw-unpacked
//! frames, plus the TIFF 6.0 and Exif specifications and ExifTool's
//! published Samsung tag tables. No decoder source was consulted.

use crate::bits::{BitPump, BitPumpLsb, BitPumpMsb, BitPumpMsb32};
use crate::formats::common;
use crate::tiff::{tags, Ifd, ImageLayout, Tiff};
use crate::{Cfa, CfaColor, Error, Format, RawData, RawImage, Rect, Result};

/// Uncompressed, one 16-bit word a pixel (the EX1 and the compacts).
const COMPRESSION_PLAIN: u32 = 32769;
/// The first lossless codec — but also what the 2010 NX bodies write
/// on a frame that is merely packed, which is why the strip's size
/// decides rather than this number.
const COMPRESSION_V1: u32 = 32770;
/// The firmware-table Huffman codec.
const COMPRESSION_V2: u32 = 32772;
/// The NX1/NX500 codec.
const COMPRESSION_V3: u32 = 32773;

/// Makernote tags this decoder reads. Names are ExifTool's.
mod mn {
    /// `SensorAreas`: two rectangles as `x0 y0 x1 y1`, the second of
    /// which is the active area.
    pub const SENSOR_AREAS: u16 = 0xA010;
    /// `EncryptionKey`: eleven longs, the first four of which are
    /// added to the white-balance and black-level tags.
    pub const ENCRYPTION_KEY: u16 = 0xA020;
    /// `WB_RGGBLevelsUncorrected`: the as-shot balance, R G1 G2 B,
    /// green at 4096.
    pub const WB_LEVELS: u16 = 0xA021;
    /// `WB_RGGBLevelsBlack`: the black point, R G1 G2 B.
    pub const BLACK_LEVELS: u16 = 0xA028;
}

/// SubIFD tag 0xA010, `SamsungRawPointersOffset`: where the per-row
/// offsets of a v1 stream live. Not to be confused with the makernote
/// tag of the same number.
const RAW_POINTERS_OFFSET: u16 = 0xA010;

/// Decode an SRW into its sensor frame and metadata.
pub fn decode(bytes: &[u8]) -> Result<RawImage> {
    let tiff = Tiff::parse(bytes)?;
    let ifd = raw_ifd(&tiff)?;
    let layout = ImageLayout::of(&tiff, ifd)?;
    let (width, height) = (layout.width, layout.height);
    if width == 0 || height == 0 || width > 1 << 16 || height > 1 << 16 {
        return Err(Error::Corrupt(format!("SRW frame is {width}x{height}")));
    }
    let bits = layout.bits_per_sample;
    if !(8..=16).contains(&bits) {
        return Err(Error::Unsupported(format!("SRW with {bits} bits a sample")));
    }
    // One strip is what every SRW writes, and every codec below is a
    // single stream over it.
    let [(start, len)] = layout.chunks[..] else {
        return Err(Error::Unsupported(format!(
            "SRW frame in {} strips, not one",
            layout.chunks.len()
        )));
    };
    let strip = &bytes[start..start + len];

    // The strip bounds the frame: every layout stores at least a bit
    // a sample, so a header claiming more is a forgery and must not
    // size an allocation.
    let pixels = crate::frame_samples(width, height, 1)?;
    if len.saturating_mul(8) < pixels {
        return Err(Error::Corrupt(format!(
            "SRW frame of {pixels} samples in {len} bytes"
        )));
    }
    let packed_bytes = (width * bits as usize).div_ceil(8) * height;
    let data = if len >= pixels * 2 {
        // Samsung writes its sensor words little-endian whatever the
        // TIFF's own byte order says, so this does not follow `tiff`.
        plain(strip, pixels)
    } else if len >= packed_bytes {
        // Two packings, differing only in which end of a byte the
        // first sample comes from. The 2010 bodies (NX5, NX10) write
        // the frame MSB-first and are the only ones in the corpus that
        // set SamplesPerPixel to 1; every later body writes it
        // LSB-first and claims three samples a pixel. That tag is the
        // only thing in the file that tracks the split.
        let msb_first = ifd.get(tags::SAMPLES_PER_PIXEL).and_then(|e| e.u32(0)) == Some(1);
        packed(strip, width, height, bits, msb_first)
    } else {
        match layout.compression {
            // 32769 promises whole samples and nothing else; a strip
            // too small to hold them is a broken file, not a codec
            // this crate has yet to learn.
            COMPRESSION_PLAIN => {
                return Err(Error::Corrupt(format!(
                    "SRW says uncompressed but its {len}-byte strip cannot hold {pixels} samples"
                )))
            }
            COMPRESSION_V1 => {
                let pointers = row_pointers(&tiff, ifd, height)?;
                decompress_v1(strip, &pointers, width, height)
            }
            COMPRESSION_V2 => decompress_v2(strip, width, height),
            COMPRESSION_V3 => decompress_v3(strip, width, height)?,
            other => {
                return Err(Error::Unsupported(format!(
                    "SRW compression {other} with a strip too small to be plain samples"
                )))
            }
        }
    };

    let mut raw = RawImage::new(
        Format::Srw,
        width,
        height,
        1,
        RawData::U16(data),
        cfa(ifd).unwrap_or(Cfa::GRBG),
    );
    let (make, model) = tiff.make_model();
    raw.set_camera(&make, &model);
    // The sensor SubIFD's CFAPattern is wrong on several bodies —
    // checked three ways against the oracle frames and the cameras'
    // own JPEGs — so those are corrected by model.
    if let Some(fixed) = cfa_override(&raw.clean_model) {
        raw.cfa = fixed;
    }
    raw.orientation = common::orientation(&tiff);
    raw.metadata = common::metadata(&tiff);
    raw.preview = common::largest_jpeg(&tiff);
    // Samsung records no saturation point anywhere; the bit depth is
    // it, and the frames in the corpus do clip at exactly that.
    raw.white_level = ((1u32 << bits) - 1) as f32;

    let maker = makernote(&tiff);
    if let Some(maker) = maker.as_ref() {
        let key = quad(maker, mn::ENCRYPTION_KEY).unwrap_or([0.0; 4]);
        // Samsung stores both level quads with a fixed eleven-long
        // "encryption key" added, the first four entries of which
        // apply here. Subtracting it is what turns the tag into the
        // number the camera meant, and every body in the corpus ships
        // the same key.
        if let Some(levels) = quad(maker, mn::BLACK_LEVELS) {
            let levels = std::array::from_fn(|i| levels[i] - key[i]);
            raw.black_levels = by_position(&raw.cfa, levels);
        }
        if let Some(wb) = quad(maker, mn::WB_LEVELS) {
            let wb: [f32; 4] = std::array::from_fn(|i| wb[i] - key[i]);
            let green = wb[1];
            if green > 0.0 && wb.iter().all(|v| *v > 0.0) {
                // R G1 G2 B in the tag; R G B G2 in `wb_coeffs`.
                raw.wb_coeffs = [wb[0] / green, 1.0, wb[3] / green, wb[2] / green];
            }
        }
        raw.crop = active_area(maker, width, height);
    }

    raw.apply_camera_table();
    Ok(raw)
}

/// The largest embedded JPEG: SRW keeps a full-size one in the first
/// SubIFD and a thumbnail in IFD1's.
pub fn preview(bytes: &[u8]) -> Result<Option<Vec<u8>>> {
    Ok(common::largest_jpeg(&Tiff::parse(bytes)?))
}

// ------------------------------------------------------------ structure

/// The SubIFD holding sensor samples.
///
/// No SRW sets PhotometricInterpretation on it, so it is found the way
/// [`crate::probe`] finds it: the IFD with strips more than eight bits
/// deep. The full-size preview's IFD is JPEG-compressed and has no
/// BitsPerSample at all, so the two never collide.
fn raw_ifd<'a>(tiff: &'a Tiff<'_>) -> Result<&'a Ifd> {
    tiff.all()
        .into_iter()
        .find(|ifd| {
            ifd.has(tags::STRIP_OFFSETS)
                && ifd
                    .get(tags::BITS_PER_SAMPLE)
                    .and_then(|e| e.u32(0))
                    .unwrap_or(0)
                    > 8
        })
        .ok_or_else(|| Error::Corrupt("SRW with no sensor SubIFD".into()))
}

/// Bodies whose CFAPattern tag does not describe the frame it sits
/// in. Verified per body by the green-parity correlation of the
/// oracle frame and by chroma against the camera's embedded JPEG; a
/// wrong phase here swaps red and blue in the developed picture.
fn cfa_override(clean_model: &str) -> Option<Cfa> {
    let model = clean_model.to_ascii_uppercase();
    let model = model.trim();
    match model {
        "NX2000" | "NX300" | "EK-GN120" | "NX3000" => Some(Cfa::GRBG),
        "NX MINI" => Some(Cfa::RGGB),
        _ => None,
    }
}

/// `CFARepeatPatternDim` and `CFAPattern` from the sensor SubIFD, as
/// the pattern at the frame's own origin.
///
/// LibRaw's "Filter pattern" line names the pattern at *its* crop
/// origin, which on several bodies is an odd column in and so reads a
/// column-shifted version of this. The file's tag is the frame's, and
/// [`crate::RawImage::cfa`] is defined at the frame origin.
fn cfa(ifd: &Ifd) -> Option<Cfa> {
    let dim = ifd.get(tags::CFA_REPEAT_PATTERN_DIM)?;
    let (width, height) = (dim.u32(0)? as usize, dim.u32(1)? as usize);
    let pattern = ifd.get(tags::CFA_PATTERN)?.bytes()?;
    if width == 0 || height == 0 || pattern.len() < width * height {
        return None;
    }
    let color = |v: u8| match v {
        0 => Some(CfaColor::Red),
        1 => Some(CfaColor::Green),
        2 => Some(CfaColor::Blue),
        _ => None,
    };
    let colors: Option<Vec<CfaColor>> = pattern[..width * height]
        .iter()
        .map(|v| color(*v))
        .collect();
    let colors = colors?;
    if (width, height) == (2, 2) {
        Some(Cfa::Bayer([colors[0], colors[1], colors[2], colors[3]]))
    } else {
        Some(Cfa::Pattern {
            width,
            height,
            colors,
        })
    }
}

/// The makernote as a TIFF.
///
/// Samsung's is a bare IFD — no signature, no byte-order mark — whose
/// offsets are measured from the makernote's own start. It shares the
/// file's byte order.
fn makernote<'a>(tiff: &Tiff<'a>) -> Option<Tiff<'a>> {
    let start = tiff.find(tags::MAKER_NOTE)?.offset;
    Tiff::parse_at_relative(tiff.bytes(), start, start, tiff.little_endian()).ok()
}

/// A four-element makernote value as floats, in the tag's own order.
fn quad(maker: &Tiff<'_>, tag: u16) -> Option<[f32; 4]> {
    let entry = maker.find(tag)?;
    let values: Vec<f32> = (0..4)
        .filter_map(|i| entry.f64(i))
        .map(|v| v as f32)
        .collect();
    values.try_into().ok()
}

/// Spread a tag's R G1 G2 B quad over the frame's four CFA positions.
fn by_position(cfa: &Cfa, values: [f32; 4]) -> [f32; 4] {
    let mut greens = 0;
    std::array::from_fn(|i| match cfa.color_at(i % 2, i / 2) {
        Some(CfaColor::Red) => values[0],
        Some(CfaColor::Blue) => values[3],
        Some(CfaColor::Green | CfaColor::Green2) => {
            greens += 1;
            values[greens.min(2)]
        }
        _ => values[0],
    })
}

/// The active area, from the second rectangle of `SensorAreas`.
///
/// The tag holds two `x0 y0 x1 y1` rectangles: the sensor's whole
/// readout and, second, the part meant to be developed. The corners
/// are inclusive of the first and exclusive of the second, so the
/// width is `x1 - x0`.
fn active_area(maker: &Tiff<'_>, width: usize, height: usize) -> Rect {
    let whole = Rect {
        x: 0,
        y: 0,
        width,
        height,
    };
    let Some(areas) = maker.find(mn::SENSOR_AREAS) else {
        return whole;
    };
    let corner = |i: usize| areas.u32(i).map(|v| v as usize);
    let (Some(x0), Some(y0), Some(x1), Some(y1)) = (corner(4), corner(5), corner(6), corner(7))
    else {
        return whole;
    };
    if x1 <= x0 || y1 <= y0 || x1 > width || y1 > height {
        return whole;
    }
    Rect {
        x: x0,
        y: y0,
        width: x1 - x0,
        height: y1 - y0,
    }
}

/// The per-row byte offsets of a v1 stream, relative to the strip.
fn row_pointers(tiff: &Tiff<'_>, ifd: &Ifd, height: usize) -> Result<Vec<u32>> {
    let offset = ifd
        .get(RAW_POINTERS_OFFSET)
        .and_then(|e| e.u32(0))
        .filter(|o| *o != 0)
        .ok_or_else(|| Error::Corrupt("compressed SRW without a row-pointer table".into()))?
        as usize;
    // The table is four bytes a row; the frame's height, not the
    // length tag (which is only ever that number), says how many.
    let base = tiff.base() + offset;
    let want = height * 4;
    let table = tiff
        .bytes()
        .get(base..base + want)
        .ok_or_else(|| Error::Corrupt("SRW row-pointer table outside the file".into()))?;
    Ok(table
        .chunks(4)
        .filter(|b| b.len() == 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect())
}

// ------------------------------------------------------- uncompressed

/// One 16-bit little-endian word a pixel.
fn plain(strip: &[u8], pixels: usize) -> Vec<u16> {
    let mut out = vec![0u16; pixels];
    for (sample, word) in out.iter_mut().zip(strip.chunks(2)) {
        if let [low, high] = word {
            *sample = u16::from_le_bytes([*low, *high]);
        }
    }
    out
}

/// Samples packed at `bits`, rows starting on byte boundaries.
fn packed(strip: &[u8], width: usize, height: usize, bits: u32, msb_first: bool) -> Vec<u16> {
    let stride = (width * bits as usize).div_ceil(8);
    let mut out = vec![0u16; width * height];
    for (row, samples) in out.chunks_mut(width).enumerate() {
        let from = row * stride;
        let source = strip
            .get(from..(from + stride).min(strip.len()))
            .unwrap_or(&[]);
        if msb_first {
            let mut pump = BitPumpMsb::new(source);
            for sample in samples.iter_mut() {
                *sample = pump.get(bits) as u16;
            }
        } else {
            let mut pump = BitPumpLsb::new(source);
            for sample in samples.iter_mut() {
                *sample = pump.get(bits) as u16;
            }
        }
    }
    out
}

// ------------------------------------------------------------ codec v1

/// How many pixels of one row a v1 block covers.
const V1_BLOCK: usize = 16;

/// Decode Samsung's first lossless codec (Compression 32770 with a
/// row-pointer table).
///
/// The pointer table has an entry a row, but a *stream* is not a row:
/// stream `s` holds the columns of parity `s & 1` from both rows of
/// the pair `s & !1`. Two streams therefore cover two rows, and the
/// frame is read as four interleaved colour planes rather than as
/// scan lines.
///
/// A stream is a bit sequence read most-significant bit first out of
/// 32-bit *little-endian* words, in blocks of sixteen values covering
/// sixteen columns: eight from the pair's first row and eight from its
/// second, at the eight columns of the stream's parity. Each block
/// opens with
///
/// ```text
///   1 bit   direction: 0 predict along the row, 1 predict up
///   2 bits  x4  what to do to each of the four difference lengths:
///               0 leave it, 1 add one, 2 take one off,
///               3 read a fresh 4-bit length
/// ```
///
/// The four lengths are indexed by which row of the pair the value
/// belongs to and which half of the block it is in, so a block can
/// spend more bits on its noisier half. Each value is then that many
/// bits, two's complement, added to a predictor:
///
/// * along the row (direction 0), every value in a row of the block
///   shares one predictor: the last value of the same row in the
///   *previous* block. The first block of a row starts from 128.
/// * upwards (direction 1), the predictor is the value in the same
///   slot of an earlier stream — the one before this for the first
///   row of the pair, two before for the second, which is what puts
///   each value against the nearest already-decoded sample of a
///   neighbouring colour plane.
fn decompress_v1(strip: &[u8], pointers: &[u32], width: usize, height: usize) -> Vec<u16> {
    let mut out = vec![0u16; width * height];
    let at = |out: &[u16], row: usize, col: usize| -> u16 {
        out.get(row * width + col).copied().unwrap_or(0)
    };
    for stream in 0..height {
        let pair = stream & !1;
        let parity = stream & 1;
        let start = pointers.get(stream).copied().unwrap_or(0) as usize;
        let mut pump = BitPumpMsb32::new(strip.get(start..).unwrap_or(&[]));
        // The first two rows have no row above to lean on, so they
        // start expecting wider differences.
        let mut lengths = [if pair < 2 { 7i32 } else { 4 }; 4];
        for base in (0..width).step_by(V1_BLOCK) {
            let up = pump.get(1) == 1;
            let ops = [pump.get(2), pump.get(2), pump.get(2), pump.get(2)];
            for (length, op) in lengths.iter_mut().zip(ops) {
                match op {
                    3 => *length = pump.get(4) as i32,
                    2 => *length -= 1,
                    1 => *length += 1,
                    _ => {}
                }
            }
            // Evens first, then odds: within a block the pair's first
            // row is decoded before its second.
            for c in (0..16).map(|i| (i % 8) * 2 + i / 8) {
                let index = ((c & 1) << 1) | (c >> 3);
                let length = lengths[index];
                if !(0..=16).contains(&length) {
                    // A length outside the format's range means the
                    // stream has come apart; leave the rest of the row
                    // black rather than read wild widths.
                    break;
                }
                let value = pump.get(length as u32) as i32;
                let diff = if length > 0 && value & (1 << (length - 1)) != 0 {
                    value - (1 << length)
                } else {
                    value
                };
                let row = pair + (c & 1);
                let col = base + (c & !1) + parity;
                if col >= width || row >= height {
                    continue;
                }
                let predictor = if up {
                    if c & 1 == 1 {
                        // The same slot two streams back: the same
                        // colour, one row pair up.
                        if pair >= 1 {
                            at(&out, pair - 1, col)
                        } else {
                            0
                        }
                    } else if parity == 1 {
                        // The stream before this one is the same row
                        // pair's other parity: the pixel to the left.
                        at(&out, pair, col - 1)
                    } else if pair >= 2 && col + 1 < width {
                        at(&out, pair - 2, col + 1)
                    } else {
                        0
                    }
                } else if base >= 2 {
                    at(&out, row, base - 2 + parity)
                } else {
                    128
                };
                out[row * width + col] = predictor.wrapping_add(diff as u16);
            }
        }
    }
    out
}

// ------------------------------------------------------------ codec v2

/// The Huffman code of Compression 32772, as `(code length, symbol)`
/// in the order the codes are handed out.
///
/// Nothing in the file carries it: the camera and the decoder both
/// have it built in, so it is a fact about the format, recovered here
/// by fitting a prefix code to real frames until they reproduced their
/// LibRaw-unpacked samples exactly (they do, on every 32772 file in
/// this crate's corpus, to the last bit).
///
/// The symbol is the number of extra bits the difference is written
/// in, as in lossless JPEG. The lengths are not monotonic — a 6-bit
/// code is handed out before a 5-bit one — so this is *not* a
/// canonical table and cannot go through [`crate::bits::HuffTable`];
/// [`V2Huffman`] decodes it with a flat lookup instead.
const V2_CODE: [(u8, u8); 14] = [
    (3, 4),
    (3, 7),
    (2, 6),
    (2, 5),
    (4, 3),
    (6, 0),
    (7, 9),
    (8, 10),
    (9, 11),
    (10, 12),
    (10, 13),
    (5, 1),
    (4, 8),
    (4, 2),
];

/// Width of [`V2Huffman`]'s lookup, and the longest code in
/// [`V2_CODE`].
const V2_BITS: u32 = 10;

/// A complete lookup for [`V2_CODE`]: every ten-bit window maps
/// straight to `(code length, symbol)`.
struct V2Huffman {
    lookup: Vec<(u8, u8)>,
}

impl V2Huffman {
    fn new() -> V2Huffman {
        let mut lookup = Vec::with_capacity(1 << V2_BITS);
        for (length, symbol) in V2_CODE {
            // A code of `length` bits owns every window that begins
            // with it, and the codes are handed out in order, so
            // filling the table in order places them.
            lookup.resize(
                lookup.len() + (1 << (V2_BITS - length as u32)),
                (length, symbol),
            );
        }
        // The code is complete: the fourteen entries tile the window
        // exactly. Anything else would be a bug in the table above.
        debug_assert_eq!(lookup.len(), 1 << V2_BITS);
        lookup.resize(1 << V2_BITS, (V2_BITS as u8, 0));
        V2Huffman { lookup }
    }

    /// The next difference: a length symbol, then that many bits,
    /// sign-extended the way lossless JPEG does it.
    fn diff(&self, pump: &mut impl BitPump) -> i32 {
        let (length, symbol) = self.lookup[pump.peek(V2_BITS) as usize];
        pump.consume(length as u32);
        if symbol == 0 {
            return 0;
        }
        let bits = symbol as u32;
        let value = pump.get(bits) as i32;
        if value < 1 << (bits - 1) {
            value - (1 << bits) + 1
        } else {
            value
        }
    }
}

/// Decode Compression 32772: one Huffman-coded difference a pixel over
/// a single stream, predicted from the pixel two to the left, with the
/// first two pixels of each row continuing a total kept per row parity
/// and column parity. The same predictor Pentax uses, over Samsung's
/// own fixed code.
fn decompress_v2(strip: &[u8], width: usize, height: usize) -> Vec<u16> {
    let huffman = V2Huffman::new();
    let mut pump = BitPumpMsb::new(strip);
    let mut out = vec![0u16; width * height];
    let mut vertical = [[0u16; 2]; 2];
    for (row, samples) in out.chunks_mut(width).enumerate() {
        let mut horizontal = [0u16; 2];
        for (col, sample) in samples.iter_mut().enumerate() {
            let diff = huffman.diff(&mut pump) as u16;
            if col < 2 {
                let seed = &mut vertical[row & 1][col];
                *seed = seed.wrapping_add(diff);
                horizontal[col] = *seed;
            } else {
                horizontal[col & 1] = horizontal[col & 1].wrapping_add(diff);
            }
            *sample = horizontal[col & 1];
        }
    }
    out
}

// ------------------------------------------------------------ codec v3

/// Columns to a block of the NX1/NX500 codec.
const V3_BLOCK: usize = 16;
/// The stream header's size, and the boundary every row starts on.
const V3_ALIGN: usize = 16;
/// The narrowest and shortest frame the codec is defined for. Below
/// this a block's prediction would reach outside the frame.
const V3_MIN_WIDTH: usize = 646;
const V3_MIN_HEIGHT: usize = 436;

/// Bits of the header's option nibble. Each one *removes* a per-block
/// field from the stream, so a decoder that ignored them would read
/// the fields that are not there and lose the stream immediately.
mod v3 {
    /// The difference lengths are in every block; the one-bit "lengths
    /// unchanged" flag is not written.
    pub const LENGTHS_ALWAYS: u32 = 1;
    /// The prediction mode is one bit: 0 means mode 7, 1 means mode 3.
    pub const SHORT_MODE: u32 = 2;
    /// No quantiser-scale field: the file is lossless.
    pub const LOSSLESS: u32 = 4;
}

/// How far left or right of the target column a prediction mode looks,
/// and whether it averages two references to land on a half-way
/// offset. Modes 0..=6; mode 7 predicts along the row instead.
const V3_MOTION_OFFSET: [i32; 7] = [-4, -2, -2, 0, 0, 2, 4];
const V3_MOTION_AVERAGE: [bool; 7] = [false, false, true, false, true, false, false];

/// The sixteen-byte stream header. Only three of its fields have a
/// known meaning; the rest is identical between the two bodies in the
/// corpus and is skipped by width.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct V3Header {
    /// Sample depth, stored one less than it is.
    depth: u32,
    /// See the [`v3`] option bits.
    options: u32,
    /// The predictor for the first sixteen columns of every row.
    initial: u16,
}

/// Read the header out of the first sixteen bytes of the stream.
///
/// The whole header is bit-packed and read with the codec's own
/// reader — most significant bit first inside 32-bit little-endian
/// words — so getting the word order wrong is caught here rather than
/// three megapixels later.
fn v3_header(strip: &[u8]) -> V3Header {
    let mut pump = BitPumpMsb32::new(strip);
    pump.consume(20);
    let depth = pump.get(4) + 1;
    // Bits 24..=83: unknown, and the same on both bodies.
    pump.consume(8);
    pump.consume(32);
    pump.consume(16);
    pump.consume(4);
    let options = pump.get(4);
    // Bits 88..=113: unknown.
    pump.consume(26);
    V3Header {
        depth,
        options,
        initial: pump.get(14) as u16,
    }
}

/// Decode Samsung's last codec (Compression 32773).
///
/// The frame is one stream. Rows are independent: each starts on a
/// sixteen-byte boundary measured from the start of the stream, with
/// the bit reader reset, so a row's length is only known once it has
/// been decoded. Within a row, columns are taken sixteen at a time.
///
/// Every block first *fills* its sixteen columns with a prediction and
/// then adds a difference to each in place. The prediction is either
/// along the row — every column takes the value two columns back,
/// which for the first block of a row is the header's initial value —
/// or from the two rows above: on this GRBG sensor the greens of a row
/// sit diagonally next to the greens of the row above and the reds and
/// blues sit two rows straight up, so a mode picks that reference and
/// then slides it left or right by up to four columns. It is a
/// one-dimensional motion vector, and the two averaging modes give it
/// the half-way offsets.
///
/// The differences are coded in an interleaved order — all eight
/// even-parity columns of the block, then all eight odd ones — in four
/// groups of four, each group with its own bit width carried from the
/// previous block. A quantiser scale, refreshed every fourth block,
/// multiplies them; at scale 0, which is what a lossless file uses
/// throughout, it is the identity.
fn decompress_v3(strip: &[u8], width: usize, height: usize) -> Result<Vec<u16>> {
    if width < V3_MIN_WIDTH || height < V3_MIN_HEIGHT {
        return Err(Error::Corrupt(format!(
            "SRW compression 32773 needs at least {V3_MIN_WIDTH}x{V3_MIN_HEIGHT}, not {width}x{height}"
        )));
    }
    if strip.len() < V3_ALIGN {
        return Err(Error::Corrupt(
            "SRW compression 32773 with no room for its header".into(),
        ));
    }
    let header = v3_header(strip);
    if !(8..=16).contains(&header.depth) {
        return Err(Error::Corrupt(format!(
            "SRW compression 32773 claims {} bits a sample",
            header.depth
        )));
    }
    let datamax = ((1u32 << header.depth) - 1) as i32;
    let mut out = vec![0u16; width * height];
    // Row 0 opens immediately after the header.
    let mut at = V3_ALIGN;
    for row in 0..height {
        let mut pump = BitPumpMsb32::new(strip.get(at..).unwrap_or(&[]));
        decode_v3_row(&mut pump, &mut out, row, width, &header, datamax);
        // The next row starts at the next sixteen-byte boundary. The
        // reader fetches whole 32-bit words, so rounding the bits it
        // consumed up to a byte and then to sixteen lands exactly
        // where its own fetch pointer would.
        at += pump.position().div_ceil(8).next_multiple_of(V3_ALIGN);
    }
    Ok(out)
}

/// One row: blocks of sixteen columns until fewer than sixteen are
/// left. A width that is not a whole number of blocks leaves its last
/// few columns black; no body in the corpus has one.
fn decode_v3_row(
    pump: &mut impl BitPump,
    out: &mut [u16],
    row: usize,
    width: usize,
    header: &V3Header,
    datamax: i32,
) {
    let mut scale = 0i32;
    let mut motion = 7u32;
    // The first two rows have no rows above to lean on, so they open
    // expecting the wide differences of an absolute-ish first block.
    let mut lengths = [if row < 2 { 7i32 } else { 4 }; 4];
    let mut col = 0;
    while col + V3_BLOCK <= width {
        // Once every four blocks, and only on a lossy file.
        if header.options & v3::LOSSLESS == 0 && col.is_multiple_of(4 * V3_BLOCK) {
            match pump.get(2) {
                1 => scale -= 2,
                2 => scale += 2,
                3 => scale = pump.get(12) as i32,
                _ => {}
            }
        }
        if header.options & v3::SHORT_MODE != 0 {
            motion = if pump.get(1) == 0 { 7 } else { 3 };
        } else if pump.get(1) == 0 {
            motion = pump.get(3);
        }
        // Rows 0 and 1 have no rows above them: only mode 7 is legal
        // there, and a file that says otherwise is broken rather than
        // a reason to read outside the frame.
        let mode = if row < 2 { 7 } else { motion };
        v3_predict(out, row, col, width, mode, header.initial);

        // With option bit 0 set the lengths are always written; with
        // it clear a leading 1 bit means "the previous block's".
        if header.options & v3::LENGTHS_ALWAYS != 0 || pump.get(1) == 0 {
            v3_lengths(pump, &mut lengths);
        }
        if lengths
            .iter()
            .any(|l| !(0..=header.depth as i32 + 1).contains(l))
        {
            // A length the format cannot mean: the stream has come
            // apart, and reading on would only spend bits wildly.
            return;
        }

        for i in 0..V3_BLOCK {
            let target = v3_target(row, col, i);
            let length = lengths[i / 4];
            // A length of zero reads no bits and codes a quantised
            // difference of zero — but *not* a difference of zero: the
            // dequantiser's mid-point offset still applies, so a lossy
            // file shifts such a group up by its scale.
            let quantised = if length == 0 {
                0
            } else {
                let value = pump.get(length as u32) as i32;
                // Two's complement in `length` bits.
                if value & (1 << (length - 1)) != 0 {
                    value - (1 << length)
                } else {
                    value
                }
            };
            // The lossy step, and its mid-point reconstruction
            // offset. At scale 0 this is the identity.
            let difference = quantised * (2 * scale + 1) + scale;
            let at = row * width + target;
            if let Some(sample) = out.get_mut(at) {
                // The clamp is not cosmetic: without it one clipped
                // highlight propagates into every row below through
                // the prediction above.
                *sample = (*sample as i32 + difference).clamp(0, datamax) as u16;
            }
        }
        col += V3_BLOCK;
    }
}

/// The four difference lengths of a block, one for each group of four
/// entries of the interleaved order.
///
/// All four two-bit selectors are read before any of them is resolved,
/// and a selector's "predicted value" is the length the same group had
/// in the previous block of this row.
fn v3_lengths(pump: &mut impl BitPump, lengths: &mut [i32; 4]) {
    let flags = [pump.get(2), pump.get(2), pump.get(2), pump.get(2)];
    for (length, flag) in lengths.iter_mut().zip(flags) {
        *length = match flag {
            1 => *length + 1,
            2 => *length - 1,
            3 => pump.get(4) as i32,
            _ => *length,
        };
    }
}

/// Which column entry `i` of a block's sixteen differences belongs to.
///
/// Differences are coded interleaved by column parity: on an even row
/// the eight even columns come first and the eight odd ones after, and
/// on an odd row the other way round. The predictions of the same
/// block were written in plain column order, so the two meet in the
/// frame.
fn v3_target(row: usize, col: usize, i: usize) -> usize {
    if row.is_multiple_of(2) {
        col + 2 * (i % 8) + i / 8
    } else {
        col + 2 * (i % 8) + 1 - i / 8
    }
}

/// Fill a block's sixteen columns with their predictions, before any
/// difference is read.
fn v3_predict(out: &mut [u16], row: usize, col: usize, width: usize, mode: u32, initial: u16) {
    let base = row * width;
    if mode >= 7 || row < 2 {
        // Along the row, and sequentially: because the source is two
        // columns back, every even offset in the block ends up with
        // the last even column of the previous block and every odd
        // offset with the last odd one.
        for i in 0..V3_BLOCK {
            let at = base + col + i;
            if at >= out.len() {
                return;
            }
            out[at] = if col == 0 { initial } else { out[at - 2] };
        }
        return;
    }
    let offset = V3_MOTION_OFFSET[mode as usize] as isize;
    let average = V3_MOTION_AVERAGE[mode as usize];
    for i in 0..V3_BLOCK {
        // Green samples sit diagonally above, one row up and one
        // column across; red and blue sit two rows straight up.
        let far = !(row + i).is_multiple_of(2);
        let reference_row = if far { row - 2 } else { row - 1 };
        let across = if far {
            0
        } else if i.is_multiple_of(2) {
            1
        } else {
            -1
        };
        let from = (reference_row * width) as isize + (col + i) as isize + offset + across;
        // The frame is one flat buffer, so a reference that runs off
        // the side of a row is simply a neighbouring row's sample; one
        // that runs off the buffer reads as black.
        let sample = |index: isize| -> u32 {
            usize::try_from(index)
                .ok()
                .and_then(|i| out.get(i))
                .map_or(0, |s| *s as u32)
        };
        let value = if average {
            (sample(from) + sample(from + 2) + 1) >> 1
        } else {
            sample(from)
        };
        let at = base + col + i;
        if at < out.len() {
            out[at] = value as u16;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    #[test]
    fn the_v2_code_tiles_its_window() {
        // A prefix code is complete exactly when its lengths' weights
        // sum to one; anything else and some bit pattern decodes to
        // nothing, or two codes overlap.
        let total: u32 = V2_CODE
            .iter()
            .map(|(length, _)| 1u32 << (V2_BITS - *length as u32))
            .sum();
        assert_eq!(total, 1 << V2_BITS);
        // Every symbol 0..=13 appears exactly once: they are difference
        // lengths, and a missing one could not be encoded.
        let mut symbols: Vec<u8> = V2_CODE.iter().map(|(_, symbol)| *symbol).collect();
        symbols.sort_unstable();
        assert_eq!(symbols, (0..14).collect::<Vec<u8>>());
    }

    #[test]
    fn the_v2_code_decodes_its_own_symbols() {
        let huffman = V2Huffman::new();
        // Symbol 6 has the two-bit code 01 and takes six value bits.
        // JPEG's categories put the positive half at the top: 0b111111
        // is 63, the largest a six-bit difference reaches.
        let mut pump = BitPumpMsb::new(&[0b01_111111, 0b00_000000]);
        assert_eq!(huffman.diff(&mut pump), 63);
        // The bottom half is negative: 0b011111 is one below the
        // halfway mark, so -32.
        let mut pump = BitPumpMsb::new(&[0b01_011111, 0b00_000000]);
        assert_eq!(huffman.diff(&mut pump), -32);
        // Symbol 5 has code 10 and takes five bits; 0b00000 is the
        // bottom of the negative half, -31.
        let mut pump = BitPumpMsb::new(&[0b1000_0000]);
        assert_eq!(huffman.diff(&mut pump), -31);
        // Symbol 0 (code 110100) is a difference of zero and reads no
        // value bits at all.
        let mut pump = BitPumpMsb::new(&[0b1101_0011, 0b0100_0000]);
        assert_eq!(huffman.diff(&mut pump), 0);
        assert_eq!(huffman.diff(&mut pump), 0);
    }

    #[test]
    fn v2_differences_run_along_rows() {
        // Six pixels of two rows, every difference zero except the
        // four seeds. Symbol 3 is the code 1100 with three value bits,
        // whose positive half is 4..=7; symbol 0, the code 110100, is
        // a difference of zero and reads nothing after it.
        fn seed(bits: &mut BitWriter, value: u32) {
            bits.put(0b1100, 4);
            bits.put(value, 3);
        }
        let mut bits = BitWriter::default();
        for row in [[4, 5], [6, 7]] {
            seed(&mut bits, row[0]);
            seed(&mut bits, row[1]);
            for _ in 0..4 {
                bits.put(0b110100, 6);
            }
        }
        let out = decompress_v2(&bits.finish(), 6, 2);
        assert_eq!(out, vec![4, 5, 4, 5, 4, 5, 6, 7, 6, 7, 6, 7]);
    }

    // ------------------------------------------------------- codec v3

    /// Writes bits MSB-first into 32-bit little-endian words, the
    /// order [`BitPumpMsb32`] reads them back in.
    #[derive(Default)]
    struct Words {
        out: Vec<u8>,
        cache: u64,
        bits: u32,
    }

    impl Words {
        fn put(&mut self, value: u32, bits: u32) {
            self.cache = (self.cache << bits) | (value & ((1u64 << bits) - 1) as u32) as u64;
            self.bits += bits;
            while self.bits >= 32 {
                self.bits -= 32;
                let word = (self.cache >> self.bits) as u32;
                self.out.extend_from_slice(&word.to_le_bytes());
            }
        }
        fn finish(mut self) -> Vec<u8> {
            if self.bits > 0 {
                let pad = 32 - self.bits;
                self.put(0, pad);
            }
            self.out
        }
    }

    /// The sixteen header bytes of the two bodies in the corpus. Ten
    /// of the sixteen have no known meaning, but the three that do are
    /// enough to catch a reader that takes the words the wrong way up.
    #[test]
    fn the_v3_header_is_read_out_of_little_endian_words() {
        let nx1 = v3_header(&[
            0x4B, 0x6D, 0x33, 0x56, 0xF0, 0x10, 0x60, 0x19, 0x00, 0x05, 0x00, 0x00, 0x80, 0x00,
            0x02, 0x01,
        ]);
        assert_eq!(
            nx1,
            V3Header {
                depth: 14,
                // Lengths always present, and lossless.
                options: v3::LENGTHS_ALWAYS | v3::LOSSLESS,
                initial: 128,
            }
        );
        let nx500 = v3_header(&[
            0x45, 0x6D, 0x33, 0x56, 0xF0, 0x10, 0x60, 0x19, 0x00, 0x00, 0x00, 0x00, 0x80, 0x00,
            0x02, 0x01,
        ]);
        assert_eq!(
            nx500,
            V3Header {
                depth: 14,
                options: 0,
                initial: 128,
            }
        );
    }

    /// The four length selectors, resolved against the previous
    /// block's lengths. This is the NX1's row 0, block at column 32:
    /// flags 0, 3, 0, 1 over the lengths 10, 9, 9, 8 that its block at
    /// column 16 left behind.
    #[test]
    fn v3_lengths_carry_from_the_previous_block() {
        let mut bits = Words::default();
        bits.put(0, 2);
        bits.put(3, 2);
        bits.put(0, 2);
        bits.put(1, 2);
        // The one literal, for the group whose flag was 3. All four
        // selectors are read before any literal.
        bits.put(11, 4);
        let stream = bits.finish();
        let mut pump = BitPumpMsb32::new(&stream);
        let mut lengths = [10, 9, 9, 8];
        v3_lengths(&mut pump, &mut lengths);
        assert_eq!(lengths, [10, 11, 9, 9]);
        assert_eq!(pump.position(), 12);
    }

    /// The interleaved order the sixteen differences arrive in.
    #[test]
    fn v3_differences_are_interleaved_by_column_parity() {
        let even: Vec<usize> = (0..16).map(|i| v3_target(0, 16, i)).collect();
        assert_eq!(
            even,
            vec![16, 18, 20, 22, 24, 26, 28, 30, 17, 19, 21, 23, 25, 27, 29, 31]
        );
        let odd: Vec<usize> = (0..16).map(|i| v3_target(1, 16, i)).collect();
        assert_eq!(
            odd,
            vec![17, 19, 21, 23, 25, 27, 29, 31, 16, 18, 20, 22, 24, 26, 28, 30]
        );
    }

    /// Mode 7 predicts two columns back, sequentially, so a whole
    /// block takes the previous block's last even and last odd column.
    /// The first block of a row takes the header's initial value.
    #[test]
    fn v3_mode_seven_predicts_along_the_row() {
        let width = 32;
        let mut frame = vec![0u16; width * 3];
        v3_predict(&mut frame, 0, 0, width, 7, 128);
        assert!(frame[..16].iter().all(|s| *s == 128));
        // Give the first block the NX1's real row 0 and predict the
        // second: every even offset becomes 4333, every odd 2362.
        frame[14] = 4333;
        frame[15] = 2362;
        v3_predict(&mut frame, 0, 16, width, 7, 128);
        for (i, sample) in frame[16..32].iter().enumerate() {
            assert_eq!(*sample, if i.is_multiple_of(2) { 4333 } else { 2362 });
        }
    }

    /// Modes 0..=6 take their reference from the row above for the
    /// diagonal (green) neighbours and from two rows above for the
    /// ones straight up, then slide it sideways. Mode 3 is the
    /// no-slide, no-average case; mode 4 averages two references two
    /// columns apart.
    #[test]
    fn v3_motion_prediction_reads_the_two_rows_above() {
        let width = 32;
        let mut frame = vec![0u16; width * 3];
        for c in 0..width {
            frame[c] = 100 + c as u16;
            frame[width + c] = 200 + c as u16;
        }
        v3_predict(&mut frame, 2, 0, width, 3, 0);
        // Row 2, column 0 comes from row 1 column 1; column 1 from row
        // 0 column 1; column 2 from row 1 column 3; and so on.
        assert_eq!(
            &frame[2 * width..2 * width + 6],
            &[201, 101, 203, 103, 205, 105]
        );
        v3_predict(&mut frame, 2, 0, width, 4, 0);
        // Averaging takes the reference and the one two columns on.
        // (201 + 203 + 1) >> 1 and so on: the reference and the one
        // two columns further along.
        assert_eq!(&frame[2 * width..2 * width + 4], &[202, 102, 204, 104]);
    }

    /// The dequantiser, and its mid-point offset. A group whose length
    /// resolves to zero reads no bits, but still picks up the offset:
    /// the difference is `scale`, not zero.
    #[test]
    fn v3_dequantises_around_the_interval_midpoint() {
        let dequantise = |q: i32, scale: i32| q * (2 * scale + 1) + scale;
        // The NX500's first difference: 3364 = 3 * 1121 + 1.
        assert_eq!(dequantise(1121, 1), 3364);
        // Lossless files leave it the identity.
        assert_eq!(dequantise(-97, 0), -97);
        assert_eq!(dequantise(0, 3), 3);
    }

    /// A width narrower than a block, and a stream with no room for a
    /// header, are refused rather than decoded into nonsense.
    #[test]
    fn v3_refuses_frames_it_cannot_describe() {
        assert!(decompress_v3(&[0; 4096], 640, 480).is_err());
        assert!(decompress_v3(&[0; 8], 6496, 4336).is_err());
        // An all-zero header claims one bit a sample.
        assert!(decompress_v3(&vec![0u8; 1 << 16], V3_MIN_WIDTH, V3_MIN_HEIGHT).is_err());
        // A real header over a stream of zeros: every field reads
        // zero, which is a legal stream, and nothing panics.
        let mut stream = vec![
            0x4B, 0x6D, 0x33, 0x56, 0xF0, 0x10, 0x60, 0x19, 0x00, 0x05, 0x00, 0x00, 0x80, 0x00,
            0x02, 0x01,
        ];
        stream.resize(1 << 16, 0);
        let frame = decompress_v3(&stream, V3_MIN_WIDTH, V3_MIN_HEIGHT).unwrap();
        assert_eq!(frame.len(), V3_MIN_WIDTH * V3_MIN_HEIGHT);
    }

    #[test]
    fn packed_reads_both_bit_orders() {
        // 0x012 and 0x345 packed twelve bits at a time.
        let msb = packed(&[0x01, 0x23, 0x45], 2, 1, 12, true);
        assert_eq!(msb, vec![0x012, 0x345]);
        // The same two samples the other way up: 0x012 first means the
        // low nibble of byte 1 carries the top of the sample.
        let lsb = packed(&[0x12, 0x50, 0x34], 2, 1, 12, false);
        assert_eq!(lsb, vec![0x012, 0x345]);
    }

    #[test]
    fn plain_words_are_little_endian() {
        assert_eq!(plain(&[0x34, 0x12, 0x78, 0x56], 2), vec![0x1234, 0x5678]);
        // A short strip leaves the rest of the frame black.
        assert_eq!(plain(&[0x34], 2), vec![0, 0]);
    }

    #[test]
    fn truncated_streams_do_not_panic() {
        for len in [0usize, 1, 3, 7, 16, 64] {
            assert_eq!(decompress_v2(&vec![0xa5; len], 32, 4).len(), 128);
            let pointers = vec![0u32; 4];
            assert_eq!(decompress_v1(&vec![0x5a; len], &pointers, 32, 4).len(), 128);
            assert_eq!(packed(&vec![0xff; len], 32, 4, 12, true).len(), 128);
        }
        // Row pointers past the end of the strip must not read wildly.
        let pointers = vec![u32::MAX; 4];
        assert_eq!(decompress_v1(&[0; 64], &pointers, 32, 4).len(), 128);
    }

    #[test]
    fn by_position_follows_the_filter_array() {
        let values = [1.0, 2.0, 3.0, 4.0];
        assert_eq!(by_position(&Cfa::RGGB, values), [1.0, 2.0, 3.0, 4.0]);
        assert_eq!(by_position(&Cfa::GRBG, values), [2.0, 1.0, 4.0, 3.0]);
        assert_eq!(by_position(&Cfa::GBRG, values), [2.0, 4.0, 1.0, 3.0]);
    }

    #[derive(Default)]
    struct BitWriter {
        out: Vec<u8>,
        cache: u32,
        bits: u32,
    }

    impl BitWriter {
        fn put(&mut self, value: u32, bits: u32) {
            self.cache = (self.cache << bits) | (value & ((1 << bits) - 1));
            self.bits += bits;
            while self.bits >= 8 {
                self.bits -= 8;
                self.out.push((self.cache >> self.bits) as u8);
            }
        }
        fn finish(mut self) -> Vec<u8> {
            if self.bits > 0 {
                self.out.push((self.cache << (8 - self.bits)) as u8);
            }
            self.out
        }
    }

    // ------------------------------------------------------------ corpus

    /// Files this decoder knowingly cannot read, with the reason. A
    /// corpus file that is `Unsupported` for any other reason fails.
    /// Empty: every Samsung compression in the corpus decodes. The
    /// list stays for the compressions a future body might bring.
    const UNSUPPORTED: &[&str] = &[];

    fn corpus() -> Vec<PathBuf> {
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
                    .is_some_and(|e| e.eq_ignore_ascii_case("srw"))
                {
                    out.push(path);
                }
            }
        }
        out.sort();
        out
    }

    fn oracle(path: &Path) -> Option<(usize, usize, Vec<u16>)> {
        let mut name = path.as_os_str().to_os_string();
        name.push(".tiff");
        let path = PathBuf::from(name);
        if !path.exists() {
            return None;
        }
        let image = image::open(&path).expect("oracle TIFF loads").into_luma16();
        let (width, height) = (image.width() as usize, image.height() as usize);
        Some((width, height, image.into_raw()))
    }

    fn identify(path: &Path) -> Option<String> {
        let mut name = path.as_os_str().to_os_string();
        name.push(".identify.txt");
        std::fs::read_to_string(PathBuf::from(name)).ok()
    }

    fn field<'a>(report: &'a str, key: &str) -> Option<&'a str> {
        report
            .lines()
            .find_map(|line| line.trim_start().strip_prefix(key))
            .map(str::trim)
    }

    fn numbers(report: &str, key: &str) -> Vec<f32> {
        field(report, key)
            .map(|rest| {
                rest.split(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-'))
                    .filter(|t| !t.is_empty())
                    .filter_map(|t| t.parse().ok())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn cfa_of(report: &str) -> Option<Cfa> {
        let pattern = field(report, "Filter pattern:")?;
        let colors: Vec<CfaColor> = pattern
            .chars()
            .take(4)
            .map(|c| match c {
                'R' => CfaColor::Red,
                'B' => CfaColor::Blue,
                _ => CfaColor::Green,
            })
            .collect();
        Some(Cfa::Bayer(colors.try_into().ok()?))
    }

    /// A corpus file by name, with its decoded frame.
    fn decoded(name: &str) -> Option<RawImage> {
        let path = corpus()
            .into_iter()
            .find(|p| p.file_name().is_some_and(|f| f == name))?;
        let bytes = std::fs::read(path).expect("corpus file reads");
        Some(crate::decode(&bytes).expect("decodes"))
    }

    fn samples(raw: &RawImage) -> &[u16] {
        let RawData::U16(data) = &raw.data else {
            panic!("SRW frames are integers")
        };
        data
    }

    /// The NX1: option flags 5, so the lengths are in every block and
    /// the file is lossless. Its first row exercises mode 7 from the
    /// header's initial value, four 4-bit length literals and the
    /// interleaved difference order in one block.
    #[test]
    fn the_nx1_decodes_its_first_rows_sample_for_sample() {
        let Some(raw) = decoded("NX1-sam_9364.srw") else {
            return;
        };
        assert_eq!((raw.width, raw.height), (6496, 4336));
        assert_eq!(raw.cfa, Cfa::GRBG);
        assert_eq!(raw.white_level, 16383.0);
        let frame = samples(&raw);
        let width = raw.width;
        assert_eq!(
            &frame[..16],
            &[
                4524, 2306, 4662, 2339, 4504, 2464, 4839, 2432, 4384, 2344, 4423, 2373, 4353, 2218,
                4333, 2362
            ]
        );
        // The second block's predictions are the first's last even and
        // last odd column, 4333 and 2362.
        assert_eq!(
            &[frame[16], frame[18], frame[20], frame[22]],
            &[4230, 4309, 4604, 4501]
        );

        // Row 2 is the first that can use the motion modes; its first
        // block chooses mode 3 — no slide, no averaging — so column 0
        // predicts from row 1 column 1 and column 1 from row 0
        // column 1.
        assert_eq!(&[frame[width + 1], frame[width + 3]], &[4495, 4458]);
        assert_eq!(&[frame[1], frame[3]], &[2306, 2339]);
        let row2 = &frame[2 * width..];
        assert_eq!(
            &[row2[0], row2[2], row2[4], row2[6]],
            &[4904, 4453, 4666, 4558]
        );
    }

    /// The NX500: option flags 0, so every field is present and the
    /// file is lossy. Its row 0 opens with a scale selector reading 3
    /// and a twelve-bit literal of 1, so every difference on that row
    /// is `3q + 1`.
    #[test]
    fn the_nx500_decodes_its_lossy_first_rows_sample_for_sample() {
        let Some(raw) = decoded("NX500-SAM_2922.SRW") else {
            return;
        };
        assert_eq!((raw.width, raw.height), (6496, 4336));
        let frame = samples(&raw);
        let width = raw.width;
        assert_eq!(
            &frame[..16],
            &[
                3492, 1695, 3510, 1668, 3531, 1620, 3495, 1680, 3531, 1728, 3504, 1728, 3537, 1641,
                3477, 1725
            ]
        );
        // Row 2's first block uses mode 4: offset zero and an average
        // of two references two columns apart. Its predictions are
        // therefore these averages of rows 0 and 1.
        let average = |a: u16, b: u16| (a as u32 + b as u32 + 1) >> 1;
        assert_eq!(average(frame[width + 1], frame[width + 3]), 3482);
        assert_eq!(average(frame[1], frame[3]), 1682);
        assert_eq!(average(frame[width + 3], frame[width + 5]), 3440);
        assert_eq!(average(frame[3], frame[5]), 1644);
        assert_eq!(average(frame[width + 5], frame[width + 7]), 3446);
        assert_eq!(average(frame[5], frame[7]), 1650);
        assert_eq!(average(frame[width + 7], frame[width + 9]), 3506);
        assert_eq!(average(frame[7], frame[9]), 1704);
        let row2 = &frame[2 * width..];
        assert_eq!(
            &[row2[0], row2[2], row2[4], row2[6]],
            &[3546, 3489, 3450, 3450]
        );
        // The clamp: no sample may leave the fourteen-bit range, and
        // this frame does reach the top of it.
        assert_eq!(frame.iter().copied().max(), Some(16383));
    }

    #[test]
    fn corpus_matches_the_oracle() {
        let files = corpus();
        let mut matched = 0;
        let mut skipped = 0;
        for path in &files {
            let bytes = std::fs::read(path).expect("corpus file reads");
            assert_eq!(
                crate::probe(&bytes),
                Some(Format::Srw),
                "{} did not probe as SRW",
                path.display()
            );
            let raw = match crate::decode(&bytes) {
                Ok(raw) => raw,
                Err(Error::Unsupported(why)) if UNSUPPORTED.iter().any(|a| why.contains(a)) => {
                    skipped += 1;
                    // Even a frame we cannot decode must give up its
                    // preview without a fuss.
                    let preview = preview(&bytes).expect("preview of an unsupported SRW");
                    assert!(preview.is_some(), "{}: no preview", path.display());
                    continue;
                }
                Err(error) => panic!("{}: {error}", path.display()),
            };
            raw.validate().expect("decoded frame is self-consistent");
            matched += 1;

            if let Some((width, height, expected)) = oracle(path) {
                assert_eq!(
                    (raw.width, raw.height),
                    (width, height),
                    "{}: frame size",
                    path.display()
                );
                let RawData::U16(got) = &raw.data else {
                    panic!("{}: SRW frames are integers", path.display())
                };
                let wrong: Vec<usize> = got
                    .iter()
                    .zip(&expected)
                    .enumerate()
                    .filter(|(_, (a, b))| a != b)
                    .map(|(i, _)| i)
                    .collect();
                assert!(
                    wrong.is_empty(),
                    "{}: {} of {} samples differ from the oracle; first: {:?}",
                    path.display(),
                    wrong.len(),
                    got.len(),
                    wrong
                        .iter()
                        .take(4)
                        .map(|i| (i % width, i / width, got[*i], expected[*i]))
                        .collect::<Vec<_>>()
                );
            }

            if let Some(report) = identify(path) {
                let size = numbers(&report, "Full size:");
                if let [width, height] = size[..] {
                    assert_eq!(
                        (raw.width, raw.height),
                        (width as usize, height as usize),
                        "{}: full size",
                        path.display()
                    );
                }
                // LibRaw names the pattern at its own crop origin,
                // which on several Samsung bodies is an odd column in.
                // Ours is the file's CFAPattern at the frame origin,
                // so one of the four phases of ours must be LibRaw's.
                // Strictly: the pattern at the crop origin must be the
                // one LibRaw names at its crop origin. Any phase of
                // ours would pass for any Bayer pattern. LibRaw rounds
                // an odd origin (the NX10's column 21) up to even to
                // keep the phase, so compare there.
                if let Some(theirs) = cfa_of(&report) {
                    assert_eq!(
                        raw.cfa.shifted(
                            raw.crop.x.next_multiple_of(2),
                            raw.crop.y.next_multiple_of(2)
                        ),
                        theirs,
                        "{}: filter pattern at the crop origin disagrees with LibRaw",
                        path.display()
                    );
                }
                let black = numbers(&report, "cblack[0 .. 3]:");
                if let [r, g, b, g2] = black[..] {
                    let expected = by_position(&raw.cfa, [r, g, g2, b]);
                    assert_eq!(raw.black_levels, expected, "{}: black", path.display());
                }
                let shot = numbers(&report, "As shot");
                if let [r, g, b, g2] = shot[..4.min(shot.len())] {
                    let want = [r / g, 1.0, b / g, g2 / g];
                    for (got, want) in raw.wb_coeffs.iter().zip(want) {
                        assert!(
                            (got - want).abs() < 1e-4,
                            "{}: white balance {:?} not {:?}",
                            path.display(),
                            raw.wb_coeffs,
                            want
                        );
                    }
                }
                if let Some(flip) = numbers(&report, "Image flip:").first() {
                    let expected = match *flip as i32 {
                        3 => crate::Orientation::Rotate180,
                        5 => crate::Orientation::Rotate270CW,
                        6 => crate::Orientation::Rotate90CW,
                        _ => crate::Orientation::Normal,
                    };
                    assert_eq!(raw.orientation, expected, "{}: flip", path.display());
                }
            }

            assert!(raw.crop.x + raw.crop.width <= raw.width);
            assert!(raw.crop.y + raw.crop.height <= raw.height);
            let preview = raw.preview.as_ref().expect("every SRW carries a preview");
            image::load_from_memory(preview).expect("preview decodes");
        }
        eprintln!("srw: {matched} corpus files matched, {skipped} unsupported");
    }

    #[test]
    fn truncated_files_do_not_panic() {
        for path in corpus() {
            let bytes = std::fs::read(&path).expect("corpus file reads");
            for numerator in [0usize, 1, 2, 3, 5, 8, 13, 100, 500, 900, 999] {
                let cut = bytes.len() * numerator / 1000;
                let _ = crate::decode(&bytes[..cut]);
                let _ = preview(&bytes[..cut]);
            }
        }
    }
}
