//! The note tool's options and the notes panel.

use super::*;

/// The Note tool's own controls: who is writing, in what colour, and a
/// way to clear the lot -- the three Photoshop puts in its options bar.
pub(super) fn note_options(ws: &Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let editing = ws.note_edit_buffer(NoteField::Author);
    let author = match editing {
        Some(buffer) => format!("{buffer}|"),
        None if ws.view.note_author.is_empty() => "Author".to_string(),
        None => ws.view.note_author.clone(),
    };
    let placeholder = editing.is_none() && ws.view.note_author.is_empty();
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .child(
            div()
                .text_size(px(11.0))
                .text_color(gpui::rgb(palette().text_dim))
                .child("Author"),
        )
        .child(
            div()
                .w(px(120.0))
                .h(px(22.0))
                .px_1()
                .flex()
                .items_center()
                .rounded_sm()
                .cursor_text()
                .bg(gpui::rgb(palette().field_bg))
                .border_1()
                .border_color(gpui::rgb(if editing.is_some() {
                    palette().accent
                } else {
                    palette().panel_edge
                }))
                .text_size(px(12.0))
                .text_color(gpui::rgb(if placeholder {
                    palette().text_faint
                } else {
                    palette().text
                }))
                .overflow_hidden()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|ws, _e, _w, cx| ws.begin_note_author_edit(cx)),
                )
                .child(author),
        )
        .child(
            div()
                .size(px(18.0))
                .rounded_sm()
                .bg(swatch_hex(ws.editor.note_color))
                .border_1()
                .border_color(gpui::rgb(palette().text_faint))
                .cursor_pointer()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|ws, _e, _w, cx| ws.open_color_picker(ColorTarget::Note, cx)),
                ),
        )
        .child(ui::button(
            "Clear All",
            false,
            |ws, _w, cx| ws.clear_notes(cx),
            cx,
        ))
}

/// A small square button in the Notes panel's header.
pub(super) fn note_button(
    id: &'static str,
    label: &'static str,
    enabled: bool,
    on_click: impl Fn(&mut Workspace, &mut Context<Workspace>) + 'static,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let colour = if enabled {
        palette().text
    } else {
        palette().text_faint
    };
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .size(px(20.0))
        .rounded_sm()
        .text_size(px(13.0))
        .text_color(gpui::rgb(colour))
        .when(enabled, |d| {
            d.cursor_pointer()
                .hover(|s| s.bg(gpui::rgb(palette().hover)))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |ws, _e, _w, cx| on_click(ws, cx)),
                )
        })
        .child(label)
}

/// Photoshop's Notes panel: one note at a time, with its author, its text
/// and a way to walk the rest.
///
/// Absent entirely when there is nothing to show and the Note tool is not
/// out -- the side column is 260px wide and already carries four panels,
/// so an empty fifth would cost the layers panel rows it needs more.
pub(super) fn notes_panel(ws: &Workspace, cx: &mut Context<Workspace>) -> Option<gpui::AnyElement> {
    let doc = ws.doc.as_ref()?;
    let count = doc.notes.len();
    if count == 0 && ws.editor.active_tool != "note" {
        return None;
    }
    let index = ws.active_note().unwrap_or(0);
    let note = doc.notes.get(index);
    let editing = ws.note_edit_buffer(NoteField::Text(index));
    let author = note
        .map(|n| {
            if n.author.is_empty() {
                "Unattributed".to_string()
            } else {
                n.author.clone()
            }
        })
        .unwrap_or_default();
    // The caret goes on the end of the buffer, matching the layers
    // panel's inline rename and the dialogs' fields.
    let body = match editing {
        Some(buffer) => format!("{buffer}|"),
        None => note.map(|n| n.text.clone()).unwrap_or_default(),
    };
    let empty_body = editing.is_none() && body.is_empty();

    let header = div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .child(panel_title("Notes"))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .child(note_button(
                    "note-prev",
                    "\u{2039}",
                    count > 1,
                    |ws, cx| ws.step_note(-1, cx),
                    cx,
                ))
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(gpui::rgb(palette().text_dim))
                        .child(if count == 0 {
                            "0".to_string()
                        } else {
                            format!("{} / {count}", index + 1)
                        }),
                )
                .child(note_button(
                    "note-next",
                    "\u{203A}",
                    count > 1,
                    |ws, cx| ws.step_note(1, cx),
                    cx,
                ))
                .child(
                    div()
                        .id("note-delete")
                        .flex()
                        .items_center()
                        .justify_center()
                        .size(px(20.0))
                        .rounded_sm()
                        .when(count > 0, |d| {
                            d.cursor_pointer()
                                .hover(|s| s.bg(gpui::rgb(palette().hover)))
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |ws, _e, _w, cx| ws.delete_note(index, cx)),
                                )
                        })
                        .child(icon(
                            "trash",
                            13.0,
                            if count > 0 {
                                palette().text
                            } else {
                                palette().text_faint
                            },
                        )),
                ),
        );

    let panel = div()
        .flex()
        .flex_col()
        .flex_none()
        .p_2()
        .gap_1()
        .border_t_1()
        .border_color(gpui::rgb(palette().panel_edge))
        .child(header);

    let panel = if count == 0 {
        panel.child(
            div()
                .text_size(px(11.0))
                .text_color(gpui::rgb(palette().text_faint))
                .child("Click the canvas to leave a note."),
        )
    } else {
        panel
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(gpui::rgb(palette().text_dim))
                    .child(author),
            )
            .child(
                div()
                    .id("note-body")
                    .h(px(72.0))
                    .p_1()
                    .rounded_sm()
                    .cursor_text()
                    .overflow_hidden()
                    .bg(gpui::rgb(palette().field_bg))
                    .border_1()
                    .border_color(gpui::rgb(if editing.is_some() {
                        palette().accent
                    } else {
                        palette().panel_edge
                    }))
                    .text_size(px(12.0))
                    .text_color(gpui::rgb(if empty_body {
                        palette().text_faint
                    } else {
                        palette().text
                    }))
                    .flex()
                    .flex_col()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |ws, _e, _w, cx| ws.begin_note_edit(index, cx)),
                    )
                    // One child per line: a note is a paragraph, and a
                    // single text child would run the whole thing
                    // together on one line.
                    .children(if empty_body {
                        vec![SharedString::from("Click to write")]
                    } else {
                        body.split('\n')
                            .map(|line| SharedString::from(line.to_string()))
                            .collect()
                    }),
            )
    };
    Some(panel.into_any_element())
}

// ===== toolbar =====
