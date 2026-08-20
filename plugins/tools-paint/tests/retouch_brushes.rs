//! The brush-based retouch tools: healing, spot healing, blur, sharpen,
//! smudge, background eraser and the history brush.

use photoslop_color::{Depth, Rgba};
use photoslop_core::{Document, Layer, LayerId, TileCoord, TILE_SIZE};
use photoslop_plugin_api::{EditorState, Modifiers, PointerInput, ToolCtx, ToolPlugin};

fn input(x: f32, y: f32) -> PointerInput {
    PointerInput {
        x,
        y,
        pressure: 1.0,
        modifiers: Modifiers::default(),
    }
}

fn alt(x: f32, y: f32) -> PointerInput {
    PointerInput {
        modifiers: Modifiers {
            alt: true,
            ..Default::default()
        },
        ..input(x, y)
    }
}

fn set(doc: &mut Document, layer: LayerId, x: i32, y: i32, c: Rgba) {
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

fn doc_with(paint: impl Fn(&mut Document, LayerId)) -> Document {
    let mut doc = Document::new("t", 140, 140, Depth::Eight);
    let layer = Layer::new_raster("bg");
    let id = layer.id;
    doc.push_layer(layer);
    for y in 0..140 {
        for x in 0..140 {
            set(&mut doc, id, x, y, Rgba::new(0.6, 0.5, 0.45, 1.0));
        }
    }
    paint(&mut doc, id);
    doc.active_layer = Some(id);
    doc.snapshot_history_source();
    doc
}

fn tool(id: &str) -> Box<dyn ToolPlugin> {
    photoslop_tools_paint::tool_for_test(id).expect("unknown tool")
}

/// A blemish on flat skin should come out the colour of the skin.
#[test]
fn healing_brush_takes_colour_from_around_the_dab() {
    let mut doc = doc_with(|doc, id| {
        for y in 66..74 {
            for x in 66..74 {
                set(doc, id, x, y, Rgba::new(0.95, 0.15, 0.15, 1.0));
            }
        }
    });
    let mut state = EditorState {
        brush_size: 22.0,
        brush_hardness: 1.0,
        ..Default::default()
    };
    let mut t = tool("heal");
    let mut ctx = ToolCtx {
        doc: &mut doc,
        state: &mut state,
    };
    // Alt-click clean skin, then paint over the blemish.
    t.on_pointer_down(&mut ctx, alt(30.0, 30.0));
    t.on_pointer_down(&mut ctx, input(70.0, 70.0));
    t.on_pointer_up(&mut ctx, input(70.0, 70.0));

    let px = get(&doc, 70, 70);
    assert!(
        px.r < 0.75 && px.g > 0.35,
        "blemish colour survived healing: {px:?}"
    );
}

#[test]
fn spot_healing_removes_a_blemish_with_no_source_point() {
    let mut doc = doc_with(|doc, id| {
        for y in 66..74 {
            for x in 66..74 {
                set(doc, id, x, y, Rgba::new(0.95, 0.15, 0.15, 1.0));
            }
        }
    });
    let mut state = EditorState {
        brush_size: 24.0,
        brush_hardness: 1.0,
        ..Default::default()
    };
    let mut t = tool("spot_heal");
    let mut ctx = ToolCtx {
        doc: &mut doc,
        state: &mut state,
    };
    t.on_pointer_down(&mut ctx, input(70.0, 70.0));
    t.on_pointer_up(&mut ctx, input(70.0, 70.0));

    let px = get(&doc, 70, 70);
    assert!(
        (px.r - 0.6).abs() < 0.12 && (px.g - 0.5).abs() < 0.12,
        "spot healing did not reach the skin colour: {px:?}"
    );
}

#[test]
fn blur_tool_softens_an_edge_and_sharpen_hardens_it() {
    let make = || {
        doc_with(|doc, id| {
            for y in 0..140 {
                for x in 0..140 {
                    let v = if x < 70 { 0.2 } else { 0.8 };
                    set(doc, id, x, y, Rgba::new(v, v, v, 1.0));
                }
            }
        })
    };
    let contrast = |doc: &Document| (get(doc, 71, 70).r - get(doc, 68, 70).r).abs();

    let mut doc = make();
    let before = contrast(&doc);
    let mut state = EditorState {
        brush_size: 30.0,
        brush_hardness: 1.0,
        ..Default::default()
    };
    {
        let mut t = tool("blur");
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        for _ in 0..6 {
            t.on_pointer_down(&mut ctx, input(70.0, 70.0));
            t.on_pointer_up(&mut ctx, input(70.0, 70.0));
        }
    }
    assert!(
        contrast(&doc) < before,
        "blur did not soften the edge ({} vs {before})",
        contrast(&doc)
    );

    let mut doc = make();
    {
        let mut t = tool("sharpen");
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        t.on_pointer_down(&mut ctx, input(70.0, 70.0));
        t.on_pointer_up(&mut ctx, input(70.0, 70.0));
    }
    // Sharpening a step edge overshoots on the two pixels either side of
    // the step itself -- further out, every neighbour is the same value
    // and an unsharp mask has nothing to do.
    let (dark, light) = (get(&doc, 69, 70).r, get(&doc, 70, 70).r);
    assert!(
        dark < 0.2 && light > 0.8,
        "sharpen produced no overshoot at the step: {dark} / {light}"
    );
}

#[test]
fn smudge_drags_colour_along_the_stroke() {
    let mut doc = doc_with(|doc, id| {
        for y in 0..140 {
            for x in 60..70 {
                set(doc, id, x, y, Rgba::new(0.1, 0.1, 0.9, 1.0));
            }
        }
    });
    let mut state = EditorState {
        brush_size: 20.0,
        brush_hardness: 1.0,
        ..Default::default()
    };
    let mut t = tool("smudge");
    let mut ctx = ToolCtx {
        doc: &mut doc,
        state: &mut state,
    };
    t.on_pointer_down(&mut ctx, input(64.0, 70.0));
    for x in 65..95 {
        t.on_pointer_move(&mut ctx, input(x as f32, 70.0));
    }
    t.on_pointer_up(&mut ctx, input(95.0, 70.0));

    // Blue should have been dragged to the right of the stripe.
    let px = get(&doc, 78, 70);
    assert!(px.b > px.r + 0.05, "no colour was dragged along: {px:?}");
}

#[test]
fn background_eraser_only_removes_the_sampled_colour() {
    let mut doc = doc_with(|doc, id| {
        // Right half is a distinct green "background".
        for y in 0..140 {
            for x in 70..140 {
                set(doc, id, x, y, Rgba::new(0.1, 0.8, 0.2, 1.0));
            }
        }
    });
    let mut state = EditorState {
        brush_size: 40.0,
        brush_hardness: 1.0,
        ..Default::default()
    };
    let mut t = tool("background_eraser");
    let mut ctx = ToolCtx {
        doc: &mut doc,
        state: &mut state,
    };
    // Centre the brush on the seam, sampling the green.
    t.on_pointer_down(&mut ctx, input(80.0, 70.0));
    t.on_pointer_up(&mut ctx, input(80.0, 70.0));

    assert!(get(&doc, 85, 70).a < 0.5, "green background was not erased");
    assert!(
        get(&doc, 65, 70).a > 0.9,
        "the skin side was erased as well"
    );
}

#[test]
fn history_brush_paints_back_the_opened_state() {
    let mut doc = doc_with(|_, _| {});
    let id = doc.active_layer.unwrap();
    // Scribble after the snapshot was taken.
    for y in 60..80 {
        for x in 60..80 {
            set(&mut doc, id, x, y, Rgba::new(1.0, 0.0, 1.0, 1.0));
        }
    }
    let mut state = EditorState {
        brush_size: 30.0,
        brush_hardness: 1.0,
        ..Default::default()
    };
    let mut t = tool("history_brush");
    let mut ctx = ToolCtx {
        doc: &mut doc,
        state: &mut state,
    };
    t.on_pointer_down(&mut ctx, input(70.0, 70.0));
    t.on_pointer_up(&mut ctx, input(70.0, 70.0));

    let px = get(&doc, 70, 70);
    assert!(
        (px.r - 0.6).abs() < 0.05 && px.b < 0.5,
        "history brush did not restore the original: {px:?}"
    );
}
