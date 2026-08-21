//! Real-world fixture corpus (fixtures/psd/). The ImageMagick-generated
//! files there were produced with `convert` (see names starting `im_`).
//! The test SKIPS (does not fail) when the directory is missing or empty so
//! fresh checkouts without fixtures still pass.

use schist_codec_psd::{is_psd, read_psd};
use schist_core::Layer;
use std::path::PathBuf;

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/psd")
}

fn fixture_files() -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(fixtures_dir()) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = rd
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "psd" || e == "psb"))
        .collect();
    files.sort();
    files
}

fn pixel(layer: &Layer, x: i32, y: i32) -> [u8; 4] {
    layer.as_raster().unwrap().tiles.pixel(x, y).to_u8()
}

#[test]
fn all_fixture_psds_open() {
    let files = fixture_files();
    if files.is_empty() {
        eprintln!("skipping: no fixtures in {:?}", fixtures_dir());
        return;
    }
    for path in files {
        let bytes = std::fs::read(&path).unwrap();
        assert!(is_psd(&bytes), "{path:?} lacks 8BPS signature");
        let doc =
            read_psd(&bytes).unwrap_or_else(|e| panic!("failed to open fixture {path:?}: {e}"));
        assert!(doc.width > 0 && doc.height > 0, "{path:?} has empty canvas");
        assert!(!doc.tree.layers.is_empty(), "{path:?} produced no layers");
    }
}

#[test]
fn imagemagick_two_layers_pixel_colors() {
    let path = fixtures_dir().join("im_two_layers.psd");
    if !path.exists() {
        eprintln!("skipping: {path:?} not present");
        return;
    }
    // Built with: convert ( -size 64x64 xc:red ( -size 32x32 xc:blue )
    //   -geometry +8+8 -composite ) ( -size 64x64 xc:red )
    //   ( -size 32x32 xc:blue -set page +8+8 ) -depth 8 im_two_layers.psd
    let doc = read_psd(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!((doc.width, doc.height), (64, 64));
    assert_eq!(doc.tree.layers.len(), 2);
    // Bottom-to-top order: red base first, blue square (at +8+8) on top.
    let red = &doc.tree.layers[0];
    let blue = &doc.tree.layers[1];
    assert_eq!(pixel(red, 0, 0), [255, 0, 0, 255]);
    assert_eq!(pixel(red, 63, 63), [255, 0, 0, 255]);
    assert_eq!(pixel(blue, 8, 8), [0, 0, 255, 255]);
    assert_eq!(pixel(blue, 39, 39), [0, 0, 255, 255]);
    assert_eq!(pixel(blue, 4, 4), [0, 0, 0, 0]); // outside the offset rect
}

#[test]
fn imagemagick_flat_gradient_is_full_canvas() {
    let path = fixtures_dir().join("im_flat_gradient.psd");
    if !path.exists() {
        eprintln!("skipping: {path:?} not present");
        return;
    }
    // convert -size 64x64 gradient:red-blue -depth 8 -compress RLE ...
    let doc = read_psd(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(doc.tree.layers.len(), 1);
    let l = &doc.tree.layers[0];
    let top = pixel(l, 32, 0);
    let bottom = pixel(l, 32, 63);
    assert!(
        top[0] > 200 && top[2] < 50,
        "top should be red-ish: {top:?}"
    );
    assert!(
        bottom[2] > 200 && bottom[0] < 50,
        "bottom should be blue-ish: {bottom:?}"
    );
}

#[test]
fn imagemagick_grayscale_gradient() {
    let path = fixtures_dir().join("im_gray_flat.psd");
    if !path.exists() {
        eprintln!("skipping: {path:?} not present");
        return;
    }
    let doc = read_psd(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(doc.mode, schist_color::ColorMode::Grayscale);
    let l = &doc.tree.layers[0];
    let px = pixel(l, 10, 0);
    // Gray replicated into RGB.
    assert_eq!(px[0], px[1]);
    assert_eq!(px[1], px[2]);
    let dark = pixel(l, 16, 0);
    let light = pixel(l, 16, 31);
    assert!(
        dark[0] < 30 && light[0] > 225,
        "gradient ends: {dark:?} {light:?}"
    );
}
