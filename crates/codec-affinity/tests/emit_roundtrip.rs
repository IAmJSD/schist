//! The serializer must reproduce, byte for byte, every object graph in
//! every fixture — and, when `SCHIST_AFFINITY_CORPUS` points at real
//! document folders, every graph there too. This is the exporter's
//! bedrock: if re-emitting what Affinity wrote is exact, emitting
//! patched versions of it stands on solid ground.

use schist_codec_affinity::archive::Archive;
use schist_codec_affinity::{emit, graph};

/// Graph-document magic: an entry payload starting with these bytes is
/// an object graph (doc.dat, or a tile wrapped in a `Data` document).
const GRAPH_MAGIC: [u8; 4] = [0x00, 0xFF, 0x4B, 0x53];

fn fixture_dirs() -> Vec<std::path::PathBuf> {
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures");
    let mut dirs = vec![root.join("affinity"), root.join("affinity-probe")];
    if let Ok(corpus) = std::env::var("SCHIST_AFFINITY_CORPUS") {
        dirs.extend(corpus.split(':').filter(|d| !d.is_empty()).map(Into::into));
    }
    dirs
}

/// The exporter's normalization passes encode two stream invariants
/// Affinity's reader enforces (declare-once type chains, object ids
/// numbered 0, 1, 2… in definition order). Real documents already
/// satisfy both, so the passes must be a no-op on them — if either
/// changes a real doc.dat, the invariant model is wrong.
#[test]
fn normalization_is_identity_on_real_documents() {
    let mut docs = 0usize;
    for dir in fixture_dirs() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for file in entries.flatten() {
            let path = file.path();
            let is_affinity = path
                .extension()
                .is_some_and(|x| x.to_string_lossy().starts_with("af"));
            if !is_affinity || path.to_string_lossy().contains("~lock~") {
                continue;
            }
            let bytes = std::fs::read(&path).unwrap();
            let Ok(archive) = Archive::parse(&bytes) else {
                continue;
            };
            let Some(entry) = archive.head("doc.dat") else {
                continue;
            };
            let plain = archive.extract(entry).unwrap();
            let mut g = graph::parse(&plain).unwrap();
            emit::normalize_declarations(&mut g);
            emit::renumber_ids(&mut g);
            let emitted = emit::serialize(&g).unwrap();
            assert_eq!(emitted, plain, "{path:?}: normalization changed the graph");
            docs += 1;
        }
    }
    assert!(docs > 0, "no documents exercised — fixtures missing?");
    eprintln!("normalization was a no-op on {docs} real documents");
}

#[test]
fn every_graph_reserializes_byte_exactly() {
    let mut files = 0usize;
    let mut graphs = 0usize;
    let mut failures: Vec<String> = Vec::new();

    for dir in fixture_dirs() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for file in entries.flatten() {
            let path = file.path();
            let is_affinity = path
                .extension()
                .is_some_and(|x| x.to_string_lossy().starts_with("af"));
            if !is_affinity || path.to_string_lossy().contains("~lock~") {
                continue;
            }
            let bytes = std::fs::read(&path).unwrap();
            let Ok(archive) = Archive::parse(&bytes) else {
                continue;
            };
            files += 1;
            let names: Vec<String> = archive.names().map(String::from).collect();
            for name in names {
                let Some(entry) = archive.head(&name) else {
                    continue;
                };
                let Ok(plain) = archive.extract(entry) else {
                    failures.push(format!("{path:?} {name}: extract failed"));
                    continue;
                };
                if !plain.starts_with(&GRAPH_MAGIC) {
                    continue;
                }
                let parsed = match graph::parse(&plain) {
                    Ok(g) => g,
                    Err(e) => {
                        failures.push(format!("{path:?} {name}: parse failed: {e}"));
                        continue;
                    }
                };
                graphs += 1;
                match emit::serialize(&parsed) {
                    Ok(emitted) if emitted == plain => {}
                    Ok(emitted) => {
                        let at = emitted
                            .iter()
                            .zip(&plain)
                            .position(|(a, b)| a != b)
                            .unwrap_or_else(|| emitted.len().min(plain.len()));
                        failures.push(format!(
                            "{path:?} {name}: {} vs {} bytes, first difference at {at}",
                            emitted.len(),
                            plain.len()
                        ));
                    }
                    Err(e) => failures.push(format!("{path:?} {name}: emit failed: {e}")),
                }
            }
        }
    }

    assert!(
        failures.is_empty(),
        "{} of {graphs} graphs (over {files} files) did not round-trip:\n{}",
        failures.len(),
        failures.join("\n")
    );
    assert!(graphs > 0, "no graphs exercised — fixtures missing?");
    eprintln!("{graphs} graphs across {files} files round-tripped byte-exactly");
}
