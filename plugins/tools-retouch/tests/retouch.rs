//! Each retouch tool should change what it claims to change and leave the
//! rest alone.

use photoslop_color::{Depth, Rgba};
use photoslop_core::{Document, IntRect, Layer, SelectOp, TileCoord, TILE_SIZE};
use photoslop_plugin_api::{EditorState, Modifiers, PointerInput, ToolCtx, ToolPlugin};
use photoslop_tools_retouch::*;

fn input(x: f32, y: f32) -> PointerInput {
    PointerInput {
        x,
        y,
        pressure: 1.0,
        modifiers: Modifiers::default(),
    }
}

fn set(doc: &mut Document, layer: photoslop_core::LayerId, x: i32, y: i32, c: Rgba) {
    let raster = doc.tree.find_mut(layer).unwrap().as_raster_mut().unwrap();
    let coord = TileCoord::containing(x, y);
    let trect = coord.rect();
    let buf = raster.tiles.get_mut_or_insert(coord, Depth::Eight);
    buf.set(((y - trect.top) * TILE_SIZE + (x - trect.left)) as usize, c);
}

fn get(doc: &Document, x: i32, y: i32) -> Rgba {
    doc.tree
        .iter()
        .next()
        .unwrap()
        .as_raster()
        .unwrap()
        .tiles
        .pixel(x, y)
}

/// 120x120 flat grey, with whatever `paint` puts on top.
fn doc_with(paint: impl Fn(&mut Document, photoslop_core::LayerId)) -> Document {
    let mut doc = Document::new("t", 120, 120, Depth::Eight);
    let layer = Layer::new_raster("bg");
    let id = layer.id;
    doc.push_layer(layer);
    for y in 0..120 {
        for x in 0..120 {
            set(&mut doc, id, x, y, Rgba::new(0.5, 0.5, 0.5, 1.0));
        }
    }
    paint(&mut doc, id);
    doc.active_layer = Some(id);
    doc
}

#[test]
fn red_eye_kills_red_and_leaves_everything_else() {
    let mut doc = doc_with(|doc, id| {
        // A red pupil at (60,60), and a red shirt well away from it.
        for y in 55..65 {
            for x in 55..65 {
                set(doc, id, x, y, Rgba::new(0.9, 0.1, 0.1, 1.0));
            }
        }
        for y in 10..20 {
            for x in 10..20 {
                set(doc, id, x, y, Rgba::new(0.9, 0.1, 0.1, 1.0));
            }
        }
    });
    let mut state = EditorState::default();
    let mut tool = RedEyeTool::default_tool();
    {
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        tool.on_pointer_down(&mut ctx, input(52.0, 52.0));
        tool.on_pointer_up(&mut ctx, input(68.0, 68.0));
    }
    let pupil = get(&doc, 60, 60);
    assert!(pupil.r < 0.4, "pupil is still red: {pupil:?}");
    assert!(pupil.r <= pupil.g + 0.05, "red still leads: {pupil:?}");
    let shirt = get(&doc, 15, 15);
    assert!(
        shirt.r > 0.8,
        "red outside the box was changed too: {shirt:?}"
    );
}

#[test]
fn magic_eraser_clears_the_matching_region_only() {
    let mut doc = doc_with(|doc, id| {
        for y in 40..80 {
            for x in 40..80 {
                set(doc, id, x, y, Rgba::new(0.1, 0.8, 0.2, 1.0));
            }
        }
    });
    let mut state = EditorState::default();
    let mut tool = MagicEraserTool::default_tool();
    {
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        tool.on_pointer_down(&mut ctx, input(60.0, 60.0));
    }
    assert_eq!(get(&doc, 60, 60).a, 0.0, "green square not erased");
    assert_eq!(get(&doc, 10, 10).a, 1.0, "grey background erased too");
}

#[test]
fn inpaint_fills_a_hole_from_its_surroundings() {
    // A flat blue field with a square hole punched in the middle.
    let doc = doc_with(|doc, id| {
        for y in 0..120 {
            for x in 0..120 {
                set(doc, id, x, y, Rgba::new(0.2, 0.3, 0.9, 1.0));
            }
        }
    });
    let tiles = doc
        .tree
        .iter()
        .next()
        .unwrap()
        .as_raster()
        .unwrap()
        .tiles
        .clone();
    let rect = IntRect::new(40, 40, 80, 80);
    let mut hole = vec![false; 40 * 40];
    for y in 8..32 {
        for x in 8..32 {
            hole[y * 40 + x] = true;
        }
    }
    let filled = photoslop_tools_retouch::inpaint(&tiles, rect, &hole);
    let mid = filled[20 * 40 + 20];
    assert!(
        (mid.r - 0.2).abs() < 0.05 && (mid.b - 0.9).abs() < 0.05,
        "hole was not filled with the surrounding colour: {mid:?}"
    );
}

#[test]
fn content_aware_move_moves_the_subject_and_fills_behind_it() {
    let mut doc = doc_with(|doc, id| {
        for y in 30..50 {
            for x in 30..50 {
                set(doc, id, x, y, Rgba::new(0.9, 0.2, 0.2, 1.0));
            }
        }
    });
    doc.selection
        .select_rect(IntRect::new(30, 30, 50, 50), SelectOp::Replace);
    let mut state = EditorState::default();
    let mut tool = ContentAwareMoveTool::default_tool();
    {
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        tool.on_pointer_down(&mut ctx, input(40.0, 40.0));
        tool.on_pointer_up(&mut ctx, input(80.0, 80.0));
    }
    let moved = get(&doc, 80, 80);
    assert!(
        moved.r > 0.8 && moved.g < 0.4,
        "subject did not move: {moved:?}"
    );
    let behind = get(&doc, 40, 40);
    assert!(
        behind.r < 0.7 && (behind.r - behind.g).abs() < 0.15,
        "hole was not filled with background: {behind:?}"
    );
}

#[test]
fn patch_takes_texture_from_the_source_and_colour_from_the_destination() {
    // Left half dark, right half light, with a blemish on the left.
    let mut doc = doc_with(|doc, id| {
        for y in 0..120 {
            for x in 0..120 {
                let v = if x < 60 { 0.3 } else { 0.7 };
                set(doc, id, x, y, Rgba::new(v, v, v, 1.0));
            }
        }
        for y in 25..35 {
            for x in 25..35 {
                set(doc, id, x, y, Rgba::new(0.95, 0.1, 0.1, 1.0));
            }
        }
    });
    doc.selection
        .select_rect(IntRect::new(25, 25, 35, 35), SelectOp::Replace);
    let mut state = EditorState::default();
    let mut tool = PatchTool::default_tool();
    {
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        // Drag onto the light half to sample from there.
        tool.on_pointer_down(&mut ctx, input(30.0, 30.0));
        tool.on_pointer_up(&mut ctx, input(90.0, 30.0));
    }
    let patched = get(&doc, 30, 30);
    assert!(patched.r < 0.5, "blemish is still there: {patched:?}");
    // Colour matched to the dark half it sits in, not the light source.
    assert!(
        patched.r < 0.55,
        "took the source's brightness instead of the destination's: {patched:?}"
    );
}
