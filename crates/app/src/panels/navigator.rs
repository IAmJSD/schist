//! The navigator panel's thumbnail and viewport rectangle.

use super::*;

/// A thumbnail of the whole document with the viewport marked, plus a zoom
/// slider.
pub fn navigator(ws: &mut Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let zoom_ratio = ((ws.zoom.log2() + 7.0) / 12.0).clamp(0.0, 1.0);
    let zoom_label = format!("{:.0}%", ws.zoom * 100.0);
    let thumb = ws.document_thumbnail();
    div()
        .flex()
        .flex_col()
        .p_2()
        .gap_1()
        .border_t_1()
        .border_color(gpui::rgb(palette().panel_edge))
        .child(panel_title("Navigator"))
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(|ws, ev: &MouseDownEvent, _w, cx| {
                ws.open_context_menu(ContextTarget::Navigator, ev.position, cx);
            }),
        )
        .child(
            div()
                .flex()
                .items_center()
                .justify_center()
                .h(px(90.0))
                .bg(gpui::rgb(palette().field_bg))
                .rounded_sm()
                .children(thumb.map(|t| img(t).max_w(px(220.0)).max_h(px(84.0)))),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(crate::ui::slider_track(
                    "nav-zoom",
                    zoom_ratio,
                    150.0,
                    |ws, r, cx| {
                        // Log scale: 0.8% .. 3200%.
                        let zoom = 2f32.powf(r * 12.0 - 7.0);
                        ws.set_zoom(zoom);
                        // The slider fires per mouse-move; damp the rebuilds
                        // like wheel zoom.
                        ws.view_gesture_event(cx);
                    },
                    cx,
                ))
                .child(
                    div()
                        .w(px(48.0))
                        .flex_none()
                        .text_size(px(11.0))
                        .child(zoom_label),
                ),
        )
}
