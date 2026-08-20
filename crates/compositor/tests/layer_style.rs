use photoslop_color::{Depth, Rgba};
use photoslop_core::{Document, Layer, TileCoord};

#[test]
fn composited_document_shows_a_layer_shadow() {
    let mut doc = Document::new("t", 200, 200, Depth::Eight);
    let mut layer = Layer::new_raster("sq");
    {
        let raster = layer.as_raster_mut().unwrap();
        for y in 60..140 {
            for x in 60..140 {
                let coord = TileCoord::containing(x, y);
                let trect = coord.rect();
                let buf = raster.tiles.get_mut_or_insert(coord, Depth::Eight);
                let ix = ((y - trect.top) * photoslop_core::TILE_SIZE + (x - trect.left)) as usize;
                buf.set(ix, Rgba::new(1.0, 0.0, 0.0, 1.0));
            }
        }
    }
    layer.style.drop_shadow.enabled = true;
    layer.style.drop_shadow.settings.distance = 10.0;
    layer.style.drop_shadow.settings.size = 4.0;
    layer.style.drop_shadow.settings.angle = 135.0;
    layer.styled = photoslop_layer_fx::render(&layer).map(std::sync::Arc::new);
    assert!(layer.styled.is_some(), "fx renderer produced nothing");
    doc.push_layer(layer);

    let out = photoslop_compositor::composite_region_rgba8(
        &doc,
        photoslop_core::IntRect::new(0, 0, 200, 200),
    );
    let at = |x: usize, y: usize| -> [u8; 4] {
        let i = (y * 200 + x) * 4;
        [out[i], out[i + 1], out[i + 2], out[i + 3]]
    };
    assert!(at(100, 100)[0] > 200, "layer itself missing");
    let sh = at(146, 146);
    assert!(sh[3] > 20, "no shadow below-right of the square: {sh:?}");
    assert_eq!(at(50, 50)[3], 0, "shadow on the lit side");
}
