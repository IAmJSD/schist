//! Region-based retouching: patch, content-aware move, red eye and the
//! magic eraser.
//!
//! These all work on a whole region at once rather than a brush stroke, so
//! they live apart from the stroke engine in `tools-paint`. What they share
//! is [`inpaint`], a diffusion fill that grows the surrounding pixels
//! inwards over a hole. Photoshop's content-aware fill searches the image
//! for matching patches; diffusion is the honest simple relative -- it is
//! right for smooth surroundings and visibly blurry over texture, which is
//! at least a predictable failure.

use rayon::prelude::*;
use schist_color::Rgba;
use schist_core::{Document, IntRect, LayerId, Selection, TileCoord, TileMap, TILE_SIZE};
use schist_plugin_api::{
    EditorState, OptionValue, Overlay, PluginManifest, PluginRegistry, PointerInput, ToolCtx,
    ToolOption, ToolPlugin,
};

/// The layer a retouch tool should work on.
fn target_layer(doc: &Document) -> Option<LayerId> {
    let id = doc.active_layer?;
    doc.tree
        .find(id)
        .filter(|l| l.as_raster().is_some() && !l.locked)
        .map(|l| l.id)
}

fn layer_tiles(doc: &Document, layer: LayerId) -> Option<TileMap> {
    doc.tree
        .find(layer)
        .and_then(|l| l.as_raster())
        .map(|r| r.tiles.clone())
}

/// Write a rectangle of pixels into a layer as one history entry.
fn write_rect(
    doc: &mut Document,
    layer: LayerId,
    rect: IntRect,
    name: &str,
    px: impl Fn(i32, i32) -> Option<Rgba>,
) {
    if rect.is_empty() {
        return;
    }
    let mut edit = doc.begin_edit(name.to_string());
    for coord in TileCoord::covering(&rect) {
        let trect = coord.rect();
        let clip = trect.intersect(&rect);
        if clip.is_empty() {
            continue;
        }
        let Some(tile) = edit.writable_tile(layer, coord) else {
            break;
        };
        for y in clip.top..clip.bottom {
            for x in clip.left..clip.right {
                if let Some(c) = px(x, y) {
                    let ix = ((y - trect.top) * TILE_SIZE + (x - trect.left)) as usize;
                    tile.set(ix, c);
                }
            }
        }
    }
    edit.commit();
}

/// Fill the pixels where `hole` is true by diffusing in from the edge.
///
/// Jacobi relaxation over the hole: every pass replaces each hole pixel
/// with the average of its four neighbours, so colour seeps inwards from
/// the boundary until the patch is smooth. Enough passes to cross the
/// hole's smaller dimension is plenty.
pub fn inpaint(tiles: &TileMap, rect: IntRect, hole: &[bool]) -> Vec<Rgba> {
    let (w, h) = (rect.width().max(0) as usize, rect.height().max(0) as usize);
    let mut buf: Vec<Rgba> = Vec::with_capacity(w * h);
    for y in 0..h {
        for x in 0..w {
            buf.push(tiles.pixel(rect.left + x as i32, rect.top + y as i32));
        }
    }
    if w == 0 || h == 0 || !hole.iter().any(|&v| v) {
        return buf;
    }
    // Seed the hole with the mean of the boundary so relaxation starts
    // somewhere sensible instead of from whatever was there.
    let (mut acc, mut n) = ([0f32; 4], 0f32);
    for y in 0..h {
        for x in 0..w {
            if hole[y * w + x] {
                continue;
            }
            let touching = [(1i32, 0i32), (-1, 0), (0, 1), (0, -1)]
                .iter()
                .any(|(dx, dy)| {
                    let (sx, sy) = (x as i32 + dx, y as i32 + dy);
                    sx >= 0
                        && sy >= 0
                        && (sx as usize) < w
                        && (sy as usize) < h
                        && hole[sy as usize * w + sx as usize]
                });
            if touching {
                let c = buf[y * w + x];
                acc[0] += c.r;
                acc[1] += c.g;
                acc[2] += c.b;
                acc[3] += c.a;
                n += 1.0;
            }
        }
    }
    if n == 0.0 {
        return buf;
    }
    let seed = Rgba::new(acc[0] / n, acc[1] / n, acc[2] / n, acc[3] / n);
    for i in 0..buf.len() {
        if hole[i] {
            buf[i] = seed;
        }
    }
    let passes = (w.min(h) as u32).clamp(8, 160);
    let mut next = buf.clone();
    for _ in 0..passes {
        // A row at a time, in parallel. This runs synchronously on
        // pointer release: over a 1500x1500 selection it is 160 sweeps of
        // 2.25 M pixels, and the window locked up for seconds with no
        // cursor change to explain it. Each row reads `buf` and writes
        // only its own slice of `next`, so the passes stay exactly the
        // Jacobi iteration they were -- same output, spread over the
        // cores.
        next.par_chunks_mut(w).enumerate().for_each(|(y, out_row)| {
            for (x, out) in out_row.iter_mut().enumerate() {
                let i = y * w + x;
                if !hole[i] {
                    continue;
                }
                let (mut acc, mut n) = ([0f32; 4], 0f32);
                for (dx, dy) in [(1i32, 0i32), (-1, 0), (0, 1), (0, -1)] {
                    let (sx, sy) = (x as i32 + dx, y as i32 + dy);
                    if sx < 0 || sy < 0 || sx as usize >= w || sy as usize >= h {
                        continue;
                    }
                    let c = buf[sy as usize * w + sx as usize];
                    acc[0] += c.r;
                    acc[1] += c.g;
                    acc[2] += c.b;
                    acc[3] += c.a;
                    n += 1.0;
                }
                if n > 0.0 {
                    *out = Rgba::new(acc[0] / n, acc[1] / n, acc[2] / n, acc[3] / n);
                }
            }
        });
        std::mem::swap(&mut buf, &mut next);
    }
    buf
}

/// The selection's coverage as a hole mask over `rect`.
fn selection_hole(sel: &Selection, rect: IntRect) -> Vec<bool> {
    let (w, h) = (rect.width().max(0) as usize, rect.height().max(0) as usize);
    let mut out = vec![false; w * h];
    for y in 0..h {
        for x in 0..w {
            out[y * w + x] = sel.coverage(rect.left + x as i32, rect.top + y as i32) >= 128;
        }
    }
    out
}

/// Grow a rect by `by` and clip it to the canvas.
fn padded(rect: IntRect, by: i32, canvas: IntRect) -> IntRect {
    IntRect::new(
        rect.left - by,
        rect.top - by,
        rect.right + by,
        rect.bottom + by,
    )
    .intersect(&canvas)
}

// ---------------------------------------------------------------- patch

/// Patch tool: drag the selection onto an area that looks how you want it
/// to look, and the selection is refilled from there with its own colour
/// and lighting kept.
///
/// The same mean-shift trick as the healing brush: texture from the source,
/// colour from the destination.
pub struct PatchTool {
    drag: Option<((f32, f32), (f32, f32))>,
    /// Off means the drag picks the *destination* instead, Photoshop's
    /// "Destination" patch mode.
    source_mode: bool,
}

impl PatchTool {
    /// A tool with its default settings.
    pub fn default_tool() -> Self {
        Self::new()
    }

    fn new() -> Self {
        PatchTool {
            drag: None,
            source_mode: true,
        }
    }
}

impl ToolPlugin for PatchTool {
    fn id(&self) -> &'static str {
        "patch"
    }
    fn name(&self) -> &'static str {
        "Patch"
    }
    fn icon(&self) -> &'static str {
        "patch"
    }
    fn group(&self) -> &'static str {
        "heal"
    }

    fn options(&self) -> Vec<ToolOption> {
        vec![ToolOption::choice(
            "patch-mode",
            "Patch",
            &["Source", "Destination"],
            if self.source_mode { 0 } else { 1 },
        )]
    }

    fn set_option(&mut self, key: &str, value: OptionValue) {
        if key == "patch-mode" {
            self.source_mode = value.index() == 0;
        }
    }

    fn on_pointer_down(&mut self, _ctx: &mut ToolCtx, input: PointerInput) {
        self.drag = Some(((input.x, input.y), (input.x, input.y)));
    }

    fn on_pointer_move(&mut self, _ctx: &mut ToolCtx, input: PointerInput) {
        if let Some((_, to)) = self.drag.as_mut() {
            *to = (input.x, input.y);
        }
    }

    fn on_pointer_up(&mut self, ctx: &mut ToolCtx, input: PointerInput) {
        let Some((from, _)) = self.drag.take() else {
            return;
        };
        let (dx, dy) = (
            (input.x - from.0).round() as i32,
            (input.y - from.1).round() as i32,
        );
        if dx == 0 && dy == 0 {
            return;
        }
        let Some(layer) = target_layer(ctx.doc) else {
            return;
        };
        if ctx.doc.selection.is_empty() {
            return;
        }
        let canvas = ctx.doc.canvas_rect();
        let rect = ctx.doc.selection.bounds().intersect(&canvas);
        let Some(tiles) = layer_tiles(ctx.doc, layer) else {
            return;
        };
        let sel = ctx.doc.selection.clone();
        // In Source mode the drag points at where to sample from; in
        // Destination mode the selection's pixels are dropped there.
        let (dst_rect, off) = if self.source_mode {
            (rect, (dx, dy))
        } else {
            (
                IntRect::new(
                    rect.left + dx,
                    rect.top + dy,
                    rect.right + dx,
                    rect.bottom + dy,
                )
                .intersect(&canvas),
                (-dx, -dy),
            )
        };
        // Match colour across the *boundary*, not the interior.
        //
        // Averaging over the whole region would transfer the mean of
        // whatever is being removed -- patch out a red blemish and the
        // "healed" result comes back red. Photoshop solves a Poisson
        // equation to make the seam vanish; sampling the ring just outside
        // the region is the cheap version of the same idea, and it is what
        // makes the patch take the colour of its surroundings.
        let ring = padded(dst_rect, 5, canvas);
        let (mut sm, mut dm, mut n) = ([0f32; 3], [0f32; 3], 0f32);
        for y in ring.top..ring.bottom {
            for x in ring.left..ring.right {
                let sel_x = if self.source_mode { x } else { x - dx };
                let sel_y = if self.source_mode { y } else { y - dy };
                if sel.coverage(sel_x, sel_y) >= 128 {
                    continue; // inside the region being replaced
                }
                let sp = tiles.pixel(x + off.0, y + off.1);
                let dp = tiles.pixel(x, y);
                for c in 0..3 {
                    sm[c] += [sp.r, sp.g, sp.b][c];
                    dm[c] += [dp.r, dp.g, dp.b][c];
                }
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
        let source_mode = self.source_mode;
        write_rect(ctx.doc, layer, dst_rect, "Patch", |x, y| {
            let sel_x = if source_mode { x } else { x - dx };
            let sel_y = if source_mode { y } else { y - dy };
            let cov = sel.coverage(sel_x, sel_y) as f32 / 255.0;
            if cov <= 0.0 {
                return None;
            }
            let sp = tiles.pixel(x + off.0, y + off.1);
            let dp = tiles.pixel(x, y);
            let healed = Rgba {
                r: (sp.r + shift[0]).clamp(0.0, 1.0),
                g: (sp.g + shift[1]).clamp(0.0, 1.0),
                b: (sp.b + shift[2]).clamp(0.0, 1.0),
                a: sp.a,
            };
            Some(Rgba {
                r: dp.r + (healed.r - dp.r) * cov,
                g: dp.g + (healed.g - dp.g) * cov,
                b: dp.b + (healed.b - dp.b) * cov,
                a: dp.a + (healed.a - dp.a) * cov,
            })
        });
    }

    fn on_cancel(&mut self, _ctx: &mut ToolCtx) {
        self.drag = None;
    }

    fn overlays(&self, _doc: &Document, _state: &EditorState) -> Vec<Overlay> {
        match self.drag {
            Some((from, to)) => vec![Overlay::Line {
                x1: from.0,
                y1: from.1,
                x2: to.0,
                y2: to.1,
            }],
            None => Vec::new(),
        }
    }
}

// ------------------------------------------------- content-aware move

/// Drag the selected pixels somewhere else and fill in behind them.
pub struct ContentAwareMoveTool {
    drag: Option<((f32, f32), (f32, f32))>,
    /// On duplicates instead of moving, Photoshop's "Extend" mode.
    extend: bool,
}

impl ContentAwareMoveTool {
    /// A tool with its default settings.
    pub fn default_tool() -> Self {
        Self::new()
    }

    fn new() -> Self {
        ContentAwareMoveTool {
            drag: None,
            extend: false,
        }
    }
}

impl ToolPlugin for ContentAwareMoveTool {
    fn id(&self) -> &'static str {
        "content_aware_move"
    }
    fn name(&self) -> &'static str {
        "Content-Aware Move"
    }
    fn icon(&self) -> &'static str {
        "content-move"
    }
    fn group(&self) -> &'static str {
        "heal"
    }

    fn options(&self) -> Vec<ToolOption> {
        vec![ToolOption::choice(
            "cam-mode",
            "Mode",
            &["Move", "Extend"],
            self.extend as usize,
        )]
    }

    fn set_option(&mut self, key: &str, value: OptionValue) {
        if key == "cam-mode" {
            self.extend = value.index() == 1;
        }
    }

    fn on_pointer_down(&mut self, _ctx: &mut ToolCtx, input: PointerInput) {
        self.drag = Some(((input.x, input.y), (input.x, input.y)));
    }

    fn on_pointer_move(&mut self, _ctx: &mut ToolCtx, input: PointerInput) {
        if let Some((_, to)) = self.drag.as_mut() {
            *to = (input.x, input.y);
        }
    }

    fn on_pointer_up(&mut self, ctx: &mut ToolCtx, input: PointerInput) {
        let Some((from, _)) = self.drag.take() else {
            return;
        };
        let (dx, dy) = (
            (input.x - from.0).round() as i32,
            (input.y - from.1).round() as i32,
        );
        if dx == 0 && dy == 0 || ctx.doc.selection.is_empty() {
            return;
        }
        let Some(layer) = target_layer(ctx.doc) else {
            return;
        };
        let canvas = ctx.doc.canvas_rect();
        let src = ctx.doc.selection.bounds().intersect(&canvas);
        let Some(tiles) = layer_tiles(ctx.doc, layer) else {
            return;
        };
        let sel = ctx.doc.selection.clone();

        // Fill the hole the selection leaves behind, unless extending.
        let filled = if self.extend {
            Vec::new()
        } else {
            let pad = padded(src, 8, canvas);
            let hole = selection_hole(&sel, pad);
            inpaint(&tiles, pad, &hole)
        };
        let hole_rect = padded(src, 8, canvas);

        let dst = IntRect::new(src.left + dx, src.top + dy, src.right + dx, src.bottom + dy)
            .intersect(&canvas);
        let touched = hole_rect.union(&dst);
        let extend = self.extend;
        let hw = hole_rect.width().max(0) as usize;
        write_rect(ctx.doc, layer, touched, "Content-Aware Move", |x, y| {
            // Moved pixels win where the selection now lands.
            let cov = sel.coverage(x - dx, y - dy) as f32 / 255.0;
            if cov > 0.0 {
                let moved = tiles.pixel(x - dx, y - dy);
                let under = tiles.pixel(x, y);
                return Some(Rgba {
                    r: under.r + (moved.r - under.r) * cov,
                    g: under.g + (moved.g - under.g) * cov,
                    b: under.b + (moved.b - under.b) * cov,
                    a: under.a + (moved.a - under.a) * cov,
                });
            }
            if extend || !hole_rect.contains(x, y) {
                return None;
            }
            let cut = sel.coverage(x, y) as f32 / 255.0;
            if cut <= 0.0 {
                return None;
            }
            let i = (y - hole_rect.top) as usize * hw + (x - hole_rect.left) as usize;
            let patched = *filled.get(i)?;
            let orig = tiles.pixel(x, y);
            Some(Rgba {
                r: orig.r + (patched.r - orig.r) * cut,
                g: orig.g + (patched.g - orig.g) * cut,
                b: orig.b + (patched.b - orig.b) * cut,
                a: orig.a + (patched.a - orig.a) * cut,
            })
        });
    }

    fn on_cancel(&mut self, _ctx: &mut ToolCtx) {
        self.drag = None;
    }

    fn overlays(&self, _doc: &Document, _state: &EditorState) -> Vec<Overlay> {
        match self.drag {
            Some((from, to)) => vec![Overlay::Line {
                x1: from.0,
                y1: from.1,
                x2: to.0,
                y2: to.1,
            }],
            None => Vec::new(),
        }
    }
}

// -------------------------------------------------------------- red eye

/// Drag a box over a pupil and the red goes out of it.
pub struct RedEyeTool {
    anchor: Option<(f32, f32)>,
    current: Option<(f32, f32)>,
    /// How red a pixel has to be, 0..=1.
    amount: f32,
    /// How dark the corrected pupil ends up, 0..=1.
    darken: f32,
}

impl RedEyeTool {
    /// A tool with its default settings.
    pub fn default_tool() -> Self {
        Self::new()
    }

    fn new() -> Self {
        RedEyeTool {
            anchor: None,
            current: None,
            amount: 0.5,
            darken: 0.5,
        }
    }
}

impl ToolPlugin for RedEyeTool {
    fn id(&self) -> &'static str {
        "red_eye"
    }
    fn name(&self) -> &'static str {
        "Red Eye"
    }
    fn icon(&self) -> &'static str {
        "red-eye"
    }
    fn group(&self) -> &'static str {
        "heal"
    }

    fn options(&self) -> Vec<ToolOption> {
        vec![
            ToolOption::slider(
                "redeye-size",
                "Pupil Size",
                self.amount * 100.0,
                1.0,
                100.0,
                "%",
            ),
            ToolOption::slider(
                "redeye-darken",
                "Darken",
                self.darken * 100.0,
                0.0,
                100.0,
                "%",
            ),
        ]
    }

    fn set_option(&mut self, key: &str, value: OptionValue) {
        match key {
            "redeye-size" => self.amount = (value.num() / 100.0).clamp(0.01, 1.0),
            "redeye-darken" => self.darken = (value.num() / 100.0).clamp(0.0, 1.0),
            _ => {}
        }
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
        let Some(a) = self.anchor.take() else { return };
        self.current = None;
        let Some(layer) = target_layer(ctx.doc) else {
            return;
        };
        // A click with no drag still treats a small box around it as the
        // pupil, which is how the tool is usually used.
        let mut rect = IntRect::new(
            a.0.min(input.x).floor() as i32,
            a.1.min(input.y).floor() as i32,
            a.0.max(input.x).ceil() as i32,
            a.1.max(input.y).ceil() as i32,
        );
        if rect.width() < 3 || rect.height() < 3 {
            let r = 12;
            rect = IntRect::new(
                a.0 as i32 - r,
                a.1 as i32 - r,
                a.0 as i32 + r,
                a.1 as i32 + r,
            );
        }
        let rect = rect.intersect(&ctx.doc.canvas_rect());
        let Some(tiles) = layer_tiles(ctx.doc, layer) else {
            return;
        };
        let (threshold, darken) = (1.0 - self.amount, self.darken);
        write_rect(ctx.doc, layer, rect, "Red Eye", |x, y| {
            let c = tiles.pixel(x, y);
            if c.a <= 0.0 {
                return None;
            }
            // Redness: how much the red channel leads the others.
            let other = c.g.max(c.b);
            let redness = if c.r > 1e-4 { (c.r - other) / c.r } else { 0.0 };
            if redness <= threshold {
                return None;
            }
            // Replace red with the green/blue level, then darken, which is
            // what turns a glowing pupil back into a pupil.
            let grey = (c.g + c.b) / 2.0;
            let k = 1.0 - darken;
            Some(Rgba {
                r: grey * k,
                g: c.g * k,
                b: c.b * k,
                a: c.a,
            })
        });
    }

    fn on_cancel(&mut self, _ctx: &mut ToolCtx) {
        self.anchor = None;
        self.current = None;
    }

    fn overlays(&self, _doc: &Document, _state: &EditorState) -> Vec<Overlay> {
        match (self.anchor, self.current) {
            (Some(a), Some(c)) => vec![Overlay::AntsRect(IntRect::new(
                a.0.min(c.0) as i32,
                a.1.min(c.1) as i32,
                a.0.max(c.0) as i32,
                a.1.max(c.1) as i32,
            ))],
            _ => Vec::new(),
        }
    }
}

// --------------------------------------------------------- magic eraser

/// Click and every pixel matching what you clicked becomes transparent.
pub struct MagicEraserTool {
    tolerance: u8,
    contiguous: bool,
}

impl MagicEraserTool {
    /// A tool with its default settings.
    pub fn default_tool() -> Self {
        Self::new()
    }

    fn new() -> Self {
        MagicEraserTool {
            tolerance: 32,
            contiguous: true,
        }
    }
}

impl ToolPlugin for MagicEraserTool {
    fn id(&self) -> &'static str {
        "magic_eraser"
    }
    fn name(&self) -> &'static str {
        "Magic Eraser"
    }
    fn icon(&self) -> &'static str {
        "eraser-magic"
    }
    fn group(&self) -> &'static str {
        "eraser"
    }

    fn options(&self) -> Vec<ToolOption> {
        vec![
            ToolOption::slider(
                "me-tolerance",
                "Tolerance",
                self.tolerance as f32,
                0.0,
                255.0,
                "",
            ),
            ToolOption::toggle("me-contiguous", "Contiguous", self.contiguous),
        ]
    }

    fn set_option(&mut self, key: &str, value: OptionValue) {
        match key {
            "me-tolerance" => self.tolerance = value.num().round().clamp(0.0, 255.0) as u8,
            "me-contiguous" => self.contiguous = value.bool(),
            _ => {}
        }
    }

    fn on_pointer_down(&mut self, ctx: &mut ToolCtx, input: PointerInput) {
        let Some(layer) = target_layer(ctx.doc) else {
            return;
        };
        let canvas = ctx.doc.canvas_rect();
        let (x, y) = (input.x.floor() as i32, input.y.floor() as i32);
        if !canvas.contains(x, y) {
            return;
        }
        let Some(tiles) = layer_tiles(ctx.doc, layer) else {
            return;
        };
        let target = tiles.pixel(x, y).to_u8();
        let tol = self.tolerance as i32;
        let matches = |p: [u8; 4]| {
            p.iter()
                .zip(target.iter())
                .all(|(&a, &b)| (a as i32 - b as i32).abs() <= tol)
        };
        let sel = ctx.doc.selection.clone();

        let mut erase: Vec<(i32, i32)> = Vec::new();
        if self.contiguous {
            let w = canvas.width() as usize;
            let mut seen = vec![false; w * canvas.height() as usize];
            let mut stack = vec![(x, y)];
            seen[(y - canvas.top) as usize * w + (x - canvas.left) as usize] = true;
            while let Some((cx, cy)) = stack.pop() {
                erase.push((cx, cy));
                for (nx, ny) in [(cx - 1, cy), (cx + 1, cy), (cx, cy - 1), (cx, cy + 1)] {
                    if !canvas.contains(nx, ny) {
                        continue;
                    }
                    let ix = (ny - canvas.top) as usize * w + (nx - canvas.left) as usize;
                    if seen[ix] {
                        continue;
                    }
                    seen[ix] = true;
                    if matches(tiles.pixel(nx, ny).to_u8()) {
                        stack.push((nx, ny));
                    }
                }
            }
        } else {
            for py in canvas.top..canvas.bottom {
                for px in canvas.left..canvas.right {
                    if matches(tiles.pixel(px, py).to_u8()) {
                        erase.push((px, py));
                    }
                }
            }
        }
        if erase.is_empty() {
            return;
        }
        let set: std::collections::HashSet<(i32, i32)> = erase.iter().copied().collect();
        let mut rect = IntRect::EMPTY;
        for (x, y) in &erase {
            rect = rect.union(&IntRect::new(*x, *y, *x + 1, *y + 1));
        }
        write_rect(ctx.doc, layer, rect, "Magic Eraser", |x, y| {
            if !set.contains(&(x, y)) {
                return None;
            }
            let cov = sel.coverage(x, y) as f32 / 255.0;
            if cov <= 0.0 {
                return None;
            }
            let c = tiles.pixel(x, y);
            Some(Rgba {
                a: c.a * (1.0 - cov),
                ..c
            })
        });
    }

    fn on_pointer_move(&mut self, _ctx: &mut ToolCtx, _input: PointerInput) {}
    fn on_pointer_up(&mut self, _ctx: &mut ToolCtx, _input: PointerInput) {}
}

pub struct RetouchToolsPlugin;

impl PluginManifest for RetouchToolsPlugin {
    fn id(&self) -> &'static str {
        "schist.tools-retouch"
    }

    fn register(&self, registry: &mut PluginRegistry) {
        registry.register_tool(Box::new(PatchTool::new()));
        registry.register_tool(Box::new(ContentAwareMoveTool::new()));
        registry.register_tool(Box::new(RedEyeTool::new()));
        registry.register_tool(Box::new(MagicEraserTool::new()));
    }
}
