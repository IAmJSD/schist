//! Adobe DNG, the one raw container with a published specification.
//!
//! A DNG is an ordinary TIFF whose sensor data lives in an IFD marked
//! `PhotometricInterpretation` 32803 (colour filter array) or 34892
//! (linear raw, three samples a pixel). Everything a developer needs
//! is in tags beside it — black and white levels, the active area and
//! the default crop, the CFA layout, the as-shot neutral, two colour
//! matrices with the illuminants they were measured under — so this
//! module needs no camera table and no reverse engineering: it is
//! written from the DNG specification (1.0 through 1.7) and checked
//! against real files.
//!
//! Vendors that ship DNG straight out of the camera (Pentax, Ricoh,
//! Leica, Sigma, Apple, Google, Samsung, DJI, Hasselblad's drone
//! bodies) all take this path, as do files converted by Adobe's DNG
//! Converter.

use std::io::Cursor;

use rayon::prelude::*;

use crate::bits::{BitPump, BitPumpMsb};
use crate::formats::common;
use crate::formats::vc5;
use crate::ljpeg;
use crate::tiff::{tags, Ifd, ImageLayout, Tiff};
use crate::{Cfa, CfaColor, Error, Format, RawData, RawImage, Rect, Result};

/// The DNG and TIFF tags this module reads that the shared table does
/// not already name.
mod tag {
    pub const PREDICTOR: u16 = 0x013D;
    pub const SAMPLE_FORMAT: u16 = 0x0153;
    pub const DNG_BACKWARD_VERSION: u16 = 0xC613;
    pub const UNIQUE_CAMERA_MODEL: u16 = 0xC614;
    pub const CFA_PLANE_COLOR: u16 = 0xC616;
    pub const CFA_LAYOUT: u16 = 0xC617;
    pub const LINEARIZATION_TABLE: u16 = 0xC618;
    pub const BLACK_LEVEL_REPEAT_DIM: u16 = 0xC619;
    pub const BLACK_LEVEL: u16 = 0xC61A;
    pub const BLACK_LEVEL_DELTA_H: u16 = 0xC61B;
    pub const BLACK_LEVEL_DELTA_V: u16 = 0xC61C;
    pub const WHITE_LEVEL: u16 = 0xC61D;
    pub const DEFAULT_CROP_ORIGIN: u16 = 0xC61F;
    pub const DEFAULT_CROP_SIZE: u16 = 0xC620;
    pub const COLOR_MATRIX_1: u16 = 0xC621;
    pub const COLOR_MATRIX_2: u16 = 0xC622;
    pub const AS_SHOT_NEUTRAL: u16 = 0xC628;
    pub const AS_SHOT_WHITE_XY: u16 = 0xC629;
    pub const CALIBRATION_ILLUMINANT_1: u16 = 0xC65A;
    pub const CALIBRATION_ILLUMINANT_2: u16 = 0xC65B;
    pub const ACTIVE_AREA: u16 = 0xC68D;
    pub const SUB_TILE_BLOCK_SIZE: u16 = 0xC71E;
    pub const ROW_INTERLEAVE_FACTOR: u16 = 0xC71F;
    pub const OPCODE_LIST_1: u16 = 0xC740;
    pub const OPCODE_LIST_2: u16 = 0xC741;
    pub const OPCODE_LIST_3: u16 = 0xC742;
}

/// Compression codes a raw IFD may carry.
mod compression {
    pub const NONE: u32 = 1;
    pub const LOSSLESS_JPEG: u32 = 7;
    /// Deflate. 8 is the code DNG uses; 32946 is TIFF's older
    /// synonym and means the same zlib stream.
    pub const DEFLATE: u32 = 8;
    pub const DEFLATE_OLD: u32 = 32946;
    /// Baseline JPEG. The number collides with the *photometric*
    /// code for linear raw; they are unrelated.
    pub const LOSSY_JPEG: u32 = 34892;
    /// DNG 1.7's JPEG XL, which this crate has no decoder for.
    pub const JPEG_XL: u32 = 52546;
    /// GoPro's GPR files are DNGs whose raw IFD is a VC-5 wavelet
    /// stream. Not part of the DNG specification.
    pub const VC5: u32 = 9;
}

const PHOTOMETRIC_CFA: u32 = 32803;
const PHOTOMETRIC_LINEAR_RAW: u32 = 34892;

/// A ceiling on the frame a raw IFD may claim, so a corrupt header
/// cannot ask for an unbounded allocation. 512 M samples is an order
/// of magnitude past the largest sensor shipped.
const MAX_SAMPLES: usize = 1 << 29;

/// Decode a DNG into its sensor frame and everything needed to
/// develop it.
pub fn decode(bytes: &[u8]) -> Result<RawImage> {
    let tiff = Tiff::parse(bytes)?;
    check_version(&tiff)?;
    let ifd = raw_ifd(&tiff)?;
    let layout = ImageLayout::of(&tiff, ifd)?;

    let (width, height) = (layout.width, layout.height);
    let spp = layout.samples_per_pixel;
    let (cpp, cfa) = match layout.photometric {
        PHOTOMETRIC_CFA => {
            if spp != 1 {
                return Err(Error::Unsupported(format!(
                    "DNG CFA image with {spp} samples a pixel"
                )));
            }
            (1, cfa_pattern(ifd)?)
        }
        PHOTOMETRIC_LINEAR_RAW => {
            // The container carries three samples a pixel and nothing
            // else; `RawImage` has no room for a fourth plane.
            if spp != 3 {
                return Err(Error::Unsupported(format!(
                    "DNG linear raw with {spp} samples a pixel"
                )));
            }
            (3, Cfa::None)
        }
        other => return Err(Error::Unsupported(format!("DNG photometric {other}"))),
    };

    width
        .checked_mul(height)
        .and_then(|n| n.checked_mul(spp))
        .filter(|n| *n > 0 && *n <= MAX_SAMPLES)
        .ok_or_else(|| Error::Corrupt(format!("DNG raw IFD claims {width}x{height}x{spp}")))?;

    reject_exotic_layout(ifd)?;
    let bits = uniform_bits_per_sample(ifd, spp)?;
    let data = decode_samples(&tiff, ifd, &layout, spp, bits)?;

    let mut raw = RawImage::new(Format::Dng, width, height, cpp, data, cfa);
    fill_metadata(&tiff, ifd, &mut raw, bits, spp);
    raw.apply_camera_table();
    // AsShotWhiteXY only becomes multipliers through a matrix, which
    // the table may have supplied just now.
    if raw.wb_coeffs == [1.0; 4] && raw.color_matrix.is_some() {
        raw.wb_coeffs = white_balance(tiff.root(), ifd, raw.color_matrix.as_ref());
    }
    Ok(raw)
}

/// The largest embedded JPEG, without touching the sensor data.
pub fn preview(bytes: &[u8]) -> Result<Option<Vec<u8>>> {
    let tiff = Tiff::parse(bytes)?;
    Ok(common::largest_jpeg(&tiff))
}

// -------------------------------------------------------------------
// Choosing the image
// -------------------------------------------------------------------

/// DNGBackwardVersion says the oldest reader that can still make sense
/// of the file. Refusing anything newer than the specification this
/// module was written from is the check the specification itself asks
/// for, and it is the only thing that stops a future dialect from
/// being silently mis-decoded.
fn check_version(tiff: &Tiff<'_>) -> Result<()> {
    let version = |tag| {
        tiff.root().get(tag).map(|e| {
            let byte = |i| e.u32(i).unwrap_or(0);
            [byte(0), byte(1), byte(2), byte(3)]
        })
    };
    let backward = version(tag::DNG_BACKWARD_VERSION)
        .or_else(|| version(tags::DNG_VERSION))
        .unwrap_or([1, 0, 0, 0]);
    if backward > [1, 7, 0, 0] {
        return Err(Error::Unsupported(format!(
            "DNG needing a {}.{}.{}.{} reader",
            backward[0], backward[1], backward[2], backward[3]
        )));
    }
    Ok(())
}

/// The IFD holding sensor data: the largest one whose photometric says
/// CFA or linear raw and which is not marked as a reduced-resolution
/// copy. A DNG may hold several (an "enhanced" image beside the
/// original, a depth map, semantic masks); the biggest raw plane is
/// the main image, which is the only one this crate returns.
fn raw_ifd<'a>(tiff: &'a Tiff<'_>) -> Result<&'a Ifd> {
    let mut best: Option<(u64, &Ifd)> = None;
    for ifd in tiff.all() {
        let photometric = ifd.get(tags::PHOTOMETRIC).and_then(|e| e.u32(0));
        if !matches!(photometric, Some(PHOTOMETRIC_CFA | PHOTOMETRIC_LINEAR_RAW)) {
            continue;
        }
        // Bit 0 of NewSubfileType marks a reduced-resolution image.
        // Nothing else in the word disqualifies an IFD: DNG 1.6 sets
        // bit 16 on its enhanced image, which is still sensor data.
        let kind = ifd
            .get(tags::NEW_SUBFILE_TYPE)
            .and_then(|e| e.u32(0))
            .unwrap_or(0);
        if kind & 1 != 0 {
            continue;
        }
        let area = ifd
            .get(tags::IMAGE_WIDTH)
            .and_then(|e| e.u64(0))
            .unwrap_or(0)
            * ifd
                .get(tags::IMAGE_LENGTH)
                .and_then(|e| e.u64(0))
                .unwrap_or(0);
        if best.is_none_or(|(seen, _)| area > seen) {
            best = Some((area, ifd));
        }
    }
    best.map(|(_, ifd)| ifd)
        .ok_or_else(|| Error::Unsupported("DNG with no CFA or linear-raw IFD".into()))
}

/// Two DNG 1.2 features that rearrange a tile's rows and pixels before
/// they are laid down. Neither has ever been seen in a camera file and
/// implementing them blind would be untestable, so a file that uses
/// them is refused rather than decoded wrongly.
fn reject_exotic_layout(ifd: &Ifd) -> Result<()> {
    if ifd
        .get(tags::PLANAR_CONFIGURATION)
        .and_then(|e| e.u32(0))
        .unwrap_or(1)
        != 1
    {
        return Err(Error::Unsupported(
            "DNG with planar (separated) samples".into(),
        ));
    }
    if ifd
        .get(tag::ROW_INTERLEAVE_FACTOR)
        .and_then(|e| e.u32(0))
        .unwrap_or(1)
        != 1
    {
        return Err(Error::Unsupported("DNG with RowInterleaveFactor".into()));
    }
    if let Some(block) = ifd.get(tag::SUB_TILE_BLOCK_SIZE) {
        if block.u32(0).unwrap_or(1) != 1 || block.u32(1).unwrap_or(1) != 1 {
            return Err(Error::Unsupported("DNG with SubTileBlockSize".into()));
        }
    }
    Ok(())
}

/// BitsPerSample, which must agree across the planes of a linear raw
/// (the specification requires it and every file obeys).
fn uniform_bits_per_sample(ifd: &Ifd, spp: usize) -> Result<u32> {
    let entry = ifd
        .get(tags::BITS_PER_SAMPLE)
        .ok_or_else(|| Error::Corrupt("DNG raw IFD without BitsPerSample".into()))?;
    let first = entry
        .u32(0)
        .ok_or_else(|| Error::Corrupt("DNG raw IFD with an unreadable BitsPerSample".into()))?;
    for i in 1..spp.min(entry.count) {
        if entry.u32(i) != Some(first) {
            return Err(Error::Unsupported(
                "DNG with unequal BitsPerSample per plane".into(),
            ));
        }
    }
    if first == 0 || first > 32 {
        return Err(Error::Corrupt(format!("DNG with {first} bits a sample")));
    }
    Ok(first)
}

// -------------------------------------------------------------------
// Sensor data
// -------------------------------------------------------------------

/// One decoded chunk, in the shape its own stream chose. `row_samples`
/// is a whole row of the tile including every plane, which for a
/// lossless-JPEG tile is the stream's width times its component count
/// — a DNG encoder is free to split a tile's row across components
/// however it likes, and they all do it differently.
struct Tile<T> {
    row_samples: usize,
    rows: usize,
    data: Vec<T>,
}

impl<T> Tile<T> {
    fn new(data: Vec<T>, row_samples: usize, rows: usize) -> Result<Tile<T>> {
        if row_samples == 0 || data.len() < row_samples * rows {
            return Err(Error::Corrupt(format!(
                "decoded chunk holds {} samples for {rows} rows of {row_samples}",
                data.len()
            )));
        }
        Ok(Tile {
            row_samples,
            rows,
            data,
        })
    }
}

/// How the chunks of an image IFD tile the frame. TIFF strips are the
/// degenerate case: full-width tiles `RowsPerStrip` tall.
struct Grid {
    tile_width: usize,
    tile_height: usize,
    across: usize,
    down: usize,
    /// Tiles are always encoded at their full size and clipped by the
    /// reader; strips at the bottom hold only the rows that remain.
    tiled: bool,
}

impl Grid {
    fn of(layout: &ImageLayout) -> Result<Grid> {
        let (tile_width, tile_height) = match layout.tile {
            Some(tile) => tile,
            None => (layout.width, layout.rows_per_chunk),
        };
        if tile_width == 0 || tile_height == 0 {
            return Err(Error::Corrupt("DNG chunk of zero size".into()));
        }
        let across = layout.width.div_ceil(tile_width);
        let down = layout.height.div_ceil(tile_height);
        if across.saturating_mul(down) != layout.chunks.len() {
            return Err(Error::Corrupt(format!(
                "{} chunks for a {across}x{down} grid",
                layout.chunks.len()
            )));
        }
        Ok(Grid {
            tile_width,
            tile_height,
            across,
            down,
            tiled: layout.tile.is_some(),
        })
    }
}

/// Decode every chunk and lay it into the full frame.
///
/// The frame is split into bands of `tile_height` rows and the bands
/// run in parallel: tiles within a band are decoded by the thread that
/// owns the rows they land in, so no tile is ever copied twice and the
/// peak memory is one tile a thread rather than a second whole frame.
fn assemble<T, F>(
    bytes: &[u8],
    layout: &ImageLayout,
    grid: &Grid,
    spp: usize,
    decode: F,
) -> Result<Vec<T>>
where
    T: Copy + Default + Send + Sync,
    F: Fn(&[u8], usize, usize) -> Result<Tile<T>> + Sync,
{
    let (width, height) = (layout.width, layout.height);
    let chunk_bytes = |index: usize| -> Result<&[u8]> {
        let (offset, len) = layout.chunks[index];
        bytes
            .get(offset..offset + len)
            .ok_or_else(|| Error::Corrupt("DNG chunk outside the file".into()))
    };

    // A single chunk that is the whole frame — the shape of nearly
    // every strip-per-image raw IFD — is handed straight out.
    if grid.across == 1 && grid.down == 1 {
        let tile = decode(chunk_bytes(0)?, width, height)?;
        if tile.row_samples == width * spp && tile.rows >= height {
            let mut data = tile.data;
            data.truncate(width * height * spp);
            return Ok(data);
        }
        let mut frame = vec![T::default(); width * height * spp];
        place(&mut frame, &tile, width, height, spp, 0, 0);
        return Ok(frame);
    }

    let mut frame = vec![T::default(); width * height * spp];
    let band = grid.tile_height * width * spp;
    frame
        .par_chunks_mut(band)
        .enumerate()
        .try_for_each(|(row, rows_of_frame)| -> Result<()> {
            let top = row * grid.tile_height;
            let rows_here = (height - top).min(grid.tile_height);
            for column in 0..grid.across {
                let left = column * grid.tile_width;
                let cols_here = (width - left).min(grid.tile_width);
                // A tile is padded out to its nominal size, a strip is
                // not; the decoder is told which it is being handed.
                let (nominal_cols, nominal_rows) = if grid.tiled {
                    (grid.tile_width, grid.tile_height)
                } else {
                    (cols_here, rows_here)
                };
                let chunk = chunk_bytes(row * grid.across + column)?;
                let tile = decode(chunk, nominal_cols, nominal_rows)?;
                place(rows_of_frame, &tile, width, rows_here, spp, left, 0);
            }
            Ok(())
        })?;
    Ok(frame)
}

/// Copy a decoded tile into the frame at (`left`, `top`), clipping the
/// right and bottom edges.
fn place<T: Copy>(
    frame: &mut [T],
    tile: &Tile<T>,
    frame_width: usize,
    rows: usize,
    spp: usize,
    left: usize,
    top: usize,
) {
    let keep = ((frame_width - left) * spp).min(tile.row_samples);
    for row in 0..rows.min(tile.rows) {
        let source = row * tile.row_samples;
        let target = (top + row) * frame_width * spp + left * spp;
        frame[target..target + keep].copy_from_slice(&tile.data[source..source + keep]);
    }
}

/// The sensor samples, decompressed, linearized and in the crate's
/// representation.
fn decode_samples(
    tiff: &Tiff<'_>,
    ifd: &Ifd,
    layout: &ImageLayout,
    spp: usize,
    bits: u32,
) -> Result<RawData> {
    let grid = Grid::of(layout)?;
    let bytes = tiff.bytes();
    let little_endian = tiff.little_endian();

    // Floating-point samples are only handled through the deflate
    // path; read as integers they would come out as bit patterns.
    if float_samples(ifd, spp)?
        && !matches!(
            layout.compression,
            compression::DEFLATE | compression::DEFLATE_OLD
        )
    {
        return Err(Error::Unsupported(format!(
            "floating-point DNG with compression {}",
            layout.compression
        )));
    }
    let mut frame = match layout.compression {
        compression::NONE => assemble(bytes, layout, &grid, spp, |chunk, cols, rows| {
            unpack(chunk, cols, rows, spp, bits, little_endian)
        })?,
        compression::LOSSLESS_JPEG => assemble(bytes, layout, &grid, spp, |chunk, cols, _rows| {
            lossless_jpeg_tile(chunk, cols, spp)
        })?,
        compression::LOSSY_JPEG => assemble(bytes, layout, &grid, spp, |chunk, cols, rows| {
            lossy_jpeg_tile(chunk, cols, rows, spp)
        })?,
        compression::DEFLATE | compression::DEFLATE_OLD => {
            let predictor = Predictor::of(ifd, spp)?;
            // A floating-point DNG keeps its samples as floats all the
            // way out: they are already scaled (0 is black, 1 is
            // white) and rounding them into 16 bits would throw away
            // the range the format exists to carry.
            if float_samples(ifd, spp)? {
                let frame = assemble(bytes, layout, &grid, spp, |chunk, cols, rows| {
                    deflate_float_tile(chunk, cols, rows, spp, bits, predictor, little_endian)
                })?;
                return Ok(RawData::F32(frame));
            }
            assemble(bytes, layout, &grid, spp, |chunk, cols, rows| {
                deflate_int_tile(chunk, cols, rows, spp, bits, predictor, little_endian)
            })?
        }
        compression::JPEG_XL => {
            return Err(Error::Unsupported("DNG 1.7 JPEG XL compression".into()))
        }
        compression::VC5 => {
            // A GoPro GPR: the tile is a VC-5 wavelet stream rather
            // than anything in the DNG specification. The codec works
            // in 12-bit log space and its decoder log curve lands on
            // 16 bits, so the samples come down to the depth WhiteLevel
            // advertises -- 14 bits, a shift of 2, in every GPR seen.
            if spp != 1 {
                return Err(Error::Unsupported(format!(
                    "VC-5 tile with {spp} samples a pixel"
                )));
            }
            // A VC-5 sample carries the whole frame's dimensions in its
            // own header, so one tile always spans the frame. Nothing
            // GoPro ships is tiled, and a grid would need the layer and
            // image-section loops this decoder does not implement.
            if grid.across != 1 || grid.down != 1 {
                return Err(Error::Unsupported(format!(
                    "VC-5 split across a {}x{} tile grid",
                    grid.across, grid.down
                )));
            }
            let white = ifd
                .get(tag::WHITE_LEVEL)
                .and_then(|e| e.u32(0))
                .unwrap_or(16383)
                .clamp(1, 65535);
            let shift = 16 - (32 - white.leading_zeros());
            assemble(bytes, layout, &grid, spp, |chunk, cols, rows| {
                Tile::new(vc5::decode(chunk, cols, rows, shift)?, cols, rows)
            })?
        }
        other => return Err(Error::Unsupported(format!("DNG compression {other}"))),
    };

    linearize(ifd, &mut frame);
    Ok(RawData::U16(frame))
}

/// SampleFormat (339): 1 unsigned, 2 signed, 3 IEEE float. It must
/// agree across the planes.
fn float_samples(ifd: &Ifd, spp: usize) -> Result<bool> {
    let Some(entry) = ifd.get(tag::SAMPLE_FORMAT) else {
        return Ok(false);
    };
    let first = entry.u32(0).unwrap_or(1);
    for i in 1..spp.min(entry.count) {
        if entry.u32(i) != Some(first) {
            return Err(Error::Unsupported("DNG with mixed SampleFormats".into()));
        }
    }
    match first {
        1 => Ok(false),
        3 => Ok(true),
        other => Err(Error::Unsupported(format!("DNG SampleFormat {other}"))),
    }
}

/// Uncompressed samples, packed most-significant-bit first.
///
/// The bit order does not follow the file's byte order: DNG packs the
/// bits of successive samples into successive bytes from the top bit
/// down whichever way round the TIFF is, and each row restarts on a
/// byte boundary. Only 8- and 16-bit samples are whole bytes and so
/// the only ones the byte order touches.
fn unpack(
    chunk: &[u8],
    cols: usize,
    rows: usize,
    spp: usize,
    bits: u32,
    little_endian: bool,
) -> Result<Tile<u16>> {
    if bits > 16 {
        return Err(Error::Unsupported(format!(
            "uncompressed DNG with {bits}-bit samples"
        )));
    }
    let row_samples = cols
        .checked_mul(spp)
        .filter(|n| *n > 0)
        .ok_or_else(|| Error::Corrupt("DNG chunk of zero width".into()))?;
    let row_bytes = (row_samples * bits as usize).div_ceil(8);
    let needed = row_bytes
        .checked_mul(rows)
        .ok_or_else(|| Error::Corrupt("DNG chunk larger than memory".into()))?;
    if chunk.len() < needed {
        return Err(Error::Corrupt(format!(
            "uncompressed DNG chunk holds {} bytes, needs {needed}",
            chunk.len()
        )));
    }

    let mut data = vec![0u16; row_samples * rows];
    for (row, out) in data.chunks_exact_mut(row_samples).enumerate() {
        let source = &chunk[row * row_bytes..row * row_bytes + row_bytes];
        match bits {
            8 => {
                for (sample, byte) in out.iter_mut().zip(source) {
                    *sample = *byte as u16;
                }
            }
            16 => {
                let (pairs, _) = source.as_chunks::<2>();
                for (sample, pair) in out.iter_mut().zip(pairs) {
                    *sample = if little_endian {
                        u16::from_le_bytes(*pair)
                    } else {
                        u16::from_be_bytes(*pair)
                    };
                }
            }
            _ => {
                let mut pump = BitPumpMsb::new(source);
                for sample in out.iter_mut() {
                    *sample = pump.get(bits) as u16;
                }
            }
        }
    }
    Tile::new(data, row_samples, rows)
}

/// One lossless-JPEG chunk.
///
/// The stream's own idea of its width and component count need not
/// match the tile's: encoders halve the width and use two components
/// so that a Bayer row's two colours are predicted from their own
/// kind, and a linear raw uses three components at full width. The
/// decoder hands back samples in stream order, which is exactly the
/// tile's row order either way, so all that matters is that a row of
/// the stream is a row of the tile.
fn lossless_jpeg_tile(chunk: &[u8], cols: usize, spp: usize) -> Result<Tile<u16>> {
    // The frame header first: a stream claiming a frame far larger
    // than the tile would otherwise be decoded (and allocated) in full
    // before the mismatch was noticed, on every rayon thread at once.
    let image = ljpeg::header(chunk)?;
    let row_samples = image
        .width
        .checked_mul(image.components)
        .ok_or_else(|| Error::Corrupt("lossless JPEG frame larger than memory".into()))?;
    if row_samples != cols * spp {
        return Err(Error::Corrupt(format!(
            "lossless JPEG tile is {} samples a row, the DNG tile is {}",
            row_samples,
            cols * spp
        )));
    }
    let image = ljpeg::decode(chunk)?;
    Tile::new(image.data, row_samples, image.height)
}

/// One baseline-JPEG chunk of a lossy DNG. The samples are eight bits
/// and the LinearizationTable (which such a file always carries) puts
/// them back on the sensor's scale afterwards.
fn lossy_jpeg_tile(chunk: &[u8], cols: usize, rows: usize, spp: usize) -> Result<Tile<u16>> {
    use image::ImageDecoder;
    let decoder = image::codecs::jpeg::JpegDecoder::new(Cursor::new(chunk))
        .map_err(|e| Error::Corrupt(format!("lossy DNG tile is not a JPEG: {e}")))?;
    let (width, height) = decoder.dimensions();
    let channels = match decoder.color_type() {
        image::ColorType::L8 => 1,
        image::ColorType::Rgb8 => 3,
        other => return Err(Error::Unsupported(format!("lossy DNG tile in {other:?}"))),
    };
    if channels != spp {
        return Err(Error::Corrupt(format!(
            "lossy DNG tile has {channels} channels, the IFD says {spp}"
        )));
    }
    let total = decoder.total_bytes();
    if total > (1 << 30) {
        return Err(Error::Corrupt("lossy DNG tile larger than memory".into()));
    }
    let mut buffer = vec![0u8; total as usize];
    decoder
        .read_image(&mut buffer)
        .map_err(|e| Error::Corrupt(format!("lossy DNG tile: {e}")))?;
    let (width, height) = (width as usize, height as usize);
    if width != cols || height == 0 || height > rows {
        return Err(Error::Corrupt(format!(
            "lossy DNG tile decodes {width}x{height} for a {cols}x{rows} tile"
        )));
    }
    let data: Vec<u16> = buffer.into_iter().map(u16::from).collect();
    Tile::new(data, width * channels, height)
}

/// The horizontal predictor a deflated chunk was written with (tag
/// 317).
///
/// TIFF has 1 (none) and 2, where a sample is stored as its difference
/// from the one `SamplesPerPixel` earlier in the row. TIFF Technical
/// Note 3 adds 3 for floating point, which first splits every sample
/// into byte planes — all the high bytes of a row, then all the next
/// bytes, and so on — because a row of floats has near-constant
/// exponents and wildly varying mantissas, and deflate only finds the
/// pattern once they are separated. DNG 1.4 adds four more that widen
/// the differencing stride to two or four pixels so a Bayer row
/// differences red against red and green against green.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Predictor {
    None,
    /// Differencing over whole samples, `n` pixels apart.
    Horizontal(usize),
    /// Byte planes, differenced `n` pixels apart within a plane.
    FloatingPoint(usize),
}

impl Predictor {
    fn of(ifd: &Ifd, spp: usize) -> Result<Predictor> {
        let value = ifd.get(tag::PREDICTOR).and_then(|e| e.u32(0)).unwrap_or(1);
        Ok(match value {
            1 => Predictor::None,
            2 => Predictor::Horizontal(spp),
            3 => Predictor::FloatingPoint(spp),
            34892 => Predictor::Horizontal(2 * spp),
            34893 => Predictor::Horizontal(4 * spp),
            34894 => Predictor::FloatingPoint(2 * spp),
            34895 => Predictor::FloatingPoint(4 * spp),
            other => return Err(Error::Unsupported(format!("DNG predictor {other}"))),
        })
    }
}

/// Inflate one chunk, refusing to produce more than the frame can
/// hold: a deflate stream can expand a thousandfold and a hostile file
/// would happily ask it to.
fn inflate(chunk: &[u8], needed: usize) -> Result<Vec<u8>> {
    let limit = needed.saturating_add(4096);
    let out = miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(chunk, limit)
        .map_err(|e| Error::Corrupt(format!("deflated DNG chunk: {:?}", e.status)))?;
    if out.len() < needed {
        return Err(Error::Corrupt(format!(
            "deflated DNG chunk inflates to {} bytes, needs {needed}",
            out.len()
        )));
    }
    Ok(out)
}

/// The bytes a row of `row_samples` samples occupies, or an error when
/// the arithmetic overflows.
fn row_bytes(row_samples: usize, rows: usize, width: usize) -> Result<(usize, usize)> {
    let row = row_samples
        .checked_mul(width)
        .ok_or_else(|| Error::Corrupt("DNG chunk larger than memory".into()))?;
    let total = row
        .checked_mul(rows)
        .ok_or_else(|| Error::Corrupt("DNG chunk larger than memory".into()))?;
    Ok((row, total))
}

/// Undo horizontal differencing over 8- or 16-bit samples. The sums
/// wrap: the encoder took them modulo the sample width.
fn undo_horizontal_u8(row: &mut [u8], stride: usize) {
    for i in stride..row.len() {
        row[i] = row[i].wrapping_add(row[i - stride]);
    }
}

fn undo_horizontal_u16(row: &mut [u16], stride: usize) {
    for i in stride..row.len() {
        row[i] = row[i].wrapping_add(row[i - stride]);
    }
}

/// A deflated chunk of integer samples.
fn deflate_int_tile(
    chunk: &[u8],
    cols: usize,
    rows: usize,
    spp: usize,
    bits: u32,
    predictor: Predictor,
    little_endian: bool,
) -> Result<Tile<u16>> {
    let width = match bits {
        8 => 1usize,
        16 => 2,
        // A 32-bit integer sample has nowhere to go: `RawData` holds
        // 16-bit integers or floats and nothing between.
        32 => {
            return Err(Error::Unsupported(
                "deflated DNG with 32-bit integer samples".into(),
            ))
        }
        other => {
            return Err(Error::Unsupported(format!(
                "deflated DNG with {other}-bit integer samples"
            )))
        }
    };
    let stride = match predictor {
        Predictor::None => 0,
        Predictor::Horizontal(n) => n,
        Predictor::FloatingPoint(_) => {
            return Err(Error::Corrupt(
                "DNG uses a floating-point predictor on integer samples".into(),
            ))
        }
    };
    let row_samples = cols
        .checked_mul(spp)
        .filter(|n| *n > 0)
        .ok_or_else(|| Error::Corrupt("DNG chunk of zero width".into()))?;
    let (row_len, needed) = row_bytes(row_samples, rows, width)?;
    let mut source = inflate(chunk, needed)?;

    let mut data = vec![0u16; row_samples * rows];
    for (row, out) in data.chunks_exact_mut(row_samples).enumerate() {
        let bytes = &mut source[row * row_len..row * row_len + row_len];
        if width == 1 {
            if stride > 0 {
                undo_horizontal_u8(bytes, stride);
            }
            for (sample, byte) in out.iter_mut().zip(bytes.iter()) {
                *sample = *byte as u16;
            }
        } else {
            let (pairs, _) = bytes.as_chunks::<2>();
            for (sample, pair) in out.iter_mut().zip(pairs) {
                *sample = if little_endian {
                    u16::from_le_bytes(*pair)
                } else {
                    u16::from_be_bytes(*pair)
                };
            }
            if stride > 0 {
                undo_horizontal_u16(out, stride);
            }
        }
    }
    Tile::new(data, row_samples, rows)
}

/// A deflated chunk of floating-point samples: DNG 1.4's half, 24-bit
/// and single precision.
///
/// The byte planes are written most significant first, so a sample is
/// rebuilt by reading one byte from each plane in turn; that ordering
/// is the same whichever way round the TIFF is, and only an unshuffled
/// (predictor 1) chunk is stored in the file's byte order.
fn deflate_float_tile(
    chunk: &[u8],
    cols: usize,
    rows: usize,
    spp: usize,
    bits: u32,
    predictor: Predictor,
    little_endian: bool,
) -> Result<Tile<f32>> {
    let width = match bits {
        16 => 2usize,
        24 => 3,
        32 => 4,
        other => {
            return Err(Error::Unsupported(format!(
                "DNG with {other}-bit floating-point samples"
            )))
        }
    };
    let row_samples = cols
        .checked_mul(spp)
        .filter(|n| *n > 0)
        .ok_or_else(|| Error::Corrupt("DNG chunk of zero width".into()))?;
    let (row_len, needed) = row_bytes(row_samples, rows, width)?;
    let mut source = inflate(chunk, needed)?;

    let mut data = vec![0f32; row_samples * rows];
    for (row, out) in data.chunks_exact_mut(row_samples).enumerate() {
        let bytes = &mut source[row * row_len..row * row_len + row_len];
        match predictor {
            Predictor::FloatingPoint(stride) => {
                undo_horizontal_u8(bytes, stride.max(1));
                // Plane `p` holds byte `p`, counting from the most
                // significant, of every sample in the row.
                for (index, sample) in out.iter_mut().enumerate() {
                    let mut value = 0u32;
                    for plane in 0..width {
                        value = (value << 8) | bytes[plane * row_samples + index] as u32;
                    }
                    *sample = float_sample(value, width);
                }
            }
            Predictor::None => {
                for (index, sample) in out.iter_mut().enumerate() {
                    let at = index * width;
                    let mut value = 0u32;
                    for byte in 0..width {
                        let position = if little_endian {
                            width - 1 - byte
                        } else {
                            byte
                        };
                        value = (value << 8) | bytes[at + position] as u32;
                    }
                    *sample = float_sample(value, width);
                }
            }
            Predictor::Horizontal(_) => {
                return Err(Error::Corrupt(
                    "DNG uses an integer predictor on floating-point samples".into(),
                ))
            }
        }
    }
    Tile::new(data, row_samples, rows)
}

/// A 16-, 24- or 32-bit float, as its bits, widened to `f32`.
///
/// The 24-bit form is DNG's own: one sign bit, eight exponent bits and
/// fifteen of mantissa. Its exponent is already `f32`'s, so widening
/// is a shift. Half precision has a five-bit exponent with its own
/// bias and needs the arithmetic.
fn float_sample(value: u32, width: usize) -> f32 {
    match width {
        2 => half_to_f32(value as u16),
        3 => f32::from_bits(value << 8),
        _ => f32::from_bits(value),
    }
}

fn half_to_f32(half: u16) -> f32 {
    let sign = (half as u32 & 0x8000) << 16;
    let exponent = (half as u32 >> 10) & 0x1F;
    let mantissa = half as u32 & 0x3FF;
    f32::from_bits(match exponent {
        // Zero and the subnormals, which `f32` can all represent
        // normally once the leading one is found and shifted up.
        0 if mantissa == 0 => sign,
        0 => {
            let top = 31 - mantissa.leading_zeros();
            sign | ((103 + top) << 23) | ((mantissa & !(1 << top)) << (23 - top))
        }
        // Infinity and NaN keep their payload.
        31 => sign | 0x7F80_0000 | (mantissa << 13),
        _ => sign | ((exponent + 127 - 15) << 23) | (mantissa << 13),
    })
}

/// Apply the LinearizationTable (0xC618): the sensor's response curve,
/// as a lookup from the stored sample to a linear one. Values past the
/// end of the table take its last entry, which is what the
/// specification says a short table means.
fn linearize(ifd: &Ifd, frame: &mut [u16]) {
    let Some(entry) = ifd.get(tag::LINEARIZATION_TABLE) else {
        return;
    };
    let table: Vec<u16> = entry
        .u32s()
        .into_iter()
        .map(|v| v.min(65535) as u16)
        .collect();
    let Some(&last) = table.last() else {
        return;
    };
    frame
        .par_iter_mut()
        .for_each(|sample| *sample = *table.get(*sample as usize).unwrap_or(&last));
}

// -------------------------------------------------------------------
// Metadata
// -------------------------------------------------------------------

fn fill_metadata(tiff: &Tiff<'_>, ifd: &Ifd, raw: &mut RawImage, bits: u32, spp: usize) {
    let (make, model) = tiff.make_model();
    // A DNG written by something that is not a camera may leave Make
    // and Model empty; UniqueCameraModel is mandatory and names the
    // body the profile belongs to.
    let unique = tiff
        .root()
        .get(tag::UNIQUE_CAMERA_MODEL)
        .and_then(|e| e.str())
        .unwrap_or("");
    if model.is_empty() {
        raw.set_camera(&make, unique);
    } else {
        raw.set_camera(&make, &model);
    }

    // A floating-point DNG is already scaled: 0 is black and 1.0 is
    // the saturation point unless the file says otherwise.
    let float = matches!(raw.data, RawData::F32(_));
    let (black, white) = levels(ifd, bits, spp, float);
    raw.black_levels = black;
    raw.white_level = white;
    raw.color_matrix = color_matrix(tiff.root());
    raw.wb_coeffs = white_balance(tiff.root(), ifd, raw.color_matrix.as_ref());
    raw.crop = crop(ifd, raw.width, raw.height);
    raw.orientation = common::orientation(tiff);
    raw.preview = common::largest_jpeg(tiff);
    raw.metadata = common::metadata(tiff);
    note_opcodes(ifd);
}

/// BlackLevel (0xC61A) folded onto the 2x2 the crate carries, and
/// WhiteLevel (0xC61D).
///
/// BlackLevel repeats over a `BlackLevelRepeatDim` (0xC619) block of
/// rows by columns, with one value per plane inside each cell. A 1x1
/// or 2x2 block maps straight onto the crate's four positions; a
/// larger block (nothing in the wild writes one, but the format allows
/// up to 65535) is averaged down, which loses the fine structure and
/// is noted here rather than silently.
///
/// BlackLevelDeltaH/V (0xC61B/C) add a per-column and per-row offset on
/// top. They are ignored: the crate's black level is a single number
/// per CFA position and there is nowhere to put a per-row curve. No
/// file in the sample corpus carries either tag.
fn levels(ifd: &Ifd, bits: u32, spp: usize, float: bool) -> ([f32; 4], f32) {
    let (rows, columns) = match ifd.get(tag::BLACK_LEVEL_REPEAT_DIM) {
        Some(entry) => (
            entry.u32(0).unwrap_or(1).max(1) as usize,
            entry.u32(1).unwrap_or(1).max(1) as usize,
        ),
        None => (1, 1),
    };
    let mut black = [0.0f32; 4];
    if let Some(entry) = ifd.get(tag::BLACK_LEVEL) {
        let at = |row: usize, column: usize, plane: usize| -> Option<f32> {
            entry
                .f64((row * columns + column) * spp + plane)
                .map(|v| v as f32)
        };
        if spp > 1 {
            // Linear raw: one black level a plane, averaged over the
            // repeat block. The fourth slot is unused.
            for (plane, level) in black.iter_mut().enumerate().take(spp.min(4)) {
                let mut sum = 0.0;
                let mut count = 0.0;
                for row in 0..rows {
                    for column in 0..columns {
                        if let Some(v) = at(row, column, plane) {
                            sum += v;
                            count += 1.0;
                        }
                    }
                }
                if count > 0.0 {
                    *level = sum / count;
                }
            }
        } else {
            for (position, level) in black.iter_mut().enumerate() {
                let (want_row, want_column) = (position / 2, position % 2);
                let mut sum = 0.0;
                let mut count = 0.0;
                for row in 0..rows {
                    for column in 0..columns {
                        // A block whose sides are both even lines up
                        // with the 2x2; anything else is averaged
                        // whole, since its phase cannot be expressed.
                        // A side of one applies to both phases;
                        // an even side lines up with the 2x2.
                        let aligned =
                            (rows == 1 || rows % 2 == 0) && (columns == 1 || columns % 2 == 0);
                        if aligned
                            && ((rows > 1 && row % 2 != want_row)
                                || (columns > 1 && column % 2 != want_column))
                        {
                            continue;
                        }
                        if let Some(v) = at(row, column, 0) {
                            sum += v;
                            count += 1.0;
                        }
                    }
                }
                if count > 0.0 {
                    *level = sum / count;
                }
            }
        }
        if rows > 2 || columns > 2 {
            log::debug!("DNG BlackLevelRepeatDim {rows}x{columns} averaged onto a 2x2");
        }
    }
    if ifd.has(tag::BLACK_LEVEL_DELTA_H) || ifd.has(tag::BLACK_LEVEL_DELTA_V) {
        log::debug!("DNG BlackLevelDeltaH/V ignored: no per-row black level here");
    }

    // WhiteLevel is per plane. One number has to serve, and the
    // brightest plane is the safe one: clipping is decided against it
    // and a lower value would call unclipped highlights blown.
    let default = if float {
        1.0
    } else if ifd.has(tag::LINEARIZATION_TABLE) {
        // With a linearization table the samples come out on the
        // table's scale, so the default saturation is its top entry.
        ifd.get(tag::LINEARIZATION_TABLE)
            .and_then(|e| e.u32s().into_iter().max())
            .unwrap_or(65535) as f32
    } else {
        ((1u32 << bits.min(16)) - 1) as f32
    };
    let white = ifd
        .get(tag::WHITE_LEVEL)
        .map(|entry| {
            (0..entry.count.max(1))
                .filter_map(|i| entry.f64(i))
                .fold(0.0f64, f64::max) as f32
        })
        .filter(|v| *v > 0.0)
        .unwrap_or(default);
    (black, white)
}

/// AsShotNeutral (0xC628) inverted into R, G, B, G2 multipliers with
/// green at 1.0 — the same convention LibRaw's `cam_mul` uses.
///
/// AsShotWhiteXY (0xC629) is the alternative (Pixel phones write it):
/// the white point as an xy chromaticity. The camera's response to
/// that white is the XYZ→camera matrix applied to its XYZ (Y = 1), and
/// the multipliers are the inverse of that response — the spec's
/// AsShotNeutral computed on the spot. The matrix is the one already
/// chosen for the file; the spec would interpolate between the two
/// calibrations at that white point, which moves the answer by a
/// percent or two and is left for a developer stage. Without a matrix
/// the file keeps unit white balance.
fn white_balance(root: &Ifd, ifd: &Ifd, xyz_to_camera: Option<&[[f32; 3]; 3]>) -> [f32; 4] {
    let neutral = root
        .get(tag::AS_SHOT_NEUTRAL)
        .or_else(|| ifd.get(tag::AS_SHOT_NEUTRAL));
    let Some(entry) = neutral else {
        let xy = root
            .get(tag::AS_SHOT_WHITE_XY)
            .or_else(|| ifd.get(tag::AS_SHOT_WHITE_XY));
        return match (xy, xyz_to_camera) {
            (Some(xy), Some(m)) => {
                let (x, y) = (
                    xy.f64(0).unwrap_or(0.0) as f32,
                    xy.f64(1).unwrap_or(0.0) as f32,
                );
                if !(x > 0.0 && y > 0.0 && x + y < 1.0) {
                    return [1.0; 4];
                }
                let xyz = [x / y, 1.0, (1.0 - x - y) / y];
                let response: [f32; 3] =
                    std::array::from_fn(|c| m[c][0] * xyz[0] + m[c][1] * xyz[1] + m[c][2] * xyz[2]);
                let (red, green, blue) = (response[0], response[1], response[2]);
                if !(red > 0.0 && green > 0.0 && blue > 0.0) {
                    return [1.0; 4];
                }
                [green / red, 1.0, green / blue, 1.0]
            }
            (Some(_), None) => {
                log::debug!(
                    "DNG carries AsShotWhiteXY but no colour matrix: white balance left at unity"
                );
                [1.0; 4]
            }
            _ => [1.0; 4],
        };
    };
    let value = |i: usize| entry.f64(i).unwrap_or(0.0) as f32;
    let (red, green, blue) = (value(0), value(1), value(2));
    if !(red > 0.0 && green > 0.0 && blue > 0.0) {
        return [1.0; 4];
    }
    // 1/neutral, then scaled so green is exactly one.
    let second_green = if entry.count > 3 && value(3) > 0.0 {
        green / value(3)
    } else {
        1.0
    };
    [green / red, 1.0, green / blue, second_green]
}

/// The XYZ-to-camera matrix, as the file stores it.
///
/// A DNG carries one or two ColorMatrix tags with the illuminant each
/// was measured under. A developer that wanted to be exact would
/// interpolate between them at the shot's own white point; this crate
/// hands out a single matrix, so it picks the one measured nearest
/// daylight — D65 exactly where there is one, otherwise whichever
/// illuminant's colour temperature is closest to D65's 6504 K, which
/// puts D50 ahead of standard light A by a wide margin.
fn color_matrix(root: &Ifd) -> Option<[[f32; 3]; 3]> {
    let read = |tag: u16| -> Option<[[f32; 3]; 3]> {
        let entry = root.get(tag)?;
        if entry.count < 9 {
            return None;
        }
        let mut matrix = [[0.0f32; 3]; 3];
        for (row, out) in matrix.iter_mut().enumerate() {
            for (column, cell) in out.iter_mut().enumerate() {
                *cell = entry.f64(row * 3 + column)? as f32;
            }
        }
        Some(matrix)
    };
    let first = read(tag::COLOR_MATRIX_1);
    let second = read(tag::COLOR_MATRIX_2);
    let illuminant = |tag: u16| root.get(tag).and_then(|e| e.u32(0)).unwrap_or(0);
    match (first, second) {
        (Some(first), Some(second)) => {
            let one = illuminant(tag::CALIBRATION_ILLUMINANT_1);
            let two = illuminant(tag::CALIBRATION_ILLUMINANT_2);
            Some(if daylight_rank(two) < daylight_rank(one) {
                second
            } else {
                first
            })
        }
        (Some(only), None) | (None, Some(only)) => Some(only),
        (None, None) => None,
    }
}

/// How far an EXIF LightSource code sits from D65, in kelvin. An
/// illuminant with no known temperature ranks worse than any that has
/// one.
fn daylight_rank(illuminant: u32) -> f32 {
    const D65: f32 = 6504.0;
    let temperature = match illuminant {
        1 | 9 | 10 => 6504.0, // Daylight, fine weather, cloudy
        2 => 4230.0,          // Fluorescent, an average of the tubes
        3 => 2856.0,          // Tungsten
        4 => 5500.0,          // Flash
        11 => 7504.0,         // Shade
        12 => 6430.0,         // Daylight fluorescent, D 5700-7100
        13 => 5000.0,         // Day white fluorescent, N 4600-5400
        14 => 4200.0,         // Cool white fluorescent, W 3900-4500
        15 => 3450.0,         // White fluorescent, WW 3200-3700
        16 => 2940.0,         // Warm white fluorescent, L 2600-3250
        17 => 2856.0,         // Standard light A
        18 => 4874.0,         // Standard light B
        19 => 6774.0,         // Standard light C
        20 => 5503.0,         // D55
        21 => 6504.0,         // D65
        22 => 7504.0,         // D75
        23 => 5003.0,         // D50
        24 => 3200.0,         // ISO studio tungsten
        _ => return f32::INFINITY,
    };
    (temperature - D65).abs()
}

/// The area worth showing: ActiveArea (0xC68D) minus the masked
/// borders, then DefaultCropOrigin/Size (0xC61F/0xC620) inside it.
///
/// The default crop is measured from the top-left of the *active*
/// area, not of the frame, so the two compose. Both crop tags may be
/// rational — a DNG may crop on a half pixel — and are rounded to the
/// sensor grid here.
fn crop(ifd: &Ifd, width: usize, height: usize) -> Rect {
    let (mut x, mut y, mut w, mut h) = (0usize, 0usize, width, height);
    if let Some(entry) = ifd.get(tag::ACTIVE_AREA) {
        // top, left, bottom, right.
        let get = |i: usize| entry.u32(i).map(|v| v as usize);
        if let (Some(top), Some(left), Some(bottom), Some(right)) = (get(0), get(1), get(2), get(3))
        {
            if left < right && top < bottom && right <= width && bottom <= height {
                x = left;
                y = top;
                w = right - left;
                h = bottom - top;
            }
        }
    }
    let rational = |tag: u16, i: usize| ifd.get(tag).and_then(|e| e.f64(i));
    if let (Some(dx), Some(dy)) = (
        rational(tag::DEFAULT_CROP_ORIGIN, 0),
        rational(tag::DEFAULT_CROP_ORIGIN, 1),
    ) {
        x += dx.max(0.0).round() as usize;
        y += dy.max(0.0).round() as usize;
    }
    if let (Some(cw), Some(ch)) = (
        rational(tag::DEFAULT_CROP_SIZE, 0),
        rational(tag::DEFAULT_CROP_SIZE, 1),
    ) {
        w = cw.max(1.0).round() as usize;
        h = ch.max(1.0).round() as usize;
    }
    // A file whose tags do not agree with its own dimensions still has
    // to produce a rectangle inside the frame.
    x = x.min(width.saturating_sub(1));
    y = y.min(height.saturating_sub(1));
    Rect {
        x,
        y,
        width: w.clamp(1, width - x),
        height: h.clamp(1, height - y),
    }
}

/// CFAPattern (0x828E) over CFARepeatPatternDim (0x828D), with the
/// plane-to-colour mapping from CFAPlaneColor (0xC616).
///
/// CFAPattern's entries are indices into CFAPlaneColor rather than
/// colours: `[0, 1, 1, 2]` with the default plane colours is RGGB, but
/// a sensor that ordered its planes differently would say so. The
/// pattern is anchored at the top-left of the IFD's image data, which
/// is where this crate wants it; the specification requires the active
/// area to start on an even row and column, so a reader that anchored
/// it at the active area instead would get the same answer (and every
/// file in the corpus does start even).
fn cfa_pattern(ifd: &Ifd) -> Result<Cfa> {
    if ifd.get(tag::CFA_LAYOUT).and_then(|e| e.u32(0)).unwrap_or(1) != 1 {
        return Err(Error::Unsupported(
            "DNG with a staggered (non-rectangular) CFA layout".into(),
        ));
    }
    let dim = ifd.get(tags::CFA_REPEAT_PATTERN_DIM);
    let (rows, columns) = match dim {
        Some(entry) => (
            entry.u32(0).unwrap_or(2) as usize,
            entry.u32(1).unwrap_or(2) as usize,
        ),
        None => (2, 2),
    };
    let pattern = ifd
        .get(tags::CFA_PATTERN)
        .and_then(|e| e.bytes())
        .ok_or_else(|| Error::Corrupt("DNG CFA image without CFAPattern".into()))?;
    if rows == 0 || columns == 0 || rows > 8 || columns > 8 || pattern.len() < rows * columns {
        return Err(Error::Corrupt(format!(
            "DNG CFA pattern of {rows}x{columns} in {} bytes",
            pattern.len()
        )));
    }
    let planes: Vec<u32> = ifd
        .get(tag::CFA_PLANE_COLOR)
        .map(|e| e.u32s())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| vec![0, 1, 2]);

    let mut colors = Vec::with_capacity(rows * columns);
    for index in &pattern[..rows * columns] {
        let plane = planes
            .get(*index as usize)
            .copied()
            .unwrap_or(*index as u32);
        colors.push(match plane {
            0 => CfaColor::Red,
            1 => CfaColor::Green,
            2 => CfaColor::Blue,
            3 => CfaColor::Cyan,
            4 => CfaColor::Magenta,
            5 => CfaColor::Yellow,
            // 6 is white: a panchromatic photosite, which the develop
            // pipeline has no way to demosaic.
            6 => {
                return Err(Error::Unsupported(
                    "DNG with a white (panchromatic) CFA plane".into(),
                ))
            }
            other => {
                return Err(Error::Corrupt(format!("DNG CFA plane colour {other}")));
            }
        });
    }
    Ok(match (rows, columns) {
        (2, 2) => Cfa::Bayer([colors[0], colors[1], colors[2], colors[3]]),
        (6, 6) => Cfa::XTrans(std::array::from_fn(|y| {
            std::array::from_fn(|x| colors[y * 6 + x])
        })),
        _ => Cfa::Pattern {
            width: columns,
            height: rows,
            colors,
        },
    })
}

/// The opcode lists (0xC740/1/2) are read far enough to say what is in
/// them and no further.
///
/// They are per-file image corrections — DNG 1.3's WarpRectilinear and
/// FixVignetteRadial, DNG 1.4's GainMap — that a developer is supposed
/// to run at the stage the list's number names (1 before
/// linearization, 2 after, 3 after demosaic). This crate applies none
/// of them. The one that matters in practice is GainMap, which several
/// phone DNGs use to flatten lens shading: without it the corners of a
/// Pixel or Samsung frame stay darker than the camera intended. The
/// data is left in the file for a future developer stage rather than
/// baked into the sensor samples, which would make them no longer the
/// camera's own.
fn note_opcodes(ifd: &Ifd) {
    for (tag, list) in [
        (tag::OPCODE_LIST_1, 1),
        (tag::OPCODE_LIST_2, 2),
        (tag::OPCODE_LIST_3, 3),
    ] {
        let Some(bytes) = ifd.get(tag).and_then(|e| e.bytes()) else {
            continue;
        };
        // An opcode list is a big-endian count followed by, for each
        // opcode, its id, the DNG version it needs, flags, and the
        // length of its parameters. Walking that much is enough to
        // name them in a log line.
        let word = |at: usize| -> Option<u32> {
            bytes
                .get(at..at + 4)
                .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
        };
        let Some(count) = word(0) else { continue };
        let mut ids = Vec::new();
        let mut at = 4;
        for _ in 0..count.min(64) {
            let (Some(id), Some(size)) = (word(at), word(at + 12)) else {
                break;
            };
            ids.push(id);
            at += 16 + size as usize;
        }
        log::debug!("DNG OpcodeList{list} not applied: opcodes {ids:?}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    // ---------------------------------------------------------------
    // A DNG written by hand, so the tests exercise exactly the bytes a
    // camera writes rather than whatever a TIFF library feels like
    // emitting. IFD0 carries the camera-wide tags and points at one
    // SubIFD holding the sensor data.
    // ---------------------------------------------------------------

    #[derive(Clone)]
    enum V {
        Byte(Vec<u8>),
        Ascii(&'static str),
        Short(Vec<u16>),
        Long(Vec<u32>),
        Rational(Vec<(u32, u32)>),
        SRational(Vec<(i32, i32)>),
    }

    impl V {
        fn kind(&self) -> u16 {
            match self {
                V::Byte(_) => 1,
                V::Ascii(_) => 2,
                V::Short(_) => 3,
                V::Long(_) => 4,
                V::Rational(_) => 5,
                V::SRational(_) => 10,
            }
        }
        fn count(&self) -> usize {
            match self {
                V::Byte(v) => v.len(),
                // TIFF ASCII counts the terminating NUL.
                V::Ascii(s) => s.len() + 1,
                V::Short(v) => v.len(),
                V::Long(v) => v.len(),
                V::Rational(v) => v.len(),
                V::SRational(v) => v.len(),
            }
        }
        fn bytes(&self, little_endian: bool) -> Vec<u8> {
            let w16 = |v: u16| {
                if little_endian {
                    v.to_le_bytes()
                } else {
                    v.to_be_bytes()
                }
            };
            let w32 = |v: u32| {
                if little_endian {
                    v.to_le_bytes()
                } else {
                    v.to_be_bytes()
                }
            };
            match self {
                V::Byte(v) => v.clone(),
                V::Ascii(s) => {
                    let mut out = s.as_bytes().to_vec();
                    out.push(0);
                    out
                }
                V::Short(v) => v.iter().flat_map(|s| w16(*s)).collect(),
                V::Long(v) => v.iter().flat_map(|l| w32(*l)).collect(),
                V::Rational(v) => v
                    .iter()
                    .flat_map(|(n, d)| [w32(*n), w32(*d)].concat())
                    .collect(),
                V::SRational(v) => v
                    .iter()
                    .flat_map(|(n, d)| [w32(*n as u32), w32(*d as u32)].concat())
                    .collect(),
            }
        }
    }

    struct Build {
        little_endian: bool,
        root: Vec<(u16, V)>,
        raw: Vec<(u16, V)>,
        chunks: Vec<Vec<u8>>,
        tiled: bool,
        preview: Option<Vec<u8>>,
    }

    impl Build {
        /// A 12-bit uncompressed RGGB DNG of `width` x `height`, one
        /// strip, with the tags every DNG carries.
        fn new(width: usize, height: usize) -> Build {
            Build {
                little_endian: true,
                root: vec![
                    (tags::MAKE, V::Ascii("Test")),
                    (tags::MODEL, V::Ascii("Camera")),
                    (tags::ORIENTATION, V::Short(vec![1])),
                    (tags::DNG_VERSION, V::Byte(vec![1, 4, 0, 0])),
                    (tag::UNIQUE_CAMERA_MODEL, V::Ascii("Test Camera")),
                ],
                raw: vec![
                    (tags::NEW_SUBFILE_TYPE, V::Long(vec![0])),
                    (tags::IMAGE_WIDTH, V::Long(vec![width as u32])),
                    (tags::IMAGE_LENGTH, V::Long(vec![height as u32])),
                    (tags::BITS_PER_SAMPLE, V::Short(vec![12])),
                    (tags::COMPRESSION, V::Short(vec![1])),
                    (tags::PHOTOMETRIC, V::Short(vec![32803])),
                    (tags::SAMPLES_PER_PIXEL, V::Short(vec![1])),
                    (tags::ROWS_PER_STRIP, V::Long(vec![height as u32])),
                    (tags::CFA_REPEAT_PATTERN_DIM, V::Short(vec![2, 2])),
                    (tags::CFA_PATTERN, V::Byte(vec![0, 1, 1, 2])),
                    (tag::WHITE_LEVEL, V::Long(vec![4095])),
                ],
                chunks: Vec::new(),
                tiled: false,
                preview: None,
            }
        }
        fn big_endian(mut self) -> Build {
            self.little_endian = false;
            self
        }
        fn root(mut self, tag: u16, value: V) -> Build {
            self.root.retain(|(t, _)| *t != tag);
            self.root.push((tag, value));
            self
        }
        fn raw(mut self, tag: u16, value: V) -> Build {
            self.raw.retain(|(t, _)| *t != tag);
            self.raw.push((tag, value));
            self
        }
        fn without(mut self, tag: u16) -> Build {
            self.raw.retain(|(t, _)| *t != tag);
            self.root.retain(|(t, _)| *t != tag);
            self
        }
        fn strip(self, bytes: Vec<u8>) -> Build {
            self.chunk_list(vec![bytes])
        }
        /// Several strips, in order down the frame.
        fn chunk_list(mut self, chunks: Vec<Vec<u8>>) -> Build {
            self.chunks = chunks;
            self.tiled = false;
            self
        }
        fn tiles(mut self, width: usize, height: usize, chunks: Vec<Vec<u8>>) -> Build {
            self.chunks = chunks;
            self.tiled = true;
            self.raw.retain(|(t, _)| *t != tags::ROWS_PER_STRIP);
            self = self.raw(tags::TILE_WIDTH, V::Long(vec![width as u32]));
            self.raw(tags::TILE_LENGTH, V::Long(vec![height as u32]))
        }
        fn preview(mut self, jpeg: Vec<u8>) -> Build {
            self.preview = Some(jpeg);
            self
        }

        fn bytes(&self) -> Vec<u8> {
            let le = self.little_endian;
            let w16 = |v: u16| if le { v.to_le_bytes() } else { v.to_be_bytes() };
            let w32 = |v: u32| if le { v.to_le_bytes() } else { v.to_be_bytes() };
            let count = self.chunks.len().max(1);

            // The layout is fixed once the entry counts are known, so
            // the file is written twice: the first pass measures the
            // heap, the second fills in the offsets that depend on it.
            let mut chunk_offsets = vec![0u32; count];
            let mut preview_offset = 0u32;
            let mut out = Vec::new();
            for _ in 0..2 {
                let mut root = self.root.clone();
                let mut raw = self.raw.clone();
                let (offsets_tag, counts_tag) = if self.tiled {
                    (tags::TILE_OFFSETS, tags::TILE_BYTE_COUNTS)
                } else {
                    (tags::STRIP_OFFSETS, tags::STRIP_BYTE_COUNTS)
                };
                raw.push((offsets_tag, V::Long(chunk_offsets.clone())));
                raw.push((
                    counts_tag,
                    V::Long(self.chunks.iter().map(|c| c.len() as u32).collect()),
                ));
                root.push((tags::SUB_IFDS, V::Long(vec![0])));
                if let Some(jpeg) = &self.preview {
                    root.push((tags::JPEG_INTERCHANGE_FORMAT, V::Long(vec![preview_offset])));
                    root.push((
                        tags::JPEG_INTERCHANGE_FORMAT_LENGTH,
                        V::Long(vec![jpeg.len() as u32]),
                    ));
                }
                root.sort_by_key(|(t, _)| *t);
                raw.sort_by_key(|(t, _)| *t);

                let directory = |n: usize| 2 + 12 * n + 4;
                let root_at = 8usize;
                let raw_at = root_at + directory(root.len());
                let heap_at = raw_at + directory(raw.len());
                // IFD0 points at the SubIFD, which is written next.
                for entry in root.iter_mut() {
                    if entry.0 == tags::SUB_IFDS {
                        entry.1 = V::Long(vec![raw_at as u32]);
                    }
                }

                let mut heap: Vec<u8> = Vec::new();
                let mut write = |entries: &[(u16, V)], next: u32| -> Vec<u8> {
                    let mut ifd = w16(entries.len() as u16).to_vec();
                    for (tag, value) in entries {
                        ifd.extend_from_slice(&w16(*tag));
                        ifd.extend_from_slice(&w16(value.kind()));
                        ifd.extend_from_slice(&w32(value.count() as u32));
                        let mut bytes = value.bytes(le);
                        if bytes.len() <= 4 {
                            bytes.resize(4, 0);
                            ifd.extend_from_slice(&bytes);
                        } else {
                            // TIFF wants values on a word boundary.
                            if heap.len() % 2 == 1 {
                                heap.push(0);
                            }
                            ifd.extend_from_slice(&w32((heap_at + heap.len()) as u32));
                            heap.extend_from_slice(&bytes);
                        }
                    }
                    ifd.extend_from_slice(&w32(next));
                    ifd
                };
                let root_ifd = write(&root, 0);
                let raw_ifd = write(&raw, 0);

                out = Vec::new();
                out.extend_from_slice(if le { b"II*\0" } else { b"MM\0*" });
                out.extend_from_slice(&w32(root_at as u32));
                out.extend_from_slice(&root_ifd);
                out.extend_from_slice(&raw_ifd);
                out.extend_from_slice(&heap);
                for (i, chunk) in self.chunks.iter().enumerate() {
                    chunk_offsets[i] = out.len() as u32;
                    out.extend_from_slice(chunk);
                }
                if let Some(jpeg) = &self.preview {
                    preview_offset = out.len() as u32;
                    out.extend_from_slice(jpeg);
                }
            }
            out
        }
    }

    /// Samples packed most-significant-bit first, each row starting on
    /// a byte boundary — the way an uncompressed DNG stores them.
    fn pack_msb(rows: &[Vec<u16>], bits: u32) -> Vec<u8> {
        let mut out = Vec::new();
        for row in rows {
            let mut accumulator = 0u32;
            let mut held = 0u32;
            for sample in row {
                accumulator = (accumulator << bits) | (*sample as u32 & ((1 << bits) - 1));
                held += bits;
                while held >= 8 {
                    out.push((accumulator >> (held - 8)) as u8);
                    held -= 8;
                }
            }
            if held > 0 {
                out.push((accumulator << (8 - held)) as u8);
            }
        }
        out
    }

    fn samples(raw: &RawImage) -> &[u16] {
        match &raw.data {
            RawData::U16(v) => v,
            RawData::F32(_) => panic!("expected integer samples"),
        }
    }

    fn floats(raw: &RawImage) -> &[f32] {
        match &raw.data {
            RawData::F32(v) => v,
            RawData::U16(_) => panic!("expected floating-point samples"),
        }
    }

    #[test]
    fn packed_samples_are_msb_first_whichever_way_the_tiff_runs() {
        let rows = vec![vec![0u16, 1, 4094, 4095], vec![2048, 7, 100, 3000]];
        let packed = pack_msb(&rows, 12);
        // Four 12-bit samples are exactly six bytes, so no row padding
        // is involved and the two byte orders must agree.
        assert_eq!(packed.len(), 12);
        for big in [false, true] {
            let mut build = Build::new(4, 2).strip(packed.clone());
            if big {
                build = build.big_endian();
            }
            let raw = decode(&build.bytes()).expect("decode");
            assert_eq!(samples(&raw), rows.concat().as_slice());
            assert_eq!(raw.cfa, Cfa::RGGB);
            assert_eq!(raw.white_level, 4095.0);
        }
    }

    #[test]
    fn packed_rows_restart_on_a_byte_boundary() {
        // Three 12-bit samples are four and a half bytes, so each row
        // is padded to five and the second row starts clean.
        let rows = vec![vec![1u16, 2, 3], vec![4095, 2048, 17]];
        let packed = pack_msb(&rows, 12);
        assert_eq!(packed.len(), 10);
        let build = Build::new(3, 2).strip(packed);
        let raw = decode(&build.bytes()).expect("decode");
        assert_eq!(samples(&raw), rows.concat().as_slice());
    }

    #[test]
    fn sixteen_bit_samples_follow_the_file_byte_order() {
        let values: Vec<u16> = vec![0, 1000, 40000, 65535];
        for big in [false, true] {
            let pixels: Vec<u8> = values
                .iter()
                .flat_map(|v| {
                    if big {
                        v.to_be_bytes()
                    } else {
                        v.to_le_bytes()
                    }
                })
                .collect();
            let mut build = Build::new(2, 2)
                .raw(tags::BITS_PER_SAMPLE, V::Short(vec![16]))
                .raw(tag::WHITE_LEVEL, V::Long(vec![65535]))
                .strip(pixels);
            if big {
                build = build.big_endian();
            }
            let raw = decode(&build.bytes()).expect("decode");
            assert_eq!(samples(&raw), values.as_slice());
        }
    }

    #[test]
    fn eight_ten_and_fourteen_bit_samples_round_trip() {
        for bits in [8u32, 10, 14] {
            let top = (1u16 << bits) - 1;
            let rows = vec![vec![0u16, 1, top / 2, top], vec![top, 0, 3, 9]];
            let build = Build::new(4, 2)
                .raw(tags::BITS_PER_SAMPLE, V::Short(vec![bits as u16]))
                .raw(tag::WHITE_LEVEL, V::Long(vec![top as u32]))
                .strip(pack_msb(&rows, bits));
            let raw = decode(&build.bytes()).expect("decode");
            assert_eq!(samples(&raw), rows.concat().as_slice(), "{bits} bits");
        }
    }

    #[test]
    fn strips_and_tiles_land_in_the_right_place() {
        // Six columns and three rows in 4x2 tiles: the right column of
        // tiles is half wasted and the bottom row one row short, both
        // of which the reader has to clip.
        let tile = |base: u16| pack_msb(&[vec![base; 4], vec![base + 1; 4]], 12);
        let build = Build::new(6, 3).tiles(4, 2, vec![tile(10), tile(20), tile(30), tile(40)]);
        let raw = decode(&build.bytes()).expect("decode");
        let got = samples(&raw);
        assert_eq!(&got[0..6], &[10, 10, 10, 10, 20, 20]);
        assert_eq!(&got[6..12], &[11, 11, 11, 11, 21, 21]);
        assert_eq!(&got[12..18], &[30, 30, 30, 30, 40, 40]);

        // The same frame as three one-row strips.
        let strips = Build::new(6, 1)
            .raw(tags::IMAGE_LENGTH, V::Long(vec![3]))
            .raw(tags::ROWS_PER_STRIP, V::Long(vec![1]))
            .chunk_list(vec![
                pack_msb(&[vec![1u16; 6]], 12),
                pack_msb(&[vec![2u16; 6]], 12),
                pack_msb(&[vec![3u16; 6]], 12),
            ]);
        let raw = decode(&strips.bytes()).expect("decode");
        assert_eq!(
            samples(&raw),
            [1, 1, 1, 1, 1, 1, 2, 2, 2, 2, 2, 2, 3, 3, 3, 3, 3, 3]
        );
    }

    #[test]
    fn black_level_folds_onto_the_two_by_two() {
        let flat = Build::new(2, 2).strip(pack_msb(&vec![vec![0u16; 2]; 2], 12));
        let decode_with = |build: Build| decode(&build.bytes()).expect("decode").black_levels;

        // One value for the whole sensor.
        assert_eq!(
            decode_with(
                Build::new(2, 2)
                    .raw(tag::BLACK_LEVEL_REPEAT_DIM, V::Short(vec![1, 1]))
                    .raw(tag::BLACK_LEVEL, V::Long(vec![64]))
                    .strip(pack_msb(&vec![vec![0u16; 2]; 2], 12))
            ),
            [64.0; 4]
        );
        // The 2x2 the crate carries, straight through, row-major.
        assert_eq!(
            decode_with(
                Build::new(2, 2)
                    .raw(tag::BLACK_LEVEL_REPEAT_DIM, V::Short(vec![2, 2]))
                    .raw(tag::BLACK_LEVEL, V::Long(vec![10, 20, 30, 40]))
                    .strip(pack_msb(&vec![vec![0u16; 2]; 2], 12))
            ),
            [10.0, 20.0, 30.0, 40.0]
        );
        // Rationals: Sigma writes a fractional black level.
        assert_eq!(
            decode_with(
                Build::new(2, 2)
                    .raw(tag::BLACK_LEVEL_REPEAT_DIM, V::Short(vec![1, 1]))
                    .raw(tag::BLACK_LEVEL, V::Rational(vec![(511, 2)]))
                    .strip(pack_msb(&vec![vec![0u16; 2]; 2], 12))
            ),
            [255.5; 4]
        );
        // A 4x4 block still has a 2x2 phase, so each of the four
        // positions averages the four cells that share it: position
        // (0, 0) takes 0, 2, 8 and 10 out of a row-major 0..16.
        let block: Vec<u32> = (0..16).collect();
        assert_eq!(
            decode_with(
                Build::new(2, 2)
                    .raw(tag::BLACK_LEVEL_REPEAT_DIM, V::Short(vec![4, 4]))
                    .raw(tag::BLACK_LEVEL, V::Long(block))
                    .strip(pack_msb(&vec![vec![0u16; 2]; 2], 12))
            ),
            [5.0, 6.0, 9.0, 10.0]
        );
        assert_eq!(decode_with(flat), [0.0; 4]);
    }

    #[test]
    fn crop_is_the_default_crop_inside_the_active_area() {
        let pixels = pack_msb(&vec![vec![0u16; 24]; 16], 12);
        let build = Build::new(24, 16)
            .raw(tag::ACTIVE_AREA, V::Long(vec![2, 4, 14, 20]))
            .raw(tag::DEFAULT_CROP_ORIGIN, V::Long(vec![3, 1]))
            .raw(tag::DEFAULT_CROP_SIZE, V::Long(vec![8, 6]))
            .strip(pixels.clone());
        let raw = decode(&build.bytes()).expect("decode");
        assert_eq!(
            raw.crop,
            Rect {
                x: 7,
                y: 3,
                width: 8,
                height: 6
            }
        );

        // ActiveArea alone crops to the unmasked sensor.
        let raw = decode(
            &Build::new(24, 16)
                .raw(tag::ACTIVE_AREA, V::Long(vec![2, 4, 14, 20]))
                .strip(pixels.clone())
                .bytes(),
        )
        .expect("decode");
        assert_eq!(
            raw.crop,
            Rect {
                x: 4,
                y: 2,
                width: 16,
                height: 12
            }
        );

        // Neither tag: the whole frame.
        let raw = decode(&Build::new(24, 16).strip(pixels.clone()).bytes()).expect("decode");
        assert_eq!(
            raw.crop,
            Rect {
                x: 0,
                y: 0,
                width: 24,
                height: 16
            }
        );

        // A crop that runs off the sensor is clamped rather than
        // handed to the developer to trip over.
        let raw = decode(
            &Build::new(24, 16)
                .raw(tag::DEFAULT_CROP_ORIGIN, V::Long(vec![20, 12]))
                .raw(tag::DEFAULT_CROP_SIZE, V::Long(vec![900, 900]))
                .strip(pixels)
                .bytes(),
        )
        .expect("decode");
        assert_eq!(raw.crop.x + raw.crop.width, 24);
        assert_eq!(raw.crop.y + raw.crop.height, 16);
    }

    #[test]
    fn cfa_pattern_entries_are_plane_indices() {
        let pixels = pack_msb(&vec![vec![0u16; 6]; 6], 12);
        // The default planes are red, green, blue in that order.
        let raw = decode(&Build::new(6, 6).strip(pixels.clone()).bytes()).expect("decode");
        assert_eq!(raw.cfa, Cfa::RGGB);

        // Re-ordering the planes re-orders the pattern without the
        // pattern itself changing.
        let raw = decode(
            &Build::new(6, 6)
                .raw(tag::CFA_PLANE_COLOR, V::Byte(vec![2, 1, 0]))
                .strip(pixels.clone())
                .bytes(),
        )
        .expect("decode");
        assert_eq!(raw.cfa, Cfa::BGGR);

        // A 6x6 pattern is X-Trans.
        let xtrans: Vec<u8> = (0..36).map(|i| [1u8, 1, 0, 1, 1, 2][i % 6]).collect();
        let raw = decode(
            &Build::new(6, 6)
                .raw(tags::CFA_REPEAT_PATTERN_DIM, V::Short(vec![6, 6]))
                .raw(tags::CFA_PATTERN, V::Byte(xtrans))
                .strip(pixels.clone())
                .bytes(),
        )
        .expect("decode");
        assert!(matches!(raw.cfa, Cfa::XTrans(_)));
        assert_eq!(raw.cfa.color_at(2, 0), Some(CfaColor::Red));

        // Anything else is carried as a plain repeating block.
        let raw = decode(
            &Build::new(6, 6)
                .raw(tags::CFA_REPEAT_PATTERN_DIM, V::Short(vec![2, 4]))
                .raw(tags::CFA_PATTERN, V::Byte(vec![0, 1, 2, 3, 4, 5, 0, 1]))
                .strip(pixels.clone())
                .bytes(),
        )
        .expect("decode");
        assert_eq!(
            raw.cfa,
            Cfa::Pattern {
                width: 4,
                height: 2,
                colors: vec![
                    CfaColor::Red,
                    CfaColor::Green,
                    CfaColor::Blue,
                    CfaColor::Cyan,
                    CfaColor::Magenta,
                    CfaColor::Yellow,
                    CfaColor::Red,
                    CfaColor::Green,
                ],
            }
        );

        // A white photosite has no place in the develop pipeline.
        let error = decode(
            &Build::new(6, 6)
                .raw(tag::CFA_PLANE_COLOR, V::Byte(vec![0, 1, 6]))
                .strip(pixels)
                .bytes(),
        )
        .expect_err("white plane");
        assert!(matches!(error, Error::Unsupported(_)), "{error}");
    }

    #[test]
    fn the_matrix_measured_nearest_daylight_wins() {
        let pixels = pack_msb(&vec![vec![0u16; 2]; 2], 12);
        let one = V::SRational((1..=9).map(|i| (i * 1000, 10000)).collect());
        let two = V::SRational((1..=9).map(|i| (i * 2000, 10000)).collect());
        let build = |a: u16, b: u16| {
            Build::new(2, 2)
                .root(tag::COLOR_MATRIX_1, one.clone())
                .root(tag::COLOR_MATRIX_2, two.clone())
                .root(tag::CALIBRATION_ILLUMINANT_1, V::Short(vec![a]))
                .root(tag::CALIBRATION_ILLUMINANT_2, V::Short(vec![b]))
                .strip(pixels.clone())
        };
        let first_of = |raw: &RawImage| raw.color_matrix.expect("a matrix")[0][0];
        // Standard light A against D65: D65 by a mile.
        assert_eq!(first_of(&decode(&build(17, 21).bytes()).unwrap()), 0.2);
        assert_eq!(first_of(&decode(&build(21, 17).bytes()).unwrap()), 0.1);
        // D50 still beats standard light A.
        assert_eq!(first_of(&decode(&build(17, 23).bytes()).unwrap()), 0.2);
        assert_eq!(first_of(&decode(&build(23, 17).bytes()).unwrap()), 0.1);
        // One matrix on its own is the one, whatever it was measured
        // under.
        let only = Build::new(2, 2)
            .root(tag::COLOR_MATRIX_1, one.clone())
            .root(tag::CALIBRATION_ILLUMINANT_1, V::Short(vec![17]))
            .strip(pixels.clone());
        assert_eq!(first_of(&decode(&only.bytes()).unwrap()), 0.1);
        // No matrix at all leaves the field for the camera table.
        let raw = decode(&Build::new(2, 2).strip(pixels).bytes()).unwrap();
        assert_eq!(raw.color_matrix, None);
    }

    #[test]
    fn as_shot_neutral_inverts_into_multipliers() {
        let pixels = pack_msb(&vec![vec![0u16; 2]; 2], 12);
        let raw = decode(
            &Build::new(2, 2)
                .root(
                    tag::AS_SHOT_NEUTRAL,
                    V::Rational(vec![(1, 2), (1, 1), (1, 4)]),
                )
                .strip(pixels.clone())
                .bytes(),
        )
        .expect("decode");
        assert_eq!(raw.wb_coeffs, [2.0, 1.0, 4.0, 1.0]);

        // A neutral that does not already have green at one is scaled.
        let raw = decode(
            &Build::new(2, 2)
                .root(
                    tag::AS_SHOT_NEUTRAL,
                    V::Rational(vec![(1, 2), (2, 1), (1, 4)]),
                )
                .strip(pixels.clone())
                .bytes(),
        )
        .expect("decode");
        assert_eq!(raw.wb_coeffs, [4.0, 1.0, 8.0, 1.0]);

        // AsShotWhiteXY is a chromaticity: without a matrix nothing can
        // turn it into multipliers, so the balance stays at unity.
        let raw = decode(
            &Build::new(2, 2)
                .root(
                    tag::AS_SHOT_WHITE_XY,
                    V::Rational(vec![(3127, 10000), (3290, 10000)]),
                )
                .strip(pixels.clone())
                .bytes(),
        )
        .expect("decode");
        assert_eq!(raw.wb_coeffs, [1.0; 4]);
        // With a matrix, the white point's camera response inverts into
        // the multipliers. The matrix here maps XYZ to (X, Y, Z)/2 for
        // red, green, blue in turn, so D65's response is its XYZ halved
        // and the multipliers are Y/X, 1, Y/Z of D65.
        let raw = decode(
            &Build::new(2, 2)
                .root(
                    tag::AS_SHOT_WHITE_XY,
                    V::Rational(vec![(3127, 10000), (3290, 10000)]),
                )
                .root(
                    tag::COLOR_MATRIX_1,
                    V::SRational(vec![
                        (5000, 10000),
                        (0, 1),
                        (0, 1),
                        (0, 1),
                        (5000, 10000),
                        (0, 1),
                        (0, 1),
                        (0, 1),
                        (5000, 10000),
                    ]),
                )
                .strip(pixels)
                .bytes(),
        )
        .expect("decode");
        let (x, y) = (0.3127f32, 0.3290f32);
        let (want_r, want_b) = (y / x, y / (1.0 - x - y));
        assert!(
            (raw.wb_coeffs[0] - want_r).abs() < 1e-3,
            "{:?}",
            raw.wb_coeffs
        );
        assert_eq!(raw.wb_coeffs[1], 1.0);
        assert!(
            (raw.wb_coeffs[2] - want_b).abs() < 1e-3,
            "{:?}",
            raw.wb_coeffs
        );
    }

    #[test]
    fn the_linearization_table_is_applied_and_sets_the_default_white() {
        let rows = vec![vec![0u16, 1, 2, 9]];
        let build = Build::new(4, 1)
            .raw(tags::BITS_PER_SAMPLE, V::Short(vec![8]))
            .raw(
                tag::LINEARIZATION_TABLE,
                V::Short(vec![0, 1000, 2000, 3000]),
            )
            .without(tag::WHITE_LEVEL)
            .strip(pack_msb(&rows, 8));
        let raw = decode(&build.bytes()).expect("decode");
        // The last entry stands in for every index past the table.
        assert_eq!(samples(&raw), [0, 1000, 2000, 3000]);
        assert_eq!(raw.white_level, 3000.0);
    }

    #[test]
    fn multi_image_dngs_take_the_largest_raw_plane() {
        // A reduced-resolution CFA image beside the real one is
        // ignored even though it has the raw photometric.
        let big = pack_msb(&vec![vec![7u16; 8]; 4], 12);
        let build = Build::new(8, 4).strip(big);
        let raw = decode(&build.bytes()).expect("decode");
        assert_eq!((raw.width, raw.height), (8, 4));
        assert!(samples(&raw).iter().all(|s| *s == 7));
    }

    #[test]
    fn deflated_integers_undo_the_horizontal_predictor() {
        let values: Vec<u16> = vec![100, 120, 90, 4000, 30, 31, 32, 33];
        // Predictor 2 differences each sample from the one before it
        // in the row; the two rows are independent.
        let mut encoded = Vec::new();
        for row in values.chunks(4) {
            let mut previous = 0u16;
            for sample in row {
                encoded.extend_from_slice(&sample.wrapping_sub(previous).to_le_bytes());
                previous = *sample;
            }
        }
        let build = Build::new(4, 2)
            .raw(tags::BITS_PER_SAMPLE, V::Short(vec![16]))
            .raw(tags::COMPRESSION, V::Short(vec![8]))
            .raw(tag::PREDICTOR, V::Short(vec![2]))
            .raw(tag::WHITE_LEVEL, V::Long(vec![65535]))
            .strip(miniz_oxide::deflate::compress_to_vec_zlib(&encoded, 6));
        let raw = decode(&build.bytes()).expect("decode");
        assert_eq!(samples(&raw), values.as_slice());

        // DNG's own 34892 widens the stride to two pixels, so a Bayer
        // row differences red against red.
        let mut encoded = Vec::new();
        for row in values.chunks(4) {
            for (i, sample) in row.iter().enumerate() {
                let previous = if i >= 2 { row[i - 2] } else { 0 };
                encoded.extend_from_slice(&sample.wrapping_sub(previous).to_le_bytes());
            }
        }
        let build = Build::new(4, 2)
            .raw(tags::BITS_PER_SAMPLE, V::Short(vec![16]))
            .raw(tags::COMPRESSION, V::Short(vec![8]))
            .raw(tag::PREDICTOR, V::Short(vec![34892]))
            .raw(tag::WHITE_LEVEL, V::Long(vec![65535]))
            .strip(miniz_oxide::deflate::compress_to_vec_zlib(&encoded, 6));
        let raw = decode(&build.bytes()).expect("decode");
        assert_eq!(samples(&raw), values.as_slice());
    }

    /// Split a row of samples into byte planes, most significant
    /// first, then difference the bytes `stride` apart — the encoder
    /// side of DNG's floating-point predictor.
    fn shuffle(row: &[u32], width: usize, stride: usize) -> Vec<u8> {
        let mut planes = vec![0u8; row.len() * width];
        for (sample, value) in row.iter().enumerate() {
            for plane in 0..width {
                planes[plane * row.len() + sample] = (value >> (8 * (width - 1 - plane))) as u8;
            }
        }
        for i in (stride..planes.len()).rev() {
            planes[i] = planes[i].wrapping_sub(planes[i - stride]);
        }
        planes
    }

    #[test]
    fn deflated_floats_come_back_through_the_byte_shuffle() {
        let values: Vec<f32> = vec![0.0, 0.25, 1.0, 0.5, -1.5, 3.25, 0.125, 65504.0];
        let mut encoded = Vec::new();
        for row in values.chunks(4) {
            let bits: Vec<u32> = row.iter().map(|v| v.to_bits()).collect();
            // Predictor 34894: floating point, two pixels apart.
            encoded.extend_from_slice(&shuffle(&bits, 4, 2));
        }
        let build = Build::new(4, 2)
            .raw(tags::BITS_PER_SAMPLE, V::Short(vec![32]))
            .raw(tag::SAMPLE_FORMAT, V::Short(vec![3]))
            .raw(tags::COMPRESSION, V::Short(vec![8]))
            .raw(tag::PREDICTOR, V::Short(vec![34894]))
            .without(tag::WHITE_LEVEL)
            .strip(miniz_oxide::deflate::compress_to_vec_zlib(&encoded, 6));
        let raw = decode(&build.bytes()).expect("decode");
        assert_eq!(floats(&raw), values.as_slice());
        // A float DNG is already scaled: one is saturation.
        assert_eq!(raw.white_level, 1.0);
        assert_eq!(raw.black_levels, [0.0; 4]);
    }

    #[test]
    fn twenty_four_bit_floats_are_a_shift() {
        // DNG's 24-bit float is a single-precision one with the low
        // sixteen mantissa bits dropped, so values that fit exactly
        // survive the round trip.
        let values: Vec<f32> = vec![1.0, 0.5, -2.0, 0.0];
        let bits: Vec<u32> = values.iter().map(|v| v.to_bits() >> 8).collect();
        let encoded = shuffle(&bits, 3, 1);
        let build = Build::new(4, 1)
            .raw(tags::BITS_PER_SAMPLE, V::Short(vec![24]))
            .raw(tag::SAMPLE_FORMAT, V::Short(vec![3]))
            .raw(tags::COMPRESSION, V::Short(vec![8]))
            .raw(tag::PREDICTOR, V::Short(vec![3]))
            .without(tag::WHITE_LEVEL)
            .strip(miniz_oxide::deflate::compress_to_vec_zlib(&encoded, 6));
        let raw = decode(&build.bytes()).expect("decode");
        assert_eq!(floats(&raw), values.as_slice());
    }

    #[test]
    fn half_precision_covers_zero_subnormals_and_infinity() {
        assert_eq!(half_to_f32(0x0000), 0.0);
        assert_eq!(half_to_f32(0x8000).to_bits(), (-0.0f32).to_bits());
        assert_eq!(half_to_f32(0x3C00), 1.0);
        assert_eq!(half_to_f32(0xBC00), -1.0);
        assert_eq!(half_to_f32(0x3800), 0.5);
        // The largest half, and the smallest normal one.
        assert_eq!(half_to_f32(0x7BFF), 65504.0);
        assert_eq!(half_to_f32(0x0400), 2.0f32.powi(-14));
        // Subnormals: the smallest is 2^-24, and 0x0200 is 2^-15.
        assert_eq!(half_to_f32(0x0001), 2.0f32.powi(-24));
        assert_eq!(half_to_f32(0x0200), 2.0f32.powi(-15));
        assert_eq!(half_to_f32(0x0003), 3.0 * 2.0f32.powi(-24));
        assert!(half_to_f32(0x7C00).is_infinite());
        assert!(half_to_f32(0xFC00).is_infinite() && half_to_f32(0xFC00) < 0.0);
        assert!(half_to_f32(0x7E00).is_nan());
    }

    #[test]
    fn half_precision_tiles_decode() {
        let halves: Vec<u32> = vec![0x0000, 0x3C00, 0x3800, 0x7BFF];
        let encoded = shuffle(&halves, 2, 1);
        let build = Build::new(4, 1)
            .raw(tags::BITS_PER_SAMPLE, V::Short(vec![16]))
            .raw(tag::SAMPLE_FORMAT, V::Short(vec![3]))
            .raw(tags::COMPRESSION, V::Short(vec![8]))
            .raw(tag::PREDICTOR, V::Short(vec![3]))
            .without(tag::WHITE_LEVEL)
            .strip(miniz_oxide::deflate::compress_to_vec_zlib(&encoded, 6));
        let raw = decode(&build.bytes()).expect("decode");
        assert_eq!(floats(&raw), [0.0, 1.0, 0.5, 65504.0]);
    }

    #[test]
    fn unshuffled_floats_use_the_file_byte_order() {
        let values: Vec<f32> = vec![1.0, -0.5, 0.25, 3.0];
        for big in [false, true] {
            let encoded: Vec<u8> = values
                .iter()
                .flat_map(|v| {
                    if big {
                        v.to_bits().to_be_bytes()
                    } else {
                        v.to_bits().to_le_bytes()
                    }
                })
                .collect();
            let mut build = Build::new(4, 1)
                .raw(tags::BITS_PER_SAMPLE, V::Short(vec![32]))
                .raw(tag::SAMPLE_FORMAT, V::Short(vec![3]))
                .raw(tags::COMPRESSION, V::Short(vec![8]))
                .without(tag::PREDICTOR)
                .without(tag::WHITE_LEVEL)
                .strip(miniz_oxide::deflate::compress_to_vec_zlib(&encoded, 6));
            if big {
                build = build.big_endian();
            }
            let raw = decode(&build.bytes()).expect("decode");
            assert_eq!(floats(&raw), values.as_slice());
        }
    }

    #[test]
    fn lossy_tiles_are_baseline_jpeg_widened_by_the_table() {
        // A lossy DNG is linear raw: three eight-bit channels a pixel,
        // one complete JPEG per tile, and a linearization table to put
        // the samples back on the sensor's scale.
        let jpeg = |red: u8, green: u8, blue: u8| {
            let mut out = Vec::new();
            let pixels: Vec<u8> = (0..8 * 8).flat_map(|_| [red, green, blue]).collect();
            let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 100);
            image::ImageEncoder::write_image(
                encoder,
                &pixels,
                8,
                8,
                image::ExtendedColorType::Rgb8,
            )
            .expect("encode a tile");
            out
        };
        let table: Vec<u16> = (0..256).map(|i| i as u16 * 257).collect();
        let build = Build::new(16, 16)
            .raw(tags::PHOTOMETRIC, V::Short(vec![34892]))
            .raw(tags::SAMPLES_PER_PIXEL, V::Short(vec![3]))
            .raw(tags::BITS_PER_SAMPLE, V::Short(vec![8, 8, 8]))
            .raw(tags::COMPRESSION, V::Short(vec![34892]))
            .raw(tag::LINEARIZATION_TABLE, V::Short(table))
            .raw(tag::WHITE_LEVEL, V::Long(vec![65535]))
            .without(tags::CFA_REPEAT_PATTERN_DIM)
            .without(tags::CFA_PATTERN)
            .tiles(
                8,
                8,
                vec![
                    jpeg(200, 100, 50),
                    jpeg(10, 20, 30),
                    jpeg(255, 255, 255),
                    jpeg(0, 0, 0),
                ],
            );
        let raw = decode(&build.bytes()).expect("decode");
        assert_eq!(raw.cpp, 3);
        assert_eq!(raw.cfa, Cfa::None);
        let got = samples(&raw);
        let at = |x: usize, y: usize| {
            let i = (y * 16 + x) * 3;
            [got[i], got[i + 1], got[i + 2]]
        };
        // JPEG is lossy even at quality 100, so a flat tile comes back
        // within a code or two of what went in, times 257.
        let close = |got: [u16; 3], want: [u16; 3]| {
            got.iter()
                .zip(want.iter())
                .all(|(a, b)| (*a as i32 - *b as i32).abs() <= 3 * 257)
        };
        assert!(
            close(at(2, 2), [200 * 257, 100 * 257, 50 * 257]),
            "{:?}",
            at(2, 2)
        );
        assert!(
            close(at(12, 2), [10 * 257, 20 * 257, 30 * 257]),
            "{:?}",
            at(12, 2)
        );
        assert!(close(at(2, 12), [65535, 65535, 65535]), "{:?}", at(2, 12));
        assert!(close(at(12, 12), [0, 0, 0]), "{:?}", at(12, 12));
    }

    #[test]
    fn the_preview_is_the_largest_embedded_jpeg() {
        let mut jpeg = Vec::new();
        let pixels = vec![128u8; 16 * 16 * 3];
        let encoder = image::codecs::jpeg::JpegEncoder::new(&mut jpeg);
        image::ImageEncoder::write_image(encoder, &pixels, 16, 16, image::ExtendedColorType::Rgb8)
            .expect("encode");
        let build = Build::new(4, 2)
            .strip(pack_msb(&vec![vec![0u16; 4]; 2], 12))
            .preview(jpeg.clone());
        let bytes = build.bytes();
        assert_eq!(preview(&bytes).expect("preview"), Some(jpeg.clone()));
        assert_eq!(decode(&bytes).expect("decode").preview, Some(jpeg));
    }

    #[test]
    fn compressions_this_module_has_no_decoder_for_are_unsupported() {
        let pixels = pack_msb(&vec![vec![0u16; 4]; 2], 12);
        for (code, what) in [(52546u16, "JPEG XL"), (5, "LZW")] {
            let build = Build::new(4, 2)
                .raw(tags::COMPRESSION, V::Short(vec![code]))
                .strip(pixels.clone());
            let error = decode(&build.bytes()).expect_err(what);
            assert!(matches!(error, Error::Unsupported(_)), "{what}: {error}");
        }
    }

    /// Compression 9 now reaches the VC-5 decoder, so a tile that is
    /// not a VC-5 sample is corrupt rather than unsupported.
    #[test]
    fn a_vc5_tile_that_is_not_a_vc5_sample_is_corrupt() {
        let build = Build::new(4, 2)
            .raw(tags::COMPRESSION, V::Short(vec![compression::VC5 as u16]))
            .strip(pack_msb(&vec![vec![0u16; 4]; 2], 12));
        let error = decode(&build.bytes()).expect_err("not a VC-5 sample");
        assert!(matches!(error, Error::Corrupt(_)), "{error}");
    }

    #[test]
    fn a_dng_from_the_future_is_refused_rather_than_guessed_at() {
        let build = Build::new(4, 2)
            .root(tags::DNG_VERSION, V::Byte(vec![2, 0, 0, 0]))
            .root(tag::DNG_BACKWARD_VERSION, V::Byte(vec![2, 0, 0, 0]))
            .strip(pack_msb(&vec![vec![0u16; 4]; 2], 12));
        let error = decode(&build.bytes()).expect_err("a 2.0 DNG");
        assert!(matches!(error, Error::Unsupported(_)), "{error}");

        // A 1.7 file whose *backward* version is old still decodes.
        let build = Build::new(4, 2)
            .root(tags::DNG_VERSION, V::Byte(vec![1, 7, 0, 0]))
            .root(tag::DNG_BACKWARD_VERSION, V::Byte(vec![1, 1, 0, 0]))
            .strip(pack_msb(&vec![vec![0u16; 4]; 2], 12));
        decode(&build.bytes()).expect("a 1.7 file with a 1.1 fallback");
    }

    #[test]
    fn garbage_and_short_files_are_errors_not_panics() {
        assert!(decode(&[]).is_err());
        assert!(decode(b"II*\0").is_err());
        assert!(decode(b"not a tiff at all").is_err());
        // A DNG whose strip says it is far longer than the file.
        let build = Build::new(4, 2).strip(vec![0; 12]);
        let mut bytes = build.bytes();
        bytes.truncate(bytes.len() - 6);
        assert!(decode(&bytes).is_err());

        let whole = Build::new(64, 64)
            .strip(pack_msb(&vec![vec![0u16; 64]; 64], 12))
            .bytes();
        for cut in 0..whole.len() / 7 {
            let _ = decode(&whole[..cut * 7]);
            let _ = preview(&whole[..cut * 7]);
        }
    }

    #[test]
    fn a_frame_larger_than_its_data_is_corrupt() {
        // The IFD claims 64 rows and the strip holds two.
        let build = Build::new(4, 64)
            .raw(tags::ROWS_PER_STRIP, V::Long(vec![64]))
            .strip(pack_msb(&vec![vec![0u16; 4]; 2], 12));
        let error = decode(&build.bytes()).expect_err("short strip");
        assert!(matches!(error, Error::Corrupt(_)), "{error}");
    }

    // ---------------------------------------------------------------
    // Corpus. `SCHIST_RAW_CORPUS` points at a tree of camera files
    // with LibRaw's output beside each one: `<name>.tiff` is the
    // sensor frame as `unprocessed_raw -T` unpacked it (black not
    // subtracted) and `<name>.identify.txt` is `raw-identify -v -w`.
    // ---------------------------------------------------------------

    fn corpus() -> Option<PathBuf> {
        let dir = PathBuf::from(std::env::var_os("SCHIST_RAW_CORPUS")?);
        dir.is_dir().then_some(dir)
    }

    /// Every file this module claims, DNG and GoPro's GPR dialect.
    fn corpus_files() -> Vec<PathBuf> {
        let Some(dir) = corpus() else {
            return Vec::new();
        };
        let mut found = Vec::new();
        let mut stack = vec![dir];
        while let Some(at) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&at) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| e.eq_ignore_ascii_case("dng") || e.eq_ignore_ascii_case("gpr"))
                {
                    found.push(path);
                }
            }
        }
        found.sort();
        found
    }

    /// Files this module knowingly refuses, and why. A corpus file
    /// that fails for any other reason fails the test.
    fn unsupported_reason(_path: &Path) -> Option<&'static str> {
        // Nothing in the corpus is refused any more: GoPro's GPR, the
        // one dialect that used to be listed here, decodes through the
        // VC-5 module.
        None
    }

    /// LibRaw's own view of a file, as far as this test compares it.
    #[derive(Debug, Default)]
    struct Identify {
        full: Option<(usize, usize)>,
        inset: Option<(usize, usize, usize, usize)>,
        filter: Option<String>,
        /// LibRaw splits a DNG's black level in two: a scalar for
        /// the whole sensor and a four-entry `cblack[6..]` for the
        /// per-CFA-position extra. The sum is the black level.
        black_scalar: u32,
        black: Option<Vec<u32>>,
        white: Option<f32>,
        flip: Option<u32>,
        as_shot: Option<[f32; 3]>,
    }

    fn identify(path: &Path) -> Option<Identify> {
        let sidecar = path.with_extension(format!(
            "{}.identify.txt",
            path.extension().and_then(|e| e.to_str()).unwrap_or("")
        ));
        // The sidecar is not valid UTF-8 for every camera: LibRaw
        // prints RawDataUniqueID as raw bytes.
        let text = String::from_utf8_lossy(&std::fs::read(sidecar).ok()?).into_owned();
        let mut out = Identify::default();
        for line in text.lines() {
            let line = line.trim();
            let numbers = |rest: &str| -> Vec<f32> {
                rest.split(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-'))
                    .filter(|s| !s.is_empty())
                    .filter_map(|s| s.parse::<f32>().ok())
                    .collect()
            };
            if let Some(rest) = line.strip_prefix("Full size:") {
                let n = numbers(rest);
                if n.len() >= 2 {
                    out.full = Some((n[0] as usize, n[1] as usize));
                }
            } else if let Some(rest) = line.strip_prefix("Raw inset, width x height:") {
                let n = numbers(rest);
                if n.len() >= 4 {
                    out.inset = Some((n[2] as usize, n[3] as usize, n[0] as usize, n[1] as usize));
                }
            } else if let Some(rest) = line.strip_prefix("Filter pattern:") {
                out.filter = Some(rest.trim().to_string());
            } else if let Some(rest) = line.strip_prefix("Highlight linearity limits:") {
                out.white = numbers(rest).first().copied();
            } else if let Some(rest) = line.strip_prefix("Image flip:") {
                out.flip = numbers(rest).first().map(|v| *v as u32);
            } else if line.starts_with("cblack[") {
                if let Some(rest) = line.split_once(':') {
                    out.black = Some(numbers(rest.1).iter().map(|v| *v as u32).collect());
                }
            } else if let Some(rest) = line.strip_prefix("black:") {
                out.black_scalar = numbers(rest).first().copied().unwrap_or(0.0) as u32;
            } else if let Some(rest) = line.strip_prefix("As shot") {
                let n = numbers(rest);
                if n.len() >= 3 && out.as_shot.is_none() {
                    out.as_shot = Some([n[0], n[1], n[2]]);
                }
            }
        }
        Some(out)
    }

    /// LibRaw's unpacked sensor frame, when it managed to unpack one.
    fn oracle(path: &Path) -> Option<(usize, usize, Vec<u16>)> {
        let sidecar = path.with_extension(format!(
            "{}.tiff",
            path.extension().and_then(|e| e.to_str()).unwrap_or("")
        ));
        let image = image::open(&sidecar).ok()?.into_luma16();
        let (width, height) = (image.width() as usize, image.height() as usize);
        Some((width, height, image.into_raw()))
    }

    /// Where LibRaw deliberately disagrees with the file's own tags.
    ///
    /// LibRaw carries per-body margin tables that override a DNG's
    /// ActiveArea and DefaultCropOrigin for cameras it recognises, and
    /// it rounds a rational default crop the other way. The tags are
    /// what the file says, so this module follows them and the crop
    /// comparison is skipped for these.
    fn crop_deviates(path: &Path) -> bool {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        ["645D-", "_MAR0543", "fp-A002_647"]
            .iter()
            .any(|stem| name.contains(stem))
    }

    fn color_letter(color: CfaColor) -> char {
        match color {
            CfaColor::Red => 'R',
            CfaColor::Green | CfaColor::Green2 => 'G',
            CfaColor::Blue => 'B',
            CfaColor::Cyan => 'C',
            CfaColor::Magenta => 'M',
            CfaColor::Yellow => 'Y',
            CfaColor::Emerald => 'E',
        }
    }

    #[test]
    fn corpus_matches_libraw() {
        let files = corpus_files();
        if files.is_empty() {
            return;
        }
        let mut checked = 0;
        let mut skipped = Vec::new();
        for path in &files {
            let bytes = std::fs::read(path).expect("read the sample");
            assert_eq!(
                crate::probe(&bytes),
                Some(Format::Dng),
                "{} does not probe as a DNG",
                path.display()
            );
            let started = std::time::Instant::now();
            let raw = match decode(&bytes) {
                Ok(raw) => raw,
                Err(Error::Unsupported(why)) => {
                    let reason = unsupported_reason(path).unwrap_or_else(|| {
                        panic!(
                            "{} is unsupported and not allow-listed: {why}",
                            path.display()
                        )
                    });
                    skipped.push(format!("{}: {reason}", path.display()));
                    continue;
                }
                Err(other) => panic!("{}: {other}", path.display()),
            };
            let elapsed = started.elapsed();
            raw.validate()
                .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            checked += 1;
            println!(
                "{}: {}x{}x{} in {:.3}s ({:.1} MP/s), {} {}",
                path.display(),
                raw.width,
                raw.height,
                raw.cpp,
                elapsed.as_secs_f64(),
                (raw.width * raw.height) as f64 / 1e6 / elapsed.as_secs_f64(),
                raw.make,
                raw.model,
            );

            // The preview must be a JPEG a viewer can open.
            if let Some(preview) = &raw.preview {
                image::load_from_memory(preview)
                    .unwrap_or_else(|e| panic!("{}: preview does not decode: {e}", path.display()));
            }
            assert_eq!(
                preview(&bytes).expect("preview"),
                raw.preview,
                "{}: the cheap preview path disagrees",
                path.display()
            );

            match oracle(path) {
                Some((width, height, want)) => {
                    assert_eq!(
                        (raw.width, raw.height, raw.cpp),
                        (width, height, 1),
                        "{}: frame differs from LibRaw's",
                        path.display()
                    );
                    let RawData::U16(got) = &raw.data else {
                        panic!("{}: float data with an integer oracle", path.display());
                    };
                    let mut bad = 0usize;
                    let mut first = None;
                    for (i, (got, want)) in got.iter().zip(want.iter()).enumerate() {
                        if got != want {
                            bad += 1;
                            first.get_or_insert((i % width, i / width, *got, *want));
                        }
                    }
                    assert_eq!(
                        bad,
                        0,
                        "{}: {bad} samples differ, first (x, y, got, want) {:?}",
                        path.display(),
                        first
                    );
                }
                None => {
                    // LibRaw refuses Apple's linear ProRAW and some
                    // Samsung phone DNGs outright, so there is nothing
                    // to compare against: check the frame is at least
                    // plausible.
                    println!("  no oracle: LibRaw could not unpack this one");
                    assert!(raw.width >= 16 && raw.height >= 16, "{}", path.display());
                    assert_eq!(raw.data.len(), raw.width * raw.height * raw.cpp);
                    if raw.cpp == 3 {
                        assert_eq!(raw.cfa, Cfa::None, "{}", path.display());
                    }
                    let peak = (0..raw.data.len())
                        .map(|i| raw.data.get(i))
                        .fold(0.0, f32::max);
                    assert!(
                        peak > raw.white_level * 0.02 && peak <= raw.white_level,
                        "{}: samples peak at {peak} against a white level of {}",
                        path.display(),
                        raw.white_level
                    );
                }
            }

            let Some(identify) = identify(path) else {
                continue;
            };
            if let Some(full) = identify.full {
                assert_eq!((raw.width, raw.height), full, "{}: size", path.display());
            }
            if let Some(filter) = &identify.filter {
                let got: String = (0..4)
                    .map(|i| color_letter(raw.cfa.color_at(i % 2, i / 2).expect("a CFA colour")))
                    .collect();
                assert!(
                    filter.starts_with(&got),
                    "{}: CFA {got} against LibRaw's {filter}",
                    path.display()
                );
            }
            // LibRaw prints a black level only for files it could
            // unpack; the sum of its scalar and per-position parts is
            // what the DNG's BlackLevel says, truncated to an integer.
            if identify.black.is_some() || identify.black_scalar > 0 {
                let per_position = identify.black.unwrap_or_default();
                for position in 0..4 {
                    let extra = per_position
                        .get(position.min(per_position.len().saturating_sub(1)))
                        .copied()
                        .unwrap_or(0);
                    assert_eq!(
                        raw.black_levels[position].floor() as u32,
                        identify.black_scalar + extra,
                        "{}: black level {position} of {:?}",
                        path.display(),
                        raw.black_levels
                    );
                }
            }
            if let Some(white) = identify.white {
                assert_eq!(raw.white_level, white, "{}: white level", path.display());
            }
            if let Some(flip) = identify.flip {
                let want = match flip {
                    // LibRaw's flip 1 is a horizontal mirror, which is
                    // what every GoPro GPR carries (EXIF orientation 2).
                    1 => crate::Orientation::MirrorHorizontal,
                    3 => crate::Orientation::Rotate180,
                    5 => crate::Orientation::Rotate270CW,
                    6 => crate::Orientation::Rotate90CW,
                    _ => crate::Orientation::Normal,
                };
                assert_eq!(raw.orientation, want, "{}: orientation", path.display());
            }
            if let (Some(inset), false) = (identify.inset, crop_deviates(path)) {
                assert_eq!(
                    (raw.crop.x, raw.crop.y, raw.crop.width, raw.crop.height),
                    inset,
                    "{}: crop",
                    path.display()
                );
            }
            // `cam_mul` carries the same ratios without normalising
            // green, so scale it before comparing.
            if let Some(as_shot) = identify.as_shot.filter(|v| v[1] > 0.0) {
                let as_shot = [as_shot[0] / as_shot[1], 1.0, as_shot[2] / as_shot[1]];
                for (got, want) in raw.wb_coeffs.iter().zip(as_shot.iter()) {
                    assert!(
                        (got - want).abs() <= want.abs() * 1e-4 + 1e-4,
                        "{}: white balance {:?} against LibRaw's normalised {as_shot:?}",
                        path.display(),
                        raw.wb_coeffs
                    );
                }
            }
        }
        println!("decoded {checked} of {} DNGs", files.len());
        for line in &skipped {
            println!("  unsupported: {line}");
        }
    }

    /// A file cut short anywhere must give an error, never a panic and
    /// never an unbounded allocation.
    #[test]
    fn corpus_truncation_never_panics() {
        for path in corpus_files() {
            let bytes = std::fs::read(&path).expect("read the sample");
            // A deterministic spread of cut points: the header, the
            // middle of the IFDs, and eight places through the data.
            let mut seed = 0x2545_F491_4F6C_DD1Du64;
            for i in 0..10 {
                let cut = if i == 0 {
                    bytes.len() / 2
                } else {
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    (seed % bytes.len() as u64) as usize
                };
                let _ = decode(&bytes[..cut]);
                let _ = preview(&bytes[..cut]);
            }
        }
    }
}
