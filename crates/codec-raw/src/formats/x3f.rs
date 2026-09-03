//! Sigma X3F: the raw of the Foveon cameras, whose sensor stacks
//! three photodiodes at every pixel instead of putting a colour
//! filter over each one. An X3F frame therefore has three samples a
//! pixel and no CFA at all — [`Cfa::None`], `cpp = 3`.
//!
//! The container is Sigma's own. A file opens with `FOVb`, a version
//! and a header whose shape depends on that version, and *ends* with
//! a `u32` pointing at the directory: `SECd`, a version, a count, and
//! then twelve bytes an entry — offset, length, and a four-character
//! type.
//!
//! The sections that matter:
//!
//!  * `IMA2` (and `IMAG` on the oldest bodies) — an image, headed
//!    `SECi` with a type (1 raw, 2 processed), a data format, the
//!    dimensions and a row size. A camera writes two or three of
//!    them: a full-size JPEG, sometimes a small one, and the sensor
//!    data.
//!  * `PROP` — a property list, UTF-16 keys and values, holding the
//!    camera's name, exposure and white-balance mode. The Quattro
//!    bodies dropped it and put an Exif block in their preview JPEG
//!    instead, so both are read.
//!  * `CAMF` — the camera's calibration, including the matrices that
//!    turn Foveon's three stacked responses into colour. It is
//!    obfuscated, and decoding it is out of this module's scope:
//!    `color_matrix` is left to the camera table.
//!
//! The compressed sensor data is Foveon's "TRUE" entropy code. Two
//! generations decode here:
//!
//!  * **Merrill / TRUE-II** (format 30): three independent
//!    full-resolution planes, each a category-Huffman + signed
//!    difference bitstream over a two-parity horizontal predictor.
//!  * **Quattro** (format 35, and the sd-Quattro 37/39/41 variants):
//!    the same entropy engine, but the top layer is full resolution
//!    while the two below it are quarter resolution. The top layer is
//!    stored with a few overscan columns that are discarded; the two
//!    quarter-resolution layers land on the even-row/even-column
//!    lattice of the full frame, zero elsewhere — the exact shape of
//!    the LibRaw oracle (colour/AF interpolation disabled).
//!
//! Both emit three samples a pixel, plane order {0 bottom, 1 middle,
//! 2 top}, black/white left as metadata. Colour reconstruction (the
//! CAMF matrices, the Quattro's R/G + AF interpolation) is out of
//! scope: the camera table supplies the matrix.
//!
//! The bodies before the Merrill mark their sensor section with
//! *type 3* instead of 1 and come in two families:
//!
//!  * **TRUE-I** (type 3, format 30: DP1, DP1s, DP1x, DP2, DP2s, DP2x,
//!    SD15) — the Merrill engine exactly, on a 2688x1792 frame with its
//!    own crop and levels.
//!  * **The SD9/SD10/SD14 Huffman code** (type 3, format 6) — a
//!    per-file quantiser table and code table, a row-offset table at
//!    the end of the section, and the three colours interleaved pixel
//!    by pixel in one bitstream. The section holds no black, white or
//!    crop; a per-body table supplies them.
//!
//! Format 5 (the Polaroid x530's relabel of format 6), format 11 (the
//! 8-bit thumbnail flavour) and the uncompressed 32-bit packed variant
//! of the SD code (row stride non-zero) have no sample and stay
//! [`Error::Unsupported`] by name.

use crate::bits::{BitPump, BitPumpMsb};
use crate::tiff::Tiff;
use crate::{
    frame_samples, Cfa, Error, Format, Metadata, Orientation, RawData, RawImage, Rect, Result,
};
use rayon::prelude::*;

/// Every X3F begins here.
const MAGIC: &[u8; 4] = b"FOVb";

/// One directory entry: where a section is and what it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Section {
    offset: usize,
    length: usize,
    kind: [u8; 4],
}

/// An image section's header, and where its data starts.
#[derive(Debug, Clone, Copy)]
struct Image {
    /// 1 (Merrill and later) or 3 (every earlier body) for the
    /// sensor's own data, 2 for a processed picture.
    kind: u32,
    /// How the data is coded; see [`formats`].
    format: u32,
    columns: usize,
    rows: usize,
    /// The row stride in bytes: zero for every entropy-coded section,
    /// and on the SD code the mark of the packed uncompressed variant.
    row_size: usize,
    data: usize,
    length: usize,
}

/// The data formats an image section declares.
mod formats {
    /// Three 16-bit samples a pixel, uncompressed.
    pub const PLAIN: u32 = 3;
    /// The Polaroid x530's relabel of the SD Huffman code (the same
    /// engine, and the reference swaps its channels 0 and 2). No
    /// sample; unsupported by name.
    pub const X530: u32 = 5;
    /// The SD9/SD10/SD14 Huffman code: per-file quantiser and code
    /// tables, a row-offset table, three colours interleaved a pixel.
    pub const SD_HUFFMAN: u32 = 6;
    /// The 8-bit thumbnail flavour of the SD code (no quantiser table;
    /// the SD10 writes a 567x378 one). A preview, not sensor data.
    pub const SD_THUMBNAIL: u32 = 11;
    // 18 and 25 are the baseline JPEGs a camera writes for its
    // previews; they are recognised by their SOI rather than by
    // number, because the code varies with the body and any of them
    // may be the largest one.
    /// Foveon's "TRUE" entropy code: TRUE-II on the DP/SD Merrill and
    /// SD1 bodies (section type 1), TRUE-I on the DP1/DP2 compacts and
    /// the SD15 (type 3). The engine is identical; only the frame and
    /// its levels differ.
    pub const TRUE: u32 = 30;
    /// The Quattro's code, whose top layer is full resolution over two
    /// quarter-resolution layers. 37/39/41 are the sd-Quattro variants
    /// of the same geometry.
    pub const QUATTRO: u32 = 35;
    pub const SDQ_37: u32 = 37;
    pub const SDQ_39: u32 = 39;
    pub const SDQ_41: u32 = 41;
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

/// The file's sections, in directory order.
fn directory(bytes: &[u8]) -> Result<Vec<Section>> {
    if bytes.get(0..4) != Some(MAGIC) {
        return Err(Error::Corrupt("not an X3F: no FOVb signature".into()));
    }
    // The directory's position is the last four bytes of the file.
    let end = bytes
        .len()
        .checked_sub(4)
        .ok_or_else(|| Error::Corrupt("X3F too short to hold a directory pointer".into()))?;
    let start = u32_at(bytes, end).unwrap_or(0) as usize;
    if bytes.get(start..start + 4) != Some(b"SECd") {
        return Err(Error::Corrupt(
            "X3F directory pointer does not lead to a SECd".into(),
        ));
    }
    let count = u32_at(bytes, start + 8).unwrap_or(0) as usize;
    // Twelve bytes an entry: a count larger than the file could hold
    // is a lie, and capping it keeps the allocation honest.
    let count = count.min(bytes.len() / 12);
    let mut sections = Vec::with_capacity(count);
    for i in 0..count {
        let at = start + 12 + i * 12;
        let (Some(offset), Some(length)) = (u32_at(bytes, at), u32_at(bytes, at + 4)) else {
            break;
        };
        let Some(kind) = bytes.get(at + 8..at + 12) else {
            break;
        };
        let (offset, length) = (offset as usize, length as usize);
        // A section reaching past the end of the file is not one.
        if offset
            .checked_add(length)
            .is_none_or(|end| end > bytes.len())
        {
            continue;
        }
        sections.push(Section {
            offset,
            length,
            kind: [kind[0], kind[1], kind[2], kind[3]],
        });
    }
    if sections.is_empty() {
        return Err(Error::Corrupt(
            "X3F directory holds no readable section".into(),
        ));
    }
    Ok(sections)
}

/// An image section's header. Seven `u32`s — magic, version, type,
/// format, columns, rows, row size — and then the data.
fn image(bytes: &[u8], section: &Section) -> Option<Image> {
    if bytes.get(section.offset..section.offset + 4) != Some(b"SECi") {
        return None;
    }
    let word = |i: usize| u32_at(bytes, section.offset + i * 4);
    let data = section.offset + 28;
    Some(Image {
        kind: word(2)?,
        format: word(3)?,
        columns: word(4)? as usize,
        rows: word(5)? as usize,
        row_size: word(6)? as usize,
        data,
        length: section.length.checked_sub(28)?,
    })
}

/// Whether a stream is a JPEG a viewer could show: a real frame
/// header, not a lossless or hierarchical one. Walks marker segments
/// only, and stops at the first frame or at anything malformed.
fn is_displayable_jpeg(stream: &[u8]) -> bool {
    if !stream.starts_with(&[0xFF, 0xD8]) {
        return false;
    }
    let mut at = 2;
    while at + 4 <= stream.len() {
        if stream[at] != 0xFF {
            return false;
        }
        match stream[at + 1] {
            0xFF => {
                at += 1;
                continue;
            }
            0x01 | 0xD0..=0xD8 => {
                at += 2;
                continue;
            }
            0xC0 | 0xC1 | 0xC2 | 0xC9 | 0xCA => return true,
            0xC3 | 0xC5..=0xC7 | 0xCB | 0xCD..=0xCF => return false,
            0xDA | 0xD9 => return false,
            _ => {}
        }
        let length = u16::from_be_bytes([stream[at + 2], stream[at + 3]]) as usize;
        if length < 2 {
            return false;
        }
        at += 2 + length;
    }
    false
}

/// The largest showable JPEG among the file's image sections.
fn largest_jpeg(bytes: &[u8], sections: &[Section]) -> Option<Vec<u8>> {
    sections
        .iter()
        .filter(|s| s.kind == *b"IMA2" || s.kind == *b"IMAG")
        .filter_map(|s| image(bytes, s))
        .filter_map(|i| bytes.get(i.data..i.data + i.length))
        .filter(|stream| is_displayable_jpeg(stream))
        .max_by_key(|stream| stream.len())
        .map(<[u8]>::to_vec)
}

/// The PROP section: a header of six `u32`s (magic, version, entry
/// count, character format, reserved, total length), then a pair of
/// `u32` offsets an entry, then a pool of NUL-terminated UTF-16
/// strings the offsets index **in characters**.
fn properties(bytes: &[u8], section: &Section) -> Vec<(String, String)> {
    if bytes.get(section.offset..section.offset + 4) != Some(b"SECp") {
        return Vec::new();
    }
    let count = u32_at(bytes, section.offset + 8).unwrap_or(0) as usize;
    // Eight bytes an entry, and the pool has to hold something.
    let count = count.min(section.length / 8);
    let pool = section.offset + 24 + count * 8;
    let string = |chars: usize| -> Option<String> {
        let mut at = pool.checked_add(chars.checked_mul(2)?)?;
        let mut out = String::new();
        loop {
            let unit = u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?);
            if unit == 0 {
                return Some(out);
            }
            // The camera writes plain ASCII in UTF-16 units; anything
            // outside the basic plane would be a surrogate, and there
            // is no camera that writes one.
            out.push(char::from_u32(unit as u32)?);
            at += 2;
            if out.len() > 512 {
                return Some(out);
            }
        }
    };
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let at = section.offset + 24 + i * 8;
        let (Some(name), Some(value)) = (u32_at(bytes, at), u32_at(bytes, at + 4)) else {
            break;
        };
        if let (Some(name), Some(value)) = (string(name as usize), string(value as usize)) {
            if !name.is_empty() {
                out.push((name, value));
            }
        }
    }
    out
}

/// The Exif block inside a preview JPEG, which is where the Quattro
/// bodies put what earlier ones put in PROP.
fn preview_exif(jpeg: &[u8]) -> Option<Tiff<'_>> {
    let mut at = 2;
    while at + 4 <= jpeg.len() {
        if jpeg[at] != 0xFF {
            return None;
        }
        let marker = jpeg[at + 1];
        if marker == 0xFF {
            at += 1;
            continue;
        }
        if matches!(marker, 0xD8..=0xDA) {
            return None;
        }
        let length = u16::from_be_bytes([jpeg[at + 2], jpeg[at + 3]]) as usize;
        if length < 2 {
            return None;
        }
        if marker == 0xE1 && jpeg.get(at + 4..at + 10) == Some(b"Exif\0\0") {
            // The TIFF header sits at the end of the marker's own
            // identifier, and every offset in it is relative to there.
            return Tiff::parse_embedded(jpeg, at + 10).ok();
        }
        at += 2 + length;
    }
    None
}

/// A property list, with the lookups the rest of the module wants.
struct Properties(Vec<(String, String)>);

impl Properties {
    fn get(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.trim())
            .filter(|value| !value.is_empty())
    }
    fn number(&self, key: &str) -> Option<f32> {
        self.get(key).and_then(|v| v.parse().ok())
    }
}

// --- The TRUE / TRUE-II / Quattro entropy engine ------------------
//
// Both generations share one engine and differ only in plane
// geometry. A layer is a category-Huffman stream: each Huffman symbol
// `k` is a magnitude-bit count, and `k` bits then give a signed
// difference (JPEG's DC rule: a leading 1 is the positive half, a
// leading 0 the negative). Differences run through a two-parity
// horizontal predictor, so each pixel predicts from the pixel two
// columns to its left; the two leftmost columns of a row predict from
// the two leftmost columns of the row two above (same parities).

/// A little-endian reader that never runs past the buffer.
fn take_u16(bytes: &[u8], off: &mut usize) -> Result<u16> {
    let b = bytes
        .get(*off..*off + 2)
        .ok_or_else(|| Error::Corrupt("X3F TRUE header truncated".into()))?;
    *off += 2;
    Ok(u16::from_le_bytes([b[0], b[1]]))
}

fn take_u32(bytes: &[u8], off: &mut usize) -> Result<u32> {
    let b = bytes
        .get(*off..*off + 4)
        .ok_or_else(|| Error::Corrupt("X3F TRUE header truncated".into()))?;
    *off += 4;
    Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// A canonical "category" Huffman table, built from the file's byte
/// pairs. The symbol value (a magnitude-bit count) is the pair's index
/// in the table; the code is the top `L` bits of the code byte, MSB
/// first. Decoding is a single flat lookup on the longest code length.
struct Huff {
    max_len: u32,
    /// `1 << max_len` entries of `(symbol, code_length)`; a length of
    /// zero marks an index no code reaches (an invalid bit pattern).
    table: Vec<(u8, u8)>,
}

impl Huff {
    /// `pairs[k] = (code length L, code byte C)`; `L == 0` never
    /// appears here (the terminator is dropped by the caller).
    fn build(pairs: &[(u8, u8)]) -> Result<Huff> {
        let max_len = pairs.iter().map(|&(l, _)| l as u32).max().unwrap_or(0);
        // A code is stored left-justified in one byte, so no length
        // above eight can exist in this table whatever the coding
        // could express in principle; a symbol is a magnitude bit
        // count, so no sensor needs more than seventeen of them
        // (0..=16). Both bounds keep the shifts below from underflowing
        // and the bit reader's 32-bit reads in range.
        if max_len == 0 || max_len > 8 || pairs.iter().any(|&(l, _)| l == 0) {
            return Err(Error::Corrupt(format!(
                "X3F Huffman code length {max_len} out of range"
            )));
        }
        if pairs.len() > 17 {
            return Err(Error::Corrupt(format!(
                "X3F Huffman table of {} symbols; a difference has at most 16 bits",
                pairs.len()
            )));
        }
        let mut table = vec![(0u8, 0u8); 1usize << max_len];
        for (symbol, &(l, c)) in pairs.iter().enumerate() {
            let l = l as u32;
            // The L-bit code, right-justified: the code byte holds it
            // left-justified in eight bits.
            let code = (c >> (8 - l)) as usize;
            let shift = max_len - l;
            let base = code << shift;
            for slot in table.iter_mut().skip(base).take(1usize << shift) {
                if slot.1 != 0 {
                    return Err(Error::Corrupt(
                        "X3F Huffman codes are not prefix-free".into(),
                    ));
                }
                *slot = (symbol as u8, l as u8);
            }
        }
        Ok(Huff { max_len, table })
    }

    /// The next symbol, or `None` on a bit pattern no code reaches.
    #[inline]
    fn decode(&self, pump: &mut BitPumpMsb) -> Option<u32> {
        let (symbol, len) = self.table[pump.peek(self.max_len) as usize];
        if len == 0 {
            return None;
        }
        pump.consume(len as u32);
        Some(symbol as u32)
    }
}

/// One signed difference: a symbol `k`, then `k` magnitude bits under
/// the leading-bit sign rule.
#[inline]
fn difference(pump: &mut BitPumpMsb, k: u32) -> i32 {
    // The table builder bounds symbols at 16; the clamp makes that
    // structural rather than a distant invariant.
    let k = k.min(16);
    if k == 0 {
        return 0;
    }
    let v = pump.get(k) as i32;
    if (v >> (k - 1)) & 1 == 1 {
        v
    } else {
        v - ((1i32 << k) - 1)
    }
}

/// Decode one layer's bitstream into its own `cols x ROWS` grid.
///
/// `slice` is the layer's bytes from its 16-byte-aligned start; the bit
/// reader zero-fills past the end, so a stream that is exactly long
/// enough decodes cleanly and a truncated one simply fails to find a
/// code (returning `Err`) rather than reading out of bounds.
fn decode_layer(
    slice: &[u8],
    huff: &Huff,
    seed: i32,
    cols: usize,
    rows: usize,
) -> Result<Vec<u16>> {
    let n = frame_samples(cols, rows, 1)?;
    let mut out = vec![0u16; n];
    let mut pump = BitPumpMsb::new(slice);
    // The 2x2 of left-edge seeds, one per (row parity, column parity),
    // carries the two leftmost pixels down to the row two below.
    let mut left = [[seed; 2]; 2];
    for r in 0..rows {
        // Two running predictors, one for even columns and one for odd.
        let mut acc = [0i32; 2];
        let row = &mut out[r * cols..(r + 1) * cols];
        for (c, cell) in row.iter_mut().enumerate() {
            let k = huff
                .decode(&mut pump)
                .ok_or_else(|| Error::Corrupt("X3F TRUE bitstream: no code matched".into()))?;
            let diff = difference(&mut pump, k);
            let predictor = if c < 2 {
                left[r & 1][c & 1]
            } else {
                acc[c & 1]
            };
            let value = predictor.wrapping_add(diff);
            acc[c & 1] = value;
            if c < 2 {
                left[r & 1][c & 1] = value;
            }
            // Values stay in the 14-bit sensor range by construction;
            // `as u16` merely truncates a hostile stream's overflow
            // rather than panicking.
            *cell = value as u16;
        }
    }
    Ok(out)
}

/// The parsed TRUE section header and the layer bitstream slices.
struct TrueSection<'a> {
    seeds: [i32; 3],
    huff: Huff,
    /// Each layer's own decode grid (columns, rows).
    grids: [(usize, usize); 3],
    /// The three layer bitstreams, each from its 16-byte-aligned start.
    streams: [&'a [u8]; 3],
    /// The declared compressed byte size of each layer (kept for tests
    /// and diagnostics; the streams above are clamped to the section).
    #[cfg_attr(not(test), allow(dead_code))]
    sizes: [usize; 3],
    /// True for the genuine Quattro geometry (layers 0/1 quarter-res);
    /// false for Merrill and for a flat-layout Quattro (all full-res).
    quattro: bool,
}

/// Parse everything between the 28-byte image header and the layer
/// bitstreams: the optional Quattro plane table, the seeds, the
/// per-file Huffman table, the plane byte sizes, and the 16-byte-padded
/// layer addresses.
fn parse_true<'a>(bytes: &'a [u8], raw: &Image, quattro_format: bool) -> Result<TrueSection<'a>> {
    let cols = raw.columns;
    let rows = raw.rows;
    // Bound the header parse to the section's own bytes.
    let section_end = raw
        .data
        .checked_add(raw.length)
        .filter(|&e| e <= bytes.len())
        .unwrap_or(bytes.len());
    let mut off = raw.data;

    // 2a: the Quattro plane table (skipped on Merrill).
    let mut grids = [(cols, rows); 3];
    let quattro = if quattro_format {
        for grid in &mut grids {
            let c = take_u16(bytes, &mut off)? as usize;
            let r = take_u16(bytes, &mut off)? as usize;
            *grid = (c, r);
        }
        // Layer 0 half-height is the true Quattro layout; full-height
        // is a flat file whose engine is exactly Merrill's. Either
        // way the table must describe *this* frame: the two lower
        // layers alike, the top layer the frame's height and its
        // width plus at most a small overscan. A table that says
        // otherwise is a forgery sizing an allocation, not a camera.
        let (c0, r0) = grids[0];
        let quattro = if r0 == rows / 2 && c0 == cols / 2 {
            true
        } else if (c0, r0) == (cols, rows) {
            false
        } else {
            return Err(Error::Corrupt(format!(
                "X3F Quattro plane table: layer 0 is {c0}x{r0} for a {cols}x{rows} frame"
            )));
        };
        // The top layer's overscan is a few hundred columns on the
        // Quattro bodies (6272 for a 5888-wide frame); a quarter of
        // the width bounds it without excluding a real back.
        if grids[1] != grids[0]
            || grids[2].1 != rows
            || grids[2].0 < cols
            || grids[2].0 > cols + cols / 4
        {
            return Err(Error::Corrupt(format!(
                "X3F Quattro plane table {grids:?} does not describe a {cols}x{rows} frame"
            )));
        }
        quattro
    } else {
        false
    };

    // 2b: seeds (three used, one reserved word).
    let seeds = [
        take_u16(bytes, &mut off)? as i32,
        take_u16(bytes, &mut off)? as i32,
        take_u16(bytes, &mut off)? as i32,
    ];
    let _reserved = take_u16(bytes, &mut off)?;

    // 2c: the byte-pair Huffman table, read to the zero-length pair.
    let mut pairs: Vec<(u8, u8)> = Vec::new();
    loop {
        let l = *bytes
            .get(off)
            .ok_or_else(|| Error::Corrupt("X3F Huffman table truncated".into()))?;
        let c = *bytes
            .get(off + 1)
            .ok_or_else(|| Error::Corrupt("X3F Huffman table truncated".into()))?;
        off += 2;
        if l == 0 {
            break;
        }
        pairs.push((l, c));
        // A Foveon table is a dozen-odd codes; a runaway means a lost
        // terminator in a corrupt file.
        if pairs.len() > 256 {
            return Err(Error::Corrupt("X3F Huffman table has no terminator".into()));
        }
    }
    let huff = Huff::build(&pairs)?;

    // 2d: a reserved word for Quattro/SDQ only.
    if quattro_format {
        let _unknown = take_u32(bytes, &mut off)?;
    }

    // 2e: the three compressed layer byte sizes.
    let sizes = [
        take_u32(bytes, &mut off)? as usize,
        take_u32(bytes, &mut off)? as usize,
        take_u32(bytes, &mut off)? as usize,
    ];

    // 2f: layer starts, each rounded up to a 16-byte boundary. The
    // slices are clamped to the section so a lying size cannot reach
    // past the file; the bit reader zero-fills any shortfall. Sizes
    // that add up to more than the section is a lie outright.
    let round16 = |n: usize| n.div_ceil(16) * 16;
    // The last layer need not be padded, so the bound is the plain
    // sum, not the rounded one.
    let declared = sizes
        .iter()
        .try_fold(0usize, |acc, n| acc.checked_add(*n))
        .ok_or_else(|| Error::Corrupt("X3F layer sizes overflow".into()))?;
    if declared > section_end.saturating_sub(off) {
        return Err(Error::Corrupt(format!(
            "X3F layers declare {declared} bytes in a {}-byte section",
            section_end.saturating_sub(off)
        )));
    }
    let mut start = off;
    let mut streams: [&[u8]; 3] = [&[]; 3];
    for i in 0..3 {
        let end = start.saturating_add(sizes[i]).min(section_end);
        streams[i] = bytes.get(start..end).unwrap_or(&[]);
        start = start.saturating_add(round16(sizes[i]));
    }

    Ok(TrueSection {
        seeds,
        huff,
        grids,
        streams,
        sizes,
        quattro,
    })
}

/// Decode a TRUE section to the interleaved three-plane frame the rest
/// of the crate expects: `(y * cols + x) * 3 + channel`, channel =
/// layer = {0 bottom, 1 middle, 2 top}.
fn decode_true(bytes: &[u8], raw: &Image, quattro_format: bool) -> Result<Vec<u16>> {
    let (cols, rows) = (raw.columns, raw.rows);
    let section = parse_true(bytes, raw, quattro_format)?;

    let n = frame_samples(cols, rows, 3)?;
    let mut data = vec![0u16; n];

    // Decode the three layers independently, each on its own grid.
    // They share nothing but the (read-only) Huffman table, so the
    // three bitstreams decode in parallel.
    let huff = &section.huff;
    let planes: Vec<Vec<u16>> = (0..3)
        .into_par_iter()
        .map(|i| {
            let (gc, gr) = section.grids[i];
            decode_layer(section.streams[i], huff, section.seeds[i], gc, gr)
        })
        .collect::<Result<Vec<_>>>()?;

    if section.quattro {
        // Top layer: full resolution, its overscan columns (beyond
        // `cols`) discarded. A well-formed file has `top_cols >= cols`
        // and `top_rows == rows`; the `min`s keep a malformed one from
        // reading past the decoded grid.
        let (top_cols, top_rows) = section.grids[2];
        let top = &planes[2];
        for r in 0..rows.min(top_rows) {
            let src = &top[r * top_cols..(r + 1) * top_cols];
            for c in 0..cols.min(top_cols) {
                data[(r * cols + c) * 3 + 2] = src[c];
            }
        }
        // Bottom and middle layers: quarter resolution, placed on the
        // even/even lattice with zeros left elsewhere (the oracle's
        // shape; the odd positions are LibRaw's interpolation domain,
        // which is out of scope).
        for (channel, plane) in [(0usize, &planes[0]), (1usize, &planes[1])] {
            let (gc, gr) = section.grids[channel];
            for pr in 0..gr {
                let fr = 2 * pr;
                if fr >= rows {
                    break;
                }
                for pc in 0..gc {
                    let fc = 2 * pc;
                    if fc >= cols {
                        break;
                    }
                    data[(fr * cols + fc) * 3 + channel] = plane[pr * gc + pc];
                }
            }
        }
    } else {
        // Merrill (and flat-layout Quattro): three full-resolution
        // planes, one per channel, mapped one to one.
        for (channel, plane) in planes.iter().enumerate() {
            let (gc, gr) = section.grids[channel];
            if gc != cols || gr != rows {
                return Err(Error::Corrupt(format!(
                    "X3F layer {channel} is {gc}x{gr} for a {cols}x{rows} frame"
                )));
            }
            for r in 0..rows {
                for c in 0..cols {
                    data[(r * cols + c) * 3 + channel] = plane[r * cols + c];
                }
            }
        }
    }

    Ok(data)
}

// --- The SD9 / SD10 / SD14 Huffman code (format 6) ----------------
//
// The first Foveon bodies coded the sensor differently from TRUE. The
// section carries two tables of its own: a *value-mapping* table of
// 1024 signed 16-bit reconstruction levels (Sigma quantised the true
// differences onto these before coding, so the decoder simply adds
// the level) and a *coding* table of 1024 packed words, one per slot,
// giving that slot's prefix code. Then the pixel data, then — at the
// very end of the section, with no count — one `u32` per row giving
// the row's byte offset from the first pixel-data byte.
//
// A row is one bitstream read from a fresh reader: for every column,
// colour 0, 1, 2 in turn, each a code whose level is added to that
// colour's own running accumulator. All three accumulators restart at
// the frame-wide offset on every row, so there is no vertical
// prediction and no column-parity split. The offset is the reference
// decoder's "auto legacy offset": decode with the accumulators at 0,
// find the smallest value anywhere in the frame, and if it is
// negative lift every sample by that much (otherwise clamp at 0).
// Done in one pass over a signed buffer here; the two-pass form of the
// reference gives the same samples.

/// The table depth of format 6: `2^10` slots, the code's nominal
/// sample depth. Nothing in the file states it.
const SD_SLOTS: usize = 1024;
/// A code word holds its length in five bits and the code in the
/// remaining twenty-seven, so no code can be longer than that.
const SD_MAX_CODE_LEN: u32 = 27;
/// Codes up to this long resolve with one table lookup; longer ones
/// (the rare large differences) walk the tree from where the lookup
/// left off.
const SD_LOOKUP_BITS: u32 = 12;
/// A tree child or lookup entry that is a leaf carries this flag and
/// the 16-bit level in its low half.
const SD_LEAF: u32 = 1 << 31;

/// The per-file prefix code of an SD section, resolved to the level
/// each code denotes.
struct SdCode {
    /// A binary tree: `nodes[i][bit]` is 0 where no code reaches, a
    /// leaf (`SD_LEAF | level`), or the index of the next node. Node
    /// 0 is the root.
    nodes: Vec<[u32; 2]>,
    /// One entry per `SD_LOOKUP_BITS`-bit prefix: 0 where no code
    /// reaches, `SD_LEAF | len << 16 | level` for a code that ends
    /// within the prefix, else the node to continue from.
    lookup: Vec<u32>,
}

impl SdCode {
    /// Build from the file's coding words and the levels they map to.
    /// `mapping[i]` is the signed level slot `i` denotes, as its
    /// 16-bit pattern.
    fn build(coding: &[u32], mapping: &[u16]) -> Result<SdCode> {
        let mut nodes: Vec<[u32; 2]> = vec![[0; 2]];
        let mut populated = 0usize;
        for (slot, (&word, &level)) in coding.iter().zip(mapping).enumerate() {
            if word == 0 {
                // An unused slot: its symbol never occurs.
                continue;
            }
            let len = word >> 27;
            if len == 0 || len > SD_MAX_CODE_LEN {
                return Err(Error::Corrupt(format!(
                    "X3F SD code table: slot {slot} has code length {len}"
                )));
            }
            // The code sits right-justified; only its `len` low bits
            // are the code, MSB first.
            let code = word & 0x07FF_FFFF;
            let mut node = 0usize;
            for bit_index in (0..len).rev() {
                let bit = ((code >> bit_index) & 1) as usize;
                let child = nodes[node][bit];
                if bit_index == 0 {
                    if child != 0 {
                        return Err(Error::Corrupt(format!(
                            "X3F SD code table: slot {slot}'s code is a prefix of another"
                        )));
                    }
                    nodes[node][bit] = SD_LEAF | level as u32;
                } else if child == 0 {
                    let next = nodes.len() as u32;
                    nodes.push([0; 2]);
                    nodes[node][bit] = next;
                    node = next as usize;
                } else if child & SD_LEAF != 0 {
                    return Err(Error::Corrupt(format!(
                        "X3F SD code table: another code is a prefix of slot {slot}'s"
                    )));
                } else {
                    node = child as usize;
                }
            }
            populated += 1;
        }
        if populated == 0 {
            return Err(Error::Corrupt("X3F SD code table holds no code".into()));
        }

        // The first-level lookup: walk every prefix down the tree.
        let mut lookup = vec![0u32; 1 << SD_LOOKUP_BITS];
        for (prefix, entry) in lookup.iter_mut().enumerate() {
            let mut node = 0usize;
            for depth in 0..SD_LOOKUP_BITS {
                let bit = (prefix >> (SD_LOOKUP_BITS - 1 - depth)) & 1;
                let child = nodes[node][bit];
                if child == 0 {
                    break;
                }
                if child & SD_LEAF != 0 {
                    *entry = SD_LEAF | ((depth + 1) << 16) | (child & 0xFFFF);
                    break;
                }
                node = child as usize;
                if depth + 1 == SD_LOOKUP_BITS {
                    *entry = node as u32;
                }
            }
        }
        Ok(SdCode { nodes, lookup })
    }

    /// The next level, or `None` on a bit pattern no code reaches.
    #[inline]
    fn decode(&self, pump: &mut BitPumpMsb) -> Option<i16> {
        let entry = self.lookup[pump.peek(SD_LOOKUP_BITS) as usize];
        if entry == 0 {
            return None;
        }
        if entry & SD_LEAF != 0 {
            pump.consume((entry >> 16) & 0x1F);
            return Some((entry & 0xFFFF) as u16 as i16);
        }
        pump.consume(SD_LOOKUP_BITS);
        let mut node = entry as usize;
        // Every child index was pushed by `build`, so the walk stays
        // inside `nodes` and ends within the tree's depth.
        loop {
            let child = self.nodes[node][pump.get(1) as usize];
            if child == 0 {
                return None;
            }
            if child & SD_LEAF != 0 {
                return Some((child & 0xFFFF) as u16 as i16);
            }
            node = child as usize;
        }
    }
}

/// An SD section's tables and where its rows are.
struct SdSection<'a> {
    code: SdCode,
    /// The entropy-coded pixel data of every row, in one run.
    pixels: &'a [u8],
    /// Each row's byte offset into `pixels`.
    row_offsets: Vec<usize>,
}

/// Parse the two tables and the row-offset table around the pixel
/// data; everything is validated against the section's own length.
fn parse_sd<'a>(bytes: &'a [u8], raw: &Image) -> Result<SdSection<'a>> {
    let (cols, rows) = (raw.columns, raw.rows);
    if cols == 0 || rows == 0 {
        return Err(Error::Corrupt(format!("X3F SD frame of {cols}x{rows}")));
    }
    // The frame is bounded before the row table sizes anything.
    frame_samples(cols, rows, 3)?;
    let section = raw
        .data
        .checked_add(raw.length)
        .and_then(|end| bytes.get(raw.data..end))
        .ok_or_else(|| Error::Corrupt("X3F SD section runs past the file".into()))?;
    let tables = SD_SLOTS * 2 + SD_SLOTS * 4;
    let pixel_len = rows
        .checked_mul(4)
        .and_then(|row_table| section.len().checked_sub(tables + row_table))
        .ok_or_else(|| {
            Error::Corrupt(format!(
                "X3F SD section of {} bytes cannot hold its tables and {rows} row offsets",
                section.len()
            ))
        })?;
    let mapping: Vec<u16> = section[..SD_SLOTS * 2]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| u16::from_le_bytes(*b))
        .collect();
    let coding: Vec<u32> = section[SD_SLOTS * 2..tables]
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| u32::from_le_bytes(*b))
        .collect();
    let code = SdCode::build(&coding, &mapping)?;
    let pixels = &section[tables..tables + pixel_len];
    // The row offsets, relative to the first pixel-data byte. Every
    // camera starts at zero and writes strictly increasing,
    // four-byte-aligned offsets. Besides rejecting a malformed table,
    // enforcing that structure prevents a tiny pixel run from being
    // aliased as thousands of rows and expanded into a multi-gigabyte
    // frame.
    let row_offsets = section[tables + pixel_len..]
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| u32::from_le_bytes(*b) as usize)
        .enumerate()
        .map(|(r, off)| {
            if off >= pixel_len {
                Err(Error::Corrupt(format!(
                    "X3F SD row {r} starts at byte {off} of {pixel_len} of pixel data"
                )))
            } else {
                Ok(off)
            }
        })
        .collect::<Result<Vec<_>>>()?;
    if row_offsets.first() != Some(&0) {
        return Err(Error::Corrupt(
            "X3F SD first row does not start at byte zero".into(),
        ));
    }
    for (row, &offset) in row_offsets.iter().enumerate() {
        if offset % 4 != 0 {
            return Err(Error::Corrupt(format!(
                "X3F SD row {row} starts at unaligned byte {offset}"
            )));
        }
    }
    for (row, offsets) in row_offsets.windows(2).enumerate() {
        if offsets[0] >= offsets[1] {
            return Err(Error::Corrupt(format!(
                "X3F SD row offsets do not increase at rows {row} and {}",
                row + 1
            )));
        }
    }
    Ok(SdSection {
        code,
        pixels,
        row_offsets,
    })
}

/// Decode one row from a fresh reader over `slice`: the raw
/// accumulators (offset 0) interleaved `c0 c1 c2` a pixel, the
/// smallest of them, and the bits consumed.
///
/// The reference keeps each accumulator as a signed 16-bit quantity,
/// so the arithmetic is 16-bit wrapping: real frames stay far from the
/// edge and a hostile one merely wraps rather than panicking.
fn decode_sd_row(code: &SdCode, slice: &[u8], cols: usize) -> Result<(Vec<i16>, i16, usize)> {
    let mut pump = BitPumpMsb::new(slice);
    let mut acc = [0i16; 3];
    let mut min = i16::MAX;
    let mut out = vec![0i16; cols * 3];
    for pixel in out.as_chunks_mut::<3>().0 {
        for (colour, cell) in pixel.iter_mut().enumerate() {
            let level = code
                .decode(&mut pump)
                .ok_or_else(|| Error::Corrupt("X3F SD bitstream: no code matched".into()))?;
            acc[colour] = acc[colour].wrapping_add(level);
            *cell = acc[colour];
            min = min.min(acc[colour]);
        }
    }
    // The reader zero-fills past the end, and a run of zero bits may
    // well be a valid code (the SD9's "0" level is `0000`), so a short
    // row is caught by its bit count rather than by a failed match.
    let bits = pump.position();
    if bits > slice.len() * 8 {
        return Err(Error::Corrupt(format!(
            "X3F SD row needs {bits} bits of the {} available",
            slice.len() * 8
        )));
    }
    Ok((out, min, bits))
}

/// Decode an SD section to the interleaved three-plane frame.
fn decode_sd(bytes: &[u8], raw: &Image) -> Result<Vec<u16>> {
    let (cols, rows) = (raw.columns, raw.rows);
    let n = frame_samples(cols, rows, 3)?;
    let section = parse_sd(bytes, raw)?;
    let code = &section.code;
    // Rows share nothing but the tables: each has its own reader and
    // restarts its accumulators, so they decode in parallel. Bound each
    // reader by the next row offset; the final row ends with the pixel
    // run. This makes the table a partition of the compressed data,
    // rather than a way to decode the same bytes repeatedly.
    let decoded: Vec<(Vec<i16>, i16)> = section
        .row_offsets
        .par_iter()
        .enumerate()
        .map(|(r, &start)| {
            let end = section
                .row_offsets
                .get(r + 1)
                .copied()
                .unwrap_or(section.pixels.len());
            let (row, min, _) = decode_sd_row(code, &section.pixels[start..end], cols)?;
            Ok((row, min))
        })
        .collect::<Result<Vec<_>>>()?;
    let min = decoded.iter().map(|(_, m)| *m).min().unwrap_or(0);
    let mut data = Vec::with_capacity(n);
    for (row, _) in &decoded {
        data.extend(row.iter().map(|&v| {
            if min < 0 {
                // Lift the whole frame so its minimum lands on 0: `v`
                // is at least `min`, so this fits sixteen bits.
                (v as i32 - min as i32) as u16
            } else {
                v.max(0) as u16
            }
        }));
    }
    Ok(data)
}

pub fn decode(bytes: &[u8]) -> Result<RawImage> {
    let sections = directory(bytes)?;
    let raw = sections
        .iter()
        .filter(|s| s.kind == *b"IMA2" || s.kind == *b"IMAG")
        .filter_map(|s| image(bytes, s))
        // Type 1 (Merrill and later) or 3 (every earlier body) is the
        // sensor's own data; type 2 is a picture the camera made from
        // it. The tag is no guide: the SD9/SD10 file its sensor data
        // under `IMAG`, first in the directory.
        .find(|i| i.kind == 1 || i.kind == 3)
        .ok_or_else(|| Error::Unsupported("X3F with no sensor image section".into()))?;

    let samples = match raw.format {
        formats::PLAIN => {
            let count = raw
                .columns
                .checked_mul(raw.rows)
                .and_then(|n| n.checked_mul(3))
                .ok_or_else(|| Error::Corrupt("X3F frame too large".into()))?;
            let end = count
                .checked_mul(2)
                .and_then(|n| raw.data.checked_add(n))
                .ok_or_else(|| Error::Corrupt("X3F frame too large".into()))?;
            let data = bytes
                .get(raw.data..end)
                .ok_or_else(|| Error::Corrupt("X3F plain frame runs past the file".into()))?;
            data.as_chunks::<2>()
                .0
                .iter()
                .map(|b| u16::from_le_bytes(*b))
                .collect()
        }
        formats::TRUE => decode_true(bytes, &raw, false)?,
        formats::QUATTRO | formats::SDQ_37 | formats::SDQ_39 | formats::SDQ_41 => {
            decode_true(bytes, &raw, true)?
        }
        formats::SD_HUFFMAN if raw.row_size != 0 => {
            // A non-zero row stride means the packed uncompressed
            // form: a `u32` a pixel holding three 10-bit table indices.
            // No camera in the corpus writes it.
            return Err(Error::Unsupported(format!(
                "Foveon SD uncompressed packed sensor data (X3F image format 6, row stride {}); \
                 the preview and the metadata are still readable",
                raw.row_size
            )));
        }
        formats::SD_HUFFMAN => decode_sd(bytes, &raw)?,
        formats::X530 => {
            return Err(Error::Unsupported(
                "Polaroid x530 Foveon sensor data (X3F image format 5); the preview and the \
                 metadata are still readable"
                    .into(),
            ));
        }
        formats::SD_THUMBNAIL => {
            return Err(Error::Unsupported(
                "Foveon 8-bit Huffman thumbnail as sensor data (X3F image format 11); the \
                 preview and the metadata are still readable"
                    .into(),
            ));
        }
        other => return Err(Error::Unsupported(format!("X3F image format {other}"))),
    };

    let mut image = RawImage::new(
        Format::X3f,
        raw.columns,
        raw.rows,
        3,
        RawData::U16(samples),
        // Three stacked photodiodes: every pixel already has all
        // three colours, so there is no filter array to describe.
        Cfa::None,
    );
    describe(bytes, &sections, &mut image);
    // Levels and the active area: the file states neither in a place
    // this module reads, and the camera table carries no Foveon
    // entries, so the frame would otherwise develop against a 65535
    // white. The crop is applied only where it fits the frame.
    let (black, white, crop) = body_levels(&image.clean_model, raw.format, raw.columns, raw.rows);
    if image.white_level == 65535.0 {
        image.black_levels = [black; 4];
        image.white_level = white;
    }
    if crop.x + crop.width <= image.width && crop.y + crop.height <= image.height {
        image.crop = crop;
    }
    image.apply_camera_table();
    Ok(image)
}

/// The black, white and active area of a Foveon frame, by body
/// (`clean_model`), code and frame size. Sigma writes none of them in
/// a place this module reads; these are the reference decoder's
/// per-body values, its saturation estimates for white included.
///
/// Every generation is here: the Merrill's 4928x3264 and the Quattro's
/// 5888x3672 as before; the SD9/SD10's 2304x1531; and the 2688x1792
/// shared by the SD14 (format 6) and the TRUE-I bodies, which crop
/// alike (the DP2 alone at (13,16)) but sit at different levels. The
/// half-size raw modes of the older bodies have no sample; an unknown
/// size keeps its generation's crop only where it fits.
fn body_levels(clean_model: &str, format: u32, width: usize, height: usize) -> (f32, f32, Rect) {
    let model = clean_model.to_ascii_uppercase();
    let rect = |x, y, width, height| Rect {
        x,
        y,
        width,
        height,
    };
    match (width, height) {
        // SD9 and SD10 alike.
        (2304, 1531) => (15.0, 12000.0, rect(20, 8, 2266, 1510)),
        (2688, 1792) => {
            let crop = if model == "DP2" {
                rect(13, 16, 2651, 1767)
            } else {
                rect(18, 12, 2651, 1767)
            };
            let (black, white) = match model.as_str() {
                "SD14" => (15.0, 14000.0),
                "SD15" => (15.0, 2900.0),
                "DP1" => (0.0, 2100.0),
                "DP1S" => (0.0, 2200.0),
                "DP1X" => (0.0, 3560.0),
                "DP2" => (0.0, 2326.0),
                "DP2S" | "DP2X" => (0.0, 2300.0),
                // An unlisted body: the SD14's levels for its code, the
                // DP2s's for TRUE-I.
                _ if format == formats::SD_HUFFMAN => (15.0, 14000.0),
                _ => (0.0, 2300.0),
            };
            (black, white, crop)
        }
        _ if format == formats::TRUE => (31.0, 3900.0, rect(12, 0, 4807, 3205)),
        _ => (2047.0, 16383.0, rect(204, 24, 5446, 3624)),
    }
}

/// Everything about a frame that is not its samples: the camera, the
/// exposure, the orientation and the preview. Shared by [`decode`]
/// and used on its own for the frames this module cannot unpack.
fn describe(bytes: &[u8], sections: &[Section], image: &mut RawImage) {
    let properties = Properties(
        sections
            .iter()
            .find(|s| s.kind == *b"PROP")
            .map(|s| properties(bytes, s))
            .unwrap_or_default(),
    );
    let preview = largest_jpeg(bytes, sections);
    // The Quattro bodies dropped PROP and put an ordinary Exif block
    // in the preview instead.
    let exif = preview.as_deref().and_then(preview_exif);

    let (mut make, mut model) = (
        properties.get("CAMMANUF").unwrap_or("").to_string(),
        properties.get("CAMMODEL").unwrap_or("").to_string(),
    );
    if let Some(tiff) = &exif {
        let (exif_make, exif_model) = tiff.make_model();
        if make.is_empty() {
            make = exif_make;
        }
        if model.is_empty() {
            model = exif_model;
        }
    }
    image.set_camera(&make, &model);

    image.metadata = Metadata {
        iso: properties.number("ISO"),
        // SHUTTER is seconds; EXPTIME is the same in microseconds.
        exposure_time: properties
            .number("SHUTTER")
            .or_else(|| properties.number("EXPTIME").map(|t| t / 1e6)),
        f_number: properties.number("APERTURE"),
        focal_length: properties.number("FLENGTH"),
        lens: properties.get("LENSMODEL").map(str::to_string),
        date_time: None,
    };
    if let Some(tiff) = &exif {
        // Exif fills whatever the properties did not, and is the only
        // source of a formatted timestamp: PROP's TIME is a Unix
        // second count, which this crate has no clock to format.
        let from_exif = crate::formats::common::metadata(tiff);
        let take = |a: &mut Option<f32>, b: Option<f32>| {
            if a.is_none() {
                *a = b;
            }
        };
        take(&mut image.metadata.iso, from_exif.iso);
        take(&mut image.metadata.exposure_time, from_exif.exposure_time);
        take(&mut image.metadata.f_number, from_exif.f_number);
        take(&mut image.metadata.focal_length, from_exif.focal_length);
        if image.metadata.lens.is_none() {
            image.metadata.lens = from_exif.lens;
        }
        image.metadata.date_time = from_exif.date_time;
    }

    // ROTATION is degrees clockwise, the way the camera was held.
    image.orientation = match properties.number("ROTATION").map(|r| r as i32) {
        Some(90) => Orientation::Rotate90CW,
        Some(180) => Orientation::Rotate180,
        Some(270) => Orientation::Rotate270CW,
        Some(_) => Orientation::Normal,
        None => exif
            .as_ref()
            .map(crate::formats::common::orientation)
            .unwrap_or_default(),
    };
    // The white balance a Foveon needs lives in the obfuscated CAMF
    // block; PROP only names the mode ("Auto", "Daylight"). Unit
    // multipliers are the honest answer, and the developer's own
    // white balance is what a Foveon frame gets in practice.
    drop(exif);
    image.preview = preview;
}

pub fn preview(bytes: &[u8]) -> Result<Option<Vec<u8>>> {
    Ok(largest_jpeg(bytes, &directory(bytes)?))
}

/// The camera and exposure of an X3F whose sensor data this module
/// cannot unpack — everything [`decode`] would have filled in.
///
/// A gallery that has a preview and a name for a file is not stuck
/// just because the Foveon entropy code is unread.
pub fn metadata(bytes: &[u8]) -> Result<(String, String, Metadata, Option<Vec<u8>>)> {
    let sections = directory(bytes)?;
    let mut image = RawImage::new(Format::X3f, 1, 1, 3, RawData::U16(vec![0; 3]), Cfa::None);
    describe(bytes, &sections, &mut image);
    Ok((image.make, image.model, image.metadata, image.preview))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formats::hasselblad::corpus;

    /// A file with one image section and one property list.
    fn build(image_kind: u32, format: u32, columns: u32, rows: u32, data: &[u8]) -> Vec<u8> {
        build_with_stride(image_kind, format, columns, rows, 0, data)
    }

    /// The same, with the image header's row-stride word set.
    fn build_with_stride(
        image_kind: u32,
        format: u32,
        columns: u32,
        rows: u32,
        stride: u32,
        data: &[u8],
    ) -> Vec<u8> {
        let mut out = MAGIC.to_vec();
        out.extend_from_slice(&0x0003_0000u32.to_le_bytes());
        out.resize(104, 0);
        let image_at = out.len();
        out.extend_from_slice(b"SECi");
        for word in [0x0002_0000u32, image_kind, format, columns, rows, stride] {
            out.extend_from_slice(&word.to_le_bytes());
        }
        out.extend_from_slice(data);
        let image_len = out.len() - image_at;

        let property_at = out.len();
        let entries: [(&str, &str); 2] = [("CAMMODEL", "SIGMA DP2 Merrill"), ("ISO", "400")];
        out.extend_from_slice(b"SECp");
        out.extend_from_slice(&0x0002_0000u32.to_le_bytes());
        out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        let mut pool: Vec<u16> = Vec::new();
        let mut offsets = Vec::new();
        for (name, value) in entries {
            let name_at = pool.len();
            pool.extend(name.encode_utf16());
            pool.push(0);
            let value_at = pool.len();
            pool.extend(value.encode_utf16());
            pool.push(0);
            offsets.push((name_at as u32, value_at as u32));
        }
        for (name, value) in &offsets {
            out.extend_from_slice(&name.to_le_bytes());
            out.extend_from_slice(&value.to_le_bytes());
        }
        for unit in &pool {
            out.extend_from_slice(&unit.to_le_bytes());
        }
        let property_len = out.len() - property_at;

        let directory_at = out.len();
        out.extend_from_slice(b"SECd");
        out.extend_from_slice(&0x0002_0000u32.to_le_bytes());
        out.extend_from_slice(&2u32.to_le_bytes());
        for (offset, length, kind) in [
            (image_at, image_len, b"IMA2"),
            (property_at, property_len, b"PROP"),
        ] {
            out.extend_from_slice(&(offset as u32).to_le_bytes());
            out.extend_from_slice(&(length as u32).to_le_bytes());
            out.extend_from_slice(kind);
        }
        out.extend_from_slice(&(directory_at as u32).to_le_bytes());
        out
    }

    #[test]
    fn a_plain_frame_is_three_samples_a_pixel() {
        let pixels: Vec<u16> = (0..2 * 2 * 3).map(|i| 1000 + i as u16).collect();
        let data: Vec<u8> = pixels.iter().flat_map(|v| v.to_le_bytes()).collect();
        let file = build(1, formats::PLAIN, 2, 2, &data);
        let raw = decode(&file).unwrap();
        raw.validate().unwrap();
        assert_eq!((raw.width, raw.height, raw.cpp), (2, 2, 3));
        assert_eq!(raw.cfa, Cfa::None);
        assert_eq!(raw.data, RawData::U16(pixels));
        assert_eq!(raw.clean_model, "DP2 Merrill");
        assert_eq!(raw.metadata.iso, Some(400.0));
    }

    #[test]
    fn the_sampleless_sd_variants_are_unsupported_by_name() {
        // The x530 relabel, the 8-bit thumbnail flavour and the packed
        // uncompressed variant (row stride set) of the SD code have no
        // sample and are refused by name; format 6 itself decodes.
        for (format, stride, word) in [
            (formats::X530, 0, "x530"),
            (formats::SD_THUMBNAIL, 0, "thumbnail"),
            (formats::SD_HUFFMAN, 32, "stride"),
        ] {
            for kind in [1, 3] {
                let file = build_with_stride(kind, format, 8, 8, stride, &[0; 64]);
                match decode(&file) {
                    Err(Error::Unsupported(message)) => {
                        assert!(message.contains(word), "{message}")
                    }
                    other => panic!("format {format} type {kind}: {other:?}"),
                }
            }
        }
        // A format-6 section too short for its tables is corrupt, not
        // unsupported.
        assert!(matches!(
            decode(&build(3, formats::SD_HUFFMAN, 8, 8, &[0; 64])),
            Err(Error::Corrupt(_))
        ));
    }

    /// An SD (format 6) section body: the mapping table from
    /// `(slot, level)` pairs, the coding table from `(slot, length,
    /// code)` triples, each row's bytes padded to four, and the row
    /// offset table at the end.
    fn sd_section(
        levels: &[(usize, i16)],
        codes: &[(usize, u32, u32)],
        rows: &[Vec<u8>],
    ) -> Vec<u8> {
        let mut mapping = [0u16; SD_SLOTS];
        for &(slot, level) in levels {
            mapping[slot] = level as u16;
        }
        let mut coding = [0u32; SD_SLOTS];
        for &(slot, len, code) in codes {
            coding[slot] = (len << 27) | code;
        }
        let mut out: Vec<u8> = mapping.iter().flat_map(|v| v.to_le_bytes()).collect();
        out.extend(coding.iter().flat_map(|v| v.to_le_bytes()));
        let mut offsets = Vec::new();
        let mut pixels = Vec::new();
        for (i, row) in rows.iter().enumerate() {
            if i > 0 {
                // The cameras start every row on a four-byte boundary;
                // the last row runs to the end of the pixel data.
                pixels.resize(pixels.len().div_ceil(4) * 4, 0);
            }
            offsets.push(pixels.len() as u32);
            pixels.extend_from_slice(row);
        }
        out.extend(pixels);
        out.extend(offsets.iter().flat_map(|v| v.to_le_bytes()));
        out
    }

    /// The toy code the SD tests share: four short codes, one that
    /// ends past the 12-bit lookup, and one of the longest length.
    const SD_LEVELS: &[(usize, i16)] = &[(0, -3), (1, 0), (2, 5), (3, 100), (4, 7)];
    const SD_CODES: &[(usize, u32, u32)] = &[
        (0, 4, 0b0000),
        (1, 4, 0b0001),
        (2, 4, 0b0010),
        (3, 24, 0xFF_FFFF),
        (4, 13, 0b1_1111_1111_1110),
    ];

    #[test]
    fn sd_rows_interleave_three_accumulators_and_restart_each_row() {
        // Row 0: (+5, -3, 0), (0, +5, -3) -> acc 5,-3,0, 5,2,-3.
        let mut w = BitWriter::new();
        for code in [0b0010, 0b0000, 0b0001, 0b0001, 0b0010, 0b0000] {
            w.put(code, 4);
        }
        let row0 = w.finish();
        // Row 1: (+100 by the 24-bit code, +7 by the 13-bit one, -3),
        // (-3, 0, +5) -> acc 100,7,-3, 97,7,2. Fresh accumulators: row
        // 0's values do not carry down.
        let mut w = BitWriter::new();
        w.put(0xFF_FFFF, 24);
        w.put(0b1_1111_1111_1110, 13);
        w.put(0b0000, 4);
        w.put(0b0000, 4);
        w.put(0b0001, 4);
        w.put(0b0010, 4);
        let row1 = w.finish();
        let body = sd_section(SD_LEVELS, SD_CODES, &[row0.clone(), row1.clone()]);
        let file = build(3, formats::SD_HUFFMAN, 2, 2, &body);
        let raw = decode(&file).unwrap();
        raw.validate().unwrap();
        assert_eq!((raw.width, raw.height, raw.cpp), (2, 2, 3));
        // MIN = -3 lifts every sample by 3.
        assert_eq!(
            raw.data,
            RawData::U16(vec![8, 0, 3, 8, 5, 0, 103, 10, 0, 100, 10, 5])
        );

        // The bit counts and raw accumulators, row by row.
        let sections = directory(&file).unwrap();
        let image = image(&file, &sections[0]).unwrap();
        let section = parse_sd(&file, &image).unwrap();
        assert_eq!(section.row_offsets, vec![0, 4]);
        let (acc, min, bits) = decode_sd_row(&section.code, section.pixels, 2).unwrap();
        assert_eq!((acc, min, bits), (vec![5, -3, 0, 5, 2, -3], -3, 24));
        let (acc, min, bits) = decode_sd_row(&section.code, &section.pixels[4..], 2).unwrap();
        assert_eq!((acc, min, bits), (vec![100, 7, -3, 97, 7, 2], -3, 53));

        // A frame whose minimum is not negative is emitted as decoded.
        let mut w = BitWriter::new();
        for code in [0b0010, 0b0001, 0b0010, 0b0001, 0b0010, 0b0010] {
            w.put(code, 4);
        }
        let body = sd_section(SD_LEVELS, SD_CODES, &[w.finish()]);
        let raw = decode(&build(3, formats::SD_HUFFMAN, 2, 1, &body)).unwrap();
        assert_eq!(raw.data, RawData::U16(vec![5, 0, 5, 5, 5, 10]));
    }

    #[test]
    fn sd_code_tables_are_validated() {
        let table = |codes: &[(usize, u32, u32)]| {
            let mut coding = [0u32; SD_SLOTS];
            for &(slot, len, code) in codes {
                coding[slot] = (len << 27) | code;
            }
            coding
        };
        let mapping = [0u16; SD_SLOTS];
        // The toy table builds, and its long codes decode through the
        // tree beyond the 12-bit lookup.
        let code = SdCode::build(&table(SD_CODES), &mapping).unwrap();
        let mut w = BitWriter::new();
        w.put(0xFF_FFFF, 24);
        w.put(0b1_1111_1111_1110, 13);
        let bytes = w.finish();
        let mut pump = BitPumpMsb::new(&bytes);
        assert_eq!(code.decode(&mut pump), Some(0));
        assert_eq!(pump.position(), 24);
        assert_eq!(code.decode(&mut pump), Some(0));
        assert_eq!(pump.position(), 37);
        // A pattern no code reaches, short and long.
        let mut pump = BitPumpMsb::new(&[0b0011_0000]);
        assert_eq!(code.decode(&mut pump), None);
        // Thirteen ones then a zero: past the lookup, off the tree.
        let mut pump = BitPumpMsb::new(&[0xFF, 0xF8, 0x00]);
        assert_eq!(code.decode(&mut pump), None);
        // A code that is a prefix of another, either way round; a
        // length past 27; an empty table.
        assert!(SdCode::build(&table(&[(0, 4, 0b0000), (1, 6, 0b000010)]), &mapping).is_err());
        assert!(SdCode::build(&table(&[(0, 6, 0b000010), (1, 4, 0b0000)]), &mapping).is_err());
        assert!(SdCode::build(&table(&[(0, 4, 0b0000), (1, 4, 0b0000)]), &mapping).is_err());
        assert!(SdCode::build(&table(&[(0, 28, 0)]), &mapping).is_err());
        assert!(SdCode::build(&table(&[]), &mapping).is_err());
    }

    #[test]
    fn sd_rows_outside_the_section_are_corrupt() {
        let mut w = BitWriter::new();
        for code in [0b0010, 0b0000, 0b0001, 0b0001, 0b0010, 0b0000] {
            w.put(code, 4);
        }
        let row = w.finish();
        // A row offset at or past the end of the pixel data.
        let mut body = sd_section(SD_LEVELS, SD_CODES, &[row.clone(), row.clone()]);
        let n = body.len();
        body[n - 4..].copy_from_slice(&8u32.to_le_bytes());
        assert!(matches!(
            decode(&build(3, formats::SD_HUFFMAN, 2, 2, &body)),
            Err(Error::Corrupt(_))
        ));
        // Rows cannot alias, run backwards, or start between the
        // format's four-byte boundaries. Otherwise a few compressed
        // bytes could be reused to materialise an arbitrarily large
        // frame.
        let body = sd_section(SD_LEVELS, SD_CODES, &[row.clone(), row.clone()]);
        for offsets in [[0u32, 0], [4, 0], [0, 3], [4, 8]] {
            let mut malformed = body.clone();
            let at = malformed.len() - 8;
            malformed[at..at + 4].copy_from_slice(&offsets[0].to_le_bytes());
            malformed[at + 4..].copy_from_slice(&offsets[1].to_le_bytes());
            assert!(matches!(
                decode(&build(3, formats::SD_HUFFMAN, 2, 2, &malformed)),
                Err(Error::Corrupt(_))
            ));
        }
        // A row that needs more bits than the pixel data holds: the
        // reader's zero fill would decode as the "-3" code, so the bit
        // count is what catches it.
        let body = sd_section(SD_LEVELS, SD_CODES, &[row[..2].to_vec()]);
        assert!(matches!(
            decode(&build(3, formats::SD_HUFFMAN, 2, 1, &body)),
            Err(Error::Corrupt(_))
        ));
        // A frame the tables cannot describe (zero rows), and a lying
        // frame size.
        let body = sd_section(SD_LEVELS, SD_CODES, &[]);
        assert!(decode(&build(3, formats::SD_HUFFMAN, 2, 0, &body)).is_err());
        assert!(decode(&build(3, formats::SD_HUFFMAN, 60000, 60000, &body)).is_err());
    }

    #[test]
    fn body_levels_follow_the_spec_table() {
        let sd = formats::SD_HUFFMAN;
        let t = formats::TRUE;
        let r = |x, y, width, height| Rect {
            x,
            y,
            width,
            height,
        };
        assert_eq!(
            body_levels("SD9", sd, 2304, 1531),
            (15.0, 12000.0, r(20, 8, 2266, 1510))
        );
        assert_eq!(
            body_levels("SD10", sd, 2304, 1531),
            (15.0, 12000.0, r(20, 8, 2266, 1510))
        );
        assert_eq!(
            body_levels("SD14", sd, 2688, 1792),
            (15.0, 14000.0, r(18, 12, 2651, 1767))
        );
        assert_eq!(
            body_levels("SD15", t, 2688, 1792),
            (15.0, 2900.0, r(18, 12, 2651, 1767))
        );
        assert_eq!(
            body_levels("DP1", t, 2688, 1792),
            (0.0, 2100.0, r(18, 12, 2651, 1767))
        );
        assert_eq!(
            body_levels("DP1S", t, 2688, 1792),
            (0.0, 2200.0, r(18, 12, 2651, 1767))
        );
        assert_eq!(
            body_levels("DP1x", t, 2688, 1792),
            (0.0, 3560.0, r(18, 12, 2651, 1767))
        );
        assert_eq!(
            body_levels("DP2", t, 2688, 1792),
            (0.0, 2326.0, r(13, 16, 2651, 1767))
        );
        assert_eq!(
            body_levels("DP2s", t, 2688, 1792),
            (0.0, 2300.0, r(18, 12, 2651, 1767))
        );
        assert_eq!(
            body_levels("DP2x", t, 2688, 1792),
            (0.0, 2300.0, r(18, 12, 2651, 1767))
        );
        assert_eq!(
            body_levels("DP2 Merrill", t, 4928, 3264),
            (31.0, 3900.0, r(12, 0, 4807, 3205))
        );
        assert_eq!(
            body_levels("dp0 Quattro", formats::QUATTRO, 5888, 3672),
            (2047.0, 16383.0, r(204, 24, 5446, 3624))
        );
    }

    /// The Merrill worked Huffman table of the spec's Section 2c, as
    /// `(code length, code byte)` pairs indexed by symbol.
    const MERRILL_PAIRS: &[(u8, u8)] = &[
        (3, 0x80),
        (3, 0x40),
        (3, 0x20),
        (4, 0xC0),
        (4, 0x60),
        (4, 0xE0),
        (4, 0x70),
        (4, 0xF0),
        (4, 0x10),
        (4, 0xA0),
        (5, 0xD0),
        (6, 0xD8),
        (6, 0xDC),
    ];

    #[test]
    fn huffman_is_a_canonical_category_table() {
        let huff = Huff::build(MERRILL_PAIRS).unwrap();
        assert_eq!(huff.max_len, 6);
        // Feed each code, MSB first, and read back its symbol (= the
        // magnitude-bit count).
        for (symbol, &(len, code)) in MERRILL_PAIRS.iter().enumerate() {
            // A byte whose top `len` bits are the code, low bits set so
            // the peek beyond the code cannot accidentally shorten it.
            let bytes = [code | (0xFF >> len)];
            let mut pump = BitPumpMsb::new(&bytes);
            assert_eq!(
                huff.decode(&mut pump).unwrap(),
                symbol as u32,
                "symbol {symbol}"
            );
            assert_eq!(pump.position(), len as usize);
        }
        // A prefix-free violation is rejected, not silently overwritten.
        assert!(Huff::build(&[(1, 0x80), (2, 0x80)]).is_err());
    }

    #[test]
    fn the_sign_rule_is_leading_bit() {
        // k=9, V=26 (000011010): leading 0 → negative half.
        let mut pump = BitPumpMsb::new(&[0b0000_1101, 0b0000_0000]);
        assert_eq!(difference(&mut pump, 9), 26 - 511);
        // k=4, V=11 (1011): leading 1 → positive half.
        let mut pump = BitPumpMsb::new(&[0b1011_0000]);
        assert_eq!(difference(&mut pump, 4), 11);
        // k=0 consumes nothing and is zero.
        let mut pump = BitPumpMsb::new(&[0xFF]);
        assert_eq!(difference(&mut pump, 0), 0);
        assert_eq!(pump.position(), 0);
    }

    /// Encode symbols and magnitudes MSB-first, the way a layer stream
    /// is laid out, so a decode can be checked without a real file.
    struct BitWriter {
        bytes: Vec<u8>,
        acc: u32,
        nbits: u32,
    }
    impl BitWriter {
        fn new() -> Self {
            BitWriter {
                bytes: Vec::new(),
                acc: 0,
                nbits: 0,
            }
        }
        fn put(&mut self, value: u32, n: u32) {
            for i in (0..n).rev() {
                self.acc = (self.acc << 1) | ((value >> i) & 1);
                self.nbits += 1;
                if self.nbits == 8 {
                    self.bytes.push(self.acc as u8);
                    self.acc = 0;
                    self.nbits = 0;
                }
            }
        }
        fn finish(mut self) -> Vec<u8> {
            if self.nbits > 0 {
                self.bytes.push((self.acc << (8 - self.nbits)) as u8);
            }
            self.bytes
        }
    }

    /// Split a signed difference into the symbol (magnitude-bit count)
    /// and the value bits the spec's Section 4 rule expects.
    fn encode_diff(d: i32) -> (u32, u32) {
        if d == 0 {
            return (0, 0);
        }
        let k = 32 - (d.unsigned_abs()).leading_zeros();
        let v = if d > 0 { d } else { d + ((1 << k) - 1) } as u32;
        (k, v)
    }

    #[test]
    fn the_predictor_reproduces_the_worked_first_row() {
        // Spec Section 4/9, Merrill DP2M1726 layer 0 row 0.
        let huff = Huff::build(MERRILL_PAIRS).unwrap();
        let diffs = [-485, -485, 0, 0, 0, 0, 0, 0, 0, 11, 0, -4];
        let want: [u16; 12] = [27, 27, 27, 27, 27, 27, 27, 27, 27, 38, 27, 34];

        let mut writer = BitWriter::new();
        for &d in &diffs {
            let (k, v) = encode_diff(d);
            let (len, code) = MERRILL_PAIRS[k as usize];
            // The code, right-justified to its own length.
            writer.put((code >> (8 - len)) as u32, len as u32);
            writer.put(v, k);
        }
        let stream = writer.finish();
        let plane = decode_layer(&stream, &huff, 512, diffs.len(), 1).unwrap();
        assert_eq!(plane, want);
    }

    #[test]
    fn quattro_maps_layers_onto_the_even_lattice() {
        // A tiny synthetic Quattro: 4x2 frame, quarter-res 2x1 layers,
        // full-res top 5 wide (one overscan column past the frame's 4).
        // The one-symbol Huffman table maps every code to symbol 0
        // (zero-magnitude), so every difference is 0 and every decoded
        // pixel is the seed 512 — leaving only the placement rule for
        // the assertions to read.
        let (cols, rows) = (4usize, 2usize);
        // Header: plane table (2x1, 2x1, 3x2), seeds, huff, quattro
        // word, three sizes, then three 16-aligned all-"1"-bit streams.
        let ones = |n: usize| vec![0xFFu8; n];
        let mut payload = Vec::new();
        for (c, r) in [(2u16, 1u16), (2, 1), (5, 2)] {
            payload.extend_from_slice(&c.to_le_bytes());
            payload.extend_from_slice(&r.to_le_bytes());
        }
        payload.extend_from_slice(&512u16.to_le_bytes()); // seeds (unused: seed passed below)
        payload.extend_from_slice(&512u16.to_le_bytes());
        payload.extend_from_slice(&512u16.to_le_bytes());
        payload.extend_from_slice(&0u16.to_le_bytes());
        payload.extend_from_slice(&[1, 0x80, 0, 0]); // huff: (1,0x80) then terminator
        payload.extend_from_slice(&0u32.to_le_bytes()); // quattro reserved word
                                                        // sizes: layer0/1 are 2 pixels (2 bits → 1 byte); layer2 is 10
                                                        // pixels (10 bits → 2 bytes).
        let sizes = [1u32, 1, 2];
        for sz in sizes {
            payload.extend_from_slice(&sz.to_le_bytes());
        }
        let start = payload.len();
        // Each layer's "all-ones" stream from its 16-byte-aligned start.
        payload.extend(ones(1));
        payload.resize(start + 16, 0);
        payload.extend(ones(1));
        payload.resize(start + 32, 0);
        payload.extend(ones(2));

        let raw = Image {
            kind: 1,
            format: formats::QUATTRO,
            columns: cols,
            rows,
            row_size: 0,
            data: 0,
            length: payload.len(),
        };
        let section = parse_true(&payload, &raw, true).unwrap();
        assert!(section.quattro);
        assert_eq!(section.sizes, [1, 1, 2]);
        assert_eq!(section.grids, [(2, 1), (2, 1), (5, 2)]);
        let data = decode_true(&payload, &raw, true).unwrap();
        // Plane 2 (top) is full resolution: every pixel = 512.
        for r in 0..rows {
            for c in 0..cols {
                assert_eq!(data[(r * cols + c) * 3 + 2], 512, "top ({r},{c})");
            }
        }
        // Planes 0/1 populate only the even/even lattice.
        for r in 0..rows {
            for c in 0..cols {
                let want = if r % 2 == 0 && c % 2 == 0 { 512 } else { 0 };
                assert_eq!(data[(r * cols + c) * 3], want, "bottom ({r},{c})");
                assert_eq!(data[(r * cols + c) * 3 + 1], want, "middle ({r},{c})");
            }
        }
    }

    #[test]
    fn properties_are_utf16_indexed_by_character() {
        let file = build(1, formats::PLAIN, 1, 1, &[0; 6]);
        let sections = directory(&file).unwrap();
        let list = properties(&file, sections.iter().find(|s| s.kind == *b"PROP").unwrap());
        assert_eq!(
            list,
            vec![
                ("CAMMODEL".to_string(), "SIGMA DP2 Merrill".to_string()),
                ("ISO".to_string(), "400".to_string()),
            ]
        );
    }

    #[test]
    fn a_lossless_jpeg_is_not_a_preview() {
        // SOF3 is sensor data wearing a JPEG's clothes.
        let lossless = [
            0xFFu8, 0xD8, 0xFF, 0xC3, 0, 11, 16, 0, 1, 0, 1, 1, 0, 0x11, 0,
        ];
        assert!(!is_displayable_jpeg(&lossless));
        let baseline = [
            0xFFu8, 0xD8, 0xFF, 0xC0, 0, 11, 8, 0, 1, 0, 1, 1, 0, 0x11, 0,
        ];
        assert!(is_displayable_jpeg(&baseline));
        assert!(!is_displayable_jpeg(&[]));
    }

    #[test]
    fn garbage_is_not_an_x3f() {
        assert!(decode(&[0u8; 64]).is_err());
        assert!(decode(b"FOVb").is_err());
        let file = build(1, formats::PLAIN, 2, 2, &[0; 24]);
        for cut in 0..file.len() {
            let _ = decode(&file[..cut]);
            let _ = preview(&file[..cut]);
        }
    }

    /// The Foveon oracle TIFF: 16-bit RGB, three samples a pixel,
    /// interleaved in the same `(y*w+x)*3+channel` order this module
    /// emits. Read with the crate's own TIFF parser (the `image`
    /// crate's default allocation limit refuses the ~130 MB frames).
    fn oracle3(path: &std::path::Path) -> Option<(usize, usize, Vec<u16>)> {
        let mut name = path.as_os_str().to_os_string();
        name.push(".tiff");
        let bytes = std::fs::read(&name).ok()?;
        let tiff = crate::tiff::Tiff::parse(&bytes).ok()?;
        let layout = crate::tiff::ImageLayout::of(&tiff, tiff.root()).ok()?;
        if layout.bits_per_sample != 16 || layout.samples_per_pixel != 3 {
            panic!(
                "{}: oracle is {} bits x {} samples, expected 16x3",
                path.display(),
                layout.bits_per_sample,
                layout.samples_per_pixel
            );
        }
        let le = tiff.little_endian();
        let mut out = Vec::with_capacity(layout.width * layout.height * 3);
        for (start, len) in &layout.chunks {
            out.extend(
                bytes[*start..*start + *len]
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|b| {
                        if le {
                            u16::from_le_bytes(*b)
                        } else {
                            u16::from_be_bytes(*b)
                        }
                    }),
            );
        }
        out.truncate(layout.width * layout.height * 3);
        Some((layout.width, layout.height, out))
    }

    /// Assert the spec's worked intermediate values for the two primary
    /// files — plane byte sizes, first-row layer values, and (Quattro)
    /// the active-crop corner — before the full oracle comparison.
    fn worked_checks(name: &str, bytes: &[u8]) {
        let sections = directory(bytes).unwrap();
        let raw = sections
            .iter()
            .filter(|s| s.kind == *b"IMA2" || s.kind == *b"IMAG")
            .filter_map(|s| image(bytes, s))
            .find(|i| i.kind == 1 || i.kind == 3)
            .unwrap();
        // Plane `ch` of pixel (row, col) of an interleaved frame.
        let px = |data: &[u16], cols: usize, r: usize, c: usize| -> [u16; 3] {
            let at = (r * cols + c) * 3;
            [data[at], data[at + 1], data[at + 2]]
        };
        let plane = |data: &[u16], cols: usize, r: usize, ch: usize, n: usize| -> Vec<u16> {
            (0..n).map(|c| px(data, cols, r, c)[ch]).collect()
        };
        let plane_max = |data: &[u16], ch: usize| -> u16 {
            data.iter().skip(ch).step_by(3).copied().max().unwrap()
        };
        match name {
            "SIGMA_DP2_Merrill-DP2M1726.X3F" => {
                let section = parse_true(bytes, &raw, false).unwrap();
                assert_eq!(
                    section.sizes,
                    [14938353, 16704166, 17228647],
                    "{name} sizes"
                );
                assert!(!section.quattro);
                let l0 = decode_layer(section.streams[0], &section.huff, section.seeds[0], 4928, 1)
                    .unwrap();
                assert_eq!(&l0[..12], &[27, 27, 27, 27, 27, 27, 27, 27, 27, 38, 27, 34]);
                let l1 = decode_layer(section.streams[1], &section.huff, section.seeds[1], 4928, 1)
                    .unwrap();
                assert_eq!((l1[0], l1[1], l1[9], l1[10]), (27, 27, 29, 39));
                let l2 = decode_layer(section.streams[2], &section.huff, section.seeds[2], 4928, 1)
                    .unwrap();
                assert_eq!((l2[0], l2[1], l2[9], l2[11]), (31, 31, 30, 32));
            }
            "DP0_Quattro-_SDI0263.X3F" => {
                let section = parse_true(bytes, &raw, true).unwrap();
                assert_eq!(section.sizes, [6831306, 6927865, 29012749], "{name} sizes");
                assert!(section.quattro);
                assert_eq!(section.grids, [(2944, 1836), (2944, 1836), (6272, 3672)]);
                let l0 = decode_layer(section.streams[0], &section.huff, section.seeds[0], 2944, 1)
                    .unwrap();
                assert_eq!(&l0[..6], &[2198, 2026, 2044, 2047, 2049, 2061]);
                let l1 = decode_layer(section.streams[1], &section.huff, section.seeds[1], 2944, 1)
                    .unwrap();
                assert_eq!(&l1[..3], &[2125, 2040, 2036]);
                let l2 = decode_layer(section.streams[2], &section.huff, section.seeds[2], 6272, 1)
                    .unwrap();
                assert_eq!(&l2[..3], &[2087, 2039, 2044]);

                // The mapped full frame: plane 0 on the even lattice,
                // plane 2 full-res, and the active-crop corner values.
                let (cols, rows) = (5888usize, 3672usize);
                let data = decode_true(bytes, &raw, true).unwrap();
                let p0 = |r: usize, c: usize| data[(r * cols + c) * 3];
                let p2 = |r: usize, c: usize| data[(r * cols + c) * 3 + 2];
                assert_eq!(
                    [p0(0, 0), p0(0, 1), p0(0, 2), p0(0, 3), p0(0, 4)],
                    [2198, 0, 2026, 0, 2044]
                );
                assert!(
                    (0..cols).all(|c| p0(1, c) == 0),
                    "plane 0 row 1 is all zero"
                );
                assert_eq!([p2(0, 0), p2(0, 1), p2(0, 2)], [2087, 2039, 2044]);
                let _ = rows;
                // Active crop origin (top 24, left 204).
                assert_eq!([p0(24, 204), p0(24, 206)], [2641, 2654]);
                assert_eq!([p2(24, 204), p2(24, 205), p2(24, 206)], [2981, 2968, 2998]);
            }
            "Sigma_SD9-RAW_SIGMA_SD9_SRGB.X3F"
            | "Sigma_SD10-RAW_SIGMA_SD10.X3F"
            | "SIGMA_SD14-20120818-SDIM4579.X3F" => {
                // Spec foveon_sd.md §2, §3, §4, §6c and §7.
                let sd14 = name.contains("SD14");
                let sd9 = name.contains("SD9");
                let cols = if sd14 { 2688 } else { 2304 };
                let rows = if sd14 { 1792 } else { 1531 };
                assert_eq!(
                    (raw.kind, raw.format, raw.row_size, raw.columns, raw.rows),
                    (3, formats::SD_HUFFMAN, 0, cols, rows),
                    "{name} header"
                );
                let section = parse_sd(bytes, &raw).unwrap();
                let (pixel_len, offsets, last) = if sd14 {
                    (9_469_392, [0, 4268, 8540, 12812, 17076, 21360], 9_465_120)
                } else if sd9 {
                    (6_613_844, [0, 3504, 7008, 10508, 14008, 17512], 6_610_348)
                } else {
                    (7_552_392, [0, 3468, 6936, 10404, 13872, 17336], 7_548_924)
                };
                assert_eq!(section.pixels.len(), pixel_len, "{name} D");
                assert_eq!(&section.row_offsets[..6], &offsets, "{name} rowoff");
                assert_eq!(section.row_offsets[rows - 1], last, "{name} rowoff last");

                // The tables as parsed: populated slots, first slot,
                // the length set, the worked words and mapping anchors.
                let table = &bytes[raw.data..raw.data + 6144];
                let mapping: Vec<i16> = table[..2048]
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|b| i16::from_le_bytes(*b))
                    .collect();
                let coding: Vec<u32> = table[2048..]
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|b| u32::from_le_bytes(*b))
                    .collect();
                let populated: Vec<usize> = (0..SD_SLOTS).filter(|&i| coding[i] != 0).collect();
                let (count, first) = if sd14 {
                    (446, 268)
                } else if sd9 {
                    (231, 387)
                } else {
                    (677, 166)
                };
                assert_eq!(
                    (populated.len(), populated[0]),
                    (count, first),
                    "{name} slots"
                );
                let mut lengths: Vec<u32> = populated.iter().map(|&i| coding[i] >> 27).collect();
                lengths.sort_unstable();
                lengths.dedup();
                assert_eq!(lengths, (4..=24).collect::<Vec<u32>>(), "{name} lengths");
                if sd9 {
                    assert_eq!((coding[387], coding[389]), (0xB03F_FFF8, 0xC0FF_FFFF));
                } else if sd14 {
                    assert_eq!((coding[268], coding[269]), (0xB800_0002, 0xC000_0000));
                }
                let zero = if sd14 { 512 } else { 511 };
                assert_eq!(
                    (mapping[0], mapping[zero], mapping[1023]),
                    (-16384, 0, 16384)
                );
                assert_eq!(mapping[zero + 1], if sd14 { 2 } else { 3 });
                assert!(
                    mapping[1..].windows(2).all(|w| w[0] <= w[1]),
                    "{name} monotone"
                );

                // Row 0 with the accumulators at 0, and its bit count.
                let (acc, _, bits) = decode_sd_row(&section.code, section.pixels, cols).unwrap();
                if sd9 {
                    assert_eq!(&acc[..12], &[9, -30, 12, 9, -12, 3, 9, -12, 0, -9, 3, 3]);
                    assert_eq!(bits, 28_001);
                } else {
                    let c0: Vec<i16> = acc.iter().step_by(3).take(12).copied().collect();
                    if sd14 {
                        assert_eq!(c0, [0, 10, 0, -2, 0, 0, 2, 0, 5, 5, 3, 5]);
                        assert_eq!(bits, 34_115);
                    } else {
                        assert_eq!(c0, [156, 84, 42, 15, 3, 3, 3, 3, 0, 0, -3, -6]);
                        assert_eq!(bits, 27_713);
                    }
                }

                // The frame: MIN, the shifted rows, the active corner,
                // the last pixel and the plane maxima.
                let data = decode_sd(bytes, &raw).unwrap();
                let shift = data[0] as i32 - acc[0] as i32;
                assert_eq!(
                    shift,
                    if sd14 {
                        70
                    } else if sd9 {
                        66
                    } else {
                        202
                    },
                    "{name} MIN"
                );
                assert_eq!(
                    *data.iter().min().unwrap(),
                    0,
                    "{name} minimum after the shift"
                );
                /// Row 0 per plane, row 1 plane 0, the active-corner
                /// pixels, the last pixel and the plane maxima.
                type SdWorked = ([[u16; 12]; 3], [u16; 8], [[u16; 3]; 3], [u16; 3], [u16; 3]);
                let (r0, r1, corner, last, max): SdWorked = if sd9 {
                    (
                        [
                            [75, 75, 75, 57, 66, 57, 63, 75, 63, 69, 63, 69],
                            [36, 54, 54, 69, 69, 60, 57, 69, 63, 63, 66, 66],
                            [78, 69, 66, 69, 75, 63, 63, 66, 69, 66, 63, 69],
                        ],
                        [75, 63, 54, 63, 66, 57, 54, 66],
                        [[381, 372, 321], [396, 369, 321], [408, 390, 327]],
                        [63, 63, 54],
                        [1788, 1734, 1425],
                    )
                } else if sd14 {
                    (
                        [
                            [70, 80, 70, 68, 70, 70, 72, 70, 75, 75, 73, 75],
                            [70, 86, 70, 72, 65, 75, 80, 75, 73, 78, 71, 73],
                            [70, 72, 67, 69, 76, 66, 73, 71, 66, 73, 71, 69],
                        ],
                        [70, 60, 65, 70, 70, 68, 70, 63],
                        [[925, 1249, 1290], [955, 1254, 1306], [0, 0, 0]],
                        [57, 72, 69],
                        [2952, 3201, 2856],
                    )
                } else {
                    (
                        [
                            [358, 286, 244, 217, 205, 205, 205, 205, 202, 202, 199, 196],
                            [337, 271, 238, 220, 208, 205, 199, 199, 202, 202, 205, 202],
                            [247, 226, 214, 211, 202, 205, 202, 202, 205, 205, 205, 208],
                        ],
                        [352, 283, 247, 220, 205, 199, 196, 196],
                        [[5352, 6424, 6241], [5554, 6671, 6498], [0, 0, 0]],
                        [265, 211, 214],
                        [9848, 9869, 9545],
                    )
                };
                for ch in 0..3 {
                    assert_eq!(
                        plane(&data, cols, 0, ch, 12),
                        r0[ch],
                        "{name} row 0 plane {ch}"
                    );
                    assert_eq!(plane_max(&data, ch), max[ch], "{name} plane {ch} max");
                }
                assert_eq!(plane(&data, cols, 1, 0, 8), r1, "{name} row 1 plane 0");
                let (top, left) = if sd14 { (12, 18) } else { (8, 20) };
                for (i, want) in corner.iter().enumerate() {
                    if *want != [0, 0, 0] {
                        assert_eq!(px(&data, cols, top, left + i), *want, "{name} corner +{i}");
                    }
                }
                assert_eq!(
                    px(&data, cols, rows - 1, cols - 1),
                    last,
                    "{name} last pixel"
                );
            }
            "DP1-RAW_SIGMA_DP1.X3F"
            | "Sigma_DP1s-RAW_SIGMA_DP1S.X3F"
            | "SIGMA_SD15-_SDI5651.X3F" => {
                // Spec foveon_sd.md §8: the Merrill engine under type 3.
                assert_eq!(
                    (raw.kind, raw.format, raw.columns, raw.rows),
                    (3, formats::TRUE, 2688, 1792),
                    "{name} header"
                );
                let section = parse_true(bytes, &raw, false).unwrap();
                assert_eq!(section.seeds, [512, 512, 512], "{name} seeds");
                assert!(!section.quattro);
                let merrill = Huff::build(MERRILL_PAIRS).unwrap();
                assert_eq!(
                    (section.huff.max_len, &section.huff.table),
                    (merrill.max_len, &merrill.table),
                    "{name} Huffman table"
                );
                let (sizes, first, corner): ([usize; 3], [[u16; 12]; 3], [u16; 3]) = match name {
                    "DP1-RAW_SIGMA_DP1.X3F" => (
                        [2_887_612, 2_913_191, 2_895_127],
                        [
                            [0, 0, 1, 1, 0, 0, 0, 3, 0, 0, u16::MAX, u16::MAX],
                            [
                                0,
                                0,
                                1,
                                0,
                                0,
                                0,
                                1,
                                u16::MAX,
                                u16::MAX,
                                u16::MAX,
                                u16::MAX,
                                u16::MAX,
                            ],
                            [
                                0,
                                0,
                                2,
                                0,
                                2,
                                0,
                                0,
                                u16::MAX,
                                u16::MAX,
                                u16::MAX,
                                u16::MAX,
                                u16::MAX,
                            ],
                        ],
                        [31, 52, 64],
                    ),
                    "Sigma_DP1s-RAW_SIGMA_DP1S.X3F" => (
                        [4_116_577, 4_265_314, 4_152_872],
                        [
                            [0, 0, 0, 2, 0, 1, 4, 0, 3, 2, 1, 3],
                            [u16::MAX; 12],
                            [u16::MAX; 12],
                        ],
                        [225, 393, 450],
                    ),
                    _ => (
                        [4_871_649, 4_954_112, 4_907_320],
                        [[0; 12], [0; 12], [0; 12]],
                        [313, 502, 716],
                    ),
                };
                assert_eq!(section.sizes, sizes, "{name} sizes");
                for (layer, columns) in first.iter().enumerate() {
                    let row = decode_layer(
                        section.streams[layer],
                        &section.huff,
                        section.seeds[layer],
                        2688,
                        1,
                    )
                    .unwrap();
                    // `u16::MAX` marks a column the spec does not list.
                    for (c, want) in columns.iter().enumerate() {
                        if *want != u16::MAX {
                            assert_eq!(row[c], *want, "{name} layer {layer} col {c}");
                        }
                    }
                }
                let data = decode_true(bytes, &raw, false).unwrap();
                assert_eq!(px(&data, 2688, 12, 18), corner, "{name} active corner");
            }
            _ => {}
        }
    }

    /// `Image size` and `black` from the identify sidecar, when present.
    fn identify_facts(path: &std::path::Path) -> (Option<(usize, usize)>, Option<f32>) {
        let Some(text) = corpus::identify(path) else {
            return (None, None);
        };
        let mut size = None;
        let mut black = None;
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("Image size:") {
                let mut it = rest.split('x').map(|t| t.trim().parse::<usize>());
                if let (Some(Ok(w)), Some(Ok(h))) = (it.next(), it.next()) {
                    size = Some((w, h));
                }
            } else if let Some(rest) = line.strip_prefix("black:") {
                black = rest.trim().parse().ok();
            }
        }
        (size, black)
    }

    #[test]
    fn corpus_decodes_against_the_oracle() {
        let files = corpus::files(&["x3f"]);
        // Every corpus file has an oracle and must decode exactly; no
        // file is legitimately unsupported here (the SD9/10/14 code has
        // no sample). Any format that ends up unsupported is listed.
        let allow_unsupported: &[&str] = &[];
        for path in &files {
            let bytes = std::fs::read(path).unwrap();
            let name = corpus::name(path);
            assert_eq!(
                crate::probe(&bytes),
                Some(Format::X3f),
                "{name} did not probe as X3F"
            );

            let (make, model, _meta, jpeg) = super::metadata(&bytes).unwrap();
            assert_eq!(make, "SIGMA", "{name}: make");
            assert!(model.starts_with("SIGMA"), "{name}: model {model:?}");
            // The SD9 and SD10 write no JPEG at all — their processed
            // pictures are an uncompressed 567x378 pixmap (the SD10's
            // in the 8-bit Huffman flavour) and a 189x126 one — so
            // there the preview is legitimately absent.
            let no_preview: &[&str] = &[
                "Sigma_SD9-RAW_SIGMA_SD9_SRGB.X3F",
                "Sigma_SD10-RAW_SIGMA_SD10.X3F",
            ];
            match jpeg {
                Some(jpeg) => {
                    let decoded = image::load_from_memory(&jpeg)
                        .unwrap_or_else(|e| panic!("{name}: preview will not decode: {e}"));
                    assert!(
                        decoded.width() >= 640,
                        "{name}: preview {}px",
                        decoded.width()
                    );
                    assert_eq!(preview(&bytes).unwrap(), Some(jpeg), "{name}");
                }
                None => assert!(no_preview.contains(&name.as_str()), "{name}: no preview"),
            }

            worked_checks(&name, &bytes);

            let raw = match decode(&bytes) {
                Ok(raw) => raw,
                Err(Error::Unsupported(msg)) if allow_unsupported.contains(&name.as_str()) => {
                    eprintln!("{name}: unsupported (allowed): {msg}");
                    continue;
                }
                Err(e) => panic!("{name}: {e}"),
            };
            raw.validate().unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(raw.cpp, 3, "{name}: cpp");
            assert_eq!(raw.cfa, Cfa::None, "{name}");
            let (size, black) = identify_facts(path);
            if let Some((w, h)) = size {
                assert_eq!(
                    (raw.crop.width, raw.crop.height),
                    (w, h),
                    "{name}: crop size"
                );
            }
            if let Some(black) = black {
                assert_eq!(raw.black_levels, [black; 4], "{name}: black");
            }
            assert!(raw.white_level < 65535.0, "{name}: white level unset");

            if let Some((w, h, oracle)) = oracle3(path) {
                assert_eq!((raw.width, raw.height), (w, h), "{name}: dimensions");
                let RawData::U16(data) = &raw.data else {
                    panic!("{name}: not U16")
                };
                assert_eq!(data.len(), oracle.len(), "{name}: sample count");
                let mut mismatches = 0usize;
                let mut first = Vec::new();
                for (i, (&got, &want)) in data.iter().zip(&oracle).enumerate() {
                    if got != want {
                        mismatches += 1;
                        if first.len() < 8 {
                            let px = i / 3;
                            first.push(format!(
                                "({},{}) ch{}: {got} != {want}",
                                px % w,
                                px / w,
                                i % 3
                            ));
                        }
                    }
                }
                assert_eq!(
                    mismatches,
                    0,
                    "{name}: {mismatches}/{} samples differ, first: {first:?}",
                    data.len()
                );
                eprintln!("x3f: {name} matches oracle ({w}x{h}x3)");
            } else {
                panic!("{name}: no oracle TIFF present");
            }
        }
        eprintln!("x3f: {} corpus files checked", files.len());
    }

    /// The file cut at `cut` with its directory carried over and the
    /// pointer fixed, so the cut lands inside the image section rather
    /// than in the directory, which a plain truncation always destroys
    /// first (the pointer is the file's last four bytes).
    fn cut_keeping_directory(bytes: &[u8], cut: usize) -> Vec<u8> {
        let dir = u32::from_le_bytes(bytes[bytes.len() - 4..].try_into().unwrap()) as usize;
        let directory = &bytes[dir..bytes.len() - 4];
        let mut out = bytes[..cut.min(dir)].to_vec();
        let new_dir = out.len() as u32;
        out.extend_from_slice(directory);
        out.extend_from_slice(&new_dir.to_le_bytes());
        out
    }

    #[test]
    fn corpus_truncations_do_not_panic() {
        for path in corpus::files(&["x3f"]) {
            corpus::check_truncations(&path, decode);
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            for cut in [0, 1, 4, 64, 1024, bytes.len() / 2, bytes.len() - 1] {
                let _ = preview(&bytes[..cut.min(bytes.len())]);
            }
            // Cuts inside the sensor section: the entropy decoder must
            // see short layers and a short header, not just a missing
            // directory.
            for fraction in [2, 3, 5, 10, 50, 200, 1000] {
                let cut = bytes.len() - bytes.len() / fraction;
                let short = cut_keeping_directory(&bytes, cut);
                match decode(&short) {
                    Ok(_) | Err(Error::Corrupt(_)) | Err(Error::Unsupported(_)) => {}
                    Err(other) => panic!("{}: {other:?}", path.display()),
                }
            }
            // And a forged Huffman table: a code length past eight and
            // a table of too many symbols must be refused, not indexed.
            for (len, syms) in [(9u8, 12usize), (3, 40), (24, 12), (0, 12)] {
                let pairs: Vec<(u8, u8)> = (0..syms)
                    .map(|i| (len, (i as u8).wrapping_mul(17)))
                    .collect();
                assert!(
                    Huff::build(&pairs).is_err(),
                    "length {len} x {syms} symbols"
                );
            }
        }
    }
}
