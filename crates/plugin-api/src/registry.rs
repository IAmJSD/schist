//! Plugin registries — the kernel's catalog of everything installed.

use crate::{CodecPlugin, Command, CommandPlugin, FilterPlugin, ToolPlugin};

/// All registered plugins, assembled at startup by the app shell from each
/// enabled `PluginManifest`.
#[derive(Default)]
pub struct PluginRegistry {
    tools: Vec<Box<dyn ToolPlugin>>,
    codecs: Vec<Box<dyn CodecPlugin>>,
    commands: Vec<Command>,
    filters: Vec<Box<dyn FilterPlugin>>,
}

impl PluginRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register_tool(&mut self, tool: Box<dyn ToolPlugin>) {
        debug_assert!(
            !self.tools.iter().any(|t| t.id() == tool.id()),
            "duplicate tool id {}",
            tool.id()
        );
        self.tools.push(tool);
    }

    pub fn register_codec(&mut self, codec: Box<dyn CodecPlugin>) {
        self.codecs.push(codec);
    }

    pub fn register_commands(&mut self, plugin: &dyn CommandPlugin) {
        self.commands.extend(plugin.commands());
    }

    pub fn register_filter(&mut self, filter: Box<dyn FilterPlugin>) {
        self.filters.push(filter);
    }

    pub fn tools(&self) -> impl Iterator<Item = &dyn ToolPlugin> {
        self.tools.iter().map(|t| t.as_ref())
    }

    pub fn tool_mut(&mut self, id: &str) -> Option<&mut Box<dyn ToolPlugin>> {
        self.tools.iter_mut().find(|t| t.id() == id)
    }

    pub fn tool_ids(&self) -> Vec<&'static str> {
        self.tools.iter().map(|t| t.id()).collect()
    }

    pub fn codecs(&self) -> impl Iterator<Item = &dyn CodecPlugin> {
        self.codecs.iter().map(|c| c.as_ref())
    }

    /// Find a codec by sniffing bytes, falling back to file extension.
    pub fn codec_for(&self, bytes: &[u8], extension: Option<&str>) -> Option<&dyn CodecPlugin> {
        self.codecs
            .iter()
            .find(|c| c.probe(bytes))
            .or_else(|| {
                let ext = extension?.to_ascii_lowercase();
                self.codecs
                    .iter()
                    .find(|c| c.extensions().contains(&ext.as_str()))
            })
            .map(|c| c.as_ref())
    }

    pub fn commands(&self) -> &[Command] {
        &self.commands
    }

    pub fn command(&self, id: &str) -> Option<&Command> {
        self.commands.iter().find(|c| c.id == id)
    }

    pub fn filters(&self) -> impl Iterator<Item = &dyn FilterPlugin> {
        self.filters.iter().map(|f| f.as_ref())
    }
}

/// One plugin crate's entry point.
pub trait PluginManifest {
    fn id(&self) -> &'static str;
    fn register(&self, registry: &mut PluginRegistry);
}
