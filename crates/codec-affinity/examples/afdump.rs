//! Dump an Affinity file's container and object graph as text.
//!
//! ```sh
//! cargo run -p schist-codec-affinity --example afdump -- file.afphoto
//! ```

fn main() {
    let path = std::env::args().nth(1).expect("usage: afdump <file>");
    let bytes = std::fs::read(&path).expect("read file");
    match schist_codec_affinity::import::dump(&bytes) {
        Ok(text) => println!("{text}"),
        Err(e) => {
            eprintln!("{path}: {e}");
            std::process::exit(1);
        }
    }
}
