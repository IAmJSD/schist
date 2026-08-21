//! Stored paths: drawing them, editing them, and using them.

use schist_color::Depth;
use schist_core::{Anchor, Document, Layer, SubPath, VectorPath};
use schist_plugin_api::{
    EditorState, Modifiers, OptionValue, PointerInput, ToolCtx, ToolPlugin,
};
use schist_tools_vector::paths::*;

fn input(x: f32, y: f32) -> PointerInput {
    PointerInput {
        x,
        y,
        pressure: 1.0,
        modifiers: Modifiers::default(),
    }
}

fn doc() -> Document {
    let mut d = Document::new("t", 200, 200, Depth::Eight);
    d.push_layer(Layer::new_raster("bg"));
    d
}

fn square_path() -> VectorPath {
    let mut p = VectorPath::new("Square");
    p.subpaths.push(SubPath {
        anchors: vec![
            Anchor::corner(50.0, 50.0),
            Anchor::corner(150.0, 50.0),
            Anchor::corner(150.0, 150.0),
            Anchor::corner(50.0, 150.0),
        ],
        closed: true,
    });
    p
}

#[test]
fn freeform_pen_thins_a_drag_into_a_few_anchors() {
    let mut doc = doc();
    let mut state = EditorState::default();
    let mut tool = FreeformPenTool::new(false);
    let mut ctx = ToolCtx {
        doc: &mut doc,
        state: &mut state,
    };
    // A straight drag sampled at every pixel: the fitted path should keep
    // the two ends and throw away the hundred points in between.
    tool.on_pointer_down(&mut ctx, input(10.0, 10.0));
    for x in 11..=110 {
        tool.on_pointer_move(&mut ctx, input(x as f32, 10.0));
    }
    tool.on_pointer_up(&mut ctx, input(110.0, 10.0));

    assert_eq!(doc.paths.len(), 1, "no path was stored");
    let anchors = doc.paths[0].anchors().count();
    assert!(
        (2..=6).contains(&anchors),
        "expected a handful of anchors, got {anchors}"
    );
    assert_eq!(doc.active_path, Some(0), "new path did not become active");
}

#[test]
fn freeform_fit_controls_how_much_is_kept() {
    let wobble = |fit: f32| {
        let mut doc = doc();
        let mut state = EditorState::default();
        let mut tool = FreeformPenTool::new(false);
        tool.set_option("freeform-fit", OptionValue::Num(fit));
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        tool.on_pointer_down(&mut ctx, input(10.0, 10.0));
        for x in 11..=110 {
            // A sawtooth, so there is real detail to discard.
            let y = 10.0 + if x % 6 < 3 { 4.0 } else { 0.0 };
            tool.on_pointer_move(&mut ctx, input(x as f32, y));
        }
        tool.on_pointer_up(&mut ctx, input(110.0, 10.0));
        doc.paths[0].anchors().count()
    };
    assert!(
        wobble(0.5) > wobble(10.0),
        "a tighter fit should keep more anchors"
    );
}

#[test]
fn curvature_pen_closes_on_the_first_point() {
    let mut doc = doc();
    let mut state = EditorState::default();
    let mut tool = FreeformPenTool::new(true);
    let mut ctx = ToolCtx {
        doc: &mut doc,
        state: &mut state,
    };
    for p in [(20.0, 20.0), (120.0, 20.0), (120.0, 120.0)] {
        tool.on_pointer_down(&mut ctx, input(p.0, p.1));
        tool.on_pointer_up(&mut ctx, input(p.0, p.1));
    }
    assert!(ctx.doc.paths.is_empty(), "closed too early");
    tool.on_pointer_down(&mut ctx, input(21.0, 21.0));
    assert_eq!(doc.paths.len(), 1, "clicking the first point did not close");
    // The curvature pen fits curves, so every anchor gets handles.
    assert!(
        doc.paths[0]
            .anchors()
            .all(|(_, _, a)| a.handle_out != (0.0, 0.0)),
        "curvature pen left corner points"
    );
}

#[test]
fn path_selection_drags_the_whole_path() {
    let mut doc = doc();
    doc.paths.push(square_path());
    doc.active_path = Some(0);
    let mut state = EditorState::default();
    let mut tool = PathSelectTool::new(ArrowKind::Path);
    let mut ctx = ToolCtx {
        doc: &mut doc,
        state: &mut state,
    };
    tool.on_pointer_down(&mut ctx, input(50.0, 50.0));
    tool.on_pointer_move(&mut ctx, input(70.0, 60.0));
    tool.on_pointer_up(&mut ctx, input(70.0, 60.0));

    let pts: Vec<(f32, f32)> = doc.paths[0].anchors().map(|(_, _, a)| a.point).collect();
    assert_eq!(pts[0], (70.0, 60.0), "first anchor did not move with it");
    assert_eq!(pts[2], (170.0, 160.0), "the path did not move as one");
}

#[test]
fn direct_selection_moves_one_anchor_only() {
    let mut doc = doc();
    doc.paths.push(square_path());
    doc.active_path = Some(0);
    let mut state = EditorState::default();
    let mut tool = PathSelectTool::new(ArrowKind::Direct);
    let mut ctx = ToolCtx {
        doc: &mut doc,
        state: &mut state,
    };
    tool.on_pointer_down(&mut ctx, input(150.0, 50.0));
    tool.on_pointer_move(&mut ctx, input(160.0, 30.0));
    tool.on_pointer_up(&mut ctx, input(160.0, 30.0));

    let pts: Vec<(f32, f32)> = doc.paths[0].anchors().map(|(_, _, a)| a.point).collect();
    assert_eq!(pts[1], (160.0, 30.0), "the grabbed anchor did not move");
    assert_eq!(pts[0], (50.0, 50.0), "a different anchor moved too");
    assert_eq!(pts[2], (150.0, 150.0), "a different anchor moved too");
}

#[test]
fn direct_selection_ignores_a_click_on_nothing() {
    let mut doc = doc();
    doc.paths.push(square_path());
    doc.active_path = Some(0);
    let before = doc.paths[0].clone();
    let mut state = EditorState::default();
    let mut tool = PathSelectTool::new(ArrowKind::Direct);
    let mut ctx = ToolCtx {
        doc: &mut doc,
        state: &mut state,
    };
    tool.on_pointer_down(&mut ctx, input(100.0, 100.0));
    tool.on_pointer_move(&mut ctx, input(120.0, 120.0));
    tool.on_pointer_up(&mut ctx, input(120.0, 120.0));
    assert_eq!(doc.paths[0], before, "dragging empty space moved the path");
}

#[test]
fn dragging_a_handle_swings_its_partner() {
    let mut doc = doc();
    let mut path = square_path();
    path.smooth_all();
    let grabbed = path.subpaths[0].anchors[1];
    doc.paths.push(path);
    doc.active_path = Some(0);
    let (hx, hy) = (
        grabbed.point.0 + grabbed.handle_out.0,
        grabbed.point.1 + grabbed.handle_out.1,
    );
    let mut state = EditorState::default();
    let mut tool = PathSelectTool::new(ArrowKind::Direct);
    let mut ctx = ToolCtx {
        doc: &mut doc,
        state: &mut state,
    };
    tool.on_pointer_down(&mut ctx, input(hx, hy));
    tool.on_pointer_move(&mut ctx, input(hx + 10.0, hy + 10.0));
    tool.on_pointer_up(&mut ctx, input(hx + 10.0, hy + 10.0));

    let a = doc.paths[0].subpaths[0].anchors[1];
    assert_ne!(a.handle_out, grabbed.handle_out, "handle did not move");
    assert_eq!(
        a.handle_in,
        (-a.handle_out.0, -a.handle_out.1),
        "the opposite handle did not stay mirrored"
    );
    assert_eq!(a.point, grabbed.point, "the anchor itself moved");
}

#[test]
fn smoothing_gives_every_anchor_mirrored_handles() {
    let mut path = square_path();
    path.smooth_all();
    for (_, _, a) in path.anchors() {
        assert_eq!(
            a.handle_in,
            (-a.handle_out.0, -a.handle_out.1),
            "handles are not mirrored"
        );
        assert_ne!(a.handle_out, (0.0, 0.0), "anchor was left a corner");
    }
}

#[test]
fn flattening_a_closed_square_covers_its_area() {
    let path = square_path();
    let flat = flatten(&path);
    let rect = schist_core::IntRect::new(0, 0, 200, 200);
    let mask = schist_vector::rasterize(&flat, rect, schist_vector::FillRule::NonZero);
    let at = |x: usize, y: usize| mask[y * 200 + x];
    assert_eq!(at(100, 100), 255, "middle of the square is not filled");
    assert_eq!(at(20, 20), 0, "outside the square is filled");
}

#[test]
fn custom_shape_draws_the_chosen_preset() {
    let mut doc = doc();
    let mut state = EditorState {
        foreground: schist_color::Rgba::new(1.0, 0.0, 0.0, 1.0),
        ..Default::default()
    };
    let mut tool = CustomShapeTool::new();
    // "Cross" fills its centre but leaves its corners empty, which tells
    // the presets apart from a plain rectangle.
    tool.set_option("custom-shape", OptionValue::Choice(4));
    let mut ctx = ToolCtx {
        doc: &mut doc,
        state: &mut state,
    };
    tool.on_pointer_down(&mut ctx, input(40.0, 40.0));
    tool.on_pointer_up(&mut ctx, input(140.0, 140.0));

    let top = doc.tree.iter().last().unwrap().as_raster().unwrap();
    assert!(
        top.tiles.pixel(90, 90).a > 0.5,
        "centre of the cross is empty"
    );
    assert!(
        top.tiles.pixel(45, 45).a < 0.5,
        "the corner should be outside a cross"
    );
}
