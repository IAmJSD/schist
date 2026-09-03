//! Image Size and Canvas Size.

use super::*;

pub(super) fn resample_options() -> Vec<(SharedString, Resample)> {
    [
        Resample::Classic(Filter::Bicubic),
        Resample::Classic(Filter::Bilinear),
        Resample::Classic(Filter::Nearest),
        Resample::Neural("waifu2x-photo"),
        Resample::Neural("waifu2x-art"),
    ]
    .into_iter()
    .map(|r| (r.display_name().into(), r))
    .collect()
}

pub(super) fn image_size(
    ws: &mut Workspace,
    state: &DialogState,
    width: u32,
    height: u32,
    resample: Resample,
    link: bool,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let (doc_w, doc_h) = ws
        .doc
        .as_ref()
        .map(|d| (d.width, d.height))
        .unwrap_or((1, 1));
    let aspect = doc_w as f32 / doc_h.max(1) as f32;

    let body = div()
        .flex()
        .flex_col()
        .gap_1()
        .child(ui::field_row(
            "Width",
            ui::num_field(
                ui::NumField {
                    id: "image-size-w",
                    value: width as f32,
                    suffix: " px",
                    step: 10.0,
                    focused: state.focused_field == Some("image-size-w"),
                    buffer: state.field_buffer.clone(),
                },
                move |ws, delta| {
                    ws.update_modal(|m| {
                        if let Modal::ImageSize {
                            width,
                            height,
                            link,
                            ..
                        } = m
                        {
                            *width = step_dim(*width, delta);
                            if *link {
                                *height = ((*width as f32 / aspect).round().max(1.0)) as u32;
                            }
                        }
                    });
                },
                cx,
            ),
        ))
        .child(ui::field_row(
            "Height",
            ui::num_field(
                ui::NumField {
                    id: "image-size-h",
                    value: height as f32,
                    suffix: " px",
                    step: 10.0,
                    focused: state.focused_field == Some("image-size-h"),
                    buffer: state.field_buffer.clone(),
                },
                move |ws, delta| {
                    ws.update_modal(|m| {
                        if let Modal::ImageSize {
                            width,
                            height,
                            link,
                            ..
                        } = m
                        {
                            *height = step_dim(*height, delta);
                            if *link {
                                *width = ((*height as f32 * aspect).round().max(1.0)) as u32;
                            }
                        }
                    });
                },
                cx,
            ),
        ))
        .child(ui::field_row(
            "Constrain",
            ui::checkbox(
                "Keep proportions",
                link,
                |ws, _cx| {
                    ws.update_modal(|m| {
                        if let Modal::ImageSize { link, .. } = m {
                            *link = !*link;
                        }
                    });
                },
                cx,
            ),
        ))
        .child(ui::field_row(
            "Resample",
            ui::dropdown(
                &ws.dropdown,
                ui::Dropdown {
                    popup: Popup::Field("image-size-filter"),
                    is_open: state.open_popup == Some(Popup::Field("image-size-filter")),
                    current: resample,
                    label: (resample.display_name()).into(),
                    width: 170.0,
                    options: resample_options(),
                },
                |ws, value, _cx| {
                    ws.update_modal(|m| {
                        if let Modal::ImageSize { resample, .. } = m {
                            *resample = value;
                        }
                    });
                },
                cx,
            ),
        ))
        .child(
            div()
                .text_size(px(11.0))
                .text_color(gpui::rgb(ui::palette().text_dim))
                .child(format!("Currently {doc_w} × {doc_h} px")),
        );

    let actions = div()
        .flex()
        .flex_row()
        .gap_2()
        .child(ui::button(
            "Cancel",
            false,
            |ws, _w, cx| ws.close_modal(cx),
            cx,
        ))
        .child(ui::button(
            "OK",
            true,
            move |ws, _w, cx| {
                // The neural path closes the dialog itself, because it may
                // have to put a "working" one up in its place.
                if let Resample::Neural(id) = resample {
                    ws.resize_image_neural(width, height, id, cx);
                    return;
                }
                if let Some(doc) = ws.doc.as_mut() {
                    schist_tools_transform::resize_image_with(doc, width, height, resample);
                }
                ws.status = format!("Image size: {width} × {height}").into();
                ws.close_modal(cx);
                ws.after_change(cx);
                ws.fit_to_view();
            },
            cx,
        ));

    ui::modal_frame("Image Size", 340.0, body, actions)
}

/// The nine-way anchor grid for Canvas Size.
pub(super) fn anchor_grid(anchor: (f32, f32), cx: &mut Context<Workspace>) -> impl IntoElement {
    let mut grid = div().flex().flex_col().gap_1();
    for row in 0..3 {
        let mut line = div().flex().flex_row().gap_1();
        for col in 0..3 {
            let value = (col as f32 / 2.0, row as f32 / 2.0);
            let selected = (anchor.0 - value.0).abs() < 0.01 && (anchor.1 - value.1).abs() < 0.01;
            line = line.child(
                div()
                    .size(px(18.0))
                    .rounded_sm()
                    .bg(gpui::rgb(if selected {
                        ui::palette().accent
                    } else {
                        ui::palette().field_bg
                    }))
                    .border_1()
                    .border_color(gpui::rgb(ui::palette().edge))
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(move |ws, _e, _w, cx| {
                            ws.update_modal(|m| {
                                if let Modal::CanvasSize { anchor, .. } = m {
                                    *anchor = value;
                                }
                            });
                            cx.notify();
                        }),
                    ),
            );
        }
        grid = grid.child(line);
    }
    grid
}

pub(super) fn canvas_size(
    ws: &mut Workspace,
    state: &DialogState,
    width: u32,
    height: u32,
    anchor: (f32, f32),
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let (doc_w, doc_h) = ws
        .doc
        .as_ref()
        .map(|d| (d.width, d.height))
        .unwrap_or((1, 1));
    let body = div()
        .flex()
        .flex_col()
        .gap_1()
        .child(ui::field_row(
            "Width",
            ui::num_field(
                ui::NumField {
                    id: "canvas-size-w",
                    value: width as f32,
                    suffix: " px",
                    step: 10.0,
                    focused: state.focused_field == Some("canvas-size-w"),
                    buffer: state.field_buffer.clone(),
                },
                |ws, delta| {
                    ws.update_modal(|m| {
                        if let Modal::CanvasSize { width, .. } = m {
                            *width = step_dim(*width, delta);
                        }
                    });
                },
                cx,
            ),
        ))
        .child(ui::field_row(
            "Height",
            ui::num_field(
                ui::NumField {
                    id: "canvas-size-h",
                    value: height as f32,
                    suffix: " px",
                    step: 10.0,
                    focused: state.focused_field == Some("canvas-size-h"),
                    buffer: state.field_buffer.clone(),
                },
                |ws, delta| {
                    ws.update_modal(|m| {
                        if let Modal::CanvasSize { height, .. } = m {
                            *height = step_dim(*height, delta);
                        }
                    });
                },
                cx,
            ),
        ))
        .child(ui::field_row("Anchor", anchor_grid(anchor, cx)))
        .child(
            div()
                .text_size(px(11.0))
                .text_color(gpui::rgb(ui::palette().text_dim))
                .child(format!("Currently {doc_w} × {doc_h} px")),
        );

    let actions = div()
        .flex()
        .flex_row()
        .gap_2()
        .child(ui::button(
            "Cancel",
            false,
            |ws, _w, cx| ws.close_modal(cx),
            cx,
        ))
        .child(ui::button(
            "OK",
            true,
            move |ws, _w, cx| {
                if let Some(doc) = ws.doc.as_mut() {
                    schist_tools_transform::resize_canvas(doc, width, height, anchor);
                }
                ws.status = format!("Canvas size: {width} × {height}").into();
                ws.close_modal(cx);
                ws.after_change(cx);
                ws.fit_to_view();
            },
            cx,
        ));

    ui::modal_frame("Canvas Size", 340.0, body, actions)
}
