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
//!  * **5** — "IIQ S": format 3's bit stream exactly, carrying an
//!    eight-bit companded signal that a square law expands back to
//!    fourteen bits. See [`COMPANDING_DIVISOR`].
//!  * **6** — the other "IIQ S", and a different codec altogether:
//!    eight-pixel blocks, each with a per-parity range code and a
//!    per-block precision, over two parity predictors. See
//!    [`decode_row_format6`].
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
    /// A global black offset. Only the white level is derived from it.
    pub const T_BLACK: u32 = 0x021D;
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
fn decode_row(bytes: &[u8], out: &mut [u16], expand: Option<&[u16; 256]>) {
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
        let value = pred[parity].clamp(0, 0x3FFF);
        // Format 5 rides on this stream but carries an eight-bit
        // signal: everything below 256 is a companded code and comes
        // back through the square law, and the handful of samples
        // above it are highlights the predictor ran past.
        let value = match expand {
            Some(table) if value < 256 => table[value as usize] as i32,
            _ => value,
        };
        // 14-bit samples, shifted to share the 16-bit raster's scale.
        *sample = (value as u16) << 2;
    }
}

/// IIQ L and format 5: one independent bit stream a row, found
/// through the row-offset table. `expand` is the format-5 companding
/// table, or `None` for format 3.
fn decompress(
    data: &[u8],
    offsets: &[u8],
    width: usize,
    height: usize,
    expand: Option<&[u16; 256]>,
) -> Result<Vec<u16>> {
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
                decode_row(bits, line, expand);
            }
        });
    Ok(out)
}

// ------------------------------------------------------------ format 5

/// The divisor of format 5's square companding law.
///
/// A stored sample `v` under 256 stands for `round(v * v / 3.969)`.
/// The constant is chosen so that `v` = 255 lands exactly on 16383:
/// the whole eight-bit stored range maps onto the whole fourteen-bit
/// sample range with fine steps in the shadows and coarse ones in the
/// highlights. That is the entire lossy part of format 5 — the bit
/// stream itself is format 3's, and lossless.
const COMPANDING_DIVISOR: f32 = 3.969;

/// The 256-entry expansion of [`COMPANDING_DIVISOR`], built in 32-bit
/// float because that is the arithmetic the constant was fitted in.
fn companding_table() -> [u16; 256] {
    std::array::from_fn(|v| {
        let v = v as f32;
        (v * v / COMPANDING_DIVISOR + 0.5) as u16
    })
}

// ------------------------------------------------------------ format 6

/// Pixels to a block of format 6.
const BLOCK6: usize = 8;
/// The range code that means "every pixel of this parity in this block
/// is a raw fourteen-bit absolute value".
const ESCAPE6: i32 = 9;
/// Bits of an absolute format-6 sample, and of the tail pixels.
const ABSOLUTE6: u32 = 14;
/// The reference's per-row read buffer, and so the most bytes of a row
/// any decoder of this format will look at.
fn max_row_bytes(width: usize) -> usize {
    width * 3 + 2
}

/// Where each row's bytes are, derived from the row-offset table.
///
/// The table is not sorted. On a back that reads its two sensor halves
/// out in opposite directions — the iXU180 rises to `split_row` and
/// then falls — a decoder that took "the next entry minus this one" as
/// the row's length would get negative numbers for half the frame.
/// Sorting the offsets and taking the gap to the next one in *file*
/// order is what actually bounds a row.
fn row_extents(offsets: &[u8], data_len: usize, height: usize) -> Result<Vec<(usize, usize)>> {
    if offsets.len() < height * 4 {
        return Err(Error::Corrupt(format!(
            "Phase One row table holds {} bytes for {height} rows",
            offsets.len()
        )));
    }
    let starts: Vec<usize> = offsets[..height * 4]
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| u32::from_le_bytes(*b) as usize)
        .collect();
    let mut order: Vec<usize> = (0..height).collect();
    order.sort_by_key(|row| starts[*row]);
    let mut out = vec![(0usize, 0usize); height];
    for (i, row) in order.iter().enumerate() {
        let start = starts[*row].min(data_len);
        let next = order
            .get(i + 1)
            .map_or(data_len, |r| starts[*r])
            .min(data_len);
        out[*row] = (start, next.saturating_sub(start));
    }
    Ok(out)
}

/// The variable-length code that follows a `00` range prefix, giving
/// the parity's range outright rather than a step. Four to five bits,
/// and the values are in no useful order — it is a hand-built code,
/// not a canonical one.
fn absolute_range(pump: &mut impl BitPump) -> i32 {
    let peek = pump.peek(5);
    let (value, length) = if peek & 0b1_0000 != 0 {
        // 10 -> 3, 11 -> 2
        (if peek & 0b0_1000 != 0 { 2 } else { 3 }, 2)
    } else if peek & 0b0_1000 != 0 {
        // 010 -> 1, 011 -> 4
        (if peek & 0b0_0100 != 0 { 4 } else { 1 }, 3)
    } else if peek & 0b0_0100 != 0 {
        // 0010 -> 6, 0011 -> 5
        (if peek & 0b0_0010 != 0 { 5 } else { 6 }, 4)
    } else {
        // 00000 -> 9, 00001 -> 8, 00010 -> 0, 00011 -> 7
        (
            match peek & 0b11 {
                0 => 9,
                1 => 8,
                2 => 0,
                _ => 7,
            },
            5,
        )
    };
    pump.consume(length);
    value
}

/// One row of a format-6 stream.
///
/// A row opens with sixteen bits of header, of which only the low
/// three are used: they give `init_bits`, the number of bits a
/// difference is transmitted in before any per-block extension.
///
/// The row is then blocks of eight pixels. Each block carries a range
/// code for each column parity — a two-bit step against the previous
/// block's range, or an escape into an explicit value — and one
/// precision extension shared by both parities. Together those say how
/// many bits a difference is sent in (`take`), how many low bits of it
/// were thrown away (`shift`, the lossy step) and what to subtract to
/// re-centre it on zero (`bias`). A range of nine is the escape: every
/// pixel of that parity is a plain fourteen-bit sample instead.
///
/// Whatever is left after the last whole block — four pixels on the
/// iXU180 — is a run of plain fourteen-bit samples that update nothing.
fn decode_row_format6(bytes: &[u8], out: &mut [u16]) {
    let width = out.len();
    if width < BLOCK6 {
        return;
    }
    let mut pump = BitPumpMsb32::new(bytes);
    let init_bits = (pump.get(16) & 7) as i32;
    let blocks = ((width - BLOCK6) >> 3) + 1;
    // Both predictors and both ranges run the length of the row and
    // reset at its start.
    let mut previous = [0i32; 2];
    let mut range = [0i32; 2];
    let store = |sample: &mut u16, value: i32| {
        *sample = ((value as i64) << 2).clamp(0, u16::MAX as i64) as u16;
    };
    for block in 0..blocks {
        for side in range.iter_mut() {
            match pump.get(2) {
                0b01 => *side -= 1,
                0b10 => {}
                0b11 => *side += 1,
                _ => *side = absolute_range(&mut pump),
            }
        }
        // The relative steps are unbounded — nothing stops a row from
        // walking its range past 9 or below 0 — so only the
        // arithmetic below is bounded, and a nonsensical range costs
        // that row its samples rather than the frame.
        let extra = if pump.get(1) == 1 {
            0
        } else {
            1 + pump.get(2) as i32
        };
        // At most 7 + 4 bits: `init_bits` is three bits wide and the
        // extension reaches four.
        let take = (init_bits + extra) as u32;
        // A range narrower than the precision extension asks for a
        // negative quantiser, which cannot be meant. It happens on the
        // IQ140 samples, always with a transmitted value of zero, so
        // what it should do is unobservable; treating it as no shift
        // at all is what those frames confirm.
        let shift: [u32; 2] =
            std::array::from_fn(|p| range[p].saturating_sub(extra).clamp(0, 24) as u32);
        let bias: [i32; 2] =
            std::array::from_fn(|p| (1i32 << (init_bits + range[p] - 1).clamp(0, 30)) - 1);
        for i in 0..BLOCK6 {
            let side = i & 1;
            let value = if range[side] == ESCAPE6 {
                pump.get(ABSOLUTE6) as i32
            } else {
                previous[side]
                    .wrapping_add((pump.get(take) as i32) << shift[side])
                    .wrapping_sub(bias[side])
            };
            // The predictor keeps the value unshifted and unclamped;
            // only what goes into the frame is scaled and clipped.
            previous[side] = value;
            if let Some(sample) = out.get_mut(block * BLOCK6 + i) {
                store(sample, value);
            }
        }
    }
    for sample in out[blocks * BLOCK6..].iter_mut() {
        let value = pump.get(ABSOLUTE6) as i32;
        store(sample, value);
    }
}

/// Format 6: one independent bit stream a row, bounded by the sorted
/// row-offset table.
fn decompress_format6(
    data: &[u8],
    offsets: &[u8],
    width: usize,
    height: usize,
) -> Result<Vec<u16>> {
    let samples = crate::frame_samples(width, height, 1)?;
    let extents = row_extents(offsets, data.len(), height)?;
    let cap = max_row_bytes(width);
    let mut out = vec![0u16; samples];
    out.par_chunks_exact_mut(width)
        .zip(extents.par_iter())
        .for_each(|(line, (start, len))| {
            // A row longer than the reference's own read buffer is
            // read only as far as that buffer goes, which is what
            // keeps a mis-sorted table from reading a whole frame.
            if let Some(bits) = data.get(*start..*start + (*len).min(cap)) {
                decode_row_format6(bits, line);
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
    // Every compressed format carries 14-bit samples shifted up by
    // two, so the largest one a row can hold is 0xFFFC; tag 0x021D is
    // a global offset already inside them. (The reference leaves
    // format 6 at a flat 0xFFFF, which looks like an oversight rather
    // than a difference between the codecs, and nothing in the
    // unpacked frame depends on it.)
    let compressed_white = 65532.0 - dir.int(p1::T_BLACK).unwrap_or(0) as f32;
    let (samples, white) = match format {
        0 => (unpack(data, width, height)?, 65535.0),
        3 | 5 | 6 => {
            let offsets = dir.blob(p1::ROW_OFFSETS).ok_or_else(|| {
                Error::Corrupt(format!(
                    "Phase One format {format} with no row-offset table"
                ))
            })?;
            let samples = match format {
                6 => decompress_format6(data, offsets, width, height)?,
                5 => {
                    let table = companding_table();
                    decompress(data, offsets, width, height, Some(&table))?
                }
                _ => decompress(data, offsets, width, height, None)?,
            };
            (samples, compressed_white)
        }
        other => return Err(Error::Unsupported(format!("Phase One compression {other}"))),
    };
    if white <= 0.0 {
        return Err(Error::Corrupt(format!(
            "Phase One black offset {} leaves no range",
            dir.int(p1::T_BLACK).unwrap_or(0)
        )));
    }

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
    /// Empty: every Phase One compression in the corpus decodes.
    const UNSUPPORTED: &[(&str, &str)] = &[];

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
        decode_row(&bytes, &mut out, None);
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
        decode_row(&bytes, &mut out, None);
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
        decode_row(&bytes, &mut out, None);
        assert_eq!(&out[8..], &[4000 << 2, 4001 << 2]);
    }

    #[test]
    fn a_short_row_table_is_corrupt() {
        assert!(matches!(
            decompress(&[0; 64], &[0; 4], 8, 4, None),
            Err(Error::Corrupt(_))
        ));
    }

    #[test]
    fn rows_pointing_outside_the_data_stay_black() {
        // Two rows, the second addressed past the end.
        let mut offsets = Vec::new();
        offsets.extend_from_slice(&0u32.to_le_bytes());
        offsets.extend_from_slice(&u32::MAX.to_le_bytes());
        let out = decompress(&[0; 64], &offsets, 8, 2, None).unwrap();
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

    // ------------------------------------------------- formats 5 and 6

    /// A corpus file by name.
    fn sample(name: &str) -> Option<Vec<u8>> {
        let path = corpus::files(&["iiq", "tif"])
            .into_iter()
            .find(|p| corpus::name(p) == name)?;
        std::fs::read(path).ok()
    }

    /// The sensor data and the row-offset table of a corpus file.
    fn sensor(bytes: &[u8]) -> (&[u8], &[u8], usize, usize) {
        let dir = Directory::parse(bytes).expect("Phase One directory");
        let raw = dir.entry(p1::RAW_DATA).expect("sensor data");
        let start = raw.value as usize + BASE;
        let data = &bytes[start..start + raw.length as usize];
        let offsets = dir.blob(p1::ROW_OFFSETS).expect("row table");
        let width = dir.int(p1::RAW_WIDTH).unwrap_or(0) as usize;
        let height = dir.int(p1::RAW_HEIGHT).unwrap_or(0) as usize;
        (data, offsets, width, height)
    }

    /// The square companding law, against the table it must produce.
    ///
    /// The divisor is an empirical constant whose only justification
    /// is that it lands `expand[255]` exactly on 16383, so the table
    /// is the specification and the formula only a way of writing it.
    #[test]
    fn the_companding_law_reproduces_its_table() {
        const EXPAND: [u16; 256] = [
            0, 0, 1, 2, 4, 6, 9, 12, 16, 20, 25, 30, 36, 43, 49, 57, 64, 73, 82, 91, 101, 111, 122,
            133, 145, 157, 170, 184, 198, 212, 227, 242, 258, 274, 291, 309, 327, 345, 364, 383,
            403, 424, 444, 466, 488, 510, 533, 557, 580, 605, 630, 655, 681, 708, 735, 762, 790,
            819, 848, 877, 907, 938, 969, 1000, 1032, 1064, 1098, 1131, 1165, 1200, 1235, 1270,
            1306, 1343, 1380, 1417, 1455, 1494, 1533, 1572, 1612, 1653, 1694, 1736, 1778, 1820,
            1863, 1907, 1951, 1996, 2041, 2086, 2133, 2179, 2226, 2274, 2322, 2371, 2420, 2469,
            2520, 2570, 2621, 2673, 2725, 2778, 2831, 2885, 2939, 2993, 3049, 3104, 3160, 3217,
            3274, 3332, 3390, 3449, 3508, 3568, 3628, 3689, 3750, 3812, 3874, 3937, 4000, 4064,
            4128, 4193, 4258, 4324, 4390, 4457, 4524, 4592, 4660, 4729, 4798, 4868, 4938, 5009,
            5080, 5152, 5224, 5297, 5371, 5444, 5519, 5594, 5669, 5745, 5821, 5898, 5975, 6053,
            6132, 6210, 6290, 6370, 6450, 6531, 6612, 6694, 6777, 6859, 6943, 7027, 7111, 7196,
            7281, 7367, 7454, 7541, 7628, 7716, 7804, 7893, 7983, 8073, 8163, 8254, 8346, 8438,
            8530, 8623, 8717, 8811, 8905, 9000, 9095, 9191, 9288, 9385, 9482, 9580, 9679, 9778,
            9878, 9978, 10078, 10179, 10281, 10383, 10485, 10588, 10692, 10796, 10900, 11006,
            11111, 11217, 11324, 11431, 11538, 11647, 11755, 11864, 11974, 12084, 12195, 12306,
            12417, 12529, 12642, 12755, 12869, 12983, 13098, 13213, 13328, 13444, 13561, 13678,
            13796, 13914, 14033, 14152, 14272, 14392, 14512, 14634, 14755, 14878, 15000, 15123,
            15247, 15371, 15496, 15621, 15747, 15873, 16000, 16127, 16255, 16383,
        ];
        assert_eq!(companding_table(), EXPAND);
        // The two ends are what fix the constant.
        assert_eq!(EXPAND[0], 0);
        assert_eq!(EXPAND[255], 16383);
    }

    /// Row lengths come from sorting the offsets, not from the row
    /// order: a back that reads its two sensor halves out in opposite
    /// directions writes a table that falls in the middle.
    #[test]
    fn row_lengths_come_from_the_sorted_offsets() {
        // Four rows written 0, 300, 200, 100: rows 1..3 descend.
        let mut table = Vec::new();
        for offset in [0u32, 300, 200, 100] {
            table.extend_from_slice(&offset.to_le_bytes());
        }
        let extents = row_extents(&table, 400, 4).unwrap();
        assert_eq!(extents, vec![(0, 100), (300, 100), (200, 100), (100, 100)]);
        // A table too short for the frame is corrupt.
        assert!(row_extents(&table, 400, 5).is_err());
    }

    /// The absolute form of a format-6 range code: a hand-built
    /// prefix code of four to five bits, in no useful order.
    #[test]
    fn the_absolute_range_code_decodes_its_ten_values() {
        for (bits, want) in [
            ("10", 3),
            ("11", 2),
            ("010", 1),
            ("011", 4),
            ("0010", 6),
            ("0011", 5),
            ("00000", 9),
            ("00001", 8),
            ("00010", 0),
            ("00011", 7),
        ] {
            // The code, left-aligned in a 32-bit little-endian word,
            // padded with ones so a decoder that consumed too few or
            // too many bits would not accidentally agree.
            let mut word = u32::from_str_radix(bits, 2).unwrap() << (32 - bits.len());
            word |= u32::MAX >> bits.len();
            let stream = word.to_le_bytes();
            let mut pump = BitPumpMsb32::new(&stream);
            assert_eq!(absolute_range(&mut pump), want, "code {bits}");
            assert_eq!(pump.position(), bits.len(), "code {bits} length");
        }
    }

    /// The iXU180: format 6, with a row table that reverses at
    /// `split_row`, escape blocks, both parities carrying independent
    /// ranges, and a four-pixel tail.
    #[test]
    fn the_ixu180_decodes_its_first_rows_sample_for_sample() {
        let Some(bytes) = sample("iXU180-cap_22908.IIQ") else {
            return;
        };
        let (data, offsets, width, height) = sensor(&bytes);
        assert_eq!((width, height), (10380, 7816));
        let extents = row_extents(offsets, data.len(), height).unwrap();
        // The table's own order rises to row 3887 and then falls, and
        // the two halves are *interleaved* in the file: sorted by
        // offset the rows run 0, 7775, 1, 7774, 2, ... So a row's
        // bytes end where the next offset in file order begins, not
        // where the next row's offset does.
        assert_eq!(
            extents[..5]
                .iter()
                .map(|(start, _)| *start)
                .collect::<Vec<_>>(),
            vec![0, 9736, 19484, 29244, 38972]
        );
        assert_eq!(
            extents[..5].iter().map(|(_, len)| *len).collect::<Vec<_>>(),
            vec![4884, 4888, 4892, 4872, 4880]
        );
        let lengths: Vec<usize> = extents.iter().map(|(_, l)| *l).collect();
        assert_eq!(lengths.iter().min(), Some(&4844));
        assert_eq!(lengths.iter().max(), Some(&11800));
        // Every row fits the reference's own read buffer, so nothing
        // here is being silently truncated.
        assert!(lengths.iter().all(|l| *l <= max_row_bytes(width)));
        // Row 0 begins at the sensor data itself, and its header's
        // low three bits are `init_bits`.
        assert_eq!(extents[0].0, 0);
        assert_eq!(&data[..4], &[0x01, 0x00, 0x03, 0x00]);

        let mut row = vec![0u16; width];
        decode_row_format6(&data[..extents[0].1], &mut row);
        // Block 0: both ranges read the absolute form of 9, the
        // escape, so all eight pixels are plain fourteen-bit values.
        assert_eq!(
            &row[..8],
            &[1040, 1052, 1076, 1072, 33056, 33408, 33248, 33216]
        );
        // Block 1: both ranges 5, extra 2, so take 5, shift 3 and
        // bias 127 for both parities.
        let stored = |v: i32| (v << 2) as u16;
        assert_eq!(
            &row[8..16],
            &[8281, 8273, 8306, 8378, 8219, 8275, 8300, 8212].map(stored)
        );
        // Block 2: the two parities' ranges differ (8 and 6), so they
        // carry different shifts and biases over one shared `take`.
        assert_eq!(
            &row[16..24],
            &[8333, 8149, 8174, 8358, 8687, 8127, 8144, 8224].map(stored)
        );

        // The *second* row in file order, at offset 4884. It is frame
        // row 7775, not row 1: the second half of the sensor is
        // written into the gaps between the first half's rows.
        let (start, len) = extents[7775];
        assert_eq!((start, len), (4884, 4852));
        let mut row = vec![0u16; width];
        decode_row_format6(&data[start..start + len], &mut row);
        assert_eq!(
            &row[..8],
            &[268, 253, 275, 264, 8328, 8088, 8264, 8232].map(stored)
        );
        assert_eq!(
            &row[8..16],
            &[8265, 8297, 8218, 8202, 8243, 8155, 8124, 8180].map(stored)
        );
        assert_eq!(&row[16..18], &[8317, 8229].map(stored));
        // And the tail: the last four columns of a 10380-wide row are
        // outside the 1297 whole blocks and are plain 14-bit samples.
        assert_eq!(((width - BLOCK6) >> 3) + 1, 1297);
        assert!(row[width - 4..].iter().all(|s| s.is_multiple_of(4)));
    }

    /// The H 25: format 5, whose stream is format 3's exactly and
    /// whose samples are eight-bit companded codes.
    #[test]
    fn the_h25_companded_row_matches_its_stored_codes() {
        let Some(bytes) = sample("H_25-H25_Outdoor_.IIQ") else {
            return;
        };
        let (data, offsets, width, height) = sensor(&bytes);
        assert_eq!((width, height), (4134, 5488));
        let extents = row_extents(offsets, data.len(), height).unwrap();
        assert_eq!(extents[0].0, 0);
        assert_eq!(extents[1].0, 2732);

        // The same row read twice: once as format 3 would, giving the
        // stored eight-bit codes, and once as format 5 does.
        let mut stored = vec![0u16; width];
        decode_row(&data[extents[0].0..], &mut stored, None);
        assert_eq!(
            stored[..16].iter().map(|s| s >> 2).collect::<Vec<_>>(),
            vec![57, 57, 66, 57, 57, 57, 65, 64, 65, 65, 65, 64, 64, 64, 64, 64]
        );
        let table = companding_table();
        let mut expanded = vec![0u16; width];
        decode_row(&data[extents[0].0..], &mut expanded, Some(&table));
        assert_eq!(
            expanded[..16].iter().map(|s| s >> 2).collect::<Vec<_>>(),
            vec![
                819, 819, 1098, 819, 819, 819, 1064, 1032, 1064, 1064, 1064, 1032, 1032, 1032,
                1032, 1032
            ]
        );
        let mut second = vec![0u16; width];
        decode_row(&data[extents[1].0..], &mut second, Some(&table));
        assert_eq!(
            second[..16].iter().map(|s| s >> 2).collect::<Vec<_>>(),
            vec![819, 819, 877, 790, 819, 819, 848, 848, 848, 848, 848, 848, 848, 819, 819, 848]
        );
    }

    /// The P25+ pair: the same back and the same geometry, one file
    /// format 5 and one format 3, so the companding is the only
    /// difference between them. The format-3 file's row 0 is well
    /// above 256 and must come through untouched.
    #[test]
    fn the_p25_pair_differs_only_by_the_companding() {
        let (Some(lossy), Some(lossless)) =
            (sample("P25+-CF028662.IIQ"), sample("P25+-CF028663.IIQ"))
        else {
            return;
        };
        let table = companding_table();
        let (data, offsets, width, height) = sensor(&lossy);
        let extents = row_extents(offsets, data.len(), height).unwrap();
        let mut stored = vec![0u16; width];
        decode_row(&data[extents[0].0..], &mut stored, None);
        assert_eq!(
            stored[..16].iter().map(|s| s >> 2).collect::<Vec<_>>(),
            vec![31, 31, 181, 181, 181, 181, 181, 181, 181, 181, 195, 197, 192, 196, 199, 196]
        );
        let mut row = vec![0u16; width];
        decode_row(&data[extents[0].0..], &mut row, Some(&table));
        assert_eq!(
            row[..16].iter().map(|s| s >> 2).collect::<Vec<_>>(),
            vec![
                242, 242, 8254, 8254, 8254, 8254, 8254, 8254, 8254, 8254, 9580, 9778, 9288, 9679,
                9978, 9679
            ]
        );

        let (data, offsets, width, height) = sensor(&lossless);
        let extents = row_extents(offsets, data.len(), height).unwrap();
        let mut row = vec![0u16; width];
        decode_row(&data[extents[0].0..], &mut row, None);
        assert_eq!(
            row[..16].iter().map(|s| s >> 2).collect::<Vec<_>>(),
            vec![
                256, 256, 8192, 8192, 8192, 8192, 8192, 8192, 8192, 8192, 10720, 10496, 10368,
                10912, 10752, 10464
            ]
        );
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
