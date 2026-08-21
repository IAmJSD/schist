//! Batch-import a directory of Affinity files and report per-file results.
//!
//! ```sh
//! cargo run -p schist-codec-affinity --example aftriage -- /path/to/dir
//! ```

fn main() {
    let dir = std::env::args().nth(1).expect("usage: aftriage <dir>");
    let mut paths: Vec<_> = std::fs::read_dir(&dir)
        .expect("read dir")
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .is_some_and(|x| x.to_string_lossy().starts_with("af"))
        })
        .collect();
    paths.sort();
    for path in paths {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let bytes = std::fs::read(&path).expect("read file");
        match schist_codec_affinity::read_affinity(&bytes) {
            Ok((doc, report)) => {
                if std::env::var("AFTRIAGE_NAMES").is_ok() {
                    fn walk(layers: &[schist_core::Layer], depth: usize) {
                        for l in layers {
                            println!(
                                "{}- {:?}{}",
                                "  ".repeat(depth),
                                l.name,
                                if l.mask.is_some() { " [mask]" } else { "" }
                            );
                            if let Some(children) = l.children() {
                                walk(children, depth + 1);
                            }
                        }
                    }
                    walk(&doc.tree.layers, 1);
                }
                let mut kinds: std::collections::BTreeMap<String, usize> = Default::default();
                for (_, k) in &report.skipped {
                    *kinds.entry(k.clone()).or_default() += 1;
                }
                println!(
                    "{name}: {}x{} raster:{} groups:{} masks:{} text:{} shapes:{} adj:{} skipped:{} {:?}",
                    doc.width, doc.height, report.raster_layers, report.groups,
                    report.masks, report.text_layers, report.shapes, report.adjustments, report.skipped.len(), kinds
                );
            }
            Err(e) => println!("{name}: ERROR {e}"),
        }
    }
}
