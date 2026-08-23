//! Converting decompressed planar channel bytes into tile maps.

use schist_color::{Depth, Rgba};
use schist_core::{IntRect, MaskTileMap, TileBuf, TileCoord, TileMap, TILE_SIZE};

/// Convert one decompressed big-endian channel plane to normalized f32.
///
/// * 8-bit: `v / 255`
/// * 16-bit: big-endian u16, `v / 65535` (we normalize the file's full u16
///   range; Photoshop's internal 0..=32768 representation is not what's on
///   disk for the formats we read)
/// * 32-bit: big-endian IEEE f32, kept as-is (may exceed 1.0 for HDR)
pub fn plane_to_f32(bytes: &[u8], depth: Depth) -> Vec<f32> {
    match depth {
        Depth::Eight => bytes.iter().map(|&v| v as f32 / 255.0).collect(),
        Depth::Sixteen => bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_be_bytes([c[0], c[1]]) as f32 / 65535.0)
            .collect(),
        Depth::ThirtyTwo => bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| f32::from_be_bytes([c[0], c[1], c[2], c[3]]))
            .collect(),
    }
}

/// Planar f32 channels for one layer rect. A missing color plane reads as
/// 0.0; a missing alpha plane reads as fully opaque (PSD layers without an
/// alpha channel are opaque within their bounds).
#[derive(Default)]
pub struct ColorPlanes {
    pub r: Option<Vec<f32>>,
    pub g: Option<Vec<f32>>,
    pub b: Option<Vec<f32>>,
    pub a: Option<Vec<f32>>,
}

impl ColorPlanes {
    pub fn is_empty(&self) -> bool {
        self.r.is_none() && self.g.is_none() && self.b.is_none() && self.a.is_none()
    }

    #[inline]
    fn pixel(&self, i: usize) -> Rgba {
        let get = |p: &Option<Vec<f32>>, default: f32| {
            p.as_ref()
                .map_or(default, |v| v.get(i).copied().unwrap_or(default))
        };
        Rgba::new(
            get(&self.r, 0.0),
            get(&self.g, 0.0),
            get(&self.b, 0.0),
            get(&self.a, 1.0),
        )
    }
}

/// Write planar channel data (row-major over `rect`) into a tile map at the
/// document's native depth. Blank (fully transparent) tiles are pruned.
pub fn fill_tiles(tiles: &mut TileMap, depth: Depth, rect: IntRect, planes: &ColorPlanes) {
    if rect.is_empty() || planes.is_empty() {
        return;
    }
    let w = rect.width() as usize;
    for coord in TileCoord::covering(&rect) {
        let trect = coord.rect();
        let clip = trect.intersect(&rect);
        if clip.is_empty() {
            continue;
        }
        let buf = tiles.get_mut_or_insert(coord, depth);
        for y in clip.top..clip.bottom {
            let sy = (y - rect.top) as usize;
            let ly = (y - trect.top) as usize;
            for x in clip.left..clip.right {
                let sx = (x - rect.left) as usize;
                let lx = (x - trect.left) as usize;
                buf.set(ly * TILE_SIZE as usize + lx, planes.pixel(sy * w + sx));
            }
        }
    }
    tiles.prune_blank();
}

/// 8-bit fast path of [`fill_tiles`]: interleave raw `[r, g, b, a]` u8
/// channel planes straight into `U8` tiles, skipping the f32 round-trip
/// (which is the identity at this depth). Missing colour planes read as
/// 0 and a missing alpha plane as opaque, matching `ColorPlanes::pixel`;
/// so does a short plane past its end.
pub fn fill_tiles_u8(tiles: &mut TileMap, rect: IntRect, planes: [Option<&[u8]>; 4]) {
    if rect.is_empty() || planes.iter().all(|p| p.is_none()) {
        return;
    }
    let w = rect.width() as usize;
    let n = w * rect.height() as usize;
    fn plane<'p>(n: usize, p: Option<&'p [u8]>, default: u8) -> std::borrow::Cow<'p, [u8]> {
        match p {
            Some(s) if s.len() >= n => std::borrow::Cow::Borrowed(s),
            Some(s) => {
                let mut v = s.to_vec();
                v.resize(n, default);
                std::borrow::Cow::Owned(v)
            }
            None => std::borrow::Cow::Owned(vec![default; n]),
        }
    }
    let (r, g, b, a) = (
        plane(n, planes[0], 0),
        plane(n, planes[1], 0),
        plane(n, planes[2], 0),
        plane(n, planes[3], 0xFF),
    );
    for coord in TileCoord::covering(&rect) {
        let trect = coord.rect();
        let clip = trect.intersect(&rect);
        if clip.is_empty() {
            continue;
        }
        let TileBuf::U8(d) = tiles.get_mut_or_insert(coord, Depth::Eight) else {
            unreachable!("freshly inserted tile is U8");
        };
        let cols = (clip.right - clip.left) as usize;
        for y in clip.top..clip.bottom {
            let s0 = (y - rect.top) as usize * w + (clip.left - rect.left) as usize;
            let l0 =
                (y - trect.top) as usize * TILE_SIZE as usize + (clip.left - trect.left) as usize;
            let dst = &mut d[l0 * 4..(l0 + cols) * 4];
            for (i, px) in dst.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                *px = [r[s0 + i], g[s0 + i], b[s0 + i], a[s0 + i]];
            }
        }
    }
    tiles.prune_blank();
}

/// Write an 8-bit coverage plane (row-major over `rect`) into a mask tile
/// map. PSD mask channels are 8-bit regardless of document depth.
pub fn fill_mask_tiles(tiles: &mut MaskTileMap, rect: IntRect, bytes: &[u8]) {
    if rect.is_empty() {
        return;
    }
    let w = rect.width() as usize;
    for coord in TileCoord::covering(&rect) {
        let trect = coord.rect();
        let clip = trect.intersect(&rect);
        if clip.is_empty() {
            continue;
        }
        let buf = tiles.get_mut_or_insert(coord);
        let cols = (clip.right - clip.left) as usize;
        for y in clip.top..clip.bottom {
            let s0 = (y - rect.top) as usize * w + (clip.left - rect.left) as usize;
            let l0 =
                (y - trect.top) as usize * TILE_SIZE as usize + (clip.left - trect.left) as usize;
            if let Some(src) = bytes.get(s0..s0 + cols) {
                buf[l0..l0 + cols].copy_from_slice(src);
            } else {
                // Short plane (corrupt file): whatever exists, then zeros.
                for (i, dst) in buf[l0..l0 + cols].iter_mut().enumerate() {
                    *dst = bytes.get(s0 + i).copied().unwrap_or(0);
                }
            }
        }
    }
}
