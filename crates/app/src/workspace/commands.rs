//! Dispatching commands and tools, and the post-change refresh every
//! edit funnels through.

use super::*;

impl Workspace {
    /// Re-rasterize any layer whose effects are stale.
    ///
    /// The styled raster is derived from the layer's pixels plus its
    /// style, so it has to be rebuilt whenever either moves. Layers with
    /// no effects keep `styled == None` and cost nothing.
    pub(super) fn refresh_layer_styles(&mut self) {
        let Some(doc) = self.doc.as_mut() else { return };
        let mut grew = Vec::new();
        // Shape layers first: their pixels are derived from their path,
        // and any effects are derived from those pixels in turn.
        let depth = doc.depth;
        let canvas = doc.canvas_rect();
        reshape_layers(&mut doc.tree.layers, depth, canvas, &mut grew);
        schist_compositor::restyle_layers(&mut doc.tree.layers, &mut grew);
        // A shadow can appear outside the layer's old bounds, so the
        // newly covered area has to be repainted too.
        for rect in grew {
            doc.add_damage(rect);
        }
    }

    pub fn after_change(&mut self, cx: &mut Context<Self>) {
        self.refresh_layer_styles();
        if let Some(doc) = &mut self.doc {
            let damage = doc.take_damage();
            for rect in &damage {
                self.cache.invalidate(rect);
                for coord in TileCoord::covering(rect) {
                    self.display_tiles.remove(&coord);
                }
                // The preview only drains this list when it is the active
                // render path (far zoom-out), so at working zooms a long
                // stroke would grow it forever. Past a threshold, collapse
                // to "rebuild everything on next refresh".
                if self.preview.dirty.len() >= 256 {
                    self.preview.dirty.clear();
                    self.preview.valid = false;
                } else {
                    self.preview.dirty.push(*rect);
                }
            }
        }
        cx.notify();
    }

    pub fn run_command(&mut self, id: &str, cx: &mut Context<Self>) {
        // Grow, Similar and Color Range take their tolerance from the
        // magic wand, exactly as Photoshop does.
        self.sync_wand_tolerance();
        // Pasting prefers whatever is on the system clipboard, so copying
        // in another application and pasting here works. If there is no
        // image there, the internal clipboard is used unchanged.
        if id.starts_with("edit.paste") {
            self.sync_clipboard_in(cx);
        }
        let profile_before = self.doc.as_ref().and_then(|doc| doc.icc_profile.clone());
        let Some(doc) = self.doc.as_mut() else { return };
        if let Some(command) = self.registry.command(id) {
            let mut ctx = CommandCtx {
                doc,
                state: &mut self.editor,
                refusal: None,
            };
            (command.run)(&mut ctx);
            // Reporting the command's own title regardless meant every
            // silent no-op looked like it had worked.
            self.status = match ctx.refusal {
                Some(why) => why.into(),
                None => command.title.into(),
            };
            // ...and copying makes the pixels available everywhere else.
            if id.starts_with("edit.copy") || id == "edit.cut" {
                self.sync_clipboard_out(cx);
            }
        } else {
            log::warn!("unknown command {id}");
        }
        // Undo and redo are plugin commands, so a profile restored by
        // their history operation must rebuild the cached display hop.
        // Damage alone only drops pixels rendered through that cache.
        if self.doc.as_ref().and_then(|doc| doc.icc_profile.clone()) != profile_before {
            self.rebuild_color_transforms();
        }
        self.after_change(cx);
    }

    /// Mirror the magic wand's tolerance into the shared editor state.
    pub fn sync_wand_tolerance(&mut self) {
        if let Some(t) = self
            .registry
            .tools()
            .find(|t| t.id() == "wand")
            .and_then(|t| t.options().into_iter().find(|o| o.key == "wand-tolerance"))
        {
            self.editor.tolerance = t.value.num().round().clamp(0.0, 255.0) as u8;
        }
    }

    pub fn activate_tool(&mut self, id: &str, cx: &mut Context<Self>) {
        let previous = self.editor.active_tool;
        if previous != id {
            if let (Some(doc), Some(tool)) = (self.doc.as_mut(), self.registry.tool_mut(previous)) {
                let mut ctx = ToolCtx {
                    doc,
                    state: &mut self.editor,
                };
                tool.on_deactivate(&mut ctx);
            }
        }
        if let Some(tool) = self.registry.tool_mut(id) {
            let id = tool.id();
            let name = tool.name();
            let group = tool.group();
            self.group_active.insert(group, id);
            self.editor.active_tool = id;
            self.status = format!("Tool: {name}").into();
            if let (Some(doc), Some(tool)) = (self.doc.as_mut(), self.registry.tool_mut(id)) {
                let mut ctx = ToolCtx {
                    doc,
                    state: &mut self.editor,
                };
                tool.on_activate(&mut ctx);
            }
        }
        self.after_change(cx);
    }

    /// Apply an options-bar change to the active tool, then let it react
    /// with the document available.
    pub fn set_tool_option(
        &mut self,
        key: &'static str,
        value: schist_plugin_api::OptionValue,
        cx: &mut Context<Self>,
    ) {
        let tool_id = self.editor.active_tool;
        let Some(tool) = self.registry.tool_mut(tool_id) else {
            return;
        };
        tool.set_option(key, value);
        if let (Some(doc), Some(tool)) = (self.doc.as_mut(), self.registry.tool_mut(tool_id)) {
            let mut ctx = ToolCtx {
                doc,
                state: &mut self.editor,
            };
            tool.on_option_changed(&mut ctx, key);
        }
        self.after_change(cx);
    }
}
