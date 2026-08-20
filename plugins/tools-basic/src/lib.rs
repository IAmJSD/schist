//! Basic tools: Move, Eyedropper, Hand, Zoom.
//!
//! Hand and Zoom manipulate the *viewport*, which belongs to the canvas
//! view, not the document — the canvas checks the active tool id and
//! handles their pointer input itself; the tool objects exist so the
//! toolbar and keymap treat them uniformly.

use photoslop_color::Rgba;
use photoslop_core::IntRect;
use photoslop_plugin_api::{PluginManifest, PluginRegistry, PointerInput, ToolCtx, ToolPlugin};

/// Move tool: drags the active layer.
///
/// The pixels follow the cursor live. During the drag that happens through
/// the layer's transient `render_offset`, so a 100-megapixel layer costs
/// nothing per mouse event; on release the offset is baked into the tiles
/// as a single undoable edit.
pub struct MoveTool {
    drag: Option<Drag>,
}

struct Drag {
    layer: photoslop_core::LayerId,
    start: (f32, f32),
    /// The offset currently applied to the layer.
    offset: (i32, i32),
}

impl MoveTool {
    fn new() -> Self {
        MoveTool { drag: None }
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

    fn on_pointer_down(&mut self, ctx: &mut ToolCtx, input: PointerInput) {
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
        if let Some(layer) = ctx.doc.tree.find_mut(drag.layer) {
            layer.render_offset = (0, 0);
        }
        if dx == 0 && dy == 0 {
            ctx.doc.damage_all();
            return;
        }
        let mut edit = ctx.doc.begin_edit("Move Layer");
        edit.translate_layer(drag.layer, dx, dy);
        edit.commit();
    }

    fn on_cancel(&mut self, ctx: &mut ToolCtx) {
        if let Some(drag) = self.drag.take() {
            if let Some(layer) = ctx.doc.tree.find_mut(drag.layer) {
                layer.render_offset = (0, 0);
            }
            ctx.doc.damage_all();
        }
    }

    // No overlay: the layer itself moves, so an outline would just be
    // noise on top of it.
}

/// Eyedropper: picks the composited color under the cursor into the
/// foreground (Alt-click: background), like Photoshop's default
/// "all layers" sampling.
pub struct EyedropperTool;

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

    fn on_pointer_down(&mut self, ctx: &mut ToolCtx, input: PointerInput) {
        let x = input.x.floor() as i32;
        let y = input.y.floor() as i32;
        if !ctx.doc.canvas_rect().contains(x, y) {
            return;
        }
        let px =
            photoslop_compositor::composite_region_rgba8(ctx.doc, IntRect::from_xywh(x, y, 1, 1));
        let color = Rgba::from_u8(px[0], px[1], px[2], 255);
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
        "photoslop.tools-basic"
    }

    fn register(&self, registry: &mut PluginRegistry) {
        registry.register_tool(Box::new(MoveTool::new()));
        registry.register_tool(Box::new(EyedropperTool));
        registry.register_tool(Box::new(HandTool));
        registry.register_tool(Box::new(ZoomTool));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use photoslop_color::Depth;
    use photoslop_core::{blit_rgba8, Document, Layer};
    use photoslop_plugin_api::{EditorState, Modifiers};

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
        let mut tool = EyedropperTool;
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        tool.on_pointer_down(&mut ctx, input(20.0, 20.0));
        assert_eq!(state.foreground.to_u8(), [255, 0, 0, 255]);
    }
}
