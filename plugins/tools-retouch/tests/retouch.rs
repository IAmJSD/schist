//! Each retouch tool should change what it claims to change and leave the
//! rest alone.

use schist_color::{Depth, Rgba};
use schist_core::{Document, IntRect, Layer, SelectOp, TileCoord, TILE_SIZE};
use schist_plugin_api::{EditorState, Modifiers, PointerInput, ToolCtx, ToolPlugin};
use schist_tools_retouch::*;

fn input(x: f32, y: f32) -> PointerInput {
    PointerInput {
        x,
        y,
        pressure: 1.0,
        modifiers: Modifiers::default(),
    }
}

fn set(doc: &mut Document, layer: schist_core::LayerId, x: i32, y: i32, c: Rgba) {
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
fn doc_with(paint: impl Fn(&mut Document, schist_core::LayerId)) -> Document {
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
    let filled = schist_tools_retouch::inpaint(&tiles, rect, &hole);
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

#[test]
fn content_aware_fill_puts_texture_back_rather_than_an_average() {
    // The whole point, and the one thing diffusion could never do: a
    // hole in a textured surface has to come back textured. A fill that
    // relaxed the boundary inwards would leave the middle of this
    // perfectly smooth, which is exactly what the assertion below
    // measures and what it used to fail.
    let doc = doc_with(|doc, id| {
        let mut seed = 0x9e37_79b9u32;
        for y in 0..120 {
            for x in 0..120 {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let n = (seed >> 8) as f32 / u32::MAX as f32;
                // Coarse stripes with grain on top: structure to follow
                // and texture to reproduce.
                let base = 0.35 + 0.25 * (((x + y) / 7) % 2) as f32;
                let v = (base + n * 0.22).clamp(0.0, 1.0);
                set(doc, id, x, y, Rgba::new(v, v * 0.9, v * 0.7, 1.0));
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
    let rect = IntRect::new(20, 20, 100, 100);
    let (w, h) = (80usize, 80usize);
    let mut hole = vec![false; w * h];
    for y in 20..60 {
        for x in 20..60 {
            hole[y * w + x] = true;
        }
    }
    let filled = schist_tools_retouch::inpaint(&tiles, rect, &hole);

    // Mean absolute difference between neighbouring pixels, well inside
    // the hole against the same measure on real pixels.
    let rough = |x0: usize, y0: usize| {
        let (mut acc, mut n) = (0.0f32, 0.0f32);
        for y in y0..y0 + 16 {
            for x in x0..x0 + 16 {
                acc += (filled[y * w + x].r - filled[y * w + x + 1].r).abs();
                acc += (filled[y * w + x].r - filled[(y + 1) * w + x].r).abs();
                n += 2.0;
            }
        }
        acc / n
    };
    let (inside, outside) = (rough(32, 32), rough(2, 2));
    assert!(
        inside > outside * 0.5,
        "the hole came back smooth: {inside:.4} against {outside:.4} outside it"
    );
    // And it is the picture's texture, not invented brightness.
    let mean = |x0: usize, y0: usize| {
        let mut acc = 0.0f32;
        for y in y0..y0 + 16 {
            for x in x0..x0 + 16 {
                acc += filled[y * w + x].r;
            }
        }
        acc / 256.0
    };
    assert!(
        (mean(32, 32) - mean(2, 2)).abs() < 0.12,
        "the fill is the wrong brightness: {:.3} against {:.3}",
        mean(32, 32),
        mean(2, 2)
    );
}

#[test]
fn red_eye_stays_inside_the_selection() {
    // Every sibling tool in this file gates on `sel.coverage`; red eye
    // did not, so it recoloured matching pixels outside the selection.
    let mut doc = doc_with(|doc, id| {
        for y in 40..80 {
            for x in 20..100 {
                set(doc, id, x, y, Rgba::new(0.9, 0.1, 0.1, 1.0));
            }
        }
    });
    // Select only the left half of the red band.
    doc.selection
        .select_rect(IntRect::from_xywh(0, 0, 60, 120), SelectOp::Replace);

    let mut state = EditorState::default();
    let mut tool = RedEyeTool::default_tool();
    {
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        tool.on_pointer_down(&mut ctx, input(20.0, 40.0));
        tool.on_pointer_move(&mut ctx, input(100.0, 80.0));
        tool.on_pointer_up(&mut ctx, input(100.0, 80.0));
    }

    let inside = get(&doc, 40, 60);
    let outside = get(&doc, 80, 60);
    assert!(
        inside.r < 0.5,
        "inside the selection must be corrected: {inside:?}"
    );
    assert!(
        (outside.r - 0.9).abs() < 0.05,
        "outside must be untouched: {outside:?}"
    );
}
