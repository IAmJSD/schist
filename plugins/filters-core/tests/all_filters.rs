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
    const EXPECTED_NO_OPS: &[&str] = &[
        "filter.offset",
        "filter.camera_raw",
        // Custom starts as the identity kernel, exactly as Photoshop's
        // does: it is a kernel editor, and the kernel it opens with is
        // "leave this alone".
        "filter.custom",
        // These three need something this image does not have and no
        // slider can supply: a layer underneath to match against, or a
        // face to work on. Photoshop greys the first two out until there
        // is a layer below; doing nothing is the same answer.
        "filter.neural.harmonization",
        "filter.neural.landscape_mixer",
        "filter.neural.face_to_caricature",
        "filter.neural.makeup_transfer",
        // Smart Portrait opens with every expression at neutral and the
        // light where it already is, because the picture it was given is
        // the picture the photographer took: it is a set of adjustments,
        // and none of them start applied.
        "filter.neural.smart_portrait",
        // Lens Correction opens with every correction at zero, because
        // there is no lens profile to prefill it from and guessing would
        // be worse than leaving it alone. Photoshop's does the same
        // without a profile.
        "filter.lens_correction",
    ];
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
        let mut values = FilterValues::defaults(&f.params());
        // Reduce Noise has two stages after the smoothing that are not
        // smoothing: it sharpens details back up (Photoshop's own default
        // is 25%) and it rebuilds each channel from the pixel's own
        // luminance plus smoothed chroma, which can raise the contrast of
        // any single channel while lowering the picture's. What is being
        // checked here is the smoothing, so those two are turned off
        // rather than the defaults being changed to suit the test.
        values.set("sharpen", 0.0);
        values.set("colour", 0.0);
        f.apply(&mut px, w, h, &values);
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
    let mean = |px: &[f32]| px.as_chunks::<4>().0.iter().map(|p| p[0]).sum::<f32>();

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

#[test]
fn colorize_puts_colour_into_something_that_has_none() {
    // Whichever path it takes -- the network or the luminance ramp -- the
    // one thing Colorize must do is leave a greyscale image not grey,
    // without moving its luminance while it does.
    let (w, h) = (64usize, 64usize);
    let mut px = vec![0.0f32; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let i = (y * w + x) * 4;
            // A ramp with a disc cut through it, so there is something to
            // recognise as well as something to shade.
            let disc = ((x as f32 - 32.0).hypot(y as f32 - 32.0)) < 18.0;
            let v = if disc { 0.7 } else { x as f32 / w as f32 };
            px[i] = v;
            px[i + 1] = v;
            px[i + 2] = v;
            px[i + 3] = 1.0;
        }
    }
    let before = px.clone();
    let reg = registry();
    let f = reg
        .filters()
        .find(|f| f.id() == "filter.neural.colorize")
        .expect("colorize");
    f.apply(&mut px, w, h, &FilterValues::defaults(&f.params()));

    let luma = |p: &[f32]| 0.299 * p[0] + 0.587 * p[1] + 0.114 * p[2];
    let mut coloured = 0;
    for (a, b) in px.as_chunks::<4>().0.iter().zip(before.as_chunks::<4>().0) {
        let spread = a[0].max(a[1]).max(a[2]) - a[0].min(a[1]).min(a[2]);
        if spread > 0.01 {
            coloured += 1;
        }
        assert!(
            (luma(a) - luma(b)).abs() < 0.05,
            "the luminance moved: {:.3} -> {:.3}",
            luma(b),
            luma(a)
        );
    }
    assert!(
        coloured > w * h / 4,
        "only {coloured} of {} pixels got any colour",
        w * h
    );
}

#[test]
fn skin_smoothing_leaves_things_that_are_not_skin_alone() {
    // The filter is gated twice over -- on faces when the model is
    // installed, and on skin colour either way -- so a blue-green image
    // has to come back untouched however it is run.
    let (w, h) = (48usize, 48usize);
    let mut px = vec![0.0f32; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let i = (y * w + x) * 4;
            px[i] = 0.1;
            px[i + 1] = 0.4 + 0.4 * ((x / 4) % 2) as f32;
            px[i + 2] = 0.8;
            px[i + 3] = 1.0;
        }
    }
    let before = px.clone();
    let reg = registry();
    let f = reg
        .filters()
        .find(|f| f.id() == "filter.neural.skin_smoothing")
        .expect("skin smoothing");
    f.apply(&mut px, w, h, &FilterValues::defaults(&f.params()));
    assert_eq!(px, before, "it smoothed something that was not skin");
}

#[test]
fn the_filters_that_ask_for_a_backdrop_use_it() {
    // The filters that declare they want the pixels underneath each have
    // to do nothing without one -- checked by the no-op list above -- and
    // move the image towards the backdrop's colour with one.
    //
    // Makeup Transfer is the exception, and not because it ignores the
    // backdrop: it needs a *face* in both pictures, and neither this
    // image nor a flat colour has one. What it does in that case is
    // checked below instead.
    const NEEDS_A_FACE: &[&str] = &["filter.neural.makeup_transfer"];
    let (w, h) = (48usize, 48usize);
    let warm: Vec<f32> = (0..w * h).flat_map(|_| [0.85f32, 0.55, 0.2, 1.0]).collect();
    let mean = |px: &[f32]| {
        let n = (px.len() / 4) as f32;
        let mut acc = [0.0f32; 3];
        for p in px.as_chunks::<4>().0 {
            for c in 0..3 {
                acc[c] += p[c] / n;
            }
        }
        acc
    };
    let reg = registry();
    let mut asked = 0;
    for f in reg
        .filters()
        .filter(|f| f.wants_backdrop() && !NEEDS_A_FACE.contains(&f.id()))
    {
        asked += 1;
        let before = image(w, h);
        let mut px = before.clone();
        let values = FilterValues::defaults(&f.params());
        let context = schist_plugin_api::FilterContext {
            backdrop: Some(&warm),
            ..Default::default()
        };
        f.apply_with(&mut px, w, h, &values, &context);
        let (was, now, want) = (mean(&before), mean(&px), mean(&warm));
        for c in 0..3 {
            let closer = (now[c] - want[c]).abs() < (was[c] - want[c]).abs();
            assert!(
                closer,
                "{} did not move channel {c} towards the backdrop: {} -> {} (backdrop {})",
                f.id(),
                was[c],
                now[c],
                want[c]
            );
        }
    }
    assert!(
        asked >= 3,
        "expected the three matching filters, found {asked}"
    );
}

#[test]
fn makeup_transfer_leaves_a_picture_with_no_face_in_it_alone() {
    // It moves colour from one face to another, so with no face to move
    // it to the answer is the picture it was given. Getting this wrong
    // would tint whole layers the colour of whatever sat underneath --
    // and a flat backdrop is exactly the case where a filter that
    // averaged first and looked for a face second would show it.
    let (w, h) = (48usize, 48usize);
    let warm: Vec<f32> = (0..w * h).flat_map(|_| [0.85f32, 0.55, 0.2, 1.0]).collect();
    let before = image(w, h);
    let mut px = before.clone();
    let reg = registry();
    let f = reg
        .filters()
        .find(|f| f.id() == "filter.neural.makeup_transfer")
        .expect("makeup transfer");
    let context = schist_plugin_api::FilterContext {
        backdrop: Some(&warm),
        ..Default::default()
    };
    f.apply_with(
        &mut px,
        w,
        h,
        &FilterValues::defaults(&f.params()),
        &context,
    );
    assert_eq!(px, before, "it made up something that was not a face");
}

#[test]
fn the_two_tone_filters_draw_in_the_colours_they_are_given() {
    // Photoshop's Sketch group renders in the foreground and background
    // colours, and Clouds and Fibers render *between* them. With the
    // swatches at their defaults that is black on white, which is why
    // the difference is invisible until somebody changes them -- so the
    // check is that a filter handed two unmistakable colours produces
    // neither black nor white.
    let (w, h) = (32usize, 32usize);
    let context = schist_plugin_api::FilterContext {
        foreground: schist_color::Rgba {
            r: 0.8,
            g: 0.1,
            b: 0.1,
            a: 1.0,
        },
        background: schist_color::Rgba {
            r: 0.1,
            g: 0.2,
            b: 0.7,
            a: 1.0,
        },
        ..Default::default()
    };
    let reg = registry();
    let mut checked = 0;
    for f in reg
        .filters()
        .filter(|f| f.category() == "Sketch" || matches!(f.id(), "filter.clouds" | "filter.fibers"))
    {
        // Water Paper keeps the photograph's own colour, as Photoshop's
        // does; it is in the Sketch menu but is not a two-tone filter.
        if f.id() == "filter.water_paper" {
            continue;
        }
        checked += 1;
        let mut px = image(w, h);
        let values = FilterValues::defaults(&f.params());
        f.apply_with(&mut px, w, h, &values, &context);
        // Every pixel should sit between the two colours, so nothing
        // should be more green than either of them is.
        let greenest = px
            .as_chunks::<4>()
            .0
            .iter()
            .map(|p| p[1])
            .fold(0.0f32, f32::max);
        assert!(
            greenest <= 0.45,
            "{} ignored the colours it was given: greenest channel {greenest:.2}",
            f.id()
        );
    }
    assert!(checked >= 14, "expected the two-tone set, found {checked}");
}

#[test]
fn displace_uses_the_map_it_is_given() {
    // A map that is mid grey on the left and hard right on the red
    // channel should leave the left alone and shove the right sideways.
    let (w, h) = (64usize, 32usize);
    let map = schist_plugin_api::FilterImage {
        width: w,
        height: h,
        pixels: (0..w * h)
            .flat_map(|i| {
                let right = (i % w) > w / 2;
                [if right { 1.0f32 } else { 0.5 }, 0.5, 0.5, 1.0]
            })
            .collect(),
    };
    let reg = registry();
    let f = reg
        .filters()
        .find(|f| f.id() == "filter.displace")
        .expect("displace");
    let before = image(w, h);
    let mut px = before.clone();
    let mut values = FilterValues::defaults(&f.params());
    values.set("scale", 20.0);
    values.set("vscale", 0.0);
    let context = schist_plugin_api::FilterContext {
        map: Some(&map),
        ..Default::default()
    };
    f.apply_with(&mut px, w, h, &values, &context);

    let moved = |x: usize| {
        (0..h)
            .map(|y| {
                let i = (y * w + x) * 4;
                (px[i] - before[i]).abs()
            })
            .sum::<f32>()
    };
    assert!(
        moved(w / 4) < 1e-3,
        "mid grey in the map should mean no movement"
    );
    assert!(moved(w * 3 / 4) > 0.1, "the map's push was ignored");
}
