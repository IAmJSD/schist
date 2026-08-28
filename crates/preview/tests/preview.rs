//! Previews of real files, over the vendored fixtures.
//!
//! Following the codec convention, these skip (pass) when a corpus is
//! missing so a partial checkout still builds green.

use schist_preview::{render, render_file, Source, MAX_EDGE};
use std::path::PathBuf;

fn fixture(rel: &str) -> Option<PathBuf> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .join(rel);
    if path.exists() {
        Some(path)
    } else {
        eprintln!("skipping: no fixture at {}", path.display());
        None
    }
}

/// Not all zero, not all one colour: a real picture came out.
fn has_content(rgba: &[u8]) -> bool {
    let pixels = rgba.as_chunks::<4>().0;
    pixels.iter().any(|px| *px != pixels[0])
}

#[test]
fn an_affinity_file_previews_from_its_embedded_thumbnail() {
    let Some(path) = fixture("affinity-probe/shp_pie.af") else {
        return;
    };
    // The probe fixtures are 512x512 and Affinity embeds a thumbnail of
    // the same size, so anything smaller is served from it directly.
    let preview = render_file(&path, 128).unwrap();
    assert_eq!(preview.source, Source::Embedded);
    assert_eq!((preview.width, preview.height), (128, 128));
    assert_eq!(preview.rgba.len(), 128 * 128 * 4);
    assert!(has_content(&preview.rgba));
}

#[test]
fn a_bigger_request_than_the_thumbnail_composites_the_document() {
    let Some(path) = fixture("affinity-probe/shp_pie.af") else {
        return;
    };
    let preview = render_file(&path, MAX_EDGE).unwrap();
    assert_eq!(preview.source, Source::Composited);
    // Never upscaled past the document's own size.
    assert_eq!((preview.width, preview.height), (512, 512));
    assert!(has_content(&preview.rgba));
}

#[test]
fn a_psd_without_a_thumbnail_resource_composites() {
    let Some(path) = fixture("psd/im_two_layers.psd") else {
        return;
    };
    let preview = render_file(&path, 64).unwrap();
    assert_eq!(preview.source, Source::Composited);
    assert!(preview.width.max(preview.height) <= 64);
    assert!(has_content(&preview.rgba));
    // PNG round trip: what the Quick Look extensions actually hand over.
    let png = preview.to_png().unwrap();
    let decoded = image::load_from_memory(&png).unwrap().into_rgba8();
    assert_eq!(
        (decoded.width(), decoded.height()),
        (preview.width, preview.height)
    );
}

#[test]
fn a_plain_image_decodes_and_scales() {
    // A 64x32 checkerboard, encoded as PNG.
    let mut img = image::RgbaImage::new(64, 32);
    for (x, y, px) in img.enumerate_pixels_mut() {
        *px = if (x + y) % 2 == 0 {
            image::Rgba([255, 255, 255, 255])
        } else {
            image::Rgba([0, 0, 0, 255])
        };
    }
    let mut png = std::io::Cursor::new(Vec::new());
    img.write_to(&mut png, image::ImageFormat::Png).unwrap();

    let preview = render(png.get_ref(), 512).unwrap();
    assert_eq!(preview.source, Source::Decoded);
    // Under the requested size, so it is left alone rather than blown up.
    assert_eq!((preview.width, preview.height), (64, 32));

    let preview = render(png.get_ref(), 16).unwrap();
    assert_eq!((preview.width, preview.height), (16, 8));

    // Sizes below the floor are raised to it, not honoured.
    let preview = render(png.get_ref(), 2).unwrap();
    assert_eq!((preview.width, preview.height), (16, 8));
}

#[test]
fn an_unreadable_file_is_an_error_not_a_panic() {
    assert!(render(b"", 256).is_err());
    assert!(render(b"nothing recognizable here", 256).is_err());
    // A PSD signature with nothing behind it.
    assert!(render(b"8BPS\x00\x01rubbish", 256).is_err());
}
