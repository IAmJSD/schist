//! Each retouch tool should change what it claims to change and leave the
//! rest alone.

use schist_color::{Depth, Rgba};
use schist_core::{Document, IntRect, Layer, SelectOp, TileCoord, TileMap, TILE_SIZE};
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

/// Parallelising the inpaint must not change what it produces.
///
/// It is 160 sweeps of the padded selection bounds, run synchronously on
/// pointer release: over a 1500x1500 selection the window locked up for
/// seconds. Each row now reads the previous pass and writes only its own
/// slice, so the iteration is unchanged — only spread over the cores.
#[test]
fn the_inpaint_matches_a_sequential_jacobi_solve() {
    let mut tiles = TileMap::default();
    let rect = IntRect::from_xywh(0, 0, 40, 40);
    for y in 0..40 {
        for x in 0..40 {
            let v = (x + y) as f32 / 80.0;
            let coord = TileCoord::containing(x, y);
            let trect = coord.rect();
            let buf = tiles.get_mut_or_insert(coord, Depth::Eight);
            buf.set(
                ((y - trect.top) * TILE_SIZE + (x - trect.left)) as usize,
                Rgba::new(v, 1.0 - v, 0.5, 1.0),
            );
        }
    }
    // A square hole in the middle.
    let hole: Vec<bool> = (0..40 * 40)
        .map(|i| {
            let (x, y) = (i % 40, i / 40);
            (12..28).contains(&x) && (12..28).contains(&y)
        })
        .collect();

    let got = schist_tools_retouch::inpaint(&tiles, rect, &hole);
    let want = sequential_inpaint(&tiles, rect, &hole);
    assert_eq!(got.len(), want.len());
    for (i, (g, w)) in got.iter().zip(&want).enumerate() {
        assert!(
            (g.r - w.r).abs() < 1e-5 && (g.g - w.g).abs() < 1e-5 && (g.b - w.b).abs() < 1e-5,
            "pixel {i}: {g:?} != {w:?}"
        );
    }
    // And it actually filled the hole with something plausible.
    let centre = got[20 * 40 + 20];
    assert!(centre.a > 0.99 && centre.r > 0.0);
}

/// The inpaint's inner loop, written the way it was before rayon.
fn sequential_inpaint(tiles: &TileMap, rect: IntRect, hole: &[bool]) -> Vec<Rgba> {
    let (w, h) = (rect.width() as usize, rect.height() as usize);
    let mut buf: Vec<Rgba> = (0..w * h)
        .map(|i| tiles.pixel(rect.left + (i % w) as i32, rect.top + (i / w) as i32))
        .collect();
    // Seed the hole with the mean of everything outside it, as `inpaint`
    // does.
    let mut acc = [0f32; 4];
    let mut n = 0f32;
    for (i, px) in buf.iter().enumerate() {
        if !hole[i] {
            acc[0] += px.r;
            acc[1] += px.g;
            acc[2] += px.b;
            acc[3] += px.a;
            n += 1.0;
        }
    }
    let seed = if n > 0.0 {
        Rgba::new(acc[0] / n, acc[1] / n, acc[2] / n, acc[3] / n)
    } else {
        Rgba::new(0.0, 0.0, 0.0, 0.0)
    };
    for i in 0..buf.len() {
        if hole[i] {
            buf[i] = seed;
        }
    }
    let passes = (w.min(h) as u32).clamp(8, 160);
    let mut next = buf.clone();
    for _ in 0..passes {
        for y in 0..h {
            for x in 0..w {
                let i = y * w + x;
                if !hole[i] {
                    continue;
                }
                let (mut acc, mut n) = ([0f32; 4], 0f32);
                for (dx, dy) in [(1i32, 0i32), (-1, 0), (0, 1), (0, -1)] {
                    let (sx, sy) = (x as i32 + dx, y as i32 + dy);
                    if sx < 0 || sy < 0 || sx as usize >= w || sy as usize >= h {
                        continue;
                    }
                    let c = buf[sy as usize * w + sx as usize];
                    acc[0] += c.r;
                    acc[1] += c.g;
                    acc[2] += c.b;
                    acc[3] += c.a;
                    n += 1.0;
                }
                if n > 0.0 {
                    next[i] = Rgba::new(acc[0] / n, acc[1] / n, acc[2] / n, acc[3] / n);
                }
            }
        }
        std::mem::swap(&mut buf, &mut next);
    }
    buf
}
