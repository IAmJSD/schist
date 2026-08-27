//! PiPL parsing, and the PE resource walk that finds one in a DLL.

mod common;

use common::PiplBuilder;
use schist_plugin_host_8bf as bf;
use schist_plugin_host_8bf::abi::fourcc;
use schist_plugin_host_8bf::pipl::{key, kind, CodeArch, Endian, Pipl, PiplError};

fn sample() -> Vec<u8> {
    sample_builder().build()
}

fn sample_builder() -> PiplBuilder {
    PiplBuilder::new()
        .ostype(key::KIND, kind::FILTER)
        .i32(key::VERSION, 4 << 16)
        .ostype(key::REQUIRED_HOST, fourcc(b"8BIM"))
        .pstring(key::NAME, "Invert Test")
        .pstring(key::CATEGORY, "Schist")
        // Grayscale and RGB, most significant bit first: 0b0101_0000.
        // This is byte-for-byte what G'MIC-Qt's own PiPL carries.
        .raw(key::SUPPORTED_MODES, vec![0b0101_0000, 0])
        .cstring(key::CODE_WIN64_X86, "entry_advance")
        .raw(
            key::FILTER_CASE_INFO,
            vec![
                1, 1, 0, 0, // flat, no selection
                1, 1, 0, 0, // flat, with selection
                0, 0, 0, 0, // floating selection: unsupported
                1, 1, 0, 0, 1, 1, 0, 0, 1, 1, 0, 0, 1, 1, 0, 0,
            ],
        )
}

#[test]
fn parses_the_documented_container() {
    let p = Pipl::parse(&sample(), Endian::Little).unwrap();
    assert_eq!(p.version, 0);
    assert_eq!(p.kind(), Some(kind::FILTER));
    assert_eq!(p.version_pair(), Some((4, 0)));
    assert_eq!(p.required_host(), Some(fourcc(b"8BIM")));
    assert_eq!(p.name().as_deref(), Some("Invert Test"));
    assert_eq!(p.category().as_deref(), Some("Schist"));
    assert_eq!(
        p.entry_point(CodeArch::Win64X86).as_deref(),
        Some("entry_advance")
    );
    assert_eq!(p.entry_point(CodeArch::Win32X86), None);
    assert_eq!(p.code_archs(), vec![CodeArch::Win64X86]);
}

#[test]
fn mode_flags_run_most_significant_bit_first() {
    let p = Pipl::parse(&sample(), Endian::Little).unwrap();
    use schist_plugin_host_8bf::abi::mode;
    assert_eq!(p.supports_mode(mode::BITMAP), Some(false));
    assert_eq!(p.supports_mode(mode::GRAY_SCALE), Some(true));
    assert_eq!(p.supports_mode(mode::INDEXED_COLOR), Some(false));
    assert_eq!(p.supports_mode(mode::RGB_COLOR), Some(true));
    assert_eq!(p.supports_mode(mode::CMYK_COLOR), Some(false));
    assert_eq!(p.supports_mode(mode::HSL_COLOR), Some(false));
    // Past the end of the flag set is "not declared", not a panic.
    assert_eq!(p.supports_mode(mode::RGB_48), Some(false));
    assert_eq!(p.supports_mode(mode::GRAY_32), Some(false));
}

#[test]
fn filter_case_info_indexes_by_case_number() {
    use schist_plugin_host_8bf::abi::filter_case;
    let p = Pipl::parse(&sample(), Endian::Little).unwrap();
    let fci = p.filter_case_info().unwrap();
    assert!(fci[filter_case::FLAT_IMAGE_NO_SELECTION as usize - 1].is_supported());
    assert!(!fci[filter_case::FLOATING_SELECTION as usize - 1].is_supported());
}

#[test]
fn properties_are_padded_to_four_bytes() {
    // "Invert Test" is 11 characters, so the Pascal string is 12 bytes
    // and needs no padding; "Schist" is 6 + 1 = 7 and needs one byte.
    // Getting that wrong desynchronises everything after it, so the
    // proof is that the later properties still read correctly.
    let p = Pipl::parse(&sample(), Endian::Little).unwrap();
    assert_eq!(p.properties.len(), 8);
    assert!(p.filter_case_info().is_some());
}

#[test]
fn big_endian_is_the_mac_byte_order() {
    // Same properties, byte-swapped: a Mac PiPL. Parsing it as
    // little-endian must not accidentally succeed.
    let be = sample_builder().big_endian().build();
    assert_ne!(be, sample(), "the two byte orders must differ");
    let p = Pipl::parse(&be, Endian::Big).unwrap();
    assert_eq!(p.kind(), Some(kind::FILTER));
    assert_eq!(p.name().as_deref(), Some("Invert Test"));
    assert!(Pipl::parse(&be, Endian::Little).is_err());
}

#[test]
fn refuses_rather_than_guesses_on_damaged_input() {
    assert_eq!(
        Pipl::parse(&[0; 4], Endian::Little),
        Err(PiplError::TooShort)
    );

    let mut truncated = sample();
    truncated.truncate(40);
    assert!(matches!(
        Pipl::parse(&truncated, Endian::Little),
        Err(PiplError::Truncated { .. })
    ));

    // A count no real property list would carry.
    let mut absurd = sample();
    absurd[4..8].copy_from_slice(&1_000_000i32.to_le_bytes());
    assert!(matches!(
        Pipl::parse(&absurd, Endian::Little),
        Err(PiplError::BadCount(1_000_000))
    ));

    // A length that runs off the end.
    let mut overlong = sample();
    overlong[20..24].copy_from_slice(&9999i32.to_le_bytes());
    assert!(matches!(
        Pipl::parse(&overlong, Endian::Little),
        Err(PiplError::Truncated { .. })
    ));
}

#[test]
fn finds_the_pipl_inside_a_real_dll() {
    let dir = tempfile::tempdir().unwrap();
    let Some(dll) = common::build_windows_dll(dir.path(), &sample()) else {
        return;
    };

    let found = bf::inspect_file(&dll).expect("PiPL should be discoverable");
    assert_eq!(found.len(), 1);
    let f = &found[0];
    assert_eq!(f.machine, bf::pe::Machine::Amd64);
    assert_eq!(f.pipl.kind(), Some(kind::FILTER));
    assert_eq!(f.menu_name(), "Schist > Invert Test");
    assert_eq!(
        f.pipl.entry_point(CodeArch::Win64X86).as_deref(),
        Some("entry_advance")
    );

    // On a non-Windows build the entry point for the *running*
    // architecture is absent, and the blocker says why in a way a user
    // could act on.
    if cfg!(windows) {
        assert!(f.blocker().is_none());
    } else {
        assert_eq!(f.blocker(), Some(bf::Blocker::WrongPlatform));
        assert!(f.blocker().unwrap().to_string().contains("Wine"));
    }
}

#[test]
fn a_dll_without_a_pipl_is_not_a_plug_in() {
    let dir = tempfile::tempdir().unwrap();
    // Reuse the DLL builder but hand it an empty property list, then
    // check the *directory* scan silently skips unrelated files.
    let Some(_dll) = common::build_windows_dll(dir.path(), &PiplBuilder::new().build()) else {
        return;
    };
    std::fs::write(dir.path().join("readme.txt"), b"not a plug-in").unwrap();
    let found = bf::discover_dir(dir.path()).unwrap();
    // The empty list parses but declares nothing, so it is discovered
    // with no kind and blocked rather than silently offered.
    for f in &found {
        assert!(f.blocker().is_some());
    }
}
