use schist_document::{export, import, materialize, registry, SharedDocument};
use schist_plugin_api::PluginManifest;
use std::path::Path;
fn check(bytes: &[u8], name: &str) {
    let codecs = registry();
    let original = import(&codecs, bytes, name).unwrap();
    let seed = SharedDocument::new(&original).unwrap().full_state();
    let independent = import(&codecs, bytes, name).unwrap();
    assert_eq!(
        seed,
        SharedDocument::new(&independent).unwrap().full_state(),
        "bootstrap must be deterministic: {name}"
    );
    let restored = materialize(&seed).unwrap();
    assert_eq!(
        (original.width, original.height, original.depth),
        (restored.width, restored.height, restored.depth)
    );
    assert_eq!(original.tree.layers.len(), restored.tree.layers.len());
    for (a, b) in original.tree.layers.iter().zip(&restored.tree.layers) {
        assert_eq!(a.name, b.name);
        assert_eq!(a.opacity, b.opacity);
        assert_eq!(a.visible, b.visible);
        if let (Some(a), Some(b)) = (a.as_raster(), b.as_raster()) {
            assert_eq!(a.tiles.pixel(0, 0), b.tiles.pixel(0, 0));
        }
    }
    let editable = export(&codecs, &restored, "psd").unwrap();
    let reopened = import(&codecs, &editable.bytes, "edited.psd").unwrap();
    assert_eq!(
        (reopened.width, reopened.height),
        (original.width, original.height)
    );
}
#[test]
fn all_builtin_registrations_match_the_desktop() {
    let actual = registry();
    let mut desktop = schist_plugin_api::PluginRegistry::new();
    schist_codecs_common::CommonCodecsPlugin.register(&mut desktop);
    schist_codecs_common::PsdPlugin.register(&mut desktop);
    assert_eq!(
        actual
            .codecs()
            .map(|c| (c.id(), c.extensions()))
            .collect::<Vec<_>>(),
        desktop
            .codecs()
            .map(|c| (c.id(), c.extensions()))
            .collect::<Vec<_>>()
    );
}
#[test]
fn common_raster_formats_share_the_same_model() {
    for (format, name) in [
        (image::ImageFormat::Png, "color.png"),
        (image::ImageFormat::Jpeg, "color.jpg"),
        (image::ImageFormat::WebP, "color.webp"),
        (image::ImageFormat::Tiff, "color.tiff"),
    ] {
        let img = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            8,
            6,
            image::Rgb([20, 100, 200]),
        ));
        let mut bytes = std::io::Cursor::new(Vec::new());
        img.write_to(&mut bytes, format).unwrap();
        check(bytes.get_ref(), name);
    }
}
#[test]
fn layered_psd_and_affinity_fixtures_roundtrip() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures");
    for relative in [
        "psd/im_two_layers.psd",
        "psd/im_three_layers_rle.psd",
        "psd/im_gray_flat.psd",
        "affinity/color.afdesign",
        "affinity/raster_test.afdesign",
    ] {
        let path = root.join(relative);
        check(
            &std::fs::read(&path).unwrap(),
            path.file_name().unwrap().to_str().unwrap(),
        );
    }
}
#[test]
fn deep_raster_precision_survives_the_shared_model() {
    let img = image::DynamicImage::ImageRgba16(image::ImageBuffer::from_pixel(
        8,
        6,
        image::Rgba([0x0180u16, 0x8000, 0xffff, 0xffff]),
    ));
    let mut bytes = std::io::Cursor::new(Vec::new());
    img.write_to(&mut bytes, image::ImageFormat::Png).unwrap();
    check(bytes.get_ref(), "deep.png");
}
#[test]
fn heic_uses_the_same_optional_decoder_as_desktop() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/heif/rgb.heic");
    let bytes = std::fs::read(path).unwrap();
    match import(&registry(), &bytes, "rgb.heic") {
        Err(e) if schist_codecs_common::heif::no_decoder_available(&e) => {
            eprintln!("HEIC runtime unavailable: {e}")
        }
        Err(e) => panic!("{e:#}"),
        Ok(_) => check(&bytes, "rgb.heic"),
    }
}
