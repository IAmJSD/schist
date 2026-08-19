//! Photoslop — a plugin-first image editor on GPUI. See PLAN.md.

mod actions;
mod keymap;
mod panels;
mod workspace;

use gpui::{px, size, App, Application, Bounds, WindowBounds, WindowOptions};
use photoslop_plugin_api::{CodecPlugin, PluginManifest, PluginRegistry};
use workspace::Workspace;

/// PSD/PSB import via `photoslop-codec-psd` (write support lands in M6).
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

/// Assemble the first-party plugin set. Every entry here is optional — the
/// app boots (to an empty shell) with any or all of them removed.
fn build_registry() -> PluginRegistry {
    let mut registry = PluginRegistry::new();
    let manifests: Vec<Box<dyn PluginManifest>> = vec![
        Box::new(photoslop_tools_basic::BasicToolsPlugin),
        Box::new(photoslop_tools_paint::PaintToolsPlugin),
        Box::new(photoslop_tools_select::SelectToolsPlugin),
        Box::new(photoslop_commands_core::CoreCommandsPlugin),
        Box::new(photoslop_codecs_common::CommonCodecsPlugin),
        Box::new(PsdPlugin),
    ];
    for manifest in manifests {
        log::info!("loading plugin {}", manifest.id());
        manifest.register(&mut registry);
    }
    registry
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let registry = build_registry();

    Application::new().run(move |cx: &mut App| {
        cx.bind_keys(keymap::build_bindings(&registry));
        let bounds = Bounds::centered(None, size(px(1440.0), px(900.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_window, cx| cx.new(|cx| Workspace::new(registry, cx)),
        )
        .expect("failed to open window");
        cx.activate(true);
    });
}
