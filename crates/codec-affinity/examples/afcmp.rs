//! Dev tool: write our composite of a file and the file's own thumbnail
//! side by side, so a score from `afscore` can be looked at.
//!
//! ```sh
//! cargo run -p schist-codec-affinity --example afcmp -- file.af /tmp/out
//! ```
//! leaves `<stem>.affinity.png` and `<stem>.schist.png` in the directory.

/// The app rebuilds styled rasters when a document opens; these tools
/// must do the same for effects to composite.
fn restyle(layers: &mut [schist_core::Layer]) {
    for l in layers {
        if let schist_core::LayerKind::Group(g) = &mut l.kind {
            restyle(&mut g.children);
        }
        if !l.style.is_empty() {
            l.styled = schist_compositor::render_styled(l).map(std::sync::Arc::new);
        }
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: afcmp <file> <outdir>");
    let dir = args.next().expect("usage: afcmp <file> <outdir>");
    let bytes = std::fs::read(&path).expect("read");
    let archive = schist_codec_affinity::Archive::parse(&bytes).expect("parse");
    let thumb = image::load_from_memory(archive.thumbnail().expect("thumb"))
        .expect("decode")
        .to_rgba8();
    let stem = std::path::Path::new(&path)
        .file_stem()
        .unwrap()
        .to_string_lossy()
        .to_string();
    thumb.save(format!("{dir}/{stem}.affinity.png")).expect("save");

    let (mut doc, _) = schist_codec_affinity::read_affinity(&bytes).expect("import");
    restyle(&mut doc.tree.layers);
    let region = schist_core::IntRect::from_size(doc.width, doc.height);
    let pixels = schist_compositor::composite_region_rgba8(&doc, region);
    let ours = image::RgbaImage::from_raw(doc.width, doc.height, pixels).expect("buffer");
    // `resize` is a convolution even at the same size, which smears a
    // probe card of hard colour edges into a blur the comparison then
    // blames on the importer; only resample when the thumbnail really
    // is a different size.
    let ours = if ours.dimensions() == thumb.dimensions() {
        ours
    } else {
        image::imageops::resize(&ours, thumb.width(), thumb.height(), image::imageops::Triangle)
    };
    ours.save(format!("{dir}/{stem}.schist.png")).expect("save");
    println!("{stem}: {}x{}", thumb.width(), thumb.height());
}
