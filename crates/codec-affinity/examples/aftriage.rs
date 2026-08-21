//! Batch-import a directory of Affinity files and report per-file results.
//!
//! ```sh
//! cargo run -p photoslop-codec-affinity --example aftriage -- /path/to/dir
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
        match photoslop_codec_affinity::read_affinity(&bytes) {
            Ok((doc, report)) => {
                let mut kinds: std::collections::BTreeMap<String, usize> = Default::default();
                for (_, k) in &report.skipped {
                    *kinds.entry(k.clone()).or_default() += 1;
                }
                println!(
                    "{name}: {}x{} raster:{} groups:{} masks:{} skipped:{} {:?}",
                    doc.width, doc.height, report.raster_layers, report.groups,
                    report.masks, report.skipped.len(), kinds
                );
            }
            Err(e) => println!("{name}: ERROR {e}"),
        }
    }
}
