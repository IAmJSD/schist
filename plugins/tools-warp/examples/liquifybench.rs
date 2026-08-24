//! What a Liquify stroke costs per pointer event. Run with
//! `cargo run --release -p schist-tools-warp --example liquifybench`.
//!
//! A dab only moves the mesh under the brush, so what matters is that the
//! numbers track the brush and not the layer: the 12-megapixel row should
//! cost about what the 0.8-megapixel one does at the same size.

use schist_color::{Depth, Rgba};
use schist_core::{Document, Layer, TileCoord, TILE_PIXELS};
use schist_plugin_api::{EditorState, OptionValue, PointerInput, ToolCtx, ToolPlugin};
use schist_tools_warp::liquify::LiquifyTool;
use std::time::Instant;

fn at(x: f32, y: f32) -> PointerInput {
    PointerInput {
        x,
        y,
        pressure: 1.0,
        modifiers: Default::default(),
    }
}

fn document(w: u32, h: u32) -> Document {
    let mut doc = Document::new("bench", w, h, Depth::Eight);
    let mut layer = Layer::new_raster("art");
    let rect = doc.canvas_rect();
    if let Some(raster) = layer.as_raster_mut() {
        let mut state = 0x2545F4914F6CDD1Du64;
        for coord in TileCoord::covering(&rect) {
            let buf = raster.tiles.get_mut_or_insert(coord, Depth::Eight);
            for i in 0..TILE_PIXELS {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                let n = (state >> 40) as f32 / 16777216.0;
                buf.set(i, Rgba::new(n, 1.0 - n, 0.5, 1.0));
            }
        }
    }
    doc.active_layer = Some(layer.id);
    doc.tree.layers.push(layer);
    doc
}

fn main() {
    let ms = |start: Instant| start.elapsed().as_secs_f64() * 1000.0;
    println!(
        "{:>14}  {:>5}  {:>8}  {:>13}  {:>8}",
        "layer", "brush", "pick up", "pointer move", "commit"
    );
    for (w, h, size) in [
        (1024u32, 768u32, 100.0f32),
        (4000, 3000, 100.0),
        (4000, 3000, 1000.0),
    ] {
        let mut doc = document(w, h);
        let mut state = EditorState::default();
        let mut tool = LiquifyTool::new();
        tool.set_option("liquify-size", OptionValue::Num(size));
        let mut ctx = ToolCtx {
            doc: &mut doc,
            state: &mut state,
        };

        let start = Instant::now();
        tool.on_activate(&mut ctx);
        let begin_ms = ms(start);

        let (cx, cy) = (w as f32 / 2.0, h as f32 / 2.0);
        tool.on_pointer_down(&mut ctx, at(cx, cy));
        let moves = 40;
        let start = Instant::now();
        for i in 1..=moves {
            tool.on_pointer_move(&mut ctx, at(cx + i as f32 * 3.0, cy + i as f32));
        }
        let move_ms = ms(start) / moves as f64;
        tool.on_pointer_up(&mut ctx, at(cx, cy));
        ctx.doc.take_damage();

        let start = Instant::now();
        tool.on_deactivate(&mut ctx);
        let commit_ms = ms(start);

        println!(
            "{:>9}x{:<4} {size:>5.0}  {begin_ms:>6.1} ms  {move_ms:>10.2} ms  {commit_ms:>6.1} ms",
            w, h
        );
    }
}
