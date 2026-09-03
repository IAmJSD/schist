//! Canon's CRX codec: the wavelet-and-Golomb scheme inside a CR3.
//!
//! A CRX image is a grid of *tiles*. Each tile holds one *component*
//! per colour-filter position — a Bayer frame is split into its four
//! 2x2 sub-planes, so a component is half the frame wide and half as
//! tall — and each component holds one *subband* per wavelet band.
//! With zero wavelet levels (what Canon's "RAW" quality writes) a
//! component is a single LL band whose coefficients are the sensor
//! samples themselves; with levels (what "CRAW" writes) there are
//! `3 * levels + 1` bands, a quantiser per band and a 5/3 integer
//! lifting wavelet to invert.
//!
//! Every subband is an independently addressed bitstream with its own
//! entropy-coder state, so tiles, components and bands can all be
//! decoded in parallel; only the wavelet joins them back up.
//!
//! The shapes and sizes come from two places: the `CMP1` box in the
//! track's sample entry ([`ImageHeader`]), and a header of small
//! tag/size records in front of the sample data in `mdat`
//! ([`parse_tiles`]).
//!
//! ## The entropy coder in one page
//!
//! Bits are read most-significant first straight down the bytes, with
//! no stuffing and no alignment anywhere. A coefficient magnitude `m`
//! is a Golomb-Rice code with a running parameter `K`: `m >> K` in
//! unary (that many zeros, then a one), then the low `K` bits. A
//! unary prefix of 41 or more zeros is an escape and the magnitude is
//! a flat 21-bit field instead — which is how every lossless band
//! opens, because its first residual is `black - 8192`. `m` folds to
//! a signed residual as `d = -(m & 1) ^ (m >> 1)`.
//!
//! `K` adapts after every symbol from the magnitude (blended with the
//! gradient one column to the right on the line above, where the line
//! decoder says so) and carries across line boundaries; see [`adapt`].
//!
//! Sitting under all of that is a run mode: at a position where the
//! neighbourhood says the next samples are probably a repeat, one bit
//! says whether a run is present and an exponential/remainder code
//! gives its length. A zero-length run is normal and frequent — it
//! costs one bit and means "no run here". It is the single thing a
//! partial implementation misses: the flag bit is then read as the
//! start of the next symbol's unary prefix and everything after it is
//! noise.
//!
//! Two line coders use those pieces. Mode A ([`Coder::mode_a_line`])
//! codes the LL band, predicting from the left on line 0 and from
//! Canon's own four-way gradient selector after that. Mode B
//! ([`Coder::mode_b_line`]) codes the detail bands, where the sample
//! *is* the residual, runs are tested against a zero neighbourhood
//! and `K` is steered by a per-column memory of what the line above
//! used.

use crate::bits::{BitPump, BitPumpMsb};
use crate::{Error, Result};
use rayon::prelude::*;

/// The `CMP1` box: what shape the coded image is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageHeader {
    /// `0x0100` on the EOS R / RP / M50 / 90D generation, `0x0200`
    /// from the R5 on. It selects which dialect the `mdat` index
    /// speaks and where the quantiser lives (see [`parse_tiles`]).
    pub version: u16,
    /// The whole coded frame, which is also the raw sensor frame.
    pub width: usize,
    pub height: usize,
    /// One tile of it, in frame coordinates. Tiles divide the frame
    /// left to right, top to bottom; the last tile in a row or column
    /// is whatever is left.
    pub tile_width: usize,
    pub tile_height: usize,
    /// Bits per sensor sample, 14 on every body seen.
    pub bits: u32,
    /// Components per tile: 4, the Bayer sub-planes, in the semantic
    /// order R, G on R's row, G on B's row, B. (A single 8-bit
    /// non-Bayer plane is also allowed by the format.)
    pub planes: usize,
    /// Which 2x2 phase those four sit on; see [`cfa_position`]. 0 on
    /// every full-size raw track, 1 on the small "SD" one.
    pub cfa_layout: u8,
    /// 0 for the normal Bayer path, 1 for a signed one and 3 for a
    /// decorrelated-colour one. Only 0 has ever been seen.
    pub enc_type: u8,
    /// Wavelet levels: 0 for lossless RAW, 3 for cRAW so far.
    pub levels: usize,
    /// Whether the wavelet runs across tile columns / rows. The
    /// encoder only sets these when there is both a wavelet and more
    /// than one tile in that direction; the decoder derives the tile
    /// neighbour flags from the grid instead and only reports them.
    pub tile_cols_linked: bool,
    pub tile_rows_linked: bool,
    /// Bytes of tag/size records in front of the sample data.
    pub mdat_header_size: usize,
    /// Precision of the green sum in the `enc_type == 3`
    /// reconstruction; `bits` unless an extended header says
    /// otherwise.
    pub median_bits: u32,
}

impl ImageHeader {
    /// Parse the payload of a `CMP1` box (the bytes after its 8-byte
    /// box header; 52 of them on every body seen).
    pub fn parse(payload: &[u8]) -> Result<ImageHeader> {
        if payload.len() < 36 {
            return Err(Error::Corrupt(format!("CMP1 is {} bytes", payload.len())));
        }
        let u16_at = |at: usize| u16::from_be_bytes([payload[at], payload[at + 1]]);
        let u32_at = |at: usize| {
            u32::from_be_bytes([
                payload[at],
                payload[at + 1],
                payload[at + 2],
                payload[at + 3],
            ])
        };
        let version = u16_at(4);
        let width = u32_at(8) as usize;
        let height = u32_at(12) as usize;
        let tile_width = u32_at(16) as usize;
        let tile_height = u32_at(20) as usize;
        let bits = payload[24] as u32;
        // Two fields to the byte from here on: a count in the high
        // nibble, a code in the low one.
        let planes = (payload[25] >> 4) as usize;
        let cfa_layout = payload[25] & 0xf;
        let enc_type = payload[26] >> 4;
        let levels = (payload[26] & 0xf) as usize;
        let flags = payload[27];
        let mdat_header_size = u32_at(28) as usize;
        // The extended header carries a second precision for the
        // decorrelated-colour path. No file in the corpus sets it.
        let median_bits = if payload[32] & 0x80 != 0
            && planes == 4
            && payload.len() > 56
            && payload[56] & 0x40 != 0
            && payload.len() > 84
        {
            payload[84] as u32
        } else {
            bits
        };

        let header = ImageHeader {
            version,
            width,
            height,
            tile_width,
            tile_height,
            bits,
            planes,
            cfa_layout,
            enc_type,
            levels,
            tile_cols_linked: flags & 0x80 != 0,
            tile_rows_linked: flags & 0x40 != 0,
            mdat_header_size,
            median_bits,
        };
        header.validate()?;
        Ok(header)
    }

    /// The constraints a conforming stream satisfies. They are worth
    /// checking up front: everything downstream sizes buffers from
    /// these numbers, and a file that breaks one of them is lying
    /// about itself rather than using a variant we do not know.
    fn validate(&self) -> Result<()> {
        let bad = |what: String| Err(Error::Corrupt(what));
        // The frame ceiling first: every allocation below is sized
        // from these two fields.
        crate::frame_samples(self.width, self.height, 1)?;
        if self.bits == 0 {
            return bad("CRX with no bits a sample".into());
        }
        if !matches!(self.version, 0x0100 | 0x0200) {
            return Err(Error::Unsupported(format!(
                "CRX header version {:#06x}",
                self.version
            )));
        }
        if self.mdat_header_size == 0 {
            return bad("CRX with no mdat index".into());
        }
        if self.levels > 3 {
            return bad(format!("CRX with {} wavelet levels", self.levels));
        }
        match self.enc_type {
            1 if self.bits > 15 => return bad(format!("CRX enc 1 with {} bits", self.bits)),
            0 | 3 if self.bits > 14 => {
                return bad(format!("CRX enc {} with {} bits", self.enc_type, self.bits))
            }
            0 | 1 | 3 => {}
            other => return bad(format!("CRX encoding {other}")),
        }
        if self.planes == 1 {
            if self.cfa_layout != 0 || self.enc_type != 0 || self.bits != 8 {
                return bad("CRX single plane that is not 8-bit greyscale".into());
            }
        } else if self.planes != 4 {
            return bad(format!("CRX with {} planes", self.planes));
        } else if self.cfa_layout > 3 || self.bits == 8 {
            return bad(format!(
                "CRX Bayer frame, layout {}, {} bits",
                self.cfa_layout, self.bits
            ));
        } else if (self.width | self.height | self.tile_width | self.tile_height) & 1 != 0 {
            return bad("CRX Bayer frame with an odd dimension".into());
        }
        if self.width == 0
            || self.height == 0
            || self.tile_width == 0
            || self.tile_height == 0
            || self.tile_width > self.width
            || self.tile_height > self.height
        {
            return bad(format!(
                "CRX frame {}x{} in tiles of {}x{}",
                self.width, self.height, self.tile_width, self.tile_height
            ));
        }
        let (pw, ph) = self.plane_size();
        let (tw, th) = self.tile_size();
        // The wavelet needs a couple of coefficients at every level
        // and the frame has to fit the 15-bit coordinates the tile
        // records carry, so both ends are bounded.
        if tw < 22 || th < 22 || pw > 0x7fff || ph > 0x7fff {
            return bad(format!("CRX plane {pw}x{ph} in tiles of {tw}x{th}"));
        }
        let (cols, rows) = self.tile_grid();
        if cols > 255 || rows > 255 || pw - tw * (cols - 1) < 22 || ph - th * (rows - 1) < 22 {
            return bad(format!("CRX tile grid {cols}x{rows}"));
        }
        Ok(())
    }

    /// The frame in the *plane domain*: a Bayer frame's four
    /// components are each half the frame across, and every tile and
    /// band shape below is expressed in those halved coordinates.
    pub fn plane_size(&self) -> (usize, usize) {
        match self.planes {
            1 => (self.width, self.height),
            _ => (self.width / 2, self.height / 2),
        }
    }

    /// One tile, in the plane domain.
    pub fn tile_size(&self) -> (usize, usize) {
        match self.planes {
            1 => (self.tile_width, self.tile_height),
            _ => (self.tile_width / 2, self.tile_height / 2),
        }
    }

    /// Tiles across and down.
    pub fn tile_grid(&self) -> (usize, usize) {
        let (pw, ph) = self.plane_size();
        let (tw, th) = self.tile_size();
        (pw.div_ceil(tw.max(1)), ph.div_ceil(th.max(1)))
    }

    /// Bands in one component: one LL band, then HL/LH/HH per level.
    pub fn bands(&self) -> usize {
        3 * self.levels + 1
    }
}

/// Tile neighbour flags. They are derived from the tile's place in
/// the grid, never from the header's "linked" bits, and they say on
/// which sides the tile's bands carry a few extra coefficients so the
/// wavelet has real data at an internal seam instead of a symmetric
/// extension.
const RIGHT: u8 = 1;
const LEFT: u8 = 2;
const BELOW: u8 = 4;
const ABOVE: u8 = 8;

/// One subband's slice of a component's data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Band {
    /// Where the band's bits are, inside the whole `mdat` sample.
    pub data: std::ops::Range<usize>,
    /// Bytes at the end of that range which are not bitstream.
    pub trailing: usize,
    /// Index within the component, 0 for LL.
    pub index: usize,
    /// Version 1: the band's quantiser exponent, 4 (unity) when
    /// lossless. Version 2 leaves it 4 and scales the tile's
    /// quantiser map instead.
    pub q_param: i32,
    /// Version 1: whether a quantiser delta precedes every line.
    pub q_per_line: bool,
    /// Version 2: the band's scaling of the tile's quantiser map.
    pub q_step_base: i32,
    pub q_step_mult: i32,
}

impl Band {
    /// The bytes that actually hold the entropy-coded bitstream.
    fn bitstream<'a>(&self, sample: &'a [u8]) -> &'a [u8] {
        let end = self.data.end.saturating_sub(self.trailing);
        sample.get(self.data.start..end).unwrap_or(&[])
    }
}

/// One component — one Bayer sub-plane — of a tile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plane {
    pub data: std::ops::Range<usize>,
    pub index: usize,
    /// Whether the LL band uses the predictive line coder (Mode A).
    /// Set in every file seen; when clear the LL band is coded like a
    /// detail band.
    pub ref_prev_line: bool,
    /// The spatial-domain rounding mask, 0 in every file seen. It is
    /// only legal with no wavelet, where it makes Mode A lossy.
    pub round_mask: i32,
    pub round_bits: u32,
    pub bands: Vec<Band>,
}

/// One tile of the frame. Its position and size are in the plane
/// domain (see [`ImageHeader::plane_size`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tile {
    pub data: std::ops::Range<usize>,
    /// Position in the tile grid.
    pub col: usize,
    pub row: usize,
    /// Where this tile sits in a component, and how big it is.
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
    /// Which sides have a neighbour: `RIGHT | LEFT | BELOW | ABOVE`.
    pub neighbours: u8,
    /// Version 2 only: the quantiser map's bitstream at the front of
    /// the tile, and a few bytes after it whose meaning is unknown
    /// (4 on an R5, 7 on an R8) which are simply skipped.
    pub qp_size: usize,
    pub extra_size: usize,
    pub planes: Vec<Plane>,
}

/// A record in the `mdat` index: a big-endian tag, a big-endian
/// payload length, then the payload.
struct Record<'a> {
    tag: u16,
    payload: &'a [u8],
}

fn records(header: &[u8]) -> Result<Vec<Record<'_>>> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while at + 4 <= header.len() {
        let tag = u16::from_be_bytes([header[at], header[at + 1]]);
        let len = u16::from_be_bytes([header[at + 2], header[at + 3]]) as usize;
        let end = at + 4 + len;
        if end > header.len() {
            return Err(Error::Corrupt(
                "CRX header record runs past the header".into(),
            ));
        }
        out.push(Record {
            tag,
            payload: &header[at + 4..end],
        });
        at = end;
    }
    Ok(out)
}

fn be16(b: &[u8], at: usize) -> Result<u32> {
    b.get(at..at + 2)
        .map(|s| u16::from_be_bytes([s[0], s[1]]) as u32)
        .ok_or_else(|| Error::Corrupt("short CRX header record".into()))
}

fn be32(b: &[u8], at: usize) -> Result<u32> {
    b.get(at..at + 4)
        .map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
        .ok_or_else(|| Error::Corrupt("short CRX header record".into()))
}

/// Read the tag/size records in front of a sample and turn them into
/// the tile / component / band tree, with each node's byte range
/// inside `sample` filled in.
///
/// The records come in two dialects. Version 1 (EOS R generation)
/// uses tags 0xff01 tile, 0xff02 component, 0xff03 band, each with an
/// 8-byte payload of a size and one packed word. Version 2 (R5
/// onwards) uses 0xff11 / 0xff12 / 0xff13; its band record is sixteen
/// bytes carrying a scale rather than an exponent, and its tile
/// record may be sixteen too, saying how much of the tile is a
/// quantiser map in front of the components. In both dialects a
/// node's children tile its bytes exactly, the tiles tile the sample
/// after the index, and every node carries its own index so a
/// misparse is caught rather than decoded as noise.
pub fn parse_tiles(header: &ImageHeader, sample: &[u8]) -> Result<Vec<Tile>> {
    if header.mdat_header_size > sample.len() {
        return Err(Error::Corrupt(format!(
            "CRX index of {} bytes in a {}-byte sample",
            header.mdat_header_size,
            sample.len()
        )));
    }
    let v2 = header.version == 0x0200;
    let (tile_tag, plane_tag, band_tag) = if v2 {
        (0xff11u16, 0xff12u16, 0xff13u16)
    } else {
        (0xff01, 0xff02, 0xff03)
    };
    let (cols, rows) = header.tile_grid();
    let (pw, ph) = header.plane_size();
    let (tw, th) = header.tile_size();
    let mut tiles: Vec<Tile> = Vec::new();
    // Where the next node of each kind starts: tiles follow the
    // index, components follow the previous component inside their
    // tile, bands the previous band inside their component.
    let mut tile_at = header.mdat_header_size;
    let mut plane_at = 0usize;
    let mut band_at = 0usize;

    for record in records(&sample[..header.mdat_header_size])? {
        if record.tag == tile_tag {
            let size = be32(record.payload, 0)? as usize;
            let index = tiles.len();
            if index >= cols * rows {
                return Err(Error::Corrupt("more CRX tiles than the grid holds".into()));
            }
            if be16(record.payload, 4)? as usize != index {
                return Err(Error::Corrupt("CRX tile out of order".into()));
            }
            // The long form is version 2's; the short one is the same
            // shape as version 1's and carries no quantiser map.
            let (qp_size, extra_size) = match be16(record.payload, 6)? {
                0 => (0, 0),
                0x4000 if record.payload.len() >= 16 => (
                    be32(record.payload, 8)? as usize,
                    be16(record.payload, 12)? as usize,
                ),
                other => {
                    return Err(Error::Corrupt(format!(
                        "CRX tile record flags {other:#06x}"
                    )))
                }
            };
            let (col, row) = (index % cols, index / cols);
            let end = tile_at
                .checked_add(size)
                .filter(|end| *end <= sample.len())
                .ok_or_else(|| Error::Corrupt("CRX tile runs past the sample".into()))?;
            let mut neighbours = 0;
            if cols > 1 {
                neighbours |= if col + 1 < cols { RIGHT } else { 0 };
                neighbours |= if col > 0 { LEFT } else { 0 };
            }
            if rows > 1 {
                neighbours |= if row + 1 < rows { BELOW } else { 0 };
                neighbours |= if row > 0 { ABOVE } else { 0 };
            }
            let (x, y) = (col * tw, row * th);
            tiles.push(Tile {
                data: tile_at..end,
                col,
                row,
                x,
                y,
                width: tw.min(pw - x),
                height: th.min(ph - y),
                neighbours,
                qp_size,
                extra_size,
                planes: Vec::new(),
            });
            // The quantiser map and the unexplained extra bytes sit
            // at the front of the tile, so the first component starts
            // after both.
            plane_at = tile_at
                .checked_add(qp_size)
                .and_then(|at| at.checked_add(extra_size))
                .filter(|at| *at <= end)
                .ok_or_else(|| Error::Corrupt("CRX quantiser map runs past its tile".into()))?;
            tile_at = end;
        } else if record.tag == plane_tag {
            let size = be32(record.payload, 0)? as usize;
            let packed = *record
                .payload
                .get(4)
                .ok_or_else(|| Error::Corrupt("short CRX component record".into()))?;
            let tile = tiles
                .last_mut()
                .ok_or_else(|| Error::Corrupt("CRX component before any tile".into()))?;
            let index = tile.planes.len();
            if (packed >> 4) as usize != index {
                return Err(Error::Corrupt("CRX component out of order".into()));
            }
            let end = plane_at
                .checked_add(size)
                .filter(|end| *end <= tile.data.end)
                .ok_or_else(|| Error::Corrupt("CRX component runs past its tile".into()))?;
            let ref_prev_line = packed & 8 != 0;
            let round_bits = ((packed >> 1) & 3) as u32;
            if round_bits != 0 && !(header.levels == 0 && ref_prev_line) {
                return Err(Error::Corrupt(
                    "CRX rounded component with a wavelet".into(),
                ));
            }
            tile.planes.push(Plane {
                data: plane_at..end,
                index,
                ref_prev_line,
                round_mask: if round_bits == 0 {
                    0
                } else {
                    1 << (round_bits - 1)
                },
                round_bits,
                bands: Vec::new(),
            });
            band_at = plane_at;
            plane_at = end;
        } else if record.tag == band_tag {
            let size = be32(record.payload, 0)? as usize;
            let plane = tiles
                .last_mut()
                .and_then(|t| t.planes.last_mut())
                .ok_or_else(|| Error::Corrupt("CRX band before any component".into()))?;
            let index = plane.bands.len();
            let end = band_at
                .checked_add(size)
                .filter(|end| *end <= plane.data.end)
                .ok_or_else(|| Error::Corrupt("CRX band runs past its component".into()))?;
            let mut band = Band {
                data: band_at..end,
                trailing: 0,
                index,
                q_param: 4,
                q_per_line: false,
                q_step_base: 1,
                q_step_mult: 0,
            };
            if v2 && record.payload.len() >= 16 {
                if (be16(record.payload, 4)? >> 12) as usize != index {
                    return Err(Error::Corrupt("CRX band out of order".into()));
                }
                band.q_step_mult = be16(record.payload, 6)? as i32;
                band.q_step_base = be32(record.payload, 8)? as i32;
                band.trailing = be16(record.payload, 12)? as usize;
            } else {
                // One packed word: the band counter, a flag, an
                // eight-bit exponent and the count of trailing bytes.
                let word = be32(record.payload, 4)?;
                if (word >> 28) as usize != index {
                    return Err(Error::Corrupt("CRX band out of order".into()));
                }
                band.q_per_line = word & (1 << 27) != 0;
                band.q_param = ((word >> 19) & 0xff) as i32;
                band.trailing = (word & 0x7ffff) as usize;
            }
            if band.trailing > size {
                return Err(Error::Corrupt("CRX band is all trailing bytes".into()));
            }
            plane.bands.push(band);
            band_at = end;
        }
        // Anything else in the index is not a node of the tree; the
        // parser ignores it rather than failing, so a body that adds
        // a record still decodes.
    }

    if tiles.len() != cols * rows {
        return Err(Error::Corrupt(format!(
            "CRX index has {} tiles, the grid wants {}",
            tiles.len(),
            cols * rows
        )));
    }
    for tile in &tiles {
        if tile.planes.len() != header.planes {
            return Err(Error::Corrupt(format!(
                "CRX tile has {} components, CMP1 says {}",
                tile.planes.len(),
                header.planes
            )));
        }
        for plane in &tile.planes {
            if plane.bands.len() != header.bands() {
                return Err(Error::Corrupt(format!(
                    "CRX component has {} bands, {} levels wants {}",
                    plane.bands.len(),
                    header.levels,
                    header.bands()
                )));
            }
        }
    }
    Ok(tiles)
}

/// How many extra coefficients a band carries on a side where the
/// tile has a neighbour, addressed as `EXTRA[levels - 1][dim & 7]`
/// and read in pairs, one pair per level from the finest.
///
/// This is an empirical property of the encoder: it is how many
/// overlap coefficients get emitted so that the lifting reconstructs
/// continuously across a tile seam, for each combination of
/// decomposition depth and tile-dimension residue. There is no
/// derivation for it here; it is checked against three tile
/// geometries and both seam directions in the tests.
const EXTRA: [[[usize; 6]; 8]; 3] = [
    [
        [1, 1, 0, 0, 0, 0],
        [1, 0, 0, 0, 0, 0],
        [1, 1, 0, 0, 0, 0],
        [1, 0, 0, 0, 0, 0],
        [1, 1, 0, 0, 0, 0],
        [1, 0, 0, 0, 0, 0],
        [1, 1, 0, 0, 0, 0],
        [1, 0, 0, 0, 0, 0],
    ],
    [
        [1, 1, 1, 1, 0, 0],
        [1, 0, 1, 0, 0, 0],
        [1, 2, 2, 1, 0, 0],
        [1, 1, 1, 1, 0, 0],
        [1, 1, 1, 1, 0, 0],
        [1, 0, 1, 0, 0, 0],
        [1, 2, 2, 1, 0, 0],
        [1, 1, 1, 1, 0, 0],
    ],
    [
        [1, 1, 1, 1, 1, 1],
        [1, 0, 1, 0, 1, 0],
        [1, 2, 2, 2, 2, 1],
        [1, 1, 1, 2, 2, 1],
        [1, 1, 1, 2, 2, 1],
        [1, 0, 1, 1, 1, 1],
        [1, 2, 2, 1, 1, 1],
        [1, 1, 1, 1, 1, 1],
    ],
];

/// One subband's shape inside a tile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Shape {
    pub width: usize,
    pub height: usize,
    /// The overlap coefficients at each edge. Only the version-2
    /// quantiser map cares where they are; the wavelet gets them from
    /// the widths.
    pub row_start: usize,
    pub row_end: usize,
    pub col_start: usize,
    pub col_end: usize,
    /// How far a band column shifts right to land on the quantiser
    /// map's column, which is pre-downsampled vertically per level
    /// but not horizontally.
    pub level_shift: u32,
    /// Level group, 0 = coarsest. The LL band shares the coarsest.
    pub group: usize,
}

/// The shape of every band of one component of `tile`.
///
/// Bands are indexed LL first, then each level group from coarsest to
/// finest as HL, LH, HH. The dyadic split walks the other way — from
/// the finest, halving the tile each time — and the extra coefficient
/// counts of [`EXTRA`] widen a band on each side the tile has a
/// neighbour.
pub fn band_shapes(levels: usize, tile_w: usize, tile_h: usize, neighbours: u8) -> Vec<Shape> {
    let mut out = vec![Shape::default(); 3 * levels + 1];
    if levels == 0 {
        out[0] = Shape {
            width: tile_w,
            height: tile_h,
            ..Shape::default()
        };
        return out;
    }
    let across = EXTRA[levels - 1][tile_w & 7];
    let down = EXTRA[levels - 1][tile_h & 7];
    let (mut bw, mut bh) = (tile_w, tile_h);
    for level in 0..levels {
        let (odd_w, odd_h) = (bw & 1, bh & 1);
        bw = (bw + odd_w) >> 1;
        bh = (bh + odd_h) >> 1;
        let (mut ex_w0, mut ex_w1, mut ex_h0, mut ex_h1) = (0usize, 0usize, 0usize, 0usize);
        let (mut col_start, mut row_start) = (0usize, 0usize);
        if neighbours & RIGHT != 0 {
            ex_w0 = across[2 * level];
            ex_w1 = across[2 * level + 1];
        }
        if neighbours & LEFT != 0 {
            ex_w0 += 1;
            col_start = 1;
        }
        if neighbours & BELOW != 0 {
            ex_h0 = down[2 * level];
            ex_h1 = down[2 * level + 1];
        }
        if neighbours & ABOVE != 0 {
            ex_h0 += 1;
            row_start = 1;
        }
        let hh = 3 * (levels - level);
        let common = Shape {
            level_shift: (3 - (level + 1)) as u32,
            group: levels - 1 - level,
            ..Shape::default()
        };
        out[hh] = Shape {
            width: bw + ex_w0 - odd_w,
            height: bh + ex_h0 - odd_h,
            row_start,
            row_end: ex_h0 - row_start,
            col_start,
            col_end: ex_w0 - col_start,
            ..common
        };
        out[hh - 1] = Shape {
            width: bw + ex_w1,
            height: bh + ex_h0 - odd_h,
            row_start,
            row_end: ex_h0 - row_start,
            col_start: 0,
            col_end: ex_w1,
            ..common
        };
        out[hh - 2] = Shape {
            width: bw + ex_w0 - odd_w,
            height: bh + ex_h1,
            row_start: 0,
            row_end: ex_h1,
            col_start,
            col_end: ex_w0 - col_start,
            ..common
        };
    }
    let right = if neighbours & RIGHT != 0 {
        across[2 * levels - 1]
    } else {
        0
    };
    let below = if neighbours & BELOW != 0 {
        down[2 * levels - 1]
    } else {
        0
    };
    out[0] = Shape {
        width: bw + right,
        height: bh + below,
        row_start: 0,
        row_end: below,
        col_start: 0,
        col_end: right,
        level_shift: (3 - levels) as u32,
        group: 0,
    };
    out
}

/// The bit reader: most-significant bit first, straight down a band's
/// bytes, with no stuffing, no padding between symbols and no restart
/// markers. Each band's reader starts at bit 0 of its own range.
struct Reader<'a> {
    pump: BitPumpMsb<'a>,
    /// Bits in the band's range. The pump itself reads zeros for ever
    /// past the end, so this is what tells us a decode went wrong.
    limit: usize,
    bad: bool,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Reader<'a> {
        Reader {
            pump: BitPumpMsb::new(bytes),
            limit: bytes.len() * 8,
            bad: false,
        }
    }

    #[inline(always)]
    fn get(&mut self, n: u32) -> u32 {
        if n == 0 {
            0
        } else {
            self.pump.get(n)
        }
    }

    #[inline(always)]
    fn bit(&mut self) -> u32 {
        self.pump.get(1)
    }

    /// Count zeros up to the next 1 bit, consuming it.
    #[inline(always)]
    fn zeros(&mut self) -> u32 {
        let mut count = 0;
        loop {
            let word = self.pump.peek(24);
            if word != 0 {
                let n = word.leading_zeros() - 8;
                self.pump.consume(n + 1);
                return count + n;
            }
            self.pump.consume(24);
            count += 24;
            // Past the end of a truncated or hostile band the pump
            // feeds zeros for ever; stop rather than count them.
            if count >= 256 {
                self.bad = true;
                return ESCAPE_ZEROS;
            }
        }
    }

    /// A Golomb-Rice magnitude with parameter `k`. A unary prefix of
    /// `escape` zeros or more means the magnitude follows as a flat
    /// `wide`-bit field instead.
    ///
    /// The threshold is a common off-by-one trap: in the first symbol
    /// of a line-0 bitstream the escape is preceded by a run-mode
    /// flag bit that is also 0, so a naive reading sees 42 zeros
    /// before the 1. Counting from there appears to work on line 0
    /// and desynchronises later.
    #[inline(always)]
    fn golomb(&mut self, k: u32, escape: u32, wide: u32) -> u32 {
        let z = self.zeros();
        if z >= escape {
            self.get(wide)
        } else if k > 0 {
            (z << k) | self.get(k)
        } else {
            z
        }
    }

    /// Whether the band's bitstream was read to its end and no
    /// further: the encoder pads the last byte, so anything more than
    /// seven bits either way means the decode went off the rails.
    fn consumed_exactly(&self) -> bool {
        !self.bad && self.pump.position() <= self.limit && self.limit - self.pump.position() < 8
    }
}

/// The unary prefix length at which a coefficient magnitude escapes
/// to a flat field, and the width of that field.
const ESCAPE_ZEROS: u32 = 41;
const ESCAPE_BITS: u32 = 21;
/// The same two for both quantiser coders, which are otherwise the
/// same code. These are format constants, not derivable.
const Q_ESCAPE_ZEROS: u32 = 23;
const Q_ESCAPE_BITS: u32 = 8;

/// Fold a magnitude back to a signed residual: `m = 2d` for `d >= 0`
/// and `m = -2d - 1` for `d < 0`.
#[inline(always)]
fn fold(m: u32) -> i32 {
    ((m >> 1) as i32) ^ -((m & 1) as i32)
}

/// The adaptive Golomb parameter.
///
/// `u` is the magnitude just decoded, sometimes blended with a
/// neighbour gradient. `K` rises by at most two per symbol and falls
/// by at most one, and can never go below zero because the test that
/// lowers it compares against `2^(K-1)`, which is 0 at `K = 0`.
/// `kmax` of 0 means no clamp — the detail-band coder applies its own
/// instead.
#[inline(always)]
fn adapt(k: u32, u: u32, kmax: u32) -> u32 {
    let mut next = k as i32;
    if u < ((1u32 << k) >> 1) {
        next -= 1;
    }
    if (u >> k) > 2 {
        next += 1;
    }
    if (u >> k) > 5 {
        next += 1;
    }
    let mut next = next.max(0) as u32;
    if kmax != 0 && next >= kmax {
        next = kmax;
    }
    // The rule bounds K by the magnitudes it sees, so this only
    // matters for a hostile stream: it keeps every shift in range.
    next.min(30)
}

/// Canon's four-way predictor: numerically the JPEG-LS gradient median, written as Canon's selector but a
/// selector between the left neighbour, the one above, and the left
/// neighbour carried along the gradient of the line above.
///
/// It is the single most common reason a partly-working decoder
/// diverges a few dozen samples into line 1.
#[inline(always)]
fn predict(a: i32, b: i32, c: i32) -> i32 {
    let dh = b.wrapping_sub(c);
    let negative = dh < 0;
    let x = (c < a) != negative;
    let y = (a < b) != negative;
    match (x, y) {
        (true, true) => b,
        (true, false) => a,
        _ => a.wrapping_add(dh),
    }
}

/// How much a continuation bit adds to a run at each run state, and
/// how wide the remainder field that ends the run is. `RUNLEN[s]` is
/// always `1 << RUNBITS[s]`.
const RUNLEN: [usize; 32] = [
    1, 1, 1, 1, 2, 2, 2, 2, 4, 4, 4, 4, 8, 8, 8, 8, 16, 16, 32, 32, 64, 64, 128, 128, 256, 512,
    1024, 2048, 4096, 8192, 16384, 32768,
];
const RUNBITS: [u32; 32] = [
    0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 9, 10, 11, 12, 13,
    14, 15,
];

/// Which line coder a subband uses (section 9 of the format notes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// The LL band's predictive coder. `round` is 0 in every file
    /// seen; when it is not, reconstruction is scaled and the run
    /// tests compare against it instead of against zero, which makes
    /// a wavelet-free frame lossy in the spatial domain.
    A { round: i32, round_bits: u32 },
    /// The detail-band coder.
    B,
}

/// What the tests want to see of a band's inner workings. Off unless
/// a test turns it on, so decoding a real frame does not accumulate
/// millions of entries.
#[cfg(test)]
#[derive(Debug, Default)]
struct Trace {
    on: bool,
    /// `K` after each symbol.
    k: Vec<u32>,
    /// Every run, as (line, column, length, state before, after).
    runs: Vec<(usize, usize, usize, usize, usize)>,
}

/// One subband's line coder: the bit reader, two padded line buffers
/// and, for the detail bands, a per-column memory of `K`.
///
/// `K` and the run state carry across line boundaries within a band;
/// nothing carries between bands.
struct Coder<'a> {
    r: Reader<'a>,
    width: usize,
    /// `width + 2` entries: one padding element on each side, so the
    /// left, above-left and above-right neighbours are defined at the
    /// edges. Sample `i` lives at index `i + 1`.
    cur: Vec<i32>,
    prev: Vec<i32>,
    kcol: Vec<u32>,
    k: u32,
    /// The run state, 0..31.
    s: usize,
    line: usize,
    #[cfg(test)]
    trace: Trace,
}

impl<'a> Coder<'a> {
    fn new(bytes: &'a [u8], width: usize) -> Coder<'a> {
        Coder {
            r: Reader::new(bytes),
            width,
            cur: vec![0; width + 2],
            prev: vec![0; width + 2],
            kcol: vec![0; width],
            k: 0,
            s: 0,
            line: 0,
            #[cfg(test)]
            trace: Trace::default(),
        }
    }

    #[inline(always)]
    fn symbol(&mut self) -> u32 {
        self.r.golomb(self.k, ESCAPE_ZEROS, ESCAPE_BITS)
    }

    #[cfg(test)]
    fn note_k(&mut self) {
        if self.trace.on {
            self.trace.k.push(self.k);
        }
    }

    #[cfg(not(test))]
    #[inline(always)]
    fn note_k(&mut self) {}

    #[cfg(test)]
    fn note_run(&mut self, column: usize, run: usize, before: usize) {
        if self.trace.on {
            self.trace
                .runs
                .push((self.line, column, run, before, self.s));
        }
    }

    #[cfg(not(test))]
    #[inline(always)]
    fn note_run(&mut self, _column: usize, _run: usize, _before: usize) {}

    /// Read a run length, given how many columns are still available.
    ///
    /// One bit says whether a run is present at all; then each further
    /// 1 bit adds the current bucket size and grows the run state, and
    /// the first 0 bit ends that phase and is followed by a remainder
    /// field, after which the state shrinks by one. If the growing
    /// phase reaches or overshoots the limit there is no remainder and
    /// the state does not shrink.
    ///
    /// A zero-length run — one bit, spent to say "no run here" — is
    /// normal and frequent.
    fn run(&mut self, limit: usize, at_last_column: bool) -> usize {
        if self.r.bit() == 0 {
            return 0;
        }
        let mut run = 1usize;
        // At the very last column of a detail-band line there is
        // nothing to grow into, so the exponential phase is skipped
        // rather than read and thrown away.
        if !at_last_column {
            while self.r.bit() == 1 {
                run += RUNLEN[self.s];
                if run > limit {
                    return limit;
                }
                if self.s < 31 {
                    self.s += 1;
                }
                if run == limit {
                    return run;
                }
            }
        }
        if run < limit {
            run += self.r.get(RUNBITS[self.s]) as usize;
            if self.s > 0 {
                self.s -= 1;
            }
            if run > limit {
                // A conforming encoder cannot write this.
                self.r.bad = true;
                run = limit;
            }
        }
        run
    }

    /// Mode A, line 0: predict from the left, with 0 standing in for
    /// the sample before column 0. Because sensor values are far from
    /// zero the run test can only fire at column 0, so a real line 0
    /// holds exactly one run-mode flag bit — the first bit of the
    /// band, and it is 0.
    fn mode_a_line0(&mut self, round: i32) {
        let w = self.width;
        self.cur[0] = 0;
        let mut prev = 0i32;
        let mut i = 0usize;
        while i + 1 < w {
            if prev.abs() > round {
                // The prediction is the left neighbour.
            } else {
                let before = self.s;
                let run = self.run(w - i, false);
                self.note_run(i, run, before);
                for _ in 0..run {
                    self.cur[i + 1] = prev;
                    i += 1;
                }
                if i >= w {
                    break;
                }
            }
            let m = self.symbol();
            self.cur[i + 1] = prev.wrapping_add(reconstruct(m, round));
            self.k = adapt(self.k, m, 15);
            self.note_k();
            prev = self.cur[i + 1];
            i += 1;
        }
        if i + 1 == w {
            let m = self.symbol();
            self.cur[i + 1] = prev.wrapping_add(reconstruct(m, round));
            self.k = adapt(self.k, m, 15);
            self.note_k();
        }
        // One past the last sample the buffer holds last + 1, which
        // guarantees the next line sees a different above-right than
        // above at the right edge and so cannot start a run there.
        self.cur[w + 1] = self.cur[w].wrapping_add(1);
    }

    /// Mode A, lines 1 and later: Canon's four-way gradient predictor,
    /// with a run whenever the left, above and above-right neighbours
    /// agree. The symbol decoded straight after a run predicts from
    /// above rather than from the gradient.
    fn mode_a_line(&mut self, round: i32, round_bits: u32) {
        let w = self.width;
        // The element left of column 0 of this line is the sample
        // above column 0; the one left of column 0 of the previous
        // line is whatever that buffer's padding holds, which is 0 on
        // line 1 and the sample from two lines up after that.
        self.cur[0] = self.prev[1];
        if round != 0 {
            self.prev[0] = self.prev[1];
        }
        let mut i = 0usize;
        // Only the rounded mode uses this; it makes a symbol that
        // followed a large above-right gradient sticky for one column.
        let mut reached = false;
        while i + 1 < w {
            let flat = if round == 0 {
                let (a, b, e) = (self.cur[i], self.prev[i + 1], self.prev[i + 2]);
                a == b && a == e
            } else {
                let (a, b, c, e) = (
                    self.cur[i],
                    self.prev[i + 1],
                    self.prev[i],
                    self.prev[i + 2],
                );
                if (e - b).abs() > round {
                    reached = true;
                    false
                } else if reached || (c - a).abs() > round {
                    reached = false;
                    false
                } else {
                    true
                }
            };
            if flat {
                let before = self.s;
                let run = self.run(w - i, false);
                self.note_run(i, run, before);
                let a = self.cur[i];
                for _ in 0..run {
                    self.cur[i + 1] = a;
                    i += 1;
                }
                if i >= w {
                    break;
                }
            }
            let (a, b, c, e) = (
                self.cur[i],
                self.prev[i + 1],
                self.prev[i],
                self.prev[i + 2],
            );
            // After a run the prediction is flat — the sample above —
            // and not the gradient. Getting that wrong shows up as a
            // wrong prediction and a wrong K at the same column.
            let prediction = if flat { b } else { predict(a, b, c) };
            let m = self.symbol();
            self.cur[i + 1] = prediction.wrapping_add(reconstruct(m, round));
            let last = i + 1 == w;
            let u = if last {
                m
            } else if round == 0 {
                blend(m, e.wrapping_sub(b).unsigned_abs())
            } else {
                blend(m, scaled_gradient(b, e, round, round_bits).unsigned_abs())
            };
            self.k = adapt(self.k, u, 15);
            self.note_k();
            if round != 0 && flat && !last {
                reached = (self.prev[i + 3] - self.prev[i + 2]).abs() > round;
            }
            i += 1;
        }
        if i + 1 == w {
            let (a, b, c) = (self.cur[i], self.prev[i + 1], self.prev[i]);
            let m = self.symbol();
            self.cur[i + 1] = predict(a, b, c).wrapping_add(reconstruct(m, round));
            self.k = adapt(self.k, m, 15);
            self.note_k();
        }
        self.cur[w + 1] = self.cur[w].wrapping_add(1);
    }

    /// Mode B, line 0. Detail-band coefficients cluster hard around
    /// zero, so this coder predicts nothing — the sample *is* the
    /// residual — tests for runs against a zero neighbourhood, and
    /// keeps a per-column memory of `K`. A symbol that follows a run
    /// folds `m + 1`, which excludes the zero a run would have
    /// covered from the alphabet.
    fn mode_b_line0(&mut self) {
        let w = self.width;
        self.cur[0] = 0;
        let mut i = 0usize;
        while i + 1 < w {
            let mut biased = false;
            if self.cur[i] == 0 {
                let before = self.s;
                let run = self.run(w - i, false);
                self.note_run(i, run, before);
                for _ in 0..run {
                    self.cur[i + 1] = 0;
                    self.kcol[i] = 0;
                    i += 1;
                }
                if i >= w {
                    break;
                }
                biased = true;
            }
            let m = self.symbol();
            self.cur[i + 1] = fold(if biased { m + 1 } else { m });
            self.k = adapt(self.k, m, 15);
            self.note_k();
            self.kcol[i] = self.k;
            i += 1;
        }
        if i + 1 == w {
            let m = self.symbol();
            self.cur[i + 1] = fold(m);
            self.k = adapt(self.k, m, 15);
            self.note_k();
            self.kcol[i] = self.k;
        }
        // Unlike Mode A this mode pads with zero, so the next line's
        // run test at the right edge sees the zeros it expects.
        self.cur[w + 1] = 0;
    }

    /// Mode B, lines 1 and later. `Kcol` is how the line above steers
    /// this line's parameter: `Kcol[i + 1]` is still the previous
    /// line's value one column to the right, because this line only
    /// overwrites `Kcol[i]` after reading it.
    fn mode_b_line(&mut self) {
        let w = self.width;
        let mut i = 0usize;
        while i + 1 < w {
            let (a, b, e) = (self.cur[i], self.prev[i + 1], self.prev[i + 2]);
            if a == 0 && b == 0 && e == 0 {
                let before = self.s;
                let run = self.run(w - i, i + 1 == w);
                self.note_run(i, run, before);
                for _ in 0..run {
                    self.cur[i + 1] = 0;
                    self.kcol[i] = 0;
                    i += 1;
                }
                if i + 1 >= w {
                    if i + 1 == w {
                        let m = self.symbol();
                        self.cur[i + 1] = fold(m + 1);
                        self.k = adapt(self.k, m, 15);
                        self.note_k();
                        self.kcol[i] = self.k;
                    }
                    // The run ran to the end of the line; there is no
                    // trailing symbol.
                    return;
                }
                let m = self.symbol();
                self.cur[i + 1] = fold(m + 1);
                self.k = adapt(self.k, m, 0);
                self.steer(i);
            } else {
                let m = self.symbol();
                self.cur[i + 1] = fold(m);
                self.k = adapt(self.k, m, 0);
                self.steer(i);
            }
            self.note_k();
            self.kcol[i] = self.k;
            i += 1;
        }
        if i + 1 == w {
            let m = self.symbol();
            self.cur[i + 1] = fold(m);
            self.k = adapt(self.k, m, 15);
            self.note_k();
            self.kcol[i] = self.k;
        }
    }

    /// The pull the line above has on `K`, applied where the adapt
    /// step was left unclamped.
    #[inline(always)]
    fn steer(&mut self, i: usize) {
        if self.kcol[i + 1] as i32 - self.k as i32 <= 1 {
            self.k = self.k.min(15);
        } else {
            self.k += 1;
        }
    }
}

/// Reconstruct a sample's contribution from a magnitude. Without
/// spatial rounding this is just the folded residual; with it the
/// residual is scaled, which is what makes that mode lossy.
#[inline(always)]
fn reconstruct(m: u32, round: i32) -> i32 {
    let d = fold(m);
    if round == 0 {
        d
    } else {
        (2 * round).wrapping_mul(d) + if d < 0 { -1 } else { 0 }
    }
}

/// The above-right gradient the rounded mode feeds to `K`, scaled by
/// the rounding it applies to samples.
#[inline(always)]
fn scaled_gradient(b: i32, e: i32, round: i32, round_bits: u32) -> i32 {
    if e > b {
        (e - b + round - 1) >> round_bits
    } else {
        -((b - e + round) >> round_bits)
    }
}

/// The six steps a quantiser exponent cycles through; the exponent's
/// sixth is the shift. `q = 4` gives 64 >> 6 = 1, unity, which is why
/// every lossless band and every coarse cRAW band carries 4.
const QSTEP: [i32; 6] = [0x28, 0x2d, 0x33, 0x39, 0x40, 0x48];

/// An empirical ceiling on a quantiser step. Version 2 clamps to it
/// explicitly; it also stands in here for the saturating branch of
/// the exponent mapping, which no real file takes.
const QSTEP_MAX: i32 = 0x168000;

/// Map a quantiser exponent to its step.
fn step_of(q: i32) -> i32 {
    let q = q.clamp(0, 0xff);
    let (sixths, rest) = (q / 6, (q % 6) as usize);
    if sixths >= 6 {
        QSTEP_MAX
    } else {
        QSTEP[rest] >> (6 - sixths)
    }
}

/// Where a band's coefficients get their scale from.
enum Quant<'a> {
    /// One step for the whole band (both versions' common case, and
    /// unity for everything lossless).
    Uniform(i32),
    /// Version 1 with the per-line update flag: a delta to the
    /// exponent precedes every line, in the band's own bitstream.
    /// No band in the corpus sets the flag, but it has to be honoured
    /// because ignoring it would consume the wrong bits.
    PerLine { exponent: i32, k: u32 },
    /// Version 2: the tile's quantiser map, scaled by the band.
    Map {
        rows: &'a [Vec<i32>],
        shape: Shape,
        base: i32,
        mult: i32,
    },
}

impl Quant<'_> {
    /// Called before each line is decoded, because that is where the
    /// version-1 delta sits in the bitstream. Returns the step when
    /// the whole line shares one, otherwise fills `scratch` with a
    /// step per column.
    fn begin_line(
        &mut self,
        r: &mut Reader,
        line: usize,
        scratch: &mut [i32],
    ) -> Result<Option<i32>> {
        match self {
            Quant::Uniform(step) => Ok(Some(*step)),
            Quant::PerLine { exponent, k } => {
                let v = r.golomb(*k, Q_ESCAPE_ZEROS, Q_ESCAPE_BITS);
                *exponent += fold(v);
                *k = adapt(*k, v, 0);
                if *k > 7 {
                    return Err(Error::Corrupt("CRX per-line quantiser ran away".into()));
                }
                Ok(Some(step_of(*exponent)))
            }
            Quant::Map {
                rows,
                shape,
                base,
                mult,
            } => {
                if rows.is_empty() {
                    return Err(Error::Corrupt("CRX band with no quantiser map".into()));
                }
                // The map is one entry per 8x2 block of the tile, so
                // a band line indexes it around whatever overlap
                // coefficients the band carries at its edges.
                let inner = shape.height.saturating_sub(shape.row_end);
                let step_row = if line < shape.row_start {
                    0
                } else if line < inner {
                    line - shape.row_end
                } else {
                    inner.saturating_sub(shape.row_start + 1)
                };
                let row = &rows[step_row.min(rows.len() - 1)];
                let inner_col = shape.width.saturating_sub(shape.col_end);
                let last = inner_col.saturating_sub(shape.col_start + 1) >> shape.level_shift;
                for (i, out) in scratch.iter_mut().enumerate() {
                    let at = if i < shape.col_start {
                        0
                    } else if i < inner_col {
                        (i - shape.col_start) >> shape.level_shift
                    } else {
                        last
                    };
                    let s = row[at.min(row.len() - 1)];
                    // File-controlled base and multiplier: widen so
                    // a hostile record cannot overflow the sum.
                    *out = (i64::from(*base) + ((i64::from(s) * i64::from(*mult)) >> 3))
                        .clamp(1, i64::from(QSTEP_MAX)) as i32;
                }
                Ok(None)
            }
        }
    }
}

/// Decode one subband into `shape.width * shape.height` dequantised
/// coefficients, row-major.
///
/// The quantiser is applied on the way out rather than in the line
/// buffers, because the line coders predict from — and test runs
/// against — the coded values, not the scaled ones.
fn decode_band(bytes: &[u8], shape: Shape, mode: Mode, quant: &mut Quant) -> Result<Vec<i32>> {
    let (w, h) = (shape.width, shape.height);
    if w == 0 || h == 0 {
        return Ok(Vec::new());
    }
    // A band with no bitstream at all is a band of zeros.
    if bytes.is_empty() {
        return Ok(vec![0; crate::frame_samples(w, h, 1)?]);
    }
    let mut coder = Coder::new(bytes, w);
    run_band(&mut coder, h, mode, quant)
}

/// `(m + 2 * g) / 2`, the neighbour-blended update term, in 64 bits:
/// a runaway stream can push the line buffers to values whose
/// gradient doubles past `u32`.
#[inline]
fn blend(m: u32, gradient: u32) -> u32 {
    ((u64::from(m) + 2 * u64::from(gradient)) >> 1).min(u64::from(u32::MAX)) as u32
}

/// The body of [`decode_band`], with the coder handed in so a test
/// can watch what it does.
fn run_band(coder: &mut Coder, h: usize, mode: Mode, quant: &mut Quant) -> Result<Vec<i32>> {
    let w = coder.width;
    let mut out = Vec::with_capacity(crate::frame_samples(w, h, 1)?);
    let mut scratch = vec![0i32; w];
    for line in 0..h {
        coder.line = line;
        let step = quant.begin_line(&mut coder.r, line, &mut scratch)?;
        match mode {
            Mode::A { round, .. } if line == 0 => coder.mode_a_line0(round),
            Mode::A { round, round_bits } => coder.mode_a_line(round, round_bits),
            Mode::B if line == 0 => coder.mode_b_line0(),
            Mode::B => coder.mode_b_line(),
        }
        let line = &coder.cur[1..=w];
        match step {
            Some(1) => out.extend_from_slice(line),
            Some(step) => out.extend(line.iter().map(|c| c.wrapping_mul(step))),
            None => out.extend(
                line.iter()
                    .zip(&scratch)
                    .map(|(c, step)| c.wrapping_mul(*step)),
            ),
        }
        std::mem::swap(&mut coder.cur, &mut coder.prev);
    }
    if coder.r.bad {
        return Err(Error::Corrupt(
            "CRX band ran off the end of its data".into(),
        ));
    }
    if !coder.r.consumed_exactly() {
        // Not fatal on its own, but it means something upstream is
        // wrong: a conforming band is consumed to its last byte.
        log::debug!(
            "crx: a {w}x{h} band consumed {} of {} bits",
            coder.r.pump.position(),
            coder.r.limit
        );
    }
    Ok(out)
}

/// The horizontal half of the inverse 5/3 lifting: interleave a
/// low-pass and a high-pass row back into `out`.
///
/// At an outer image edge the high band is extended symmetrically; at
/// an internal tile seam the neighbouring tile's overlap coefficients
/// (the extra columns of [`band_shapes`]) stand in for the extension,
/// which is why a seam reconstructs continuously instead of showing a
/// narrow vertical band of wrong pixels.
fn lift_row(low: &[i32], high: &[i32], out: &mut [i32], left: bool, right: bool) {
    let wd = out.len();
    if wd == 0 {
        return;
    }
    let l = |i: usize| low.get(i).copied().unwrap_or(0);
    let h = |i: usize| high.get(i).copied().unwrap_or(0);
    if wd == 1 {
        out[0] = l(0);
        return;
    }
    let mut hi = 0usize;
    if left {
        // With a left neighbour the high band starts one early: index
        // 0 is the overlap coefficient, used only here.
        out[0] = l(0).wrapping_sub((h(0).wrapping_add(h(1)).wrapping_add(2)) >> 2);
        hi = 1;
    } else {
        out[0] = l(0).wrapping_sub((h(0).wrapping_add(1)) >> 1);
    }
    let (mut li, mut o, mut i) = (1usize, 0usize, 0usize);
    while i + 3 < wd {
        let delta = l(li).wrapping_sub((h(hi).wrapping_add(h(hi + 1)).wrapping_add(2)) >> 2);
        out[o + 1] = h(hi).wrapping_add((delta.wrapping_add(out[o])) >> 1);
        out[o + 2] = delta;
        li += 1;
        hi += 1;
        o += 2;
        i += 2;
    }
    if right {
        let delta = l(li).wrapping_sub((h(hi).wrapping_add(h(hi + 1)).wrapping_add(2)) >> 2);
        out[o + 1] = h(hi).wrapping_add((delta.wrapping_add(out[o])) >> 1);
        if wd & 1 == 1 {
            out[o + 2] = delta;
        }
    } else if wd & 1 == 1 {
        let delta = l(li).wrapping_sub((h(hi).wrapping_add(1)) >> 1);
        out[o + 1] = h(hi).wrapping_add((delta.wrapping_add(out[o])) >> 1);
        out[o + 2] = delta;
    } else {
        out[o + 1] = out[o].wrapping_add(h(hi));
    }
}

/// The vertical half: the same lifting again, a whole row at a time,
/// over rows that have already been reconstructed horizontally.
///
/// The above/below seam cases mirror the left/right ones of
/// [`lift_row`]; no file in the corpus has a tile above or below
/// another, so they are written from the format's symmetry and are
/// unverified.
#[allow(clippy::too_many_arguments)]
fn lift_columns(
    lows: &[i32],
    highs: &[i32],
    out: &mut [i32],
    width: usize,
    height: usize,
    above: bool,
    below: bool,
) {
    if width == 0 || height == 0 {
        return;
    }
    let n_low = lows.len() / width;
    let n_high = highs.len() / width;
    let low = |m: usize| {
        let m = m.min(n_low.saturating_sub(1));
        &lows[m * width..(m + 1) * width]
    };
    let high = |m: usize| {
        let m = m.min(n_high.saturating_sub(1));
        &highs[m * width..(m + 1) * width]
    };
    if n_low == 0 || n_high == 0 {
        if n_low > 0 {
            out[..width].copy_from_slice(low(0));
        }
        return;
    }
    if height == 1 {
        out[..width].copy_from_slice(low(0));
        return;
    }
    let mut hi = 0usize;
    {
        let (l0, h0) = (low(0), high(0));
        if above {
            let h1 = high(1);
            for c in 0..width {
                out[c] = l0[c].wrapping_sub((h0[c].wrapping_add(h1[c]).wrapping_add(2)) >> 2);
            }
            hi = 1;
        } else {
            for c in 0..width {
                out[c] = l0[c].wrapping_sub((h0[c].wrapping_add(1)) >> 1);
            }
        }
    }
    let (mut li, mut o, mut i) = (1usize, 0usize, 0usize);
    while i + 3 < height {
        let (l, h0, h1) = (low(li), high(hi), high(hi + 1));
        for c in 0..width {
            let delta = l[c].wrapping_sub((h0[c].wrapping_add(h1[c]).wrapping_add(2)) >> 2);
            out[(o + 1) * width + c] =
                h0[c].wrapping_add((delta.wrapping_add(out[o * width + c])) >> 1);
            out[(o + 2) * width + c] = delta;
        }
        li += 1;
        hi += 1;
        o += 2;
        i += 2;
    }
    if below {
        let (l, h0, h1) = (low(li), high(hi), high(hi + 1));
        for c in 0..width {
            let delta = l[c].wrapping_sub((h0[c].wrapping_add(h1[c]).wrapping_add(2)) >> 2);
            out[(o + 1) * width + c] =
                h0[c].wrapping_add((delta.wrapping_add(out[o * width + c])) >> 1);
            if height & 1 == 1 {
                out[(o + 2) * width + c] = delta;
            }
        }
    } else if height & 1 == 1 {
        let (l, h0) = (low(li), high(hi));
        for c in 0..width {
            let delta = l[c].wrapping_sub((h0[c].wrapping_add(1)) >> 1);
            out[(o + 1) * width + c] =
                h0[c].wrapping_add((delta.wrapping_add(out[o * width + c])) >> 1);
            out[(o + 2) * width + c] = delta;
        }
    } else {
        let h0 = high(hi);
        for c in 0..width {
            out[(o + 1) * width + c] = out[o * width + c].wrapping_add(h0[c]);
        }
    }
}

/// One level of the inverse wavelet: combine a low-pass image with
/// the level's HL, LH and HH bands.
#[allow(clippy::too_many_arguments)]
fn inverse_level(
    low: &[i32],
    low_w: usize,
    hl: &[i32],
    hl_w: usize,
    lh: &[i32],
    lh_w: usize,
    hh: &[i32],
    hh_w: usize,
    out_w: usize,
    out_h: usize,
    neighbours: u8,
) -> Vec<i32> {
    let (left, right) = (neighbours & LEFT != 0, neighbours & RIGHT != 0);
    // Horizontally reconstruct the vertically-low rows (the low-pass
    // image with HL) and the vertically-high ones (LH with HH), then
    // lift those against each other down the columns.
    let horizontal = |a: &[i32], aw: usize, b: &[i32], bw: usize| -> Vec<i32> {
        let rows = a.len().checked_div(aw).unwrap_or(0);
        let mut out = vec![0i32; rows * out_w];
        out.par_chunks_mut(out_w).enumerate().for_each(|(m, row)| {
            let hi = if bw == 0 {
                &[][..]
            } else {
                b.get(m * bw..(m + 1) * bw).unwrap_or(&[])
            };
            lift_row(&a[m * aw..(m + 1) * aw], hi, row, left, right);
        });
        out
    };
    let lows = horizontal(low, low_w, hl, hl_w);
    let highs = horizontal(lh, lh_w, hh, hh_w);
    let mut out = vec![0i32; out_w * out_h];
    lift_columns(
        &lows,
        &highs,
        &mut out,
        out_w,
        out_h,
        neighbours & ABOVE != 0,
        neighbours & BELOW != 0,
    );
    out
}

/// Decode one component of one tile to its signed samples, which are
/// `tile.width * tile.height` values centred on zero.
fn decode_component(
    sample: &[u8],
    header: &ImageHeader,
    tile: &Tile,
    plane: &Plane,
    shapes: &[Shape],
    steps: Option<&[Vec<Vec<i32>>]>,
) -> Result<Vec<i32>> {
    let mut bands: Vec<Vec<i32>> = plane
        .bands
        .par_iter()
        .zip(shapes.par_iter())
        .map(|(band, shape)| {
            // Only the LL band can use the predictive coder, and only
            // when the component says so; everything else is a detail
            // band whatever its index.
            let mode = if band.index == 0 && plane.ref_prev_line {
                Mode::A {
                    round: plane.round_mask,
                    round_bits: plane.round_bits,
                }
            } else {
                Mode::B
            };
            let mut quant = match steps {
                Some(steps) if header.levels > 0 => Quant::Map {
                    rows: steps.get(shape.group).map(Vec::as_slice).unwrap_or(&[]),
                    shape: *shape,
                    base: band.q_step_base,
                    mult: band.q_step_mult,
                },
                _ if band.q_per_line => Quant::PerLine {
                    exponent: band.q_param,
                    k: 0,
                },
                _ => Quant::Uniform(step_of(band.q_param)),
            };
            decode_band(band.bitstream(sample), *shape, mode, &mut quant)
        })
        .collect::<Result<Vec<_>>>()?;

    if header.levels == 0 {
        return Ok(std::mem::take(&mut bands[0]));
    }
    let mut low = std::mem::take(&mut bands[0]);
    let mut low_w = shapes[0].width;
    for group in 0..header.levels {
        // Each level's output is the next one's low-pass input; the
        // last is the component itself.
        let (out_w, out_h) = if group + 1 < header.levels {
            (
                shapes[3 * (group + 1) + 2].width,
                shapes[3 * (group + 1) + 1].height,
            )
        } else {
            (tile.width, tile.height)
        };
        let (hl, lh, hh) = (3 * group + 1, 3 * group + 2, 3 * group + 3);
        low = inverse_level(
            &low,
            low_w,
            &bands[hl],
            shapes[hl].width,
            &bands[lh],
            shapes[lh].width,
            &bands[hh],
            shapes[hh].width,
            out_w,
            out_h,
            tile.neighbours,
        );
        low_w = out_w;
        for band in [hl, lh, hh] {
            bands[band] = Vec::new();
        }
    }
    Ok(low)
}

/// Where a component sits in the 2x2 colour-filter cell, as
/// `(column, row)`.
///
/// The four components are always in the semantic order R, G on R's
/// row, G on B's row, B; `cfa_layout` says which corner each of those
/// occupies.
pub fn cfa_position(layout: u8, plane: usize) -> (usize, usize) {
    const CELL: [[(usize, usize); 4]; 4] = [
        [(0, 0), (1, 0), (0, 1), (1, 1)],
        [(1, 0), (0, 0), (1, 1), (0, 1)],
        [(0, 1), (1, 1), (0, 0), (1, 0)],
        [(1, 1), (0, 1), (1, 0), (0, 0)],
    ];
    CELL[(layout & 3) as usize][plane & 3]
}

/// Decode one CRX sample into the full frame, `width * height`
/// samples row-major with the components interleaved back onto the
/// sensor's colour-filter grid.
pub fn decode(header: &ImageHeader, sample: &[u8]) -> Result<Vec<u16>> {
    let tiles = parse_tiles(header, sample)?;
    if header.enc_type != 0 {
        // Encoding 1 is a signed path and 3 a decorrelated-colour one
        // that has to reconstruct all four components together. No
        // sample of either exists to check an implementation against,
        // and the frame this returns cannot carry signed samples.
        return Err(Error::Unsupported(format!(
            "CRX encoding {} (only the Bayer path has ever been seen)",
            header.enc_type
        )));
    }
    // Version 2 puts one quantiser map at the front of each tile,
    // which every band of that tile scales; version 1 carries an
    // exponent per band instead.
    let maps: Vec<Vec<Vec<Vec<i32>>>> = tiles
        .par_iter()
        .map(|tile| {
            if tile.qp_size == 0 || header.levels == 0 {
                return Ok(Vec::new());
            }
            let bytes = sample
                .get(tile.data.start..tile.data.start + tile.qp_size)
                .ok_or_else(|| Error::Corrupt("CRX quantiser map outside the sample".into()))?;
            quantiser_map(bytes, tile.width, tile.height, header.levels)
        })
        .collect::<Result<Vec<_>>>()?;

    let median = 1i32 << (header.bits - 1);
    let white = (1i32 << header.bits) - 1;
    let mut work: Vec<(usize, usize)> = Vec::new();
    for (t, tile) in tiles.iter().enumerate() {
        for p in 0..tile.planes.len() {
            work.push((t, p));
        }
    }
    // Tiles and components are wholly independent of each other, and
    // so are the bands inside them; only the wavelet joins a
    // component's bands back up.
    let planes: Vec<(usize, usize, Vec<u16>)> = work
        .par_iter()
        .map(|(t, p)| {
            let tile = &tiles[*t];
            let shapes = band_shapes(header.levels, tile.width, tile.height, tile.neighbours);
            let steps = if maps[*t].is_empty() {
                None
            } else {
                Some(maps[*t].as_slice())
            };
            let data = decode_component(sample, header, tile, &tile.planes[*p], &shapes, steps)?;
            // The coder's samples are signed and centred on zero; the
            // level shift and the clamp are the last thing that
            // happens to them.
            Ok((
                *t,
                *p,
                data.into_iter()
                    .map(|v| median.wrapping_add(v).clamp(0, white) as u16)
                    .collect(),
            ))
        })
        .collect::<Result<Vec<_>>>()?;

    let (width, height) = (header.width, header.height);
    let cell = if header.planes == 1 { 1 } else { 2 };
    // Each plane scatters its own rows: one pass over the samples,
    // rather than every frame row scanning every tile of every plane
    // (which made a 255x255 tile grid take seconds).
    let mut frame = vec![0u16; crate::frame_samples(width, height, 1)?];
    for (t, p, data) in &planes {
        let tile = &tiles[*t];
        if tile.width == 0 {
            continue;
        }
        let (cx, cy) = if cell == 1 {
            (0, 0)
        } else {
            cfa_position(header.cfa_layout, *p)
        };
        for py in 0..tile.height {
            let fy = cell * (tile.y + py) + cy;
            if fy >= height {
                break;
            }
            let Some(line) = data
                .get(py * tile.width..)
                .and_then(|d| d.get(..tile.width))
            else {
                break;
            };
            let row = &mut frame[fy * width..][..width];
            for (x, value) in line.iter().enumerate() {
                let fx = cell * (tile.x + x) + cx;
                if let Some(out) = row.get_mut(fx) {
                    *out = *value;
                }
            }
        }
    }
    Ok(frame)
}

/// Decode a tile's quantiser map and fold it into one step table per
/// level group, coarsest first.
///
/// The map is one exponent per 8x2 block of the tile, coded with a
/// small predictive Golomb coder of its own. A level's table is the
/// map with its rows combined in fours, in pairs or not at all, which
/// is how it comes to match a band that has been downsampled that far
/// vertically.
fn quantiser_map(
    bytes: &[u8],
    tile_w: usize,
    tile_h: usize,
    levels: usize,
) -> Result<Vec<Vec<Vec<i32>>>> {
    let (qw, qh) = (tile_w.div_ceil(8), tile_h.div_ceil(2));
    let base = quantiser_grid(bytes, qw, qh)?;
    let at = |row: usize, col: usize| base[row.min(qh - 1) * qw + col];
    let mut out = Vec::with_capacity(levels);
    for group in 0..levels {
        let span = 1usize << (levels - 1 - group);
        let rows = tile_h.div_ceil(2 * span);
        let mut table = Vec::with_capacity(rows);
        for r in 0..rows {
            let mut row = Vec::with_capacity(qw);
            for c in 0..qw {
                let value = match span {
                    1 => at(r, c),
                    2 => (at(2 * r, c) + at(2 * r + 1, c)) / 2,
                    // Four rows are averaged with a bias that rounds
                    // toward zero rather than the plain truncating
                    // divide two rows get. Nothing explains the
                    // difference; the entries are positive in
                    // practice, so the bias never fires.
                    _ => {
                        let sum: i32 = (0..span).map(|k| at(span * r + k, c)).sum();
                        (if sum < 0 { 3 } else { 0 } + sum) >> 2
                    }
                };
                row.push(step_of(value));
            }
            table.push(row);
        }
        out.push(table);
    }
    Ok(out)
}

/// The quantiser map's own bitstream: row 0 predicts from the left,
/// later rows use the same four-way gradient predictor as the LL
/// band's line coder, and there is no run mode. Every entry is
/// four larger than it is coded.
fn quantiser_grid(bytes: &[u8], qw: usize, qh: usize) -> Result<Vec<i32>> {
    if qw == 0 || qh == 0 {
        return Err(Error::Corrupt("CRX empty quantiser map".into()));
    }
    let mut r = Reader::new(bytes);
    let mut cur = vec![0i32; qw + 2];
    let mut prev = vec![0i32; qw + 2];
    let mut out = vec![0i32; qw * qh];
    let mut k = 0u32;
    for row in 0..qh {
        if row == 0 {
            let mut left = 0i32;
            for i in 0..qw {
                let v = r.golomb(k, Q_ESCAPE_ZEROS, Q_ESCAPE_BITS);
                left = left.wrapping_add(fold(v));
                cur[i + 1] = left;
                k = adapt(k, v, 7);
            }
        } else {
            cur[0] = prev[1];
            for i in 0..qw {
                let (a, b, c, e) = (cur[i], prev[i + 1], prev[i], prev[i + 2]);
                let v = r.golomb(k, Q_ESCAPE_ZEROS, Q_ESCAPE_BITS);
                cur[i + 1] = predict(a, b, c).wrapping_add(fold(v));
                k = if i + 1 < qw {
                    adapt(k, blend(v, e.wrapping_sub(b).unsigned_abs()), 7)
                } else {
                    adapt(k, v, 7)
                };
            }
        }
        cur[qw + 1] = cur[qw].wrapping_add(1);
        out[row * qw..(row + 1) * qw].copy_from_slice(&cur[1..=qw]);
        std::mem::swap(&mut cur, &mut prev);
    }
    if r.bad {
        return Err(Error::Corrupt("CRX quantiser map ran off its data".into()));
    }
    for v in &mut out {
        *v += 4;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------------------------------------------------------
    // Hand-built streams: the mechanics.
    // ---------------------------------------------------------------

    /// A CMP1 payload shaped like the ones real files carry.
    fn cmp1(version: u16, w: u32, h: u32, tw: u32, th: u32, levels: u8, index: u32) -> Vec<u8> {
        let mut p = vec![0u8; 52];
        p[0..2].copy_from_slice(&if version == 0x0100 { 0xff00u16 } else { 0xff10 }.to_be_bytes());
        p[2..4].copy_from_slice(&0x0030u16.to_be_bytes());
        p[4..6].copy_from_slice(&version.to_be_bytes());
        p[8..12].copy_from_slice(&w.to_be_bytes());
        p[12..16].copy_from_slice(&h.to_be_bytes());
        p[16..20].copy_from_slice(&tw.to_be_bytes());
        p[20..24].copy_from_slice(&th.to_be_bytes());
        p[24] = 14;
        p[25] = 0x40;
        p[26] = levels;
        p[28..32].copy_from_slice(&index.to_be_bytes());
        p
    }

    fn record(tag: u16, payload: &[u8]) -> Vec<u8> {
        let mut out = tag.to_be_bytes().to_vec();
        out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        out.extend_from_slice(payload);
        out
    }

    /// Bits, most significant first, as the coder writes them.
    #[derive(Default)]
    struct Writer {
        bytes: Vec<u8>,
        held: u32,
        n: u32,
    }

    impl Writer {
        fn put(&mut self, value: u32, bits: u32) -> &mut Writer {
            for i in (0..bits).rev() {
                self.held = (self.held << 1) | ((value >> i) & 1);
                self.n += 1;
                if self.n == 8 {
                    self.bytes.push(self.held as u8);
                    self.held = 0;
                    self.n = 0;
                }
            }
            self
        }
        fn zeros(&mut self, n: u32) -> &mut Writer {
            for _ in 0..n {
                self.put(0, 1);
            }
            self
        }
        fn done(&mut self) -> Vec<u8> {
            while self.n != 0 {
                self.put(0, 1);
            }
            std::mem::take(&mut self.bytes)
        }
    }

    #[test]
    fn image_header_fields() {
        // The CMP1 payload of Canon/EOS_R-RAW_ISO_100.CR3, byte for
        // byte, with its 20 zero bytes of tail.
        let mut payload = vec![
            0xff, 0x00, 0x00, 0x30, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x1a, 0xe8, 0x00, 0x00,
            0x11, 0xc2, 0x00, 0x00, 0x0d, 0x74, 0x00, 0x00, 0x11, 0xc2, 0x0e, 0x40, 0x00, 0x00,
            0x00, 0x00, 0x00, 0xd8,
        ];
        payload.extend(std::iter::repeat_n(0u8, 20));
        let header = ImageHeader::parse(&payload).unwrap();
        assert_eq!(header.version, 0x0100);
        assert_eq!((header.width, header.height), (6888, 4546));
        assert_eq!((header.tile_width, header.tile_height), (3444, 4546));
        assert_eq!(header.bits, 14);
        assert_eq!(header.planes, 4);
        assert_eq!(header.cfa_layout, 0);
        assert_eq!(header.enc_type, 0);
        assert_eq!(header.levels, 0);
        assert!(!header.tile_cols_linked && !header.tile_rows_linked);
        assert_eq!(header.mdat_header_size, 216);
        assert_eq!(header.median_bits, 14);
        // A Bayer frame is coded as four half-size planes, and every
        // tile and band shape lives in those coordinates.
        assert_eq!(header.plane_size(), (3444, 2273));
        assert_eq!(header.tile_size(), (1722, 2273));
        assert_eq!(header.tile_grid(), (2, 1));
        assert_eq!(header.bands(), 1);
        // The same frame in cRAW: three levels, ten bands, and the
        // flag that says the wavelet spans the tile columns.
        let mut lossy = cmp1(0x0100, 6888, 4546, 3444, 4546, 3, 1080);
        lossy[27] = 0x80;
        let lossy = ImageHeader::parse(&lossy).unwrap();
        assert_eq!(lossy.levels, 3);
        assert_eq!(lossy.bands(), 10);
        assert!(lossy.tile_cols_linked && !lossy.tile_rows_linked);
    }

    #[test]
    fn a_cmp1_that_breaks_its_own_rules_is_rejected() {
        assert!(ImageHeader::parse(&[0u8; 8]).is_err());
        // An unknown dialect, a zero dimension, a tile bigger than the
        // frame, an odd Bayer frame, a tile too small to transform, a
        // frame too big for the tile records' coordinates.
        assert!(matches!(
            ImageHeader::parse(&cmp1(0x0300, 64, 64, 64, 64, 0, 16)),
            Err(Error::Unsupported(_))
        ));
        assert!(ImageHeader::parse(&cmp1(0x0100, 0, 64, 64, 64, 0, 16)).is_err());
        assert!(ImageHeader::parse(&cmp1(0x0100, 64, 64, 128, 64, 0, 16)).is_err());
        assert!(ImageHeader::parse(&cmp1(0x0100, 65, 64, 65, 64, 0, 16)).is_err());
        assert!(ImageHeader::parse(&cmp1(0x0100, 40, 40, 40, 40, 0, 16)).is_err());
        assert!(ImageHeader::parse(&cmp1(0x0100, 1 << 17, 64, 64, 64, 0, 16)).is_err());
        // A header with no index cannot describe anything.
        assert!(ImageHeader::parse(&cmp1(0x0100, 64, 64, 64, 64, 0, 0)).is_err());
        // Fourteen bits with eight-bit single-plane rules.
        let mut single = cmp1(0x0100, 64, 64, 64, 64, 0, 16);
        single[25] = 0x10;
        assert!(ImageHeader::parse(&single).is_err());
    }

    /// The first 120 bytes of the mdat index of
    /// `Canon/EOS_R-RAW_ISO_100.CR3`, which is tile 0 in full and the
    /// record that opens tile 1.
    fn eos_r_index() -> Vec<u8> {
        let mut index = Vec::new();
        index.extend(record(0xff01, &hex("00d08d20 0000 0000")));
        for (size, plane, band) in [
            (0x0031_8740u32, 0x08u8, 0x0020_0004u32),
            (0x0036_27c0, 0x18, 0x0020_0000),
            (0x0036_2110, 0x28, 0x0020_0005),
            (0x0032_bd10, 0x38, 0x0020_0007),
        ] {
            let mut p = size.to_be_bytes().to_vec();
            p.extend([plane, 0, 0, 0]);
            index.extend(record(0xff02, &p));
            let mut b = size.to_be_bytes().to_vec();
            b.extend(band.to_be_bytes());
            index.extend(record(0xff03, &b));
        }
        index.extend(record(0xff01, &hex("00de2498 0001 0000")));
        index
    }

    fn hex(s: &str) -> Vec<u8> {
        let digits: Vec<u8> = s.bytes().filter(u8::is_ascii_hexdigit).collect();
        digits
            .chunks(2)
            .map(|p| u8::from_str_radix(std::str::from_utf8(p).unwrap(), 16).unwrap())
            .collect()
    }

    #[test]
    fn index_records_of_a_real_lossless_frame() {
        // Tile 1's four components, invented so the tree adds up; the
        // rest of the index is the file's own bytes.
        let mut index = eos_r_index();
        for plane in 0..4u8 {
            let size = 0x00de_2498u32 / 4;
            let mut p = size.to_be_bytes().to_vec();
            p.extend([plane << 4 | 8, 0, 0, 0]);
            index.extend(record(0xff02, &p));
            let mut b = size.to_be_bytes().to_vec();
            b.extend(0x0020_0004u32.to_be_bytes());
            index.extend(record(0xff03, &b));
        }
        assert_eq!(index.len(), 216);
        let header = ImageHeader::parse(&cmp1(0x0100, 6888, 4546, 3444, 4546, 0, 216)).unwrap();
        let mut sample = index.clone();
        sample.extend(std::iter::repeat_n(0u8, 0x00d0_8d20 + 0x00de_2498));
        let tiles = parse_tiles(&header, &sample).unwrap();

        assert_eq!(tiles.len(), 2);
        assert_eq!(tiles[0].data, 216..216 + 0x00d0_8d20);
        assert_eq!((tiles[0].width, tiles[0].height), (1722, 2273));
        assert_eq!((tiles[0].x, tiles[0].y), (0, 0));
        assert_eq!((tiles[1].x, tiles[1].y), (1722, 0));
        // The neighbour flags come from the grid, never from the
        // header's linked bits.
        assert_eq!(tiles[0].neighbours, RIGHT);
        assert_eq!(tiles[1].neighbours, LEFT);
        assert_eq!(tiles[0].qp_size, 0);
        assert_eq!(tiles[0].extra_size, 0);
        let plane = &tiles[0].planes[0];
        assert!(plane.ref_prev_line);
        assert_eq!(plane.round_mask, 0);
        assert_eq!(plane.data, 216..216 + 0x0031_8740);
        let band = &plane.bands[0];
        assert_eq!(band.index, 0);
        assert!(!band.q_per_line);
        assert_eq!(band.q_param, 4);
        assert_eq!(step_of(band.q_param), 1);
        // Four bytes of the band's range are not bitstream, so the
        // coder gets 3245884 of the component's 3245888.
        assert_eq!(band.trailing, 4);
        assert_eq!(band.bitstream(&sample).len(), 3_245_884);
        assert_eq!(tiles[0].planes[1].bands[0].trailing, 0);
        assert_eq!(tiles[0].planes[3].bands[0].trailing, 7);
    }

    #[test]
    fn a_version_2_tile_record_carries_a_quantiser_map() {
        let mut index = Vec::new();
        let mut tile = 400u32.to_be_bytes().to_vec();
        tile.extend([0, 0]);
        tile.extend(0x4000u16.to_be_bytes());
        tile.extend(40u32.to_be_bytes());
        tile.extend(4u16.to_be_bytes());
        tile.extend([0, 0]);
        index.extend(record(0xff11, &tile));
        for plane in 0..4u8 {
            let mut p = 89u32.to_be_bytes().to_vec();
            p.extend([plane << 4 | 8, 0, 0, 0]);
            index.extend(record(0xff12, &p));
            for band in 0..10u16 {
                let mut b = 8u32.to_be_bytes().to_vec();
                b.extend((band << 12).to_be_bytes());
                b.extend(8u16.to_be_bytes());
                b.extend(0u32.to_be_bytes());
                b.extend(1u16.to_be_bytes());
                b.extend([0, 0]);
                index.extend(record(0xff13, &b));
            }
        }
        let start = index.len();
        let header =
            ImageHeader::parse(&cmp1(0x0200, 128, 128, 128, 128, 3, start as u32)).unwrap();
        let mut sample = index;
        sample.extend(std::iter::repeat_n(0u8, 400));
        let tiles = parse_tiles(&header, &sample).unwrap();
        assert_eq!(tiles[0].qp_size, 40);
        assert_eq!(tiles[0].extra_size, 4);
        // The components start after the map and the four bytes whose
        // meaning nobody knows.
        assert_eq!(tiles[0].planes[0].data.start, start + 44);
        let band = &tiles[0].planes[0].bands[3];
        assert_eq!((band.q_step_base, band.q_step_mult), (0, 8));
        assert_eq!(band.trailing, 1);
    }

    #[test]
    fn an_index_that_does_not_add_up_is_rejected() {
        let header = ImageHeader::parse(&cmp1(0x0100, 128, 64, 64, 64, 0, 12)).unwrap();
        let mut short = record(0xff01, &hex("00000008 0000 0000"));
        short.extend([0u8; 8]);
        // One tile record for a two-tile grid.
        assert!(parse_tiles(&header, &short).is_err());
        // A tile bigger than the sample.
        let mut big = record(0xff01, &hex("00100000 0000 0000"));
        big.extend([0u8; 8]);
        assert!(parse_tiles(&header, &big).is_err());
        // A tile out of order.
        let mut misnumbered = record(0xff01, &hex("00000008 0007 0000"));
        misnumbered.extend([0u8; 8]);
        assert!(parse_tiles(&header, &misnumbered).is_err());
        // An index longer than the sample it describes.
        let header = ImageHeader::parse(&cmp1(0x0100, 64, 64, 64, 64, 0, 1 << 20)).unwrap();
        assert!(parse_tiles(&header, &[0u8; 32]).is_err());
    }

    #[test]
    fn band_shapes_of_the_eos_r_craw_tiles() {
        // Tile 0 of Canon_EOS_R_CRAW_ISO_100.CR3 (1722x2273, a
        // neighbour on the right) and tile 1 (the same size, a
        // neighbour on the left).
        let right = band_shapes(3, 1722, 2273, RIGHT);
        let widths: Vec<usize> = right.iter().map(|s| s.width).collect();
        let heights: Vec<usize> = right.iter().map(|s| s.height).collect();
        assert_eq!(widths, [217, 217, 217, 217, 432, 433, 432, 862, 863, 862]);
        assert_eq!(
            heights,
            [285, 285, 284, 284, 569, 568, 568, 1137, 1136, 1136]
        );
        let left = band_shapes(3, 1722, 2273, LEFT);
        let widths: Vec<usize> = left.iter().map(|s| s.width).collect();
        assert_eq!(widths, [216, 216, 216, 216, 431, 431, 431, 862, 861, 862]);
        // Every band of a tile with no vertical neighbour is the same
        // height whichever side the horizontal seam is on.
        assert_eq!(
            left.iter().map(|s| s.height).collect::<Vec<_>>(),
            heights.clone()
        );
        // The level shifts the version-2 quantiser map indexes with.
        assert_eq!(
            right.iter().map(|s| s.level_shift).collect::<Vec<_>>(),
            [0, 0, 0, 0, 1, 1, 1, 2, 2, 2]
        );
        assert_eq!(
            right.iter().map(|s| s.group).collect::<Vec<_>>(),
            [0, 0, 0, 0, 1, 1, 1, 2, 2, 2]
        );
        // A tile with no neighbours at all carries no overlap.
        let alone = band_shapes(3, 1722, 2273, 0);
        assert_eq!(
            alone.iter().map(|s| s.width).collect::<Vec<_>>(),
            [216, 215, 216, 215, 430, 431, 430, 861, 861, 861]
        );
        assert!(alone
            .iter()
            .all(|s| s.col_start + s.col_end + s.row_start + s.row_end == 0));
    }

    #[test]
    fn band_shapes_of_a_second_tile_geometry() {
        // Canon/EOS_90D-cRAW-ISO-100.CR3: 1782x2366 with the right
        // seam, which lands on a different row of the overlap table
        // (1782 & 7 == 6) from the EOS R's.
        let shapes = band_shapes(3, 1782, 2366, RIGHT);
        let got: Vec<(usize, usize)> = shapes.iter().map(|s| (s.width, s.height)).collect();
        assert_eq!(
            got,
            [
                (224, 296),
                (224, 296),
                (224, 296),
                (224, 296),
                (447, 592),
                (447, 591),
                (447, 591),
                (892, 1183),
                (893, 1183),
                (892, 1183),
            ]
        );
        // With no wavelet there is one band and it is the tile.
        let flat = band_shapes(0, 1782, 2366, RIGHT);
        assert_eq!(flat.len(), 1);
        assert_eq!((flat[0].width, flat[0].height), (1782, 2366));
    }

    #[test]
    fn the_escape_needs_forty_one_zeros() {
        // How every lossless band opens: the run-mode flag, then the
        // escape, then a flat 21-bit magnitude. A decoder that counts
        // the flag as part of the unary prefix and escapes at 42 sees
        // exactly the same thing here and desynchronises later.
        let bytes = Writer::default()
            .zeros(1)
            .zeros(41)
            .put(1, 1)
            .put(15367, 21)
            .done();
        assert_eq!(bytes.len(), 8);
        let mut r = Reader::new(&bytes);
        assert_eq!(r.bit(), 0);
        assert_eq!(r.golomb(0, ESCAPE_ZEROS, ESCAPE_BITS), 15367);
        assert_eq!(fold(15367), -7684);
        assert!(r.consumed_exactly());
        // Below the threshold it is an ordinary Rice code.
        let bytes = Writer::default().zeros(3).put(1, 1).put(0b10, 2).done();
        let mut r = Reader::new(&bytes);
        assert_eq!(r.golomb(2, ESCAPE_ZEROS, ESCAPE_BITS), 14);
        // A band of nothing but zero bits must not spin for ever.
        let mut r = Reader::new(&[0u8; 64]);
        assert_eq!(r.golomb(0, ESCAPE_ZEROS, ESCAPE_BITS), 0);
        assert!(r.bad);
    }

    #[test]
    fn k_follows_the_magnitudes_of_a_real_line() {
        // The magnitudes of the first 24 symbols of line 0 of tile 0
        // component 0 of Canon/EOS_R-RAW_ISO_100.CR3, and the K the
        // reference has after each of them.
        let magnitudes = [
            15367u32, 14, 5, 0, 2, 1, 2, 5, 6, 0, 5, 4, 6, 3, 1, 7, 1, 1, 0, 2, 10, 8, 7, 7,
        ];
        let want = [
            2u32, 3, 3, 2, 2, 1, 1, 1, 2, 1, 1, 1, 2, 2, 1, 2, 1, 1, 0, 0, 2, 2, 2, 2,
        ];
        let mut k = 0;
        let got: Vec<u32> = magnitudes
            .iter()
            .map(|m| {
                k = adapt(k, *m, 15);
                k
            })
            .collect();
        assert_eq!(got, want);
        // K rises by at most two and falls by at most one, and never
        // goes below zero.
        assert_eq!(adapt(0, 0, 15), 0);
        assert_eq!(adapt(0, 1 << 20, 15), 2);
        assert_eq!(adapt(4, 0, 15), 3);
        assert_eq!(adapt(14, 1 << 20, 15), 15);
        // Kmax 0 means no clamp; the detail-band coder applies its own.
        assert_eq!(adapt(14, 1 << 20, 0), 16);
    }

    #[test]
    fn the_four_way_predictor_is_the_median_predictor() {
        // Line 1, column 0 of the EOS R's first band: the left
        // neighbour is the padding, which is the sample above, and
        // the above-left is the previous line's padding, still zero.
        assert_eq!(predict(-7684, -7684, 0), -7684);
        // Canon writes this as a two-bit selector over the sign of
        // the gradient above rather than as a median, but the two
        // agree on every input: a decoder that diverges in line 1 is
        // missing run mode, not the predictor.
        let median = |a: i32, b: i32, c: i32| {
            if c >= a.max(b) {
                a.min(b)
            } else if c <= a.min(b) {
                a.max(b)
            } else {
                a + b - c
            }
        };
        for a in -4..=4 {
            for b in -4..=4 {
                for c in -4..=4 {
                    assert_eq!(predict(a, b, c), median(a, b, c), "{a} {b} {c}");
                }
            }
        }
    }

    #[test]
    fn runs_grow_and_shrink_their_state() {
        let bits = |value: u32, n: u32| Writer::default().put(value, n).done();
        // One 0 bit: no run at all, and nothing else is read.
        let none = bits(0, 1);
        // 1, then one continuation: length 2, and the state grows and
        // shrinks straight back.
        let two = bits(0b110, 3);
        // Two continuations leave the state one higher.
        let three = bits(0b1110, 4);
        // At a state with a remainder field the field is read and
        // added: state 4 adds 2 per continuation and has one spare
        // bit, so 1, 0, then a 1 bit of remainder is a run of 2.
        let remainder = bits(0b101, 3);
        // A run that reaches the limit stops there, reads no
        // remainder and leaves the state where the growth put it.
        let capped = bits(0b11, 2);
        let run = |bytes: &[u8], state: usize, limit: usize| {
            let mut coder = Coder::new(bytes, 8);
            coder.s = state;
            let run = coder.run(limit, false);
            (run, coder.s, coder.r.pump.position())
        };
        assert_eq!(run(&none, 0, 1000), (0, 0, 1));
        assert_eq!(run(&two, 0, 1000), (2, 0, 3));
        assert_eq!(run(&three, 0, 1000), (3, 1, 4));
        assert_eq!(run(&remainder, 4, 1000), (2, 3, 3));
        assert_eq!(run(&capped, 0, 2), (2, 1, 2));
        // Every bucket is a power of two wide.
        for s in 0..32 {
            assert_eq!(RUNLEN[s], 1 << RUNBITS[s]);
        }
    }

    #[test]
    fn lifting_interpolates_between_the_low_band() {
        // With a zero high band the reconstruction is the low band
        // interleaved with its own midpoints.
        let mut out = vec![0i32; 4];
        lift_row(&[10, 20], &[0, 0], &mut out, false, false);
        assert_eq!(out, [10, 15, 20, 20]);
        let mut out = vec![0i32; 5];
        lift_row(&[10, 20, 30], &[0, 0, 0], &mut out, false, false);
        assert_eq!(out, [10, 15, 20, 25, 30]);
        // A high band shifts both halves: it is subtracted from the
        // even samples a quarter at a time and added to the odd ones.
        let mut out = vec![0i32; 4];
        lift_row(&[10, 20], &[4, 0], &mut out, false, false);
        assert_eq!(out, [8, 17, 19, 19]);
        // One column is the low sample itself.
        let mut out = vec![0i32; 1];
        lift_row(&[7], &[3], &mut out, false, false);
        assert_eq!(out, [7]);
        // The vertical half does the same thing a row at a time.
        let mut out = vec![0i32; 4 * 2];
        lift_columns(
            &[10, 10, 20, 20],
            &[0, 0, 0, 0],
            &mut out,
            2,
            4,
            false,
            false,
        );
        assert_eq!(out, [10, 10, 15, 15, 20, 20, 20, 20]);
    }

    #[test]
    fn quantiser_exponents_map_to_the_steps_real_files_use() {
        // The exponents seen in version-1 cRAW bands.
        assert_eq!(step_of(4), 1);
        assert_eq!(step_of(16), 4);
        assert_eq!(step_of(22), 8);
        assert_eq!(step_of(26), 12);
        assert_eq!(step_of(32), 25);
        // The saturating branch, which no real file takes.
        assert_eq!(step_of(255), QSTEP_MAX);
    }

    #[test]
    fn hostile_samples_are_errors_not_panics() {
        let mut index = record(0xff01, &hex("00000100 0000 0000"));
        for plane in 0..4u8 {
            let mut p = 64u32.to_be_bytes().to_vec();
            p.extend([plane << 4 | 8, 0, 0, 0]);
            index.extend(record(0xff02, &p));
            let mut b = 64u32.to_be_bytes().to_vec();
            b.extend(0x0020_0000u32.to_be_bytes());
            index.extend(record(0xff03, &b));
        }
        let header =
            ImageHeader::parse(&cmp1(0x0100, 64, 64, 64, 64, 0, index.len() as u32)).unwrap();
        for fill in [0x00u8, 0xff, 0x5a] {
            let mut sample = index.clone();
            sample.extend(std::iter::repeat_n(fill, 0x100));
            // Whatever the bits say, this either decodes to something
            // or reports the file as corrupt; it never panics and
            // never runs away.
            match decode(&header, &sample) {
                Ok(frame) => assert_eq!(frame.len(), 64 * 64),
                Err(Error::Corrupt(_)) => {}
                Err(e) => panic!("{e}"),
            }
        }
        // Truncations of the index itself.
        for cut in 0..index.len() {
            let _ = parse_tiles(&header, &index[..cut]);
        }
    }

    // ---------------------------------------------------------------
    // Corpus: real CRX streams.
    // ---------------------------------------------------------------

    /// The first sample of a track, from `stsz` and `co64`/`stco`.
    fn sample_of<'a>(bytes: &'a [u8], stbl: &crate::bmff::Box_) -> Option<&'a [u8]> {
        let stsz = bytes.get(stbl.child(b"stsz")?.data.clone())?;
        let be = |b: &[u8], at: usize| -> Option<usize> {
            Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?) as usize)
        };
        let size = match be(stsz, 4)? {
            0 => be(stsz, 12)?,
            uniform => uniform,
        };
        let offset = match (stbl.child(b"co64"), stbl.child(b"stco")) {
            (Some(co64), _) => {
                let p = bytes.get(co64.data.clone())?;
                usize::try_from(u64::from_be_bytes(p.get(8..16)?.try_into().ok()?)).ok()?
            }
            (None, Some(stco)) => be(bytes.get(stco.data.clone())?, 8)?,
            (None, None) => return None,
        };
        bytes.get(offset..offset.checked_add(size)?)
    }

    /// Every CRX stream in a CR3: its header and its sample.
    fn streams(bytes: &[u8]) -> Vec<(ImageHeader, &[u8])> {
        let Ok(boxes) = crate::bmff::parse(bytes) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for trak in boxes
            .iter()
            .filter(|b| &b.kind == b"moov")
            .flat_map(|m| m.children.iter())
            .filter(|b| &b.kind == b"trak")
        {
            let Some(stbl) = trak.find_all(b"stbl").into_iter().next() else {
                continue;
            };
            let Some(entry) = stbl.child(b"stsd").and_then(|s| s.children.first()) else {
                continue;
            };
            let Some(cmp1) = entry.child(b"CMP1") else {
                continue;
            };
            let Ok(header) = ImageHeader::parse(&bytes[cmp1.data.clone()]) else {
                continue;
            };
            if let Some(sample) = sample_of(bytes, stbl) {
                out.push((header, sample));
            }
        }
        out
    }

    /// The largest CRX stream in a CR3 — the full-size raw track.
    fn raw_stream(bytes: &[u8]) -> Option<(ImageHeader, &[u8])> {
        streams(bytes)
            .into_iter()
            .max_by_key(|(h, _)| h.width * h.height)
    }

    fn corpus_file(name: &str) -> Option<Vec<u8>> {
        let root = crate::tiff::tests::corpus()?;
        std::fs::read(root.join(name)).ok()
    }

    /// Decode one band of one component of one tile, with the coder's
    /// workings recorded.
    fn one_band(
        header: &ImageHeader,
        sample: &[u8],
        tile: usize,
        plane: usize,
        band: usize,
        lines: Option<usize>,
    ) -> (Vec<i32>, Shape, Trace, bool) {
        let tiles = parse_tiles(header, sample).expect("the index");
        let t = &tiles[tile];
        let shapes = band_shapes(header.levels, t.width, t.height, t.neighbours);
        let p = &t.planes[plane];
        let b = &p.bands[band];
        let mode = if band == 0 && p.ref_prev_line {
            Mode::A {
                round: p.round_mask,
                round_bits: p.round_bits,
            }
        } else {
            Mode::B
        };
        let mut quant = if b.q_per_line {
            Quant::PerLine {
                exponent: b.q_param,
                k: 0,
            }
        } else {
            Quant::Uniform(step_of(b.q_param))
        };
        let mut coder = Coder::new(b.bitstream(sample), shapes[band].width);
        coder.trace.on = true;
        let data = run_band(
            &mut coder,
            lines.unwrap_or(shapes[band].height),
            mode,
            &mut quant,
        )
        .expect("the band");
        let exact = coder.r.consumed_exactly();
        (data, shapes[band], std::mem::take(&mut coder.trace), exact)
    }

    #[test]
    fn eos_r_lossless_band_matches_the_reference() {
        let Some(bytes) = corpus_file("Canon/EOS_R-RAW_ISO_100.CR3") else {
            return;
        };
        let (header, sample) = raw_stream(&bytes).expect("a raw track");
        assert_eq!(header.levels, 0);
        let (data, shape, trace, exact) = one_band(&header, sample, 0, 0, 0, Some(3));
        assert_eq!((shape.width, shape.height), (1722, 2273));

        // Line 0: predicted from the left, opening with the escape.
        #[rustfmt::skip]
        let line0 = [
            -7684, -7677, -7680, -7680, -7679, -7680, -7679, -7682, -7679, -7679, -7682, -7680,
            -7677, -7679, -7680, -7684, -7685, -7686, -7686, -7685, -7680, -7676, -7680, -7684,
            -7675, -7684, -7680, -7680, -7682, -7685, -7680, -7686, -7675, -7677, -7679, -7677,
            -7679, -7677, -7677, -7677, -7677, -7677, -7680, -7684, -7682, -7680, -7679, -7680,
            -7679, -7680, -7684, -7684, -7682, -7682, -7684, -7674, -7677, -7685, -7685, -7679,
            -7682, -7680, -7682, -7682,
        ];
        assert_eq!(&data[..64], &line0);
        // Line 1: the four-way predictor, and a run at column 52.
        #[rustfmt::skip]
        let line1 = [
            -7676, -7682, -7684, -7684, -7684, -7679, -7676, -7680, -7680, -7680, -7679, -7682,
            -7680, -7682, -7686, -7677, -7682, -7680, -7682, -7680, -7686, -7684, -7680, -7682,
            -7680, -7679, -7682, -7679, -7680, -7676, -7680, -7674, -7679, -7677, -7682, -7680,
            -7684, -7682, -7679, -7679, -7680, -7679, -7685, -7677, -7679, -7682, -7685, -7679,
            -7679, -7680, -7677, -7682, -7680, -7675, -7676, -7672, -7676, -7680, -7684, -7675,
            -7686, -7677, -7682, -7682,
        ];
        assert_eq!(&data[1722..1722 + 64], &line1);
        #[rustfmt::skip]
        let line2 = [
            -7680, -7677, -7679, -7680, -7675, -7680, -7680, -7684, -7685, -7675, -7684, -7680,
            -7680, -7676, -7682, -7686,
        ];
        assert_eq!(&data[2 * 1722..2 * 1722 + 16], &line2);

        // K after each of the first 64 symbols of lines 0 and 1. Line
        // 0 spends exactly one symbol a column, so line 1's start at
        // 1722, and K carries across the boundary rather than
        // resetting.
        #[rustfmt::skip]
        let k0 = [
            2u32, 3, 3, 2, 2, 1, 1, 1, 2, 1, 1, 1, 2, 2, 1, 2, 1, 1, 0, 0, 2, 2, 2, 2,
            3, 3, 3, 2, 2, 2, 2, 2, 3, 2, 2, 2, 2, 2, 1, 0, 0, 0, 1, 2, 2, 2, 2, 1,
            1, 1, 2, 1, 1, 0, 1, 3, 3, 3, 2, 3, 3, 3, 2, 1,
        ];
        assert_eq!(&trace.k[..64], &k0);
        #[rustfmt::skip]
        let k1 = [
            3u32, 3, 2, 1, 1, 2, 2, 2, 1, 1, 1, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2,
            3, 3, 2, 2, 2, 3, 3, 3, 3, 3, 3, 2, 2, 1, 1, 0, 0, 1, 2, 2, 2, 2, 2, 2,
            1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2,
        ];
        assert_eq!(&trace.k[1722..1722 + 64], &k1);

        // Where run mode engages, as (column, length, state before,
        // after). Line 0 has exactly one — the flag at column 0 — and
        // line 1's first sixteen are these.
        let runs: Vec<_> = trace.runs.iter().filter(|r| r.0 == 0).collect();
        assert_eq!(runs.len(), 1);
        assert_eq!((runs[0].1, runs[0].2), (0, 0));
        let line1_runs: Vec<(usize, usize, usize, usize)> = trace
            .runs
            .iter()
            .filter(|r| r.0 == 1)
            .map(|r| (r.1, r.2, r.3, r.4))
            .collect();
        assert_eq!(
            &line1_runs[..16],
            &[
                (52, 0, 0, 0),
                (213, 2, 0, 0),
                (216, 0, 0, 0),
                (238, 0, 0, 0),
                (250, 1, 0, 0),
                (259, 0, 0, 0),
                (275, 0, 0, 0),
                (279, 0, 0, 0),
                (305, 0, 0, 0),
                (311, 1, 0, 0),
                (387, 3, 0, 1),
                (417, 0, 1, 1),
                (504, 0, 1, 1),
                (567, 1, 1, 0),
                (631, 0, 0, 0),
                (657, 0, 0, 0),
            ]
        );
        assert_eq!(line1_runs.len(), 41);
        assert_eq!(trace.runs.iter().filter(|r| r.0 == 2).count(), 45);
        // Three lines is not the whole band, so it has not been
        // consumed yet.
        assert!(!exact);

        // The whole band, which must land exactly on its last byte.
        let (data, shape, _, exact) = one_band(&header, sample, 0, 0, 0, None);
        assert!(exact, "the band did not end on its last byte");
        assert_eq!(data.len(), shape.width * shape.height);
    }

    #[test]
    fn eos_r_craw_bands_match_the_reference() {
        let Some(bytes) = corpus_file("Canon_EOS_R_CRAW_ISO_100.CR3") else {
            return;
        };
        let (header, sample) = raw_stream(&bytes).expect("a raw track");
        assert_eq!((header.version, header.levels), (0x0100, 3));
        let tiles = parse_tiles(&header, sample).unwrap();
        // Every band of tile 0 component 0, with its declared
        // bitstream length and its first sixteen dequantised
        // coefficients.
        #[rustfmt::skip]
        let want: [(usize, [i32; 16]); 10] = [
            (59543,  [-7680, -7678, -7680, -7677, -7680, -7680, -7680, -7678, -7680, -7677, -7678, -7679, -7679, -7678, -7678, -7677]),
            (53927,  [2, 1, -2, -2, 2, 3, -2, 0, 0, -2, 1, 0, 1, -3, -2, 0]),
            (46812,  [0, 1, 0, 0, 0, -2, 2, 2, 0, 3, 0, 1, 0, 0, 2, 2]),
            (51876,  [1, 2, 5, 5, 1, 0, -2, 0, 1, -6, -2, 1, 3, 1, -6, -3]),
            (136810, [0, 4, 0, 0, 0, 4, 4, 0, -4, -4, -4, -4, 4, 0, 0, 4]),
            (121881, [0, 0, -4, 0, 0, 4, 0, 0, 0, 0, 4, 0, -4, 0, 0, 0]),
            (112121, [0, 8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 8, 0, 0, 0, 0]),
            (330553, [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
            (315674, [0, 0, 0, 0, 0, 0, 0, 0, 12, 0, 0, 0, 0, 0, 0, 12]),
            (238244, [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
        ];
        let steps = [1, 1, 1, 1, 4, 4, 8, 12, 12, 25];
        for (index, (length, head)) in want.iter().enumerate() {
            let band = &tiles[0].planes[0].bands[index];
            assert_eq!(
                band.bitstream(sample).len(),
                *length,
                "band {index} bitstream length"
            );
            assert_eq!(step_of(band.q_param), steps[index], "band {index} step");
            let (data, _, _, exact) = one_band(&header, sample, 0, 0, index, None);
            assert_eq!(&data[..16], head, "band {index} line 0");
            assert!(exact, "band {index} did not end on its last byte");
            // Every detail coefficient is a multiple of the step it
            // was quantised with.
            if index > 0 {
                assert!(data.iter().all(|c| c % steps[index] == 0));
            }
        }
        // Band 9's line 0 is one run of 862 zeros.
        let (_, shape, trace, _) = one_band(&header, sample, 0, 0, 9, Some(1));
        assert_eq!(shape.width, 862);
        assert_eq!(trace.runs.len(), 1);
        assert_eq!(trace.runs[0].2, 862);
        // Band 1's line 0 opens with a zero-length run and the biased
        // symbol that follows one, then a run of one at column 8.
        let (data, _, trace, _) = one_band(&header, sample, 0, 0, 1, Some(1));
        assert_eq!(data[0], 2);
        assert_eq!((trace.runs[0].1, trace.runs[0].2), (0, 0));
        assert_eq!((trace.runs[1].1, trace.runs[1].2), (8, 1));
    }

    #[test]
    fn eos_r_craw_inverse_wavelet_matches_the_reference() {
        let Some(bytes) = corpus_file("Canon_EOS_R_CRAW_ISO_100.CR3") else {
            return;
        };
        let (header, sample) = raw_stream(&bytes).expect("a raw track");
        let tiles = parse_tiles(&header, sample).unwrap();
        let tile = &tiles[0];
        let shapes = band_shapes(header.levels, tile.width, tile.height, tile.neighbours);
        let data = decode_component(sample, &header, tile, &tile.planes[0], &shapes, None).unwrap();
        assert_eq!(data.len(), 1722 * 2273);
        #[rustfmt::skip]
        let want = [
            [-7681, -7681, -7680, -7679, -7678, -7678, -7678, -7678, -7677, -7678, -7679, -7680, -7680, -7680, -7680, -7683],
            [-7681, -7682, -7681, -7681, -7680, -7679, -7677, -7679, -7681, -7681, -7681, -7681, -7680, -7681, -7681, -7678],
            [-7681, -7682, -7682, -7682, -7681, -7679, -7676, -7680, -7684, -7683, -7682, -7681, -7680, -7681, -7681, -7684],
            [-7681, -7682, -7682, -7682, -7681, -7680, -7679, -7681, -7683, -7677, -7682, -7681, -7680, -7681, -7681, -7678],
        ];
        for (row, want) in want.iter().enumerate() {
            assert_eq!(&data[row * 1722..row * 1722 + 16], want, "row {row}");
        }
    }

    #[test]
    fn the_first_frame_rows_of_both_eos_r_qualities() {
        // The level shift and the interleave back onto the Bayer
        // grid: even columns of even rows are component 0, odd
        // columns of even rows component 1, and so on.
        #[rustfmt::skip]
        let cases: [(&str, [[u16; 16]; 4]); 2] = [
            ("Canon/EOS_R-RAW_ISO_100.CR3", [
                [508, 514, 515, 512, 512, 510, 512, 512, 513, 513, 512, 510, 513, 513, 510, 512],
                [510, 512, 508, 508, 512, 510, 512, 512, 514, 515, 509, 512, 515, 510, 514, 515],
                [516, 505, 510, 513, 508, 514, 508, 513, 508, 510, 513, 508, 516, 512, 512, 510],
                [509, 513, 515, 512, 512, 515, 512, 512, 514, 513, 512, 512, 515, 508, 507, 515],
            ]),
            ("Canon_EOS_R_CRAW_ISO_100.CR3", [
                [511, 511, 511, 511, 512, 511, 513, 511, 514, 511, 514, 511, 514, 512, 514, 512],
                [510, 514, 512, 512, 515, 511, 514, 510, 513, 509, 512, 512, 512, 516, 512, 515],
                [511, 512, 510, 512, 511, 512, 511, 511, 512, 511, 513, 511, 515, 511, 513, 511],
                [513, 514, 513, 512, 513, 511, 513, 511, 514, 511, 513, 511, 513, 512, 513, 512],
            ]),
        ];
        for (name, want) in cases {
            let Some(bytes) = corpus_file(name) else {
                return;
            };
            let (header, sample) = raw_stream(&bytes).expect("a raw track");
            let frame = decode(&header, sample).unwrap();
            assert_eq!(frame.len(), header.width * header.height);
            for (row, want) in want.iter().enumerate() {
                assert_eq!(
                    &frame[row * header.width..row * header.width + 16],
                    want,
                    "{name} row {row}"
                );
            }
        }
    }

    /// Every CMP1 in the corpus, with the sample its track points at.
    #[test]
    fn corpus_crx_headers_describe_their_samples() {
        let Some(root) = crate::tiff::tests::corpus() else {
            return;
        };
        let mut problems: Vec<String> = Vec::new();
        let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        let mut checked = 0;
        for path in crate::tiff::tests::samples(&root) {
            let is_cr3 = path
                .extension()
                .map(|e| e.to_string_lossy().to_ascii_uppercase() == "CR3")
                .unwrap_or(false);
            if !is_cr3 {
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            let name = path
                .strip_prefix(&root)
                .unwrap_or(&path)
                .display()
                .to_string();
            for (header, sample) in streams(&bytes) {
                if header.planes != 4 || header.bits != 14 {
                    problems.push(format!(
                        "{name}: {} planes of {} bits",
                        header.planes, header.bits
                    ));
                }
                seen.insert(format!(
                    "v{:#06x} levels {} enc {} tiles {:?} cfa {}",
                    header.version,
                    header.levels,
                    header.enc_type,
                    header.tile_grid(),
                    header.cfa_layout
                ));
                match parse_tiles(&header, sample) {
                    Err(e) => {
                        problems.push(format!("{name}: {}x{}: {e}", header.width, header.height))
                    }
                    Ok(tiles) => {
                        checked += 1;
                        // Tiles must fill the sample after the index,
                        // components must fill their tile and bands
                        // their component, with nothing left over.
                        let mut at = header.mdat_header_size;
                        for tile in &tiles {
                            if tile.data.start != at {
                                problems
                                    .push(format!("{name}: tile at {} not {at}", tile.data.start));
                            }
                            at = tile.data.end;
                            let mut plane_at = tile.data.start + tile.qp_size + tile.extra_size;
                            for plane in &tile.planes {
                                if plane.data.start != plane_at {
                                    problems.push(format!(
                                        "{name}: component at {} not {plane_at}",
                                        plane.data.start
                                    ));
                                }
                                plane_at = plane.data.end;
                                let mut band_at = plane.data.start;
                                for band in &plane.bands {
                                    if band.data.start != band_at {
                                        problems.push(format!(
                                            "{name}: band at {} not {band_at}",
                                            band.data.start
                                        ));
                                    }
                                    band_at = band.data.end;
                                }
                                if band_at != plane.data.end {
                                    problems.push(format!(
                                        "{name}: bands leave {} bytes",
                                        plane.data.end - band_at
                                    ));
                                }
                            }
                            if plane_at != tile.data.end {
                                problems.push(format!(
                                    "{name}: components leave {} bytes",
                                    tile.data.end - plane_at
                                ));
                            }
                            let shapes = band_shapes(
                                header.levels,
                                tile.width,
                                tile.height,
                                tile.neighbours,
                            );
                            if shapes.len() != header.bands() {
                                problems.push(format!("{name}: {} shapes", shapes.len()));
                            }
                            if shapes.iter().any(|s| s.width == 0 || s.height == 0) {
                                problems.push(format!("{name}: an empty band shape"));
                            }
                        }
                        if at != sample.len() {
                            problems
                                .push(format!("{name}: tiles leave {} bytes", sample.len() - at));
                        }
                        // A lossless frame's bands all carry the unit
                        // quantiser; a version-1 lossy one does not.
                        let unit = tiles
                            .iter()
                            .flat_map(|t| &t.planes)
                            .flat_map(|p| &p.bands)
                            .all(|b| b.q_param == 4);
                        if header.levels == 0 && !unit {
                            problems.push(format!("{name}: a lossless band with a quantiser"));
                        }
                    }
                }
            }
        }
        assert!(
            problems.is_empty(),
            "{} problems:\n{}",
            problems.len(),
            problems.join("\n")
        );
        eprintln!("corpus: {checked} CRX streams; shapes seen:");
        for shape in &seen {
            eprintln!("    {shape}");
        }
    }

    /// Every band of every corpus CR3 must be consumed to its last
    /// byte — the strongest single check that the entropy coder is
    /// right, because a stream that is decoded wrongly almost never
    /// lands on the end.
    #[test]
    fn corpus_crx_bands_are_consumed_exactly() {
        let Some(root) = crate::tiff::tests::corpus() else {
            return;
        };
        let mut problems: Vec<String> = Vec::new();
        for path in crate::tiff::tests::samples(&root) {
            if path
                .extension()
                .map(|e| e.to_string_lossy().to_ascii_uppercase())
                != Some("CR3".into())
            {
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            let name = path
                .strip_prefix(&root)
                .unwrap_or(&path)
                .display()
                .to_string();
            let Some((header, sample)) = raw_stream(&bytes) else {
                continue;
            };
            if header.enc_type != 0 {
                continue;
            }
            let Ok(tiles) = parse_tiles(&header, sample) else {
                continue;
            };
            for (t, tile) in tiles.iter().enumerate() {
                for (p, plane) in tile.planes.iter().enumerate() {
                    for (b, band) in plane.bands.iter().enumerate() {
                        let (_, _, _, exact) = one_band(&header, sample, t, p, b, None);
                        if !exact {
                            problems.push(format!(
                                "{name}: tile {t} component {p} band {b} ({} bytes)",
                                band.bitstream(sample).len()
                            ));
                        }
                    }
                }
            }
        }
        assert!(
            problems.is_empty(),
            "{} bands not consumed exactly:\n{}",
            problems.len(),
            problems.join("\n")
        );
    }
}
