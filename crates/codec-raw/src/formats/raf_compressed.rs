//! Fujifilm's compressed sensor strips, from the X-T2 (2016) onwards.
//!
//! This module reads the compressed stream's *header* — which is
//! documented enough to check a file against its own container and to
//! say precisely what a file is — and refuses the entropy-coded body.
//! Nothing in the RAF container announces compression: the decision is
//! made in [`super::raf`] from the strip's byte count, and a strip that
//! is neither 16-bit words nor packed samples arrives here.
//!
//! # What the header says
//!
//! Sixteen big-endian bytes, then one 32-bit length per vertical
//! stripe, the table padded to a 16-byte boundary:
//!
//! ```text
//! 0   u16  0x4953, the signature
//! 2   u8   version: 0 on the Bayer bodies, 1 on the X-Trans ones
//! 3   u8   sensor: 0x10 X-Trans, 0x00 Bayer
//! 4   u8   bits a sample (12 or 14)
//! 5   u16  frame height
//! 7   u16  frame width rounded up to a whole number of stripes
//! 9   u16  frame width
//! 11  u16  stripe width in pixels (768 in every file seen)
//! 13  u8   stripes across the frame
//! 14  u16  six-row blocks down the frame (height / 6)
//! ```
//!
//! Every field is redundant with the container or with another field,
//! which is what makes the header worth parsing even without a
//! decoder: `rounded_width == stripes * stripe_width`,
//! `blocks * 6 == height`, and the width, height and depth must equal
//! the ones the RAF's own tags gave. A file that disagrees with itself
//! is corrupt, and saying so is more use to a caller than a blanket
//! "unsupported".
//!
//! # What is not implemented
//!
//! The body. Each stripe is decoded independently, six sensor rows at
//! a time, into twelve line buffers — three red, six green, three blue,
//! each `stripe_width * 2 / 3` samples for X-Trans (`/ 2` for Bayer),
//! with the previous block's last two lines of each colour kept as
//! history. A sample is predicted from its neighbours in the line
//! above (and, at odd positions, from the two already-decoded
//! neighbours beside it), the prediction error is classified into one
//! of 41 gradient contexts, and the error is Golomb-Rice coded with a
//! per-context adaptive parameter.
//!
//! The entropy layer is fully pinned down against real files, and is
//! recorded here so that whoever finishes this starts from the part
//! that is already known to be right:
//!
//! * A context is a pair `(sum, count)` starting at
//!   `(max(2, ((1 << bits) + 32) >> 6), 1)` — `(256, 1)` at 14 bits —
//!   and `k` is the smallest shift with `count << k >= sum`, capped at
//!   12. Before each update, `sum` and `count` are halved if `count`
//!   has reached 64; then `count` is incremented and `sum` grows by
//!   the magnitude of the difference just decoded.
//! * A symbol is a unary run of zero bits terminated by a one, then
//!   `k` more bits: `sample = (zeros << k) + rest`. When the run
//!   reaches `4 * bits - bits - 1` zeros (41 at 14 bits) the symbol
//!   escapes: the terminating one is still consumed, then `bits`
//!   literal bits follow and `sample = literal + 1`. Consuming that
//!   one bit is the whole difference between decoding a file and
//!   decoding noise.
//! * `sample` folds to a signed difference: even gives `sample / 2`,
//!   odd gives `-(sample + 1) / 2`.
//! * The gradient quantiser has thresholds 0, 0x12, 0x43, 0x114 and
//!   the saturation value, giving nine levels from two differences;
//!   the context is `9 * a + b`, folded to 41 by taking the magnitude
//!   and negating the difference when the index was negative.
//!
//! Two checks say this is right rather than merely plausible. Decoding
//! a stripe's leading run of identical samples reproduces the observed
//! code lengths exactly — one code of 9 bits, two of 8, four of 7,
//! eight of 6, sixteen of 5, thirty-two of 4, then thirty-two each of
//! 3, 2 and 1, for two contexts in step — and the first symbol after
//! that run, an escape, yields a difference of 1067 against a
//! prediction of zero, which is the value the oracle has for the first
//! non-blank pixel of the frame (the X100F sample's row 5, column 0).
//! Samples also arrive in groups of `stripe_width`, each group taking
//! the next of three gradient sets, so the sets cycle 0, 1, 2, 0, 1, 2
//! over a six-row block.
//!
//! What is *not* pinned down is the order the twelve line buffers'
//! even and odd positions are visited in, and which of their slots
//! hold sensor pixels rather than interpolated filler. The three
//! two-row passes of an X-Trans block are not alike — the array puts
//! two red pixels in the first pair of rows and three in each of the
//! others — so there is no single loop over the passes to infer, and
//! every uniform arrangement tried desynchronises the bit reader
//! within the first row. Rather than return a frame that looks decoded
//! and is not, this module refuses, and [`Header::parse`] still
//! extracts everything the stream says about itself.

use crate::{Cfa, Error, Result};

/// A compressed strip's header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub version: u8,
    /// 0x10 on the X-Trans bodies, 0 on the Bayer ones.
    pub sensor: u8,
    pub bits: u32,
    pub width: usize,
    pub height: usize,
    /// `width` rounded up to a whole number of stripes.
    pub rounded_width: usize,
    pub stripe_width: usize,
    pub stripes: usize,
    /// Six-row blocks down the frame.
    pub blocks: usize,
}

/// The signature every compressed strip starts with.
pub const SIGNATURE: u16 = 0x4953;

/// Rows of the sensor in one coded block, for both filter arrays.
pub const BLOCK_ROWS: usize = 6;

impl Header {
    /// Read and check a header. `Err(Corrupt)` when the strip claims a
    /// geometry that contradicts itself.
    pub fn parse(strip: &[u8]) -> Result<Header> {
        let be16 = |at: usize| -> Result<usize> {
            strip
                .get(at..at + 2)
                .and_then(|b| b.try_into().ok())
                .map(|b| u16::from_be_bytes(b) as usize)
                .ok_or_else(|| Error::Corrupt("truncated compressed strip header".into()))
        };
        let byte = |at: usize| -> Result<u8> {
            strip
                .get(at)
                .copied()
                .ok_or_else(|| Error::Corrupt("truncated compressed strip header".into()))
        };
        if be16(0)? != SIGNATURE as usize {
            return Err(Error::Corrupt(
                "compressed sensor strip without its 0x4953 signature".into(),
            ));
        }
        let header = Header {
            version: byte(2)?,
            sensor: byte(3)?,
            bits: byte(4)? as u32,
            height: be16(5)?,
            rounded_width: be16(7)?,
            width: be16(9)?,
            stripe_width: be16(11)?,
            stripes: byte(13)? as usize,
            blocks: be16(14)?,
        };
        // Four cross-checks the format makes free: every one of them
        // holds in every sample file, and a stream that fails one is
        // not a stream this crate could decode even with a decoder.
        if header.stripe_width == 0 || header.stripes == 0 {
            return Err(Error::Corrupt("compressed strip with no stripes".into()));
        }
        if header.stripes * header.stripe_width != header.rounded_width {
            return Err(Error::Corrupt(format!(
                "{} stripes of {} do not make the rounded width {}",
                header.stripes, header.stripe_width, header.rounded_width
            )));
        }
        if header.rounded_width < header.width
            || header.rounded_width - header.width >= header.stripe_width
        {
            return Err(Error::Corrupt(format!(
                "rounded width {} does not round {} up by less than a stripe",
                header.rounded_width, header.width
            )));
        }
        if header.blocks * BLOCK_ROWS != header.height {
            return Err(Error::Corrupt(format!(
                "{} six-row blocks do not make {} rows",
                header.blocks, header.height
            )));
        }
        Ok(header)
    }

    /// Where the entropy-coded data starts: the header, then a 32-bit
    /// length for each stripe, padded to a 16-byte boundary.
    pub fn data_offset(&self) -> usize {
        16 + (self.stripes * 4).div_ceil(16) * 16
    }

    /// Each stripe's compressed length, in order.
    pub fn stripe_lengths(&self, strip: &[u8]) -> Result<Vec<usize>> {
        let mut out = Vec::with_capacity(self.stripes);
        for i in 0..self.stripes {
            let at = 16 + i * 4;
            let bytes: [u8; 4] = strip
                .get(at..at + 4)
                .and_then(|b| b.try_into().ok())
                .ok_or_else(|| Error::Corrupt("truncated stripe length table".into()))?;
            out.push(u32::from_be_bytes(bytes) as usize);
        }
        Ok(out)
    }

    /// Whether the entropy-coded body is lossless.
    ///
    /// Nothing in the header says so directly, so this asks the only
    /// question a header can answer: how much room the body takes.
    /// Fujifilm's lossless mode spends between seven and nine bits a
    /// pixel on every sample in the corpus (X100F 8.51, X-Pro3 8.15,
    /// X-T5 7.64); the lossy mode quantises first and spends far less
    /// — the GFX100S sample takes 4.71, which no lossless coder gets
    /// from a 14-bit frame with ordinary photon noise. Six bits is
    /// comfortably between the two.
    pub fn looks_lossless(&self, body_bytes: usize) -> bool {
        let pixels = self.width as u64 * self.height as u64;
        pixels > 0 && (body_bytes as u64 * 8) / pixels >= 6
    }
}

/// Decode a compressed sensor strip into `width * height` samples.
///
/// Always `Err`: see the module documentation for what is known about
/// the coding and what is not.
pub fn decode(strip: &[u8], width: usize, height: usize, cfa: &Cfa) -> Result<Vec<u16>> {
    let header = Header::parse(strip)?;
    if header.width != width || header.height != height {
        return Err(Error::Corrupt(format!(
            "compressed strip is {}x{} inside a {width}x{height} frame",
            header.width, header.height
        )));
    }
    // The header names the filter array too, and disagreeing with the
    // container would mean one of the two was misread.
    let x_trans = matches!(cfa, Cfa::XTrans(_));
    if x_trans != (header.sensor == 0x10) {
        return Err(Error::Corrupt(format!(
            "compressed strip says sensor type {:#04x} for a {} frame",
            header.sensor,
            if x_trans { "X-Trans" } else { "Bayer" }
        )));
    }
    let body = strip.len().saturating_sub(header.data_offset());
    Err(Error::Unsupported(format!(
        "RAF: Fujifilm {} compression ({} {}-bit, {} stripes of {})",
        if header.looks_lossless(body) {
            "lossless"
        } else {
            "lossy"
        },
        if x_trans { "X-Trans" } else { "Bayer" },
        header.bits,
        header.stripes,
        header.stripe_width
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The X100F's real header, byte for byte.
    const X100F: [u8; 16] = [
        0x49, 0x53, 0x01, 0x10, 0x0e, 0x0f, 0xc6, 0x18, 0x00, 0x17, 0xa0, 0x03, 0x00, 0x08, 0x02,
        0xa1,
    ];

    #[test]
    fn reads_a_real_header() {
        let header = Header::parse(&X100F).unwrap();
        assert_eq!(
            header,
            Header {
                version: 1,
                sensor: 0x10,
                bits: 14,
                height: 4038,
                rounded_width: 6144,
                width: 6048,
                stripe_width: 768,
                stripes: 8,
                blocks: 673,
            }
        );
        // Eight lengths is 32 bytes, which is already a multiple of 16.
        assert_eq!(header.data_offset(), 48);
    }

    #[test]
    fn the_length_table_is_padded_to_sixteen_bytes() {
        // The X-Pro3's nine stripes: 36 bytes of table, padded to 48.
        let mut header = Header::parse(&X100F).unwrap();
        header.stripes = 9;
        assert_eq!(header.data_offset(), 64);
        header.stripes = 16;
        assert_eq!(header.data_offset(), 80);
    }

    #[test]
    fn rejects_a_header_that_contradicts_itself() {
        assert!(matches!(Header::parse(&X100F[..8]), Err(Error::Corrupt(_))));
        let mut bad = X100F;
        bad[0] = 0;
        assert!(matches!(Header::parse(&bad), Err(Error::Corrupt(_))));
        // Stripes that do not cover the rounded width.
        let mut bad = X100F;
        bad[13] = 7;
        assert!(matches!(Header::parse(&bad), Err(Error::Corrupt(_))));
        // A block count that does not make the height.
        let mut bad = X100F;
        bad[14..16].copy_from_slice(&100u16.to_be_bytes());
        assert!(matches!(Header::parse(&bad), Err(Error::Corrupt(_))));
        // A rounded width that rounds by more than a whole stripe.
        let mut bad = X100F;
        bad[9..11].copy_from_slice(&1000u16.to_be_bytes());
        assert!(matches!(Header::parse(&bad), Err(Error::Corrupt(_))));
    }

    #[test]
    fn the_body_size_tells_lossless_from_lossy() {
        let header = Header::parse(&X100F).unwrap();
        // The X100F's own body: 8.5 bits a pixel.
        assert!(header.looks_lossless(25_977_312));
        // The GFX100S's rate, scaled to this frame: 4.7 bits a pixel.
        assert!(!header.looks_lossless(6048 * 4038 * 47 / 80));
        assert!(!header.looks_lossless(0));
    }

    #[test]
    fn decoding_says_what_the_file_is() {
        let mut strip = X100F.to_vec();
        strip.resize(16 + 32 + 25_977_264, 0);
        let cfa = Cfa::XTrans([[crate::CfaColor::Green; 6]; 6]);
        let why = decode(&strip, 6048, 4038, &cfa).unwrap_err().to_string();
        assert!(why.contains("lossless"), "{why}");
        assert!(why.contains("X-Trans"), "{why}");
        // A Bayer container with an X-Trans header is a misread, not
        // an unsupported file.
        assert!(matches!(
            decode(&strip, 6048, 4038, &Cfa::RGGB),
            Err(Error::Corrupt(_))
        ));
        assert!(matches!(
            decode(&strip, 10, 10, &cfa),
            Err(Error::Corrupt(_))
        ));
    }
}
