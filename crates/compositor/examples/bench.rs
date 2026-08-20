//! Compositor throughput measurement.
//!
//! Interactive editing only recomposites *damaged, visible* tiles, so the
//! number that matters is "one screenful of tiles through the layer stack",
//! not the whole canvas. Run with `cargo run --release -p
//! photoslop-compositor --example bench`.

use photoslop_adjustments::{Curve, Curves, Params};
use photoslop_color::Depth;
use photoslop_compositor::{composite_region_rgba8, composite_tile, TileCache};
use photoslop_core::{
    blit_rgba8, AdjustmentData, AdjustmentKind, BlendMode, Document, IntRect, Layer, LayerKind,
    TileCoord,
};
use std::time::Instant;

fn solid_layer(name: &str, w: u32, h: u32, rgba: [u8; 4], blend: BlendMode) -> Layer {
    let mut layer = Layer::new_raster(name);
    let buf: Vec<u8> = rgba
        .iter()
        .cycle()
        .take((w * h * 4) as usize)
        .copied()
        .collect();
    blit_rgba8(
        &mut layer.as_raster_mut().unwrap().tiles,
        Depth::Eight,
        IntRect::from_size(w, h),
        &buf,
    );
    layer.blend = blend;
    layer
}

fn curves_layer() -> Layer {
    let params = Params::Curves(Curves {
        rgb: Curve {
            points: vec![(0.0, 0.05), (0.25, 0.35), (0.75, 0.8), (1.0, 0.95)],
        },
        ..Default::default()
    });
    let mut layer = Layer::new_raster("Curves");
    layer.kind = LayerKind::Adjustment(AdjustmentData {
        kind: AdjustmentKind::Curves,
        raw: Vec::new(),
        params_json: Some(serde_json::to_string(&params).unwrap()),
    });
    layer
}

fn main() {
    // ~100 megapixels, the figure PLAN.md §7 sets as the M8 target.
    let (w, h) = (11_000u32, 9_100u32);
    println!(
        "building a {}x{} document ({:.0} MP)…",
        w,
        h,
        (w as f64 * h as f64) / 1e6
    );
    let build = Instant::now();
    let mut doc = Document::new("bench", w, h, Depth::Eight);
    doc.push_layer(solid_layer(
        "bg",
        w,
        h,
        [40, 60, 90, 255],
        BlendMode::Normal,
    ));
    doc.push_layer(solid_layer(
        "overlay",
        w,
        h,
        [200, 120, 60, 160],
        BlendMode::Overlay,
    ));
    doc.push_layer(solid_layer(
        "screen",
        w,
        h,
        [30, 30, 30, 200],
        BlendMode::Screen,
    ));
    doc.push_layer(curves_layer());
    println!("  built in {:?}", build.elapsed());

    // One screenful at 100% zoom: 1920x1080 rounds up to 8x5 tiles.
    let viewport = IntRect::from_xywh(2048, 1024, 1920, 1080);
    let coords: Vec<TileCoord> = TileCoord::covering(&viewport).collect();
    println!(
        "\nviewport recomposite: {} tiles ({} layers incl. a curves adjustment)",
        coords.len(),
        doc.tree.len()
    );

    for round in 0..3 {
        let start = Instant::now();
        let mut cache = TileCache::new();
        cache.prewarm(&doc, &coords);
        let elapsed = start.elapsed();
        println!(
            "  pass {round}: {:?}  ({:.1} fps equivalent)",
            elapsed,
            1.0 / elapsed.as_secs_f64()
        );
    }

    // Single-tile cost, the unit a brush stroke actually dirties.
    let start = Instant::now();
    let n = 64;
    for i in 0..n {
        let _ = composite_tile(&doc, coords[i % coords.len()]);
    }
    println!("\nsingle tile: {:?} each", start.elapsed() / n as u32);

    // Whole-canvas flatten, the cost of an export or a PSD save.
    let start = Instant::now();
    let _ = composite_region_rgba8(&doc, doc.canvas_rect());
    println!(
        "full {}MP flatten: {:?}",
        (w as f64 * h as f64 / 1e6) as u32,
        start.elapsed()
    );
}
