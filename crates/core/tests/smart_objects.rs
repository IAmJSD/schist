//! The property that makes a smart object worth having: transforming it
//! repeatedly costs no more quality than transforming it once.

use photoslop_color::{Depth, Rgba};
use photoslop_core::{Affine, IntRect, SmartObject, TileCoord, TileMap, TILE_SIZE};

/// 128x128 of fine detail — a one-pixel checkerboard, which is the first
/// thing to disappear when an image is scaled down and back up.
fn detailed_source() -> TileMap {
    let mut tiles = TileMap::new();
    for y in 0..128i32 {
        for x in 0..128i32 {
            let on = (x + y) % 2 == 0;
            let c = if on {
                Rgba::new(1.0, 1.0, 1.0, 1.0)
            } else {
                Rgba::new(0.0, 0.0, 0.0, 1.0)
            };
            let coord = TileCoord::containing(x, y);
            let trect = coord.rect();
            let buf = tiles.get_mut_or_insert(coord, Depth::Eight);
            buf.set(((y - trect.top) * TILE_SIZE + (x - trect.left)) as usize, c);
        }
    }
    tiles
}

/// Mean absolute difference between two tile maps over a rect.
fn difference(a: &TileMap, b: &TileMap, rect: IntRect) -> f32 {
    let mut sum = 0.0;
    let mut n = 0.0;
    for y in rect.top..rect.bottom {
        for x in rect.left..rect.right {
            let (p, q) = (a.pixel(x, y), b.pixel(x, y));
            sum += (p.r - q.r).abs() + (p.g - q.g).abs() + (p.b - q.b).abs();
            n += 3.0;
        }
    }
    if n == 0.0 {
        0.0
    } else {
        sum / n
    }
}

/// How much detail survived: the average difference between neighbouring
/// pixels. A blurred checkerboard has almost none.
fn detail(tiles: &TileMap, rect: IntRect) -> f32 {
    let mut sum = 0.0;
    let mut n = 0.0;
    for y in rect.top..rect.bottom {
        for x in rect.left + 1..rect.right {
            sum += (tiles.pixel(x, y).r - tiles.pixel(x - 1, y).r).abs();
            n += 1.0;
        }
    }
    if n == 0.0 {
        0.0
    } else {
        sum / n
    }
}

const CLIP: IntRect = IntRect {
    left: -256,
    top: -256,
    right: 512,
    bottom: 512,
};

#[test]
fn a_round_trip_through_a_tenth_scale_keeps_the_detail() {
    let source = detailed_source();
    let mut so = SmartObject::wrap(source.clone(), "checker");

    // Down to a tenth, then back up: ten times.
    for _ in 0..10 {
        so.apply(&Affine::scale(0.1, 0.1));
        so.apply(&Affine::scale(10.0, 10.0));
    }
    let rendered = so.render(Depth::Eight, CLIP);

    let inner = IntRect::new(8, 8, 120, 120);
    let before = detail(&source, inner);
    let after = detail(&rendered, inner);
    assert!(
        after > before * 0.9,
        "detail collapsed: {before} -> {after}"
    );
    assert!(
        difference(&source, &rendered, inner) < 0.02,
        "pixels drifted from the source"
    );
}

#[test]
fn the_same_abuse_destroys_an_ordinary_layer() {
    // The comparison that makes the point: doing it destructively.
    let source = detailed_source();
    let mut tiles = source.clone();
    for _ in 0..10 {
        tiles = photoslop_core::resample::transform_tiles(
            &tiles,
            &Affine::scale(0.1, 0.1),
            Depth::Eight,
            photoslop_core::Filter::Bicubic,
            CLIP,
        );
        tiles = photoslop_core::resample::transform_tiles(
            &tiles,
            &Affine::scale(10.0, 10.0),
            Depth::Eight,
            photoslop_core::Filter::Bicubic,
            CLIP,
        );
    }
    let inner = IntRect::new(8, 8, 120, 120);
    assert!(
        detail(&tiles, inner) < detail(&source, inner) * 0.5,
        "a destructive round trip was expected to lose detail"
    );
}

#[test]
fn transforms_compose_rather_than_stacking_resamples() {
    let mut so = SmartObject::wrap(detailed_source(), "checker");
    so.apply(&Affine::scale(2.0, 2.0));
    so.apply(&Affine::scale(0.5, 0.5));
    // Two transforms that cancel should leave the identity behind.
    let m = so.transform;
    assert!(
        (m.a - 1.0).abs() < 1e-5 && (m.d - 1.0).abs() < 1e-5,
        "{m:?}"
    );
    let rendered = so.render(Depth::Eight, CLIP);
    assert!(
        difference(&detailed_source(), &rendered, IntRect::new(8, 8, 120, 120)) < 0.01,
        "cancelling transforms did not return the original"
    );
}

#[test]
fn placed_bounds_follow_the_transform() {
    let mut so = SmartObject::wrap(detailed_source(), "checker");
    // Bounds round outward by a pixel so they always cover the artwork.
    let before = so.placed_bounds();
    assert!(
        (before.width() - 128).abs() <= 1,
        "unscaled bounds {before:?}"
    );

    so.apply(&Affine::scale(2.0, 2.0));
    let after = so.placed_bounds();
    assert!(
        (after.width() - 256).abs() <= 2,
        "bounds did not scale: {after:?}"
    );

    so.apply(&Affine::translate(50.0, -20.0));
    let moved = so.placed_bounds();
    assert!(
        (moved.left - (after.left + 50)).abs() <= 1,
        "bounds did not translate: {moved:?}"
    );
}

#[test]
fn an_empty_source_renders_nothing_rather_than_panicking() {
    let so = SmartObject::wrap(TileMap::new(), "empty");
    assert!(so.placed_bounds().is_empty());
    assert!(so.render(Depth::Eight, CLIP).content_bounds().is_empty());
}
