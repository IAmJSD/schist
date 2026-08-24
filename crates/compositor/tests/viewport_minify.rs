//! Minifying must average coverage, not accumulate it.
//!
//! A parity test against the GPU backend cannot catch this: both
//! implementations shared the same missing normalization, so they agreed
//! with each other while disagreeing with the source pixels. These assert
//! absolute output instead.

use schist_compositor::viewport::{render_viewport_cpu, ViewportParams};
use schist_core::{IntRect, TILE_PIXELS, TILE_SIZE};
use std::sync::Arc;

/// One tile of uniform straight-alpha red at the given coverage.
fn red_tile(alpha: u8) -> Arc<Vec<u8>> {
    let mut px = Vec::with_capacity(TILE_PIXELS * 4);
    for _ in 0..TILE_PIXELS {
        px.extend_from_slice(&[220, 40, 40, alpha]);
    }
    Arc::new(px)
}

fn params(zoom: f32) -> ViewportParams {
    ViewportParams {
        width: 32,
        height: 32,
        origin: (0.0, 0.0),
        zoom,
        scale_factor: 1.0,
        rotation: 0.0,
        canvas: IntRect::new(0, 0, TILE_SIZE, TILE_SIZE),
        grid_origin: (0, 0),
        grid_cols: 1,
        grid_rows: 1,
        surround: 0x20,
    }
}

/// Sample the blue and green bytes of the centre pixel. The viewport
/// composites onto the checkerboard, so a half-covered red pixel lands
/// between the checker grey and the source red.
fn centre_bg(out: &[u8], p: &ViewportParams) -> (u8, u8) {
    let idx = ((p.height / 2) * p.width + p.width / 2) * 4;
    (out[idx], out[idx + 1])
}

#[test]
fn minified_alpha_is_averaged_not_summed() {
    // 25% zoom => 4 taps per axis, i.e. 16 samples. Summing coverage
    // would multiply alpha by 16 and saturate every one of these.
    let p = params(0.25);
    assert!(p.box_taps() > 1, "expected the minifying branch");

    let faint = render_viewport_cpu(&p, &[Some(red_tile(64))]);
    let half = render_viewport_cpu(&p, &[Some(red_tile(128))]);
    let solid = render_viewport_cpu(&p, &[Some(red_tile(255))]);

    let (fb, fg) = centre_bg(&faint, &p);
    let (hb, hg) = centre_bg(&half, &p);
    let (sb, sg) = centre_bg(&solid, &p);

    // Distinct coverages must stay distinct after minification.
    assert_ne!(
        (fb, fg),
        (hb, hg),
        "25% and 50% coverage rendered identically"
    );
    assert_ne!(
        (hb, hg),
        (sb, sg),
        "50% and 100% coverage rendered identically"
    );

    // Lower coverage lets more of the light checkerboard through, so the
    // blue channel (red's complement here) must fall as coverage rises.
    assert!(
        fb > hb && hb > sb,
        "coverage must darken the complement monotonically: {fb} {hb} {sb}"
    );

    // A fully opaque tile is exactly the source colour.
    assert_eq!((sb, sg), (40, 40), "opaque red should survive minification");
}

#[test]
fn minified_half_coverage_matches_unminified() {
    // The same uniform tile at 1:1 and minified must agree: averaging a
    // constant field is that constant.
    let one_to_one = render_viewport_cpu(&params(1.0), &[Some(red_tile(128))]);
    let minified = render_viewport_cpu(&params(0.25), &[Some(red_tile(128))]);

    let a = centre_bg(&one_to_one, &params(1.0));
    let b = centre_bg(&minified, &params(0.25));
    assert_eq!(
        a, b,
        "uniform 50% coverage should look the same minified as at 1:1"
    );
}
