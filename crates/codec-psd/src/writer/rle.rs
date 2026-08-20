//! PackBits (RLE) encoder — the inverse of `reader::rle`.
//!
//! Runs of 3+ equal bytes become `(257 - n)` + the byte; everything else is
//! emitted as literal runs `(n - 1)` + bytes. Both run kinds cap at 128
//! bytes, which is what the one-byte control code can express.

/// PackBits-encode one row.
pub fn pack_row(src: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(src.len() + src.len() / 128 + 2);
    let mut i = 0;
    while i < src.len() {
        // How long is the run of identical bytes starting here?
        let mut run = 1;
        while i + run < src.len() && src[i + run] == src[i] && run < 128 {
            run += 1;
        }
        if run >= 3 {
            out.push((257 - run) as u8);
            out.push(src[i]);
            i += run;
            continue;
        }
        // Literal run: consume until a 3-run starts or we hit the cap.
        let start = i;
        let mut lit = 0;
        while i < src.len() && lit < 128 {
            if i + 2 < src.len() && src[i] == src[i + 1] && src[i] == src[i + 2] {
                break;
            }
            i += 1;
            lit += 1;
        }
        out.push((lit - 1) as u8);
        out.extend_from_slice(&src[start..start + lit]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reader::rle::unpack_row_for_test;

    fn round_trip(data: &[u8]) {
        let packed = pack_row(data);
        let mut out = vec![0u8; data.len()];
        unpack_row_for_test(&packed, &mut out).expect("unpack");
        assert_eq!(out, data, "round trip failed for {data:?}");
    }

    #[test]
    fn encodes_runs() {
        let packed = pack_row(&[5, 5, 5, 5]);
        assert_eq!(packed, vec![253, 5]); // 257 - 4 = 253
        round_trip(&[5, 5, 5, 5]);
    }

    #[test]
    fn encodes_literals() {
        let packed = pack_row(&[1, 2, 3]);
        assert_eq!(packed, vec![2, 1, 2, 3]);
        round_trip(&[1, 2, 3]);
    }

    #[test]
    fn mixed_and_long_inputs_round_trip() {
        round_trip(&[]);
        round_trip(&[9]);
        round_trip(&[1, 1, 2, 3, 3, 3, 3, 4, 5, 5, 5]);
        round_trip(&vec![0u8; 300]);
        round_trip(&(0..=255u8).collect::<Vec<_>>());
        let mut pattern = Vec::new();
        for i in 0..1000u32 {
            pattern.push(if i % 7 < 4 { 0xAB } else { (i % 251) as u8 });
        }
        round_trip(&pattern);
    }

    #[test]
    fn never_exceeds_worst_case_growth() {
        // PackBits worst case is 1 extra control byte per 128 literals.
        let data: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
        let packed = pack_row(&data);
        assert!(packed.len() <= data.len() + data.len() / 128 + 1);
    }
}
