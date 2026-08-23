//! CPU vs GPU compositor throughput on the same document the CPU bench
//! uses (`schist-compositor --example bench`). Run with
//! `cargo run --release -p schist-compositor-gpu --example bench`.

use schist_adjustments::{Curve, Curves, Params};
use schist_compositor::{Compositor, CpuCompositor};
use schist_compositor_gpu::GpuCompositor;
use schist_core::color::Depth;
use schist_core::{
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

fn run(name: &str, backend: &dyn Compositor, doc: &Document, coords: &[TileCoord]) {
    for round in 0..3 {
        let start = Instant::now();
        let _ = backend.tiles_rgba8(doc, coords);
        let elapsed = start.elapsed();
        println!(
            "  {name} pass {round}: {elapsed:?}  ({:.1} fps equivalent)",
            1.0 / elapsed.as_secs_f64()
        );
    }
    let start = Instant::now();
    let n = 64u32;
    for i in 0..n as usize {
        let _ = backend.tile(doc, coords[i % coords.len()]);
    }
    println!("  {name} single tile: {:?} each", start.elapsed() / n);
    let start = Instant::now();
    let _ = backend.region_rgba8(doc, doc.canvas_rect());
    println!("  {name} full flatten: {:?}", start.elapsed());
}

fn main() {
    let (w, h) = (11_000u32, 9_100u32);
    println!(
        "building a {}x{} document ({:.0} MP)…",
        w,
        h,
        (w as f64 * h as f64) / 1e6
    );
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

    let viewport = IntRect::from_xywh(2048, 1024, 1920, 1080);
    let coords: Vec<TileCoord> = TileCoord::covering(&viewport).collect();
    println!(
        "viewport recomposite: {} tiles, {} layers incl. a curves adjustment\n",
        coords.len(),
        doc.tree.len()
    );

    run("cpu", &CpuCompositor, &doc, &coords);
    match GpuCompositor::new() {
        Ok(gpu) => {
            println!("\ngpu backend: {}", gpu.describe());
            run("gpu", &gpu, &doc, &coords);
        }
        Err(e) => println!("\nno gpu backend: {e}"),
    }
}
