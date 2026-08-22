//! Dev tool: composite each leaf layer of an import on its own, into a
//! numbered PNG per layer, so a stray artifact in the full composite
//! can be traced back to the layer that draws it.
//!
//! ```sh
//! cargo run -p schist-codec-affinity --example afsolo -- file.af /tmp/solo
//! ```

use schist_core::{Layer, LayerKind};

/// Every non-group layer, paired with its path through the tree.
fn leaves(layers: &[Layer], path: &str, out: &mut Vec<(String, Layer)>) {
    for (i, layer) in layers.iter().enumerate() {
        let child = format!("{path}/{i}");
        match &layer.kind {
            LayerKind::Group(g) => leaves(&g.children, &child, out),
            _ => out.push((child, layer.clone())),
        }
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: afsolo <file> <outdir>");
    let dir = args.next().expect("usage: afsolo <file> <outdir>");
    let bytes = std::fs::read(&path).expect("read");
    let (mut doc, _) = schist_codec_affinity::read_affinity(&bytes).expect("import");

    let mut solo = Vec::new();
    leaves(&doc.tree.layers, "", &mut solo);
    std::fs::create_dir_all(&dir).expect("mkdir");

    let region = schist_core::IntRect::from_size(doc.width, doc.height);
    for (n, (id, layer)) in solo.iter().enumerate() {
        println!("{n:02} {id} {:?} {:?}", layer.name, layer.tight_bounds());
        doc.tree.layers = vec![layer.clone()];
        let pixels = schist_compositor::composite_region_rgba8(&doc, region);
        image::RgbaImage::from_raw(doc.width, doc.height, pixels)
            .expect("buffer")
            .save(format!("{dir}/{n:02}.png"))
            .expect("save");
    }
}
