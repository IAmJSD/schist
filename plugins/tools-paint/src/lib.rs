//! Painting tools: brush, pencil, eraser — plus the shared stroke engine.
//!
//! Photoshop opacity semantics: within one stroke, overlapping dabs
//! accumulate *coverage* (max, not sum), and the stroke composites onto the
//! pre-stroke pixels at `coverage × tool opacity`. So scribbling over the
//! same spot at 50% opacity stays 50%, but two separate strokes darken.

use rustc_hash::FxHashMap;
use schist_color::Rgba;
use schist_core::{
    Document, IntRect, LayerId, LayerKind, StrokeEdit, TileCoord, TileMap, TILE_SIZE,
};
use schist_plugin_api::{
    EditorState, OptionValue, Overlay, PluginManifest, PluginRegistry, PointerInput, ToolCtx,
    ToolOption, ToolPlugin,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaintMode {
    Brush,
    Pencil,
    Eraser,
    /// Clone stamp: copies pixels from an offset source point.
    Clone,
    /// Dodge lightens, Burn darkens, Sponge changes saturation.
    Dodge,
    Burn,
    Sponge,
    /// Blur softens under the brush, Sharpen does the opposite.
    Blur,
    Sharpen,
    /// Smudge drags colour along the stroke.
    Smudge,
    /// Healing brush: texture from an alt-clicked source, colour and
    /// lighting from the destination.
    Heal,
    /// Spot healing: no source point, it fills from the surroundings.
    SpotHeal,
    /// Paints back from the snapshot the document was opened with.
    HistoryBrush,
    /// Erases pixels that match the colour under the brush centre.
    BackgroundEraser,
}

/// Where a dab's colour comes from.
#[derive(Clone)]
enum Ink {
    Solid(Rgba),
    Erase,
    /// Sample the pre-stroke layer at a fixed offset.
    Clone {
        source: TileMap,
        dx: i32,
        dy: i32,
    },
    /// Tonal adjustment of whatever is already there.
    Tone(Tone),
    /// Convolve the pre-stroke pixels under the brush.
    Convolve {
        snapshot: TileMap,
        sharpen: bool,
    },
    /// Drag colour along the stroke. The carried colour lives on the
    /// stroke, not here, because it changes dab to dab.
    Smudge,
    /// Healing: `patch` holds this dab's replacement pixels, recomputed
    /// per dab in `prepare_heal`.
    Heal,
    /// Paint back from a snapshot of the whole layer.
    Restore {
        snapshot: TileMap,
    },
    /// Erase pixels close to `target` in colour.
    BackgroundErase {
        target: Rgba,
        tolerance: f32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tone {
    Dodge,
    Burn,
    Sponge,
}

/// Photoshop-style dodge/burn: scale toward white or black, weighted so
/// midtones move more than the extremes; sponge pulls colour toward or away
/// from its own luminance.
fn apply_tone(tone: Tone, px: Rgba, amount: f32) -> Rgba {
    let lum = 0.3 * px.r + 0.59 * px.g + 0.11 * px.b;
    match tone {
        Tone::Dodge => {
            let w = amount * (1.0 - (lum - 0.5).abs() * 0.8);
            Rgba {
                r: px.r + (1.0 - px.r) * w,
                g: px.g + (1.0 - px.g) * w,
                b: px.b + (1.0 - px.b) * w,
                a: px.a,
            }
        }
        Tone::Burn => {
            let w = amount * (1.0 - (lum - 0.5).abs() * 0.8);
            Rgba {
                r: px.r * (1.0 - w),
                g: px.g * (1.0 - w),
                b: px.b * (1.0 - w),
                a: px.a,
            }
        }
        Tone::Sponge => Rgba {
            r: px.r + (lum - px.r) * amount,
            g: px.g + (lum - px.g) * amount,
            b: px.b + (lum - px.b) * amount,
            a: px.a,
        },
    }
}

/// One in-progress stroke.
struct Stroke {
    edit: StrokeEdit,
    layer: LayerId,
    /// Accumulated dab coverage (0..=1) per touched pixel, keyed by tile.
    coverage: FxHashMap<TileCoord, Box<[f32]>>,
    last: (f32, f32),
    /// Leftover distance to the next dab from the previous segment.
    spacing_debt: f32,
    ink: Ink,
    opacity: f32,
    size: f32,
    hardness: f32,
    mode: PaintMode,
    /// The layer as it was when the stroke began. Tile maps are
    /// copy-on-write, so this is a handful of Arc clones, not a copy.
    snapshot: TileMap,
    /// Smudge: the colour the brush is currently dragging.
    carried: Option<Rgba>,
    /// Healing: replacement pixels for the dab being stamped, in document
    /// coordinates over `heal_rect`.
    heal: Vec<Rgba>,
    heal_rect: IntRect,
    /// Healing brush: offset from the alt-clicked source point.
    heal_offset: (i32, i32),
}

impl Stroke {
    fn begin(
        ctx: &mut ToolCtx,
        input: PointerInput,
        mode: PaintMode,
        ink: Ink,
        heal_offset: (i32, i32),
    ) -> Option<Stroke> {
        let layer = paintable_layer(ctx.doc)?;
        let mut stroke = Stroke {
            edit: StrokeEdit::new(match mode {
                PaintMode::Brush => "Brush",
                PaintMode::Pencil => "Pencil",
                PaintMode::Eraser => "Eraser",
                PaintMode::Clone => "Clone Stamp",
                PaintMode::Dodge => "Dodge",
                PaintMode::Burn => "Burn",
                PaintMode::Sponge => "Sponge",
                PaintMode::Blur => "Blur",
                PaintMode::Sharpen => "Sharpen",
                PaintMode::Smudge => "Smudge",
                PaintMode::Heal => "Healing Brush",
                PaintMode::SpotHeal => "Spot Healing Brush",
                PaintMode::HistoryBrush => "History Brush",
                PaintMode::BackgroundEraser => "Background Eraser",
            }),
            layer,
            coverage: FxHashMap::default(),
            last: (input.x, input.y),
            spacing_debt: 0.0,
            ink,
            opacity: ctx.state.tool_opacity,
            size: ctx.state.brush_size,
            hardness: if mode == PaintMode::Pencil {
                1.0
            } else {
                ctx.state.brush_hardness
            },
            mode,
            snapshot: ctx
                .doc
                .tree
                .find(layer)
                .and_then(|l| l.as_raster())
                .map(|r| r.tiles.clone())
                .unwrap_or_default(),
            carried: None,
            heal: Vec::new(),
            heal_rect: IntRect::EMPTY,
            heal_offset,
        };
        stroke.dab(ctx.doc, input.x, input.y, input.pressure);
        Some(stroke)
    }

    fn spacing(&self) -> f32 {
        (self.size * 0.15).max(1.0)
    }

    fn extend(&mut self, doc: &mut Document, x: f32, y: f32, pressure: f32) {
        let (lx, ly) = self.last;
        let dx = x - lx;
        let dy = y - ly;
        let dist = (dx * dx + dy * dy).sqrt();
        if dist <= f32::EPSILON {
            return;
        }
        let spacing = self.spacing();
        let mut t = self.spacing_debt;
        while t <= dist {
            let f = t / dist;
            self.dab(doc, lx + dx * f, ly + dy * f, pressure);
            t += spacing;
        }
        self.spacing_debt = t - dist;
        self.last = (x, y);
    }

    /// Stamp one dab: raise coverage, then re-composite affected pixels
    /// from their pre-stroke values.
    fn dab(&mut self, doc: &mut Document, cx: f32, cy: f32, pressure: f32) {
        let radius = (self.size / 2.0 * pressure.max(0.05)).max(0.5);
        let bounds = IntRect::new(
            (cx - radius).floor() as i32,
            (cy - radius).floor() as i32,
            (cx + radius).ceil() as i32 + 1,
            (cy + radius).ceil() as i32 + 1,
        );
        // Healing and smudging need to look at a whole dab's worth of
        // pixels before any of them can be written, so they compute their
        // replacement up front.
        match self.mode {
            PaintMode::Heal | PaintMode::SpotHeal => self.prepare_heal(bounds, cx, cy, radius),
            PaintMode::Smudge => self.prepare_smudge(doc, cx, cy, radius),
            _ => {}
        }
        // Hard inner radius; anti-aliased single-pixel rim even at
        // hardness 1 so pencil still gets its crisp-but-not-jagged edge.
        let inner = radius * self.hardness.clamp(0.0, 0.99);
        let selection = doc.selection.clone();
        for coord in TileCoord::covering(&bounds) {
            let trect = coord.rect();
            let clip = trect.intersect(&bounds);
            if clip.is_empty() {
                continue;
            }
            // Coverage accumulation buffer for this tile.
            let cov = self
                .coverage
                .entry(coord)
                .or_insert_with(|| vec![0f32; (TILE_SIZE * TILE_SIZE) as usize].into_boxed_slice());
            let mut touched = false;
            for y in clip.top..clip.bottom {
                for x in clip.left..clip.right {
                    let d = ((x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2)).sqrt();
                    if d >= radius {
                        continue;
                    }
                    let mut a = if d <= inner {
                        1.0
                    } else {
                        1.0 - (d - inner) / (radius - inner)
                    };
                    if self.mode == PaintMode::Pencil {
                        // Pencil: binary coverage.
                        a = if a >= 0.5 { 1.0 } else { 0.0 };
                    }
                    a *= selection.coverage(x, y) as f32 / 255.0;
                    if a <= 0.0 {
                        continue;
                    }
                    let ix = ((y - trect.top) * TILE_SIZE + (x - trect.left)) as usize;
                    if a > cov[ix] {
                        cov[ix] = a;
                        touched = true;
                    }
                }
            }
            if !touched {
                continue;
            }
            // Re-composite this tile's touched pixels from pre-stroke state.
            let original = self.edit.pre_stroke_tile(doc, self.layer, coord);
            // Ensure before-capture happens (and get write access).
            let cov = self.coverage.get(&coord).unwrap();
            let (ink, opacity) = (self.ink.clone(), self.opacity);
            let carried = self.carried;
            let (heal, heal_rect) = (&self.heal, self.heal_rect);
            let Some(tile) = self.edit.writable_tile(doc, self.layer, coord) else {
                continue;
            };
            for y in clip.top..clip.bottom {
                for x in clip.left..clip.right {
                    let ix = ((y - trect.top) * TILE_SIZE + (x - trect.left)) as usize;
                    let c = cov[ix];
                    if c <= 0.0 {
                        continue;
                    }
                    let orig = match &original {
                        Some(t) => t.get(ix),
                        None => Rgba::TRANSPARENT,
                    };
                    let a = c * opacity;
                    let out = match &ink {
                        Ink::Erase => Rgba {
                            a: orig.a * (1.0 - a),
                            ..orig
                        },
                        Ink::Solid(color) => Rgba { a, ..*color }.over(orig),
                        Ink::Clone { source, dx, dy } => {
                            let src = source.pixel(x - dx, y - dy);
                            Rgba {
                                a: src.a * a,
                                ..src
                            }
                            .over(orig)
                        }
                        Ink::Tone(tone) => {
                            if orig.a <= 0.0 {
                                orig
                            } else {
                                apply_tone(*tone, orig, a)
                            }
                        }
                        Ink::Convolve { snapshot, sharpen } => {
                            let out = convolve_at(snapshot, x, y, *sharpen);
                            Rgba {
                                r: orig.r + (out.r - orig.r) * a,
                                g: orig.g + (out.g - orig.g) * a,
                                b: orig.b + (out.b - orig.b) * a,
                                a: orig.a,
                            }
                        }
                        Ink::Smudge => match carried {
                            Some(c) if orig.a > 0.0 => Rgba {
                                r: orig.r + (c.r - orig.r) * a,
                                g: orig.g + (c.g - orig.g) * a,
                                b: orig.b + (c.b - orig.b) * a,
                                a: orig.a + (c.a - orig.a) * a,
                            },
                            _ => orig,
                        },
                        Ink::Heal => {
                            let src = heal_at(heal, heal_rect, x, y);
                            match src {
                                Some(src) => Rgba {
                                    r: orig.r + (src.r - orig.r) * a,
                                    g: orig.g + (src.g - orig.g) * a,
                                    b: orig.b + (src.b - orig.b) * a,
                                    a: orig.a.max(src.a * a),
                                },
                                None => orig,
                            }
                        }
                        Ink::Restore { snapshot } => {
                            let src = snapshot.pixel(x, y);
                            Rgba {
                                r: orig.r + (src.r - orig.r) * a,
                                g: orig.g + (src.g - orig.g) * a,
                                b: orig.b + (src.b - orig.b) * a,
                                a: orig.a + (src.a - orig.a) * a,
                            }
                        }
                        Ink::BackgroundErase { target, tolerance } => {
                            let d = (orig.r - target.r)
                                .abs()
                                .max((orig.g - target.g).abs())
                                .max((orig.b - target.b).abs());
                            if d <= *tolerance {
                                // Feather out over the last tenth of the
                                // tolerance so edges do not come out jagged.
                                let soft = if *tolerance > 0.0 {
                                    ((tolerance - d) / (tolerance * 0.25).max(1e-4)).clamp(0.0, 1.0)
                                } else {
                                    1.0
                                };
                                Rgba {
                                    a: orig.a * (1.0 - a * soft),
                                    ..orig
                                }
                            } else {
                                orig
                            }
                        }
                    };
                    tile.set(ix, out);
                }
            }
            doc.add_damage(clip);
        }
    }

    /// Work out this dab's replacement pixels.
    ///
    /// The healing brush takes texture from the sampled source and colour
    /// from the destination: it shifts the source patch so that its mean
    /// matches the destination's, which is a cheap stand-in for
    /// Photoshop's gradient-domain solve and is indistinguishable on the
    /// small dabs the tool is actually used with.
    ///
    /// Spot healing has no source, so it interpolates the ring of pixels
    /// just outside the dab across the hole, weighted by inverse distance
    /// -- which is what removes a blemish.
    fn prepare_heal(&mut self, bounds: IntRect, cx: f32, cy: f32, radius: f32) {
        self.heal_rect = bounds;
        let (w, h) = (
            bounds.width().max(0) as usize,
            bounds.height().max(0) as usize,
        );
        self.heal = vec![Rgba::TRANSPARENT; w * h];
        if w == 0 || h == 0 {
            return;
        }
        let dst = &self.snapshot;

        if self.mode == PaintMode::Heal {
            let (dx, dy) = self.heal_offset;
            // Match colour across the dab's *rim*, not its interior.
            //
            // Averaging over the whole dab would carry the mean of the
            // thing being healed away -- paint over a red blemish and the
            // result comes back red. The ring just outside the dab is the
            // skin you actually want the patch to match.
            let outer = radius + 3.0;
            let (mut sm, mut dm, mut n) = ([0f32; 3], [0f32; 3], 0f32);
            let scan = IntRect::new(
                (cx - outer).floor() as i32,
                (cy - outer).floor() as i32,
                (cx + outer).ceil() as i32 + 1,
                (cy + outer).ceil() as i32 + 1,
            );
            for y in scan.top..scan.bottom {
                for x in scan.left..scan.right {
                    let d = (x as f32 + 0.5 - cx).hypot(y as f32 + 0.5 - cy);
                    if d < radius || d >= outer {
                        continue;
                    }
                    let sp = dst.pixel(x - dx, y - dy);
                    let dp = dst.pixel(x, y);
                    sm[0] += sp.r;
                    sm[1] += sp.g;
                    sm[2] += sp.b;
                    dm[0] += dp.r;
                    dm[1] += dp.g;
                    dm[2] += dp.b;
                    n += 1.0;
                }
            }
            if n == 0.0 {
                return;
            }
            let shift = [
                (dm[0] - sm[0]) / n,
                (dm[1] - sm[1]) / n,
                (dm[2] - sm[2]) / n,
            ];
            for y in 0..h {
                for x in 0..w {
                    let (px, py) = (bounds.left + x as i32, bounds.top + y as i32);
                    let sp = dst.pixel(px - dx, py - dy);
                    self.heal[y * w + x] = Rgba {
                        r: (sp.r + shift[0]).clamp(0.0, 1.0),
                        g: (sp.g + shift[1]).clamp(0.0, 1.0),
                        b: (sp.b + shift[2]).clamp(0.0, 1.0),
                        a: sp.a,
                    };
                }
            }
            return;
        }

        // Spot healing: gather the ring just outside the dab.
        let mut ring: Vec<(f32, f32, Rgba)> = Vec::new();
        let outer = radius + 3.0;
        let scan = IntRect::new(
            (cx - outer).floor() as i32,
            (cy - outer).floor() as i32,
            (cx + outer).ceil() as i32 + 1,
            (cy + outer).ceil() as i32 + 1,
        );
        for y in scan.top..scan.bottom {
            for x in scan.left..scan.right {
                let d = (x as f32 + 0.5 - cx).hypot(y as f32 + 0.5 - cy);
                if d >= radius && d < outer {
                    let p = dst.pixel(x, y);
                    if p.a > 0.0 {
                        ring.push((x as f32 + 0.5, y as f32 + 0.5, p));
                    }
                }
            }
        }
        if ring.is_empty() {
            return;
        }
        for y in 0..h {
            for x in 0..w {
                let (fx, fy) = (
                    bounds.left as f32 + x as f32 + 0.5,
                    bounds.top as f32 + y as f32 + 0.5,
                );
                let (mut acc, mut wsum) = ([0f32; 4], 0f32);
                for (rx, ry, c) in &ring {
                    // Inverse square distance: nearby ring pixels dominate,
                    // which keeps the patch following local colour.
                    let d2 = (rx - fx).powi(2) + (ry - fy).powi(2);
                    let wgt = 1.0 / (d2 + 1.0);
                    acc[0] += c.r * wgt;
                    acc[1] += c.g * wgt;
                    acc[2] += c.b * wgt;
                    acc[3] += c.a * wgt;
                    wsum += wgt;
                }
                self.heal[y * w + x] = Rgba {
                    r: acc[0] / wsum,
                    g: acc[1] / wsum,
                    b: acc[2] / wsum,
                    a: acc[3] / wsum,
                };
            }
        }
    }

    /// Pick up the colour under the brush, mixing it with what is already
    /// being carried. A low mix means the smear fades quickly.
    fn prepare_smudge(&mut self, doc: &Document, cx: f32, cy: f32, radius: f32) {
        let Some(raster) = doc.tree.find(self.layer).and_then(|l| l.as_raster()) else {
            return;
        };
        let (mut acc, mut n) = ([0f32; 4], 0f32);
        let r = radius.max(1.0) as i32;
        let (ix, iy) = (cx as i32, cy as i32);
        for dy in -r..=r {
            for dx in -r..=r {
                if (dx * dx + dy * dy) as f32 > radius * radius {
                    continue;
                }
                let p = raster.tiles.pixel(ix + dx, iy + dy);
                acc[0] += p.r;
                acc[1] += p.g;
                acc[2] += p.b;
                acc[3] += p.a;
                n += 1.0;
            }
        }
        if n == 0.0 {
            return;
        }
        let here = Rgba {
            r: acc[0] / n,
            g: acc[1] / n,
            b: acc[2] / n,
            a: acc[3] / n,
        };
        self.carried = Some(match self.carried {
            None => here,
            Some(c) => Rgba {
                r: c.r + (here.r - c.r) * 0.35,
                g: c.g + (here.g - c.g) * 0.35,
                b: c.b + (here.b - c.b) * 0.35,
                a: c.a + (here.a - c.a) * 0.35,
            },
        });
    }

    fn finish(self, doc: &mut Document) {
        if let Some(layer) = doc.tree.find_mut(self.layer) {
            if let LayerKind::Raster(r) = &mut layer.kind {
                r.tiles.prune_blank();
            }
        }
        self.edit.commit(doc);
    }
}

/// Topmost paintable (raster, unlocked, visible) layer if the active layer
/// isn't paintable.
fn paintable_layer(doc: &Document) -> Option<LayerId> {
    if let Some(id) = doc.active_layer {
        if let Some(l) = doc.tree.find(id) {
            if matches!(l.kind, LayerKind::Raster(_)) && !l.locked {
                return Some(id);
            }
        }
    }
    doc.tree
        .iter()
        .filter(|l| matches!(l.kind, LayerKind::Raster(_)) && !l.locked)
        .map(|l| l.id)
        .last()
}

pub struct PaintTool {
    mode: PaintMode,
    stroke: Option<Stroke>,
    cursor: Option<(f32, f32)>,
    /// Clone stamp: the alt-clicked source point, and the offset locked in
    /// when the first stroke after it begins.
    clone_source: Option<(f32, f32)>,
    clone_offset: Option<(i32, i32)>,
    /// Background eraser colour tolerance, 0..=1.
    tolerance: f32,
}

impl PaintTool {
    fn new(mode: PaintMode) -> Self {
        PaintTool {
            mode,
            stroke: None,
            cursor: None,
            clone_source: None,
            clone_offset: None,
            tolerance: 0.12,
        }
    }

    /// Build the ink for a new stroke, or `None` if the tool isn't ready
    /// (clone stamp without a source point).
    fn ink_for(&mut self, ctx: &mut ToolCtx, input: PointerInput) -> Option<Ink> {
        Some(match self.mode {
            PaintMode::Eraser => Ink::Erase,
            PaintMode::Brush | PaintMode::Pencil => Ink::Solid(ctx.state.foreground),
            PaintMode::Dodge => Ink::Tone(Tone::Dodge),
            PaintMode::Burn => Ink::Tone(Tone::Burn),
            PaintMode::Sponge => Ink::Tone(Tone::Sponge),
            PaintMode::Clone => {
                let source = self.clone_source?;
                // Lock the offset on the first dab after setting a source,
                // then keep it for subsequent strokes (aligned cloning).
                let offset = *self.clone_offset.get_or_insert((
                    (input.x - source.0).round() as i32,
                    (input.y - source.1).round() as i32,
                ));
                let layer = paintable_layer(ctx.doc)?;
                let tiles = ctx
                    .doc
                    .tree
                    .find(layer)
                    .and_then(|l| l.as_raster())
                    .map(|r| r.tiles.clone())?;
                Ink::Clone {
                    source: tiles,
                    dx: offset.0,
                    dy: offset.1,
                }
            }
            PaintMode::Blur => Ink::Convolve {
                snapshot: layer_tiles(ctx.doc)?,
                sharpen: false,
            },
            PaintMode::Sharpen => Ink::Convolve {
                snapshot: layer_tiles(ctx.doc)?,
                sharpen: true,
            },
            PaintMode::Smudge => Ink::Smudge,
            PaintMode::Heal => {
                // Like the clone stamp, healing needs an alt-clicked
                // source before it can do anything.
                let source = self.clone_source?;
                self.clone_offset.get_or_insert((
                    (input.x - source.0).round() as i32,
                    (input.y - source.1).round() as i32,
                ));
                Ink::Heal
            }
            PaintMode::SpotHeal => Ink::Heal,
            PaintMode::HistoryBrush => Ink::Restore {
                snapshot: ctx
                    .doc
                    .history_source
                    .get(&paintable_layer(ctx.doc)?)
                    .cloned()?,
            },
            PaintMode::BackgroundEraser => {
                let tiles = layer_tiles(ctx.doc)?;
                Ink::BackgroundErase {
                    // Photoshop samples the colour under the brush's
                    // crosshair when the stroke starts.
                    target: tiles.pixel(input.x as i32, input.y as i32),
                    tolerance: self.tolerance,
                }
            }
        })
    }
}

/// The active paintable layer's tiles.
fn layer_tiles(doc: &Document) -> Option<TileMap> {
    let layer = paintable_layer(doc)?;
    doc.tree
        .find(layer)
        .and_then(|l| l.as_raster())
        .map(|r| r.tiles.clone())
}

impl ToolPlugin for PaintTool {
    fn id(&self) -> &'static str {
        match self.mode {
            PaintMode::Brush => "brush",
            PaintMode::Pencil => "pencil",
            PaintMode::Eraser => "eraser",
            PaintMode::Clone => "clone",
            PaintMode::Dodge => "dodge",
            PaintMode::Burn => "burn",
            PaintMode::Sponge => "sponge",
            PaintMode::Blur => "blur",
            PaintMode::Sharpen => "sharpen",
            PaintMode::Smudge => "smudge",
            PaintMode::Heal => "heal",
            PaintMode::SpotHeal => "spot_heal",
            PaintMode::HistoryBrush => "history_brush",
            PaintMode::BackgroundEraser => "background_eraser",
        }
    }

    fn name(&self) -> &'static str {
        match self.mode {
            PaintMode::Brush => "Brush",
            PaintMode::Pencil => "Pencil",
            PaintMode::Eraser => "Eraser",
            PaintMode::Clone => "Clone Stamp",
            PaintMode::Dodge => "Dodge",
            PaintMode::Burn => "Burn",
            PaintMode::Sponge => "Sponge",
            PaintMode::Blur => "Blur",
            PaintMode::Sharpen => "Sharpen",
            PaintMode::Smudge => "Smudge",
            PaintMode::Heal => "Healing Brush",
            PaintMode::SpotHeal => "Spot Healing Brush",
            PaintMode::HistoryBrush => "History Brush",
            PaintMode::BackgroundEraser => "Background Eraser",
        }
    }

    fn icon(&self) -> &'static str {
        match self.mode {
            PaintMode::Brush => "brush",
            PaintMode::Pencil => "pencil",
            PaintMode::Eraser => "eraser",
            PaintMode::Clone => "clone",
            PaintMode::Dodge => "dodge",
            PaintMode::Burn => "burn",
            PaintMode::Sponge => "sponge",
            PaintMode::Blur => "blur",
            PaintMode::Sharpen => "sharpen",
            PaintMode::Smudge => "smudge",
            PaintMode::Heal | PaintMode::SpotHeal => "heal",
            PaintMode::HistoryBrush => "history-brush",
            PaintMode::BackgroundEraser => "eraser-background",
        }
    }

    fn shortcut(&self) -> Option<&'static str> {
        match self.mode {
            PaintMode::Brush => Some("b"),
            PaintMode::Pencil => None, // cycles with the brush via shift-b
            PaintMode::Eraser => Some("e"),
            PaintMode::Clone => Some("s"),
            PaintMode::Dodge => Some("o"),
            PaintMode::Burn | PaintMode::Sponge => None,
            PaintMode::SpotHeal => Some("j"),
            PaintMode::HistoryBrush => Some("y"),
            PaintMode::Blur
            | PaintMode::Sharpen
            | PaintMode::Smudge
            | PaintMode::Heal
            | PaintMode::BackgroundEraser => None,
        }
    }

    fn group(&self) -> &'static str {
        match self.mode {
            // Photoshop's own groupings: tools that share a toolbar slot.
            PaintMode::Brush | PaintMode::Pencil => "brush",
            PaintMode::Eraser | PaintMode::BackgroundEraser => "eraser",
            PaintMode::Clone => "clone",
            PaintMode::Dodge | PaintMode::Burn | PaintMode::Sponge => "dodge",
            PaintMode::SpotHeal | PaintMode::Heal => "heal",
            PaintMode::Blur | PaintMode::Sharpen | PaintMode::Smudge => "blur",
            PaintMode::HistoryBrush => "history_brush",
        }
    }

    fn options(&self) -> Vec<ToolOption> {
        match self.mode {
            PaintMode::BackgroundEraser => vec![ToolOption::slider(
                "bge-tolerance",
                "Tolerance",
                self.tolerance * 100.0,
                1.0,
                100.0,
                "%",
            )],
            _ => Vec::new(),
        }
    }

    fn set_option(&mut self, key: &str, value: OptionValue) {
        if key == "bge-tolerance" {
            self.tolerance = (value.num() / 100.0).clamp(0.01, 1.0);
        }
    }

    fn on_pointer_down(&mut self, ctx: &mut ToolCtx, input: PointerInput) {
        self.cursor = Some((input.x, input.y));
        // Alt-click sets the clone stamp's source point.
        if matches!(self.mode, PaintMode::Clone | PaintMode::Heal) && input.modifiers.alt {
            self.clone_source = Some((input.x, input.y));
            self.clone_offset = None;
            return;
        }
        let Some(ink) = self.ink_for(ctx, input) else {
            return;
        };
        let heal_offset = self.clone_offset.unwrap_or((0, 0));
        self.stroke = Stroke::begin(ctx, input, self.mode, ink, heal_offset);
    }

    fn on_pointer_move(&mut self, ctx: &mut ToolCtx, input: PointerInput) {
        self.cursor = Some((input.x, input.y));
        if let Some(stroke) = &mut self.stroke {
            stroke.extend(ctx.doc, input.x, input.y, input.pressure);
        }
    }

    fn on_pointer_up(&mut self, ctx: &mut ToolCtx, _input: PointerInput) {
        if let Some(stroke) = self.stroke.take() {
            stroke.finish(ctx.doc);
        }
    }

    fn on_cancel(&mut self, ctx: &mut ToolCtx) {
        if let Some(stroke) = self.stroke.take() {
            stroke.edit.cancel(ctx.doc);
        }
    }

    fn overlays(&self, _doc: &Document, state: &EditorState) -> Vec<Overlay> {
        match self.cursor {
            Some((cx, cy)) => {
                vec![Overlay::Circle {
                    cx,
                    cy,
                    r: state.brush_size / 2.0,
                }]
            }
            None => Vec::new(),
        }
    }
}

// ===== gradient =====

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GradientKind {
    Linear,
    Radial,
}

/// The five shapes Photoshop's gradient tool can draw. Kept separate from
/// [`GradientKind`], which is the tool's *identity*: the registry looks
/// tools up by `id()`, so the two registered gradients must keep the ids
/// they were registered under even when you change the style.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GradientStyle {
    Linear,
    Radial,
    Angle,
    Reflected,
    Diamond,
}

const GRADIENT_STYLES: &[&str] = &["Linear", "Radial", "Angle", "Reflected", "Diamond"];
const GRADIENT_FILLS: &[&str] = &["Foreground to Background", "Foreground to Transparent"];

impl GradientStyle {
    fn from_index(i: usize) -> GradientStyle {
        match i {
            1 => GradientStyle::Radial,
            2 => GradientStyle::Angle,
            3 => GradientStyle::Reflected,
            4 => GradientStyle::Diamond,
            _ => GradientStyle::Linear,
        }
    }
    fn index(self) -> usize {
        match self {
            GradientStyle::Linear => 0,
            GradientStyle::Radial => 1,
            GradientStyle::Angle => 2,
            GradientStyle::Reflected => 3,
            GradientStyle::Diamond => 4,
        }
    }
}

/// Gradient tool: drag to define the axis, release to fill.
pub struct GradientTool {
    pub kind: GradientKind,
    /// Fade the foreground out instead of ending on the background colour.
    pub to_transparent: bool,
    /// The shape drawn. Starts at whichever one this tool was registered
    /// as, and follows the options bar after that.
    style: GradientStyle,
    /// Run the ramp backwards.
    reverse: bool,
    /// Break up the banding a long, shallow ramp shows in 8-bit.
    dither: bool,
    anchor: Option<(f32, f32)>,
    current: Option<(f32, f32)>,
}

impl GradientTool {
    fn new(kind: GradientKind) -> GradientTool {
        GradientTool {
            kind,
            to_transparent: false,
            style: match kind {
                GradientKind::Linear => GradientStyle::Linear,
                GradientKind::Radial => GradientStyle::Radial,
            },
            reverse: false,
            dither: false,
            anchor: None,
            current: None,
        }
    }
}

impl ToolPlugin for GradientTool {
    fn id(&self) -> &'static str {
        match self.kind {
            GradientKind::Linear => "gradient",
            GradientKind::Radial => "gradient.radial",
        }
    }
    fn name(&self) -> &'static str {
        match self.kind {
            GradientKind::Linear => "Gradient",
            GradientKind::Radial => "Radial Gradient",
        }
    }
    fn icon(&self) -> &'static str {
        "gradient"
    }
    fn shortcut(&self) -> Option<&'static str> {
        matches!(self.kind, GradientKind::Linear).then_some("g")
    }

    fn group(&self) -> &'static str {
        "gradient"
    }

    fn on_pointer_down(&mut self, _ctx: &mut ToolCtx, input: PointerInput) {
        self.anchor = Some((input.x, input.y));
        self.current = Some((input.x, input.y));
    }

    fn on_pointer_move(&mut self, _ctx: &mut ToolCtx, input: PointerInput) {
        if self.anchor.is_some() {
            self.current = Some((input.x, input.y));
        }
    }

    fn on_pointer_up(&mut self, ctx: &mut ToolCtx, input: PointerInput) {
        let Some((ax, ay)) = self.anchor.take() else {
            return;
        };
        self.current = None;
        let (bx, by) = (input.x, input.y);
        if (bx - ax).abs() < 0.5 && (by - ay).abs() < 0.5 {
            return;
        }
        fill_gradient(
            ctx,
            GradientFill {
                style: self.style,
                to_transparent: self.to_transparent,
                reverse: self.reverse,
                dither: self.dither,
            },
            (ax, ay),
            (bx, by),
        );
    }

    fn on_cancel(&mut self, _ctx: &mut ToolCtx) {
        self.anchor = None;
        self.current = None;
    }

    fn options(&self) -> Vec<ToolOption> {
        vec![
            ToolOption::choice(
                "gradient-fill",
                "Gradient",
                GRADIENT_FILLS,
                usize::from(self.to_transparent),
            ),
            ToolOption::choice(
                "gradient-style",
                "Style",
                GRADIENT_STYLES,
                self.style.index(),
            ),
            ToolOption::toggle("gradient-reverse", "Reverse", self.reverse),
            ToolOption::toggle("gradient-dither", "Dither", self.dither),
        ]
    }

    fn set_option(&mut self, key: &str, value: OptionValue) {
        match key {
            "gradient-fill" => self.to_transparent = value.index() == 1,
            "gradient-style" => self.style = GradientStyle::from_index(value.index()),
            "gradient-reverse" => self.reverse = value.bool(),
            "gradient-dither" => self.dither = value.bool(),
            _ => {}
        }
    }

    fn overlays(&self, _doc: &Document, _state: &EditorState) -> Vec<Overlay> {
        match (self.anchor, self.current) {
            (Some(a), Some(c)) => vec![Overlay::Line {
                x1: a.0,
                y1: a.1,
                x2: c.0,
                y2: c.1,
            }],
            _ => Vec::new(),
        }
    }
}

/// Everything the options bar contributes to one gradient.
struct GradientFill {
    style: GradientStyle,
    to_transparent: bool,
    reverse: bool,
    dither: bool,
}

fn fill_gradient(ctx: &mut ToolCtx, fill: GradientFill, from: (f32, f32), to: (f32, f32)) {
    let GradientFill {
        style,
        to_transparent,
        reverse,
        dither,
    } = fill;
    let Some(layer) = paintable_layer(ctx.doc) else {
        return;
    };
    let start = ctx.state.foreground;
    let end = if to_transparent {
        Rgba {
            a: 0.0,
            ..ctx.state.foreground
        }
    } else {
        ctx.state.background
    };
    let opacity = ctx.state.tool_opacity;
    let canvas = ctx.doc.canvas_rect();
    let region = if ctx.doc.selection.is_empty() {
        canvas
    } else {
        ctx.doc.selection.bounds().intersect(&canvas)
    };
    if region.is_empty() {
        return;
    }
    let selection = ctx.doc.selection.clone();
    let (dx, dy) = (to.0 - from.0, to.1 - from.1);
    let len_sq = (dx * dx + dy * dy).max(1e-6);
    let radius = len_sq.sqrt().max(1e-6);

    let mut edit = ctx.doc.begin_edit("Gradient");
    for coord in TileCoord::covering(&region) {
        let trect = coord.rect();
        let clip = trect.intersect(&region);
        if clip.is_empty() {
            continue;
        }
        let Some(tile) = edit.writable_tile(layer, coord) else {
            break;
        };
        for y in clip.top..clip.bottom {
            for x in clip.left..clip.right {
                let sel = selection.coverage(x, y) as f32 / 255.0;
                if sel <= 0.0 {
                    continue;
                }
                let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
                let (ox, oy) = (px - from.0, py - from.1);
                // Distance along the drag, and across it, in units of the
                // drag's own length -- the frame every style works in.
                let along = (ox * dx + oy * dy) / len_sq;
                let across = (ox * -dy + oy * dx) / len_sq;
                let mut t = match style {
                    GradientStyle::Linear => along.clamp(0.0, 1.0),
                    GradientStyle::Radial => (ox.hypot(oy) / radius).clamp(0.0, 1.0),
                    // One full turn around the start point, measured from
                    // the direction of the drag.
                    GradientStyle::Angle => {
                        (oy.atan2(ox) - dy.atan2(dx)).rem_euclid(std::f32::consts::TAU)
                            / std::f32::consts::TAU
                    }
                    GradientStyle::Reflected => along.abs().clamp(0.0, 1.0),
                    GradientStyle::Diamond => (along.abs() + across.abs()).clamp(0.0, 1.0),
                };
                if reverse {
                    t = 1.0 - t;
                }
                if dither {
                    // A pixel-stable ordered wobble of well under one
                    // 8-bit step, which is enough to break up the banding
                    // a shallow ramp shows without looking like noise.
                    let n = (((x * 7 + y * 13) & 7) as f32 / 8.0 - 0.5) / 255.0;
                    t = (t + n).clamp(0.0, 1.0);
                }
                let src = Rgba {
                    r: start.r + (end.r - start.r) * t,
                    g: start.g + (end.g - start.g) * t,
                    b: start.b + (end.b - start.b) * t,
                    a: (start.a + (end.a - start.a) * t) * opacity * sel,
                };
                let ix = ((y - trect.top) * TILE_SIZE + (x - trect.left)) as usize;
                tile.set(ix, src.over(tile.get(ix)));
            }
        }
    }
    edit.commit();
}

// ===== paint bucket =====

/// Flood-fills contiguous similar pixels with the foreground colour.
pub struct BucketTool {
    /// 0..=255 per-channel tolerance.
    pub tolerance: u8,
    /// Off fills every matching pixel in the layer, not just the region
    /// joined to the one you clicked.
    contiguous: bool,
    /// Decide what matches from the composited image rather than from the
    /// layer being painted. The paint still lands on the active layer.
    all_layers: bool,
}

impl BucketTool {
    fn new() -> BucketTool {
        BucketTool {
            tolerance: 32,
            contiguous: true,
            all_layers: false,
        }
    }
}

impl ToolPlugin for BucketTool {
    fn id(&self) -> &'static str {
        "bucket"
    }
    fn name(&self) -> &'static str {
        "Paint Bucket"
    }
    fn icon(&self) -> &'static str {
        "bucket"
    }
    fn group(&self) -> &'static str {
        // The bucket shares the gradient's slot, as in Photoshop.
        "gradient"
    }

    fn on_pointer_down(&mut self, ctx: &mut ToolCtx, input: PointerInput) {
        let (x, y) = (input.x.floor() as i32, input.y.floor() as i32);
        let Some(layer) = paintable_layer(ctx.doc) else {
            return;
        };
        let canvas = ctx.doc.canvas_rect();
        if !canvas.contains(x, y) {
            return;
        }
        let Some(tiles) = ctx
            .doc
            .tree
            .find(layer)
            .and_then(|l| l.as_raster())
            .map(|r| r.tiles.clone())
        else {
            return;
        };
        let w = canvas.width() as usize;
        // What decides a match: the layer's own pixels, or everything you
        // can see. Sampling the composite is what "All Layers" means --
        // the paint still goes onto the active layer either way.
        let composite = self
            .all_layers
            .then(|| schist_compositor::composite_region_rgba8(ctx.doc, canvas));
        let sample = |px: i32, py: i32| -> [u8; 4] {
            match &composite {
                Some(buf) => {
                    let i = ((py - canvas.top) as usize * w + (px - canvas.left) as usize) * 4;
                    [buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]
                }
                None => tiles.pixel(px, py).to_u8(),
            }
        };
        let target = sample(x, y);
        let tol = self.tolerance as i32;
        let matches = |px: [u8; 4]| {
            px.iter()
                .zip(target.iter())
                .all(|(&a, &b)| (a as i32 - b as i32).abs() <= tol)
        };

        let selection = ctx.doc.selection.clone();
        let mut filled: Vec<(i32, i32)> = Vec::new();
        if self.contiguous {
            // 4-connected flood fill bounded by the canvas and the selection.
            let mut visited = vec![false; w * canvas.height() as usize];
            let mut stack = vec![(x, y)];
            visited[(y - canvas.top) as usize * w + (x - canvas.left) as usize] = true;
            while let Some((cx, cy)) = stack.pop() {
                if selection.coverage(cx, cy) == 0 {
                    continue;
                }
                filled.push((cx, cy));
                for (nx, ny) in [(cx - 1, cy), (cx + 1, cy), (cx, cy - 1), (cx, cy + 1)] {
                    if !canvas.contains(nx, ny) {
                        continue;
                    }
                    let ix = (ny - canvas.top) as usize * w + (nx - canvas.left) as usize;
                    if visited[ix] {
                        continue;
                    }
                    visited[ix] = true;
                    if matches(sample(nx, ny)) {
                        stack.push((nx, ny));
                    }
                }
            }
        } else {
            for py in canvas.top..canvas.bottom {
                for px in canvas.left..canvas.right {
                    if selection.coverage(px, py) != 0 && matches(sample(px, py)) {
                        filled.push((px, py));
                    }
                }
            }
        }
        if filled.is_empty() {
            return;
        }

        let color = ctx.state.foreground;
        let opacity = ctx.state.tool_opacity;
        let mut edit = ctx.doc.begin_edit("Paint Bucket");
        let mut by_tile: FxHashMap<TileCoord, Vec<(i32, i32)>> = FxHashMap::default();
        for (px, py) in filled {
            by_tile
                .entry(TileCoord::containing(px, py))
                .or_default()
                .push((px, py));
        }
        for (coord, pixels) in by_tile {
            let trect = coord.rect();
            let Some(tile) = edit.writable_tile(layer, coord) else {
                break;
            };
            for (px, py) in pixels {
                let sel = selection.coverage(px, py) as f32 / 255.0;
                let ix = ((py - trect.top) * TILE_SIZE + (px - trect.left)) as usize;
                let src = Rgba {
                    a: color.a * opacity * sel,
                    ..color
                };
                tile.set(ix, src.over(tile.get(ix)));
            }
        }
        edit.commit();
    }

    fn options(&self) -> Vec<ToolOption> {
        vec![
            ToolOption::slider(
                "bucket-tolerance",
                "Tolerance",
                self.tolerance as f32,
                0.0,
                255.0,
                "",
            ),
            ToolOption::toggle("bucket-contiguous", "Contiguous", self.contiguous),
            ToolOption::toggle("bucket-all-layers", "All Layers", self.all_layers),
        ]
    }

    fn set_option(&mut self, key: &str, value: OptionValue) {
        match key {
            "bucket-tolerance" => self.tolerance = value.num().clamp(0.0, 255.0) as u8,
            "bucket-contiguous" => self.contiguous = value.bool(),
            "bucket-all-layers" => self.all_layers = value.bool(),
            _ => {}
        }
    }

    fn on_pointer_move(&mut self, _ctx: &mut ToolCtx, _input: PointerInput) {}
    fn on_pointer_up(&mut self, _ctx: &mut ToolCtx, _input: PointerInput) {}
}

/// Build one paint tool by id. Exists so integration tests can drive a
/// single tool without standing up a whole registry.
pub fn tool_for_test(id: &str) -> Option<Box<dyn ToolPlugin>> {
    let mode = match id {
        "brush" => PaintMode::Brush,
        "pencil" => PaintMode::Pencil,
        "eraser" => PaintMode::Eraser,
        "clone" => PaintMode::Clone,
        "dodge" => PaintMode::Dodge,
        "burn" => PaintMode::Burn,
        "sponge" => PaintMode::Sponge,
        "blur" => PaintMode::Blur,
        "sharpen" => PaintMode::Sharpen,
        "smudge" => PaintMode::Smudge,
        "heal" => PaintMode::Heal,
        "spot_heal" => PaintMode::SpotHeal,
        "history_brush" => PaintMode::HistoryBrush,
        "background_eraser" => PaintMode::BackgroundEraser,
        _ => return None,
    };
    Some(Box::new(PaintTool::new(mode)))
}

pub struct PaintToolsPlugin;

impl PluginManifest for PaintToolsPlugin {
    fn id(&self) -> &'static str {
        "schist.tools-paint"
    }

    fn register(&self, registry: &mut PluginRegistry) {
        registry.register_tool(Box::new(PaintTool::new(PaintMode::Brush)));
        registry.register_tool(Box::new(PaintTool::new(PaintMode::Pencil)));
        registry.register_tool(Box::new(PaintTool::new(PaintMode::Eraser)));
        registry.register_tool(Box::new(PaintTool::new(PaintMode::Clone)));
        registry.register_tool(Box::new(PaintTool::new(PaintMode::Dodge)));
        registry.register_tool(Box::new(PaintTool::new(PaintMode::Burn)));
        registry.register_tool(Box::new(PaintTool::new(PaintMode::Sponge)));
        registry.register_tool(Box::new(PaintTool::new(PaintMode::SpotHeal)));
        registry.register_tool(Box::new(PaintTool::new(PaintMode::Heal)));
        registry.register_tool(Box::new(PaintTool::new(PaintMode::BackgroundEraser)));
        registry.register_tool(Box::new(PaintTool::new(PaintMode::HistoryBrush)));
        registry.register_tool(Box::new(PaintTool::new(PaintMode::Blur)));
        registry.register_tool(Box::new(PaintTool::new(PaintMode::Sharpen)));
        registry.register_tool(Box::new(PaintTool::new(PaintMode::Smudge)));
        registry.register_tool(Box::new(GradientTool::new(GradientKind::Linear)));
        registry.register_tool(Box::new(GradientTool::new(GradientKind::Radial)));
        registry.register_tool(Box::new(BucketTool::new()));
    }
}

// Silence unused-dep warning: pixel-ops is used by future paint modes
// (brush blend modes); keep the wiring alive.
#[allow(unused_imports)]
use schist_pixel_ops as _;

#[cfg(test)]
mod tests {
    use super::*;
    use schist_color::Depth;
    use schist_core::Layer;
    use schist_plugin_api::{EditorState, Modifiers, OptionValue};

    fn doc_with_layer() -> Document {
        let mut doc = Document::new("t", 128, 128, Depth::Eight);
        doc.push_layer(Layer::new_raster("paint"));
        doc
    }

    fn input(x: f32, y: f32) -> PointerInput {
        PointerInput {
            x,
            y,
            pressure: 1.0,
            modifiers: Modifiers::default(),
        }
    }

    fn pixel(doc: &Document, x: i32, y: i32) -> [u8; 4] {
        doc.tree.layers[0]
            .as_raster()
            .unwrap()
            .tiles
            .pixel(x, y)
            .to_u8()
    }

    /// Two separated red squares. Contiguous fill reaches one; the
    /// non-contiguous mode reaches both.
    fn two_squares_doc() -> Document {
        let mut doc = Document::new("t", 128, 128, Depth::Eight);
        let mut layer = Layer::new_raster("sq");
        let red = [255u8, 0, 0, 255].repeat(16 * 16);
        for origin in [(8, 8), (80, 80)] {
            schist_core::blit_rgba8(
                &mut layer.as_raster_mut().unwrap().tiles,
                Depth::Eight,
                IntRect::from_xywh(origin.0, origin.1, 16, 16),
                &red,
            );
        }
        doc.push_layer(layer);
        doc
    }

    #[test]
    fn a_non_contiguous_bucket_reaches_the_far_square() {
        for contiguous in [true, false] {
            let mut doc = two_squares_doc();
            let mut state = EditorState {
                foreground: schist_color::Rgba::from_u8(0, 0, 255, 255),
                ..EditorState::default()
            };
            let mut tool = BucketTool::new();
            tool.set_option("bucket-contiguous", OptionValue::Bool(contiguous));
            let mut ctx = ToolCtx {
                doc: &mut doc,
                state: &mut state,
            };
            tool.on_pointer_down(&mut ctx, input(12.0, 12.0));

            assert_eq!(pixel(&doc, 12, 12)[2], 255, "the clicked square fills");
            let far = pixel(&doc, 88, 88)[2];
            if contiguous {
                assert_eq!(far, 0, "a contiguous fill must not jump the gap");
            } else {
                assert_eq!(far, 255, "a non-contiguous fill takes every match");
            }
        }
    }

    #[test]
    fn the_bucket_can_match_against_the_composite() {
        // Red below, an empty layer above. Clicking on the empty layer
        // matches its own transparency and floods everything; matching the
        // composite instead confines the fill to the red square.
        let mut doc = two_squares_doc();
        doc.push_layer(Layer::new_raster("empty"));
        let mut state = EditorState {
            foreground: schist_color::Rgba::from_u8(0, 0, 255, 255),
            ..EditorState::default()
        };
        let mut tool = BucketTool::new();
        tool.set_option("bucket-all-layers", OptionValue::Bool(true));
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        tool.on_pointer_down(&mut ctx, input(12.0, 12.0));

        let top = doc.tree.layers.last().unwrap().as_raster().unwrap();
        assert_eq!(top.tiles.pixel(12, 12).to_u8()[2], 255, "inside the square");
        assert_eq!(
            top.tiles.pixel(60, 60).to_u8()[3],
            0,
            "outside it, where the composite does not match"
        );
    }

    #[test]
    fn the_gradient_styles_are_not_all_the_same_picture() {
        let sample = |style: usize, reverse: bool| {
            let mut doc = doc_with_layer();
            let mut state = EditorState {
                foreground: schist_color::Rgba::from_u8(255, 255, 255, 255),
                background: schist_color::Rgba::from_u8(0, 0, 0, 255),
                ..EditorState::default()
            };
            let mut tool = GradientTool::new(GradientKind::Linear);
            tool.set_option("gradient-style", OptionValue::Choice(style));
            tool.set_option("gradient-reverse", OptionValue::Bool(reverse));
            let mut ctx = ToolCtx {
                doc: &mut doc,
                state: &mut state,
            };
            tool.on_pointer_down(&mut ctx, input(32.0, 64.0));
            tool.on_pointer_up(&mut ctx, input(96.0, 64.0));
            // Three probes: before the start, at the midpoint, past the end.
            [
                pixel(&doc, 16, 64)[0],
                pixel(&doc, 64, 64)[0],
                pixel(&doc, 110, 64)[0],
            ]
        };

        let linear = sample(0, false);
        assert_eq!(
            linear[0], 255,
            "clamped to the start colour before the drag"
        );
        assert_eq!(linear[2], 0, "and to the end colour past it");
        assert!(
            linear[1] > 100 && linear[1] < 160,
            "halfway along should be halfway between: {linear:?}"
        );

        // What makes reflected reflected: it is symmetric about the point
        // the drag started from, where linear is flat on the near side.
        let mirrored = |style: usize| {
            let mut doc = doc_with_layer();
            let mut state = EditorState {
                foreground: schist_color::Rgba::from_u8(255, 255, 255, 255),
                background: schist_color::Rgba::from_u8(0, 0, 0, 255),
                ..EditorState::default()
            };
            let mut tool = GradientTool::new(GradientKind::Linear);
            tool.set_option("gradient-style", OptionValue::Choice(style));
            let mut ctx = ToolCtx {
                doc: &mut doc,
                state: &mut state,
            };
            tool.on_pointer_down(&mut ctx, input(64.0, 64.0));
            tool.on_pointer_up(&mut ctx, input(112.0, 64.0));
            // Pixel centres, so these two are genuinely equidistant from
            // the start: 40.5 is 23.5 left of 64, and 87.5 is 23.5 right.
            (pixel(&doc, 40, 64)[0], pixel(&doc, 87, 64)[0])
        };
        let (near, far) = mirrored(3);
        assert_eq!(near, far, "reflected should mirror about the start point");
        let (near, far) = mirrored(0);
        assert_ne!(near, far, "linear should not");

        for style in [1usize, 2, 4] {
            assert_ne!(sample(style, false), linear, "style {style} matched linear");
        }

        // Reversing swaps the ends.
        let rev = sample(0, true);
        assert_eq!(rev[0], 0);
        assert_eq!(rev[2], 255);
    }

    #[test]
    fn brush_paints_and_undoes() {
        let mut doc = doc_with_layer();
        let mut state = EditorState {
            foreground: Rgba::new(1.0, 0.0, 0.0, 1.0),
            ..Default::default()
        };
        let mut tool = PaintTool::new(PaintMode::Brush);
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        tool.on_pointer_down(&mut ctx, input(30.0, 30.0));
        tool.on_pointer_move(&mut ctx, input(70.0, 30.0));
        tool.on_pointer_up(&mut ctx, input(70.0, 30.0));

        let p = pixel(&doc, 50, 30);
        assert_eq!(p[0], 255, "stroke center painted: {p:?}");
        assert_eq!(p[3], 255);
        assert_eq!(pixel(&doc, 50, 100)[3], 0, "far pixel untouched");

        assert_eq!(doc.undo().as_deref(), Some("Brush"));
        assert_eq!(pixel(&doc, 50, 30)[3], 0, "undo clears stroke");
        doc.redo();
        assert_eq!(pixel(&doc, 50, 30)[0], 255, "redo restores");
    }

    #[test]
    fn stroke_opacity_does_not_compound_within_stroke() {
        let mut doc = doc_with_layer();
        let mut state = EditorState {
            foreground: Rgba::BLACK,
            tool_opacity: 0.5,
            ..Default::default()
        };
        let mut tool = PaintTool::new(PaintMode::Brush);
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        tool.on_pointer_down(&mut ctx, input(40.0, 40.0));
        // Scribble back and forth over the same spot.
        for _ in 0..5 {
            tool.on_pointer_move(&mut ctx, input(60.0, 40.0));
            tool.on_pointer_move(&mut ctx, input(40.0, 40.0));
        }
        tool.on_pointer_up(&mut ctx, input(40.0, 40.0));
        let a = pixel(&doc, 50, 40)[3];
        assert!((a as i32 - 128).abs() <= 2, "opacity stayed ~50%: {a}");
    }

    #[test]
    fn separate_strokes_compound() {
        let mut doc = doc_with_layer();
        let mut state = EditorState {
            foreground: Rgba::BLACK,
            tool_opacity: 0.5,
            ..Default::default()
        };
        let mut tool = PaintTool::new(PaintMode::Brush);
        for _ in 0..2 {
            let mut ctx = ToolCtx {
                doc: &mut doc,
                state: &mut state,
            };
            tool.on_pointer_down(&mut ctx, input(50.0, 40.0));
            tool.on_pointer_up(&mut ctx, input(50.0, 40.0));
        }
        let a = pixel(&doc, 50, 40)[3];
        assert!(a > 170 && a < 210, "two 50% strokes ≈ 75%: {a}");
    }

    #[test]
    fn eraser_clears() {
        let mut doc = doc_with_layer();
        let mut state = EditorState {
            foreground: Rgba::BLACK,
            ..Default::default()
        };
        let mut brush = PaintTool::new(PaintMode::Brush);
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        brush.on_pointer_down(&mut ctx, input(50.0, 50.0));
        brush.on_pointer_up(&mut ctx, input(50.0, 50.0));
        assert!(pixel(&doc, 50, 50)[3] > 0);

        let mut eraser = PaintTool::new(PaintMode::Eraser);
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        eraser.on_pointer_down(&mut ctx, input(50.0, 50.0));
        eraser.on_pointer_up(&mut ctx, input(50.0, 50.0));
        assert_eq!(pixel(&doc, 50, 50)[3], 0, "erased");
    }

    #[test]
    fn selection_confines_painting() {
        use schist_core::SelectOp;
        let mut doc = doc_with_layer();
        doc.selection
            .select_rect(IntRect::from_xywh(0, 0, 45, 128), SelectOp::Replace);
        let mut state = EditorState {
            foreground: Rgba::BLACK,
            ..Default::default()
        };
        let mut tool = PaintTool::new(PaintMode::Brush);
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        tool.on_pointer_down(&mut ctx, input(30.0, 30.0));
        tool.on_pointer_move(&mut ctx, input(80.0, 30.0));
        tool.on_pointer_up(&mut ctx, input(80.0, 30.0));
        assert!(pixel(&doc, 40, 30)[3] > 0, "inside selection painted");
        assert_eq!(pixel(&doc, 60, 30)[3], 0, "outside selection untouched");
    }

    #[test]
    fn cancel_rolls_back() {
        let mut doc = doc_with_layer();
        let mut state = EditorState::default();
        let mut tool = PaintTool::new(PaintMode::Brush);
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        tool.on_pointer_down(&mut ctx, input(50.0, 50.0));
        tool.on_cancel(&mut ctx);
        assert_eq!(pixel(&doc, 50, 50)[3], 0);
        assert!(!doc.history.can_undo());
    }
}

#[cfg(test)]
mod m7_tests {
    use super::*;
    use schist_color::Depth;
    use schist_core::{blit_rgba8, Layer, SelectOp};
    use schist_plugin_api::Modifiers;

    fn filled_doc(rgba: [u8; 4]) -> Document {
        let mut doc = Document::new("t", 64, 64, Depth::Eight);
        let mut layer = Layer::new_raster("bg");
        let buf: Vec<u8> = rgba.iter().cycle().take(64 * 64 * 4).copied().collect();
        blit_rgba8(
            &mut layer.as_raster_mut().unwrap().tiles,
            Depth::Eight,
            IntRect::from_size(64, 64),
            &buf,
        );
        doc.push_layer(layer);
        doc
    }

    fn input(x: f32, y: f32) -> PointerInput {
        PointerInput {
            x,
            y,
            pressure: 1.0,
            modifiers: Modifiers::default(),
        }
    }

    fn alt(x: f32, y: f32) -> PointerInput {
        PointerInput {
            modifiers: Modifiers {
                alt: true,
                ..Default::default()
            },
            ..input(x, y)
        }
    }

    fn px(doc: &Document, x: i32, y: i32) -> [u8; 4] {
        doc.tree.layers[0]
            .as_raster()
            .unwrap()
            .tiles
            .pixel(x, y)
            .to_u8()
    }

    #[test]
    fn clone_stamp_copies_from_the_source_point() {
        // Left half red, right half blue; clone red onto the blue side.
        let mut doc = Document::new("t", 64, 64, Depth::Eight);
        let mut layer = Layer::new_raster("bg");
        let mut buf = vec![0u8; 64 * 64 * 4];
        for y in 0..64 {
            for x in 0..64 {
                let i = (y * 64 + x) * 4;
                let c: [u8; 4] = if x < 32 {
                    [255, 0, 0, 255]
                } else {
                    [0, 0, 255, 255]
                };
                buf[i..i + 4].copy_from_slice(&c);
            }
        }
        blit_rgba8(
            &mut layer.as_raster_mut().unwrap().tiles,
            Depth::Eight,
            IntRect::from_size(64, 64),
            &buf,
        );
        doc.push_layer(layer);

        let mut state = EditorState {
            brush_size: 10.0,
            brush_hardness: 1.0,
            ..Default::default()
        };
        let mut tool = PaintTool::new(PaintMode::Clone);
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        // Source in the red half, then paint in the blue half.
        tool.on_pointer_down(&mut ctx, alt(10.0, 32.0));
        tool.on_pointer_down(&mut ctx, input(50.0, 32.0));
        tool.on_pointer_up(&mut ctx, input(50.0, 32.0));

        assert_eq!(px(&doc, 50, 32), [255, 0, 0, 255], "cloned red pixels");
        assert_eq!(px(&doc, 60, 32), [0, 0, 255, 255], "outside the dab");
    }

    #[test]
    fn clone_without_a_source_does_nothing() {
        let mut doc = filled_doc([10, 10, 10, 255]);
        let mut state = EditorState::default();
        let mut tool = PaintTool::new(PaintMode::Clone);
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        tool.on_pointer_down(&mut ctx, input(20.0, 20.0));
        tool.on_pointer_up(&mut ctx, input(20.0, 20.0));
        assert_eq!(px(&doc, 20, 20), [10, 10, 10, 255]);
        assert!(!doc.history.can_undo());
    }

    #[test]
    fn dodge_lightens_and_burn_darkens() {
        let mut state = EditorState {
            brush_size: 12.0,
            brush_hardness: 1.0,
            ..Default::default()
        };

        let mut doc = filled_doc([128, 128, 128, 255]);
        let mut tool = PaintTool::new(PaintMode::Dodge);
        {
            let mut ctx = ToolCtx {
                doc: &mut doc,
                state: &mut state,
            };
            tool.on_pointer_down(&mut ctx, input(32.0, 32.0));
            tool.on_pointer_up(&mut ctx, input(32.0, 32.0));
        }
        assert!(
            px(&doc, 32, 32)[0] > 140,
            "dodge lightened: {:?}",
            px(&doc, 32, 32)
        );

        let mut doc = filled_doc([128, 128, 128, 255]);
        let mut tool = PaintTool::new(PaintMode::Burn);
        {
            let mut ctx = ToolCtx {
                doc: &mut doc,
                state: &mut state,
            };
            tool.on_pointer_down(&mut ctx, input(32.0, 32.0));
            tool.on_pointer_up(&mut ctx, input(32.0, 32.0));
        }
        assert!(
            px(&doc, 32, 32)[0] < 116,
            "burn darkened: {:?}",
            px(&doc, 32, 32)
        );
    }

    #[test]
    fn dodge_leaves_transparent_pixels_alone() {
        let mut doc = Document::new("t", 64, 64, Depth::Eight);
        doc.push_layer(Layer::new_raster("empty"));
        let mut state = EditorState {
            brush_size: 12.0,
            ..Default::default()
        };
        let mut tool = PaintTool::new(PaintMode::Dodge);
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        tool.on_pointer_down(&mut ctx, input(32.0, 32.0));
        tool.on_pointer_up(&mut ctx, input(32.0, 32.0));
        assert_eq!(px(&doc, 32, 32)[3], 0);
    }

    #[test]
    fn linear_gradient_ramps_between_colors() {
        let mut doc = filled_doc([0, 0, 0, 0]);
        let mut state = EditorState {
            foreground: Rgba::new(1.0, 0.0, 0.0, 1.0),
            background: Rgba::new(0.0, 0.0, 1.0, 1.0),
            ..Default::default()
        };
        let mut tool = GradientTool::new(GradientKind::Linear);
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        tool.on_pointer_down(&mut ctx, input(0.0, 32.0));
        tool.on_pointer_move(&mut ctx, input(63.0, 32.0));
        tool.on_pointer_up(&mut ctx, input(63.0, 32.0));

        let left = px(&doc, 1, 32);
        let mid = px(&doc, 32, 32);
        let right = px(&doc, 62, 32);
        assert!(left[0] > 240 && left[2] < 20, "starts foreground: {left:?}");
        assert!(
            right[2] > 240 && right[0] < 20,
            "ends background: {right:?}"
        );
        assert!(
            mid[0] > 100 && mid[0] < 160 && mid[2] > 100 && mid[2] < 160,
            "midpoint blends: {mid:?}"
        );
        doc.undo();
        assert_eq!(px(&doc, 32, 32)[3], 0, "undo clears the gradient");
    }

    #[test]
    fn radial_gradient_is_centered_on_the_start_point() {
        let mut doc = filled_doc([0, 0, 0, 0]);
        let mut state = EditorState {
            foreground: Rgba::new(1.0, 1.0, 1.0, 1.0),
            background: Rgba::new(0.0, 0.0, 0.0, 1.0),
            ..Default::default()
        };
        let mut tool = GradientTool::new(GradientKind::Radial);
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        tool.on_pointer_down(&mut ctx, input(32.0, 32.0));
        tool.on_pointer_up(&mut ctx, input(62.0, 32.0));
        assert!(px(&doc, 32, 32)[0] > 240, "centre is the start colour");
        assert!(px(&doc, 62, 32)[0] < 20, "edge is the end colour");
        // Equidistant points match, whatever the direction.
        assert_eq!(px(&doc, 32, 12)[0], px(&doc, 12, 32)[0]);
    }

    #[test]
    fn gradient_respects_the_selection() {
        let mut doc = filled_doc([0, 0, 0, 0]);
        doc.selection
            .select_rect(IntRect::from_xywh(0, 0, 32, 64), SelectOp::Replace);
        let mut state = EditorState {
            foreground: Rgba::new(1.0, 0.0, 0.0, 1.0),
            ..Default::default()
        };
        let mut tool = GradientTool::new(GradientKind::Linear);
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        tool.on_pointer_down(&mut ctx, input(0.0, 32.0));
        tool.on_pointer_up(&mut ctx, input(63.0, 32.0));
        assert!(px(&doc, 10, 32)[3] > 0, "inside selection filled");
        assert_eq!(px(&doc, 50, 32)[3], 0, "outside selection untouched");
    }

    #[test]
    fn bucket_fills_a_contiguous_region_only() {
        // Two separate black squares on transparent; filling one must not
        // touch the other.
        let mut doc = Document::new("t", 64, 64, Depth::Eight);
        let mut layer = Layer::new_raster("bg");
        let black = [0u8, 0, 0, 255].repeat(10 * 10);
        blit_rgba8(
            &mut layer.as_raster_mut().unwrap().tiles,
            Depth::Eight,
            IntRect::from_xywh(5, 5, 10, 10),
            &black,
        );
        blit_rgba8(
            &mut layer.as_raster_mut().unwrap().tiles,
            Depth::Eight,
            IntRect::from_xywh(40, 40, 10, 10),
            &black,
        );
        doc.push_layer(layer);

        let mut state = EditorState {
            foreground: Rgba::new(0.0, 1.0, 0.0, 1.0),
            ..Default::default()
        };
        let mut tool = BucketTool::new();
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        tool.on_pointer_down(&mut ctx, input(9.0, 9.0));

        assert_eq!(px(&doc, 9, 9), [0, 255, 0, 255], "clicked square filled");
        assert_eq!(px(&doc, 45, 45), [0, 0, 0, 255], "other square untouched");
        doc.undo();
        assert_eq!(px(&doc, 9, 9), [0, 0, 0, 255]);
    }

    #[test]
    fn bucket_tolerance_limits_the_spread() {
        let mut doc = Document::new("t", 32, 32, Depth::Eight);
        let mut layer = Layer::new_raster("bg");
        let mut buf = vec![0u8; 32 * 32 * 4];
        for y in 0..32 {
            for x in 0..32 {
                let i = (y * 32 + x) * 4;
                // A hard step in the middle, well beyond the tolerance.
                let v = if x < 16 { 10u8 } else { 200 };
                buf[i..i + 4].copy_from_slice(&[v, v, v, 255]);
            }
        }
        blit_rgba8(
            &mut layer.as_raster_mut().unwrap().tiles,
            Depth::Eight,
            IntRect::from_size(32, 32),
            &buf,
        );
        doc.push_layer(layer);

        let mut state = EditorState {
            foreground: Rgba::new(1.0, 0.0, 0.0, 1.0),
            ..Default::default()
        };
        let mut tool = BucketTool::new();
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        tool.on_pointer_down(&mut ctx, input(4.0, 16.0));
        assert_eq!(px(&doc, 4, 16), [255, 0, 0, 255], "dark side filled");
        assert_eq!(px(&doc, 25, 16), [200, 200, 200, 255], "light side kept");
    }
}

/// A 3x3 blur or sharpen of the pre-stroke pixels at one point.
fn convolve_at(tiles: &TileMap, x: i32, y: i32, sharpen: bool) -> Rgba {
    let mut acc = [0f32; 4];
    for dy in -1..=1 {
        for dx in -1..=1 {
            let p = tiles.pixel(x + dx, y + dy);
            acc[0] += p.r;
            acc[1] += p.g;
            acc[2] += p.b;
            acc[3] += p.a;
        }
    }
    let mean = Rgba {
        r: acc[0] / 9.0,
        g: acc[1] / 9.0,
        b: acc[2] / 9.0,
        a: acc[3] / 9.0,
    };
    if !sharpen {
        return mean;
    }
    // Sharpen is the blur subtracted rather than added: an unsharp mask.
    let c = tiles.pixel(x, y);
    Rgba {
        r: (c.r + (c.r - mean.r)).clamp(0.0, 1.0),
        g: (c.g + (c.g - mean.g)).clamp(0.0, 1.0),
        b: (c.b + (c.b - mean.b)).clamp(0.0, 1.0),
        a: c.a,
    }
}

/// Look up a prepared heal patch pixel.
fn heal_at(patch: &[Rgba], rect: IntRect, x: i32, y: i32) -> Option<Rgba> {
    if !rect.contains(x, y) {
        return None;
    }
    let w = rect.width() as usize;
    patch
        .get((y - rect.top) as usize * w + (x - rect.left) as usize)
        .copied()
}
