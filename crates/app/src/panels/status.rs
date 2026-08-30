//! The status bar along the bottom of the window.

use super::*;

pub fn status_bar(ws: &Workspace) -> impl IntoElement {
    let title = ws
        .doc
        .as_ref()
        .map(|d| {
            format!(
                "{}{}  {}×{}",
                d.title,
                if d.dirty { " •" } else { "" },
                d.width,
                d.height
            )
        })
        .unwrap_or_else(|| "No document".into());
    let zoom = format!("{:.0}%", ws.zoom * 100.0);
    let brush = format!("{:.0}px", ws.editor.brush_size);
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_4()
        .h(px(24.0))
        .flex_none()
        .px_2()
        .bg(gpui::rgb(palette().status_bg))
        .border_t_1()
        .border_color(gpui::rgb(palette().panel_edge))
        .text_size(px(11.0))
        .text_color(gpui::rgb(palette().text_dim))
        .child(title)
        .child(zoom)
        .child(brush)
        .child(div().flex_grow())
        .child(ws.status.clone())
}

// ===== context menus =====
