//! The layers panel: rows, blend mode and opacity controls.

use super::*;

pub(super) struct LayerRow {
    id: LayerId,
    depth: usize,
    kind: RowKind,
    name: String,
    visible: bool,
    active: bool,
    /// In the multi-selection but not the active layer.
    selected: bool,
    open: bool,
    /// The layer has effects switched on, shown as Photoshop's fx badge.
    fx: bool,
    /// The layer is a smart object.
    smart: bool,
}

pub(super) enum RowKind {
    Raster,
    Group,
    Adjustment,
}

pub(super) fn flatten_layers(
    layers: &[Layer],
    depth: usize,
    active: Option<LayerId>,
    selected: &[LayerId],
    out: &mut Vec<LayerRow>,
) {
    for layer in layers.iter().rev() {
        let (kind, open) = match &layer.kind {
            LayerKind::Group(g) => (RowKind::Group, g.open),
            LayerKind::Adjustment(_) => (RowKind::Adjustment, false),
            LayerKind::Raster(_) => (RowKind::Raster, false),
        };
        out.push(LayerRow {
            id: layer.id,
            depth,
            kind,
            name: layer.name.clone(),
            visible: layer.visible,
            active: Some(layer.id) == active,
            selected: Some(layer.id) != active && selected.contains(&layer.id),
            open,
            fx: !layer.style.is_empty(),
            smart: layer.smart.is_some(),
        });
        if let LayerKind::Group(g) = &layer.kind {
            if g.open {
                flatten_layers(&g.children, depth + 1, active, selected, out);
            }
        }
    }
}

pub(super) fn blend_mode_control(
    ws: &mut Workspace,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let active_layer = ws.doc.as_ref().and_then(|d| d.active_layer);
    let current = active_layer
        .and_then(|id| ws.doc.as_ref().and_then(|d| d.tree.find(id)))
        .map(|l| l.blend)
        .unwrap_or(BlendMode::Normal);
    let is_open = ws.open_popup == Some(Popup::BlendModes);
    let mut button = div()
        .relative()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .flex_grow()
        .h(px(20.0))
        .px_1()
        .rounded_sm()
        .bg(gpui::rgb(palette().field_bg))
        .text_size(px(11.0))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|ws, _e, _w, cx| ws.toggle_popup(Popup::BlendModes, cx)),
        )
        .child(current.display_name())
        .child(icon("chevron-down", 11.0, palette().text_dim));
    if is_open {
        if let Some(layer_id) = active_layer {
            let rows: Vec<gpui::AnyElement> = BlendMode::layer_modes()
                .iter()
                .map(|&mode| {
                    let selected = mode == current;
                    div()
                        .px_2()
                        .h(px(20.0))
                        .flex_none()
                        .flex()
                        .items_center()
                        .text_size(px(11.0))
                        .when_active(selected)
                        .hover(move |s| {
                            if selected {
                                s
                            } else {
                                s.bg(gpui::rgb(palette().hover))
                            }
                        })
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |ws, _e, _w, cx| {
                                ws.close_popup(cx);
                                ws.set_blend_mode(layer_id, mode, cx);
                            }),
                        )
                        .child(mode.display_name())
                        .into_any_element()
                })
                .collect();
            button = button.child(deferred(
                div()
                    .id("blend-modes")
                    .absolute()
                    .top(px(22.0))
                    .left_0()
                    .w(px(150.0))
                    .max_h(px(320.0))
                    .overflow_y_scroll()
                    .py_1()
                    .bg(gpui::rgb(palette().popup_bg))
                    .text_color(gpui::rgb(palette().text))
                    .border_1()
                    .border_color(gpui::rgb(palette().edge))
                    .rounded_sm()
                    .shadow_lg()
                    .occlude()
                    .on_mouse_down_out(cx.listener(|ws, _e, _w, cx| ws.close_popup(cx)))
                    .children(rows),
            ));
        }
    }
    button
}

pub(super) fn layers_panel(ws: &mut Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let mut rows = Vec::new();
    let active_layer = ws.doc.as_ref().and_then(|d| d.active_layer);
    if let Some(doc) = &ws.doc {
        flatten_layers(
            &doc.tree.layers,
            0,
            doc.active_layer,
            &doc.selected_layers(),
            &mut rows,
        );
    }
    // Drop indicator while rows are being dragged, and any rename field.
    let layer_drop = ws.layer_drop;
    let rename = ws.layer_rename.clone();
    let thumbs: Vec<Option<Arc<RenderImage>>> =
        rows.iter().map(|r| ws.layer_thumbnail(r.id)).collect();
    let opacity_display = active_layer
        .map(|id| slider_get(ws, SliderTarget::LayerOpacity(id)))
        .map(|v| format!("{:.0}%", v * 100.0))
        .unwrap_or_default();

    div()
        .flex()
        .flex_col()
        .flex_grow()
        // A floor, not zero: the side column scrolls now, and a growing
        // panel with no floor would collapse to nothing under a tall
        // Info tab instead of pushing the column past the window.
        .min_h(px(220.0))
        .p_2()
        .gap_1()
        .border_t_1()
        .border_color(gpui::rgb(palette().panel_edge))
        .child(panel_title("Layers"))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(blend_mode_control(ws, cx))
                .child(match active_layer {
                    Some(id) => slider(
                        "layer-opacity",
                        "",
                        opacity_display,
                        SliderTarget::LayerOpacity(id),
                        ws,
                        cx,
                    )
                    .into_any_element(),
                    None => div().into_any_element(),
                }),
        )
        .child(
            div()
                .id("layers-scroll")
                .flex()
                .flex_col()
                .flex_grow()
                .min_h(px(0.0))
                .overflow_y_scroll()
                .children(rows.into_iter().zip(thumbs).map(|(row, thumb)| {
                    let id = row.id;
                    let is_active_row = row.active;
                    let is_selected_row = row.selected;
                    let entity = cx.entity();
                    let mut base = div()
                        .relative()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_1()
                        .px_1()
                        .h(px(34.0))
                        .flex_none()
                        .rounded_sm()
                        .when_active(row.active);
                    if row.selected {
                        base = base.bg(gpui::rgb(palette().selection_bg));
                    }
                    base.hover(move |s| {
                        if is_active_row || is_selected_row {
                            s
                        } else {
                            s.bg(gpui::rgb(palette().hover))
                        }
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |ws, ev: &MouseDownEvent, _w, cx| {
                            ws.layer_row_mouse_down(id, ev, cx);
                        }),
                    )
                    .on_mouse_move(cx.listener(move |ws, ev: &MouseMoveEvent, _w, cx| {
                        ws.layer_row_mouse_move(id, ev, cx);
                    }))
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(move |ws, _ev: &MouseUpEvent, _w, cx| {
                            ws.finish_layer_drag(cx);
                        }),
                    )
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |ws, ev: &MouseDownEvent, _w, cx| {
                            ws.open_context_menu(ContextTarget::Layer(id), ev.position, cx);
                        }),
                    )
                    .child(
                        // Row bounds, so a drag knows which half of the
                        // row the pointer is in.
                        canvas(
                            move |bounds, _window, cx| {
                                entity.update(cx, |ws, _| ws.record_layer_row_bounds(id, bounds));
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .size_full(),
                    )
                    .child(
                        // Visibility eye.
                        div()
                            .flex()
                            .items_center()
                            .justify_center()
                            .size(px(20.0))
                            .flex_none()
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |ws, _e, _w, cx| {
                                    if let Some(doc) = &mut ws.doc {
                                        let mut edit = doc.begin_edit("Toggle Visibility");
                                        edit.change_props(id, |l| l.visible = !l.visible);
                                        edit.commit();
                                    }
                                    ws.after_change(cx);
                                    cx.stop_propagation();
                                }),
                            )
                            .child(icon(
                                if row.visible { "eye" } else { "eye-off" },
                                13.0,
                                if row.visible {
                                    palette().text
                                } else {
                                    palette().text_dim
                                },
                            )),
                    )
                    .child(div().w(px(row.depth as f32 * 12.0)).flex_none())
                    .child(match &row.kind {
                        RowKind::Group => div()
                            .flex()
                            .items_center()
                            .justify_center()
                            .size(px(16.0))
                            .flex_none()
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |ws, _e, _w, cx| {
                                    ws.toggle_group_open(id, cx);
                                    cx.stop_propagation();
                                }),
                            )
                            .child(icon(
                                if row.open {
                                    "chevron-down"
                                } else {
                                    "chevron-right"
                                },
                                11.0,
                                palette().text_dim,
                            ))
                            .into_any_element(),
                        _ => div().w(px(0.0)).into_any_element(),
                    })
                    .child(
                        // Thumbnail (raster) or type icon.
                        div()
                            .flex()
                            .items_center()
                            .justify_center()
                            .w(px(38.0))
                            .h(px(30.0))
                            .flex_none()
                            .bg(gpui::rgb(palette().field_bg))
                            .rounded_sm()
                            // Adjustment layers open their settings
                            // from the thumbnail, like Photoshop.
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |ws, _e, _w, cx| ws.edit_adjustment(id, cx)),
                            )
                            .child(match (&row.kind, thumb) {
                                (RowKind::Raster, Some(t)) => {
                                    img(t).max_w(px(36.0)).max_h(px(28.0)).into_any_element()
                                }
                                (RowKind::Group, _) => {
                                    icon("folder", 14.0, palette().text_dim).into_any_element()
                                }
                                _ => icon("adjust", 13.0, palette().text_dim).into_any_element(),
                            }),
                    )
                    .child(match &rename {
                        // Inline rename: an editable field with the
                        // dialogs' caret convention.
                        Some((rid, buffer)) if *rid == id => div()
                            .flex_grow()
                            .h(px(22.0))
                            .px_1()
                            .flex()
                            .items_center()
                            .rounded_sm()
                            .bg(gpui::rgb(palette().field_bg))
                            .border_1()
                            .border_color(gpui::rgb(palette().accent))
                            .text_size(px(12.0))
                            .text_color(gpui::rgb(palette().text))
                            .child(format!("{buffer}|"))
                            .into_any_element(),
                        _ => div()
                            .flex_grow()
                            .text_size(px(12.0))
                            .overflow_hidden()
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |ws, ev: &MouseDownEvent, _w, cx| {
                                    if ev.click_count >= 2 {
                                        ws.begin_layer_rename(id, cx);
                                        cx.stop_propagation();
                                    }
                                }),
                            )
                            .child(row.name)
                            .into_any_element(),
                    })
                    .children(row.smart.then(|| {
                        div()
                            .flex_none()
                            .px_1()
                            .rounded_sm()
                            .text_size(px(10.0))
                            .text_color(gpui::rgb(palette().text_dim))
                            .bg(gpui::rgb(palette().control_bg))
                            .child("SO")
                    }))
                    .children(row.fx.then(|| {
                        div()
                            .flex_none()
                            .px_1()
                            .rounded_sm()
                            .text_size(px(10.0))
                            .text_color(gpui::rgb(palette().text_dim))
                            .bg(gpui::rgb(palette().control_bg))
                            .child("fx")
                    }))
                    // Drop indicators while a row drag is in flight: a
                    // bar on the receiving edge, or an outline when the
                    // drop lands inside a group.
                    .children((layer_drop == Some(LayerDrop::Above(id))).then(|| {
                        div()
                            .absolute()
                            .top(px(0.0))
                            .left_0()
                            .right_0()
                            .h(px(2.0))
                            .bg(gpui::rgb(palette().accent))
                    }))
                    .children((layer_drop == Some(LayerDrop::Below(id))).then(|| {
                        div()
                            .absolute()
                            .bottom(px(0.0))
                            .left_0()
                            .right_0()
                            .h(px(2.0))
                            .bg(gpui::rgb(palette().accent))
                    }))
                    .children((layer_drop == Some(LayerDrop::Into(id))).then(|| {
                        div()
                            .absolute()
                            .inset_0()
                            .rounded_sm()
                            .border_2()
                            .border_color(gpui::rgb(palette().accent))
                    }))
                }))
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|ws, _ev: &MouseUpEvent, _w, cx| {
                        ws.finish_layer_drag(cx);
                    }),
                ),
        )
        .child(
            // Action buttons.
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_end()
                .gap_1()
                .pt_1()
                .border_t_1()
                .border_color(gpui::rgb(palette().divider))
                .child(icon_button("layer-new", "layer.new", cx))
                .child(icon_button("group-new", "layer.group", cx))
                .child(icon_button("duplicate", "layer.duplicate", cx))
                .child(icon_button("merge-down", "layer.merge_down", cx))
                .child(icon_button("trash", "layer.delete", cx)),
        )
}

// ===== history panel =====
