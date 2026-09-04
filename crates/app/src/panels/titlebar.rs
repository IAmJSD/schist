//! Window title chrome painted into AppKit's transparent title-bar area.

use super::*;

/// A title bar that follows Schist's own theme. macOS keeps drawing and
/// operating the traffic-light controls above this row; the centred label
/// stays clear of them and follows the active document.
pub fn title_bar(ws: &Workspace) -> impl IntoElement {
    let title: SharedString = match ws.doc.as_ref() {
        Some(doc) if doc.dirty => format!("{} • — Schist", doc.title).into(),
        Some(doc) => format!("{} — Schist", doc.title).into(),
        None => "Schist".into(),
    };

    div()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .h(px(28.0))
        .w_full()
        .bg(gpui::rgb(palette().panel_bg))
        .border_b_1()
        .border_color(gpui::rgb(palette().panel_edge))
        .child(
            div()
                .max_w(px(520.0))
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .text_size(px(11.0))
                .text_color(gpui::rgb(palette().text_dim))
                .child(title),
        )
}
