//! Photoslop — a plugin-first image editor on GPUI. See PLAN.md.

mod actions;
mod assets;
mod crash;
mod dialogs;
mod keymap;
mod panels;
mod ui;
mod workspace;

use gpui::{px, size, App, AppContext as _, Application, Bounds, WindowBounds, WindowOptions};
use photoslop_plugin_api::{CodecPlugin, PluginManifest, PluginRegistry};
use workspace::Workspace;

/// PSD/PSB import and export via `photoslop-codec-psd`.
struct PsdCodec;

impl CodecPlugin for PsdCodec {
    fn id(&self) -> &'static str {
        "codec.psd"
    }
    fn name(&self) -> &'static str {
        "Photoshop PSD"
    }
    fn extensions(&self) -> &'static [&'static str] {
        &["psd", "psb"]
    }
    fn probe(&self, bytes: &[u8]) -> bool {
        photoslop_codec_psd::is_psd(bytes)
    }
    fn import(&self, bytes: &[u8]) -> anyhow::Result<photoslop_core::Document> {
        Ok(photoslop_codec_psd::read_psd(bytes)?)
    }
    fn can_export(&self) -> bool {
        true
    }
    fn export(&self, doc: &photoslop_core::Document) -> anyhow::Result<Vec<u8>> {
        Ok(photoslop_codec_psd::write_psd(doc)?)
    }
}

struct PsdPlugin;

impl PluginManifest for PsdPlugin {
    fn id(&self) -> &'static str {
        "photoslop.codec-psd"
    }
    fn register(&self, registry: &mut PluginRegistry) {
        registry.register_codec(Box::new(PsdCodec));
    }
}

/// Crash reporting stays off unless the user opts in.
fn crash_reports_enabled() -> bool {
    if std::env::var("PHOTOSLOP_CRASH_REPORTS").is_ok_and(|v| v == "1") {
        return true;
    }
    std::env::var("XDG_CONFIG_HOME")
        .ok()
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| std::path::PathBuf::from(h).join(".config"))
        })
        .map(|d| d.join("photoslop/preferences.json"))
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|v| v.get("crash_reports").and_then(|b| b.as_bool()))
        .unwrap_or(false)
}

/// Assemble the first-party plugin set. Every entry here is optional — the
/// app boots (to an empty shell) with any or all of them removed.
fn build_registry() -> (PluginRegistry, photoslop_plugin_host_wasm::PluginManager) {
    let mut registry = PluginRegistry::new();
    let manifests: Vec<Box<dyn PluginManifest>> = vec![
        Box::new(photoslop_tools_basic::BasicToolsPlugin),
        Box::new(photoslop_tools_paint::PaintToolsPlugin),
        Box::new(photoslop_tools_select::SelectToolsPlugin),
        Box::new(photoslop_tools_transform::TransformToolsPlugin),
        Box::new(photoslop_tools_vector::VectorToolsPlugin),
        Box::new(photoslop_tools_type::TypeToolsPlugin),
        Box::new(photoslop_commands_core::CoreCommandsPlugin),
        Box::new(photoslop_filters_core::CoreFiltersPlugin),
        Box::new(photoslop_codecs_common::CommonCodecsPlugin),
        Box::new(PsdPlugin),
    ];
    for manifest in manifests {
        log::info!("loading plugin {}", manifest.id());
        manifest.register(&mut registry);
    }
    // Third-party WebAssembly plugins, sandboxed (M9).
    let manager = match photoslop_plugin_host_wasm::PluginManager::plugin_dir() {
        Some(dir) => photoslop_plugin_host_wasm::PluginManager::load_dir(&dir, &mut registry),
        None => photoslop_plugin_host_wasm::PluginManager::default(),
    };
    (registry, manager)
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    // Opt-in: PHOTOSLOP_CRASH_REPORTS=1, or the preference file's
    // crash_reports flag once the user enables it in Preferences.
    crash::install_handler(crash_reports_enabled());
    let (registry, plugin_manager) = build_registry();

    Application::new()
        .with_assets(assets::Assets)
        .run(move |cx: &mut App| {
            cx.bind_keys(keymap::build_bindings(&registry));
            let bounds = Bounds::centered(None, size(px(1440.0), px(900.0)), cx);
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                |_window, cx| {
                    let workspace = cx.new(|cx| {
                        let mut ws = Workspace::new(registry, plugin_manager, cx);
                        if let Some(path) = std::env::args().nth(1) {
                            ws.load_file(path.into(), cx);
                        } else if let Some(recovery) = Workspace::pending_recovery() {
                            // A previous session died with unsaved work.
                            log::info!("recovering {recovery:?}");
                            ws.recover_from(recovery, cx);
                        }
                        ws
                    });
                    workspace
                },
            )
            .expect("failed to open window");
            cx.activate(true);
        });
}
