//! Dev tool: report, and optionally save, a file's embedded thumbnail.
//!
//! For a 512x512 document that thumbnail is Affinity's own render of
//! the page, byte for byte — which is what makes the probe fixtures in
//! `fixtures/affinity-probe` usable as ground truth.
//!
//! ```sh
//! cargo run -p schist-codec-affinity --example afthumb -- file.af [out.png]
//! ```

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: afthumb <file> [out.png]");
    let bytes = std::fs::read(&path).expect("read");
    let archive = schist_codec_affinity::Archive::parse(&bytes).expect("parse");
    let thumb = archive.thumbnail().expect("no thumbnail in this file");
    let kind = if thumb.starts_with(b"\x89PNG") {
        "png"
    } else if thumb.starts_with(&[0xFF, 0xD8]) {
        "jpeg"
    } else {
        "?"
    };
    let img = image::load_from_memory(thumb).expect("decode").to_rgba8();
    println!(
        "{path}: {kind} {}x{} ({} bytes)",
        img.width(),
        img.height(),
        thumb.len()
    );
    if let Some(out) = std::env::args().nth(2) {
        std::fs::write(&out, thumb).expect("write");
        println!("wrote {out}");
    }
}
