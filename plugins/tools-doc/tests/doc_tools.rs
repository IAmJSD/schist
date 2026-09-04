//! Artboards, slices, frames, notes and counts.

use schist_color::{Depth, Rgba};
use schist_core::{Document, IntRect, Layer};
use schist_plugin_api::{EditorState, Modifiers, Overlay, PointerInput, ToolCtx, ToolPlugin};
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

/// Run a gesture against a tool, borrowing the document and editor state
/// for exactly as long as it takes -- the assertions afterwards need them
/// back.
fn with(
    tool: &mut PointTool,
    doc: &mut Document,
    state: &mut EditorState,
    body: impl FnOnce(&mut PointTool, &mut ToolCtx),
) {
    let mut ctx = ToolCtx { doc, state };
    body(tool, &mut ctx);
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

/// Every tool here writes straight to a list on the `Document` rather
/// than going through `begin_edit`, so nothing set `dirty`. The fix that
/// prompted this covered the Clear menu items but not the tools that
/// *create* the state, which is the common way to get it: place notes or
/// drag out slices on a saved document, close the tab, and there was no
/// prompt and autosave had skipped it.
#[test]
fn creating_document_furniture_counts_as_an_unsaved_change() {
    type Place = Box<dyn Fn(&mut Document)>;
    let cases: Vec<(&str, Place)> = vec![
        (
            "artboard",
            Box::new(|d: &mut Document| {
                let mut t = RectTool::new(RectKind::Artboard);
                let mut s = EditorState::default();
                let mut c = ToolCtx {
                    doc: d,
                    state: &mut s,
                };
                t.on_pointer_down(&mut c, input(20.0, 20.0));
                t.on_pointer_up(&mut c, input(120.0, 90.0));
            }),
        ),
        (
            "slice",
            Box::new(|d: &mut Document| {
                let mut t = RectTool::new(RectKind::Slice);
                let mut s = EditorState::default();
                let mut c = ToolCtx {
                    doc: d,
                    state: &mut s,
                };
                t.on_pointer_down(&mut c, input(20.0, 20.0));
                t.on_pointer_up(&mut c, input(120.0, 90.0));
            }),
        ),
        (
            "note",
            Box::new(|d: &mut Document| {
                let mut t = PointTool::new(PointKind::Note);
                let mut s = EditorState::default();
                let mut c = ToolCtx {
                    doc: d,
                    state: &mut s,
                };
                t.on_pointer_down(&mut c, input(40.0, 40.0));
            }),
        ),
        (
            "count",
            Box::new(|d: &mut Document| {
                let mut t = PointTool::new(PointKind::Count);
                let mut s = EditorState::default();
                let mut c = ToolCtx {
                    doc: d,
                    state: &mut s,
                };
                t.on_pointer_down(&mut c, input(40.0, 40.0));
            }),
        ),
    ];
    for (what, place) in cases {
        let mut d = doc();
        d.dirty = false;
        place(&mut d);
        assert!(d.dirty, "placing a {what} left the document looking saved");
    }
}

/// Removing and moving them counts too.
#[test]
fn removing_and_moving_furniture_counts_as_well() {
    // A note dragged to a new position.
    let mut d = doc();
    let mut s = EditorState::default();
    let mut t = PointTool::new(PointKind::Note);
    {
        let mut c = ToolCtx {
            doc: &mut d,
            state: &mut s,
        };
        t.on_pointer_down(&mut c, input(40.0, 40.0));
        t.on_pointer_up(&mut c, input(40.0, 40.0));
    }
    d.dirty = false;
    {
        let mut c = ToolCtx {
            doc: &mut d,
            state: &mut s,
        };
        t.on_pointer_down(&mut c, input(40.0, 40.0));
        t.on_pointer_move(&mut c, input(90.0, 70.0));
        t.on_pointer_up(&mut c, input(90.0, 70.0));
    }
    assert!(d.dirty, "moving a note left the document looking saved");

    // And alt-clicking it away.
    d.dirty = false;
    {
        let mut c = ToolCtx {
            doc: &mut d,
            state: &mut s,
        };
        t.on_pointer_down(&mut c, alt(90.0, 70.0));
    }
    assert!(d.notes.is_empty(), "the note was removed");
    assert!(d.dirty, "removing a note left the document looking saved");

    // An artboard dragged across the canvas.
    let mut d = doc();
    let mut r = RectTool::new(RectKind::Artboard);
    {
        let mut c = ToolCtx {
            doc: &mut d,
            state: &mut s,
        };
        r.on_pointer_down(&mut c, input(20.0, 20.0));
        r.on_pointer_up(&mut c, input(120.0, 90.0));
    }
    d.dirty = false;
    {
        let mut c = ToolCtx {
            doc: &mut d,
            state: &mut s,
        };
        r.on_pointer_down(&mut c, input(70.0, 55.0));
        r.on_pointer_move(&mut c, input(140.0, 95.0));
        r.on_pointer_up(&mut c, input(140.0, 95.0));
    }
    assert!(
        d.dirty,
        "moving an artboard left the document looking saved"
    );
}

/// Notes used to sit outside the history entirely: placing, moving and
/// deleting one all mutated the document directly, so ⌘Z reached past
/// them to whatever was edited before and the note stayed where it was.
#[test]
fn placing_moving_and_deleting_a_note_can_be_undone() {
    let mut doc = doc();
    let mut state = EditorState::default();
    let mut tool = PointTool::new(PointKind::Note);

    with(&mut tool, &mut doc, &mut state, |t, c| {
        t.on_pointer_down(c, input(100.0, 100.0));
        t.on_pointer_up(c, input(100.0, 100.0));
    });
    assert_eq!(doc.undo().as_deref(), Some("Place Note"));
    assert!(doc.notes.is_empty(), "undo left the note behind");
    assert_eq!(doc.redo().as_deref(), Some("Place Note"));
    assert_eq!(doc.notes.len(), 1, "redo did not put the note back");

    with(&mut tool, &mut doc, &mut state, |t, c| {
        t.on_pointer_down(c, input(100.0, 100.0));
        t.on_pointer_move(c, input(150.0, 120.0));
        t.on_pointer_up(c, input(150.0, 120.0));
    });
    assert_eq!(doc.notes[0].at, (150.0, 120.0));
    assert_eq!(doc.undo().as_deref(), Some("Move Note"));
    assert_eq!(doc.notes[0].at, (100.0, 100.0), "the move was not undone");

    with(&mut tool, &mut doc, &mut state, |t, c| {
        t.on_pointer_down(c, alt(100.0, 100.0));
    });
    assert!(doc.notes.is_empty());
    assert_eq!(doc.undo().as_deref(), Some("Delete Note"));
    assert_eq!(doc.notes.len(), 1, "the deleted note did not come back");
}

/// A drag is one entry for the whole gesture, and a click that only
/// selects is none at all -- otherwise picking a note up to read it would
/// bury the last real edit under a pile of no-ops.
#[test]
fn a_drag_is_one_history_entry_and_a_bare_click_is_none() {
    let mut doc = doc();
    let mut state = EditorState::default();
    let mut tool = PointTool::new(PointKind::Note);

    with(&mut tool, &mut doc, &mut state, |t, c| {
        t.on_pointer_down(c, input(100.0, 100.0));
        t.on_pointer_up(c, input(100.0, 100.0));
    });
    with(&mut tool, &mut doc, &mut state, |t, c| {
        t.on_pointer_down(c, input(100.0, 100.0));
        for x in 1..=10 {
            t.on_pointer_move(c, input(100.0 + x as f32, 100.0));
        }
        t.on_pointer_up(c, input(110.0, 100.0));
    });
    assert_eq!(doc.undo().as_deref(), Some("Move Note"));
    assert_eq!(doc.notes[0].at, (100.0, 100.0), "ten moves, one undo");

    // Press and release without moving: a selection, not an edit.
    with(&mut tool, &mut doc, &mut state, |t, c| {
        t.on_pointer_down(c, input(100.0, 100.0));
        t.on_pointer_up(c, input(100.0, 100.0));
    });
    assert_eq!(
        doc.undo().as_deref(),
        Some("Place Note"),
        "clicking a note to select it recorded an edit of its own"
    );
}

/// New notes take the author and colour the options bar is showing, and
/// arrive empty: the text is typed in the Notes panel afterwards. They
/// used to arrive holding a "Note 3" placeholder nothing could edit.
#[test]
fn a_new_note_takes_the_authors_name_and_colour_and_no_text() {
    let mut doc = doc();
    let mut state = EditorState {
        note_author: "Astrid".into(),
        note_color: Rgba::new(0.2, 0.4, 1.0, 1.0),
        ..EditorState::default()
    };
    let mut tool = PointTool::new(PointKind::Note);
    with(&mut tool, &mut doc, &mut state, |t, c| {
        t.on_pointer_down(c, input(10.0, 10.0));
        t.on_pointer_up(c, input(10.0, 10.0));
    });
    let note = &doc.notes[0];
    assert_eq!(note.author, "Astrid");
    assert_eq!(note.color, Rgba::new(0.2, 0.4, 1.0, 1.0));
    assert_eq!(note.text, "", "a new note arrived with placeholder text");
}

/// Clicking a note is what tells the Notes panel which one to show, and
/// deleting one has to leave that pointing somewhere sane.
#[test]
fn clicking_a_note_selects_it_for_the_panel() {
    let mut doc = doc();
    let mut state = EditorState::default();
    let mut tool = PointTool::new(PointKind::Note);
    for (x, y) in [(20.0, 20.0), (200.0, 40.0), (60.0, 150.0)] {
        with(&mut tool, &mut doc, &mut state, |t, c| {
            t.on_pointer_down(c, input(x, y));
            t.on_pointer_up(c, input(x, y));
        });
    }
    assert_eq!(
        state.active_note,
        Some(2),
        "the note just placed is selected"
    );

    with(&mut tool, &mut doc, &mut state, |t, c| {
        t.on_pointer_down(c, input(20.0, 20.0));
        t.on_pointer_up(c, input(20.0, 20.0));
    });
    assert_eq!(state.active_note, Some(0));

    // Deleting note 0 shifts every later note down one; a selection that
    // did not follow would leave the panel showing a different note than
    // the canvas had outlined.
    with(&mut tool, &mut doc, &mut state, |t, c| {
        t.on_pointer_down(c, input(200.0, 40.0));
        t.on_pointer_up(c, input(200.0, 40.0));
    });
    assert_eq!(state.active_note, Some(1));
    with(&mut tool, &mut doc, &mut state, |t, c| {
        t.on_pointer_down(c, alt(20.0, 20.0));
    });
    assert_eq!(
        state.active_note,
        Some(0),
        "the selection did not follow the note it was on"
    );
}

/// Overlapping notes are hit topmost-first, so the one you can see is the
/// one you grab.
#[test]
fn the_note_on_top_is_the_one_clicked() {
    let mut doc = doc();
    let mut state = EditorState::default();
    let mut tool = PointTool::new(PointKind::Note);
    for _ in 0..2 {
        with(&mut tool, &mut doc, &mut state, |t, c| {
            t.on_pointer_down(c, input(50.0, 50.0));
            t.on_pointer_up(c, input(50.0, 50.0));
        });
    }
    // The second click landed on the first note, so only one exists; place
    // a second one just far enough away to clear the hit radius, then drag
    // it back on top of the first.
    with(&mut tool, &mut doc, &mut state, |t, c| {
        t.on_pointer_down(c, input(90.0, 50.0));
        t.on_pointer_up(c, input(90.0, 50.0));
        t.on_pointer_down(c, input(90.0, 50.0));
        t.on_pointer_move(c, input(52.0, 51.0));
        t.on_pointer_up(c, input(52.0, 51.0));
    });
    assert_eq!(doc.notes.len(), 2);

    with(&mut tool, &mut doc, &mut state, |t, c| {
        t.on_pointer_down(c, input(51.0, 50.0));
        t.on_pointer_up(c, input(51.0, 50.0));
    });
    assert_eq!(state.active_note, Some(1), "the buried note was grabbed");
}

/// The canvas draws notes whichever tool is in use, from this same list,
/// which is why it is public and why the selected one is flagged in it.
#[test]
fn every_note_gets_a_marker_and_the_selected_one_is_flagged() {
    let mut doc = doc();
    let mut state = EditorState::default();
    let mut tool = PointTool::new(PointKind::Note);
    for (x, y) in [(20.0, 20.0), (200.0, 40.0)] {
        with(&mut tool, &mut doc, &mut state, |t, c| {
            t.on_pointer_down(c, input(x, y));
            t.on_pointer_up(c, input(x, y));
        });
    }
    let overlays = note_overlays(&doc, Some(1));
    assert_eq!(overlays.len(), 2);
    let flagged: Vec<bool> = overlays
        .iter()
        .map(|o| match o {
            Overlay::NoteMarker { selected, .. } => *selected,
            other => panic!("expected a note marker, got {other:?}"),
        })
        .collect();
    assert_eq!(flagged, vec![false, true]);

    // Nothing selected is a legitimate state -- no tool has touched the
    // notes yet, or the selected one was just deleted.
    assert!(note_overlays(&doc, None).iter().all(|o| matches!(
        o,
        Overlay::NoteMarker {
            selected: false,
            ..
        }
    )));
}
