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
use gpui::{img, AppContext as _, StatefulInteractiveElement as _};

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
        let context_menu = gallery_context_menu(self, cx);
        let root = div()
            .flex()
            .flex_col()
            .flex_grow()
            .min_h(px(0.0))
            .bg(gpui::rgb(pal().grid_bg))
            .text_color(gpui::rgb(pal().text))
            // The focus handle has to stay in the tree for keybindings
            // (⌘K preferences, ⌘⇧G back to the editor, ⌘O open) to
            // dispatch, exactly as the canvas and start screen keep it.
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|ws, ev: &gpui::KeyDownEvent, window, cx| {
                // A dialog over the gallery owns the keyboard, exactly
                // as the editor body arranges for its own dialogs:
                // Enter fires the primary button, everything else goes
                // to the focused field.
                if ws.modal.is_some() {
                    match ev.keystroke.key.as_str() {
                        "enter" => {
                            ws.commit_focused_field();
                            ws.confirm_modal(window, cx);
                        }
                        key => {
                            ws.field_key(key, ev.keystroke.key_char.as_deref());
                        }
                    }
                    cx.notify();
                    cx.stop_propagation();
                    return;
                }
                if ws.gallery_search_key(ev, cx) || ws.gallery_nav_key(ev, cx) {
                    cx.stop_propagation();
                }
            }))
            .child(top_strip(self, cx))
            .child(body)
            .child(tray(self, cx))
            .children(context_menu);
        // Rendering cells is what queues their thumbnails; make sure a
        // loader is running for whatever this frame asked for — and if
        // decodes have been failing for want of HEIC support, offer it.
        self.kick_thumb_loader(cx);
        // Smart buckets re-score whenever the index moved, so they
        // fill themselves as photos are indexed and imported.
        self.refresh_smart_buckets(cx);
        self.maybe_offer_heif(cx);
        self.gallery_reveal_tick(cx);
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
        .children(ws.library.map_filter_label().map(|label| {
            // While the map filter is on it wears the least
            // ignorable thing in the strip — a filter you forgot is a
            // gallery that looks mysteriously empty.
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .h(px(24.0))
                .px_2()
                .rounded_md()
                .bg(gpui::rgb(pal().select_border))
                .text_color(gpui::rgb(0xFFFFFF))
                .text_size(px(12.0))
                .cursor_pointer()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|ws, _e: &MouseDownEvent, _w, cx| ws.open_map_filter(cx)),
                )
                .child(format!("Map filter: {label}"))
                .child(
                    div()
                        .px_1()
                        .hover(|s| s.bg(gpui::rgb(0xFFFFFF30)).rounded_sm())
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|ws, _e: &MouseDownEvent, _w, cx| {
                                cx.stop_propagation();
                                ws.clear_map_filter(cx);
                            }),
                        )
                        .child("\u{2715}"),
                )
        }))
        .child(search_box(ws, cx))
        .child(div().flex_grow())
        .child(gallery_button(
            "Settings…",
            false,
            |ws, _w, cx| {
                ws.snapshot_preferences();
                ws.open_modal(Modal::Preferences, cx);
            },
            cx,
        ))
        .child(gallery_button(
            "Open…",
            false,
            crate::keymap::open_file_dialog,
            cx,
        ))
        .child(gallery_button(
            "New File…",
            false,
            |ws, _w, cx| ws.open_new_file_picker(cx),
            cx,
        ))
        .children(has_doc.then(|| {
            gallery_button(
                "Back to Editing",
                false,
                |ws, _w, cx| ws.toggle_gallery(cx),
                cx,
            )
        }))
}

/// The search box: type a description, photos rank by it. Takes the
/// keyboard while active (the key context flips to text entry, so
/// letters stop being tool shortcuts). Without the two Search models it
/// is a doorway to Manage Models instead of a lie.
fn search_box(ws: &mut Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let ready = schist_neural::embed::ready();
    let active = ws.library.search_active;
    let text = ws.library.search.clone();
    let (indexed, total) = ws.library.index_progress();
    let shown: SharedString = if text.is_empty() && !active {
        if !ready {
            // Places work off EXIF alone; content search needs the two
            // Search models (Gallery ▸ Manage Models…).
            "Search places… (models add content search)".into()
        } else if indexed < total {
            format!("Search ({indexed}/{total} indexed)").into()
        } else {
            "Search photos…".into()
        }
    } else {
        format!("{text}{}", if active { "\u{258f}" } else { "" }).into()
    };
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .w(px(260.0))
        .h(px(24.0))
        .px_2()
        .rounded_md()
        .bg(gpui::rgb(pal().grid_bg))
        .border_1()
        .border_color(gpui::rgb(if active {
            pal().select_border
        } else {
            pal().chrome_edge
        }))
        .text_size(px(12.0))
        .text_color(gpui::rgb(if text.is_empty() {
            pal().text_dim
        } else {
            pal().text
        }))
        .cursor(gpui::CursorStyle::IBeam)
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |ws, _e: &MouseDownEvent, _w, cx| {
                ws.library.search_active = true;
                // A click lands a caret, not a selection.
                ws.library.search_selected = false;
                // Focusing the box is the signal to start loading the
                // towers, so the first query answers quickly without
                // every gallery open paying their ~300 MB up front.
                ws.warm_search_engine(cx);
                cx.notify();
            }),
        )
        .child(div().flex_grow().truncate().child(
            // ⌘A's selection, drawn the way every field draws one.
            if ws.library.search_selected && !text.is_empty() {
                div()
                    .rounded_sm()
                    .px(px(1.0))
                    .bg(gpui::rgb(pal().select_border))
                    .text_color(gpui::rgb(0xFFFFFF))
                    .child(SharedString::from(text.clone()))
                    .into_any_element()
            } else {
                div().child(shown).into_any_element()
            },
        ))
        .children((!text.is_empty()).then(|| {
            div()
                .px_1()
                .text_color(gpui::rgb(pal().text_dim))
                .hover(|s| s.text_color(gpui::rgb(pal().text)))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|ws, _e: &MouseDownEvent, _w, cx| {
                        cx.stop_propagation();
                        ws.gallery_search_clear(cx);
                    }),
                )
                .child("\u{2715}")
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
                .child("GROUP BY"),
        )
        .child({
            let current = ws.library.group_by;
            let mut row = div().flex().flex_row().gap_1().px_2().pb_2();
            for group in super::library::GroupBy::ALL {
                let active = group == current;
                row = row.child(
                    div()
                        .px_2()
                        .h(px(20.0))
                        .flex()
                        .items_center()
                        .rounded_md()
                        .text_size(px(11.0))
                        .cursor_pointer()
                        .bg(gpui::rgb(if active {
                            pal().sidebar_selected
                        } else {
                            pal().button_bg
                        }))
                        .hover(move |s| {
                            if active {
                                s
                            } else {
                                s.bg(gpui::rgb(pal().button_hover))
                            }
                        })
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |ws, _e: &MouseDownEvent, _w, cx| {
                                ws.set_gallery_group(group, cx);
                            }),
                        )
                        .child(group.label()),
                );
            }
            row
        })
        .child({
            let active = ws.library.map_filter.is_some();
            div()
                .mx_2()
                .mb_1()
                .px_2()
                .h(px(22.0))
                .flex()
                .items_center()
                .justify_between()
                .rounded_md()
                .text_size(px(11.0))
                .cursor_pointer()
                .bg(gpui::rgb(if active {
                    pal().select_border
                } else {
                    pal().button_bg
                }))
                .text_color(gpui::rgb(if active { 0xFFFFFF } else { pal().text }))
                .hover(move |s| {
                    if active {
                        s
                    } else {
                        s.bg(gpui::rgb(pal().button_hover))
                    }
                })
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|ws, _e: &MouseDownEvent, _w, cx| ws.open_map_filter(cx)),
                )
                .child(if active {
                    "Map filter on"
                } else {
                    "Map filter…"
                })
                .children(active.then(|| div().child("\u{25cf}")))
        })
        .child(
            div()
                .px_2()
                .pt_1()
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
        .child(
            div()
                .px_2()
                .pt_2()
                .pb_1()
                .text_size(px(11.0))
                .text_color(gpui::rgb(pal().text_dim))
                .child("BUCKETS"),
        )
        .children({
            let buckets: Vec<(usize, String, usize, bool)> = ws
                .library
                .buckets
                .iter()
                .enumerate()
                .map(|(i, b)| (i, b.name.clone(), b.contents().len(), b.is_smart()))
                .collect();
            let viewing = ws.library.bucket_filter;
            let mut rows: Vec<gpui::AnyElement> = Vec::new();
            for (i, name, count, smart) in buckets {
                rows.push(
                    bucket_row(i, name, count, smart, viewing == Some(i), cx).into_any_element(),
                );
            }
            rows
        })
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
                    cx.listener(|ws, _e: &MouseDownEvent, _w, cx| {
                        // Born holding the selection, so "new bucket
                        // from these" is the dialog's Create away.
                        let selected = ws.library.selected.clone();
                        ws.gallery_new_bucket(selected, cx);
                    }),
                )
                .child("+ New bucket"),
        )
}

/// One bucket in the sidebar: a drop target, a view of its contents on
/// click, and its own right-click menu for the group actions. Smart
/// buckets — the self-filling kind — wear a ✦.
fn bucket_row(
    index: usize,
    name: String,
    count: usize,
    smart: bool,
    viewing: bool,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    use super::library::{GalleryContext, GalleryDrag};
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .px_2()
        .h(px(24.0))
        .text_size(px(12.0))
        .cursor_pointer()
        .bg(gpui::rgb(if viewing {
            pal().sidebar_selected
        } else {
            pal().chrome_bg
        }))
        .hover(|s| s.bg(gpui::rgb(pal().sidebar_selected)))
        .drag_over::<GalleryDrag>(|s, _, _, _| s.bg(gpui::rgb(pal().select_border)))
        .on_drop(cx.listener(move |ws, drag: &GalleryDrag, _w, cx| {
            ws.library.add_to_bucket(index, &drag.paths);
            cx.notify();
        }))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |ws, _e: &MouseDownEvent, _w, cx| {
                ws.library.bucket_filter = if ws.library.bucket_filter == Some(index) {
                    None
                } else {
                    Some(index)
                };
                ws.library.folder_filter = None;
                cx.notify();
            }),
        )
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(move |ws, ev: &MouseDownEvent, _w, cx| {
                ws.library.context = Some((ev.position, GalleryContext::Bucket(index)));
                cx.notify();
            }),
        )
        .child(
            div()
                .flex_grow()
                .truncate()
                .child(SharedString::from(if smart {
                    format!("\u{2726} {name}")
                } else {
                    name
                })),
        )
        .child(
            div()
                .text_size(px(10.0))
                .text_color(gpui::rgb(pal().text_dim))
                .child(format!("{count}")),
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
                ws.library.bucket_filter = None;
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
    if let Some(drop_root) = root.clone() {
        // Dragged photos land here as a move — files, sidecars,
        // versions and all.
        row = row
            .drag_over::<super::library::GalleryDrag>(|s, _, _, _| {
                s.bg(gpui::rgb(pal().select_border))
            })
            .on_drop(
                cx.listener(move |ws, drag: &super::library::GalleryDrag, _w, cx| {
                    ws.move_photos_to(drag.paths.clone(), drop_root.clone(), cx);
                }),
            );
    }
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
    let selected: Vec<PathBuf> = ws.library.selected.clone();
    let hide_flagged = ws.view.gallery_hide_nsfw;
    // Owned snapshot: the cells below borrow the workspace mutably to
    // fetch thumbnails, so they cannot also iterate `sections` in place.
    let mut sections: Vec<(String, String, Vec<super::library::Entry>)> =
        if let Some(results) = &ws.library.search_results {
            // A search flattens the groups into one ranked strip.
            let by_path: FxHashMap<&PathBuf, &super::library::Entry> = ws
                .library
                .sections
                .iter()
                .flat_map(|s| s.entries.iter())
                .map(|e| (&e.path, e))
                .collect();
            let entries: Vec<super::library::Entry> = results
                .iter()
                .filter_map(|(path, _)| by_path.get(path).map(|e| (*e).clone()))
                .filter(|e| ws.library.passes_map(&e.path))
                .collect();
            let title = match &ws.library.search_place {
                Some(place) => format!("Search results · near {place}"),
                None => "Search results".to_string(),
            };
            vec![(title, String::new(), entries)]
        } else {
            ws.library.grouped()
        };
    // The content filter applies whatever the grouping.
    for (_, _, entries) in &mut sections {
        entries.retain(|e| !(hide_flagged && ws.library.is_flagged(&e.path)));
    }
    sections.retain(|(_, _, entries)| !entries.is_empty());
    let scanning = ws.library.scanning;
    let grid_entity = cx.entity();
    let mut column = div()
        .id("gallery-grid")
        .flex()
        .flex_col()
        .flex_grow()
        .min_h(px(0.0))
        .overflow_y_scroll()
        .track_scroll(&ws.library.grid_scroll)
        .bg(gpui::rgb(pal().grid_bg))
        .p_2()
        // Record the viewport rectangle, so keyboard navigation can
        // work out columns per row and keep the selection on screen.
        .child(
            canvas(
                move |bounds, _window, cx| {
                    grid_entity.update(cx, |ws, _| ws.library.grid_bounds = bounds);
                },
                |_, _, _, _| {},
            )
            .absolute()
            .size_full(),
        );
    if sections.is_empty() {
        // Say why the grid is bare, rather than showing a void: a
        // bucket may simply be empty, a scan may be running, or the
        // watched folders may hold nothing Schist can decode.
        column = column.child(
            div()
                .p_4()
                .text_size(px(12.0))
                .text_color(gpui::rgb(pal().text_dim))
                .child(
                    match ws
                        .library
                        .bucket_filter
                        .and_then(|i| ws.library.buckets.get(i))
                    {
                        Some(bucket) if bucket.is_smart() => {
                            "Nothing matches this bucket's rule yet — matches appear as \
                         photos are indexed. Dragging photos in works too."
                        }
                        Some(_) => {
                            "This bucket is empty. Drag photos onto its row in the sidebar \
                         to add them."
                        }
                        None if scanning => "Scanning folders\u{2026}",
                        None => {
                            "No photos found in the watched folders. Images Schist can open \
                         (PNG, JPEG, WebP, TIFF, HEIC, PSD, Affinity) appear here; \
                         sub-folders are scanned six levels deep."
                        }
                    },
                ),
        );
    }
    for (title, subtitle, entries) in sections {
        let detail = if subtitle.is_empty() {
            format!("{} photos", entries.len())
        } else {
            format!("{subtitle} — {}", entries.len())
        };
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
                        .child(detail),
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
    selected: &[PathBuf],
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    use super::library::{GalleryContext, GalleryDrag};
    let thumb = ws.library.thumb(&entry);
    let is_selected = selected.iter().any(|p| p == &entry.path);
    let is_lead = selected.last() == Some(&entry.path);
    let click_path = entry.path.clone();
    let context_path = entry.path.clone();
    let drag_path = entry.path.clone();
    // Dragging carries the whole selection when the pressed cell is in
    // it, and just the pressed cell otherwise.
    let drag_paths: Vec<PathBuf> = if is_selected {
        selected.to_vec()
    } else {
        vec![entry.path.clone()]
    };
    let inner = cell - 10.0;
    div()
        .id(SharedString::from(format!("cell-{}", entry.path.display())))
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
        .on_drag(
            GalleryDrag { paths: drag_paths },
            move |drag, _offset, _window, cx| {
                let label = if drag.paths.len() == 1 {
                    drag_path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "1 photo".into())
                } else {
                    format!("{} photos", drag.paths.len())
                };
                cx.new(|_| DragGhost { label })
            },
        )
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |ws, ev: &MouseDownEvent, _w, cx| {
                let path = click_path.clone();
                if ev.modifiers.platform || ev.modifiers.control {
                    // ⌘-click: in or out, keeping the rest.
                    ws.library.toggle_selected(path);
                } else if ev.modifiers.shift {
                    ws.gallery_select_range_to(path);
                } else if ev.click_count >= 2 {
                    ws.library.select_single(path.clone());
                    ws.open_from_gallery(path, cx);
                } else if !ws.library.is_selected(&click_path) {
                    // A plain press on an unselected photo selects it —
                    // and on a selected one keeps the selection, so a
                    // drag can carry the lot.
                    ws.library.select_single(path);
                }
                ws.library.context = None;
                cx.notify();
            }),
        )
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(move |ws, ev: &MouseDownEvent, _w, cx| {
                // Right-click acts on the selection when it lands in
                // it, on this photo alone otherwise.
                if !ws.library.is_selected(&context_path) {
                    ws.library.select_single(context_path.clone());
                }
                ws.library.context =
                    Some((ev.position, GalleryContext::Photo(context_path.clone())));
                cx.notify();
            }),
        )
        .children(is_lead.then(|| {
            // The lead cell reports where it landed, for the
            // keyboard's scroll-into-view.
            let cell_entity = cx.entity();
            canvas(
                move |bounds, _window, cx| {
                    cell_entity.update(cx, |ws, _| ws.library.selected_bounds = Some(bounds));
                },
                |_, _, _, _| {},
            )
            .absolute()
            .size_full()
        }))
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

/// The little ghost that rides the pointer during a drag.
struct DragGhost {
    label: String,
}

impl gpui::Render for DragGhost {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .rounded_md()
            .bg(gpui::rgb(pal().select_border))
            .text_color(gpui::rgb(0xFFFFFF))
            .text_size(px(11.0))
            .child(SharedString::from(self.label.clone()))
    }
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
        .children((ws.library.selected.len() > 1).then(|| {
            div()
                .text_size(px(11.0))
                .text_color(gpui::rgb(pal().text_dim))
                .child(format!("{} selected", ws.library.selected.len()))
        }))
        .children(selected.as_ref().is_some_and(|e| e.edited).then(|| {
            div()
                .text_size(px(11.0))
                .text_color(gpui::rgb(pal().text_dim))
                .child("edited — versions kept beside the file")
        }))
        .children({
            let hidden = if ws.view.gallery_hide_nsfw {
                ws.library.flagged_count()
            } else {
                0
            };
            (hidden > 0).then(|| {
                div()
                    .text_size(px(11.0))
                    .text_color(gpui::rgb(pal().text_dim))
                    .child(format!("{hidden} hidden by the content filter"))
            })
        })
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
/// The shared boundary editor: preset chips, the navigable map, and
/// the draw/clear/zoom tools under it. The import dialog and the map
/// filter both edit the same drawn boundary through this.
fn boundary_editor(ws: &mut Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    use super::library_geo::PLACES;
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

    div()
        .flex()
        .flex_col()
        .gap_2()
        .child(chips)
        .child(map_element(ws, cx))
        .child(tools)
}

pub(crate) fn camera_import_options_dialog(
    ws: &mut Workspace,
    source: ImportSource,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let label = super::library::source_label(&source);
    let selection = ws.library.map.selection;
    let selection_name = ws.library.map.selection_name.clone();
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
        .child(boundary_editor(ws, cx))
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

/// The gallery's map filter: draw where, Apply, and the grid shows
/// only photos taken there — with the loud banner in the top strip
/// saying so until it is turned off.
pub(crate) fn map_filter_dialog(
    ws: &mut Workspace,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let selection = ws.library.map.selection;
    let filtering = ws.library.map_filter.is_some();
    let status = match (&selection, &ws.library.map.selection_name) {
        (Some(_), Some(name)) => format!(
            "Apply shows only photos taken in {name}; photos without an EXIF position hide."
        ),
        (Some(b), None) => format!(
            "Apply shows only photos taken inside {:.3}°, {:.3}° to {:.3}°, {:.3}°; \
             photos without an EXIF position hide.",
            b.south, b.west, b.north, b.east
        ),
        (None, _) => "Draw a boundary (Shift-drag, or a preset chip), then Apply. Applying with \
             nothing drawn turns the filter off."
            .to_string(),
    };
    let body = div()
        .flex()
        .flex_col()
        .gap_2()
        .child(boundary_editor(ws, cx))
        .child(
            div()
                .text_size(px(11.0))
                .text_color(gpui::rgb(crate::ui::palette().text_dim))
                .child(status),
        );
    let mut actions = div().flex().flex_row().gap_2();
    if filtering {
        actions = actions.child(crate::ui::button(
            "Turn Filter Off",
            false,
            |ws, _w, cx| {
                ws.clear_map_filter(cx);
                ws.close_modal(cx);
            },
            cx,
        ));
    }
    actions = actions
        .child(crate::ui::button(
            "Cancel",
            false,
            |ws, _w, cx| ws.close_modal(cx),
            cx,
        ))
        .child(crate::ui::button(
            "Apply",
            true,
            |ws, _w, cx| ws.apply_map_filter(cx),
            cx,
        ));
    crate::ui::modal_frame("Map Filter", 580.0, body, actions)
}

/// One text field of the bucket dialog: the layer-name pattern, with a
/// dimmed placeholder while nothing is typed.
fn bucket_field(
    id: &'static str,
    value: String,
    placeholder: String,
    ws: &Workspace,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let focused = ws.focused_field == Some(id);
    let typed = if focused && !ws.field_buffer.is_empty() {
        ws.field_buffer.clone()
    } else {
        value
    };
    let empty = typed.is_empty();
    // The caret belongs to what was typed, never to the placeholder —
    // "Bucket 1|" in full text colour read as an already-filled field.
    // While the field is empty the caret sits alone at the start, the
    // placeholder ghosted behind it, the way every real input does it.
    let caret_and_text = div()
        .text_color(gpui::rgb(crate::ui::palette().text))
        .child(if focused {
            format!("{typed}|")
        } else {
            typed.clone()
        });
    let mut field = div()
        .w(px(360.0))
        .h(px(22.0))
        .px_1()
        .flex()
        .flex_row()
        .items_center()
        .rounded_sm()
        .bg(gpui::rgb(crate::ui::palette().field_bg))
        .border_1()
        .border_color(gpui::rgb(if focused {
            crate::ui::palette().accent
        } else {
            crate::ui::palette().field_bg
        }))
        .text_size(px(12.0))
        .overflow_hidden()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |ws, _e: &MouseDownEvent, _w, cx| {
                ws.commit_focused_field();
                ws.focus_field(id, typed.clone());
                cx.notify();
            }),
        )
        .child(caret_and_text);
    if empty {
        field = field.child(
            div()
                .text_color(gpui::rgb(crate::ui::palette().text_dim))
                .child(placeholder),
        );
    }
    field
}

/// Create or edit a bucket: its name, and the optional smart rule —
/// a search query, an area drawn on the map, or both — that keeps it
/// filling itself as photos are indexed and imported.
pub(crate) fn bucket_name_dialog(
    ws: &mut Workspace,
    name: String,
    query: String,
    photos: usize,
    editing: Option<usize>,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let name_fallback = match editing.and_then(|i| ws.library.buckets.get(i)) {
        Some(bucket) => bucket.name.clone(),
        None => format!("Bucket {}", ws.library.buckets.len() + 1),
    };
    let name_field = bucket_field("bucket-name", name, name_fallback, ws, cx);
    let query_field = bucket_field(
        "bucket-query",
        query.clone(),
        "e.g. dog on a beach (optional)".to_string(),
        ws,
        cx,
    );
    // What the rule adds up to right now, so nothing is set silently —
    // the map keeps its boundary between dialogs by design, and this
    // line is where a leftover one gets noticed.
    let area_name = ws.library.map.selection.map(|_| {
        ws.library
            .map
            .selection_name
            .clone()
            .unwrap_or_else(|| "the drawn area".to_string())
    });
    let live_query = if ws.focused_field == Some("bucket-query") && !ws.field_buffer.is_empty() {
        ws.field_buffer.clone()
    } else {
        query
    };
    let rule_line = match (live_query.trim(), &area_name) {
        ("", None) => "No rule: an ordinary bucket, filled by dragging photos in.".to_string(),
        (q, None) => format!("Keeps every photo matching \u{201c}{q}\u{201d}."),
        ("", Some(area)) => format!("Keeps every photo taken in {area}."),
        (q, Some(area)) => {
            format!("Keeps every photo matching \u{201c}{q}\u{201d} taken in {area}.")
        }
    };
    let mut body = div()
        .flex()
        .flex_col()
        .gap_2()
        .child(crate::ui::field_row("Name", name_field))
        .child(crate::ui::field_row("Search", query_field))
        .child(
            div()
                .text_size(px(11.0))
                .text_color(gpui::rgb(crate::ui::palette().text_dim))
                .child(
                    "Shift-drag the map (or pick a preset) to add an area; \
                     a tiny drag clears it. Rules keep the bucket filled \
                     automatically.",
                ),
        )
        .child(boundary_editor(ws, cx))
        .child(
            div()
                .text_size(px(11.0))
                .text_color(gpui::rgb(crate::ui::palette().text_dim))
                .child(rule_line),
        );
    if photos > 0 {
        body = body.child(
            div()
                .text_size(px(11.0))
                .text_color(gpui::rgb(crate::ui::palette().text_dim))
                .child(if photos == 1 {
                    "Starts holding the selected photo.".to_string()
                } else {
                    format!("Starts holding the {photos} selected photos.")
                }),
        );
    }
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
            if editing.is_some() { "Save" } else { "Create" },
            true,
            |ws, _w, cx| {
                ws.commit_focused_field();
                let Some(Modal::BucketName {
                    name,
                    query,
                    photos,
                    editing,
                }) = ws.modal.clone()
                else {
                    return;
                };
                let query = {
                    let q = query.trim();
                    (!q.is_empty()).then(|| q.to_string())
                };
                let area = ws.library.map.selection.map(|bounds| {
                    (
                        bounds,
                        ws.library
                            .map
                            .selection_name
                            .clone()
                            .unwrap_or_else(|| "Selected Area".to_string()),
                    )
                });
                let index = match editing {
                    Some(index) => index,
                    None => ws.library.add_bucket(name.clone()),
                };
                ws.library.configure_bucket(index, name, query, area);
                if !photos.is_empty() {
                    ws.library.add_to_bucket(index, &photos);
                }
                ws.close_modal(cx);
            },
            cx,
        ));
    let title = if editing.is_some() {
        "Edit Bucket"
    } else {
        "New Bucket"
    };
    crate::ui::modal_frame(title, 580.0, body, actions)
}

/// The gallery's right-click menu: on a photo it acts on the whole
/// selection, on a bucket it acts on the bucket.
fn gallery_context_menu(
    ws: &mut Workspace,
    cx: &mut Context<Workspace>,
) -> Option<gpui::AnyElement> {
    use super::library::GalleryContext;
    let (position, target) = ws.library.context.clone()?;
    /// What a menu row does when clicked.
    type RowAction = std::rc::Rc<dyn Fn(&mut Workspace, &mut Window, &mut Context<Workspace>)>;
    let mut rows: Vec<gpui::AnyElement> = Vec::new();
    let row = |label: String,
               rows: &mut Vec<gpui::AnyElement>,
               cx: &mut Context<Workspace>,
               act: RowAction| {
        rows.push(
            div()
                .px_2()
                .h(px(24.0))
                .flex()
                .items_center()
                .text_size(px(12.0))
                .cursor_pointer()
                .hover(|s| {
                    s.bg(gpui::rgb(crate::ui::palette().accent))
                        .text_color(gpui::rgb(crate::ui::palette().accent_text))
                })
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |ws, _e: &MouseDownEvent, window, cx| {
                        ws.library.context = None;
                        act(ws, window, cx);
                        cx.notify();
                    }),
                )
                .child(SharedString::from(label))
                .into_any_element(),
        );
    };
    let sep = |rows: &mut Vec<gpui::AnyElement>| {
        rows.push(
            div()
                .h(px(1.0))
                .my_1()
                .bg(gpui::rgb(crate::ui::palette().edge))
                .into_any_element(),
        );
    };
    match target {
        GalleryContext::Photo(path) => {
            // The right-click handler put this photo in the selection,
            // so the selection is what every action takes.
            let acting = ws.library.selected.clone();
            let n = acting.len();

            {
                let open = path.clone();
                row(
                    "Edit".into(),
                    &mut rows,
                    cx,
                    std::rc::Rc::new(move |ws, _w, cx| {
                        ws.open_from_gallery(open.clone(), cx);
                    }),
                );
            }
            {
                let reveal = path.clone();
                row(
                    "Reveal in file manager".into(),
                    &mut rows,
                    cx,
                    std::rc::Rc::new(move |_ws, _w, _cx| {
                        super::library_ops::reveal_in_file_manager(&reveal);
                    }),
                );
            }
            sep(&mut rows);
            for (i, bucket) in ws.library.buckets.iter().enumerate() {
                let add = acting.clone();
                row(
                    format!("Add to {}", bucket.name),
                    &mut rows,
                    cx,
                    std::rc::Rc::new(move |ws, _w, _cx| {
                        ws.library.add_to_bucket(i, &add);
                    }),
                );
            }
            {
                let add = acting.clone();
                row(
                    "Add to new bucket".into(),
                    &mut rows,
                    cx,
                    std::rc::Rc::new(move |ws, _w, cx| {
                        ws.gallery_new_bucket(add.clone(), cx);
                    }),
                );
            }
            // Only hand-added photos can be removed — a smart rule's
            // match would just come back on the next pass.
            let removable = ws.library.bucket_filter.filter(|&b| {
                ws.library
                    .buckets
                    .get(b)
                    .is_some_and(|bucket| bucket.photos.contains(&path))
            });
            if let Some(bucket) = removable {
                let drop_path = path.clone();
                row(
                    "Remove from this bucket".into(),
                    &mut rows,
                    cx,
                    std::rc::Rc::new(move |ws, _w, _cx| {
                        ws.library.remove_from_bucket(bucket, &drop_path);
                    }),
                );
            }
            sep(&mut rows);
            {
                let zip = acting.clone();
                let label = if n > 1 {
                    format!("Save {n} as ZIP…")
                } else {
                    "Save as ZIP…".to_string()
                };
                row(
                    label,
                    &mut rows,
                    cx,
                    std::rc::Rc::new(move |ws, window, cx| {
                        ws.save_photos_zip(zip.clone(), "photos.zip".into(), window, cx);
                    }),
                );
            }
            {
                let up = acting;
                let label = if n > 1 {
                    format!("Upscale {n} ×2")
                } else {
                    "Upscale ×2".to_string()
                };
                row(
                    label,
                    &mut rows,
                    cx,
                    std::rc::Rc::new(move |ws, _w, cx| {
                        ws.upscale_photos(up.clone(), cx);
                    }),
                );
            }
        }
        GalleryContext::Bucket(index) => {
            // The group actions act on everything the bucket holds:
            // the hand-picked photos and the smart rule's matches.
            let (photos, name, smart) = ws
                .library
                .buckets
                .get(index)
                .map(|b| (b.contents(), b.name.clone(), b.is_smart()))
                .unwrap_or_default();
            row(
                "Edit bucket…".into(),
                &mut rows,
                cx,
                std::rc::Rc::new(move |ws, _w, cx| {
                    ws.gallery_edit_bucket(index, cx);
                }),
            );
            sep(&mut rows);
            {
                let zip = photos.clone();
                let suggested = format!("{}.zip", name.to_lowercase().replace(' ', "-"));
                row(
                    format!("Save all as ZIP… ({})", photos.len()),
                    &mut rows,
                    cx,
                    std::rc::Rc::new(move |ws, window, cx| {
                        ws.save_photos_zip(zip.clone(), suggested.clone(), window, cx);
                    }),
                );
            }
            {
                let up = photos;
                row(
                    "Upscale all ×2".into(),
                    &mut rows,
                    cx,
                    std::rc::Rc::new(move |ws, _w, cx| {
                        ws.upscale_photos(up.clone(), cx);
                    }),
                );
            }
            sep(&mut rows);
            row(
                // A smart bucket's matches come back on the next pass;
                // only the hand-added photos are the user's to clear.
                if smart {
                    "Clear added photos".into()
                } else {
                    "Clear bucket".into()
                },
                &mut rows,
                cx,
                std::rc::Rc::new(move |ws, _w, _cx| {
                    ws.library.clear_bucket(index);
                }),
            );
            row(
                "Delete bucket".into(),
                &mut rows,
                cx,
                std::rc::Rc::new(move |ws, _w, _cx| {
                    ws.library.delete_bucket(index);
                }),
            );
        }
    }
    Some(
        gpui::deferred(
            div()
                .absolute()
                .left(position.x)
                .top(position.y)
                .w(px(220.0))
                .py_1()
                .bg(gpui::rgb(crate::ui::palette().popup_bg))
                .text_color(gpui::rgb(crate::ui::palette().text))
                .border_1()
                .border_color(gpui::rgb(crate::ui::palette().edge))
                .rounded_sm()
                .shadow_lg()
                .occlude()
                .on_mouse_down_out(cx.listener(|ws, _e, _w, cx| {
                    ws.library.context = None;
                    cx.notify();
                }))
                .children(rows),
        )
        .into_any_element(),
    )
}
