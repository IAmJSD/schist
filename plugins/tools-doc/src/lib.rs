//! Tools that edit the document's furniture rather than its pixels:
//! artboards, frames, slices, notes and counts.
//!
//! They share a shape -- drag out a rectangle or click a point, and the
//! result goes into a list on the `Document` -- so they share this crate
//! and the small amount of hit-testing and drawing that goes with it.

use schist_color::Rgba;
use schist_core::{
    Artboard, CountGroup, Document, IntRect, Layer, LayerMask, MaskTileMap, Note, Slice, TileCoord,
    TILE_SIZE,
};
use schist_plugin_api::{
    EditorState, OptionValue, Overlay, PluginManifest, PluginRegistry, PointerInput, ToolCtx,
    ToolOption, ToolPlugin,
};

/// A tool that drags out a rectangle. Artboards, slices and frames differ
/// only in what they do with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RectKind {
    Artboard,
    Slice,
    Frame,
}

pub struct RectTool {
    kind: RectKind,
    anchor: Option<(f32, f32)>,
    current: Option<(f32, f32)>,
    /// Frames only: draw an ellipse rather than a rectangle.
    ellipse: bool,
    /// Whichever existing item is being dragged, if any.
    grabbed: Option<usize>,
    last: Option<(f32, f32)>,
}

impl RectTool {
    pub fn new(kind: RectKind) -> Self {
        RectTool {
            kind,
            anchor: None,
            current: None,
            ellipse: false,
            grabbed: None,
            last: None,
        }
    }

    /// The rectangle between two corners, rounded outwards.
    fn drag_rect(from: (f32, f32), to: (f32, f32)) -> IntRect {
        IntRect::new(
            from.0.min(to.0).floor() as i32,
            from.1.min(to.1).floor() as i32,
            from.0.max(to.0).ceil() as i32,
            from.1.max(to.1).ceil() as i32,
        )
    }

    /// Which existing artboard/slice contains this point, if any.
    fn hit(&self, doc: &Document, x: f32, y: f32) -> Option<usize> {
        let (ix, iy) = (x as i32, y as i32);
        match self.kind {
            RectKind::Artboard => doc.artboards.iter().position(|a| a.rect.contains(ix, iy)),
            RectKind::Slice => doc.slices.iter().position(|s| s.rect.contains(ix, iy)),
            RectKind::Frame => None,
        }
    }

    /// Build the frame layer: an empty raster clipped by a mask of the
    /// drawn shape, which is what makes anything pasted into it fit.
    fn make_frame(&self, doc: &mut Document, rect: IntRect) {
        if rect.width() < 2 || rect.height() < 2 {
            return;
        }
        let mut mask_tiles = MaskTileMap::new();
        let mut b = schist_vector::PathBuilder::new();
        if self.ellipse {
            b.ellipse(rect);
        } else {
            b.rect(rect);
        }
        let path = b.build(0.25);
        let cov = schist_vector::rasterize(&path, rect, schist_vector::FillRule::NonZero);
        let w = rect.width() as usize;
        for coord in TileCoord::covering(&rect) {
            let trect = coord.rect();
            let clip = trect.intersect(&rect);
            if clip.is_empty() {
                continue;
            }
            let buf = mask_tiles.get_mut_or_insert(coord);
            for y in clip.top..clip.bottom {
                for x in clip.left..clip.right {
                    let v = cov[(y - rect.top) as usize * w + (x - rect.left) as usize];
                    buf[((y - trect.top) * TILE_SIZE + (x - trect.left)) as usize] = v;
                }
            }
        }
        let n = doc.frame_count() + 1;
        let mut layer = Layer::new_raster(format!("Frame {n}"));
        layer.is_frame = true;
        layer.mask = Some(LayerMask {
            tiles: mask_tiles,
            enabled: true,
            linked: true,
            default_value: 0,
            bounds: rect,
        });
        // A frame with nothing in it shows as a light placeholder, the
        // way Photoshop's empty frames do.
        let placeholder = Rgba::new(0.85, 0.85, 0.85, 1.0);
        if let Some(raster) = layer.as_raster_mut() {
            for coord in TileCoord::covering(&rect) {
                let trect = coord.rect();
                let clip = trect.intersect(&rect);
                if clip.is_empty() {
                    continue;
                }
                let buf = raster.tiles.get_mut_or_insert(coord, doc.depth);
                for y in clip.top..clip.bottom {
                    for x in clip.left..clip.right {
                        buf.set(
                            ((y - trect.top) * TILE_SIZE + (x - trect.left)) as usize,
                            placeholder,
                        );
                    }
                }
            }
        }
        let id = layer.id;
        let path = schist_core::LayerPath(vec![doc.tree.layers.len()]);
        let mut edit = doc.begin_edit("Frame");
        edit.insert_layer(path, layer);
        edit.commit();
        doc.active_layer = Some(id);
    }
}

impl ToolPlugin for RectTool {
    fn id(&self) -> &'static str {
        match self.kind {
            RectKind::Artboard => "artboard",
            RectKind::Slice => "slice",
            RectKind::Frame => "frame",
        }
    }
    fn name(&self) -> &'static str {
        match self.kind {
            RectKind::Artboard => "Artboard",
            RectKind::Slice => "Slice",
            RectKind::Frame => "Frame",
        }
    }
    fn icon(&self) -> &'static str {
        match self.kind {
            RectKind::Artboard => "artboard",
            RectKind::Slice => "slice",
            RectKind::Frame => "frame",
        }
    }
    fn shortcut(&self) -> Option<&'static str> {
        match self.kind {
            RectKind::Frame => Some("k"),
            _ => None,
        }
    }
    fn group(&self) -> &'static str {
        match self.kind {
            RectKind::Frame => "frame",
            _ => "artboard",
        }
    }

    fn options(&self) -> Vec<ToolOption> {
        match self.kind {
            RectKind::Frame => vec![ToolOption::choice(
                "frame-shape",
                "Shape",
                &["Rectangle", "Ellipse"],
                self.ellipse as usize,
            )],
            _ => Vec::new(),
        }
    }

    fn set_option(&mut self, key: &str, value: OptionValue) {
        if key == "frame-shape" {
            self.ellipse = value.index() == 1;
        }
    }

    fn on_pointer_down(&mut self, ctx: &mut ToolCtx, input: PointerInput) {
        // Clicking an existing artboard or slice moves it; clicking empty
        // space starts a new one.
        if let Some(i) = self.hit(ctx.doc, input.x, input.y) {
            self.grabbed = Some(i);
            self.last = Some((input.x, input.y));
            return;
        }
        self.anchor = Some((input.x, input.y));
        self.current = Some((input.x, input.y));
    }

    fn on_pointer_move(&mut self, ctx: &mut ToolCtx, input: PointerInput) {
        if let (Some(i), Some(last)) = (self.grabbed, self.last) {
            let (dx, dy) = (
                (input.x - last.0).round() as i32,
                (input.y - last.1).round() as i32,
            );
            if dx == 0 && dy == 0 {
                return;
            }
            self.last = Some((input.x, input.y));
            let rect = match self.kind {
                RectKind::Artboard => ctx.doc.artboards.get_mut(i).map(|a| &mut a.rect),
                RectKind::Slice => ctx.doc.slices.get_mut(i).map(|s| &mut s.rect),
                RectKind::Frame => None,
            };
            if let Some(rect) = rect {
                *rect = IntRect::new(
                    rect.left + dx,
                    rect.top + dy,
                    rect.right + dx,
                    rect.bottom + dy,
                );
                // Moving one is a change to the document, not a repaint.
                ctx.doc.mark_dirty();
            }
            ctx.doc.add_damage(ctx.doc.canvas_rect());
            return;
        }
        if self.anchor.is_some() {
            self.current = Some((input.x, input.y));
        }
    }

    fn on_pointer_up(&mut self, ctx: &mut ToolCtx, input: PointerInput) {
        self.grabbed = None;
        self.last = None;
        let Some(from) = self.anchor.take() else {
            return;
        };
        self.current = None;
        let rect = Self::drag_rect(from, (input.x, input.y));
        if rect.width() < 2 || rect.height() < 2 {
            return;
        }
        match self.kind {
            RectKind::Artboard => {
                let n = ctx.doc.artboards.len() + 1;
                ctx.doc.artboards.push(Artboard {
                    name: format!("Artboard {n}"),
                    rect,
                });
            }
            RectKind::Slice => {
                let n = ctx.doc.slices.len() + 1;
                ctx.doc.slices.push(Slice {
                    name: format!("Slice {n}"),
                    rect,
                    user: true,
                });
            }
            // A frame is a layer, so `make_frame` commits an edit and is
            // already dirty; the other two are plain document state.
            RectKind::Frame => self.make_frame(ctx.doc, rect),
        }
        if !matches!(self.kind, RectKind::Frame) {
            ctx.doc.mark_dirty();
        }
        ctx.doc.add_damage(ctx.doc.canvas_rect());
    }

    fn on_cancel(&mut self, _ctx: &mut ToolCtx) {
        self.anchor = None;
        self.current = None;
        self.grabbed = None;
    }

    fn overlays(&self, doc: &Document, _state: &EditorState) -> Vec<Overlay> {
        let mut out = Vec::new();
        // Existing items, so they can be seen while the tool is active.
        match self.kind {
            RectKind::Artboard => out.extend(doc.artboards.iter().map(|a| Overlay::Rect(a.rect))),
            RectKind::Slice => out.extend(doc.slices.iter().map(|s| Overlay::AntsRect(s.rect))),
            RectKind::Frame => {}
        }
        if let (Some(a), Some(c)) = (self.anchor, self.current) {
            let rect = IntRect::new(
                a.0.min(c.0) as i32,
                a.1.min(c.1) as i32,
                a.0.max(c.0) as i32,
                a.1.max(c.1) as i32,
            );
            out.push(Overlay::AntsRect(rect));
        }
        out
    }
}

/// Click to leave a note or a tally mark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointKind {
    Note,
    Count,
}

pub struct PointTool {
    kind: PointKind,
    grabbed: Option<usize>,
    /// The notes as they stood when the drag began. A move is one history
    /// entry for the whole gesture, not one per pointer-move, so the
    /// `before` has to outlive the mutations made for live feedback.
    drag_before: Option<Vec<Note>>,
}

impl PointTool {
    pub fn new(kind: PointKind) -> Self {
        PointTool {
            kind,
            grabbed: None,
            drag_before: None,
        }
    }
}

/// Screen-space radius of a note's marker. Fixed rather than scaled, so
/// notes stay findable when the document is zoomed out.
pub const NOTE_MARKER_R: f32 = 7.0;

/// The markers for every note in `doc`, with `active` outlined.
///
/// Free rather than a method on the tool, and called by the shell rather
/// than returned from `overlays()`, because notes belong to the document
/// and not to whoever is holding the Note tool. Drawing them from the
/// tool made a note left for a colleague vanish the moment anyone picked
/// up a brush, which is indistinguishable from it having been deleted.
pub fn note_overlays(doc: &Document, active: Option<usize>) -> Vec<Overlay> {
    doc.notes
        .iter()
        .enumerate()
        .map(|(i, n)| Overlay::NoteMarker {
            x: n.at.0,
            y: n.at.1,
            color: n.color,
            selected: active == Some(i),
        })
        .collect()
}

/// The note whose marker covers `(x, y)`, topmost first: later notes are
/// drawn over earlier ones, so they have to be hit in the same order.
fn note_at(doc: &Document, x: f32, y: f32, radius: f32) -> Option<usize> {
    doc.notes
        .iter()
        .enumerate()
        .rev()
        .find(|(_, n)| (n.at.0 - x).hypot(n.at.1 - y) <= radius)
        .map(|(i, _)| i)
}

impl ToolPlugin for PointTool {
    fn id(&self) -> &'static str {
        match self.kind {
            PointKind::Note => "note",
            PointKind::Count => "count",
        }
    }
    fn name(&self) -> &'static str {
        match self.kind {
            PointKind::Note => "Note",
            PointKind::Count => "Count",
        }
    }
    fn icon(&self) -> &'static str {
        match self.kind {
            PointKind::Note => "note",
            PointKind::Count => "count",
        }
    }
    fn group(&self) -> &'static str {
        "note"
    }

    fn on_pointer_down(&mut self, ctx: &mut ToolCtx, input: PointerInput) {
        let r = 10.0 / ctx.state.zoom.max(0.01);
        match self.kind {
            PointKind::Note => {
                // Alt-click removes; clicking an existing note selects it
                // and grabs it for a drag.
                if let Some(i) = note_at(ctx.doc, input.x, input.y, r) {
                    if input.modifiers.alt {
                        let mut edit = ctx.doc.begin_edit("Delete Note");
                        edit.change_notes(|notes| {
                            notes.remove(i);
                        });
                        edit.commit();
                        // Every note after the removed one shifted down.
                        ctx.state.active_note = match ctx.state.active_note {
                            Some(a) if a == i => None,
                            Some(a) if a > i => Some(a - 1),
                            other => other,
                        };
                    } else {
                        self.grabbed = Some(i);
                        self.drag_before = Some(ctx.doc.notes.clone());
                        ctx.state.active_note = Some(i);
                    }
                } else {
                    // A new note is born empty and selected: the Notes
                    // panel is where its text gets typed, and placing one
                    // is the request to type into it. It used to arrive
                    // holding the placeholder "Note 3", which no panel
                    // ever showed and no user could change.
                    let author = ctx.state.note_author.clone();
                    let color = ctx.state.note_color;
                    let mut edit = ctx.doc.begin_edit("Place Note");
                    edit.change_notes(|notes| {
                        notes.push(Note::new((input.x, input.y), author, color));
                    });
                    edit.commit();
                    ctx.state.active_note = Some(ctx.doc.notes.len() - 1);
                }
            }
            PointKind::Count => {
                // Every click adds a mark; alt-click takes the nearest
                // one back off, which is how Photoshop's Count works.
                if ctx.doc.counts.is_empty() {
                    ctx.doc.counts.push(CountGroup {
                        name: "Count 1".into(),
                        points: Vec::new(),
                    });
                }
                let group = ctx.doc.counts.last_mut().unwrap();
                if input.modifiers.alt {
                    if let Some(i) = group
                        .points
                        .iter()
                        .position(|p| (p.0 - input.x).hypot(p.1 - input.y) <= r)
                    {
                        group.points.remove(i);
                        ctx.doc.mark_dirty();
                    }
                } else {
                    group.points.push((input.x, input.y));
                    ctx.doc.mark_dirty();
                }
            }
        }
        // Counts are drawn straight from the document rather than through
        // an edit, so they still have to ask for the repaint themselves.
        // Notes go through the history now, which does it for them.
        if self.kind == PointKind::Count {
            ctx.doc.add_damage(ctx.doc.canvas_rect());
        }
    }

    fn on_pointer_move(&mut self, ctx: &mut ToolCtx, input: PointerInput) {
        if let Some(i) = self.grabbed {
            // Moved live for feedback; the history entry is assembled once
            // the drag ends, from the snapshot taken when it started.
            if let Some(note) = ctx.doc.notes.get_mut(i) {
                note.at = (input.x, input.y);
            }
        }
    }

    fn on_pointer_up(&mut self, ctx: &mut ToolCtx, _input: PointerInput) {
        self.grabbed = None;
        let Some(before) = self.drag_before.take() else {
            return;
        };
        // Rewind to the pre-drag state so the builder captures the whole
        // gesture as one op, then re-apply what the drag arrived at.
        // `change_notes` is a no-op when the two match, so a click that
        // only selected a note leaves no history entry.
        let after = std::mem::replace(&mut ctx.doc.notes, before);
        let mut edit = ctx.doc.begin_edit("Move Note");
        edit.change_notes(|notes| *notes = after);
        edit.commit();
    }

    fn overlays(&self, doc: &Document, _state: &EditorState) -> Vec<Overlay> {
        let mut out = Vec::new();
        match self.kind {
            // Nothing: the shell draws note markers itself, from
            // `note_overlays`, so that they are there under every tool.
            PointKind::Note => {}
            PointKind::Count => {
                for group in &doc.counts {
                    for p in &group.points {
                        out.push(Overlay::Circle {
                            cx: p.0,
                            cy: p.1,
                            r: 5.0,
                        });
                    }
                }
            }
        }
        out
    }
}

pub struct DocToolsPlugin;

impl PluginManifest for DocToolsPlugin {
    fn id(&self) -> &'static str {
        "schist.tools-doc"
    }

    fn register(&self, registry: &mut PluginRegistry) {
        registry.register_tool(Box::new(RectTool::new(RectKind::Frame)));
        registry.register_tool(Box::new(RectTool::new(RectKind::Artboard)));
        registry.register_tool(Box::new(RectTool::new(RectKind::Slice)));
        registry.register_tool(Box::new(PointTool::new(PointKind::Note)));
        registry.register_tool(Box::new(PointTool::new(PointKind::Count)));
    }
}
