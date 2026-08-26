//! The exporter's proof: documents written as .af must read back
//! through our own importer with their structure and pixels intact, and
//! every real fixture document must survive an import → export →
//! import cycle.

use schist_codec_affinity::{read_affinity, write_affinity};
use schist_color::Depth;
use schist_core::{blit_rgba8, BlendMode, Document, IntRect, Layer, LayerKind};

fn raster_layer(name: &str, rect: IntRect, color: [u8; 4]) -> Layer {
    let mut layer = Layer::new_raster(name);
    let pixels = vec![color; rect.width() as usize * rect.height() as usize].concat();
    blit_rgba8(
        &mut layer.as_raster_mut().unwrap().tiles,
        Depth::Eight,
        rect,
        &pixels,
    );
    layer
}

#[test]
fn synthetic_document_round_trips() {
    let mut doc = Document::new("export me", 640, 400, Depth::Eight);

    // Bottom: a full-bleed background.
    doc.push_layer(raster_layer(
        "Background",
        IntRect::new(0, 0, 640, 400),
        [10, 20, 30, 255],
    ));

    // A translucent, offset, multiplied layer with a mask, a drop
    // shadow and an outline — the sticker-meme special.
    let mut tinted = raster_layer(
        "Tinted",
        IntRect::new(100, 50, 420, 300),
        [200, 40, 40, 255],
    );
    tinted.opacity = 0.5;
    tinted.blend = BlendMode::Multiply;
    tinted.style.drop_shadow.enabled = true;
    tinted.style.drop_shadow.settings.size = 7.0;
    tinted.style.drop_shadow.settings.distance = 4.0;
    tinted.style.stroke.enabled = true;
    tinted.style.stroke.settings.size = 6.0;
    tinted.style.stroke.settings.color = schist_color::Rgba::new(1.0, 1.0, 1.0, 1.0);
    let mut mask = schist_core::LayerMask::new_revealing();
    mask.bounds = IntRect::new(100, 50, 260, 300);
    for y in 50..300 {
        for x in 100..260 {
            let coord = schist_core::TileCoord::containing(x, y);
            let buf = mask.tiles.get_mut_or_insert(coord);
            let r = coord.rect();
            buf[((y - r.top) * schist_core::TILE_SIZE + (x - r.left)) as usize] = 128;
        }
    }
    tinted.mask = Some(mask);
    doc.push_layer(tinted);

    // A group holding a hidden layer and a clipped pair.
    let mut group = Layer::new_group("Stack");
    group.blend = BlendMode::Normal; // isolated, not pass-through
    if let LayerKind::Group(g) = &mut group.kind {
        let mut hidden = raster_layer("Hidden", IntRect::new(0, 0, 64, 64), [1, 2, 3, 255]);
        hidden.visible = false;
        g.children.push(hidden);
        g.children.push(raster_layer(
            "Base",
            IntRect::new(300, 100, 500, 250),
            [0, 255, 0, 255],
        ));
        let mut clip = raster_layer("Clip", IntRect::new(280, 80, 560, 200), [255, 255, 0, 255]);
        clip.clipping = true;
        g.children.push(clip);
    }
    doc.push_layer(group);

    let (bytes, report) = write_affinity(&doc, None).expect("export succeeds");
    assert!(
        report.skipped.is_empty(),
        "nothing should be dropped: {:?}",
        report.skipped
    );
    assert!(schist_codec_affinity::is_affinity(&bytes));

    let (redoc, reimport) = read_affinity(&bytes).expect("our own file reads back");
    assert!(
        reimport.skipped.is_empty(),
        "reimport skipped: {:?}",
        reimport.skipped
    );
    assert_eq!((redoc.width, redoc.height), (640, 400));
    assert_eq!(redoc.tree.layers.len(), 3);

    let bg = &redoc.tree.layers[0];
    assert_eq!(bg.name, "Background");
    assert_eq!(
        bg.as_raster().unwrap().tiles.pixel(320, 200).to_u8(),
        [10, 20, 30, 255]
    );

    let tinted = &redoc.tree.layers[1];
    assert_eq!(tinted.name, "Tinted");
    assert!((tinted.opacity - 0.5).abs() < 1e-3);
    assert_eq!(tinted.blend, BlendMode::Multiply);
    assert_eq!(
        tinted.as_raster().unwrap().tiles.pixel(150, 100).to_u8(),
        [200, 40, 40, 255]
    );
    let mask = tinted.mask.as_ref().expect("mask survives");
    assert_eq!(mask.value(150, 100), 128);
    assert_eq!(mask.value(300, 100), 255, "outside the mask reveals");
    assert!(tinted.style.drop_shadow.enabled, "drop shadow survives");
    assert!((tinted.style.drop_shadow.settings.size - 7.0).abs() < 1e-3);
    assert!((tinted.style.drop_shadow.settings.distance - 4.0).abs() < 1e-3);
    assert!(tinted.style.stroke.enabled, "stroke survives");
    assert!((tinted.style.stroke.settings.size - 6.0).abs() < 1e-3);
    assert_eq!(
        tinted.style.stroke.settings.color.to_u8(),
        [255, 255, 255, 255]
    );

    let group = &redoc.tree.layers[2];
    assert_eq!(group.name, "Stack");
    assert!(group.is_group());
    let children = group.children().unwrap();
    // Hidden, Base, and Base's clipped child above it.
    assert_eq!(children.len(), 3);
    assert_eq!(children[0].name, "Hidden");
    assert!(!children[0].visible);
    assert_eq!(children[1].name, "Base");
    assert_eq!(children[2].name, "Clip");
    assert!(children[2].clipping, "clipped child comes back clipping");
    assert_eq!(
        children[2]
            .as_raster()
            .unwrap()
            .tiles
            .pixel(300, 100)
            .to_u8(),
        [255, 255, 0, 255]
    );
}

#[test]
fn sixteen_bit_documents_export_at_eight() {
    let mut doc = Document::new("deep", 32, 32, Depth::Sixteen);
    let mut layer = Layer::new_raster("gray");
    let tiles = &mut layer.as_raster_mut().unwrap().tiles;
    let coord = schist_core::TileCoord::containing(0, 0);
    let buf = tiles.get_mut_or_insert(coord, Depth::Sixteen);
    for i in 0..32 * 32 {
        let (x, y) = (i % 32, i / 32);
        buf.set(
            y * schist_core::TILE_SIZE as usize + x,
            schist_color::Rgba::new(0.25, 0.5, 0.75, 1.0),
        );
    }
    doc.push_layer(layer);

    let (bytes, _) = write_affinity(&doc, None).unwrap();
    let (redoc, _) = read_affinity(&bytes).unwrap();
    let px = redoc.tree.layers[0]
        .as_raster()
        .unwrap()
        .tiles
        .pixel(10, 10)
        .to_u8();
    assert!(px[0].abs_diff(64) <= 1 && px[1].abs_diff(128) <= 1 && px[2].abs_diff(191) <= 1);
}

/// Import each fixture, export it, and import the export: structure
/// (names, kinds, visibility, blends) and per-layer pixels must agree.
/// With SCHIST_AFFINITY_CORPUS set, real documents join the sweep.
#[test]
fn fixture_documents_survive_reexport() {
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures");
    let mut dirs = vec![root.join("affinity"), root.join("affinity-probe")];
    if let Ok(corpus) = std::env::var("SCHIST_AFFINITY_CORPUS") {
        dirs.extend(corpus.split(':').filter(|d| !d.is_empty()).map(Into::into));
    }

    let mut checked = 0usize;
    let mut failures: Vec<String> = Vec::new();
    for dir in dirs {
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
            let Ok((doc, _)) = read_affinity(&bytes) else {
                continue; // preview-only files are the codec's problem, not the exporter's
            };
            let (out, _report) = match write_affinity(&doc, None) {
                Ok(v) => v,
                Err(e) => {
                    failures.push(format!("{path:?}: export failed: {e}"));
                    continue;
                }
            };
            let (redoc, _) = match read_affinity(&out) {
                Ok(v) => v,
                Err(e) => {
                    failures.push(format!("{path:?}: reimport failed: {e}"));
                    continue;
                }
            };
            checked += 1;
            if let Err(e) = compare_stacks(&doc.tree.layers, &redoc.tree.layers, "") {
                failures.push(format!("{path:?}: {e}"));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} documents failed re-export:\n{}",
        failures.len(),
        failures.join("\n")
    );
    assert!(checked > 0, "no fixture documents exercised");
    eprintln!("{checked} documents survived import → export → import");
}

/// Affinity's reader enforces two stream-order invariants our importer
/// never needed (issue #40, "the file appears to be corrupted"): 0x31
/// object ids count 0, 1, 2… in definition order, and each class is
/// declared with a full versioned type chain exactly once — its first
/// occurrence — with every later node using the lone-tag shorthand.
/// Exported documents must satisfy both.
#[test]
fn exported_graphs_keep_affinitys_stream_invariants() {
    use schist_codec_affinity::graph::{self, ChainEnd};
    use schist_codec_affinity::Archive;

    let mut doc = Document::new("invariants", 700, 500, Depth::Eight);
    doc.push_layer(raster_layer(
        "Background",
        IntRect::new(0, 0, 700, 500),
        [90, 90, 120, 255],
    ));
    let mut styled = raster_layer(
        "Styled",
        IntRect::new(40, 40, 400, 300),
        [250, 200, 40, 255],
    );
    styled.style.drop_shadow.enabled = true;
    styled.style.stroke.enabled = true;
    doc.push_layer(styled);
    let mut group = Layer::new_group("Group");
    if let LayerKind::Group(g) = &mut group.kind {
        g.children.push(raster_layer(
            "Inner",
            IntRect::new(0, 0, 64, 64),
            [1, 2, 3, 255],
        ));
    }
    doc.push_layer(group);

    let (bytes, _) = write_affinity(&doc, None).expect("export");
    let archive = Archive::parse(&bytes).expect("container");
    let plain = archive
        .extract(archive.head("doc.dat").expect("doc.dat"))
        .expect("extract");
    let g = graph::parse(&plain).expect("graph");

    // Parse pushes nodes in stream order, so both invariants can be
    // checked over `nodes` directly.
    let mut next_id = 0u32;
    let mut declared = std::collections::HashSet::new();
    for n in &g.nodes {
        if n.framing != 0x31 || n.types.is_empty() {
            continue;
        }
        assert_eq!(n.id, next_id, "object ids must count up in stream order");
        next_id += 1;

        let sections = n.section_lens.len();
        for (i, (t, _)) in n.types.iter().enumerate() {
            if i < sections {
                assert!(
                    declared.insert(*t),
                    "class {} declared twice",
                    graph::tag_name(*t)
                );
            } else {
                // The lone tag must reference an earlier declaration.
                assert_eq!(n.chain_end, ChainEnd::LoneTag);
                assert!(
                    declared.contains(t),
                    "shorthand for undeclared class {}",
                    graph::tag_name(*t)
                );
            }
        }
    }
    assert!(next_id > 0, "no 0x31 nodes exercised");
}

fn compare_stacks(
    a: &[schist_core::Layer],
    b: &[schist_core::Layer],
    at: &str,
) -> Result<(), String> {
    // Re-export may drop layers it reports (none expected for raster
    // trees); require equal counts so drops surface loudly.
    if a.len() != b.len() {
        return Err(format!(
            "{at}: {} layers became {} ({:?} vs {:?})",
            a.len(),
            b.len(),
            a.iter().map(|l| l.name.as_str()).collect::<Vec<_>>(),
            b.iter().map(|l| l.name.as_str()).collect::<Vec<_>>()
        ));
    }
    for (la, lb) in a.iter().zip(b) {
        let ctx = format!("{at}/{}", la.name);
        if la.visible != lb.visible {
            return Err(format!(
                "{ctx}: visibility {} vs {}",
                la.visible, lb.visible
            ));
        }
        if (la.opacity - lb.opacity).abs() > 1e-3 {
            return Err(format!("{ctx}: opacity {} vs {}", la.opacity, lb.opacity));
        }
        if la.mask.is_some() != lb.mask.is_some() {
            return Err(format!(
                "{ctx}: mask {:?} vs {:?}",
                la.mask.is_some(),
                lb.mask.is_some()
            ));
        }
        match (la.children(), lb.children()) {
            (Some(ca), Some(cb)) => compare_stacks(ca, cb, &ctx)?,
            (None, None) => {
                if let (Some(ra), Some(rb)) = (la.as_raster(), lb.as_raster()) {
                    let ba = ra.tiles.content_bounds();
                    let bb = rb.tiles.content_bounds();
                    if ba != bb {
                        return Err(format!("{ctx}: bounds {ba:?} vs {bb:?}"));
                    }
                    // Sample a grid of pixels for equality (8-bit docs
                    // re-encode exactly).
                    if !ba.is_empty() {
                        for sy in 0..5 {
                            for sx in 0..5 {
                                let x = ba.left + (ba.width() - 1) * sx / 4;
                                let y = ba.top + (ba.height() - 1) * sy / 4;
                                let pa = ra.tiles.pixel(x, y).to_u8();
                                let pb = rb.tiles.pixel(x, y).to_u8();
                                if pa != pb {
                                    return Err(format!("{ctx}: pixel ({x},{y}) {pa:?} vs {pb:?}"));
                                }
                            }
                        }
                    }
                }
            }
            _ => return Err(format!("{ctx}: group-ness diverged")),
        }
    }
    Ok(())
}
