//! Artboards, slices, frames, notes and counts.

use schist_color::Depth;
use schist_core::{Document, IntRect, Layer};
use schist_plugin_api::{EditorState, Modifiers, PointerInput, ToolCtx, ToolPlugin};
use schist_tools_doc::*;

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

fn doc() -> Document {
    let mut d = Document::new("t", 400, 300, Depth::Eight);
    d.push_layer(Layer::new_raster("bg"));
    d.active_layer = Some(d.tree.layers[0].id);
    d
}

fn drag(tool: &mut dyn ToolPlugin, ctx: &mut ToolCtx, from: (f32, f32), to: (f32, f32)) {
    tool.on_pointer_down(ctx, input(from.0, from.1));
    tool.on_pointer_move(ctx, input(to.0, to.1));
    tool.on_pointer_up(ctx, input(to.0, to.1));
}

#[test]
fn dragging_creates_an_artboard() {
    let mut doc = doc();
    let mut state = EditorState::default();
    let mut tool = RectTool::new(RectKind::Artboard);
    {
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        drag(&mut tool, &mut ctx, (20.0, 30.0), (180.0, 150.0));
    }
    assert_eq!(doc.artboards.len(), 1);
    assert_eq!(doc.artboards[0].rect, IntRect::new(20, 30, 180, 150));
}

#[test]
fn clicking_an_existing_artboard_moves_it_instead_of_making_another() {
    let mut doc = doc();
    let mut state = EditorState::default();
    let mut tool = RectTool::new(RectKind::Artboard);
    {
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        drag(&mut tool, &mut ctx, (20.0, 20.0), (120.0, 120.0));
        drag(&mut tool, &mut ctx, (60.0, 60.0), (90.0, 70.0));
    }

    assert_eq!(doc.artboards.len(), 1, "a second artboard was created");
    assert_eq!(
        doc.artboards[0].rect,
        IntRect::new(50, 30, 150, 130),
        "the artboard did not move by the drag"
    );
}

#[test]
fn a_tiny_drag_creates_nothing() {
    let mut doc = doc();
    let mut state = EditorState::default();
    let mut tool = RectTool::new(RectKind::Slice);
    {
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        drag(&mut tool, &mut ctx, (50.0, 50.0), (50.5, 50.5));
    }
    assert!(doc.slices.is_empty());
}

#[test]
fn a_frame_layer_is_masked_to_its_shape() {
    let mut doc = doc();
    let mut state = EditorState::default();
    let mut tool = RectTool::new(RectKind::Frame);
    {
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        drag(&mut tool, &mut ctx, (40.0, 40.0), (160.0, 140.0));
    }
    let frame = doc.tree.iter().find(|l| l.is_frame).expect("frame layer");
    let mask = frame.mask.as_ref().expect("frame has a mask");
    assert_eq!(
        mask.value(100, 90),
        255,
        "inside the frame is not masked in"
    );
    assert_eq!(mask.value(10, 10), 0, "outside the frame is not masked out");
}

#[test]
fn an_elliptical_frame_masks_out_its_corners() {
    let mut doc = doc();
    let mut state = EditorState::default();
    let mut tool = RectTool::new(RectKind::Frame);
    tool.set_option("frame-shape", schist_plugin_api::OptionValue::Choice(1));
    {
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        drag(&mut tool, &mut ctx, (40.0, 40.0), (160.0, 140.0));
    }
    let frame = doc.tree.iter().find(|l| l.is_frame).unwrap();
    let mask = frame.mask.as_ref().unwrap();
    assert_eq!(mask.value(100, 90), 255, "centre of the ellipse");
    assert_eq!(mask.value(42, 42), 0, "corner should be outside an ellipse");
}

#[test]
fn notes_are_placed_moved_and_removed() {
    let mut doc = doc();
    let mut state = EditorState::default();
    let mut tool = PointTool::new(PointKind::Note);
    {
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        tool.on_pointer_down(&mut ctx, input(100.0, 100.0));
        tool.on_pointer_up(&mut ctx, input(100.0, 100.0));
    }
    assert_eq!(doc.notes.len(), 1);

    // Clicking the note grabs it rather than adding a second.
    {
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        tool.on_pointer_down(&mut ctx, input(102.0, 101.0));
        tool.on_pointer_move(&mut ctx, input(150.0, 120.0));
        tool.on_pointer_up(&mut ctx, input(150.0, 120.0));
    }
    assert_eq!(doc.notes.len(), 1, "a second note was added");
    assert_eq!(doc.notes[0].at, (150.0, 120.0), "the note did not move");

    {
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        tool.on_pointer_down(&mut ctx, alt(150.0, 120.0));
    }
    assert!(doc.notes.is_empty(), "alt-click did not remove the note");
}

#[test]
fn counting_adds_marks_and_alt_click_takes_them_back() {
    let mut doc = doc();
    let mut state = EditorState::default();
    let mut tool = PointTool::new(PointKind::Count);
    {
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        for (x, y) in [(20.0, 20.0), (60.0, 40.0), (100.0, 80.0)] {
            tool.on_pointer_down(&mut ctx, input(x, y));
            tool.on_pointer_up(&mut ctx, input(x, y));
        }
    }
    assert_eq!(doc.counts.len(), 1, "expected one count group");
    assert_eq!(doc.counts[0].points.len(), 3);

    {
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        tool.on_pointer_down(&mut ctx, alt(60.0, 40.0));
    }
    assert_eq!(doc.counts[0].points.len(), 2, "alt-click did not remove");
    // ...and it removed the right one.
    assert!(!doc.counts[0]
        .points
        .iter()
        .any(|p| (p.0 - 60.0).abs() < 1.0));
}

#[test]
fn slices_and_artboards_are_separate_lists() {
    let mut doc = doc();
    let mut state = EditorState::default();
    {
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };
        let mut a = RectTool::new(RectKind::Artboard);
        drag(&mut a, &mut ctx, (0.0, 0.0), (100.0, 100.0));
        let mut sl = RectTool::new(RectKind::Slice);
        drag(&mut sl, &mut ctx, (10.0, 10.0), (50.0, 50.0));
    }
    assert_eq!(doc.artboards.len(), 1);
    assert_eq!(doc.slices.len(), 1);
    assert!(doc.slices[0].user, "a drawn slice should be a user slice");
}
