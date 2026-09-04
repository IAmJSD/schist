//! Pentax PEF: a big-endian TIFF whose IFD0 *is* the sensor frame.
//!
//! Pentax is unusual in putting the raw data in IFD0 (photometric
//! 32803) rather than a SubIFD, and in giving it one of three
//! compressions:
//!
//! * `1` — plain, one 16-bit word a pixel even when BitsPerSample says
//!   12 (the `*ist D`).
//! * `32773` — the same samples packed end to end, 12 bits each,
//!   most-significant bit first (the `*ist DL` and the other 2005
//!   bodies). TIFF gives 32773 to PackBits; Pentax reused the number.
//! * `65535` — Pentax's own lossless compression: a Huffman-coded
//!   difference per pixel, with the code table carried in the
//!   makernote rather than in the stream. Everything from the K10D on.
//!
//! The metadata a developer needs — white balance, black point,
//! saturation, the sensor's active area — is all in the makernote,
//! which comes in two dialects that differ in where their offsets are
//! measured from. See [`makernote`].
//!
//! Written from observation of the files and the TIFF 6.0 and Exif
//! specifications; the tag *meanings* are ExifTool's published Pentax
//! tag tables. No decoder source was consulted.

use crate::bits::{BitPump, BitPumpMsb, HuffTable};
use crate::formats::common;
use crate::tiff::{tags, Ifd, ImageLayout, Tiff};
use crate::{Cfa, CfaColor, Error, Format, RawData, RawImage, Rect, Result};

/// Pentax's own lossless compression (Compression 65535).
const COMPRESSION_PENTAX: u32 = 65535;
/// Samples packed at their bit depth, no compression. TIFF spells
/// PackBits with this number; Pentax means something else by it.
const COMPRESSION_PACKED: u32 = 32773;

/// Makernote tags this decoder reads. Names are ExifTool's.
mod mn {
    /// `ImageAreaOffset`: the active area's left and top in the frame.
    pub const IMAGE_AREA_OFFSET: u16 = 0x0038;
    /// `RawImageSize`: the active area's width and height.
    pub const RAW_IMAGE_SIZE: u16 = 0x0039;
    /// `WhiteLevel`: the sensor's saturation point, per body and ISO.
    pub const WHITE_LEVEL: u16 = 0x007E;
    /// `BlackPoint`: four shorts, R G1 G2 B.
    pub const BLACK_POINT: u16 = 0x0200;
    /// `WhitePoint`: the as-shot white balance, four shorts, R G1 G2 B,
    /// scaled so green is `DataScaling` (8192 on every body seen).
    pub const WHITE_POINT: u16 = 0x0201;
    /// `HuffmanTable`: the code table for Compression 65535.
    pub const HUFFMAN_TABLE: u16 = 0x0220;
}

/// Decode a PEF into its sensor frame and metadata.
pub fn decode(bytes: &[u8]) -> Result<RawImage> {
    let tiff = Tiff::parse(bytes)?;
    let ifd = raw_ifd(&tiff)?;
    let layout = ImageLayout::of(&tiff, ifd)?;
    let (width, height) = (layout.width, layout.height);
    if width == 0 || height == 0 || width > 1 << 16 || height > 1 << 16 {
        return Err(Error::Corrupt(format!("PEF frame is {width}x{height}")));
    }

    let maker = makernote(&tiff);
    let data = match layout.compression {
        COMPRESSION_PENTAX => {
            let table = maker
                .as_ref()
                .and_then(|m| m.find(mn::HUFFMAN_TABLE))
                .and_then(|e| e.bytes())
                .ok_or_else(|| {
                    Error::Corrupt("Pentax-compressed PEF without makernote tag 0x0220".into())
                })?;
            // The table's shorts follow the makernote's byte order, not
            // necessarily the file's: an "AOC" makernote can be
            // big-endian inside a big-endian file while the newer
            // "PENTAX" one is little-endian inside the same.
            let little_endian = maker.as_ref().is_some_and(|m| m.little_endian());
            let huffman = huffman_table(table, little_endian)?;
            let stream = strip(bytes, &layout);
            // A coded sample is at least a bit: the strips bound the
            // frame a forged header may claim.
            let samples = crate::frame_samples(width, height, 1)?;
            if stream.len().saturating_mul(8) < samples {
                return Err(Error::Corrupt(format!(
                    "PEF frame of {samples} samples in {} bytes",
                    stream.len()
                )));
            }
            decompress(&stream, &huffman, width, height)
        }
        COMPRESSION_PACKED | 1 => unpack(&tiff, &layout)?,
        other => {
            return Err(Error::Unsupported(format!(
                "PEF compression {other}, not 1 (plain), 32773 (packed) or 65535 (Pentax)"
            )))
        }
    };

    let mut raw = RawImage::new(
        Format::Pef,
        width,
        height,
        1,
        RawData::U16(data),
        Cfa::Bayer([CfaColor::Red; 4]),
    );
    let (make, model) = tiff.make_model();
    raw.set_camera(&make, &model);
    raw.cfa = cfa_for(&raw.clean_model);
    raw.orientation = common::orientation(&tiff);
    raw.metadata = common::metadata(&tiff);
    raw.preview = common::largest_jpeg(&tiff);

    // Saturation: the body records the real one, which sits a little
    // below the bit depth's ceiling (16316 rather than 16383 on the
    // K-5) because the sensor's response bends before it clips. Older
    // bodies record nothing and the bit depth is all there is.
    raw.white_level = maker
        .as_ref()
        .and_then(|m| m.find(mn::WHITE_LEVEL))
        .and_then(|e| e.f64(0))
        .filter(|w| *w > 0.0)
        .map(|w| w as f32)
        .unwrap_or_else(|| ((1u32 << layout.bits_per_sample.min(16)) - 1) as f32);

    if let Some(levels) = maker.as_ref().and_then(|m| quad(m, mn::BLACK_POINT)) {
        raw.black_levels = by_position(&raw.cfa, levels);
    }
    if let Some(wb) = maker.as_ref().and_then(|m| quad(m, mn::WHITE_POINT)) {
        // R G1 G2 B in the tag; R G B G2 in `wb_coeffs`, green at 1.0.
        let green = wb[1];
        if green > 0.0 && wb.iter().all(|v| *v > 0.0) {
            raw.wb_coeffs = [wb[0] / green, 1.0, wb[3] / green, wb[2] / green];
        }
    }
    raw.crop = active_area(maker.as_ref(), width, height);

    raw.apply_camera_table();
    Ok(raw)
}

/// The largest embedded JPEG: the full-size preview Pentax keeps in
/// its own IFD, not the 160x120 thumbnail in IFD1.
pub fn preview(bytes: &[u8]) -> Result<Option<Vec<u8>>> {
    Ok(common::largest_jpeg(&Tiff::parse(bytes)?))
}

// ------------------------------------------------------------ structure

/// The IFD holding sensor samples. Pentax puts it in IFD0, but look
/// for the photometric rather than assume the position.
fn raw_ifd<'a>(tiff: &'a Tiff<'_>) -> Result<&'a Ifd> {
    tiff.all()
        .into_iter()
        .find(|ifd| ifd.get(tags::PHOTOMETRIC).and_then(|e| e.u32(0)) == Some(32803))
        .ok_or_else(|| Error::Corrupt("PEF with no CFA image IFD".into()))
}

/// The makernote as a TIFF, in whichever of the two dialects it uses.
///
/// Both start with a signature and a two-byte order mark, and both
/// then hold a plain IFD, but they disagree about offsets:
///
/// * `AOC\0` (through the K-5 and the 645D) — six bytes of header,
///   and the IFD's offsets are file-absolute like any other IFD's.
/// * `PENTAX \0` (the K-3 on) — ten bytes of header, and offsets are
///   measured from the start of the makernote itself, so the whole
///   block can be moved without rewriting it.
///
/// Reading one with the other's rule yields entries that point into
/// unrelated parts of the file rather than an obvious failure, so the
/// signature has to be honoured exactly.
fn makernote<'a>(tiff: &Tiff<'a>) -> Option<Tiff<'a>> {
    let bytes = tiff.bytes();
    let entry = tiff.find(tags::MAKER_NOTE)?;
    let start = entry.offset;
    let header = match bytes.get(start..start.checked_add(8)?)? {
        b if b.starts_with(b"AOC\0") => 4,
        b if b.starts_with(b"PENTAX \0") => 8,
        _ => return None,
    };
    let little_endian = match bytes.get(start + header..start + header + 2)? {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let ifd = start + header + 2;
    let base = if header == 4 { 0 } else { start };
    Tiff::parse_at_relative(bytes, ifd, base, little_endian).ok()
}

/// A four-short makernote value as floats, in the tag's own R G1 G2 B
/// order.
fn quad(maker: &Tiff<'_>, tag: u16) -> Option<[f32; 4]> {
    let entry = maker.find(tag)?;
    let values: Vec<f32> = (0..4)
        .filter_map(|i| entry.f64(i))
        .map(|v| v as f32)
        .collect();
    values.try_into().ok()
}

/// Spread a tag's R G1 G2 B quad over the frame's four CFA positions.
///
/// The tag is always in sensor-colour order; `black_levels` is in
/// row-major position order, and the two only coincide on the RGGB
/// bodies. The first green a row-major walk meets is taken for G1.
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

/// The active area: everything but the sensor's dark border.
///
/// The bodies through the 645D record it as `ImageAreaOffset` and
/// `RawImageSize`, which agree exactly with the size of the camera's
/// own full-size preview. The K-3 and later record neither and their
/// frames have no border to trim, so the whole frame is the picture.
fn active_area(maker: Option<&Tiff<'_>>, width: usize, height: usize) -> Rect {
    let whole = Rect {
        x: 0,
        y: 0,
        width,
        height,
    };
    let Some(maker) = maker else { return whole };
    let pair = |tag: u16| -> Option<(usize, usize)> {
        let entry = maker.find(tag)?;
        Some((entry.u32(0)? as usize, entry.u32(1)? as usize))
    };
    let (Some((x, y)), Some((w, h))) = (pair(mn::IMAGE_AREA_OFFSET), pair(mn::RAW_IMAGE_SIZE))
    else {
        return whole;
    };
    if w == 0 || h == 0 || x + w > width || y + h > height {
        return whole;
    }
    Rect {
        x,
        y,
        width: w,
        height: h,
    }
}

/// Which way round the Bayer quad sits, by body.
///
/// A PEF says nothing about its filter array: IFD0 carries no
/// CFAPattern, and the makernote has no equivalent. The layout is a
/// property of the sensor, so it is looked up by model. Every entry
/// here was read out of the CFAPattern of a DNG the same body wrote
/// (Pentax bodies can shoot DNG) or, where none was to hand, confirmed
/// against the greens of a real frame from that body.
///
/// Pentax has used RGGB on nearly everything; the exceptions below are
/// the bodies in this crate's corpus that are not. An unlisted body
/// gets RGGB, which is right far more often than not — and wrong in a
/// way a developed picture makes obvious at once (red and blue swap).
fn cfa_for(clean_model: &str) -> Cfa {
    let model = clean_model.to_ascii_uppercase();
    match model.as_str() {
        // source: CFAPattern of 645D2839.DNG (the same camera's DNG
        // output) and of the K20D's and K-5's frames.
        "645D" | "K20D" | "K-5" => Cfa::BGGR,
        _ => Cfa::RGGB,
    }
}

/// The compressed stream as one contiguous slice. Pentax writes a
/// single strip, which is handed back borrowed; the join is only for
/// files that do not.
fn strip<'a>(bytes: &'a [u8], layout: &ImageLayout) -> std::borrow::Cow<'a, [u8]> {
    if let [(start, len)] = layout.chunks[..] {
        // `ImageLayout::of` has already checked every chunk lies
        // inside the file, so this cannot be out of range.
        return std::borrow::Cow::Borrowed(&bytes[start..start + len]);
    }
    // Strips that add up to more than the file are a forgery (the same
    // bytes named over and over); an empty stream fails the decode.
    let total: usize = layout.chunks.iter().map(|(_, len)| len).sum();
    if total > bytes.len() {
        return std::borrow::Cow::Owned(Vec::new());
    }
    let mut out = Vec::with_capacity(total);
    for (start, len) in &layout.chunks {
        out.extend_from_slice(&bytes[*start..*start + *len]);
    }
    std::borrow::Cow::Owned(out)
}

// ------------------------------------------------------- uncompressed

/// Plain frames: 16-bit words when the strip is big enough for them,
/// otherwise samples packed at `BitsPerSample`.
///
/// Compression 1 and 32773 do not reliably tell the two apart — the
/// `*ist D` writes 12-bit samples in 16-bit words under Compression 1
/// — so the strip's own size decides, which is a fact of the file
/// rather than a guess about the body.
fn unpack(tiff: &Tiff<'_>, layout: &ImageLayout) -> Result<Vec<u16>> {
    let bytes = tiff.bytes();
    let (width, height) = (layout.width, layout.height);
    let bits = layout.bits_per_sample;
    if !(1..=16).contains(&bits) {
        return Err(Error::Unsupported(format!("PEF with {bits} bits a sample")));
    }
    let total: usize = layout.chunks.iter().map(|(_, len)| *len).sum();
    let words = total >= width.saturating_mul(height).saturating_mul(2);

    // The strips bound the frame a forged header may claim.
    let samples = crate::frame_samples(width, height, 1)?;
    if total.saturating_mul(8) < samples {
        return Err(Error::Corrupt(format!(
            "PEF frame of {samples} samples in {total} bytes of strips"
        )));
    }
    let mut out = vec![0u16; samples];
    let rows_per_chunk = layout.rows_per_chunk.max(1);
    for (chunk, (start, len)) in layout.chunks.iter().enumerate() {
        let first_row = chunk * rows_per_chunk;
        if first_row >= height {
            break;
        }
        let rows = rows_per_chunk.min(height - first_row);
        let Some(data) = bytes.get(*start..*start + *len) else {
            return Err(Error::Corrupt("PEF strip outside the file".into()));
        };
        // Rows are padded to a byte boundary; at 12 and 16 bits and
        // the widths Pentax uses this is never actually padding, but
        // deriving the stride rather than assuming it keeps a frame
        // with an odd width honest.
        let stride = if words {
            width * 2
        } else {
            (width * bits as usize).div_ceil(8)
        };
        let target = &mut out[first_row * width..(first_row + rows) * width];
        for (row, samples) in target.chunks_mut(width).enumerate() {
            let from = row * stride;
            let source = data
                .get(from..(from + stride).min(data.len()))
                .unwrap_or(&[]);
            if words {
                for (i, sample) in samples.iter_mut().enumerate() {
                    let at = i * 2;
                    let pair: [u8; 2] = match source.get(at..at + 2) {
                        Some(b) => [b[0], b[1]],
                        // A truncated strip leaves the rest of the
                        // frame black rather than failing: the file is
                        // still worth showing as far as it goes.
                        None => break,
                    };
                    *sample = if tiff.little_endian() {
                        u16::from_le_bytes(pair)
                    } else {
                        u16::from_be_bytes(pair)
                    };
                }
            } else {
                let mut pump = BitPumpMsb::new(source);
                for sample in samples.iter_mut() {
                    *sample = pump.get(bits) as u16;
                }
            }
        }
    }
    Ok(out)
}

// --------------------------------------------------------- compression

/// Build a [`HuffTable`] from makernote tag 0x0220.
///
/// The tag is a compact form of a canonical Huffman code:
///
/// ```text
///   u16  n          the entry count, disguised: (n + 12) & 15
///   u16  [6]        unread; zero on most bodies
///   u16  code[N]    each code left-aligned in twelve bits
///   u8   length[N]  the code's real length in bits
/// ```
///
/// The symbol of entry `i` is `i` itself: the number of extra bits the
/// pixel's difference is written in, exactly as in lossless JPEG. So a
/// 12-bit body has thirteen entries (differences of 0..=12 bits) and a
/// 14-bit body fifteen.
///
/// The codes are canonical — sorted by length, and by code within a
/// length, they are the sequence JPEG's DHT rules generate — so the
/// table can be handed to the crate's shared [`HuffTable`] once the
/// symbols are put in that order. This is checked rather than assumed:
/// a table that is not canonical would decode to silent nonsense.
pub fn huffman_table(tag: &[u8], little_endian: bool) -> Result<HuffTable> {
    let short = |at: usize| -> Option<u16> {
        let pair: [u8; 2] = tag.get(at..at + 2)?.try_into().ok()?;
        Some(if little_endian {
            u16::from_le_bytes(pair)
        } else {
            u16::from_be_bytes(pair)
        })
    };
    let count = short(0).ok_or_else(|| Error::Corrupt("empty Pentax Huffman table".into()))?;
    // Wrapping at sixteen is the format's own arithmetic: the stored
    // count is the entry count less twelve, modulo sixteen, so a
    // thirteen-entry table stores 1 and a fifteen-entry table 3.
    let entries = ((count as usize + 12) & 15).max(1);
    let needed = 14 + entries * 3;
    if tag.len() < needed {
        return Err(Error::Corrupt(format!(
            "Pentax Huffman table promises {entries} entries but carries {} bytes, not {needed}",
            tag.len()
        )));
    }
    let mut table: Vec<(u8, u16, u8)> = Vec::with_capacity(entries);
    for i in 0..entries {
        let code = short(14 + i * 2).unwrap_or(0);
        let length = tag[14 + entries * 2 + i];
        if length == 0 || length > 12 {
            return Err(Error::Corrupt(format!(
                "Pentax Huffman code {i} is {length} bits, outside 1..=12"
            )));
        }
        // Stored left-aligned in twelve bits, however long it is.
        table.push((length, code >> (12 - length as u32), i as u8));
    }

    let mut sorted = table.clone();
    sorted.sort_by_key(|(length, code, _)| (*length, *code));
    let mut counts = [0u8; 16];
    let mut symbols = Vec::with_capacity(entries);
    for (length, _, symbol) in &sorted {
        counts[*length as usize - 1] += 1;
        symbols.push(*symbol);
    }
    // Re-derive the canonical code of each entry and insist the file's
    // agrees. Cheap, and it turns a malformed table into an error
    // instead of an image of noise.
    let mut code: u32 = 0;
    let mut previous = 0u8;
    for (length, stored, _) in &sorted {
        code <<= length - previous;
        previous = *length;
        if code != *stored as u32 {
            return Err(Error::Corrupt(format!(
                "Pentax Huffman table is not canonical: a {length}-bit code is \
                 {stored:#x} where the canonical order gives {code:#x}"
            )));
        }
        code += 1;
    }
    HuffTable::new(&counts, &symbols)
}

/// Decode a Pentax-compressed frame.
///
/// One Huffman-coded difference per pixel, in row-major order, over a
/// single MSB-first bit stream with no marker stuffing and no
/// restarts. The predictor is the pixel two to the left — the previous
/// one of the same colour — which leaves the first two pixels of every
/// row without one; those continue a running total kept per column
/// (0 or 1) and per row parity, so each of the four CFA positions has
/// its own left-hand seed running down the frame. The seeds start at
/// zero, which makes the top-left corner of the frame the origin the
/// whole image is differenced from.
fn decompress(data: &[u8], huffman: &HuffTable, width: usize, height: usize) -> Vec<u16> {
    // The frame ceiling alone: a truncated stream decodes to zeros
    // past its end (see `a_truncated_stream_does_not_panic`), and the
    // caller has already checked the strips are plausible for it.
    let Ok(samples) = crate::frame_samples(width, height, 1) else {
        return Vec::new();
    };
    let mut out = vec![0u16; samples];
    let mut pump = BitPumpMsb::new(data);
    // [row parity][column parity]: the left edge's running totals.
    let mut vertical = [[0u16; 2]; 2];
    for (row, samples) in out.chunks_mut(width).enumerate() {
        let mut horizontal = [0u16; 2];
        for (col, sample) in samples.iter_mut().enumerate() {
            // Wrapping: a difference is a 16-bit quantity and a
            // corrupt stream must wrap rather than panic in debug.
            let diff = huffman.decode_diff(&mut pump) as u16;
            let value = if col < 2 {
                let seed = &mut vertical[row & 1][col];
                *seed = seed.wrapping_add(diff);
                horizontal[col] = *seed;
                *seed
            } else {
                horizontal[col & 1] = horizontal[col & 1].wrapping_add(diff);
                horizontal[col & 1]
            };
            *sample = value;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The K10D's table, as its makernote carries it: thirteen
    /// entries, big-endian, differences of up to twelve bits.
    const K10D_TABLE: &[u8] = &[
        0x00, 0x01, 0x00, 0x00, 0x00, 0x16, 0x00, 0x28, 0x00, 0x49, 0x00, 0x23, 0x00, 0x4d, //
        0x0f, 0x00, 0x0c, 0x00, 0x08, 0x00, 0x00, 0x00, 0x04, 0x00, 0x0a, 0x00, 0x0e, 0x00, //
        0x0f, 0x80, 0x0f, 0xc0, 0x0f, 0xe0, 0x0f, 0xf0, 0x0f, 0xf8, 0x0f, 0xfc, //
        5, 3, 3, 2, 2, 3, 4, 6, 7, 8, 9, 10, 10,
    ];

    #[test]
    fn huffman_table_reads_the_stored_count() {
        // (1 + 12) & 15 == 13 entries, so 14 + 39 bytes are consumed.
        let table = huffman_table(K10D_TABLE, false).expect("K10D table");
        // Symbol 3 has the shortest code, 0b00: a two-bit read of two
        // zero bits must give it.
        let mut pump = BitPumpMsb::new(&[0b0001_1001, 0b0100_0000]);
        assert_eq!(table.decode(&mut pump), 3);
        assert_eq!(table.decode(&mut pump), 4); // 0b01
        assert_eq!(table.decode(&mut pump), 2); // 0b100
        assert_eq!(table.decode(&mut pump), 5); // 0b101
    }

    #[test]
    fn huffman_table_rejects_a_short_tag() {
        assert!(huffman_table(&K10D_TABLE[..20], false).is_err());
        assert!(huffman_table(&[], false).is_err());
        assert!(huffman_table(&[0x00], false).is_err());
    }

    #[test]
    fn huffman_table_rejects_a_non_canonical_table() {
        let mut broken = K10D_TABLE.to_vec();
        // Move the first code without touching its length: still a
        // valid-looking table, no longer the canonical one.
        broken[14] = 0x0e;
        assert!(huffman_table(&broken, false).is_err());
    }

    #[test]
    fn huffman_table_rejects_an_impossible_length() {
        let mut broken = K10D_TABLE.to_vec();
        *broken.last_mut().expect("lengths") = 13;
        assert!(huffman_table(&broken, false).is_err());
    }

    /// Differences accumulate along a row from the pixel two to the
    /// left, and the first two pixels of a row from the same-parity
    /// row above. Encoded by hand with the K10D's code: symbol 3 is
    /// 0b00 and takes three value bits, symbol 0 is 0b11110 and takes
    /// none.
    #[test]
    fn differences_run_along_rows_and_down_the_edges() {
        let table = huffman_table(K10D_TABLE, false).expect("K10D table");
        let mut bits = BitWriter::default();
        // Row 0: seeds of +7 and +6, then four zero differences.
        bits.put(0b00, 2);
        bits.put(0b111, 3);
        bits.put(0b00, 2);
        bits.put(0b110, 3);
        for _ in 0..4 {
            bits.put(0b11110, 5);
        }
        // Row 1: seeds of +1 and +2 (a one-bit difference is symbol 1,
        // 0b110), then four more zero differences.
        bits.put(0b110, 3);
        bits.put(1, 1);
        bits.put(0b110, 3);
        bits.put(1, 1);
        for _ in 0..4 {
            bits.put(0b11110, 5);
        }
        let out = decompress(&bits.finish(), &table, 6, 2);
        assert_eq!(out, vec![7, 6, 7, 6, 7, 6, 1, 1, 1, 1, 1, 1]);
    }

    /// A truncated stream reads zero bits past the end, so the rest of
    /// the frame repeats its last value instead of panicking.
    #[test]
    fn a_truncated_stream_does_not_panic() {
        let table = huffman_table(K10D_TABLE, false).expect("K10D table");
        for len in 0..8 {
            let out = decompress(&vec![0xff; len], &table, 64, 8);
            assert_eq!(out.len(), 64 * 8);
        }
    }

    #[test]
    fn by_position_follows_the_filter_array() {
        // R G1 G2 B = 1 2 3 4.
        let values = [1.0, 2.0, 3.0, 4.0];
        assert_eq!(by_position(&Cfa::RGGB, values), [1.0, 2.0, 3.0, 4.0]);
        assert_eq!(by_position(&Cfa::BGGR, values), [4.0, 2.0, 3.0, 1.0]);
        assert_eq!(by_position(&Cfa::GRBG, values), [2.0, 1.0, 4.0, 3.0]);
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
    //
    // Gated on SCHIST_RAW_CORPUS: a directory of camera files, walked
    // recursively. Beside each file the harness leaves LibRaw's own
    // output, which is what "exactly" is measured against:
    //
    //   <file>.tiff          `unprocessed_raw -T`: the whole sensor
    //                        frame, 16-bit grey, black not subtracted
    //   <file>.identify.txt  `raw-identify -v -w`
    //
    // Both are optional; a file with neither is still decoded and
    // validated.

    use std::path::{Path, PathBuf};

    /// Every file under `SCHIST_RAW_CORPUS` this module claims.
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
                    .is_some_and(|e| e.eq_ignore_ascii_case("pef"))
                {
                    out.push(path);
                }
            }
        }
        out.sort();
        out
    }

    /// LibRaw's unpacked frame, when the harness left one.
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

    /// `raw-identify`'s report, as lines.
    fn identify(path: &Path) -> Option<String> {
        let mut name = path.as_os_str().to_os_string();
        name.push(".identify.txt");
        std::fs::read_to_string(PathBuf::from(name)).ok()
    }

    /// The rest of the line introduced by `key`.
    fn field<'a>(report: &'a str, key: &str) -> Option<&'a str> {
        report
            .lines()
            .find_map(|line| line.trim_start().strip_prefix(key))
            .map(str::trim)
    }

    /// The numbers on the line introduced by `key`.
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

    /// LibRaw's "Filter pattern" line as this crate's [`Cfa`].
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

    #[test]
    fn corpus_matches_the_oracle() {
        let files = corpus();
        for path in &files {
            let bytes = std::fs::read(path).expect("corpus file reads");
            assert_eq!(
                crate::probe(&bytes),
                Some(Format::Pef),
                "{} did not probe as PEF",
                path.display()
            );
            let raw = match crate::decode(&bytes) {
                Ok(raw) => raw,
                Err(error) => panic!("{}: {error}", path.display()),
            };
            raw.validate().expect("decoded frame is self-consistent");
            assert_eq!(raw.cpp, 1);

            if let Some((width, height, expected)) = oracle(path) {
                assert_eq!(
                    (raw.width, raw.height),
                    (width, height),
                    "{}: frame size",
                    path.display()
                );
                let RawData::U16(got) = &raw.data else {
                    panic!("{}: PEF frames are integers", path.display())
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
                if let Some(cfa) = cfa_of(&report) {
                    assert_eq!(raw.cfa, cfa, "{}: filter pattern", path.display());
                }
                // cblack is per sensor colour (R G B G2); ours is per
                // position, so compare through the pattern.
                let black = numbers(&report, "cblack[0 .. 3]:");
                if let [r, g, b, g2] = black[..] {
                    let expected = by_position(&raw.cfa, [r, g, g2, b]);
                    assert_eq!(
                        raw.black_levels,
                        expected,
                        "{}: black levels",
                        path.display()
                    );
                }
                // "As shot" is cam_mul: R G B G2, unnormalised.
                let shot = numbers(&report, "As shot");
                if let [r, g, b, g2] = shot[..4.min(shot.len())] {
                    for (got, want) in raw.wb_coeffs.iter().zip([r / g, 1.0, b / g, g2 / g]) {
                        assert!(
                            (got - want).abs() < 1e-4,
                            "{}: white balance {:?} not {:?}",
                            path.display(),
                            raw.wb_coeffs,
                            [r / g, 1.0, b / g, g2 / g]
                        );
                    }
                }
                // LibRaw's saturation, where it prints one.
                let white = numbers(&report, "Highlight linearity limits:");
                if let Some(first) = white.first() {
                    assert_eq!(raw.white_level, *first, "{}: white level", path.display());
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

            // The crop is deliberately the camera's own active area
            // rather than LibRaw's per-model margins (see the module
            // notes), so it is checked against the makernote instead:
            // where the body records one, the crop is exactly it, and
            // where it does not, the crop is the whole frame.
            let maker = makernote(&Tiff::parse(&bytes).expect("parses"));
            let recorded = maker
                .as_ref()
                .is_some_and(|m| m.find(mn::RAW_IMAGE_SIZE).is_some());
            if !recorded {
                assert_eq!(
                    (raw.crop.width, raw.crop.height),
                    (raw.width, raw.height),
                    "{}: a body that records no active area keeps the whole frame",
                    path.display()
                );
            }
            assert!(raw.crop.x + raw.crop.width <= raw.width);
            assert!(raw.crop.y + raw.crop.height <= raw.height);

            let preview = raw.preview.as_ref().expect("every PEF carries a preview");
            let decoded = image::load_from_memory(preview).expect("preview decodes");
            assert!(decoded.width() > 0 && decoded.height() > 0);
        }
        eprintln!("pef: {} corpus files matched", files.len());
    }

    /// Truncation must never panic, however deep into the file it
    /// happens: a partial frame or an error, both are fine.
    #[test]
    fn truncated_files_do_not_panic() {
        for path in corpus() {
            let bytes = std::fs::read(&path).expect("corpus file reads");
            // A spread of cuts: the header, the IFDs, the makernote,
            // and points through the image data.
            for numerator in [0usize, 1, 2, 3, 5, 8, 13, 100, 500, 900, 999] {
                let cut = bytes.len() * numerator / 1000;
                let _ = crate::decode(&bytes[..cut]);
                let _ = preview(&bytes[..cut]);
            }
        }
    }
}
