//! Select ▸ Transform Selection moves the mask and nothing else.

use schist_core::{Affine, IntRect, SelectOp, Selection};

const CANVAS: IntRect = IntRect {
    left: 0,
    top: 0,
    right: 200,
    bottom: 200,
};

fn square() -> Selection {
    let mut s = Selection::new();
    s.select_rect(IntRect::new(50, 50, 100, 100), SelectOp::Replace);
    s
}

#[test]
fn translating_moves_the_whole_mask() {
    let moved = square().transformed(&Affine::translate(30.0, 20.0), CANVAS);
    // The square was (50,50)-(100,100); after the move its middle is here.
    assert_eq!(moved.coverage(105, 95), 255, "middle did not move with it");
    assert_eq!(moved.coverage(60, 60), 0, "old position still selected");
    let b = moved.bounds();
    assert!((b.left - 80).abs() <= 1, "bounds did not translate: {b:?}");
}

#[test]
fn scaling_grows_the_mask() {
    let scaled = square().transformed(&Affine::scale(2.0, 2.0), CANVAS);
    let b = scaled.bounds();
    assert!(
        (b.width() - 100).abs() <= 2,
        "scaled bounds are wrong: {b:?}"
    );
    assert_eq!(scaled.coverage(150, 150), 255, "interior not filled");
}

#[test]
fn a_rotation_keeps_a_soft_edge_rather_than_a_staircase() {
    let mut feathered = square();
    feathered.feather(3.0);
    let rotated = feathered.transformed(&Affine::rotate(0.4).around(75.0, 75.0), CANVAS);
    // Somewhere along the rotated edge there must be partial coverage.
    let partial = (0..200).any(|x| {
        (0..200).any(|y| {
            let c = rotated.coverage(x, y);
            c > 20 && c < 235
        })
    });
    assert!(partial, "the rotated edge came out hard");
}

#[test]
fn transforming_an_empty_selection_gives_an_empty_one() {
    let out = Selection::new().transformed(&Affine::scale(2.0, 2.0), CANVAS);
    assert!(out.is_empty());
}

#[test]
fn a_degenerate_matrix_does_not_panic() {
    let out = square().transformed(&Affine::scale(0.0, 0.0), CANVAS);
    assert!(out.bounds().is_empty());
}

#[test]
fn moving_a_selection_off_canvas_clips_it() {
    let out = square().transformed(&Affine::translate(1000.0, 0.0), CANVAS);
    assert!(out.bounds().is_empty(), "should have gone off the canvas");
}
