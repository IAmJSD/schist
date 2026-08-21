//! Tests over real Affinity-authored files in fixtures/affinity.
//!
//! Following the PSD convention, these skip (pass) when the corpus is
//! missing so a partial checkout still builds green.

use std::path::PathBuf;

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/affinity")
}

fn fixtures() -> Vec<(String, Vec<u8>)> {
    let Ok(entries) = std::fs::read_dir(fixture_dir()) else {
        eprintln!("skipping: no fixtures at {}", fixture_dir().display());
        return Vec::new();
    };
    let mut out: Vec<(String, Vec<u8>)> = entries
        .flatten()
        .filter(|e| {
            e.path()
                .extension()
                .is_some_and(|x| x.to_string_lossy().starts_with("af"))
        })
        .map(|e| {
            (
                e.file_name().to_string_lossy().into_owned(),
                std::fs::read(e.path()).unwrap(),
            )
        })
        .collect();
    out.sort();
    out
}

#[test]
fn archives_parse_and_all_entries_extract() {
    for (name, bytes) in fixtures() {
        let archive = photoslop_codec_affinity::Archive::parse(&bytes)
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        let names: Vec<String> = archive.names().map(str::to_owned).collect();
        assert!(names.iter().any(|n| n == "doc.dat"), "{name}: no doc.dat");
        for entry_name in &names {
            if let Some(entry) = archive.head(entry_name) {
                let data = archive
                    .extract(entry)
                    .unwrap_or_else(|e| panic!("{name}/{entry_name}: {e}"));
                assert!(!data.is_empty(), "{name}/{entry_name}: empty");
            }
        }
        println!("{name}: {} entries ok", names.len());
    }
}

#[test]
fn graphs_parse() {
    for (name, bytes) in fixtures() {
        let dump = photoslop_codec_affinity::import::dump(&bytes)
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(dump.contains("DocR"), "{name}: no document root in dump");
        println!("=== {name}: dump {} chars", dump.len());
    }
}

/// raster_test.afdesign holds one pixel layer: a black brush stroke whose
/// alpha tile is bounded by rect [22,32]–[157,157] (visible in the raw
/// block table). Verify the decoded pixels honor that geometry.
#[test]
fn raster_pixels_land_where_the_file_says() {
    let path = fixture_dir().join("raster_test.afdesign");
    let Ok(bytes) = std::fs::read(&path) else {
        eprintln!("skipping: no {}", path.display());
        return;
    };
    let (doc, report) = photoslop_codec_affinity::read_affinity(&bytes).unwrap();
    assert!(report.complete());
    assert_eq!(report.raster_layers, 1);

    fn find_raster(layers: &[photoslop_core::Layer]) -> Option<&photoslop_core::Layer> {
        layers.iter().find_map(|l| match l.children() {
            Some(children) => find_raster(children),
            None => l.as_raster().is_some().then_some(l),
        })
    }
    let layer = find_raster(&doc.tree.layers).expect("a raster layer");
    let tiles = &layer.as_raster().unwrap().tiles;

    let mut inside = 0u32;
    let mut outside = 0u32;
    for y in 0..180 {
        for x in 0..180 {
            let px = tiles.pixel(x, y).to_u8();
            if px[3] > 0 {
                let in_rect = (22..157).contains(&x) && (32..157).contains(&y);
                if in_rect {
                    inside += 1;
                    // The stroke is black paint: color must be black
                    // wherever it is opaque.
                    if px[3] == 255 {
                        assert_eq!(&px[..3], &[0, 0, 0], "at {x},{y}");
                    }
                } else {
                    outside += 1;
                }
            }
        }
    }
    assert!(inside > 100, "stroke has substance ({inside} px)");
    assert_eq!(outside, 0, "no paint outside the block rect");
}

#[test]
fn documents_import() {
    for (name, bytes) in fixtures() {
        let (doc, report) = photoslop_codec_affinity::read_affinity(&bytes)
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(doc.width > 0 && doc.height > 0, "{name}: empty canvas");
        println!(
            "{name}: {}×{}, {} raster, {} groups, {} skipped {:?}",
            doc.width,
            doc.height,
            report.raster_layers,
            report.groups,
            report.skipped.len(),
            report.skipped
        );
    }
}
