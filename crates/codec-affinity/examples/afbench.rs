//! Time each stage of an Affinity import: `afbench <file>...`
//!
//! Prints per-file wall times for archive parse, doc.dat extraction,
//! graph parse, and the full import, so codec changes can be measured
//! against a real corpus.

use std::time::Instant;

fn main() {
    for path in std::env::args().skip(1) {
        let bytes = std::fs::read(&path).expect("read file");
        let t0 = Instant::now();
        let archive = match schist_codec_affinity::Archive::parse(&bytes) {
            Ok(a) => a,
            Err(e) => {
                println!("{path}: archive parse failed: {e}");
                continue;
            }
        };
        let t_archive = t0.elapsed();

        let t1 = Instant::now();
        let entry = archive.head("doc.dat").expect("doc.dat");
        let doc_bytes = archive.extract(entry).expect("extract doc.dat");
        let t_doc = t1.elapsed();

        let t2 = Instant::now();
        let full = schist_codec_affinity::read_affinity(&bytes);
        let t_full = t2.elapsed();

        if std::env::var_os("AFBENCH_LAYERS").is_some() {
            if let Ok((doc, _)) = &full {
                for layer in doc.tree.iter() {
                    let b = layer.content_bounds();
                    println!(
                        "  layer {:?}: {}x{} at ({},{})",
                        layer.name,
                        b.width(),
                        b.height(),
                        b.left,
                        b.top
                    );
                }
            }
        }
        match full {
            Ok((doc, report)) => println!(
                "{path}: {:.1} MiB, archive {:?}, doc.dat {:?} ({} KiB), full import {:?} ({} raster layers, {} skipped, {}x{})",
                bytes.len() as f64 / (1024.0 * 1024.0),
                t_archive,
                t_doc,
                doc_bytes.len() / 1024,
                t_full,
                report.raster_layers,
                report.skipped.len(),
                doc.width,
                doc.height,
            ),
            Err(e) => println!("{path}: import failed after {t_full:?}: {e}"),
        }
    }
}
