//! Liquify.
//!
//! Photoshop puts this in a modal dialog with its own preview; here it is
//! a canvas tool that warps in place, which is the same set of brushes
//! against the real document. Enter bakes the mesh into the layer, Escape
//! throws it away.

use photoslop_core::{Document, IntRect, TileMap};
use photoslop_plugin_api::{
    EditorState, OptionValue, Overlay, PointerInput, ToolCtx, ToolOption, ToolPlugin,
};

use crate::mesh::{warp_tiles, Mesh};

/// Photoshop's Liquify brushes.
const MODES: &[&str] = &[
    "Forward Warp",
    "Reconstruct",
    "Twirl CW",
    "Twirl CCW",
    "Pucker",
    "Bloat",
    "Push Left",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Forward,
    Reconstruct,
    TwirlCw,
    TwirlCcw,
    Pucker,
    Bloat,
    PushLeft,
}

impl Mode {
    fn from_index(i: usize) -> Mode {
        match i {
            1 => Mode::Reconstruct,
            2 => Mode::TwirlCw,
            3 => Mode::TwirlCcw,
            4 => Mode::Pucker,
            5 => Mode::Bloat,
            6 => Mode::PushLeft,
            _ => Mode::Forward,
        }
    }

    fn index(self) -> usize {
        match self {
            Mode::Forward => 0,
            Mode::Reconstruct => 1,
            Mode::TwirlCw => 2,
            Mode::TwirlCcw => 3,
            Mode::Pucker => 4,
            Mode::Bloat => 5,
            Mode::PushLeft => 6,
        }
    }
}

struct Session {
    layer: photoslop_core::LayerId,
    /// The layer as it was before any warping. Every render goes from
    /// here, so the mesh is applied once rather than compounding.
    original: TileMap,
    mesh: Mesh,
    last: Option<(f32, f32)>,
}

pub struct LiquifyTool {
    mode: Mode,
    size: f32,
    pressure: f32,
    session: Option<Session>,
    cursor: Option<(f32, f32)>,
}

impl LiquifyTool {
    pub fn new() -> Self {
        LiquifyTool {
            mode: Mode::Forward,
            size: 100.0,
            pressure: 50.0,
            session: None,
            cursor: None,
        }
    }

    fn begin(&mut self, ctx: &mut ToolCtx) {
        let Some(id) = ctx.doc.active_layer else {
            return;
        };
        let Some(raster) = ctx
            .doc
            .tree
            .find(id)
            .filter(|l| !l.locked)
            .and_then(|l| l.as_raster())
        else {
            return;
        };
        // Warp over the artwork grown by one brush, so pixels can be
        // pushed outwards past the original edge.
        let rect = raster
            .tiles
            .content_bounds()
            .inflated(self.size.ceil() as i32)
            .intersect(&ctx.doc.canvas_rect());
        if rect.is_empty() {
            return;
        }
        self.session = Some(Session {
            layer: id,
            original: raster.tiles.clone(),
            mesh: Mesh::new(rect),
            last: None,
        });
    }

    /// Re-render the layer from the snapshot through the current mesh.
    fn render(&self, doc: &mut Document) {
        let Some(session) = &self.session else { return };
        let depth = doc.depth;
        let clip = doc.canvas_rect();
        let warped = warp_tiles(&session.original, &session.mesh, depth, clip);
        // Anything the mesh does not cover keeps its original pixels.
        let mut tiles = session.original.clone();
        for (coord, buf) in warped.iter() {
            *tiles.get_mut_or_insert(*coord, depth) = (**buf).clone();
        }
        if let Some(raster) = doc
            .tree
            .find_mut(session.layer)
            .and_then(|l| l.as_raster_mut())
        {
            raster.tiles = tiles;
        }
        doc.add_damage(session.mesh.rect);
    }

    /// Apply one dab of the current brush to the mesh.
    fn dab(&mut self, x: f32, y: f32, dx: f32, dy: f32) {
        let Some(session) = &mut self.session else {
            return;
        };
        let radius = self.size / 2.0;
        let strength = (self.pressure / 100.0).clamp(0.0, 1.0);
        match self.mode {
            Mode::Forward => {
                // Fetch from behind the drag, which drags pixels along.
                session.mesh.for_each_near(x, y, radius, |off, w, _, _| {
                    off.0 -= dx * w * strength;
                    off.1 -= dy * w * strength;
                });
            }
            Mode::PushLeft => {
                // Perpendicular to the drag: pixels slide to its left.
                let len = dx.hypot(dy);
                if len < 1e-4 {
                    return;
                }
                let (px, py) = (dy / len, -dx / len);
                let push = len * strength;
                session.mesh.for_each_near(x, y, radius, |off, w, _, _| {
                    off.0 -= px * push * w;
                    off.1 -= py * push * w;
                });
            }
            Mode::Reconstruct => session.mesh.relax(x, y, radius, strength * 0.5),
            Mode::TwirlCw | Mode::TwirlCcw => {
                let sign = if self.mode == Mode::TwirlCw {
                    1.0
                } else {
                    -1.0
                };
                let step = strength * 0.25 * sign;
                session.mesh.for_each_near(x, y, radius, |off, w, rx, ry| {
                    // Rotate the fetch point about the brush centre.
                    let a = step * w;
                    let (s, c) = a.sin_cos();
                    let (fx, fy) = (rx + off.0, ry + off.1);
                    off.0 = fx * c - fy * s - rx;
                    off.1 = fx * s + fy * c - ry;
                });
            }
            Mode::Pucker | Mode::Bloat => {
                // Pucker pulls pixels in, so it fetches from further out.
                let sign = if self.mode == Mode::Pucker { 1.0 } else { -1.0 };
                let step = strength * 0.15 * sign;
                session.mesh.for_each_near(x, y, radius, |off, w, rx, ry| {
                    let (fx, fy) = (rx + off.0, ry + off.1);
                    off.0 += fx * step * w;
                    off.1 += fy * step * w;
                });
            }
        }
    }

    fn commit(&mut self, ctx: &mut ToolCtx) {
        let Some(session) = self.session.take() else {
            return;
        };
        self.cursor = None;
        if session.mesh.is_identity() {
            return;
        }
        // Put the original back so the recorded edit has the right before,
        // then write the warped result as one entry.
        let depth = ctx.doc.depth;
        let clip = ctx.doc.canvas_rect();
        let warped = warp_tiles(&session.original, &session.mesh, depth, clip);
        let mut tiles = session.original.clone();
        for (coord, buf) in warped.iter() {
            *tiles.get_mut_or_insert(*coord, depth) = (**buf).clone();
        }
        if let Some(raster) = ctx
            .doc
            .tree
            .find_mut(session.layer)
            .and_then(|l| l.as_raster_mut())
        {
            raster.tiles = session.original.clone();
        }
        let mut edit = ctx.doc.begin_edit("Liquify");
        edit.replace_layer_tiles(session.layer, tiles);
        edit.commit();
    }
}

impl Default for LiquifyTool {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolPlugin for LiquifyTool {
    fn id(&self) -> &'static str {
        "liquify"
    }
    fn name(&self) -> &'static str {
        "Liquify"
    }
    fn icon(&self) -> &'static str {
        "liquify"
    }
    fn group(&self) -> &'static str {
        "warp"
    }

    fn options(&self) -> Vec<ToolOption> {
        vec![
            ToolOption::choice("liquify-mode", "Tool", MODES, self.mode.index()),
            ToolOption::slider("liquify-size", "Size", self.size, 10.0, 1000.0, " px"),
            ToolOption::slider(
                "liquify-pressure",
                "Pressure",
                self.pressure,
                1.0,
                100.0,
                "",
            ),
        ]
    }

    fn set_option(&mut self, key: &str, value: OptionValue) {
        match key {
            "liquify-mode" => self.mode = Mode::from_index(value.index()),
            "liquify-size" => self.size = value.num(),
            "liquify-pressure" => self.pressure = value.num(),
            _ => {}
        }
    }

    fn on_activate(&mut self, ctx: &mut ToolCtx) {
        self.begin(ctx);
    }

    fn on_pointer_down(&mut self, ctx: &mut ToolCtx, input: PointerInput) {
        if self.session.is_none() {
            self.begin(ctx);
        }
        if let Some(session) = &mut self.session {
            session.last = Some((input.x, input.y));
        }
        // A click with no drag still applies the radial brushes.
        self.dab(input.x, input.y, 0.0, 0.0);
        self.render(ctx.doc);
    }

    fn on_pointer_move(&mut self, ctx: &mut ToolCtx, input: PointerInput) {
        self.cursor = Some((input.x, input.y));
        let Some(last) = self.session.as_ref().and_then(|s| s.last) else {
            return;
        };
        let (dx, dy) = (input.x - last.0, input.y - last.1);
        if dx.hypot(dy) < 1.0 {
            return;
        }
        self.dab(input.x, input.y, dx, dy);
        if let Some(session) = &mut self.session {
            session.last = Some((input.x, input.y));
        }
        self.render(ctx.doc);
    }

    fn on_pointer_up(&mut self, _ctx: &mut ToolCtx, _input: PointerInput) {
        if let Some(session) = &mut self.session {
            session.last = None;
        }
    }

    fn on_commit(&mut self, ctx: &mut ToolCtx) {
        self.commit(ctx);
        self.begin(ctx);
    }

    fn on_deactivate(&mut self, ctx: &mut ToolCtx) {
        self.commit(ctx);
    }

    fn on_cancel(&mut self, ctx: &mut ToolCtx) {
        // Throw the mesh away and put the pixels back.
        if let Some(session) = self.session.take() {
            if let Some(raster) = ctx
                .doc
                .tree
                .find_mut(session.layer)
                .and_then(|l| l.as_raster_mut())
            {
                raster.tiles = session.original;
            }
            ctx.doc.add_damage(session.mesh.rect);
        }
        self.cursor = None;
    }

    fn overlays(&self, _doc: &Document, _state: &EditorState) -> Vec<Overlay> {
        match self.cursor {
            Some((cx, cy)) => vec![Overlay::Circle {
                cx,
                cy,
                r: self.size / 2.0,
            }],
            None => Vec::new(),
        }
    }
}

/// Exposed so tests can drive a warp without standing up a tool session.
pub fn liquify_region(
    src: &TileMap,
    rect: IntRect,
    mesh: &Mesh,
    depth: photoslop_color::Depth,
) -> TileMap {
    warp_tiles(src, mesh, depth, rect)
}
