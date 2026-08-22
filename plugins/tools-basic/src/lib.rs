//! Basic tools: Move, Eyedropper, Hand, Zoom.
//!
//! Hand and Zoom manipulate the *viewport*, which belongs to the canvas
//! view, not the document — the canvas checks the active tool id and
//! handles their pointer input itself; the tool objects exist so the
//! toolbar and keymap treat them uniformly.

use schist_color::Rgba;
use schist_core::IntRect;
use schist_plugin_api::{
    OptionValue, PluginManifest, PluginRegistry, PointerInput, ToolCtx, ToolOption, ToolPlugin,
};

/// Move tool: drags the active layer.
///
/// The pixels follow the cursor live. During the drag that happens through
/// the layer's transient `render_offset`, so a 100-megapixel layer costs
/// nothing per mouse event; on release the offset is baked into the tiles
/// as a single undoable edit.
pub struct MoveTool {
    drag: Option<Drag>,
    /// Pick the layer under the cursor instead of moving whichever layer
    /// happens to be selected, as Photoshop's Auto-Select does.
    auto_select: bool,
    /// With auto-select on, whether a hit inside a group moves the whole
    /// group or just the one layer.
    auto_select_group: bool,
}

const AUTO_TARGETS: &[&str] = &["Layer", "Group"];

struct Drag {
    layer: schist_core::LayerId,
    start: (f32, f32),
    /// The offset currently applied to the layer.
    offset: (i32, i32),
}

impl MoveTool {
    fn new() -> Self {
        MoveTool {
            drag: None,
            auto_select: false,
            auto_select_group: false,
        }
    }

    /// The topmost visible layer with an opaque pixel under the cursor.
    ///
    /// `tree.iter()` runs bottom to top, so the last hit is the topmost
    /// one. Bounds alone are not enough -- a layer's box usually contains
    /// a lot of transparency, and Photoshop picks what you can see.
    fn layer_at(doc: &schist_core::Document, x: f32, y: f32) -> Option<schist_core::LayerId> {
        let (px, py) = (x.floor() as i32, y.floor() as i32);
        let mut hit = None;
        for layer in doc.tree.iter() {
            if !layer.visible || layer.locked || !layer.tight_bounds().contains(px, py) {
                continue;
            }
            let opaque = layer
                .as_raster()
                .map(|r| r.tiles.pixel(px, py).a > 0.0)
                .unwrap_or(true);
            if opaque {
                hit = Some(layer.id);
            }
        }
        hit
    }

    /// The outermost group containing `id`, or `id` itself if it is not in
    /// one.
    fn group_of(doc: &schist_core::Document, id: schist_core::LayerId) -> schist_core::LayerId {
        let Some(path) = doc.tree.path_of(id) else {
            return id;
        };
        if path.0.len() < 2 {
            return id;
        }
        doc.tree.layers.get(path.0[0]).map(|l| l.id).unwrap_or(id)
    }

    /// Drop the live `render_offset` a drag was previewing with, repainting
    /// only where the preview actually was.
    ///
    /// Damaging the whole document here instead cost every cached tile --
    /// and the canvas re-composites and colour-manages what it throws away
    /// -- for a click that moved nothing at all, which is what an ordinary
    /// click with the Move tool is.
    fn undo_preview(ctx: &mut ToolCtx, layer_id: schist_core::LayerId) {
        let Some(layer) = ctx.doc.tree.find_mut(layer_id) else {
            return;
        };
        if layer.render_offset == (0, 0) {
            return;
        }
        let mut damage = layer.content_bounds();
        layer.render_offset = (0, 0);
        damage = damage.union(&layer.content_bounds());
        ctx.doc.add_damage(damage.inflated(1));
    }
}

impl ToolPlugin for MoveTool {
    fn id(&self) -> &'static str {
        "move"
    }
    fn name(&self) -> &'static str {
        "Move"
    }
    fn icon(&self) -> &'static str {
        "move"
    }
    fn shortcut(&self) -> Option<&'static str> {
        Some("v")
    }

    fn options(&self) -> Vec<ToolOption> {
        let mut opts = vec![ToolOption::toggle(
            "move-auto",
            "Auto-Select",
            self.auto_select,
        )];
        if self.auto_select {
            opts.push(ToolOption::choice(
                "move-auto-target",
                "",
                AUTO_TARGETS,
                usize::from(self.auto_select_group),
            ));
        }
        opts
    }

    fn set_option(&mut self, key: &str, value: OptionValue) {
        match key {
            "move-auto" => self.auto_select = value.bool(),
            "move-auto-target" => self.auto_select_group = value.index() == 1,
            _ => {}
        }
    }

    fn on_pointer_down(&mut self, ctx: &mut ToolCtx, input: PointerInput) {
        if self.auto_select {
            if let Some(hit) = Self::layer_at(ctx.doc, input.x, input.y) {
                let pick = if self.auto_select_group {
                    Self::group_of(ctx.doc, hit)
                } else {
                    hit
                };
                ctx.doc.active_layer = Some(pick);
            }
        }
        let Some(id) = ctx.doc.active_layer else {
            return;
        };
        let Some(layer) = ctx.doc.tree.find(id) else {
            return;
        };
        if layer.locked {
            return;
        }
        self.drag = Some(Drag {
            layer: id,
            start: (input.x, input.y),
            offset: (0, 0),
        });
    }

    fn on_pointer_move(&mut self, ctx: &mut ToolCtx, input: PointerInput) {
        let Some(drag) = &mut self.drag else { return };
        let offset = (
            (input.x - drag.start.0).round() as i32,
            (input.y - drag.start.1).round() as i32,
        );
        if offset == drag.offset {
            return;
        }
        let previous = drag.offset;
        drag.offset = offset;
        let layer_id = drag.layer;
        // Redraw where the layer was and where it now is.
        let mut damage = IntRect::EMPTY;
        if let Some(layer) = ctx.doc.tree.find_mut(layer_id) {
            damage = damage.union(&layer.content_bounds());
            layer.render_offset = offset;
            damage = damage.union(&layer.content_bounds());
        }
        let _ = previous;
        ctx.doc.add_damage(damage.inflated(1));
    }

    fn on_pointer_up(&mut self, ctx: &mut ToolCtx, input: PointerInput) {
        let Some(drag) = self.drag.take() else { return };
        // Take the offset from the release point: a fast drag can deliver
        // down and up with no move event in between.
        let (dx, dy) = (
            (input.x - drag.start.0).round() as i32,
            (input.y - drag.start.1).round() as i32,
        );
        // Put the layer back where its pixels actually are, then record the
        // move as one edit so undo restores the original position.
        Self::undo_preview(ctx, drag.layer);
        if dx == 0 && dy == 0 {
            return;
        }
        let mut edit = ctx.doc.begin_edit("Move Layer");
        edit.translate_layer(drag.layer, dx, dy);
        edit.commit();
    }

    fn on_cancel(&mut self, ctx: &mut ToolCtx) {
        if let Some(drag) = self.drag.take() {
            Self::undo_preview(ctx, drag.layer);
        }
    }

    // No overlay: the layer itself moves, so an outline would just be
    // noise on top of it.
}

/// Eyedropper: picks the composited color under the cursor into the
/// foreground (Alt-click: background), like Photoshop's default
/// "all layers" sampling.
pub struct EyedropperTool {
    /// Index into `SAMPLE_SIZES`.
    sample: usize,
    current_layer_only: bool,
}

/// Photoshop's sample sizes, and the square each averages over.
const SAMPLE_SIZES: &[&str] = &[
    "Point Sample",
    "3 by 3 Average",
    "5 by 5 Average",
    "11 by 11 Average",
    "31 by 31 Average",
];
const SAMPLE_EXTENTS: [i32; 5] = [1, 3, 5, 11, 31];
const SAMPLE_SCOPES: &[&str] = &["All Layers", "Current Layer"];

impl EyedropperTool {
    fn new() -> Self {
        EyedropperTool {
            sample: 0,
            current_layer_only: false,
        }
    }

    /// Average the `n` by `n` square centred on the pixel, ignoring
    /// anything outside the canvas so an edge sample is not dragged
    /// towards black.
    fn sample_at(&self, ctx: &ToolCtx, x: i32, y: i32) -> Option<Rgba> {
        let n = SAMPLE_EXTENTS[self.sample.min(SAMPLE_EXTENTS.len() - 1)];
        let half = n / 2;
        let area = IntRect::from_xywh(x - half, y - half, n as u32, n as u32)
            .intersect(&ctx.doc.canvas_rect());
        if area.is_empty() {
            return None;
        }
        if self.current_layer_only {
            let layer = ctx.doc.tree.find(ctx.doc.active_layer?)?;
            let raster = layer.as_raster()?;
            let mut acc = [0.0f32; 4];
            let mut count = 0.0;
            for py in area.top..area.bottom {
                for px in area.left..area.right {
                    let p = raster.tiles.pixel(px, py);
                    acc[0] += p.r;
                    acc[1] += p.g;
                    acc[2] += p.b;
                    acc[3] += p.a;
                    count += 1.0;
                }
            }
            return Some(Rgba::new(
                acc[0] / count,
                acc[1] / count,
                acc[2] / count,
                1.0,
            ));
        }
        let px = schist_compositor::composite_region_rgba8(ctx.doc, area);
        let mut acc = [0u32; 3];
        let count = (area.width() * area.height()) as u32;
        for p in px.as_chunks::<4>().0 {
            acc[0] += p[0] as u32;
            acc[1] += p[1] as u32;
            acc[2] += p[2] as u32;
        }
        Some(Rgba::from_u8(
            (acc[0] / count) as u8,
            (acc[1] / count) as u8,
            (acc[2] / count) as u8,
            255,
        ))
    }
}

impl ToolPlugin for EyedropperTool {
    fn id(&self) -> &'static str {
        "eyedropper"
    }
    fn name(&self) -> &'static str {
        "Eyedropper"
    }
    fn icon(&self) -> &'static str {
        "eyedropper"
    }
    fn shortcut(&self) -> Option<&'static str> {
        Some("i")
    }

    fn options(&self) -> Vec<ToolOption> {
        vec![
            ToolOption::choice("dropper-size", "Sample Size", SAMPLE_SIZES, self.sample),
            ToolOption::choice(
                "dropper-scope",
                "Sample",
                SAMPLE_SCOPES,
                usize::from(self.current_layer_only),
            ),
        ]
    }

    fn set_option(&mut self, key: &str, value: OptionValue) {
        match key {
            "dropper-size" => self.sample = value.index().min(SAMPLE_EXTENTS.len() - 1),
            "dropper-scope" => self.current_layer_only = value.index() == 1,
            _ => {}
        }
    }

    fn on_pointer_down(&mut self, ctx: &mut ToolCtx, input: PointerInput) {
        let x = input.x.floor() as i32;
        let y = input.y.floor() as i32;
        if !ctx.doc.canvas_rect().contains(x, y) {
            return;
        }
        let Some(color) = self.sample_at(ctx, x, y) else {
            return;
        };
        if input.modifiers.alt {
            ctx.state.background = color;
        } else {
            ctx.state.foreground = color;
        }
    }

    fn on_pointer_move(&mut self, ctx: &mut ToolCtx, input: PointerInput) {
        // Drag keeps sampling, like Photoshop.
        self.on_pointer_down(ctx, input);
    }

    fn on_pointer_up(&mut self, _ctx: &mut ToolCtx, _input: PointerInput) {}
}

/// Viewport tools — no-ops at the document level (see module docs).
macro_rules! viewport_tool {
    ($ty:ident, $id:literal, $name:literal, $icon:literal, $key:literal) => {
        pub struct $ty;

        impl ToolPlugin for $ty {
            fn id(&self) -> &'static str {
                $id
            }
            fn name(&self) -> &'static str {
                $name
            }
            fn icon(&self) -> &'static str {
                $icon
            }
            fn shortcut(&self) -> Option<&'static str> {
                Some($key)
            }
            fn on_pointer_down(&mut self, _: &mut ToolCtx, _: PointerInput) {}
            fn on_pointer_move(&mut self, _: &mut ToolCtx, _: PointerInput) {}
            fn on_pointer_up(&mut self, _: &mut ToolCtx, _: PointerInput) {}
        }
    };
}

viewport_tool!(HandTool, "hand", "Hand", "hand", "h");
viewport_tool!(ZoomTool, "zoom", "Zoom", "zoom", "z");

pub struct BasicToolsPlugin;

impl PluginManifest for BasicToolsPlugin {
    fn id(&self) -> &'static str {
        "schist.tools-basic"
    }

    fn register(&self, registry: &mut PluginRegistry) {
        registry.register_tool(Box::new(MoveTool::new()));
        registry.register_tool(Box::new(EyedropperTool::new()));
        registry.register_tool(Box::new(HandTool));
        registry.register_tool(Box::new(ZoomTool));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use schist_color::Depth;
    use schist_core::{blit_rgba8, Document, Layer};
    use schist_plugin_api::{EditorState, Modifiers, OptionValue};

    fn input(x: f32, y: f32) -> PointerInput {
        PointerInput {
            x,
            y,
            pressure: 1.0,
            modifiers: Modifiers::default(),
        }
    }

    fn red_square_doc() -> Document {
        let mut doc = Document::new("t", 256, 256, Depth::Eight);
        let mut layer = Layer::new_raster("sq");
        let buf = [255u8, 0, 0, 255].repeat(32 * 32);
        blit_rgba8(
            &mut layer.as_raster_mut().unwrap().tiles,
            Depth::Eight,
            IntRect::from_xywh(10, 10, 32, 32),
            &buf,
        );
        doc.push_layer(layer);
        doc
    }

    /// A 2x2 checker of red and white at the origin, so a point sample and
    /// an averaging sample cannot agree.
    fn checker_doc() -> Document {
        let mut doc = Document::new("t", 64, 64, Depth::Eight);
        let mut layer = Layer::new_raster("checker");
        let mut buf = Vec::new();
        for y in 0..32 {
            for x in 0..32 {
                let on = (x + y) % 2 == 0;
                buf.extend_from_slice(if on {
                    &[255u8, 0, 0, 255]
                } else {
                    &[255u8, 255, 255, 255]
                });
            }
        }
        blit_rgba8(
            &mut layer.as_raster_mut().unwrap().tiles,
            Depth::Eight,
            IntRect::from_xywh(0, 0, 32, 32),
            &buf,
        );
        doc.push_layer(layer);
        doc
    }

    #[test]
    fn the_eyedropper_sample_size_actually_averages() {
        let mut doc = checker_doc();
        let mut state = EditorState::default();

        let mut point = EyedropperTool::new();
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        point.on_pointer_down(&mut ctx, input(4.0, 4.0));
        let single = ctx.state.foreground.to_u8();
        // (4,4) is an "on" square, so a point sample is pure red.
        assert_eq!([single[0], single[1], single[2]], [255, 0, 0]);

        let mut wide = EyedropperTool::new();
        wide.set_option("dropper-size", OptionValue::Choice(2)); // 5 by 5
        wide.on_pointer_down(&mut ctx, input(4.0, 4.0));
        let avg = ctx.state.foreground.to_u8();
        assert_eq!(avg[0], 255, "red channel is saturated either way");
        assert!(
            avg[1] > 100 && avg[1] < 155,
            "a 5x5 of a checker should land near half, got {}",
            avg[1]
        );
    }

    #[test]
    fn the_eyedropper_can_ignore_the_layers_above() {
        let mut doc = checker_doc();
        // A solid blue layer on top: the composite is blue, the layer
        // underneath is not.
        let mut top = Layer::new_raster("blue");
        blit_rgba8(
            &mut top.as_raster_mut().unwrap().tiles,
            Depth::Eight,
            IntRect::from_xywh(0, 0, 32, 32),
            &[0u8, 0, 255, 255].repeat(32 * 32),
        );
        doc.push_layer(top);
        let under = doc.tree.layers[0].id;

        let mut state = EditorState::default();
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        let mut tool = EyedropperTool::new();
        tool.on_pointer_down(&mut ctx, input(4.0, 4.0));
        assert_eq!(ctx.state.foreground.to_u8()[2], 255, "sees the blue layer");

        ctx.doc.active_layer = Some(under);
        tool.set_option("dropper-scope", OptionValue::Choice(1));
        tool.on_pointer_down(&mut ctx, input(4.0, 4.0));
        let px = ctx.state.foreground.to_u8();
        assert_eq!(
            [px[0], px[1], px[2]],
            [255, 0, 0],
            "sees only its own layer"
        );
    }

    #[test]
    fn auto_select_picks_the_topmost_opaque_layer() {
        let mut doc = red_square_doc();
        let under = doc.active_layer.unwrap();
        // A second square, offset, above the first.
        let mut top = Layer::new_raster("top");
        blit_rgba8(
            &mut top.as_raster_mut().unwrap().tiles,
            Depth::Eight,
            IntRect::from_xywh(100, 100, 32, 32),
            &[0u8, 255, 0, 255].repeat(32 * 32),
        );
        let top_id = top.id;
        doc.push_layer(top);
        doc.active_layer = Some(under);

        let mut state = EditorState::default();
        let mut tool = MoveTool::new();
        tool.set_option("move-auto", OptionValue::Bool(true));
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        // Over the upper square: auto-select should switch to it.
        tool.on_pointer_down(&mut ctx, input(110.0, 110.0));
        assert_eq!(ctx.doc.active_layer, Some(top_id));
        tool.on_pointer_up(&mut ctx, input(110.0, 110.0));

        // Over the lower square, where the upper one is transparent.
        tool.on_pointer_down(&mut ctx, input(20.0, 20.0));
        assert_eq!(ctx.doc.active_layer, Some(under));
        tool.on_pointer_up(&mut ctx, input(20.0, 20.0));

        // Empty canvas leaves the selection alone.
        tool.on_pointer_down(&mut ctx, input(200.0, 200.0));
        assert_eq!(ctx.doc.active_layer, Some(under));
    }

    #[test]
    fn move_translates_pixels_and_undoes() {
        let mut doc = red_square_doc();
        let id = doc.active_layer.unwrap();
        let mut state = EditorState::default();
        let mut tool = MoveTool::new();
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        tool.on_pointer_down(&mut ctx, input(20.0, 20.0));
        tool.on_pointer_move(&mut ctx, input(120.0, 70.0));
        tool.on_pointer_up(&mut ctx, input(120.0, 70.0));

        let px = |doc: &Document, x, y| {
            doc.tree
                .find(id)
                .unwrap()
                .as_raster()
                .unwrap()
                .tiles
                .pixel(x, y)
                .to_u8()
        };
        assert_eq!(px(&doc, 15, 15)[3], 0, "old spot empty");
        assert_eq!(px(&doc, 115, 65), [255, 0, 0, 255], "moved by (100,50)");

        doc.undo();
        assert_eq!(px(&doc, 15, 15), [255, 0, 0, 255], "undo restores");
        doc.redo();
        assert_eq!(px(&doc, 115, 65), [255, 0, 0, 255]);
    }

    #[test]
    fn a_click_that_moves_nothing_repaints_nothing() {
        let mut doc = red_square_doc();
        let mut state = EditorState::default();
        let mut tool = MoveTool::new();
        doc.take_damage();
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        tool.on_pointer_down(&mut ctx, input(20.0, 20.0));
        tool.on_pointer_up(&mut ctx, input(20.0, 20.0));
        // Damaging the canvas here drops every cached composited tile, and
        // clicking is how you pick a layer to move in the first place.
        assert!(doc.take_damage().is_empty());
    }

    #[test]
    fn a_drag_back_to_the_start_still_undraws_the_preview() {
        let mut doc = red_square_doc();
        let id = doc.active_layer.unwrap();
        let mut state = EditorState::default();
        let mut tool = MoveTool::new();
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        tool.on_pointer_down(&mut ctx, input(20.0, 20.0));
        tool.on_pointer_move(&mut ctx, input(40.0, 30.0));
        doc.take_damage();
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        // Released where it started, so there is no edit to record -- but
        // the preview drawn at +20,+10 is still on screen and has to go.
        tool.on_pointer_up(&mut ctx, input(20.0, 20.0));

        assert_eq!(doc.tree.find(id).unwrap().render_offset, (0, 0));
        let covered = doc
            .take_damage()
            .iter()
            .fold(IntRect::EMPTY, |acc, r| acc.union(r));
        assert!(
            covered.contains(50, 40),
            "where the preview was must repaint: {covered:?}"
        );
    }

    #[test]
    fn drag_moves_pixels_live_before_release() {
        let mut doc = red_square_doc();
        let id = doc.active_layer.unwrap();
        let mut state = EditorState::default();
        let mut tool = MoveTool::new();
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        tool.on_pointer_down(&mut ctx, input(20.0, 20.0));
        tool.on_pointer_move(&mut ctx, input(120.0, 70.0));

        // Mid-drag the layer already reads as moved...
        let layer = doc.tree.find(id).unwrap();
        assert_eq!(layer.render_offset, (100, 50));
        assert_eq!(
            layer.tight_bounds(),
            IntRect::from_xywh(110, 60, 32, 32),
            "bounds follow the drag"
        );
        // ...without any pixels having been rewritten yet.
        assert_eq!(
            layer.as_raster().unwrap().tiles.pixel(15, 15).to_u8(),
            [255, 0, 0, 255],
            "tiles untouched during the drag"
        );
        assert!(!doc.history.can_undo(), "nothing recorded mid-drag");
    }

    #[test]
    fn cancelling_a_drag_puts_the_layer_back() {
        let mut doc = red_square_doc();
        let id = doc.active_layer.unwrap();
        let mut state = EditorState::default();
        let mut tool = MoveTool::new();
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        tool.on_pointer_down(&mut ctx, input(20.0, 20.0));
        tool.on_pointer_move(&mut ctx, input(120.0, 70.0));
        tool.on_cancel(&mut ctx);
        let layer = doc.tree.find(id).unwrap();
        assert_eq!(layer.render_offset, (0, 0));
        assert_eq!(layer.tight_bounds(), IntRect::from_xywh(10, 10, 32, 32));
    }

    #[test]
    fn releasing_bakes_the_offset_into_the_pixels() {
        let mut doc = red_square_doc();
        let id = doc.active_layer.unwrap();
        let mut state = EditorState::default();
        let mut tool = MoveTool::new();
        {
            let mut ctx = ToolCtx {
                doc: &mut doc,
                state: &mut state,
            };
            tool.on_pointer_down(&mut ctx, input(20.0, 20.0));
            tool.on_pointer_move(&mut ctx, input(120.0, 70.0));
            tool.on_pointer_up(&mut ctx, input(120.0, 70.0));
        }
        let layer = doc.tree.find(id).unwrap();
        assert_eq!(layer.render_offset, (0, 0), "offset consumed");
        assert_eq!(
            layer.as_raster().unwrap().tiles.pixel(115, 65).to_u8(),
            [255, 0, 0, 255],
            "pixels really moved"
        );
        doc.undo();
        let layer = doc.tree.find(id).unwrap();
        assert_eq!(
            layer.as_raster().unwrap().tiles.pixel(15, 15).to_u8(),
            [255, 0, 0, 255]
        );
    }

    #[test]
    fn move_unaligned_offset_preserves_pixels() {
        let mut doc = red_square_doc();
        let id = doc.active_layer.unwrap();
        let mut state = EditorState::default();
        let mut tool = MoveTool::new();
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        tool.on_pointer_down(&mut ctx, input(0.0, 0.0));
        tool.on_pointer_up(&mut ctx, input(3.0, 7.0));
        let tiles = &doc.tree.find(id).unwrap().as_raster().unwrap().tiles;
        assert_eq!(tiles.pixel(13, 17).to_u8(), [255, 0, 0, 255]);
        assert_eq!(tiles.pixel(12, 16).to_u8()[3], 0);
    }

    #[test]
    fn eyedropper_picks_composite_color() {
        let mut doc = red_square_doc();
        let mut state = EditorState::default();
        let mut tool = EyedropperTool::new();
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        tool.on_pointer_down(&mut ctx, input(20.0, 20.0));
        assert_eq!(state.foreground.to_u8(), [255, 0, 0, 255]);
    }
}
