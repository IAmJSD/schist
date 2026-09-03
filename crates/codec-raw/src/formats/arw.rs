//! Sony ARW, SR2 and SRF.
//!
//! Sony has shipped five different ways of storing a frame in what is
//! otherwise an ordinary TIFF, and the container barely tells them
//! apart: `Compression` is 1 or the private 32767 for four of them.
//! What actually decides is how many bytes the strip holds per pixel:
//!
//! | bytes/pixel | variant                                        |
//! |-------------|------------------------------------------------|
//! | 1.0         | ARW 2.x "compressed": 16 pixels per 16 bytes    |
//! | 1.5         | 12 bits packed two pixels to three bytes        |
//! | 2.0         | one 16-bit word a pixel (12- or 14-bit data)    |
//!
//! plus `Compression` 7, which is Sony's "lossless compressed" (ARW
//! 4.x): 512x512 tiles of plain lossless JPEG. The two oddities that
//! predate all of that are the DSLR-A100's ARW 1.0, a column-major
//! DPCM with a fixed Huffman table inherited from Minolta, and the
//! DSC-F828's SRF, whose sensor data is encrypted with Sony's LFSR
//! stream cipher.
//!
//! The metadata a developer needs — black level, as-shot white
//! balance, the sensor's active area — is not in the TIFF either. It
//! lives in an IFD that `DNGPrivateData` (0xC634) points at, whose
//! 0x7200/0x7201/0x7221 triple gives the offset, length and key of a
//! second IFD encrypted with the same LFSR. [`sr2_metadata`] decrypts
//! and reads it.
//!
//! Clean-room: written from observation of the sample files against
//! LibRaw's `unprocessed_raw` output, from ExifTool's published tag
//! tables, and from the openly documented Sony LFSR. No copyleft
//! decoder's source was consulted.

use rayon::prelude::*;

use crate::bits::{BitPump, BitPumpMsb};
use crate::formats::common;
use crate::tiff::{tags, Entry, Ifd, ImageLayout, Tiff};
use crate::{Cfa, CfaColor, Error, Format, RawData, RawImage, Rect, Result};

/// The Sony-private tags, in the two directories they live in.
mod stags {
    /// In the raw SubIFD: 0 uncompressed, 2 lossy, 4 lossless.
    pub const RAW_FILE_TYPE: u16 = 0x7000;
    /// Four 16-bit knee points defining the curve that expands an
    /// ARW 2.x 11-bit sample to 14 bits.
    pub const TONE_CURVE: u16 = 0x7010;
    /// In the SR2Private IFD.
    pub const SR2_SUBIFD_OFFSET: u16 = 0x7200;
    pub const SR2_SUBIFD_LENGTH: u16 = 0x7201;
    pub const SR2_SUBIFD_KEY: u16 = 0x7221;
    /// In the decrypted SR2SubIFD.
    pub const BLACK_LEVEL_2: u16 = 0x7300;
    pub const WB_GRBG_LEVELS: u16 = 0x7303;
    pub const BLACK_LEVEL: u16 = 0x7310;
    pub const WB_RGGB_LEVELS: u16 = 0x7313;
    /// left, top, right, bottom of the active area, in sensor pixels.
    pub const CROP_RECT: u16 = 0x74C3;

    pub const DNG_PRIVATE_DATA: u16 = 0xC634;
    pub const DEFAULT_CROP_ORIGIN: u16 = 0xC61F;
    pub const DEFAULT_CROP_SIZE: u16 = 0xC620;
}

/// Sony's private `Compression` value, used for both the block-packed
/// "compressed" raw and for plainly packed or word-aligned data.
const COMPRESSION_SONY: u32 = 32767;

/// How the sensor data is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Variant {
    /// ARW 2.x: 16 same-colour pixels per 16 bytes, through the tone
    /// curve to 14 bits.
    Blocks,
    /// 12 bits a pixel, two pixels to three bytes, low bits first.
    Packed12,
    /// One 16-bit word a pixel. Which end comes first is decided from
    /// the samples, not from a model list: ARW writes them
    /// little-endian, the DSC-R1's SR2 and the DSC-F828's SRF
    /// big-endian.
    Words,
    /// ARW 4.x lossless: 512x512 tiles of lossless JPEG.
    LosslessTiles,
}

pub fn decode(bytes: &[u8]) -> Result<RawImage> {
    let tiff = Tiff::parse(bytes)?;
    let (make, model) = tiff.make_model();

    // The A100 is a rebadged Minolta and stores nothing the way any
    // later Sony does: its "SubIFDs" tag is a bare data offset and its
    // sizes come from an embedded MRW block, so it gets its own path.
    if let Some(mrw) = minolta_block(&tiff) {
        return decode_arw1(&tiff, &mrw, &make, &model);
    }

    let ifd = raw_ifd(&tiff)
        .ok_or_else(|| Error::Unsupported("arw: no sensor IFD in this file".into()))?;
    let layout = sony_layout(&tiff, ifd)?;
    let (width, height) = (layout.width, layout.height);
    if width == 0 || height == 0 || width > 1 << 16 || height > 1 << 16 {
        return Err(Error::Corrupt(format!(
            "arw: implausible frame {width}x{height}"
        )));
    }
    let pixels = width * height;

    let variant = variant_of(ifd, &layout)?;
    let data = match variant {
        Variant::LosslessTiles => decode_lossless_tiles(bytes, &layout)?,
        Variant::Blocks => {
            let curve = tone_curve(ifd)?;
            decode_blocks(single_chunk(&tiff, &layout, pixels)?, width, height, &curve)?
        }
        Variant::Packed12 => decode_packed12(single_chunk(&tiff, &layout, pixels * 3 / 2)?, pixels),
        Variant::Words => {
            let chunk = single_chunk(&tiff, &layout, pixels * 2)?;
            // SRF hides the sensor behind the LFSR; SR2 and ARW do
            // not, and the byte order can only be judged once the
            // samples are in the clear.
            let plain = if tiff.root().has(stags::DNG_PRIVATE_DATA) {
                std::borrow::Cow::Borrowed(chunk)
            } else {
                match srf_data_key(&tiff, bytes) {
                    Some(key) => std::borrow::Cow::Owned(sony_decrypt(chunk, key)),
                    // Decoding the ciphertext as samples would hand
                    // back noise with no error.
                    None => {
                        return Err(Error::Unsupported(
                            "SRF whose sensor-data key could not be found".into(),
                        ))
                    }
                }
            };
            let little_endian = words_look_little_endian(&plain, tiff.little_endian());
            decode_words(&plain, pixels, little_endian)
        }
    };
    if data.len() != pixels {
        return Err(Error::Corrupt(format!(
            "arw: decoded {} samples for {width}x{height}",
            data.len()
        )));
    }

    let cfa = cfa_of(ifd, &make, &model);
    let mut raw = RawImage::new(Format::Arw, width, height, 1, RawData::U16(data), cfa);
    raw.set_camera(&make, &model);
    raw.orientation = common::orientation(&tiff);
    raw.preview = common::largest_jpeg(&tiff);
    raw.metadata = common::metadata(&tiff);

    // Everything below is 14-bit-scaled but the 12-bit packed frames,
    // which LibRaw leaves at the sensor's own scale; the levels the
    // camera records are always on the 14-bit one, so they shift too.
    let shift = if variant == Variant::Packed12 { 2 } else { 0 };
    raw.white_level = ((16383u32 >> shift) as f32).max(1.0);
    let meta = sr2_metadata(&tiff, bytes);
    if let Some(black) = meta.as_ref().and_then(|m| m.black) {
        raw.black_levels = black.map(|b| (b >> shift) as f32);
    }
    if let Some(wb) = meta.as_ref().and_then(|m| m.wb) {
        raw.wb_coeffs = wb;
    }
    raw.crop = crop_of(ifd, meta.as_ref(), width, height);
    raw.apply_camera_table();
    Ok(raw)
}

pub fn preview(bytes: &[u8]) -> Result<Option<Vec<u8>>> {
    Ok(common::largest_jpeg(&Tiff::parse(bytes)?))
}

// ---------------------------------------------------------------- //
// Picking the sensor IFD and working out how it is stored.
// ---------------------------------------------------------------- //

/// A four-byte value Sony writes as `BYTE[4]` where a LONG was meant.
///
/// `DNGPrivateData` (0xC634) is an offset and the SR2SubIFD key
/// (0x7221) is a 32-bit number, but both are typed as undifferentiated
/// bytes; read that way, `Entry::u32(0)` would hand back the first byte
/// alone. Anything already typed as an integer is taken as it stands.
fn long_of(entry: &Entry, little_endian: bool) -> Option<u32> {
    match entry.bytes() {
        Some(bytes) if bytes.len() >= 4 => {
            let word: [u8; 4] = bytes[..4].try_into().ok()?;
            Some(if little_endian {
                u32::from_le_bytes(word)
            } else {
                u32::from_be_bytes(word)
            })
        }
        _ => entry.u32(0),
    }
}

/// The IFD holding sensor samples.
///
/// Every ARW and SR2 marks it with PhotometricInterpretation 32803
/// (CFA). The DSC-F828's SRF marks nothing: its IFD0 claims to be an
/// RGB image of three 14-bit samples, so the fallback takes the only
/// directory deeper than eight bits that is not a JPEG.
fn raw_ifd<'a>(tiff: &'a Tiff<'_>) -> Option<&'a Ifd> {
    let cfa = tiff
        .all()
        .into_iter()
        .find(|ifd| ifd.get(tags::PHOTOMETRIC).and_then(|e| e.u32(0)) == Some(32803));
    cfa.or_else(|| {
        tiff.all().into_iter().find(|ifd| {
            let deep = ifd
                .get(tags::BITS_PER_SAMPLE)
                .and_then(|e| e.u32(0))
                .unwrap_or(8)
                > 8;
            let compression = ifd
                .get(tags::COMPRESSION)
                .and_then(|e| e.u32(0))
                .unwrap_or(1);
            deep && !matches!(compression, 6 | 7)
        })
    })
}

/// Strips, tiles and byte counts, with SRF's missing `StripOffsets`
/// filled in.
///
/// The F828 records `StripByteCounts` and `RowsPerStrip` but no
/// offset at all: its sensor data is simply the tail of the file.
fn sony_layout(tiff: &Tiff<'_>, ifd: &Ifd) -> Result<ImageLayout> {
    // Only the missing-offset case is ours to patch up: a strip that
    // runs off the end of a truncated file must still be an error.
    if ifd.has(tags::STRIP_OFFSETS) || ifd.has(tags::TILE_OFFSETS) {
        return ImageLayout::of(tiff, ifd);
    }
    let mut layout = ImageLayout {
        width: ifd
            .get(tags::IMAGE_WIDTH)
            .and_then(|e| e.u32(0))
            .ok_or_else(|| Error::Corrupt("arw: sensor IFD without ImageWidth".into()))?
            as usize,
        height: ifd
            .get(tags::IMAGE_LENGTH)
            .and_then(|e| e.u32(0))
            .ok_or_else(|| Error::Corrupt("arw: sensor IFD without ImageLength".into()))?
            as usize,
        bits_per_sample: ifd
            .get(tags::BITS_PER_SAMPLE)
            .and_then(|e| e.u32(0))
            .unwrap_or(8),
        samples_per_pixel: 1,
        compression: ifd
            .get(tags::COMPRESSION)
            .and_then(|e| e.u32(0))
            .unwrap_or(1),
        photometric: ifd
            .get(tags::PHOTOMETRIC)
            .and_then(|e| e.u32(0))
            .unwrap_or(1),
        chunks: Vec::new(),
        tile: None,
        rows_per_chunk: 0,
    };
    let len = ifd
        .get(tags::STRIP_BYTE_COUNTS)
        .and_then(|e| e.u64(0))
        .and_then(|n| usize::try_from(n).ok())
        .ok_or_else(|| {
            Error::Corrupt("arw: sensor IFD with neither strip offsets nor byte counts".into())
        })?;
    let start = tiff
        .bytes()
        .len()
        .checked_sub(len)
        .ok_or_else(|| Error::Corrupt("arw: strip longer than the file".into()))?;
    layout.rows_per_chunk = layout.height;
    layout.chunks = vec![(start, len)];
    Ok(layout)
}

/// Which of the five storage schemes this IFD uses.
fn variant_of(ifd: &Ifd, layout: &ImageLayout) -> Result<Variant> {
    if layout.compression == 7 || layout.tile.is_some() {
        return Ok(Variant::LosslessTiles);
    }
    if !matches!(layout.compression, 1 | COMPRESSION_SONY) {
        return Err(Error::Unsupported(format!(
            "arw: compression {} is not a Sony raw scheme",
            layout.compression
        )));
    }
    let pixels = layout.width * layout.height;
    let bytes: usize = layout.chunks.iter().map(|(_, len)| *len).sum();
    // The strip is always exactly one of these three sizes; anything
    // else is a variant nobody has shipped, or a truncated file.
    if bytes == pixels {
        return Ok(Variant::Blocks);
    }
    if bytes == pixels / 2 * 3 && pixels.is_multiple_of(2) {
        return Ok(Variant::Packed12);
    }
    if bytes == pixels * 2 {
        return Ok(Variant::Words);
    }
    Err(Error::Unsupported(format!(
        "arw: {bytes} bytes for {}x{} pixels is no known Sony layout (compression {}, {} bits, raw type {:?})",
        layout.width,
        layout.height,
        layout.compression,
        layout.bits_per_sample,
        ifd.get(stags::RAW_FILE_TYPE).and_then(|e| e.u32(0))
    )))
}

/// Whether word-per-pixel data is little- or big-endian.
///
/// ARW writes native (little-endian) words; the DSC-R1's SR2 and the
/// DSC-F828's SRF write big-endian ones inside a little-endian TIFF.
/// Rather than keep a list of models, decide from the samples: no Sony
/// sensor is deeper than fourteen bits, so the wrong byte order shows
/// up at once as a flood of values with the top two bits set.
fn words_look_little_endian(chunk: &[u8], default: bool) -> bool {
    // Words from all over the frame, not one stretch of it: a flat
    // band (a dark frame, a lens-cap shot) says nothing either way,
    // and masked first rows read as near-zero whichever way round
    // they are taken. A tie falls back to the container's own byte
    // order rather than to a coin.
    let words = chunk.as_chunks::<2>().0;
    let step = (words.len() / (1 << 16)).max(1);
    let (mut little_over, mut big_over) = (0usize, 0usize);
    for word in words.iter().step_by(step) {
        if u16::from_le_bytes(*word) > 0x3fff {
            little_over += 1;
        }
        if u16::from_be_bytes(*word) > 0x3fff {
            big_over += 1;
        }
    }
    match little_over.cmp(&big_over) {
        std::cmp::Ordering::Less => true,
        std::cmp::Ordering::Greater => false,
        std::cmp::Ordering::Equal => default,
    }
}

/// The single strip of an untiled frame, checked against the size the
/// variant needs.
fn single_chunk<'a>(tiff: &Tiff<'a>, layout: &ImageLayout, want: usize) -> Result<&'a [u8]> {
    let (start, len) = *layout
        .chunks
        .first()
        .ok_or_else(|| Error::Corrupt("arw: sensor IFD with no strip".into()))?;
    if layout.chunks.len() > 1 {
        return Err(Error::Unsupported(
            "arw: sensor data split over several strips".into(),
        ));
    }
    if len < want {
        return Err(Error::Corrupt(format!(
            "arw: strip holds {len} bytes, needs {want}"
        )));
    }
    tiff.bytes()
        .get(start..start + want)
        .ok_or_else(|| Error::Corrupt("arw: strip lies outside the file".into()))
}

// ---------------------------------------------------------------- //
// ARW 2.x: sixteen pixels to sixteen bytes.
// ---------------------------------------------------------------- //

/// The 11-bit-to-14-bit expansion curve from tag 0x7010.
///
/// The tag holds four knee points. Divided by four they are indices
/// into a 12-bit domain; the curve rises by 1 per step below the
/// first, then by 2, 4, 8 and 16 in the segments between them, so the
/// camera spends its precision where the eye does. Sony's stored
/// values are the *inputs*: `[8000, 10400, 12900, 14100]` means knees
/// at 2000, 2600, 3225 and 3525.
fn tone_curve(ifd: &Ifd) -> Result<Vec<u16>> {
    let entry = ifd.get(stags::TONE_CURVE).ok_or_else(|| {
        Error::Unsupported("arw: compressed frame without a tone curve (tag 0x7010)".into())
    })?;
    let mut knees = [0usize; 6];
    knees[5] = 4095;
    for i in 0..4 {
        let v = entry
            .u32(i)
            .ok_or_else(|| Error::Corrupt("arw: tone curve with fewer than four knees".into()))?;
        knees[i + 1] = ((v >> 2) & 0xfff) as usize;
    }
    if knees.windows(2).any(|w| w[0] >= w[1]) {
        return Err(Error::Unsupported(format!(
            "arw: tone curve knees {:?} are not increasing",
            &knees[1..5]
        )));
    }
    let mut curve = vec![0u16; 4096];
    for segment in 0..5 {
        let step = 1u32 << segment;
        for j in knees[segment] + 1..=knees[segment + 1] {
            curve[j] = (curve[j - 1] as u32 + step).min(u16::MAX as u32) as u16;
        }
    }
    Ok(curve)
}

/// One 16-byte block: the brightest and darkest of sixteen pixels,
/// where they sit, and seven-bit deltas for the other fourteen.
///
/// The block is one 128-bit little-endian word. Bits 0..10 are the
/// maximum, 11..21 the minimum (both 11 bits), 22..25 and 26..29 the
/// indices of the pixel holding each, and the fourteen 7-bit deltas
/// run from bit 30. A delta is scaled by `1 << shift`, the smallest
/// shift that lets 127 steps span the block's range.
#[inline]
fn decode_block(block: u128, out: &mut [u16; 16]) {
    let max = (block & 0x7ff) as u16;
    let min = ((block >> 11) & 0x7ff) as u16;
    let imax = ((block >> 22) & 0xf) as usize;
    let imin = ((block >> 26) & 0xf) as usize;
    let mut shift = 0u32;
    while shift < 4 && (0x80u16 << shift) <= max.wrapping_sub(min) {
        shift += 1;
    }
    let mut delta = 0;
    for (i, pixel) in out.iter_mut().enumerate() {
        *pixel = if i == imax {
            max
        } else if i == imin {
            min
        } else {
            // A block naming the same slot for both extremes skips
            // one pixel instead of two, which would ask for a
            // fifteenth delta past the word's end; it is a broken
            // block, and reading the last delta again keeps it inert.
            let raw = ((block >> (30 + 7 * delta.min(13))) & 0x7f) as u16;
            delta += 1;
            (min + (raw << shift)).min(0x7ff)
        };
    }
}

/// A whole ARW 2.x frame.
///
/// The blocks are laid out along a row two colour streams at a time:
/// the first sixteen bytes hold the sixteen even columns 0..30, the
/// next sixteen the odd columns 1..31, then the next pair covers
/// columns 32..63, and so on. One block therefore never mixes red
/// with green, which is what makes a shared minimum and a 7-bit delta
/// enough.
fn decode_blocks(chunk: &[u8], width: usize, height: usize, curve: &[u16]) -> Result<Vec<u16>> {
    if !width.is_multiple_of(32) {
        return Err(Error::Unsupported(format!(
            "arw: compressed frame {width} wide is not a whole number of 32-pixel block pairs"
        )));
    }
    let mut out = vec![0u16; width * height];
    out.par_chunks_mut(width)
        .zip(chunk.par_chunks(width))
        .for_each(|(row, source)| {
            let mut pixels = [0u16; 16];
            for (block_index, word) in source.as_chunks::<16>().0.iter().enumerate() {
                decode_block(u128::from_le_bytes(*word), &mut pixels);
                let first = (block_index / 2) * 32 + (block_index % 2);
                for (i, pixel) in pixels.iter().enumerate() {
                    // The 11-bit sample indexes the curve at twice its
                    // value: the curve's domain is the 12-bit one the
                    // knee points are expressed in.
                    row[first + i * 2] = curve[(*pixel as usize) << 1];
                }
            }
        });
    Ok(out)
}

// ---------------------------------------------------------------- //
// The plain layouts.
// ---------------------------------------------------------------- //

/// Two pixels to three bytes, each pixel's low eight bits first.
fn decode_packed12(chunk: &[u8], pixels: usize) -> Vec<u16> {
    let mut out = vec![0u16; pixels];
    out.par_chunks_mut(2)
        .zip(chunk.par_chunks_exact(3))
        .for_each(|(pair, b)| {
            pair[0] = (b[0] as u16) | ((b[1] as u16 & 0x0f) << 8);
            if let Some(second) = pair.get_mut(1) {
                *second = ((b[1] as u16) >> 4) | ((b[2] as u16) << 4);
            }
        });
    out
}

/// One 16-bit word a pixel.
fn decode_words(chunk: &[u8], pixels: usize, little_endian: bool) -> Vec<u16> {
    let mut out = vec![0u16; pixels];
    out.par_iter_mut()
        .zip(chunk.par_chunks_exact(2))
        .for_each(|(pixel, b)| {
            *pixel = if little_endian {
                u16::from_le_bytes([b[0], b[1]])
            } else {
                u16::from_be_bytes([b[0], b[1]])
            };
        });
    out
}

// ---------------------------------------------------------------- //
// ARW 4.x lossless: tiles of lossless JPEG.
// ---------------------------------------------------------------- //

/// A tiled frame of lossless-JPEG tiles.
///
/// Each 512x512 sensor tile is a complete SOF3 stream of four
/// components at half the tile's dimensions: the four components are
/// the four positions of one Bayer quad, so component `c` of JPEG
/// pixel (x, y) is sensor pixel (2x + c%2, 2y + c/2). Frames are
/// padded out to whole tiles — an 8672x5784 sensor becomes 8704x6144 —
/// and the padding decodes with everything else.
fn decode_lossless_tiles(bytes: &[u8], layout: &ImageLayout) -> Result<Vec<u16>> {
    let (tile_width, tile_height) = layout
        .tile
        .filter(|(w, h)| *w > 0 && *h > 0)
        .ok_or_else(|| Error::Unsupported("arw: lossless frame without tile dimensions".into()))?;
    let across = layout.width.div_ceil(tile_width);
    let down = layout.height.div_ceil(tile_height);
    if across * down != layout.chunks.len() {
        return Err(Error::Corrupt(format!(
            "arw: {}x{} tiles of {tile_width}x{tile_height} but {} offsets",
            across,
            down,
            layout.chunks.len()
        )));
    }
    let width = layout.width;
    let mut out = vec![0u16; width * layout.height];
    // Tiles are independent streams; a row of them is a comfortable
    // unit of work (204 tiles on an A1, 247 on an A7R V).
    let rows: Vec<(usize, &mut [u16])> = out.chunks_mut(width * tile_height).enumerate().collect();
    rows.into_par_iter().try_for_each(|(tile_row, band)| {
        for tile_col in 0..across {
            let (start, len) = layout.chunks[tile_row * across + tile_col];
            let stream = bytes
                .get(start..start + len)
                .ok_or_else(|| Error::Corrupt("arw: tile lies outside the file".into()))?;
            let jpeg = crate::ljpeg::decode(stream)?;
            if jpeg.components != 4 {
                return Err(Error::Unsupported(format!(
                    "arw: lossless tile has {} components, expected four",
                    jpeg.components
                )));
            }
            if jpeg.width * 2 != tile_width || jpeg.height * 2 != tile_height {
                return Err(Error::Corrupt(format!(
                    "arw: lossless tile decodes {}x{} for a {tile_width}x{tile_height} tile",
                    jpeg.width, jpeg.height
                )));
            }
            let left = tile_col * tile_width;
            for y in 0..jpeg.height {
                for x in 0..jpeg.width {
                    let quad = &jpeg.data[(y * jpeg.width + x) * 4..][..4];
                    for (c, sample) in quad.iter().enumerate() {
                        let row = y * 2 + c / 2;
                        let col = left + x * 2 + c % 2;
                        if row * width + col < band.len() && col < width {
                            band[row * width + col] = *sample;
                        }
                    }
                }
            }
        }
        Ok(())
    })?;
    Ok(out)
}

// ---------------------------------------------------------------- //
// ARW 1.0: the DSLR-A100.
// ---------------------------------------------------------------- //

/// The A100's embedded Minolta block, when `DNGPrivateData` points at
/// one. Later Sonys point the same tag at an SR2Private IFD instead.
struct MinoltaBlock {
    /// Full sensor extent, from the PRD block.
    sensor: (usize, usize),
    /// The area the camera calls the picture.
    image: (usize, usize),
    /// R, G1, G2, B white-balance levels from the WBG block.
    wb: Option<[u16; 4]>,
    /// Where the compressed sensor data starts.
    data_offset: usize,
}

fn minolta_block(tiff: &Tiff<'_>) -> Option<MinoltaBlock> {
    let bytes = tiff.bytes();
    let little_endian = tiff.little_endian();
    let start = tiff
        .root()
        .get(stags::DNG_PRIVATE_DATA)
        .and_then(|e| long_of(e, little_endian))? as usize;
    // MRM is Minolta's own signature; the A100 writes MRI. Both are
    // followed by a length and then the same PRD/WBG sub-blocks.
    let head = bytes.get(start..start + 8)?;
    if !matches!(&head[..4], b"\0MRM" | b"\0MRI") {
        return None;
    }
    let u16_at = |at: usize| -> Option<u16> {
        let b: [u8; 2] = bytes.get(at..at + 2)?.try_into().ok()?;
        Some(if little_endian {
            u16::from_le_bytes(b)
        } else {
            u16::from_be_bytes(b)
        })
    };
    // Minolta's own MRW numbers its blocks big-endian; the copy the
    // A100 embeds follows the host TIFF instead, so the lengths are
    // read in the file's order like everything else here.
    let u32_at = |at: usize| -> Option<u32> {
        let b: [u8; 4] = bytes.get(at..at + 4)?.try_into().ok()?;
        Some(if little_endian {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        })
    };
    let end = start + 8 + u32_at(start + 4)? as usize;
    let mut at = start + 8;
    let (mut sensor, mut image, mut wb) = (None, None, None);
    while at + 8 <= end.min(bytes.len()) {
        let tag: [u8; 4] = bytes.get(at..at + 4)?.try_into().ok()?;
        let len = u32_at(at + 4)? as usize;
        let body = at + 8;
        match &tag {
            // PRD: eight bytes of version, then the sensor and image
            // dimensions as height/width pairs.
            b"\0PRD" if len >= 16 => {
                sensor = Some((u16_at(body + 10)? as usize, u16_at(body + 8)? as usize));
                image = Some((u16_at(body + 14)? as usize, u16_at(body + 12)? as usize));
            }
            // WBG: four scale exponents, then four levels.
            b"\0WBG" if len >= 12 => {
                wb = Some([
                    u16_at(body + 4)?,
                    u16_at(body + 6)?,
                    u16_at(body + 8)?,
                    u16_at(body + 10)?,
                ]);
            }
            _ => {}
        }
        at = body.checked_add(len)?;
    }
    // On the A100 the SubIFDs tag is not a directory pointer at all;
    // it is where the Huffman stream begins.
    let data_offset = tiff.root().get(tags::SUB_IFDS).and_then(|e| e.u32(0))? as usize;
    Some(MinoltaBlock {
        sensor: sensor?,
        image: image?,
        wb,
        data_offset,
    })
}

/// The eighteen codes of the A100's fixed Huffman table, as
/// `(code length, difference bits)`. Codes are assigned in this order,
/// longest first, each taking the next block of a flat 15-bit lookup —
/// so the two 15-bit codes are 0 and 1, the 14-bit code is 1, and so
/// on up to the two 2-bit codes 2 and 3. The lengths add to exactly
/// one, so the table is complete.
const ARW1_CODES: [(u32, u32); 18] = [
    (15, 17),
    (15, 16),
    (14, 15),
    (13, 14),
    (12, 13),
    (11, 12),
    (10, 11),
    (9, 10),
    (8, 9),
    (7, 8),
    (6, 7),
    (5, 6),
    (4, 5),
    (3, 4),
    (3, 3),
    (3, 0),
    (2, 2),
    (2, 1),
];

/// The flat 32768-entry lookup `ARW1_CODES` describes, as
/// `(length, difference bits)` per 15-bit prefix.
fn arw1_table() -> Vec<(u8, u8)> {
    let mut table = Vec::with_capacity(1 << 15);
    for (length, bits) in ARW1_CODES {
        table.resize(
            table.len() + (1usize << (15 - length)),
            (length as u8, bits as u8),
        );
    }
    table
}

/// The DSLR-A100's ARW 1.0 frame.
///
/// Unlike everything after it, this is scanned *down* columns, from
/// the last to the first, and within a column the even rows come
/// before the odd ones — so a difference is always taken against the
/// pixel two rows up, under the same colour filter. The running sum
/// is never reset, not even between columns.
fn decode_arw1(tiff: &Tiff<'_>, mrw: &MinoltaBlock, make: &str, model: &str) -> Result<RawImage> {
    let (sensor_width, sensor_height) = mrw.sensor;
    // The stream carries one more column than the sensor block
    // advertises, and the last row is decoded but never stored.
    let width = sensor_width + 1;
    let height = sensor_height;
    if width * height > 1 << 28 {
        return Err(Error::Corrupt("arw: implausible A100 frame".into()));
    }
    let stream = tiff
        .bytes()
        .get(mrw.data_offset..)
        .ok_or_else(|| Error::Corrupt("arw: A100 data offset past the end of the file".into()))?;

    let table = arw1_table();
    let mut pump = BitPumpMsb::new(stream);
    let mut out = vec![0u16; width * height];
    let mut sum: i32 = 0;
    let order: Vec<usize> = (0..height)
        .step_by(2)
        .chain((1..height).step_by(2))
        .collect();
    for col in (0..width).rev() {
        for &row in &order {
            let (length, bits) = table[pump.peek(15) as usize];
            pump.consume(length as u32);
            let mut diff = 0i32;
            if bits > 0 {
                let raw = pump.get(bits as u32) as i32;
                // JPEG's difference extension: a value whose top bit
                // is clear is the negative half of the range.
                diff = if raw & (1 << (bits - 1)) != 0 {
                    raw
                } else {
                    raw - ((1i32 << bits) - 1)
                };
            }
            sum = sum.saturating_add(diff);
            // The final row is padding the encoder emits and the
            // camera never shows; leaving it zero is what LibRaw does.
            if row + 1 < height {
                out[row * width + col] = sum.clamp(0, 0xffff) as u16;
            }
        }
    }

    let mut raw = RawImage::new(Format::Arw, width, height, 1, RawData::U16(out), Cfa::GRBG);
    raw.set_camera(make, model);
    raw.orientation = common::orientation(tiff);
    raw.preview = common::largest_jpeg(tiff);
    raw.metadata = common::metadata(tiff);
    // Twelve bits, and the A100 records no black level anywhere: the
    // masked columns it does not ship would be the only evidence.
    raw.white_level = 4095.0;
    if let Some([r, g1, _g2, b]) = mrw.wb {
        if g1 > 0 {
            let g = g1 as f32;
            raw.wb_coeffs = [r as f32 / g, 1.0, b as f32 / g, 1.0];
        }
    }
    raw.crop = Rect {
        x: 0,
        y: 0,
        width: mrw.image.0.min(width),
        height: mrw.image.1.min(height),
    };
    raw.apply_camera_table();
    Ok(raw)
}

// ---------------------------------------------------------------- //
// The LFSR, and the metadata behind it.
// ---------------------------------------------------------------- //

/// Sony's stream cipher: a 127-word lagged-Fibonacci generator seeded
/// from a 32-bit key, XORed over the data as big-endian words.
///
/// Four words come from iterating `key = key * 48828125 + 1`; the rest
/// of the pad is `(pad[i-4] ^ pad[i-2]) << 1 | (pad[i-3] ^ pad[i-1]) >> 31`,
/// and from then on each output word replaces `pad[p]` with
/// `pad[p+1] ^ pad[p+65]`. Trailing bytes that do not fill a word are
/// left alone, exactly as the camera leaves them.
fn sony_decrypt(data: &[u8], key: u32) -> Vec<u8> {
    let mut pad = [0u32; 128];
    let mut k = key;
    for slot in pad.iter_mut().take(4) {
        k = k.wrapping_mul(48828125).wrapping_add(1);
        *slot = k;
    }
    pad[3] = (pad[3] << 1) | ((pad[0] ^ pad[2]) >> 31);
    for i in 4..127 {
        pad[i] = ((pad[i - 4] ^ pad[i - 2]) << 1) | ((pad[i - 3] ^ pad[i - 1]) >> 31);
    }
    let mut out = data.to_vec();
    let mut p = 127usize;
    for word in out.as_chunks_mut::<4>().0 {
        let v = pad[(p + 1) & 127] ^ pad[(p + 65) & 127];
        pad[p & 127] = v;
        p = p.wrapping_add(1);
        *word = (u32::from_be_bytes(*word) ^ v).to_be_bytes();
    }
    out
}

/// What the encrypted SR2SubIFD gives us.
#[derive(Debug, Default, Clone, PartialEq)]
struct Sr2 {
    /// Per CFA position, on the 14-bit scale.
    black: Option<[u32; 4]>,
    /// R, G, B, G2 multipliers with green at 1.0.
    wb: Option<[f32; 4]>,
    /// left, top, right, bottom of the active area.
    crop: Option<[u32; 4]>,
}

/// Decrypt and read the SR2SubIFD.
///
/// `DNGPrivateData` holds a single LONG: the offset of a plain IFD
/// (the "SR2Private" one) whose 0x7200/0x7201 give the position and
/// length of a second, encrypted directory and whose 0x7221 is its
/// key. The encrypted block is an ordinary IFD once decrypted, with
/// offsets still measured from the start of the file — so it is
/// parsed inside a buffer that puts it back where it came from.
fn sr2_metadata(tiff: &Tiff<'_>, bytes: &[u8]) -> Option<Sr2> {
    let little_endian = tiff.little_endian();
    let private = tiff
        .root()
        .get(stags::DNG_PRIVATE_DATA)
        .and_then(|e| long_of(e, little_endian))? as usize;
    let outer = Tiff::parse_at(bytes, private, little_endian).ok()?;
    let root = outer.root();
    let offset = root.get(stags::SR2_SUBIFD_OFFSET).and_then(|e| e.u32(0))? as usize;
    let length = root.get(stags::SR2_SUBIFD_LENGTH).and_then(|e| e.u32(0))? as usize;
    let key = root
        .get(stags::SR2_SUBIFD_KEY)
        .and_then(|e| long_of(e, little_endian))?;
    // A hostile file could name a gigabyte here; the real ones are
    // under 64 KB and the block has to lie inside the file anyway.
    if length == 0 || length > 1 << 22 {
        return None;
    }
    let encrypted = bytes.get(offset..offset.checked_add(length)?)?;
    let mut buffer = vec![0u8; offset];
    buffer.extend_from_slice(&sony_decrypt(encrypted, key));

    let inner = Tiff::parse_at(&buffer, offset, little_endian).ok()?;
    let ifd = inner.root();
    let quad = |tag: u16| -> Option<[u32; 4]> {
        let entry = ifd.get(tag)?;
        Some([entry.u32(0)?, entry.u32(1)?, entry.u32(2)?, entry.u32(3)?])
    };
    let mut sr2 = Sr2 {
        black: quad(stags::BLACK_LEVEL).or_else(|| quad(stags::BLACK_LEVEL_2)),
        wb: None,
        crop: quad(stags::CROP_RECT),
    };
    // WB_RGGBLevels is R, G1, G2, B; the DSC-R1 writes WB_GRBGLevels
    // instead, in the order its own filter array runs.
    if let Some([r, g1, g2, b]) = quad(stags::WB_RGGB_LEVELS) {
        sr2.wb = normalise_wb(r, g1, b, g2);
    } else if let Some([g1, r, b, g2]) = quad(stags::WB_GRBG_LEVELS) {
        sr2.wb = normalise_wb(r, g1, b, g2);
    }
    Some(sr2)
}

fn normalise_wb(r: u32, g: u32, b: u32, g2: u32) -> Option<[f32; 4]> {
    if g == 0 || r == 0 || b == 0 {
        return None;
    }
    let g = g as f32;
    Some([
        r as f32 / g,
        1.0,
        b as f32 / g,
        (g2 as f32 / g).max(f32::MIN_POSITIVE),
    ])
}

/// The key the DSC-F828's sensor data is encrypted with, or `None`
/// when the file is an SR2 (which is stored in the clear).
///
/// SRF chains several IFDs after the maker note, each encrypted with a
/// key held by the one before it. The first key is not reachable from
/// the chain — it sits in an opaque block near the end of the file —
/// so it is found by trying every aligned word between the maker note
/// and the sensor data and keeping the one that turns the first
/// encrypted directory into a well-formed two-entry IFD holding
/// exactly the tags it should. That is self-checking, and cheaper than
/// it sounds: the pad costs about 130 operations a candidate.
fn srf_data_key(tiff: &Tiff<'_>, bytes: &[u8]) -> Option<u32> {
    // Only SRF looks like this: no SR2Private, and a maker note whose
    // own next-IFD pointer is the head of the encrypted chain.
    if tiff.root().has(stags::DNG_PRIVATE_DATA) {
        return None;
    }
    let maker = tiff.exif()?.get(tags::MAKER_NOTE)?;
    let start = maker.offset;
    // `count`, not the decoded bytes: the F828's maker note is most of
    // a megabyte and the TIFF parser declines to hold values that big.
    let count = maker.count;
    let little_endian = tiff.little_endian();
    let u16_at = |at: usize| -> Option<u16> {
        let b: [u8; 2] = bytes.get(at..at + 2)?.try_into().ok()?;
        Some(if little_endian {
            u16::from_le_bytes(b)
        } else {
            u16::from_be_bytes(b)
        })
    };
    let u32_at = |at: usize| -> Option<u32> {
        let b: [u8; 4] = bytes.get(at..at + 4)?.try_into().ok()?;
        Some(if little_endian {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        })
    };
    // The maker note is itself an IFD; its next-IFD pointer is the
    // first encrypted directory.
    let entries = u16_at(start)? as usize;
    if entries > 64 {
        return None;
    }
    let chain = u32_at(start + 2 + entries * 12)? as usize;
    let head = bytes.get(chain..chain + 32)?;
    let end = start.checked_add(count)?.min(bytes.len());

    for at in (start..end.saturating_sub(4)).step_by(4) {
        let candidate = u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?);
        let plain = sony_decrypt(head, candidate);
        // Two entries: tag 0 is the key of the next directory, tag 1
        // is the key the sensor data itself is encrypted with. Both
        // are single LONGs.
        let field = |off: usize| -> u32 {
            let b: [u8; 4] = plain[off..off + 4].try_into().expect("four bytes");
            if little_endian {
                u32::from_le_bytes(b)
            } else {
                u32::from_be_bytes(b)
            }
        };
        let short = |off: usize| -> u16 {
            let b: [u8; 2] = plain[off..off + 2].try_into().expect("two bytes");
            if little_endian {
                u16::from_le_bytes(b)
            } else {
                u16::from_be_bytes(b)
            }
        };
        if short(0) == 2
            && short(2) == 0
            && short(4) == 4
            && field(6) == 1
            && short(14) == 1
            && short(16) == 4
            && field(18) == 1
        {
            return Some(field(22));
        }
    }
    None
}

// ---------------------------------------------------------------- //
// Filter array and crop.
// ---------------------------------------------------------------- //

/// The filter array at the frame's own origin.
///
/// ARW and SR2 both carry a plain EXIF `CFAPattern`. The DSC-F828 does
/// not: it is a four-colour RGBE sensor and the only description of
/// its layout is the camera's.
fn cfa_of(ifd: &Ifd, _make: &str, model: &str) -> Cfa {
    if let Some(bytes) = ifd.get(tags::CFA_PATTERN).and_then(|e| e.bytes()) {
        let dim = ifd
            .get(tags::CFA_REPEAT_PATTERN_DIM)
            .map(|e| e.u32s())
            .unwrap_or_default();
        let (w, h) = match dim.as_slice() {
            [w, h] => (*w as usize, *h as usize),
            _ => (2, 2),
        };
        if w == 2 && h == 2 && bytes.len() >= 4 {
            let colors: Vec<CfaColor> = bytes[..4].iter().map(|c| cfa_color(*c)).collect();
            return Cfa::Bayer([colors[0], colors[1], colors[2], colors[3]]);
        }
        if w > 0 && h > 0 && bytes.len() >= w * h && w * h <= 64 {
            return Cfa::Pattern {
                width: w,
                height: h,
                colors: bytes[..w * h].iter().map(|c| cfa_color(*c)).collect(),
            };
        }
    }
    if model.contains("F828") {
        // Red, emerald / green, blue: the F828's four-colour array,
        // which LibRaw prints as ERBG once its own five-column left
        // margin is taken off.
        return Cfa::Bayer([
            CfaColor::Red,
            CfaColor::Emerald,
            CfaColor::Green,
            CfaColor::Blue,
        ]);
    }
    Cfa::RGGB
}

/// EXIF `CFAPattern` colour codes.
fn cfa_color(code: u8) -> CfaColor {
    match code {
        0 => CfaColor::Red,
        1 => CfaColor::Green,
        2 => CfaColor::Blue,
        3 => CfaColor::Cyan,
        4 => CfaColor::Magenta,
        5 => CfaColor::Yellow,
        _ => CfaColor::Emerald,
    }
}

/// The active area.
///
/// The decrypted SR2SubIFD's 0x74C3 gives it as left/top/right/bottom
/// on every ARW and SR2 from the A700 on. Newer bodies also write the
/// DNG `DefaultCropOrigin`/`DefaultCropSize` pair, which agrees with
/// it. With neither — the DSC-F828 — the whole frame is shown.
fn crop_of(ifd: &Ifd, meta: Option<&Sr2>, width: usize, height: usize) -> Rect {
    let whole = Rect {
        x: 0,
        y: 0,
        width,
        height,
    };
    if let Some([left, top, right, bottom]) = meta.and_then(|m| m.crop) {
        let (left, top, right, bottom) =
            (left as usize, top as usize, right as usize, bottom as usize);
        if right > left && bottom > top && right <= width && bottom <= height {
            return Rect {
                x: left,
                y: top,
                width: right - left,
                height: bottom - top,
            };
        }
    }
    let origin = ifd
        .get(stags::DEFAULT_CROP_ORIGIN)
        .map(|e| e.u32s())
        .unwrap_or_default();
    let size = ifd
        .get(stags::DEFAULT_CROP_SIZE)
        .map(|e| e.u32s())
        .unwrap_or_default();
    if let ([x, y], [w, h]) = (&origin[..], &size[..]) {
        let (x, y, w, h) = (*x as usize, *y as usize, *w as usize, *h as usize);
        if w > 0 && h > 0 && x + w <= width && y + h <= height {
            return Rect {
                x,
                y,
                width: w,
                height: h,
            };
        }
    }
    whole
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tiff::tests::{corpus, samples};
    use crate::Orientation;
    use std::path::{Path, PathBuf};

    // ---------------------------------------------------------------
    // The mechanics, on bytes built by hand.
    // ---------------------------------------------------------------

    /// The 128-bit little-endian block an ARW 2.x row is made of.
    fn block(max: u16, min: u16, imax: usize, imin: usize, deltas: [u16; 14]) -> u128 {
        let mut word =
            (max as u128) | ((min as u128) << 11) | ((imax as u128) << 22) | ((imin as u128) << 26);
        for (i, delta) in deltas.iter().enumerate() {
            word |= (*delta as u128 & 0x7f) << (30 + 7 * i);
        }
        word
    }

    #[test]
    fn block_places_its_extremes_and_scales_the_rest() {
        let mut out = [0u16; 16];
        // A range of 36 needs no shift: 127 steps already span it.
        decode_block(
            block(
                338,
                302,
                7,
                11,
                [12, 24, 26, 32, 20, 28, 30, 10, 8, 6, 2, 14, 16, 2],
            ),
            &mut out,
        );
        assert_eq!(out[7], 338, "the maximum goes where imax says");
        assert_eq!(out[11], 302, "and the minimum where imin says");
        assert_eq!(out[0], 302 + 12);
        assert_eq!(out[15], 302 + 2);

        // 300 apart: 127 steps of one do not reach it, nor 127 of
        // two, so the shift settles at two and each delta is worth
        // four.
        let mut out = [0u16; 16];
        decode_block(block(400, 100, 0, 1, [1; 14]), &mut out);
        assert_eq!(out[0], 400);
        assert_eq!(out[1], 100);
        assert_eq!(out[2], 104);
    }

    #[test]
    fn block_deltas_cannot_escape_eleven_bits() {
        let mut out = [0u16; 16];
        // A near-full range takes the largest shift, and 127 << 3 on
        // top of a high minimum would overflow the 11-bit domain.
        decode_block(block(2047, 1500, 0, 1, [127; 14]), &mut out);
        assert!(
            out.iter().all(|v| *v <= 0x7ff),
            "clamped to eleven bits: {out:?}"
        );
    }

    /// A one-entry little-endian TIFF holding just the tone curve,
    /// for `tone_curve`: header, an IFD of one SHORT[4] entry whose
    /// value sits after the directory.
    fn curve_tiff(values: [u16; 4]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"II*\0");
        bytes.extend_from_slice(&8u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&stags::TONE_CURVE.to_le_bytes());
        bytes.extend_from_slice(&3u16.to_le_bytes());
        bytes.extend_from_slice(&4u32.to_le_bytes());
        bytes.extend_from_slice(&26u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        for v in values {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        bytes
    }

    #[test]
    fn tone_curve_follows_its_knees() {
        let bytes = curve_tiff([8000, 10400, 12900, 14100]);
        let tiff = Tiff::parse(&bytes).expect("hand-built TIFF parses");
        let curve = tone_curve(tiff.root()).expect("a well-formed curve");
        // Knees at 2000, 2600, 3225, 3525 with slopes 1, 2, 4, 8, 16.
        assert_eq!(curve[0], 0);
        assert_eq!(curve[2000], 2000);
        assert_eq!(curve[2600], 3200);
        assert_eq!(curve[3225], 5700);
        assert_eq!(curve[3525], 8100);
        assert_eq!(curve[4095], 17220.min(u16::MAX as u32) as u16);
        // The eleven-bit domain is entered at twice the sample value.
        assert_eq!(curve[338 << 1], 676);
    }

    #[test]
    fn tone_curve_rejects_a_curve_that_does_not_rise() {
        let flat = curve_tiff([0, 0, 0, 0]);
        let tiff = Tiff::parse(&flat).expect("hand-built TIFF parses");
        assert!(matches!(
            tone_curve(tiff.root()),
            Err(Error::Unsupported(_))
        ));
        let jumbled = curve_tiff([12900, 10400, 8000, 14100]);
        let tiff = Tiff::parse(&jumbled).expect("hand-built TIFF parses");
        assert!(matches!(
            tone_curve(tiff.root()),
            Err(Error::Unsupported(_))
        ));
    }

    #[test]
    fn arw1_table_is_a_complete_code() {
        let table = arw1_table();
        assert_eq!(
            table.len(),
            1 << 15,
            "the eighteen codes tile the whole lookup"
        );
        // The first two entries are the 15-bit codes, the last block
        // the 2-bit one; every entry names a length and a bit count.
        assert_eq!(table[0], (15, 17));
        assert_eq!(table[1], (15, 16));
        assert_eq!(table[1 << 14], (2, 2));
        assert_eq!(table[(1 << 15) - 1], (2, 1));
        let kraft: f64 = ARW1_CODES
            .iter()
            .map(|(len, _)| 2f64.powi(-(*len as i32)))
            .sum();
        assert!((kraft - 1.0).abs() < 1e-12, "Kraft sum {kraft}");
    }

    #[test]
    fn lfsr_is_its_own_inverse() {
        let plain: Vec<u8> = (0..64u8).collect();
        let once = sony_decrypt(&plain, 0x4433_2211);
        assert_ne!(once, plain);
        assert_eq!(
            sony_decrypt(&once, 0x4433_2211),
            plain,
            "XOR twice is identity"
        );
        // A different key gives a different stream.
        assert_ne!(sony_decrypt(&plain, 0x4433_2212), once);
        // Bytes past the last whole word are left alone.
        let odd = sony_decrypt(&[1, 2, 3, 4, 5, 6], 7);
        assert_eq!(&odd[4..], &[5, 6]);
    }

    #[test]
    fn packed_twelve_unpacks_low_bits_first() {
        // 0xABC and 0x123 written as Sony writes them.
        let out = decode_packed12(&[0xbc, 0x3a, 0x12], 2);
        assert_eq!(out, vec![0x0abc, 0x0123]);
    }

    #[test]
    fn words_read_in_both_orders() {
        assert_eq!(decode_words(&[0x34, 0x12], 1, true), vec![0x1234]);
        assert_eq!(decode_words(&[0x12, 0x34], 1, false), vec![0x1234]);
    }

    #[test]
    fn word_order_is_judged_by_which_way_round_fits_fourteen_bits() {
        // Fourteen-bit samples written little-endian: read the other
        // way round nearly every one of them overflows.
        let little: Vec<u8> = (0..4096u16)
            .flat_map(|i| (i % 0x3fff).to_le_bytes())
            .collect();
        assert!(words_look_little_endian(&little, false));
        let big: Vec<u8> = (1..4096u16).flat_map(|i| (i * 4).to_be_bytes()).collect();
        assert!(!words_look_little_endian(&big, true));
    }

    #[test]
    fn a_row_of_blocks_lands_in_two_interleaved_colour_streams() {
        // Two blocks, each sixteen pixels of one colour: the first
        // fills the even columns of 0..32, the second the odd ones.
        let curve: Vec<u16> = (0..4096u32).map(|v| v as u16).collect();
        let mut chunk = Vec::new();
        chunk.extend_from_slice(&block(100, 100, 0, 1, [0; 14]).to_le_bytes());
        chunk.extend_from_slice(&block(200, 200, 0, 1, [0; 14]).to_le_bytes());
        let row = decode_blocks(&chunk, 32, 1, &curve).expect("a 32-pixel row");
        assert_eq!(row.len(), 32);
        assert!(
            row.iter().step_by(2).all(|v| *v == 200),
            "even columns: {row:?}"
        );
        assert!(row[1..].iter().step_by(2).all(|v| *v == 400), "odd columns");
    }

    #[test]
    fn a_frame_that_is_not_whole_blocks_is_unsupported() {
        let curve = vec![0u16; 4096];
        assert!(matches!(
            decode_blocks(&[0; 16], 16, 1, &curve),
            Err(Error::Unsupported(_))
        ));
    }

    #[test]
    fn garbage_is_never_a_raw() {
        assert!(decode(&[]).is_err());
        assert!(decode(b"II*\0\x08\0\0\0").is_err());
        let mut noise: Vec<u8> = b"II*\0\x08\0\0\0".to_vec();
        noise.extend((0..4096u32).map(|i| i.wrapping_mul(2654435761).to_le_bytes()[0]));
        assert!(decode(&noise).is_err());
    }

    // ---------------------------------------------------------------
    // Corpus. SCHIST_RAW_CORPUS points at a directory of real files
    // with LibRaw's `unprocessed_raw -T` output beside each as
    // `<file>.tiff` and `raw-identify -v -w` as `<file>.identify.txt`.
    // ---------------------------------------------------------------

    /// Files this module knowingly does not reproduce exactly, and why.
    fn deviations(name: &str) -> Option<&'static str> {
        match name {
            // LibRaw shows this four-colour RGBE sensor with a
            // five-column left margin and a 3287-pixel width, both of
            // which come from its own model table rather than from the
            // file. Nothing in the SRF records an active area, so the
            // whole frame is shown and the pattern is reported at the
            // frame's own origin (RGGB-shaped RE/GB, which is LibRaw's
            // ERBG shifted one column left).
            "DSC-F828-DSC06227.SRF" => {
                Some("SRF: no recorded crop, so the whole frame with the pattern at its origin")
            }
            // LibRaw keeps the A100's last, never-written row and
            // shows the full 3881-column frame. The camera's own MRW
            // block says the picture is 3872x2592, which is what the
            // crop follows.
            "DSLR-A100-_DSC0258.ARW" => {
                Some("A100: crop from the camera's MRW block, not LibRaw's full frame")
            }
            _ => None,
        }
    }

    /// Samples `crate::probe` does not recognise today, with the
    /// reason. The fix belongs in lib.rs, which this worker does not
    /// own; `formats::arw::decode` handles both.
    fn unprobeable(name: &str) -> Option<&'static str> {
        match name {
            // `probe_tiff` needs an IFD with StripOffsets to call a
            // deep-strip directory sensor data, and the F828 records
            // byte counts but no offset at all.
            "DSC-F828-DSC06227.SRF" => Some("SRF has no StripOffsets for probe_tiff to find"),
            // The A100's SubIFDs tag points at raw bytes rather than a
            // directory, so the walk finds no CFA photometric.
            "DSLR-A100-_DSC0258.ARW" => Some("A100's SubIFDs tag is a data offset, not an IFD"),
            _ => None,
        }
    }

    fn is_sony(path: &Path) -> bool {
        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        matches!(ext.as_str(), "arw" | "sr2" | "srf")
    }

    /// The `raw-identify -v -w` sidecar, as its non-empty lines.
    fn identify(path: &Path) -> Option<Vec<String>> {
        let mut name = path.as_os_str().to_os_string();
        name.push(".identify.txt");
        let text = std::fs::read_to_string(PathBuf::from(name)).ok()?;
        Some(text.lines().map(|l| l.trim().to_string()).collect())
    }

    /// The numbers on the identify line starting with `prefix`.
    fn numbers(lines: &[String], prefix: &str) -> Option<Vec<i64>> {
        let line = lines.iter().find(|l| l.starts_with(prefix))?;
        Some(
            line[prefix.len()..]
                .split(|c: char| !(c.is_ascii_digit() || c == '-'))
                .filter(|s| !s.is_empty())
                .filter_map(|s| s.parse::<i64>().ok())
                .collect(),
        )
    }

    /// The filter array as raw-identify prints it: sixteen letters,
    /// the colour at (row = i / 2, column = i % 2) of the *cropped*
    /// image.
    fn pattern_string(cfa: &Cfa) -> String {
        (0..16)
            .map(|i| match cfa.color_at(i % 2, i / 2) {
                Some(CfaColor::Red) => 'R',
                Some(CfaColor::Green) | Some(CfaColor::Green2) => 'G',
                Some(CfaColor::Blue) => 'B',
                Some(CfaColor::Emerald) => 'E',
                Some(CfaColor::Cyan) => 'C',
                Some(CfaColor::Magenta) => 'M',
                Some(CfaColor::Yellow) => 'Y',
                None => '?',
            })
            .collect()
    }

    fn oracle(path: &Path) -> Option<(usize, usize, Vec<u16>)> {
        let mut name = path.as_os_str().to_os_string();
        name.push(".tiff");
        let image = image::open(PathBuf::from(name)).ok()?.into_luma16();
        Some((
            image.width() as usize,
            image.height() as usize,
            image.into_raw(),
        ))
    }

    #[test]
    fn corpus_matches_the_oracle() {
        let Some(root) = corpus() else { return };
        let mut seen = 0;
        let mut problems: Vec<String> = Vec::new();
        for path in samples(&root).into_iter().filter(|p| is_sony(p)) {
            let name = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            seen += 1;

            match crate::probe(&bytes) {
                Some(Format::Arw) => {}
                other if unprobeable(&name).is_some() => {
                    // Known, and not this module's to fix: `probe`
                    // lives in lib.rs. Decoding still has to work.
                    assert!(other.is_none(), "{name}: probe says {other:?}");
                }
                other => {
                    problems.push(format!("{name}: probe says {other:?}, not Arw"));
                    continue;
                }
            }
            let raw = match decode(&bytes) {
                Ok(raw) => raw,
                Err(e) => {
                    problems.push(format!("{name}: decode failed: {e}"));
                    continue;
                }
            };
            if let Err(e) = raw.validate() {
                problems.push(format!("{name}: validate: {e}"));
                continue;
            }
            let deviation = deviations(&name);

            // The sensor data, sample for sample.
            if let Some((width, height, want)) = oracle(&path) {
                if (width, height) != (raw.width, raw.height) {
                    problems.push(format!(
                        "{name}: {}x{} decoded, oracle is {width}x{height}",
                        raw.width, raw.height
                    ));
                } else {
                    let RawData::U16(got) = &raw.data else {
                        problems.push(format!("{name}: not 16-bit data"));
                        continue;
                    };
                    let mismatches: Vec<usize> = got
                        .iter()
                        .zip(want.iter())
                        .enumerate()
                        .filter(|(_, (a, b))| a != b)
                        .map(|(i, _)| i)
                        .take(6)
                        .collect();
                    let total = got.iter().zip(want.iter()).filter(|(a, b)| a != b).count();
                    if total > 0 {
                        let first: Vec<String> = mismatches
                            .iter()
                            .map(|i| {
                                format!(
                                    "({},{}) got {} want {}",
                                    i % width,
                                    i / width,
                                    got[*i],
                                    want[*i]
                                )
                            })
                            .collect();
                        problems.push(format!(
                            "{name}: {total} of {} samples differ: {}",
                            got.len(),
                            first.join(", ")
                        ));
                    }
                }
            } else {
                problems.push(format!("{name}: no <file>.tiff oracle beside it"));
            }

            let Some(lines) = identify(&path) else {
                problems.push(format!("{name}: no identify.txt beside it"));
                continue;
            };

            // Frame size, black, white balance, orientation.
            if let Some([w, h]) = numbers(&lines, "Full size:").as_deref() {
                if (*w as usize, *h as usize) != (raw.width, raw.height) {
                    problems.push(format!(
                        "{name}: full size {w}x{h}, decoded {}x{}",
                        raw.width, raw.height
                    ));
                }
            }
            // raw-identify prints "black:" for a single level and
            // "cblack[0 .. 3]:" when the four positions differ; it
            // prints neither when the file records none, and then the
            // camera table is free to supply one.
            if let Some(black) =
                numbers(&lines, "cblack[0 .. 3]:").or_else(|| numbers(&lines, "black:"))
            {
                let black: Vec<f32> = black.iter().map(|v| *v as f32).collect();
                let want_black = match black.as_slice() {
                    [one] => [*one; 4],
                    [a, b, c, d, ..] => [*a, *b, *c, *d],
                    _ => [0.0; 4],
                };
                if raw.black_levels != want_black {
                    problems.push(format!(
                        "{name}: black {:?}, identify says {want_black:?}",
                        raw.black_levels
                    ));
                }
            }
            if let Some(shot) = numbers(&lines, "As shot") {
                // The line is four levels then four EVs; the decimal
                // points split into extra "numbers", so only the first
                // four are the multipliers.
                if let [r, g, b, g2, ..] = shot[..] {
                    let want = [
                        r as f32 / g as f32,
                        1.0,
                        b as f32 / g as f32,
                        g2 as f32 / g as f32,
                    ];
                    let off = (0..4).any(|i| (raw.wb_coeffs[i] - want[i]).abs() > 1e-4);
                    if off {
                        problems.push(format!(
                            "{name}: wb {:?}, identify says {want:?}",
                            raw.wb_coeffs
                        ));
                    }
                }
            }
            if let Some([flip]) = numbers(&lines, "Image flip:").as_deref() {
                let want = match flip {
                    3 => Orientation::Rotate180,
                    5 => Orientation::Rotate270CW,
                    6 => Orientation::Rotate90CW,
                    _ => Orientation::Normal,
                };
                if raw.orientation != want {
                    problems.push(format!(
                        "{name}: orientation {:?}, flip {flip}",
                        raw.orientation
                    ));
                }
            }

            // The filter array, anchored where the crop starts.
            if let Some(line) = lines.iter().find(|l| l.starts_with("Filter pattern:")) {
                let want = line["Filter pattern:".len()..].trim();
                let got = pattern_string(&raw.cfa.shifted(raw.crop.x, raw.crop.y));
                if got != want && deviation.is_none() {
                    problems.push(format!("{name}: pattern {got}, identify says {want}"));
                }
            }

            // The crop, against LibRaw's own "Raw inset" where the
            // file records one.
            if let Some(inset) = numbers(&lines, "Raw inset, width x height:") {
                let (want, origin) = match inset[..] {
                    [w, h] => (Some((w as usize, h as usize)), (0, 0)),
                    [w, h, x, y, ..] => (Some((w as usize, h as usize)), (x as usize, y as usize)),
                    _ => (None, (0, 0)),
                };
                if let Some((w, h)) = want {
                    if (raw.crop.width, raw.crop.height) != (w, h) {
                        problems.push(format!("{name}: crop {:?}, raw inset {w}x{h}", raw.crop));
                    }
                    // LibRaw only prints the inset's origin when it
                    // knows one; where it does not, ours comes from
                    // the file's own 0x74C3 and is left unchecked.
                    if origin != (0, 0) && (raw.crop.x, raw.crop.y) != origin {
                        problems.push(format!(
                            "{name}: crop origin {:?}, raw inset at {origin:?}",
                            raw.crop
                        ));
                    }
                }
            }

            // The preview has to be a picture.
            match &raw.preview {
                Some(jpeg) => {
                    if image::load_from_memory(jpeg).is_err() {
                        problems.push(format!("{name}: preview does not decode"));
                    }
                }
                None => problems.push(format!("{name}: no preview")),
            }
        }
        assert!(seen > 0, "SCHIST_RAW_CORPUS holds no Sony samples");
        assert!(
            problems.is_empty(),
            "{} problems:\n{}",
            problems.len(),
            problems.join("\n")
        );
    }

    #[test]
    fn truncated_files_never_panic() {
        let Some(root) = corpus() else { return };
        for path in samples(&root).into_iter().filter(|p| is_sony(p)) {
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            // Ten cuts spread over the file, plus the awkward ones
            // right at the header and right before the end.
            let mut cuts: Vec<usize> = (1..=10).map(|i| bytes.len() * i / 11).collect();
            cuts.extend([0, 4, 8, 16, bytes.len() - 1]);
            for cut in cuts {
                let _ = decode(&bytes[..cut]);
                let _ = preview(&bytes[..cut]);
                let _ = crate::probe(&bytes[..cut]);
            }
            // And a middle byte flipped, which is the other way a
            // decoder gets asked to read past its buffers.
            let mut damaged = bytes.clone();
            let middle = damaged.len() / 2;
            damaged[middle] ^= 0xff;
            let _ = decode(&damaged);
        }
    }
}
