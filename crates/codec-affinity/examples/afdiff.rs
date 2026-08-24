//! Dev tool: composite an imported document next to the file's
//! embedded thumbnail, for eyeballing import mismatches.
//!
//! ```sh
//! cargo run -p schist-codec-affinity --example afdiff -- file.afdesign out.png
//! ```

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
    let path = args.next().expect("usage: afdiff <file> <out.png>");
    let out_path = args.next().expect("usage: afdiff <file> <out.png>");
    let bytes = std::fs::read(&path).expect("read");
    let archive = schist_codec_affinity::Archive::parse(&bytes).expect("parse");
    let thumb = image::load_from_memory(archive.thumbnail().expect("thumb"))
        .expect("decode")
        .to_rgba8();
    let (mut doc, report) = schist_codec_affinity::read_affinity(&bytes).expect("import");
    eprintln!("{report:?}");
    restyle(&mut doc.tree.layers);

    // Match the thumbnail's proportions: composite at document size,
    // then scale to the thumbnail's height for the side-by-side.
    let (w, h) = (doc.width, doc.height);
    let region = schist_core::IntRect::from_size(w, h);
    let pixels = schist_compositor::composite_region_rgba8(&doc, region);
    let ours = image::RgbaImage::from_raw(w, h, pixels).expect("buffer");
    let th = thumb.height();
    let tw = (w as u64 * th as u64 / h as u64) as u32;
    let ours = image::imageops::resize(&ours, tw, th, image::imageops::Triangle);

    let mut img = image::RgbaImage::from_pixel(tw + thumb.width(), th, image::Rgba([255; 4]));
    image::imageops::overlay(&mut img, &ours, 0, 0);
    image::imageops::overlay(&mut img, &thumb, tw as i64, 0);
    img.save(&out_path).expect("save");
    eprintln!("wrote {out_path}");
}
