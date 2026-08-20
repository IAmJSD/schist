//! Sparse, copy-on-write tile storage.
//!
//! All raster content in Photoslop — layer pixels, masks, selections — lives
//! in fixed-size square tiles. Tiles are reference-counted (`Arc`); cloning a
//! layer or snapshotting for undo shares tiles until a write actually happens
//! (`Arc::make_mut`). A missing tile in the map means "fully transparent".

use crate::geom::IntRect;
use photoslop_color::{f32_to_u16, f32_to_u8, u16_to_f32, u8_to_f32, Depth, Rgba};
use rustc_hash::FxHashMap;
use std::sync::Arc;

pub const TILE_SIZE: i32 = 256;
pub const TILE_PIXELS: usize = (TILE_SIZE as usize) * (TILE_SIZE as usize);

/// Tile coordinate in tile units (document pixel position / TILE_SIZE,
/// floor division so negative positions work).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TileCoord {
    pub tx: i32,
    pub ty: i32,
}

impl TileCoord {
    pub fn containing(x: i32, y: i32) -> TileCoord {
        TileCoord {
            tx: x.div_euclid(TILE_SIZE),
            ty: y.div_euclid(TILE_SIZE),
        }
    }

    /// Document-space rect covered by this tile.
    pub fn rect(&self) -> IntRect {
        IntRect::from_xywh(
            self.tx * TILE_SIZE,
            self.ty * TILE_SIZE,
            TILE_SIZE as u32,
            TILE_SIZE as u32,
        )
    }

    /// All tile coords intersecting a document-space rect.
    pub fn covering(rect: &IntRect) -> impl Iterator<Item = TileCoord> {
        let (l, t, r, b) = if rect.is_empty() {
            (0, 0, 0, 0)
        } else {
            (
                rect.left.div_euclid(TILE_SIZE),
                rect.top.div_euclid(TILE_SIZE),
                (rect.right - 1).div_euclid(TILE_SIZE) + 1,
                (rect.bottom - 1).div_euclid(TILE_SIZE) + 1,
            )
        };
        (t..b).flat_map(move |ty| (l..r).map(move |tx| TileCoord { tx, ty }))
    }
}

/// Interleaved RGBA pixel data for one tile, at the document's native depth.
#[derive(Debug, Clone, PartialEq)]
pub enum TileBuf {
    U8(Box<[u8]>),
    U16(Box<[u16]>),
    F32(Box<[f32]>),
}

impl TileBuf {
    pub fn new(depth: Depth) -> TileBuf {
        match depth {
            Depth::Eight => TileBuf::U8(vec![0u8; TILE_PIXELS * 4].into_boxed_slice()),
            Depth::Sixteen => TileBuf::U16(vec![0u16; TILE_PIXELS * 4].into_boxed_slice()),
            Depth::ThirtyTwo => TileBuf::F32(vec![0f32; TILE_PIXELS * 4].into_boxed_slice()),
        }
    }

    pub fn depth(&self) -> Depth {
        match self {
            TileBuf::U8(_) => Depth::Eight,
            TileBuf::U16(_) => Depth::Sixteen,
            TileBuf::F32(_) => Depth::ThirtyTwo,
        }
    }

    #[inline]
    pub fn get(&self, ix: usize) -> Rgba {
        let i = ix * 4;
        match self {
            TileBuf::U8(d) => Rgba::new(
                u8_to_f32(d[i]),
                u8_to_f32(d[i + 1]),
                u8_to_f32(d[i + 2]),
                u8_to_f32(d[i + 3]),
            ),
            TileBuf::U16(d) => Rgba::new(
                u16_to_f32(d[i]),
                u16_to_f32(d[i + 1]),
                u16_to_f32(d[i + 2]),
                u16_to_f32(d[i + 3]),
            ),
            TileBuf::F32(d) => Rgba::new(d[i], d[i + 1], d[i + 2], d[i + 3]),
        }
    }

    #[inline]
    pub fn set(&mut self, ix: usize, px: Rgba) {
        let i = ix * 4;
        match self {
            TileBuf::U8(d) => {
                let [r, g, b, a] = px.to_u8();
                d[i] = r;
                d[i + 1] = g;
                d[i + 2] = b;
                d[i + 3] = a;
            }
            TileBuf::U16(d) => {
                d[i] = f32_to_u16(px.r);
                d[i + 1] = f32_to_u16(px.g);
                d[i + 2] = f32_to_u16(px.b);
                d[i + 3] = f32_to_u16(px.a);
            }
            TileBuf::F32(d) => {
                d[i] = px.r;
                d[i + 1] = px.g;
                d[i + 2] = px.b;
                d[i + 3] = px.a;
            }
        }
    }

    /// Decode the whole tile into a straight-alpha f32 RGBA buffer
    /// (TILE_PIXELS * 4 floats).
    pub fn decode_f32(&self, out: &mut [f32]) {
        debug_assert_eq!(out.len(), TILE_PIXELS * 4);
        match self {
            TileBuf::U8(d) => {
                for (o, v) in out.iter_mut().zip(d.iter()) {
                    *o = u8_to_f32(*v);
                }
            }
            TileBuf::U16(d) => {
                for (o, v) in out.iter_mut().zip(d.iter()) {
                    *o = u16_to_f32(*v);
                }
            }
            TileBuf::F32(d) => out.copy_from_slice(d),
        }
    }

    /// Encode from a straight-alpha f32 RGBA buffer.
    pub fn encode_f32(&mut self, src: &[f32]) {
        debug_assert_eq!(src.len(), TILE_PIXELS * 4);
        match self {
            TileBuf::U8(d) => {
                for (o, v) in d.iter_mut().zip(src.iter()) {
                    *o = f32_to_u8(*v);
                }
            }
            TileBuf::U16(d) => {
                for (o, v) in d.iter_mut().zip(src.iter()) {
                    *o = f32_to_u16(*v);
                }
            }
            TileBuf::F32(d) => d.copy_from_slice(src),
        }
    }

    /// True if every pixel is fully transparent.
    pub fn is_blank(&self) -> bool {
        match self {
            TileBuf::U8(d) => d.chunks_exact(4).all(|p| p[3] == 0),
            TileBuf::U16(d) => d.chunks_exact(4).all(|p| p[3] == 0),
            TileBuf::F32(d) => d.chunks_exact(4).all(|p| p[3] == 0.0),
        }
    }
}

/// Sparse map of tiles. Missing tile == fully transparent.
#[derive(Debug, Clone, Default)]
pub struct TileMap {
    tiles: FxHashMap<TileCoord, Arc<TileBuf>>,
}

impl TileMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, coord: TileCoord) -> Option<&Arc<TileBuf>> {
        self.tiles.get(&coord)
    }

    /// Copy-on-write mutable access; creates a blank tile if absent.
    pub fn get_mut_or_insert(&mut self, coord: TileCoord, depth: Depth) -> &mut TileBuf {
        let arc = self
            .tiles
            .entry(coord)
            .or_insert_with(|| Arc::new(TileBuf::new(depth)));
        Arc::make_mut(arc)
    }

    pub fn insert(&mut self, coord: TileCoord, buf: Arc<TileBuf>) {
        self.tiles.insert(coord, buf);
    }

    pub fn remove(&mut self, coord: TileCoord) -> Option<Arc<TileBuf>> {
        self.tiles.remove(&coord)
    }

    pub fn snapshot(&self, coord: TileCoord) -> Option<Arc<TileBuf>> {
        self.tiles.get(&coord).cloned()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&TileCoord, &Arc<TileBuf>)> {
        self.tiles.iter()
    }

    pub fn coords(&self) -> impl Iterator<Item = TileCoord> + '_ {
        self.tiles.keys().copied()
    }

    pub fn len(&self) -> usize {
        self.tiles.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tiles.is_empty()
    }

    /// Bounding rect of all stored tiles in document space (conservative:
    /// tile granularity, includes blank-but-allocated tiles).
    pub fn tile_bounds(&self) -> IntRect {
        let mut bounds = IntRect::EMPTY;
        for coord in self.tiles.keys() {
            bounds = bounds.union(&coord.rect());
        }
        bounds
    }

    /// Pixel-tight bounds of non-transparent content (as opposed to the
    /// tile-granular `tile_bounds`). Used when writing PSD layer rects and
    /// when clamping resample lookups to the real edge of the artwork.
    pub fn content_bounds(&self) -> IntRect {
        let mut out = IntRect::EMPTY;
        for (coord, buf) in self.iter() {
            let trect = coord.rect();
            for ly in 0..TILE_SIZE {
                let mut first: Option<i32> = None;
                let mut last = 0;
                for lx in 0..TILE_SIZE {
                    if buf.get((ly * TILE_SIZE + lx) as usize).a > 0.0 {
                        first.get_or_insert(lx);
                        last = lx;
                    }
                }
                if let Some(first) = first {
                    out = out.union(&IntRect::new(
                        trect.left + first,
                        trect.top + ly,
                        trect.left + last + 1,
                        trect.top + ly + 1,
                    ));
                }
            }
        }
        out
    }

    /// Read a single pixel at document coordinates.
    pub fn pixel(&self, x: i32, y: i32) -> Rgba {
        let coord = TileCoord::containing(x, y);
        match self.tiles.get(&coord) {
            None => Rgba::TRANSPARENT,
            Some(buf) => {
                let lx = x.rem_euclid(TILE_SIZE) as usize;
                let ly = y.rem_euclid(TILE_SIZE) as usize;
                buf.get(ly * TILE_SIZE as usize + lx)
            }
        }
    }

    /// A cheap identity for the map's current contents.
    ///
    /// Tiles are copy-on-write behind an `Arc`, so the pointer identity of
    /// every tile plus its coordinate changes exactly when the pixels do.
    /// That makes this far cheaper than hashing megabytes of pixels, and
    /// it errs on the side of reporting a change.
    pub fn fingerprint(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut acc = 0u64;
        for (coord, buf) in self.tiles.iter() {
            let mut h = rustc_hash::FxHasher::default();
            coord.hash(&mut h);
            (Arc::as_ptr(buf) as usize).hash(&mut h);
            // XOR so the order the map iterates in does not matter.
            acc ^= h.finish();
        }
        acc
    }

    /// Drop tiles that have become fully transparent.
    pub fn prune_blank(&mut self) {
        self.tiles.retain(|_, buf| !buf.is_blank());
    }

    /// Lossless integer translation of the whole map. Tile-aligned moves
    /// re-key tiles; unaligned moves re-slice pixels across tiles.
    pub fn translated(&self, dx: i32, dy: i32, depth: photoslop_color::Depth) -> TileMap {
        if dx == 0 && dy == 0 {
            return self.clone();
        }
        let mut out = TileMap::new();
        if dx.rem_euclid(TILE_SIZE) == 0 && dy.rem_euclid(TILE_SIZE) == 0 {
            let (tdx, tdy) = (dx.div_euclid(TILE_SIZE), dy.div_euclid(TILE_SIZE));
            for (coord, buf) in self.iter() {
                out.insert(
                    TileCoord {
                        tx: coord.tx + tdx,
                        ty: coord.ty + tdy,
                    },
                    buf.clone(),
                );
            }
            return out;
        }
        for (coord, buf) in self.iter() {
            let src_rect = coord.rect();
            let dst_rect = src_rect.translated(dx, dy);
            for dst_coord in TileCoord::covering(&dst_rect) {
                let clip = dst_coord.rect().intersect(&dst_rect);
                if clip.is_empty() {
                    continue;
                }
                let dst = out.get_mut_or_insert(dst_coord, depth);
                let drect = dst_coord.rect();
                for y in clip.top..clip.bottom {
                    let sy = (y - dy - src_rect.top) as usize;
                    let ly = (y - drect.top) as usize;
                    for x in clip.left..clip.right {
                        let sx = (x - dx - src_rect.left) as usize;
                        let lx = (x - drect.left) as usize;
                        let px = buf.get(sy * TILE_SIZE as usize + sx);
                        dst.set(ly * TILE_SIZE as usize + lx, px);
                    }
                }
            }
        }
        out.prune_blank();
        out
    }
}

/// Sparse map of single-channel u8 tiles (masks, selections).
/// Missing tile == 0 coverage.
#[derive(Debug, Clone, Default)]
pub struct MaskTileMap {
    tiles: FxHashMap<TileCoord, Arc<[u8; TILE_PIXELS]>>,
}

impl MaskTileMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, coord: TileCoord) -> Option<&Arc<[u8; TILE_PIXELS]>> {
        self.tiles.get(&coord)
    }

    pub fn get_mut_or_insert(&mut self, coord: TileCoord) -> &mut [u8; TILE_PIXELS] {
        let arc = self
            .tiles
            .entry(coord)
            .or_insert_with(|| Arc::new([0u8; TILE_PIXELS]));
        Arc::make_mut(arc)
    }

    pub fn insert(&mut self, coord: TileCoord, buf: Arc<[u8; TILE_PIXELS]>) {
        self.tiles.insert(coord, buf);
    }

    pub fn iter(&self) -> impl Iterator<Item = (&TileCoord, &Arc<[u8; TILE_PIXELS]>)> {
        self.tiles.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.tiles.is_empty()
    }

    pub fn value(&self, x: i32, y: i32) -> u8 {
        let coord = TileCoord::containing(x, y);
        match self.tiles.get(&coord) {
            None => 0,
            Some(buf) => {
                let lx = x.rem_euclid(TILE_SIZE) as usize;
                let ly = y.rem_euclid(TILE_SIZE) as usize;
                buf[ly * TILE_SIZE as usize + lx]
            }
        }
    }

    pub fn tile_bounds(&self) -> IntRect {
        let mut bounds = IntRect::EMPTY;
        for coord in self.tiles.keys() {
            bounds = bounds.union(&coord.rect());
        }
        bounds
    }

    pub fn prune_blank(&mut self) {
        self.tiles.retain(|_, buf| buf.iter().any(|&v| v != 0));
    }

    /// Lossless integer translation (see `TileMap::translated`).
    pub fn translated(&self, dx: i32, dy: i32) -> MaskTileMap {
        if dx == 0 && dy == 0 {
            return self.clone();
        }
        let mut out = MaskTileMap::new();
        if dx.rem_euclid(TILE_SIZE) == 0 && dy.rem_euclid(TILE_SIZE) == 0 {
            let (tdx, tdy) = (dx.div_euclid(TILE_SIZE), dy.div_euclid(TILE_SIZE));
            for (coord, buf) in self.iter() {
                out.insert(
                    TileCoord {
                        tx: coord.tx + tdx,
                        ty: coord.ty + tdy,
                    },
                    buf.clone(),
                );
            }
            return out;
        }
        for (coord, buf) in self.iter() {
            let src_rect = coord.rect();
            let dst_rect = src_rect.translated(dx, dy);
            for dst_coord in TileCoord::covering(&dst_rect) {
                let clip = dst_coord.rect().intersect(&dst_rect);
                if clip.is_empty() {
                    continue;
                }
                let dst = out.get_mut_or_insert(dst_coord);
                let drect = dst_coord.rect();
                for y in clip.top..clip.bottom {
                    let sy = (y - dy - src_rect.top) as usize;
                    let ly = (y - drect.top) as usize;
                    for x in clip.left..clip.right {
                        let sx = (x - dx - src_rect.left) as usize;
                        let lx = (x - drect.left) as usize;
                        dst[ly * TILE_SIZE as usize + lx] = buf[sy * TILE_SIZE as usize + sx];
                    }
                }
            }
        }
        out.prune_blank();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tile_coord_negative_coordinates() {
        assert_eq!(TileCoord::containing(-1, -1), TileCoord { tx: -1, ty: -1 });
        assert_eq!(TileCoord::containing(0, 0), TileCoord { tx: 0, ty: 0 });
        assert_eq!(TileCoord::containing(255, 256), TileCoord { tx: 0, ty: 1 });
    }

    #[test]
    fn covering_spans_partial_tiles() {
        let rect = IntRect::from_xywh(-10, 0, 20, 10);
        let coords: Vec<_> = TileCoord::covering(&rect).collect();
        assert_eq!(
            coords,
            vec![TileCoord { tx: -1, ty: 0 }, TileCoord { tx: 0, ty: 0 }]
        );
    }

    #[test]
    fn covering_empty_rect_is_empty() {
        assert_eq!(TileCoord::covering(&IntRect::EMPTY).count(), 0);
    }

    #[test]
    fn cow_write_does_not_affect_snapshot() {
        let mut map = TileMap::new();
        let coord = TileCoord { tx: 0, ty: 0 };
        map.get_mut_or_insert(coord, Depth::Eight)
            .set(0, Rgba::WHITE);
        let snap = map.snapshot(coord).unwrap();
        map.get_mut_or_insert(coord, Depth::Eight)
            .set(0, Rgba::BLACK);
        assert_eq!(snap.get(0), Rgba::WHITE);
        assert_eq!(map.get(coord).unwrap().get(0), Rgba::BLACK);
    }

    #[test]
    fn pixel_reads_across_tiles() {
        let mut map = TileMap::new();
        let px = Rgba::new(0.5, 0.25, 0.75, 1.0);
        let coord = TileCoord::containing(300, 5);
        let lx = 300i32.rem_euclid(TILE_SIZE) as usize;
        map.get_mut_or_insert(coord, Depth::ThirtyTwo)
            .set(5 * TILE_SIZE as usize + lx, px);
        assert_eq!(map.pixel(300, 5), px);
        assert_eq!(map.pixel(299, 5), Rgba::TRANSPARENT);
    }
}
