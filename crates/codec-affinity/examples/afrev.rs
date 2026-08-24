//! Dev tool: list every revision of an entry (default "doc.dat") across
//! the container's savepoints, in FAT-chain order, marking the head.
//!
//! ```sh
//! cargo run -p schist-codec-affinity --example afrev -- file.af [entry]
//! ```

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: afrev <file> [entry]");
    let name = args.next().unwrap_or_else(|| "doc.dat".into());
    let bytes = std::fs::read(&path).expect("read file");
    let archive = schist_codec_affinity::Archive::parse(&bytes).expect("parse");
    let head = archive.head(&name).map(|e| (e.offset, e.savepoint));
    for e in archive.revisions(&name) {
        println!(
            "{name}: flag {} savepoint {} offset {} size {}{}",
            e.flag,
            e.savepoint,
            e.offset,
            e.size,
            if head == Some((e.offset, e.savepoint)) {
                "  <- head"
            } else {
                ""
            }
        );
    }
}
