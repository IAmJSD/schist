//! Modal dialogs: Image Size, Canvas Size, and (from M8/M10) filters,
//! adjustments, export options and preferences.

use crate::ui;
use crate::workspace::{Modal, Popup, Workspace};
use gpui::{
    div, px, Context, InteractiveElement as _, IntoElement, ParentElement as _, SharedString,
    Styled as _,
};
use photoslop_core::Filter;

/// Render whichever modal is open, if any.
pub fn render(ws: &mut Workspace, cx: &mut Context<Workspace>) -> Option<gpui::AnyElement> {
    let modal = ws.modal.clone()?;
    Some(match modal {
        Modal::ImageSize {
            width,
            height,
            filter,
            link,
        } => image_size(ws, width, height, filter, link, cx).into_any_element(),
        Modal::CanvasSize {
            width,
            height,
            anchor,
        } => canvas_size(ws, width, height, anchor, cx).into_any_element(),
    })
}

fn filter_options() -> Vec<(SharedString, Filter)> {
    vec![
        (Filter::Bicubic.display_name().into(), Filter::Bicubic),
        (Filter::Bilinear.display_name().into(), Filter::Bilinear),
        (Filter::Nearest.display_name().into(), Filter::Nearest),
    ]
}

fn image_size(
    ws: &mut Workspace,
    width: u32,
    height: u32,
    filter: Filter,
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
                "image-size-w",
                width as f32,
                " px",
                10.0,
                move |ws, delta| {
                    ws.update_modal(|m| {
                        if let Modal::ImageSize {
                            width,
                            height,
                            link,
                            ..
                        } = m
                        {
                            *width = ((*width as f32 + delta).max(1.0)) as u32;
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
                "image-size-h",
                height as f32,
                " px",
                10.0,
                move |ws, delta| {
                    ws.update_modal(|m| {
                        if let Modal::ImageSize {
                            width,
                            height,
                            link,
                            ..
                        } = m
                        {
                            *height = ((*height as f32 + delta).max(1.0)) as u32;
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
                |ws| {
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
                Popup::Field("image-size-filter"),
                &filter,
                filter.display_name(),
                150.0,
                filter_options(),
                |ws, value| {
                    ws.update_modal(|m| {
                        if let Modal::ImageSize { filter, .. } = m {
                            *filter = value;
                        }
                    });
                },
                cx,
            ),
        ))
        .child(
            div()
                .text_size(px(11.0))
                .text_color(gpui::rgb(ui::TEXT_DIM))
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
                    photoslop_tools_transform::resize_image(doc, width, height, filter);
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
fn anchor_grid(anchor: (f32, f32), cx: &mut Context<Workspace>) -> impl IntoElement {
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
                    .bg(gpui::rgb(if selected { ui::ACCENT } else { ui::FIELD_BG }))
                    .border_1()
                    .border_color(gpui::rgb(ui::EDGE))
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

fn canvas_size(
    ws: &mut Workspace,
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
                "canvas-size-w",
                width as f32,
                " px",
                10.0,
                |ws, delta| {
                    ws.update_modal(|m| {
                        if let Modal::CanvasSize { width, .. } = m {
                            *width = ((*width as f32 + delta).max(1.0)) as u32;
                        }
                    });
                },
                cx,
            ),
        ))
        .child(ui::field_row(
            "Height",
            ui::num_field(
                "canvas-size-h",
                height as f32,
                " px",
                10.0,
                |ws, delta| {
                    ws.update_modal(|m| {
                        if let Modal::CanvasSize { height, .. } = m {
                            *height = ((*height as f32 + delta).max(1.0)) as u32;
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
                .text_color(gpui::rgb(ui::TEXT_DIM))
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
                    photoslop_tools_transform::resize_canvas(doc, width, height, anchor);
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
