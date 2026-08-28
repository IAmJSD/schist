//! Discovered plug-ins reaching the filter registry.
//!
//! The fixture is a real (cross-linked) Windows DLL carrying a PiPL, so
//! this covers the path the app and the MCP server actually take:
//! scan a folder, decide what can run here, register the rest.
#![cfg(feature = "registry")]

mod common;

use common::PiplBuilder;
use schist_plugin_api::PluginRegistry;
use schist_plugin_host_8bf::abi::fourcc;
use schist_plugin_host_8bf::manager::{Interactive, PluginManager};
use schist_plugin_host_8bf::pipl::{key, kind};

fn pipl(name: &str, category: &str) -> Vec<u8> {
    PiplBuilder::new()
        .ostype(key::KIND, kind::FILTER)
        .i32(key::VERSION, 4 << 16)
        .ostype(key::REQUIRED_HOST, fourcc(b"8BIM"))
        .pstring(key::NAME, name)
        .pstring(key::CATEGORY, category)
        .raw(key::SUPPORTED_MODES, vec![0b0101_0000, 0])
        .cstring(key::CODE_WIN64_X86, "PluginMain")
        .raw(key::FILTER_CASE_INFO, [1u8, 1, 0, 0].repeat(7))
        .build()
}

#[test]
fn a_plug_in_in_the_folder_becomes_a_filter() {
    let dir = tempfile::tempdir().unwrap();
    let Some(_dll) = common::build_windows_dll(dir.path(), &pipl("Invert", "Schist")) else {
        return;
    };

    let mut registry = PluginRegistry::new();
    let manager =
        PluginManager::load_dirs(&[dir.path().to_path_buf()], &mut registry, Interactive::No);

    assert_eq!(manager.entries.len(), 1, "one filter in one plug-in");
    let entry = &manager.entries[0];
    assert_eq!(entry.name, "Schist > Invert");
    assert_eq!(entry.id, "8bf.test.pluginmain");
    assert!(entry.enabled);

    // Whether it can run here depends on the machine — Wine installed,
    // a helper for this architecture present. What must hold either way
    // is that the two answers agree: a plug-in is offered as a filter
    // exactly when nothing blocks it.
    let registered = registry.filters().any(|f| f.id() == entry.id);
    assert_eq!(
        registered,
        entry.blocker.is_none(),
        "listed blocker {:?} disagrees with registration",
        entry.blocker
    );
    if registered {
        let filter = registry.filters().find(|f| f.id() == entry.id).unwrap();
        assert_eq!(filter.name(), "Invert");
        assert_eq!(filter.category(), "Schist");
        // A `.8bf` has no parameter list; its dialog is its UI.
        assert!(filter.params().is_empty());
    }
}

#[test]
fn a_disabled_plug_in_is_listed_but_not_registered() {
    let dir = tempfile::tempdir().unwrap();
    let Some(_dll) = common::build_windows_dll(dir.path(), &pipl("Invert", "Schist")) else {
        return;
    };
    std::fs::write(dir.path().join("disabled.txt"), "8bf.test.pluginmain\n").unwrap();

    let mut registry = PluginRegistry::new();
    let manager =
        PluginManager::load_dirs(&[dir.path().to_path_buf()], &mut registry, Interactive::No);

    assert_eq!(manager.entries.len(), 1);
    assert!(!manager.entries[0].enabled);
    assert_eq!(registry.filters().count(), 0);
}

#[test]
fn a_folder_with_no_plug_ins_is_not_an_error() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("readme.txt"), "not a plug-in").unwrap();
    let missing = dir.path().join("nothing-here");

    let mut registry = PluginRegistry::new();
    let manager = PluginManager::load_dirs(
        &[dir.path().to_path_buf(), missing],
        &mut registry,
        Interactive::No,
    );

    assert!(manager.entries.is_empty());
    assert_eq!(registry.filters().count(), 0);
}

#[test]
fn toggling_writes_a_list_the_next_scan_reads() {
    let dir = tempfile::tempdir().unwrap();
    let Some(_dll) = common::build_windows_dll(dir.path(), &pipl("Invert", "Schist")) else {
        return;
    };

    let mut registry = PluginRegistry::new();
    let mut manager =
        PluginManager::load_dirs(&[dir.path().to_path_buf()], &mut registry, Interactive::No);
    manager.set_enabled("8bf.test.pluginmain", false, dir.path());

    let mut again = PluginRegistry::new();
    let reloaded =
        PluginManager::load_dirs(&[dir.path().to_path_buf()], &mut again, Interactive::No);
    assert!(!reloaded.entries[0].enabled);
    assert_eq!(again.filters().count(), 0);
}
