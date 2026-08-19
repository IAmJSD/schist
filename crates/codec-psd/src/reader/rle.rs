//! PackBits (RLE) decompression, as used by PSD compression method 1.
//!
//! PSD RLE layout per channel: first a table of per-row compressed byte
//! counts (u16 each in PSD, u32 in PSB — one of the format's PSB length
//! widenings), then the PackBits-compressed rows back to back.

use super::cursor::Cursor;
use crate::error::PsdError;

/// Decode one PackBits-compressed row into exactly `out.len()` bytes.
///
/// PackBits packets: a header byte `n` as i8 —
/// * `0..=127`: copy the next `n + 1` literal bytes,
/// * `-127..=-1`: repeat the next byte `1 - n` times,
/// * `-128`: no-op (skip).
///
/// Errors if the packets overrun the expected row length or the source runs
/// dry before the row is complete (never panics).
pub fn unpack_row(src: &mut Cursor, out: &mut [u8]) -> Result<(), PsdError> {
    let mut filled = 0usize;
    while filled < out.len() {
        let n = src.u8()? as i8;
        match n {
            -128 => continue, // no-op filler byte
            0..=127 => {
                let count = n as usize + 1;
                if filled + count > out.len() {
                    return Err(PsdError::Corrupt("RLE literal packet overruns row".into()));
                }
                out[filled..filled + count].copy_from_slice(src.take(count)?);
                filled += count;
            }
            _ => {
                let count = (1 - n as isize) as usize; // 1 - n, n in -127..=-1 => 2..=128
                if filled + count > out.len() {
                    return Err(PsdError::Corrupt("RLE repeat packet overruns row".into()));
                }
                let value = src.u8()?;
                out[filled..filled + count].fill(value);
                filled += count;
            }
        }
    }
    Ok(())
}

/// Decode a whole RLE-compressed channel: `rows` rows of `row_bytes` each.
/// `cur` must be positioned at the per-row byte-count table.
pub fn unpack_channel(
    cur: &mut Cursor,
    rows: usize,
    row_bytes: usize,
    psb: bool,
) -> Result<Vec<u8>, PsdError> {
    let total = rows
        .checked_mul(row_bytes)
        .ok_or_else(|| PsdError::Corrupt("RLE channel size overflows".into()))?;
    let mut counts = Vec::with_capacity(rows.min(1 << 20));
    for _ in 0..rows {
        counts.push(cur.len_rle_row(psb)? as usize);
    }
    let mut out = vec![0u8; total];
    for (row, &count) in counts.iter().enumerate() {
        // Sub-cursor per row: a corrupt row can't consume its neighbors.
        let mut row_src = cur.sub(count)?;
        unpack_row(
            &mut row_src,
            &mut out[row * row_bytes..(row + 1) * row_bytes],
        )?;
        // Trailing pad bytes inside a row's declared count are legal; ignore.
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_then_repeat_packets() {
        // Literal packet of 3 bytes (header 2), then a repeat of 5 bytes
        // (header -4, value 9).
        let src = [2u8, 10, 20, 30, 0xFC, 9];
        let mut cur = Cursor::new(&src);
        let mut out = [0u8; 8];
        unpack_row(&mut cur, &mut out).unwrap();
        assert_eq!(out, [10, 20, 30, 9, 9, 9, 9, 9]);
    }

    #[test]
    fn noop_byte_is_skipped() {
        let src = [0x80u8, 0, 42]; // -128 no-op, then literal of 1
        let mut cur = Cursor::new(&src);
        let mut out = [0u8; 1];
        unpack_row(&mut cur, &mut out).unwrap();
        assert_eq!(out, [42]);
    }

    #[test]
    fn overrun_is_error_not_panic() {
        let src = [3u8, 1, 2, 3, 4]; // 4 literals into a 2-byte row
        let mut cur = Cursor::new(&src);
        let mut out = [0u8; 2];
        assert!(matches!(
            unpack_row(&mut cur, &mut out),
            Err(PsdError::Corrupt(_))
        ));
    }

    #[test]
    fn truncated_source_is_error() {
        let src = [7u8, 1, 2]; // promises 8 literals, has 2
        let mut cur = Cursor::new(&src);
        let mut out = [0u8; 8];
        assert!(matches!(
            unpack_row(&mut cur, &mut out),
            Err(PsdError::Truncated { .. })
        ));
    }

    #[test]
    fn channel_rows_decode_independently() {
        // Two rows of 4 bytes: row 0 = repeat 0xAA x4 (2 bytes compressed),
        // row 1 = literal 4 bytes (5 bytes compressed).
        let mut src = Vec::new();
        src.extend_from_slice(&2u16.to_be_bytes());
        src.extend_from_slice(&5u16.to_be_bytes());
        src.extend_from_slice(&[0xFD, 0xAA]); // -3 => repeat 4
        src.extend_from_slice(&[3, 1, 2, 3, 4]);
        let mut cur = Cursor::new(&src);
        let out = unpack_channel(&mut cur, 2, 4, false).unwrap();
        assert_eq!(out, [0xAA, 0xAA, 0xAA, 0xAA, 1, 2, 3, 4]);
    }
}
