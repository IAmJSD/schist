//! Dev tool: print the text spec the importer resolved for each text
//! layer, from the `PsTx` block it stores for re-editing.
//!
//! ```sh
//! cargo run -p schist-codec-affinity --example aftext -- file.afphoto
//! ```

use schist_core::{Layer, LayerKind};

fn walk(layers: &[Layer], file: &str) {
    for layer in layers {
        if let LayerKind::Group(g) = &layer.kind {
            walk(&g.children, file);
        }
        for block in &layer.extras {
            if &block.key == b"PsTx" {
                let spec: serde_json::Value = match serde_json::from_slice(&block.data) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let s = &spec["spec"];
                println!(
                    "{file:34} {:?} family={} bold={} size={:.2} align={} lh={} wrap={} bounds={:?}",
                    s["text"].as_str().unwrap_or("").replace('\n', "⏎"),
                    s["family"],
                    s["bold"],
                    s["size"].as_f64().unwrap_or(0.0),
                    s["align"],
                    s["line_height"],
                    s["wrap_width"],
                    layer.tight_bounds(),
                );
            }
        }
    }
}

fn main() {
    for path in std::env::args().skip(1) {
        let bytes = std::fs::read(&path).expect("read");
        let Ok((doc, _)) = schist_codec_affinity::read_affinity(&bytes) else {
            continue;
        };
        let name = std::path::Path::new(&path)
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        walk(&doc.tree.layers, &name);
    }
}
