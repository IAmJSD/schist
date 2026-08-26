//! A stroke that re-renders only what each dab touched has to land on the
//! same pixels as one sweep of the whole mesh, or Liquify's speed would be
//! bought with seams and stale patches.

use schist_color::{Depth, Rgba};
use schist_core::{IntRect, TileCoord, TileMap};
use schist_tools_warp::mesh::{warp_into, warp_tiles, Mesh};

const RECT: IntRect = IntRect {
    left: 0,
    top: 0,
    right: 300,
    bottom: 220,
};

fn artwork() -> TileMap {
    let mut tiles = TileMap::new();
    for coord in TileCoord::covering(&RECT) {
        let trect = coord.rect();
        let buf = tiles.get_mut_or_insert(coord, Depth::ThirtyTwo);
        for ly in 0..schist_core::TILE_SIZE {
            for lx in 0..schist_core::TILE_SIZE {
                let (x, y) = (trect.left + lx, trect.top + ly);
                // A pattern with detail at every scale, so a stale patch
                // or a fetch from the wrong place shows up as a mismatch.
                let v = (((x * 7) ^ (y * 13)) % 255) as f32 / 255.0;
                buf.set(
                    (ly * schist_core::TILE_SIZE + lx) as usize,
                    Rgba::new(v, 1.0 - v, (x % 32) as f32 / 32.0, 1.0),
                );
            }
        }
    }
    tiles
}

/// The dabs of one stroke: a forward push, a twirl and a bloat, so the
/// mesh ends up with offsets of both signs on both axes.
fn dabs(mesh: &mut Mesh, i: usize) -> IntRect {
    let (x, y) = (60.0 + i as f32 * 9.0, 70.0 + (i as f32 * 5.0).sin() * 30.0);
    let radius = 24.0 + (i % 3) as f32 * 12.0;
    let dirty = mesh.dab_rect(x, y, radius);
    match i % 3 {
        0 => mesh.for_each_near(x, y, radius, |off, w, _, _| {
            off.0 -= 9.0 * w;
            off.1 -= 4.0 * w;
        }),
        1 => mesh.for_each_near(x, y, radius, |off, w, rx, ry| {
            let (s, c) = (0.3 * w).sin_cos();
            let (fx, fy) = (rx + off.0, ry + off.1);
            off.0 = fx * c - fy * s - rx;
            off.1 = fx * s + fy * c - ry;
        }),
        _ => mesh.for_each_near(x, y, radius, |off, w, rx, ry| {
            let (fx, fy) = (rx + off.0, ry + off.1);
            off.0 -= fx * 0.2 * w;
            off.1 -= fy * 0.2 * w;
        }),
    }
    dirty
}

#[test]
fn rendering_dab_by_dab_matches_one_sweep_of_the_whole_mesh() {
    let src = artwork();
    let mut mesh = Mesh::new(RECT);
    // The stroke, each dab re-rendering only its own footprint on top of
    // what the last one left, which is what the tool does.
    let mut running = src.clone();
    for i in 0..24 {
        let dirty = dabs(&mut mesh, i);
        warp_into(&mut running, &src, &mesh, Depth::ThirtyTwo, dirty, 0);
    }
    // The same mesh, applied in one pass over everything, with the source
    // handed over whole under a token.
    let swept = warp_tiles(&src, &mesh, Depth::ThirtyTwo, RECT, 7);

    for y in RECT.top..RECT.bottom {
        for x in RECT.left..RECT.right {
            let (a, b) = (running.pixel(x, y), swept.pixel(x, y));
            assert!(
                (a.r - b.r).abs() < 1e-6
                    && (a.g - b.g).abs() < 1e-6
                    && (a.b - b.b).abs() < 1e-6
                    && (a.a - b.a).abs() < 1e-6,
                "({x}, {y}) drifted: incremental {a:?}, swept {b:?}"
            );
        }
    }
}

#[test]
fn a_dab_leaves_everything_outside_its_footprint_alone() {
    let src = artwork();
    let mut mesh = Mesh::new(RECT);
    let mut out = src.clone();
    let dirty = dabs(&mut mesh, 0);
    warp_into(&mut out, &src, &mesh, Depth::ThirtyTwo, dirty, 0);
    for y in RECT.top..RECT.bottom {
        for x in RECT.left..RECT.right {
            if dirty.contains(x, y) {
                continue;
            }
            assert_eq!(
                (out.pixel(x, y).r, out.pixel(x, y).a),
                (src.pixel(x, y).r, src.pixel(x, y).a),
                "({x}, {y}) changed outside the dab"
            );
        }
    }
}

#[test]
fn a_cropped_source_plane_reaches_as_far_as_the_displacement_does() {
    // One big push, rendered through a window far smaller than the throw,
    // so the resample has to read well outside the window it is filling.
    let src = artwork();
    let mut mesh = Mesh::new(RECT);
    mesh.for_each_near(150.0, 110.0, 100.0, |off, w, _, _| off.0 -= 60.0 * w);
    let window = IntRect::new(140, 100, 160, 120);
    let mut cropped = TileMap::new();
    warp_into(&mut cropped, &src, &mesh, Depth::ThirtyTwo, window, 0);
    let whole = warp_tiles(&src, &mesh, Depth::ThirtyTwo, RECT, 11);
    for y in window.top..window.bottom {
        for x in window.left..window.right {
            let (a, b) = (cropped.pixel(x, y), whole.pixel(x, y));
            assert!(
                (a.r - b.r).abs() < 1e-6 && (a.a - b.a).abs() < 1e-6,
                "({x}, {y}) lost pixels the crop should have covered: {a:?} vs {b:?}"
            );
        }
    }
}

// ===== the tool itself =====

use schist_core::{Document, Layer};
use schist_plugin_api::{EditorState, OptionValue, PointerInput, ToolCtx, ToolPlugin};
use schist_tools_warp::liquify::LiquifyTool;

fn document() -> (Document, schist_core::LayerId) {
    let mut doc = Document::new("t", 300, 220, Depth::ThirtyTwo);
    let mut layer = Layer::new_raster("art");
    if let Some(raster) = layer.as_raster_mut() {
        raster.tiles = artwork();
    }
    let id = layer.id;
    doc.tree.layers.push(layer);
    doc.active_layer = Some(id);
    (doc, id)
}

fn at(x: f32, y: f32) -> PointerInput {
    PointerInput {
        x,
        y,
        pressure: 1.0,
        modifiers: Default::default(),
    }
}

fn tiles_of(doc: &Document, id: schist_core::LayerId) -> TileMap {
    doc.tree
        .find(id)
        .and_then(|l| l.as_raster())
        .map(|r| r.tiles.clone())
        .unwrap()
}

fn stroke(tool: &mut LiquifyTool, ctx: &mut ToolCtx) {
    tool.on_pointer_down(ctx, at(100.0, 110.0));
    for i in 1..=20 {
        tool.on_pointer_move(ctx, at(100.0 + i as f32 * 4.0, 110.0));
    }
    tool.on_pointer_up(ctx, at(180.0, 110.0));
}

#[test]
fn a_pointer_move_damages_the_brush_and_not_the_layer() {
    let (mut doc, _) = document();
    let mut state = EditorState::default();
    let mut tool = LiquifyTool::new();
    let mut ctx = ToolCtx {
        doc: &mut doc,
        state: &mut state,
    };
    tool.on_activate(&mut ctx);
    tool.on_pointer_down(&mut ctx, at(100.0, 110.0));
    ctx.doc.take_damage();
    tool.on_pointer_move(&mut ctx, at(112.0, 110.0));
    let damage = ctx.doc.take_damage();
    let area: i64 = damage
        .iter()
        .map(|r| r.width() as i64 * r.height() as i64)
        .sum();
    // The default brush is 100 across; a footprint plus a cell of slop is
    // around 12 000 pixels, where the whole layer is 66 000.
    assert!(
        area < 20_000,
        "a dab repainted {area} pixels, which is most of the layer"
    );
}

#[test]
fn committing_writes_the_warp_and_undo_takes_it_back() {
    let (mut doc, id) = document();
    let before = tiles_of(&doc, id);
    let mut state = EditorState::default();
    let mut tool = LiquifyTool::new();
    {
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        tool.on_activate(&mut ctx);
        stroke(&mut tool, &mut ctx);
        tool.on_commit(&mut ctx);
    }
    let after = tiles_of(&doc, id);
    let moved = (0..220)
        .flat_map(|y| (0..300).map(move |x| (x, y)))
        .filter(|&(x, y)| (after.pixel(x, y).r - before.pixel(x, y).r).abs() > 1e-4)
        .count();
    assert!(moved > 1000, "the stroke barely changed anything: {moved}");
    // Nothing outside the brush's reach along the stroke moved. (Inside
    // it, even a pixel the mesh did not displace comes back through the
    // resample, which costs it a bit of float.)
    for y in 0..220 {
        for x in 0..300 {
            if (40..=240).contains(&x) && (50..=175).contains(&y) {
                continue;
            }
            assert_eq!(
                after.pixel(x, y).r,
                before.pixel(x, y).r,
                "({x}, {y}) changed, far from the stroke"
            );
        }
    }
    assert_eq!(doc.undo().as_deref(), Some("Liquify"));
    let undone = tiles_of(&doc, id);
    for y in 0..220 {
        for x in 0..300 {
            assert_eq!(
                undone.pixel(x, y).r,
                before.pixel(x, y).r,
                "undo left ({x}, {y}) warped"
            );
        }
    }
}

#[test]
fn escape_puts_the_pixels_back() {
    let (mut doc, id) = document();
    let before = tiles_of(&doc, id);
    let mut state = EditorState::default();
    let mut tool = LiquifyTool::new();
    let mut ctx = ToolCtx {
        doc: &mut doc,
        state: &mut state,
    };
    tool.on_activate(&mut ctx);
    stroke(&mut tool, &mut ctx);
    tool.on_cancel(&mut ctx);
    let after = tiles_of(ctx.doc, id);
    for y in 0..220 {
        for x in 0..300 {
            assert_eq!(
                after.pixel(x, y).r,
                before.pixel(x, y).r,
                "cancel left ({x}, {y}) warped"
            );
        }
    }
}

/// Raising Size mid-session must widen the mesh.
///
/// The extent is cut in `begin`, which only runs on activate, on the
/// first press, and after a commit — so the extra reach past the
/// artwork's edge stayed unavailable and pixels could not be pushed as
/// far as the new brush promised.
#[test]
fn raising_the_brush_size_recuts_the_mesh() {
    let mut doc = Document::new("t", 2000, 2000, Depth::Eight);
    let mut layer = Layer::new_raster("art");
    let buf = [200u8, 30, 30, 255].repeat(64 * 64);
    schist_core::blit_rgba8(
        &mut layer.as_raster_mut().unwrap().tiles,
        Depth::Eight,
        schist_core::IntRect::from_xywh(900, 900, 64, 64),
        &buf,
    );
    let id = layer.id;
    doc.tree.layers.push(layer);
    doc.active_layer = Some(id);

    let mut state = EditorState::default();
    let mut tool = LiquifyTool::new();
    let mut ctx = ToolCtx {
        doc: &mut doc,
        state: &mut state,
    };
    tool.on_activate(&mut ctx);
    let small = tool.mesh_rect().expect("a session");

    tool.set_option("liquify-size", OptionValue::Num(800.0));
    tool.on_pointer_down(&mut ctx, at(930.0, 930.0));
    let large = tool.mesh_rect().expect("a session");

    assert!(
        large.width() > small.width() && large.height() > small.height(),
        "mesh stayed {small:?} after the brush grew to 800 (now {large:?})"
    );
}

/// But not once pixels have been pushed: re-cutting the grid would throw
/// the accumulated offsets away, and Enter re-cuts it anyway.
#[test]
fn a_warp_in_progress_keeps_its_mesh() {
    let (mut doc, _id) = document();
    let mut state = EditorState::default();
    let mut tool = LiquifyTool::new();
    let mut ctx = ToolCtx {
        doc: &mut doc,
        state: &mut state,
    };
    tool.on_activate(&mut ctx);
    stroke(&mut tool, &mut ctx);
    let warped = tool.mesh_rect().expect("a session");

    tool.set_option("liquify-size", OptionValue::Num(800.0));
    tool.on_pointer_down(&mut ctx, at(100.0, 110.0));

    assert_eq!(
        tool.mesh_rect(),
        Some(warped),
        "the size slider re-cut a mesh that already held a warp"
    );

    // Committing re-cuts it for the new brush, which is where the extra
    // reach becomes available.
    tool.on_commit(&mut ctx);
    let after = tool.mesh_rect().expect("a session");
    assert!(after.width() >= warped.width());
}
