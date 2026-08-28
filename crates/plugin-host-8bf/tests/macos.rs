//! Discovering a macOS plug-in bundle.
//!
//! Built here rather than met in the wild: this machine is Linux, so
//! what these prove is that the parsing agrees with the formats as
//! documented, not that it agrees with a plug-in Adobe's tools produced.
//! That distinction cost three bugs on the Windows side before a real
//! plug-in was ever run, and it will probably cost some here too.

mod common;

use common::PiplBuilder;
use schist_plugin_host_8bf as bf;
use schist_plugin_host_8bf::abi::fourcc;
use schist_plugin_host_8bf::launch::PluginAbi;
use schist_plugin_host_8bf::pipl::{key, kind};
use std::path::Path;

/// A 64-bit Mach-O header for `cpu`, which is all discovery reads.
///
/// Little-endian throughout, magic included: a Mach-O is written in its
/// own target's byte order, and every Mac architecture Schist can host
/// is little-endian. Writing the magic the other way round — which this
/// did until a real bundle was read on a Mac — describes a PowerPC
/// binary instead.
fn macho(cpu: u32) -> Vec<u8> {
    let mut v = 0xfeed_facfu32.to_le_bytes().to_vec();
    v.extend_from_slice(&cpu.to_le_bytes());
    v.extend_from_slice(&[0u8; 24]);
    v
}

/// A resource fork holding one `PiPL`.
fn fork(body: &[u8]) -> Vec<u8> {
    let data_off = 256usize;
    let mut v = vec![0u8; data_off];
    v.extend_from_slice(&(body.len() as u32).to_be_bytes());
    v.extend_from_slice(body);
    let map_off = v.len();
    v.extend_from_slice(&[0u8; 24]);
    v.extend_from_slice(&28u16.to_be_bytes()); // type list offset
    v.extend_from_slice(&0u16.to_be_bytes()); // name list offset
    v.extend_from_slice(&0u16.to_be_bytes()); // one type, stored as n-1
    v.extend_from_slice(b"PiPL");
    v.extend_from_slice(&0u16.to_be_bytes()); // one resource
    v.extend_from_slice(&10u16.to_be_bytes()); // ref list offset
    v.extend_from_slice(&128u16.to_be_bytes()); // id
    v.extend_from_slice(&0xffffu16.to_be_bytes()); // no name
    v.push(0);
    v.extend_from_slice(&[0, 0, 0]); // data offset, three bytes
    v.extend_from_slice(&0u32.to_be_bytes());
    v[0..4].copy_from_slice(&(data_off as u32).to_be_bytes());
    v[4..8].copy_from_slice(&(map_off as u32).to_be_bytes());
    v
}

/// Lay out a `.plugin` the way macOS does.
fn bundle(root: &Path, name: &str, cpu: u32, pipl: &[u8]) -> std::path::PathBuf {
    let dir = root.join(format!("{name}.plugin"));
    std::fs::create_dir_all(dir.join("Contents/MacOS")).unwrap();
    std::fs::create_dir_all(dir.join("Contents/Resources")).unwrap();
    std::fs::write(dir.join("Contents/MacOS").join(name), macho(cpu)).unwrap();
    std::fs::write(
        dir.join("Contents/Resources").join(format!("{name}.rsrc")),
        fork(pipl),
    )
    .unwrap();
    dir
}

fn sample_pipl(entry_key: u32, entry: &str) -> Vec<u8> {
    PiplBuilder::new()
        .big_endian()
        .ostype(key::KIND, kind::FILTER)
        .ostype(key::REQUIRED_HOST, fourcc(b"8BIM"))
        .pstring(key::NAME, "Twirl")
        .pstring(key::CATEGORY, "Distort")
        .cstring(entry_key, entry)
        .build()
}

#[test]
fn an_apple_silicon_bundle_is_read_off_disk() {
    let dir = tempfile::tempdir().unwrap();
    let plugin = bundle(
        dir.path(),
        "Twirl",
        0x0100_000c, // arm64
        &sample_pipl(fourcc(b"mm64"), "PluginMain"),
    );

    let found = bf::inspect_bundle(&plugin).expect("the bundle should be readable");
    assert_eq!(found.len(), 1);
    let f = &found[0];
    assert_eq!(f.abi, Some(PluginAbi::MacArm64));
    assert_eq!(f.menu_name(), "Distort > Twirl");
    // The user sees the bundle; the helper is given the binary inside.
    assert_eq!(f.container, plugin);
    assert!(f.path.ends_with("Contents/MacOS/Twirl"));
    assert_eq!(f.entry_point.as_deref(), Some("PluginMain"));
}

#[test]
fn an_intel_bundle_is_read_the_same_way() {
    let dir = tempfile::tempdir().unwrap();
    let plugin = bundle(
        dir.path(),
        "Ripple",
        0x0100_0007, // x86-64
        &sample_pipl(fourcc(b"ma64"), "main_entry"),
    );
    let found = bf::inspect_bundle(&plugin).unwrap();
    assert_eq!(found[0].abi, Some(PluginAbi::MacX86_64));
    assert_eq!(found[0].entry_point.as_deref(), Some("main_entry"));
}

#[test]
fn a_mac_plug_in_is_discovered_but_refused_off_mac() {
    let dir = tempfile::tempdir().unwrap();
    bundle(
        dir.path(),
        "Twirl",
        0x0100_000c,
        &sample_pipl(fourcc(b"mm64"), "PluginMain"),
    );
    // Discovery is pure byte parsing, so it works anywhere — which is
    // what lets a Linux build say *why* it cannot run a Mac plug-in
    // rather than simply not listing it.
    let found = bf::discover_dir(dir.path()).unwrap();
    assert_eq!(found.len(), 1);
    if cfg!(target_os = "macos") {
        return;
    }
    let blocker = found[0].blocker().expect("not runnable off macOS");
    assert!(
        blocker.to_string().contains("macOS"),
        "should say why: {blocker}"
    );
}

#[test]
fn a_bundle_with_no_binary_is_not_a_plug_in() {
    let dir = tempfile::tempdir().unwrap();
    let empty = dir.path().join("Hollow.plugin");
    std::fs::create_dir_all(empty.join("Contents/Resources")).unwrap();
    assert!(bf::inspect_bundle(&empty).is_err());
    // And a folder scan skips it rather than failing the whole scan.
    assert!(bf::discover_dir(dir.path()).unwrap().is_empty());
}
