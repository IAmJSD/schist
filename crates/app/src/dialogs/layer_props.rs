//! Layer properties.

use super::*;

/// Layer Properties: rename a layer.
pub(super) fn layer_properties(
    state: &DialogState,
    layer: schist_core::LayerId,
    name: String,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let focused = state.focused_field == Some("layer-name");
    let committed = name.clone();
    let shown = if focused && !state.field_buffer.is_empty() {
        state.field_buffer.clone()
    } else {
        name.clone()
    };
    let body = ui::field_row(
        "Name",
        div()
            .w(px(200.0))
            .h(px(22.0))
            .px_1()
            .flex()
            .items_center()
            .rounded_sm()
            .bg(gpui::rgb(ui::palette().field_bg))
            .border_1()
            .border_color(gpui::rgb(if focused {
                ui::palette().accent
            } else {
                ui::palette().field_bg
            }))
            .text_size(px(12.0))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |ws, _e, _w, cx| {
                    ws.focus_field("layer-name", committed.clone());
                    cx.notify();
                }),
            )
            // A caret makes it obvious the field takes typing; it
            // blinks, and the arrows move it.
            .child(if focused {
                let (before, after) = if state.field_buffer.is_empty() {
                    // Nothing typed yet: the committed name shows with
                    // the caret at its end, as it always did.
                    (shown.clone(), String::new())
                } else {
                    let at = state.field_cursor.min(shown.len());
                    (shown[..at].to_string(), shown[at..].to_string())
                };
                ui::caret_run(before, after, state.caret_on, ui::palette().text).into_any_element()
            } else {
                div().child(shown.clone()).into_any_element()
            }),
    );

    let committed = name;
    let actions = div()
        .flex()
        .flex_row()
        .gap_2()
        .child(ui::button(
            "Cancel",
            false,
            |ws, _w, cx| ws.close_modal(cx),
            cx,
        ))
        .child(ui::button(
            "OK",
            true,
            move |ws, _w, cx| {
                let name = if ws.field_buffer.is_empty() {
                    committed.clone()
                } else {
                    ws.field_buffer.clone()
                };
                ws.rename_layer(layer, name, cx);
                ws.close_modal(cx);
            },
            cx,
        ));
    ui::modal_frame("Layer Properties", 340.0, body, actions)
}
