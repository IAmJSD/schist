//! The gallery view, drawn the way Picasa drew its library: a warm light
//! chrome whatever the editor theme, a folder list on the left, a white
//! grid of thumbnails grouped under blue folder headers, and a tray along
//! the bottom with the green action button and the thumbnail-size slider.
//!
//! Deliberately not on `ui::palette()`: the gallery is a lightbox for
//! photographs, and Picasa's point was that the chrome stays pale and
//! recedes. The editor keeps its own theme either side of it.

use super::*;
use gpui::{img, StatefulInteractiveElement as _};

const GRID_BG: u32 = 0xFFFFFF;
const CHROME_BG: u32 = 0xEDEDE6;
const CHROME_EDGE: u32 = 0xC9C9C0;
const TRAY_BG: u32 = 0xE3E3DC;
const SIDEBAR_SELECTED: u32 = 0xCFE0F2;
const HEADER_BLUE: u32 = 0x2A5DB0;
const TEXT: u32 = 0x2B2B2B;
const TEXT_DIM: u32 = 0x7A7A72;
const CELL_EDGE: u32 = 0xDDDDDD;
const SELECT_BLUE: u32 = 0x4A90D9;
const SELECT_FILL: u32 = 0xE8F0FB;
const PICASA_GREEN: u32 = 0x5C9E31;
const PICASA_GREEN_HOVER: u32 = 0x6DB33F;

impl Workspace {
    pub(super) fn render_gallery(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        // Rendering cells is what queues their thumbnails; make sure a
        // loader is running for whatever this frame asked for.
        let body = if self.library.folders.is_empty() {
            gallery_empty_state(cx).into_any_element()
        } else {
            div()
                .flex()
                .flex_row()
                .flex_grow()
                .min_h(px(0.0))
                .child(sidebar(self, cx))
                .child(grid(self, cx))
                .into_any_element()
        };
        let root = div()
            .flex()
            .flex_col()
            .flex_grow()
            .min_h(px(0.0))
            .bg(gpui::rgb(GRID_BG))
            .text_color(gpui::rgb(TEXT))
            .child(top_strip(self, cx))
            .child(body)
            .child(tray(self, cx));
        self.kick_thumb_loader(cx);
        root
    }
}

fn gallery_button(
    label: &'static str,
    green: bool,
    on_click: impl Fn(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .justify_center()
        .h(px(24.0))
        .px_3()
        .rounded_md()
        .text_size(px(12.0))
        .cursor_pointer()
        .bg(gpui::rgb(if green { PICASA_GREEN } else { 0xF7F7F2 }))
        .text_color(gpui::rgb(if green { 0xFFFFFF } else { TEXT }))
        .border_1()
        .border_color(gpui::rgb(if green { PICASA_GREEN } else { CHROME_EDGE }))
        .hover(move |s| s.bg(gpui::rgb(if green { PICASA_GREEN_HOVER } else { 0xFFFFFF })))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |ws, _e: &MouseDownEvent, window, cx| on_click(ws, window, cx)),
        )
        .child(label)
}

/// The strip under the menu bar: import and folder buttons on the left,
/// as Picasa keeps its Import button, and the way back to the editor on
/// the right.
fn top_strip(ws: &mut Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let has_doc = ws.doc.is_some();
    let importing = ws.library.importing;
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .h(px(38.0))
        .flex_none()
        .px_2()
        .bg(gpui::rgb(CHROME_BG))
        .border_b_1()
        .border_color(gpui::rgb(CHROME_EDGE))
        .child(gallery_button(
            if importing {
                "Importing…"
            } else {
                "Import…"
            },
            true,
            |ws, _w, cx| ws.gallery_import_camera(cx),
            cx,
        ))
        .child(gallery_button(
            "Add Folder…",
            false,
            |ws, window, cx| ws.gallery_add_folder(window, cx),
            cx,
        ))
        .child(gallery_button(
            "Refresh",
            false,
            |ws, _w, cx| ws.library_rescan(cx),
            cx,
        ))
        .child(div().flex_grow())
        .child(
            div()
                .text_size(px(13.0))
                .text_color(gpui::rgb(TEXT_DIM))
                .child("Gallery"),
        )
        .child(div().flex_grow())
        .children(has_doc.then(|| {
            gallery_button(
                "Back to Editing",
                false,
                |ws, _w, cx| ws.toggle_gallery(cx),
                cx,
            )
        }))
        .children((!has_doc).then(|| {
            gallery_button(
                "New File…",
                false,
                |ws, _w, cx| {
                    ws.library.open = false;
                    ws.open_new_document_dialog(cx);
                    cx.notify();
                },
                cx,
            )
        }))
}

/// Nothing watched yet: say what the gallery is and offer the two ways in.
fn gallery_empty_state(cx: &mut Context<Workspace>) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .flex_grow()
        .items_center()
        .justify_center()
        .gap_3()
        .child(
            div()
                .text_size(px(16.0))
                .text_color(gpui::rgb(TEXT))
                .child("Your gallery is empty"),
        )
        .child(
            div()
                .text_size(px(12.0))
                .text_color(gpui::rgb(TEXT_DIM))
                .child("Watch folders of photos, or import from a camera. Files stay where they are; edits are versioned beside them."),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .gap_2()
                .child(gallery_button(
                    "Add Folder…",
                    true,
                    |ws, window, cx| ws.gallery_add_folder(window, cx),
                    cx,
                ))
                .child(gallery_button(
                    "Import from Camera…",
                    false,
                    |ws, _w, cx| ws.gallery_import_camera(cx),
                    cx,
                )),
        )
}

/// The folder list: Picasa's left column, minus the years.
fn sidebar(ws: &mut Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let filter = ws.library.folder_filter.clone();
    let folders: Vec<(PathBuf, usize)> = ws
        .library
        .folders
        .iter()
        .map(|root| {
            let count = ws
                .library
                .sections
                .iter()
                .filter(|s| s.dir.starts_with(root))
                .map(|s| s.entries.len())
                .sum();
            (root.clone(), count)
        })
        .collect();
    let total: usize = folders.iter().map(|(_, n)| n).sum();
    let mut rows: Vec<gpui::AnyElement> = Vec::new();
    rows.push(sidebar_row("All Photos", total, filter.is_none(), None, cx).into_any_element());
    for (root, count) in folders {
        let label = root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| root.display().to_string());
        let selected = filter.as_deref() == Some(root.as_path());
        rows.push(sidebar_row(label, count, selected, Some(root), cx).into_any_element());
    }
    div()
        .id("gallery-sidebar")
        .flex()
        .flex_col()
        .w(px(210.0))
        .flex_none()
        .overflow_y_scroll()
        .bg(gpui::rgb(CHROME_BG))
        .border_r_1()
        .border_color(gpui::rgb(CHROME_EDGE))
        .child(
            div()
                .px_2()
                .pt_2()
                .pb_1()
                .text_size(px(11.0))
                .text_color(gpui::rgb(TEXT_DIM))
                .child("FOLDERS"),
        )
        .children(rows)
        .child(
            div()
                .px_2()
                .h(px(24.0))
                .flex()
                .items_center()
                .text_size(px(12.0))
                .text_color(gpui::rgb(HEADER_BLUE))
                .cursor_pointer()
                .hover(|s| s.bg(gpui::rgb(SIDEBAR_SELECTED)))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|ws, _e: &MouseDownEvent, window, cx| {
                        ws.gallery_add_folder(window, cx)
                    }),
                )
                .child("+ Add folder…"),
        )
}

fn sidebar_row(
    label: impl Into<SharedString>,
    count: usize,
    selected: bool,
    root: Option<PathBuf>,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let filter = root.clone();
    let mut row = div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .px_2()
        .h(px(24.0))
        .text_size(px(12.0))
        .cursor_pointer()
        .bg(gpui::rgb(if selected {
            SIDEBAR_SELECTED
        } else {
            CHROME_BG
        }))
        .hover(|s| s.bg(gpui::rgb(SIDEBAR_SELECTED)))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |ws, _e: &MouseDownEvent, _w, cx| {
                ws.library.folder_filter = filter.clone();
                cx.notify();
            }),
        )
        .child(div().flex_grow().truncate().child(label.into()))
        .child(
            div()
                .text_size(px(10.0))
                .text_color(gpui::rgb(TEXT_DIM))
                .child(format!("{count}")),
        );
    if let Some(root) = root {
        // The quiet way out, matching Picasa's "Remove from Picasa":
        // stop watching, never delete.
        row = row.child(
            div()
                .id(SharedString::from(format!("unwatch-{}", root.display())))
                .pl_1()
                .text_size(px(11.0))
                .text_color(gpui::rgb(TEXT_DIM))
                .hover(|s| s.text_color(gpui::rgb(TEXT)))
                .tooltip(crate::ui::tip("Stop watching this folder", None))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |ws, _e: &MouseDownEvent, _w, cx| {
                        cx.stop_propagation();
                        ws.gallery_remove_folder(&root.clone(), cx);
                    }),
                )
                .child("\u{2715}"),
        );
    }
    row
}

/// The white grid: folder headers with a rule, then wrapped thumbnails.
fn grid(ws: &mut Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let cell = ws.library.thumb_px;
    let selected = ws.library.selected.clone();
    // Owned snapshot: the cells below borrow the workspace mutably to
    // fetch thumbnails, so they cannot also iterate `sections` in place.
    let sections: Vec<(PathBuf, Vec<super::library::Entry>)> = ws
        .library
        .visible_sections()
        .map(|s| (s.dir.clone(), s.entries.clone()))
        .collect();
    let scanning = ws.library.scanning;
    let mut column = div()
        .id("gallery-grid")
        .flex()
        .flex_col()
        .flex_grow()
        .min_h(px(0.0))
        .overflow_y_scroll()
        .bg(gpui::rgb(GRID_BG))
        .p_2();
    if scanning && sections.is_empty() {
        column = column.child(
            div()
                .p_4()
                .text_size(px(12.0))
                .text_color(gpui::rgb(TEXT_DIM))
                .child("Scanning folders\u{2026}"),
        );
    }
    for (dir, entries) in sections {
        let title = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| dir.display().to_string());
        column = column.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .pt_2()
                .pb_1()
                .child(
                    div()
                        .text_size(px(13.0))
                        .text_color(gpui::rgb(HEADER_BLUE))
                        .child(title),
                )
                .child(
                    div()
                        .text_size(px(10.0))
                        .text_color(gpui::rgb(TEXT_DIM))
                        .child(format!("{} — {}", dir.display(), entries.len())),
                ),
        );
        column = column.child(div().h(px(1.0)).mb_2().bg(gpui::rgb(CELL_EDGE)));
        let mut wrap = div().flex().flex_row().flex_wrap().gap_2().pb_2();
        for entry in entries {
            wrap = wrap.child(cell_element(ws, entry, cell, &selected, cx));
        }
        column = column.child(wrap);
    }
    column
}

fn cell_element(
    ws: &mut Workspace,
    entry: super::library::Entry,
    cell: f32,
    selected: &Option<PathBuf>,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let thumb = ws.library.thumb(&entry);
    let is_selected = selected.as_deref() == Some(entry.path.as_path());
    let click_path = entry.path.clone();
    let inner = cell - 10.0;
    div()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .w(px(cell))
        .h(px(cell))
        .flex_none()
        .relative()
        .rounded_sm()
        .bg(gpui::rgb(if is_selected { SELECT_FILL } else { GRID_BG }))
        .border_2()
        .border_color(gpui::rgb(if is_selected { SELECT_BLUE } else { CELL_EDGE }))
        .cursor_pointer()
        .hover(move |s| {
            if is_selected {
                s
            } else {
                s.border_color(gpui::rgb(0xB9CBE0))
            }
        })
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |ws, ev: &MouseDownEvent, _w, cx| {
                ws.library.selected = Some(click_path.clone());
                if ev.click_count >= 2 {
                    ws.open_from_gallery(click_path.clone(), cx);
                }
                cx.notify();
            }),
        )
        .children(thumb.map(|t| img(t).max_w(px(inner)).max_h(px(inner))))
        .children(ws.library.thumb_failed(&entry.path).then(|| {
            div()
                .text_size(px(10.0))
                .text_color(gpui::rgb(TEXT_DIM))
                .child("no preview")
        }))
        .children(entry.edited.then(|| {
            // Picasa's little brush: a corner badge saying this photo
            // carries an edit.
            div()
                .absolute()
                .bottom(px(3.0))
                .left(px(3.0))
                .px_1()
                .rounded_sm()
                .bg(gpui::rgb(PICASA_GREEN))
                .text_size(px(9.0))
                .text_color(gpui::rgb(0xFFFFFF))
                .child("edited")
        }))
}

/// The bottom tray: selection details and the green Edit button on the
/// left, the photo count and size slider on the right.
fn tray(ws: &mut Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let selected = ws.library.selected_entry().cloned();
    let count = ws.library.photo_count();
    let thumb_px = ws.library.thumb_px;
    let ratio = (thumb_px - 80.0) / 160.0;
    let name = selected
        .as_ref()
        .and_then(|e| e.path.file_name())
        .map(|n| n.to_string_lossy().into_owned());
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_3()
        .h(px(40.0))
        .flex_none()
        .px_2()
        .bg(gpui::rgb(TRAY_BG))
        .border_t_1()
        .border_color(gpui::rgb(CHROME_EDGE))
        .children(selected.as_ref().map(|entry| {
            let open = entry.path.clone();
            gallery_button(
                "Edit",
                true,
                move |ws, _w, cx| ws.open_from_gallery(open.clone(), cx),
                cx,
            )
        }))
        .children(name.map(|name| {
            div()
                .text_size(px(12.0))
                .text_color(gpui::rgb(TEXT))
                .child(name)
        }))
        .children(selected.as_ref().is_some_and(|e| e.edited).then(|| {
            div()
                .text_size(px(11.0))
                .text_color(gpui::rgb(TEXT_DIM))
                .child("edited — versions kept beside the file")
        }))
        .child(div().flex_grow())
        .child(
            div()
                .text_size(px(11.0))
                .text_color(gpui::rgb(TEXT_DIM))
                .child(format!("{count} photos")),
        )
        .child(crate::ui::slider_track(
            "gallery-thumb-size",
            ratio,
            110.0,
            |ws, r, cx| {
                ws.set_gallery_thumb_px(80.0 + r * 160.0);
                cx.notify();
            },
            cx,
        ))
}

/// Several mounted cameras: ask which one to import.
pub(crate) fn camera_import_dialog(
    sources: &[PathBuf],
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let mut body = div().flex().flex_col().gap_1().child(
        div()
            .text_size(px(12.0))
            .child("More than one camera volume is mounted. Import from:"),
    );
    for source in sources {
        let pick = source.clone();
        let label = source
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| source.display().to_string());
        body = body.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .px_2()
                .h(px(26.0))
                .rounded_sm()
                .text_size(px(12.0))
                .hover(|s| s.bg(gpui::rgb(crate::ui::palette().hover)))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |ws, _e: &MouseDownEvent, _w, cx| {
                        ws.close_modal(cx);
                        ws.import_camera(pick.clone(), cx);
                    }),
                )
                .child(label)
                .child(
                    div()
                        .text_size(px(10.0))
                        .text_color(gpui::rgb(crate::ui::palette().text_dim))
                        .child(source.display().to_string()),
                ),
        );
    }
    let actions = div().flex().flex_row().gap_2().child(crate::ui::button(
        "Cancel",
        false,
        |ws, _w, cx| ws.close_modal(cx),
        cx,
    ));
    crate::ui::modal_frame("Import from Camera", 420.0, body, actions)
}
