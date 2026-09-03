//! Nikon NEF and NRW.
//!
//! A NEF is an ordinary TIFF whose IFD0 holds a small RGB thumbnail
//! and whose SubIFDs hold the full-size JPEG preview and the sensor
//! frame. The sensor IFD is the one whose PhotometricInterpretation is
//! 32803 (CFA); on the bodies that write more than one, the largest
//! wins. NRW — the raw of the Coolpix P-series and the Coolpix A — is
//! the same container with a different extension.
//!
//! Three ways of storing the samples have shipped, and the tags do
//! not tell them apart on their own — the strip length does, because
//! only a coded frame is smaller than the samples it holds:
//!
//! * 12 bits packed. The bodies that tag the frame `Compression` 1
//!   (the D1, the Coolpix E-series) pack it the way TIFF says to, most
//!   significant bit first; the modern bodies' "uncompressed (reduced
//!   to 12 bit)" mode keeps Nikon's private 34713 and packs least
//!   significant bit first. Two old quirks live here: the D100 writes
//!   ten samples into the first fifteen bytes of every sixteen and its
//!   rows hold more samples than its ImageWidth admits to, and the
//!   Coolpix E-series writes every even row of the frame before every
//!   odd one. The Coolpix compacts' NRW packs into 32-bit
//!   little-endian words — every four bytes reversed.
//! * 14 or 16 bits in plain 16-bit words, in the file's byte order.
//!   The Coolpix P-series' NRW spends two bytes a sample as well, but
//!   puts its twelve bits at the top of a big-endian word whichever
//!   way round the file itself is.
//! * Nikon's own Huffman coding, `Compression` 34713, described at
//!   [`Linearization`] and [`decompress`] below. The Z 8 and Z 9's
//!   "High Efficiency" successor to it is not decoded.
//!
//! The interesting metadata is in the makernote, which is its own
//! little TIFF: after the string `Nikon\0` and a two-byte version
//! (`02 10`, `02 11` on everything since 2003) come two spare bytes
//! and then a complete TIFF header at offset 10, with every offset
//! inside it measured from there — [`Tiff::parse_embedded`] to the
//! letter. The very first bodies (the D1) wrote a bare IFD with no
//! header at all and offsets into the main file.
//!
//! What the makernote is read for: the as-shot white balance
//! (WB_RBLevels, and the Coolpix E-series' own blob), the black level,
//! the frame the camera means to show, and — on a coded frame — the
//! curve and the initial predictors. Nikon also writes an encrypted
//! ColorBalance (0x0097) whose key is the body's serial number and
//! shutter count; nothing here decrypts it, because every body in the
//! sample corpus writes the plain WB_RBLevels beside it.
//!
//! The crop this hands out is the camera's own CropArea, not LibRaw's
//! per-model margin table: LibRaw trims a few columns off a D800E and
//! two rows off a Nikon 1 from a list of its own, and those bodies
//! record no such border anywhere in the file.
//!
//! Nothing here is derived from another decoder: the layouts come
//! from the TIFF and Exif specifications, ExifTool's published tag
//! tables, and observation of the sample files against LibRaw's
//! `unprocessed_raw` output.

use crate::bits::{BitPump, BitPumpLsb, BitPumpMsb, BitPumpMsb32, HuffTable};
use crate::formats::common;
use crate::tiff::{tags, Ifd, ImageLayout, Tiff};
use crate::{Cfa, CfaColor, Error, Format, RawData, RawImage, Rect, Result};

/// Nikon's makernote tags, by the names ExifTool's tables give them.
mod maker {
    /// WB_RBLevels: red, blue, green1, green2 as rationals.
    pub const WB_RB_LEVELS: u16 = 0x000C;
    /// ColorBalanceA, the Coolpix E-series' white balance blob.
    pub const COLOR_BALANCE_A: u16 = 0x0014;
    /// BlackLevel, four shorts, one per filter-array position.
    pub const BLACK_LEVEL: u16 = 0x003D;
    /// CropArea: left, top, width, height of the frame the camera
    /// means to show.
    pub const CROP_AREA: u16 = 0x0045;
    /// NEFCompression: which of the storage modes above was used.
    pub const COMPRESSION: u16 = 0x0093;
    /// ColorBalance, in a dozen versioned shapes and encrypted in
    /// most of them.
    pub const COLOR_BALANCE: u16 = 0x0097;
    /// NEFLinearizationTable: the curve, the initial predictors and
    /// the Huffman variant of a compressed frame.
    pub const LINEARIZATION: u16 = 0x0096;
}

/// Nikon's private `Compression` value for both the Huffman-coded
/// frames and the "uncompressed" modes of the bodies that still tag
/// them this way.
const NIKON_COMPRESSION: u32 = 34713;

// ------------------------------------------------------------ container

/// The makernote as a TIFF of its own.
///
/// Two shapes exist. Since 2003 the entry begins `Nikon\0` and a
/// version, and a whole TIFF (header included, byte order of its own,
/// offsets relative to the header) starts ten bytes in. Before that —
/// the D1 and the first Coolpix raws — the entry is a bare IFD in the
/// file's byte order whose offsets point into the file.
fn makernote<'a>(tiff: &Tiff<'a>) -> Option<Tiff<'a>> {
    let entry = tiff.exif()?.get(tags::MAKER_NOTE)?;
    let at = entry.offset;
    let bytes = tiff.bytes();
    if bytes.get(at..at + 6) == Some(b"Nikon\0") {
        // Version 2 (`02 xx`) carries a header at +10. Version 1
        // (`01 00`, some Coolpix) puts a bare IFD at +8 whose offsets
        // are the main file's.
        if bytes.get(at + 6) == Some(&2) {
            return Tiff::parse_embedded(bytes, at + 10).ok();
        }
        return Tiff::parse_at(bytes, at + 8, tiff.little_endian()).ok();
    }
    Tiff::parse_at(bytes, at, tiff.little_endian()).ok()
}

/// The sensor IFD: PhotometricInterpretation 32803 (CFA), and where a
/// body writes more than one such IFD, the biggest of them. Nikon
/// itself has only ever written one, but a file that has been through
/// a converter can carry a reduced copy beside it.
fn sensor_ifd<'a>(tiff: &'a Tiff<'a>) -> Option<&'a Ifd> {
    tiff.all()
        .into_iter()
        .filter(|ifd| ifd.get(tags::PHOTOMETRIC).and_then(|e| e.u32(0)) == Some(32803))
        .max_by_key(|ifd| {
            let side = |tag| ifd.get(tag).and_then(|e| e.u32(0)).unwrap_or(0) as u64;
            side(tags::IMAGE_WIDTH) * side(tags::IMAGE_LENGTH)
        })
}

/// The filter array from the IFD's own CFAPattern (0x828E): 0 red,
/// 1 green, 2 blue, in a 2x2 repeat. Every Nikon sensor is Bayer.
fn cfa_of(ifd: &Ifd) -> Result<Cfa> {
    let pattern = ifd
        .get(tags::CFA_PATTERN)
        .and_then(|e| e.bytes().map(|b| b.to_vec()))
        .filter(|b| b.len() >= 4)
        .ok_or_else(|| Error::Corrupt("NEF sensor IFD without a 2x2 CFAPattern".into()))?;
    let mut colors = [CfaColor::Green; 4];
    for (out, code) in colors.iter_mut().zip(pattern.iter()) {
        *out = match code {
            0 => CfaColor::Red,
            1 => CfaColor::Green,
            2 => CfaColor::Blue,
            other => return Err(Error::Corrupt(format!("NEF CFAPattern colour {other}"))),
        };
    }
    Ok(Cfa::Bayer(colors))
}

// -------------------------------------------------------------- storage

/// How the samples of a strip are laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Storage {
    /// A packed bit stream.
    Packed(Packing),
    /// One 16-bit word a sample, in the file's byte order.
    Words16,
    /// Nikon's Huffman coding.
    Huffman,
}

/// The shape of a packed bit stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Packing {
    bits: u32,
    /// Bytes from the start of one row to the start of the next.
    /// `None` for one continuous stream in which a row may begin
    /// part-way through a byte.
    row_stride: Option<usize>,
    /// Samples a group, and the group's size in bytes, for the bodies
    /// that leave whole bytes spare at the end of every group. The
    /// D100 writes ten 12-bit samples — fifteen bytes — into every
    /// sixteen, and no other body in the sample corpus groups at all.
    group: Option<(usize, usize)>,
    /// Which end of the stream a sample's first bit comes from.
    order: BitOrder,
}

/// How a packed stream's bits are ordered. Three have shipped, and
/// nothing in the TIFF structure tells them apart — the makernote
/// does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BitOrder {
    /// Most significant bit first, which is TIFF's own packing: the
    /// D1, the D100 and the Coolpix E-series.
    Msb,
    /// Least significant bit first: the modern bodies' "uncompressed
    /// (reduced to 12 bit)" mode.
    Lsb,
    /// Most significant bit first within 32-bit little-endian words —
    /// every four bytes reversed before reading. The Coolpix
    /// compacts' NRW packs this way, whatever the file's own byte
    /// order says.
    Msb32,
}

/// The bit order a frame is packed in.
///
/// `Compression` 1 is TIFF's own uncompressed, and the bodies that use
/// it pack the way TIFF says to. Under Nikon's private 34713 the
/// makernote's NEFCompression says which mode it is, and mode 6,
/// "uncompressed (reduced to 12 bit)", is the one that packs least
/// significant bit first. The Coolpix compacts' NRW is its own thing
/// again and says so in the makernote: its ColorBalanceA slot holds
/// the string `NRW ` and a version rather than a colour table.
fn bit_order(nef_compression: Option<u32>, nrw: bool) -> BitOrder {
    if nrw {
        BitOrder::Msb32
    } else if nef_compression == Some(6) {
        BitOrder::Lsb
    } else {
        BitOrder::Msb
    }
}

/// The Coolpix compacts' own block, when this is one of their NRWs.
///
/// They put it in the slot the older Coolpix used for a colour table
/// (0x0014) and mark it with the signature `NRW ` and a version. Two
/// things are read out of it: that it is there at all, which is what
/// says the frame is packed in 32-bit words, and the black level at
/// offset 32.
fn nrw_block<'a>(maker: &'a Tiff<'_>) -> Option<&'a [u8]> {
    maker
        .root()
        .get(maker::COLOR_BALANCE_A)
        .and_then(|e| e.bytes())
        .filter(|blob| blob.starts_with(b"NRW "))
}

/// How the samples are stored, and how many of them a row really
/// holds — which is not always the ImageWidth the IFD gives.
///
/// The `Compression` tag alone cannot say: the modern bodies'
/// "uncompressed (reduced to 12 bit)" mode writes packed samples under
/// Nikon's private 34713, so the honest test is whether the strips are
/// big enough to hold the frame whole — a Huffman frame is always
/// smaller than the packed one it codes. The makernote's own
/// NEFCompression (0x0093) is used only where it is decisive: it names
/// the two Huffman modes outright, and it is the only place the Z 8 and
/// Z 9's new High Efficiency codec announces itself.
fn storage(
    order: BitOrder,
    nef_compression: Option<u32>,
    width: usize,
    rows: usize,
    bits: u32,
    strips: usize,
    bytes: usize,
) -> Result<(usize, Storage)> {
    match nef_compression {
        // 1 lossy (type 1), 3 lossless, 4 lossy (type 2).
        Some(1 | 3 | 4) => return Ok((width, Storage::Huffman)),
        // 13 and 14 are "High Efficiency" and "High Efficiency*", the
        // TicoRAW-derived codec of the Z 9 and Z 8. Nothing here
        // decodes it, and it is worth saying so by name.
        Some(mode @ 13..=16) => {
            return Err(Error::Unsupported(format!(
                "NEF High Efficiency compression (NEFCompression {mode}, the Z 8 and Z 9)"
            )))
        }
        _ => {}
    }
    let words = width.saturating_mul(rows).saturating_mul(2);
    let packed_row = (width * bits as usize).div_ceil(8);
    let packed_tight = (width * rows * bits as usize).div_ceil(8);
    let packing = |row_stride, group| {
        Storage::Packed(Packing {
            bits,
            row_stride,
            group,
            order,
        })
    };
    if bytes >= words {
        // Two bytes a sample, but not always the sample itself:
        // NEFCompression 7 is "unpacked 12 bits", the sample in the top
        // of a big-endian word whatever byte order the file itself is
        // in. Reading it as a group of one sample in every two bytes
        // takes the twelve bits from the top of the pair and drops the
        // four spare ones below them.
        if nef_compression == Some(7) {
            return Ok((width, packing(Some(width * 2), Some((1, 2)))));
        }
        return Ok((width, Storage::Words16));
    }
    // One strip whose rows are longer than the samples need, and whose
    // length divides into sixteen-byte groups: the D100, which packs
    // ten samples into the first fifteen bytes of every group and
    // leaves the sixteenth alone. Its rows hold more samples than the
    // IFD's ImageWidth admits to, and the group count is what says how
    // many — LibRaw reads the same 3040 out of a frame the IFD calls
    // 3034 wide.
    if strips == 1 && bits == 12 && rows > 0 && bytes.is_multiple_of(rows) {
        let stride = bytes / rows;
        let grouped = stride / 16 * 10;
        if stride != packed_row && stride.is_multiple_of(16) && grouped >= width {
            return Ok((grouped, packing(Some(stride), Some((10, 16)))));
        }
    }
    if bytes >= packed_row.saturating_mul(rows) {
        // Room for every row to start on a byte boundary.
        Ok((width, packing(Some(packed_row), None)))
    } else if bytes >= packed_tight {
        Ok((width, packing(None, None)))
    } else {
        Ok((width, Storage::Huffman))
    }
}

/// Unpack `rows` rows of `width` packed samples.
///
/// Past the end of the data the pump reads zeros, so a truncated strip
/// yields a short-but-black frame rather than an error.
fn unpack_packed(data: &[u8], width: usize, rows: usize, packing: Packing, out: &mut [u16]) {
    let bits = packing.bits;
    let fill = |data: &[u8], out: &mut [u16]| match packing.order {
        BitOrder::Msb => {
            let mut pump = BitPumpMsb::new(data);
            out.iter_mut().for_each(|s| *s = pump.get(bits) as u16);
        }
        BitOrder::Lsb => {
            let mut pump = BitPumpLsb::new(data);
            out.iter_mut().for_each(|s| *s = pump.get(bits) as u16);
        }
        BitOrder::Msb32 => {
            let mut pump = BitPumpMsb32::new(data);
            out.iter_mut().for_each(|s| *s = pump.get(bits) as u16);
        }
    };
    let Some(stride) = packing.row_stride else {
        let end = (width * rows).min(out.len());
        fill(data, &mut out[..end]);
        return;
    };
    for (row, samples) in out.chunks_mut(width).enumerate().take(rows) {
        let row = data.get(row * stride..).unwrap_or_default();
        match packing.group {
            None => fill(row, samples),
            Some((per_group, group_bytes)) => {
                for (group, samples) in samples.chunks_mut(per_group).enumerate() {
                    fill(row.get(group * group_bytes..).unwrap_or_default(), samples);
                }
            }
        }
    }
}

/// Put the rows of an E-series Coolpix frame back in order.
///
/// The Coolpix NEFs of the E5000/E8800 generation are written as two
/// fields: every even row of the frame, then every odd one. Nothing in
/// the file says so — the model name is the only mark, and the
/// giveaway in a sample is that the second stored row is the frame's
/// third.
fn deinterlace_fields(data: &mut [u16], width: usize, height: usize) {
    let mut sorted = vec![0u16; data.len()];
    let fields = height.div_ceil(2);
    for (stored, row) in data.chunks_exact(width).enumerate().take(height) {
        let target = if stored < fields {
            stored * 2
        } else {
            (stored - fields) * 2 + 1
        };
        sorted[target * width..(target + 1) * width].copy_from_slice(row);
    }
    data.copy_from_slice(&sorted);
}

/// Whether a body writes its rows in two fields: the Coolpix E-series
/// and nothing else, which its model name (`E8800`) gives away.
fn interlaced_fields(model: &str) -> bool {
    let name = model.trim().trim_start_matches("NIKON ").trim();
    let mut chars = name.chars();
    chars.next() == Some('E') && !name[1..].is_empty() && chars.all(|c| c.is_ascii_digit())
}

/// Unpack 16-bit words in the file's byte order.
fn unpack_words(data: &[u8], little_endian: bool, out: &mut [u16]) {
    let (words, _) = data.as_chunks::<2>();
    for (sample, word) in out.iter_mut().zip(words) {
        *sample = if little_endian {
            u16::from_le_bytes(*word)
        } else {
            u16::from_be_bytes(*word)
        };
    }
}

// ------------------------------------------------------------ compressed

/// Nikon's Huffman coding, and the makernote table that configures it.
///
/// NEFLinearizationTable (makernote 0x0096) is not only a curve. Its
/// bytes are, in the makernote's byte order:
///
/// ```text
///   0   version, two bytes
///  (2)  a 2110-byte block only the `0x49` version carries
///   +0  four shorts: the initial predictors, one per filter position
///   +8  a short: how many points the curve has
///  +10  the curve
/// ```
///
/// Three versions have shipped, and the version is also what says
/// which Huffman variant coded the frame:
///
/// * `0x46` — "lossless". The curve is a straight ramp and is not
///   stored at all (the point count is there, the points are not), so
///   a sample means what it says and saturation is the full depth.
/// * `0x44 0x20` — "lossy (type 2)". 257 points spaced `2^bits / 256`
///   apart, linearly interpolated between: the stored sample is an
///   index into a curve that is the identity at the bottom and
///   stretches towards the top, which is where the lost precision
///   went. A short at offset 562 — just past the curve and the first
///   of the two Huffman tables the tag also carries — holds the row at
///   which the second table takes over on the bodies that switch.
/// * `0x44 0x10` — "lossy (type 1)", the older bodies. The curve is
///   stored point for point and is shorter than the sample depth: the
///   D60's is 683 long, so a sample is an index into 683 values, not a
///   12-bit number.
///
/// Everything after the curve (a length and thirty-odd bytes, twice
/// over on the lossy versions) is left alone: the six Huffman tables
/// below are fixed, and a frame decodes to the oracle's samples
/// exactly without reading it.
struct Linearization {
    /// The first version byte, which picks the Huffman variant.
    version: u8,
    /// Initial vertical predictors, `[row parity][column parity]`.
    vpred: [[i32; 2]; 2],
    /// Sample value for each stored index. Its length is also the
    /// clamp: a prediction outside it is pinned to the ends.
    curve: Vec<u16>,
    /// The row at which the second Huffman table takes over, 0 for
    /// the frames coded with one table throughout.
    split: usize,
}

impl Linearization {
    /// Parse the tag. `bits` is the sensor IFD's BitsPerSample, which
    /// sets the curve's domain where the tag does not carry one.
    fn parse(blob: &[u8], little_endian: bool, bits: u32) -> Result<Linearization> {
        let short = |at: usize| -> Result<u16> {
            let pair = blob
                .get(at..at + 2)
                .ok_or_else(|| Error::Corrupt("NEF linearization table cut short".into()))?;
            let pair = [pair[0], pair[1]];
            Ok(if little_endian {
                u16::from_le_bytes(pair)
            } else {
                u16::from_be_bytes(pair)
            })
        };
        let version = *blob
            .first()
            .ok_or_else(|| Error::Corrupt("empty NEF linearization table".into()))?;
        let minor = blob.get(1).copied().unwrap_or(0);
        // The 0x49 version parks 2110 bytes of something else between
        // the version and the predictors.
        let mut at = if version == 0x49 { 2 + 2110 } else { 2 };
        let vpred = [
            [short(at)? as i32, short(at + 2)? as i32],
            [short(at + 4)? as i32, short(at + 6)? as i32],
        ];
        at += 8;
        let points = short(at)? as usize;
        at += 2;

        let max = 1usize << bits.min(16);
        let (curve, split) = if version == 0x46 || points < 2 {
            // Lossless: the ramp, generated rather than read.
            ((0..max).map(|i| i as u16).collect(), 0)
        } else if minor == 0x20 {
            let step = max / (points - 1);
            if step == 0 {
                return Err(Error::Corrupt(
                    "NEF curve with more points than samples".into(),
                ));
            }
            // The points sit `step` apart and the values between them
            // are the straight line from one to the next. The last
            // point lands one past the end of the domain, which is
            // what the final run interpolates towards.
            let mut anchors = Vec::with_capacity(points);
            for i in 0..points {
                anchors.push(short(at + 2 * i)? as u32);
            }
            let mut curve = Vec::with_capacity(max);
            for i in 0..max {
                let (whole, part) = (i / step, i % step);
                let low = anchors.get(whole).copied().unwrap_or(0);
                let high = anchors.get(whole + 1).copied().unwrap_or(low);
                let value = low as i64 + (high as i64 - low as i64) * part as i64 / step as i64;
                curve.push(value.clamp(0, 0xffff) as u16);
            }
            // A short at a fixed offset in the tag, past the curve and
            // the first of its two Huffman tables.
            let split = short(562).unwrap_or(0) as usize;
            (curve, split)
        } else {
            // The whole curve, point for point; it is shorter than the
            // sample depth and its length is the clamp.
            let mut curve = Vec::with_capacity(points);
            for i in 0..points {
                curve.push(short(at + 2 * i)?);
            }
            (curve, 0)
        };
        if curve.is_empty() {
            return Err(Error::Corrupt("NEF curve with no points".into()));
        }
        Ok(Linearization {
            version,
            vpred,
            curve,
            split,
        })
    }

    /// The saturation point: the curve's last value, which on the
    /// lossy versions is below the depth's own ceiling.
    fn white_level(&self) -> f32 {
        self.curve.iter().copied().max().unwrap_or(0) as f32
    }
}

/// The Huffman tables, in JPEG's shape: how many codes there are of
/// each length 1..=16, then the symbols in canonical order. A symbol's
/// low four bits are how many bits of difference follow it; the high
/// four are a left shift, which is how the tables used after a split
/// code a coarse difference in fewer bits.
///
/// Which table a frame uses follows from the linearization version and
/// the sample depth: `0x46` (lossless) picks the second of each pair,
/// anything else the first, and 14-bit frames take the 14-bit pair.
///
/// These were read back out of the sample files rather than copied
/// from anywhere: for a frame whose samples are known (LibRaw's
/// `unprocessed_raw` output) every difference and therefore every
/// symbol along the stream is known too, which pins each symbol's code
/// down to one possibility. Every code below is one that a D850, D7200,
/// D3500, Z 30, D300S, D3300, Nikon 1 V1 or D60 frame actually used.
struct Tree {
    counts: [u8; 16],
    symbols: &'static [u8],
}

/// 12-bit, lossy.
const TREE_12_LOSSY: Tree = Tree {
    counts: [0, 1, 5, 1, 1, 1, 1, 1, 2, 0, 0, 0, 0, 0, 0, 0],
    symbols: &[5, 4, 3, 6, 2, 7, 1, 0, 8, 9, 11, 10, 12],
};
/// 12-bit, lossless.
const TREE_12_LOSSLESS: Tree = Tree {
    counts: [0, 1, 4, 2, 3, 1, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    symbols: &[5, 4, 6, 3, 7, 2, 8, 1, 9, 0, 10, 11, 12],
};
/// 14-bit, lossy.
const TREE_14_LOSSY: Tree = Tree {
    counts: [0, 1, 4, 3, 1, 1, 1, 1, 1, 2, 0, 0, 0, 0, 0, 0],
    symbols: &[5, 6, 4, 7, 8, 3, 9, 2, 1, 0, 10, 11, 12, 13, 14],
};
/// 14-bit, lossless.
const TREE_14_LOSSLESS: Tree = Tree {
    counts: [0, 1, 4, 2, 2, 3, 1, 2, 0, 0, 0, 0, 0, 0, 0, 0],
    symbols: &[7, 6, 8, 5, 9, 4, 10, 3, 11, 12, 2, 0, 1, 13, 14],
};
/// 12-bit lossy, from the split row down. This is where the shifted
/// symbols live: a difference nine bits wide read six bits deep is the
/// commonest thing in the frame, because the rows below the split have
/// been lifted and their noise with them.
///
/// Two of its twelve codes never came up in the D5000 frame it was
/// read out of — 8 million pixels of it — so their symbols are not
/// known. `0x5a` fills the first because it is the only symbol that
/// continues the run of widths in its group (10, 8, 7, 6, 5, each read
/// five bits deep); the second is left as [`UNKNOWN`] rather
/// than guessed at.
const TREE_12_LOSSY_SPLIT: Tree = Tree {
    counts: [0, 1, 5, 1, 1, 1, 1, 2, 0, 0, 0, 0, 0, 0, 0, 0],
    symbols: &[
        0x39, 0x5a, 0x38, 0x27, 0x16, 0x05, 0x04, 0x03, 0x02, 0x01, 0x00, UNKNOWN,
    ],
};

/// The one code in the second table whose symbol no sample has ever
/// shown. A frame that uses it stops with an error rather than
/// quietly garbling from that pixel on.
const UNKNOWN: u8 = 0xf0;

/// The table for a frame's version and depth.
fn tree_for(version: u8, bits: u32) -> &'static Tree {
    match (version, bits) {
        (0x46, 14) => &TREE_14_LOSSLESS,
        (0x46, _) => &TREE_12_LOSSLESS,
        (_, 14) => &TREE_14_LOSSY,
        (_, _) => &TREE_12_LOSSY,
    }
}

/// The table a lossy frame changes to at its split row. Only the
/// 12-bit one is known: the split is a 2008-vintage feature (the D40,
/// the D5000, the D90 era) and no 14-bit body in the sample corpus
/// sets one, so there is no frame to read a 14-bit second table out
/// of and none is guessed at.
fn split_tree_for(bits: u32) -> Result<&'static Tree> {
    if bits == 12 {
        Ok(&TREE_12_LOSSY_SPLIT)
    } else {
        Err(Error::Unsupported(
            "NEF whose Huffman table changes partway down a 14-bit frame".into(),
        ))
    }
}

/// The difference a symbol and the bits after it stand for.
///
/// The symbol's low four bits are the width; the value is read that
/// wide and sign-extended the way lossless JPEG does it, a leading
/// zero bit meaning negative. The symbol's high four bits are a left
/// shift: the value is read `width - shift` bits wide instead and
/// moved up into place with its low bit set, so it lands in the middle
/// of the step it stands for rather than at the bottom. None of the
/// four tables above uses a shift — it belongs to the second table of
/// each lossy pair, which [`decompress`] refuses — but the encoding is
/// the symbol's, not the table's, so it is decoded here.
fn difference(symbol: u16, pump: &mut impl BitPump) -> Result<i32> {
    let len = (symbol & 15) as u32;
    let shift = (symbol >> 4) as u32;
    if shift > len {
        return Err(Error::Corrupt(format!("NEF difference symbol {symbol:#x}")));
    }
    if len == 0 {
        return Ok(0);
    }
    let raw = pump.get(len - shift) as i32;
    let mut diff = (((raw << 1) + 1) << shift) >> 1;
    if diff & (1 << (len - 1)) == 0 {
        diff -= (1 << len) - i32::from(shift == 0);
    }
    Ok(diff)
}

/// Decode a Huffman-coded frame into `out`.
///
/// The stream begins at the first byte of the strip — no header, no
/// marker — and is read most significant bit first with no stuffing of
/// any kind. Each pixel is one Huffman symbol and the difference bits
/// after it (see [`difference`]).
///
/// The prediction is horizontal but two pixels back, so each filter
/// colour predicts from its own kind: `hpred[col & 1]`. The first two
/// columns of a row have no such neighbour and predict from the same
/// two columns of the row before last — one running predictor per row
/// parity, seeded from the linearization table, which is why a frame's
/// rows come in pairs. The result is an index into the curve, clamped
/// to it; the curve turns it into the sample.
fn decompress(
    data: &[u8],
    width: usize,
    height: usize,
    bits: u32,
    table: &Linearization,
    out: &mut [u16],
) -> Result<()> {
    let tree = tree_for(table.version, bits);
    let split = (table.split != 0 && table.split < height).then_some(table.split);
    let second = match split {
        Some(_) => Some(HuffTable::new(
            &split_tree_for(bits)?.counts,
            split_tree_for(bits)?.symbols,
        )?),
        None => None,
    };
    let mut huffman = HuffTable::new(&tree.counts, tree.symbols)?;
    let mut pump = BitPumpMsb::new(data);
    let mut vpred = table.vpred;
    let mut hpred = [0i32; 2];
    let ceiling = table.curve.len() as i32 - 1;

    for row in 0..height {
        // The bodies that lift their shadows change table partway
        // down, mid-stream: the bit position carries straight on and
        // so do the predictors.
        if split == Some(row) {
            if let Some(second) = second.clone() {
                huffman = second;
            }
        }
        for col in 0..width {
            let symbol = huffman.decode(&mut pump);
            // The split table's other unobserved slot is a guess; a
            // frame that reaches it is worth hearing about, since a
            // wrong width would garble the rest silently.
            if symbol == 0x5a {
                log::warn!("NEF: split-table code 0x5a decoded at row {row}, column {col}; no sample has verified it");
            }
            if symbol == UNKNOWN as u16 {
                return Err(Error::Unsupported(format!(
                    "NEF using the one code of Nikon's second Huffman table that no \
                     sample has ever shown (row {row}, column {col})"
                )));
            }
            let diff = difference(symbol, &mut pump)?;
            let predicted = if col < 2 {
                vpred[row & 1][col] = vpred[row & 1][col].wrapping_add(diff);
                hpred[col] = vpred[row & 1][col];
                hpred[col]
            } else {
                hpred[col & 1] = hpred[col & 1].wrapping_add(diff);
                hpred[col & 1]
            };
            out[row * width + col] = table.curve[predicted.clamp(0, ceiling) as usize];
        }
    }
    Ok(())
}

// -------------------------------------------------------------- metadata

/// The as-shot white balance as R, G, B, G2 multipliers with green 1.
///
/// WB_RBLevels (0x000C) is four rationals in the order red, blue,
/// green1, green2 — the tag's own name says which two come first — and
/// every body since the D1 writes it but the D100, which has the first
/// version of ColorBalance (0x0097) instead: the same four numbers as
/// plain shorts at offset 72. The Coolpix E-series has neither; its
/// ColorBalanceA blob (0x0014) holds red and blue as big-endian
/// 256ths at offset 1248, with green implicitly 1.
fn white_balance(maker: &Tiff<'_>) -> Option<[f32; 4]> {
    let root = maker.root();
    let little_endian = maker.little_endian();
    if let Some(entry) = root.get(maker::WB_RB_LEVELS) {
        let value = |i: usize| entry.f64(i).map(|v| v as f32);
        let (red, blue, green) = (value(0)?, value(1)?, value(2).unwrap_or(1.0));
        let green2 = value(3).unwrap_or(green);
        if green > 0.0 && green2 > 0.0 && red > 0.0 && blue > 0.0 {
            return Some([red / green, 1.0, blue / green, green2 / green]);
        }
    }
    // ColorBalance's first version is four plain shorts, red, blue and
    // the two greens, at a fixed offset. The D100 is the one body in
    // the sample corpus that writes no WB_RBLevels beside it. From
    // version 0204 on the block is encrypted with the body's serial
    // number and shutter count and this decoder does not undo it —
    // nothing needs it to, because every body that writes an
    // encrypted one writes WB_RBLevels as well.
    if let Some(blob) = root.get(maker::COLOR_BALANCE).and_then(|e| e.bytes()) {
        if blob.starts_with(b"0100") {
            if let Some(levels) = blob.get(72..80) {
                let short = |i: usize| {
                    let pair = [levels[i * 2], levels[i * 2 + 1]];
                    if little_endian {
                        u16::from_le_bytes(pair)
                    } else {
                        u16::from_be_bytes(pair)
                    }
                };
                let (red, blue, green) = (short(0) as f32, short(1) as f32, short(2) as f32);
                let green2 = short(3) as f32;
                if red > 0.0 && blue > 0.0 && green > 0.0 && green2 > 0.0 {
                    return Some([red / green, 1.0, blue / green, green2 / green]);
                }
            }
        }
    }
    if let Some(blob) = root.get(maker::COLOR_BALANCE_A).and_then(|e| e.bytes()) {
        // Only the 2560-byte shape has been seen, and its layout is
        // fixed; a blob of another length is a variant this does not
        // know the offsets of.
        if blob.len() == 2560 {
            let at =
                |offset: usize| u16::from_be_bytes([blob[offset], blob[offset + 1]]) as f32 / 256.0;
            let (red, blue) = (at(1248), at(1250));
            if red > 0.0 && blue > 0.0 {
                return Some([red, 1.0, blue, 1.0]);
            }
        }
    }
    None
}

/// The black level, one value per filter-array position.
///
/// BlackLevel (0x003D) is written in 14-bit units whatever the frame's
/// depth, so a 12-bit file's 400 means 100 — the sample corpus agrees
/// with LibRaw on that for every body that carries the tag (D850,
/// D7200, D3500, Z 30). The Coolpix compacts' NRW puts a single level
/// in its own block instead, at a fixed offset past the `NRW `
/// signature, in the samples' own units. Bodies older than either
/// record no black at all and their frames sit at zero.
fn black_levels(maker: &Tiff<'_>, bits: u32) -> Option<[f32; 4]> {
    if let Some(blob) = nrw_block(maker) {
        // Little-endian, as the two bodies that write the block are;
        // it is a vendor blob and carries no byte order of its own.
        let level = blob.get(32..34)?;
        return Some([u16::from_le_bytes([level[0], level[1]]) as f32; 4]);
    }
    let entry = maker.root().get(maker::BLACK_LEVEL)?;
    let shift = 14u32.saturating_sub(bits);
    let level = |i: usize| entry.u32(i).map(|v| (v >> shift) as f32);
    let first = level(0)?;
    Some([
        first,
        level(1).unwrap_or(first),
        level(2).unwrap_or(first),
        level(3).unwrap_or(first),
    ])
}

/// Masked columns at the right edge of a frame whose body records no
/// CropArea: the D5000 keeps 42 of them, the D800E 46, the Coolpix A
/// 52, the D300S 32. They are not the picture and not sensor black
/// either — the readout leaves them at a constant (255/256 on the
/// D5000, 599 then 0 on the Coolpix A), so shown, they are a coloured
/// band. That constancy is the signature: a masked column varies down
/// the rows only by read noise, while a column of a photograph varies
/// by orders of magnitude more. The band is the run of such columns
/// ending at the frame's edge, at most 64 wide and followed by columns
/// that do vary; a lens-cap frame, flat everywhere, reaches the limit
/// and is left alone.
fn masked_right_columns(data: &[u16], width: usize, height: usize) -> usize {
    const LOOK: usize = 64;
    if width < 4 * LOOK || height < 8 || data.len() < width * height {
        return 0;
    }
    let rows = (height / 4..height * 3 / 4).step_by(2);
    // Standard deviation down the middle rows, in raw units.
    let spread = |column: usize| -> f64 {
        let (mut sum, mut squares, mut n) = (0f64, 0f64, 0f64);
        for row in rows.clone() {
            let v = data[row * width + column] as f64;
            sum += v;
            squares += v * v;
            n += 1.0;
        }
        let mean = sum / n.max(1.0);
        (squares / n.max(1.0) - mean * mean).max(0.0).sqrt()
    };
    // Masked columns sit at read-noise level (1–6 on every body seen)
    // while the picture beside them spreads tens to thousands; both
    // conditions, so a flat sky reaching the edge — flat inside too —
    // is not taken for a border.
    let reference: [f64; 2] = std::array::from_fn(|parity| {
        let mut spreads: Vec<f64> = (width - 2 * LOOK..width - LOOK)
            .filter(|c| c % 2 == parity)
            .map(spread)
            .collect();
        spreads.sort_by(|a, b| a.total_cmp(b));
        spreads[spreads.len() / 2]
    });
    let masked = |column: usize| {
        let sd = spread(column);
        sd < 8.0 && sd * 10.0 < reference[column % 2]
    };
    let mut band = 0;
    while band < LOOK && masked(width - 1 - band) {
        band += 1;
    }
    if !(2..LOOK).contains(&band) || masked(width - 1 - band) || masked(width - 2 - band) {
        return 0;
    }
    band
}

/// The area the camera means to show: CropArea (0x0045) as left, top,
/// width, height. Bodies older than the tag record none; see
/// `masked_right_columns` for the border those keep.
fn crop(maker: &Tiff<'_>, width: usize, height: usize) -> Rect {
    let whole = Rect {
        x: 0,
        y: 0,
        width,
        height,
    };
    let Some(entry) = maker.root().get(maker::CROP_AREA) else {
        return whole;
    };
    let value = |i: usize| entry.u32(i).map(|v| v as usize);
    let (Some(x), Some(y), Some(w), Some(h)) = (value(0), value(1), value(2), value(3)) else {
        return whole;
    };
    let outside = |start: usize, span: usize, whole: usize| {
        start.checked_add(span).is_none_or(|end| end > whole)
    };
    if w == 0 || h == 0 || outside(x, w, width) || outside(y, h, height) {
        return whole;
    }
    Rect {
        x,
        y,
        width: w,
        height: h,
    }
}

// ---------------------------------------------------------------- decode

pub fn decode(bytes: &[u8]) -> Result<RawImage> {
    let tiff = Tiff::parse(bytes)?;
    let (make, model) = tiff.make_model();
    if !make.to_ascii_uppercase().starts_with("NIKON") {
        return Err(Error::NotRaw);
    }
    let ifd = sensor_ifd(&tiff).ok_or_else(|| {
        // The COOLSCAN film scanners write their scans into a NEF with
        // an RGB SubIFD and no filter array at all: three samples a
        // pixel of scanner output, not sensor data.
        Error::Unsupported(format!(
            "{model}: a NEF with no CFA image (a COOLSCAN scan?)"
        ))
    })?;
    let layout = ImageLayout::of(&tiff, ifd)?;
    let (width, height) = (layout.width, layout.height);
    if width == 0 || height == 0 || width > 1 << 16 || height > 1 << 16 {
        return Err(Error::Corrupt(format!("NEF frame of {width}x{height}")));
    }
    let cfa = cfa_of(ifd)?;
    let maker = makernote(&tiff);

    let bits = layout.bits_per_sample;
    if !matches!(bits, 12 | 14 | 16) {
        return Err(Error::Unsupported(format!("NEF with {bits}-bit samples")));
    }
    if layout.samples_per_pixel != 1 {
        return Err(Error::Unsupported(format!(
            "NEF with {} samples a pixel",
            layout.samples_per_pixel
        )));
    }

    let nef_compression = maker
        .as_ref()
        .and_then(|m| m.root().get(maker::COMPRESSION))
        .and_then(|e| e.u32(0));
    if !matches!(layout.compression, 1 | NIKON_COMPRESSION) {
        return Err(Error::Unsupported(format!(
            "NEF with TIFF compression {}",
            layout.compression
        )));
    }
    let total: usize = layout.chunks.iter().map(|(_, len)| *len).sum();
    // NEFCompression is Nikon's own word and only means anything under
    // Nikon's own compression tag.
    let nef_compression = nef_compression.filter(|_| layout.compression == NIKON_COMPRESSION);
    let nrw = maker.as_ref().and_then(nrw_block).is_some();
    let (width, store) = storage(
        bit_order(nef_compression, nrw),
        nef_compression,
        width,
        height,
        bits,
        layout.chunks.len(),
        total,
    )?;
    // Every storage keeps at least a bit a sample, so the strips bound
    // the frame a forged header may claim.
    let samples = crate::frame_samples(width, height, 1)?;
    if total.saturating_mul(8) < samples {
        return Err(Error::Corrupt(format!(
            "NEF frame of {samples} samples in {total} bytes of strips"
        )));
    }
    let mut data = vec![0u16; samples];
    let mut linearization = None;
    match store {
        Storage::Packed(packing) => {
            // Every strip holds RowsPerStrip whole rows and starts its
            // own bit stream: the D1 writes 67 strips of 20 rows.
            let mut row = 0;
            for (start, len) in &layout.chunks {
                if row >= height {
                    break;
                }
                let rows = layout.rows_per_chunk.min(height - row);
                let out = &mut data[row * width..(row + rows) * width];
                unpack_packed(&bytes[*start..*start + *len], width, rows, packing, out);
                row += rows;
            }
            if interlaced_fields(&model) {
                deinterlace_fields(&mut data, width, height);
            }
        }
        Storage::Words16 => {
            let mut row = 0;
            for (start, len) in &layout.chunks {
                if row >= height {
                    break;
                }
                let rows = layout.rows_per_chunk.min(height - row);
                let out = &mut data[row * width..(row + rows) * width];
                unpack_words(&bytes[*start..*start + *len], tiff.little_endian(), out);
                row += rows;
            }
        }
        Storage::Huffman => {
            let blob = maker
                .as_ref()
                .and_then(|m| m.root().get(maker::LINEARIZATION))
                .and_then(|e| e.bytes())
                .ok_or_else(|| {
                    Error::Corrupt("Huffman-coded NEF without a linearization table".into())
                })?;
            let little_endian = maker.as_ref().is_some_and(|m| m.little_endian());
            let table = Linearization::parse(blob, little_endian, bits)?;
            // One strip, always: nothing splits a Nikon Huffman stream.
            let (start, len) = layout.chunks[0];
            decompress(
                &bytes[start..start + len],
                width,
                height,
                bits,
                &table,
                &mut data,
            )?;
            linearization = Some(table);
        }
    }

    let mut raw = RawImage::new(Format::Nef, width, height, 1, RawData::U16(data), cfa);
    raw.set_camera(&make, &model);
    // A lossy frame's samples stop at the top of its curve, which is
    // below the depth's own ceiling; everything else saturates at the
    // depth.
    raw.white_level = match &linearization {
        Some(table) => table.white_level(),
        None => ((1u32 << bits) - 1) as f32,
    };
    if let Some(maker) = &maker {
        if let Some(coeffs) = white_balance(maker) {
            raw.wb_coeffs = coeffs;
        }
        if let Some(black) = black_levels(maker, bits) {
            raw.black_levels = black;
        }
        raw.crop = crop(maker, width, height);
    }
    // Bodies from before CropArea keep their masked right-hand columns
    // in the frame; find them in the data rather than in a table.
    if raw.crop.width == width && raw.crop.x == 0 {
        if let RawData::U16(data) = &raw.data {
            let masked = masked_right_columns(data, width, height);
            if masked > 0 {
                raw.crop.width = (width - masked) & !1;
            }
        }
    }
    raw.orientation = common::orientation(&tiff);
    raw.metadata = common::metadata(&tiff);
    raw.preview = common::largest_jpeg(&tiff);
    raw.apply_camera_table();
    Ok(raw)
}

pub fn preview(bytes: &[u8]) -> Result<Option<Vec<u8>>> {
    let tiff = Tiff::parse(bytes)?;
    Ok(common::largest_jpeg(&tiff))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------- units

    /// A 12-bit `Packed` storage for the tests.
    fn packed(row_stride: Option<usize>, order: BitOrder) -> Storage {
        Storage::Packed(Packing {
            bits: 12,
            row_stride,
            group: None,
            order,
        })
    }

    /// Its packing, for the unpacker's own tests.
    fn packing(row_stride: Option<usize>, order: BitOrder) -> Packing {
        let Storage::Packed(packing) = packed(row_stride, order) else {
            unreachable!()
        };
        packing
    }

    #[test]
    fn twelve_bit_samples_unpack_from_either_end_of_the_stream() {
        // TIFF's order: AB CD EF -> 0xABC, 0xDEF.
        let mut out = [0u16; 2];
        unpack_packed(
            &[0xab, 0xcd, 0xef],
            2,
            1,
            packing(None, BitOrder::Msb),
            &mut out,
        );
        assert_eq!(out, [0xabc, 0xdef]);
        // Nikon's own: the same bytes the other way round.
        unpack_packed(
            &[0xab, 0xcd, 0xef],
            2,
            1,
            packing(None, BitOrder::Lsb),
            &mut out,
        );
        assert_eq!(out, [0xdab, 0xefc]);
        // Rows padded to a byte boundary: an odd width leaves half a
        // byte at the end of every row.
        let mut out = [0u16; 2];
        unpack_packed(
            &[0x12, 0x30, 0x45, 0x60],
            1,
            2,
            packing(Some(2), BitOrder::Msb),
            &mut out,
        );
        assert_eq!(out, [0x123, 0x456]);
        // ... and the same bytes read as one continuous stream are a
        // different pair of samples: the second starts mid-byte.
        unpack_packed(
            &[0x12, 0x30, 0x45, 0x60],
            1,
            2,
            packing(None, BitOrder::Msb),
            &mut out,
        );
        assert_eq!(out, [0x123, 0x045]);
        // Past the end the pump reads zeros rather than panicking.
        let mut out = [0u16; 4];
        unpack_packed(&[0xff], 4, 1, packing(None, BitOrder::Msb), &mut out);
        assert_eq!(out, [0xff0, 0, 0, 0]);
        // The D100's grouping: two samples in the first three bytes
        // of every four, the fourth skipped. Four samples in a row of
        // eight bytes are two such groups...
        let mut out = [0u16; 4];
        let grouped = Packing {
            group: Some((2, 4)),
            ..packing(Some(8), BitOrder::Msb)
        };
        let bytes = [0xab, 0xcd, 0xef, 0xff, 0x12, 0x34, 0x56, 0xff];
        unpack_packed(&bytes, 4, 1, grouped, &mut out);
        assert_eq!(out, [0xabc, 0xdef, 0x123, 0x456]);
        // ... and the same bytes as two rows of one group each.
        let grouped = Packing {
            row_stride: Some(4),
            ..grouped
        };
        unpack_packed(&bytes, 2, 2, grouped, &mut out);
        assert_eq!(out, [0xabc, 0xdef, 0x123, 0x456]);
    }

    #[test]
    fn the_bit_order_comes_from_the_makernote() {
        // Plain TIFF uncompressed, and every Nikon mode but one.
        assert_eq!(bit_order(None, false), BitOrder::Msb);
        assert_eq!(bit_order(Some(2), false), BitOrder::Msb);
        // "Uncompressed (reduced to 12 bit)".
        assert_eq!(bit_order(Some(6), false), BitOrder::Lsb);
        // The Coolpix compacts' NRW, whatever else it says.
        assert_eq!(bit_order(None, true), BitOrder::Msb32);
        assert_eq!(bit_order(Some(6), true), BitOrder::Msb32);
    }

    #[test]
    fn thirty_two_bit_words_are_read_back_to_front() {
        // Four bytes reversed, then twelve bits at a time.
        let mut out = [0u16; 2];
        unpack_packed(
            &[0x0c, 0xce, 0xc0, 0x0c],
            2,
            1,
            packing(None, BitOrder::Msb32),
            &mut out,
        );
        assert_eq!(out, [0x0cc, 0x0ce]);
    }

    #[test]
    fn the_coolpix_e_series_writes_its_rows_in_two_fields() {
        assert!(interlaced_fields("E8800"));
        assert!(interlaced_fields("NIKON E5400"));
        assert!(!interlaced_fields("COOLPIX B700"));
        assert!(!interlaced_fields("NIKON D850"));
        assert!(!interlaced_fields("E"));
        // Five rows: stored 0 2 4 1 3, read back 0 1 2 3 4.
        let mut data = [0, 2, 4, 1, 3];
        deinterlace_fields(&mut data, 1, 5);
        assert_eq!(data, [0, 1, 2, 3, 4]);
        let mut data = [0, 2, 1, 3];
        deinterlace_fields(&mut data, 1, 4);
        assert_eq!(data, [0, 1, 2, 3]);
    }

    #[test]
    fn sixteen_bit_samples_follow_the_files_byte_order() {
        let mut out = [0u16; 2];
        unpack_words(&[0x34, 0x12, 0xff, 0x3f], true, &mut out);
        assert_eq!(out, [0x1234, 0x3fff]);
        unpack_words(&[0x34, 0x12, 0xff, 0x3f], false, &mut out);
        assert_eq!(out, [0x3412, 0xff3f]);
        // A short strip leaves the rest of the row as it found it.
        let mut out = [7u16; 3];
        unpack_words(&[0x34, 0x12], true, &mut out);
        assert_eq!(out, [0x1234, 7, 7]);
    }

    #[test]
    fn storage_follows_the_strip_length() {
        // 8288x5520: two bytes a pixel, three bytes to two pixels,
        // and a compressed stream that is smaller than either.
        let (w, h) = (8288, 5520);
        assert_eq!(
            storage(BitOrder::Msb, None, w, h, 14, 1, w * h * 2).unwrap(),
            (w, Storage::Words16)
        );
        assert_eq!(
            storage(BitOrder::Msb, None, w, h, 12, 1, w * h * 3 / 2).unwrap(),
            (w, packed(Some(w * 3 / 2), BitOrder::Msb))
        );
        // Nikon's own compression tag on a packed frame also says
        // which way round its bits go.
        assert_eq!(
            storage(BitOrder::Lsb, Some(6), w, h, 12, 1, w * h * 3 / 2).unwrap(),
            (w, packed(Some(w * 3 / 2), BitOrder::Lsb))
        );
        assert_eq!(
            storage(BitOrder::Msb, None, w, h, 12, 1, 36_987_742).unwrap(),
            (w, Storage::Huffman)
        );
        // An odd width: four rows of five 12-bit samples are 30 bytes
        // packed tight and 32 with every row on a byte boundary.
        assert_eq!(
            storage(BitOrder::Msb, None, 5, 4, 12, 1, 30).unwrap(),
            (5, packed(None, BitOrder::Msb))
        );
        assert_eq!(
            storage(BitOrder::Msb, None, 5, 4, 12, 1, 32).unwrap(),
            (5, packed(Some(8), BitOrder::Msb))
        );
        assert_eq!(
            storage(BitOrder::Msb, None, 5, 4, 12, 1, 29).unwrap(),
            (5, Storage::Huffman)
        );
        // The D100: 2024 rows of 4864 bytes, ten samples to every
        // sixteen, which is 3040 a row and not the 3034 the IFD says.
        let (width, store) = storage(BitOrder::Msb, Some(2), 3034, 2024, 12, 1, 9_844_736).unwrap();
        assert_eq!(width, 3040);
        assert_eq!(
            store,
            Storage::Packed(Packing {
                bits: 12,
                row_stride: Some(4864),
                group: Some((10, 16)),
                order: BitOrder::Msb,
            })
        );
        // The makernote's own word is taken where it is decisive.
        assert_eq!(
            storage(BitOrder::Msb, Some(3), w, h, 12, 1, w * h * 2).unwrap(),
            (w, Storage::Huffman)
        );
        assert!(matches!(
            storage(BitOrder::Msb, Some(13), w, h, 14, 1, 100),
            Err(Error::Unsupported(_))
        ));
        // "Unpacked 12 bits": two bytes a sample, the sample at the
        // top of the pair rather than the pair itself.
        assert_eq!(
            storage(BitOrder::Msb, Some(7), 8, 16, 12, 1, 8 * 16 * 2).unwrap(),
            (
                8,
                Storage::Packed(Packing {
                    bits: 12,
                    row_stride: Some(16),
                    group: Some((1, 2)),
                    order: BitOrder::Msb,
                })
            )
        );
    }

    /// A linearization tag: version, predictors, point count, points.
    fn linearization(version: [u8; 2], vpred: u16, points: &[u16], tail: usize) -> Vec<u8> {
        let mut out = version.to_vec();
        for _ in 0..4 {
            out.extend_from_slice(&vpred.to_le_bytes());
        }
        out.extend_from_slice(&(points.len() as u16).to_le_bytes());
        for point in points {
            out.extend_from_slice(&point.to_le_bytes());
        }
        out.resize(out.len() + tail, 0);
        out
    }

    #[test]
    fn the_lossless_version_makes_its_own_ramp() {
        // 0x46 stores the point count but not the points.
        let blob = linearization([0x46, 0x30], 2048, &[], 34);
        let mut blob = blob;
        blob[10..12].copy_from_slice(&34u16.to_le_bytes());
        let table = Linearization::parse(&blob, true, 14).unwrap();
        assert_eq!(table.vpred, [[2048, 2048], [2048, 2048]]);
        assert_eq!(table.curve.len(), 16384);
        assert_eq!(table.curve[0], 0);
        assert_eq!(table.curve[16383], 16383);
        assert_eq!(table.white_level(), 16383.0);
        assert_eq!(table.split, 0);
    }

    #[test]
    fn the_sparse_lossy_curve_interpolates_between_its_points() {
        // Five points over a 16-sample domain: step 4.
        let blob = linearization([0x44, 0x20], 328, &[0, 4, 8, 40, 100], 0);
        let table = Linearization::parse(&blob, true, 4).unwrap();
        assert_eq!(table.vpred, [[328, 328], [328, 328]]);
        assert_eq!(
            table.curve,
            vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 16, 24, 32, 40, 55, 70, 85]
        );
        assert_eq!(table.white_level(), 85.0);
    }

    #[test]
    fn the_old_lossy_curve_is_stored_point_for_point() {
        // Version 0x44 0x10: the points are the curve, and there are
        // fewer of them than the depth allows, so the clamp is theirs.
        let blob = linearization([0x44, 0x10], 328, &[0, 3, 9, 27, 81], 34);
        let table = Linearization::parse(&blob, true, 12).unwrap();
        assert_eq!(table.curve, vec![0, 3, 9, 27, 81]);
        assert_eq!(table.white_level(), 81.0);
    }

    #[test]
    fn a_truncated_linearization_table_is_an_error() {
        assert!(Linearization::parse(&[], true, 12).is_err());
        assert!(Linearization::parse(&[0x46, 0x30, 0, 0], true, 12).is_err());
        // A point count that would make the step zero.
        let blob = linearization([0x44, 0x20], 0, &[0; 40], 0);
        assert!(Linearization::parse(&blob, true, 4).is_err());
    }

    #[test]
    fn every_tree_is_a_complete_huffman_code() {
        for tree in [
            &TREE_12_LOSSY,
            &TREE_12_LOSSLESS,
            &TREE_14_LOSSY,
            &TREE_14_LOSSLESS,
            &TREE_12_LOSSY_SPLIT,
        ] {
            let total: usize = tree.counts.iter().map(|c| *c as usize).sum();
            assert_eq!(total, tree.symbols.len(), "counts and symbols disagree");
            // Kraft's equality: a code that leaves nothing over.
            let kraft: f64 = tree
                .counts
                .iter()
                .enumerate()
                .map(|(i, count)| *count as f64 / (1u64 << (i + 1)) as f64)
                .sum();
            assert!(
                (kraft - 1.0).abs() < 1e-9,
                "tree leaves {kraft} of the code space"
            );
            HuffTable::new(&tree.counts, tree.symbols).expect("tree builds");
        }
        // The version and depth pick between them.
        assert_eq!(tree_for(0x46, 12).symbols, TREE_12_LOSSLESS.symbols);
        assert_eq!(tree_for(0x46, 14).symbols, TREE_14_LOSSLESS.symbols);
        assert_eq!(tree_for(0x44, 12).symbols, TREE_12_LOSSY.symbols);
        assert_eq!(tree_for(0x49, 14).symbols, TREE_14_LOSSY.symbols);
        assert_eq!(
            split_tree_for(12).unwrap().symbols,
            TREE_12_LOSSY_SPLIT.symbols
        );
        assert!(split_tree_for(14).is_err());
    }

    #[test]
    fn differences_are_sign_extended_the_lossless_jpeg_way() {
        let bits = |text: &str| -> Vec<u8> {
            let mut bits: Vec<u8> = text.chars().map(|c| (c == '1') as u8).collect();
            while !bits.len().is_multiple_of(8) {
                bits.push(0);
            }
            bits.chunks(8)
                .map(|byte| byte.iter().fold(0u8, |acc, bit| (acc << 1) | bit))
                .collect()
        };
        // A symbol with the top value bit set is the value itself...
        let data = bits("110");
        assert_eq!(difference(3, &mut BitPumpMsb::new(&data)).unwrap(), 6);
        // ... and without it, that value less (1 << len) - 1.
        let data = bits("001");
        assert_eq!(difference(3, &mut BitPumpMsb::new(&data)).unwrap(), 1 - 7);
        // Width zero is no difference and reads no bits.
        assert_eq!(difference(0, &mut BitPumpMsb::new(&data)).unwrap(), 0);
        // A shift reads fewer bits and lands mid-step: symbol 0x39 is
        // nine bits wide read six bits deep, so 0b110011 stands for
        // 0b1100111 (the low bit set) shifted up to 0b110011100 = 412.
        let data = bits("110011");
        assert_eq!(difference(0x39, &mut BitPumpMsb::new(&data)).unwrap(), 412);
        // A shift wider than the value is not a symbol any table has.
        assert!(difference(0x51, &mut BitPumpMsb::new(&data)).is_err());
    }

    #[test]
    fn a_hand_coded_stream_predicts_along_the_row_and_down_the_pairs() {
        // 12-bit lossless codes: 5 is "00", 4 "010", 3 "100", 0 "11110".
        let mut bits: Vec<u8> = Vec::new();
        let mut push = |text: &str| bits.extend(text.chars().map(|c| (c == '1') as u8));
        // Row 0: +16, +17 seed the two column predictors, then two
        // pixels that predict from two columns back: +7 and -6.
        push("00");
        push("10000");
        push("00");
        push("10001");
        push("100");
        push("111");
        push("100");
        push("001");
        // Row 1 seeds from its own parity's predictors, still 512
        // because no row 1 has been before it: +9 and -14. Then a
        // zero-width difference and a five-bit -31.
        push("010");
        push("1001");
        push("010");
        push("0001");
        push("11110");
        push("00");
        push("00000");
        while !bits.len().is_multiple_of(8) {
            bits.push(0);
        }
        let data: Vec<u8> = bits
            .chunks(8)
            .map(|byte| byte.iter().fold(0u8, |acc, bit| (acc << 1) | bit))
            .collect();
        let table = Linearization {
            version: 0x46,
            vpred: [[512, 512], [512, 512]],
            curve: (0..4096).map(|i| i as u16).collect(),
            split: 0,
        };
        let mut out = [0u16; 8];
        decompress(&data, 4, 2, 12, &table, &mut out).unwrap();
        assert_eq!(out, [528, 529, 535, 523, 521, 498, 521, 467]);

        // The curve is what a prediction is looked up in, and it
        // clamps: a curve of four values sees every prediction pinned
        // into 0..=3.
        let table = Linearization {
            version: 0x46,
            vpred: [[0, 0], [0, 0]],
            curve: vec![10, 20, 30, 40],
            split: 0,
        };
        let mut out = [0u16; 8];
        decompress(&data, 4, 2, 12, &table, &mut out).unwrap();
        assert_eq!(out, [40, 40, 40, 40, 40, 10, 40, 10]);
    }

    #[test]
    fn a_frame_changes_table_at_its_split_row() {
        let table = Linearization {
            version: 0x44,
            vpred: [[328, 328], [328, 328]],
            curve: (0..4096).map(|i| i as u16).collect(),
            split: 1,
        };
        // All-zero bits: the first table reads "00" as symbol 5, five
        // bits of difference, all zeros, which is -31 a pixel; the
        // second reads the same two bits as symbol 0x39, a nine-bit
        // difference read six bits deep, which is -508 and takes the
        // prediction below the curve, where it pins to its bottom.
        let mut out = [0u16; 4];
        decompress(&[0; 32], 2, 2, 12, &table, &mut out).unwrap();
        assert_eq!(out, [297, 297, 0, 0]);
        // There is no second table for a 14-bit frame.
        assert!(matches!(
            decompress(&[0; 32], 2, 2, 14, &table, &mut out),
            Err(Error::Unsupported(_))
        ));
    }

    #[test]
    fn the_second_tables_unknown_code_is_refused_rather_than_guessed() {
        let table = Linearization {
            version: 0x44,
            vpred: [[328, 328], [328, 328]],
            curve: (0..4096).map(|i| i as u16).collect(),
            split: 0,
        };
        // 0xff bits are the second table's longest code, the one no
        // sample has ever used.
        let split = Linearization { split: 1, ..table };
        let mut out = [0u16; 4];
        assert!(matches!(
            decompress(&[0xff; 32], 2, 2, 12, &split, &mut out),
            Err(Error::Unsupported(_))
        ));
    }

    // ------------------------------------------------------------ corpus
    //
    // Gated on SCHIST_RAW_CORPUS: a directory of camera files, walked
    // recursively for the two extensions this module claims. Beside
    // each file the harness leaves LibRaw's own output, which is what
    // "exactly" means here:
    //
    //   <file>.tiff          `unprocessed_raw -T`: the whole sensor
    //                        frame, 16-bit grey, black not subtracted
    //   <file>.identify.txt  `raw-identify -v -w`
    //
    // Both are optional; a file with neither is still decoded and
    // validated.

    use std::path::{Path, PathBuf};

    /// Every NEF and NRW under `SCHIST_RAW_CORPUS`.
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
                    .is_some_and(|e| e.eq_ignore_ascii_case("nef") || e.eq_ignore_ascii_case("nrw"))
                {
                    out.push(path);
                }
            }
        }
        out.sort();
        out
    }

    /// Files this module knowingly does not decode, and why. A sample
    /// that stops being on this list has to start decoding instead.
    fn unsupported(path: &Path) -> Option<&'static str> {
        let name = path.file_name()?.to_string_lossy().to_ascii_uppercase();
        // The COOLSCAN film scanners put 16-bit RGB scans in a NEF:
        // three samples a pixel through no filter array, so there is
        // no sensor frame to hand out and nothing to demosaic.
        name.contains("COOLSCAN")
            .then_some("a COOLSCAN film scan, RGB rather than CFA")
    }

    /// Bodies whose black level LibRaw supplies from a table of its
    /// own. The files record none anywhere — exiftool finds none
    /// either — and this decoder does not invent one; the camera table
    /// is where such a value belongs.
    const BLACK_FROM_LIBRAW_TABLE: &[&str] = &["COOLPIX P330", "COOLPIX P7700"];

    /// How far right the oracle's samples have to move to be this
    /// decoder's.
    ///
    /// The P7700 and the P330 write the same thing — twelve bits at
    /// the top of a sixteen-bit word — and LibRaw shifts the P330's
    /// samples down to where they belong but hands the P7700's out in
    /// the word it found them in, saying so with a black level and a
    /// saturation point sixteen times the P330's. This decoder takes
    /// the twelve bits on both, so its P7700 frame is the oracle's
    /// four bits over and develops to the same picture.
    fn oracle_shift(model: &str) -> u32 {
        u32::from(model == "COOLPIX P7700") * 4
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
        Some((
            image.width() as usize,
            image.height() as usize,
            image.into_raw(),
        ))
    }

    /// `raw-identify`'s report.
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
                rest.split(|c: char| !(c.is_ascii_digit() || c == '.'))
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
        let (mut checked, mut failures, mut extra) = (0, Vec::new(), Vec::new());
        for path in &files {
            let bytes = std::fs::read(path).expect("corpus file reads");
            assert_eq!(
                crate::probe(&bytes),
                Some(Format::Nef),
                "{} did not probe as NEF",
                path.display()
            );
            let raw = match (crate::decode(&bytes), unsupported(path)) {
                (Err(Error::Unsupported(message)), Some(reason)) => {
                    // The allow-list still has to be right about why.
                    assert!(!message.is_empty(), "{}: expected {reason}", path.display());
                    continue;
                }
                (Ok(_), Some(reason)) => {
                    panic!("{} decoded but is listed as {reason}", path.display())
                }
                (Err(error), _) => panic!("{}: {error}", path.display()),
                (Ok(raw), None) => raw,
            };
            raw.validate().expect("decoded frame is self-consistent");
            assert_eq!(raw.cpp, 1);
            checked += 1;
            let shift = oracle_shift(&raw.model);

            if let Some((width, height, expected)) = oracle(path) {
                assert_eq!(
                    (raw.width, raw.height),
                    (width, height),
                    "{}: frame size",
                    path.display()
                );
                let RawData::U16(got) = &raw.data else {
                    panic!("{}: NEF frames are integers", path.display())
                };
                // LibRaw stops short of the bottom of the frame on the
                // bodies its own table gives a smaller height than the
                // sensor IFD does (the Nikon 1 bodies by two rows, the
                // D60 by three): the rows are in the file, coded like
                // every other row, and this decoder reads them, so the
                // oracle's untouched zeros there are not a difference
                // to hold against it. Everything the oracle did decode
                // has to match sample for sample.
                let decoded = height
                    - expected
                        .rchunks_exact(width)
                        .take_while(|row| row.iter().all(|s| *s == 0))
                        .count();
                let wrong: Vec<usize> = got[..decoded * width]
                    .iter()
                    .zip(&expected)
                    .enumerate()
                    .filter(|(_, (a, b))| **a != **b >> shift)
                    .map(|(i, _)| i)
                    .collect();
                if !wrong.is_empty() {
                    failures.push(format!(
                        "{}: {} of {} samples differ from the oracle; first: {:?}",
                        path.display(),
                        wrong.len(),
                        decoded * width,
                        wrong
                            .iter()
                            .take(4)
                            .map(|i| (i % width, i / width, got[*i], expected[*i] >> shift))
                            .collect::<Vec<_>>()
                    ));
                    continue;
                }
                if decoded < height {
                    extra.push(format!(
                        "{}: {} rows past LibRaw's {decoded}",
                        path.display(),
                        height - decoded
                    ));
                }
            }

            if let Some(report) = identify(path) {
                if let [width, height] = numbers(&report, "Full size:")[..] {
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
                // "black:" is printed only when it is not zero, which
                // is exactly when the file carries a black level.
                if !BLACK_FROM_LIBRAW_TABLE.contains(&raw.model.as_str()) {
                    let black = numbers(&report, "black:");
                    let want = black.first().copied().unwrap_or(0.0) / (1 << shift) as f32;
                    assert_eq!(
                        raw.black_levels,
                        [want; 4],
                        "{}: black level",
                        path.display()
                    );
                }
                // "As shot" is cam_mul: R G B G2, unnormalised.
                let shot = numbers(&report, "As shot");
                if let [r, g, b, g2] = shot[..4.min(shot.len())] {
                    let want = [r / g, 1.0, b / g, g2 / g];
                    // LibRaw hands out unit multipliers for the D1
                    // whatever the file says; ours are the file's.
                    if want != [1.0; 4] || raw.wb_coeffs == [1.0; 4] {
                        for (got, want) in raw.wb_coeffs.iter().zip(want) {
                            assert!(
                                (got - want).abs() < 1e-4,
                                "{}: white balance {:?} not {want:?}",
                                path.display(),
                                raw.wb_coeffs,
                            );
                        }
                    }
                }
                // The "Raw inset" line is the makernote's CropArea,
                // which is what this module hands out as the crop.
                if let [w, h, x, y] = numbers(&report, "Raw inset, width x height:")[..] {
                    assert_eq!(
                        raw.crop,
                        Rect {
                            x: x as usize,
                            y: y as usize,
                            width: w as usize,
                            height: h as usize
                        },
                        "{}: crop",
                        path.display()
                    );
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

            assert!(raw.crop.x + raw.crop.width <= raw.width);
            assert!(raw.crop.y + raw.crop.height <= raw.height);
            assert!(
                raw.white_level > raw.black_levels[0],
                "{}: saturation below black",
                path.display()
            );

            // The preview is the camera's own full-size JPEG and has
            // to be a picture something can show.
            let preview = super::preview(&bytes).expect("preview reads");
            assert_eq!(
                preview,
                raw.preview,
                "{}: two ways to the preview",
                path.display()
            );
            if let Some(preview) = preview {
                let decoded = image::load_from_memory(&preview)
                    .unwrap_or_else(|e| panic!("{}: preview does not decode: {e}", path.display()));
                assert!(
                    decoded.width() > 160,
                    "{}: preview is only {}x{}",
                    path.display(),
                    decoded.width(),
                    decoded.height()
                );
            }
        }
        assert!(
            failures.is_empty(),
            "{} of {checked} files differ from the oracle:\n  {}",
            failures.len(),
            failures.join("\n  ")
        );
        assert!(
            files.is_empty() || checked > 0,
            "corpus found no decodable NEF"
        );
        // Not a failure, but worth seeing in the log: which frames
        // this decoder reads further down than LibRaw does.
        for line in extra {
            println!("decoded {line}");
        }
    }

    /// A file cut short must fail, not panic and not hang.
    #[test]
    fn corpus_truncations_do_not_panic() {
        for path in corpus() {
            let bytes = std::fs::read(&path).expect("corpus file reads");
            // Ten cuts spread over the file, plus the awkward ones at
            // the very start where the header itself is incomplete.
            let mut cuts: Vec<usize> = (1..=10).map(|i| bytes.len() * i / 11).collect();
            cuts.extend([0, 4, 8, 16, 100]);
            for cut in cuts {
                let short = &bytes[..cut.min(bytes.len())];
                // Any answer is fine as long as it is an answer.
                let _ = decode(short);
                let _ = super::preview(short);
            }
            // And a body whose bytes have been scribbled on.
            let mut damaged = bytes.clone();
            for (i, byte) in damaged.iter_mut().enumerate().skip(1024) {
                *byte ^= (i as u8).wrapping_mul(31);
            }
            let _ = decode(&damaged);
        }
    }

    #[test]
    fn rejects_files_that_are_not_nikon_raws() {
        assert!(decode(b"not a tiff at all").is_err());
        assert!(preview(b"not a tiff at all").is_err());
    }
}
