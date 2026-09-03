//! Canon's CRX codec: the wavelet-and-Golomb scheme inside a CR3.
//!
//! A CRX image is a grid of *tiles*. Each tile holds one *plane* per
//! colour-filter position — a Bayer frame is split into its four 2x2
//! sub-planes, so a plane is half the tile wide and half as tall — and
//! each plane holds one *subband* per wavelet band. With zero wavelet
//! levels (what Canon's "RAW" quality writes) a plane is a single LL
//! band and the coefficients are the sensor samples themselves; with
//! levels (what "CRAW" writes) there are `3 * levels + 1` bands and a
//! 5/3-style integer lifting wavelet to invert, with a per-band
//! quantiser in the lossy case.
//!
//! The shapes and sizes come from two places: the `CMP1` box in the
//! track's sample entry ([`ImageHeader`]), and a header of small
//! tag/size records in front of the sample data in `mdat`
//! ([`parse_tiles`]). Nothing about the layouts here is guessed from
//! another decoder's source: the record tags and field positions were
//! read off real files and cross-checked against the public CR3 notes.

//! ## What is known about the coefficient bitstream
//!
//! [`decode`] does not decode coefficients yet, and this is what was
//! established about them from the corpus, so the next hand does not
//! start from nothing. All of it was checked against
//! `unprocessed_raw`'s output for `EOS R RAW`, tile 0 plane 0 — a
//! lossless zero-level band, 1722 by 2273 coefficients — and none of
//! it comes from another decoder.
//!
//! Bits are read most-significant first, straight down the bytes
//! ([`crate::bits::BitPumpMsb`]), from the band's first byte. A
//! coefficient is coded as its prediction residual `d`, folded to a
//! non-negative `m`: `m = 2d` for `d >= 0`, `m = -2d - 1` for `d < 0`.
//! `m` is then a Rice code with a running parameter `k`: the quotient
//! `m >> k` in unary — that many zero bits, then a one — followed by
//! the low `k` bits of `m`. `k` starts at 0 at the head of a band. A
//! quotient reaching 42 escapes: 42 zeros, a one, then `m` in a
//! 21-bit field.
//!
//! `k` is updated after every coefficient from
//!
//! ```text
//! s = m + (the folded magnitude one to the right on the line above)
//! raise k while s >= 6 << k, by at most two;
//! if it did not rise and s < (1 << k) - 1, lower it by one.
//! ```
//!
//! On the first line, which has no line above, `s = 2m` — the same
//! rule with the coefficient standing in for its own neighbour. `k`
//! carries across the line boundary. An accumulator scheme in the
//! manner of JPEG-LS was tried and does not reproduce the stream, and
//! neither does a `k` read only from the line above: it is a running
//! state that the line above steers.
//!
//! The first line predicts from the left, with `1 << (bits - 1)`
//! standing in for the pixel before the first — which is why every
//! lossless band opens with the escape, its first residual being
//! about `black - 8192`. Later lines predict with the gradient median
//! of the left, above and above-left neighbours (`min(a, b)` when
//! `c >= max(a, b)`, `max(a, b)` when `c <= min(a, b)`, else
//! `a + b - c`).
//!
//! That decodes the whole of line 0 exactly, and line 1 exactly up to
//! its 53rd coefficient. There it wants `k` to fall and then rise
//! again over two coefficients that the rule above holds steady, and
//! wants `min(a, b)` where the gradient median gives `max(a, b)` —
//! two symptoms at one spot, which is what a mode switch looks like:
//! most likely the zero-run mode that the magnitudes of the line
//! above select into. Finding that is all that stands between this
//! and lossless CR3 frames.

use crate::{Error, Result};

/// The `CMP1` box: what shape the coded image is.
///
/// The first four bytes are a marker (0xff00 or 0xff10) and the size
/// of the rest, then a version that says which of the two record
/// dialects the `mdat` header speaks (see [`parse_tiles`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageHeader {
    /// 1 on the EOS R / RP / M50 / 90D generation, 2 from the R5 on.
    pub version: u8,
    /// The whole coded frame, which is also the raw sensor frame.
    pub width: usize,
    pub height: usize,
    /// One tile of it. Tiles divide the frame left to right, top to
    /// bottom; the last tile in a row or column is whatever is left.
    pub tile_width: usize,
    pub tile_height: usize,
    /// Bits per sensor sample, 14 on every body seen.
    pub bits: u32,
    /// Planes per tile: 4, the Bayer sub-planes, in the order
    /// top-left, top-right, bottom-left, bottom-right.
    pub planes: usize,
    /// Which 2x2 phase the frame starts on. 0 everywhere on the
    /// full-size raw track; the small "SD" raw track uses 1.
    pub cfa_layout: u8,
    /// Wavelet levels: 0 for lossless RAW, 3 for cRAW so far.
    pub levels: u32,
    /// Whether the wavelet runs across tile columns / rows, which the
    /// encoder only sets when there is both a wavelet and more than
    /// one tile in that direction.
    pub tile_cols_linked: bool,
    pub tile_rows_linked: bool,
    /// Bytes of tag/size records in front of the sample data.
    pub mdat_header_size: usize,
}

impl ImageHeader {
    /// Parse the payload of a `CMP1` box (the 52 bytes after its
    /// 8-byte box header).
    pub fn parse(payload: &[u8]) -> Result<ImageHeader> {
        if payload.len() < 32 {
            return Err(Error::Corrupt(format!("CMP1 is {} bytes", payload.len())));
        }
        let u32_at = |at: usize| {
            u32::from_be_bytes([
                payload[at],
                payload[at + 1],
                payload[at + 2],
                payload[at + 3],
            ])
        };
        // Byte 4 is the record dialect of the mdat header — 1 or 2,
        // with the byte after it zero — and byte 24 onwards is a run
        // of small fields, one or two to the byte. Everything wider
        // than a byte in here is big-endian.
        let version = payload[4];
        let width = u32_at(8) as usize;
        let height = u32_at(12) as usize;
        let tile_width = u32_at(16) as usize;
        let tile_height = u32_at(20) as usize;
        let bits = payload[24] as u32;
        let planes = (payload[25] >> 4) as usize;
        let cfa_layout = payload[25] & 0xf;
        let levels = (payload[26] & 0xf) as u32;
        let flags = payload[27];
        let mdat_header_size = u32_at(28) as usize;

        if width == 0 || height == 0 || tile_width == 0 || tile_height == 0 {
            return Err(Error::Corrupt("CMP1 with a zero dimension".into()));
        }
        // Everything downstream indexes a u16 buffer of width*height
        // and slices tiles out of it; a frame that cannot be allocated
        // is a file lying about itself, not a decode we should attempt.
        if width > 1 << 17 || height > 1 << 17 {
            return Err(Error::Corrupt(format!("CMP1 frame {width}x{height}")));
        }
        if !(1..=16).contains(&bits) {
            return Err(Error::Corrupt(format!("CMP1 says {bits} bits a sample")));
        }
        Ok(ImageHeader {
            version,
            width,
            height,
            tile_width,
            tile_height,
            bits,
            planes,
            cfa_layout,
            levels,
            tile_cols_linked: flags & 0x80 != 0,
            tile_rows_linked: flags & 0x40 != 0,
            mdat_header_size,
        })
    }

    /// Tiles across and down.
    pub fn tile_grid(&self) -> (usize, usize) {
        (
            self.width.div_ceil(self.tile_width),
            self.height.div_ceil(self.tile_height),
        )
    }

    /// Bands in one plane: one LL band, then HL/LH/HH per level.
    pub fn bands(&self) -> usize {
        3 * self.levels as usize + 1
    }
}

/// One subband's slice of a plane's data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Band {
    /// Where the band's bits are, inside the whole `mdat` sample.
    pub data: std::ops::Range<usize>,
    /// Index within the plane, 0 for LL.
    pub index: usize,
    /// The twelve bits under the band counter in the record's flags.
    /// Every band of a lossless frame carries 32 here; in a version-1
    /// cRAW frame it grows with the wavelet level (32 for the level-3
    /// bands, up to 256 for the finest), which is what a quantiser
    /// looks like. Version 2 leaves it zero and puts a quantiser
    /// table at the front of the tile instead.
    pub q_step: u32,
    /// The whole flags word as written, for a caller that wants to
    /// report on the bits whose meaning is not established here.
    pub flags: u32,
}

/// One Bayer sub-plane of a tile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plane {
    pub data: std::ops::Range<usize>,
    pub index: usize,
    /// Bit 27 of the flags, set in every file seen.
    pub extra_flag: bool,
    /// The whole flags word as written; its top nibble is the plane
    /// counter and the rest is not established here.
    pub flags: u32,
    pub bands: Vec<Band>,
}

/// One tile of the frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tile {
    pub data: std::ops::Range<usize>,
    /// Position in the tile grid.
    pub col: usize,
    pub row: usize,
    /// Pixels of the frame this tile covers.
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
    /// Version 2 only: the quantiser table that precedes the plane
    /// data inside the tile, in bytes.
    pub qp_table_size: usize,
    pub planes: Vec<Plane>,
}

/// A record in the `mdat` header: a big-endian tag, a big-endian
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

fn be32(b: &[u8], at: usize) -> Result<u32> {
    b.get(at..at + 4)
        .map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
        .ok_or_else(|| Error::Corrupt("short CRX header record".into()))
}

/// Read the tag/size records in front of a sample and turn them into
/// the tile / plane / band tree, with each node's byte range inside
/// `sample` filled in.
///
/// The records come in two dialects. Version 1 (EOS R generation) uses
/// tags 0xff01 tile, 0xff02 plane, 0xff03 band, each with an 8-byte
/// payload of a size and a flags word. Version 2 (R5 onwards) uses
/// 0xff11 / 0xff12 / 0xff13, and the tile and band records carry
/// sixteen bytes: the tile's says how much of the tile is a quantiser
/// table before the planes start. In both, a node's children tile its
/// bytes exactly and the tiles tile the sample after the header.
pub fn parse_tiles(header: &ImageHeader, sample: &[u8]) -> Result<Vec<Tile>> {
    if header.mdat_header_size > sample.len() {
        return Err(Error::Corrupt(format!(
            "CRX header of {} bytes in a {}-byte sample",
            header.mdat_header_size,
            sample.len()
        )));
    }
    let (tile_tag, plane_tag, band_tag) = match header.version {
        1 => (0xff01u16, 0xff02u16, 0xff03u16),
        2 => (0xff11, 0xff12, 0xff13),
        other => return Err(Error::Unsupported(format!("CRX header version {other}"))),
    };
    let (cols, rows) = header.tile_grid();
    let mut tiles: Vec<Tile> = Vec::new();
    // Where the next node of each kind starts: tiles follow the
    // header, planes follow the previous plane inside their tile,
    // bands the previous band inside their plane.
    let mut tile_at = header.mdat_header_size;
    let mut plane_at = 0usize;
    let mut band_at = 0usize;

    for record in records(&sample[..header.mdat_header_size])? {
        if record.tag == tile_tag {
            let size = be32(record.payload, 0)? as usize;
            let qp_table_size = if header.version >= 2 {
                be32(record.payload, 8)? as usize
            } else {
                0
            };
            let index = tiles.len();
            if index >= cols * rows {
                return Err(Error::Corrupt("more CRX tiles than the grid holds".into()));
            }
            let (col, row) = (index % cols, index / cols);
            let x = col * header.tile_width;
            let y = row * header.tile_height;
            let end = tile_at
                .checked_add(size)
                .filter(|end| *end <= sample.len())
                .ok_or_else(|| Error::Corrupt("CRX tile runs past the sample".into()))?;
            tiles.push(Tile {
                data: tile_at..end,
                col,
                row,
                x,
                y,
                width: header.tile_width.min(header.width - x),
                height: header.tile_height.min(header.height - y),
                qp_table_size,
                planes: Vec::new(),
            });
            // Version 2 puts the quantiser table at the front of the
            // tile, so the first plane starts after it.
            plane_at = tile_at + qp_table_size.min(size);
            tile_at = end;
        } else if record.tag == plane_tag {
            let size = be32(record.payload, 0)? as usize;
            let flags = be32(record.payload, 4)?;
            let tile = tiles
                .last_mut()
                .ok_or_else(|| Error::Corrupt("CRX plane before any tile".into()))?;
            let end = plane_at
                .checked_add(size)
                .filter(|end| *end <= tile.data.end)
                .ok_or_else(|| Error::Corrupt("CRX plane runs past its tile".into()))?;
            tile.planes.push(Plane {
                data: plane_at..end,
                index: tile.planes.len(),
                extra_flag: flags & 0x0800_0000 != 0,
                flags,
                bands: Vec::new(),
            });
            band_at = plane_at;
            plane_at = end;
        } else if record.tag == band_tag {
            let size = be32(record.payload, 0)? as usize;
            let flags = be32(record.payload, 4)?;
            let plane = tiles
                .last_mut()
                .and_then(|t| t.planes.last_mut())
                .ok_or_else(|| Error::Corrupt("CRX band before any plane".into()))?;
            let end = band_at
                .checked_add(size)
                .filter(|end| *end <= plane.data.end)
                .ok_or_else(|| Error::Corrupt("CRX band runs past its plane".into()))?;
            plane.bands.push(Band {
                data: band_at..end,
                index: plane.bands.len(),
                // The quantiser sits in the twelve bits under the
                // band counter; 0x20 is unity and every lossless band
                // carries exactly that.
                q_step: (flags >> 16) & 0xfff,
                flags,
            });
            band_at = end;
        }
        // Anything else in the header is not a node of the tree; the
        // parser ignores it rather than failing, so a body that adds a
        // record still decodes.
    }

    if tiles.len() != cols * rows {
        return Err(Error::Corrupt(format!(
            "CRX header has {} tiles, the grid wants {}",
            tiles.len(),
            cols * rows
        )));
    }
    for tile in &tiles {
        if tile.planes.len() != header.planes {
            return Err(Error::Corrupt(format!(
                "CRX tile has {} planes, CMP1 says {}",
                tile.planes.len(),
                header.planes
            )));
        }
        for plane in &tile.planes {
            if plane.bands.len() != header.bands() {
                return Err(Error::Corrupt(format!(
                    "CRX plane has {} bands, {} levels wants {}",
                    plane.bands.len(),
                    header.levels,
                    header.bands()
                )));
            }
        }
    }
    Ok(tiles)
}

/// Decode one CRX sample into the full frame, `width * height`
/// samples row-major with the Bayer planes interleaved back.
pub fn decode(header: &ImageHeader, sample: &[u8]) -> Result<Vec<u16>> {
    // Parsed even though the coefficients are out of reach, so that a
    // file whose header does not describe itself is reported as the
    // corruption it is rather than as an unsupported variant.
    let tiles = parse_tiles(header, sample)?;
    let bands: usize = tiles
        .iter()
        .flat_map(|t| &t.planes)
        .map(|p| p.bands.len())
        .sum();
    Err(Error::Unsupported(format!(
        "the CRX entropy coder (v{}, {} wavelet levels, {} tile(s), {bands} bands): \
         the container and every header are read, the coefficients are not",
        header.version,
        header.levels,
        tiles.len(),
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A CMP1 payload shaped like the ones real files carry.
    fn cmp1(version: u8, w: u32, h: u32, tw: u32, th: u32, levels: u8, hdr: u32) -> Vec<u8> {
        let mut p = vec![0u8; 52];
        p[0..2].copy_from_slice(&0xff00u16.to_be_bytes());
        p[2..4].copy_from_slice(&0x0030u16.to_be_bytes());
        p[4] = version;
        p[8..12].copy_from_slice(&w.to_be_bytes());
        p[12..16].copy_from_slice(&h.to_be_bytes());
        p[16..20].copy_from_slice(&tw.to_be_bytes());
        p[20..24].copy_from_slice(&th.to_be_bytes());
        p[24] = 14;
        p[25] = 0x40;
        p[26] = levels;
        p[28..32].copy_from_slice(&hdr.to_be_bytes());
        p
    }

    fn record(tag: u16, payload: &[u8]) -> Vec<u8> {
        let mut out = tag.to_be_bytes().to_vec();
        out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        out.extend_from_slice(payload);
        out
    }

    fn sizes(size: u32, flags: u32) -> Vec<u8> {
        let mut out = size.to_be_bytes().to_vec();
        out.extend_from_slice(&flags.to_be_bytes());
        out
    }

    #[test]
    fn image_header_fields() {
        let header = ImageHeader::parse(&cmp1(1, 6888, 4546, 3444, 4546, 0, 216)).unwrap();
        assert_eq!(header.version, 1);
        assert_eq!((header.width, header.height), (6888, 4546));
        assert_eq!(header.tile_grid(), (2, 1));
        assert_eq!(header.bits, 14);
        assert_eq!(header.planes, 4);
        assert_eq!(header.bands(), 1);
        let lossy = ImageHeader::parse(&cmp1(1, 6888, 4546, 3444, 4546, 3, 1080)).unwrap();
        assert_eq!(lossy.bands(), 10);
    }

    #[test]
    fn a_short_or_impossible_cmp1_is_corrupt() {
        assert!(ImageHeader::parse(&[0u8; 8]).is_err());
        assert!(ImageHeader::parse(&cmp1(1, 0, 4546, 3444, 4546, 0, 216)).is_err());
        let mut deep = cmp1(1, 100, 100, 100, 100, 0, 16);
        deep[24] = 0;
        assert!(ImageHeader::parse(&deep).is_err());
    }

    #[test]
    fn tiles_planes_and_bands_tile_their_parents() {
        // Two tiles of four planes, one band each: the shape of a
        // lossless EOS R frame, in miniature.
        let mut hdr = Vec::new();
        for tile in 0..2u32 {
            hdr.extend(record(0xff01, &sizes(40, tile << 16)));
            for plane in 0..4u32 {
                hdr.extend(record(0xff02, &sizes(10, 0x0800_0000 | (plane << 28))));
                hdr.extend(record(0xff03, &sizes(10, 0x0020_0000)));
            }
        }
        let header = ImageHeader::parse(&cmp1(1, 8, 4, 4, 4, 0, hdr.len() as u32)).unwrap();
        let mut sample = hdr.clone();
        sample.extend(std::iter::repeat_n(0u8, 80));
        let tiles = parse_tiles(&header, &sample).unwrap();
        assert_eq!(tiles.len(), 2);
        assert_eq!(tiles[0].data, hdr.len()..hdr.len() + 40);
        assert_eq!(tiles[1].data, hdr.len() + 40..hdr.len() + 80);
        assert_eq!(tiles[1].col, 1);
        assert_eq!(tiles[0].planes[1].data, hdr.len() + 10..hdr.len() + 20);
        assert!(tiles[0].planes[0].extra_flag);
        assert_eq!(tiles[0].planes[0].bands[0].q_step, 32);
        assert_eq!(
            tiles[0].planes[3].bands[0].data,
            hdr.len() + 30..hdr.len() + 40
        );
    }

    #[test]
    fn a_header_that_does_not_add_up_is_rejected() {
        let header = ImageHeader::parse(&cmp1(1, 8, 4, 4, 4, 0, 12)).unwrap();
        // One tile record for a two-tile grid.
        let hdr = record(0xff01, &sizes(8, 0));
        let mut sample = hdr.clone();
        sample.extend([0u8; 8]);
        assert!(parse_tiles(&header, &sample).is_err());
        // A tile bigger than the sample.
        let hdr = record(0xff01, &sizes(1 << 20, 0));
        let mut sample = hdr.clone();
        sample.extend([0u8; 8]);
        assert!(parse_tiles(&header, &sample).is_err());
        // A header longer than the sample it describes.
        let header = ImageHeader::parse(&cmp1(1, 8, 4, 8, 4, 0, 1 << 20)).unwrap();
        assert!(parse_tiles(&header, &[0u8; 32]).is_err());
    }

    #[test]
    fn an_unknown_record_dialect_is_unsupported() {
        let header = ImageHeader::parse(&cmp1(7, 8, 4, 8, 4, 0, 0)).unwrap();
        assert!(matches!(
            parse_tiles(&header, &[0u8; 8]),
            Err(Error::Unsupported(_))
        ));
    }

    // ---------------------------------------------------------------
    // Corpus: the headers of real CRX streams.
    // ---------------------------------------------------------------

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
            let Ok(boxes) = crate::bmff::parse(&bytes) else {
                problems.push(format!("{name}: does not parse"));
                continue;
            };
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
                let header = match ImageHeader::parse(&bytes[cmp1.data.clone()]) {
                    Ok(header) => header,
                    Err(e) => {
                        problems.push(format!("{name}: CMP1: {e}"));
                        continue;
                    }
                };
                // The sample entry's own VisualSampleEntry width and
                // height are the frame on the EOS R generation and
                // the crop on the R5 and later, so they only bound
                // CMP1's rather than matching it — enough to catch a
                // field read at the wrong offset.
                let visual = &bytes[entry.data.clone()];
                let dim = |at: usize| u16::from_be_bytes([visual[at], visual[at + 1]]) as usize;
                if dim(24) > header.width || dim(26) > header.height || dim(24) * 2 < header.width {
                    problems.push(format!(
                        "{name}: CMP1 {}x{}, sample entry {}x{}",
                        header.width,
                        header.height,
                        dim(24),
                        dim(26)
                    ));
                }
                if header.planes != 4 || header.bits != 14 {
                    problems.push(format!(
                        "{name}: {} planes of {} bits",
                        header.planes, header.bits
                    ));
                }
                seen.insert(format!(
                    "v{} levels {} tiles {:?} cfa {}",
                    header.version,
                    header.levels,
                    header.tile_grid(),
                    header.cfa_layout
                ));
                // Find this track's single sample and check the tree.
                let Some(sample) = sample_of(&bytes, stbl) else {
                    problems.push(format!("{name}: no sample table"));
                    continue;
                };
                match parse_tiles(&header, sample) {
                    Err(e) => {
                        problems.push(format!("{name}: {}x{}: {e}", header.width, header.height))
                    }
                    Ok(tiles) => {
                        checked += 1;
                        // Tiles must fill the sample after the header,
                        // planes must fill their tile and bands their
                        // plane, with nothing left over.
                        let mut at = header.mdat_header_size;
                        for tile in &tiles {
                            if tile.data.start != at {
                                problems
                                    .push(format!("{name}: tile at {} not {at}", tile.data.start));
                            }
                            at = tile.data.end;
                            let mut plane_at = tile.data.start + tile.qp_table_size;
                            for plane in &tile.planes {
                                if plane.data.start != plane_at {
                                    problems.push(format!(
                                        "{name}: plane at {} not {plane_at}",
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
                            // A version-2 tile pads its end to a word
                            // or two; a version-1 one is exact.
                            let slack = tile.data.end - plane_at;
                            if slack > if header.version >= 2 { 7 } else { 0 } {
                                problems.push(format!("{name}: planes leave {slack} bytes"));
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
                            .all(|b| b.q_step == 32);
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
}
