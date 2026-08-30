//! Layer comps: capture and restore layer visibility and appearance.

use super::*;

impl Workspace {
    /// Capture every layer's visibility and appearance as a named comp.
    pub fn new_layer_comp(&mut self, cx: &mut Context<Self>) {
        let Some(doc) = self.doc.as_mut() else { return };
        let states: Vec<schist_core::LayerCompState> = doc
            .tree
            .iter()
            .map(|l| schist_core::LayerCompState {
                layer: l.id,
                visible: l.visible,
                opacity: l.opacity,
                fill_opacity: l.fill_opacity,
                blend: l.blend,
                style: l.style,
            })
            .collect();
        let n = doc.layer_comps.len() + 1;
        let mut comp = schist_core::LayerComp::new(format!("Layer Comp {n}"));
        comp.states = states;
        doc.layer_comps.push(comp);
        doc.mark_dirty();
        self.status = "Layer comp captured".into();
        cx.notify();
    }

    /// Restore a comp. Pixels are untouched: a comp is a way of showing
    /// the same artwork several ways, not a second copy of it.
    pub fn apply_layer_comp(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(doc) = self.doc.as_mut() else { return };
        let Some(comp) = doc.layer_comps.get(index).cloned() else {
            return;
        };
        let mut edit = doc.begin_edit(format!("Apply {}", comp.name));
        for state in &comp.states {
            edit.change_props(state.layer, |l| {
                if comp.apply_visibility {
                    l.visible = state.visible;
                }
                if comp.apply_appearance {
                    l.opacity = state.opacity;
                    l.fill_opacity = state.fill_opacity;
                    l.blend = state.blend;
                    l.style = state.style;
                    // The cached raster belongs to the old style.
                    l.styled = None;
                }
            });
        }
        edit.commit();
        self.status = comp.name.into();
        self.after_change(cx);
    }

    pub fn delete_layer_comp(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(doc) = self.doc.as_mut() {
            if index < doc.layer_comps.len() {
                doc.layer_comps.remove(index);
                doc.mark_dirty();
            }
        }
        cx.notify();
    }
}
