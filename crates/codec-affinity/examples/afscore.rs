//! Dev tool: RMS difference between a composited import and the file's
//! embedded thumbnail (lower is better).
fn main() {
    let path = std::env::args().nth(1).expect("usage: afscore <file>");
    let bytes = std::fs::read(&path).expect("read");
    let archive = schist_codec_affinity::Archive::parse(&bytes).expect("parse");
    let thumb = image::load_from_memory(archive.thumbnail().expect("thumb"))
        .expect("decode")
        .to_rgba8();
    let (doc, _) = schist_codec_affinity::read_affinity(&bytes).expect("import");
    let region = schist_core::IntRect::from_size(doc.width, doc.height);
    let pixels = schist_compositor::composite_region_rgba8(&doc, region);
    let ours = image::RgbaImage::from_raw(doc.width, doc.height, pixels).expect("buffer");
    let ours = image::imageops::resize(
        &ours,
        thumb.width(),
        thumb.height(),
        image::imageops::Triangle,
    );
    let mut sum = 0.0f64;
    let mut n = 0u64;
    for (a, b) in ours.pixels().zip(thumb.pixels()) {
        // composite both over white so transparency compares fairly
        let over = |p: &image::Rgba<u8>, i: usize| {
            let alpha = p.0[3] as f64 / 255.0;
            p.0[i] as f64 * alpha + 255.0 * (1.0 - alpha)
        };
        for i in 0..3 {
            let d = over(a, i) - over(b, i);
            sum += d * d;
            n += 1;
        }
    }
    println!("{}: rms {:.2}", path, (sum / n as f64).sqrt());
}
