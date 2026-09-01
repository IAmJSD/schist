//! The start screen: what an empty workspace shows instead of a bare
//! canvas. New-document presets and Open on every platform; the recents
//! and the gallery doorway on desktop, where files have paths to come
//! back from and folders exist to watch.

use super::*;
use crate::workspace::NewDocBackground;
use gpui::Focusable as _;
use schist_color::{ColorMode, Depth};

/// A preset card: click to get a document of that size straight away.
fn preset_card(
    label: &'static str,
    width: u32,
    height: u32,
    ppi: f32,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .justify_center()
        .w(px(150.0))
        .h(px(56.0))
        .px_3()
        .rounded_md()
        .bg(gpui::rgb(palette().panel_bg))
        .border_1()
        .border_color(gpui::rgb(palette().edge))
        .cursor_pointer()
        .hover(|s| s.border_color(gpui::rgb(palette().accent)))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |ws, _e: &MouseDownEvent, _w, cx| {
                ws.create_document(
                    "",
                    width,
                    height,
                    ppi,
                    ColorMode::Rgb,
                    Depth::Eight,
                    NewDocBackground::White,
                );
                cx.notify();
            }),
        )
        .child(div().text_size(px(12.0)).child(label))
        .child(
            div()
                .text_size(px(10.0))
                .text_color(gpui::rgb(palette().text_dim))
                .child(format!("{width} × {height} px")),
        )
}

fn start_button(
    label: &'static str,
    primary: bool,
    on_click: impl Fn(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .justify_center()
        .h(px(26.0))
        .px_3()
        .rounded_md()
        .text_size(px(12.0))
        .cursor_pointer()
        .bg(gpui::rgb(if primary {
            palette().accent
        } else {
            palette().button_bg
        }))
        .text_color(gpui::rgb(if primary {
            palette().accent_text
        } else {
            palette().text
        }))
        .hover(move |s| {
            s.bg(gpui::rgb(if primary {
                palette().accent_hover
            } else {
                palette().button_hover
            }))
        })
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |ws, _e: &MouseDownEvent, window, cx| on_click(ws, window, cx)),
        )
        .child(label)
}

fn section_label(text: &'static str) -> impl IntoElement {
    div()
        .text_size(px(11.0))
        .text_color(gpui::rgb(palette().text_dim))
        .child(text.to_uppercase())
}

/// The recents column. Desktop only: browser paths are invented per
/// session, so there is nothing to come back to.
#[cfg(not(target_arch = "wasm32"))]
fn recents(ws: &Workspace, cx: &mut Context<Workspace>) -> Option<gpui::AnyElement> {
    if ws.library.recents.is_empty() {
        return None;
    }
    let mut column = div()
        .flex()
        .flex_col()
        .gap_1()
        .child(section_label("Recent"));
    for path in ws.library.recents.iter().take(8) {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        let dir = path
            .parent()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        let open = path.clone();
        column = column.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .px_2()
                .h(px(24.0))
                .rounded_sm()
                .cursor_pointer()
                .hover(|s| s.bg(gpui::rgb(palette().hover)))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |ws, _e: &MouseDownEvent, _w, cx| {
                        ws.load_file(open.clone(), cx);
                    }),
                )
                .child(div().text_size(px(12.0)).child(name))
                .child(
                    div()
                        .text_size(px(10.0))
                        .text_color(gpui::rgb(palette().text_faint))
                        .truncate()
                        .child(dir),
                ),
        );
    }
    Some(column.into_any_element())
}

/// The gallery doorway: set one up, or step into the one that exists.
#[cfg(not(target_arch = "wasm32"))]
fn gallery_card(ws: &Workspace, cx: &mut Context<Workspace>) -> gpui::AnyElement {
    let folders = ws.library.folders.len();
    let column = div()
        .flex()
        .flex_col()
        .gap_2()
        .child(section_label("Gallery"));
    let column = if folders == 0 {
        column
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(gpui::rgb(palette().text_dim))
                    .child(
                        "Browse folders of photos Picasa-style and edit them without touching \
                         the originals — every save keeps a version.",
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .child(start_button(
                        "Set Up Gallery…",
                        false,
                        |ws, window, cx| ws.gallery_add_folder(window, cx),
                        cx,
                    ))
                    .child(start_button(
                        "Import from Camera…",
                        false,
                        |ws, _w, cx| ws.gallery_import_camera(cx),
                        cx,
                    )),
            )
    } else {
        column.child(div().flex().flex_row().gap_2().child(start_button(
            "Open Gallery",
            false,
            |ws, _w, cx| ws.toggle_gallery(cx),
            cx,
        )))
    };
    column.into_any_element()
}

/// What shows when no document is open and the gallery is not showing.
pub fn start_screen(ws: &mut Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let focus = ws.focus_handle(cx);
    let mut presets = div().flex().flex_row().flex_wrap().gap_2();
    for &(label, w, h, ppi) in crate::dialogs::NEW_DOC_PRESETS {
        presets = presets.child(preset_card(label, w, h, ppi, cx));
    }
    presets = presets.child(
        div()
            .flex()
            .items_center()
            .justify_center()
            .w(px(150.0))
            .h(px(56.0))
            .rounded_md()
            .border_1()
            .border_color(gpui::rgb(palette().edge))
            .text_size(px(12.0))
            .text_color(gpui::rgb(palette().text_dim))
            .cursor_pointer()
            .hover(|s| s.border_color(gpui::rgb(palette().accent)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|ws, _e: &MouseDownEvent, _w, cx| {
                    ws.open_new_document_dialog(cx);
                }),
            )
            .child("Custom…"),
    );
    let column = div()
        .flex()
        .flex_col()
        .gap_4()
        .w(px(660.0))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_3()
                .child(div().text_size(px(22.0)).child("Schist"))
                .child(div().flex_grow())
                .child(start_button(
                    "Open…",
                    true,
                    crate::keymap::open_file_dialog,
                    cx,
                )),
        )
        .child(section_label("Create new"))
        .child(presets);
    #[cfg(not(target_arch = "wasm32"))]
    let column = column.children(recents(ws, cx)).child(gallery_card(ws, cx));
    div()
        .flex()
        .flex_grow()
        .size_full()
        .items_center()
        .justify_center()
        .bg(gpui::rgb(palette().canvas_bg))
        // The focus handle has to stay in the tree for the keymap to
        // dispatch against, exactly as the canvas keeps it.
        .track_focus(&focus)
        .child(column)
}
