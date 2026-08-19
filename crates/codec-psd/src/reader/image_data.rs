//! Merged (flattened) Image Data section — the last section of the file.
//!
//! Layout: u16 compression method, then *planar* channel data for the whole
//! canvas, all channels back to back in header channel order. For RLE the
//! per-row byte-count table covers ALL channels' rows up front
//! (`channels * height` entries), followed by the PackBits rows.

use super::cursor::Cursor;
use super::header::Header;
use super::rle::unpack_row;
use crate::error::PsdError;

/// Raw (decompressed, still big-endian) planes, one per header channel.
pub struct CompositePlanes {
    pub planes: Vec<Vec<u8>>,
}

/// Parse the merged image data. Returns `None` if the section is absent
/// (out-of-spec, but tolerated — layered files don't need it).
pub fn parse_image_data(
    cur: &mut Cursor,
    header: &Header,
) -> Result<Option<CompositePlanes>, PsdError> {
    if cur.remaining() < 2 {
        return Ok(None);
    }
    let comp = cur.u16()?;
    let channels = header.channels as usize;
    let rows = header.height as usize;
    let row_bytes = header.width as usize * header.depth.bytes_per_channel();
    let plane_bytes = rows
        .checked_mul(row_bytes)
        .ok_or_else(|| PsdError::Corrupt("composite size overflows".into()))?;

    let mut planes = Vec::with_capacity(channels);
    match comp {
        0 => {
            for _ in 0..channels {
                planes.push(cur.take(plane_bytes)?.to_vec());
            }
        }
        1 => {
            // Row-count table for every channel first (u16 entries in PSD,
            // u32 in PSB), then compressed rows channel-major.
            let mut counts = Vec::with_capacity((channels * rows).min(1 << 20));
            for _ in 0..channels * rows {
                counts.push(cur.len_rle_row(header.psb)? as usize);
            }
            // Guard corrupt headers: PackBits expands at most 64x.
            if plane_bytes
                .checked_mul(channels)
                .is_none_or(|t| t > cur.remaining() * 64)
            {
                return Err(PsdError::Corrupt(
                    "composite RLE data too short for canvas size".into(),
                ));
            }
            for ch in 0..channels {
                let mut plane = vec![0u8; plane_bytes];
                for row in 0..rows {
                    let count = counts[ch * rows + row];
                    let mut src = cur.sub(count)?;
                    unpack_row(&mut src, &mut plane[row * row_bytes..(row + 1) * row_bytes])?;
                }
                planes.push(plane);
            }
        }
        2 | 3 => {
            return Err(PsdError::Unsupported(format!(
                "zip-compressed merged image data (method {comp})"
            )))
        }
        c => {
            return Err(PsdError::Corrupt(format!(
                "unknown image data compression {c}"
            )))
        }
    }
    Ok(Some(CompositePlanes { planes }))
}
