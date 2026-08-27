//! Pin down the `FilterRecord` layout.
//!
//! Two things are being checked. First, that the offsets do not move
//! silently: the table below is the record of what this host believes
//! the ABI to be, so any edit that shifts a field fails here rather than
//! in a plug-in's face. Second, that Rust's `repr(C)` agrees with a C
//! compiler laying out the same declaration — which is the assumption
//! the whole host rests on, since Adobe's prose does not state a packing
//! rule.

mod common;

use schist_plugin_host_8bf::abi::FilterRecord;
use std::mem::offset_of;

/// Field offsets on x86-64 with natural alignment. Kept in the same
/// order as `probe_offsets` in tests/fixtures/plugin.c.
const FIELDS: &[(&str, usize)] = &[
    ("serial_number", offset_of!(FilterRecord, serial_number)),
    ("abort_proc", offset_of!(FilterRecord, abort_proc)),
    ("parameters", offset_of!(FilterRecord, parameters)),
    ("image_size", offset_of!(FilterRecord, image_size)),
    ("planes", offset_of!(FilterRecord, planes)),
    ("filter_rect", offset_of!(FilterRecord, filter_rect)),
    ("background", offset_of!(FilterRecord, background)),
    ("max_space", offset_of!(FilterRecord, max_space)),
    ("in_rect", offset_of!(FilterRecord, in_rect)),
    ("in_data", offset_of!(FilterRecord, in_data)),
    ("out_data", offset_of!(FilterRecord, out_data)),
    ("is_floating", offset_of!(FilterRecord, is_floating)),
    ("mask_rect", offset_of!(FilterRecord, mask_rect)),
    ("mask_data", offset_of!(FilterRecord, mask_data)),
    ("back_color", offset_of!(FilterRecord, back_color)),
    ("host_sig", offset_of!(FilterRecord, host_sig)),
    ("image_mode", offset_of!(FilterRecord, image_mode)),
    ("monitor", offset_of!(FilterRecord, monitor)),
    ("platform_data", offset_of!(FilterRecord, platform_data)),
    ("handle_procs", offset_of!(FilterRecord, handle_procs)),
    ("filter_case", offset_of!(FilterRecord, filter_case)),
    ("advance_state", offset_of!(FilterRecord, advance_state)),
    (
        "sampling_support",
        offset_of!(FilterRecord, sampling_support),
    ),
    ("input_rate", offset_of!(FilterRecord, input_rate)),
    ("in_layer_planes", offset_of!(FilterRecord, in_layer_planes)),
    ("in_column_bytes", offset_of!(FilterRecord, in_column_bytes)),
    (
        "image_services_procs",
        offset_of!(FilterRecord, image_services_procs),
    ),
    (
        "mask_tile_origin",
        offset_of!(FilterRecord, mask_tile_origin),
    ),
    (
        "descriptor_parameters",
        offset_of!(FilterRecord, descriptor_parameters),
    ),
    ("error_string", offset_of!(FilterRecord, error_string)),
    ("s_sp_basic", offset_of!(FilterRecord, s_sp_basic)),
    ("depth", offset_of!(FilterRecord, depth)),
    (
        "icc_profile_data",
        offset_of!(FilterRecord, icc_profile_data),
    ),
    ("has_image_scrap", offset_of!(FilterRecord, has_image_scrap)),
    (
        "big_document_data",
        offset_of!(FilterRecord, big_document_data),
    ),
    ("reserved", offset_of!(FilterRecord, reserved)),
];

#[test]
fn the_record_starts_where_the_declaration_says() {
    // Anchors chosen because they are where a packing mistake shows
    // first and worst. `abort_proc` is a pointer directly after an
    // int32: at 4 the record is packed to four bytes, at 8 it is
    // naturally aligned. Natural alignment is wrong, and wrong in a way
    // that costs eight bytes by the time you reach `platform_data` —
    // far enough that a real plug-in reads a pointer out of the middle
    // of the monitor record and faults on the value it finds there.
    assert_eq!(offset_of!(FilterRecord, serial_number), 0);
    assert_eq!(offset_of!(FilterRecord, abort_proc), 4);
    assert_eq!(std::mem::align_of::<FilterRecord>(), 4);

    #[cfg(target_pointer_width = "64")]
    {
        assert_eq!(offset_of!(FilterRecord, platform_data), 216);
        assert_eq!(offset_of!(FilterRecord, buffer_procs), 224);
    }
}

/// The exact table is 64-bit. On a 32-bit target every pointer is four
/// bytes and `packed(4)` coincides with natural alignment, so the
/// offsets differ throughout and there is nothing to pin — the C
/// cross-check below still runs there and is what matters.
#[cfg(target_pointer_width = "64")]
#[test]
fn offsets_are_stable() {
    // Regenerate deliberately, never casually: a change here changes
    // what every plug-in sees.
    let expected: Vec<usize> = vec![
        0, 4, 20, 28, 32, 34, 42, 56, 64, 88, 100, 112, 116, 124, 136, 144, 156, 176, 216, 256,
        268, 280, 308, 312, 328, 368, 384, 428, 432, 440, 464, 480, 484, 500, 504, 512,
    ];
    let actual: Vec<usize> = FIELDS.iter().map(|(_, o)| *o).collect();
    if actual != expected {
        let table: Vec<String> = FIELDS
            .iter()
            .map(|(n, o)| format!("    {n}: {o}"))
            .collect();
        panic!(
            "FilterRecord offsets moved:\n{}\nactual = {actual:?}",
            table.join("\n")
        );
    }
    assert_eq!(std::mem::size_of::<FilterRecord>(), 560);
}

#[test]
fn a_c_compiler_lays_the_record_out_the_same_way() {
    let dir = tempfile::tempdir().unwrap();
    let Some(so) = common::build_native_plugin(dir.path()) else {
        return;
    };
    unsafe {
        let lib = libloading::Library::new(&so).unwrap();
        let probe_sizeof: libloading::Symbol<unsafe extern "C" fn() -> usize> =
            lib.get(b"probe_sizeof\0").unwrap();
        let probe_offsets: libloading::Symbol<unsafe extern "C" fn(*mut usize, usize) -> usize> =
            lib.get(b"probe_offsets\0").unwrap();

        assert_eq!(
            probe_sizeof(),
            std::mem::size_of::<FilterRecord>(),
            "C and Rust disagree on sizeof(FilterRecord)"
        );

        let mut buf = vec![0usize; FIELDS.len()];
        let n = probe_offsets(buf.as_mut_ptr(), buf.len());
        assert_eq!(n, FIELDS.len(), "probe returned {n} offsets");
        for (i, (name, rust)) in FIELDS.iter().enumerate() {
            assert_eq!(
                buf[i], *rust,
                "field {name}: C says {}, Rust says {rust}",
                buf[i]
            );
        }
    }
}
