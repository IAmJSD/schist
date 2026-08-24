//! CPU tile compositor — the reference implementation of Schist's
//! rendering semantics. Any other backend must match it tile-for-tile;
//! `schist-compositor-gpu` is held to that with parity tests.
//!
//! The public `composite_*` functions dispatch through the active
//! [`backend`] (CPU unless the app installs the GPU compositor), so every
//! caller — canvas, exports, tools — picks up acceleration without
//! knowing about it. The `*_cpu` variants are the reference
//! implementation and always run on the CPU.
//!
//! Composites the layer tree bottom-up per 256×256 tile:
//! groups isolate unless pass-through, layer masks multiply source alpha,
//! clipping layers are confined to their base layer's alpha, and adjustment
//! layers re-colour the backdrop beneath them (mask- and clip-aware).

pub mod viewport;

use rayon::prelude::*;
use rustc_hash::FxHashMap;
use schist_adjustments::Params;
use schist_color::Rgba;
use schist_core::{
    AdjustmentData, BlendMode, Document, IntRect, Layer, LayerKind, TileCoord, TILE_PIXELS,
    TILE_SIZE,
};
use schist_pixel_ops::blend_pixel;
use std::sync::{Arc, OnceLock, RwLock};

type TileF32 = Vec<f32>; // TILE_PIXELS * 4, straight-alpha RGBA

fn blank_tile() -> TileF32 {
    vec![0.0; TILE_PIXELS * 4]
}

/// Reusable tile buffers.
///
/// Compositing a tile needs one scratch buffer per nested layer/group, and
/// each is a megabyte. Allocating and zeroing them per tile dominated the
/// profile, so buffers are pooled per compositing call instead.
#[derive(Default)]
struct Scratch {
    free: Vec<TileF32>,
}

impl Scratch {
    fn take(&mut self) -> TileF32 {
        match self.free.pop() {
            Some(mut buf) => {
                buf.fill(0.0);
                buf
            }
            None => blank_tile(),
        }
    }

    fn give(&mut self, buf: TileF32) {
        // A handful is plenty: nesting depth, not tile count, sets demand.
        if self.free.len() < 8 {
            self.free.push(buf);
        }
    }
}

/// The rendering backend seam.
///
/// [`CpuCompositor`] is the reference; `schist-compositor-gpu` provides a
/// wgpu implementation that the app installs with [`set_backend`]. The
/// default methods are the CPU reference, so a backend only overrides
/// what it accelerates — and a backend that cannot express a document
/// (unsupported feature, no adapter) is expected to fall back to the
/// `*_cpu` functions itself, never to return something different.
pub trait Compositor: Send + Sync {
    /// Short name for logs and the UI ("cpu", "gpu").
    fn name(&self) -> &'static str;

    /// Composite one tile to straight-alpha f32 RGBA.
    fn tile(&self, doc: &Document, coord: TileCoord) -> Vec<f32>;

    /// Composite several tiles to RGBA8 (straight alpha), one buffer per
    /// coord in order. The batch form is where a GPU backend earns its
    /// keep: one upload, one dispatch, one readback.
    fn tiles_rgba8(&self, doc: &Document, coords: &[TileCoord]) -> Vec<Vec<u8>> {
        coords
            .par_iter()
            .map(|&c| {
                let f = self.tile(doc, c);
                let mut bytes = vec![0u8; TILE_PIXELS * 4];
                for (b, v) in bytes.iter_mut().zip(f.iter()) {
                    *b = (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
                }
                bytes
            })
            .collect()
    }

    /// Composite a region to straight-alpha f32 RGBA.
    fn region_f32(&self, doc: &Document, region: IntRect) -> Vec<f32> {
        composite_region_f32_cpu(doc, region)
    }

    /// Composite a region to straight-alpha RGBA8.
    fn region_rgba8(&self, doc: &Document, region: IntRect) -> Vec<u8> {
        composite_region_rgba8_cpu(doc, region)
    }

    /// Resample already-composited display tiles into a viewport image
    /// (see [`viewport`]). `None` means "not accelerated here" and the
    /// caller runs [`viewport::render_viewport_cpu`].
    fn viewport(
        &self,
        params: &viewport::ViewportParams,
        grid: &[Option<Arc<Vec<u8>>>],
    ) -> Option<Vec<u8>> {
        let _ = (params, grid);
        None
    }
}

/// Reference implementation: tiles composited on the CPU, parallel across
/// tiles, with the blend semantics every other backend must match.
#[derive(Debug, Clone, Copy, Default)]
pub struct CpuCompositor;

impl Compositor for CpuCompositor {
    fn name(&self) -> &'static str {
        "cpu"
    }

    fn tile(&self, doc: &Document, coord: TileCoord) -> Vec<f32> {
        composite_tile_cpu(doc, coord)
    }
}

/// The active rendering backend. CPU until [`set_backend`] installs
/// something else.
static BACKEND: OnceLock<RwLock<Arc<dyn Compositor>>> = OnceLock::new();

fn backend_cell() -> &'static RwLock<Arc<dyn Compositor>> {
    BACKEND.get_or_init(|| RwLock::new(Arc::new(CpuCompositor)))
}

/// Install the backend the `composite_*` dispatchers use.
pub fn set_backend(backend: Arc<dyn Compositor>) {
    *backend_cell().write().unwrap() = backend;
}

/// The currently active backend.
pub fn backend() -> Arc<dyn Compositor> {
    backend_cell().read().unwrap().clone()
}

/// Composite one document tile to straight-alpha f32 RGBA on the active
/// backend.
pub fn composite_tile(doc: &Document, coord: TileCoord) -> TileF32 {
    backend().tile(doc, coord)
}

/// Composite a region to straight-alpha f32 RGBA on the active backend.
pub fn composite_region_f32(doc: &Document, region: IntRect) -> Vec<f32> {
    backend().region_f32(doc, region)
}

/// Composite a region to straight-alpha RGBA8 on the active backend.
pub fn composite_region_rgba8(doc: &Document, region: IntRect) -> Vec<u8> {
    backend().region_rgba8(doc, region)
}

/// Composite one document tile to straight-alpha f32 RGBA (CPU reference).
pub fn composite_tile_cpu(doc: &Document, coord: TileCoord) -> TileF32 {
    let mut scratch = Scratch::default();
    let mut dst = blank_tile();
    composite_layers(doc, &doc.tree.layers, coord, &mut dst, &mut scratch);
    dst
}

/// Composite an arbitrary document-space region to straight-alpha f32 RGBA,
/// tightly packed `region.width() * region.height() * 4` floats. Used where
/// full precision matters (16/32-bit export, PSD merged-image data).
/// CPU reference.
pub fn composite_region_f32_cpu(doc: &Document, region: IntRect) -> Vec<f32> {
    let w = region.width() as usize;
    let h = region.height() as usize;
    let mut out = vec![0.0f32; w * h * 4];
    let coords: Vec<TileCoord> = TileCoord::covering(&region).collect();
    let tiles: Vec<(TileCoord, TileF32)> = coords
        .into_par_iter()
        .map(|c| (c, composite_tile_cpu(doc, c)))
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
                out[d..d + 4].copy_from_slice(&tile[s..s + 4]);
            }
        }
    }
    out
}

/// Composite an arbitrary document-space region to RGBA8 (straight alpha),
/// tightly packed `region.width() * region.height() * 4` bytes.
/// CPU reference.
pub fn composite_region_rgba8_cpu(doc: &Document, region: IntRect) -> Vec<u8> {
    let w = region.width() as usize;
    let h = region.height() as usize;
    let mut out = vec![0u8; w * h * 4];
    let coords: Vec<TileCoord> = TileCoord::covering(&region).collect();
    let tiles: Vec<(TileCoord, TileF32)> = coords
        .into_par_iter()
        .map(|c| (c, composite_tile_cpu(doc, c)))
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

/// The styled raster for a layer that carries effects — any layer kind.
///
/// Rasters feed the fx renderer their own pixels. Groups are flattened
/// here first — `schist_layer_fx` cannot composite, which is why every
/// restyle pass routes through this function instead of calling it
/// directly. Restyle bottom-up: a styled group's children must already
/// carry their own styled rasters when the group is flattened.
pub fn render_styled(layer: &Layer) -> Option<schist_core::StyledRaster> {
    if layer.style.is_empty() {
        return None;
    }
    let LayerKind::Group(g) = &layer.kind else {
        return schist_layer_fx::render(layer);
    };
    // Content bounds from the children (not `tight_bounds`, which would
    // read a stale styled raster from a previous restyle).
    let mut region = IntRect::EMPTY;
    for child in &g.children {
        if child.visible {
            region = region.union(&child.tight_bounds());
        }
    }
    if region.is_empty() {
        return None;
    }
    // Flatten the children alone — the group's own mask, opacity and
    // blend stay with the compositor, which applies them when it blends
    // the styled raster.
    let dummy = Document::new("", 1, 1, schist_color::Depth::Eight);
    let w = region.width() as usize;
    let h = region.height() as usize;
    let mut flat = vec![0.0f32; w * h * 4];
    let coords: Vec<TileCoord> = TileCoord::covering(&region).collect();
    let tiles: Vec<(TileCoord, TileF32)> = coords
        .into_par_iter()
        .map(|c| {
            let mut scratch = Scratch::default();
            let mut dst = blank_tile();
            composite_layers(&dummy, &g.children, c, &mut dst, &mut scratch);
            (c, dst)
        })
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
                flat[d..d + 4].copy_from_slice(&tile[s..s + 4]);
            }
        }
    }
    schist_layer_fx::render_content(
        region,
        |x, y| {
            if x < region.left || x >= region.right || y < region.top || y >= region.bottom {
                return Rgba::TRANSPARENT;
            }
            let i = ((y - region.top) as usize * w + (x - region.left) as usize) * 4;
            Rgba::new(flat[i], flat[i + 1], flat[i + 2], flat[i + 3])
        },
        &layer.style,
        layer.fill_opacity,
    )
}

/// Composite a run of sibling layers (bottom-to-top) onto `dst` for `coord`.
fn composite_layers(
    doc: &Document,
    layers: &[Layer],
    coord: TileCoord,
    dst: &mut TileF32,
    scratch: &mut Scratch,
) {
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
            let mut group_buf = scratch.take();
            render_single_layer(doc, layer, coord, &mut group_buf, 1.0, scratch);
            let base_alpha: Vec<f32> = group_buf.as_chunks::<4>().0.iter().map(|p| p[3]).collect();
            for clip_layer in &layers[i + 1..clip_end] {
                if !clip_layer.visible {
                    continue;
                }
                // A clipped adjustment recolours only the base layer's
                // pixels, which is exactly the buffer we just built.
                if let LayerKind::Adjustment(data) = &clip_layer.kind {
                    apply_adjustment(clip_layer, data, coord, &mut group_buf, Some(&base_alpha));
                    continue;
                }
                let mut src = scratch.take();
                render_single_layer(doc, clip_layer, coord, &mut src, 1.0, scratch);
                let opacity = clip_layer.opacity * content_alpha(clip_layer);
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
                scratch.give(src);
            }
            blend_buf_onto(
                layer.blend,
                &group_buf,
                dst,
                coord,
                layer.opacity * content_alpha(layer),
                layer,
                doc,
            );
            scratch.give(group_buf);
        } else {
            match &layer.kind {
                // A group with effects has been flattened and styled by
                // `render_styled`; composite the styled raster like a
                // raster layer (effects force isolation, so PassThrough
                // does not apply).
                LayerKind::Group(_) if layer.styled.is_some() => {
                    let mut src = scratch.take();
                    render_single_layer(doc, layer, coord, &mut src, 1.0, scratch);
                    let mode = if layer.blend == BlendMode::PassThrough {
                        BlendMode::Normal
                    } else {
                        layer.blend
                    };
                    blend_buf_onto(
                        mode,
                        &src,
                        dst,
                        coord,
                        layer.opacity * content_alpha(layer),
                        layer,
                        doc,
                    );
                    scratch.give(src);
                }
                LayerKind::Group(g) => {
                    let pass_through = layer.blend == BlendMode::PassThrough
                        && layer.opacity >= 1.0
                        && layer.mask.is_none();
                    if pass_through {
                        composite_layers(doc, &g.children, coord, dst, scratch);
                    } else {
                        let mut group_buf = scratch.take();
                        composite_layers(doc, &g.children, coord, &mut group_buf, scratch);
                        let mode = if layer.blend == BlendMode::PassThrough {
                            BlendMode::Normal
                        } else {
                            layer.blend
                        };
                        blend_buf_onto(mode, &group_buf, dst, coord, layer.opacity, layer, doc);
                        scratch.give(group_buf);
                    }
                }
                LayerKind::Raster(r) => {
                    // Skip layers with nothing in this tile entirely.
                    if layer.render_offset == (0, 0) && r.tiles.get(coord).is_none() {
                        i = clip_end;
                        continue;
                    }
                    let mut src = scratch.take();
                    render_single_layer(doc, layer, coord, &mut src, 1.0, scratch);
                    blend_buf_onto(
                        layer.blend,
                        &src,
                        dst,
                        coord,
                        layer.opacity * content_alpha(layer),
                        layer,
                        doc,
                    );
                    scratch.give(src);
                }
                LayerKind::Adjustment(data) => {
                    apply_adjustment(layer, data, coord, dst, None);
                }
            }
        }
        i = clip_end;
    }
}

/// Resolve an adjustment layer's parameters: our canonical JSON when the
/// user has edited it, otherwise the preserved PSD payload.
fn resolve_params(data: &AdjustmentData) -> Params {
    if let Some(json) = &data.params_json {
        match serde_json::from_str::<Params>(json) {
            Ok(p) => return p,
            Err(err) => log::warn!("adjustment params unreadable: {err}"),
        }
    }
    schist_adjustments::parse_psd(data.kind, &data.raw)
}

/// Apply an adjustment layer to the backdrop already accumulated in `dst`.
///
/// `confine` optionally limits the effect to a per-pixel alpha (used when
/// the adjustment is clipped to the layer below).
fn apply_adjustment(
    layer: &Layer,
    data: &AdjustmentData,
    coord: TileCoord,
    dst: &mut TileF32,
    confine: Option<&[f32]>,
) {
    // Compile once per tile: turns per-pixel spline evaluation into a
    // table lookup for levels/curves/brightness and friends.
    let params = resolve_params(data).prepare();
    if params.is_identity() {
        return;
    }
    let mask = layer.mask.as_ref().filter(|m| m.enabled);
    let opacity = (layer.opacity * content_alpha(layer)).clamp(0.0, 1.0);
    if opacity <= 0.0 {
        return;
    }
    let trect = coord.rect();
    let is_fill = params.is_fill();
    for p in 0..TILE_PIXELS {
        let x = trect.left + (p as i32 % TILE_SIZE);
        let y = trect.top + (p as i32 / TILE_SIZE);
        let mut weight = opacity;
        if let Some(m) = mask {
            weight *= m.value(x, y) as f32 / 255.0;
        }
        if let Some(alpha) = confine {
            weight *= alpha[p];
        }
        if weight <= 0.0 {
            continue;
        }
        let d = &mut dst[p * 4..p * 4 + 4];
        let base = Rgba::new(d[0], d[1], d[2], d[3]);
        if base.a <= 0.0 && !is_fill {
            continue; // nothing beneath to adjust
        }
        let out = if let Some(src) = params.fill_color() {
            // Fill layers paint their colour rather than transforming the
            // backdrop, so they composite like a solid source.
            blend_pixel(layer.blend, Rgba { a: weight, ..src }, base, x, y)
        } else {
            let adjusted = params.apply(base);
            Rgba {
                r: base.r + (adjusted.r - base.r) * weight,
                g: base.g + (adjusted.g - base.g) * weight,
                b: base.b + (adjusted.b - base.b) * weight,
                a: base.a,
            }
        };
        d[0] = out.r;
        d[1] = out.g;
        d[2] = out.b;
        d[3] = out.a;
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
    scratch: &mut Scratch,
) {
    // A layer with effects composites its styled raster instead of its
    // own pixels: the fx renderer has already folded in fill opacity and
    // drawn the shadows and glows around it.
    if let Some(styled) = layer.styled.as_ref() {
        match layer.render_offset {
            (0, 0) => {
                if let Some(tile) = styled.tiles.get(coord) {
                    tile.decode_f32(buf);
                }
            }
            (dx, dy) => {
                let trect = coord.rect();
                for p in 0..TILE_PIXELS {
                    let x = trect.left + (p as i32 % TILE_SIZE) - dx;
                    let y = trect.top + (p as i32 / TILE_SIZE) - dy;
                    let px = styled.tiles.pixel(x, y);
                    if px.a <= 0.0 {
                        continue;
                    }
                    buf[p * 4] = px.r;
                    buf[p * 4 + 1] = px.g;
                    buf[p * 4 + 2] = px.b;
                    buf[p * 4 + 3] = px.a;
                }
            }
        }
        apply_mask_and_alpha(layer, coord, buf, alpha_scale);
        return;
    }
    match &layer.kind {
        LayerKind::Raster(raster) => match layer.render_offset {
            (0, 0) => {
                if let Some(tile) = raster.tiles.get(coord) {
                    tile.decode_f32(buf);
                }
            }
            // A layer being dragged is sampled through its offset instead
            // of having a megabyte of tiles rewritten per mouse event.
            (dx, dy) => {
                let trect = coord.rect();
                for p in 0..TILE_PIXELS {
                    let x = trect.left + (p as i32 % TILE_SIZE) - dx;
                    let y = trect.top + (p as i32 / TILE_SIZE) - dy;
                    let px = raster.tiles.pixel(x, y);
                    if px.a <= 0.0 {
                        continue;
                    }
                    buf[p * 4] = px.r;
                    buf[p * 4 + 1] = px.g;
                    buf[p * 4 + 2] = px.b;
                    buf[p * 4 + 3] = px.a;
                }
            }
        },
        LayerKind::Group(g) => {
            composite_layers(doc, &g.children, coord, buf, scratch);
        }
        LayerKind::Adjustment(_) => {}
    }
    apply_mask_and_alpha(layer, coord, buf, alpha_scale);
}

/// Scale a rendered tile's alpha by the layer mask and any extra factor.
fn apply_mask_and_alpha(layer: &Layer, coord: TileCoord, buf: &mut TileF32, alpha_scale: f32) {
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
    // A styled group went through `render_single_layer`, which already
    // applied the mask; re-applying it here would double it.
    let group_mask = match (&layer.kind, layer.mask.as_ref().filter(|m| m.enabled)) {
        (LayerKind::Group(_), Some(m)) if layer.styled.is_none() => Some(m),
        _ => None,
    };
    // With no mask, separable blend modes run through a span loop that
    // dispatches the mode once instead of per pixel.
    if group_mask.is_none()
        && mode != BlendMode::Normal
        && schist_pixel_ops::blend_span(mode, src, dst, opacity)
    {
        return;
    }
    // Normal blending with no mask and full opacity is plain source-over,
    // which skips the whole generic blend path.
    if mode == BlendMode::Normal && group_mask.is_none() && opacity >= 1.0 {
        for p in 0..TILE_PIXELS {
            let s = &src[p * 4..p * 4 + 4];
            if s[3] <= 0.0 {
                continue;
            }
            let d = &mut dst[p * 4..p * 4 + 4];
            if s[3] >= 1.0 {
                d.copy_from_slice(s);
                continue;
            }
            let out = Rgba::new(s[0], s[1], s[2], s[3]).over(Rgba::new(d[0], d[1], d[2], d[3]));
            d[0] = out.r;
            d[1] = out.g;
            d[2] = out.b;
            d[3] = out.a;
        }
        return;
    }
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
        let bytes = backend()
            .tiles_rgba8(doc, &[coord])
            .pop()
            .unwrap_or_else(|| vec![0u8; TILE_PIXELS * 4]);
        let arc = Arc::new(bytes);
        self.tiles.insert(coord, arc.clone());
        arc
    }

    /// Composite several tiles in one batch ahead of `get` calls.
    pub fn prewarm(&mut self, doc: &Document, coords: &[TileCoord]) {
        let missing: Vec<TileCoord> = coords
            .iter()
            .copied()
            .filter(|c| !self.tiles.contains_key(c))
            .collect();
        if missing.is_empty() {
            return;
        }
        let computed = backend().tiles_rgba8(doc, &missing);
        for (c, bytes) in missing.into_iter().zip(computed) {
            self.tiles.insert(c, Arc::new(bytes));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use schist_color::Depth;
    use schist_core::{blit_rgba8, Layer, LayerMask, SelectOp};

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

#[cfg(test)]
mod adjustment_tests {
    use super::*;
    use schist_adjustments::Params;
    use schist_color::Depth;
    use schist_core::{blit_rgba8, AdjustmentData, AdjustmentKind, Layer, LayerMask};

    fn solid(name: &str, rect: IntRect, rgba: [u8; 4]) -> Layer {
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

    fn adjustment(kind: AdjustmentKind, params: Params) -> Layer {
        let mut layer = Layer::new_raster("adj");
        layer.kind = LayerKind::Adjustment(AdjustmentData {
            kind,
            raw: Vec::new(),
            params_json: Some(serde_json::to_string(&params).unwrap()),
        });
        layer
    }

    fn px(doc: &Document, x: i32, y: i32) -> [u8; 4] {
        let out = composite_region_rgba8(doc, IntRect::from_xywh(x, y, 1, 1));
        [out[0], out[1], out[2], out[3]]
    }

    fn doc_with_gray() -> Document {
        let mut doc = Document::new("t", 64, 64, Depth::Eight);
        doc.push_layer(solid(
            "bg",
            IntRect::from_size(64, 64),
            [128, 128, 128, 255],
        ));
        doc
    }

    #[test]
    fn invert_adjustment_affects_layers_below() {
        let mut doc = doc_with_gray();
        doc.push_layer(adjustment(AdjustmentKind::Invert, Params::Invert));
        let p = px(&doc, 10, 10);
        assert!((p[0] as i32 - 127).abs() <= 2, "inverted mid-grey: {p:?}");

        let mut doc2 = Document::new("t", 64, 64, Depth::Eight);
        doc2.push_layer(solid("bg", IntRect::from_size(64, 64), [200, 50, 0, 255]));
        doc2.push_layer(adjustment(AdjustmentKind::Invert, Params::Invert));
        assert_eq!(px(&doc2, 5, 5), [55, 205, 255, 255]);
    }

    #[test]
    fn hidden_adjustment_does_nothing() {
        let mut doc = doc_with_gray();
        let mut adj = adjustment(AdjustmentKind::Invert, Params::Invert);
        adj.visible = false;
        doc.push_layer(adj);
        assert_eq!(px(&doc, 10, 10), [128, 128, 128, 255]);
    }

    #[test]
    fn adjustment_opacity_blends_the_effect() {
        let mut doc = doc_with_gray();
        let mut adj = adjustment(AdjustmentKind::Invert, Params::Invert);
        adj.opacity = 0.5;
        doc.push_layer(adj);
        // Half-strength invert of mid-grey stays mid-grey.
        let p = px(&doc, 10, 10);
        assert!((p[0] as i32 - 128).abs() <= 2, "{p:?}");

        let mut doc = Document::new("t", 64, 64, Depth::Eight);
        doc.push_layer(solid("bg", IntRect::from_size(64, 64), [0, 0, 0, 255]));
        let mut adj = adjustment(AdjustmentKind::Invert, Params::Invert);
        adj.opacity = 0.25;
        doc.push_layer(adj);
        let p = px(&doc, 10, 10);
        assert!(
            (p[0] as i32 - 64).abs() <= 2,
            "quarter invert of black: {p:?}"
        );
    }

    #[test]
    fn adjustment_mask_limits_the_effect() {
        let mut doc = doc_with_gray();
        let mut adj = adjustment(AdjustmentKind::Invert, Params::Invert);
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
        adj.mask = Some(mask);
        doc.push_layer(adj);
        // The masked half inverts (127), the rest stays 128.
        assert!((px(&doc, 10, 10)[0] as i32 - 127).abs() <= 2);
        assert_eq!(px(&doc, 50, 10)[0], 128);
    }

    #[test]
    fn clipped_adjustment_only_touches_its_base_layer() {
        let mut doc = Document::new("t", 128, 64, Depth::Eight);
        doc.push_layer(solid(
            "bottom",
            IntRect::from_size(128, 64),
            [200, 200, 200, 255],
        ));
        // Base covers the left half only.
        doc.push_layer(solid(
            "base",
            IntRect::from_xywh(0, 0, 64, 64),
            [100, 100, 100, 255],
        ));
        let mut adj = adjustment(AdjustmentKind::Invert, Params::Invert);
        adj.clipping = true;
        doc.push_layer(adj);

        assert_eq!(px(&doc, 10, 10)[0], 155, "base inverted (100 -> 155)");
        assert_eq!(
            px(&doc, 100, 10)[0],
            200,
            "layer outside the base untouched"
        );
    }

    #[test]
    fn adjustment_does_not_paint_on_transparency() {
        let mut doc = Document::new("t", 64, 64, Depth::Eight);
        doc.push_layer(Layer::new_raster("empty"));
        doc.push_layer(adjustment(AdjustmentKind::Invert, Params::Invert));
        assert_eq!(px(&doc, 10, 10)[3], 0, "still transparent");
    }

    #[test]
    fn solid_color_fill_layer_paints_its_color() {
        let mut doc = Document::new("t", 64, 64, Depth::Eight);
        doc.push_layer(Layer::new_raster("empty"));
        doc.push_layer(adjustment(
            AdjustmentKind::SolidColor,
            Params::SolidColor {
                rgba: [1.0, 0.0, 0.0, 1.0],
            },
        ));
        assert_eq!(
            px(&doc, 10, 10),
            [255, 0, 0, 255],
            "fill covers transparency"
        );
    }

    #[test]
    fn adjustment_inside_a_group_stays_in_the_group() {
        let mut doc = Document::new("t", 64, 64, Depth::Eight);
        doc.push_layer(solid("bg", IntRect::from_size(64, 64), [0, 0, 0, 255]));
        let mut group = Layer::new_group("g");
        group.blend = BlendMode::Normal; // isolated
        if let LayerKind::Group(g) = &mut group.kind {
            g.children.push(solid(
                "inner",
                IntRect::from_xywh(0, 0, 32, 64),
                [80, 80, 80, 255],
            ));
            g.children
                .push(adjustment(AdjustmentKind::Invert, Params::Invert));
        }
        doc.push_layer(group);
        assert_eq!(px(&doc, 10, 10)[0], 175, "inner layer inverted (80 -> 175)");
        assert_eq!(
            px(&doc, 50, 10)[0],
            0,
            "background outside the group intact"
        );
    }

    #[test]
    fn adjustment_falls_back_to_the_psd_payload() {
        let mut doc = doc_with_gray();
        let mut layer = Layer::new_raster("posterize");
        layer.kind = LayerKind::Adjustment(AdjustmentData {
            kind: AdjustmentKind::Threshold,
            raw: 200u16.to_be_bytes().to_vec(),
            params_json: None,
        });
        doc.push_layer(layer);
        // Threshold at 200/255 turns mid-grey black.
        assert_eq!(px(&doc, 10, 10)[0], 0);
    }
}

/// Fill opacity, unless the layer's effects renderer already applied it to
/// the styled raster -- applying it twice would double-fade the content
/// and wrongly fade the effects too.
fn content_alpha(layer: &Layer) -> f32 {
    if layer.styled.is_some() {
        1.0
    } else {
        layer.fill_opacity
    }
}
