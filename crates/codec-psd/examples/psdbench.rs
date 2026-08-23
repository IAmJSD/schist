//! Time PSD read/write on realistic inputs: `psdbench <file>...`
//!
//! A `.psd`/`.psb` argument is timed directly. An Affinity argument is
//! imported and written out as PSD first (cached next to /tmp) so the
//! corpus of large real documents can exercise this codec too.

use std::time::Instant;

fn main() {
    for path in std::env::args().skip(1) {
        let bytes = std::fs::read(&path).expect("read file");
        let psd = if schist_codec_psd::is_psd(&bytes) {
            bytes
        } else {
            let stem = std::path::Path::new(&path)
                .file_stem()
                .unwrap()
                .to_string_lossy()
                .into_owned();
            let cache = std::env::temp_dir().join(format!("psdbench-{stem}.psd"));
            if let Ok(cached) = std::fs::read(&cache) {
                cached
            } else {
                let (doc, _) = schist_codec_affinity::read_affinity(&bytes).expect("affinity");
                let t = Instant::now();
                let out = schist_codec_psd::write_psd(&doc).expect("write_psd");
                println!(
                    "{path}: write_psd {:?} ({:.1} MiB)",
                    t.elapsed(),
                    out.len() as f64 / (1024.0 * 1024.0)
                );
                std::fs::write(&cache, &out).expect("cache psd");
                out
            }
        };
        let t = Instant::now();
        match schist_codec_psd::read_psd(&psd) {
            Ok(doc) => println!(
                "{path}: {:.1} MiB psd, read_psd {:?} ({}x{})",
                psd.len() as f64 / (1024.0 * 1024.0),
                t.elapsed(),
                doc.width,
                doc.height
            ),
            Err(e) => println!("{path}: read_psd failed after {:?}: {e}", t.elapsed()),
        }
    }
}
