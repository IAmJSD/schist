//! Vanishing Point clones onto the active layer, and has to obey the same
//! two rules every other paint tool does: not onto a locked or hidden
//! layer, and not outside the selection.

use schist_color::{Depth, Rgba};
use schist_core::{Document, IntRect, Layer, LayerId, SelectOp, TileCoord};
use schist_plugin_api::{EditorState, Modifiers, OptionValue, PointerInput, ToolCtx, ToolPlugin};
use schist_tools_warp::perspective::VanishingPointTool;

fn doc() -> (Document, LayerId) {
    let mut doc = Document::new("t", 200, 200, Depth::ThirtyTwo);
    doc.push_layer(Layer::new_raster("art"));
    let id = doc.tree.layers[0].id;
    let raster = doc.tree.find_mut(id).unwrap().as_raster_mut().unwrap();
    // A left half to clone from, so a stamp on the right is visible.
    for coord in TileCoord::covering(&IntRect::from_xywh(0, 0, 200, 200)) {
        let trect = coord.rect();
        let buf = raster.tiles.get_mut_or_insert(coord, Depth::ThirtyTwo);
        for ly in 0..schist_core::TILE_SIZE {
            for lx in 0..schist_core::TILE_SIZE {
                let x = trect.left + lx;
                let c = if x < 100 {
                    Rgba::new(1.0, 0.0, 0.0, 1.0)
                } else {
                    Rgba::new(0.0, 0.0, 1.0, 1.0)
                };
                buf.set((ly * schist_core::TILE_SIZE + lx) as usize, c);
            }
        }
    }
    doc.active_layer = Some(id);
    (doc, id)
}

fn at(x: f32, y: f32, alt: bool) -> PointerInput {
    PointerInput {
        x,
        y,
        pressure: 1.0,
        modifiers: Modifiers {
            alt,
            ..Default::default()
        },
    }
}

fn px(doc: &Document, id: LayerId, x: i32, y: i32) -> Rgba {
    doc.tree
        .find(id)
        .unwrap()
        .as_raster()
        .unwrap()
        .tiles
        .pixel(x, y)
}

/// Alt-pick inside the red half, then stamp at `to`.
fn stamp(tool: &mut VanishingPointTool, doc: &mut Document, to: (f32, f32)) {
    let mut state = EditorState::default();
    let mut ctx = ToolCtx {
        doc,
        state: &mut state,
    };
    tool.on_activate(&mut ctx);
    tool.set_option("vp-phase", OptionValue::Choice(1));
    tool.on_pointer_down(&mut ctx, at(50.0, 100.0, true));
    tool.on_pointer_down(&mut ctx, at(to.0, to.1, false));
    tool.on_pointer_up(&mut ctx, at(to.0, to.1, false));
}

#[test]
fn the_stamp_lands_on_an_ordinary_layer() {
    let (mut doc, id) = doc();
    let mut tool = VanishingPointTool::new();
    stamp(&mut tool, &mut doc, (150.0, 100.0));
    assert!(
        px(&doc, id, 150, 100).r > 0.5,
        "the red source should have been cloned across"
    );
}

#[test]
fn a_locked_layer_refuses_the_stamp() {
    let (mut doc, id) = doc();
    doc.tree.find_mut(id).unwrap().locked = true;
    let mut tool = VanishingPointTool::new();
    stamp(&mut tool, &mut doc, (150.0, 100.0));
    assert_eq!(px(&doc, id, 150, 100), Rgba::new(0.0, 0.0, 1.0, 1.0));
    assert_eq!(doc.history.undo_name(), None, "and records no edit");
}

#[test]
fn a_hidden_layer_refuses_the_stamp() {
    let (mut doc, id) = doc();
    doc.tree.find_mut(id).unwrap().visible = false;
    let mut tool = VanishingPointTool::new();
    stamp(&mut tool, &mut doc, (150.0, 100.0));
    assert_eq!(px(&doc, id, 150, 100), Rgba::new(0.0, 0.0, 1.0, 1.0));
}

#[test]
fn the_stamp_stays_inside_the_selection() {
    let (mut doc, id) = doc();
    // Only the top half is selected; the dab covers both.
    doc.selection
        .select_rect(IntRect::from_xywh(0, 0, 200, 100), SelectOp::Replace);
    let mut tool = VanishingPointTool::new();
    stamp(&mut tool, &mut doc, (150.0, 100.0));
    assert!(px(&doc, id, 150, 95).r > 0.5, "inside the selection");
    assert_eq!(
        px(&doc, id, 150, 110),
        Rgba::new(0.0, 0.0, 1.0, 1.0),
        "outside it the layer is untouched"
    );
}
