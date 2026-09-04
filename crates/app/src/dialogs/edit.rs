//! Edit-menu dialogs: Content-Aware Scale, Stroke, Fill, Select ▸
//! Modify and Color Range.

use super::*;

/// Edit ▸ Content-Aware Scale.
pub(super) fn content_aware_scale_dialog(
    state: &DialogState,
    width: u32,
    height: u32,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let body = div()
        .flex()
        .flex_col()
        .gap_1()
        .child(ui::field_row(
            "Width",
            ui::num_field(
                ui::NumField {
                    id: "cas-width",
                    value: width as f32,
                    suffix: " px",
                    step: 10.0,
                    focused: state.focused_field == Some("cas-width"),
                    buffer: state.field_buffer.clone(),
                },
                |ws, v| {
                    ws.update_modal(|m| {
                        if let Modal::ContentAwareScale { width, .. } = m {
                            // `v` is the step delta, as in Image Size.
                            *width = step_dim(*width, v);
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
                    id: "cas-height",
                    value: height as f32,
                    suffix: " px",
                    step: 10.0,
                    focused: state.focused_field == Some("cas-height"),
                    buffer: state.field_buffer.clone(),
                },
                |ws, v| {
                    ws.update_modal(|m| {
                        if let Modal::ContentAwareScale { height, .. } = m {
                            // `v` is the step delta, as in Image Size.
                            *height = step_dim(*height, v);
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
                .child("Carves low-detail seams. A selection marks what to protect."),
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
            |ws, _w, cx| {
                let mut run = None;
                ws.update_modal(|m| {
                    if let Modal::ContentAwareScale { width, height } = m {
                        run = Some((*width, *height));
                    }
                });
                ws.close_modal(cx);
                if let Some((w, h)) = run {
                    ws.content_aware_scale(w, h, cx);
                }
            },
            cx,
        ));
    ui::modal_frame("Content-Aware Scale", 340.0, body, actions)
}

/// Edit ▸ Stroke.
pub(super) fn stroke_dialog(
    ws: &Workspace,
    _state: &DialogState,
    width: f32,
    position: schist_core::StrokePosition,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    use schist_core::StrokePosition;
    let body = div()
        .flex()
        .flex_col()
        .gap_1()
        .child(param_slider(
            SliderSpec {
                id: "stroke-width",
                label: "Width",
                value: width,
                min: 1.0,
                max: 250.0,
                suffix: " px",
                ..Default::default()
            },
            |ws, v, _cx| {
                ws.update_modal(|m| {
                    if let Modal::Stroke { width, .. } = m {
                        *width = v;
                    }
                });
            },
            cx,
        ))
        .child(ui::field_row(
            "Location",
            ui::dropdown(
                &ws.dropdown,
                ui::Dropdown {
                    popup: Popup::Field("stroke-position"),
                    is_open: ws.open_popup == Some(Popup::Field("stroke-position")),
                    current: position,
                    label: match position {
                        StrokePosition::Inside => "Inside",
                        StrokePosition::Center => "Center",
                        StrokePosition::Outside => "Outside",
                    }
                    .into(),
                    width: 150.0,
                    options: vec![
                        ("Inside".into(), StrokePosition::Inside),
                        ("Center".into(), StrokePosition::Center),
                        ("Outside".into(), StrokePosition::Outside),
                    ],
                },
                |ws, p, _cx| {
                    ws.update_modal(|m| {
                        if let Modal::Stroke { position, .. } = m {
                            *position = p;
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
                .child("Strokes the selection in the foreground colour."),
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
            |ws, _w, cx| {
                let mut run = None;
                ws.update_modal(|m| {
                    if let Modal::Stroke { width, position } = m {
                        run = Some((*width, *position));
                    }
                });
                ws.close_modal(cx);
                if let Some((w, p)) = run {
                    ws.stroke_selection(w, p, cx);
                }
            },
            cx,
        ));
    ui::modal_frame("Stroke", 340.0, body, actions)
}

/// Edit ▸ Fill.
pub(super) fn fill_dialog(
    ws: &Workspace,
    _state: &DialogState,
    source: crate::workspace::FillSource,
    opacity: f32,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    use crate::workspace::FillSource;
    let body = div()
        .flex()
        .flex_col()
        .gap_1()
        .child(ui::field_row(
            "Contents",
            ui::dropdown(
                &ws.dropdown,
                ui::Dropdown {
                    popup: Popup::Field("fill-source"),
                    is_open: ws.open_popup == Some(Popup::Field("fill-source")),
                    current: source,
                    label: source.label().into(),
                    width: 170.0,
                    options: FillSource::ALL
                        .iter()
                        .map(|s| (SharedString::from(s.label()), *s))
                        .collect(),
                },
                |ws, s, _cx| {
                    ws.update_modal(|m| {
                        if let Modal::Fill { source, .. } = m {
                            *source = s;
                        }
                    });
                },
                cx,
            ),
        ))
        .child(param_slider(
            SliderSpec {
                id: "fill-opacity",
                label: "Opacity",
                value: opacity * 100.0,
                min: 0.0,
                max: 100.0,
                suffix: "%",
                ..Default::default()
            },
            |ws, v, _cx| {
                ws.update_modal(|m| {
                    if let Modal::Fill { opacity, .. } = m {
                        *opacity = v / 100.0;
                    }
                });
            },
            cx,
        ));
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
            |ws, _w, cx| {
                let mut run = None;
                ws.update_modal(|m| {
                    if let Modal::Fill { source, opacity } = m {
                        run = Some((*source, *opacity));
                    }
                });
                ws.close_modal(cx);
                if let Some((s, o)) = run {
                    ws.fill_selection(s, o, cx);
                }
            },
            cx,
        ));
    ui::modal_frame("Fill", 340.0, body, actions)
}

/// Select ▸ Modify: one amount and an OK.
pub(super) fn modify_dialog(
    _state: &DialogState,
    kind: crate::workspace::ModifyKind,
    amount: f32,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    use crate::workspace::ModifyKind;
    let max = match kind {
        ModifyKind::Smooth => 100.0,
        ModifyKind::Feather => 250.0,
        _ => 500.0,
    };
    let body = div().flex().flex_col().gap_1().child(param_slider(
        SliderSpec {
            id: "modify-amount",
            label: kind.label(),
            value: amount,
            min: if kind == ModifyKind::Feather {
                0.0
            } else {
                1.0
            },
            max,
            suffix: " px",
            ..Default::default()
        },
        |ws, v, _cx| {
            ws.update_modal(|m| {
                if let Modal::SelectModify { amount, .. } = m {
                    *amount = v;
                }
            });
        },
        cx,
    ));
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
                let mut run = None;
                ws.update_modal(|m| {
                    if let Modal::SelectModify { kind, amount } = m {
                        run = Some((*kind, *amount));
                    }
                });
                ws.close_modal(cx);
                if let Some((kind, amount)) = run {
                    ws.apply_select_modify(kind, amount, cx);
                }
            },
            cx,
        ));
    ui::modal_frame(kind.title(), 320.0, body, actions)
}

/// Select ▸ Color Range.
pub(super) fn color_range_dialog(
    _state: &DialogState,
    tolerance: f32,
    target: schist_color::Rgba,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let swatch = {
        let q = |v: f32| ((v.clamp(0.0, 1.0) * 255.0).round() as u32) & 0xFF;
        (q(target.r) << 16) | (q(target.g) << 8) | q(target.b)
    };
    let body = div()
        .flex()
        .flex_col()
        .gap_1()
        .child(ui::field_row(
            "Sampled",
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .id("color-range-swatch")
                        .size(px(18.0))
                        .flex_none()
                        .rounded_sm()
                        .border_1()
                        .border_color(gpui::rgb(ui::palette().edge))
                        .bg(gpui::rgb(swatch))
                        .cursor_pointer()
                        .hover(|s| s.border_color(gpui::rgb(ui::palette().text)))
                        .on_mouse_down(
                            gpui::MouseButton::Left,
                            cx.listener(move |ws, _e, _w, cx| {
                                // This dialog stays open underneath the
                                // picker and takes the colour on OK.
                                ws.open_color_picker_on(ColorTarget::ColorRange, target, cx);
                            }),
                        ),
                )
                .child(ui::button(
                    "Use Foreground",
                    false,
                    |ws, _w, cx| {
                        let fg = ws.editor.foreground;
                        ws.update_modal(|m| {
                            if let Modal::ColorRange { target, .. } = m {
                                *target = fg;
                            }
                        });
                        cx.notify();
                    },
                    cx,
                )),
        ))
        .child(param_slider(
            SliderSpec {
                id: "color-range-fuzziness",
                label: "Fuzziness",
                value: tolerance,
                min: 0.0,
                max: 200.0,
                suffix: "",
                ..Default::default()
            },
            |ws, v, _cx| {
                ws.update_modal(|m| {
                    if let Modal::ColorRange { tolerance, .. } = m {
                        *tolerance = v;
                    }
                });
            },
            cx,
        ))
        .child(
            div()
                .text_size(px(11.0))
                .text_color(gpui::rgb(ui::palette().text_dim))
                .child("Selects pixels near the sampled colour on the active layer."),
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
                let mut run = None;
                ws.update_modal(|m| {
                    if let Modal::ColorRange { tolerance, target } = m {
                        run = Some((*tolerance, *target));
                    }
                });
                ws.close_modal(cx);
                if let Some((tolerance, target)) = run {
                    ws.apply_color_range(tolerance, target, cx);
                }
            },
            cx,
        ));
    ui::modal_frame("Color Range", 360.0, body, actions)
}
