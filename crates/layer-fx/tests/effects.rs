//! Each effect should put pixels where Photoshop puts them.

use schist_color::{Depth, Rgba};
use schist_core::{IntRect, Layer, LayerStyle, StrokePosition};

/// A 20x20 opaque red square at (40,40) on an otherwise empty layer.
fn square_layer() -> Layer {
    let mut layer = Layer::new_raster("test");
    let raster = layer.as_raster_mut().unwrap();
    for y in 40..60 {
        for x in 40..60 {
            let coord = schist_core::TileCoord::containing(x, y);
            let trect = coord.rect();
            let buf = raster.tiles.get_mut_or_insert(coord, Depth::Eight);
            let ix = ((y - trect.top) * schist_core::TILE_SIZE + (x - trect.left)) as usize;
            buf.set(ix, Rgba::new(1.0, 0.0, 0.0, 1.0));
        }
    }
    layer
}

fn alpha_at(styled: &schist_core::StyledRaster, x: i32, y: i32) -> f32 {
    styled.tiles.pixel(x, y).a
}

#[test]
fn no_style_renders_nothing() {
    let layer = square_layer();
    assert!(schist_layer_fx::render(&layer).is_none());
}

#[test]
fn drop_shadow_falls_outside_the_layer() {
    let mut layer = square_layer();
    layer.style.drop_shadow.enabled = true;
    layer.style.drop_shadow.settings.distance = 8.0;
    layer.style.drop_shadow.settings.size = 2.0;
    // 135 degrees: light from the upper left, so the shadow goes down-right.
    layer.style.drop_shadow.settings.angle = 135.0;
    let styled = schist_layer_fx::render(&layer).expect("styled");

    // The raster must have grown past the square to hold the shadow.
    assert!(
        styled.bounds.right > 60,
        "bounds {:?} did not grow for the shadow",
        styled.bounds
    );
    // Down and right of the square's corner: in shadow.
    assert!(
        alpha_at(&styled, 64, 64) > 0.1,
        "no shadow below-right of the layer"
    );
    // Up and left: nothing.
    assert_eq!(
        alpha_at(&styled, 34, 34),
        0.0,
        "shadow appeared on the lit side"
    );
    // The layer itself is still opaque and still red.
    let px = styled.tiles.pixel(50, 50);
    assert!(
        px.a > 0.99 && px.r > 0.9,
        "layer pixels were damaged: {px:?}"
    );
}

#[test]
fn outside_stroke_draws_a_band_around_the_edge() {
    let mut layer = square_layer();
    layer.style.stroke.enabled = true;
    layer.style.stroke.settings.size = 4.0;
    layer.style.stroke.settings.position = StrokePosition::Outside;
    layer.style.stroke.settings.color = Rgba::new(0.0, 0.0, 1.0, 1.0);
    let styled = schist_layer_fx::render(&layer).expect("styled");

    // Two pixels outside the left edge: inside a 4px outside stroke.
    let px = styled.tiles.pixel(38, 50);
    assert!(px.a > 0.5, "no stroke just outside the edge");
    assert!(px.b > 0.9 && px.r < 0.1, "stroke is not the stroke colour");
    // Eight pixels out: beyond it.
    assert!(
        styled.tiles.pixel(31, 50).a < 0.1,
        "stroke reached too far out"
    );
    // An outside stroke must not paint over the layer.
    assert!(
        styled.tiles.pixel(50, 50).r > 0.9,
        "outside stroke covered the layer"
    );
}

#[test]
fn inside_stroke_stays_within_the_layer() {
    let mut layer = square_layer();
    layer.style.stroke.enabled = true;
    layer.style.stroke.settings.size = 4.0;
    layer.style.stroke.settings.position = StrokePosition::Inside;
    layer.style.stroke.settings.color = Rgba::new(0.0, 0.0, 1.0, 1.0);
    let styled = schist_layer_fx::render(&layer).expect("styled");

    assert!(
        styled.tiles.pixel(38, 50).a < 0.1,
        "inside stroke leaked outside the layer"
    );
    assert!(
        styled.tiles.pixel(41, 50).b > 0.5,
        "no stroke just inside the edge"
    );
    assert!(
        styled.tiles.pixel(50, 50).r > 0.9,
        "inside stroke reached the middle"
    );
}

#[test]
fn color_overlay_recolours_only_the_layer() {
    let mut layer = square_layer();
    layer.style.color_overlay.enabled = true;
    layer.style.color_overlay.settings.color = Rgba::new(0.0, 1.0, 0.0, 1.0);
    let styled = schist_layer_fx::render(&layer).expect("styled");

    let px = styled.tiles.pixel(50, 50);
    assert!(px.g > 0.9 && px.r < 0.1, "overlay did not recolour: {px:?}");
    assert_eq!(
        styled.tiles.pixel(30, 30).a,
        0.0,
        "overlay painted outside the layer"
    );
}

#[test]
fn outer_glow_surrounds_the_layer_evenly() {
    let mut layer = square_layer();
    layer.style.outer_glow.enabled = true;
    layer.style.outer_glow.settings.size = 6.0;
    let styled = schist_layer_fx::render(&layer).expect("styled");

    // A glow has no direction, so all four sides should light up.
    for (x, y, side) in [
        (37, 50, "left"),
        (62, 50, "right"),
        (50, 37, "top"),
        (50, 62, "bottom"),
    ] {
        assert!(alpha_at(&styled, x, y) > 0.05, "no glow on the {side} side");
    }
}

#[test]
fn fill_opacity_fades_the_layer_but_not_its_shadow() {
    let mut layer = square_layer();
    layer.fill_opacity = 0.0;
    layer.style.drop_shadow.enabled = true;
    layer.style.drop_shadow.settings.distance = 10.0;
    layer.style.drop_shadow.settings.size = 1.0;
    layer.style.drop_shadow.settings.knockout = false;
    let styled = schist_layer_fx::render(&layer).expect("styled");

    // Photoshop's Fill hides the content and leaves the effects.
    assert!(
        styled.tiles.pixel(50, 50).r < 0.1,
        "fill opacity did not hide the layer's own pixels"
    );
    let shadowed = (0..40).any(|d| alpha_at(&styled, 60 + d % 20, 60 + d / 20) > 0.05);
    assert!(shadowed, "fill opacity also killed the shadow");
}

#[test]
fn outset_grows_with_the_biggest_effect() {
    let mut style = LayerStyle::default();
    assert_eq!(style.outset(), 1, "no effects should need no room");
    style.drop_shadow.enabled = true;
    style.drop_shadow.settings.distance = 30.0;
    style.drop_shadow.settings.size = 10.0;
    assert!(style.outset() >= 40, "shadow reach not accounted for");
}

#[test]
fn styled_bounds_cover_every_painted_pixel() {
    let mut layer = square_layer();
    layer.style.drop_shadow.enabled = true;
    layer.style.drop_shadow.settings.distance = 20.0;
    layer.style.drop_shadow.settings.size = 8.0;
    layer.style.outer_glow.enabled = true;
    layer.style.outer_glow.settings.size = 12.0;
    let styled = schist_layer_fx::render(&layer).expect("styled");

    let painted: IntRect = styled.tiles.content_bounds();
    assert!(
        styled.bounds.left <= painted.left
            && styled.bounds.top <= painted.top
            && styled.bounds.right >= painted.right
            && styled.bounds.bottom >= painted.bottom,
        "declared bounds {:?} do not cover painted {painted:?}",
        styled.bounds
    );
}
