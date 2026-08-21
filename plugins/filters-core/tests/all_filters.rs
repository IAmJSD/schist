//! Properties every filter in the set has to hold, checked against all of
//! them at once so a new filter cannot quietly skip them.

use schist_plugin_api::{FilterValues, PluginManifest, PluginRegistry};

fn registry() -> PluginRegistry {
    let mut reg = PluginRegistry::default();
    schist_filters_core::CoreFiltersPlugin.register(&mut reg);
    reg
}

/// A small test image with a bit of everything: a gradient, a hard edge, a
/// transparent corner, saturated colour, and a few isolated speckles so
/// the noise-removal filters have something to remove.
fn image(w: usize, h: usize) -> Vec<f32> {
    let mut px = vec![0.0f32; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let i = (y * w + x) * 4;
            let t = x as f32 / w as f32;
            px[i] = if y < h / 2 { t } else { 0.9 };
            px[i + 1] = y as f32 / h as f32;
            px[i + 2] = if x < w / 3 { 0.1 } else { 0.8 };
            px[i + 3] = if x < 3 && y < 3 { 0.0 } else { 1.0 };
        }
    }
    // Speckle: single pixels well away from their neighbours.
    for (sx, sy) in [(7usize, 7usize), (13, 4), (5, 11), (11, 9)] {
        if sx < w && sy < h {
            let i = (sy * w + sx) * 4;
            px[i] = 0.0;
            px[i + 1] = 0.0;
            px[i + 2] = 0.0;
        }
    }
    px
}

#[test]
fn every_filter_is_registered_with_a_name_and_category() {
    let reg = registry();
    let mut ids = Vec::new();
    for f in reg.filters() {
        assert!(!f.name().is_empty(), "{} has no name", f.id());
        assert!(!f.category().is_empty(), "{} has no category", f.id());
        ids.push(f.id());
    }
    ids.sort();
    let before = ids.len();
    ids.dedup();
    assert_eq!(before, ids.len(), "duplicate filter ids");
    assert!(before >= 40, "expected the full filter set, found {before}");
}

#[test]
fn every_filter_leaves_the_buffer_finite_and_in_range() {
    let (w, h) = (33usize, 21usize);
    for f in registry().filters() {
        let mut px = image(w, h);
        let values = FilterValues::defaults(&f.params());
        f.apply(&mut px, w, h, &values);
        assert_eq!(px.len(), w * h * 4, "{} resized the buffer", f.id());
        for (i, v) in px.iter().enumerate() {
            assert!(v.is_finite(), "{} produced {v} at index {i}", f.id());
            assert!(
                (-0.001..=1.001).contains(v),
                "{} produced {v} outside 0..=1 at index {i}",
                f.id()
            );
        }
    }
}

#[test]
fn every_filter_survives_degenerate_sizes() {
    // A one-pixel image and a zero-sized one are both reachable through a
    // small selection, so neither may panic or index out of bounds.
    for f in registry().filters() {
        let values = FilterValues::defaults(&f.params());
        let mut one = vec![0.5f32, 0.5, 0.5, 1.0];
        f.apply(&mut one, 1, 1, &values);
        let mut none: Vec<f32> = Vec::new();
        f.apply(&mut none, 0, 0, &values);
        let mut thin = vec![0.5f32; 4 * 5];
        f.apply(&mut thin, 5, 1, &values);
        let mut tall = vec![0.5f32; 4 * 5];
        f.apply(&mut tall, 1, 5, &values);
    }
}

#[test]
fn every_filter_does_something_at_its_defaults() {
    // A filter whose default settings are a no-op is almost always a bug.
    // Two are not: Offset defaults to no shift and Camera Raw to a
    // neutral development, exactly as Photoshop's do -- opening either
    // and touching nothing should leave the image alone.
    const EXPECTED_NO_OPS: &[&str] = &["filter.offset", "filter.camera_raw"];
    let (w, h) = (33usize, 21usize);
    for f in registry().filters() {
        let before = image(w, h);
        let mut px = before.clone();
        let values = FilterValues::defaults(&f.params());
        f.apply(&mut px, w, h, &values);
        let changed = px
            .iter()
            .zip(before.iter())
            .any(|(a, b)| (a - b).abs() > 1e-4);
        if EXPECTED_NO_OPS.contains(&f.id()) {
            assert!(!changed, "{} was expected to be a no-op", f.id());
        } else {
            assert!(changed, "{} did nothing at its defaults", f.id());
        }
    }
}

#[test]
fn every_filter_is_deterministic() {
    // Two runs with the same input must agree, or previews would flicker
    // and the committed result would not match what was previewed.
    let (w, h) = (17usize, 13usize);
    for f in registry().filters() {
        let values = FilterValues::defaults(&f.params());
        let mut a = image(w, h);
        let mut b = image(w, h);
        f.apply(&mut a, w, h, &values);
        f.apply(&mut b, w, h, &values);
        assert_eq!(a, b, "{} is not deterministic", f.id());
    }
}

#[test]
fn blurs_and_noise_reduction_lower_local_contrast() {
    let (w, h) = (41usize, 41usize);
    let contrast = |px: &[f32]| {
        let mut sum = 0.0;
        for y in 0..h {
            for x in 1..w {
                let i = (y * w + x) * 4;
                sum += (px[i] - px[i - 4]).abs();
            }
        }
        sum
    };
    let reg = registry();
    for id in [
        "filter.gaussian_blur",
        "filter.box_blur",
        "filter.lens_blur",
        "filter.surface_blur",
        "filter.average",
        "filter.reduce_noise",
    ] {
        let f = reg.filters().find(|f| f.id() == id).expect(id);
        let before = image(w, h);
        let mut px = before.clone();
        f.apply(&mut px, w, h, &FilterValues::defaults(&f.params()));
        assert!(
            contrast(&px) < contrast(&before),
            "{id} did not reduce local contrast"
        );
    }
}

#[test]
fn sharpeners_raise_local_contrast() {
    let (w, h) = (41usize, 41usize);
    let contrast = |px: &[f32]| {
        let mut sum = 0.0;
        for y in 0..h {
            for x in 1..w {
                let i = (y * w + x) * 4;
                sum += (px[i] - px[i - 4]).abs();
            }
        }
        sum
    };
    let reg = registry();
    for id in [
        "filter.sharpen",
        "filter.unsharp_mask",
        "filter.smart_sharpen",
    ] {
        let f = reg.filters().find(|f| f.id() == id).expect(id);
        let before = image(w, h);
        let mut px = before.clone();
        f.apply(&mut px, w, h, &FilterValues::defaults(&f.params()));
        assert!(
            contrast(&px) >= contrast(&before),
            "{id} did not raise local contrast"
        );
    }
}

#[test]
fn maximum_and_minimum_are_opposites() {
    let (w, h) = (21usize, 21usize);
    let reg = registry();
    let max = reg.filters().find(|f| f.id() == "filter.maximum").unwrap();
    let min = reg.filters().find(|f| f.id() == "filter.minimum").unwrap();
    let before = image(w, h);
    let mean = |px: &[f32]| px.chunks_exact(4).map(|p| p[0]).sum::<f32>();

    let mut grown = before.clone();
    max.apply(&mut grown, w, h, &FilterValues::defaults(&max.params()));
    let mut shrunk = before.clone();
    min.apply(&mut shrunk, w, h, &FilterValues::defaults(&min.params()));

    assert!(mean(&grown) > mean(&before), "Maximum did not grow lights");
    assert!(mean(&shrunk) < mean(&before), "Minimum did not grow darks");
}

#[test]
fn offset_wraps_the_image_around() {
    let (w, h) = (8usize, 4usize);
    let reg = registry();
    let f = reg.filters().find(|f| f.id() == "filter.offset").unwrap();
    let mut px = vec![0.0f32; w * h * 4];
    // Mark the top-left pixel.
    px[0] = 1.0;
    px[3] = 1.0;
    let mut values = FilterValues::defaults(&f.params());
    values.set("x", 3.0);
    values.set("y", 1.0);
    f.apply(&mut px, w, h, &values);
    let i = (w + 3) * 4;
    assert_eq!(px[i], 1.0, "the marked pixel did not move to (3, 1)");
    assert_eq!(px[0], 0.0, "the original position was not vacated");
}
