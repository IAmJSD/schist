//! Basic tools: Move, Eyedropper, Hand, Zoom.
//!
//! Hand and Zoom manipulate the *viewport*, which belongs to the canvas
//! view, not the document — the canvas checks the active tool id and
//! handles their pointer input itself; the tool objects exist so the
//! toolbar and keymap treat them uniformly.

use photoslop_color::Rgba;
use photoslop_core::{Document, IntRect};
use photoslop_plugin_api::{
    EditorState, Overlay, PluginManifest, PluginRegistry, PointerInput, ToolCtx, ToolPlugin,
};

/// Move tool: drags the active layer. Pixels move on release (the canvas
/// shows the outline during the drag); arrow-key nudge arrives with M11's
/// keybind audit.
pub struct MoveTool {
    drag: Option<Drag>,
}

struct Drag {
    layer: photoslop_core::LayerId,
    start: (f32, f32),
    current: (f32, f32),
    bounds: IntRect,
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
        "✥"
    }
    fn shortcut(&self) -> Option<&'static str> {
        Some("v")
    }

    fn on_pointer_down(&mut self, ctx: &mut ToolCtx, input: PointerInput) {
        let Some(id) = ctx.doc.active_layer else { return };
        let Some(layer) = ctx.doc.tree.find(id) else { return };
        if layer.locked {
            return;
        }
        self.drag = Some(Drag {
            layer: id,
            start: (input.x, input.y),
            current: (input.x, input.y),
            bounds: layer.content_bounds(),
        });
    }

    fn on_pointer_move(&mut self, _ctx: &mut ToolCtx, input: PointerInput) {
        if let Some(drag) = &mut self.drag {
            drag.current = (input.x, input.y);
        }
    }

    fn on_pointer_up(&mut self, ctx: &mut ToolCtx, input: PointerInput) {
        let Some(drag) = self.drag.take() else { return };
        let dx = (input.x - drag.start.0).round() as i32;
        let dy = (input.y - drag.start.1).round() as i32;
        if dx == 0 && dy == 0 {
            return;
        }
        let mut edit = ctx.doc.begin_edit("Move Layer");
        edit.translate_layer(drag.layer, dx, dy);
        edit.commit();
    }

    fn on_cancel(&mut self, _ctx: &mut ToolCtx) {
        self.drag = None;
    }

    fn overlays(&self, _doc: &Document, _state: &EditorState) -> Vec<Overlay> {
        match &self.drag {
            Some(drag) if !drag.bounds.is_empty() => {
                let dx = (drag.current.0 - drag.start.0).round() as i32;
                let dy = (drag.current.1 - drag.start.1).round() as i32;
                vec![Overlay::Rect(drag.bounds.translated(dx, dy))]
            }
            _ => Vec::new(),
        }
    }
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
        "💧"
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
        let px = photoslop_compositor::composite_region_rgba8(ctx.doc, IntRect::from_xywh(x, y, 1, 1));
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

viewport_tool!(HandTool, "hand", "Hand", "✋", "h");
viewport_tool!(ZoomTool, "zoom", "Zoom", "🔍", "z");

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
    use photoslop_core::{blit_rgba8, Layer};
    use photoslop_plugin_api::Modifiers;

    fn input(x: f32, y: f32) -> PointerInput {
        PointerInput { x, y, pressure: 1.0, modifiers: Modifiers::default() }
    }

    fn red_square_doc() -> Document {
        let mut doc = Document::new("t", 256, 256, Depth::Eight);
        let mut layer = Layer::new_raster("sq");
        let buf = vec![255u8, 0, 0, 255].repeat(32 * 32);
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
        let mut ctx = ToolCtx { doc: &mut doc, state: &mut state };
        tool.on_pointer_down(&mut ctx, input(20.0, 20.0));
        tool.on_pointer_move(&mut ctx, input(120.0, 70.0));
        tool.on_pointer_up(&mut ctx, input(120.0, 70.0));

        let px = |doc: &Document, x, y| {
            doc.tree.find(id).unwrap().as_raster().unwrap().tiles.pixel(x, y).to_u8()
        };
        assert_eq!(px(&doc, 15, 15)[3], 0, "old spot empty");
        assert_eq!(px(&doc, 115, 65), [255, 0, 0, 255], "moved by (100,50)");

        doc.undo();
        assert_eq!(px(&doc, 15, 15), [255, 0, 0, 255], "undo restores");
        doc.redo();
        assert_eq!(px(&doc, 115, 65), [255, 0, 0, 255]);
    }

    #[test]
    fn move_unaligned_offset_preserves_pixels() {
        let mut doc = red_square_doc();
        let id = doc.active_layer.unwrap();
        let mut state = EditorState::default();
        let mut tool = MoveTool::new();
        let mut ctx = ToolCtx { doc: &mut doc, state: &mut state };
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
        let mut ctx = ToolCtx { doc: &mut doc, state: &mut state };
        tool.on_pointer_down(&mut ctx, input(20.0, 20.0));
        assert_eq!(state.foreground.to_u8(), [255, 0, 0, 255]);
    }
}
