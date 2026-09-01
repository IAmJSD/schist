//! The gallery view, laid out the way Picasa laid out its library: a
//! folder list on the left, a grid of thumbnails grouped under blue
//! folder headers, and a tray along the bottom with the green action
//! button and the thumbnail-size slider.
//!
//! It keeps its own palette rather than `ui::palette()` — a photo grid
//! wants quieter, flatter chrome than a panel set — but it follows the
//! theme choice: the light theme gets Picasa's warm white lightbox, the
//! dark theme a Lightroom-grey version of the same room, so opening the
//! gallery from a dark editor is not a flashbang.

use super::*;
use gpui::{img, StatefulInteractiveElement as _};

/// The gallery's chrome colours for one theme.
struct GalleryPalette {
    /// Behind the thumbnails.
    grid_bg: u32,
    /// The top strip and sidebar.
    chrome_bg: u32,
    chrome_edge: u32,
    tray_bg: u32,
    sidebar_selected: u32,
    /// Folder headers and the add-folder link — Picasa's blue.
    header: u32,
    text: u32,
    text_dim: u32,
    cell_edge: u32,
    /// Cell border under the pointer.
    cell_hover: u32,
    select_border: u32,
    select_fill: u32,
    button_bg: u32,
    button_hover: u32,
    /// The green action buttons and the "edited" badge.
    green: u32,
    green_hover: u32,
}

/// Picasa: white grid, warm grey chrome.
const GALLERY_LIGHT: GalleryPalette = GalleryPalette {
    grid_bg: 0xFFFFFF,
    chrome_bg: 0xEDEDE6,
    chrome_edge: 0xC9C9C0,
    tray_bg: 0xE3E3DC,
    sidebar_selected: 0xCFE0F2,
    header: 0x2A5DB0,
    text: 0x2B2B2B,
    text_dim: 0x7A7A72,
    cell_edge: 0xDDDDDD,
    cell_hover: 0xB9CBE0,
    select_border: 0x4A90D9,
    select_fill: 0xE8F0FB,
    button_bg: 0xF7F7F2,
    button_hover: 0xFFFFFF,
    green: 0x5C9E31,
    green_hover: 0x6DB33F,
};

/// The same room with the lights down — Lightroom's greys.
const GALLERY_DARK: GalleryPalette = GalleryPalette {
    grid_bg: 0x232323,
    chrome_bg: 0x2B2B2B,
    chrome_edge: 0x1C1C1C,
    tray_bg: 0x282828,
    sidebar_selected: 0x3A4A5C,
    header: 0x7FB0E8,
    text: 0xD8D8D8,
    text_dim: 0x8F8F8A,
    cell_edge: 0x3A3A3A,
    cell_hover: 0x55708C,
    select_border: 0x4A90D9,
    select_fill: 0x2C3A4A,
    button_bg: 0x383838,
    button_hover: 0x444444,
    green: 0x5C9E31,
    green_hover: 0x6DB33F,
};

fn pal() -> &'static GalleryPalette {
    if crate::ui::is_light() {
        &GALLERY_LIGHT
    } else {
        &GALLERY_DARK
    }
}

impl Workspace {
    pub(super) fn render_gallery(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
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
            .bg(gpui::rgb(pal().grid_bg))
            .text_color(gpui::rgb(pal().text))
            .child(top_strip(self, cx))
            .child(body)
            .child(tray(self, cx));
        // Rendering cells is what queues their thumbnails; make sure a
        // loader is running for whatever this frame asked for.
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
        .bg(gpui::rgb(if green { pal().green } else { pal().button_bg }))
        .text_color(gpui::rgb(if green { 0xFFFFFF } else { pal().text }))
        .border_1()
        .border_color(gpui::rgb(if green {
            pal().green
        } else {
            pal().chrome_edge
        }))
        .hover(move |s| {
            s.bg(gpui::rgb(if green {
                pal().green_hover
            } else {
                pal().button_hover
            }))
        })
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
        .bg(gpui::rgb(pal().chrome_bg))
        .border_b_1()
        .border_color(gpui::rgb(pal().chrome_edge))
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
                .text_color(gpui::rgb(pal().text_dim))
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
                .text_color(gpui::rgb(pal().text))
                .child("Your gallery is empty"),
        )
        .child(
            div()
                .text_size(px(12.0))
                .text_color(gpui::rgb(pal().text_dim))
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
        .bg(gpui::rgb(pal().chrome_bg))
        .border_r_1()
        .border_color(gpui::rgb(pal().chrome_edge))
        .child(
            div()
                .px_2()
                .pt_2()
                .pb_1()
                .text_size(px(11.0))
                .text_color(gpui::rgb(pal().text_dim))
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
                .text_color(gpui::rgb(pal().header))
                .cursor_pointer()
                .hover(|s| s.bg(gpui::rgb(pal().sidebar_selected)))
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
            pal().sidebar_selected
        } else {
            pal().chrome_bg
        }))
        .hover(|s| s.bg(gpui::rgb(pal().sidebar_selected)))
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
                .text_color(gpui::rgb(pal().text_dim))
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
                .text_color(gpui::rgb(pal().text_dim))
                .hover(|s| s.text_color(gpui::rgb(pal().text)))
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

/// The grid: folder headers with a rule, then wrapped thumbnails.
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
        .bg(gpui::rgb(pal().grid_bg))
        .p_2();
    if sections.is_empty() {
        // Say why the grid is bare, rather than showing a void: a scan
        // may be running, or the watched folders may hold nothing
        // Schist can decode.
        column = column.child(
            div()
                .p_4()
                .text_size(px(12.0))
                .text_color(gpui::rgb(pal().text_dim))
                .child(if scanning {
                    "Scanning folders\u{2026}"
                } else {
                    "No photos found in the watched folders. Images Schist can open \
                     (PNG, JPEG, WebP, TIFF, HEIC, PSD, Affinity) appear here; \
                     sub-folders are scanned six levels deep."
                }),
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
                        .text_color(gpui::rgb(pal().header))
                        .child(title),
                )
                .child(
                    div()
                        .text_size(px(10.0))
                        .text_color(gpui::rgb(pal().text_dim))
                        .child(format!("{} — {}", dir.display(), entries.len())),
                ),
        );
        column = column.child(div().h(px(1.0)).mb_2().bg(gpui::rgb(pal().cell_edge)));
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
        .bg(gpui::rgb(if is_selected {
            pal().select_fill
        } else {
            pal().grid_bg
        }))
        .border_2()
        .border_color(gpui::rgb(if is_selected {
            pal().select_border
        } else {
            pal().cell_edge
        }))
        .cursor_pointer()
        .hover(move |s| {
            if is_selected {
                s
            } else {
                s.border_color(gpui::rgb(pal().cell_hover))
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
                .text_color(gpui::rgb(pal().text_dim))
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
                .bg(gpui::rgb(pal().green))
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
        .bg(gpui::rgb(pal().tray_bg))
        .border_t_1()
        .border_color(gpui::rgb(pal().chrome_edge))
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
                .text_color(gpui::rgb(pal().text))
                .child(name)
        }))
        .children(selected.as_ref().is_some_and(|e| e.edited).then(|| {
            div()
                .text_size(px(11.0))
                .text_color(gpui::rgb(pal().text_dim))
                .child("edited — versions kept beside the file")
        }))
        .child(div().flex_grow())
        // The editor's status bar is hidden here, so the tray carries the
        // status line — otherwise an import's outcome lands nowhere.
        .child(
            div()
                .max_w(px(420.0))
                .truncate()
                .text_size(px(11.0))
                .text_color(gpui::rgb(pal().text_dim))
                .child(ws.status.clone()),
        )
        .child(
            div()
                .text_size(px(11.0))
                .text_color(gpui::rgb(pal().text_dim))
                .child(format!("{count} photos")),
        )
        .child(size_slider(ratio, cx))
}

/// The thumbnail-size slider, drawn on the gallery's own palette so it
/// does not import the editor theme's near-black track onto the tray.
fn size_slider(ratio: f32, cx: &mut Context<Workspace>) -> impl IntoElement {
    const WIDTH: f32 = 110.0;
    let entity = cx.entity();
    let set = move |ws: &mut Workspace, r: f32| {
        ws.set_gallery_thumb_px(80.0 + r * 160.0);
    };
    let down = set;
    let moved = set;
    div()
        .relative()
        .w(px(WIDTH))
        .h(px(12.0))
        .flex_none()
        .rounded_sm()
        .bg(gpui::rgb(pal().chrome_edge))
        .child(
            div()
                .absolute()
                .left_0()
                .top_0()
                .bottom_0()
                .w(px(WIDTH * ratio.clamp(0.0, 1.0)))
                .rounded_sm()
                .bg(gpui::rgb(pal().select_border)),
        )
        .child(
            gpui::canvas(
                move |bounds, _window, cx| {
                    entity.update(cx, |ws, _| {
                        ws.record_slider_bounds("gallery-thumb-size", bounds)
                    });
                },
                |_, _, _, _| {},
            )
            .absolute()
            .size_full(),
        )
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |ws, ev: &gpui::MouseDownEvent, _w, cx| {
                ws.begin_slider("gallery-thumb-size", ratio);
                if let Some(r) = ws.slider_ratio("gallery-thumb-size", ev.position) {
                    down(ws, r);
                }
                cx.notify();
            }),
        )
        .on_mouse_move(cx.listener(move |ws, ev: &gpui::MouseMoveEvent, _w, cx| {
            if ev.pressed_button == Some(MouseButton::Left)
                && ws.dragging_slider("gallery-thumb-size")
            {
                if let Some(r) = ws.slider_ratio("gallery-thumb-size", ev.position) {
                    moved(ws, r);
                    cx.notify();
                }
            }
        }))
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(|ws, _ev: &gpui::MouseUpEvent, _w, _cx| {
                ws.end_slider("gallery-thumb-size");
            }),
        )
}

/// The camera picker. Several mounted cameras ask which one; none says
/// so and offers a rescan, because Import… must always answer the click.
pub(crate) fn camera_import_dialog(
    sources: &[ImportSource],
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    if sources.is_empty() {
        let body = div()
            .flex()
            .flex_col()
            .gap_2()
            .child(div().text_size(px(12.0)).child("No cameras found."))
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(gpui::rgb(crate::ui::palette().text_dim))
                    .child(
                        "Plug a camera, memory card or iPhone in. An iPhone must be \
                         unlocked, with Trust This Computer answered, before it shows its \
                         photos — do that, give it a moment, and press Scan Again. Anything \
                         that mounts as a disk with a DCIM folder counts too.",
                    ),
            );
        let actions = div()
            .flex()
            .flex_row()
            .gap_2()
            .child(crate::ui::button(
                "Cancel",
                false,
                |ws, _w, cx| ws.close_modal(cx),
                cx,
            ))
            .child(crate::ui::button(
                "Scan Again",
                true,
                |ws, _w, cx| {
                    ws.close_modal(cx);
                    ws.gallery_import_camera(cx);
                },
                cx,
            ));
        return crate::ui::modal_frame("Import from Camera", 420.0, body, actions);
    }
    let mut body = div().flex().flex_col().gap_1().child(
        div()
            .text_size(px(12.0))
            .child("More than one camera is reachable. Import from:"),
    );
    for source in sources {
        let pick = source.clone();
        let label = super::library::source_label(source);
        let detail = match source {
            ImportSource::Volume(path) => path.display().to_string(),
            ImportSource::Device { .. } => "via Image Capture".to_string(),
        };
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
                        // On to the options: the map, the destination.
                        ws.open_modal(
                            Modal::CameraImportOptions {
                                source: pick.clone(),
                            },
                            cx,
                        );
                    }),
                )
                .child(label)
                .child(
                    div()
                        .text_size(px(10.0))
                        .text_color(gpui::rgb(crate::ui::palette().text_dim))
                        .child(detail),
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

/// Import options for one camera: a navigable OpenStreetMap view.
/// Drag pans, the wheel zooms about the pointer, Shift-drag (or the
/// Draw button) sets the boundary, and the preset chips jump to known
/// cities — every one of which can then be panned away from or redrawn.
pub(crate) fn camera_import_options_dialog(
    ws: &mut Workspace,
    source: ImportSource,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    use super::library_geo::PLACES;
    let label = super::library::source_label(&source);
    let selection = ws.library.map.selection;
    let selection_name = ws.library.map.selection_name.clone();
    let draw_mode = ws.library.map.draw_mode;
    let zoom = ws.library.map.zoom;

    // Preset chips: jump the map there and make that box the boundary.
    let mut chips = div().flex().flex_row().flex_wrap().gap_1();
    for place in PLACES {
        let active = selection_name.as_deref() == Some(place.name);
        chips = chips.child(
            div()
                .px_2()
                .h(px(20.0))
                .flex()
                .items_center()
                .rounded_md()
                .text_size(px(11.0))
                .cursor_pointer()
                .bg(gpui::rgb(if active {
                    crate::ui::palette().selection_bg
                } else {
                    crate::ui::palette().control_bg
                }))
                .hover(move |s| {
                    if active {
                        s
                    } else {
                        s.bg(gpui::rgb(crate::ui::palette().hover))
                    }
                })
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |ws, _e: &MouseDownEvent, _w, cx| {
                        ws.library.map.jump_to(place.name, place.bounds);
                        cx.notify();
                    }),
                )
                .child(place.name),
        );
    }

    let mut tools = div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .child(map_tool_button(
            if draw_mode { "Drawing…" } else { "Draw area" },
            draw_mode,
            |ws, cx| {
                ws.library.map.draw_mode = !ws.library.map.draw_mode;
                cx.notify();
            },
            cx,
        ));
    if selection.is_some() {
        tools = tools.child(map_tool_button(
            "Clear boundary",
            false,
            |ws, cx| {
                ws.library.map.clear_selection();
                cx.notify();
            },
            cx,
        ));
    }
    tools = tools
        .child(
            div()
                .text_size(px(10.0))
                .text_color(gpui::rgb(crate::ui::palette().text_dim))
                .truncate()
                .child("drag pans · scroll zooms · shift-drag draws"),
        )
        .child(div().flex_grow())
        .child(map_tool_button(
            "−",
            false,
            |ws, cx| {
                ws.library.map.zoom_center(-1);
                cx.notify();
            },
            cx,
        ))
        .child(
            div()
                .text_size(px(10.0))
                .text_color(gpui::rgb(crate::ui::palette().text_dim))
                .child(format!("z{zoom}")),
        )
        .child(map_tool_button(
            "+",
            false,
            |ws, cx| {
                ws.library.map.zoom_center(1);
                cx.notify();
            },
            cx,
        ));

    let summary = match (&selection, &selection_name) {
        (Some(_), Some(name)) => format!(
            "Boundary: {name} — only photos whose EXIF position falls inside it import; \
             photos without a position stay on the camera."
        ),
        (Some(b), None) => format!(
            "Boundary: {:.3}°, {:.3}° to {:.3}°, {:.3}° — only photos whose EXIF position \
             falls inside it import.",
            b.south, b.west, b.north, b.east
        ),
        (None, _) => "No boundary — everything on the camera imports.".to_string(),
    };
    let dest_name = match (&selection, &selection_name) {
        (Some(_), Some(name)) => name.clone(),
        (Some(_), None) => "Selected Area".to_string(),
        (None, _) => label.clone(),
    };

    let body = div()
        .flex()
        .flex_col()
        .gap_2()
        .child(chips)
        .child(map_element(ws, cx))
        .child(tools)
        .child(
            div()
                .text_size(px(11.0))
                .text_color(gpui::rgb(crate::ui::palette().text_dim))
                .child(summary),
        )
        .child(
            div()
                .text_size(px(11.0))
                .text_color(gpui::rgb(crate::ui::palette().text_dim))
                .child(format!(
                    "Into ~/Pictures/Schist Imports/{dest_name} — already-imported files \
                     are skipped, so re-running is safe."
                )),
        );

    let area = selection.map(|b| {
        (
            b,
            selection_name.unwrap_or_else(|| "Selected Area".to_string()),
        )
    });
    let actions = div()
        .flex()
        .flex_row()
        .gap_2()
        .child(crate::ui::button(
            "Cancel",
            false,
            |ws, _w, cx| ws.close_modal(cx),
            cx,
        ))
        .child(crate::ui::button(
            "Import",
            true,
            move |ws, _w, cx| {
                ws.close_modal(cx);
                ws.import_camera(source.clone(), area.clone(), cx);
            },
            cx,
        ));
    crate::ui::modal_frame(format!("Import from {label}"), 580.0, body, actions)
}

fn map_tool_button(
    label: impl Into<SharedString>,
    active: bool,
    on_click: impl Fn(&mut Workspace, &mut Context<Workspace>) + 'static,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    div()
        .px_2()
        .h(px(20.0))
        .flex()
        .items_center()
        .rounded_sm()
        .text_size(px(11.0))
        .cursor_pointer()
        .bg(gpui::rgb(if active {
            crate::ui::palette().accent
        } else {
            crate::ui::palette().button_bg
        }))
        .text_color(gpui::rgb(if active {
            crate::ui::palette().accent_text
        } else {
            crate::ui::palette().text
        }))
        .hover(move |s| {
            if active {
                s
            } else {
                s.bg(gpui::rgb(crate::ui::palette().button_hover))
            }
        })
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |ws, _e: &MouseDownEvent, _w, cx| on_click(ws, cx)),
        )
        .child(label.into())
}

/// The navigable map itself: tiles painted like the document canvas —
/// one quad each, laid out in prepaint — with the boundary over them.
fn map_element(ws: &mut Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let entity = cx.entity();
    let draw_mode = ws.library.map.draw_mode;
    div()
        .id("gallery-map")
        .relative()
        .w_full()
        .h(px(300.0))
        .flex_none()
        .overflow_hidden()
        .rounded_sm()
        .border_1()
        .border_color(gpui::rgb(crate::ui::palette().edge))
        .cursor(if draw_mode {
            gpui::CursorStyle::Crosshair
        } else {
            gpui::CursorStyle::OpenHand
        })
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |ws, ev: &MouseDownEvent, _w, cx| {
                let pos = (f32::from(ev.position.x), f32::from(ev.position.y));
                let drawing = ev.modifiers.shift || ws.library.map.draw_mode;
                ws.library.map.begin_drag(pos, drawing);
                cx.notify();
            }),
        )
        .on_mouse_move(cx.listener(|ws, ev: &MouseMoveEvent, _w, cx| {
            if ev.pressed_button == Some(MouseButton::Left) {
                let pos = (f32::from(ev.position.x), f32::from(ev.position.y));
                if ws.library.map.drag_to(pos) {
                    cx.notify();
                }
            }
        }))
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(|ws, _ev: &MouseUpEvent, _w, cx| {
                ws.library.map.end_drag();
                cx.notify();
            }),
        )
        .on_scroll_wheel(cx.listener(|ws, ev: &gpui::ScrollWheelEvent, _w, cx| {
            let dy = match ev.delta {
                gpui::ScrollDelta::Pixels(p) => f32::from(p.y),
                gpui::ScrollDelta::Lines(l) => l.y * 40.0,
            };
            let pos = (f32::from(ev.position.x), f32::from(ev.position.y));
            if ws.library.map.wheel(dy, pos) {
                cx.notify();
            }
        }))
        .child(
            canvas(
                move |bounds, _window, cx| {
                    entity.update(cx, |ws, cx| {
                        let paint = ws.prepare_map_paint(bounds);
                        // Whatever this frame queued starts fetching.
                        ws.kick_map_tiles(cx);
                        paint
                    })
                },
                move |_bounds, paint: super::library_geo::MapPaint, window, _cx| {
                    // Sea-grey where a tile has not arrived, so loading
                    // reads as loading rather than as a hole.
                    for rect in paint.missing {
                        window.paint_quad(gpui::fill(rect, gpui::rgb(0xC9D4DC)));
                    }
                    for (rect, img) in paint.tiles {
                        let _ = window.paint_image(rect, gpui::Corners::default(), img, 0, false);
                    }
                    if let Some(sel) = paint.selection {
                        window.paint_quad(gpui::fill(sel, gpui::rgba(0x4A90D930)));
                        window.paint_quad(gpui::outline(
                            sel,
                            gpui::rgb(0x2A66B0),
                            gpui::BorderStyle::Solid,
                        ));
                    }
                },
            )
            .size_full(),
        )
        // Attribution rides the map's own corner, the way every web map
        // carries it, where it cannot collide with the dialog's rows.
        .child(
            div()
                .absolute()
                .bottom(px(2.0))
                .right(px(4.0))
                .px_1()
                .rounded_sm()
                .bg(gpui::rgba(0xFFFFFFB0))
                .text_size(px(9.0))
                .text_color(gpui::rgb(0x333333))
                .child("\u{a9} OpenStreetMap contributors"),
        )
}

/// A device import failed: what happened, what to do about it, and a
/// Try Again that keeps the source and the drawn boundary.
pub(crate) fn camera_import_failed_dialog(
    source: ImportSource,
    area: Option<(crate::workspace::GeoBounds, String)>,
    message: String,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let label = super::library::source_label(&source);
    let body = div()
        .flex()
        .flex_col()
        .gap_2()
        .child(div().text_size(px(12.0)).child(message))
        .child(
            div()
                .text_size(px(11.0))
                .text_color(gpui::rgb(crate::ui::palette().text_dim))
                .child(
                    "If the device is locked: unlock it, tap Trust This Computer when it \
                     asks, keep it plugged in, and try again.",
                ),
        );
    let actions = div()
        .flex()
        .flex_row()
        .gap_2()
        .child(crate::ui::button(
            "Cancel",
            false,
            |ws, _w, cx| ws.close_modal(cx),
            cx,
        ))
        .child(crate::ui::button(
            "Try Again",
            true,
            move |ws, _w, cx| {
                ws.close_modal(cx);
                ws.import_camera(source.clone(), area.clone(), cx);
            },
            cx,
        ));
    crate::ui::modal_frame(format!("Import from {label}"), 420.0, body, actions)
}
