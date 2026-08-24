//! Committing a puppet warp must keep the deformation the preview showed.
//!
//! The mesh is solved from the pin list, so anything that empties that
//! list before solving yields the identity and silently discards the
//! whole warp.

use schist_color::{Depth, Rgba};
use schist_core::{Document, Layer, LayerId, TileCoord, TILE_SIZE};
use schist_plugin_api::{EditorState, Modifiers, PointerInput, ToolCtx, ToolPlugin};
use schist_tools_warp::puppet::PuppetWarpTool;

const W: i32 = 200;
const H: i32 = 200;

fn input(x: f32, y: f32) -> PointerInput {
    PointerInput {
        x,
        y,
        pressure: 1.0,
        modifiers: Modifiers::default(),
    }
}

fn set(doc: &mut Document, layer: LayerId, x: i32, y: i32, c: Rgba) {
    let raster = doc.tree.find_mut(layer).unwrap().as_raster_mut().unwrap();
    let coord = TileCoord::containing(x, y);
    let trect = coord.rect();
    let buf = raster.tiles.get_mut_or_insert(coord, Depth::Eight);
    buf.set(((y - trect.top) * TILE_SIZE + (x - trect.left)) as usize, c);
}

fn pixel(doc: &Document, x: i32, y: i32) -> Rgba {
    doc.tree
        .iter()
        .next()
        .unwrap()
        .as_raster()
        .unwrap()
        .tiles
        .pixel(x, y)
}

/// A vertical colour ramp, so moving pixels sideways is invisible but
/// moving them vertically is not, and every row is distinguishable.
fn doc_with_ramp() -> (Document, LayerId) {
    let mut doc = Document::new("t", W as u32, H as u32, Depth::Eight);
    let layer = Layer::new_raster("bg");
    let id = layer.id;
    doc.push_layer(layer);
    for y in 0..H {
        let v = y as f32 / H as f32;
        for x in 0..W {
            set(&mut doc, id, x, y, Rgba::new(v, 1.0 - v, 0.25, 1.0));
        }
    }
    doc.active_layer = Some(id);
    (doc, id)
}

fn snapshot(doc: &Document) -> Vec<Rgba> {
    let mut out = Vec::with_capacity((W * H) as usize);
    for y in 0..H {
        for x in 0..W {
            out.push(pixel(doc, x, y));
        }
    }
    out
}

fn differing(a: &[Rgba], b: &[Rgba]) -> usize {
    a.iter()
        .zip(b)
        .filter(|(p, q)| {
            (p.r - q.r).abs() > 1e-6 || (p.g - q.g).abs() > 1e-6 || (p.b - q.b).abs() > 1e-6
        })
        .count()
}

/// Drop an anchor pin at each corner, then drag a pin in the middle
/// downward, exactly as a user would.
fn drag_centre_pin(tool: &mut PuppetWarpTool, ctx: &mut ToolCtx, dy: f32) {
    for (x, y) in [(10.0, 10.0), (190.0, 10.0), (10.0, 190.0), (190.0, 190.0)] {
        tool.on_pointer_down(ctx, input(x, y));
        tool.on_pointer_up(ctx, input(x, y));
    }
    tool.on_pointer_down(ctx, input(100.0, 100.0));
    tool.on_pointer_move(ctx, input(100.0, 100.0 + dy));
    tool.on_pointer_up(ctx, input(100.0, 100.0 + dy));
}

#[test]
fn committing_keeps_the_warp() {
    let (mut doc, _id) = doc_with_ramp();
    let mut state = EditorState::default();
    let mut tool = PuppetWarpTool::new();

    let original = snapshot(&doc);

    let mut ctx = ToolCtx {
        doc: &mut doc,
        state: &mut state,
    };
    tool.on_activate(&mut ctx);
    drag_centre_pin(&mut tool, &mut ctx, 40.0);

    // The live preview must already differ, or the test is not exercising
    // a real deformation.
    let previewed = snapshot(ctx.doc);
    assert!(
        differing(&original, &previewed) > 1000,
        "preview did not deform the layer"
    );

    tool.on_commit(&mut ctx);

    let committed = snapshot(&doc);
    let changed = differing(&original, &committed);
    assert!(
        changed > 1000,
        "commit discarded the warp: only {changed} pixels differ from the original"
    );
}

#[test]
fn committing_matches_what_the_preview_showed() {
    let (mut doc, _id) = doc_with_ramp();
    let mut state = EditorState::default();
    let mut tool = PuppetWarpTool::new();
    let mut ctx = ToolCtx {
        doc: &mut doc,
        state: &mut state,
    };

    tool.on_activate(&mut ctx);
    drag_centre_pin(&mut tool, &mut ctx, 40.0);
    let previewed = snapshot(ctx.doc);

    tool.on_commit(&mut ctx);
    let committed = snapshot(ctx.doc);

    assert_eq!(
        differing(&previewed, &committed),
        0,
        "committed pixels do not match the preview"
    );
}

#[test]
fn undo_restores_the_original() {
    let (mut doc, _id) = doc_with_ramp();
    let mut state = EditorState::default();
    let mut tool = PuppetWarpTool::new();
    let original = snapshot(&doc);

    let mut ctx = ToolCtx {
        doc: &mut doc,
        state: &mut state,
    };
    tool.on_activate(&mut ctx);
    drag_centre_pin(&mut tool, &mut ctx, 40.0);
    tool.on_commit(&mut ctx);

    doc.undo();
    assert_eq!(
        differing(&original, &snapshot(&doc)),
        0,
        "undo did not restore the pre-warp pixels"
    );
}

#[test]
fn pins_left_untouched_commit_nothing() {
    let (mut doc, _id) = doc_with_ramp();
    let mut state = EditorState::default();
    let mut tool = PuppetWarpTool::new();
    let original = snapshot(&doc);

    let mut ctx = ToolCtx {
        doc: &mut doc,
        state: &mut state,
    };
    tool.on_activate(&mut ctx);
    // Drop pins but never drag one.
    for (x, y) in [(50.0, 50.0), (150.0, 150.0)] {
        tool.on_pointer_down(&mut ctx, input(x, y));
        tool.on_pointer_up(&mut ctx, input(x, y));
    }
    tool.on_commit(&mut ctx);

    assert_eq!(
        differing(&original, &snapshot(&doc)),
        0,
        "an undragged pin set should leave the layer alone"
    );
}
