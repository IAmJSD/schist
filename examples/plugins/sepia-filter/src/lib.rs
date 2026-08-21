//! Example third-party filter: sepia tone.
//!
//! Build: `cargo build --release --target wasm32-unknown-unknown -p schist-example-sepia`
//! Install: copy `target/wasm32-unknown-unknown/release/schist_example_sepia.wasm`
//! into `~/.config/schist/plugins/`.

use schist_plugin_sdk::*;

schist_filter! {
    id: "com.example.sepia",
    name: "Sepia",
    category: "Plugins",
    params: [
        param("amount", "Amount", 0.0, 100.0, 100.0, "%"),
    ],
    apply: |pixels: &mut [f32], _width: usize, _height: usize, params: &Params| {
        let amount = (params.get_or("amount", 100.0) / 100.0).clamp(0.0, 1.0);
        for px in pixels.chunks_exact_mut(4) {
            let (r, g, b) = (px[0], px[1], px[2]);
            // The classic sepia matrix.
            let sr = (0.393 * r + 0.769 * g + 0.189 * b).min(1.0);
            let sg = (0.349 * r + 0.686 * g + 0.168 * b).min(1.0);
            let sb = (0.272 * r + 0.534 * g + 0.131 * b).min(1.0);
            px[0] = r + (sr - r) * amount;
            px[1] = g + (sg - g) * amount;
            px[2] = b + (sb - b) * amount;
        }
    }
}
