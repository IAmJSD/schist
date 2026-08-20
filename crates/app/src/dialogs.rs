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
        Modal::Filter { id, values } => filter_dialog(ws, id, values, cx).into_any_element(),
        Modal::Adjustment {
            layer,
            params,
            original,
        } => adjustment_dialog(layer, params, original, cx).into_any_element(),
        Modal::PluginManager => plugin_manager(ws, cx).into_any_element(),
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

/// One slider row's description, shared by the filter and adjustment
/// dialogs (both render the same control from the same shape).
struct SliderSpec {
    id: &'static str,
    label: &'static str,
    value: f32,
    min: f32,
    max: f32,
    suffix: &'static str,
}

/// A labelled slider row used by the filter and adjustment dialogs.
fn param_slider(
    spec: SliderSpec,
    on_change: impl Fn(&mut Workspace, f32) + Clone + 'static,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let SliderSpec {
        id,
        label,
        value,
        min,
        max,
        suffix,
    } = spec;
    let span = (max - min).max(1e-6);
    let ratio = ((value - min) / span).clamp(0.0, 1.0);
    let display = if max - min > 20.0 {
        format!("{value:.0}{suffix}")
    } else {
        format!("{value:.2}{suffix}")
    };
    let set = on_change.clone();
    ui::field_row(
        label,
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .child(ui::slider_track(
                id,
                ratio,
                120.0,
                move |ws, r| set(ws, min + r * span),
                cx,
            ))
            .child(
                div()
                    .w(px(56.0))
                    .flex_none()
                    .text_size(px(11.0))
                    .child(display),
            ),
    )
}

fn filter_dialog(
    ws: &mut Workspace,
    id: &'static str,
    values: photoslop_plugin_api::FilterValues,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let (name, specs) = ws
        .registry
        .filters()
        .find(|f| f.id() == id)
        .map(|f| (f.name().to_string(), f.params()))
        .unwrap_or_else(|| (id.to_string(), Vec::new()));

    let mut body = div().flex().flex_col().gap_1();
    for spec in specs {
        let key = spec.key;
        body = body.child(param_slider(
            SliderSpec {
                id: spec.key,
                label: spec.label,
                value: values.get(spec.key),
                min: spec.min,
                max: spec.max,
                suffix: spec.suffix,
            },
            move |ws, v| {
                ws.update_modal(|m| {
                    if let Modal::Filter { values, .. } = m {
                        values.set(key, v);
                    }
                });
            },
            cx,
        ));
    }
    body = body.child(
        div()
            .text_size(px(11.0))
            .text_color(gpui::rgb(ui::TEXT_DIM))
            .child("Applies to the active layer, inside the selection."),
    );

    let apply_values = values.clone();
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
                ws.apply_filter(id, &apply_values, cx);
                ws.close_modal(cx);
            },
            cx,
        ));
    ui::modal_frame(name, 360.0, body, actions)
}

fn adjustment_dialog(
    layer: photoslop_core::LayerId,
    params: photoslop_adjustments::Params,
    original: (Option<String>, Vec<u8>),
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let specs = params.param_specs();
    let title = params.display_name().to_string();
    let mut body = div().flex().flex_col().gap_1();
    for spec in specs {
        let key = spec.key;
        body = body.child(param_slider(
            SliderSpec {
                id: spec.key,
                label: spec.label,
                value: spec.value,
                min: spec.min,
                max: spec.max,
                suffix: spec.suffix,
            },
            move |ws, v| {
                // Live preview: write straight onto the layer as the
                // slider moves, then commit one history entry on OK.
                let mut updated = None;
                ws.update_modal(|m| {
                    if let Modal::Adjustment { params, .. } = m {
                        params.set_param(key, v);
                        updated = Some(params.clone());
                    }
                });
                if let Some(params) = updated {
                    ws.preview_adjustment(layer, &params);
                }
            },
            cx,
        ));
    }

    let committed = params.clone();
    let cancel_original = original.clone();
    let actions = div()
        .flex()
        .flex_row()
        .gap_2()
        .child(ui::button(
            "Cancel",
            false,
            move |ws, _w, cx| {
                ws.revert_adjustment(layer, cancel_original.clone(), cx);
                ws.close_modal(cx);
            },
            cx,
        ))
        .child(ui::button(
            "OK",
            true,
            move |ws, _w, cx| {
                ws.commit_adjustment(layer, &committed, original.clone(), cx);
                ws.close_modal(cx);
            },
            cx,
        ));
    ui::modal_frame(title, 360.0, body, actions)
}

/// The third-party plugin manager: what loaded, what didn't and why, and
/// per-plugin enable/disable.
fn plugin_manager(ws: &mut Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let dir = photoslop_plugin_host_wasm::PluginManager::plugin_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "(no config directory)".into());
    let rows: Vec<gpui::AnyElement> = ws
        .plugins
        .entries
        .iter()
        .map(|entry| {
            let id = entry.id.clone();
            let enabled = entry.enabled;
            let kind = match &entry.kind {
                Some(photoslop_plugin_host_wasm::abi::PluginKind::Filter) => "filter",
                Some(photoslop_plugin_host_wasm::abi::PluginKind::Codec) => "format",
                None => "unavailable",
            };
            let detail = match &entry.error {
                Some(err) => err.to_string(),
                None => format!("{kind} · {}", entry.id),
            };
            let failed = entry.error.is_some();
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap_2()
                .py_1()
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .child(div().text_size(px(12.0)).child(entry.name.clone()))
                        .child(
                            div()
                                .text_size(px(10.0))
                                .text_color(gpui::rgb(if failed { 0xD08770 } else { ui::TEXT_DIM }))
                                .child(detail),
                        ),
                )
                .child(if failed {
                    div().into_any_element()
                } else {
                    ui::checkbox(
                        if enabled { "Enabled" } else { "Disabled" },
                        enabled,
                        move |ws| {
                            let id = id.clone();
                            ws.pending_plugin_toggle = Some((id, !enabled));
                        },
                        cx,
                    )
                    .into_any_element()
                })
                .into_any_element()
        })
        .collect();

    let body = div()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_size(px(11.0))
                .text_color(gpui::rgb(ui::TEXT_DIM))
                .child(format!("Plugins load from {dir}")),
        )
        .when(rows.is_empty(), |d| {
            d.child(
                div()
                    .text_size(px(12.0))
                    .py_2()
                    .child("No plugins installed yet."),
            )
        })
        .children(rows);

    let actions = div()
        .flex()
        .flex_row()
        .gap_2()
        .child(ui::button(
            "Install…",
            false,
            crate::keymap::install_plugin_dialog,
            cx,
        ))
        .child(ui::button(
            "Close",
            true,
            |ws, _w, cx| ws.close_modal(cx),
            cx,
        ));
    ui::modal_frame("Plugins", 420.0, body, actions)
}

use gpui::prelude::FluentBuilder as _;
