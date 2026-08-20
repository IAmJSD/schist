//! Warp tools: Liquify, Puppet Warp, Content-Aware Scale and Vanishing
//! Point.
//!
//! What they have in common is that they decide where each output pixel
//! comes from rather than what colour it is, which is why they share
//! [`mesh`].

pub mod liquify;
pub mod mesh;
pub mod perspective;
pub mod puppet;
pub mod scale;

use photoslop_plugin_api::{PluginManifest, PluginRegistry};

pub struct WarpToolsPlugin;

impl PluginManifest for WarpToolsPlugin {
    fn id(&self) -> &'static str {
        "photoslop.tools-warp"
    }

    fn register(&self, registry: &mut PluginRegistry) {
        registry.register_tool(Box::new(liquify::LiquifyTool::new()));
        registry.register_tool(Box::new(puppet::PuppetWarpTool::new()));
        registry.register_tool(Box::new(perspective::VanishingPointTool::new()));
    }
}
