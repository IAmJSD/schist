//! CPU tile compositor — the reference implementation of Photoslop's
//! rendering semantics (a future GPU compositor must match it, PLAN.md §2).
//!
//! Composites the layer tree bottom-up per 256×256 tile:
//! groups isolate unless pass-through, layer masks multiply source alpha,
//! clipping layers are confined to their base layer's alpha, adjustment
//! layers are currently pass-through (rendering lands in M8).

use photoslop_color::Rgba;
use photoslop_core::{
    BlendMode, Document, IntRect, Layer, LayerKind, TileCoord, TILE_PIXELS, TILE_SIZE,
};
use photoslop_pixel_ops::blend_pixel;
use rayon::prelude::*;
use rustc_hash::FxHashMap;
use std::sync::Arc;

type TileF32 = Vec<f32>; // TILE_PIXELS * 4, straight-alpha RGBA

fn blank_tile() -> TileF32 {
    vec![0.0; TILE_PIXELS * 4]
}

/// Composite one document tile to straight-alpha f32 RGBA.
pub fn composite_tile(doc: &Document, coord: TileCoord) -> TileF32 {
    let mut dst = blank_tile();
    composite_layers(doc, &doc.tree.layers, coord, &mut dst);
    dst
}

/// Composite an arbitrary document-space region to RGBA8 (straight alpha),
/// tightly packed `region.width() * region.height() * 4` bytes.
pub fn composite_region_rgba8(doc: &Document, region: IntRect) -> Vec<u8> {
    let w = region.width() as usize;
    let h = region.height() as usize;
    let mut out = vec![0u8; w * h * 4];
    let coords: Vec<TileCoord> = TileCoord::covering(&region).collect();
    let tiles: Vec<(TileCoord, TileF32)> = coords
        .into_par_iter()
        .map(|c| (c, composite_tile(doc, c)))
        .collect();
    for (coord, tile) in tiles {
        let trect = coord.rect();
        let clip = trect.intersect(&region);
        for y in clip.top..clip.bottom {
            let ly = (y - trect.top) as usize;
            let oy = (y - region.top) as usize;
            for x in clip.left..clip.right {
                let lx = (x - trect.left) as usize;
                let ox = (x - region.left) as usize;
                let s = (ly * TILE_SIZE as usize + lx) * 4;
                let d = (oy * w + ox) * 4;
                out[d] = (tile[s].clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
                out[d + 1] = (tile[s + 1].clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
                out[d + 2] = (tile[s + 2].clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
                out[d + 3] = (tile[s + 3].clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
            }
        }
    }
    out
}

/// Composite a run of sibling layers (bottom-to-top) onto `dst` for `coord`.
fn composite_layers(doc: &Document, layers: &[Layer], coord: TileCoord, dst: &mut TileF32) {
    let mut i = 0;
    while i < layers.len() {
        let layer = &layers[i];
        // Detect a clipping run: base layer followed by clipping layers.
        let mut clip_end = i + 1;
        if !layer.clipping {
            while clip_end < layers.len() && layers[clip_end].clipping {
                clip_end += 1;
            }
        }
        if !layer.visible {
            i = clip_end;
            continue;
        }
        if clip_end > i + 1 {
            // Base + clipped stack: build in isolation, confine clipped
            // layers to base alpha, then blend the lot with base's mode.
            let mut group_buf = blank_tile();
            render_single_layer(doc, layer, coord, &mut group_buf, 1.0);
            let base_alpha: Vec<f32> = group_buf.chunks_exact(4).map(|p| p[3]).collect();
            for clip_layer in &layers[i + 1..clip_end] {
                if !clip_layer.visible {
                    continue;
                }
                let mut src = blank_tile();
                render_single_layer(doc, clip_layer, coord, &mut src, 1.0);
                let opacity = clip_layer.opacity * clip_layer.fill_opacity;
                let trect = coord.rect();
                for p in 0..TILE_PIXELS {
                    let ba = base_alpha[p];
                    if ba <= 0.0 {
                        continue;
                    }
                    let s = &src[p * 4..p * 4 + 4];
                    let top = Rgba::new(s[0], s[1], s[2], s[3] * opacity * ba);
                    if top.a <= 0.0 && clip_layer.blend != BlendMode::Dissolve {
                        continue;
                    }
                    let d = &mut group_buf[p * 4..p * 4 + 4];
                    let bottom = Rgba::new(d[0], d[1], d[2], d[3]);
                    let x = trect.left + (p as i32 % TILE_SIZE);
                    let y = trect.top + (p as i32 / TILE_SIZE);
                    let out = blend_pixel(clip_layer.blend, top, bottom, x, y);
                    d[0] = out.r;
                    d[1] = out.g;
                    d[2] = out.b;
                    d[3] = out.a;
                }
            }
            blend_buf_onto(
                layer.blend,
                &group_buf,
                dst,
                coord,
                layer.opacity * layer.fill_opacity,
                layer,
                doc,
            );
        } else {
            match &layer.kind {
                LayerKind::Group(g) => {
                    let pass_through = layer.blend == BlendMode::PassThrough
                        && layer.opacity >= 1.0
                        && layer.mask.is_none();
                    if pass_through {
                        composite_layers(doc, &g.children, coord, dst);
                    } else {
                        let mut group_buf = blank_tile();
                        composite_layers(doc, &g.children, coord, &mut group_buf);
                        let mode = if layer.blend == BlendMode::PassThrough {
                            BlendMode::Normal
                        } else {
                            layer.blend
                        };
                        blend_buf_onto(mode, &group_buf, dst, coord, layer.opacity, layer, doc);
                    }
                }
                LayerKind::Raster(_) => {
                    let mut src = blank_tile();
                    render_single_layer(doc, layer, coord, &mut src, 1.0);
                    blend_buf_onto(
                        layer.blend,
                        &src,
                        dst,
                        coord,
                        layer.opacity * layer.fill_opacity,
                        layer,
                        doc,
                    );
                }
                LayerKind::Adjustment(_) => {
                    // Pass-through until adjustment rendering (M8).
                }
            }
        }
        i = clip_end;
    }
}

/// Render a single raster layer's pixels for `coord` into `buf`
/// (no blending with anything below; mask applied; opacity NOT applied —
/// callers apply it during the blend so clipped stacks work).
fn render_single_layer(
    doc: &Document,
    layer: &Layer,
    coord: TileCoord,
    buf: &mut TileF32,
    alpha_scale: f32,
) {
    match &layer.kind {
        LayerKind::Raster(raster) => {
            if let Some(tile) = raster.tiles.get(coord) {
                tile.decode_f32(buf);
            }
        }
        LayerKind::Group(g) => {
            composite_layers(doc, &g.children, coord, buf);
        }
        LayerKind::Adjustment(_) => {}
    }
    let mask = layer.mask.as_ref().filter(|m| m.enabled);
    if mask.is_none() && alpha_scale >= 1.0 {
        return;
    }
    let trect = coord.rect();
    for p in 0..TILE_PIXELS {
        let a = &mut buf[p * 4 + 3];
        if *a <= 0.0 {
            continue;
        }
        let mut scale = alpha_scale;
        if let Some(m) = mask {
            let x = trect.left + (p as i32 % TILE_SIZE);
            let y = trect.top + (p as i32 / TILE_SIZE);
            scale *= m.value(x, y) as f32 / 255.0;
        }
        *a *= scale;
    }
}

/// Blend `src` (already masked) onto `dst` with mode + opacity. The layer's
/// mask has been applied by `render_single_layer` for raster layers; for
/// isolated groups the mask must be applied here.
fn blend_buf_onto(
    mode: BlendMode,
    src: &TileF32,
    dst: &mut TileF32,
    coord: TileCoord,
    opacity: f32,
    layer: &Layer,
    _doc: &Document,
) {
    let trect = coord.rect();
    let group_mask = match (&layer.kind, layer.mask.as_ref().filter(|m| m.enabled)) {
        (LayerKind::Group(_), Some(m)) => Some(m),
        _ => None,
    };
    for p in 0..TILE_PIXELS {
        let s = &src[p * 4..p * 4 + 4];
        let x = trect.left + (p as i32 % TILE_SIZE);
        let y = trect.top + (p as i32 / TILE_SIZE);
        let mut a = s[3] * opacity;
        if let Some(m) = group_mask {
            a *= m.value(x, y) as f32 / 255.0;
        }
        if a <= 0.0 && mode != BlendMode::Dissolve {
            continue;
        }
        let top = Rgba::new(s[0], s[1], s[2], a);
        let d = &mut dst[p * 4..p * 4 + 4];
        let bottom = Rgba::new(d[0], d[1], d[2], d[3]);
        let out = blend_pixel(mode, top, bottom, x, y);
        d[0] = out.r;
        d[1] = out.g;
        d[2] = out.b;
        d[3] = out.a;
    }
}

/// Damage-driven cache of composited tiles (RGBA8 straight alpha), used by
/// the canvas view. Invalidate with document damage rects, then fetch.
#[derive(Default)]
pub struct TileCache {
    tiles: FxHashMap<TileCoord, Arc<Vec<u8>>>,
}

impl TileCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn invalidate(&mut self, rect: &IntRect) {
        if rect.is_empty() {
            return;
        }
        for coord in TileCoord::covering(rect) {
            self.tiles.remove(&coord);
        }
    }

    pub fn invalidate_all(&mut self) {
        self.tiles.clear();
    }

    /// Get (compositing on miss) the RGBA8 pixels for a tile.
    pub fn get(&mut self, doc: &Document, coord: TileCoord) -> Arc<Vec<u8>> {
        if let Some(t) = self.tiles.get(&coord) {
            return t.clone();
        }
        let f = composite_tile(doc, coord);
        let mut bytes = vec![0u8; TILE_PIXELS * 4];
        for (b, v) in bytes.iter_mut().zip(f.iter()) {
            *b = (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
        }
        let arc = Arc::new(bytes);
        self.tiles.insert(coord, arc.clone());
        arc
    }

    /// Composite several tiles in parallel ahead of `get` calls.
    pub fn prewarm(&mut self, doc: &Document, coords: &[TileCoord]) {
        let missing: Vec<TileCoord> = coords
            .iter()
            .copied()
            .filter(|c| !self.tiles.contains_key(c))
            .collect();
        let computed: Vec<(TileCoord, Vec<u8>)> = missing
            .into_par_iter()
            .map(|c| {
                let f = composite_tile(doc, c);
                let mut bytes = vec![0u8; TILE_PIXELS * 4];
                for (b, v) in bytes.iter_mut().zip(f.iter()) {
                    *b = (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
                }
                (c, bytes)
            })
            .collect();
        for (c, bytes) in computed {
            self.tiles.insert(c, Arc::new(bytes));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use photoslop_color::Depth;
    use photoslop_core::{blit_rgba8, Layer, LayerMask, SelectOp};

    fn solid_layer(name: &str, rect: IntRect, rgba: [u8; 4]) -> Layer {
        let mut layer = Layer::new_raster(name);
        let n = rect.width() as usize * rect.height() as usize;
        let buf: Vec<u8> = rgba.iter().cycle().take(n * 4).copied().collect();
        blit_rgba8(
            &mut layer.as_raster_mut().unwrap().tiles,
            Depth::Eight,
            rect,
            &buf,
        );
        layer
    }

    fn px(doc: &Document, x: i32, y: i32) -> [u8; 4] {
        let out = composite_region_rgba8(doc, IntRect::from_xywh(x, y, 1, 1));
        [out[0], out[1], out[2], out[3]]
    }

    #[test]
    fn single_opaque_layer() {
        let mut doc = Document::new("t", 64, 64, Depth::Eight);
        doc.push_layer(solid_layer(
            "red",
            IntRect::from_xywh(0, 0, 64, 64),
            [255, 0, 0, 255],
        ));
        assert_eq!(px(&doc, 10, 10), [255, 0, 0, 255]);
    }

    #[test]
    fn multiply_layers() {
        let mut doc = Document::new("t", 64, 64, Depth::Eight);
        doc.push_layer(solid_layer(
            "a",
            IntRect::from_xywh(0, 0, 64, 64),
            [128, 128, 128, 255],
        ));
        let mut top = solid_layer("b", IntRect::from_xywh(0, 0, 64, 64), [128, 128, 128, 255]);
        top.blend = BlendMode::Multiply;
        doc.push_layer(top);
        let p = px(&doc, 5, 5);
        // 128/255 * 128/255 ≈ 64.25/255
        assert!((p[0] as i32 - 64).abs() <= 1, "{p:?}");
    }

    #[test]
    fn half_opacity_over_white() {
        let mut doc = Document::new("t", 64, 64, Depth::Eight);
        doc.push_layer(solid_layer(
            "w",
            IntRect::from_xywh(0, 0, 64, 64),
            [255, 255, 255, 255],
        ));
        let mut top = solid_layer("b", IntRect::from_xywh(0, 0, 64, 64), [0, 0, 0, 255]);
        top.opacity = 0.5;
        doc.push_layer(top);
        let p = px(&doc, 5, 5);
        assert!((p[0] as i32 - 128).abs() <= 1, "{p:?}");
    }

    #[test]
    fn hidden_layer_skipped() {
        let mut doc = Document::new("t", 64, 64, Depth::Eight);
        doc.push_layer(solid_layer(
            "a",
            IntRect::from_xywh(0, 0, 64, 64),
            [0, 255, 0, 255],
        ));
        let mut top = solid_layer("b", IntRect::from_xywh(0, 0, 64, 64), [255, 0, 0, 255]);
        top.visible = false;
        doc.push_layer(top);
        assert_eq!(px(&doc, 1, 1), [0, 255, 0, 255]);
    }

    #[test]
    fn layer_mask_hides_pixels() {
        let mut doc = Document::new("t", 64, 64, Depth::Eight);
        doc.push_layer(solid_layer(
            "bg",
            IntRect::from_xywh(0, 0, 64, 64),
            [0, 0, 255, 255],
        ));
        let mut top = solid_layer("fg", IntRect::from_xywh(0, 0, 64, 64), [255, 0, 0, 255]);
        // Mask: reveal only left half.
        let mut mask = LayerMask::new_revealing();
        mask.default_value = 0;
        mask.bounds = IntRect::from_xywh(0, 0, 32, 64);
        for y in 0..64 {
            for x in 0..32 {
                let coord = TileCoord::containing(x, y);
                let buf = mask.tiles.get_mut_or_insert(coord);
                let lx = x.rem_euclid(TILE_SIZE) as usize;
                let ly = y.rem_euclid(TILE_SIZE) as usize;
                buf[ly * TILE_SIZE as usize + lx] = 255;
            }
        }
        top.mask = Some(mask);
        doc.push_layer(top);
        assert_eq!(px(&doc, 10, 10), [255, 0, 0, 255], "revealed side");
        assert_eq!(px(&doc, 50, 10), [0, 0, 255, 255], "masked side");
    }

    #[test]
    fn isolated_group_opacity() {
        // Group of [opaque red] at 50% group opacity over white:
        // must be pink (127-ish), not double-faded.
        let mut doc = Document::new("t", 64, 64, Depth::Eight);
        doc.push_layer(solid_layer(
            "bg",
            IntRect::from_xywh(0, 0, 64, 64),
            [255, 255, 255, 255],
        ));
        let mut group = Layer::new_group("g");
        group.opacity = 0.5;
        group.blend = BlendMode::Normal;
        if let LayerKind::Group(g) = &mut group.kind {
            g.children.push(solid_layer(
                "red",
                IntRect::from_xywh(0, 0, 64, 64),
                [255, 0, 0, 255],
            ));
        }
        doc.push_layer(group);
        let p = px(&doc, 5, 5);
        assert!(
            (p[0] as i32 - 255).abs() <= 1 && (p[1] as i32 - 128).abs() <= 1,
            "{p:?}"
        );
    }

    #[test]
    fn clipping_layer_confined_to_base_alpha() {
        let mut doc = Document::new("t", 128, 64, Depth::Eight);
        doc.push_layer(solid_layer(
            "bg",
            IntRect::from_xywh(0, 0, 128, 64),
            [255, 255, 255, 255],
        ));
        // Base occupies left half only.
        doc.push_layer(solid_layer(
            "base",
            IntRect::from_xywh(0, 0, 64, 64),
            [0, 0, 255, 255],
        ));
        // Clipped green covers everything but must show only over base.
        let mut clip = solid_layer("clip", IntRect::from_xywh(0, 0, 128, 64), [0, 255, 0, 255]);
        clip.clipping = true;
        doc.push_layer(clip);
        assert_eq!(px(&doc, 10, 10), [0, 255, 0, 255], "inside base");
        assert_eq!(px(&doc, 100, 10), [255, 255, 255, 255], "outside base");
    }

    #[test]
    fn cache_invalidation() {
        let mut doc = Document::new("t", 64, 64, Depth::Eight);
        let id = doc.push_layer(solid_layer(
            "a",
            IntRect::from_xywh(0, 0, 64, 64),
            [10, 20, 30, 255],
        ));
        let mut cache = TileCache::new();
        let coord = TileCoord { tx: 0, ty: 0 };
        let before = cache.get(&doc, coord);
        assert_eq!(&before[0..4], &[10, 20, 30, 255]);

        let mut edit = doc.begin_edit("paint");
        edit.writable_tile(id, coord).unwrap().set(0, Rgba::WHITE);
        edit.commit();
        for rect in doc.take_damage() {
            cache.invalidate(&rect);
        }
        let after = cache.get(&doc, coord);
        assert_eq!(&after[0..4], &[255, 255, 255, 255]);
    }

    #[test]
    fn selection_does_not_affect_composite() {
        let mut doc = Document::new("t", 64, 64, Depth::Eight);
        doc.push_layer(solid_layer(
            "a",
            IntRect::from_xywh(0, 0, 64, 64),
            [5, 6, 7, 255],
        ));
        doc.selection
            .select_rect(IntRect::from_xywh(0, 0, 8, 8), SelectOp::Replace);
        assert_eq!(px(&doc, 20, 20), [5, 6, 7, 255]);
    }
}
