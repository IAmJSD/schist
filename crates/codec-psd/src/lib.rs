//! PSD/PSB reader and writer for Schist.
//!
//! Scope: read the five PSD sections —
//! header, color mode data, image resources, layer & mask info, merged image
//! data — into a [`schist_core::Document`]. Everything we do not
//! interpret (unknown image resources, unknown additional-layer-info keys,
//! text engine data, smart objects, effects) is preserved verbatim on the
//! document / layers so the writer can round-trip it byte-for-byte.
//!
//! Supported: PSD (version 1) and PSB (version 2); 8/16/32-bit depth; RGB
//! and Grayscale color modes; raw and RLE (PackBits) channel compression;
//! groups (`lsct`), layer masks, unicode names (`luni`), adjustment layers.
//!
//! Deliberately deferred: zip/zip-with-prediction channel compression,
//! Bitmap/Indexed/CMYK/Lab/Duotone/Multichannel modes.

pub mod effects;
pub mod error;
mod reader;
pub mod vector;
mod writer;
pub mod zip;

/// Additional-layer-info keys whose length field widens to u64 in PSB files
/// (per the spec's "Photoshop Big" notes). All other keys stay u32 even in
/// PSB. Shared by the reader and the writer so the two cannot disagree
/// about which keys are widened.
pub(crate) const PSB_U64_KEYS: [[u8; 4]; 13] = [
    *b"LMsk", *b"Lr16", *b"Lr32", *b"Layr", *b"Mt16", *b"Mt32", *b"Mtrn", *b"Alph", *b"FMsk",
    *b"lnk2", *b"FEid", *b"FXid", *b"PxSD",
];

pub use error::PsdError;
pub use reader::read_psd;
pub use writer::{write_psd, write_psd_with, PSB_MAX_DIM, PSD_MAX_DIM};

/// Quick signature probe: does this buffer look like a PSD/PSB file?
///
/// Checks only the 4-byte `8BPS` magic; `read_psd` performs full validation.
pub fn is_psd(bytes: &[u8]) -> bool {
    bytes.len() >= 4 && &bytes[..4] == b"8BPS"
}

#[cfg(test)]
mod tests {
    use super::is_psd;

    #[test]
    fn is_psd_signature_check() {
        assert!(is_psd(b"8BPS\x00\x01rest"));
        assert!(is_psd(b"8BPS")); // exactly the magic is enough for a probe
        assert!(!is_psd(b"8BP"));
        assert!(!is_psd(b"9BPS\x00\x01"));
        assert!(!is_psd(b""));
    }
}
