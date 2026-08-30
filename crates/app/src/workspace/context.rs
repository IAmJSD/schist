//! Right-click context menus and layer properties.

use super::*;

impl Workspace {
    // ----- context menus -----

    /// Open a right-click menu at `position`.
    pub fn open_context_menu(
        &mut self,
        target: ContextTarget,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.commit_layer_rename(cx);
        // Right-clicking a layer selects it first, like Photoshop. If the
        // row is already in the multi-selection, the selection is kept
        // (`selected_layers` sees the active layer still inside it).
        if let (ContextTarget::Layer(id), Some(doc)) = (target, self.doc.as_mut()) {
            doc.active_layer = Some(id);
        }
        self.context_menu = Some(ContextMenu { position, target });
        self.open_popup = None;
        cx.notify();
    }

    pub fn close_context_menu(&mut self, cx: &mut Context<Self>) {
        if self.context_menu.take().is_some() {
            cx.notify();
        }
    }

    /// Open Layer Properties for the given layer.
    pub fn open_layer_properties(&mut self, layer: schist_core::LayerId, cx: &mut Context<Self>) {
        let Some(name) = self
            .doc
            .as_ref()
            .and_then(|d| d.tree.find(layer))
            .map(|l| l.name.clone())
        else {
            return;
        };
        self.open_modal(Modal::LayerProperties { layer, name }, cx);
    }

    /// Commit a rename from the Layer Properties dialog.
    pub fn rename_layer(
        &mut self,
        layer: schist_core::LayerId,
        name: String,
        cx: &mut Context<Self>,
    ) {
        if let Some(doc) = self.doc.as_mut() {
            let trimmed = name.trim();
            if trimmed.is_empty() {
                return;
            }
            let mut edit = doc.begin_edit("Rename Layer");
            edit.change_props(layer, |l| l.name = trimmed.to_string());
            edit.commit();
        }
        self.after_change(cx);
    }
}
