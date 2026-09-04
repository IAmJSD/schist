//! Compile a layer tree into the op program `composite.wgsl` interprets.
//!
//! The walk mirrors `schist_compositor::composite_layers` exactly — same
//! clip-run detection, same pass-through rules, same opacity products —
//! because the plan *is* the CPU compositor's control flow, flattened.
//! Anything the shader cannot express faithfully (mid-drag render
//! offsets, absurd nesting) returns [`Unsupported`] and the caller
//! composites on the CPU instead.

use schist_adjustments::{Params, Prepared};
use schist_core::{AdjustmentData, BlendMode, Document, Layer, LayerKind, MaskTileMap, TileMap};

/// Mirrors `MAX_DEPTH` in composite.wgsl: the deepest value stack a pixel
/// program may need (root + one level per isolated group/layer buffer).
pub const MAX_DEPTH: usize = 12;

/// Why a document can't run on the GPU. Not an error — the CPU reference
/// handles these; the variants exist so logs can say which feature bailed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unsupported {
    /// An adjustment kind the shader has no code path for.
    DirectAdjustment,
    /// A layer is mid-drag (`render_offset != 0`) and samples across tile
    /// boundaries.
    RenderOffset,
    /// Nesting deeper than the shader's fixed stack.
    TooDeep,
}

/// One source of per-tile data referenced by ops via a slot-table row.
pub enum PlanSource<'a> {
    Pixels(&'a TileMap),
    Mask(&'a MaskTileMap),
}

/// A mask reference: slot row plus the `LayerMask` semantics the shader
/// needs (bounds check, default outside).
#[derive(Debug, Clone, Copy)]
pub struct MaskRef {
    pub row: i32,
    pub bounds: [i32; 4],
    pub default_value: f32,
}

impl MaskRef {
    pub const NONE: MaskRef = MaskRef {
        row: -1,
        bounds: [0; 4],
        default_value: 1.0,
    };
}

/// One shader op. `src_fmt` is filled in at upload time (it depends on
/// which tile depths actually occur in the batch).
pub struct PlanOp {
    pub kind: u32,
    pub mode: u32,
    pub opacity: f32,
    pub flags: u32,
    pub src_ref: i32,
    pub mask: MaskRef,
    pub lut: i32,
    pub fill: [f32; 4],
    /// Which full-colour adjustment this op runs (`D_*`), or [`D_NONE`]
    /// when it is a LUT or a fill.
    pub direct: u32,
    /// Row into [`Plan::directs`] holding that adjustment's coefficients.
    pub dparams: i32,
}

pub const OP_PUSH_LAYER: u32 = 0;
pub const OP_PUSH_BLANK: u32 = 1;
pub const OP_BLEND: u32 = 2;
pub const OP_CLIP_BLEND: u32 = 3;
pub const OP_SNAPSHOT_ALPHA: u32 = 4;
pub const OP_ADJUST: u32 = 5;
pub const OP_MASK_TOP: u32 = 6;

pub const F_CONFINE: u32 = 1;
pub const F_FILL: u32 = 2;

// Full-colour adjustments: the ones that mix channels or step, so a
// per-channel LUT cannot express them.
pub const D_NONE: u32 = 0;
pub const D_HUE_SATURATION: u32 = 1;
pub const D_BLACK_WHITE: u32 = 2;
pub const D_THRESHOLD: u32 = 3;
pub const D_POSTERIZE: u32 = 4;

/// Floats per row of [`Plan::directs`] — six, the widest kind
/// (black & white's channel weights).
pub const DIRECT_STRIDE: usize = 6;

pub struct Plan<'a> {
    pub ops: Vec<PlanOp>,
    pub sources: Vec<PlanSource<'a>>,
    /// Concatenated 3×256 LUTs, 768 floats each.
    pub luts: Vec<f32>,
    /// Coefficients for the [`D_HUE_SATURATION`]-and-friends ops,
    /// [`DIRECT_STRIDE`] floats each.
    pub directs: Vec<f32>,
}

impl<'a> Plan<'a> {
    fn op(&mut self, kind: u32) -> &mut PlanOp {
        self.ops.push(PlanOp {
            kind,
            mode: mode_id(BlendMode::Normal),
            opacity: 1.0,
            flags: 0,
            src_ref: -1,
            mask: MaskRef::NONE,
            lut: -1,
            fill: [0.0; 4],
            direct: D_NONE,
            dparams: -1,
        });
        self.ops.last_mut().unwrap()
    }

    fn pixel_row(&mut self, tiles: &'a TileMap) -> i32 {
        self.sources.push(PlanSource::Pixels(tiles));
        (self.sources.len() - 1) as i32
    }

    fn direct_row(&mut self, coeffs: [f32; DIRECT_STRIDE]) -> i32 {
        self.directs.extend_from_slice(&coeffs);
        (self.directs.len() / DIRECT_STRIDE - 1) as i32
    }

    fn mask_ref(&mut self, layer: &'a Layer) -> MaskRef {
        let Some(mask) = layer.mask.as_ref().filter(|m| m.enabled) else {
            return MaskRef::NONE;
        };
        self.sources.push(PlanSource::Mask(&mask.tiles));
        MaskRef {
            row: (self.sources.len() - 1) as i32,
            bounds: [
                mask.bounds.left,
                mask.bounds.top,
                mask.bounds.right,
                mask.bounds.bottom,
            ],
            default_value: mask.default_value as f32 / 255.0,
        }
    }
}

/// `BlendMode` → the discriminant composite.wgsl matches on.
pub fn mode_id(mode: BlendMode) -> u32 {
    use BlendMode::*;
    match mode {
        PassThrough => 0,
        Normal => 1,
        Dissolve => 2,
        Darken => 3,
        Multiply => 4,
        ColorBurn => 5,
        LinearBurn => 6,
        DarkerColor => 7,
        Lighten => 8,
        Screen => 9,
        ColorDodge => 10,
        LinearDodge => 11,
        LighterColor => 12,
        Overlay => 13,
        SoftLight => 14,
        HardLight => 15,
        VividLight => 16,
        LinearLight => 17,
        PinLight => 18,
        HardMix => 19,
        Difference => 20,
        Exclusion => 21,
        Subtract => 22,
        Divide => 23,
        Hue => 24,
        Saturation => 25,
        Color => 26,
        Luminosity => 27,
    }
}

/// Fill opacity, unless the layer's effects renderer already applied it
/// (mirror of the compositor's `content_alpha`).
fn content_alpha(layer: &Layer) -> f32 {
    if layer.styled.is_some() {
        1.0
    } else {
        layer.fill_opacity
    }
}

/// Mirror of the compositor's `resolve_params`.
fn resolve_params(data: &AdjustmentData) -> Params {
    if let Some(json) = &data.params_json {
        if let Ok(p) = serde_json::from_str::<Params>(json) {
            return p;
        }
    }
    schist_adjustments::parse_psd(data.kind, &data.raw)
}

pub fn build(doc: &Document) -> Result<Plan<'_>, Unsupported> {
    let mut plan = Plan {
        ops: Vec::new(),
        sources: Vec::new(),
        luts: Vec::new(),
        directs: Vec::new(),
    };
    // Depth 1 = the root destination the shader starts with.
    emit_layers(&doc.tree.layers, &mut plan, 1)?;
    Ok(plan)
}

fn emit_layers<'a>(
    layers: &'a [Layer],
    plan: &mut Plan<'a>,
    depth: usize,
) -> Result<(), Unsupported> {
    let mut i = 0;
    while i < layers.len() {
        let layer = &layers[i];
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
            // layers to the base's alpha, then blend with the base's mode.
            emit_single(layer, plan, depth)?;
            plan.op(OP_SNAPSHOT_ALPHA);
            for clip_layer in &layers[i + 1..clip_end] {
                if !clip_layer.visible {
                    continue;
                }
                if let LayerKind::Adjustment(data) = &clip_layer.kind {
                    emit_adjust(clip_layer, data, plan, true)?;
                    continue;
                }
                emit_single(clip_layer, plan, depth + 1)?;
                let op = plan.op(OP_CLIP_BLEND);
                op.mode = mode_id(clip_layer.blend);
                op.opacity = clip_layer.opacity * content_alpha(clip_layer);
            }
            // `blend_buf_onto` re-applies the mask for group layers (on
            // top of `render_single_layer` having applied it); mirror
            // that, double application and all. Styled groups are the
            // exception on both sides: their mask is applied once, by
            // the styled-raster path.
            let mask = if matches!(layer.kind, LayerKind::Group(_)) && layer.styled.is_none() {
                plan.mask_ref(layer)
            } else {
                MaskRef::NONE
            };
            let op = plan.op(OP_BLEND);
            op.mode = mode_id(layer.blend);
            op.opacity = layer.opacity * content_alpha(layer);
            op.mask = mask;
        } else {
            match &layer.kind {
                // A styled group composites its flattened styled raster
                // (mask already applied by `emit_single`, mirroring the
                // CPU's `render_single_layer`).
                LayerKind::Group(_) if layer.styled.is_some() => {
                    emit_single(layer, plan, depth)?;
                    let mode = if layer.blend == BlendMode::PassThrough {
                        BlendMode::Normal
                    } else {
                        layer.blend
                    };
                    let op = plan.op(OP_BLEND);
                    op.mode = mode_id(mode);
                    op.opacity = layer.opacity * content_alpha(layer);
                }
                LayerKind::Group(g) => {
                    // Mirrors the CPU compositor: fill opacity is part of
                    // the pass-through test, or a group at fill 50% would
                    // render its children at full strength.
                    let pass_through = layer.blend == BlendMode::PassThrough
                        && layer.opacity >= 1.0
                        && content_alpha(layer) >= 1.0
                        && layer.mask.is_none();
                    if pass_through {
                        emit_layers(&g.children, plan, depth)?;
                    } else {
                        need_depth(depth + 1)?;
                        plan.op(OP_PUSH_BLANK);
                        emit_layers(&g.children, plan, depth + 1)?;
                        let mode = if layer.blend == BlendMode::PassThrough {
                            BlendMode::Normal
                        } else {
                            layer.blend
                        };
                        let mask = plan.mask_ref(layer);
                        let op = plan.op(OP_BLEND);
                        op.mode = mode_id(mode);
                        op.opacity = layer.opacity * content_alpha(layer);
                        op.mask = mask;
                    }
                }
                LayerKind::Raster(_) => {
                    emit_single(layer, plan, depth)?;
                    let mode = mode_id(layer.blend);
                    let opacity = layer.opacity * content_alpha(layer);
                    let op = plan.op(OP_BLEND);
                    op.mode = mode;
                    op.opacity = opacity;
                }
                LayerKind::Adjustment(data) => {
                    emit_adjust(layer, data, plan, false)?;
                }
            }
        }
        i = clip_end;
    }
    Ok(())
}

/// Mirror of `render_single_layer`: push the layer's own (masked, but not
/// yet opacity-scaled) pixels as a new stack entry.
fn emit_single<'a>(layer: &'a Layer, plan: &mut Plan<'a>, depth: usize) -> Result<(), Unsupported> {
    need_depth(depth + 1)?;
    if layer.render_offset != (0, 0) {
        return Err(Unsupported::RenderOffset);
    }
    // A layer with effects composites its styled raster instead of its
    // own pixels.
    if let Some(styled) = layer.styled.as_ref() {
        let src = plan.pixel_row(&styled.tiles);
        let mask = plan.mask_ref(layer);
        let op = plan.op(OP_PUSH_LAYER);
        op.src_ref = src;
        op.mask = mask;
        return Ok(());
    }
    match &layer.kind {
        LayerKind::Raster(raster) => {
            let src = plan.pixel_row(&raster.tiles);
            let mask = plan.mask_ref(layer);
            let op = plan.op(OP_PUSH_LAYER);
            op.src_ref = src;
            op.mask = mask;
        }
        LayerKind::Group(g) => {
            plan.op(OP_PUSH_BLANK);
            emit_layers(&g.children, plan, depth + 1)?;
            let mask = plan.mask_ref(layer);
            if mask.row >= 0 {
                let op = plan.op(OP_MASK_TOP);
                op.mask = mask;
            }
        }
        // An adjustment rendered "as a layer" (base of a clip run) has no
        // pixels of its own.
        LayerKind::Adjustment(_) => {
            plan.op(OP_PUSH_BLANK);
        }
    }
    Ok(())
}

/// Mirror of `apply_adjustment`, restricted to what the shader expresses:
/// identity (skip), fills, and LUT-compilable parameters. Everything else
/// is `Unsupported`.
fn emit_adjust<'a>(
    layer: &'a Layer,
    data: &AdjustmentData,
    plan: &mut Plan<'a>,
    confine: bool,
) -> Result<(), Unsupported> {
    let params = resolve_params(data).prepare();
    if params.is_identity() {
        return Ok(());
    }
    let opacity = (layer.opacity * content_alpha(layer)).clamp(0.0, 1.0);
    if opacity <= 0.0 {
        return Ok(());
    }
    let mut lut = -1;
    let mut flags = 0;
    let mut fill = [0.0f32; 4];
    let mut direct = D_NONE;
    let mut dparams = -1;
    match &params {
        Prepared::Identity => return Ok(()),
        Prepared::Fill(c) => {
            flags = F_FILL;
            fill = [c.r, c.g, c.b, c.a];
        }
        Prepared::Lut(table) => {
            lut = (plan.luts.len() / 768) as i32;
            for channel in table.iter() {
                plan.luts.extend_from_slice(channel);
            }
        }
        // Full-colour math: the shader has an explicit branch per kind,
        // fed the same coefficients `Params::apply` would compute.
        Prepared::Direct(p) => {
            let (kind, coeffs) = direct_coeffs(p).ok_or(Unsupported::DirectAdjustment)?;
            direct = kind;
            dparams = plan.direct_row(coeffs);
        }
    }
    let mask = plan.mask_ref(layer);
    let op = plan.op(OP_ADJUST);
    op.mode = mode_id(layer.blend);
    op.opacity = opacity;
    op.flags = flags | if confine { F_CONFINE } else { 0 };
    op.lut = lut;
    op.fill = fill;
    op.mask = mask;
    op.direct = direct;
    op.dparams = dparams;
    Ok(())
}

/// The shader op and coefficient row for a full-colour adjustment.
///
/// Everything that can be folded on the CPU is folded here — the /100
/// scalings — so the shader does the same arithmetic on the same
/// numbers `Params::apply` does, in the same order. `None` for a kind
/// the shader has no branch for.
fn direct_coeffs(params: &Params) -> Option<(u32, [f32; DIRECT_STRIDE])> {
    match params {
        // Per-range HSL tweaks need six trapezoids' worth of
        // coefficients, which don't fit the direct row — those
        // adjustments composite on the CPU.
        Params::HueSaturation { ranges, .. } if !ranges.is_empty() => None,
        Params::HueSaturation {
            hue,
            saturation,
            lightness,
            colorize,
            lightness_desaturates,
            reciprocal_saturation,
            ranges: _,
        } => Some((
            D_HUE_SATURATION,
            [
                *hue,
                saturation / 100.0,
                lightness / 100.0,
                if *colorize { 1.0 } else { 0.0 },
                // The two Affinity slider conventions; the shader
                // branches on them the way `Params::apply` does.
                if *lightness_desaturates { 1.0 } else { 0.0 },
                if *reciprocal_saturation { 1.0 } else { 0.0 },
            ],
        )),
        Params::BlackWhite {
            reds,
            yellows,
            greens,
            cyans,
            blues,
            magentas,
        } => Some((
            D_BLACK_WHITE,
            [
                reds / 100.0,
                yellows / 100.0,
                greens / 100.0,
                cyans / 100.0,
                blues / 100.0,
                magentas / 100.0,
            ],
        )),
        Params::Threshold { level } => Some((D_THRESHOLD, [*level, 0.0, 0.0, 0.0, 0.0, 0.0])),
        Params::Posterize { levels } => Some((
            D_POSTERIZE,
            [
                // The shader floors into `levels` bands and divides by
                // `levels - 1`, mirroring the CPU's convention.
                (*levels).clamp(2, 255) as f32,
                0.0,
                0.0,
                0.0,
                0.0,
                0.0,
            ],
        )),
        _ => None,
    }
}

fn need_depth(depth: usize) -> Result<(), Unsupported> {
    if depth >= MAX_DEPTH {
        Err(Unsupported::TooDeep)
    } else {
        Ok(())
    }
}
