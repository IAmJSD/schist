//! Filter parameter dialogs and adjustment layers.

use super::*;

impl Workspace {
    /// Open a filter's parameter dialog, pre-filled with its defaults.
    pub fn open_filter_dialog(&mut self, id: &'static str, cx: &mut Context<Self>) {
        let Some(filter) = self.registry.filters().find(|f| f.id() == id) else {
            return;
        };
        let mut values = schist_plugin_api::FilterValues::defaults(&filter.params());
        self.seed_raw_filter_values(id, &mut values);
        // Filters with no parameters just run.
        if values.0.is_empty() {
            self.apply_filter(id, &values, cx);
            return;
        }
        let begun = if self.is_raw_redevelopment(id) {
            self.begin_raw_filter_preview()
        } else {
            self.begin_filter_preview()
        };
        if !begun {
            cx.notify();
            return;
        }
        self.preview_filter(id, Some(&values), cx);
        self.open_modal(
            Modal::Filter {
                id,
                values,
                preview: true,
                map: None,
            },
            cx,
        );
    }

    /// Insert an adjustment layer above the active layer.
    pub fn add_adjustment(&mut self, kind: schist_core::AdjustmentKind, cx: &mut Context<Self>) {
        let params = schist_adjustments::Params::default_for(kind);
        let Some(doc) = self.doc.as_mut() else { return };
        let mut layer = Layer::new_raster(kind.display_name());
        layer.kind = schist_core::LayerKind::Adjustment(schist_core::AdjustmentData {
            kind,
            raw: Vec::new(),
            params_json: serde_json::to_string(&params).ok(),
        });
        let id = layer.id;
        let path = match doc.active_layer.and_then(|a| doc.tree.path_of(a)) {
            Some(mut p) => {
                *p.0.last_mut().unwrap() += 1;
                p
            }
            None => schist_core::LayerPath(vec![doc.tree.layers.len()]),
        };
        let mut edit = doc.begin_edit(format!("New {} Layer", kind.display_name()));
        edit.insert_layer(path, layer);
        edit.commit();
        doc.active_layer = Some(id);
        self.status = format!("Added {}", kind.display_name()).into();
        self.after_change(cx);
        // Anything with controls opens its dialog straight away.
        if !params.param_specs().is_empty() {
            let original = (serde_json::to_string(&params).ok(), Vec::new());
            self.open_modal(
                Modal::Adjustment {
                    layer: id,
                    params,
                    original,
                },
                cx,
            );
        }
    }

    /// Write edited parameters back onto an adjustment layer (live, without
    /// a history entry per slider tick).
    pub fn preview_adjustment(
        &mut self,
        layer: schist_core::LayerId,
        params: &schist_adjustments::Params,
    ) {
        let Some(doc) = self.doc.as_mut() else { return };
        if let Some(schist_core::LayerKind::Adjustment(data)) =
            doc.tree.find_mut(layer).map(|l| &mut l.kind)
        {
            data.params_json = serde_json::to_string(params).ok();
        }
        doc.damage_all();
    }

    /// Commit edited adjustment parameters as one history entry.
    pub fn commit_adjustment(
        &mut self,
        layer: schist_core::LayerId,
        params: &schist_adjustments::Params,
        original: (Option<String>, Vec<u8>),
        cx: &mut Context<Self>,
    ) {
        let after = (serde_json::to_string(params).ok(), Vec::new());
        if let Some(doc) = self.doc.as_mut() {
            // Put the pre-dialog state back first so the recorded edit has
            // the right "before"; the live preview already moved the layer.
            if let Some(schist_core::LayerKind::Adjustment(data)) =
                doc.tree.find_mut(layer).map(|l| &mut l.kind)
            {
                data.params_json = original.0.clone();
                data.raw = original.1.clone();
            }
            let mut edit = doc.begin_edit(format!("{} Settings", params.display_name()));
            // Editing parameters supersedes the preserved PSD payload, so
            // the writer emits our values rather than stale bytes.
            edit.record_adjustment_params(layer, original, after);
            edit.commit();
        }
        self.status = format!("{} updated", params.display_name()).into();
        self.after_change(cx);
    }

    /// Discard a live adjustment preview (dialog cancelled).
    pub fn revert_adjustment(
        &mut self,
        layer: schist_core::LayerId,
        original: (Option<String>, Vec<u8>),
        cx: &mut Context<Self>,
    ) {
        if let Some(doc) = self.doc.as_mut() {
            if let Some(schist_core::LayerKind::Adjustment(data)) =
                doc.tree.find_mut(layer).map(|l| &mut l.kind)
            {
                data.params_json = original.0;
                data.raw = original.1;
            }
            doc.damage_all();
        }
        self.after_change(cx);
    }

    /// Open the parameter dialog for an existing adjustment layer.
    pub fn edit_adjustment(&mut self, layer: schist_core::LayerId, cx: &mut Context<Self>) {
        let Some(doc) = self.doc.as_ref() else { return };
        let Some(schist_core::LayerKind::Adjustment(data)) = doc.tree.find(layer).map(|l| &l.kind)
        else {
            return;
        };
        let params = data
            .params_json
            .as_deref()
            .and_then(|j| serde_json::from_str(j).ok())
            .unwrap_or_else(|| schist_adjustments::parse_psd(data.kind, &data.raw));
        if params.param_specs().is_empty() {
            self.status = format!("{} has no editable settings", params.display_name()).into();
            return;
        }
        let original = (data.params_json.clone(), data.raw.clone());
        self.open_modal(
            Modal::Adjustment {
                layer,
                params,
                original,
            },
            cx,
        );
    }
}
