//! GoPro VC-5: the wavelet codec inside GPR files.
//!
//! A GPR is an ordinary DNG whose sensor IFD carries `Compression = 9`
//! and whose single tile is a VC-5 elementary bitstream (SMPTE ST 2073,
//! GoPro's "GPR" profile of it). Everything else about the file — black
//! and white levels, the CFA layout, colour matrices, the active area —
//! is plain DNG and is read by [`super::dng`]; this module only turns
//! one tile of VC-5 into the `TileWidth x TileLength` block of RGGB
//! samples that the DNG path then treats like any uncompressed tile.
//!
//! The shape of the codec, from the outside in:
//!
//! * The 2x2 Bayer mosaic is de-interleaved *before* compression into
//!   four half-resolution planes, and those planes are a reversible
//!   colour-difference transform of the quad rather than the four raw
//!   CFA colours: green sum, red-minus-green, blue-minus-green and the
//!   difference of the two greens (§7).
//! * Each plane is compressed independently by a three-level 2-D
//!   wavelet, so ten subbands per channel: one coarsest lowpass and
//!   three highpass bands per level.
//! * The lowpass band is stored as plain fixed-width integers; every
//!   highpass band is run/value entropy coded against one fixed
//!   codebook, then companded and quantised.
//! * Reconstruction runs the inverse 2/6 wavelet back up the levels and
//!   finally undoes the colour-difference transform and the camera's
//!   log curve, which is where the 12-bit working precision becomes the
//!   14-bit sensor codes the DNG advertises.
//!
//! Written from a functional specification of the bitstream plus
//! observation of the corpus files; no reference decoder source was
//! consulted.

use rayon::prelude::*;

use crate::bits::{BitPump, BitPumpMsb};
use crate::{frame_samples, Error, Result};

/// The tag dictionary. VC-5 structures its header as a stream of
/// 4-byte tag-value tuples; these are the mandatory (positive) tags a
/// GoPro Bayer stream uses. Tags this decoder does not know are
/// harmless state updates and are ignored.
mod tag {
    pub const CHANNEL_COUNT: u16 = 12;
    pub const SUBBAND_COUNT: u16 = 14;
    pub const IMAGE_WIDTH: u16 = 20;
    pub const IMAGE_HEIGHT: u16 = 21;
    pub const LOWPASS_PRECISION: u16 = 35;
    pub const SUBBAND_NUMBER: u16 = 48;
    pub const QUANTIZATION: u16 = 53;
    pub const CHANNEL_NUMBER: u16 = 62;
    pub const IMAGE_FORMAT: u16 = 84;
    pub const MAX_BITS_PER_COMPONENT: u16 = 102;
    pub const PATTERN_WIDTH: u16 = 106;
    pub const PATTERN_HEIGHT: u16 = 107;
    pub const COMPONENTS_PER_SAMPLE: u16 = 108;
    pub const PRESCALE_SHIFT: u16 = 109;
    /// Multi-layer samples (GoPro has never shipped one) carry these.
    pub const LAYER_COUNT: u16 = 120;
}

/// Chunk tags that would change the pixels but are not implemented,
/// because nothing in the GoPro corpus carries them: an explicit
/// inverse component permutation and an explicit inverse component
/// transform, which if present override the fixed Bayer scheme of §7.
mod chunk {
    pub const INVERSE_PERMUTATION: u16 = 0xC001;
    pub const INVERSE_TRANSFORM: u16 = 0xC002;
}

/// `ImageFormat` 4 is the Bayer arrangement; nothing else has ever been
/// seen in a GPR and the component transform below assumes it.
const IMAGE_FORMAT_BAYER: u16 = 4;

/// Four colour-difference planes per frame, ten subbands per plane.
const CHANNELS: usize = 4;
const SUBBANDS: usize = 10;
/// Three wavelet levels, each contributing three highpass subbands
/// above the single coarsest lowpass.
const LEVELS: usize = 3;

/// The working precision of a component plane after the inverse
/// transform, before the log curve: `MaxBitsPerComponent = 12`.
const COMPONENT_MAX: i32 = 4095;
/// The bias the three difference channels are stored with, so they can
/// be carried as unsigned values.
const MIDPOINT: i32 = 2048;
/// Reconstructed wavelet samples are clamped to the unsigned working
/// range of the transform.
const TRANSFORM_MAX: i32 = 16383;

/// (bit-length, codeword bits MSB-first, run, magnitude, kind:0=mag 1=zero-run 2=band-end)
const CODEBOOK: [(u8, u32, u16, u16, u8); 264] = [
    (1, 0x0000000, 1, 0, 0),
    (2, 0x0000002, 1, 1, 0),
    (3, 0x0000007, 1, 2, 0),
    (5, 0x0000019, 1, 3, 0),
    (6, 0x0000030, 1, 4, 0),
    (6, 0x0000036, 1, 5, 0),
    (7, 0x000006f, 1, 8, 0),
    (7, 0x0000063, 1, 6, 0),
    (7, 0x0000069, 12, 0, 1),
    (7, 0x000006b, 1, 7, 0),
    (8, 0x00000d1, 20, 0, 1),
    (8, 0x00000d4, 1, 9, 0),
    (8, 0x00000dc, 1, 10, 0),
    (9, 0x0000189, 1, 11, 0),
    (9, 0x000018a, 32, 0, 1),
    (9, 0x00001a0, 1, 12, 0),
    (9, 0x00001ab, 1, 13, 0),
    (10, 0x0000377, 1, 18, 0),
    (10, 0x0000310, 1, 14, 0),
    (10, 0x0000316, 1, 15, 0),
    (10, 0x0000343, 60, 0, 1),
    (10, 0x0000354, 1, 16, 0),
    (10, 0x0000375, 1, 17, 0),
    (11, 0x0000623, 1, 19, 0),
    (11, 0x0000684, 1, 20, 0),
    (11, 0x0000685, 100, 0, 1),
    (11, 0x00006ab, 1, 21, 0),
    (11, 0x00006ec, 1, 22, 0),
    (12, 0x0000ddb, 1, 29, 0),
    (12, 0x0000c5c, 1, 24, 0),
    (12, 0x0000c5e, 1, 25, 0),
    (12, 0x0000c44, 1, 23, 0),
    (12, 0x0000d55, 1, 26, 0),
    (12, 0x0000dd1, 1, 27, 0),
    (12, 0x0000dd3, 1, 28, 0),
    (13, 0x0001bb5, 1, 35, 0),
    (13, 0x000188b, 1, 30, 0),
    (13, 0x00018bb, 1, 31, 0),
    (13, 0x00018bf, 180, 0, 1),
    (13, 0x0001aa8, 1, 32, 0),
    (13, 0x0001ba0, 1, 33, 0),
    (13, 0x0001ba5, 320, 0, 1),
    (13, 0x0001ba4, 1, 34, 0),
    (14, 0x0003115, 1, 36, 0),
    (14, 0x0003175, 1, 37, 0),
    (14, 0x000317d, 1, 38, 0),
    (14, 0x0003553, 1, 39, 0),
    (14, 0x0003768, 1, 40, 0),
    (15, 0x0006e87, 1, 46, 0),
    (15, 0x0006ed3, 1, 47, 0),
    (15, 0x00062e8, 1, 42, 0),
    (15, 0x00062f8, 1, 43, 0),
    (15, 0x0006228, 1, 41, 0),
    (15, 0x0006aa4, 1, 44, 0),
    (15, 0x0006e85, 1, 45, 0),
    (16, 0x000c453, 1, 48, 0),
    (16, 0x000c5d3, 1, 49, 0),
    (16, 0x000c5f3, 1, 50, 0),
    (16, 0x000dda4, 1, 53, 0),
    (16, 0x000dd08, 1, 51, 0),
    (16, 0x000dd0c, 1, 52, 0),
    (17, 0x001bb4b, 1, 61, 0),
    (17, 0x001bb4a, 1, 60, 0),
    (17, 0x0018ba5, 1, 55, 0),
    (17, 0x0018be5, 1, 56, 0),
    (17, 0x001aa95, 1, 57, 0),
    (17, 0x001aa97, 1, 58, 0),
    (17, 0x00188a4, 1, 54, 0),
    (17, 0x001ba13, 1, 59, 0),
    (18, 0x0031748, 1, 62, 0),
    (18, 0x00317c8, 1, 63, 0),
    (18, 0x0035528, 1, 64, 0),
    (18, 0x003552c, 1, 65, 0),
    (18, 0x0037424, 1, 66, 0),
    (18, 0x0037434, 1, 67, 0),
    (18, 0x0037436, 1, 68, 0),
    (19, 0x0062294, 1, 69, 0),
    (19, 0x0062e92, 1, 70, 0),
    (19, 0x0062f92, 1, 71, 0),
    (19, 0x006aa52, 1, 72, 0),
    (19, 0x006aa5a, 1, 73, 0),
    (19, 0x006e86a, 1, 75, 0),
    (19, 0x006e86e, 1, 76, 0),
    (19, 0x006e84a, 1, 74, 0),
    (20, 0x00c452a, 1, 77, 0),
    (20, 0x00c5d27, 1, 78, 0),
    (20, 0x00c5f26, 1, 79, 0),
    (20, 0x00d54a6, 1, 80, 0),
    (20, 0x00d54b6, 1, 81, 0),
    (20, 0x00dd096, 1, 82, 0),
    (20, 0x00dd0d6, 1, 83, 0),
    (20, 0x00dd0de, 1, 84, 0),
    (21, 0x0188a56, 1, 85, 0),
    (21, 0x018ba4d, 1, 86, 0),
    (21, 0x018be4e, 1, 87, 0),
    (21, 0x018be4f, 1, 88, 0),
    (21, 0x01aa96e, 1, 89, 0),
    (21, 0x01ba12e, 1, 90, 0),
    (21, 0x01ba12f, 1, 91, 0),
    (21, 0x01ba1af, 1, 92, 0),
    (21, 0x01ba1bf, 1, 93, 0),
    (22, 0x037435d, 1, 99, 0),
    (22, 0x037437d, 1, 100, 0),
    (22, 0x0317498, 1, 94, 0),
    (22, 0x035529c, 1, 95, 0),
    (22, 0x035529d, 1, 96, 0),
    (22, 0x03552de, 1, 97, 0),
    (22, 0x03552df, 1, 98, 0),
    (23, 0x062e933, 1, 102, 0),
    (23, 0x062295d, 1, 101, 0),
    (23, 0x06aa53d, 1, 103, 0),
    (23, 0x06aa53f, 1, 105, 0),
    (23, 0x06aa53e, 1, 104, 0),
    (23, 0x06e86b9, 1, 106, 0),
    (23, 0x06e86f8, 1, 107, 0),
    (24, 0x0d54a79, 1, 111, 0),
    (24, 0x0c5d265, 1, 109, 0),
    (24, 0x0c452b8, 1, 108, 0),
    (24, 0x0dd0d71, 1, 113, 0),
    (24, 0x0d54a78, 1, 110, 0),
    (24, 0x0dd0d70, 1, 112, 0),
    (24, 0x0dd0df2, 1, 114, 0),
    (24, 0x0dd0df3, 1, 115, 0),
    (25, 0x188a5f6, 1, 225, 0),
    (25, 0x188a5f5, 1, 189, 0),
    (25, 0x188a5f4, 1, 188, 0),
    (25, 0x188a5f3, 1, 203, 0),
    (25, 0x188a5f2, 1, 202, 0),
    (25, 0x188a5f1, 1, 197, 0),
    (25, 0x188a5f0, 1, 207, 0),
    (25, 0x188a5ef, 1, 169, 0),
    (25, 0x188a5ee, 1, 223, 0),
    (25, 0x188a5ed, 1, 159, 0),
    (25, 0x188a5aa, 1, 235, 0),
    (25, 0x188a5e3, 1, 152, 0),
    (25, 0x188a5df, 1, 192, 0),
    (25, 0x188a589, 1, 179, 0),
    (25, 0x188a5dd, 1, 201, 0),
    (25, 0x188a578, 1, 172, 0),
    (25, 0x188a5e0, 1, 149, 0),
    (25, 0x188a588, 1, 178, 0),
    (25, 0x188a5d6, 1, 120, 0),
    (25, 0x188a5db, 1, 219, 0),
    (25, 0x188a5e1, 1, 150, 0),
    (25, 0x188a587, 1, 127, 0),
    (25, 0x188a59a, 1, 211, 0),
    (25, 0x188a5c4, 1, 125, 0),
    (25, 0x188a5ec, 1, 158, 0),
    (25, 0x188a586, 1, 247, 0),
    (25, 0x188a573, 1, 238, 0),
    (25, 0x188a59c, 1, 163, 0),
    (25, 0x188a5c8, 1, 228, 0),
    (25, 0x188a5fb, 1, 183, 0),
    (25, 0x188a5a1, 1, 217, 0),
    (25, 0x188a5eb, 1, 168, 0),
    (25, 0x188a5a8, 1, 122, 0),
    (25, 0x188a584, 1, 128, 0),
    (25, 0x188a5d2, 1, 249, 0),
    (25, 0x188a599, 1, 187, 0),
    (25, 0x188a598, 1, 186, 0),
    (25, 0x188a583, 1, 136, 0),
    (25, 0x18ba4c9, 1, 181, 0),
    (25, 0x188a5d0, 1, 255, 0),
    (25, 0x188a594, 1, 230, 0),
    (25, 0x188a582, 1, 135, 0),
    (25, 0x188a5cb, 1, 233, 0),
    (25, 0x188a5d8, 1, 222, 0),
    (25, 0x188a5e7, 1, 145, 0),
    (25, 0x188a581, 1, 134, 0),
    (25, 0x188a5ea, 1, 167, 0),
    (25, 0x188a5a9, 1, 248, 0),
    (25, 0x188a5a6, 1, 209, 0),
    (25, 0x188a580, 1, 243, 0),
    (25, 0x188a5a0, 1, 216, 0),
    (25, 0x188a59d, 1, 164, 0),
    (25, 0x188a5c3, 1, 140, 0),
    (25, 0x188a57f, 1, 157, 0),
    (25, 0x188a5c0, 1, 239, 0),
    (25, 0x188a5de, 1, 191, 0),
    (25, 0x188a5d4, 1, 251, 0),
    (25, 0x188a57e, 1, 156, 0),
    (25, 0x188a5c2, 1, 139, 0),
    (25, 0x188a592, 1, 242, 0),
    (25, 0x188a5cd, 1, 133, 0),
    (25, 0x188a57d, 1, 162, 0),
    (25, 0x188a5a3, 1, 213, 0),
    (25, 0x188a5e8, 1, 165, 0),
    (25, 0x188a5a2, 1, 212, 0),
    (25, 0x188a57c, 1, 227, 0),
    (25, 0x188a58e, 1, 198, 0),
    (25, 0x188a5b3, 1, 236, 0),
    (25, 0x188a5b2, 1, 234, 0),
    (25, 0x188a5b1, 1, 117, 0),
    (25, 0x188a5b0, 1, 215, 0),
    (25, 0x188a5af, 1, 124, 0),
    (25, 0x188a5ae, 1, 123, 0),
    (25, 0x188a5ad, 1, 254, 0),
    (25, 0x188a5ac, 1, 253, 0),
    (25, 0x188a5ab, 1, 148, 0),
    (25, 0x188a5da, 1, 218, 0),
    (25, 0x188a5e4, 1, 146, 0),
    (25, 0x188a5e5, 1, 147, 0),
    (25, 0x188a5d9, 1, 224, 0),
    (25, 0x188a5b5, 1, 143, 0),
    (25, 0x188a5bc, 1, 184, 0),
    (25, 0x188a5bd, 1, 185, 0),
    (25, 0x188a5e9, 1, 166, 0),
    (25, 0x188a5cc, 1, 132, 0),
    (25, 0x188a585, 1, 129, 0),
    (25, 0x188a5d3, 1, 250, 0),
    (25, 0x188a5e2, 1, 151, 0),
    (25, 0x188a595, 1, 119, 0),
    (25, 0x188a596, 1, 193, 0),
    (25, 0x188a5b8, 1, 176, 0),
    (25, 0x188a590, 1, 245, 0),
    (25, 0x188a5c9, 1, 229, 0),
    (25, 0x188a5a4, 1, 206, 0),
    (25, 0x188a5e6, 1, 144, 0),
    (25, 0x188a5a5, 1, 208, 0),
    (25, 0x188a5ce, 1, 137, 0),
    (25, 0x188a5bf, 1, 241, 0),
    (25, 0x188a572, 1, 237, 0),
    (25, 0x188a59b, 1, 190, 0),
    (25, 0x188a5be, 1, 240, 0),
    (25, 0x188a5c7, 1, 131, 0),
    (25, 0x188a5ca, 1, 232, 0),
    (25, 0x188a5d5, 1, 252, 0),
    (25, 0x188a57b, 1, 171, 0),
    (25, 0x188a58d, 1, 205, 0),
    (25, 0x188a58c, 1, 204, 0),
    (25, 0x188a58b, 1, 118, 0),
    (25, 0x188a58a, 1, 214, 0),
    (25, 0x18ba4c8, 1, 180, 0),
    (25, 0x188a5c5, 1, 126, 0),
    (25, 0x188a5fa, 1, 182, 0),
    (25, 0x188a5bb, 1, 175, 0),
    (25, 0x188a5c1, 1, 141, 0),
    (25, 0x188a5cf, 1, 138, 0),
    (25, 0x188a5b9, 1, 177, 0),
    (25, 0x188a5b6, 1, 153, 0),
    (25, 0x188a597, 1, 194, 0),
    (25, 0x188a5fe, 1, 160, 0),
    (25, 0x188a5d7, 1, 121, 0),
    (25, 0x188a5ba, 1, 174, 0),
    (25, 0x188a591, 1, 246, 0),
    (25, 0x188a5c6, 1, 130, 0),
    (25, 0x188a5dc, 1, 200, 0),
    (25, 0x188a57a, 1, 170, 0),
    (25, 0x188a59f, 1, 221, 0),
    (25, 0x188a5f9, 1, 196, 0),
    (25, 0x188a5b4, 1, 142, 0),
    (25, 0x188a5a7, 1, 210, 0),
    (25, 0x188a58f, 1, 199, 0),
    (25, 0x188a5fd, 1, 155, 0),
    (25, 0x188a5b7, 1, 154, 0),
    (25, 0x188a593, 1, 244, 0),
    (25, 0x188a59e, 1, 220, 0),
    (25, 0x188a5f8, 1, 195, 0),
    (25, 0x188a5ff, 1, 161, 0),
    (25, 0x188a5fc, 1, 231, 0),
    (25, 0x188a579, 1, 173, 0),
    (25, 0x188a5f7, 1, 226, 0),
    (26, 0x3114ba2, 1, 116, 0),
    (26, 0x3114ba3, 0, 1, 2),
];

const KIND_MAGNITUDE: u8 = 0;
const KIND_ZERO_RUN: u8 = 1;
const KIND_BAND_END: u8 = 2;

// -------------------------------------------------------------------
// The tag-value / chunk layer
// -------------------------------------------------------------------

/// Everything the header tuples tell the decoder about the frame.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Header {
    width: usize,
    height: usize,
    /// Per-level left shift undoing the encoder's prescale (§6.4),
    /// indexed 0 = finest level.
    descale: [u32; LEVELS],
}

/// One entropy- or raw-coded band, located in the stream.
#[derive(Debug, Clone)]
struct Codeblock<'a> {
    channel: u16,
    subband: u16,
    /// The quantiser in force for this subband; the lowpass band is
    /// unquantised and carries 0 here.
    quant: i32,
    /// Bits per coefficient of the lowpass band (`LowpassPrecision`).
    precision: u32,
    data: &'a [u8],
}

/// Split the elementary bitstream into its header state and the forty
/// codeblocks (four channels of ten subbands).
///
/// The stream is a flat sequence of big-endian 4-byte tuples: a signed
/// 16-bit tag and an unsigned 16-bit value. A non-negative tag is a
/// mandatory state update (image size, current channel, current
/// subband, current quantiser). A negative tag introduces a *chunk*,
/// and this is the one place the format needs care.
///
/// Chunks nest: a section chunk wraps the tuples and the codeblock of
/// one subband, and the codeblock chunk immediately precedes that
/// subband's coefficient bytes. Both carry the same end offset, so the
/// section is pure scaffolding — tracking `ChannelNumber` and
/// `SubbandNumber` through a flat scan is enough, and every negative
/// tag that is not a codeblock can be stepped over as a bare tuple.
///
/// A codeblock chunk is identified by its tag falling in
/// `0x9000 ..= 0xA000`, and its payload length in 32-bit segments is
///
/// ```text
/// size = ((0xA000 - tag) << 16) | value
/// ```
///
/// so the tag's distance below `0xA000` supplies the high bits of a
/// size too large for the 16-bit value alone. Bands under 64 K
/// segments therefore all read `0xA000`, and only the big finest-level
/// bands of a detailed frame drop to `0x9FFF` and below. Reading the
/// value alone decodes most files and then silently desynchronises on
/// the first large band, which is why the high bits matter.
fn parse<'a>(stream: &'a [u8]) -> Result<(Header, Vec<Codeblock<'a>>)> {
    if stream.len() < 4 || &stream[..4] != b"VC-5" {
        return Err(Error::Corrupt(
            "VC-5 sample does not start with the 'VC-5' marker".into(),
        ));
    }

    let mut width = 0usize;
    let mut height = 0usize;
    let mut prescale = 0u16;
    let mut channel_count = CHANNELS as u16;
    let mut subband_count = SUBBANDS as u16;
    let mut format = IMAGE_FORMAT_BAYER;
    let mut pattern = (2u16, 2u16);
    let mut per_sample = 1u16;
    let mut max_bits = 12u16;

    let mut channel = 0u16;
    let mut subband = 0u16;
    let mut quant = 0i32;
    let mut precision = 16u32;

    let mut layers = 1u16;
    let mut blocks: Vec<Codeblock<'a>> = Vec::new();
    let mut at = 0usize;
    while at + 4 <= stream.len() {
        let tag = u16::from_be_bytes([stream[at], stream[at + 1]]);
        let value = u16::from_be_bytes([stream[at + 2], stream[at + 3]]);
        at += 4;

        if (0x9000..=0xA000).contains(&tag) {
            // A codeblock: the payload is raw band data, measured in
            // 32-bit segments, and the next tuple follows it.
            let segments = (((0xA000 - tag) as usize) << 16) | value as usize;
            let len = segments
                .checked_mul(4)
                .ok_or_else(|| Error::Corrupt("VC-5 codeblock larger than memory".into()))?;
            let data = stream
                .get(at..at + len)
                .ok_or_else(|| Error::Corrupt("VC-5 codeblock runs past the sample".into()))?;
            at += len;
            if blocks.len() >= CHANNELS * SUBBANDS {
                return Err(Error::Corrupt(
                    "VC-5 sample carries more codeblocks than channels times subbands".into(),
                ));
            }
            blocks.push(Codeblock {
                channel,
                subband,
                quant,
                precision,
                data,
            });
        } else if tag == chunk::INVERSE_PERMUTATION || tag == chunk::INVERSE_TRANSFORM {
            // These override the fixed component permutation and
            // transform. Skipping them would silently produce the
            // wrong colours, so say so instead.
            return Err(Error::Unsupported(
                "VC-5 with an explicit inverse component permutation or transform".into(),
            ));
        } else if tag & 0x8000 != 0 {
            // Any other negative tag is a section marker, padding or a
            // Part 7 metadata chunk: scaffolding with no pixel data,
            // which this decoder steps over. Sections wrap the very
            // codeblocks they precede, so ignoring them loses nothing.
        } else {
            match tag {
                tag::IMAGE_WIDTH => width = value as usize,
                tag::IMAGE_HEIGHT => height = value as usize,
                tag::CHANNEL_COUNT => channel_count = value,
                tag::SUBBAND_COUNT => subband_count = value,
                tag::IMAGE_FORMAT => format = value,
                tag::MAX_BITS_PER_COMPONENT => max_bits = value,
                tag::PATTERN_WIDTH => pattern.0 = value,
                tag::PATTERN_HEIGHT => pattern.1 = value,
                tag::COMPONENTS_PER_SAMPLE => per_sample = value,
                tag::PRESCALE_SHIFT => prescale = value,
                tag::CHANNEL_NUMBER => channel = value,
                tag::SUBBAND_NUMBER => subband = value,
                tag::QUANTIZATION => quant = value as i32,
                tag::LOWPASS_PRECISION => precision = value as u32,
                tag::LAYER_COUNT => layers = value,
                _ => {}
            }
        }
    }

    // Reject the profiles the GoPro corpus never uses rather than
    // decoding them into nonsense.
    if format != IMAGE_FORMAT_BAYER {
        return Err(Error::Unsupported(format!(
            "VC-5 ImageFormat {format} (only 4, Bayer, is implemented)"
        )));
    }
    if pattern != (2, 2) || per_sample != 1 {
        return Err(Error::Unsupported(format!(
            "VC-5 with a {}x{} pattern of {per_sample} components a sample",
            pattern.0, pattern.1
        )));
    }
    if channel_count as usize != CHANNELS || subband_count as usize != SUBBANDS {
        return Err(Error::Unsupported(format!(
            "VC-5 with {channel_count} channels of {subband_count} subbands"
        )));
    }
    // The component transform works in twelve bits (its clamp,
    // midpoint and log curve are sized for it); a stream declaring
    // another precision would decode to the wrong values silently.
    if max_bits as i32 != 12 {
        return Err(Error::Unsupported(format!(
            "VC-5 with {max_bits}-bit components"
        )));
    }
    if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
        return Err(Error::Corrupt(format!(
            "VC-5 frame of {width}x{height} is not a whole number of 2x2 quads"
        )));
    }
    if layers > 1 {
        return Err(Error::Unsupported(format!(
            "VC-5 sample of {layers} layers (multi-layer GPR)"
        )));
    }
    if blocks.len() != CHANNELS * SUBBANDS {
        return Err(Error::Corrupt(format!(
            "VC-5 sample holds {} codeblocks, expected {}",
            blocks.len(),
            CHANNELS * SUBBANDS
        )));
    }
    // Every channel must contribute each of its subbands exactly once.
    // A chunk walk that lost synchronisation still tends to land the
    // right number of codeblocks, so this is what actually catches it.
    for (index, block) in blocks.iter().enumerate() {
        let want = (index / SUBBANDS, index % SUBBANDS);
        if (block.channel as usize, block.subband as usize) != want {
            return Err(Error::Corrupt(format!(
                "VC-5 codeblock {index} is channel {} subband {}, expected channel {} subband {}",
                block.channel, block.subband, want.0, want.1
            )));
        }
    }

    Ok((
        Header {
            width,
            height,
            descale: descale_of(prescale),
        },
        blocks,
    ))
}

/// Unpack `PrescaleShift`, which holds a 2-bit prescale per wavelet
/// level starting at bit 14 and working down.
///
/// The prescale is the number of bits the encoder shifted a level's
/// input down by to keep its coefficients in range, and reconstruction
/// shifts the same number back up. Every GoPro sample reads 0x2800, so
/// the two coarse reconstructions each shift by two bits and the finest
/// does not shift at all.
///
/// The shift is used verbatim, not halved: reconstructing the corpus
/// with a one-bit shift for a prescale of 2 lands a factor of four low.
/// It is also applied inside the horizontal filter, before that
/// filter's halving rather than after it (see [`lift`]) — undoing the
/// prescale afterwards would discard the low bits the shift is there to
/// recover and leaves the frame off by one or two codes.
fn descale_of(prescale: u16) -> [u32; LEVELS] {
    let mut descale = [0u32; LEVELS];
    for (level, slot) in descale.iter_mut().enumerate() {
        *slot = u32::from((prescale >> (14 - 2 * level)) & 3);
    }
    descale
}

// -------------------------------------------------------------------
// Band geometry
// -------------------------------------------------------------------

/// The size of each wavelet level's bands for a component plane.
///
/// Every halving rounds **up**, so an odd dimension keeps a column or
/// row that the inverse transform later discards: the Fusion frames
/// halve 1500 to 750 to 375 to 188, and reconstructing 188 rows back
/// into 375 drops the trailing odd output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Geometry {
    /// `dims[0]` is the full component plane, `dims[k]` the lowpass of
    /// level `k`, so `dims[3]` sizes the coarsest lowpass band.
    dims: [(usize, usize); LEVELS + 1],
}

impl Geometry {
    fn of(component_width: usize, component_height: usize) -> Geometry {
        let mut dims = [(0usize, 0usize); LEVELS + 1];
        dims[0] = (component_width, component_height);
        for level in 1..=LEVELS {
            let (w, h) = dims[level - 1];
            dims[level] = (w.div_ceil(2), h.div_ceil(2));
        }
        Geometry { dims }
    }

    /// The band dimensions a subband index is stored at: subbands 1..3
    /// live at the coarsest level, 4..6 one finer, 7..9 finest.
    fn band(&self, subband: usize) -> (usize, usize) {
        let level = match subband {
            0..=3 => 3,
            4..=6 => 2,
            _ => 1,
        };
        self.dims[level]
    }
}

// -------------------------------------------------------------------
// Entropy coding
// -------------------------------------------------------------------

/// The codebook as a binary trie, so a codeword is matched by walking
/// one bit at a time. The code is prefix-free and at most 26 bits, so a
/// walk always terminates; a bit pattern the codebook does not contain
/// ends at a missing child and is reported rather than guessed at.
struct Trie {
    /// Each node's two children. A non-negative entry is another node,
    /// `-1` is missing, and anything at or below `-2` encodes the
    /// codebook index of a leaf as `-(index + 2)`.
    children: Vec<[i32; 2]>,
}

impl Trie {
    fn build() -> Trie {
        let mut children: Vec<[i32; 2]> = vec![[-1, -1]];
        for (index, &(length, code, _, _, _)) in CODEBOOK.iter().enumerate() {
            let mut node = 0usize;
            for step in (0..length).rev() {
                let bit = ((code >> step) & 1) as usize;
                if step == 0 {
                    children[node][bit] = -((index as i32) + 2);
                } else {
                    let next = children[node][bit];
                    node = if next < 0 {
                        children.push([-1, -1]);
                        let fresh = children.len() - 1;
                        children[node][bit] = fresh as i32;
                        fresh
                    } else {
                        next as usize
                    };
                }
            }
        }
        Trie { children }
    }

    /// Match one codeword, returning its index in [`CODEBOOK`].
    #[inline]
    fn decode(&self, pump: &mut BitPumpMsb<'_>) -> Result<usize> {
        let mut node = 0usize;
        loop {
            let bit = pump.get(1) as usize;
            let next = self.children[node][bit];
            if next >= 0 {
                node = next as usize;
            } else if next == -1 {
                return Err(Error::Corrupt(
                    "VC-5 highpass band holds a codeword outside the codebook".into(),
                ));
            } else {
                return Ok((-(next + 2)) as usize);
            }
        }
    }
}

fn trie() -> &'static Trie {
    static TRIE: std::sync::OnceLock<Trie> = std::sync::OnceLock::new();
    TRIE.get_or_init(Trie::build)
}

/// The inverse companding curve, mapping a codebook magnitude 0..255
/// back onto the encoder's 0..1023 range.
///
/// The encoder companded large coefficients down so that every
/// magnitude fits the 256-entry codebook; GoPro uses the cubic curve
///
/// ```text
/// expanded(m) = m + trunc(768 * m^3 / 255^3)
/// ```
///
/// so 0 stays 0 and 255 becomes 1023. The lowpass band is never
/// companded.
fn expand_curve() -> &'static [i32; 256] {
    static CURVE: std::sync::OnceLock<[i32; 256]> = std::sync::OnceLock::new();
    CURVE.get_or_init(|| {
        let mut curve = [0i32; 256];
        for (m, slot) in curve.iter_mut().enumerate() {
            let m = m as i64;
            *slot = (m + (768 * m * m * m) / (255 * 255 * 255)) as i32;
        }
        curve
    })
}

/// The camera's decoder log curve: 12-bit log-domain component values
/// back to 16-bit linear.
///
/// The encoder's forward curve is a base-113 logarithm over the 16-bit
/// sensor range, so the decoder's is the matching power curve. Values
/// are then shifted down to the sensor's real bit depth.
fn log_curve() -> &'static [u16; 4096] {
    static CURVE: std::sync::OnceLock<[u16; 4096]> = std::sync::OnceLock::new();
    CURVE.get_or_init(|| {
        let mut curve = [0u16; 4096];
        for (i, slot) in curve.iter_mut().enumerate() {
            let t = (i as f64) / 4095.0;
            let linear = 65535.0 * (113f64.powf(t) - 1.0) / 112.0;
            *slot = linear.floor().clamp(0.0, 65535.0) as u16;
        }
        curve
    })
}

/// Decode one highpass subband into dequantised coefficients.
///
/// The band is filled in raster order by run/value codewords: a
/// magnitude codeword contributes one coefficient and is followed by a
/// sign bit, a zero-run codeword contributes that many zeros and has no
/// sign bit. Runs are not restarted per row, so one may span the end of
/// a row into the next. After the last coefficient the stream carries a
/// band-end marker, which is read but not required — a band that filled
/// exactly is already complete.
fn decode_highpass(block: &Codeblock<'_>, width: usize, height: usize) -> Result<Vec<i32>> {
    let count = frame_samples(width, height, 1)?;
    let mut band = vec![0i32; count];

    // Companding and dequantisation both depend only on the codebook
    // magnitude, so fold them into one 256-entry table per band:
    // expand the magnitude, scale by the subband's quantiser, and clamp
    // to the signed 16-bit range the transform works in.
    let expand = expand_curve();
    let mut level = [0i32; 256];
    for (m, slot) in level.iter_mut().enumerate() {
        *slot = (expand[m] * block.quant).clamp(i16::MIN as i32, i16::MAX as i32);
    }

    let trie = trie();
    let mut pump = BitPumpMsb::new(block.data);
    let mut at = 0usize;
    while at < count {
        let (_, _, run, value, kind) = CODEBOOK[trie.decode(&mut pump)?];
        match kind {
            KIND_BAND_END => {
                return Err(Error::Corrupt(format!(
                    "VC-5 highpass band ended after {at} of {count} coefficients"
                )))
            }
            KIND_ZERO_RUN => {
                // Zeros need no writing, the band starts zeroed; a run
                // overrunning the band is clamped rather than trusted.
                at = at.saturating_add(run as usize).min(count);
            }
            KIND_MAGNITUDE => {
                // A sign bit follows only a non-zero magnitude. The
                // one-bit codeword for a single zero coefficient has
                // none, and reading one anyway desynchronises the rest
                // of the band.
                if value != 0 {
                    let magnitude = level[value as usize];
                    band[at] = if pump.get(1) == 1 {
                        -magnitude
                    } else {
                        magnitude
                    };
                }
                at += 1;
            }
            other => {
                return Err(Error::Corrupt(format!(
                    "VC-5 codebook entry of unknown kind {other}"
                )))
            }
        }
        // A truncated or garbage band would otherwise spin here: the
        // pump feeds zeros past its input, and the one-bit codeword for
        // magnitude zero makes progress, so `at` always advances.
    }
    Ok(band)
}

/// Decode the single coarsest lowpass band, which is stored plainly as
/// fixed-width unsigned integers in raster order — no sign, no
/// prediction, no run coding, no companding.
fn decode_lowpass(block: &Codeblock<'_>, width: usize, height: usize) -> Result<Vec<i32>> {
    let count = frame_samples(width, height, 1)?;
    if block.precision == 0 || block.precision > 16 {
        return Err(Error::Corrupt(format!(
            "VC-5 LowpassPrecision {} is not 1..=16",
            block.precision
        )));
    }
    let mut band = vec![0i32; count];
    let mut pump = BitPumpMsb::new(block.data);
    for slot in band.iter_mut() {
        *slot = pump.get(block.precision) as i32;
    }
    Ok(band)
}

// -------------------------------------------------------------------
// The inverse 2/6 wavelet
// -------------------------------------------------------------------

/// One pass of the inverse 2/6 filter along a line.
///
/// The wavelet is the reversible 2/6 (two-tap lowpass, six-tap
/// highpass) spatial wavelet. Each input position yields two output
/// samples, doubling the resolution along the axis: an "even" sample
/// built by adding the highpass detail and an "odd" one by subtracting
/// it. Interior positions use their two neighbours; the first and last
/// positions have no neighbour on one side and use the one-sided
/// `11,-4,1` and `5,4,-1` taps instead.
///
/// `out` may be one shorter than `2 * low.len()`, which is how an odd
/// parent dimension drops the trailing odd sample.
///
/// `descale` undoes the encoder's prescale for this level. It is folded
/// in before the filter's final halving, so the bits it restores
/// survive; the vertical pass always passes 0 and only the horizontal
/// pass carries a shift.
fn lift(low: &[i32], high: &[i32], out: &mut [i32], descale: u32) {
    let n = low.len();
    debug_assert_eq!(n, high.len());
    debug_assert!(n >= 3);
    for c in 0..n {
        // The `+ 4` is the filter's rounding term and is added before
        // the divide-by-8; the final divide-by-2 is a bare shift.
        let (even, odd) = if c == 0 {
            (
                (11 * low[0] - 4 * low[1] + low[2] + 4) >> 3,
                (5 * low[0] + 4 * low[1] - low[2] + 4) >> 3,
            )
        } else if c == n - 1 {
            (
                (5 * low[n - 1] + 4 * low[n - 2] - low[n - 3] + 4) >> 3,
                (11 * low[n - 1] - 4 * low[n - 2] + low[n - 3] + 4) >> 3,
            )
        } else {
            (
                low[c] + ((low[c - 1] - low[c + 1] + 4) >> 3),
                low[c] + ((low[c + 1] - low[c - 1] + 4) >> 3),
            )
        };
        if let Some(slot) = out.get_mut(2 * c) {
            *slot = ((even + high[c]) << descale) >> 1;
        }
        if let Some(slot) = out.get_mut(2 * c + 1) {
            *slot = ((odd - high[c]) << descale) >> 1;
        }
    }
}

/// Invert one wavelet level: four bands in, the next finer level's
/// lowpass out.
///
/// The transform is separable and is undone in two passes. Vertically,
/// the bands pair up as (lowpass, vertical detail) and (horizontal
/// detail, diagonal detail); each pair is filtered down the columns,
/// producing a full-height lowpass and a full-height highpass
/// intermediate. Those two are then filtered across the rows to double
/// the width.
///
/// `descale` undoes the encoder's prescale for this level. It rides
/// into the horizontal filter rather than being applied to its output,
/// which keeps the low bits, and the result is then clamped to the
/// transform's unsigned working range.
#[allow(clippy::too_many_arguments)]
fn inverse_level(
    lowpass: &[i32],
    horizontal: &[i32],
    vertical: &[i32],
    diagonal: &[i32],
    band: (usize, usize),
    out: (usize, usize),
    descale: u32,
) -> Result<Vec<i32>> {
    let (band_w, band_h) = band;
    let (out_w, out_h) = out;
    if band_w < 3 || band_h < 3 {
        return Err(Error::Unsupported(format!(
            "VC-5 wavelet band of {band_w}x{band_h} is too small for the 2/6 filter"
        )));
    }

    // The two vertical intermediates are full height but still band
    // width; the horizontal pass then widens them into the output.
    let rows = frame_samples(band_w, out_h, 1)?;
    let mut low_rows = vec![0i32; rows];
    let mut high_rows = vec![0i32; rows];

    let mut column_low = vec![0i32; band_h];
    let mut column_high = vec![0i32; band_h];
    let mut column_out = vec![0i32; out_h];
    for x in 0..band_w {
        for (pair, (l, h)) in [
            (&mut low_rows, (lowpass, vertical)),
            (&mut high_rows, (horizontal, diagonal)),
        ] {
            for y in 0..band_h {
                column_low[y] = l[y * band_w + x];
                column_high[y] = h[y * band_w + x];
            }
            lift(&column_low, &column_high, &mut column_out, 0);
            for (y, value) in column_out.iter().enumerate() {
                pair[y * band_w + x] = *value;
            }
        }
    }

    let mut frame = vec![0i32; frame_samples(out_w, out_h, 1)?];
    for y in 0..out_h {
        let source = y * band_w;
        let row = &mut frame[y * out_w..(y + 1) * out_w];
        lift(
            &low_rows[source..source + band_w],
            &high_rows[source..source + band_w],
            row,
            descale,
        );
        for value in row.iter_mut() {
            *value = (*value).clamp(0, TRANSFORM_MAX);
        }
    }
    Ok(frame)
}

/// Reconstruct one component plane from its ten subbands.
fn reconstruct(
    bands: &[Vec<i32>; SUBBANDS],
    geometry: &Geometry,
    descale: [u32; LEVELS],
) -> Result<Vec<i32>> {
    let mut lowpass = bands[0].clone();
    // Levels run coarsest first: inverting level 3 makes the lowpass of
    // level 2, and inverting level 1 makes the component plane.
    for level in (1..=LEVELS).rev() {
        let first = 3 * (LEVELS - level) + 1;
        lowpass = inverse_level(
            &lowpass,
            &bands[first],
            &bands[first + 1],
            &bands[first + 2],
            geometry.dims[level],
            geometry.dims[level - 1],
            descale[level - 1],
        )?;
    }
    Ok(lowpass)
}

// -------------------------------------------------------------------
// The inverse component transform
// -------------------------------------------------------------------

/// Decode one VC-5 tile into `width * height` RGGB sensor samples.
///
/// `shift` is how far the 16-bit output of the log curve is moved down
/// to reach the sensor's real bit depth: a 14-bit GPR passes 2.
pub fn decode(stream: &[u8], width: usize, height: usize, shift: u32) -> Result<Vec<u16>> {
    let (header, blocks) = parse(stream)?;
    if header.width != width || header.height != height {
        return Err(Error::Corrupt(format!(
            "VC-5 sample is {}x{}, the DNG tile is {width}x{height}",
            header.width, header.height
        )));
    }
    if shift > 16 {
        return Err(Error::Corrupt(format!("VC-5 output shift {shift}")));
    }

    // Each channel is a half-resolution plane: the 2x2 quad is
    // de-interleaved before compression.
    let component = (width / 2, height / 2);
    let geometry = Geometry::of(component.0, component.1);
    let samples = frame_samples(width, height, 1)?;

    // The four channels are independent, so decode and reconstruct them
    // in parallel; each is well over a megapixel of wavelet work.
    let mut planes: Vec<Result<Vec<i32>>> = Vec::with_capacity(CHANNELS);
    (0..CHANNELS)
        .into_par_iter()
        .map(|channel| {
            let mut bands: [Vec<i32>; SUBBANDS] = Default::default();
            for (subband, band) in bands.iter_mut().enumerate() {
                let block = blocks
                    .iter()
                    .find(|b| b.channel as usize == channel && b.subband as usize == subband)
                    .ok_or_else(|| {
                        Error::Corrupt(format!(
                            "VC-5 sample is missing channel {channel} subband {subband}"
                        ))
                    })?;
                let (w, h) = geometry.band(subband);
                *band = if subband == 0 {
                    decode_lowpass(block, w, h)?
                } else {
                    decode_highpass(block, w, h)?
                };
            }
            reconstruct(&bands, &geometry, header.descale)
        })
        .collect_into_vec(&mut planes);
    let planes = planes.into_iter().collect::<Result<Vec<_>>>()?;

    // Undo the colour-difference transform and the log curve. The
    // channels are green sum, red-minus-green, blue-minus-green and the
    // difference of the two greens, the last three biased by a midpoint
    // so they could be carried unsigned.
    let curve = log_curve();
    let mut frame = vec![0u16; samples];
    let (green_sum, red, blue, green_diff) = (&planes[0], &planes[1], &planes[2], &planes[3]);
    frame
        .par_chunks_mut(2 * width)
        .enumerate()
        .for_each(|(y, quad_rows)| {
            let (top, bottom) = quad_rows.split_at_mut(width);
            for x in 0..component.0 {
                let at = y * component.0 + x;
                let gs = green_sum[at];
                let rg = red[at] - MIDPOINT;
                let bg = blue[at] - MIDPOINT;
                let gd = green_diff[at] - MIDPOINT;
                let look = |v: i32| curve[v.clamp(0, COMPONENT_MAX) as usize] >> shift;
                top[2 * x] = look(2 * rg + gs);
                top[2 * x + 1] = look(gs + gd);
                bottom[2 * x] = look(gs - gd);
                bottom[2 * x + 1] = look(2 * bg + gs);
            }
        });
    Ok(frame)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// The codebook has to be prefix-free or the trie would have a leaf
    /// on the path to another leaf and decoding would silently diverge.
    #[test]
    fn codebook_is_prefix_free_and_complete() {
        for (i, &(a_len, a_code, _, _, _)) in CODEBOOK.iter().enumerate() {
            assert!((1..=26).contains(&a_len), "entry {i} has length {a_len}");
            for &(b_len, b_code, _, _, _) in CODEBOOK.iter().skip(i + 1) {
                let (short, long, shift) = if a_len <= b_len {
                    (a_code, b_code, b_len - a_len)
                } else {
                    (b_code, a_code, a_len - b_len)
                };
                assert_ne!(short, long >> shift, "entry {i} is a prefix of another");
            }
        }
        let magnitudes: std::collections::BTreeSet<u16> = CODEBOOK
            .iter()
            .filter(|e| e.4 == KIND_MAGNITUDE)
            .map(|e| e.3)
            .collect();
        assert_eq!(magnitudes.len(), 256, "the 256 magnitudes must all appear");
        assert_eq!(*magnitudes.iter().next_back().unwrap(), 255);
        assert_eq!(
            CODEBOOK.iter().filter(|e| e.4 == KIND_BAND_END).count(),
            1,
            "exactly one band-end marker"
        );
        // The zero-runs the format defines.
        let runs: Vec<u16> = CODEBOOK
            .iter()
            .filter(|e| e.4 == KIND_ZERO_RUN)
            .map(|e| e.2)
            .collect();
        assert_eq!(runs, vec![12, 20, 32, 60, 100, 180, 320]);
    }

    /// The trie must decode every codeword back to the entry it came
    /// from, reading exactly its own bits and no more.
    #[test]
    fn trie_round_trips_every_codeword() {
        let trie = trie();
        for (i, &(len, code, _, _, _)) in CODEBOOK.iter().enumerate() {
            // Pack the codeword MSB-first into a buffer, padded with
            // ones so a walk that reads too far lands somewhere else.
            let mut bytes = vec![0xFFu8; 8];
            for bit in 0..len {
                let set = (code >> (len - 1 - bit)) & 1 == 1;
                let (byte, offset) = (bit as usize / 8, 7 - (bit as usize % 8));
                if set {
                    bytes[byte] |= 1 << offset;
                } else {
                    bytes[byte] &= !(1 << offset);
                }
            }
            let mut pump = BitPumpMsb::new(&bytes);
            assert_eq!(trie.decode(&mut pump).unwrap(), i, "entry {i}");
            assert_eq!(
                pump.position(),
                len as usize,
                "entry {i} read the wrong bits"
            );
        }
    }

    /// The cubic companding curve: 0..255 expands onto 0..1023.
    #[test]
    fn companding_curve_matches_the_cubic() {
        let curve = expand_curve();
        assert_eq!(curve[0], 0);
        assert_eq!(curve[255], 1023, "255 must reach 255 + 768");
        for (m, value) in curve.iter().enumerate() {
            let want = m as i64 + (768 * (m as i64).pow(3)) / (255i64.pow(3));
            assert_eq!(*value as i64, want, "magnitude {m}");
        }
        // Monotonic, or dequantisation would fold distinct levels.
        assert!(curve.windows(2).all(|w| w[0] < w[1]));
    }

    /// The decoder log curve, checked against the values the
    /// specification's worked example quotes.
    #[test]
    fn log_curve_matches_the_worked_example() {
        let curve = log_curve();
        assert_eq!(curve[1217], 1799);
        assert_eq!(curve[1843], 4326);
        assert_eq!(curve[1963], 5056);
        assert_eq!(curve[1781], 3987);
        assert_eq!(curve[0], 0);
        assert_eq!(curve[4095], 65535, "the curve must reach full scale");
        assert!(curve.windows(2).all(|w| w[0] <= w[1]));
    }

    /// The §8 worked example, from component values to sensor codes:
    /// the HERO8 frame's top-left quad.
    #[test]
    fn worked_example_component_transform() {
        let curve = log_curve();
        let quad = |gs: i32, rg: i32, bg: i32, gd: i32| {
            let (rg, bg, gd) = (rg - MIDPOINT, bg - MIDPOINT, gd - MIDPOINT);
            let look = |v: i32| curve[v.clamp(0, COMPONENT_MAX) as usize] >> 2;
            (
                look(2 * rg + gs),
                look(gs + gd),
                look(gs - gd),
                look(2 * bg + gs),
            )
        };
        // Component pixel (0, 0): GS 1903, RG 1705, BG 1987, GD 1988.
        assert_eq!(quad(1903, 1705, 1987, 1988), (449, 1081, 1264, 996));
        // Component pixel (1, 0), columns 2-3 of the same row pair.
        assert_eq!(quad(1879, 1820, 1933, 2093), (609, 1202, 1069, 835));
    }

    /// `PrescaleShift` packs a 2-bit prescale per wavelet level; the
    /// corpus value 0x2800 means the two coarse reconstructions each
    /// shift a bit back up and the finest does not.
    #[test]
    fn prescale_unpacks_per_level() {
        // The corpus value: the finest level does not shift, the two
        // coarser reconstructions each shift two bits back up.
        assert_eq!(descale_of(0x2800), [0, 2, 2]);
        assert_eq!(descale_of(0x0000), [0, 0, 0]);
        // Bits 14-15 are level 0, 12-13 level 1, 10-11 level 2.
        assert_eq!(descale_of(0x8000), [2, 0, 0]);
        assert_eq!(descale_of(0x2000), [0, 2, 0]);
        assert_eq!(descale_of(0x0800), [0, 0, 2]);
        // The shift is the prescale itself, so the field's other two
        // values are one- and three-bit shifts. Nothing in the corpus
        // uses them.
        assert_eq!(descale_of(0x4000), [1, 0, 0]);
        assert_eq!(descale_of(0xC000), [3, 0, 0]);
    }

    /// Band dimensions halve with a ceiling, which is what keeps the
    /// Fusion frames' 1500 rows landing on 188 at the coarsest level.
    #[test]
    fn band_geometry_rounds_up() {
        let hero = Geometry::of(2000, 1500);
        assert_eq!(
            hero.dims,
            [(2000, 1500), (1000, 750), (500, 375), (250, 188)]
        );
        assert_eq!(hero.band(0), (250, 188));
        assert_eq!(hero.band(3), (250, 188));
        assert_eq!(hero.band(4), (500, 375));
        assert_eq!(hero.band(6), (500, 375));
        assert_eq!(hero.band(7), (1000, 750));
        assert_eq!(hero.band(9), (1000, 750));
        let fusion = Geometry::of(1552, 1500);
        assert_eq!(
            fusion.dims,
            [(1552, 1500), (776, 750), (388, 375), (194, 188)]
        );
    }

    /// A minimal header: the marker plus the tuples `parse` insists on,
    /// with no codeblocks. Used to exercise the tag layer on its own.
    fn header_stream(prescale: u16) -> Vec<u8> {
        let mut out = b"VC-5".to_vec();
        let mut tuple = |tag: u16, value: u16| {
            out.extend_from_slice(&tag.to_be_bytes());
            out.extend_from_slice(&value.to_be_bytes());
        };
        tuple(tag::CHANNEL_COUNT, 4);
        tuple(tag::SUBBAND_COUNT, 10);
        tuple(tag::IMAGE_WIDTH, 64);
        tuple(tag::IMAGE_HEIGHT, 48);
        tuple(tag::IMAGE_FORMAT, IMAGE_FORMAT_BAYER);
        tuple(tag::PATTERN_WIDTH, 2);
        tuple(tag::PATTERN_HEIGHT, 2);
        tuple(tag::COMPONENTS_PER_SAMPLE, 1);
        tuple(tag::MAX_BITS_PER_COMPONENT, 12);
        tuple(tag::PRESCALE_SHIFT, prescale);
        out
    }

    #[test]
    fn rejects_a_sample_without_the_marker() {
        let mut stream = header_stream(0x2800);
        stream[0] = b'X';
        assert!(matches!(parse(&stream), Err(Error::Corrupt(_))));
    }

    /// A header without its forty codeblocks is corrupt, not a frame of
    /// zeros.
    #[test]
    fn rejects_a_sample_with_no_codeblocks() {
        let error = parse(&header_stream(0x2800)).expect_err("no codeblocks");
        assert!(matches!(error, Error::Corrupt(_)), "{error}");
    }

    /// The profiles the corpus never uses are refused by name rather
    /// than decoded into nonsense.
    #[test]
    fn rejects_the_unused_profiles() {
        let swap = |tag: u16, value: u16| {
            let mut stream = header_stream(0x2800);
            let mut at = 4;
            while at + 4 <= stream.len() {
                if u16::from_be_bytes([stream[at], stream[at + 1]]) == tag {
                    stream[at + 2..at + 4].copy_from_slice(&value.to_be_bytes());
                }
                at += 4;
            }
            parse(&stream).expect_err("should be refused")
        };
        // Anything but Bayer, and any pattern that is not a 2x2 of one
        // component, is a VC-5 profile this decoder does not implement.
        assert!(matches!(swap(tag::IMAGE_FORMAT, 2), Error::Unsupported(_)));
        assert!(matches!(swap(tag::PATTERN_WIDTH, 4), Error::Unsupported(_)));
        assert!(matches!(
            swap(tag::COMPONENTS_PER_SAMPLE, 3),
            Error::Unsupported(_)
        ));
        assert!(matches!(swap(tag::CHANNEL_COUNT, 3), Error::Unsupported(_)));
        assert!(matches!(swap(tag::SUBBAND_COUNT, 7), Error::Unsupported(_)));
        // An odd frame cannot be a whole number of 2x2 quads.
        assert!(matches!(swap(tag::IMAGE_WIDTH, 63), Error::Corrupt(_)));
    }

    /// The variants the corpus never uses are named rather than
    /// decoded into the wrong colours.
    #[test]
    fn rejects_the_variants_this_decoder_does_not_implement() {
        // A multi-layer sample (GoPro has never shipped one).
        let mut stream = header_stream(0x2800);
        stream.extend_from_slice(&tag::LAYER_COUNT.to_be_bytes());
        stream.extend_from_slice(&2u16.to_be_bytes());
        let error = parse(&stream).expect_err("multi-layer");
        assert!(matches!(error, Error::Unsupported(_)), "{error}");

        // An explicit inverse permutation or component transform would
        // override the fixed Bayer scheme, so it cannot be skipped.
        for chunk in [chunk::INVERSE_PERMUTATION, chunk::INVERSE_TRANSFORM] {
            let mut stream = header_stream(0x2800);
            stream.extend_from_slice(&chunk.to_be_bytes());
            stream.extend_from_slice(&1u16.to_be_bytes());
            let error = parse(&stream).expect_err("explicit transform");
            assert!(matches!(error, Error::Unsupported(_)), "{error}");
        }
    }

    /// A chunk walk that desynchronises still tends to find forty
    /// codeblocks, so the ordering check is what catches it.
    #[test]
    fn rejects_codeblocks_that_are_out_of_order() {
        let mut stream = header_stream(0x2800);
        // Forty empty codeblocks, but every one claiming channel 0
        // subband 0.
        for _ in 0..CHANNELS * SUBBANDS {
            stream.extend_from_slice(&0xA000u16.to_be_bytes());
            stream.extend_from_slice(&0u16.to_be_bytes());
        }
        let error = parse(&stream).expect_err("all one subband");
        assert!(matches!(error, Error::Corrupt(_)), "{error}");
    }

    /// The 2/6 filter's two boundary cases and its interior, checked by
    /// hand against the lifting weights.
    #[test]
    fn lift_matches_the_2_6_weights() {
        let low = [100, 200, 300, 400];
        let high = [0, 0, 0, 0];
        let mut out = [0i32; 8];
        lift(&low, &high, &mut out, 0);
        // Left border uses the one-sided 11,-4,1 and 5,4,-1 taps.
        assert_eq!(out[0], ((11 * 100 - 4 * 200 + 300 + 4) >> 3) >> 1);
        assert_eq!(out[1], ((5 * 100 + 4 * 200 - 300 + 4) >> 3) >> 1);
        // Interior uses the neighbours either side.
        assert_eq!(out[2], (200 + ((100 - 300 + 4) >> 3)) >> 1);
        assert_eq!(out[3], (200 + ((300 - 100 + 4) >> 3)) >> 1);
        // Right border mirrors the left.
        assert_eq!(out[6], ((5 * 400 + 4 * 300 - 200 + 4) >> 3) >> 1);
        assert_eq!(out[7], ((11 * 400 - 4 * 300 + 200 + 4) >> 3) >> 1);

        // The highpass detail adds to the even output and subtracts
        // from the odd, so a flat lowpass with one detail sample
        // separates the pair symmetrically.
        let mut detail = [0i32; 8];
        lift(&[500, 500, 500, 500], &[0, 40, 0, 0], &mut detail, 0);
        assert_eq!(detail[2], (500 + 40) >> 1);
        assert_eq!(detail[3], (500 - 40) >> 1);

        // An odd parent dimension simply drops the trailing sample.
        let mut odd = [0i32; 7];
        lift(&low, &high, &mut odd, 0);
        assert_eq!(odd[..7], out[..7]);
    }

    /// A band smaller than the six-tap filter is refused rather than
    /// indexed out of bounds.
    #[test]
    fn rejects_a_band_too_small_to_filter() {
        let tiny = vec![0i32; 4];
        let error = inverse_level(&tiny, &tiny, &tiny, &tiny, (2, 2), (4, 4), 0)
            .expect_err("2x2 bands cannot feed a six-tap filter");
        assert!(matches!(error, Error::Unsupported(_)), "{error}");
    }

    // ---------------------------------------------------------------
    // Corpus. The GoPro samples under `$SCHIST_RAW_CORPUS` with their
    // `.tiff` oracles beside them; the whole-file comparison lives in
    // the DNG module's corpus test, so these check the VC-5 layer.
    // ---------------------------------------------------------------

    fn corpus_files() -> Vec<PathBuf> {
        let Some(dir) = std::env::var_os("SCHIST_RAW_CORPUS").map(PathBuf::from) else {
            return Vec::new();
        };
        let mut found = Vec::new();
        let mut stack = vec![dir];
        while let Some(at) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&at) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| e.eq_ignore_ascii_case("gpr"))
                {
                    found.push(path);
                }
            }
        }
        found.sort();
        found
    }

    /// Every GPR's bitstream must resolve into exactly four channels of
    /// ten subbands with the geometry the container advertises. This is
    /// the checkpoint that catches a desynchronised chunk walk, which
    /// is the failure mode the large-codeblock size encoding causes.
    #[test]
    fn corpus_bitstreams_parse() {
        for path in corpus_files() {
            let bytes = std::fs::read(&path).expect("read the sample");
            let at =
                find_sample(&bytes).unwrap_or_else(|| panic!("{}: no VC-5 marker", path.display()));
            let (header, blocks) =
                parse(&bytes[at..]).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            assert_eq!(blocks.len(), CHANNELS * SUBBANDS, "{}", path.display());
            for (i, block) in blocks.iter().enumerate() {
                assert_eq!(
                    (block.channel as usize, block.subband as usize),
                    (i / SUBBANDS, i % SUBBANDS),
                    "{}: codeblocks out of order",
                    path.display()
                );
            }
            assert_eq!(header.descale, [0, 2, 2], "{}", path.display());
            assert_eq!(header.width % 2, 0);
            println!(
                "{}: {}x{}, {} codeblocks",
                path.display(),
                header.width,
                header.height,
                blocks.len()
            );
        }
    }

    /// The specification's staged checkpoints on the HERO8 frame: the
    /// coarsest lowpass band is a slowly varying DC image, the four
    /// component planes hold the quoted values at (0, 0), and the
    /// finished quad is 449 / 1081 / 1264 / 996.
    #[test]
    fn hero8_worked_example_end_to_end() {
        let Some(path) = corpus_files()
            .into_iter()
            .find(|p| p.to_string_lossy().contains("GOPR0009"))
        else {
            return;
        };
        let bytes = std::fs::read(&path).expect("read the sample");
        let at = find_sample(&bytes).expect("marker");
        let (header, blocks) = parse(&bytes[at..]).expect("parse");
        assert_eq!((header.width, header.height), (4000, 3000));

        let geometry = Geometry::of(header.width / 2, header.height / 2);
        assert_eq!(geometry.band(0), (250, 188));

        // Checkpoint 3: channel 0's lowpass band, 250x188 of 16-bit
        // values in the 7000-8500 range.
        let lowpass = decode_lowpass(&blocks[0], 250, 188).expect("lowpass");
        assert_eq!(lowpass.len(), 250 * 188);
        // A DC image: every coefficient fits the 16-bit precision the
        // header declares and the average sits in the range the
        // specification quotes for this frame.
        assert!(lowpass.iter().all(|v| (0..=65535).contains(v)));
        let mean = lowpass.iter().map(|v| *v as i64).sum::<i64>() / lowpass.len() as i64;
        assert!((6000..9000).contains(&mean), "lowpass mean {mean}");

        // Checkpoints 4 and 5: the component planes and the quad.
        let planes: Vec<Vec<i32>> = (0..CHANNELS)
            .map(|channel| {
                let mut bands: [Vec<i32>; SUBBANDS] = Default::default();
                for subband in 0..SUBBANDS {
                    let block = &blocks[channel * SUBBANDS + subband];
                    let (w, h) = geometry.band(subband);
                    bands[subband] = if subband == 0 {
                        decode_lowpass(block, w, h).expect("lowpass")
                    } else {
                        decode_highpass(block, w, h).expect("highpass")
                    };
                }
                reconstruct(&bands, &geometry, header.descale).expect("reconstruct")
            })
            .collect();
        assert_eq!(
            (planes[0][0], planes[1][0], planes[2][0], planes[3][0]),
            (1903, 1705, 1987, 1988),
            "component planes at (0, 0)"
        );

        let frame = decode(&bytes[at..], 4000, 3000, 2).expect("decode");
        assert_eq!(frame.len(), 4000 * 3000);
        // The specification quotes the reference frame's top-left 4x4.
        let at4 = |x: usize, y: usize| frame[y * 4000 + x];
        let corner: Vec<u16> = (0..4)
            .flat_map(|y| (0..4).map(move |x| (x, y)))
            .map(|(x, y)| at4(x, y))
            .collect();
        assert_eq!(
            corner,
            vec![
                449, 1081, 609, 1202, 1264, 996, 1069, 835, 403, 1108, 463, 1087, 1171, 812, 1119,
                890
            ]
        );
    }

    /// The tile of a GPR, located the same way the DNG module does but
    /// without pulling the TIFF parser into this module's tests.
    fn find_sample(bytes: &[u8]) -> Option<usize> {
        bytes.windows(4).position(|w| w == b"VC-5")
    }

    /// Truncated and corrupted samples must return an error, never
    /// panic and never allocate on a forged size.
    #[test]
    fn truncation_and_corruption_never_panic() {
        for path in corpus_files() {
            let bytes = std::fs::read(&path).expect("read the sample");
            let Some(at) = find_sample(&bytes) else {
                continue;
            };
            let stream = &bytes[at..];
            // Each file's own frame: the Fusion is 3104 wide, and a
            // wrong size is refused before any decoding happens.
            let (w, h) = if path.to_string_lossy().contains("FUSION") {
                (3104, 3000)
            } else {
                (4000, 3000)
            };
            for cut in 1..=16 {
                let end = stream.len() * cut / 17;
                let _ = decode(&stream[..end], w, h, 2);
            }
            // Flip bytes through the header and the first codeblock,
            // where a forged chunk size would do the most damage.
            for spot in [4, 8, 12, 36, 40, 100, 136, 140, 144, 1000, 50_000] {
                if spot >= stream.len() {
                    continue;
                }
                let mut broken = stream.to_vec();
                broken[spot] ^= 0xFF;
                let _ = decode(&broken, w, h, 2);
                let mut broken = stream.to_vec();
                broken[spot] = 0xA0;
                let _ = decode(&broken, w, h, 2);
            }
        }
    }
}
