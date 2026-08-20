//! Tools that edit the document's furniture rather than its pixels:
//! artboards, frames, slices, notes and counts.
//!
//! They share a shape -- drag out a rectangle or click a point, and the
//! result goes into a list on the `Document` -- so they share this crate
//! and the small amount of hit-testing and drawing that goes with it.

use photoslop_color::Rgba;
use photoslop_core::{
    Artboard, CountGroup, Document, IntRect, Layer, LayerMask, MaskTileMap, Note, Slice, TileCoord,
    TILE_SIZE,
};
use photoslop_plugin_api::{
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
        let mut b = photoslop_vector::PathBuilder::new();
        if self.ellipse {
            b.ellipse(rect);
        } else {
            b.rect(rect);
        }
        let path = b.build(0.25);
        let cov = photoslop_vector::rasterize(&path, rect, photoslop_vector::FillRule::NonZero);
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
        let path = photoslop_core::LayerPath(vec![doc.tree.layers.len()]);
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
            RectKind::Frame => self.make_frame(ctx.doc, rect),
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
}

impl PointTool {
    pub fn new(kind: PointKind) -> Self {
        PointTool {
            kind,
            grabbed: None,
        }
    }
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
                // Alt-click removes; clicking an existing note grabs it.
                if let Some(i) = ctx
                    .doc
                    .notes
                    .iter()
                    .position(|n| (n.at.0 - input.x).hypot(n.at.1 - input.y) <= r)
                {
                    if input.modifiers.alt {
                        ctx.doc.notes.remove(i);
                    } else {
                        self.grabbed = Some(i);
                    }
                } else {
                    let n = ctx.doc.notes.len() + 1;
                    ctx.doc.notes.push(Note {
                        at: (input.x, input.y),
                        author: String::new(),
                        text: format!("Note {n}"),
                    });
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
                    }
                } else {
                    group.points.push((input.x, input.y));
                }
            }
        }
        ctx.doc.add_damage(ctx.doc.canvas_rect());
    }

    fn on_pointer_move(&mut self, ctx: &mut ToolCtx, input: PointerInput) {
        if let Some(i) = self.grabbed {
            if let Some(note) = ctx.doc.notes.get_mut(i) {
                note.at = (input.x, input.y);
            }
            ctx.doc.add_damage(ctx.doc.canvas_rect());
        }
    }

    fn on_pointer_up(&mut self, _ctx: &mut ToolCtx, _input: PointerInput) {
        self.grabbed = None;
    }

    fn overlays(&self, doc: &Document, _state: &EditorState) -> Vec<Overlay> {
        let mut out = Vec::new();
        match self.kind {
            PointKind::Note => {
                for n in &doc.notes {
                    out.push(Overlay::Circle {
                        cx: n.at.0,
                        cy: n.at.1,
                        r: 7.0,
                    });
                }
            }
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
        "photoslop.tools-doc"
    }

    fn register(&self, registry: &mut PluginRegistry) {
        registry.register_tool(Box::new(RectTool::new(RectKind::Frame)));
        registry.register_tool(Box::new(RectTool::new(RectKind::Artboard)));
        registry.register_tool(Box::new(RectTool::new(RectKind::Slice)));
        registry.register_tool(Box::new(PointTool::new(PointKind::Note)));
        registry.register_tool(Box::new(PointTool::new(PointKind::Count)));
    }
}
