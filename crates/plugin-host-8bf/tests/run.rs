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
        .raw(key::FILTER_CASE_INFO, {
            let mut v = vec![1u8, 1, 0, 0];
            v.extend(std::iter::repeat_n(0u8, 24));
            v
        })
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
fn transparency_is_refused_rather_than_filtered_as_colour() {
    let dir = tempfile::tempdir().unwrap();
    let Some(mut filter) = load("entry_advance", dir.path()) else {
        return;
    };
    let mut image = gradient(16, 16, 4);
    let err = filter
        .apply(&mut image, &bf::RunOptions::default())
        .unwrap_err();
    match err {
        bf::HostError::BadRequest(m) => assert!(m.contains("4 planes"), "{m}"),
        other => panic!("wrong error: {other}"),
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
fn an_oversized_image_is_refused_rather_than_wrapped() {
    let dir = tempfile::tempdir().unwrap();
    let Some(mut filter) = load("entry_advance", dir.path()) else {
        return;
    };
    // Rectangles are 16-bit without BigDocumentStruct, so anything past
    // 32767 would silently wrap into a negative coordinate.
    let mut image = bf::Image {
        width: 40_000,
        height: 1,
        planes: 3,
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
