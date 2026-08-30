//! The layers panel: row selection, drag-reorder, and inline rename.

use super::*;

impl Workspace {
    // ----- layers panel: selection, drag-reorder, inline rename -----

    /// The rows the layers panel currently shows, top to bottom (closed
    /// groups hide their children). Shift-click ranges select within this.
    pub(super) fn visible_layer_ids(&self) -> Vec<schist_core::LayerId> {
        fn walk(layers: &[Layer], out: &mut Vec<schist_core::LayerId>) {
            for layer in layers.iter().rev() {
                out.push(layer.id);
                if let schist_core::LayerKind::Group(g) = &layer.kind {
                    if g.open {
                        walk(&g.children, out);
                    }
                }
            }
        }
        let mut out = Vec::new();
        if let Some(doc) = &self.doc {
            walk(&doc.tree.layers, &mut out);
        }
        out
    }

    pub fn record_layer_row_bounds(&mut self, id: schist_core::LayerId, bounds: Bounds<Pixels>) {
        self.layer_row_bounds.insert(id, bounds);
    }

    /// Mouse down on a layer row: update the selection (plain click,
    /// shift range or ctrl/cmd toggle) and arm a possible drag.
    pub fn layer_row_mouse_down(
        &mut self,
        id: schist_core::LayerId,
        ev: &MouseDownEvent,
        cx: &mut Context<Self>,
    ) {
        self.commit_layer_rename(cx);
        let visible = self.visible_layer_ids();
        let anchor = self.layer_anchor;
        let Some(doc) = self.doc.as_mut() else {
            return;
        };
        let mut selection = doc.selected_layers();
        let mut collapse = false;
        if ev.modifiers.shift {
            // Select the range between the anchor (the last plain click)
            // and this row; the anchor stays put for further shift-clicks.
            let anchor = anchor
                .filter(|a| visible.contains(a))
                .or(doc.active_layer)
                .unwrap_or(id);
            let a = visible.iter().position(|&v| v == anchor).unwrap_or(0);
            let b = visible.iter().position(|&v| v == id).unwrap_or(a);
            let (lo, hi) = (a.min(b), a.max(b));
            doc.selected = visible[lo..=hi].to_vec();
            doc.active_layer = Some(id);
        } else if ev.modifiers.secondary() {
            // Toggle this row in or out; never toggle out the last one.
            if let Some(ix) = selection.iter().position(|&s| s == id) {
                if selection.len() > 1 {
                    selection.remove(ix);
                    if doc.active_layer == Some(id) {
                        doc.active_layer = selection.last().copied();
                    }
                    doc.selected = selection;
                }
            } else {
                selection.push(id);
                doc.selected = selection;
                doc.active_layer = Some(id);
                self.layer_anchor = Some(id);
            }
        } else if selection.len() > 1 && selection.contains(&id) {
            // Pressing inside a multi-selection keeps it, so the whole
            // thing can be dragged; a plain release collapses it.
            doc.active_layer = Some(id);
            collapse = true;
        } else {
            doc.selected = vec![id];
            doc.active_layer = Some(id);
            self.layer_anchor = Some(id);
        }
        self.layer_drag = Some(LayerDrag {
            layer: id,
            start: ev.position,
            active: false,
            collapse,
        });
        self.layer_drop = None;
        cx.notify();
    }

    /// The layers a drag moves: the multi-selection when the pressed row
    /// is part of it, otherwise just the pressed row — in panel order
    /// (top first), minus any layer whose ancestor is also moving.
    pub(super) fn dragged_layers(&self) -> Vec<schist_core::LayerId> {
        let (Some(drag), Some(doc)) = (&self.layer_drag, &self.doc) else {
            return Vec::new();
        };
        let mut ids = doc.selected_layers();
        if !ids.contains(&drag.layer) {
            ids = vec![drag.layer];
        }
        let mut paths: Vec<(schist_core::LayerId, schist_core::LayerPath)> = ids
            .iter()
            .filter_map(|&id| doc.tree.path_of(id).map(|p| (id, p)))
            .collect();
        // Siblings render top-of-stack first, so descending path order
        // is panel order.
        paths.sort_by(|a, b| b.1 .0.cmp(&a.1 .0));
        let roots: Vec<bool> = paths
            .iter()
            .map(|(_, p)| {
                !paths
                    .iter()
                    .any(|(_, q)| q.0.len() < p.0.len() && p.0[..q.0.len()] == q.0[..])
            })
            .collect();
        paths
            .into_iter()
            .zip(roots)
            .filter_map(|((id, _), root)| root.then_some(id))
            .collect()
    }

    /// Mouse move over a layer row with the button down: past a small
    /// threshold the press becomes a drag, and the row under the cursor
    /// becomes the drop target — top half above, bottom half below, and
    /// the middle of a group row drops into the group.
    pub fn layer_row_mouse_move(
        &mut self,
        id: schist_core::LayerId,
        ev: &MouseMoveEvent,
        cx: &mut Context<Self>,
    ) {
        if ev.pressed_button != Some(MouseButton::Left) {
            // The release happened outside the panel; forget the drag.
            if self.layer_drag.take().is_some() {
                self.layer_drop = None;
                cx.notify();
            }
            return;
        }
        let Some(drag) = self.layer_drag.as_mut() else {
            return;
        };
        if !drag.active {
            let dx = f32::from(ev.position.x - drag.start.x).abs();
            let dy = f32::from(ev.position.y - drag.start.y).abs();
            if dx.max(dy) < 4.0 {
                return;
            }
            drag.active = true;
        }
        let drop = if self.dragged_layers().contains(&id) {
            None
        } else {
            let is_group = self
                .doc
                .as_ref()
                .and_then(|d| d.tree.find(id))
                .is_some_and(|l| matches!(l.kind, schist_core::LayerKind::Group(_)));
            let frac = self
                .layer_row_bounds
                .get(&id)
                .map(|b| {
                    (f32::from(ev.position.y) - f32::from(b.origin.y))
                        / f32::from(b.size.height).max(1.0)
                })
                .unwrap_or(0.5);
            Some(if is_group {
                if frac < 0.3 {
                    LayerDrop::Above(id)
                } else if frac > 0.7 {
                    LayerDrop::Below(id)
                } else {
                    LayerDrop::Into(id)
                }
            } else if frac < 0.5 {
                LayerDrop::Above(id)
            } else {
                LayerDrop::Below(id)
            })
        };
        if self.layer_drop != drop {
            self.layer_drop = drop;
            cx.notify();
        }
    }

    /// Mouse released after a layer-row press: commit the drop if this
    /// was a drag, otherwise collapse a kept multi-selection down to the
    /// pressed row.
    pub fn finish_layer_drag(&mut self, cx: &mut Context<Self>) {
        let moving = self.dragged_layers();
        let Some(drag) = self.layer_drag.take() else {
            return;
        };
        let drop = self.layer_drop.take();
        if drag.active {
            if let Some(drop) = drop {
                self.drop_layers(moving, drop, cx);
            }
        } else if drag.collapse {
            if let Some(doc) = self.doc.as_mut() {
                doc.selected = vec![drag.layer];
                doc.active_layer = Some(drag.layer);
            }
            self.layer_anchor = Some(drag.layer);
        }
        cx.notify();
    }

    /// Move the dragged layers to the drop position, as one undo step.
    /// Layers keep their panel order: the first lands at the drop point
    /// and each next one goes directly below the previous.
    pub(super) fn drop_layers(
        &mut self,
        moving: Vec<schist_core::LayerId>,
        drop: LayerDrop,
        cx: &mut Context<Self>,
    ) {
        let Some(doc) = self.doc.as_mut() else {
            return;
        };
        let target = match drop {
            LayerDrop::Above(t) | LayerDrop::Below(t) | LayerDrop::Into(t) => t,
        };
        if moving.is_empty() || moving.contains(&target) {
            return;
        }
        let Some(target_path) = doc.tree.path_of(target) else {
            return;
        };
        // A layer cannot move into its own subtree.
        let dest_parent: &[usize] = match drop {
            LayerDrop::Into(_) => &target_path.0,
            _ => &target_path.0[..target_path.0.len() - 1],
        };
        for id in &moving {
            let Some(p) = doc.tree.path_of(*id) else {
                return;
            };
            if dest_parent.len() >= p.0.len() && dest_parent[..p.0.len()] == p.0[..] {
                return;
            }
        }
        let mut edit = doc.begin_edit(if moving.len() > 1 {
            "Move Layers"
        } else {
            "Move Layer"
        });
        let mut prev: Option<schist_core::LayerId> = None;
        for id in moving {
            let Some(from) = edit.doc().tree.path_of(id) else {
                continue;
            };
            // Destination in pre-removal coordinates.
            let mut to = match (prev, drop) {
                // Each later layer lands directly below the previous one.
                (Some(p), _) => match edit.doc().tree.path_of(p) {
                    Some(pp) => pp,
                    None => continue,
                },
                (None, LayerDrop::Above(t)) => {
                    let Some(mut tp) = edit.doc().tree.path_of(t) else {
                        break;
                    };
                    *tp.0.last_mut().unwrap() += 1;
                    tp
                }
                (None, LayerDrop::Below(t)) => match edit.doc().tree.path_of(t) {
                    Some(tp) => tp,
                    None => break,
                },
                (None, LayerDrop::Into(g)) => {
                    let Some(mut gp) = edit.doc().tree.path_of(g) else {
                        break;
                    };
                    let top = edit
                        .doc()
                        .tree
                        .find(g)
                        .and_then(|l| l.children())
                        .map_or(0, |c| c.len());
                    gp.0.push(top);
                    gp
                }
            };
            // `move_layer` removes then reinserts, so a removal earlier in
            // the same sibling vec shifts the destination down by one.
            let d = from.0.len() - 1;
            if to.0.len() > d && to.0[..d] == from.0[..d] && to.0[d] > from.0[d] {
                to.0[d] -= 1;
            }
            if to != from {
                edit.move_layer(from, to);
            }
            prev = Some(id);
        }
        edit.commit();
        self.after_change(cx);
    }

    /// Start renaming a layer inline (double-click on its name).
    pub fn begin_layer_rename(&mut self, id: schist_core::LayerId, cx: &mut Context<Self>) {
        let Some(name) = self
            .doc
            .as_ref()
            .and_then(|d| d.tree.find(id))
            .map(|l| l.name.clone())
        else {
            return;
        };
        self.layer_rename = Some((id, name));
        self.layer_drag = None;
        self.layer_drop = None;
        self.focused_field = None;
        cx.notify();
    }

    pub fn commit_layer_rename(&mut self, cx: &mut Context<Self>) {
        if let Some((id, name)) = self.layer_rename.take() {
            // An emptied field keeps the old name: rename_layer rejects
            // blank names.
            self.rename_layer(id, name, cx);
            cx.notify();
        }
    }

    pub fn cancel_layer_rename(&mut self, cx: &mut Context<Self>) {
        if self.layer_rename.take().is_some() {
            cx.notify();
        }
    }

    /// Feed a keystroke to an inline layer rename. Consumes every key
    /// while one is open so shortcuts can't fire mid-typing.
    pub fn layer_rename_key(&mut self, ev: &gpui::KeyDownEvent, cx: &mut Context<Self>) -> bool {
        if self.layer_rename.is_none() {
            return false;
        }
        match ev.keystroke.key.as_str() {
            "enter" | "tab" => self.commit_layer_rename(cx),
            "escape" => self.cancel_layer_rename(cx),
            "backspace" => {
                if let Some((_, name)) = self.layer_rename.as_mut() {
                    name.pop();
                }
            }
            "space" => {
                if let Some((_, name)) = self.layer_rename.as_mut() {
                    name.push(' ');
                }
            }
            _ => {
                if let (Some((_, name)), Some(t)) =
                    (self.layer_rename.as_mut(), ev.keystroke.key_char.as_deref())
                {
                    if !t.is_empty() && !t.chars().any(char::is_control) {
                        name.push_str(t);
                    }
                }
            }
        }
        cx.notify();
        true
    }
}
