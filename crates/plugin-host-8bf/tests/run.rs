//! End to end: load a filter plug-in, run it, check the pixels.
//!
//! The fixture is a native shared library rather than a `.8bf`, because
//! the ABI is identical — the record is fixed-width and `extern "C"` is
//! one calling convention on both x86-64 targets — and that lets the
//! whole selector sequence, `advanceState`, the handle suite and the
//! pixel marshalling be exercised on a Linux CI box. What it does *not*
//! cover is loading a real PE plug-in, which needs Windows or stage 3's
//! Wine helper.

mod common;

use common::PiplBuilder;
use schist_plugin_host_8bf as bf;
use schist_plugin_host_8bf::abi::fourcc;
use schist_plugin_host_8bf::pipl::{key, kind, Endian, Pipl};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

fn pipl_for(entry: &str) -> Pipl {
    let bytes = PiplBuilder::new()
        .ostype(key::KIND, kind::FILTER)
        .i32(key::VERSION, 4 << 16)
        .ostype(key::REQUIRED_HOST, fourcc(b"8BIM"))
        .pstring(key::NAME, "Invert")
        .pstring(key::CATEGORY, "Schist")
        .raw(key::SUPPORTED_MODES, vec![0b0101_0000, 0])
        .cstring(key::CODE_WIN64_X86, entry)
        // All seven cases filterable, which is what a real plug-in
        // that handles layers and selections declares.
        .raw(key::FILTER_CASE_INFO, [1u8, 1, 0, 0].repeat(7))
        .build();
    Pipl::parse(&bytes, Endian::Little).unwrap()
}

/// A gradient, so a wrong stride or plane order shows up as garbage
/// rather than as a plausible-looking flat colour.
fn gradient(width: u32, height: u32, planes: u16) -> bf::Image {
    let mut img = bf::Image::new(width, height, planes);
    for y in 0..height {
        for x in 0..width {
            let i = (y as usize * width as usize + x as usize) * planes as usize;
            for p in 0..planes as usize {
                img.data[i + p] = ((x * 7 + y * 3 + p as u32 * 11) % 256) as u8;
            }
        }
    }
    img
}

fn load(entry: &str, dir: &Path) -> Option<bf::Filter> {
    let so = common::build_native_plugin(dir)?;
    Some(bf::Filter::open(&so, pipl_for(entry), entry).expect("fixture should load"))
}

#[test]
fn advance_state_drives_the_whole_filter_from_start() {
    let dir = tempfile::tempdir().unwrap();
    let Some(mut filter) = load("entry_advance", dir.path()) else {
        return;
    };
    assert_eq!(filter.name(), "Invert");

    let original = gradient(100, 70, 3);
    let mut image = original.clone();
    filter
        .apply(&mut image, &bf::RunOptions::default())
        .unwrap();

    let expected: Vec<u8> = original.data.iter().map(|&b| 255 - b).collect();
    assert_eq!(image.data, expected, "every pixel should be inverted");
}

#[test]
fn the_continue_loop_reaches_the_same_answer() {
    let dir = tempfile::tempdir().unwrap();
    let Some(mut filter) = load("entry_continue", dir.path()) else {
        return;
    };

    // Deliberately not a multiple of the plug-in's 32-pixel tile, so the
    // partial tiles at the right and bottom edges are covered.
    let original = gradient(100, 70, 3);
    let mut image = original.clone();
    filter
        .apply(&mut image, &bf::RunOptions::default())
        .unwrap();

    let expected: Vec<u8> = original.data.iter().map(|&b| 255 - b).collect();
    assert_eq!(image.data, expected);
}

#[test]
fn a_single_pixel_image_is_not_a_special_case() {
    let dir = tempfile::tempdir().unwrap();
    let Some(mut filter) = load("entry_advance", dir.path()) else {
        return;
    };
    let mut image = bf::Image::new(1, 1, 3);
    image.data = vec![10, 20, 30];
    filter
        .apply(&mut image, &bf::RunOptions::default())
        .unwrap();
    assert_eq!(image.data, vec![245, 235, 225]);
}

#[test]
fn greyscale_goes_through_the_same_path() {
    let dir = tempfile::tempdir().unwrap();
    let Some(mut filter) = load("entry_advance", dir.path()) else {
        return;
    };
    let original = gradient(33, 33, 1);
    let mut image = original.clone();
    filter
        .apply(&mut image, &bf::RunOptions::default())
        .unwrap();
    let expected: Vec<u8> = original.data.iter().map(|&b| 255 - b).collect();
    assert_eq!(image.data, expected);
}

#[test]
fn skipping_the_dialog_leaves_the_parameters_handle_unallocated() {
    let dir = tempfile::tempdir().unwrap();
    let Some(mut filter) = load("entry_advance", dir.path()) else {
        return;
    };
    // This is Photoshop's "Last Filter" path: no filterSelectorParameters
    // call, so the plug-in has to cope with a null parameters handle.
    let original = gradient(40, 40, 3);
    let mut image = original.clone();
    let opts = bf::RunOptions {
        show_dialog: false,
        ..Default::default()
    };
    filter.apply(&mut image, &opts).unwrap();
    let expected: Vec<u8> = original.data.iter().map(|&b| 255 - b).collect();
    assert_eq!(image.data, expected);
}

#[test]
fn progress_is_reported_through_the_callback() {
    let dir = tempfile::tempdir().unwrap();
    let Some(mut filter) = load("entry_advance", dir.path()) else {
        return;
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&calls);
    let opts = bf::RunOptions {
        progress: Some(Box::new(move |done, total| {
            assert!(total > 0 && done <= total, "progress {done}/{total}");
            seen.fetch_add(1, Ordering::Relaxed);
        })),
        ..Default::default()
    };
    let mut image = gradient(100, 70, 3);
    filter.apply(&mut image, &opts).unwrap();
    assert!(
        calls.load(Ordering::Relaxed) > 1,
        "expected several updates"
    );
}

#[test]
fn the_abort_flag_stops_the_filter() {
    let dir = tempfile::tempdir().unwrap();
    let Some(mut filter) = load("entry_advance", dir.path()) else {
        return;
    };
    let abort = Arc::new(AtomicBool::new(true));
    let opts = bf::RunOptions {
        abort,
        ..Default::default()
    };
    let original = gradient(100, 70, 3);
    let mut image = original.clone();
    let err = filter.apply(&mut image, &opts).unwrap_err();
    assert!(matches!(err, bf::HostError::Cancelled), "got {err}");
    assert_eq!(image.data, original.data, "a cancelled run must not edit");
}

#[test]
fn a_run_that_fails_partway_leaves_the_image_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let Some(mut filter) = load("entry_fail_midway", dir.path()) else {
        return;
    };
    // The fixture filters two tiles and then errors, so the host has
    // genuinely committed pixels before the failure — a rollback that
    // only worked for failures before the first commit would pass a
    // weaker test than this one.
    let original = gradient(100, 70, 3);
    let mut image = original.clone();
    let err = filter
        .apply(&mut image, &bf::RunOptions::default())
        .unwrap_err();
    assert!(matches!(err, bf::HostError::Plugin { .. }), "got {err}");
    assert_eq!(
        image.data, original.data,
        "a half-applied filter must roll back"
    );
}

#[test]
fn a_layer_is_offered_as_colour_planes_plus_transparency() {
    // The fixture refuses unless it is given one of the editable
    // transparency cases with the plane structure to match: three
    // colour planes then one of transparency, writable on the way out.
    let dir = tempfile::tempdir().unwrap();
    let Some(mut filter) = load("entry_layer", dir.path()) else {
        return;
    };
    let original = gradient(20, 12, 4);
    let mut image = original.clone();
    filter
        .apply(&mut image, &bf::RunOptions::default())
        .unwrap();
    let expected: Vec<u8> = original.data.iter().map(|&b| 255 - b).collect();
    assert_eq!(image.data, expected, "transparency is filtered too");
}

#[test]
fn a_plug_in_that_cannot_filter_transparency_gets_a_flat_image() {
    // Adobe: if the editable cases are unsupported the host tries the
    // protected ones, and failing those a layer can still be filtered
    // as flat. Losing the transparency beats refusing to run.
    let dir = tempfile::tempdir().unwrap();
    let Some(so) = common::build_native_plugin(dir.path()) else {
        return;
    };
    let bytes = PiplBuilder::new()
        .ostype(key::KIND, kind::FILTER)
        .pstring(key::NAME, "Flat only")
        .cstring(key::CODE_WIN64_X86, "entry_advance")
        .raw(key::FILTER_CASE_INFO, {
            // Only case 1 — flat, no selection — is filterable.
            let mut v = vec![1u8, 1, 0, 0];
            v.extend(std::iter::repeat_n(0u8, 24));
            v
        })
        .build();
    let pipl = Pipl::parse(&bytes, Endian::Little).unwrap();
    let mut filter = bf::Filter::open(&so, pipl, "entry_advance").unwrap();
    let mut image = gradient(12, 8, 4);
    filter
        .apply(&mut image, &bf::RunOptions::default())
        .expect("a layer should fall back to being filtered flat");
}

#[test]
fn a_selection_is_handed_over_and_confines_the_result() {
    let dir = tempfile::tempdir().unwrap();
    let Some(mut filter) = load("entry_masked", dir.path()) else {
        return;
    };
    let (w, h) = (16u32, 8u32);
    // Selected on the left, unselected on the right, so the fixture's
    // corner check passes and the effect has a visible edge.
    let selection: Vec<u8> = (0..w * h)
        .map(|i| if i % w < w / 2 { 255 } else { 0 })
        .collect();
    let original = gradient(w, h, 3);
    let mut image = original.clone();
    let opts = bf::RunOptions {
        selection: Some(selection),
        ..Default::default()
    };
    filter.apply(&mut image, &opts).unwrap();

    for y in 0..h {
        for x in 0..w {
            let at = ((y * w + x) * 3) as usize;
            let before = original.data[at];
            let after = image.data[at];
            if x < w / 2 {
                assert_eq!(after, 255 - before, "inside the selection at ({x},{y})");
            } else {
                assert_eq!(after, before, "outside it at ({x},{y})");
            }
        }
    }
}

#[test]
fn a_half_selected_pixel_is_blended_not_switched() {
    // autoMask is coverage, not a stencil: a pixel selected halfway
    // moves halfway.
    let dir = tempfile::tempdir().unwrap();
    let Some(mut filter) = load("entry_masked", dir.path()) else {
        return;
    };
    let (w, h) = (8u32, 4u32);
    let selection: Vec<u8> = (0..w * h)
        .map(|i| if i % w == 0 { 255 } else { 128 })
        .collect();
    let original = gradient(w, h, 3);
    let mut image = original.clone();
    let opts = bf::RunOptions {
        selection: Some(selection),
        ..Default::default()
    };
    filter.apply(&mut image, &opts).unwrap();
    // Column 1 is half selected: halfway between v and 255 - v.
    let at = 3usize;
    let before = original.data[at] as f32;
    let want = (before + ((255.0 - before) - before) * (128.0 / 255.0)).round() as u8;
    assert_eq!(image.data[at], want);
}

#[test]
fn a_selection_of_the_wrong_size_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let Some(mut filter) = load("entry_advance", dir.path()) else {
        return;
    };
    let mut image = gradient(8, 8, 3);
    let opts = bf::RunOptions {
        selection: Some(vec![255; 10]),
        ..Default::default()
    };
    match filter.apply(&mut image, &opts) {
        Err(bf::HostError::BadRequest(m)) => assert!(m.contains("one per pixel"), "{m}"),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn a_plug_in_that_declines_the_mode_is_refused_before_it_runs() {
    let dir = tempfile::tempdir().unwrap();
    let Some(so) = common::build_native_plugin(dir.path()) else {
        return;
    };
    // Same plug-in, but a PiPL declaring CMYK only.
    let bytes = PiplBuilder::new()
        .ostype(key::KIND, kind::FILTER)
        .pstring(key::NAME, "CMYK only")
        // CMYK is mode 4, so bit 3 counting from the most significant.
        .raw(key::SUPPORTED_MODES, vec![0b0000_1000, 0])
        .cstring(key::CODE_WIN64_X86, "entry_advance")
        .build();
    let pipl = Pipl::parse(&bytes, Endian::Little).unwrap();
    let mut filter = bf::Filter::open(&so, pipl, "entry_advance").unwrap();

    let mut image = gradient(8, 8, 3);
    let err = filter
        .apply(&mut image, &bf::RunOptions::default())
        .unwrap_err();
    assert!(
        matches!(err, bf::HostError::UnsupportedMode(3)),
        "got {err}"
    );
}

#[test]
fn a_plug_in_that_declines_the_flat_case_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let Some(so) = common::build_native_plugin(dir.path()) else {
        return;
    };
    let bytes = PiplBuilder::new()
        .ostype(key::KIND, kind::FILTER)
        .pstring(key::NAME, "Layers only")
        .cstring(key::CODE_WIN64_X86, "entry_advance")
        // Case 1 is inCantFilter; the plug-in wants transparency.
        .raw(key::FILTER_CASE_INFO, {
            let mut v = vec![0u8, 0, 0, 0];
            v.extend(std::iter::repeat_n(1u8, 24));
            v
        })
        .build();
    let pipl = Pipl::parse(&bytes, Endian::Little).unwrap();
    let mut filter = bf::Filter::open(&so, pipl, "entry_advance").unwrap();

    let mut image = gradient(8, 8, 3);
    let err = filter
        .apply(&mut image, &bf::RunOptions::default())
        .unwrap_err();
    assert!(matches!(err, bf::HostError::UnsupportedCase), "got {err}");
}

#[test]
fn an_oversized_image_is_refused_unless_the_plug_in_asked_for_wide_coordinates() {
    let dir = tempfile::tempdir().unwrap();
    let Some(mut filter) = load("entry_advance", dir.path()) else {
        return;
    };
    // Rectangles are 16-bit unless a plug-in claims BigDocumentStruct's
    // wide ones. The fixture does not, so it is told rather than handed
    // coordinates that have wrapped — and the narrow fields it does see
    // are clamped, never negative.
    let mut image = bf::Image {
        width: 40_000,
        height: 1,
        planes: 3,
        depth: bf::host::Depth::Eight,
        data: vec![0; 120_000],
    };
    let err = filter
        .apply(&mut image, &bf::RunOptions::default())
        .unwrap_err();
    assert!(
        matches!(err, bf::HostError::ImageTooLarge { .. }),
        "got {err}"
    );
}

#[test]
fn opening_a_non_filter_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let Some(so) = common::build_native_plugin(dir.path()) else {
        return;
    };
    let bytes = PiplBuilder::new()
        .ostype(key::KIND, kind::FORMAT)
        .cstring(key::CODE_WIN64_X86, "entry_advance")
        .build();
    let pipl = Pipl::parse(&bytes, Endian::Little).unwrap();
    match bf::Filter::open(&so, pipl, "entry_advance") {
        Err(bf::HostError::NotAFilter) => {}
        Err(e) => panic!("wrong error: {e}"),
        Ok(_) => panic!("a format module should not load as a filter"),
    }
}

#[test]
fn a_missing_entry_point_is_a_load_error_not_a_crash() {
    let dir = tempfile::tempdir().unwrap();
    let Some(so) = common::build_native_plugin(dir.path()) else {
        return;
    };
    match bf::Filter::open(&so, pipl_for("nope"), "nope") {
        Err(bf::HostError::Load(m)) => assert!(m.contains("nope"), "{m}"),
        Err(e) => panic!("wrong error: {e}"),
        Ok(_) => panic!("a missing entry point should not load"),
    }
}

/// The plug-in asks for a rectangle overhanging the image on all sides
/// and copies the padded buffer straight through, so whatever the host
/// put in the margin is what comes back out.
fn padding_case(entry: &str, expect: impl Fn(&bf::Image, i32, i32, usize) -> u8) {
    let dir = tempfile::tempdir().unwrap();
    let Some(mut filter) = load(entry, dir.path()) else {
        return;
    };
    let original = gradient(24, 20, 3);
    let mut image = original.clone();
    filter
        .apply(&mut image, &bf::RunOptions::default())
        .unwrap();

    const PAD: i32 = 8;
    for y in 0..image.height as i32 {
        for x in 0..image.width as i32 {
            for p in 0..3usize {
                let got = image.data[(y as usize * 24 + x as usize) * 3 + p];
                let want = expect(&original, x - PAD, y - PAD, p);
                assert_eq!(
                    got, want,
                    "pixel ({x},{y}) plane {p}: got {got}, want {want}"
                );
            }
        }
    }
}

fn sample_clamped(img: &bf::Image, x: i32, y: i32, p: usize) -> u8 {
    let cx = x.clamp(0, img.width as i32 - 1) as usize;
    let cy = y.clamp(0, img.height as i32 - 1) as usize;
    img.data[(cy * img.width as usize + cx) * img.planes as usize + p]
}

fn inside(img: &bf::Image, x: i32, y: i32) -> bool {
    x >= 0 && y >= 0 && x < img.width as i32 && y < img.height as i32
}

#[test]
fn out_of_bounds_requests_are_edge_replicated() {
    padding_case("entry_pad_replicate", sample_clamped);
}

#[test]
fn a_padding_value_in_0_to_255_is_a_literal_fill() {
    padding_case("entry_pad_fill", |img, x, y, p| {
        if inside(img, x, y) {
            sample_clamped(img, x, y, p)
        } else {
            200
        }
    });
}

#[test]
fn an_undocumented_padding_mode_still_yields_usable_pixels() {
    // The numeric values of the named padding modes are not in Adobe's
    // prose. Rather than guess, the host fills for 0..=255 and
    // replicates otherwise, so a mode it has never heard of still comes
    // back with real pixels instead of whatever the buffer held.
    padding_case("entry_pad_unknown", sample_clamped);
}

#[test]
fn the_buffer_suite_is_laid_out_the_way_the_guide_documents_it() {
    // The fixture declares BufferProcs from the API Guide's own text —
    // "version 2, routines 5" over Space, Allocate, Free, Lock, Unlock —
    // and refuses with a distinct code if the header, any slot, or the
    // memory it hands back is wrong.
    let dir = tempfile::tempdir().unwrap();
    let Some(mut filter) = load("entry_buffers", dir.path()) else {
        return;
    };
    let mut image = gradient(16, 16, 3);
    filter
        .apply(&mut image, &bf::RunOptions::default())
        .expect("the buffer suite should be usable");
}

#[test]
fn a_plug_in_error_string_reaches_the_caller() {
    let dir = tempfile::tempdir().unwrap();
    let Some(mut filter) = load("entry_error_string", dir.path()) else {
        return;
    };
    let mut image = gradient(8, 8, 3);
    let err = filter
        .apply(&mut image, &bf::RunOptions::default())
        .unwrap_err();
    match err {
        bf::HostError::Plugin {
            message: Some(m), ..
        } => assert_eq!(m, "the fixture declined on purpose"),
        other => panic!("expected the plug-in's own words, got: {other}"),
    }
}

#[test]
fn the_host_draws_a_plug_in_preview() {
    // Not an optional nicety: every FilterMeister-built plug-in refuses
    // to run at all if displayPixels is missing, and that is a large
    // slice of the freeware world. The fixture declares PSPixelMap
    // independently and asks the host to draw through it, then checks a
    // mode the host cannot draw is refused rather than drawn wrong.
    let dir = tempfile::tempdir().unwrap();
    let Some(mut filter) = load("entry_display", dir.path()) else {
        return;
    };
    let mut image = gradient(20, 12, 3);
    filter
        .apply(&mut image, &bf::RunOptions::default())
        .expect("the host should be able to draw a plug-in's pixels");
}

#[test]
fn the_host_answers_colour_service_requests() {
    let dir = tempfile::tempdir().unwrap();
    let Some(mut filter) = load("entry_color", dir.path()) else {
        return;
    };
    // gradient() puts (x*7 + y*3 + plane*11) % 256 in each byte, so the
    // red of pixel (1,0) is 7 — which is what the fixture checks the
    // sample-point answer against.
    let mut image = gradient(8, 4, 3);
    filter
        .apply(&mut image, &bf::RunOptions::default())
        .expect("colour services should work");
}

#[test]
fn the_host_answers_questions_about_the_document() {
    let dir = tempfile::tempdir().unwrap();
    let Some(mut filter) = load("entry_property", dir.path()) else {
        return;
    };
    let mut image = gradient(8, 4, 3);
    filter
        .apply(&mut image, &bf::RunOptions::default())
        .expect("the property suite should answer");
}

#[test]
fn an_output_rectangle_overhanging_the_image_is_served_and_clipped() {
    // Adobe says the output rectangle must be a subset of filterRect,
    // and real plug-ins ask for more anyway — Propetizer asks for a row
    // above the top edge. Refusing leaves outData null, which a plug-in
    // that ignores the error then writes through. Serving the buffer at
    // the size asked for and clipping on commit is what a host that
    // wants to survive real plug-ins does.
    let dir = tempfile::tempdir().unwrap();
    let Some(mut filter) = load("entry_out_of_bounds", dir.path()) else {
        return;
    };
    let mut image = gradient(16, 12, 3);
    filter
        .apply(&mut image, &bf::RunOptions::default())
        .unwrap();
    assert!(
        image.data.iter().all(|&b| b == 42),
        "every in-bounds pixel should have been written"
    );
    assert_eq!(image.data.len(), 16 * 12 * 3, "and nothing outside it");
}

/// A gradient of 16-bit samples over Photoshop's 0..=32768 range.
fn deep_gradient(width: u32, height: u32, planes: u16) -> bf::Image {
    let mut img = bf::Image::with_depth(width, height, planes, bf::host::Depth::Sixteen);
    for y in 0..height {
        for x in 0..width {
            for p in 0..planes as u32 {
                let v = ((x * 337 + y * 613 + p * 977) % 32769) as u16;
                let at = (((y * width + x) * planes as u32 + p) * 2) as usize;
                img.data[at..at + 2].copy_from_slice(&v.to_le_bytes());
            }
        }
    }
    img
}

#[test]
fn sixteen_bit_images_go_through_at_the_documented_range() {
    // The fixture refuses unless depth is 16, the mode is RGB48, and the
    // strides were scaled for two-byte samples — and inverts about
    // 32768, which is Photoshop's white and not 65535.
    let dir = tempfile::tempdir().unwrap();
    let Some(mut filter) = load("entry_deep", dir.path()) else {
        return;
    };
    let original = deep_gradient(19, 11, 3);
    let mut image = original.clone();
    filter
        .apply(&mut image, &bf::RunOptions::default())
        .unwrap();

    let before = original.data.as_chunks::<2>().0;
    let after = image.data.as_chunks::<2>().0;
    for (i, (b, a)) in before.iter().zip(after).enumerate() {
        assert_eq!(
            u16::from_le_bytes(*a),
            32768 - u16::from_le_bytes(*b),
            "sample {i}"
        );
    }
}

#[test]
fn a_sixteen_bit_grayscale_image_is_gray_16_not_grayscale() {
    let dir = tempfile::tempdir().unwrap();
    let Some(mut filter) = load("entry_deep", dir.path()) else {
        return;
    };
    let mut image = deep_gradient(8, 8, 1);
    filter
        .apply(&mut image, &bf::RunOptions::default())
        .expect("grayscale at 16 bits is its own mode, and the fixture checks which");
}

#[test]
fn a_big_document_runs_if_the_plug_in_claims_the_wide_coordinates() {
    // The other side of the previous test: a plug-in that sets
    // PluginUsing32BitCoordinates and works from BigDocumentStruct's
    // rectangles gets the whole document, however wide.
    let dir = tempfile::tempdir().unwrap();
    let Some(mut filter) = load("entry_big", dir.path()) else {
        return;
    };
    let mut image = bf::Image::new(40_000, 2, 3);
    image
        .data
        .iter_mut()
        .enumerate()
        .for_each(|(i, b)| *b = (i % 251) as u8);
    let original = image.clone();
    filter
        .apply(&mut image, &bf::RunOptions::default())
        .expect("a wide-coordinate plug-in should get the whole document");
    let expected: Vec<u8> = original.data.iter().map(|&b| 255 - b).collect();
    assert_eq!(image.data, expected);
}

#[test]
fn the_descriptor_block_is_always_there_even_though_scripting_is_not() {
    // Plug-ins write into `descriptorParameters` without checking it —
    // G'MIC faults on a null one — so the block is always supplied. The
    // read and write sub-suites are null, which is the documented way to
    // say scripting is unavailable, and a plug-in that cannot record has
    // to carry on regardless. The fixture insists on exactly that.
    let dir = tempfile::tempdir().unwrap();
    let Some(mut filter) = load("entry_script", dir.path()) else {
        return;
    };
    let mut image = gradient(8, 4, 3);
    filter
        .apply(&mut image, &bf::RunOptions::default())
        .unwrap();
    assert!(
        filter.recorded().is_none(),
        "nothing can be recorded while the suites are not served"
    );
}
