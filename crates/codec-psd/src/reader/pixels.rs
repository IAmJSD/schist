//! Converting decompressed planar channel bytes into tile maps.

use schist_color::{Depth, Rgba};
use schist_core::{IntRect, MaskTileMap, TileCoord, TileMap, TILE_SIZE};

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
        for y in clip.top..clip.bottom {
            let sy = (y - rect.top) as usize;
            let ly = (y - trect.top) as usize;
            for x in clip.left..clip.right {
                let sx = (x - rect.left) as usize;
                let lx = (x - trect.left) as usize;
                buf[ly * TILE_SIZE as usize + lx] = bytes.get(sy * w + sx).copied().unwrap_or(0);
            }
        }
    }
}
