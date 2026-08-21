//! PSD's zip-compressed channel data (compression methods 2 and 3).
//!
//! Method 2 is plain zlib. Method 3 adds *prediction*: before compressing,
//! each row is delta-encoded so that runs of similar values become runs of
//! near-zero bytes, which zlib then packs far better. Photoshop writes 16-
//! and 32-bit files this way by default, which is why files that opened
//! fine in Photoshop used to fail here.
//!
//! The three depths predict differently and the difference matters:
//! 8- and 16-bit delta whole samples along the row, while 32-bit first
//! *de-interleaves* the four bytes of each float into four byte-planes and
//! deltas those, because the exponent bytes of adjacent floats are far more
//! alike than the floats are.

use schist_color::Depth;

use crate::PsdError;

/// Inflate a zlib stream to exactly `expected` bytes.
fn inflate(data: &[u8], expected: usize) -> Result<Vec<u8>, PsdError> {
    let out = miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(data, expected.max(1))
        .map_err(|e| PsdError::Corrupt(format!("zip channel: {e:?}")))?;
    if out.len() < expected {
        return Err(PsdError::Corrupt(format!(
            "zip channel decompressed to {} bytes, expected {expected}",
            out.len()
        )));
    }
    Ok(out)
}

/// Decode one channel stored with compression method 2 or 3.
///
/// `row_bytes` is the width in bytes of one row for this depth.
pub fn decode_channel(
    data: &[u8],
    rows: usize,
    row_bytes: usize,
    depth: Depth,
    predicted: bool,
) -> Result<Vec<u8>, PsdError> {
    let expected = rows
        .checked_mul(row_bytes)
        .ok_or_else(|| PsdError::Corrupt("zip channel size overflows".into()))?;
    let mut out = inflate(data, expected)?;
    out.truncate(expected);
    if predicted {
        unpredict(&mut out, rows, row_bytes, depth);
    }
    Ok(out)
}

/// Encode one channel with compression method 2 or 3.
pub fn encode_channel(
    plane: &[u8],
    rows: usize,
    row_bytes: usize,
    depth: Depth,
    predicted: bool,
) -> Vec<u8> {
    let mut buf = plane.to_vec();
    if predicted {
        predict(&mut buf, rows, row_bytes, depth);
    }
    // Level 6: what zlib defaults to, and what Photoshop's files look like.
    miniz_oxide::deflate::compress_to_vec_zlib(&buf, 6)
}

/// Undo per-row delta encoding, in place.
fn unpredict(buf: &mut [u8], rows: usize, row_bytes: usize, depth: Depth) {
    for r in 0..rows {
        let row = &mut buf[r * row_bytes..(r + 1) * row_bytes];
        match depth {
            Depth::Eight => {
                for i in 1..row.len() {
                    row[i] = row[i].wrapping_add(row[i - 1]);
                }
            }
            Depth::Sixteen => {
                let n = row.len() / 2;
                let mut prev = u16::from_be_bytes([row[0], row[1]]);
                for i in 1..n {
                    let cur = u16::from_be_bytes([row[i * 2], row[i * 2 + 1]]);
                    prev = prev.wrapping_add(cur);
                    row[i * 2..i * 2 + 2].copy_from_slice(&prev.to_be_bytes());
                }
            }
            Depth::ThirtyTwo => {
                // Deltas first, on the flat byte-plane layout...
                for i in 1..row.len() {
                    row[i] = row[i].wrapping_add(row[i - 1]);
                }
                // ...then re-interleave the four byte-planes into floats.
                let n = row.len() / 4;
                let planar = row.to_vec();
                for i in 0..n {
                    for b in 0..4 {
                        row[i * 4 + b] = planar[b * n + i];
                    }
                }
            }
        }
    }
}

/// Apply per-row delta encoding, in place. The exact inverse of
/// [`unpredict`], which is what the round-trip test pins.
fn predict(buf: &mut [u8], rows: usize, row_bytes: usize, depth: Depth) {
    for r in 0..rows {
        let row = &mut buf[r * row_bytes..(r + 1) * row_bytes];
        match depth {
            Depth::Eight => {
                for i in (1..row.len()).rev() {
                    row[i] = row[i].wrapping_sub(row[i - 1]);
                }
            }
            Depth::Sixteen => {
                let n = row.len() / 2;
                for i in (1..n).rev() {
                    let cur = u16::from_be_bytes([row[i * 2], row[i * 2 + 1]]);
                    let prev = u16::from_be_bytes([row[(i - 1) * 2], row[(i - 1) * 2 + 1]]);
                    row[i * 2..i * 2 + 2].copy_from_slice(&cur.wrapping_sub(prev).to_be_bytes());
                }
            }
            Depth::ThirtyTwo => {
                let n = row.len() / 4;
                let interleaved = row.to_vec();
                for b in 0..4 {
                    for i in 0..n {
                        row[b * n + i] = interleaved[i * 4 + b];
                    }
                }
                for i in (1..row.len()).rev() {
                    row[i] = row[i].wrapping_sub(row[i - 1]);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(rows: usize, row_bytes: usize) -> Vec<u8> {
        // Smoothly varying data, which is what prediction is for.
        (0..rows * row_bytes)
            .map(|i| ((i * 7 / 3) % 251) as u8)
            .collect()
    }

    #[test]
    fn prediction_is_its_own_inverse_at_every_depth() {
        for depth in [Depth::Eight, Depth::Sixteen, Depth::ThirtyTwo] {
            let (rows, row_bytes) = (5usize, 32usize);
            let original = sample(rows, row_bytes);
            let mut buf = original.clone();
            predict(&mut buf, rows, row_bytes, depth);
            assert_ne!(buf, original, "{depth:?}: prediction did nothing");
            unpredict(&mut buf, rows, row_bytes, depth);
            assert_eq!(buf, original, "{depth:?}: prediction is not reversible");
        }
    }

    #[test]
    fn channels_round_trip_with_and_without_prediction() {
        for depth in [Depth::Eight, Depth::Sixteen, Depth::ThirtyTwo] {
            for predicted in [false, true] {
                let (rows, row_bytes) = (7usize, 64usize);
                let original = sample(rows, row_bytes);
                let packed = encode_channel(&original, rows, row_bytes, depth, predicted);
                let back =
                    decode_channel(&packed, rows, row_bytes, depth, predicted).expect("decodes");
                assert_eq!(back, original, "{depth:?} predicted={predicted}");
            }
        }
    }

    #[test]
    fn prediction_makes_smooth_data_compress_better() {
        // The whole point of method 3 over method 2.
        let (rows, row_bytes) = (64usize, 256usize);
        let data: Vec<u8> = (0..rows * row_bytes).map(|i| (i / 4) as u8).collect();
        let plain = encode_channel(&data, rows, row_bytes, Depth::Eight, false);
        let predicted = encode_channel(&data, rows, row_bytes, Depth::Eight, true);
        assert!(
            predicted.len() < plain.len(),
            "prediction did not help: {} vs {}",
            predicted.len(),
            plain.len()
        );
    }

    #[test]
    fn corrupt_input_errors_rather_than_panicking() {
        assert!(decode_channel(&[], 4, 16, Depth::Eight, false).is_err());
        assert!(decode_channel(&[1, 2, 3, 4], 4, 16, Depth::Eight, true).is_err());
        // A valid stream that is too short for the declared size.
        let short = encode_channel(&[0u8; 16], 1, 16, Depth::Eight, false);
        assert!(decode_channel(&short, 4, 16, Depth::Eight, false).is_err());
    }
}
