//! The history panel.

use super::*;

pub(super) fn history_panel(ws: &mut Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let (undo_entries, redo_entries): (Vec<String>, Vec<String>) = ws
        .doc
        .as_ref()
        .map(|d| {
            (
                d.history.entries().iter().map(|e| e.name.clone()).collect(),
                // Most-recently-undone first == next redo first.
                d.history
                    .redo_entries()
                    .iter()
                    .rev()
                    .map(|e| e.name.clone())
                    .collect(),
            )
        })
        .unwrap_or_default();
    let n_undo = undo_entries.len() as i32;

    div()
        .flex()
        .flex_col()
        .h(px(150.0))
        .flex_none()
        .p_2()
        .gap_1()
        .border_t_1()
        .border_color(gpui::rgb(palette().panel_edge))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .child(panel_title("History"))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap_1()
                        .child(icon_button("undo", "edit.undo", cx))
                        .child(icon_button("redo", "edit.redo", cx)),
                ),
        )
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(|ws, ev: &MouseDownEvent, _w, cx| {
                ws.open_context_menu(ContextTarget::History, ev.position, cx);
            }),
        )
        .child(
            div()
                .id("history-scroll")
                .flex()
                .flex_col()
                .overflow_y_scroll()
                .flex_grow()
                .min_h(px(0.0))
                // The state the document opened in. The panel could walk
                // back to "one edit applied" but never to "none": the
                // topmost row still leaves the first edit in place, so
                // getting all the way back needed one more cmd-Z.
                // Photoshop's panel has this row too.
                .child({
                    let is_current = n_undo == 0;
                    div()
                        .px_1()
                        .h(px(19.0))
                        .flex_none()
                        .text_size(px(11.0))
                        .rounded_sm()
                        .cursor_pointer()
                        .when_active(is_current)
                        .hover(move |s| {
                            if is_current {
                                s
                            } else {
                                s.bg(gpui::rgb(palette().hover))
                            }
                        })
                        .text_color(gpui::rgb(palette().text_dim))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |ws, _e, _w, cx| ws.history_jump(-n_undo, cx)),
                        )
                        .child("Opened")
                })
                .children(undo_entries.into_iter().enumerate().map(|(i, name)| {
                    // Jump so entry i becomes the last applied edit.
                    let steps = (i as i32 + 1) - n_undo;
                    let is_current = i as i32 + 1 == n_undo;
                    div()
                        .px_1()
                        .h(px(19.0))
                        .flex_none()
                        .text_size(px(11.0))
                        .rounded_sm()
                        .cursor_pointer()
                        .when_active(is_current)
                        .hover(move |s| {
                            if is_current {
                                s
                            } else {
                                s.bg(gpui::rgb(palette().hover))
                            }
                        })
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |ws, _e, _w, cx| ws.history_jump(steps, cx)),
                        )
                        .child(name)
                }))
                .children(redo_entries.into_iter().enumerate().map(|(j, name)| {
                    let steps = j as i32 + 1;
                    div()
                        .px_1()
                        .h(px(19.0))
                        .flex_none()
                        .text_size(px(11.0))
                        .rounded_sm()
                        .text_color(gpui::rgb(palette().text_faint))
                        .hover(|s| s.bg(gpui::rgb(palette().hover)))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |ws, _e, _w, cx| ws.history_jump(steps, cx)),
                        )
                        .child(name)
                })),
        )
}

// ===== status bar =====
