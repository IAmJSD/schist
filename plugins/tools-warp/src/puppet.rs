//! Puppet Warp: drop pins on the artwork, drag them, and everything
//! between follows.
//!
//! Photoshop triangulates a mesh over the subject and solves a
//! rigid-as-possible deformation. This uses Moving Least Squares with a
//! similarity constraint, from Schaefer et al.'s "Image Deformation Using
//! Moving Least Squares": for each point, weight every pin by the inverse
//! square of its distance and solve for the best similarity transform
//! under those weights. It bends the same way -- pins hold, the space
//! between them stretches smoothly, and nothing shears -- and it needs no
//! triangulation.

use schist_core::{Document, IntRect, LayerId, TileMap};
use schist_plugin_api::{
    EditorState, OptionValue, Overlay, PointerInput, ToolCtx, ToolOption, ToolPlugin,
};

use crate::mesh::{warp_tiles, Mesh};

/// A control pin: where it started, and where it has been dragged to.
#[derive(Debug, Clone, Copy)]
pub struct Pin {
    pub from: (f32, f32),
    pub to: (f32, f32),
}

/// Where a point in the *deformed* image should be fetched from.
///
/// Solved backwards -- destination to source -- because that is what a
/// resampler needs. With no pins, or with every pin left where it started,
/// this is the identity.
pub fn mls_inverse(pins: &[Pin], x: f32, y: f32, stiffness: f32) -> (f32, f32) {
    if pins.is_empty() {
        return (x, y);
    }
    // Weights by inverse distance to each pin's *current* position, since
    // we are mapping from the deformed image back to the original.
    let mut weights = Vec::with_capacity(pins.len());
    let mut total = 0.0f32;
    for p in pins {
        let d2 = (p.to.0 - x).powi(2) + (p.to.1 - y).powi(2);
        if d2 < 1e-8 {
            // Exactly on a pin: it maps to its own origin.
            return p.from;
        }
        let w = 1.0 / d2.powf(stiffness.clamp(0.5, 3.0));
        weights.push(w);
        total += w;
    }
    if total <= 0.0 || !total.is_finite() {
        return (x, y);
    }

    // Weighted centroids of both point sets.
    let mut p_star = (0.0f32, 0.0f32);
    let mut q_star = (0.0f32, 0.0f32);
    for (p, w) in pins.iter().zip(&weights) {
        p_star.0 += p.to.0 * w;
        p_star.1 += p.to.1 * w;
        q_star.0 += p.from.0 * w;
        q_star.1 += p.from.1 * w;
    }
    p_star = (p_star.0 / total, p_star.1 / total);
    q_star = (q_star.0 / total, q_star.1 / total);

    // Similarity MLS: mu is the weighted squared spread of the source
    // points, and the sum below is the rotation/scale that best carries
    // them onto the destinations.
    let mut mu = 0.0f32;
    let mut acc = (0.0f32, 0.0f32);
    let v = (x - p_star.0, y - p_star.1);
    for (p, w) in pins.iter().zip(&weights) {
        let ph = (p.to.0 - p_star.0, p.to.1 - p_star.1);
        let qh = (p.from.0 - q_star.0, p.from.1 - q_star.1);
        mu += w * (ph.0 * ph.0 + ph.1 * ph.1);
        // Schaefer's A_i is w * [ph; -ph_perp] * [v; -v_perp]^T, which
        // multiplies out to w * [[a, b], [-b, a]] with `a` the dot
        // product and `b` the cross. Left-multiplying the row vector qh
        // by that gives the two lines below; getting either sign wrong
        // turns the deformation into a shear, which is what the
        // "a uniform drag is a translation" test catches.
        let a = ph.0 * v.0 + ph.1 * v.1;
        let b = ph.0 * v.1 - ph.1 * v.0;
        acc.0 += w * (qh.0 * a - qh.1 * b);
        acc.1 += w * (qh.0 * b + qh.1 * a);
    }
    if mu.abs() < 1e-8 || !mu.is_finite() {
        return (x, y);
    }
    let out = (acc.0 / mu + q_star.0, acc.1 / mu + q_star.1);
    if out.0.is_finite() && out.1.is_finite() {
        out
    } else {
        (x, y)
    }
}

/// Build a displacement mesh from a set of pins.
pub fn mesh_from_pins(rect: IntRect, pins: &[Pin], stiffness: f32) -> Mesh {
    let mut mesh = Mesh::new(rect);
    for row in 0..mesh.rows {
        for col in 0..mesh.cols {
            let (vx, vy) = mesh.vertex_pos(col, row);
            let (sx, sy) = mls_inverse(pins, vx, vy, stiffness);
            mesh.offsets[row * mesh.cols + col] = (sx - vx, sy - vy);
        }
    }
    mesh
}

struct Session {
    layer: LayerId,
    original: TileMap,
    /// Names `original` for a backend that can keep it resident: the pins
    /// move, the snapshot behind them does not.
    token: u64,
    rect: IntRect,
}

pub struct PuppetWarpTool {
    pins: Vec<Pin>,
    dragging: Option<usize>,
    stiffness: f32,
    session: Option<Session>,
}

impl PuppetWarpTool {
    pub fn new() -> Self {
        PuppetWarpTool {
            pins: Vec::new(),
            dragging: None,
            stiffness: 1.0,
            session: None,
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
        let rect = raster
            .tiles
            .content_bounds()
            .inflated(64)
            .intersect(&ctx.doc.canvas_rect());
        if rect.is_empty() {
            return;
        }
        self.pins.clear();
        self.session = Some(Session {
            layer: id,
            original: raster.tiles.clone(),
            token: crate::mesh::next_source_token(),
            rect,
        });
    }

    fn render(&self, doc: &mut Document) {
        let Some(session) = &self.session else { return };
        if self.pins.iter().all(|p| p.from == p.to) {
            // Nothing has been dragged yet: leave the pixels alone.
            if let Some(raster) = doc
                .tree
                .find_mut(session.layer)
                .and_then(|l| l.as_raster_mut())
            {
                raster.tiles = session.original.clone();
            }
            doc.add_damage(session.rect);
            return;
        }
        let depth = doc.depth;
        let mesh = mesh_from_pins(session.rect, &self.pins, self.stiffness);
        let warped = warp_tiles(
            &session.original,
            &mesh,
            depth,
            doc.canvas_rect(),
            session.token,
        );
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
        doc.add_damage(session.rect);
    }

    fn commit(&mut self, ctx: &mut ToolCtx) {
        let Some(session) = self.session.take() else {
            return;
        };
        // Take the pins rather than clearing them: the mesh below is built
        // from them, so clearing first solves an empty pin set, which is
        // the identity, and throws the whole warp away.
        let pins = std::mem::take(&mut self.pins);
        if !pins.iter().any(|p| p.from != p.to) {
            return;
        }
        let depth = ctx.doc.depth;
        let mesh = mesh_from_pins(session.rect, &pins, self.stiffness);
        let warped = warp_tiles(
            &session.original,
            &mesh,
            depth,
            ctx.doc.canvas_rect(),
            session.token,
        );
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
        let mut edit = ctx.doc.begin_edit("Puppet Warp");
        edit.replace_layer_tiles(session.layer, tiles);
        edit.commit();
    }
}

impl Default for PuppetWarpTool {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolPlugin for PuppetWarpTool {
    fn id(&self) -> &'static str {
        "puppet_warp"
    }
    fn name(&self) -> &'static str {
        "Puppet Warp"
    }
    fn description(&self) -> &'static str {
        "Click to pin the layer at a few points, then drag one: the mesh bends around the \
         pins, stiffly or loosely as the Stiffness option says. Commit applies it."
    }
    fn icon(&self) -> &'static str {
        "puppet"
    }
    fn group(&self) -> &'static str {
        "warp"
    }

    fn options(&self) -> Vec<ToolOption> {
        vec![ToolOption::slider(
            "puppet-stiffness",
            "Stiffness",
            self.stiffness,
            0.5,
            3.0,
            "",
        )]
    }

    fn set_option(&mut self, key: &str, value: OptionValue) {
        if key == "puppet-stiffness" {
            self.stiffness = value.num();
        }
    }

    fn on_activate(&mut self, ctx: &mut ToolCtx) {
        self.begin(ctx);
    }

    fn on_pointer_down(&mut self, ctx: &mut ToolCtx, input: PointerInput) {
        if self.session.is_none() {
            self.begin(ctx);
        }
        let r = 10.0 / ctx.state.zoom.max(0.01);
        // Alt-click removes a pin; clicking one grabs it; anywhere else
        // drops a new one.
        if let Some(i) = self
            .pins
            .iter()
            .position(|p| (p.to.0 - input.x).hypot(p.to.1 - input.y) <= r)
        {
            if input.modifiers.alt {
                self.pins.remove(i);
                self.render(ctx.doc);
            } else {
                self.dragging = Some(i);
            }
            return;
        }
        self.pins.push(Pin {
            from: (input.x, input.y),
            to: (input.x, input.y),
        });
        self.dragging = Some(self.pins.len() - 1);
    }

    fn on_pointer_move(&mut self, ctx: &mut ToolCtx, input: PointerInput) {
        let Some(i) = self.dragging else { return };
        if let Some(pin) = self.pins.get_mut(i) {
            pin.to = (input.x, input.y);
        }
        self.render(ctx.doc);
    }

    fn on_pointer_up(&mut self, _ctx: &mut ToolCtx, _input: PointerInput) {
        self.dragging = None;
    }

    fn on_commit(&mut self, ctx: &mut ToolCtx) {
        self.commit(ctx);
        self.begin(ctx);
    }

    fn on_deactivate(&mut self, ctx: &mut ToolCtx) {
        self.commit(ctx);
    }

    fn on_cancel(&mut self, ctx: &mut ToolCtx) {
        if let Some(session) = self.session.take() {
            if let Some(raster) = ctx
                .doc
                .tree
                .find_mut(session.layer)
                .and_then(|l| l.as_raster_mut())
            {
                raster.tiles = session.original;
            }
            ctx.doc.add_damage(session.rect);
        }
        self.pins.clear();
    }

    fn overlays(&self, _doc: &Document, _state: &EditorState) -> Vec<Overlay> {
        let mut out = Vec::new();
        for pin in &self.pins {
            out.push(Overlay::Circle {
                cx: pin.to.0,
                cy: pin.to.1,
                r: 5.0,
            });
            if pin.from != pin.to {
                // A line back to where the pin started, so it is obvious
                // which way each one has been pulled.
                out.push(Overlay::Line {
                    x1: pin.from.0,
                    y1: pin.from.1,
                    x2: pin.to.0,
                    y2: pin.to.1,
                });
            }
        }
        out
    }
}
