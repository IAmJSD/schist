//! End-to-end: build the example plugins to WebAssembly, load them through
//! the host, and check they behave.
//!
//! This is the milestone's actual exit criterion — "an external dev builds
//! a working filter against the template" — so the test really compiles the
//! example crates rather than loading a checked-in binary. It skips (rather
//! than fails) when the wasm target isn't installed.

use photoslop_plugin_api::{CodecPlugin, FilterPlugin, FilterValues, PluginRegistry};
use photoslop_plugin_host_wasm::{LoadedPlugin, PluginManager, WasmCodec, WasmFilter};
use std::path::{Path, PathBuf};
use std::process::Command;

fn wasm_target_installed() -> bool {
    Command::new("rustup")
        .args(["target", "list", "--installed"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains("wasm32-unknown-unknown"))
        .unwrap_or(false)
}

/// Build one example plugin and return the .wasm path.
fn build_example(dir: &str, artifact: &str) -> Option<PathBuf> {
    if !wasm_target_installed() {
        eprintln!("wasm32-unknown-unknown not installed; skipping");
        return None;
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let crate_dir = root.join("examples/plugins").join(dir);
    let status = Command::new(env!("CARGO"))
        .current_dir(&crate_dir)
        .args(["build", "--release", "--target", "wasm32-unknown-unknown"])
        .status()
        .expect("running cargo");
    assert!(status.success(), "building example plugin {dir}");
    let path = crate_dir
        .join("target/wasm32-unknown-unknown/release")
        .join(artifact);
    assert!(path.exists(), "expected {path:?}");
    Some(path)
}

#[test]
fn sepia_filter_plugin_loads_and_transforms_pixels() {
    let Some(path) = build_example("sepia-filter", "photoslop_example_sepia.wasm") else {
        return;
    };
    let plugin = LoadedPlugin::load(&path).expect("plugin loads");
    assert_eq!(plugin.manifest.id, "com.example.sepia");
    assert_eq!(plugin.manifest.params.len(), 1);

    let filter = WasmFilter::new(plugin);
    assert_eq!(filter.name(), "Sepia");
    assert_eq!(filter.category(), "Plugins");
    assert_eq!(filter.params()[0].key, "amount");

    // Pure blue, fully sepia'd, should come out warm: red > blue.
    let mut pixels = vec![0.0f32, 0.0, 1.0, 1.0];
    filter.apply(&mut pixels, 1, 1, &FilterValues(vec![("amount", 100.0)]));
    assert!(pixels[0] > pixels[2], "sepia warms the image: {pixels:?}");
    assert_eq!(pixels[3], 1.0, "alpha preserved");

    // Amount 0 is a no-op.
    let mut pixels = vec![0.25f32, 0.5, 0.75, 1.0];
    let before = pixels.clone();
    filter.apply(&mut pixels, 1, 1, &FilterValues(vec![("amount", 0.0)]));
    for (a, b) in pixels.iter().zip(before.iter()) {
        assert!((a - b).abs() < 1e-4, "{pixels:?} vs {before:?}");
    }
}

#[test]
fn pgm_codec_plugin_decodes_an_image() {
    let Some(path) = build_example("pgm-codec", "photoslop_example_pgm.wasm") else {
        return;
    };
    let plugin = LoadedPlugin::load(&path).expect("plugin loads");
    assert_eq!(plugin.manifest.extensions, vec!["pgm".to_string()]);

    let codec = WasmCodec::new(plugin);
    assert!(codec.extensions().contains(&"pgm"));

    // A 2x2 PGM: black, white / mid, black.
    let mut pgm = b"P5\n# a comment\n2 2\n255\n".to_vec();
    pgm.extend_from_slice(&[0u8, 255, 128, 0]);
    assert!(codec.probe(&pgm));
    assert!(!codec.probe(b"not a pgm"));

    let doc = codec.import(&pgm).expect("decodes");
    assert_eq!((doc.width, doc.height), (2, 2));
    let tiles = &doc.tree.layers[0].as_raster().unwrap().tiles;
    assert_eq!(tiles.pixel(0, 0).to_u8(), [0, 0, 0, 255]);
    assert_eq!(tiles.pixel(1, 0).to_u8(), [255, 255, 255, 255]);
    assert_eq!(tiles.pixel(0, 1).to_u8(), [128, 128, 128, 255]);
}

#[test]
fn plugin_directory_scan_registers_and_honours_disabling() {
    let Some(sepia) = build_example("sepia-filter", "photoslop_example_sepia.wasm") else {
        return;
    };
    let dir = std::env::temp_dir().join("photoslop-plugin-scan-test");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let installed = PluginManager::install(&sepia, &dir).expect("installs");
    assert!(installed.exists());

    let mut registry = PluginRegistry::new();
    let mut manager = PluginManager::load_dir(&dir, &mut registry);
    assert_eq!(manager.entries.len(), 1);
    assert!(manager.entries[0].error.is_none());
    assert_eq!(registry.filters().count(), 1);

    // Disabling is persisted and respected on the next scan.
    manager.set_enabled("com.example.sepia", false, &dir);
    let mut registry = PluginRegistry::new();
    let manager = PluginManager::load_dir(&dir, &mut registry);
    assert_eq!(registry.filters().count(), 0, "disabled plugin not loaded");
    assert!(!manager.entries[0].enabled);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn junk_and_hostile_modules_are_refused_not_executed() {
    // Not wasm at all.
    let dir = std::env::temp_dir().join("photoslop-plugin-junk-test");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("garbage.wasm"), b"definitely not wasm").unwrap();

    // Valid wasm, but exports nothing we need.
    let empty_module = wat_minimal();
    std::fs::write(dir.join("empty.wasm"), empty_module).unwrap();

    let mut registry = PluginRegistry::new();
    let manager = PluginManager::load_dir(&dir, &mut registry);
    assert_eq!(manager.entries.len(), 2);
    assert!(
        manager.entries.iter().all(|e| e.error.is_some()),
        "both modules should be refused"
    );
    assert_eq!(registry.filters().count(), 0);
    assert_eq!(registry.codecs().count(), 0);

    let _ = std::fs::remove_dir_all(&dir);
}

/// The smallest valid wasm module: version header only.
fn wat_minimal() -> Vec<u8> {
    let mut out = b"\0asm".to_vec();
    out.extend_from_slice(&1u32.to_le_bytes());
    out
}

#[test]
fn missing_plugin_directory_is_not_an_error() {
    let dir = std::env::temp_dir().join("photoslop-plugin-absent-dir");
    let _ = std::fs::remove_dir_all(&dir);
    let mut registry = PluginRegistry::new();
    let manager = PluginManager::load_dir(&dir, &mut registry);
    assert!(manager.entries.is_empty());
}
