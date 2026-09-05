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
use gpui::{
    img, prelude::FluentBuilder as _, AppContext as _, StatefulInteractiveElement as _,
    StyledImage as _,
};

/// The gallery's chrome colours for one theme.
pub(super) struct GalleryPalette {
    /// Behind the thumbnails.
    pub(super) grid_bg: u32,
    /// The top strip and sidebar.
    pub(super) chrome_bg: u32,
    pub(super) chrome_edge: u32,
    pub(super) tray_bg: u32,
    pub(super) sidebar_selected: u32,
    /// Folder headers and the add-folder link — Picasa's blue.
    pub(super) header: u32,
    pub(super) text: u32,
    pub(super) text_dim: u32,
    pub(super) cell_edge: u32,
    /// Cell border under the pointer.
    pub(super) cell_hover: u32,
    pub(super) select_border: u32,
    pub(super) select_fill: u32,
    pub(super) button_bg: u32,
    pub(super) button_hover: u32,
    /// The green action buttons and the "edited" badge.
    pub(super) green: u32,
    pub(super) green_hover: u32,
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

pub(super) fn pal() -> &'static GalleryPalette {
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
                .child(if self.library.map_view {
                    world_map(self, cx).into_any_element()
                } else if self.library.viewer.is_some() {
                    super::library_people_view::viewer(self, cx)
                } else {
                    grid(self, cx).into_any_element()
                })
                // The same AI panel the editor has, on its own switch
                // (View ▸ AI Panel here too): the conversation, harness
                // and model carry over, the prompt says which room.
                .children(crate::panels::ai_sidebar(self, cx))
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
                if ws.gallery_viewer_key(ev, cx)
                    || ws.gallery_search_key(ev, cx)
                    || ws.gallery_nav_key(ev, cx)
                {
                    cx.stop_propagation();
                }
            }))
            .child(top_strip(self, cx))
            .child(body)
            .child(tray(self, cx))
            .children(context_menu)
            .child(drag_out_listener(cx));
        // Each cell's paint-time probe is what queues its thumbnail;
        // mark the frame so those probes stamp as "current" (the age
        // eviction refuses), and make sure a loader is running for
        // whatever the last frame asked for — and if decodes have been
        // failing for want of HEIC support, offer it.
        self.library.begin_thumb_frame();
        self.kick_thumb_loader(cx);
        // Smart buckets re-score whenever the index moved, so they
        // fill themselves as photos are indexed and imported — and a
        // search follows the bucket on show.
        self.refresh_smart_buckets(cx);
        self.gallery_search_rescope(cx);
        self.maybe_offer_heif(cx);
        self.gallery_reveal_tick(cx);
        root
    }
}

/// A drag that leaves for another window is a drag onto the desktop:
/// hand it to the platform's own drag-and-drop, which is what Finder,
/// Explorer and the file managers listen to. gpui's drag is internal
/// and would simply be lost out there, so it ends here.
///
/// Registered on the window rather than on an element: element mouse
/// listeners only fire while the pointer hovers their hitbox, and the
/// whole point is the pointer having left — under a held button every
/// platform keeps reporting its position past our edges, and this is
/// the only kind of listener that still hears those reports.
fn drag_out_listener(cx: &mut Context<Workspace>) -> impl IntoElement {
    let entity = cx.entity();
    canvas(
        |_, _, _| {},
        move |_bounds, _state, window, _cx| {
            let moves = entity.clone();
            window.on_mouse_event(move |ev: &gpui::MouseMoveEvent, phase, window, cx| {
                if phase != gpui::DispatchPhase::Bubble {
                    return;
                }
                let paths = moves.update(cx, |ws, _| {
                    if ev.pressed_button != Some(MouseButton::Left) {
                        // The button came up somewhere we never heard about.
                        ws.library.dragging = None;
                    }
                    ws.library.dragging.clone()
                });
                let Some(paths) = paths else { return };
                if !crate::drag_out::over_foreign_window(window) {
                    return;
                }
                moves.update(cx, |ws, _| ws.library.dragging = None);
                if crate::drag_out::start(&paths, window) {
                    // The platform owns the drag now; two ghosts
                    // following one pointer is one too many.
                    cx.stop_active_drag(window);
                }
            });
            let ups = entity.clone();
            window.on_mouse_event(move |_ev: &gpui::MouseUpEvent, phase, _window, cx| {
                if phase == gpui::DispatchPhase::Bubble {
                    ups.update(cx, |ws, _| ws.library.dragging = None);
                }
            });
        },
    )
    .absolute()
    .size_0()
}

pub(super) fn gallery_button(
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
        .child(search_slot(ws, cx))
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
/// What sits in the middle of the top strip: the search box once photo
/// search can run, and before that the offer to install it — a single
/// button for both models, which becomes a progress bar while they
/// download and gives way to the box when they land.
fn search_slot(ws: &mut Workspace, cx: &mut Context<Workspace>) -> gpui::AnyElement {
    if schist_neural::embed::ready() {
        return search_box(ws, cx).into_any_element();
    }
    let offer = if search_models_downloading(ws) {
        search_download_progress(ws).into_any_element()
    } else {
        gallery_button(
            "Enable photo search\u{2026}",
            false,
            |ws, _w, cx| ws.open_modal(Modal::SearchModels, cx),
            cx,
        )
        .into_any_element()
    };
    if ws.library.people.is_empty() {
        return offer;
    }
    // Without the towers the box still searches names and places, so
    // once anyone has been named it is worth having beside the offer.
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .child(search_box(ws, cx))
        .child(offer)
        .into_any_element()
}

/// The download of the two Search models, as a bar in the top strip.
fn search_download_progress(ws: &Workspace) -> impl IntoElement {
    let mut got = 0u64;
    let mut total = 0u64;
    for id in SEARCH_MODELS {
        let Some(spec) = schist_neural::spec(id) else {
            continue;
        };
        total += spec.bytes as u64;
        // A model already installed is wholly got; one downloading has
        // its counter; one not started yet has nothing.
        got += if schist_neural::installed(id) {
            spec.bytes as u64
        } else {
            ws.model_downloads
                .iter()
                .find(|d| d.id == id)
                .map(|d| d.got.load(std::sync::atomic::Ordering::Relaxed))
                .unwrap_or(0)
        };
    }
    let mb = |bytes: u64| bytes as f64 / (1 << 20) as f64;
    let ratio = if total == 0 {
        0.0
    } else {
        (got as f64 / total as f64).clamp(0.0, 1.0) as f32
    };
    div()
        .flex()
        .flex_col()
        .justify_center()
        .gap_1()
        .w(px(260.0))
        .h(px(24.0))
        .child(
            div()
                .text_size(px(10.0))
                .text_color(gpui::rgb(pal().text_dim))
                .child(SharedString::from(format!(
                    "Downloading photo search\u{2026} {:.0} of {:.0} MB",
                    mb(got),
                    mb(total)
                ))),
        )
        .child(
            div()
                .w_full()
                .h(px(4.0))
                .rounded_sm()
                .bg(gpui::rgb(pal().chrome_edge))
                .child(
                    div()
                        .h_full()
                        .w(gpui::relative(ratio))
                        .rounded_sm()
                        .bg(gpui::rgb(pal().select_border)),
                ),
        )
}

/// The box itself, which only exists once the models behind it do —
/// `search_slot` is what decides that.
fn search_box(ws: &mut Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let active = ws.library.search_active;
    let text = ws.library.search.clone();
    let (indexed, total) = ws.library.index_progress();
    let placeholder: SharedString = if !schist_neural::embed::ready() {
        "Search people and places\u{2026}".into()
    } else if indexed < total {
        format!("Search ({indexed}/{total} indexed)").into()
    } else {
        "Search photos\u{2026}".into()
    };
    let cursor = ws.library.search_cursor.min(text.len());
    let caret_on = ws.caret_on();
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
                ws.library.search_cursor = ws.library.search.len();
                ws.reset_caret_phase();
                // Focusing the box is the signal to start loading the
                // towers, so the first query answers quickly without
                // every gallery open paying their ~300 MB up front.
                ws.warm_search_engine(cx);
                cx.notify();
            }),
        )
        .child(div().flex_grow().truncate().child(
            // ⌘A's selection, drawn the way every field draws one; a
            // focused box otherwise shows a blinking caret the arrows
            // move, with the placeholder ghosted while it is empty.
            if ws.library.search_selected && !text.is_empty() {
                div()
                    .rounded_sm()
                    .px(px(1.0))
                    .bg(gpui::rgb(pal().select_border))
                    .text_color(gpui::rgb(0xFFFFFF))
                    .child(SharedString::from(text.clone()))
                    .into_any_element()
            } else if active {
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .child(crate::ui::caret_run(
                        text[..cursor].to_string(),
                        text[cursor..].to_string(),
                        caret_on,
                        pal().text,
                    ))
                    .children(text.is_empty().then(|| {
                        div()
                            .text_color(gpui::rgb(pal().text_dim))
                            .child(placeholder.clone())
                    }))
                    .into_any_element()
            } else if text.is_empty() {
                div().child(placeholder.clone()).into_any_element()
            } else {
                div()
                    .child(SharedString::from(text.clone()))
                    .into_any_element()
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

/// Nothing watched yet, which for most people is the first launch:
/// welcome them, and offer all four ways in — the two that fill the
/// gallery, and the two that open the editor — since a fresh Schist
/// has no other screen to say hello from.
fn gallery_empty_state(cx: &mut Context<Workspace>) -> impl IntoElement {
    let caption = |text: &'static str| {
        div()
            .text_size(px(12.0))
            .text_color(gpui::rgb(pal().text_dim))
            .child(text)
    };
    // One column of a readable measure, centred on the screen, with
    // everything inside it set flush left: centred ragged prose reads
    // as a poster, and this is a page.
    let column = div()
        .flex()
        .flex_col()
        .items_start()
        .w(px(520.0))
        .gap_2()
        .child(
            div()
                .text_size(px(22.0))
                .text_color(gpui::rgb(pal().text))
                .child("Welcome to Schist"),
        )
        .child(div().h(px(12.0)))
        .child(caption(
            "Watch folders of photos, or import from a camera. Files stay \
             where they are; edits are versioned beside them:",
        ))
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
        .child(div().h(px(4.0)))
        .child(
            // A hairline between the two ways in, the width of the
            // column rather than the whole window.
            div()
                .w_full()
                .h(px(1.0))
                .my_2()
                .bg(gpui::rgb(pal().cell_edge)),
        )
        .child(caption("Or open an image to edit:"))
        .child(
            div()
                .flex()
                .flex_row()
                .gap_2()
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
                )),
        )
        .child(div().h(px(8.0)))
        .child(caption(if cfg!(target_os = "macos") {
            "The gallery is always \u{2318}\u{21e7}G away, whatever you are editing."
        } else {
            "The gallery is always Ctrl+Shift+G away, whatever you are editing."
        }));
    div()
        .flex()
        .flex_col()
        .flex_grow()
        .items_center()
        .justify_center()
        .child(column)
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
                .child("VIEW"),
        )
        .children(
            [(false, "Photos"), (true, "World Map")]
                .into_iter()
                .map(|(map, label)| {
                    let active = ws.library.map_view == map;
                    div()
                        .px_2()
                        .h(px(26.0))
                        .flex()
                        .items_center()
                        .text_size(px(12.0))
                        .cursor_pointer()
                        .bg(gpui::rgb(if active {
                            pal().sidebar_selected
                        } else {
                            pal().chrome_bg
                        }))
                        .hover(|s| s.bg(gpui::rgb(pal().sidebar_selected)))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |ws, _, _, cx| {
                                ws.library.map_view = map;
                                cx.notify();
                            }),
                        )
                        .child(label)
                }),
        )
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
        .children(super::library_people_view::people_rows(ws, cx))
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

/// One filtered snapshot for both gallery views.
fn gallery_sections(ws: &Workspace) -> Vec<(String, String, Vec<super::library::Entry>)> {
    let hide_flagged = ws.view.gallery_hide_nsfw;
    // Owned snapshot: the cells below borrow the workspace mutably to
    // fetch thumbnails, so they cannot also iterate `sections` in place.
    let mut sections: Vec<(String, String, Vec<super::library::Entry>)> =
        if let Some(results) = &ws.library.search_results {
            // A search flattens the groups into one ranked strip. Made
            // while viewing a bucket, it was ranked within the bucket;
            // the strip also keeps to the bucket's current contents,
            // so a photo removed by hand leaves at once.
            let by_path: FxHashMap<&PathBuf, &super::library::Entry> = ws
                .library
                .sections
                .iter()
                .flat_map(|s| s.entries.iter())
                .map(|e| (&e.path, e))
                .collect();
            let bucket = ws
                .library
                .bucket_filter
                .and_then(|i| ws.library.buckets.get(i));
            let scope: Option<FxHashSet<&PathBuf>> =
                bucket.map(|b| b.photos.iter().chain(b.matches.iter()).collect());
            let entries: Vec<super::library::Entry> = results
                .iter()
                .filter(|(path, _)| scope.as_ref().is_none_or(|s| s.contains(path)))
                .filter_map(|(path, _)| by_path.get(path).map(|e| (*e).clone()))
                .filter(|e| ws.library.passes_map(&e.path))
                .collect();
            let mut title = match bucket {
                Some(bucket) => format!("Bucket · {} · Search results", bucket.name),
                None => "Search results".to_string(),
            };
            if let Some(place) = &ws.library.search_place {
                title.push_str(&format!(" · near {place}"));
            }
            if !ws.library.search_people.is_empty() {
                title.push_str(&format!(" · with {}", ws.library.search_people.join(", ")));
            }
            vec![(title, String::new(), entries)]
        } else {
            ws.library.grouped()
        };
    // The content filter applies whatever the grouping.
    for (_, _, entries) in &mut sections {
        entries.retain(|e| !(hide_flagged && ws.library.is_flagged(&e.path)));
    }
    sections.retain(|(_, _, entries)| !entries.is_empty());
    sections
}

/// Browse the current gallery on a world map; the strip opens every photo
/// in a cluster, including photos with identical coordinates at maximum zoom.
fn world_map(ws: &mut Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    use super::library_geo::MapSlot;
    let entries: Vec<_> = gallery_sections(ws)
        .into_iter()
        .flat_map(|(_, _, entries)| entries)
        .collect();
    let located = entries
        .iter()
        .filter(|e| photo_position(ws, &e.path).is_some())
        .count();
    let pending = entries
        .iter()
        .filter(|e| !ws.library.positions.contains_key(&e.path))
        .count();
    let unlocated = entries.len() - located - pending;
    let selected_paths: FxHashSet<_> = ws.library.map_photos.iter().collect();
    let details: Vec<_> = entries
        .into_iter()
        .filter(|e| selected_paths.contains(&e.path) && photo_position(ws, &e.path).is_some())
        .collect();
    let mut status = format!("{located} photos on map · {unlocated} without location");
    if pending > 0 {
        status.push_str(&format!(" · Reading locations: {pending} remaining"));
    }
    let mut view = div().flex().flex_col().flex_grow().min_w(px(0.0)).min_h(px(0.0))
        .child(div().flex().items_center().gap_2().p_2().flex_wrap()
            .child(div().text_size(px(12.0)).child("World Map"))
            .child(map_tool_button("−", false, |ws, cx| {
                ws.library.world_map.zoom_center(-1); cx.notify();
            }, cx))
            .child(map_tool_button("+", false, |ws, cx| {
                ws.library.world_map.zoom_center(1); cx.notify();
            }, cx))
            .child(map_tool_button("Reset view", false, |ws, cx| {
                ws.library.world_map.show_world(); cx.notify();
            }, cx))
            .child(div().text_size(px(11.0)).text_color(gpui::rgb(pal().text_dim)).child(status)))
        .child(div().px_2().pb_2().text_size(px(11.0)).text_color(gpui::rgb(pal().text_dim))
            .child(if located == 0 && pending == 0 {
                "No photos with GPS locations in this view. Add geotagged photos or change the gallery filters."
            } else {
                "Drag to pan · Scroll to zoom · Click a marker to see its photos · Double-click a photo to edit"
            }))
        .child(div().relative().flex_grow().min_h(px(0.0)).overflow_hidden()
            .child(div().absolute().size_full().child(map_element(ws, MapSlot::World, 0.0, cx))));
    if !details.is_empty() {
        let count = details.len();
        let mut strip = div()
            .id("map-photo-strip")
            .flex()
            .gap_2()
            .p_2()
            .overflow_x_scroll();
        for entry in details {
            let selected = ws.library.is_selected(&entry.path);
            let path = entry.path.clone();
            let preview = photo_preview(ws, &entry, 86.0, 62.0, cx);
            strip = strip.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .w(px(98.0))
                    .flex_none()
                    .p_1()
                    .rounded_sm()
                    .border_2()
                    .border_color(gpui::rgb(if selected {
                        pal().select_border
                    } else {
                        pal().cell_edge
                    }))
                    .cursor_pointer()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |ws, ev: &MouseDownEvent, _, cx| {
                            ws.library.select_single(path.clone());
                            if ev.click_count >= 2 {
                                ws.open_from_gallery(path.clone(), cx);
                            }
                            cx.notify();
                        }),
                    )
                    .child(preview)
                    .child(
                        div().text_size(px(10.0)).truncate().child(
                            entry
                                .path
                                .file_name()
                                .map(|s| s.to_string_lossy().into_owned())
                                .unwrap_or_default(),
                        ),
                    ),
            );
        }
        view = view
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_2()
                    .pt_2()
                    .child(
                        div()
                            .text_size(px(11.0))
                            .child(format!("{count} photos at this marker")),
                    )
                    .child(map_tool_button(
                        "Close",
                        false,
                        |ws, cx| {
                            ws.library.map_photos.clear();
                            cx.notify();
                        },
                        cx,
                    )),
            )
            .child(strip);
    }
    view
}

fn photo_position(ws: &Workspace, path: &PathBuf) -> Option<(f64, f64)> {
    let (lat, lon) = ws.library.positions.get(path).copied().flatten()?;
    super::library_geo::valid_position(lat, lon).then_some((lat, lon))
}

/// Only painted previews claim decoded pixels, just like grid cells.
fn photo_preview(
    ws: &Workspace,
    entry: &super::library::Entry,
    width: f32,
    height: f32,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let thumb = ws.library.thumb(entry);
    let entry = entry.clone();
    let entity = cx.entity();
    div()
        .relative()
        .w(px(width))
        .h(px(height))
        .flex_none()
        .overflow_hidden()
        .bg(gpui::rgb(pal().chrome_bg))
        .child(
            canvas(
                move |bounds, window, cx| {
                    if !bounds.intersects(&window.content_mask().bounds) {
                        return;
                    }
                    entity.update(cx, |ws, cx| {
                        if ws.library.note_visible(&entry) {
                            ws.kick_thumb_loader(cx);
                        }
                    });
                },
                |_, _, _, _| {},
            )
            .absolute()
            .size_full(),
        )
        .child(match thumb {
            Some(image) => img(image)
                .size_full()
                .object_fit(gpui::ObjectFit::Contain)
                .into_any_element(),
            None => div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(10.0))
                .text_color(gpui::rgb(pal().text_dim))
                .child("Photo")
                .into_any_element(),
        })
}

/// Build marker elements during prepaint so their hitboxes and thumbnails
/// use this frame's projection, including during a drag or a resize.
fn prepare_photo_markers(
    ws: &mut Workspace,
    bounds: Bounds<Pixels>,
    cx: &mut Context<Workspace>,
) -> Vec<(Point<Pixels>, gpui::AnyElement)> {
    let entries: Vec<_> = gallery_sections(ws)
        .into_iter()
        .flat_map(|(_, _, entries)| entries)
        .filter_map(|e| photo_position(ws, &e.path).map(|pos| (e, pos)))
        .collect();
    let points: Vec<_> = entries
        .iter()
        .enumerate()
        .flat_map(|(i, (_, (lat, lon)))| {
            ws.library
                .world_map
                .marker_copies(*lat, *lon)
                .into_iter()
                .filter_map(move |(x, y)| {
                    let rect = Bounds {
                        origin: point(px(x - 34.0), px(y - 64.0)),
                        size: size(px(68.0), px(70.0)),
                    };
                    rect.intersects(&bounds).then_some((i, x, y))
                })
        })
        .collect();
    let clusters = super::library_geo::cluster_markers(&points);
    let mut markers = Vec::new();
    for (cluster_index, cluster) in clusters.into_iter().enumerate() {
        let (i, x, y) = cluster[0];
        let entry = &entries[i].0;
        let mut seen = FxHashSet::default();
        let paths: Vec<_> = cluster
            .iter()
            .map(|(i, _, _)| entries[*i].0.path.clone())
            .filter(|path| seen.insert(path.clone()))
            .collect();
        let context_paths = paths.clone();
        let count = paths.len();
        let active = paths.iter().any(|p| ws.library.is_selected(p));
        let preview = photo_preview(ws, entry, 56.0, 42.0, cx);
        let label = if count > 1 {
            format!("{count} photos")
        } else {
            entry
                .path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default()
        };
        let marker = div()
            .id(SharedString::from(format!(
                "map-pin-{}-{cluster_index}",
                entry.path.display()
            )))
            .flex()
            .flex_col()
            .items_center()
            .w(px(68.0))
            .cursor_pointer()
            .tooltip(crate::ui::tip(label, None))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |ws, ev: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    ws.library.world_map.end_drag();
                    ws.library.map_photos = paths.clone();
                    ws.library.select_single(paths[0].clone());
                    if count == 1 && ev.click_count >= 2 {
                        ws.open_from_gallery(paths[0].clone(), cx);
                    }
                    cx.notify();
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |ws, ev: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    ws.library.world_map.end_drag();
                    ws.library.context = Some((
                        ev.position,
                        super::library::GalleryContext::MapCluster(context_paths.clone()),
                    ));
                    cx.notify();
                }),
            )
            .child(
                div()
                    .relative()
                    .p_1()
                    .rounded_md()
                    .border_2()
                    .border_color(gpui::rgb(if active {
                        pal().select_border
                    } else {
                        0xFFFFFF
                    }))
                    .bg(gpui::rgb(pal().chrome_bg))
                    .child(preview)
                    .children((count > 1).then(|| {
                        div()
                            .absolute()
                            .right(px(0.0))
                            .bottom(px(0.0))
                            .px_1()
                            .rounded_sm()
                            .bg(gpui::rgb(pal().select_border))
                            .text_color(gpui::rgb(0xFFFFFF))
                            .text_size(px(10.0))
                            .child(count.to_string())
                    })),
            )
            .child(div().w(px(3.0)).h(px(5.0)).bg(gpui::rgb(0xFFFFFF)))
            .child(
                div()
                    .w(px(10.0))
                    .h(px(10.0))
                    .rounded_full()
                    .border_2()
                    .border_color(gpui::rgb(0xFFFFFF))
                    .bg(gpui::rgb(pal().select_border)),
            )
            .into_any_element();
        markers.push((point(px(x - 34.0), px(y - 64.0)), marker));
    }
    markers
}

/// The grid: folder headers with a rule, then wrapped thumbnails.
fn grid(ws: &mut Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let cell = ws.library.thumb_px;
    let selected: Vec<PathBuf> = ws.library.selected.clone();
    let sections = gallery_sections(ws);
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
        // work out columns per row, the reveal logic can keep the
        // selection on screen, and the cells' visibility probes know
        // what "on screen" means. The canvas sits inside the scrolled
        // content, so its bounds scroll along with it — subtract the
        // scroll offset to get back to window coordinates, the space
        // every cell's own bounds are reported in.
        .child(
            canvas(
                {
                    let grid_entity = grid_entity.clone();
                    move |bounds, _window, cx| {
                        grid_entity.update(cx, |ws, _| {
                            let offset = ws.library.grid_scroll.offset();
                            ws.library.grid_bounds = gpui::Bounds {
                                origin: bounds.origin - offset,
                                size: bounds.size,
                            };
                        });
                    }
                },
                move |_, _, window, _| {
                    // ⌘-wheel (Ctrl elsewhere) over the grid resizes the
                    // thumbnails, as ⌘-wheel zooms a canvas. It has to
                    // win over the container's own scrolling, which runs
                    // in the bubble phase — so take it in capture and
                    // stop it there.
                    let grid_entity = grid_entity.clone();
                    window.on_mouse_event(move |ev: &gpui::ScrollWheelEvent, phase, _w, cx| {
                        if phase != gpui::DispatchPhase::Capture
                            || !(ev.modifiers.platform || ev.modifiers.control)
                        {
                            return;
                        }
                        let dy = wheel_pixels(ev);
                        let took = grid_entity.update(cx, |ws, cx| {
                            if !ws.library.grid_bounds.contains(&ev.position) {
                                return false;
                            }
                            ws.nudge_gallery_thumb_px(dy);
                            cx.notify();
                            true
                        });
                        if took {
                            cx.stop_propagation();
                        }
                    });
                },
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
                        Some(_) if ws.library.search_results.is_some() => {
                            "Nothing in this bucket matches the search. Escape clears it \
                         to show the whole bucket."
                        }
                        None if ws.library.search_results.is_some() => {
                            "Nothing matches the search. Escape clears it."
                        }
                        Some(bucket) if bucket.is_smart() => {
                            "Nothing matches this bucket's rule yet — matches appear as \
                         photos are indexed. Dragging photos in works too."
                        }
                        Some(_) => {
                            "This bucket is empty. Drag photos onto its row in the sidebar \
                         to add them."
                        }
                        None if ws.library.person_filter.is_some() => {
                            "Nothing here yet. Faces appear as photos are indexed; \
                         click a photo, then a face, to say who it is."
                        }
                        None if scanning => "Scanning folders\u{2026}",
                        None => {
                            "No photos found in the watched folders. Images Schist can open \
                         (PNG, JPEG, WebP, TIFF, HEIC, camera raws, PSD, Affinity) \
                         appear here; \
                         sub-folders are scanned six levels deep."
                        }
                    },
                ),
        );
    }
    // Virtualisation: only rows near the viewport build real cells —
    // a cell is ~20 elements with listeners, and a big library built
    // every one of them each frame, which is what "a bit laggy" was.
    // Rows are exact (uniform cells); header heights are an estimate,
    // and the viewport of margin either side absorbs the drift. The
    // lead-selected row always builds, so its bounds keep feeding the
    // keep-on-screen scroll even when it is far away.
    const HEADER_ESTIMATE: f32 = 41.0;
    let lead = ws.library.lead_selected().cloned();
    let columns = {
        let width = f32::from(ws.library.grid_bounds.size.width);
        // p_2 padding both sides, gap_2 between cells — the keyboard
        // navigation's formula, so rows agree with up/down arrows.
        (((width - 16.0 + 8.0) / (cell + 8.0)).floor() as usize).max(1)
    };
    let view_h = f32::from(ws.library.grid_bounds.size.height);
    // The first frame has no recorded viewport yet: build everything
    // once, and virtualise from the second frame on.
    let (win_top, win_bottom) = if view_h > 0.0 {
        let scroll_y = -f32::from(ws.library.grid_scroll.offset().y);
        (scroll_y - view_h, scroll_y + 2.0 * view_h)
    } else {
        (f32::MIN, f32::MAX)
    };
    let mut content_y = 8.0; // the column's p_2 top padding
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
        content_y += HEADER_ESTIMATE;
        let mut body = div().flex().flex_col();
        // Consecutive off-screen rows collapse into one spacer, so a
        // scrolled-away month costs a single empty div.
        let mut hidden = 0.0f32;
        for row_entries in entries.chunks(columns) {
            let row_top = content_y;
            content_y += cell + 8.0;
            let near = row_top <= win_bottom && row_top + cell >= win_top;
            let holds_lead = lead
                .as_ref()
                .is_some_and(|l| row_entries.iter().any(|e| &e.path == l));
            if !near && !holds_lead {
                hidden += cell + 8.0;
                continue;
            }
            if hidden > 0.0 {
                body = body.child(div().h(px(hidden)));
                hidden = 0.0;
            }
            let mut row = div().flex().flex_row().gap_2().mb_2();
            for entry in row_entries {
                row = row.child(cell_element(ws, entry.clone(), cell, &selected, cx));
            }
            body = body.child(row);
        }
        if hidden > 0.0 {
            body = body.child(div().h(px(hidden)));
        }
        column = column.child(body);
    }
    // The scrollbar gpui doesn't paint: a track along the viewport's
    // right edge, exact because the thumb reads the scroll handle's
    // own extents. Clicking the track jumps there; dragging is
    // handled by the wrapper below, so the pointer may wander off the
    // twelve-pixel strip mid-drag without dropping the thumb.
    let scrollbar = ws
        .library
        .scrollbar_geometry()
        .map(|(inset, thumb_h, travel, max_y)| {
            let scroll_y = (-f32::from(ws.library.grid_scroll.offset().y)).clamp(0.0, max_y);
            let thumb_top = inset + scroll_y / max_y * travel;
            div()
                .absolute()
                .top_0()
                .right_0()
                .bottom_0()
                .w(px(12.0))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |ws, ev: &MouseDownEvent, _w, cx| {
                        let y =
                            f32::from(ev.position.y) - f32::from(ws.library.grid_bounds.origin.y);
                        ws.library.scrollbar_grab =
                            Some(if (thumb_top..thumb_top + thumb_h).contains(&y) {
                                // Grabbed the thumb: keep the grip point.
                                y - thumb_top
                            } else {
                                // Clicked the track: the thumb jumps
                                // there, held by its middle.
                                thumb_h / 2.0
                            });
                        ws.library.scrollbar_drag_to(f32::from(ev.position.y));
                        cx.stop_propagation();
                        cx.notify();
                    }),
                )
                .child(
                    div()
                        .absolute()
                        .top(px(thumb_top))
                        .right(px(2.0))
                        .w(px(8.0))
                        .h(px(thumb_h))
                        .rounded_md()
                        .bg(gpui::rgb(pal().cell_edge))
                        .hover(|s| s.bg(gpui::rgb(pal().cell_hover))),
                )
        });
    div()
        .relative()
        .flex()
        .flex_col()
        .flex_grow()
        .min_h(px(0.0))
        // Shrinkable: a flex item's minimum width is its content's, and
        // a row of cells is a fixed width, so without this the grid
        // refuses to give up room to the AI panel beside it and pushes
        // it off the right edge. The column count follows the width
        // the next frame.
        .min_w(px(0.0))
        .overflow_hidden()
        .on_mouse_move(cx.listener(|ws, ev: &gpui::MouseMoveEvent, _w, cx| {
            if ws.library.scrollbar_grab.is_none() {
                return;
            }
            if ev.pressed_button == Some(MouseButton::Left) {
                ws.library.scrollbar_drag_to(f32::from(ev.position.y));
                cx.notify();
            } else {
                // The button went up somewhere we never heard about.
                ws.library.scrollbar_grab = None;
            }
        }))
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(|ws, _ev: &gpui::MouseUpEvent, _w, _cx| {
                ws.library.scrollbar_grab = None;
            }),
        )
        .child(column)
        .children(scrollbar)
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
    // The drag ghost shows the square being carried, so it wants the
    // same picture the cell shows.
    let ghost_thumb = thumb.clone();
    let drag_entity = cx.entity();
    let probe_entity = cx.entity();
    let probe_entry = entry.clone();
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
        // The visibility probe: at paint time it knows the cell's real
        // rectangle, and a cell within a viewport's height of the
        // screen is what queues its decode and stamps its thumbnail in
        // use. Building the element (this function) must stay free —
        // the whole grid builds every frame.
        .child(
            canvas(
                move |bounds, _window, cx| {
                    probe_entity.update(cx, |ws, cx| {
                        // One viewport of margin either side, so
                        // scrolling meets thumbnails, not placeholders.
                        let mut near = ws.library.grid_bounds;
                        near.origin.y -= near.size.height;
                        near.size.height *= 3.0;
                        if near.intersects(&bounds) && ws.library.note_visible(&probe_entry) {
                            ws.kick_thumb_loader(cx);
                        }
                    });
                },
                |_, _, _, _| {},
            )
            .absolute()
            .size_full(),
        )
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
                // What the drag carries, in case it leaves the window
                // and the platform's own drag-and-drop takes it on.
                let carried = drag.paths.clone();
                drag_entity.update(cx, |ws, _| ws.library.dragging = Some(carried));
                let label = if drag.paths.len() == 1 {
                    drag_path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "1 photo".into())
                } else {
                    format!("{} photos", drag.paths.len())
                };
                let thumb = ghost_thumb.clone();
                let count = drag.paths.len();
                cx.new(|_| DragGhost {
                    label,
                    thumb,
                    count,
                    size: cell,
                })
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

/// The ghost that rides the pointer during a drag: the picked-up
/// photo's whole square when its thumbnail is in memory (with a count
/// badge for a multi-drag), the old name pill only when it is not.
struct DragGhost {
    label: String,
    thumb: Option<Arc<gpui::RenderImage>>,
    count: usize,
    size: f32,
}

impl gpui::Render for DragGhost {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let Some(thumb) = self.thumb.clone() else {
            return div()
                .px_2()
                .py_1()
                .rounded_md()
                .bg(gpui::rgb(pal().select_border))
                .text_color(gpui::rgb(0xFFFFFF))
                .text_size(px(11.0))
                .child(SharedString::from(self.label.clone()))
                .into_any_element();
        };
        let inner = self.size - 10.0;
        div()
            .w(px(self.size))
            .h(px(self.size))
            .relative()
            .flex()
            .items_center()
            .justify_center()
            .rounded_sm()
            .bg(gpui::rgb(pal().grid_bg))
            .border_2()
            .border_color(gpui::rgb(pal().select_border))
            .opacity(0.85)
            .child(img(thumb).max_w(px(inner)).max_h(px(inner)))
            .children((self.count > 1).then(|| {
                div()
                    .absolute()
                    .top(px(-6.0))
                    .right(px(-6.0))
                    .min_w(px(18.0))
                    .h(px(18.0))
                    .px_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_full()
                    .bg(gpui::rgb(pal().select_border))
                    .text_color(gpui::rgb(0xFFFFFF))
                    .text_size(px(10.0))
                    .child(format!("{}", self.count))
            }))
            .into_any_element()
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
        .children(selected.as_ref().map(|entry| {
            // The quick look — the photo big, its faces to name.
            let view = entry.path.clone();
            gallery_button(
                "View",
                false,
                move |ws, _w, cx| ws.open_viewer(view.clone(), cx),
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
        .child(map_element(
            ws,
            super::library_geo::MapSlot::Gallery,
            300.0,
            cx,
        ))
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
/// The map, for whichever slot: tiles, panning, zoom, the drawn
/// boundary, and the markers. Shared by the gallery's import and map
/// filter and the editor's info panel.
/// A wheel event's vertical travel in pixels: a mouse wheel reports
/// lines, a trackpad pixels, and both should mean the same thing.
fn wheel_pixels(ev: &gpui::ScrollWheelEvent) -> f32 {
    match ev.delta {
        gpui::ScrollDelta::Pixels(p) => f32::from(p.y),
        gpui::ScrollDelta::Lines(l) => l.y * 40.0,
    }
}

pub(crate) fn map_element(
    ws: &mut Workspace,
    slot: super::library_geo::MapSlot,
    height: f32,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let entity = cx.entity();
    let draw_mode = ws.map_mut(slot).draw_mode;
    div()
        .id(match slot {
            super::library_geo::MapSlot::Gallery => "gallery-map",
            super::library_geo::MapSlot::World => "world-map",
            super::library_geo::MapSlot::Info => "info-map",
        })
        .relative()
        .w_full()
        .when(slot == super::library_geo::MapSlot::World, |el| el.h_full())
        .when(slot != super::library_geo::MapSlot::World, |el| {
            el.h(px(height))
        })
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
                let map = ws.map_mut(slot);
                let drawing = slot != super::library_geo::MapSlot::World
                    && (ev.modifiers.shift || map.draw_mode);
                map.begin_drag(pos, drawing);
                cx.notify();
            }),
        )
        .on_mouse_move(cx.listener(move |ws, ev: &MouseMoveEvent, _w, cx| {
            if ev.pressed_button == Some(MouseButton::Left) {
                let pos = (f32::from(ev.position.x), f32::from(ev.position.y));
                if ws.map_mut(slot).drag_to(pos) {
                    cx.notify();
                }
            }
        }))
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(move |ws, _ev: &MouseUpEvent, _w, cx| {
                ws.map_mut(slot).end_drag();
                cx.notify();
            }),
        )
        .on_scroll_wheel(cx.listener(move |ws, ev: &gpui::ScrollWheelEvent, _w, cx| {
            let dy = wheel_pixels(ev);
            let pos = (f32::from(ev.position.x), f32::from(ev.position.y));
            if ws.map_mut(slot).wheel(dy, pos) {
                cx.notify();
            }
            // The wheel over a map is zoom, as on every web map — not
            // a scroll of whatever panel the map sits in.
            cx.stop_propagation();
        }))
        .child(
            canvas(
                move |bounds, window, cx| {
                    let scale = window.scale_factor();
                    let (paint, markers) = entity.update(cx, |ws, cx| {
                        let paint = ws.prepare_map_paint(slot, bounds, scale);
                        // Whatever this frame queued starts fetching.
                        ws.kick_map_tiles(slot, cx);
                        let markers = if slot == super::library_geo::MapSlot::World {
                            prepare_photo_markers(ws, bounds, cx)
                        } else {
                            Vec::new()
                        };
                        (paint, markers)
                    });
                    let markers = markers
                        .into_iter()
                        .map(|(origin, mut marker)| {
                            marker.prepaint_as_root(
                                origin,
                                size(
                                    gpui::AvailableSpace::MinContent,
                                    gpui::AvailableSpace::MinContent,
                                ),
                                window,
                                cx,
                            );
                            marker
                        })
                        .collect();
                    (paint, markers)
                },
                move |bounds,
                      (paint, mut markers): (
                    super::library_geo::MapPaint,
                    Vec<gpui::AnyElement>,
                ),
                      window,
                      cx| {
                    if slot == super::library_geo::MapSlot::World {
                        window.paint_quad(gpui::fill(bounds, gpui::rgb(0xF2EFE9)));
                    }
                    // Sea-grey where a tile has not arrived, so loading
                    // reads as loading rather than as a hole.
                    for rect in paint.missing {
                        window.paint_quad(gpui::fill(rect, gpui::rgb(0xC9D4DC)));
                    }
                    // Each tile image carries a one-pixel gutter: draw it
                    // that much larger and clip to the true tile, so the
                    // bilinear filter never reaches past the tile's edge.
                    for (rect, img) in paint.tiles {
                        let gutter_x =
                            rect.size.width * (super::library_geo::TILE_GUTTER as f32 / 256.0);
                        let gutter_y =
                            rect.size.height * (super::library_geo::TILE_GUTTER as f32 / 256.0);
                        let padded = gpui::Bounds {
                            origin: gpui::point(rect.origin.x - gutter_x, rect.origin.y - gutter_y),
                            size: gpui::size(
                                rect.size.width + gutter_x * 2.0,
                                rect.size.height + gutter_y * 2.0,
                            ),
                        };
                        window.with_content_mask(
                            Some(gpui::ContentMask { bounds: rect }),
                            |window| {
                                let _ = window.paint_image(
                                    padded,
                                    gpui::Corners::default(),
                                    img,
                                    0,
                                    false,
                                );
                            },
                        );
                    }
                    if let Some(sel) = paint.selection {
                        window.paint_quad(gpui::fill(sel, gpui::rgba(0x4A90D930)));
                        window.paint_quad(gpui::outline(
                            sel,
                            gpui::rgb(0x2A66B0),
                            gpui::BorderStyle::Solid,
                        ));
                    }
                    // The blip: a white-ringed red dot, the way every
                    // map marks "you are here".
                    for at in paint.markers {
                        let dot = |r: f32, color: gpui::Rgba| {
                            let mut quad = gpui::fill(
                                gpui::Bounds {
                                    origin: gpui::point(at.x - px(r), at.y - px(r)),
                                    size: gpui::size(px(r * 2.0), px(r * 2.0)),
                                },
                                color,
                            );
                            quad.corner_radii = gpui::Corners::all(px(r));
                            quad
                        };
                        window.paint_quad(dot(9.0, gpui::rgba(0xFFFFFFE0)));
                        window.paint_quad(dot(6.5, gpui::rgb(0xE0362B)));
                    }
                    for marker in &mut markers {
                        marker.paint(window, cx);
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

/// The two models photo search runs on, offered together: neither is
/// any use without the other, so the gallery asks once.
pub(crate) const SEARCH_MODELS: [&str; 2] = ["embed-image", "embed-text"];

/// Whether either Search model is being fetched right now.
fn search_models_downloading(ws: &Workspace) -> bool {
    ws.model_downloads
        .iter()
        .any(|d| SEARCH_MODELS.contains(&d.id))
}

fn search_model_link(
    id: &'static str,
    label: &'static str,
    url: &'static str,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    div()
        .id(id)
        .cursor_pointer()
        .text_color(gpui::rgb(crate::ui::palette().accent))
        .hover(|style| style.text_color(gpui::rgb(crate::ui::palette().accent_hover)))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |_ws, _event, _window, cx| cx.open_url(url)),
        )
        .child(label)
}

/// The licences behind photo search, and the button that accepts them.
/// One dialog for both models: they are downloaded as a pair.
pub(crate) fn search_models_dialog(cx: &mut Context<Workspace>) -> impl IntoElement {
    let specs: Vec<&'static schist_neural::ModelSpec> = SEARCH_MODELS
        .iter()
        .filter_map(|id| schist_neural::spec(id))
        .collect();
    let total: usize = specs.iter().map(|s| s.bytes).sum();
    let mut body = div().flex().flex_col().gap_2().w(px(460.0)).child(
        div()
            .text_size(px(12.0))
            .text_color(gpui::rgb(crate::ui::palette().text))
            .child(
                "Searching photos by what is in them needs two models, \
                     downloaded once and kept on this machine. They run \
                     locally: no photo ever leaves it.",
            ),
    );
    for spec in &specs {
        body = body.child(
            div()
                .flex()
                .flex_col()
                .child(div().text_size(px(12.0)).child(SharedString::from(format!(
                    "{} \u{b7} {:.0} MB",
                    spec.name,
                    spec.bytes as f64 / (1 << 20) as f64
                )))),
        );
    }
    body = body.child(
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .text_size(px(11.0))
            .text_color(gpui::rgb(crate::ui::palette().text_dim))
            .child(search_model_link(
                "mobileclip-source",
                "MobileCLIP by Apple",
                "https://github.com/apple/ml-mobileclip",
                cx,
            ))
            .child("\u{b7}")
            .child(search_model_link(
                "mobileclip-export",
                "ONNX export by Xenova",
                "https://huggingface.co/Xenova/mobileclip_s0",
                cx,
            ))
            .child("\u{b7}")
            .child(search_model_link(
                "mobileclip-license",
                "License",
                "https://github.com/apple/ml-mobileclip/blob/main/LICENSE",
                cx,
            )),
    );
    body = body.child(
        div()
            .pt_1()
            .text_size(px(11.0))
            .text_color(gpui::rgb(crate::ui::palette().text_dim))
            .child(SharedString::from(format!(
                "Downloading installs both ({:.0} MB in all) and accepts their \
                 licences. They can be removed again under Gallery \u{25b8} \
                 Manage Models\u{2026}",
                total as f64 / (1 << 20) as f64
            ))),
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
            "Agree and Download",
            true,
            |ws, _w, cx| {
                for id in SEARCH_MODELS {
                    if !schist_neural::installed(id) {
                        ws.download_model(id, cx);
                    }
                }
                ws.close_modal(cx);
            },
            cx,
        ));
    crate::ui::modal_frame("Photo Search", 500.0, body, actions)
}

/// One text field of the bucket dialog: the layer-name pattern, with a
/// dimmed placeholder while nothing is typed.
pub(super) fn bucket_field(
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
    // It blinks, and the arrow keys move it.
    let caret_and_text = div()
        .text_color(gpui::rgb(crate::ui::palette().text))
        .child(if focused {
            let (before, after) = if ws.field_buffer.is_empty() {
                (typed.clone(), String::new())
            } else {
                let at = ws.field_cursor.min(typed.len());
                (typed[..at].to_string(), typed[at..].to_string())
            };
            crate::ui::caret_run(before, after, ws.caret_on(), crate::ui::palette().text)
                .into_any_element()
        } else {
            div().child(typed.clone()).into_any_element()
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
/// selection, on a map marker it acts on that marker's captured members,
/// and on a bucket it acts on the bucket.
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
        GalleryContext::MapCluster(photos) => {
            rows.push(
                div()
                    .px_2()
                    .py_1()
                    .text_size(px(11.0))
                    .text_color(gpui::rgb(crate::ui::palette().text_dim))
                    .child(format!("{} photos in this marker", photos.len()))
                    .into_any_element(),
            );
            for (i, bucket) in ws.library.buckets.iter().enumerate() {
                let add = photos.clone();
                row(
                    format!("Add all to {}", bucket.name),
                    &mut rows,
                    cx,
                    std::rc::Rc::new(move |ws, _w, _cx| {
                        ws.library.add_to_bucket(i, &add);
                    }),
                );
            }
            row(
                "Add all to new bucket…".into(),
                &mut rows,
                cx,
                std::rc::Rc::new(move |ws, _w, cx| {
                    // This bucket starts with the clicked photos. An old
                    // import/filter boundary must not add a smart rule.
                    ws.library.map.clear_selection();
                    ws.gallery_new_bucket(photos.clone(), cx);
                }),
            );
        }
        GalleryContext::Photo(path) => {
            // The right-click handler put this photo in the selection,
            // so the selection is what every action takes.
            let acting = ws.library.selected.clone();
            let n = acting.len();

            if n > 1 {
                let open = acting.clone();
                let opening = n.min(super::DROP_OPEN_CAP);
                row(
                    format!("Edit {opening} in tabs"),
                    &mut rows,
                    cx,
                    std::rc::Rc::new(move |ws, _w, cx| {
                        ws.open_photos_in_tabs(open.clone(), cx);
                    }),
                );
            } else {
                let open = path.clone();
                row(
                    "Edit".into(),
                    &mut rows,
                    cx,
                    std::rc::Rc::new(move |ws, _w, cx| {
                        ws.open_from_gallery(open.clone(), cx);
                    }),
                );
                let view = path.clone();
                row(
                    "View & name people".into(),
                    &mut rows,
                    cx,
                    std::rc::Rc::new(move |ws, _w, cx| {
                        ws.open_viewer(view.clone(), cx);
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
            // With the content filter on, flagged photos stay out of the
            // archive; the row counts what would actually go.
            let (zip, _held) = ws
                .library
                .zip_candidates(acting.clone(), ws.view.gallery_hide_nsfw);
            if n > 1 && zip.is_empty() {
                // Every one of them is held back: no archive to offer.
            } else if n > 1 {
                // Several photos leave as an archive.
                row(
                    format!("Save {} as ZIP…", zip.len()),
                    &mut rows,
                    cx,
                    std::rc::Rc::new(move |ws, window, cx| {
                        ws.save_photos_zip(zip.clone(), "photos.zip".into(), window, cx);
                    }),
                );
            } else {
                // One photo leaves as an image: its edit, in a chosen
                // format, at a chosen size.
                let one = path.clone();
                row(
                    "Save image as…".into(),
                    &mut rows,
                    cx,
                    std::rc::Rc::new(move |ws, _w, cx| {
                        ws.open_save_image_as(one.clone(), cx);
                    }),
                );
            }
            {
                // Turn, upscale, colour — one recipe over the lot.
                let batch = acting.clone();
                let label = if n > 1 {
                    format!("Process {n} photos\u{2026}")
                } else {
                    "Process\u{2026}".to_string()
                };
                row(
                    label,
                    &mut rows,
                    cx,
                    std::rc::Rc::new(move |ws, _w, cx| {
                        ws.open_batch_process(batch.clone(), cx);
                    }),
                );
            }
            sep(&mut rows);
            {
                let moving = acting.clone();
                let label = if n > 1 {
                    format!("Move {n} to folder\u{2026}")
                } else {
                    "Move to folder\u{2026}".to_string()
                };
                row(
                    label,
                    &mut rows,
                    cx,
                    std::rc::Rc::new(move |ws, window, cx| {
                        ws.move_photos_prompt(moving.clone(), window, cx);
                    }),
                );
            }
            // Only an edited photo has an original to go back to.
            let edited = acting
                .iter()
                .filter(|p| super::library::backing_psd(p).is_some_and(|s| s.exists()))
                .count();
            if edited > 0 {
                let revert = acting;
                let label = if edited > 1 {
                    format!("Revert {edited} to originals")
                } else {
                    "Revert to original".to_string()
                };
                row(
                    label,
                    &mut rows,
                    cx,
                    std::rc::Rc::new(move |ws, _w, cx| {
                        ws.revert_photos(revert.clone(), cx);
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
            if !photos.is_empty() {
                // Into the grid's selection, where the keyboard and
                // the photo menu can take it from here.
                let select = photos.clone();
                row(
                    format!("Select all ({})", photos.len()),
                    &mut rows,
                    cx,
                    std::rc::Rc::new(move |ws, _w, _cx| {
                        ws.library.bucket_filter = Some(index);
                        ws.library.folder_filter = None;
                        ws.library.selected = select.clone();
                    }),
                );
            }
            sep(&mut rows);
            let (zip, _held) = ws
                .library
                .zip_candidates(photos.clone(), ws.view.gallery_hide_nsfw);
            if !zip.is_empty() {
                let suggested = format!("{}.zip", name.to_lowercase().replace(' ', "-"));
                row(
                    format!("Save all as ZIP… ({})", zip.len()),
                    &mut rows,
                    cx,
                    std::rc::Rc::new(move |ws, window, cx| {
                        ws.save_photos_zip(zip.clone(), suggested.clone(), window, cx);
                    }),
                );
            }
            if !photos.is_empty() {
                let batch = photos.clone();
                row(
                    format!("Process all\u{2026} ({})", photos.len()),
                    &mut rows,
                    cx,
                    std::rc::Rc::new(move |ws, _w, cx| {
                        ws.open_batch_process(batch.clone(), cx);
                    }),
                );
                let moving = photos;
                row(
                    "Move all to folder\u{2026}".into(),
                    &mut rows,
                    cx,
                    std::rc::Rc::new(move |ws, window, cx| {
                        ws.move_photos_prompt(moving.clone(), window, cx);
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
        GalleryContext::Person(index) => {
            let (name, faces) = ws
                .library
                .people
                .get(index)
                .map(|p| (p.name.clone(), p.faces.len()))
                .unwrap_or_default();
            row(
                format!("Show {name}"),
                &mut rows,
                cx,
                std::rc::Rc::new(move |ws, _w, cx| {
                    ws.show_person(Some(super::library::PersonFilter::Person(index)), cx);
                }),
            );
            row(
                "Rename\u{2026}".into(),
                &mut rows,
                cx,
                std::rc::Rc::new(move |ws, _w, cx| {
                    ws.rename_person_prompt(index, cx);
                }),
            );
            sep(&mut rows);
            // The faces go back to unnamed; nothing is deleted.
            row(
                if faces == 1 {
                    "Forget person (1 face)".to_string()
                } else {
                    format!("Forget person ({faces} faces)")
                },
                &mut rows,
                cx,
                std::rc::Rc::new(move |ws, _w, _cx| {
                    ws.library.delete_person(index);
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
