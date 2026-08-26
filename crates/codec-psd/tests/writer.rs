//! Writer + round-trip tests.
//!
//! Every test here goes through the public API in both directions —
//! build/read a `Document`, `write_psd`, then `read_psd` the bytes back —
//! so the reader acts as the writer's oracle. Where ImageMagick is present
//! it double-checks the merged composite with a third-party decoder.

use schist_codec_psd::{read_psd, write_psd, write_psd_with};
use schist_color::{ColorMode, Depth, Rgba};
use schist_core::{
    blit_rgba8, BlendMode, Document, IntRect, Layer, LayerKind, LayerMask, PreservedResource,
    RawBlock, TileCoord, TILE_SIZE,
};

fn solid_layer(name: &str, rect: IntRect, rgba: [u8; 4], depth: Depth) -> Layer {
    let mut layer = Layer::new_raster(name);
    let n = rect.width() as usize * rect.height() as usize;
    let buf: Vec<u8> = rgba.iter().cycle().take(n * 4).copied().collect();
    blit_rgba8(&mut layer.as_raster_mut().unwrap().tiles, depth, rect, &buf);
    layer
}

fn base_doc() -> Document {
    Document::new("t", 64, 48, Depth::Eight)
}

fn pixel(doc: &Document, layer_ix: usize, x: i32, y: i32) -> [u8; 4] {
    doc.tree.layers[layer_ix]
        .as_raster()
        .expect("raster layer")
        .tiles
        .pixel(x, y)
        .to_u8()
}

#[test]
fn round_trips_pixels_and_layer_properties() {
    let mut doc = base_doc();
    doc.push_layer(solid_layer(
        "bottom",
        IntRect::from_xywh(0, 0, 64, 48),
        [10, 20, 30, 255],
        Depth::Eight,
    ));
    let mut top = solid_layer(
        "top layer",
        IntRect::from_xywh(8, 4, 16, 16),
        [200, 100, 50, 128],
        Depth::Eight,
    );
    top.blend = BlendMode::Multiply;
    top.opacity = 0.5;
    top.visible = false;
    top.clipping = true;
    doc.push_layer(top);

    let bytes = write_psd(&doc).expect("write");
    let back = read_psd(&bytes).expect("read");

    assert_eq!((back.width, back.height), (64, 48));
    assert_eq!(back.depth, Depth::Eight);
    assert_eq!(back.mode, ColorMode::Rgb);
    assert_eq!(back.tree.layers.len(), 2);

    assert_eq!(back.tree.layers[0].name, "bottom");
    assert_eq!(pixel(&back, 0, 5, 5), [10, 20, 30, 255]);

    let t = &back.tree.layers[1];
    assert_eq!(t.name, "top layer");
    assert_eq!(t.blend, BlendMode::Multiply);
    assert!(!t.visible);
    assert!(t.clipping);
    assert!((t.opacity - 0.5).abs() < 0.01, "opacity {}", t.opacity);
    assert_eq!(pixel(&back, 1, 10, 6), [200, 100, 50, 128]);
    // Outside the written layer bounds there must be nothing.
    assert_eq!(pixel(&back, 1, 40, 40)[3], 0);
}

#[test]
fn round_trips_tight_layer_bounds() {
    // A layer whose content is a small patch inside a big canvas must not
    // be written out padded to tile granularity.
    let mut doc = Document::new("t", 512, 512, Depth::Eight);
    doc.push_layer(solid_layer(
        "patch",
        IntRect::from_xywh(300, 260, 3, 2),
        [1, 2, 3, 255],
        Depth::Eight,
    ));
    let back = read_psd(&write_psd(&doc).unwrap()).unwrap();
    assert_eq!(pixel(&back, 0, 300, 260), [1, 2, 3, 255]);
    assert_eq!(pixel(&back, 0, 302, 261), [1, 2, 3, 255]);
    assert_eq!(pixel(&back, 0, 303, 261)[3], 0);
    assert_eq!(pixel(&back, 0, 299, 260)[3], 0);
}

#[test]
fn round_trips_nested_groups() {
    let mut doc = base_doc();
    doc.push_layer(solid_layer(
        "bg",
        IntRect::from_xywh(0, 0, 64, 48),
        [255, 255, 255, 255],
        Depth::Eight,
    ));

    let mut inner = Layer::new_group("inner");
    if let LayerKind::Group(g) = &mut inner.kind {
        g.children.push(solid_layer(
            "leaf",
            IntRect::from_xywh(0, 0, 8, 8),
            [0, 255, 0, 255],
            Depth::Eight,
        ));
        g.open = false;
    }
    let mut outer = Layer::new_group("outer");
    outer.opacity = 0.75;
    if let LayerKind::Group(g) = &mut outer.kind {
        g.children.push(inner);
        g.open = true;
    }
    doc.push_layer(outer);

    let back = read_psd(&write_psd(&doc).unwrap()).unwrap();
    assert_eq!(back.tree.layers.len(), 2, "bg + outer group");
    let outer = &back.tree.layers[1];
    assert_eq!(outer.name, "outer");
    assert!(outer.is_group());
    assert!((outer.opacity - 0.75).abs() < 0.01);
    assert_eq!(outer.blend, BlendMode::PassThrough);

    let inner = &outer.children().unwrap()[0];
    assert_eq!(inner.name, "inner");
    assert!(inner.is_group());
    match &inner.kind {
        LayerKind::Group(g) => {
            assert!(!g.open, "collapsed state round-trips");
            assert_eq!(g.children[0].name, "leaf");
            assert_eq!(
                g.children[0].as_raster().unwrap().tiles.pixel(2, 2).to_u8(),
                [0, 255, 0, 255]
            );
        }
        _ => panic!("expected group"),
    }
}

#[test]
fn round_trips_layer_mask() {
    let mut doc = base_doc();
    let mut layer = solid_layer(
        "masked",
        IntRect::from_xywh(0, 0, 32, 32),
        [255, 0, 0, 255],
        Depth::Eight,
    );
    let mut mask = LayerMask::new_revealing();
    mask.default_value = 0;
    mask.bounds = IntRect::from_xywh(0, 0, 16, 16);
    mask.enabled = false;
    for y in 0..16 {
        for x in 0..16 {
            let coord = TileCoord::containing(x, y);
            let buf = mask.tiles.get_mut_or_insert(coord);
            let lx = x.rem_euclid(TILE_SIZE) as usize;
            let ly = y.rem_euclid(TILE_SIZE) as usize;
            buf[ly * TILE_SIZE as usize + lx] = if x < 8 { 255 } else { 64 };
        }
    }
    layer.mask = Some(mask);
    doc.push_layer(layer);

    let back = read_psd(&write_psd(&doc).unwrap()).unwrap();
    let m = back.tree.layers[0].mask.as_ref().expect("mask survived");
    assert_eq!(m.default_value, 0);
    assert!(!m.enabled, "disabled flag round-trips");
    assert_eq!(m.bounds, IntRect::from_xywh(0, 0, 16, 16));
    assert_eq!(m.value(2, 2), 255);
    assert_eq!(m.value(12, 2), 64);
    // Outside the mask rect the default applies.
    assert_eq!(m.value(20, 20), 0);
}

#[test]
fn preserves_unknown_layer_blocks_verbatim() {
    let mut doc = base_doc();
    let mut layer = solid_layer(
        "with extras",
        IntRect::from_xywh(0, 0, 8, 8),
        [1, 1, 1, 255],
        Depth::Eight,
    );
    // An odd-length payload also exercises the pad-to-even rule.
    layer.extras.push(RawBlock {
        key: *b"xyzw",
        data: vec![0xDE, 0xAD, 0xBE, 0xEF, 0x01],
    });
    layer.extras.push(RawBlock {
        key: *b"TySh",
        data: b"pretend text engine data".to_vec(),
    });
    doc.push_layer(layer);

    let back = read_psd(&write_psd(&doc).unwrap()).unwrap();
    let extras = &back.tree.layers[0].extras;
    let xyzw = extras
        .iter()
        .find(|b| &b.key == b"xyzw")
        .expect("xyzw kept");
    assert_eq!(xyzw.data, vec![0xDE, 0xAD, 0xBE, 0xEF, 0x01]);
    let tysh = extras
        .iter()
        .find(|b| &b.key == b"TySh")
        .expect("TySh kept");
    assert_eq!(tysh.data, b"pretend text engine data");
}

#[test]
fn psb_widened_keys_round_trip() {
    // In PSB these keys carry a u64 length. The writer emitted u32 for all
    // of them, so the reader consumed four bytes of payload as part of the
    // length and the file became unreadable by Schist and Photoshop alike.
    for key in [*b"LMsk", *b"lnk2", *b"PxSD", *b"Layr"] {
        let mut doc = base_doc();
        let mut layer = solid_layer(
            "psb extras",
            IntRect::from_xywh(0, 0, 8, 8),
            [7, 8, 9, 255],
            Depth::Eight,
        );
        layer.extras.push(RawBlock {
            key,
            data: b"preserved payload".to_vec(),
        });
        doc.push_layer(layer);

        let bytes = write_psd_with(&doc, true).expect("psb write");
        let back = read_psd(&bytes).unwrap_or_else(|e| {
            panic!(
                "psb with {:?} failed to read back: {e}",
                std::str::from_utf8(&key)
            )
        });
        let block = back.tree.layers[0]
            .extras
            .iter()
            .find(|b| b.key == key)
            .unwrap_or_else(|| panic!("{:?} kept", std::str::from_utf8(&key)));
        assert_eq!(block.data, b"preserved payload");
    }
}

#[test]
fn psd_keeps_u32_lengths_for_the_same_keys() {
    // The widening is PSB-only; a plain PSD must be unaffected.
    let mut doc = base_doc();
    let mut layer = solid_layer(
        "psd extras",
        IntRect::from_xywh(0, 0, 8, 8),
        [7, 8, 9, 255],
        Depth::Eight,
    );
    layer.extras.push(RawBlock {
        key: *b"LMsk",
        data: b"preserved payload".to_vec(),
    });
    doc.push_layer(layer);

    let back = read_psd(&write_psd(&doc).unwrap()).unwrap();
    let block = back.tree.layers[0]
        .extras
        .iter()
        .find(|b| &b.key == b"LMsk")
        .expect("LMsk kept");
    assert_eq!(block.data, b"preserved payload");
}

#[test]
fn an_adjustment_layer_made_in_schist_survives_a_save() {
    // The critical one: adjustment layers created in the app carry their
    // settings only in `params_json`. With no encoder the writer emitted
    // an empty raster layer, so saving destroyed every adjustment the user
    // had made, crash-recovery snapshots included.
    use schist_core::{AdjustmentData, AdjustmentKind, LayerKind};

    let params = schist_adjustments::Params::Posterize { levels: 6 };
    let mut doc = base_doc();
    let mut layer = Layer::new_raster("Posterize");
    layer.kind = LayerKind::Adjustment(AdjustmentData {
        kind: AdjustmentKind::Posterize,
        raw: Vec::new(),
        params_json: Some(serde_json::to_string(&params).unwrap()),
    });
    doc.push_layer(layer);

    let back = read_psd(&write_psd(&doc).unwrap()).unwrap();
    let layer = &back.tree.layers[0];
    match &layer.kind {
        LayerKind::Adjustment(data) => {
            assert_eq!(data.kind, AdjustmentKind::Posterize);
            assert_eq!(
                schist_adjustments::parse_psd(data.kind, &data.raw),
                params,
                "the settings must come back"
            );
        }
        other => panic!("came back as {other:?}, not an adjustment layer"),
    }
}

#[test]
fn editing_a_photoshop_adjustment_layer_writes_the_new_settings() {
    // The other half: a layer that arrived from a PSD keeps a preserved
    // block. Once the parameters are edited that block is stale, so the
    // re-encoded one has to win or the save silently reverts the edit.
    use schist_core::{AdjustmentData, AdjustmentKind, LayerKind, RawBlock};

    let edited = schist_adjustments::Params::Threshold {
        level: 200.0 / 255.0,
    };
    let mut doc = base_doc();
    let mut layer = Layer::new_raster("Threshold");
    layer.kind = LayerKind::Adjustment(AdjustmentData {
        kind: AdjustmentKind::Threshold,
        raw: 50u16.to_be_bytes().to_vec(),
        params_json: Some(serde_json::to_string(&edited).unwrap()),
    });
    // The stale block as it came off disk.
    layer.extras.push(RawBlock {
        key: *b"thrs",
        data: 50u16.to_be_bytes().to_vec(),
    });
    doc.push_layer(layer);

    let back = read_psd(&write_psd(&doc).unwrap()).unwrap();
    match &back.tree.layers[0].kind {
        LayerKind::Adjustment(data) => assert_eq!(
            schist_adjustments::parse_psd(data.kind, &data.raw),
            edited,
            "the edit must win over the preserved block"
        ),
        other => panic!("came back as {other:?}"),
    }
}

#[test]
fn preserves_image_resources_and_resolution() {
    let mut doc = base_doc();
    doc.resolution_dpi = 144.0;
    doc.icc_profile = Some(vec![7u8; 32]);
    doc.preserved_resources.push(PreservedResource {
        id: 0x0426,
        name: vec![0, 0],
        data: vec![1, 2, 3, 4],
    });
    doc.push_layer(solid_layer(
        "l",
        IntRect::from_xywh(0, 0, 4, 4),
        [9, 9, 9, 255],
        Depth::Eight,
    ));

    let back = read_psd(&write_psd(&doc).unwrap()).unwrap();
    assert!((back.resolution_dpi - 144.0).abs() < 0.01);
    assert_eq!(back.icc_profile.as_deref(), Some(&[7u8; 32][..]));
    let custom = back
        .preserved_resources
        .iter()
        .find(|r| r.id == 0x0426)
        .expect("unknown resource kept");
    assert_eq!(custom.data, vec![1, 2, 3, 4]);
}

#[test]
fn round_trips_sixteen_bit_documents() {
    let mut doc = Document::new("deep", 16, 16, Depth::Sixteen);
    let mut layer = Layer::new_raster("hi");
    {
        let tiles = &mut layer.as_raster_mut().unwrap().tiles;
        let coord = TileCoord { tx: 0, ty: 0 };
        let buf = tiles.get_mut_or_insert(coord, Depth::Sixteen);
        for y in 0..16usize {
            for x in 0..16usize {
                buf.set(y * TILE_SIZE as usize + x, Rgba::new(0.25, 0.5, 0.75, 1.0));
            }
        }
    }
    doc.push_layer(layer);

    let back = read_psd(&write_psd(&doc).unwrap()).unwrap();
    assert_eq!(back.depth, Depth::Sixteen);
    let px = back.tree.layers[0].as_raster().unwrap().tiles.pixel(3, 3);
    assert!((px.r - 0.25).abs() < 1e-4, "r={}", px.r);
    assert!((px.g - 0.5).abs() < 1e-4, "g={}", px.g);
    assert!((px.b - 0.75).abs() < 1e-4, "b={}", px.b);
}

#[test]
fn round_trips_psb_container() {
    let mut doc = base_doc();
    doc.push_layer(solid_layer(
        "psb",
        IntRect::from_xywh(0, 0, 20, 20),
        [3, 4, 5, 255],
        Depth::Eight,
    ));
    // Force PSB on a small document to exercise the u64 length widenings.
    let bytes = write_psd_with(&doc, true).unwrap();
    assert_eq!(&bytes[4..6], &[0, 2], "version 2 = PSB");
    let back = read_psd(&bytes).unwrap();
    assert_eq!(pixel(&back, 0, 1, 1), [3, 4, 5, 255]);
}

#[test]
fn round_trips_grayscale_documents() {
    let mut doc = Document::new("gray", 16, 16, Depth::Eight);
    doc.mode = ColorMode::Grayscale;
    doc.push_layer(solid_layer(
        "g",
        IntRect::from_xywh(0, 0, 16, 16),
        [128, 128, 128, 255],
        Depth::Eight,
    ));
    let back = read_psd(&write_psd(&doc).unwrap()).unwrap();
    assert_eq!(back.mode, ColorMode::Grayscale);
    let px = pixel(&back, 0, 4, 4);
    assert_eq!(px[0], 128);
    assert_eq!(px[1], 128, "gray replicated across RGB");
}

#[test]
fn empty_and_oversized_documents_error_cleanly() {
    let empty = Document::new("zero", 0, 0, Depth::Eight);
    assert!(write_psd(&empty).is_err());

    let huge = Document::new("huge", 40_000, 10, Depth::Eight);
    // Auto-selects PSB, which allows it...
    assert!(write_psd(&huge).is_ok());
    // ...but forcing PSD must refuse rather than emit a corrupt file.
    assert!(write_psd_with(&huge, false).is_err());
}

/// Read a real Photoshop-authored fixture, write it back out, read again:
/// structure and pixels must survive our own round trip.
#[test]
fn fixture_files_survive_a_round_trip() {
    let dir = std::path::Path::new("../../fixtures/psd");
    let Ok(entries) = std::fs::read_dir(dir) else {
        eprintln!("no fixtures directory; skipping");
        return;
    };
    let mut checked = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("psd") {
            continue;
        }
        let bytes = std::fs::read(&path).unwrap();
        let original = read_psd(&bytes).expect("fixture reads");
        let rewritten = write_psd(&original).expect("fixture writes");
        let back = read_psd(&rewritten).expect("rewritten file reads");

        assert_eq!(back.width, original.width, "{path:?} width");
        assert_eq!(back.height, original.height, "{path:?} height");
        assert_eq!(back.depth, original.depth, "{path:?} depth");
        assert_eq!(
            back.tree.layers.len(),
            original.tree.layers.len(),
            "{path:?} layer count"
        );
        for (a, b) in original.tree.iter().zip(back.tree.iter()) {
            assert_eq!(a.name, b.name, "{path:?} layer name");
            assert_eq!(a.blend, b.blend, "{path:?} blend");
            assert_eq!(a.visible, b.visible, "{path:?} visibility");
        }
        // Spot-check pixels across the first raster layer.
        if let (Some(a), Some(b)) = (
            original.tree.layers.first().and_then(|l| l.as_raster()),
            back.tree.layers.first().and_then(|l| l.as_raster()),
        ) {
            for (x, y) in [(0, 0), (1, 1), (3, 2)] {
                assert_eq!(
                    a.tiles.pixel(x, y).to_u8(),
                    b.tiles.pixel(x, y).to_u8(),
                    "{path:?} pixel ({x},{y})"
                );
            }
        }
        checked += 1;
    }
    if checked == 0 {
        eprintln!("fixtures directory empty; skipping");
    }
}

/// Third-party validation: ImageMagick must decode our merged composite to
/// the colours we composited. Skipped when `convert` isn't installed.
#[test]
fn merged_composite_reads_in_imagemagick() {
    let has_convert = std::process::Command::new("convert")
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !has_convert {
        eprintln!("ImageMagick not installed; skipping");
        return;
    }
    let mut doc = Document::new("merged", 32, 32, Depth::Eight);
    doc.push_layer(solid_layer(
        "bg",
        IntRect::from_xywh(0, 0, 32, 32),
        [0, 0, 255, 255],
        Depth::Eight,
    ));
    doc.push_layer(solid_layer(
        "fg",
        IntRect::from_xywh(0, 0, 16, 32),
        [255, 0, 0, 255],
        Depth::Eight,
    ));

    let dir = std::env::temp_dir().join("schist-psd-writer-test");
    std::fs::create_dir_all(&dir).unwrap();
    let psd = dir.join("merged.psd");
    std::fs::write(&psd, write_psd(&doc).unwrap()).unwrap();

    let out = std::process::Command::new("convert")
        .arg(format!("{}[0]", psd.display()))
        .args(["-format", "%[pixel:p{4,4}] %[pixel:p{24,4}]", "info:"])
        .output()
        .expect("run convert");
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        text.contains("255,0,0") || text.contains("red"),
        "left half should be red, got: {text}"
    );
    assert!(
        text.contains("0,0,255") || text.contains("blue"),
        "right half should be blue, got: {text}"
    );
}

#[test]
fn round_trips_layer_effects_through_a_file() {
    use schist_core::{StrokePosition, Technique};

    let mut doc = base_doc();
    let mut layer = solid_layer(
        "styled",
        IntRect::new(4, 4, 40, 30),
        [200, 40, 40, 255],
        Depth::Eight,
    );
    layer.style.drop_shadow.enabled = true;
    layer.style.drop_shadow.settings.angle = 45.0;
    layer.style.drop_shadow.settings.distance = 9.0;
    layer.style.drop_shadow.settings.size = 4.0;
    layer.style.drop_shadow.settings.color = Rgba::new(0.1, 0.2, 0.3, 1.0);
    layer.style.stroke.enabled = true;
    layer.style.stroke.settings.position = StrokePosition::Inside;
    layer.style.stroke.settings.size = 5.0;
    layer.style.outer_glow.enabled = true;
    layer.style.outer_glow.settings.technique = Technique::Precise;
    doc.push_layer(layer);

    let back = read_psd(&write_psd(&doc).unwrap()).unwrap();
    let style = back.tree.layers[0].style;

    assert!(style.drop_shadow.enabled, "drop shadow lost");
    assert!((style.drop_shadow.settings.angle - 45.0).abs() < 0.01);
    assert!((style.drop_shadow.settings.distance - 9.0).abs() < 0.01);
    assert!((style.drop_shadow.settings.color.b - 0.3).abs() < 0.01);
    assert!(style.stroke.enabled, "stroke lost");
    assert_eq!(style.stroke.settings.position, StrokePosition::Inside);
    assert!((style.stroke.settings.size - 5.0).abs() < 0.01);
    assert_eq!(style.outer_glow.settings.technique, Technique::Precise);
}

#[test]
fn a_layer_with_no_effects_writes_no_effects_block() {
    let mut doc = base_doc();
    doc.push_layer(solid_layer(
        "plain",
        IntRect::new(0, 0, 10, 10),
        [1, 2, 3, 255],
        Depth::Eight,
    ));
    let back = read_psd(&write_psd(&doc).unwrap()).unwrap();
    assert!(
        back.tree.layers[0].style.is_empty(),
        "effects appeared from nowhere"
    );
    assert!(
        !back.tree.layers[0].extras.iter().any(|b| &b.key == b"lfx2"),
        "an empty lfx2 block was written"
    );
}

#[test]
fn editing_effects_replaces_the_preserved_block() {
    // A file arrives with an effects block we did not write; the user then
    // changes the effects. The saved file must carry the new ones, and
    // exactly one lfx2 block.
    let mut doc = base_doc();
    let mut layer = solid_layer(
        "styled",
        IntRect::new(0, 0, 20, 20),
        [9, 9, 9, 255],
        Depth::Eight,
    );
    layer.extras.push(RawBlock {
        key: *b"lfx2",
        data: vec![0xDE, 0xAD, 0xBE, 0xEF],
    });
    layer.style.color_overlay.enabled = true;
    layer.style.color_overlay.settings.color = Rgba::new(0.0, 1.0, 0.0, 1.0);
    doc.push_layer(layer);

    let back = read_psd(&write_psd(&doc).unwrap()).unwrap();
    let blocks = back.tree.layers[0]
        .extras
        .iter()
        .filter(|b| &b.key == b"lfx2")
        .count();
    assert_eq!(blocks, 1, "expected exactly one effects block");
    assert!(back.tree.layers[0].style.color_overlay.enabled);
    assert!((back.tree.layers[0].style.color_overlay.settings.color.g - 1.0).abs() < 0.01);
}

#[test]
fn round_trips_cmyk_and_lab_documents() {
    // These modes used to be a hard Unsupported on the way in. Pixels are
    // still edited as RGBA, so what is checked here is that the file is
    // genuinely written in its mode and comes back looking the same.
    for mode in [ColorMode::Cmyk, ColorMode::Lab] {
        let mut doc = Document::new("t", 32, 24, Depth::Eight);
        doc.mode = mode;
        doc.push_layer(solid_layer(
            "c",
            IntRect::new(0, 0, 32, 24),
            [200, 90, 40, 255],
            Depth::Eight,
        ));
        let bytes = write_psd(&doc).expect("write");
        // The header's mode number must actually say so.
        let mode_num = u16::from_be_bytes([bytes[24], bytes[25]]);
        assert_eq!(
            mode_num,
            match mode {
                ColorMode::Cmyk => 4,
                _ => 9,
            },
            "{mode:?} was not written in its own mode"
        );
        let back = read_psd(&bytes).expect("read");
        assert_eq!(back.mode, mode, "{mode:?} mode lost");
        let px = pixel(&back, 0, 10, 10);
        for c in 0..3 {
            let want = [200i32, 90, 40][c];
            assert!(
                (px[c] as i32 - want).abs() <= 6,
                "{mode:?} channel {c}: {} vs {want}",
                px[c]
            );
        }
    }
}

#[test]
fn cmyk_files_carry_four_colour_channels() {
    let mut doc = Document::new("t", 16, 16, Depth::Eight);
    doc.mode = ColorMode::Cmyk;
    doc.push_layer(solid_layer(
        "c",
        IntRect::new(0, 0, 16, 16),
        [10, 20, 30, 255],
        Depth::Eight,
    ));
    let bytes = write_psd(&doc).unwrap();
    // Header channel count: four inks plus alpha.
    assert_eq!(u16::from_be_bytes([bytes[12], bytes[13]]), 5);
}

#[test]
fn shape_layers_round_trip_as_vectors() {
    // The point of a shape layer is that it stays a shape. If it came back
    // as a picture of itself, resizing it would be lossy.
    use schist_core::{Anchor, SubPath, VectorPath, VectorShape};

    let mut doc = Document::new("t", 200, 100, Depth::Eight);
    let mut path = VectorPath::new("Rect");
    path.subpaths.push(SubPath {
        anchors: vec![
            Anchor::corner(20.0, 10.0),
            Anchor::corner(180.0, 10.0),
            Anchor::corner(180.0, 90.0),
            Anchor::corner(20.0, 90.0),
        ],
        closed: true,
    });
    let mut layer = Layer::new_raster("Shape");
    layer.shape = Some(Box::new(VectorShape::new(
        path,
        Rgba::new(0.2, 0.4, 0.8, 1.0),
    )));
    doc.push_layer(layer);

    let back = read_psd(&write_psd(&doc).unwrap()).unwrap();
    let shape = back.tree.layers[0]
        .shape
        .as_deref()
        .expect("the shape came back as pixels only");
    assert_eq!(shape.path.subpaths.len(), 1);
    assert!(shape.path.subpaths[0].closed);
    let pts: Vec<(f32, f32)> = shape.path.anchors().map(|(_, _, a)| a.point).collect();
    assert_eq!(pts.len(), 4);
    assert!((pts[0].0 - 20.0).abs() < 0.05, "corner moved: {:?}", pts[0]);
    assert!((pts[2].1 - 90.0).abs() < 0.05, "corner moved: {:?}", pts[2]);
    assert!(
        (shape.fill.b - 0.8).abs() < 0.01,
        "fill colour lost: {:?}",
        shape.fill
    );
}

#[test]
fn an_ordinary_layer_gains_no_vector_blocks() {
    let mut doc = Document::new("t", 32, 32, Depth::Eight);
    doc.push_layer(solid_layer(
        "plain",
        IntRect::new(0, 0, 32, 32),
        [1, 2, 3, 255],
        Depth::Eight,
    ));
    let back = read_psd(&write_psd(&doc).unwrap()).unwrap();
    assert!(back.tree.layers[0].shape.is_none());
    assert!(
        !back.tree.layers[0].extras.iter().any(|b| &b.key == b"vmsk"),
        "a vector mask appeared from nowhere"
    );
}
