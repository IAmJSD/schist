//! Kodak DCR, KDC and the DCS-series TIFFs.
//!
//! Kodak's raws are all TIFF containers and share almost nothing else.
//! Three generations of professional back, two of consumer compact and
//! four compression schemes wear the same two file extensions, so this
//! module works out what it is holding from the directories rather
//! than from the name.
//!
//! Two private directories carry everything the TIFF tags do not, and
//! neither is a SubIFD, so the shared parser does not reach them:
//!
//! * `KodakIFD`, tag 0x8290 of IFD0 — the DCS bodies' calibration.
//!   Tags 0x03EB..0x03EE are the crop (left, top, width, height),
//!   0x0848..0x084D six white-balance presets, 0x03FC which of them
//!   was shot, 0x0960/0x0961 the sensor size and 0x090D the
//!   linearisation curve.
//! * The KDC directory, tag 0xFE00 of IFD0 — the EasyShare compacts'.
//!   Its tags run 0xFA00..0xFB8D: 0xFA13/0xFA14 the frame, 0xFA18 the
//!   depth and 0xFA25 the as-shot white balance.
//!
//! # White balance, two conventions
//!
//! The DCS presets are *divisors*: a stored triple (r, g, b) means
//! multipliers proportional to (1/r, 1/g, 1/b), which is why LibRaw
//! prints g²/r, g, g²/b for them. The KDC triples are multipliers
//! already, scaled by 65536. Both are normalised to green here.
//!
//! # Compression
//!
//! * 7 — lossless JPEG (SOF3), on the DCS 460/560 backs. The frame is
//!   one stream half the sensor's width with two components, so the
//!   samples come out interleaved in exactly the order the sensor rows
//!   want them.
//! * 65000 — Kodak's own scheme, on the DCR bodies. See
//!   [`decode_65000_segment`].
//! * 32867 — the DC40/DC50's much older "RADC" scheme. A
//!   green/colour-difference pyramid coder behind an 8-bit table-driven
//!   Huffman front end; see [`decode_radc`].
//! * 1 — uncompressed, packed at the stated depth.
//!
//! Two Kodak variants are deliberately out of scope. RADC's finer
//! escape quantiser (`CompressedBitsPerPixel` 243, [`radc_shift`]) is
//! implemented but unexercised — no sample selects it, so it has never
//! been checked against an oracle. The DC120 is a *different* scheme
//! again, a bilinear-interpolated loader rather than this one, and is
//! not folded in here.
//!
//! Whichever codec wrote it, a DCS frame decodes to *indices* into a
//! linearisation curve rather than to samples — KodakIFD 0x090D where
//! there is a KodakIFD, IFD0's GrayResponseCurve on the 460 — and the
//! curve's last entry is the saturation point.

use crate::formats::common;
use crate::tiff::{tags, Entry, Ifd, ImageLayout, Tiff};
use crate::{Cfa, CfaColor, Error, Format, RawData, RawImage, Rect, Result};
use rayon::prelude::*;

/// IFD0's pointer to the DCS calibration directory.
const KODAK_IFD: u16 = 0x8290;
/// IFD0's pointer to the EasyShare (KDC) directory.
const KDC_IFD: u16 = 0xFE00;

// KodakIFD tags.
const KODAK_CROP_LEFT: u16 = 0x03EB;
const KODAK_CROP_TOP: u16 = 0x03EC;
const KODAK_CROP_WIDTH: u16 = 0x03ED;
const KODAK_CROP_HEIGHT: u16 = 0x03EE;
const KODAK_WB_INDEX: u16 = 0x03FC;
const KODAK_WB_FIRST: u16 = 0x0848;
const KODAK_CURVE: u16 = 0x090D;

// KDC directory tags.
const KDC_WIDTH: u16 = 0xFA13;
const KDC_HEIGHT: u16 = 0xFA14;
const KDC_CFA: u16 = 0xFA15;
const KDC_DEPTH: u16 = 0xFA18;
const KDC_WB_ASSHOT: u16 = 0xFA25;
const KDC_IMAGE_WIDTH: u16 = 0xFA31;
const KDC_IMAGE_HEIGHT: u16 = 0xFA32;
const KDC_CROP_LEFT: u16 = 0xFA3E;
const KDC_CROP_TOP: u16 = 0xFA3F;

// Tags of the SubIFD the EasyShare bodies write beside the KDC one.
const KDC_SUB_CFA_PATTERN: u16 = 0xFD09;
const KDC_SUB_OFFSETS: u16 = 0xFD04;
const KDC_SUB_OFFSET_BIAS: u16 = 0xFD14;

/// TIFF's GrayResponseCurve, the 460's linearisation table.
const GRAY_RESPONSE_CURVE: u16 = 0x0123;

// Tags of a 65000-compressed image IFD.
const K65000_SEGMENT: u16 = 0xFDE8;
const K65000_OFFSETS: u16 = 0xFDE9;

/// EXIF CompressedBitsPerPixel, tag 0x9102. The RADC codec keys its
/// escape-table quantiser off it: a value of 243 means a finer step.
const COMPRESSED_BITS_PER_PIXEL: u16 = 0x9102;

/// A private directory Kodak points at with a plain LONG offset: not a
/// SubIFD, so the shared parser never followed it, but an ordinary IFD
/// in the file's byte order once you go there.
fn private_ifd<'a>(tiff: &Tiff<'a>, tag: u16) -> Option<Tiff<'a>> {
    let offset = tiff.find(tag)?.u32(0)? as usize + tiff.base();
    Tiff::parse_at(tiff.bytes(), offset, tiff.little_endian()).ok()
}

/// The IFD holding sensor samples, if the file has one at all.
///
/// The DCR bodies and the 560 back mark it with
/// `PhotometricInterpretation` 32803 (CFA). The older DCS 460 predates
/// that value and calls its sensor plain greyscale, so the fallback is
/// the largest single-sample strip image in the file — every other
/// directory in one of these is either an RGB preview or a thumbnail
/// smaller than the frame. The EasyShare KDCs have no image directory
/// at all and describe their frame only in the private one.
fn raw_ifd<'a>(tiff: &'a Tiff<'_>) -> Option<&'a Ifd> {
    let ifds = tiff.all();
    if let Some(cfa) = ifds
        .iter()
        .find(|ifd| ifd.get(tags::PHOTOMETRIC).and_then(|e| e.u32(0)) == Some(32803))
    {
        return Some(cfa);
    }
    ifds.into_iter()
        .filter(|ifd| {
            ifd.has(tags::STRIP_OFFSETS)
                && ifd
                    .get(tags::SAMPLES_PER_PIXEL)
                    .and_then(|e| e.u32(0))
                    .unwrap_or(1)
                    == 1
        })
        .max_by_key(|ifd| {
            let side = |tag| ifd.get(tag).and_then(|e| e.u64(0)).unwrap_or(0);
            side(tags::IMAGE_WIDTH).saturating_mul(side(tags::IMAGE_LENGTH))
        })
        .filter(|ifd| ifd.has(tags::IMAGE_WIDTH) && ifd.has(tags::IMAGE_LENGTH))
}

/// A 2x2 filter array from a CFAPattern-shaped entry (0 red, 1 green,
/// 2 blue, row-major).
fn cfa_from_pattern(entry: &Entry) -> Option<Cfa> {
    let mut colors = [CfaColor::Red; 4];
    for (i, color) in colors.iter_mut().enumerate() {
        *color = match entry.u32(i)? {
            0 => CfaColor::Red,
            1 => CfaColor::Green,
            2 => CfaColor::Blue,
            _ => return None,
        };
    }
    Some(Cfa::Bayer(colors))
}

/// The linearisation curve every DCS body decodes indices into.
///
/// KodakIFD 0x090D holds it where the file has a KodakIFD: 1024
/// entries on the DCR bodies, 4096 on the 560 back, the last entry
/// being where the sensor saturates. The 460 has no KodakIFD and uses
/// the standard GrayResponseCurve instead, 256 entries for its 8-bit
/// samples. Note that 0x090D wins where both exist: the 560's own
/// GrayResponseCurve is a display curve and applying it would brighten
/// every sample by a quarter.
fn curve(tiff: &Tiff<'_>, kodak: Option<&Tiff<'_>>) -> Option<Vec<u16>> {
    let entry = kodak
        .and_then(|k| k.root().get(KODAK_CURVE))
        // The DCS 460 has no KodakIFD; its 8-bit samples are indices
        // into IFD0's 256-entry GrayResponseCurve instead, which is
        // what that tag is for on a greyscale TIFF.
        .or_else(|| tiff.find(GRAY_RESPONSE_CURVE))?;
    let table: Vec<u16> = (0..entry.count)
        .map_while(|i| entry.u32(i))
        .map(|v| v.min(u16::MAX as u32) as u16)
        .collect();
    (table.len() >= 256).then_some(table)
}

/// Apply a linearisation curve in place, clamping indices that ran
/// past its end (a corrupt segment can predict its way out of range).
fn linearize(samples: &mut [u16], table: &[u16]) {
    let last = table[table.len() - 1];
    for sample in samples {
        *sample = table.get(*sample as usize).copied().unwrap_or(last);
    }
}

// ---------------------------------------------------------- 65000

/// Kodak's "65000" compression, one 256-pixel segment at a time.
///
/// The scheme was read off the files themselves, and it is simple once
/// seen. A segment of `n` pixels opens with `ceil(n/2)` bytes of
/// lengths, one 4-bit field a pixel, **low nibble first** — so byte 0
/// holds pixel 0's length in its low half and pixel 1's in its high
/// half. The differences follow, `length` bits each, in a bitstream
/// that is neither of the usual two: the bytes pair up into 16-bit
/// **big-endian** words and the bits come out of each word from its
/// **least** significant end. A field's value is extended the way
/// JPEG extends a magnitude category — a value below half the range is
/// negative, `v - 2^len + 1` — so a length of `k` covers exactly the
/// differences whose magnitude needs `k` bits.
///
/// Each difference adds to the last pixel of the same filter column,
/// two back, and both predictors restart at zero every segment; that
/// is what lets a decoder start at any segment the offset table names.
/// The result is an index into the linearisation curve, not a sample.
fn decode_65000_segment(data: &[u8], out: &mut [u16]) -> Result<()> {
    let n = out.len();
    let table = n.div_ceil(2);
    if data.len() < table {
        return Err(Error::Corrupt(
            "Kodak 65000 segment shorter than its length table".into(),
        ));
    }
    // Read the bitstream from 16-bit big-endian words, least
    // significant bit first. Past the end it yields zeros, so a
    // truncated segment decodes to a flat run rather than failing.
    let bits = &data[table..];
    let mut accumulator: u64 = 0;
    let mut have = 0u32;
    let mut at = 0usize;
    let mut predictor = [0i32; 2];

    for i in 0..n {
        let byte = data[i / 2];
        let length = (if i % 2 == 0 { byte & 0x0f } else { byte >> 4 }) as u32;
        if length > 12 {
            return Err(Error::Corrupt(format!(
                "Kodak 65000 difference length {length} (the field is at most 12 bits)"
            )));
        }
        while have < length {
            let high = bits.get(at).copied().unwrap_or(0) as u64;
            let low = bits.get(at + 1).copied().unwrap_or(0) as u64;
            accumulator |= ((high << 8) | low) << have;
            have += 16;
            at += 2;
        }
        let mut difference = 0i32;
        if length > 0 {
            let value = (accumulator & ((1u64 << length) - 1)) as i32;
            accumulator >>= length;
            have -= length;
            difference = if value < (1 << (length - 1)) {
                value - (1 << length) + 1
            } else {
                value
            };
        }
        let slot = &mut predictor[i & 1];
        *slot = slot.saturating_add(difference);
        out[i] = (*slot).clamp(0, u16::MAX as i32) as u16;
    }
    Ok(())
}

/// A whole 65000 frame: the image IFD names the segment width
/// (0xFDE8) and carries a table of segment *end* offsets (0xFDE9)
/// relative to the strip, the first segment starting at zero.
fn decode_65000(strip: &[u8], ifd: &Ifd, width: usize, height: usize) -> Result<Vec<u16>> {
    let segment = ifd
        .get(K65000_SEGMENT)
        .and_then(|e| e.u32(0))
        .filter(|v| *v > 0)
        .ok_or_else(|| Error::Corrupt("Kodak 65000 image without a segment width".into()))?
        as usize;
    let offsets: Vec<u32> = ifd
        .get(K65000_OFFSETS)
        .map(|e| e.u32s())
        .ok_or_else(|| Error::Corrupt("Kodak 65000 image without a segment offset table".into()))?;
    let per_row = width.div_ceil(segment);
    if offsets.len() < per_row * height {
        return Err(Error::Corrupt(format!(
            "Kodak 65000 offset table holds {} entries, want {}",
            offsets.len(),
            per_row * height
        )));
    }

    let mut out = vec![0u16; width * height];
    // Segments are independent by construction, so rows are too.
    out.par_chunks_mut(width)
        .enumerate()
        .try_for_each(|(row, samples)| -> Result<()> {
            for s in 0..per_row {
                let index = row * per_row + s;
                let start = if index == 0 {
                    0
                } else {
                    offsets[index - 1] as usize
                };
                let end = offsets[index] as usize;
                if end < start || end > strip.len() {
                    return Err(Error::Corrupt(format!(
                        "Kodak 65000 segment {index} spans {start}..{end} of a {}-byte strip",
                        strip.len()
                    )));
                }
                let first = s * segment;
                let last = (first + segment).min(width);
                decode_65000_segment(&strip[start..end], &mut samples[first..last])?;
            }
            Ok(())
        })?;
    Ok(out)
}

// ------------------------------------------------------------- RADC

// The DC40/DC50 "RADC" scheme. Every frame is a fixed 768x512, decoded
// in stripes of four rows. Three colour channels are reconstructed at
// half horizontal resolution into small line buffers, then scattered
// into the Bayer output; a diagonal fix-up and a tone curve finish it.
//
// The whole thing is a green/colour-difference pyramid coder behind an
// 8-bit table-driven Huffman front end: peek eight bits, index a
// 256-entry table, consume the length the table records and take its
// symbol byte as a signed char. Tables 0..9 are "tree" tables whose
// symbol is the next tree to read; tables 10..17 carry step and
// difference values; table 18 is a procedural escape quantiser.

/// RADC frames are always this size; the reference rejects anything
/// larger, and the tag-declared 756x504 is only the active window.
const RADC_WIDTH: usize = 768;
const RADC_HEIGHT: usize = 512;
/// Working columns: everything happens at half horizontal resolution.
const RADC_HALF: usize = RADC_WIDTH / 2;
/// Line-buffer width: the 384 working columns plus two guard columns.
/// The walk seeds column `RADC_HALF` and every predictor reads its
/// right neighbour `x + 1`, so index 384 must exist; the green
/// channel's slide is offset by one short and writes index 385.
const RADC_LINE: usize = RADC_HALF + 2;
/// Saturation after the tone curve maps the ~12-bit working range up.
const RADC_MAX: u16 = 0x3FFF;

/// The 19 direct-lookup Huffman tables. Each entry packs
/// `(codeLength << 8) | symbolByte`; a peek of eight bits indexes it.
struct RadcTables {
    tables: [[u16; 256]; 19],
}

impl RadcTables {
    /// Build the tables. Tables 0..17 come from a fixed `(length,
    /// symbol)` list, replicated across `256 >> length` lookup slots and
    /// laid end to end so each 256-slot span is one table. Table 18 is
    /// the escape quantiser, whose step is `1 << shift`.
    fn build(shift: u32) -> RadcTables {
        // The (length, symbol) list, read left to right. Tables 0..9 are
        // the tree tables selected by the walk; 10..17 the value/step
        // tables. Symbols are signed. Each table's lengths sum to
        // exactly 256 lookup slots, so the boundaries fall out of a
        // straight sequential fill.
        #[rustfmt::skip]
        const SPEC: &[(u8, i8)] = &[
            (1,1),(2,3),(3,4),(4,2),(5,7),(6,5),(7,6),(7,8),
            (1,0),(2,1),(3,3),(4,4),(5,2),(6,7),(7,6),(8,5),(8,8),
            (2,1),(2,3),(3,0),(3,2),(3,4),(4,6),(5,5),(6,7),(6,8),
            (2,0),(2,1),(2,3),(3,2),(4,4),(5,6),(6,7),(7,5),(7,8),
            (2,1),(2,4),(3,0),(3,2),(3,3),(4,7),(5,5),(6,6),(6,8),
            (2,3),(3,1),(3,2),(3,4),(3,5),(3,6),(4,7),(5,0),(5,8),
            (2,3),(2,6),(3,0),(3,1),(4,4),(4,5),(4,7),(5,2),(5,8),
            (2,4),(2,7),(3,3),(3,6),(4,1),(4,2),(4,5),(5,0),(5,8),
            (2,6),(3,1),(3,3),(3,5),(3,7),(3,8),(4,0),(5,2),(5,4),
            (2,0),(2,1),(3,2),(3,3),(4,4),(4,5),(5,6),(5,7),(4,8),
            (1,0),(2,2),(2,-2),(1,-3),(1,3),
            (2,-17),(2,-5),(2,5),(2,17),(2,-7),(2,2),(2,9),(2,18),
            (2,-18),(2,-9),(2,-2),(2,7),(2,-28),(2,28),
            (3,-49),(3,-9),(3,9),(4,49),(5,-79),(5,79),
            (2,-1),(2,13),(2,26),(3,39),(4,-16),(5,55),(6,-37),(6,76),
            (2,-26),(2,-13),(2,1),(3,-39),(4,16),(5,-55),(6,-76),(6,37),
        ];
        let mut tables = [[0u16; 256]; 19];
        let mut table = 0usize;
        let mut slot = 0usize;
        for &(length, symbol) in SPEC {
            let packed = ((length as u16) << 8) | (symbol as u8 as u16);
            // A code of length L fills 2^(8-L) consecutive slots.
            for _ in 0..(256usize >> length) {
                tables[table][slot] = packed;
                slot += 1;
                if slot == 256 {
                    slot = 0;
                    table += 1;
                }
            }
        }
        debug_assert_eq!((table, slot), (18, 0), "RADC list did not fill 18 tables");

        // Table 18: coarse, evenly spaced luma levels. Code length is
        // 8-shift; the symbol is the index rounded down to a multiple of
        // 2^shift with the mid-step bit set.
        for (c, entry) in tables[18].iter_mut().enumerate() {
            let symbol = ((c >> shift) << shift) | (1 << (shift - 1));
            *entry = (((8 - shift) as u16) << 8) | symbol as u16;
        }
        RadcTables { tables }
    }
}

/// The RADC bit reader and table set. MSB-first over the payload; past
/// the end it yields zero bits so truncated input never panics.
struct Radc<'a> {
    data: &'a [u8],
    pos: usize,
    accumulator: u64,
    buffered_bits: u32,
    tables: RadcTables,
}

impl<'a> Radc<'a> {
    fn new(data: &'a [u8], shift: u32) -> Radc<'a> {
        Radc {
            data,
            pos: 0,
            accumulator: 0,
            buffered_bits: 0,
            tables: RadcTables::build(shift),
        }
    }

    /// Pull bytes until at least `need` bits are buffered, zero-filling
    /// past the end of the payload.
    fn refill(&mut self, need: u32) {
        while self.buffered_bits < need {
            let byte = self.data.get(self.pos).copied().unwrap_or(0);
            self.pos += 1;
            self.accumulator = (self.accumulator << 8) | byte as u64;
            self.buffered_bits += 8;
        }
    }

    /// The next `n` bits (n <= 8 here), MSB first.
    fn getbits(&mut self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        self.refill(n);
        let shift = self.buffered_bits - n;
        let value = ((self.accumulator >> shift) & ((1u64 << n) - 1)) as u32;
        self.buffered_bits = shift;
        self.accumulator &= (1u64 << shift) - 1;
        value
    }

    /// Decode one Huffman symbol from `table`: peek eight bits, look up,
    /// consume the recorded length, return the symbol as a signed char.
    fn radc_token(&mut self, table: usize) -> i32 {
        self.refill(8);
        let index = ((self.accumulator >> (self.buffered_bits - 8)) & 0xff) as usize;
        let entry = self.tables.tables[table][index];
        let length = (entry >> 8) as u32;
        // Every filled slot has a non-zero length; guard anyway so a
        // hostile build can never spin here.
        debug_assert!(length > 0);
        self.buffered_bits -= length.min(self.buffered_bits);
        self.accumulator &= (1u64 << self.buffered_bits) - 1;
        (entry as u8 as i8) as i32
    }
}

/// The spatial predictor. Green (channel 0) averages three neighbours
/// with the one above weighted double; the colour-difference channels
/// average the two orthogonal neighbours.
fn radc_predictor(buf: &[[[i16; RADC_LINE]; 3]; 3], c: usize, y: usize, x: usize) -> i32 {
    let at = |yy: usize, xx: usize| buf[c][yy][xx] as i32;
    if c == 0 {
        (at(y - 1, x + 1) + 2 * at(y - 1, x) + at(y, x + 1)) / 4
    } else {
        (at(y - 1, x) + at(y, x + 1)) / 2
    }
}

/// The global tone curve: piecewise-linear across a fixed knot table,
/// carrying the ~12-bit working range up to a 14-bit output and
/// clipping everything above 4095 to the maximum.
fn radc_curve() -> Vec<u16> {
    // Interleaved (x, y) control points.
    const PT: [(i64, i64); 6] = [
        (0, 0),
        (1280, 1344),
        (2320, 3616),
        (3328, 8000),
        (4095, 16383),
        (65535, 16383),
    ];
    let mut curve = vec![0u16; 65536];
    for pair in PT.windows(2) {
        let (x0, y0) = pair[0];
        let (x1, y1) = pair[1];
        for c in x0..=x1 {
            // Linear interpolation rounded to nearest, halves up: the
            // oracle differs from plain truncation by one count over
            // much of the range. Done in integers as
            // floor(v + 1/2) = (2*num + den) / (2*den).
            let (num, den) = ((c - x0) * (y1 - y0), x1 - x0);
            let value = (2 * num + den) / (2 * den) + y0;
            curve[c as usize] = value as u16;
        }
    }
    curve
}

/// The escape table's quantiser step, from the file's
/// CompressedBitsPerPixel. A value of 243 asks for the finer grid
/// (six-bit codes); everything else, the DC40/DC50 sample included
/// (which reads 1.52), gets the coarse one.
///
/// The 243 branch is untested: no corpus file selects it. See the
/// module notes.
fn radc_shift(compressed_bpp: Option<f64>) -> u32 {
    if compressed_bpp.is_some_and(|v| (v - 243.0).abs() < 0.5) {
        2
    } else {
        3
    }
}

/// Decode a whole RADC strip into the 768x512 single-channel Bayer
/// frame, black not subtracted, tone curve applied.
fn decode_radc(strip: &[u8], compressed_bpp: Option<f64>) -> Result<Vec<u16>> {
    let total = crate::frame_samples(RADC_WIDTH, RADC_HEIGHT, 1)?;
    let mut r = Radc::new(strip, radc_shift(compressed_bpp));
    let mut raw = vec![0u16; total];

    // Working buffers: three channels, three lines each, persisting
    // across stripes and channels. Initialised to 2048 (the green bias
    // the diagonal fix-up later removes).
    let mut buf = [[[2048i16; RADC_LINE]; 3]; 3];
    // Per-channel gain memory carried into the next stripe's rescale.
    let mut last = [16i64; 3];

    for row in (0..RADC_HEIGHT).step_by(4) {
        let mul = [
            r.getbits(6) as i64,
            r.getbits(6) as i64,
            r.getbits(6) as i64,
        ];
        if mul.contains(&0) {
            return Err(Error::Corrupt(
                "Kodak RADC stripe with a zero channel gain".into(),
            ));
        }

        for c in 0..3 {
            // 4.2 Rescale the carried buffer from the previous stripe's
            // gain to this one's. The val>65564 threshold and the
            // 0x7ff/round constants are empirical fixed constants from
            // the reference, reproduced verbatim.
            // The working buffer is read as signed here; the spec's
            // pseudo-code masks it to sixteen bits (unsigned). On the
            // one sample with an oracle no value is ever negative at
            // this point, so the two readings agree and the choice is
            // an interpretation awaiting a second body.
            let mut val = ((0x100_0000i64 / last[c] + 0x7ff) >> 12) * mul[c];
            let sh: u32 = if val > 65564 { 10 } else { 12 };
            let round = (1i64 << (sh - 1)) - 1;
            val <<= 12 - sh;
            for line in buf[c].iter_mut() {
                for sample in line.iter_mut() {
                    let scaled = ((*sample as i64) * val + round).min(0x7FFF_FFFF);
                    *sample = ((scaled >> sh) & 0xFFFF) as u16 as i16;
                }
            }
            last[c] = mul[c];

            // 4.3 Decode the field: green as two sub-rows, colour
            // channels as one.
            let sub_rows = if c == 0 { 2 } else { 1 };
            for sub in 0..sub_rows {
                // Seed the middle of the two working lines.
                buf[c][1][RADC_HALF] = (mul[c] << 7) as i16;
                buf[c][2][RADC_HALF] = (mul[c] << 7) as i16;

                let mut col = RADC_HALF as i32;
                let mut tree = 1usize;
                while col > 0 {
                    // The token both drives this cell and becomes the
                    // next tree to read: a non-zero token descends to
                    // tree 1..8, a zero (run) token drops back to tree 0.
                    let token = r.radc_token(tree);
                    tree = token as usize;
                    if token != 0 {
                        col -= 2;
                        if col >= 0 {
                            let x0 = col as usize;
                            // Both branches read a *fresh* token at each
                            // of the four cell positions — the spec's
                            // "apply the assignment to the four buffer
                            // positions" means the token read inside it
                            // too, as the oracle proves: one token per
                            // cell desynchronises the bitstream.
                            if token == 8 {
                                // Coarse absolute levels from the escape
                                // table, scaled by gain. The symbol is
                                // taken unsigned here.
                                for y in [1usize, 2] {
                                    for x in [x0 + 1, x0] {
                                        let level = (r.radc_token(18) & 0xff) as i64;
                                        buf[c][y][x] = (level * mul[c]) as i16;
                                    }
                                }
                            } else {
                                // Signed differences (value table
                                // tree+10) times 16, over the predictor.
                                for y in [1usize, 2] {
                                    for x in [x0 + 1, x0] {
                                        let diff = r.radc_token(tree + 10);
                                        let pred = radc_predictor(&buf, c, y, x);
                                        buf[c][y][x] = (diff * 16 + pred) as i16;
                                    }
                                }
                            }
                        }
                    } else {
                        // A run token: a run of copy/step cells.
                        loop {
                            let nreps = if col > 2 { r.radc_token(9) + 1 } else { 1 };
                            let mut rep = 0;
                            while rep < 8 && rep < nreps && col > 0 {
                                col -= 2;
                                if col >= 0 {
                                    let x0 = col as usize;
                                    for y in [1usize, 2] {
                                        for x in [x0 + 1, x0] {
                                            buf[c][y][x] = radc_predictor(&buf, c, y, x) as i16;
                                        }
                                    }
                                    // Every odd repeat adds a step.
                                    if rep & 1 == 1 {
                                        let step = r.radc_token(10) << 4;
                                        for y in [1usize, 2] {
                                            buf[c][y][x0 + 1] =
                                                (buf[c][y][x0 + 1] as i32 + step) as i16;
                                            buf[c][y][x0] = (buf[c][y][x0] as i32 + step) as i16;
                                        }
                                    }
                                }
                                rep += 1;
                            }
                            // A run of exactly nine continues.
                            if nreps != 9 {
                                break;
                            }
                        }
                    }
                }

                // 4.4 Emit the field to the Bayer output, undoing the
                // <<7 gain domain.
                // Each (x, y) lands on its own Bayer site, so the two
                // loops may run in either order.
                for y in [0usize, 1] {
                    for (x, sample) in buf[c][y + 1][..RADC_HALF].iter().enumerate() {
                        let mut value = ((*sample as i32) << 4) / (mul[c] as i32);
                        if value < 0 {
                            value = 0;
                        }
                        // Green fills both sites of its 2x2 across the
                        // two sub-rows; red and blue their one diagonal.
                        let (out_row, out_col) = if c != 0 {
                            (row + y * 2 + c - 1, x * 2 + 2 - c)
                        } else {
                            (row + sub * 2 + y, x * 2 + y)
                        };
                        raw[out_row * RADC_WIDTH + out_col] = value as u16;
                    }
                }

                // 4.5 Slide working line 2 down to line 0 as the top
                // predictor context for the next sub-row/stripe; green
                // is offset by one short.
                let off = if c == 0 { 1 } else { 0 };
                for i in 0..(RADC_LINE - off) {
                    buf[c][0][off + i] = buf[c][2][i];
                }
            }
        }

        // 5. Diagonal fix-up over the four stripe rows: the differential
        // green samples at the "odd" sites become absolute values from
        // their horizontal neighbours and the 2048 bias.
        for y in row..row + 4 {
            for x in 0..RADC_WIDTH {
                if (x + y) & 1 == 1 {
                    let left = if x > 0 { x - 1 } else { x + 1 };
                    let right = if x + 1 < RADC_WIDTH { x + 1 } else { x - 1 };
                    let mut value = (raw[y * RADC_WIDTH + x] as i32 - 2048) * 2
                        + (raw[y * RADC_WIDTH + left] as i32 + raw[y * RADC_WIDTH + right] as i32)
                            / 2;
                    if value < 0 {
                        value = 0;
                    }
                    raw[y * RADC_WIDTH + x] = value as u16;
                }
            }
        }
    }

    // 6. Global tone curve, once, over the whole frame.
    let curve = radc_curve();
    for sample in raw.iter_mut() {
        *sample = curve[*sample as usize];
    }
    Ok(raw)
}

// ---------------------------------------------------------- packing

/// Unpack samples stored without compression at `bits` a piece, most
/// significant bits first.
fn unpack(data: &[u8], pixels: usize, bits: u32, little_endian: bool) -> Result<Vec<u16>> {
    match bits {
        8 => {
            if data.len() < pixels {
                return Err(Error::Corrupt(
                    "Kodak frame shorter than its 8-bit samples".into(),
                ));
            }
            Ok(data[..pixels].iter().map(|b| *b as u16).collect())
        }
        12 => {
            let need = pixels
                .div_ceil(2)
                .checked_mul(3)
                .ok_or_else(|| Error::Corrupt("Kodak frame too large".into()))?;
            if data.len() < need {
                return Err(Error::Corrupt(format!(
                    "Kodak frame holds {} bytes of 12-bit samples, want {need}",
                    data.len()
                )));
            }
            let mut out = vec![0u16; pixels];
            for (pair, triple) in out.chunks_mut(2).zip(data.as_chunks::<3>().0) {
                pair[0] = ((triple[0] as u16) << 4) | (triple[1] as u16 >> 4);
                if let Some(second) = pair.get_mut(1) {
                    *second = ((triple[1] as u16 & 0x0f) << 8) | triple[2] as u16;
                }
            }
            Ok(out)
        }
        16 => {
            if data.len() / 2 < pixels {
                return Err(Error::Corrupt(
                    "Kodak frame shorter than its 16-bit samples".into(),
                ));
            }
            Ok(data[..pixels * 2]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|w| {
                    if little_endian {
                        u16::from_le_bytes([w[0], w[1]])
                    } else {
                        u16::from_be_bytes([w[0], w[1]])
                    }
                })
                .collect())
        }
        other => Err(Error::Unsupported(format!(
            "Kodak frame with {other}-bit samples"
        ))),
    }
}

// ------------------------------------------------------ white balance

/// The as-shot balance from the DCS presets. The six entries at
/// 0x0848 are Daylight, Tungsten, Fluorescent, Flash, Custom and
/// Camera Auto, and 0x03FC says which the shot used; the numbers are
/// divisors, so a multiplier is green over the channel.
fn dcs_white_balance(kodak: &Ifd) -> Option<[f32; 4]> {
    let index = kodak
        .get(KODAK_WB_INDEX)
        .and_then(|e| e.u32(0))
        .unwrap_or(0);
    let entry = kodak.get(KODAK_WB_FIRST + index.min(5) as u16)?;
    let (red, green, blue) = (entry.f64(0)?, entry.f64(1)?, entry.f64(2)?);
    if !(red > 0.0 && green > 0.0 && blue > 0.0) {
        return None;
    }
    Some([(green / red) as f32, 1.0, (green / blue) as f32, 1.0])
}

/// The as-shot balance from the KDC directory: multipliers scaled by
/// whatever the green entry is (65536 on every file seen).
fn kdc_white_balance(kdc: &Ifd) -> Option<[f32; 4]> {
    let entry = kdc.get(KDC_WB_ASSHOT)?;
    let (red, green, blue) = (entry.f64(0)?, entry.f64(1)?, entry.f64(2)?);
    if !(red > 0.0 && green > 0.0 && blue > 0.0) {
        return None;
    }
    Some([(red / green) as f32, 1.0, (blue / green) as f32, 1.0])
}

/// A crop rectangle, kept only when it actually lies inside the frame.
/// The EasyShare Z981's does not — its own tags put a 4288-wide
/// picture 52 columns into a 4304-wide sensor — so those files come
/// out uncropped, which is what LibRaw reports for them too.
fn crop_within(
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    frame: (usize, usize),
) -> Option<Rect> {
    (width > 0 && height > 0 && x + width <= frame.0 && y + height <= frame.1).then_some(Rect {
        x,
        y,
        width,
        height,
    })
}

// ---------------------------------------------------------- decoding

/// The DCS/DCR path: a real image IFD with strips.
fn decode_tiff_raw(
    bytes: &[u8],
    tiff: &Tiff<'_>,
    ifd: &Ifd,
    kodak: Option<&Tiff<'_>>,
) -> Result<RawImage> {
    let layout = ImageLayout::of(tiff, ifd)?;
    let [(start, len)] = layout.chunks[..] else {
        return Err(Error::Unsupported(format!(
            "Kodak sensor data in {} strips, want one",
            layout.chunks.len()
        )));
    };
    let strip = &bytes[start..start + len];
    let pixels = layout
        .width
        .checked_mul(layout.height)
        .ok_or_else(|| Error::Corrupt("Kodak frame too large".into()))?;

    let curve_table = curve(tiff, kodak);
    let mut samples = match layout.compression {
        1 => unpack(strip, pixels, layout.bits_per_sample, tiff.little_endian())?,
        7 => {
            let image = crate::ljpeg::decode(strip)?;
            // The DCS backs halve the width and use two components, so
            // the stream's samples already alternate the way the row
            // does; anything else would need a layout this has never
            // seen.
            if image.width * image.components != layout.width || image.height != layout.height {
                return Err(Error::Corrupt(format!(
                    "Kodak lossless JPEG is {}x{}x{}, want {}x{}",
                    image.width, image.height, image.components, layout.width, layout.height
                )));
            }
            image.data
        }
        65000 => decode_65000(strip, ifd, layout.width, layout.height)?,
        32867 => {
            // RADC always reconstructs the full 768x512 sensor frame,
            // wider than the 756x504 the SubIFD tags advertise, and it
            // owns its own colour layout and white level. The oracle
            // dumps the full frame, so build the result here and return
            // rather than fall through the tag-driven path below.
            let compressed_bpp = ifd.get(COMPRESSED_BITS_PER_PIXEL).and_then(|e| e.f64(0));
            let samples = decode_radc(strip, compressed_bpp)?;
            let mut raw = RawImage::new(
                Format::Kodak,
                RADC_WIDTH,
                RADC_HEIGHT,
                1,
                RawData::U16(samples),
                // Filter code 0xE1E1E1E1 (cdesc RGBG) is GRBG, which the
                // §4.4 site formulae place at the frame origin.
                Cfa::GRBG,
            );
            raw.white_level = RADC_MAX as f32;
            return Ok(raw);
        }
        other => return Err(Error::Unsupported(format!("Kodak Compression {other}"))),
    };
    if samples.len() != pixels {
        return Err(Error::Corrupt(format!(
            "Kodak decoder produced {} samples for {}x{}",
            samples.len(),
            layout.width,
            layout.height
        )));
    }

    let mut white = ((1u32 << layout.bits_per_sample.clamp(8, 16)) - 1) as f32;
    if let Some(table) = &curve_table {
        linearize(&mut samples, table);
        // 3700 of a nominal 4095 on the DCS Pro bodies, the full
        // 4095 on the 460/560 backs.
        white = table[table.len() - 1] as f32;
    }

    let cfa = ifd
        .get(tags::CFA_PATTERN)
        .and_then(cfa_from_pattern)
        .unwrap_or(Cfa::GRBG);
    let mut raw = RawImage::new(
        Format::Kodak,
        layout.width,
        layout.height,
        1,
        RawData::U16(samples),
        cfa,
    );
    raw.white_level = white;

    if let Some(kodak) = kodak {
        let root = kodak.root();
        if let Some(coeffs) = dcs_white_balance(root) {
            raw.wb_coeffs = coeffs;
        }
        let number = |tag| root.get(tag).and_then(|e| e.u32(0)).map(|v| v as usize);
        if let (Some(x), Some(y), Some(width), Some(height)) = (
            number(KODAK_CROP_LEFT),
            number(KODAK_CROP_TOP),
            number(KODAK_CROP_WIDTH),
            number(KODAK_CROP_HEIGHT),
        ) {
            if let Some(crop) = crop_within(x, y, width, height, (layout.width, layout.height)) {
                raw.crop = crop;
            }
        }
    }
    Ok(raw)
}

/// The EasyShare path: no image IFD at all, only the private
/// directories, and the samples packed somewhere the file names very
/// indirectly.
fn decode_kdc(bytes: &[u8], tiff: &Tiff<'_>, kdc: &Tiff<'_>) -> Result<RawImage> {
    let root = kdc.root();
    let number = |tag| root.get(tag).and_then(|e| e.u32(0)).map(|v| v as usize);
    let width =
        number(KDC_WIDTH).ok_or_else(|| Error::Corrupt("KDC without a frame width".into()))?;
    // The stated height is one short of the frame LibRaw reads and is
    // odd, which a Bayer frame cannot be; rounding it up to the next
    // even row gives the frame the data actually holds.
    let height = number(KDC_HEIGHT)
        .map(|h| h + (h & 1))
        .ok_or_else(|| Error::Corrupt("KDC without a frame height".into()))?;
    // 0xFA18 is the sample depth on the Z981 and nonsense (65532) on
    // the P880, so it is believed only when it could be one.
    let bits = number(KDC_DEPTH)
        .filter(|b| (8..=16).contains(b))
        .unwrap_or(12) as u32;
    if width == 0 || height == 0 {
        return Err(Error::Corrupt("KDC with an empty frame".into()));
    }

    // Where the samples start is the one thing these files never say
    // plainly. The SubIFD's 0xFD04 is a mixed bag of values that ends
    // in a long arithmetic run — one entry per band of rows, spaced by
    // exactly a band's worth of packed bytes — and 0xFD14 is a bias
    // (-64) that has to come off the first of them. Both were read off
    // the file against LibRaw's own frame; a KDC that has neither is
    // rejected rather than guessed at.
    let sub = tiff
        .all()
        .into_iter()
        .find(|ifd| ifd.has(KDC_SUB_OFFSETS))
        .ok_or_else(|| Error::Unsupported("KDC without the 0xFD04 band table".into()))?;
    let bands = sub
        .get(KDC_SUB_OFFSETS)
        .map(|e| e.u32s())
        .unwrap_or_default();
    let bias = sub
        .get(KDC_SUB_OFFSET_BIAS)
        .and_then(|e| e.f64(0))
        .unwrap_or(0.0) as i64;
    let first = longest_ramp(&bands)
        .ok_or_else(|| Error::Unsupported("KDC band table with no run of band offsets".into()))?;
    let start = usize::try_from(first as i64 - bias)
        .map_err(|_| Error::Corrupt("KDC sample offset out of range".into()))?;
    if start >= bytes.len() {
        return Err(Error::Corrupt(
            "KDC sample offset past the end of the file".into(),
        ));
    }

    let samples = unpack(&bytes[start..], width * height, bits, tiff.little_endian())?;
    // The filter array is spelled out twice and the two disagree: the
    // KDC directory's 0xFA15 names the array of the *cropped* picture
    // ("RGGB"), the SubIFD's CFAPattern the array of the frame this
    // decoder hands out. The frame's is the one that belongs here.
    let cfa = sub
        .get(KDC_SUB_CFA_PATTERN)
        .and_then(cfa_from_pattern)
        .unwrap_or(Cfa::GRBG);
    let mut raw = RawImage::new(Format::Kodak, width, height, 1, RawData::U16(samples), cfa);
    raw.white_level = ((1u32 << bits.clamp(8, 16)) - 1) as f32;
    if let Some(coeffs) = kdc_white_balance(root) {
        raw.wb_coeffs = coeffs;
    }
    if let (Some(x), Some(y), Some(w), Some(h)) = (
        number(KDC_CROP_LEFT),
        number(KDC_CROP_TOP),
        number(KDC_IMAGE_WIDTH),
        number(KDC_IMAGE_HEIGHT),
    ) {
        // The margins are a pixel out on the row axis: taking the
        // P880's 3264x2448 picture at its stated (8, 1) would give the
        // cropped image a GRBG array, and the same directory says in
        // 0xFA15 that the picture is BGGR. The filter array is the
        // camera's own statement about the crop, so the origin is
        // nudged by up to one pixel to agree with it.
        let (x, y) = match kdc_named_cfa(root) {
            Some(named) => [(0, 0), (1, 0), (0, 1), (1, 1)]
                .into_iter()
                .map(|(dx, dy)| (x + dx, y + dy))
                .find(|(x, y)| raw.cfa.shifted(*x, *y) == named)
                .unwrap_or((x, y)),
            None => (x, y),
        };
        if let Some(crop) = crop_within(x, y, w, h, (width, height)) {
            raw.crop = crop;
        }
    }
    Ok(raw)
}

/// The filter array of the *cropped* picture, spelled out in ASCII in
/// the KDC directory ("RGGB", "BGGR").
fn kdc_named_cfa(kdc: &Ifd) -> Option<Cfa> {
    let name = kdc.get(KDC_CFA)?.str()?;
    if name.len() != 4 {
        return None;
    }
    let mut colors = [CfaColor::Red; 4];
    for (color, letter) in colors.iter_mut().zip(name.chars()) {
        *color = match letter {
            'R' => CfaColor::Red,
            'G' => CfaColor::Green,
            'B' => CfaColor::Blue,
            _ => return None,
        };
    }
    Some(Cfa::Bayer(colors))
}

/// The first value of the longest run of equal, positive differences
/// in `values`, which is how the band table hides its offsets among
/// its other numbers. At least four bands are wanted so a chance pair
/// cannot win.
fn longest_ramp(values: &[u32]) -> Option<u32> {
    let (mut best_start, mut best_len) = (0usize, 0usize);
    let mut i = 0;
    while i + 1 < values.len() {
        let step = values[i + 1].checked_sub(values[i]).filter(|s| *s > 0);
        let Some(step) = step else {
            i += 1;
            continue;
        };
        let mut j = i + 1;
        while j + 1 < values.len() && values[j + 1].checked_sub(values[j]) == Some(step) {
            j += 1;
        }
        if j - i + 1 > best_len {
            best_len = j - i + 1;
            best_start = i;
        }
        i = j.max(i + 1);
    }
    (best_len >= 4).then(|| values[best_start])
}

pub fn decode(bytes: &[u8]) -> Result<RawImage> {
    let tiff = Tiff::parse(bytes)?;
    let kodak = private_ifd(&tiff, KODAK_IFD);
    let kdc = private_ifd(&tiff, KDC_IFD);

    let mut raw = match raw_ifd(&tiff) {
        Some(ifd) => decode_tiff_raw(bytes, &tiff, ifd, kodak.as_ref())?,
        None => {
            let kdc = kdc.as_ref().ok_or_else(|| {
                Error::Unsupported(
                    "Kodak TIFF with neither a CFA image IFD nor a KDC directory".into(),
                )
            })?;
            decode_kdc(bytes, &tiff, kdc)?
        }
    };

    let (make, model) = tiff.make_model();
    raw.set_camera(&make, &model);
    raw.orientation = common::orientation(&tiff);
    raw.metadata = common::metadata(&tiff);
    // The DCS bodies keep every preview in Kodak's own compression, so
    // there is frequently no JPEG to hand out at all; the EasyShare
    // compacts point at a full-size one with JPEGInterchangeFormat.
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
    use std::path::{Path, PathBuf};

    /// Every file under `$SCHIST_RAW_CORPUS` with one of `extensions`,
    /// recursively. Empty when the variable is unset, which is how the
    /// corpus tests skip on a machine without the samples.
    fn corpus(extensions: &[&str]) -> Vec<PathBuf> {
        let Ok(root) = std::env::var("SCHIST_RAW_CORPUS") else {
            return Vec::new();
        };
        let mut found = Vec::new();
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
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| extensions.iter().any(|want| e.eq_ignore_ascii_case(want)))
                {
                    found.push(path);
                }
            }
        }
        found.sort();
        found
    }

    /// LibRaw's `unprocessed_raw -T` output beside the sample.
    fn oracle(path: &Path) -> Option<(usize, usize, Vec<u16>)> {
        let mut name = path.as_os_str().to_os_string();
        name.push(".tiff");
        let image = image::open(PathBuf::from(name)).ok()?.into_luma16();
        let (width, height) = (image.width() as usize, image.height() as usize);
        Some((width, height, image.into_raw()))
    }

    /// `raw-identify -v -w` output beside the sample.
    fn identify(path: &Path) -> Option<String> {
        let mut name = path.as_os_str().to_os_string();
        name.push(".identify.txt");
        std::fs::read_to_string(PathBuf::from(name)).ok()
    }

    /// The text after `key:` on the line that starts with it.
    fn field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
        text.lines()
            .map(str::trim)
            .find(|line| line.starts_with(key))
            .map(|line| line[key.len()..].trim())
    }

    /// A "W x H" pair.
    fn size(text: &str, key: &str) -> Option<(usize, usize)> {
        let value = field(text, key)?;
        let (w, h) = value.split_once('x')?;
        Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
    }

    /// What LibRaw's "Image flip" means as an [`Orientation`].
    fn flip(text: &str) -> Option<crate::Orientation> {
        Some(match field(text, "Image flip:")?.parse::<u32>().ok()? {
            3 => crate::Orientation::Rotate180,
            5 => crate::Orientation::Rotate270CW,
            6 => crate::Orientation::Rotate90CW,
            _ => crate::Orientation::Normal,
        })
    }

    /// The first four letters of the "Filter pattern" line as a CFA.
    fn filter_pattern(text: &str) -> Option<Cfa> {
        let pattern = field(text, "Filter pattern:")?;
        let mut colors = [CfaColor::Red; 4];
        for (color, letter) in colors.iter_mut().zip(pattern.chars()) {
            *color = match letter {
                'R' => CfaColor::Red,
                'G' => CfaColor::Green,
                'B' => CfaColor::Blue,
                _ => return None,
            };
        }
        Some(Cfa::Bayer(colors))
    }

    /// The as-shot multipliers, normalised to green, from the
    /// "As shot" row of the makernote white-balance table.
    fn as_shot(text: &str) -> Option<[f32; 3]> {
        let row = field(text, "As shot")?;
        let numbers: Vec<f32> = row
            .split_whitespace()
            .take(4)
            .map_while(|v| v.parse::<f32>().ok())
            .collect();
        let [red, green, blue, ..] = numbers[..] else {
            return None;
        };
        (green > 0.0).then(|| [red / green, 1.0, blue / green])
    }

    /// Compare a decoded frame with the oracle sample for sample.
    fn compare(path: &Path, raw: &RawImage) {
        let Some((width, height, expect)) = oracle(path) else {
            eprintln!("{}: no oracle TIFF, data not checked", path.display());
            return;
        };
        assert_eq!(
            (raw.width, raw.height),
            (width, height),
            "{}: frame is {}x{}, oracle {width}x{height}",
            path.display(),
            raw.width,
            raw.height
        );
        let RawData::U16(got) = &raw.data else {
            panic!("{}: expected integer samples", path.display())
        };
        let mut wrong = 0usize;
        let mut first = Vec::new();
        for (i, (a, b)) in got.iter().zip(expect.iter()).enumerate() {
            if a != b {
                wrong += 1;
                if first.len() < 8 {
                    first.push(format!("({}, {}): {a} not {b}", i % width, i / width));
                }
            }
        }
        assert_eq!(
            wrong,
            0,
            "{}: {wrong} samples differ; {}",
            path.display(),
            first.join(", ")
        );
    }

    /// Levels, balance, crop, orientation and CFA against
    /// `raw-identify`.
    fn compare_metadata(path: &Path, raw: &RawImage) {
        let Some(text) = identify(path) else { return };
        if let Some(cfa) = filter_pattern(&text) {
            assert_eq!(raw.cfa, cfa, "{}: filter pattern", path.display());
        }
        if let Some(orientation) = flip(&text) {
            assert_eq!(
                raw.orientation,
                orientation,
                "{}: orientation",
                path.display()
            );
        }
        if let Some((width, height)) = size(&text, "Image size:") {
            let (x, y) = match field(&text, "Raw inset, width x height:") {
                Some(inset) => {
                    let left = inset
                        .split("left:")
                        .nth(1)
                        .and_then(|v| v.split_whitespace().next().and_then(|v| v.parse().ok()));
                    let top = inset
                        .split("top:")
                        .nth(1)
                        .and_then(|v| v.split_whitespace().next().and_then(|v| v.parse().ok()));
                    (left.unwrap_or(0), top.unwrap_or(0))
                }
                None => (0, 0),
            };
            let inset = field(&text, "Raw inset, width x height:")
                .and_then(|v| v.split_whitespace().next().map(str::to_string));
            let expect = match inset.and_then(|w| w.parse::<usize>().ok()) {
                Some(w) => {
                    let h = field(&text, "Raw inset, width x height:")
                        .and_then(|v| v.split_whitespace().nth(2))
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(height);
                    Rect {
                        x,
                        y,
                        width: w,
                        height: h,
                    }
                }
                None => Rect {
                    x: 0,
                    y: 0,
                    width,
                    height,
                },
            };
            if raw.crop != expect {
                eprintln!("{}: crop {:?}, LibRaw {expect:?}", path.display(), raw.crop);
            }
        }
        if let Some(expect) = as_shot(&text) {
            for (got, want) in raw.wb_coeffs.iter().zip(expect.iter()) {
                if (got - want).abs() > want * 0.02 {
                    eprintln!(
                        "{}: white balance {:?}, LibRaw {expect:?}",
                        path.display(),
                        raw.wb_coeffs
                    );
                    break;
                }
            }
        }
    }

    /// Cutting a file short must never panic, whatever it does return.
    fn truncations(path: &Path) {
        let bytes = std::fs::read(path).expect("sample readable");
        let mut seed = bytes.len() as u64;
        for _ in 0..10 {
            // A cheap deterministic spread of cut points.
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let at = (seed >> 33) as usize % bytes.len().max(1);
            let _ = crate::decode(&bytes[..at]);
            let _ = crate::probe(&bytes[..at]);
        }
    }

    #[test]
    fn decodes_a_hand_built_65000_segment() {
        // Four pixels, lengths 3, 3, 2, 1 (low nibble first), then the
        // differences 5, 6, 2, 1 packed into one big-endian word from
        // its least significant bit up.
        let data = [0x33, 0x12, 0x01, 0xb5];
        let mut out = [0u16; 4];
        decode_65000_segment(&data, &mut out).expect("segment decodes");
        assert_eq!(out, [5, 6, 7, 7]);
    }

    #[test]
    fn a_65000_length_over_twelve_bits_is_corrupt() {
        let data = [0xff, 0x00, 0x00, 0x00];
        let mut out = [0u16; 2];
        assert!(matches!(
            decode_65000_segment(&data, &mut out),
            Err(Error::Corrupt(_))
        ));
    }

    #[test]
    fn a_truncated_65000_segment_runs_out_in_zeros() {
        // Lengths present, bitstream missing: the pump reads zeros, so
        // the differences are the most negative the lengths allow and
        // nothing panics.
        let data = [0x33, 0x12];
        let mut out = [0u16; 4];
        decode_65000_segment(&data, &mut out).expect("no bitstream is still decodable");
    }

    #[test]
    fn finds_the_band_offsets_among_the_other_numbers() {
        let values = [
            1, 3158, 654611, 2, 74722, 480, 1, 85952, 189248, 292544, 395840, 499136,
        ];
        assert_eq!(longest_ramp(&values), Some(85952));
        assert_eq!(longest_ramp(&[1, 2, 3]), None);
        assert_eq!(longest_ramp(&[]), None);
    }

    #[test]
    fn unpacks_twelve_bit_samples() {
        let data = [0xab, 0xcd, 0xef, 0x12, 0x34, 0x56];
        assert_eq!(
            unpack(&data, 4, 12, false).unwrap(),
            vec![0xabc, 0xdef, 0x123, 0x456]
        );
    }

    #[test]
    fn a_linearisation_curve_is_a_lookup_with_a_clamp() {
        let table = [0u16, 10, 20, 30];
        let mut samples = [0, 2, 9];
        linearize(&mut samples, &table);
        assert_eq!(samples, [0, 20, 30]);
    }

    #[test]
    fn radc_tables_fill_sequentially() {
        let t = RadcTables::build(3);
        // Tree table 0 opens with a length-1 code for symbol 1, filling
        // the low half of its 256 slots, then a length-2 code for 3.
        assert_eq!(t.tables[0][0], (1 << 8) | 1);
        assert_eq!(t.tables[0][127], (1 << 8) | 1);
        assert_eq!(t.tables[0][128], (2 << 8) | 3);
        // Table 1 opens with length-1 symbol 0.
        // Length 1, symbol 0.
        assert_eq!(t.tables[1][0], 1 << 8);
        // Table 10's first three entries (1,0)(2,2)(2,-2) exactly fill
        // its 256 slots: 128 + 64 + 64.
        assert_eq!(t.tables[10][0], 1 << 8);
        assert_eq!(t.tables[10][128], (2 << 8) | 2);
        assert_eq!(t.tables[10][192], (2 << 8) | (0xFEu16)); // -2 as a byte
                                                             // A negative symbol round-trips through the signed-char decode.
        let entry = t.tables[10][192];
        assert_eq!((entry as u8 as i8) as i32, -2);
    }

    #[test]
    fn radc_tables_are_complete_and_prefix_free() {
        // Every slot of all 19 tables must carry a usable code length:
        // a zero would stall the reader, and a table that did not add up
        // to 256 slots would silently shift every later table along. Per
        // table the lengths must satisfy Kraft equality, sum(2^(8-L)) ==
        // 256, which is exactly what "fills its 256 slots" means.
        let t = RadcTables::build(3);
        for (index, table) in t.tables.iter().enumerate() {
            let mut slots = 0usize;
            let mut i = 0usize;
            while i < 256 {
                let length = (table[i] >> 8) as u32;
                assert!(
                    (1..=8).contains(&length),
                    "table {index} slot {i} has code length {length}"
                );
                let run = 256usize >> length;
                // Every slot a code covers must repeat that code.
                for j in i..i + run {
                    assert_eq!(table[j], table[i], "table {index} slot {j} breaks its run");
                }
                slots += run;
                i += run;
            }
            assert_eq!(slots, 256, "table {index} does not fill 256 slots");
        }
    }

    #[test]
    fn radc_shift_follows_compressed_bits_per_pixel() {
        // The DC50 sample records 152/100; only a 243 selects the finer
        // quantiser, and a file with no tag at all gets the coarse one.
        assert_eq!(radc_shift(Some(1.52)), 3);
        assert_eq!(radc_shift(None), 3);
        assert_eq!(radc_shift(Some(243.0)), 2);
    }

    #[test]
    fn radc_escape_table_is_a_coarse_quantiser() {
        // shift 3: five-bit codes over levels that are the index rounded
        // down to a multiple of 8 with bit 2 set.
        let t = RadcTables::build(3);
        assert_eq!(t.tables[18][0], (5 << 8) | 4);
        assert_eq!(t.tables[18][7], (5 << 8) | 4);
        assert_eq!(t.tables[18][8], (5 << 8) | 12);
        assert_eq!(t.tables[18][255], (5 << 8) | 252);
        // shift 2: six-bit codes over a finer grid.
        let fine = RadcTables::build(2);
        assert_eq!(fine.tables[18][0], (6 << 8) | 2);
        assert_eq!(fine.tables[18][255], (6 << 8) | 254);
    }

    #[test]
    fn radc_bit_reader_is_msb_first_and_zero_fills() {
        // 0b1011_0010, 0b1100_0000 then nothing.
        let mut r = Radc::new(&[0xB2, 0xC0], 3);
        assert_eq!(r.getbits(3), 0b101);
        assert_eq!(r.getbits(5), 0b10010);
        assert_eq!(r.getbits(2), 0b11);
        // Past the payload the reader yields zeros rather than failing.
        assert_eq!(r.getbits(8), 0);
        assert_eq!(r.getbits(8), 0);
    }

    #[test]
    fn radc_curve_passes_through_its_knots() {
        let curve = radc_curve();
        assert_eq!(curve[0], 0);
        assert_eq!(curve[1280], 1344);
        assert_eq!(curve[2320], 3616);
        assert_eq!(curve[3328], 8000);
        assert_eq!(curve[4095], 16383);
        // Everything above the last real knot clips to the maximum.
        assert_eq!(curve[4096], 16383);
        assert_eq!(curve[65535], 16383);
        // Monotone non-decreasing across a segment.
        assert!(curve[100] <= curve[200] && curve[200] <= curve[1280]);
    }

    #[test]
    fn radc_rejects_a_zero_channel_gain() {
        // Three six-bit gains of zero: the first stripe is corrupt.
        let strip = [0u8; 64];
        assert!(matches!(decode_radc(&strip, None), Err(Error::Corrupt(_))));
    }

    #[test]
    fn radc_decodes_hostile_input_without_panicking() {
        // Garbage must always terminate and either decode to a full
        // frame or be rejected as corrupt, never loop or panic. A run of
        // cut points exercises the zero-fill and the walk's bounds.
        for len in [0usize, 1, 7, 64, 500, 4096] {
            let strip = vec![0xA5u8; len];
            match decode_radc(&strip, None) {
                Ok(frame) => assert_eq!(frame.len(), RADC_WIDTH * RADC_HEIGHT),
                Err(Error::Corrupt(_)) => {}
                Err(other) => panic!("unexpected error on garbage: {other}"),
            }
        }
    }

    #[test]
    fn garbage_is_rejected() {
        assert!(decode(b"MM\0*nonsense").is_err());
        assert!(decode(&[]).is_err());
    }

    #[test]
    fn corpus_matches_the_oracle() {
        for path in corpus(&["dcr", "kdc", "tif"]) {
            let bytes = std::fs::read(&path).expect("sample readable");
            if crate::probe(&bytes) != Some(crate::Format::Kodak) {
                // The corpus mixes vendors under one extension: a TIFF
                // some other camera wrote is not this module's to read.
                continue;
            }
            let raw = match crate::decode(&bytes) {
                Ok(raw) => raw,
                Err(Error::Unsupported(why)) => {
                    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                    // The DC50 (RADC, Compression 32867) now decodes. The
                    // only Kodak variants still out of scope are the s=2
                    // RADC quantiser (CompressedBitsPerPixel 243, no
                    // corpus sample) and the DC120's separate bilinear
                    // scheme; neither appears in the corpus, so the
                    // allow-list is empty and any Unsupported is a bug.
                    let allowed: &[(&str, &str)] = &[];
                    let reason = allowed
                        .iter()
                        .find(|(file, _)| name.contains(file))
                        .unwrap_or_else(|| {
                            panic!("{}: unexpected Unsupported: {why}", path.display())
                        });
                    eprintln!(
                        "{}: unsupported as documented ({}): {why}",
                        path.display(),
                        reason.1
                    );
                    continue;
                }
                Err(other) => panic!("{}: {other}", path.display()),
            };
            raw.validate().expect("decoded frame is self-consistent");
            compare(&path, &raw);
            compare_metadata(&path, &raw);
            if let Some(preview) = &raw.preview {
                image::load_from_memory(preview)
                    .unwrap_or_else(|e| panic!("{}: preview will not decode: {e}", path.display()));
            }
        }
    }

    #[test]
    fn truncated_files_never_panic() {
        for path in corpus(&["dcr", "kdc", "tif"]) {
            truncations(&path);
        }
    }
}
