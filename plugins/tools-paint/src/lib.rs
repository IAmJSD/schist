//! Painting tools: brush, pencil, eraser — plus the shared stroke engine.
//!
//! Photoshop opacity semantics: within one stroke, overlapping dabs
//! accumulate *coverage* (max, not sum), and the stroke composites onto the
//! pre-stroke pixels at `coverage × tool opacity`. So scribbling over the
//! same spot at 50% opacity stays 50%, but two separate strokes darken.

use photoslop_color::Rgba;
use photoslop_core::{
    Document, IntRect, LayerId, LayerKind, StrokeEdit, TileCoord, TileMap, TILE_SIZE,
};
use photoslop_plugin_api::{
    EditorState, Overlay, PluginManifest, PluginRegistry, PointerInput, ToolCtx, ToolPlugin,
};
use rustc_hash::FxHashMap;

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
}

impl Stroke {
    fn begin(ctx: &mut ToolCtx, input: PointerInput, mode: PaintMode, ink: Ink) -> Option<Stroke> {
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
                    };
                    tile.set(ix, out);
                }
            }
            doc.add_damage(clip);
        }
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
}

impl PaintTool {
    fn new(mode: PaintMode) -> Self {
        PaintTool {
            mode,
            stroke: None,
            cursor: None,
            clone_source: None,
            clone_offset: None,
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
        })
    }
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
        }
    }

    fn on_pointer_down(&mut self, ctx: &mut ToolCtx, input: PointerInput) {
        self.cursor = Some((input.x, input.y));
        // Alt-click sets the clone stamp's source point.
        if self.mode == PaintMode::Clone && input.modifiers.alt {
            self.clone_source = Some((input.x, input.y));
            self.clone_offset = None;
            return;
        }
        let Some(ink) = self.ink_for(ctx, input) else {
            return;
        };
        self.stroke = Stroke::begin(ctx, input, self.mode, ink);
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

/// Gradient tool: drag to define the axis, release to fill.
pub struct GradientTool {
    pub kind: GradientKind,
    /// Fade the foreground out instead of ending on the background colour.
    pub to_transparent: bool,
    anchor: Option<(f32, f32)>,
    current: Option<(f32, f32)>,
}

impl GradientTool {
    fn new(kind: GradientKind) -> GradientTool {
        GradientTool {
            kind,
            to_transparent: false,
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
        fill_gradient(ctx, self.kind, (ax, ay), (bx, by), self.to_transparent);
    }

    fn on_cancel(&mut self, _ctx: &mut ToolCtx) {
        self.anchor = None;
        self.current = None;
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

fn fill_gradient(
    ctx: &mut ToolCtx,
    kind: GradientKind,
    from: (f32, f32),
    to: (f32, f32),
    to_transparent: bool,
) {
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
                let t = match kind {
                    GradientKind::Linear => {
                        (((px - from.0) * dx + (py - from.1) * dy) / len_sq).clamp(0.0, 1.0)
                    }
                    GradientKind::Radial => {
                        ((px - from.0).hypot(py - from.1) / radius).clamp(0.0, 1.0)
                    }
                };
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
}

impl BucketTool {
    fn new() -> BucketTool {
        BucketTool { tolerance: 32 }
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
        let target = tiles.pixel(x, y).to_u8();
        let tol = self.tolerance as i32;
        let matches = |px: [u8; 4]| {
            px.iter()
                .zip(target.iter())
                .all(|(&a, &b)| (a as i32 - b as i32).abs() <= tol)
        };

        // 4-connected flood fill bounded by the canvas and the selection.
        let w = canvas.width() as usize;
        let mut visited = vec![false; w * canvas.height() as usize];
        let mut filled: Vec<(i32, i32)> = Vec::new();
        let mut stack = vec![(x, y)];
        visited[y as usize * w + x as usize] = true;
        let selection = ctx.doc.selection.clone();
        while let Some((cx, cy)) = stack.pop() {
            if selection.coverage(cx, cy) == 0 {
                continue;
            }
            filled.push((cx, cy));
            for (nx, ny) in [(cx - 1, cy), (cx + 1, cy), (cx, cy - 1), (cx, cy + 1)] {
                if !canvas.contains(nx, ny) {
                    continue;
                }
                let ix = ny as usize * w + nx as usize;
                if visited[ix] {
                    continue;
                }
                visited[ix] = true;
                if matches(tiles.pixel(nx, ny).to_u8()) {
                    stack.push((nx, ny));
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

    fn on_pointer_move(&mut self, _ctx: &mut ToolCtx, _input: PointerInput) {}
    fn on_pointer_up(&mut self, _ctx: &mut ToolCtx, _input: PointerInput) {}
}

pub struct PaintToolsPlugin;

impl PluginManifest for PaintToolsPlugin {
    fn id(&self) -> &'static str {
        "photoslop.tools-paint"
    }

    fn register(&self, registry: &mut PluginRegistry) {
        registry.register_tool(Box::new(PaintTool::new(PaintMode::Brush)));
        registry.register_tool(Box::new(PaintTool::new(PaintMode::Pencil)));
        registry.register_tool(Box::new(PaintTool::new(PaintMode::Eraser)));
        registry.register_tool(Box::new(PaintTool::new(PaintMode::Clone)));
        registry.register_tool(Box::new(PaintTool::new(PaintMode::Dodge)));
        registry.register_tool(Box::new(PaintTool::new(PaintMode::Burn)));
        registry.register_tool(Box::new(PaintTool::new(PaintMode::Sponge)));
        registry.register_tool(Box::new(GradientTool::new(GradientKind::Linear)));
        registry.register_tool(Box::new(GradientTool::new(GradientKind::Radial)));
        registry.register_tool(Box::new(BucketTool::new()));
    }
}

// Silence unused-dep warning: pixel-ops is used by future paint modes
// (brush blend modes); keep the wiring alive.
#[allow(unused_imports)]
use photoslop_pixel_ops as _;

#[cfg(test)]
mod tests {
    use super::*;
    use photoslop_color::Depth;
    use photoslop_core::Layer;
    use photoslop_plugin_api::Modifiers;

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
        use photoslop_core::SelectOp;
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
    use photoslop_color::Depth;
    use photoslop_core::{blit_rgba8, Layer, SelectOp};
    use photoslop_plugin_api::Modifiers;

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
