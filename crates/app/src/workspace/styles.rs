//! The Layer Style dialog session: preview, commit, revert.

use super::*;

impl Workspace {
    // ----- change propagation -----

    /// Open the Layer Style dialog for the active layer.
    pub fn show_layer_style(&mut self, layer: schist_core::LayerId, cx: &mut Context<Self>) {
        let Some(style) = self
            .doc
            .as_ref()
            .and_then(|d| d.tree.find(layer))
            .map(|l| l.style)
        else {
            return;
        };
        self.open_modal(
            Modal::LayerStyle {
                layer,
                style: Box::new(style),
                original: Box::new(style),
                // Photoshop opens on whatever is on; Drop Shadow otherwise.
                active: crate::style_dialog::EFFECTS
                    .iter()
                    .rev()
                    .find(|(k, _)| style_enabled(&style, k))
                    .map(|(k, _)| *k)
                    .unwrap_or("drop_shadow"),
            },
            cx,
        );
    }

    /// Push the dialog's style onto the layer so the canvas shows it.
    pub fn preview_layer_style(
        &mut self,
        layer: schist_core::LayerId,
        style: schist_core::LayerStyle,
        cx: &mut Context<Self>,
    ) {
        if let Some(doc) = self.doc.as_mut() {
            if let Some(l) = doc.tree.find_mut(layer) {
                l.style = style;
            }
            doc.damage_all();
        }
        self.after_change(cx);
    }

    /// Re-apply the open dialog's style. Used by the controls that fire
    /// without a context of their own.
    pub fn restyle_from_modal(&mut self) {
        let mut next = None;
        if let Some(Modal::LayerStyle { layer, style, .. }) = self.modal.as_ref() {
            next = Some((*layer, **style));
        }
        if let Some((layer, style)) = next {
            if let Some(doc) = self.doc.as_mut() {
                if let Some(l) = doc.tree.find_mut(layer) {
                    l.style = style;
                }
                doc.damage_all();
            }
            self.refresh_layer_styles();
        }
    }

    /// Record the whole dialog session as one history entry.
    pub fn commit_layer_style(&mut self, cx: &mut Context<Self>) {
        let Some(Modal::LayerStyle {
            layer,
            style,
            original,
            ..
        }) = self.modal.clone()
        else {
            return;
        };
        self.modal = None;
        if let Some(doc) = self.doc.as_mut() {
            // Restore the pre-dialog style so the edit records the right
            // "before"; the live preview already moved the layer on.
            if let Some(l) = doc.tree.find_mut(layer) {
                l.style = *original;
            }
            let mut edit = doc.begin_edit("Layer Style");
            edit.record_layer_style(layer, *original, *style);
            edit.commit();
            doc.damage_all();
        }
        self.status = "Layer Style".into();
        self.after_change(cx);
    }

    /// Put the pre-dialog style back (Cancel).
    pub(super) fn revert_layer_style(&mut self) {
        let Some(Modal::LayerStyle {
            layer, original, ..
        }) = self.modal.clone()
        else {
            return;
        };
        if let Some(doc) = self.doc.as_mut() {
            if let Some(l) = doc.tree.find_mut(layer) {
                l.style = *original;
            }
            doc.damage_all();
        }
        self.refresh_layer_styles();
    }
}
