//! Extract one entry from an Affinity container and describe it.
//!
//! ```sh
//! cargo run -p schist-codec-affinity --example afentry -- file.afphoto d/1
//! ```

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: afentry <file> <entry>");
    let name = args.next().expect("usage: afentry <file> <entry>");
    let bytes = std::fs::read(&path).expect("read file");
    let archive = schist_codec_affinity::Archive::parse(&bytes).expect("parse");
    let entry = archive.head(&name).expect("no such entry");
    println!(
        "compressed {} → {} bytes, compression {:#04x}",
        entry.compressed_size, entry.size, entry.compression
    );
    let data = archive.extract(entry).expect("extract");
    println!("extracted {} bytes", data.len());
    let n = data.len().min(48);
    for b in &data[..n] {
        print!("{b:02x} ");
    }
    println!();
    for b in &data[..n] {
        print!("{} ", if b.is_ascii_graphic() { *b as char } else { '.' });
    }
    println!();
}
