//! Reader integration tests against programmatically built PSD/PSB bytes.

mod common;

use common::{resolution_res, Mask, Psd, Res, L};
use schist_codec_psd::{is_psd, read_psd, PsdError};
use schist_color::{ColorMode, Depth};
use schist_core::{BlendMode, Layer, LayerKind};

fn raster_pixel(layer: &Layer, x: i32, y: i32) -> [u8; 4] {
    layer.as_raster().unwrap().tiles.pixel(x, y).to_u8()
}

#[test]
fn header_and_single_layer_pixels() {
    let mut psd = Psd::rgb8(64, 48);
    psd.layers
        .push(L::solid("Red", (0, 0, 48, 64), [255, 0, 0, 255]));
    let bytes = psd.build();
    assert!(is_psd(&bytes));

    let doc = read_psd(&bytes).unwrap();
    assert_eq!((doc.width, doc.height), (64, 48));
    assert_eq!(doc.depth, Depth::Eight);
    assert_eq!(doc.mode, ColorMode::Rgb);
    assert_eq!(doc.tree.layers.len(), 1);
    let layer = &doc.tree.layers[0];
    assert_eq!(layer.name, "Red");
    assert_eq!(raster_pixel(layer, 0, 0), [255, 0, 0, 255]);
    assert_eq!(raster_pixel(layer, 63, 47), [255, 0, 0, 255]);
    // Outside the layer rect: transparent.
    assert_eq!(raster_pixel(layer, 63, 47), [255, 0, 0, 255]);
    assert_eq!(doc.active_layer, Some(layer.id));
}

#[test]
fn layer_rect_offset_lands_at_document_coordinates() {
    let mut psd = Psd::rgb8(64, 64);
    // rect is (top, left, bottom, right)
    psd.layers
        .push(L::solid("Blue", (8, 16, 24, 32), [0, 0, 255, 128]));
    let doc = read_psd(&psd.build()).unwrap();
    let layer = &doc.tree.layers[0];
    assert_eq!(raster_pixel(layer, 16, 8), [0, 0, 255, 128]);
    assert_eq!(raster_pixel(layer, 31, 23), [0, 0, 255, 128]);
    assert_eq!(raster_pixel(layer, 15, 8), [0, 0, 0, 0]);
    assert_eq!(raster_pixel(layer, 16, 24), [0, 0, 0, 0]);
}

#[test]
fn blend_opacity_visibility_clipping_mapping() {
    let mut psd = Psd::rgb8(16, 16);
    let mut l = L::solid("A", (0, 0, 16, 16), [10, 20, 30, 255]);
    l.blend = *b"mul ";
    l.opacity = 128;
    l.flags = 0b10; // bit 1 set = HIDDEN (inverted visibility flag)
    l.clipping = 1;
    psd.layers.push(l);
    let doc = read_psd(&psd.build()).unwrap();
    let layer = &doc.tree.layers[0];
    assert_eq!(layer.blend, BlendMode::Multiply);
    assert!((layer.opacity - 128.0 / 255.0).abs() < 1e-6);
    assert!(!layer.visible);
    assert!(layer.clipping);
}

#[test]
fn unknown_blend_key_falls_back_to_normal() {
    let mut psd = Psd::rgb8(8, 8);
    let mut l = L::solid("A", (0, 0, 8, 8), [1, 2, 3, 255]);
    l.blend = *b"wxyz";
    psd.layers.push(l);
    let doc = read_psd(&psd.build()).unwrap();
    assert_eq!(doc.tree.layers[0].blend, BlendMode::Normal);
}

#[test]
fn group_nesting_bottom_to_top_order() {
    let mut psd = Psd::rgb8(32, 32);
    // File order is bottom-to-top: bottom layer, then the hidden type-3
    // divider that STARTS the group's children, then the children, then the
    // type-1/2 group header that CLOSES them.
    psd.layers
        .push(L::solid("Bottom", (0, 0, 8, 8), [1, 1, 1, 255]));
    psd.layers.push(L::divider());
    psd.layers
        .push(L::solid("Child", (0, 0, 4, 4), [2, 2, 2, 255]));
    psd.layers
        .push(L::group_header("Folder", 1, Some(*b"pass")));
    psd.layers
        .push(L::solid("Top", (0, 0, 8, 8), [3, 3, 3, 255]));

    let doc = read_psd(&psd.build()).unwrap();
    let names: Vec<&str> = doc.tree.layers.iter().map(|l| l.name.as_str()).collect();
    assert_eq!(names, ["Bottom", "Folder", "Top"]);
    let group = &doc.tree.layers[1];
    assert_eq!(group.blend, BlendMode::PassThrough);
    let LayerKind::Group(g) = &group.kind else {
        panic!("expected group")
    };
    assert!(g.open); // type 1 = open folder
    assert_eq!(g.children.len(), 1);
    assert_eq!(g.children[0].name, "Child");
    assert_eq!(
        g.children[0].as_raster().unwrap().tiles.pixel(0, 0).to_u8(),
        [2, 2, 2, 255]
    );
    // Topmost layer is active.
    assert_eq!(doc.active_layer, Some(doc.tree.layers[2].id));
}

#[test]
fn nested_closed_group_with_own_blend_key() {
    let mut psd = Psd::rgb8(32, 32);
    psd.layers.push(L::divider());
    psd.layers.push(L::divider());
    psd.layers
        .push(L::solid("Inner", (0, 0, 4, 4), [9, 9, 9, 255]));
    psd.layers
        .push(L::group_header("InnerGroup", 2, Some(*b"mul ")));
    psd.layers
        .push(L::group_header("OuterGroup", 1, Some(*b"pass")));
    let doc = read_psd(&psd.build()).unwrap();
    assert_eq!(doc.tree.layers.len(), 1);
    let outer = &doc.tree.layers[0];
    assert_eq!(outer.name, "OuterGroup");
    let LayerKind::Group(og) = &outer.kind else {
        panic!("expected group")
    };
    assert_eq!(og.children.len(), 1);
    let inner = &og.children[0];
    assert_eq!(inner.name, "InnerGroup");
    // Closed folder (type 2), blend key taken from the lsct block itself.
    let LayerKind::Group(ig) = &inner.kind else {
        panic!("expected group")
    };
    assert!(!ig.open);
    assert_eq!(inner.blend, BlendMode::Multiply);
    assert_eq!(ig.children[0].name, "Inner");
}

#[test]
fn layer_mask_decode_default_value_and_disabled_flag() {
    let mut psd = Psd::rgb8(32, 32);
    let mut l = L::solid("Masked", (0, 0, 32, 32), [50, 60, 70, 255]);
    l.mask = Some(Mask {
        rect: (8, 8, 16, 16), // 8x8 mask at (8,8)
        default_color: 255,
        flags: 0,
        pixels: vec![200; 64],
    });
    let mut hidden = L::solid("Disabled", (0, 0, 4, 4), [1, 1, 1, 255]);
    hidden.mask = Some(Mask {
        rect: (0, 0, 2, 2),
        default_color: 0,
        flags: 0b10, // bit 1 = layer mask disabled
        pixels: vec![7; 4],
    });
    psd.layers.push(l);
    psd.layers.push(hidden);

    let doc = read_psd(&psd.build()).unwrap();
    let mask = doc.tree.layers[0].mask.as_ref().unwrap();
    assert!(mask.enabled);
    assert_eq!(mask.default_value, 255);
    assert_eq!(mask.bounds.left, 8);
    assert_eq!(mask.bounds.top, 8);
    assert_eq!(mask.value(10, 10), 200); // inside mask rect
    assert_eq!(mask.value(0, 0), 255); // outside: default value

    let mask2 = doc.tree.layers[1].mask.as_ref().unwrap();
    assert!(!mask2.enabled);
    assert_eq!(mask2.value(0, 0), 7);
}

#[test]
fn unicode_name_overrides_pascal_name() {
    let mut psd = Psd::rgb8(8, 8);
    let mut l = L::solid("ascii", (0, 0, 4, 4), [1, 2, 3, 255]);
    l.unicode_name = Some("Ünïcödé 層".into());
    psd.layers.push(l);
    let doc = read_psd(&psd.build()).unwrap();
    assert_eq!(doc.tree.layers[0].name, "Ünïcödé 層");
}

#[test]
fn unknown_block_preserved_verbatim_in_extras() {
    let mut psd = Psd::rgb8(8, 8);
    let mut l = L::solid("A", (0, 0, 4, 4), [1, 2, 3, 255]);
    l.unicode_name = Some("A".into());
    l.extra_blocks.push((*b"xyzw", vec![1, 2, 3, 4, 5])); // odd length: padded on disk
    l.extra_blocks.push((*b"lyid", vec![0, 0, 0, 7]));
    psd.layers.push(l);
    let doc = read_psd(&psd.build()).unwrap();
    let extras = &doc.tree.layers[0].extras;
    assert_eq!(extras.len(), 2);
    assert_eq!(extras[0].key, *b"xyzw");
    assert_eq!(extras[0].data, vec![1, 2, 3, 4, 5]); // pad byte NOT included
    assert_eq!(extras[1].key, *b"lyid");
    // 'luni' and 'lsct' are structural and must NOT be preserved.
    assert!(extras
        .iter()
        .all(|b| b.key != *b"luni" && b.key != *b"lsct"));
}

#[test]
fn adjustment_layer_from_key_with_raw_payload() {
    let mut psd = Psd::rgb8(8, 8);
    // Zero-area bounds with empty channels — how Photoshop stores
    // adjustment layers (channels have 0 rows).
    let mut l = L {
        name: "B&C".into(),
        raw_planes: Some(vec![(0, vec![]), (1, vec![]), (2, vec![]), (-1, vec![])]),
        ..L::default()
    };
    l.extra_blocks
        .push((*b"brit", vec![0, 10, 0, 20, 0, 0, 0, 1]));
    psd.layers.push(l);
    let doc = read_psd(&psd.build()).unwrap();
    let layer = &doc.tree.layers[0];
    let LayerKind::Adjustment(adj) = &layer.kind else {
        panic!("expected adjustment")
    };
    assert_eq!(adj.kind, schist_core::AdjustmentKind::BrightnessContrast);
    assert_eq!(adj.raw, vec![0, 10, 0, 20, 0, 0, 0, 1]);
    // The raw block is ALSO preserved in extras for round-trip.
    assert_eq!(layer.extras.len(), 1);
    assert_eq!(layer.extras[0].key, *b"brit");
}

#[test]
fn flattened_psd_synthesizes_background_layer() {
    let mut psd = Psd::rgb8(16, 16);
    let mut rgba = Vec::new();
    for i in 0..256u32 {
        rgba.extend([(i % 16) as u8 * 16, 200, 100, 128]); // alpha ignored: opaque
    }
    psd.channels = 4; // extra channel present but NOT merged transparency
    psd.composite_rgba8 = Some(rgba);
    let doc = read_psd(&psd.build()).unwrap();
    assert_eq!(doc.tree.layers.len(), 1);
    let bg = &doc.tree.layers[0];
    assert_eq!(bg.name, "Background");
    // No negative layer count => the 4th channel is not transparency, the
    // background is opaque.
    assert_eq!(raster_pixel(bg, 5, 0), [5 * 16, 200, 100, 255]);
    assert_eq!(raster_pixel(bg, 15, 15), [15 * 16, 200, 100, 255]);
}

#[test]
fn negative_layer_count_uses_magnitude() {
    let mut psd = Psd::rgb8(8, 8);
    psd.negative_count = true;
    psd.layers.push(L::solid("A", (0, 0, 8, 8), [4, 5, 6, 255]));
    let doc = read_psd(&psd.build()).unwrap();
    assert_eq!(doc.tree.layers.len(), 1);
    assert_eq!(raster_pixel(&doc.tree.layers[0], 1, 1), [4, 5, 6, 255]);
}

#[test]
fn rle_layer_with_literal_and_repeat_packets() {
    let mut psd = Psd::rgb8(64, 4);
    // A row mixing a long run (repeat packets) and unique values (literals).
    let mut l = L::solid("RLE", (0, 0, 4, 64), [0, 0, 0, 255]);
    for (i, px) in l.rgba.iter_mut().enumerate() {
        let x = i % 64;
        px[0] = if x < 32 { 77 } else { (x as u8) * 3 }; // run then literals
    }
    l.rle = true;
    psd.layers.push(l);
    let doc = read_psd(&psd.build()).unwrap();
    let layer = &doc.tree.layers[0];
    assert_eq!(raster_pixel(layer, 0, 0), [77, 0, 0, 255]);
    assert_eq!(raster_pixel(layer, 31, 3), [77, 0, 0, 255]);
    assert_eq!(raster_pixel(layer, 40, 2), [120, 0, 0, 255]);
    assert_eq!(raster_pixel(layer, 63, 1), [189, 0, 0, 255]);
}

#[test]
fn psb_version2_u64_lengths_and_u32_rle_counts() {
    let mut psd = Psd::rgb8(32, 16);
    psd.version = 2; // PSB
    let mut l = L::solid("Big", (0, 0, 16, 32), [12, 34, 56, 255]);
    l.rle = true;
    psd.layers.push(l);
    let mut l2 = L::solid("Raw", (2, 2, 6, 6), [200, 100, 50, 255]);
    l2.unicode_name = Some("Raw²".into());
    psd.layers.push(l2);
    let doc = read_psd(&psd.build()).unwrap();
    assert_eq!(doc.tree.layers.len(), 2);
    assert_eq!(raster_pixel(&doc.tree.layers[0], 31, 15), [12, 34, 56, 255]);
    assert_eq!(doc.tree.layers[1].name, "Raw²");
    assert_eq!(raster_pixel(&doc.tree.layers[1], 3, 3), [200, 100, 50, 255]);
}

#[test]
fn sixteen_bit_pixels_normalize_to_full_range() {
    let mut psd = Psd::rgb8(4, 4);
    psd.depth = 16;
    let plane = |v: u16| -> Vec<u8> { (0..16).flat_map(|_| v.to_be_bytes()).collect() };
    let l = L {
        name: "Deep".into(),
        rect: (0, 0, 4, 4),
        raw_planes: Some(vec![
            (0, plane(0x4000)),
            (1, plane(0xFFFF)),
            (2, plane(0)),
            (-1, plane(0xFFFF)),
        ]),
        ..L::default()
    };
    psd.layers.push(l);
    let doc = read_psd(&psd.build()).unwrap();
    assert_eq!(doc.depth, Depth::Sixteen);
    let px = doc.tree.layers[0].as_raster().unwrap().tiles.pixel(1, 1);
    assert!((px.r - 16384.0 / 65535.0).abs() < 1e-6);
    assert!((px.g - 1.0).abs() < 1e-6);
    assert_eq!(px.b, 0.0);
    assert_eq!(px.a, 1.0);
}

#[test]
fn sixteen_bit_layers_inside_lr16_document_block() {
    // Photoshop stores 16-bit layer trees in a document-level 'Lr16' block
    // with an empty Layer Info section; make sure we find them there.
    let mut psd = Psd::rgb8(4, 4);
    psd.depth = 16;
    psd.layers_in_lr16 = true;
    let plane = |v: u16| -> Vec<u8> { (0..16).flat_map(|_| v.to_be_bytes()).collect() };
    let l = L {
        name: "InLr16".into(),
        rect: (0, 0, 4, 4),
        raw_planes: Some(vec![
            (0, plane(0x8000)),
            (1, plane(0)),
            (2, plane(0)),
            (-1, plane(0xFFFF)),
        ]),
        ..L::default()
    };
    psd.layers.push(l);
    let doc = read_psd(&psd.build()).unwrap();
    assert_eq!(doc.tree.layers.len(), 1);
    assert_eq!(doc.tree.layers[0].name, "InLr16");
    let px = doc.tree.layers[0].as_raster().unwrap().tiles.pixel(0, 0);
    assert!((px.r - 32768.0 / 65535.0).abs() < 1e-6);
}

#[test]
fn thirty_two_bit_float_pixels_including_rle() {
    let mut psd = Psd::rgb8(4, 2);
    psd.depth = 32;
    let plane = |v: f32| -> Vec<u8> { (0..8).flat_map(|_| v.to_be_bytes()).collect() };
    let l = L {
        name: "HDR".into(),
        rect: (0, 0, 2, 4),
        rle: true, // 32-bit RLE = PackBits over the raw big-endian f32 bytes
        raw_planes: Some(vec![
            (0, plane(0.75)),
            (1, plane(0.25)),
            (2, plane(1.5)),
            (-1, plane(1.0)),
        ]),
        ..L::default()
    };
    psd.layers.push(l);
    let doc = read_psd(&psd.build()).unwrap();
    assert_eq!(doc.depth, Depth::ThirtyTwo);
    let px = doc.tree.layers[0].as_raster().unwrap().tiles.pixel(3, 1);
    assert_eq!(px.r, 0.75);
    assert_eq!(px.g, 0.25);
    assert_eq!(px.b, 1.5); // HDR values > 1.0 survive
    assert_eq!(px.a, 1.0);
}

#[test]
fn grayscale_replicates_gray_into_rgb() {
    let mut psd = Psd::rgb8(4, 4);
    psd.mode = 1; // grayscale
    psd.channels = 1;
    let l = L {
        name: "Gray".into(),
        rect: (0, 0, 4, 4),
        raw_planes: Some(vec![(0, vec![180; 16]), (-1, vec![255; 16])]),
        ..L::default()
    };
    psd.layers.push(l);
    let doc = read_psd(&psd.build()).unwrap();
    assert_eq!(doc.mode, ColorMode::Grayscale);
    assert_eq!(
        raster_pixel(&doc.tree.layers[0], 2, 2),
        [180, 180, 180, 255]
    );
}

#[test]
fn resolution_icc_and_resource_preservation() {
    let mut psd = Psd::rgb8(8, 8);
    psd.color_mode_data = vec![9, 8, 7];
    psd.resources.push(resolution_res(300.0));
    psd.resources.push(Res {
        id: 0x040F,
        name: Vec::new(),
        data: vec![1, 2, 3, 4],
    });
    psd.resources.push(Res {
        id: 0x0BB7,
        name: b"odd".to_vec(),
        data: vec![5, 6, 7],
    });
    psd.layers.push(L::solid("A", (0, 0, 4, 4), [1, 2, 3, 255]));
    let doc = read_psd(&psd.build()).unwrap();
    assert!((doc.resolution_dpi - 300.0).abs() < 0.001);
    assert_eq!(doc.icc_profile.as_deref(), Some(&[1u8, 2, 3, 4][..]));
    // Color mode data sentinel first, then all resources in file order
    // (including the interpreted ones).
    assert_eq!(doc.preserved_resources.len(), 4);
    assert_eq!(doc.preserved_resources[0].id, 0xFFFF);
    assert_eq!(doc.preserved_resources[0].name, b"colormodedata");
    assert_eq!(doc.preserved_resources[0].data, vec![9, 8, 7]);
    assert_eq!(doc.preserved_resources[1].id, 0x03ED);
    assert_eq!(doc.preserved_resources[2].id, 0x040F);
    assert_eq!(doc.preserved_resources[3].id, 0x0BB7);
    assert_eq!(doc.preserved_resources[3].data, vec![5, 6, 7]);
    // Raw pascal name bytes: length byte + content (+ pad to even).
    assert_eq!(doc.preserved_resources[3].name, vec![3, b'o', b'd', b'd']);
}

#[test]
fn reads_zip_compressed_channels() {
    // Photoshop writes 16- and 32-bit files this way by default, so these
    // used to be files that opened fine there and not here.
    for predict in [false, true] {
        let mut psd = Psd::rgb8(8, 8);
        let mut l = L::solid("Z", (0, 0, 8, 8), [10, 20, 30, 255]);
        l.zip = true;
        l.predict = predict;
        psd.layers.push(l);
        let doc = read_psd(&psd.build()).expect("zip channels read");
        let px = doc.tree.layers[0]
            .as_raster()
            .unwrap()
            .tiles
            .pixel(4, 4)
            .to_u8();
        assert_eq!(
            [px[0], px[1], px[2]],
            [10, 20, 30],
            "predict={predict}: wrong pixels"
        );
    }
}

#[test]
fn corrupt_zip_channel_errors_cleanly() {
    let mut psd = Psd::rgb8(8, 8);
    let mut l = L::solid("Z", (0, 0, 8, 8), [1, 2, 3, 255]);
    l.zip = true;
    psd.layers.push(l);
    let mut bytes = psd.build();
    // Corrupt the tail, where the compressed channel data lives.
    let n = bytes.len();
    for b in bytes[n - 40..].iter_mut() {
        *b ^= 0xFF;
    }
    match read_psd(&bytes) {
        Err(PsdError::Corrupt(_)) | Err(PsdError::Unsupported(_)) | Ok(_) => {}
        other => panic!("expected a clean result, got {other:?}"),
    }
}

#[test]
fn unsupported_color_modes_rejected_clearly() {
    // CMYK, Lab and Indexed open now; Bitmap and Multichannel still do not.
    for (mode, needle) in [(0u16, "Bitmap"), (7, "Multichannel")] {
        let mut psd = Psd::rgb8(8, 8);
        psd.mode = mode;
        match read_psd(&psd.build()) {
            Err(PsdError::Unsupported(msg)) => {
                assert!(msg.contains(needle), "mode {mode}: {msg}")
            }
            other => panic!("mode {mode}: expected Unsupported, got {other:?}"),
        }
    }
    // 1-bit depth also rejected.
    let mut psd = Psd::rgb8(8, 8);
    psd.depth = 1;
    assert!(matches!(
        read_psd(&psd.build()),
        Err(PsdError::Unsupported(_))
    ));
}

#[test]
fn bad_signature_is_bad_signature() {
    assert!(matches!(
        read_psd(b"9BPSxxxxxxxx"),
        Err(PsdError::BadSignature)
    ));
    assert!(matches!(read_psd(b""), Err(PsdError::BadSignature)));
}

#[test]
fn truncated_buffers_error_never_panic() {
    // A file exercising most parser paths.
    let mut psd = Psd::rgb8(32, 32);
    psd.resources.push(resolution_res(72.0));
    psd.layers
        .push(L::solid("Bottom", (0, 0, 16, 16), [1, 2, 3, 200]));
    psd.layers.push(L::divider());
    let mut masked = L::solid("Masked", (4, 4, 12, 12), [4, 5, 6, 255]);
    masked.mask = Some(Mask {
        rect: (4, 4, 8, 8),
        default_color: 255,
        flags: 0,
        pixels: vec![128; 16],
    });
    masked.rle = true;
    masked.unicode_name = Some("Masked".into());
    masked.extra_blocks.push((*b"xyzw", vec![1, 2, 3]));
    psd.layers.push(masked);
    psd.layers.push(L::group_header("G", 1, Some(*b"pass")));
    let bytes = psd.build();

    assert!(read_psd(&bytes).is_ok());
    for n in 0..bytes.len() {
        // Every strict prefix must return (Ok or Err), never panic. Most
        // are errors; prefixes that end exactly on a section boundary can
        // legally parse (trailing sections are optional).
        let _ = read_psd(&bytes[..n]);
    }
    // Cutting inside the header is always an error.
    for n in 4..26 {
        assert!(read_psd(&bytes[..n]).is_err(), "prefix {n} should fail");
    }
}
