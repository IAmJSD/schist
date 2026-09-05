//! The People album on screen: the sidebar's rows, the viewer with its
//! face boxes and people panel, and the two dialogs (the models to
//! install, a person's rename). Drawn on the gallery's palette, like
//! the rest of the room.

use super::library::{FaceView, GalleryContext, PersonFilter, AVATAR_PX};
use super::library_people::PEOPLE_MODELS;
use super::library_view::{bucket_field, gallery_button, pal};
use super::*;
use gpui::{img, StatefulInteractiveElement as _};
use schist_gallery::FaceRect;
use std::path::Path;

/// The colour of a face box by its state: named, picked, guessed, or
/// merely found.
fn box_color(face: &FaceView, picked: bool) -> u32 {
    if picked {
        pal().select_border
    } else if face.person.is_some() {
        pal().green
    } else if face.suggestion.is_some() {
        0xE0A030
    } else {
        0xFFFFFF
    }
}

/// A small section heading in the sidebar's style.
fn heading(text: &'static str) -> impl IntoElement {
    div()
        .px_2()
        .pt_2()
        .pb_1()
        .text_size(px(11.0))
        .text_color(gpui::rgb(pal().text_dim))
        .child(text)
}

/// A round avatar, or a grey disc while the cut is on its way.
fn avatar(image: Option<Arc<RenderImage>>, size: f32) -> gpui::AnyElement {
    let frame = div()
        .w(px(size))
        .h(px(size))
        .flex_none()
        .rounded_full()
        .overflow_hidden()
        .bg(gpui::rgb(pal().cell_edge));
    match image {
        Some(image) => frame
            .child(img(image).w(px(size)).h(px(size)))
            .into_any_element(),
        None => frame.into_any_element(),
    }
}

/// One sidebar row's worth of a person: index, name, photo count, and
/// the face the avatar is cut from.
type PersonRow = (usize, String, usize, Option<(PathBuf, FaceRect)>);

/// The sidebar's PEOPLE section: a row per person, the unnamed faces,
/// and — until the models are here — the way to get them.
pub(super) fn people_rows(
    ws: &mut Workspace,
    cx: &mut Context<Workspace>,
) -> Vec<gpui::AnyElement> {
    let mut rows: Vec<gpui::AnyElement> = vec![heading("PEOPLE").into_any_element()];
    let detector = schist_neural::installed("face");
    let recogniser = schist_neural::installed("face-embed");
    let people: Vec<PersonRow> = ws
        .library
        .people
        .iter()
        .enumerate()
        .map(|(i, p)| {
            (
                i,
                p.name.clone(),
                p.photos().len(),
                p.faces.first().map(|f| (f.photo.clone(), f.rect)),
            )
        })
        .collect();
    let viewing = ws.library.person_filter;
    let any_people = !people.is_empty();
    for (index, name, count, face) in people {
        let image = face.and_then(|(path, rect)| ws.face_avatar(&path, &rect, cx));
        let selected = viewing == Some(PersonFilter::Person(index));
        rows.push(
            div()
                .id(SharedString::from(format!("person-{index}")))
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .px_2()
                .h(px(26.0))
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
                        let next = if ws.library.person_filter == Some(PersonFilter::Person(index))
                        {
                            None
                        } else {
                            Some(PersonFilter::Person(index))
                        };
                        ws.show_person(next, cx);
                    }),
                )
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |ws, ev: &MouseDownEvent, _w, cx| {
                        ws.library.context = Some((ev.position, GalleryContext::Person(index)));
                        cx.notify();
                    }),
                )
                .child(avatar(image, 20.0))
                .child(div().flex_grow().truncate().child(SharedString::from(name)))
                .child(
                    div()
                        .text_size(px(10.0))
                        .text_color(gpui::rgb(pal().text_dim))
                        .child(format!("{count}")),
                )
                .into_any_element(),
        );
    }
    let unnamed = ws.library.unnamed_face_count();
    if unnamed > 0 {
        let selected = viewing == Some(PersonFilter::Unnamed);
        rows.push(
            div()
                .id("person-unnamed")
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .px_2()
                .h(px(26.0))
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
                    cx.listener(|ws, _e: &MouseDownEvent, _w, cx| {
                        let next = if ws.library.person_filter == Some(PersonFilter::Unnamed) {
                            None
                        } else {
                            Some(PersonFilter::Unnamed)
                        };
                        ws.show_person(next, cx);
                    }),
                )
                .child(
                    div()
                        .w(px(20.0))
                        .h(px(20.0))
                        .flex_none()
                        .rounded_full()
                        .border_1()
                        .border_color(gpui::rgb(pal().text_dim))
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(px(11.0))
                        .text_color(gpui::rgb(pal().text_dim))
                        .child("?"),
                )
                .child(div().flex_grow().truncate().child("Unnamed faces"))
                .child(
                    div()
                        .text_size(px(10.0))
                        .text_color(gpui::rgb(pal().text_dim))
                        .child(format!("{unnamed}")),
                )
                .into_any_element(),
        );
    }
    let link = |label: &'static str, cx: &mut Context<Workspace>| {
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
                cx.listener(|ws, _e: &MouseDownEvent, _w, cx| ws.open_people_models(cx)),
            )
            .child(label)
            .into_any_element()
    };
    if !detector {
        rows.push(link("+ Find faces\u{2026}", cx));
    } else if !recogniser {
        rows.push(link("+ Recognise faces\u{2026}", cx));
    } else if !any_people && unnamed == 0 {
        let (looked, total) = ws.library.faces_progress();
        rows.push(
            div()
                .px_2()
                .py_1()
                .text_size(px(11.0))
                .text_color(gpui::rgb(pal().text_dim))
                .child(if looked < total {
                    format!("Looking for faces\u{2026} {looked}/{total}")
                } else {
                    "Faces appear here as photos are indexed.".to_string()
                })
                .into_any_element(),
        );
    }
    rows
}

/// The viewer: one photo, big, its faces boxed, and the people panel
/// beside it. Replaces the grid while a photo is on show.
pub(super) fn viewer(ws: &mut Workspace, cx: &mut Context<Workspace>) -> gpui::AnyElement {
    let Some(v) = &ws.library.viewer else {
        return div().into_any_element();
    };
    let path = v.path.clone();
    let loading = v.loading;
    let image = v
        .image
        .as_ref()
        .map(|i| (i.width, i.height, i.render.clone()));
    let area = v.area;
    let drawing = v.drawing;
    let pick = v.pick;
    let faces = ws.library.faces_in(&path);
    let names: Vec<String> = ws.library.people.iter().map(|p| p.name.clone()).collect();
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let flat = ws.gallery_flat_order();
    let at = flat.iter().position(|p| p == &path);
    let position = at
        .map(|i| format!("{} of {}", i + 1, flat.len()))
        .unwrap_or_default();
    let edit_path = path.clone();
    let header = div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .h(px(36.0))
        .flex_none()
        .px_2()
        .bg(gpui::rgb(pal().chrome_bg))
        .border_b_1()
        .border_color(gpui::rgb(pal().chrome_edge))
        .child(gallery_button(
            "\u{2039} Back to photos",
            false,
            |ws, _w, cx| ws.close_viewer(cx),
            cx,
        ))
        .child(gallery_button(
            "\u{25c0}",
            false,
            |ws, _w, cx| ws.viewer_step(-1, cx),
            cx,
        ))
        .child(gallery_button(
            "\u{25b6}",
            false,
            |ws, _w, cx| ws.viewer_step(1, cx),
            cx,
        ))
        .child(
            div()
                .text_size(px(12.0))
                .text_color(gpui::rgb(pal().text))
                .truncate()
                .child(file_name),
        )
        .child(
            div()
                .text_size(px(11.0))
                .text_color(gpui::rgb(pal().text_dim))
                .child(position),
        )
        .child(div().flex_grow())
        .child(gallery_button(
            "Edit",
            true,
            move |ws, _w, cx| ws.open_from_gallery(edit_path.clone(), cx),
            cx,
        ));
    // The picture fits its room, which was measured last paint; the
    // first frame has no room yet and shows nothing for one frame.
    let picture_entity = cx.entity();
    let mut picture = div()
        .flex_grow()
        .min_w(px(0.0))
        .min_h(px(0.0))
        .relative()
        .flex()
        .items_center()
        .justify_center()
        .overflow_hidden()
        .bg(gpui::rgb(if crate::ui::is_light() {
            0xDADAD4
        } else {
            0x161616
        }))
        .child(
            canvas(
                move |bounds, _window, cx| {
                    picture_entity.update(cx, |ws, _| {
                        if let Some(viewer) = &mut ws.library.viewer {
                            viewer.area = bounds;
                        }
                    });
                },
                |_, _, _, _| {},
            )
            .absolute()
            .top_0()
            .left_0()
            .size_full(),
        );
    match image {
        Some((width, height, render)) => {
            let (aw, ah) = (f32::from(area.size.width), f32::from(area.size.height));
            if aw > 0.0 && ah > 0.0 {
                let scale = ((aw - 16.0) / width as f32).min((ah - 16.0) / height as f32);
                let scale = scale.clamp(0.01, 1.0);
                let (fw, fh) = (width as f32 * scale, height as f32 * scale);
                picture = picture.child(picture_element(
                    fw, fh, render, &faces, &names, pick, drawing, cx,
                ));
            }
        }
        None => {
            picture = picture.child(
                div()
                    .text_size(px(12.0))
                    .text_color(gpui::rgb(pal().text_dim))
                    .child(if loading {
                        "Loading\u{2026}"
                    } else {
                        "This photo could not be decoded."
                    }),
            );
        }
    }
    let panel = people_panel(ws, &path, &faces, pick, cx);
    div()
        .flex()
        .flex_col()
        .flex_grow()
        .min_h(px(0.0))
        .min_w(px(0.0))
        .child(header)
        .child(
            div()
                .flex()
                .flex_row()
                .flex_grow()
                .min_h(px(0.0))
                .min_w(px(0.0))
                .child(picture)
                .child(panel),
        )
        .into_any_element()
}

/// The photo itself, sized to fit, with a box per face and the box
/// being drawn. Presses on it pick faces or start boxes.
#[allow(clippy::too_many_arguments)]
fn picture_element(
    fw: f32,
    fh: f32,
    render: Arc<RenderImage>,
    faces: &[FaceView],
    names: &[String],
    pick: Option<FaceRect>,
    drawing: Option<((f32, f32), (f32, f32))>,
    cx: &mut Context<Workspace>,
) -> gpui::AnyElement {
    let bounds_entity = cx.entity();
    let mut el = div()
        .w(px(fw))
        .h(px(fh))
        .relative()
        .flex_none()
        .cursor(gpui::CursorStyle::Crosshair)
        .child(img(render).w(px(fw)).h(px(fh)))
        .child(
            canvas(
                move |bounds, _window, cx| {
                    bounds_entity.update(cx, |ws, _| {
                        if let Some(viewer) = &mut ws.library.viewer {
                            viewer.image_bounds = bounds;
                        }
                    });
                },
                |_, _, _, _| {},
            )
            // Pinned: an absolute element with no offsets sits at its
            // static position — after the picture, in flow — and this
            // one recorded its bounds a whole picture too low.
            .absolute()
            .top_0()
            .left_0()
            .size_full(),
        )
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|ws, ev: &MouseDownEvent, _w, cx| {
                ws.viewer_mouse_down(ev.position, cx);
            }),
        )
        .on_mouse_move(cx.listener(|ws, ev: &gpui::MouseMoveEvent, _w, cx| {
            ws.viewer_mouse_move(
                ev.position,
                ev.pressed_button == Some(MouseButton::Left),
                cx,
            );
        }))
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(|ws, _ev: &gpui::MouseUpEvent, _w, cx| ws.viewer_mouse_up(cx)),
        );
    for face in faces {
        let picked = pick.is_some_and(|p| p.same_face(&face.rect));
        let color = box_color(face, picked);
        let label = match (face.person, face.suggestion) {
            (Some(i), _) if face.auto => names.get(i).map(|n| format!("{n} \u{b7} auto")),
            (Some(i), _) => names.get(i).cloned(),
            (None, Some((i, _))) => names.get(i).map(|n| format!("{n}?")),
            (None, None) => None,
        };
        let (x, y, w, h) = (
            face.rect.x * fw,
            face.rect.y * fh,
            face.rect.w * fw,
            face.rect.h * fh,
        );
        let mut boxed = div()
            .absolute()
            .left(px(x))
            .top(px(y))
            .w(px(w))
            .h(px(h))
            .border_2()
            .border_color(gpui::rgb(color))
            .rounded_sm();
        if let Some(label) = label {
            boxed = boxed.child(
                div()
                    .absolute()
                    .top(px(h))
                    .left(px(-2.0))
                    .px_1()
                    .rounded_b_sm()
                    .bg(gpui::rgb(color))
                    .text_size(px(10.0))
                    .text_color(gpui::rgb(if color == 0xFFFFFF {
                        0x000000
                    } else {
                        0xFFFFFF
                    }))
                    .whitespace_nowrap()
                    .child(SharedString::from(label)),
            );
        }
        el = el.child(boxed);
    }
    // A hand-drawn face that is not yet a tag shows as the picked box.
    if let Some(rect) = pick.filter(|p| !faces.iter().any(|f| f.rect.same_face(p))) {
        el = el.child(
            div()
                .absolute()
                .left(px(rect.x * fw))
                .top(px(rect.y * fh))
                .w(px(rect.w * fw))
                .h(px(rect.h * fh))
                .border_2()
                .border_color(gpui::rgb(pal().select_border))
                .rounded_sm(),
        );
    }
    if let Some(((x0, y0), (x1, y1))) = drawing {
        let (l, t) = (x0.min(x1) * fw, y0.min(y1) * fh);
        let (w, h) = ((x1 - x0).abs() * fw, (y1 - y0).abs() * fh);
        el = el.child(
            div()
                .absolute()
                .left(px(l))
                .top(px(t))
                .w(px(w))
                .h(px(h))
                .border_1()
                .border_dashed()
                .border_color(gpui::rgb(0xFFFFFF)),
        );
    }
    el.into_any_element()
}

/// The panel beside the picture: every face in it, with its name, the
/// recogniser's guess, or a field to type into.
fn people_panel(
    ws: &mut Workspace,
    path: &Path,
    faces: &[FaceView],
    pick: Option<FaceRect>,
    cx: &mut Context<Workspace>,
) -> gpui::AnyElement {
    let detector = schist_neural::installed("face");
    let looked = ws.library.detected_faces(path).is_some();
    let mut col = div()
        .id("people-panel")
        .w(px(240.0))
        .flex_none()
        .flex()
        .flex_col()
        .gap_2()
        .p_2()
        .min_h(px(0.0))
        .overflow_y_scroll()
        .bg(gpui::rgb(pal().chrome_bg))
        .border_l_1()
        .border_color(gpui::rgb(pal().chrome_edge))
        .child(
            div()
                .text_size(px(11.0))
                .text_color(gpui::rgb(pal().text_dim))
                .child("PEOPLE IN THIS PHOTO"),
        );
    if !detector {
        col = col
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(gpui::rgb(pal().text))
                    .child(
                        "Faces are found by a small model that is downloaded once. \
                         Boxes can still be drawn by hand.",
                    ),
            )
            .child(gallery_button(
                "Find faces\u{2026}",
                true,
                |ws, _w, cx| ws.open_people_models(cx),
                cx,
            ));
    } else if !looked {
        col = col.child(
            div()
                .text_size(px(12.0))
                .text_color(gpui::rgb(pal().text_dim))
                .child("Looking for faces\u{2026}"),
        );
    } else if faces.is_empty() && pick.is_none() {
        col = col.child(
            div()
                .text_size(px(12.0))
                .text_color(gpui::rgb(pal().text_dim))
                .child("No faces found. Drag a box round one to add a person by hand."),
        );
    }
    for face in faces {
        let picked = pick.is_some_and(|p| p.same_face(&face.rect));
        col = col.child(face_row(ws, face, picked, cx));
    }
    // A box just drawn, not yet a face anyone knows.
    if let Some(rect) = pick.filter(|p| !faces.iter().any(|f| f.rect.same_face(p))) {
        let fresh = FaceView {
            rect,
            person: None,
            auto: false,
            suggestion: None,
            detected: false,
        };
        col = col.child(face_row(ws, &fresh, true, cx));
    }
    col = col.child(
        div()
            .pt_2()
            .text_size(px(11.0))
            .text_color(gpui::rgb(pal().text_dim))
            .child(
                "Click a face to name it; Enter saves. Drag on the photo to mark a face \
                 the detector missed. \u{2190} \u{2192} move between photos, Esc goes back.",
            ),
    );
    col.into_any_element()
}

/// A small text button on the gallery palette.
fn small_button(
    label: impl Into<SharedString>,
    primary: bool,
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
        .bg(gpui::rgb(if primary {
            pal().green
        } else {
            pal().button_bg
        }))
        .text_color(gpui::rgb(if primary { 0xFFFFFF } else { pal().text }))
        .hover(move |s| {
            s.bg(gpui::rgb(if primary {
                pal().green_hover
            } else {
                pal().button_hover
            }))
        })
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |ws, _e: &MouseDownEvent, _w, cx| {
                cx.stop_propagation();
                on_click(ws, cx)
            }),
        )
        .child(label.into())
}

/// One face in the panel: its crop, and its name, guess, or field.
fn face_row(
    ws: &mut Workspace,
    face: &FaceView,
    picked: bool,
    cx: &mut Context<Workspace>,
) -> gpui::AnyElement {
    let crop = ws.viewer_crop(&face.rect);
    let rect = face.rect;
    let named = face
        .person
        .and_then(|i| ws.library.people.get(i))
        .map(|p| p.name.clone());
    let guess = face
        .suggestion
        .and_then(|(i, _)| ws.library.people.get(i).map(|p| (i, p.name.clone())));
    let mut row = div()
        .flex()
        .flex_col()
        .gap_1()
        .p_1()
        .rounded_md()
        .bg(gpui::rgb(if picked {
            pal().sidebar_selected
        } else {
            pal().button_bg
        }));
    let pick_rect = rect;
    let mut top = div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .cursor_pointer()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |ws, _e: &MouseDownEvent, _w, cx| {
                ws.viewer_pick(pick_rect);
                cx.notify();
            }),
        )
        .child(avatar(crop, AVATAR_PX as f32 * 0.75));
    if picked {
        top = top.child(name_field(ws, cx));
        row = row.child(top);
        // Completions: the names that begin with what is typed, so a
        // known person is one click, not a whole name.
        let typed = ws.field_buffer.clone();
        let completions: Vec<(usize, String)> = ws
            .library
            .names_starting(&typed)
            .into_iter()
            .filter(|(_, n)| !n.eq_ignore_ascii_case(typed.trim()))
            .take(5)
            .collect();
        for (index, name) in completions {
            row = row.child(
                div()
                    .pl(px(56.0))
                    .h(px(20.0))
                    .flex()
                    .items_center()
                    .text_size(px(12.0))
                    .text_color(gpui::rgb(pal().header))
                    .cursor_pointer()
                    .hover(|s| s.bg(gpui::rgb(pal().button_hover)))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |ws, _e: &MouseDownEvent, _w, cx| {
                            cx.stop_propagation();
                            ws.viewer_name_as(rect, index, cx);
                        }),
                    )
                    .child(SharedString::from(name)),
            );
        }
        let mut actions = div()
            .flex()
            .flex_row()
            .gap_1()
            .pl(px(56.0))
            .child(small_button(
                "Save",
                true,
                |ws, cx| ws.viewer_commit_name(cx),
                cx,
            ));
        if named.is_some() {
            actions = actions.child(small_button(
                if face.auto { "Not them" } else { "Remove name" },
                false,
                move |ws, cx| ws.viewer_untag(rect, cx),
                cx,
            ));
        } else if face.detected {
            actions = actions.child(small_button(
                "Not a face",
                false,
                move |ws, cx| ws.viewer_ignore(rect, cx),
                cx,
            ));
        }
        actions = actions.child(small_button(
            "Cancel",
            false,
            |ws, cx| {
                ws.viewer_unpick();
                cx.notify();
            },
            cx,
        ));
        row = row.child(actions);
    } else {
        match (named, guess) {
            (Some(name), _) => {
                top = top.child(
                    div()
                        .flex_grow()
                        .truncate()
                        .text_size(px(12.0))
                        .text_color(gpui::rgb(pal().text))
                        .child(SharedString::from(name)),
                );
                if face.auto {
                    // The recogniser's doing: say so, and offer the
                    // one-click "no" a guess deserves.
                    top = top
                        .child(
                            div()
                                .text_size(px(10.0))
                                .text_color(gpui::rgb(pal().text_dim))
                                .child("auto"),
                        )
                        .child(small_button(
                            "Not them",
                            false,
                            move |ws, cx| ws.viewer_untag(rect, cx),
                            cx,
                        ));
                }
            }
            (None, Some((index, name))) => {
                top = top.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .flex_grow()
                        .child(
                            div()
                                .text_size(px(12.0))
                                .text_color(gpui::rgb(pal().text))
                                .child(SharedString::from(format!("Is this {name}?"))),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .gap_1()
                                .child(small_button(
                                    "Yes",
                                    true,
                                    move |ws, cx| ws.viewer_name_as(rect, index, cx),
                                    cx,
                                ))
                                .child(small_button(
                                    "Someone else",
                                    false,
                                    move |ws, cx| {
                                        ws.viewer_pick(rect);
                                        cx.notify();
                                    },
                                    cx,
                                )),
                        ),
                );
            }
            (None, None) => {
                top = top.child(
                    div()
                        .flex_grow()
                        .text_size(px(12.0))
                        .text_color(gpui::rgb(pal().text_dim))
                        .child("Add a name\u{2026}"),
                );
            }
        }
        row = row.child(top);
    }
    row.into_any_element()
}

/// The name field in the people panel: the search box's caret and
/// palette, the dialog fields' buffer.
fn name_field(ws: &Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let focused = ws.focused_field == Some("face-name");
    let current = ws
        .library
        .viewer
        .as_ref()
        .map(|v| v.name.clone())
        .unwrap_or_default();
    let typed = if focused {
        ws.field_buffer.clone()
    } else {
        current.clone()
    };
    let cursor = ws.field_cursor.min(typed.len());
    let caret_on = ws.caret_on();
    div()
        .flex_grow()
        .h(px(24.0))
        .px_2()
        .flex()
        .flex_row()
        .items_center()
        .rounded_md()
        .bg(gpui::rgb(pal().grid_bg))
        .border_1()
        .border_color(gpui::rgb(if focused {
            pal().select_border
        } else {
            pal().chrome_edge
        }))
        .text_size(px(12.0))
        .text_color(gpui::rgb(pal().text))
        .cursor(gpui::CursorStyle::IBeam)
        .overflow_hidden()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |ws, _e: &MouseDownEvent, _w, cx| {
                cx.stop_propagation();
                if ws.focused_field != Some("face-name") {
                    ws.focus_field("face-name", current.clone());
                }
                cx.notify();
            }),
        )
        .child(if focused {
            div()
                .flex()
                .flex_row()
                .items_center()
                .child(crate::ui::caret_run(
                    typed[..cursor].to_string(),
                    typed[cursor..].to_string(),
                    caret_on,
                    pal().text,
                ))
                .children(typed.is_empty().then(|| {
                    div()
                        .text_color(gpui::rgb(pal().text_dim))
                        .child("Who is this?")
                }))
                .into_any_element()
        } else if typed.is_empty() {
            div()
                .text_color(gpui::rgb(pal().text_dim))
                .child("Who is this?")
                .into_any_element()
        } else {
            div().child(SharedString::from(typed)).into_any_element()
        })
}

fn model_link(
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

/// The licences behind the People album, and the button that accepts
/// them. One dialog for both models: whichever is missing is fetched.
pub(crate) fn people_models_dialog(cx: &mut Context<Workspace>) -> impl IntoElement {
    let specs: Vec<&'static schist_neural::ModelSpec> = PEOPLE_MODELS
        .iter()
        .filter(|id| !schist_neural::installed(id))
        .filter_map(|id| schist_neural::spec(id))
        .collect();
    let total: usize = specs.iter().map(|s| s.bytes).sum();
    let mut body = div().flex().flex_col().gap_2().w(px(460.0)).child(
        div()
            .text_size(px(12.0))
            .text_color(gpui::rgb(crate::ui::palette().text))
            .child(
                "Finding the people in your photos needs two small models, \
                 downloaded once and kept on this machine. They run locally: \
                 no photo ever leaves it. The detector finds faces; the \
                 recogniser tells them apart, so once you have named someone \
                 a few times it can suggest them elsewhere.",
            ),
    );
    for spec in &specs {
        body = body.child(div().text_size(px(12.0)).child(SharedString::from(format!(
            "{} \u{b7} {:.1} MB \u{b7} {}",
            spec.name,
            spec.bytes as f64 / (1 << 20) as f64,
            spec.license
        ))));
    }
    body = body.child(
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .text_size(px(11.0))
            .text_color(gpui::rgb(crate::ui::palette().text_dim))
            .child(model_link(
                "ultraface-source",
                "UltraFace (ONNX Model Zoo)",
                "https://github.com/onnx/models/tree/main/validated/vision/body_analysis/ultraface",
                cx,
            ))
            .child("\u{b7}")
            .child(model_link(
                "sface-source",
                "SFace (OpenCV Zoo)",
                "https://github.com/opencv/opencv_zoo/tree/main/models/face_recognition_sface",
                cx,
            )),
    );
    body = body.child(
        div()
            .pt_1()
            .text_size(px(11.0))
            .text_color(gpui::rgb(crate::ui::palette().text_dim))
            .child(SharedString::from(format!(
                "Downloading installs {} ({:.1} MB) and accepts the licences. They can be \
                 removed again under Gallery \u{25b8} Manage Models\u{2026}",
                if specs.len() == 1 { "it" } else { "both" },
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
                for id in PEOPLE_MODELS {
                    if !schist_neural::installed(id) {
                        ws.download_model(id, cx);
                    }
                }
                ws.close_modal(cx);
            },
            cx,
        ));
    crate::ui::modal_frame("People", 500.0, body, actions)
}

/// Rename a person. Renaming to a name somebody else has merges them.
pub(crate) fn person_name_dialog(
    ws: &mut Workspace,
    index: usize,
    name: String,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let field = bucket_field("person-name", name, "Name".to_string(), ws, cx);
    let live = if ws.focused_field == Some("person-name") {
        ws.field_buffer.clone()
    } else {
        ws.library
            .people
            .get(index)
            .map(|p| p.name.clone())
            .unwrap_or_default()
    };
    let merges = ws
        .library
        .people
        .iter()
        .enumerate()
        .any(|(i, p)| i != index && p.name.eq_ignore_ascii_case(live.trim()));
    let body = div()
        .flex()
        .flex_col()
        .gap_2()
        .child(crate::ui::field_row("Name", field))
        .child(
            div()
                .text_size(px(11.0))
                .text_color(gpui::rgb(crate::ui::palette().text_dim))
                .child(if merges {
                    format!(
                        "Somebody is already called \u{201c}{}\u{201d}: saving merges the two.",
                        live.trim()
                    )
                } else {
                    "Renaming to a name somebody else already has merges the two people.".into()
                }),
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
            "Save",
            true,
            |ws, _w, cx| {
                ws.commit_focused_field();
                if let Some(Modal::PersonName { index, name }) = ws.modal.clone() {
                    ws.library.rename_person(index, &name);
                }
                ws.close_modal(cx);
            },
            cx,
        ));
    crate::ui::modal_frame("Rename Person", 420.0, body, actions)
}
