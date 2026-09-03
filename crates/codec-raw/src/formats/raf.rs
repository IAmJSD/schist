//! RAF — Fujifilm's own container, and the only one here that is not a
//! TIFF at heart.
//!
//! A RAF is three blobs bolted to a fixed 100-byte header: the
//! camera's full-size JPEG (which carries the only Exif in the file),
//! a tagged "CFA header" of Fujifilm's own metadata, and the sensor
//! frame. The header gives each an offset and a length as big-endian
//! 32-bit words, at 84, 92 and 100 respectively; nothing else in the
//! file points anywhere, so a RAF is read strictly top-down.
//!
//! The metadata block is a big-endian count followed by that many
//! `tag: u16, size: u16, data[size]` records. The handful this decoder
//! needs are named in [`meta`]; the rest are colour-science tables the
//! camera uses for its own JPEG engine.
//!
//! The sensor frame comes in two generations. Cameras up to about 2010
//! store bare 16-bit samples. Everything since wraps them in a tiny
//! private TIFF whose one IFD0 entry, 0xF000, points at the real
//! directory: dimensions, bit depth, one strip, the black levels and
//! the as-shot white balance. That strip is either 16-bit words, one
//! of two packings of narrower samples, or Fujifilm's own compression
//! (see [`super::raf_compressed`]).
//!
//! Everything here was written from observation of real files, the
//! oracle outputs beside them and ExifTool's published RAF tag tables.

use crate::bits::{BitPump, BitPumpMsb32};
use crate::formats::common;
use crate::tiff::Tiff;
use crate::{Cfa, CfaColor, Error, Format, RawData, RawImage, Rect, Result};
use rayon::prelude::*;

/// The magic every RAF starts with. The 16th byte is a space in every
/// file seen, but it is not checked: [`crate::probe`] matches 15.
const MAGIC: &[u8] = b"FUJIFILMCCD-RAW";

/// Tags of the "CFA header" metadata block, as ExifTool documents them.
pub mod meta {
    /// Full sensor frame, `(height, width)`.
    pub const RAW_FULL_SIZE: u16 = 0x0100;
    /// Where the picture starts inside that frame, `(top, left)`.
    pub const CROP_TOP_LEFT: u16 = 0x0110;
    /// How much of it is picture, `(height, width)`.
    pub const CROPPED_SIZE: u16 = 0x0111;
    /// SuperCCD bodies only: the photosite lattice's size, `(height,
    /// width)`, in column-staggered terms (ExifTool's RawImageSize).
    pub const RAW_IMAGE_SIZE: u16 = 0x0121;
    /// Four bytes describing how the samples are laid out; see
    /// [`super::Layout`].
    pub const FUJI_LAYOUT: u16 = 0x0130;
    /// The 6x6 X-Trans filter array, one byte a cell.
    pub const XTRANS_LAYOUT: u16 = 0x0131;
    /// `(bits, bits * 3)` — the only bit depth the pre-TIFF
    /// generation records.
    pub const BIT_DEPTH: u16 = 0x0141;
    /// Per-CFA-position black, in G R G2 B order.
    pub const BLACK_LEVELS: u16 = 0x4000;
    /// As-shot white balance, in G R G2 B order.
    pub const WB_GRGB: u16 = 0x2ff0;
}

/// Tags of the private TIFF in front of the sensor frame. ExifTool
/// names the ones it knows under `FujiFilm::RAFTags`; the numbering is
/// Fujifilm's own and has nothing to do with TIFF 6.0.
mod raw_tags {
    /// IFD0's only entry: an offset (relative to the block) to the
    /// directory that actually says anything.
    pub const RAW_IFD: u16 = 0xf000;
    pub const WIDTH: u16 = 0xf001;
    pub const HEIGHT: u16 = 0xf002;
    pub const BITS_PER_SAMPLE: u16 = 0xf003;
    /// Where the samples start, relative to the block.
    pub const STRIP_OFFSET: u16 = 0xf007;
    pub const STRIP_BYTE_COUNT: u16 = 0xf008;
    /// Black, one value a CFA position: 4 for Bayer, 36 for X-Trans.
    pub const BLACK_LEVELS: u16 = 0xf00a;
    /// White balance the camera's auto mode chose, `(G, R, B)`.
    pub const WB_AUTO: u16 = 0xf00d;
    /// White balance the shot was actually taken at, `(G, R, B)`.
    pub const WB_AS_SHOT: u16 = 0xf00e;
}

/// The fixed header, with every blob bounds-checked against the file.
struct Header<'a> {
    bytes: &'a [u8],
    /// Bytes 28..60, NUL-padded.
    model: String,
    /// Position of the camera's JPEG in `bytes`, and the JPEG itself.
    jpeg_at: usize,
    jpeg: &'a [u8],
    meta: &'a [u8],
    /// Position of the sensor block in `bytes`; offsets inside the
    /// private TIFF are relative to it.
    cfa_at: usize,
    cfa: &'a [u8],
}

fn be16(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}

fn be32(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

impl<'a> Header<'a> {
    fn parse(bytes: &'a [u8]) -> Result<Header<'a>> {
        if !bytes.starts_with(MAGIC) {
            return Err(Error::NotRaw);
        }
        // The three pointer pairs sit at fixed offsets; a file too
        // short to hold them is not a RAF at all.
        let slice = |at: usize, what: &str| -> Result<(usize, &'a [u8])> {
            let (offset, len) = match (be32(bytes, at), be32(bytes, at + 4)) {
                (Some(offset), Some(len)) => (offset as usize, len as usize),
                _ => return Err(Error::Corrupt("truncated RAF header".into())),
            };
            let end = offset
                .checked_add(len)
                .ok_or_else(|| Error::Corrupt(format!("{what} length out of range")))?;
            let blob = bytes.get(offset..end).ok_or_else(|| {
                Error::Corrupt(format!("{what} at {offset}..{end} is outside the file"))
            })?;
            Ok((offset, blob))
        };
        let (jpeg_at, jpeg) = slice(84, "the preview JPEG")?;
        let (_, meta) = slice(92, "the metadata block")?;
        let (cfa_at, cfa) = slice(100, "the sensor frame")?;
        let model = bytes
            .get(28..60)
            .map(|raw| {
                String::from_utf8_lossy(raw)
                    .trim_end_matches('\0')
                    .trim()
                    .to_string()
            })
            .unwrap_or_default();
        Ok(Header {
            bytes,
            model,
            jpeg_at,
            jpeg,
            meta,
            cfa_at,
            cfa,
        })
    }

    /// Every record of the metadata block, in file order. A record
    /// whose size runs off the end simply ends the list: the block is
    /// self-delimiting and a truncated one still has usable records
    /// before the break.
    fn meta_records(&self) -> Vec<(u16, &'a [u8])> {
        let Some(count) = be32(self.meta, 0) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut at = 4usize;
        // The count is a 32-bit field in a block that is a few tens of
        // kilobytes; cap it at what could physically fit.
        for _ in 0..count.min(self.meta.len() as u32 / 4) {
            let (Some(tag), Some(size)) = (be16(self.meta, at), be16(self.meta, at + 2)) else {
                break;
            };
            let Some(data) = self.meta.get(at + 4..at + 4 + size as usize) else {
                break;
            };
            out.push((tag, data));
            at += 4 + size as usize;
        }
        out
    }
}

/// One metadata record's payload, read as big-endian 16-bit words.
fn words(data: &[u8], n: usize) -> Option<Vec<u16>> {
    if data.len() < n * 2 {
        return None;
    }
    Some((0..n).filter_map(|i| be16(data, i * 2)).collect())
}

/// What tag 0x0130 says about the sample arrangement.
///
/// The second byte's bit 3 is *clear* on the SuperCCD bodies, whose
/// octagonal photosites sit on a square lattice turned 45 degrees and
/// are stored as an axis-aligned rectangle nearly twice as wide as the
/// picture (see [`Cfa::SuperCcd`]); it is set on every X-Trans and
/// Bayer body, where the samples are the picture as stored. The first
/// byte's top bit says which axis of that rectangle carries the
/// lattice's half-pitch stagger: set, each stored row is one lattice
/// row (the FinePix bodies); clear, each stored column is one lattice
/// column (the DBP back for the GX680). It is not, as this module
/// once read it, a mark of two interleaved fields.
struct Layout {
    row_staggered: bool,
    rotated: bool,
}

impl Layout {
    fn parse(data: &[u8]) -> Layout {
        Layout {
            row_staggered: data.first().is_some_and(|b| b & 0x80 != 0),
            rotated: data.get(1).is_some_and(|b| b & 8 == 0),
        }
    }
}

/// The DBP for GX680, Fujifilm's digital back for the GX680 camera,
/// as its RAF header names it. The only column-staggered SuperCCD
/// body seen, and the one whose frame the file describes least: the
/// margins, the 90-degree turn and the tiling below are keyed on this
/// name.
const DBP_FOR_GX680: &str = "DBP for GX680";

/// The DBP writes its 16-bit samples big-endian, unlike every other
/// RAF, and as eight vertical tiles laid end to end: all of tile 0's
/// rows, then tile 1's, each row `width / 8` samples.
const DBP_TILES: usize = 8;

/// A SuperCCD frame's geometry, from which [`Cfa::super_ccd`] and the
/// crop follow.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SuperCcd {
    row_staggered: bool,
    /// The photosites' rectangle inside the stored frame.
    active: Rect,
    /// Photosites along one stagger line.
    fuji_width: usize,
}

/// The active rectangle and fuji width of a rotated body's frame.
///
/// For the FinePix bodies tag 0x0121 gives the lattice as `(height,
/// width)` in column-staggered terms — a lattice row per stored row
/// pair — so a row-staggered frame halves the width and doubles the
/// height to get the rectangle it actually stores. The margins centre
/// that rectangle in the frame, rounded down to an even number so the
/// filter phase is preserved. The DBP's tag (7680 x 2720) describes
/// nothing the frame holds; its 32-column and 8-row margins are a
/// fact about the body. In both cases the photosites used are the
/// rows from the top margin to its mirror at the bottom, and exactly
/// one stagger line of columns.
fn super_ccd_geometry(
    layout: &Layout,
    image_size: Option<&[u8]>,
    width: usize,
    height: usize,
    dbp: bool,
) -> Result<SuperCcd> {
    let (active_w, left, top) = if dbp {
        (width.saturating_sub(64), 32, 8)
    } else {
        let size = image_size
            .and_then(|data| words(data, 2))
            .ok_or_else(|| Error::Corrupt("SuperCCD frame without its lattice size".into()))?;
        let (lattice_h, lattice_w) = (size[0] as usize, size[1] as usize);
        let (active_w, active_h) = if layout.row_staggered {
            (lattice_w / 2, lattice_h * 2)
        } else {
            (lattice_w, lattice_h)
        };
        if active_w == 0 || active_h == 0 || active_w > width || active_h > height {
            return Err(Error::Corrupt(format!(
                "SuperCCD lattice of {active_w}x{active_h} in a {width}x{height} frame"
            )));
        }
        let left = (width - active_w) / 4 * 2;
        let top = (height - active_h) / 4 * 2;
        (active_w, left, top)
    };
    let stagger = usize::from(layout.row_staggered);
    let fuji_width = active_w >> (1 - stagger);
    let line = fuji_width << (1 - stagger);
    let rows = height.saturating_sub(2 * top);
    if fuji_width == 0 || rows == 0 || left + line > width {
        return Err(Error::Corrupt(format!(
            "SuperCCD frame of {width}x{height} holds no lattice"
        )));
    }
    Ok(SuperCcd {
        row_staggered: layout.row_staggered,
        active: Rect {
            x: left,
            y: top,
            width: line,
            height: rows,
        },
        fuji_width,
    })
}

/// The sheared frame's 2x2 pattern for a fuji width: with an even
/// count of photosites along a stagger line the frame reads G B over
/// R G from its origin, with an odd one R G over G B. Both corpus
/// bodies are even.
fn super_ccd_bayer(fuji_width: usize) -> [CfaColor; 4] {
    if fuji_width.is_multiple_of(2) {
        [
            CfaColor::Green,
            CfaColor::Blue,
            CfaColor::Red,
            CfaColor::Green,
        ]
    } else {
        [
            CfaColor::Red,
            CfaColor::Green,
            CfaColor::Green,
            CfaColor::Blue,
        ]
    }
}

/// The DBP's tiled, big-endian strip stitched into one frame: tile
/// `t`'s row `y` lands at columns `t * tile .. (t + 1) * tile` of row
/// `y`. A strip too short for a tile leaves it black.
fn unpack_words16_be_tiles(
    strip: &[u8],
    width: usize,
    height: usize,
    tiles: usize,
) -> Result<Vec<u16>> {
    if tiles == 0 || !width.is_multiple_of(tiles) {
        return Err(Error::Corrupt(format!(
            "a {width}-sample row does not split into {tiles} tiles"
        )));
    }
    let tile = width / tiles;
    let mut out = vec![0u16; width * height];
    out.par_chunks_mut(width).enumerate().for_each(|(y, row)| {
        for (t, span) in row.chunks_exact_mut(tile).enumerate() {
            let start = (t * height + y) * tile * 2;
            let bytes = strip.get(start..start + tile * 2).unwrap_or(&[]);
            for (sample, word) in span.iter_mut().zip(bytes.as_chunks::<2>().0) {
                *sample = u16::from_be_bytes(*word);
            }
        }
    });
    Ok(out)
}

/// The 6x6 X-Trans array from tag 0x0131.
///
/// The 36 bytes are 0 red, 1 green, 2 blue — but they run from the
/// *last* cell backwards: the camera writes the array as the sensor is
/// read out, which is a 180-degree turn from the frame origin the
/// samples arrive in. Reversing them reproduces the filter pattern the
/// oracle reports for every X-Trans body in the corpus (three
/// generations, two distinct arrays).
fn xtrans_cfa(data: &[u8]) -> Result<Cfa> {
    if data.len() < 36 {
        return Err(Error::Corrupt(format!(
            "X-Trans layout is {} bytes, not 36",
            data.len()
        )));
    }
    let mut grid = [[CfaColor::Green; 6]; 6];
    for y in 0..6 {
        for x in 0..6 {
            grid[y][x] = match data[35 - (y * 6 + x)] {
                0 => CfaColor::Red,
                1 => CfaColor::Green,
                2 => CfaColor::Blue,
                other => {
                    return Err(Error::Corrupt(format!(
                        "X-Trans layout holds colour {other}"
                    )))
                }
            };
        }
    }
    Ok(Cfa::XTrans(grid))
}

/// The sensor frame's own description, however the file states it.
struct Frame<'a> {
    width: usize,
    height: usize,
    bits: u32,
    /// The samples, without the private TIFF around them.
    strip: &'a [u8],
    /// Black per CFA position, in the file's order (4 or 36 values).
    black: Vec<u32>,
    /// As-shot white balance as `(G, R, B)`.
    wb: Option<[f32; 3]>,
}

/// The private TIFF in front of the samples on 2011-and-later bodies.
fn parse_raw_tiff<'a>(header: &Header<'a>) -> Result<Option<Frame<'a>>> {
    if !matches!(header.cfa.get(0..2), Some(b"II") | Some(b"MM")) {
        return Ok(None);
    }
    let outer = Tiff::parse_embedded(header.bytes, header.cfa_at)?;
    // IFD0 carries a single entry pointing at the directory that holds
    // everything. It is type 13 (IFD) but not SubIFDs, so the shared
    // parser does not follow it for us.
    let at = outer
        .root()
        .get(raw_tags::RAW_IFD)
        .and_then(|e| e.u32(0))
        .ok_or_else(|| Error::Corrupt("sensor TIFF without its 0xF000 directory pointer".into()))?;
    let at = (at as usize)
        .checked_add(header.cfa_at)
        .ok_or_else(|| Error::Corrupt("0xF000 offset out of range".into()))?;
    let inner = Tiff::parse_at_relative(header.bytes, at, header.cfa_at, outer.little_endian())?;
    let ifd = inner.root();
    let int = |tag: u16| ifd.get(tag).and_then(|e| e.u32(0));
    let width =
        int(raw_tags::WIDTH).ok_or_else(|| Error::Corrupt("sensor TIFF without a width".into()))?;
    let height = int(raw_tags::HEIGHT)
        .ok_or_else(|| Error::Corrupt("sensor TIFF without a height".into()))?;
    let bits = int(raw_tags::BITS_PER_SAMPLE).unwrap_or(16);
    let offset = int(raw_tags::STRIP_OFFSET).unwrap_or(0) as usize;
    let len = int(raw_tags::STRIP_BYTE_COUNT).unwrap_or(0) as usize;
    let end = offset
        .checked_add(len)
        .ok_or_else(|| Error::Corrupt("sensor strip length out of range".into()))?;
    let strip = header.cfa.get(offset..end).ok_or_else(|| {
        Error::Corrupt(format!("sensor strip {offset}..{end} is outside its block"))
    })?;
    let black = ifd
        .get(raw_tags::BLACK_LEVELS)
        .map(|e| e.u32s())
        .unwrap_or_default();
    // 0xF00E is the balance the picture was shot at and 0xF00D the one
    // auto mode would have picked; they differ only when the shot was
    // not on auto, and the oracle's "As shot" follows 0xF00E.
    let wb = ifd
        .get(raw_tags::WB_AS_SHOT)
        .or_else(|| ifd.get(raw_tags::WB_AUTO))
        .and_then(|e| {
            let v = e.u32s();
            Some([*v.first()? as f32, *v.get(1)? as f32, *v.get(2)? as f32])
        });
    Ok(Some(Frame {
        width: width as usize,
        height: height as usize,
        bits,
        strip,
        black,
        wb,
    }))
}

/// How the samples of an uncompressed strip are stored.
///
/// Fujifilm has used three uncompressed layouts, and the strip's own
/// byte count tells them apart without guessing: it is exactly the
/// pixel count times two (16-bit words) or times the bit depth over
/// eight (packed). A compressed strip is recognised before either, by
/// its own header (see [`super::raf_compressed::is_compressed`]).
///
/// The two packings differ, and not in a way any tag announces. The
/// 12-bit EXR compacts pack least significant bit first across plain
/// bytes. The 14-bit Bayer bodies (X-A5 and its relatives) pack most
/// significant bit first *inside 32-bit little-endian words*, which is
/// what [`BitPumpMsb32`] reads; their 0xF005 tag counts those words a
/// row, which is how the layout announces itself indirectly. Both are
/// pinned to the oracle sample for sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Storage {
    Words16,
    /// Least significant bit first over bytes.
    PackedLsb(u32),
    /// Most significant bit first inside 32-bit little-endian words.
    PackedMsb32(u32),
    /// Fujifilm's own entropy coding, which says so in its own header.
    Compressed,
    /// A strip that is none of the above: a layout this decoder does
    /// not know, rather than one it can guess at.
    Unknown,
}

fn storage(strip: &[u8], pixels: usize, bits: u32, len: usize) -> Storage {
    // The compressed strips announce themselves, and asking them
    // first is what keeps a frame whose compressed size happens to
    // land on one of the packings from being unpacked as noise.
    if super::raf_compressed::is_compressed(strip) {
        return Storage::Compressed;
    }
    if len == pixels * 2 {
        return Storage::Words16;
    }
    if (8..=16).contains(&bits) {
        let packed_bits = (pixels as u64) * bits as u64;
        if packed_bits.is_multiple_of(8) && len as u64 == packed_bits / 8 {
            return if bits >= 13 {
                Storage::PackedMsb32(bits)
            } else {
                Storage::PackedLsb(bits)
            };
        }
    }
    Storage::Unknown
}

/// Little-endian 16-bit words, the simplest and oldest layout.
fn unpack_words16(strip: &[u8], pixels: usize) -> Vec<u16> {
    let mut out = vec![0u16; pixels];
    let (words, _) = strip.as_chunks::<2>();
    for (sample, word) in out.iter_mut().zip(words) {
        *sample = u16::from_le_bytes(*word);
    }
    out
}

/// `bits` samples an entry, one row at a time.
///
/// Rows start on a whole number of the packing's own units — a byte
/// for the LSB-first layout, a 32-bit word for the other — for every
/// real frame, so they unpack independently and in parallel. When a
/// width makes that untrue the whole strip is read as one run, which
/// is what a stream reader would do anyway.
fn unpack_packed(strip: &[u8], width: usize, height: usize, bits: u32, msb32: bool) -> Vec<u16> {
    let mut out = vec![0u16; width * height];
    let row_bits = width * bits as usize;
    let unit = if msb32 { 32 } else { 8 };
    if row_bits.is_multiple_of(unit) && height > 0 {
        let row_bytes = row_bits / 8;
        out.par_chunks_mut(width).enumerate().for_each(|(y, row)| {
            let start = y * row_bytes;
            let bytes = strip.get(start..start + row_bytes).unwrap_or(&[]);
            unpack_packed_row(bytes, row, bits, msb32);
        });
    } else {
        unpack_packed_row(strip, &mut out, bits, msb32);
    }
    out
}

fn unpack_packed_row(bytes: &[u8], out: &mut [u16], bits: u32, msb32: bool) {
    // Both pumps read zeros past the end, so a truncated strip leaves
    // the tail of the frame black rather than failing.
    if msb32 {
        let mut pump = BitPumpMsb32::new(bytes);
        for sample in out.iter_mut() {
            *sample = pump.get(bits) as u16;
        }
        return;
    }
    let mask = (1u32 << bits) - 1;
    let (mut cache, mut have) = (0u32, 0u32);
    let mut at = 0usize;
    for sample in out.iter_mut() {
        while have < bits {
            let byte = bytes.get(at).copied().unwrap_or(0) as u32;
            at += 1;
            cache |= byte << have;
            have += 8;
        }
        *sample = (cache & mask) as u16;
        cache >>= bits;
        have -= bits;
    }
}

/// Black levels reduced to the four slots [`RawImage`] has.
///
/// Bayer frames record one value per CFA position and map straight
/// across. X-Trans frames record all 36 cells of the array; every file
/// seen writes the same number 36 times, since the levels come from
/// the sensor's masked columns and not from the colour, so the mean
/// loses nothing and is honest when a file disagrees.
fn black_levels(values: &[u32]) -> Option<[f32; 4]> {
    match values.len() {
        0 => None,
        4 => Some([
            values[0] as f32,
            values[1] as f32,
            values[2] as f32,
            values[3] as f32,
        ]),
        n => {
            let mean = values.iter().map(|v| *v as f64).sum::<f64>() / n as f64;
            Some([mean as f32; 4])
        }
    }
}

/// Black or white-balance levels the old metadata block writes in
/// G, R, G2, B order, spread over the CFA positions they belong to.
fn grgb_by_position(cfa: &Cfa, values: &[u16]) -> Option<[f32; 4]> {
    let (g, r, g2, b) = (
        *values.first()? as f32,
        *values.get(1)? as f32,
        *values.get(2)? as f32,
        *values.get(3)? as f32,
    );
    let mut out = [0.0f32; 4];
    let mut greens = 0;
    for (i, level) in out.iter_mut().enumerate() {
        *level = match cfa.color_at(i % 2, i / 2)? {
            CfaColor::Red => r,
            CfaColor::Blue => b,
            _ => {
                greens += 1;
                if greens == 1 {
                    g
                } else {
                    g2
                }
            }
        };
    }
    Some(out)
}

/// The Exif of a RAF lives in the camera's JPEG and nowhere else, so
/// orientation, ISO, lens and the rest come from its APP1 segment.
/// The scan walks marker segments only and stops at the first scan.
fn jpeg_exif<'a>(bytes: &'a [u8], jpeg_at: usize, jpeg: &[u8]) -> Option<Tiff<'a>> {
    if !jpeg.starts_with(&[0xff, 0xd8]) {
        return None;
    }
    let mut at = 2usize;
    while at + 4 <= jpeg.len() {
        if jpeg[at] != 0xff {
            return None;
        }
        let marker = jpeg[at + 1];
        match marker {
            // Fill bytes, and the standalone markers with no length.
            0xff => {
                at += 1;
                continue;
            }
            0x01 | 0xd0..=0xd8 => {
                at += 2;
                continue;
            }
            // Entropy-coded data starts here; there is no Exif past it.
            0xda | 0xd9 => return None,
            _ => {}
        }
        let len = u16::from_be_bytes([jpeg[at + 2], jpeg[at + 3]]) as usize;
        if len < 2 {
            return None;
        }
        if marker == 0xe1 && jpeg.get(at + 4..at + 10) == Some(b"Exif\0\0") {
            return Tiff::parse_embedded(bytes, jpeg_at + at + 10).ok();
        }
        at += 2 + len;
    }
    None
}

/// Decode a RAF.
pub fn decode(bytes: &[u8]) -> Result<RawImage> {
    let header = Header::parse(bytes)?;
    let records = header.meta_records();
    let meta = |tag: u16| {
        records
            .iter()
            .find(|(t, _)| *t == tag)
            .map(|(_, data)| *data)
    };

    let layout = meta(meta::FUJI_LAYOUT).map(Layout::parse);
    // SuperCCD: the samples are photosites of a grid turned 45
    // degrees. The stored rectangle decodes like any uncompressed
    // frame; what differs is the filter array (a `Cfa::SuperCcd`,
    // which carries the geometry `develop` needs to un-shear it), the
    // levels and the crop, all set below.
    let super_ccd = layout.as_ref().filter(|layout| layout.rotated);
    let dbp = super_ccd.is_some() && header.model == DBP_FOR_GX680;

    let cfa = match meta(meta::XTRANS_LAYOUT) {
        Some(data) => xtrans_cfa(data)?,
        // Fujifilm's Bayer bodies record no array at all. Every one in
        // the corpus (GFX, X-A, the EXR compacts) reads out RGGB from
        // the frame origin, and so does every oracle for them.
        None => Cfa::RGGB,
    };

    let full = meta(meta::RAW_FULL_SIZE).and_then(|data| words(data, 2));
    let frame = parse_raw_tiff(&header)?;
    let (width, height) = match (&frame, &full) {
        (Some(frame), _) => (frame.width, frame.height),
        (None, Some(size)) => (size[1] as usize, size[0] as usize),
        (None, None) => return Err(Error::Corrupt("RAF states no sensor frame size".into())),
    };
    if width == 0 || height == 0 || width > 1 << 16 || height > 1 << 16 {
        return Err(Error::Corrupt(format!("sensor frame is {width}x{height}")));
    }
    let pixels = width * height;

    // The private TIFF's depth wins; the older generation records it
    // only in the metadata block, as (bits, bits * 3).
    let bits = match &frame {
        Some(frame) => frame.bits,
        None => meta(meta::BIT_DEPTH)
            .and_then(|data| be16(data, 0))
            .map(u32::from)
            .unwrap_or(16),
    };
    if !(8..=16).contains(&bits) {
        return Err(Error::Unsupported(format!("RAF: {bits} bits a sample")));
    }

    let strip = match &frame {
        Some(frame) => frame.strip,
        None => header.cfa,
    };
    let data = match storage(strip, pixels, bits, strip.len()) {
        Storage::Words16 if dbp => unpack_words16_be_tiles(strip, width, height, DBP_TILES)?,
        Storage::Words16 => unpack_words16(strip, pixels),
        Storage::PackedLsb(bits) => unpack_packed(strip, width, height, bits, false),
        Storage::PackedMsb32(bits) => unpack_packed(strip, width, height, bits, true),
        Storage::Compressed => super::raf_compressed::decode(strip, width, height, &cfa)?,
        Storage::Unknown => {
            return Err(Error::Unsupported(format!(
            "RAF: a {}-byte strip is no known layout of a {width}x{height} frame at {bits} bits",
            strip.len()
        )))
        }
    };

    let mut raw = RawImage::new(Format::Raf, width, height, 1, RawData::U16(data), cfa);
    if let Some(layout) = super_ccd {
        let geometry = super_ccd_geometry(layout, meta(meta::RAW_IMAGE_SIZE), width, height, dbp)?;
        raw.cfa = Cfa::super_ccd(
            geometry.row_staggered,
            geometry.fuji_width,
            super_ccd_bayer(geometry.fuji_width),
            (geometry.active.x, geometry.active.y),
        );
        // The inset-crop tags (0x0110/0x0111) describe the camera's
        // JPEG framing on the lattice, not anything in the stored
        // rectangle; the picture is the whole active rectangle.
        raw.crop = geometry.active;
        // Neither body records a saturation level. The DBP's samples
        // are 12-bit (its frame clips at 4095); the FinePix bodies'
        // are 14-bit, and 0x3E00 is the level the SuperCCD note gives
        // for them (the reference's table value), used as it stands —
        // `develop` does not lower it to the frame's brightest sample
        // as the reference does.
        raw.white_level = if dbp { 4095.0 } else { 15872.0 };
        // Tag 0x4000 names the colours G, R, G2, B; the stored
        // rectangle has no 2x2 to spread them over, so they are kept
        // per colour in `wb_coeffs` order (R, G, B, G2).
        if let Some(v) = meta(meta::BLACK_LEVELS).and_then(|data| words(data, 4)) {
            raw.black_levels = [v[1] as f32, v[0] as f32, v[3] as f32, v[2] as f32];
        }
    } else {
        raw.white_level = ((1u32 << bits) - 1) as f32;
        if let Some(black) = frame.as_ref().and_then(|frame| black_levels(&frame.black)) {
            raw.black_levels = black;
        } else if let Some(black) = meta(meta::BLACK_LEVELS)
            .and_then(|data| words(data, 4))
            .and_then(|values| grgb_by_position(&raw.cfa, &values))
        {
            raw.black_levels = black;
        }
        raw.crop = crop(
            &raw.cfa,
            meta(meta::CROP_TOP_LEFT),
            meta(meta::CROPPED_SIZE),
            width,
            height,
        );
    }

    // White balance: three values on the newer bodies, four in the
    // older block because it names both greens. Green is the
    // reference in both, as it is in `wb_coeffs`.
    if let Some([g, r, b]) = frame.as_ref().and_then(|frame| frame.wb) {
        if g > 0.0 {
            raw.wb_coeffs = [r / g, 1.0, b / g, 1.0];
        }
    } else if let Some(values) = meta(meta::WB_GRGB).and_then(|data| words(data, 4)) {
        let (g, r, g2, b) = (
            values[0] as f32,
            values[1] as f32,
            values[2] as f32,
            values[3] as f32,
        );
        if g > 0.0 && g2 > 0.0 {
            raw.wb_coeffs = [r / g, 1.0, b / g, g2 / g];
        }
    }

    let exif = jpeg_exif(bytes, header.jpeg_at, header.jpeg);
    if let Some(exif) = &exif {
        raw.orientation = common::orientation(exif);
        raw.metadata = common::metadata(exif);
        let (make, model) = exif.make_model();
        if !model.is_empty() {
            raw.set_camera(&make, &model);
        }
    }
    if dbp {
        // The back's Exif says upright, but its lattice is stored
        // turned: the picture is portrait as decoded and lands
        // landscape after a quarter turn clockwise, which is how the
        // reference and the camera's own JPEG show it.
        raw.orientation = crate::Orientation::Rotate90CW;
    }
    if raw.model.is_empty() {
        raw.set_camera("FUJIFILM", &header.model);
    }
    if header.jpeg.starts_with(&[0xff, 0xd8]) {
        raw.preview = Some(header.jpeg.to_vec());
    }
    raw.apply_camera_table();
    Ok(raw)
}

/// The picture's rectangle inside the sensor frame.
///
/// Tags 0x0110 and 0x0111 give it directly, but their origin is not
/// always on the filter array's period — the X-Pro3 starts its picture
/// on row 13 of a six-row array. A crop whose phase differs from the
/// frame's would make `cfa` describe the frame and not the picture, so
/// the origin is moved forward to the next cell boundary and the size
/// shrunk to match. LibRaw does the same, which is why its "Raw inset"
/// line agrees with this to the pixel on every file in the corpus.
fn crop(
    cfa: &Cfa,
    top_left: Option<&[u8]>,
    size: Option<&[u8]>,
    width: usize,
    height: usize,
) -> Rect {
    let whole = Rect {
        x: 0,
        y: 0,
        width,
        height,
    };
    let (Some(top_left), Some(size)) = (
        top_left.and_then(|data| words(data, 2)),
        size.and_then(|data| words(data, 2)),
    ) else {
        return whole;
    };
    let period = match cfa {
        Cfa::XTrans(_) => 6,
        _ => 2,
    };
    let round_up = |v: usize| v.div_ceil(period) * period;
    let (top, left) = (top_left[0] as usize, top_left[1] as usize);
    let (x, y) = (round_up(left), round_up(top));
    let cropped = Rect {
        x,
        y,
        width: (size[1] as usize).saturating_sub(x - left),
        height: (size[0] as usize).saturating_sub(y - top),
    };
    if cropped.width == 0
        || cropped.height == 0
        || cropped.x + cropped.width > width
        || cropped.y + cropped.height > height
    {
        return whole;
    }
    cropped
}

/// The camera's JPEG, which a RAF always carries at full resolution.
pub fn preview(bytes: &[u8]) -> Result<Option<Vec<u8>>> {
    let header = Header::parse(bytes)?;
    Ok(header
        .jpeg
        .starts_with(&[0xff, 0xd8])
        .then(|| header.jpeg.to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal RAF: the header's three pointers and nothing else
    /// valid, to check the bounds checks rather than any decoding.
    fn skeleton(jpeg: (u32, u32), meta: (u32, u32), cfa: (u32, u32), len: usize) -> Vec<u8> {
        let mut out = vec![0u8; len.max(108)];
        out[..MAGIC.len()].copy_from_slice(MAGIC);
        out[MAGIC.len()] = b' ';
        for (at, (offset, size)) in [(84, jpeg), (92, meta), (100, cfa)] {
            out[at..at + 4].copy_from_slice(&offset.to_be_bytes());
            out[at + 4..at + 8].copy_from_slice(&size.to_be_bytes());
        }
        out
    }

    #[test]
    fn rejects_a_file_that_is_not_a_raf() {
        assert!(matches!(decode(b"not a raf at all"), Err(Error::NotRaw)));
    }

    #[test]
    fn rejects_blobs_outside_the_file() {
        let file = skeleton((108, 16), (124, 8), (1 << 30, 1 << 30), 200);
        assert!(matches!(decode(&file), Err(Error::Corrupt(_))));
        let file = skeleton((108, 16), (124, 8), (140, u32::MAX), 200);
        assert!(matches!(decode(&file), Err(Error::Corrupt(_))));
    }

    #[test]
    fn a_truncated_metadata_block_keeps_the_records_before_the_break() {
        let mut file = skeleton((108, 2), (124, 20), (150, 40), 200);
        file[108..110].copy_from_slice(&[0xff, 0xd8]);
        // Three records claimed, one complete, the second cut short.
        file[124..128].copy_from_slice(&3u32.to_be_bytes());
        file[128..132].copy_from_slice(&[0x01, 0x00, 0x00, 0x04]);
        file[132..136].copy_from_slice(&[0x00, 0x04, 0x00, 0x08]);
        file[136..140].copy_from_slice(&[0x01, 0x11, 0x00, 0x40]);
        let header = Header::parse(&file).unwrap();
        let records = header.meta_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].0, meta::RAW_FULL_SIZE);
        assert_eq!(words(records[0].1, 2), Some(vec![4, 8]));
    }

    #[test]
    fn the_xtrans_array_is_read_backwards() {
        // The X-T1/X-T10 generation's tag 0x0131, whose frame-anchored
        // array the oracle prints as R B / G G / G G / B R ...
        let layout = [
            2, 1, 1, 0, 1, 1, 0, 1, 1, 2, 1, 1, 1, 2, 0, 1, 0, 2, 0, 1, 1, 2, 1, 1, 2, 1, 1, 0, 1,
            1, 1, 0, 2, 1, 2, 0,
        ];
        let cfa = xtrans_cfa(&layout).unwrap();
        assert_eq!(cfa.color_at(0, 0), Some(CfaColor::Red));
        assert_eq!(cfa.color_at(1, 0), Some(CfaColor::Blue));
        assert_eq!(cfa.color_at(0, 1), Some(CfaColor::Green));
        assert_eq!(cfa.color_at(0, 3), Some(CfaColor::Blue));
        assert_eq!(cfa.color_at(1, 3), Some(CfaColor::Red));
        // Eight of each of red and blue, twenty green, as X-Trans is.
        let mut reds = 0;
        for y in 0..6 {
            for x in 0..6 {
                if cfa.color_at(x, y) == Some(CfaColor::Red) {
                    reds += 1;
                }
            }
        }
        assert_eq!(reds, 8);
        assert!(matches!(xtrans_cfa(&[0, 1, 2]), Err(Error::Corrupt(_))));
        assert!(matches!(xtrans_cfa(&[7; 36]), Err(Error::Corrupt(_))));
    }

    #[test]
    fn the_layout_byte_names_superccd() {
        // X-Trans and the modern Bayer bodies: bit 3 of byte 1 set,
        // nothing staggered.
        assert!(!Layout::parse(&[0x0c, 0x0c, 0x0c, 0x0c]).rotated);
        assert!(!Layout::parse(&[0x0a, 0x0b, 0x09, 0x08]).row_staggered);
        // The S9600's row-staggered SuperCCD, and the GX680 digital
        // back's column-staggered one.
        let s9600 = Layout::parse(&[0x83, 0x82, 0x81, 0x80]);
        assert!(s9600.rotated && s9600.row_staggered);
        let gx680 = Layout::parse(&[0x00, 0x01, 0x02, 0x01]);
        assert!(gx680.rotated && !gx680.row_staggered);
        // Too short to say anything is not a SuperCCD.
        assert!(!Layout::parse(&[0x83]).rotated);
    }

    /// The two bodies' geometry, as the SuperCCD note works it out.
    #[test]
    fn superccd_geometry_of_both_bodies() {
        let s9600 = Layout::parse(&[0x83, 0x82, 0x81, 0x80]);
        // Tag 0x0121 = (1844, 4896): halved and doubled for the
        // row-staggered store, centred with even margins.
        let g =
            super_ccd_geometry(&s9600, Some(&[0x07, 0x34, 0x13, 0x20]), 2512, 3688, false).unwrap();
        assert_eq!(
            g,
            SuperCcd {
                row_staggered: true,
                active: Rect {
                    x: 32,
                    y: 0,
                    width: 2448,
                    height: 3688
                },
                fuji_width: 2448,
            }
        );
        let gx680 = Layout::parse(&[0x00, 0x01, 0x02, 0x01]);
        let g =
            super_ccd_geometry(&gx680, Some(&[0x0a, 0xa0, 0x1e, 0x00]), 5504, 3856, true).unwrap();
        assert_eq!(
            g,
            SuperCcd {
                row_staggered: false,
                active: Rect {
                    x: 32,
                    y: 8,
                    width: 5440,
                    height: 3840
                },
                fuji_width: 2720,
            }
        );
        // A lattice the frame cannot hold, a missing tag, and a frame
        // too small for the DBP's margins are all refused.
        assert!(matches!(
            super_ccd_geometry(&s9600, Some(&[0x07, 0x34, 0x13, 0x20]), 2000, 3688, false),
            Err(Error::Corrupt(_))
        ));
        assert!(matches!(
            super_ccd_geometry(&s9600, None, 2512, 3688, false),
            Err(Error::Corrupt(_))
        ));
        assert!(matches!(
            super_ccd_geometry(&gx680, None, 64, 16, true),
            Err(Error::Corrupt(_))
        ));
        assert!(matches!(
            super_ccd_geometry(&gx680, None, 72, 20, true),
            Ok(SuperCcd { fuji_width: 4, .. })
        ));
    }

    /// The sheared frame's pattern follows the fuji width's parity,
    /// and the array built from it reads back the same pattern at the
    /// active origin.
    #[test]
    fn superccd_pattern_follows_the_fuji_width() {
        let Cfa::Bayer(gbrg) = Cfa::GBRG else {
            unreachable!()
        };
        let Cfa::Bayer(rggb) = Cfa::RGGB else {
            unreachable!()
        };
        assert_eq!(super_ccd_bayer(2448), gbrg);
        assert_eq!(super_ccd_bayer(2447), rggb);
        let cfa = Cfa::super_ccd(true, 2448, super_ccd_bayer(2448), (32, 0));
        assert_eq!(cfa.super_ccd_bayer((32, 0)), Some(gbrg));
    }

    /// The DBP's strip: big-endian words, tile by tile.
    #[test]
    fn the_dbp_strip_is_tiled_and_big_endian() {
        // A 4x2 frame in two tiles of 2 columns: tile 0 holds rows 0
        // and 1 of columns 0-1, tile 1 the same rows of columns 2-3.
        let strip: Vec<u8> = [1u16, 2, 3, 4, 5, 6, 7, 8]
            .iter()
            .flat_map(|v| v.to_be_bytes())
            .collect();
        let out = unpack_words16_be_tiles(&strip, 4, 2, 2).unwrap();
        assert_eq!(out, [1, 2, 5, 6, 3, 4, 7, 8]);
        // A short strip leaves the missing tile black, and a width
        // that does not split is refused.
        let out = unpack_words16_be_tiles(&strip[..8], 4, 2, 2).unwrap();
        assert_eq!(out, [1, 2, 0, 0, 3, 4, 0, 0]);
        assert!(unpack_words16_be_tiles(&strip, 5, 2, 2).is_err());
        assert!(unpack_words16_be_tiles(&strip, 4, 2, 0).is_err());
    }

    #[test]
    fn storage_follows_the_strip_length() {
        assert_eq!(storage(&[], 1000, 14, 2000), Storage::Words16);
        assert_eq!(storage(&[], 1000, 14, 1750), Storage::PackedMsb32(14));
        assert_eq!(storage(&[], 1000, 12, 1500), Storage::PackedLsb(12));
        assert_eq!(storage(&[], 1000, 14, 900), Storage::Unknown);
        // A compressed header wins over every byte-count rule, even
        // when the count happens to match a packing exactly.
        let mut strip = vec![
            0x49, 0x53, 0x01, 0x10, 0x0e, 0x0f, 0xc6, 0x18, 0x00, 0x17, 0xa0, 0x03, 0x00, 0x08,
            0x02, 0xa1,
        ];
        strip.resize(2000, 0);
        assert_eq!(storage(&strip, 1000, 14, 2000), Storage::Compressed);
    }

    #[test]
    fn twelve_bit_samples_come_out_least_significant_bit_first() {
        // 0xABC then 0x123 packs as BC 3A 12.
        let mut out = [0u16; 2];
        unpack_packed_row(&[0xbc, 0x3a, 0x12], &mut out, 12, false);
        assert_eq!(out, [0xabc, 0x123]);
        // Past the end the pump reads zeros rather than panicking.
        let mut out = [0u16; 4];
        unpack_packed_row(&[0xff], &mut out, 12, false);
        assert_eq!(out, [0xff, 0, 0, 0]);
    }

    #[test]
    fn fourteen_bit_samples_are_read_down_from_the_top_of_each_word() {
        // The word here is 0x3FFF0000: its top fourteen bits are
        // 0b00_1111_1111_1111 and the next fourteen 0b11_0000_0000_0000.
        let mut out = [0u16; 2];
        unpack_packed_row(&[0x00, 0x00, 0xff, 0x3f], &mut out, 14, true);
        assert_eq!(out, [0x0fff, 0x3000]);
        // A tail shorter than a word is padded with zeros, so the
        // frame runs out black instead of panicking.
        let mut out = [0u16; 4];
        unpack_packed_row(&[0xff], &mut out, 14, true);
        assert_eq!(out, [0x0000, 0x000f, 0x3c00, 0x0000]);
    }

    #[test]
    fn the_crop_moves_forward_to_the_filter_array_period() {
        let xtrans = xtrans_cfa(&[1; 36]).unwrap();
        // The X-Pro3: top 13, left 6, 4160x6240 -> top 18, 4155 high.
        let rect = crop(
            &xtrans,
            Some(&[0, 13, 0, 6]),
            Some(&[0x10, 0x40, 0x18, 0x60]),
            6384,
            4182,
        );
        assert_eq!(
            rect,
            Rect {
                x: 6,
                y: 18,
                width: 6240,
                height: 4155
            }
        );
        // A Bayer frame moves by at most one row or column.
        let rect = crop(
            &Cfa::RGGB,
            Some(&[0, 7, 0, 8]),
            Some(&[0x0f, 0xa0, 0x17, 0x70]),
            6016,
            4014,
        );
        assert_eq!(
            rect,
            Rect {
                x: 8,
                y: 8,
                width: 6000,
                height: 3999
            }
        );
        // A crop that does not fit is dropped rather than trusted.
        let rect = crop(
            &Cfa::RGGB,
            Some(&[0, 0, 0, 0]),
            Some(&[0xff, 0xff, 0xff, 0xff]),
            16,
            16,
        );
        assert_eq!(rect.width, 16);
    }

    #[test]
    fn the_old_block_names_both_greens() {
        // G R G2 B, spread over an RGGB array: R, G, G2, B.
        let levels = grgb_by_position(&Cfa::RGGB, &[518, 517, 517, 516]).unwrap();
        assert_eq!(levels, [517.0, 518.0, 517.0, 516.0]);
        // ... and over a GBRG one: G, B, R, G2.
        let levels = grgb_by_position(&Cfa::GBRG, &[129, 118, 129, 118]).unwrap();
        assert_eq!(levels, [129.0, 118.0, 118.0, 129.0]);
    }

    #[test]
    fn black_levels_collapse_to_four() {
        assert_eq!(black_levels(&[]), None);
        assert_eq!(black_levels(&[1, 2, 3, 4]), Some([1.0, 2.0, 3.0, 4.0]));
        assert_eq!(black_levels(&[1022; 36]), Some([1022.0; 4]));
    }
}

/// Corpus tests: every RAF under `SCHIST_RAW_CORPUS`, against the
/// oracle files beside it (`<name>.tiff` from `unprocessed_raw -T`,
/// `<name>.identify.txt` from `raw-identify -v -w`).
#[cfg(test)]
mod corpus {
    use super::*;
    use crate::Orientation;
    use std::path::{Path, PathBuf};

    /// Files this decoder knowingly refuses, and why. A corpus file
    /// that fails for any other reason is a test failure. Empty since
    /// the SuperCCD bodies gained `Cfa::SuperCcd`.
    const UNSUPPORTED: &[(&str, &str)] = &[];

    /// Files whose as-shot multipliers the oracle does not take from
    /// the file, and why this decoder's differ.
    const WB_NOT_FROM_THE_FILE: &[(&str, &str)] = &[
        // The DBP's 0x2FF0 record (and every preset beside it) reads
        // G R G2 B = 105 258 105 179, which ExifTool reports as is;
        // the oracle prints 196 105 153 105, every preset scaled by
        // the same factors of about 0.76 on red and 6/7 on blue, and
        // nothing in the file yields those. The oracle's values do
        // neutralise the scene (the file's give a strong magenta
        // cast), so they are right and the file's are in some other
        // gain domain, but a rule that cannot be read from the file
        // is not one this decoder can apply.
        (
            "DBP_for_GX680",
            "as-shot multipliers scaled by a rule the file does not carry",
        ),
    ];

    fn corpus_files() -> Vec<PathBuf> {
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
                    .is_some_and(|e| e.eq_ignore_ascii_case("raf"))
                {
                    out.push(path);
                }
            }
        }
        out.sort();
        out
    }

    /// The value after `key:` on the line that holds it.
    fn field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
        text.lines()
            .find(|line| line.trim_start().starts_with(key))
            .map(|line| line.trim_start()[key.len()..].trim())
    }

    fn numbers(text: &str) -> Vec<i64> {
        text.split(|c: char| !c.is_ascii_digit() && c != '-')
            .filter_map(|t| t.parse().ok())
            .collect()
    }

    struct Oracle {
        text: String,
    }

    impl Oracle {
        fn load(path: &Path) -> Option<Oracle> {
            let mut sidecar = path.as_os_str().to_os_string();
            sidecar.push(".identify.txt");
            std::fs::read_to_string(PathBuf::from(sidecar))
                .ok()
                .map(|text| Oracle { text })
        }
        /// `Full size: W x H`.
        fn full_size(&self) -> Option<(usize, usize)> {
            let v = numbers(field(&self.text, "Full size:")?);
            Some((*v.first()? as usize, *v.get(1)? as usize))
        }
        /// `Raw inset, width x height: W x H left: L top: T`.
        fn inset(&self) -> Option<Rect> {
            let v = numbers(field(&self.text, "Raw inset, width x height:")?);
            Some(Rect {
                width: *v.first()? as usize,
                height: *v.get(1)? as usize,
                x: *v.get(2)? as usize,
                y: *v.get(3)? as usize,
            })
        }
        fn flip(&self) -> Option<i64> {
            numbers(field(&self.text, "Image flip:")?).first().copied()
        }
        /// The 16 characters LibRaw prints are the colours of rows 0..7
        /// at columns 0 and 1.
        fn filter_pattern(&self) -> Option<String> {
            field(&self.text, "Filter pattern:").map(str::to_string)
        }
        /// `As shot  R G B G2` in the makernote white-balance table.
        fn as_shot(&self) -> Option<[f32; 4]> {
            let v = numbers(field(&self.text, "As shot")?);
            Some([
                *v.first()? as f32,
                *v.get(1)? as f32,
                *v.get(2)? as f32,
                *v.get(3)? as f32,
            ])
        }
        /// `cblack[a .. b]: ...`, LibRaw's per-channel black in R G B G2
        /// order (or, for X-Trans, all 36 array cells).
        fn cblack(&self) -> Option<Vec<i64>> {
            let line = self
                .text
                .lines()
                .find(|l| l.trim_start().starts_with("cblack["))?;
            Some(numbers(line.split_once(':')?.1))
        }
    }

    fn allowed_unsupported(path: &Path) -> Option<&'static str> {
        let name = path.file_name()?.to_str()?;
        UNSUPPORTED
            .iter()
            .find(|(stem, _)| name.contains(stem))
            .map(|(_, why)| *why)
    }

    #[test]
    fn decodes_the_corpus_against_the_oracle() {
        let files = corpus_files();
        if files.is_empty() {
            return;
        }
        let mut checked = 0;
        for path in &files {
            let bytes = std::fs::read(path).expect("corpus file");
            assert_eq!(
                crate::probe(&bytes),
                Some(Format::Raf),
                "{} did not probe as RAF",
                path.display()
            );
            let raw = match decode(&bytes) {
                Ok(raw) => raw,
                Err(Error::Unsupported(why)) => {
                    let allowed = allowed_unsupported(path);
                    assert!(
                        allowed.is_some(),
                        "{} is unsupported: {why}",
                        path.display()
                    );
                    continue;
                }
                Err(other) => panic!("{}: {other}", path.display()),
            };
            assert!(
                allowed_unsupported(path).is_none(),
                "{} decoded but is on the unsupported list",
                path.display()
            );
            raw.validate()
                .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            checked += 1;

            // The preview must be a JPEG a viewer can actually show.
            let preview = raw.preview.as_ref().expect("every RAF carries a JPEG");
            image::load_from_memory(preview)
                .unwrap_or_else(|e| panic!("{}: preview does not decode: {e}", path.display()));
            assert_eq!(preview, &super::preview(&bytes).unwrap().unwrap());

            // A RAF's only Exif is the one in its JPEG, so an empty
            // metadata block means the APP1 walk missed it.
            assert!(raw.metadata.iso.is_some(), "{}: no ISO", path.display());
            assert!(
                raw.metadata.exposure_time.is_some(),
                "{}: no exposure",
                path.display()
            );
            // The DBP is a back on a manual camera with no lens
            // coupling: its Exif has an aperture of 0, which is none.
            assert!(
                raw.metadata.f_number.is_some() || raw.model == DBP_FOR_GX680,
                "{}: no aperture",
                path.display()
            );
            assert!(
                raw.metadata.date_time.is_some(),
                "{}: no date",
                path.display()
            );
            assert_eq!(raw.clean_make, "Fujifilm", "{}: make", path.display());
            assert!(!raw.clean_model.is_empty(), "{}: model", path.display());

            let Some(oracle) = Oracle::load(path) else {
                continue;
            };
            if let Some((width, height)) = oracle.full_size() {
                assert_eq!(
                    (raw.width, raw.height),
                    (width, height),
                    "{}: frame size",
                    path.display()
                );
            }
            let super_ccd = matches!(raw.cfa, Cfa::SuperCcd { .. });
            // A SuperCCD's "Raw inset" is the camera's JPEG framing
            // on the lattice, which the oracle parses but never
            // applies; the crop it does use is the active rectangle,
            // checked in `superccd_worked_values` below.
            if let Some(inset) = oracle.inset().filter(|_| !super_ccd) {
                assert_eq!(raw.crop, inset, "{}: crop", path.display());
            }
            if let Some(flip) = oracle.flip() {
                let expect = match flip {
                    3 => Orientation::Rotate180,
                    5 => Orientation::Rotate270CW,
                    6 => Orientation::Rotate90CW,
                    _ => Orientation::Normal,
                };
                assert_eq!(raw.orientation, expect, "{}: orientation", path.display());
            }
            if let Some(pattern) = oracle.filter_pattern() {
                // For a SuperCCD the oracle prints the *sheared*
                // frame's pattern, the one the interpolation sees.
                let sheared = raw.cfa.super_ccd_bayer((raw.crop.x, raw.crop.y));
                let mine: String = (0..16)
                    .map(|i| {
                        let color = match &sheared {
                            Some(bayer) => Some(bayer[(i >> 1 & 1) * 2 + (i & 1)]),
                            None => raw.cfa.color_at(i & 1, i >> 1),
                        };
                        match color {
                            Some(CfaColor::Red) => 'R',
                            Some(CfaColor::Green) | Some(CfaColor::Green2) => 'G',
                            Some(CfaColor::Blue) => 'B',
                            _ => '?',
                        }
                    })
                    .collect();
                assert_eq!(mine, pattern, "{}: filter pattern", path.display());
            }
            let wb_elsewhere = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|name| {
                    WB_NOT_FROM_THE_FILE
                        .iter()
                        .any(|(stem, _)| name.contains(stem))
                });
            if let Some(shot) = oracle.as_shot().filter(|_| !wb_elsewhere) {
                // LibRaw prints the same R G B G2 multipliers this
                // crate stores, unnormalised, with green as the unit.
                let expect = [shot[0] / shot[1], 1.0, shot[2] / shot[1], 1.0];
                for (mine, want) in raw.wb_coeffs.iter().zip(expect.iter()) {
                    assert!(
                        (mine - want).abs() < 1e-4,
                        "{}: white balance {:?} vs {expect:?}",
                        path.display(),
                        raw.wb_coeffs
                    );
                }
            }
            if let Some(cblack) = oracle.cblack() {
                // LibRaw indexes its per-channel black by colour, this
                // crate by CFA position, so compare the sets.
                let mut mine: Vec<i64> = raw.black_levels.iter().map(|b| *b as i64).collect();
                let mut theirs: Vec<i64> = cblack.clone();
                if theirs.len() > 4 {
                    theirs.truncate(4);
                }
                mine.sort_unstable();
                theirs.sort_unstable();
                assert_eq!(mine, theirs, "{}: black levels", path.display());
            }

            let mut tiff = path.as_os_str().to_os_string();
            tiff.push(".tiff");
            let tiff = PathBuf::from(tiff);
            if !tiff.exists() {
                continue;
            }
            // `image::open` caps its allocation, and a 100-megapixel
            // oracle is past the cap; the file is ours, so lift it.
            let want = {
                let mut reader = image::ImageReader::open(&tiff)
                    .unwrap_or_else(|e| panic!("{}: {e}", tiff.display()));
                reader.limits(image::Limits::no_limits());
                reader
                    .decode()
                    .unwrap_or_else(|e| panic!("{}: {e}", tiff.display()))
                    .into_luma16()
            };
            assert_eq!(
                (want.width() as usize, want.height() as usize),
                (raw.width, raw.height),
                "{}: oracle frame size",
                path.display()
            );
            let RawData::U16(mine) = &raw.data else {
                panic!("{}: RAF is never float", path.display())
            };
            // Nothing prints Fujifilm's saturation level, so the check
            // is that the bit depth the file states actually holds the
            // samples it stores.
            let peak = mine.iter().copied().max().unwrap_or(0);
            assert!(
                f32::from(peak) <= raw.white_level,
                "{}: sample {peak} above the white level {}",
                path.display(),
                raw.white_level
            );
            let mut wrong = 0usize;
            let mut first = Vec::new();
            for (i, (mine, want)) in mine.iter().zip(want.as_raw().iter()).enumerate() {
                if mine != want {
                    wrong += 1;
                    if first.len() < 8 {
                        first.push((i % raw.width, i / raw.width, *mine, *want));
                    }
                }
            }
            assert_eq!(
                wrong,
                0,
                "{}: {wrong} samples differ from the oracle, first (x, y, mine, oracle): {first:?}",
                path.display()
            );
        }
        assert!(checked > 0, "the corpus held no decodable RAF");
    }

    /// The SuperCCD note's worked values for the two rotated bodies:
    /// geometry, stored samples and their colours and cells, levels
    /// and the scaled values the reference feeds its interpolator.
    #[test]
    fn superccd_worked_values() {
        use crate::{super_ccd_cell, Orientation};
        /// (x, y) in the active rectangle, sample, colour, cell, the
        /// value after black, multiplier and 65535/white.
        type Sample = ((usize, usize), u16, CfaColor, (i64, i64), u16);
        struct Body {
            stem: &'static str,
            row_staggered: bool,
            fuji_width: usize,
            crop: Rect,
            white: f32,
            effective_white: f32,
            black: [f32; 4],
            wb: [f32; 4],
            orientation: Orientation,
            samples: &'static [Sample],
        }
        let bodies = [
            Body {
                stem: "FinePix_S9600",
                row_staggered: true,
                fuji_width: 2448,
                crop: Rect {
                    x: 32,
                    y: 0,
                    width: 2448,
                    height: 3688,
                },
                white: 15872.0,
                effective_white: 15604.0,
                black: [0.0; 4],
                wb: [494.0 / 320.0, 1.0, 384.0 / 320.0, 1.0],
                orientation: Orientation::Normal,
                samples: &[
                    ((1000, 1000), 5855, CfaColor::Red, (1947, 1500), 37961),
                    ((1001, 1000), 5327, CfaColor::Blue, (1946, 1501), 26847),
                    ((1002, 1000), 5400, CfaColor::Red, (1945, 1502), 35011),
                    ((1000, 1001), 7206, CfaColor::Green, (1947, 1501), 30264),
                    ((1001, 1001), 6639, CfaColor::Green, (1946, 1502), 27883),
                    ((1001, 1002), 5229, CfaColor::Red, (1947, 1502), 33902),
                ],
            },
            Body {
                stem: "DBP_for_GX680",
                row_staggered: false,
                fuji_width: 2720,
                crop: Rect {
                    x: 32,
                    y: 8,
                    width: 5440,
                    height: 3840,
                },
                white: 4095.0,
                effective_white: 3977.0,
                black: [118.0, 129.0, 118.0, 129.0],
                // What the oracle prints (196 105 153 105) is not
                // what the file records; see WB_NOT_FROM_THE_FILE.
                wb: [258.0 / 105.0, 1.0, 179.0 / 105.0, 1.0],
                orientation: Orientation::Rotate90CW,
                samples: &[
                    ((2000, 1000), 195, CfaColor::Red, (2719, 2000), 2368),
                    ((2001, 1000), 296, CfaColor::Green, (2719, 2001), 2751),
                    ((2002, 1000), 261, CfaColor::Blue, (2718, 2001), 3433),
                    ((2000, 1001), 266, CfaColor::Blue, (2720, 2001), 3553),
                    ((2001, 1001), 312, CfaColor::Green, (2720, 2002), 3015),
                    ((2000, 1002), 201, CfaColor::Red, (2721, 2002), 2553),
                ],
            },
        ];
        // The reference's multipliers, for the scaled column.
        let reference_wb = [
            [494.0 / 320.0, 1.0, 384.0 / 320.0, 1.0],
            [196.0 / 105.0, 1.0, 153.0 / 105.0, 1.0],
        ];
        let files = corpus_files();
        for (body, reference_wb) in bodies.iter().zip(reference_wb) {
            let Some(path) = files
                .iter()
                .find(|p| p.to_str().is_some_and(|p| p.contains(body.stem)))
            else {
                continue;
            };
            let bytes = std::fs::read(path).expect("corpus file");
            let raw = decode(&bytes).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            let Cfa::SuperCcd {
                row_staggered,
                fuji_width,
                ..
            } = &raw.cfa
            else {
                panic!("{}: not decoded as SuperCCD: {:?}", path.display(), raw.cfa)
            };
            assert_eq!(
                (*row_staggered, *fuji_width),
                (body.row_staggered, body.fuji_width),
                "{}",
                body.stem
            );
            assert_eq!(raw.crop, body.crop, "{}: active rectangle", body.stem);
            assert_eq!(raw.white_level, body.white, "{}: white", body.stem);
            assert_eq!(raw.black_levels, body.black, "{}: black", body.stem);
            for (mine, want) in raw.wb_coeffs.iter().zip(body.wb) {
                assert!(
                    (mine - want).abs() < 1e-5,
                    "{}: wb {:?}",
                    body.stem,
                    raw.wb_coeffs
                );
            }
            assert_eq!(raw.orientation, body.orientation, "{}", body.stem);
            let RawData::U16(data) = &raw.data else {
                unreachable!()
            };
            for ((x, y), value, color, cell, scaled) in body.samples {
                let (sx, sy) = (raw.crop.x + x, raw.crop.y + y);
                assert_eq!(
                    data[sy * raw.width + sx],
                    *value,
                    "{}: sample ({x}, {y})",
                    body.stem
                );
                assert_eq!(
                    raw.cfa.color_at(sx, sy),
                    Some(*color),
                    "{}: colour ({x}, {y})",
                    body.stem
                );
                assert_eq!(
                    super_ccd_cell(*row_staggered, *fuji_width, *x, *y),
                    *cell,
                    "{}: cell ({x}, {y})",
                    body.stem
                );
                let channel = match color {
                    CfaColor::Red => 0,
                    CfaColor::Green => 1,
                    CfaColor::Blue => 2,
                    _ => 3,
                };
                // The note's white is already less the common black
                // (4095 - 118 = 3977 on the DBP).
                let v = (*value as f64 - body.black[channel] as f64).max(0.0)
                    * reference_wb[channel]
                    * 65535.0
                    / body.effective_white as f64;
                assert_eq!(v as u16, *scaled, "{}: scaled ({x}, {y}) = {v}", body.stem);
            }
            // The padding is not the picture: the S9600 fills its
            // margins with a constant, the DBP with masked black.
            let padding = data[1000 * raw.width];
            if body.row_staggered {
                assert_eq!(padding, 8192, "{}: fill value", body.stem);
            } else {
                assert!(padding < 1200, "{}: masked border {padding}", body.stem);
            }
        }
    }

    #[test]
    fn truncation_never_panics() {
        for path in corpus_files() {
            let bytes = std::fs::read(&path).expect("corpus file");
            for cut in 1..=10 {
                // Spread the cuts over the file, including inside the
                // header, the metadata block and the sensor strip.
                let at = bytes.len() * cut / 11;
                let _ = crate::probe(&bytes[..at]);
                let _ = decode(&bytes[..at]);
                let _ = super::preview(&bytes[..at]);
            }
            // A file whose pointers survive but whose payload is gone.
            let mut damaged = bytes.clone();
            for (i, byte) in damaged.iter_mut().enumerate().take(4096) {
                if i >= 108 {
                    *byte = 0xff;
                }
            }
            let _ = decode(&damaged);
        }
    }
}
