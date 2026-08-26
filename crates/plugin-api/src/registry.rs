//! Plugin registries — the kernel's catalog of everything installed.

use crate::{CodecPlugin, Command, CommandPlugin, FilterPlugin, ToolPlugin};
use std::sync::Arc;

/// All registered plugins, assembled at startup by the app shell from each
/// enabled `PluginManifest`.
#[derive(Default)]
pub struct PluginRegistry {
    tools: Vec<Box<dyn ToolPlugin>>,
    /// `Arc`, not `Box`: decoding runs on background threads, which need
    /// to hold the codec beyond the registry borrow.
    codecs: Vec<Arc<dyn CodecPlugin>>,
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

    /// Register a codec, ignoring one whose id is already taken.
    ///
    /// WASM plugins register after the built-ins, so a plugin declaring an
    /// id that already exists never won a `find` lookup but did add a
    /// second, dead entry to the menus with no diagnostic.
    pub fn register_codec(&mut self, codec: Box<dyn CodecPlugin>) {
        if let Some(existing) = self.codecs.iter().find(|c| c.id() == codec.id()) {
            log::warn!(
                "ignoring codec {:?}: that id is already registered by {:?}",
                codec.id(),
                existing.name()
            );
            return;
        }
        self.codecs.push(Arc::from(codec));
    }

    pub fn register_commands(&mut self, plugin: &dyn CommandPlugin) {
        self.commands.extend(plugin.commands());
    }

    /// Register a filter, ignoring one whose id is already taken.
    pub fn register_filter(&mut self, filter: Box<dyn FilterPlugin>) {
        if let Some(existing) = self.filters.iter().find(|f| f.id() == filter.id()) {
            log::warn!(
                "ignoring filter {:?}: that id is already registered by {:?}",
                filter.id(),
                existing.name()
            );
            return;
        }
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

    /// Clones of every codec, for decoding on a background thread.
    pub fn shared_codecs(&self) -> Vec<Arc<dyn CodecPlugin>> {
        self.codecs.clone()
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

#[cfg(test)]
mod duplicate_id_tests {
    use super::*;
    use crate::FilterValues;

    struct Dummy(&'static str);

    impl FilterPlugin for Dummy {
        fn id(&self) -> &'static str {
            self.0
        }
        fn name(&self) -> &'static str {
            "dummy"
        }
        fn apply(
            &self,
            _pixels: &mut [f32],
            _width: usize,
            _height: usize,
            _values: &FilterValues,
        ) {
        }
    }

    #[test]
    fn a_duplicate_filter_id_is_ignored_not_shadowed() {
        // WASM plugins register after the built-ins, so a colliding id
        // never won a `find` lookup but did add a second, dead menu entry.
        let mut reg = PluginRegistry::new();
        reg.register_filter(Box::new(Dummy("filter.same")));
        reg.register_filter(Box::new(Dummy("filter.same")));
        assert_eq!(
            reg.filters().filter(|f| f.id() == "filter.same").count(),
            1,
            "the second registration must be dropped"
        );
    }
}
