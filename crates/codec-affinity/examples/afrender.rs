//! Dev tool: composite an imported document at full document size.
//!
//! ```sh
//! cargo run -p schist-codec-affinity --example afrender -- file.afdesign out.png
//! ```

fn restyle(layers: &mut [schist_core::Layer]) {
    for l in layers {
        if let schist_core::LayerKind::Group(g) = &mut l.kind {
            restyle(&mut g.children);
        }
        if !l.style.is_empty() {
            l.styled = schist_layer_fx::render(l).map(std::sync::Arc::new);
        }
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: afrender <file> <out.png>");
    let out_path = args.next().expect("usage: afrender <file> <out.png>");
    let bytes = std::fs::read(&path).expect("read");
    let (mut doc, _report) = schist_codec_affinity::read_affinity(&bytes).expect("import");
    restyle(&mut doc.tree.layers);
    let (w, h) = (doc.width, doc.height);
    let region = schist_core::IntRect::from_size(w, h);
    let pixels = schist_compositor::composite_region_rgba8(&doc, region);
    let img = image::RgbaImage::from_raw(w, h, pixels).expect("buffer");
    img.save(&out_path).expect("save");
    eprintln!("wrote {out_path} ({w}x{h})");
}
