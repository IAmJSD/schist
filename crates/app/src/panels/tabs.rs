//! The document tab strip.

use super::*;

/// Photoshop-style document tabs: one per open file, the active one lit,
/// a dot marking unsaved changes. Click to switch, middle-click or the ×
/// to close.
pub fn tab_bar(ws: &mut Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let active = ws.active_tab();
    let tabs = ws.tab_strip();
    div()
        .flex()
        .flex_row()
        .items_end()
        .h(px(26.0))
        .flex_none()
        .bg(gpui::rgb(palette().deep_bg))
        .border_b_1()
        .border_color(gpui::rgb(palette().panel_edge))
        .overflow_hidden()
        .children(tabs.into_iter().enumerate().map(|(i, (title, dirty))| {
            let is_active = i == active;
            let label: SharedString = if dirty {
                format!("{title} •").into()
            } else {
                title
            };
            let mut tab = div()
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .h(px(25.0))
                .pl_2()
                .pr_1()
                .max_w(px(180.0))
                .border_r_1()
                .border_color(gpui::rgb(palette().panel_edge))
                .text_size(px(11.0));
            tab = if is_active {
                tab.bg(gpui::rgb(palette().control_bg))
                    .text_color(gpui::rgb(palette().text))
            } else {
                tab.bg(gpui::rgb(palette().panel_bg))
                    .text_color(gpui::rgb(palette().text_dim))
                    .hover(|s| s.bg(gpui::rgb(palette().hover)))
            };
            tab.on_mouse_down(
                MouseButton::Left,
                cx.listener(move |ws, _e, _w, cx| ws.select_tab(i, cx)),
            )
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(move |ws, _e, _w, cx| ws.request_close_tab(i, cx)),
            )
            .child(
                div()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(label),
            )
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_center()
                    .size(px(16.0))
                    .rounded_sm()
                    .hover(|s| s.bg(gpui::rgb(palette().button_hover)))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |ws, _e, _w, cx| {
                            cx.stop_propagation();
                            ws.request_close_tab(i, cx);
                        }),
                    )
                    .child(icon("close", 9.0, palette().text_dim)),
            )
        }))
}

// ===== tool options bar =====
