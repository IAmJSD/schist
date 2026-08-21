//! Modal dialogs: Image Size, Canvas Size, filters,
//! adjustments, export options and preferences.

use crate::ui;
use crate::workspace::{Modal, Popup, Workspace};
use gpui::{
    div, px, Context, InteractiveElement as _, IntoElement, ParentElement as _, SharedString,
    StatefulInteractiveElement as _, Styled as _,
};
use schist_core::Filter;

/// Workspace state the dialog widgets read while rendering.
#[derive(Clone)]
struct DialogState {
    open_popup: Option<Popup>,
    focused_field: Option<&'static str>,
    field_buffer: String,
}

/// Render whichever modal is open, if any.
pub fn render(ws: &mut Workspace, cx: &mut Context<Workspace>) -> Option<gpui::AnyElement> {
    let modal = ws.modal.clone()?;
    // Snapshot the bits of workspace state the widgets need: they render
    // inside `Workspace::render`, where reading the entity would panic.
    let state = DialogState {
        open_popup: ws.open_popup,
        focused_field: ws.focused_field,
        field_buffer: ws.field_buffer.clone(),
    };
    Some(match modal {
        Modal::ImageSize {
            width,
            height,
            filter,
            link,
        } => image_size(ws, &state, width, height, filter, link, cx).into_any_element(),
        Modal::CanvasSize {
            width,
            height,
            anchor,
        } => canvas_size(ws, &state, width, height, anchor, cx).into_any_element(),
        Modal::Filter {
            id,
            values,
            preview,
        } => filter_dialog(ws, &state, id, values, preview, cx).into_any_element(),
        Modal::Adjustment {
            layer,
            params,
            original,
        } => adjustment_dialog(ws, layer, params, original, cx).into_any_element(),
        Modal::LayerStyle {
            layer,
            style,
            active,
            ..
        } => crate::style_dialog::render(ws, layer, *style, active, cx).into_any_element(),
        Modal::SelectModify { kind, amount } => {
            modify_dialog(&state, kind, amount, cx).into_any_element()
        }
        Modal::ColorRange { tolerance, target } => {
            color_range_dialog(&state, tolerance, target, cx).into_any_element()
        }
        Modal::DestructiveAdjustment {
            kind,
            params,
            preview,
        } => {
            destructive_adjustment_dialog(ws, &state, kind, *params, preview, cx).into_any_element()
        }
        Modal::Stroke { width, position } => {
            stroke_dialog(ws, &state, width, position, cx).into_any_element()
        }
        Modal::Fill { source, opacity } => {
            fill_dialog(ws, &state, source, opacity, cx).into_any_element()
        }
        Modal::ContentAwareScale { width, height } => {
            content_aware_scale_dialog(&state, width, height, cx).into_any_element()
        }
        Modal::FilterGallery {
            stack,
            selected,
            preview,
        } => crate::gallery::render(ws, stack, selected, preview, cx).into_any_element(),
        Modal::ColorPicker {
            target,
            hsv,
            original,
        } => crate::color_picker::render(ws, target, hsv, original, cx).into_any_element(),
        Modal::ConfirmCloseTab => confirm_close_tab(ws, cx).into_any_element(),
        Modal::PluginManager => plugin_manager(ws, cx).into_any_element(),
        Modal::ModelManager => model_manager(ws, cx).into_any_element(),
        Modal::Preferences => preferences(ws, &state, cx).into_any_element(),
        Modal::LayerProperties { layer, name } => {
            layer_properties(&state, layer, name, cx).into_any_element()
        }
        Modal::Export { codec, options } => {
            export_dialog(ws, &state, codec, options, cx).into_any_element()
        }
        Modal::Profile { convert, selected } => {
            profile_dialog(&state, convert, selected, cx).into_any_element()
        }
    })
}

/// "Save changes before closing?" for the active tab. Save falls back to
/// the Save As dialog for never-saved documents; the tab then stays open
/// (now clean) rather than chaining a close onto an async file prompt.
fn confirm_close_tab(ws: &mut Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let title = ws
        .doc
        .as_ref()
        .map(|d| d.title.clone())
        .unwrap_or_else(|| "Untitled".into());
    ui::modal_frame(
        "Unsaved Changes",
        380.0,
        div().text_size(px(12.0)).child(format!(
            "Save changes to \u{201C}{title}\u{201D} before closing?"
        )),
        div()
            .flex()
            .flex_row()
            .gap_2()
            .child(ui::button(
                "Don't Save",
                false,
                |ws, _window, cx| {
                    ws.close_modal(cx);
                    let index = ws.active_tab();
                    ws.close_tab(index, cx);
                },
                cx,
            ))
            .child(ui::button(
                "Cancel",
                false,
                |ws, _window, cx| ws.close_modal(cx),
                cx,
            ))
            .child(ui::button(
                "Save…",
                true,
                |ws, window, cx| {
                    ws.close_modal(cx);
                    ws.save_current(window, cx);
                    // The synchronous path (a known, writable path) leaves
                    // the document clean; only then is closing safe.
                    if ws.doc.as_ref().is_some_and(|d| !d.dirty) {
                        let index = ws.active_tab();
                        ws.close_tab(index, cx);
                    }
                },
                cx,
            )),
    )
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
    state: &DialogState,
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
                ui::Dropdown {
                    popup: Popup::Field("image-size-filter"),
                    is_open: state.open_popup == Some(Popup::Field("image-size-filter")),
                    current: filter,
                    label: (filter.display_name()).into(),
                    width: 150.0,
                    options: filter_options(),
                },
                |ws, value, _cx| {
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
                    schist_tools_transform::resize_image(doc, width, height, filter);
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

/// One slider row's description, shared by the filter and adjustment
/// dialogs (both render the same control from the same shape).
#[derive(Default)]
pub(crate) struct SliderSpec {
    pub(crate) id: &'static str,
    pub(crate) label: &'static str,
    pub(crate) value: f32,
    pub(crate) min: f32,
    pub(crate) max: f32,
    pub(crate) suffix: &'static str,
    /// If set, the value is an index into these and the row shows the
    /// name rather than the number. The slider snaps to whole steps.
    pub(crate) choices: &'static [&'static str],
}

/// A labelled slider row used by the filter and adjustment dialogs.
pub(crate) fn param_slider(
    spec: SliderSpec,
    on_change: impl Fn(&mut Workspace, f32, &mut Context<Workspace>) + Clone + 'static,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let SliderSpec {
        id,
        label,
        value,
        min,
        max,
        suffix,
        choices,
    } = spec;
    let span = (max - min).max(1e-6);
    let ratio = ((value - min) / span).clamp(0.0, 1.0);
    let display = match choices.get((value.round().max(0.0) as usize).min(choices.len())) {
        Some(name) => (*name).to_string(),
        None if max - min > 20.0 => format!("{value:.0}{suffix}"),
        None => format!("{value:.2}{suffix}"),
    };
    let snap = !choices.is_empty();
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
                move |ws, r, cx| {
                    let v = min + r * span;
                    set(ws, if snap { v.round() } else { v }, cx)
                },
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

#[allow(clippy::too_many_arguments)]
fn filter_dialog(
    ws: &mut Workspace,
    _state: &DialogState,
    id: &'static str,
    values: schist_plugin_api::FilterValues,
    preview: bool,
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
                choices: spec.choices,
            },
            move |ws, v, cx| {
                let mut next = None;
                ws.update_modal(|m| {
                    if let Modal::Filter {
                        values, preview, ..
                    } = m
                    {
                        values.set(key, v);
                        if *preview {
                            next = Some(values.clone());
                        }
                    }
                });
                if let Some(values) = next {
                    ws.preview_filter(id, Some(&values), cx);
                }
            },
            cx,
        ));
    }
    // Anything the filter wants the user to know before running it --
    // for the neural ones, whether they found their model.
    if let Some(note) = ws
        .registry
        .filters()
        .find(|f| f.id() == id)
        .and_then(|f| f.info())
    {
        body = body.child(
            div()
                .text_size(px(11.0))
                .text_color(gpui::rgb(ui::TEXT_DIM))
                .child(SharedString::from(note)),
        );
    }
    body = body
        .child(ui::checkbox(
            "Preview",
            preview,
            move |ws, cx| {
                let mut next = None;
                ws.update_modal(|m| {
                    if let Modal::Filter {
                        values, preview, ..
                    } = m
                    {
                        *preview = !*preview;
                        next = Some((*preview, values.clone()));
                    }
                });
                match next {
                    Some((true, values)) => ws.preview_filter(id, Some(&values), cx),
                    // Unticking shows the untouched pixels again.
                    Some((false, _)) => ws.preview_filter(id, None, cx),
                    None => {}
                }
            },
            cx,
        ))
        .child(
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

/// Image ▸ Adjustments: the same sliders as the adjustment layers, but
/// previewing writes pixels and OK bakes them in.
fn destructive_adjustment_dialog(
    ws: &mut Workspace,
    _state: &DialogState,
    kind: schist_core::AdjustmentKind,
    params: schist_adjustments::Params,
    preview: bool,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let specs = params.param_specs();
    // Curves has no sliders: it needs a graph.
    let curves = matches!(params, schist_adjustments::Params::Curves(_));
    let mut body = div()
        .id("destructive-adjust-body")
        .flex()
        .flex_col()
        .gap_1()
        .max_h(px(430.0))
        .overflow_y_scroll();
    if curves {
        body = body.child(crate::curve_editor::render(ws, cx));
    }
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
                ..Default::default()
            },
            move |ws, v, cx| {
                let mut next = None;
                ws.update_modal(|m| {
                    if let Modal::DestructiveAdjustment {
                        params, preview, ..
                    } = m
                    {
                        params.set_param(key, v);
                        if *preview {
                            next = Some((**params).clone());
                        }
                    }
                });
                if let Some(params) = next {
                    ws.preview_destructive_adjustment(Some(&params), cx);
                }
            },
            cx,
        ));
    }
    body = body.child(ui::checkbox(
        "Preview",
        preview,
        move |ws, cx| {
            let mut next = None;
            ws.update_modal(|m| {
                if let Modal::DestructiveAdjustment {
                    params, preview, ..
                } = m
                {
                    *preview = !*preview;
                    next = Some((*preview, (**params).clone()));
                }
            });
            match next {
                Some((true, p)) => ws.preview_destructive_adjustment(Some(&p), cx),
                Some((false, _)) => ws.preview_destructive_adjustment(None, cx),
                None => {}
            }
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
                    if let Modal::DestructiveAdjustment { kind, params, .. } = m {
                        run = Some((*kind, (**params).clone()));
                    }
                });
                ws.modal = None;
                if let Some((kind, params)) = run {
                    ws.commit_destructive_adjustment(kind, &params, cx);
                }
                cx.notify();
            },
            cx,
        ));
    ui::modal_frame(
        kind.display_name(),
        if curves { 430.0 } else { 380.0 },
        body,
        actions,
    )
}

/// Filter ▸ Neural Filters ▸ Manage Models.
///
/// The style-transfer networks are megabytes each and are somebody else's
/// work, so they are fetched here rather than shipped. The one that was
/// trained for this application is built in and cannot be removed.
fn model_manager(ws: &Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let downloading = ws.model_downloads.clone();
    let rows: Vec<gpui::AnyElement> = schist_neural::CATALOG
        .iter()
        .map(|spec| {
            let id = spec.id;
            let installed = schist_neural::installed(id);
            let busy = downloading.contains(&id);
            let size = schist_neural::installed_size(spec)
                .map(|b| format!("{:.1} MB", b as f64 / (1 << 20) as f64))
                .unwrap_or_else(|| format!("{:.1} MB", spec.bytes as f64 / (1 << 20) as f64));
            let state = if spec.built_in() {
                "Built in".to_string()
            } else if busy {
                "Downloading\u{2026}".to_string()
            } else if installed {
                format!("Installed \u{b7} {size}")
            } else {
                format!("Not installed \u{b7} {size}")
            };
            let action: gpui::AnyElement = if spec.built_in() {
                div().into_any_element()
            } else if busy {
                div()
                    .text_size(px(11.0))
                    .text_color(gpui::rgb(ui::TEXT_DIM))
                    .child("\u{2026}")
                    .into_any_element()
            } else if installed {
                ui::button(
                    "Remove",
                    false,
                    move |ws, _w, cx| ws.remove_model(id, cx),
                    cx,
                )
                .into_any_element()
            } else {
                ui::button(
                    "Download",
                    true,
                    move |ws, _w, cx| ws.download_model(id, cx),
                    cx,
                )
                .into_any_element()
            };
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .py_1()
                .child(
                    // Fixed rather than flex-grown: the notes are long
                    // enough to need wrapping, and a grown column sizes to
                    // its content and gets clipped instead.
                    div()
                        .flex()
                        .flex_col()
                        .w(px(412.0))
                        .flex_none()
                        .child(
                            div()
                                .text_size(px(12.0))
                                .child(SharedString::from(spec.name)),
                        )
                        .child(
                            div()
                                .text_size(px(11.0))
                                .text_color(gpui::rgb(ui::TEXT_DIM))
                                .child(SharedString::from(format!(
                                    "{state} \u{b7} {}",
                                    spec.license
                                ))),
                        )
                        .child(
                            div()
                                .text_size(px(11.0))
                                .text_color(gpui::rgb(ui::TEXT_DIM))
                                .child(SharedString::from(spec.note)),
                        ),
                )
                .child(action)
                .into_any_element()
        })
        .collect();

    let body = div()
        .id("model-manager-body")
        .flex()
        .flex_col()
        .gap_1()
        .w(px(520.0))
        .max_h(px(360.0))
        .overflow_y_scroll()
        .children(rows)
        .child(
            div()
                .pt_2()
                .text_size(px(11.0))
                .text_color(gpui::rgb(ui::TEXT_DIM))
                .child(SharedString::from(format!(
                    "Models are kept in {}. Filters that have no model fall \
                     back to signal processing and say so.",
                    schist_neural::model_dir().display()
                ))),
        );
    let actions = div().flex().flex_row().gap_2().child(ui::button(
        "Close",
        true,
        |ws, _w, cx| ws.close_modal(cx),
        cx,
    ));
    ui::modal_frame("Neural Filter Models", 560.0, body, actions)
}

/// Edit ▸ Content-Aware Scale.
fn content_aware_scale_dialog(
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
                            *width = v.max(1.0) as u32;
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
                            *height = v.max(1.0) as u32;
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
fn stroke_dialog(
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
                .text_color(gpui::rgb(ui::TEXT_DIM))
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
fn fill_dialog(
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
fn modify_dialog(
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
fn color_range_dialog(
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
                        .size(px(18.0))
                        .rounded_sm()
                        .border_1()
                        .border_color(gpui::rgb(0x3A3A3A))
                        .bg(gpui::rgb(swatch)),
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
                .text_color(gpui::rgb(ui::TEXT_DIM))
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

fn adjustment_dialog(
    ws: &mut Workspace,
    layer: schist_core::LayerId,
    params: schist_adjustments::Params,
    original: (Option<String>, Vec<u8>),
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let specs = params.param_specs();
    let title = params.display_name().to_string();
    let curves = matches!(params, schist_adjustments::Params::Curves(_));
    let mut body = div().flex().flex_col().gap_1();
    if curves {
        body = body.child(crate::curve_editor::render(ws, cx));
    }
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
                ..Default::default()
            },
            move |ws, v, _cx| {
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
    ui::modal_frame(title, if curves { 430.0 } else { 360.0 }, body, actions)
}

/// The third-party plugin manager: what loaded, what didn't and why, and
/// per-plugin enable/disable.
fn plugin_manager(ws: &mut Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let dir = schist_plugin_host_wasm::PluginManager::plugin_dir()
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
                Some(schist_plugin_host_wasm::abi::PluginKind::Filter) => "filter",
                Some(schist_plugin_host_wasm::abi::PluginKind::Codec) => "format",
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
                        move |ws, _cx| {
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

fn export_dialog(
    ws: &mut Workspace,
    state: &DialogState,
    codec_id: &'static str,
    options: schist_plugin_api::ExportOptions,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let codecs: Vec<(SharedString, &'static str)> = ws
        .registry
        .codecs()
        .filter(|c| c.can_export())
        .map(|c| (SharedString::from(c.name().to_string()), c.id()))
        .collect();
    let current_name = codecs
        .iter()
        .find(|(_, id)| *id == codec_id)
        .map(|(n, _)| n.clone())
        .unwrap_or_else(|| "PNG".into());
    let supports_quality = ws
        .registry
        .codecs()
        .find(|c| c.id() == codec_id)
        .map(|c| c.supports_quality())
        .unwrap_or(false);

    let mut body = div().flex().flex_col().gap_1().child(ui::field_row(
        "Format",
        ui::dropdown(
            ui::Dropdown {
                popup: Popup::Field("export-format"),
                is_open: state.open_popup == Some(Popup::Field("export-format")),
                current: codec_id,
                label: (current_name),
                width: 150.0,
                options: codecs,
            },
            |ws, value, _cx| {
                ws.update_modal(|m| {
                    if let Modal::Export { codec, .. } = m {
                        *codec = value;
                    }
                });
            },
            cx,
        ),
    ));
    if supports_quality {
        body = body.child(param_slider(
            SliderSpec {
                id: "export-quality",
                label: "Quality",
                value: options.quality as f32,
                min: 1.0,
                max: 100.0,
                suffix: "",
                ..Default::default()
            },
            |ws, v, _cx| {
                ws.update_modal(|m| {
                    if let Modal::Export { options, .. } = m {
                        options.quality = v.clamp(1.0, 100.0) as u8;
                    }
                });
            },
            cx,
        ));
    }
    body = body.child(ui::field_row(
        "Dither",
        ui::checkbox(
            "Dither when reducing to 8-bit",
            options.dither,
            |ws, _cx| {
                ws.update_modal(|m| {
                    if let Modal::Export { options, .. } = m {
                        options.dither = !options.dither;
                    }
                });
            },
            cx,
        ),
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
            "Export…",
            true,
            move |ws, window, cx| {
                ws.close_modal(cx);
                ws.export_with(codec_id, options, window, cx);
            },
            cx,
        ));
    ui::modal_frame("Export", 360.0, body, actions)
}

fn profile_dialog(
    state: &DialogState,
    convert: bool,
    selected: usize,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let builtins = schist_colormgmt::Profile::builtins();
    let options: Vec<(SharedString, usize)> = builtins
        .iter()
        .enumerate()
        .map(|(i, (name, _))| (SharedString::from(*name), i))
        .collect();
    let current = builtins
        .get(selected)
        .map(|(n, _)| SharedString::from(*n))
        .unwrap_or_else(|| "sRGB".into());

    let explanation = if convert {
        "Rewrites pixel values so colours keep their appearance."
    } else {
        "Reinterprets the existing pixel values under the new profile."
    };
    let body = div()
        .flex()
        .flex_col()
        .gap_1()
        .child(ui::field_row(
            "Profile",
            ui::dropdown(
                ui::Dropdown {
                    popup: Popup::Field("profile-pick"),
                    is_open: state.open_popup == Some(Popup::Field("profile-pick")),
                    current: selected,
                    label: (current),
                    width: 150.0,
                    options,
                },
                |ws, value, _cx| {
                    ws.update_modal(|m| {
                        if let Modal::Profile { selected, .. } = m {
                            *selected = value;
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
                .child(explanation),
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
            if convert { "Convert" } else { "Assign" },
            true,
            move |ws, _w, cx| {
                let profile = schist_colormgmt::Profile::builtins()
                    .get(selected)
                    .map(|(_, make)| make())
                    .unwrap_or_else(schist_colormgmt::Profile::srgb);
                if convert {
                    ws.convert_to_profile(profile, cx);
                } else {
                    ws.assign_profile(profile, cx);
                }
                ws.close_modal(cx);
            },
            cx,
        ));
    ui::modal_frame(
        if convert {
            "Convert to Profile"
        } else {
            "Assign Profile"
        },
        340.0,
        body,
        actions,
    )
}

/// Application preferences (⌘K).
fn preferences(
    ws: &mut Workspace,
    state: &DialogState,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let view = ws.view;
    let intent = ws.color.intent;
    let keymap_path = crate::keymap::user_keymap_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "(no config directory)".into());

    let body = div()
        .flex()
        .flex_col()
        .gap_1()
        .child(ui::field_row(
            "Theme",
            ui::dropdown(
                ui::Dropdown {
                    popup: Popup::Field("pref-theme"),
                    is_open: state.open_popup == Some(Popup::Field("pref-theme")),
                    current: view.theme,
                    label: (view.theme.display_name()).into(),
                    width: 150.0,
                    options: vec![
                        ("Dark".into(), crate::workspace::Theme::Dark),
                        ("Light".into(), crate::workspace::Theme::Light),
                    ],
                },
                |ws, theme, _cx| ws.set_theme_quiet(theme),
                cx,
            ),
        ))
        .child(ui::field_row(
            "Grid spacing",
            ui::num_field(
                ui::NumField {
                    id: "pref-grid",
                    value: view.grid_spacing,
                    suffix: " px",
                    step: 8.0,
                    focused: state.focused_field == Some("pref-grid"),
                    buffer: state.field_buffer.clone(),
                },
                |ws, delta| {
                    ws.view.grid_spacing = (ws.view.grid_spacing + delta).clamp(2.0, 1024.0);
                    ws.save_view_options();
                },
                cx,
            ),
        ))
        .child(ui::field_row(
            "Snapping",
            ui::checkbox(
                "Snap to guides, grid and canvas edges",
                view.snap,
                |ws, _cx| {
                    ws.view.snap = !ws.view.snap;
                    ws.save_view_options();
                },
                cx,
            ),
        ))
        .child(ui::field_row(
            "Rendering intent",
            ui::dropdown(
                ui::Dropdown {
                    popup: Popup::Field("pref-intent"),
                    is_open: state.open_popup == Some(Popup::Field("pref-intent")),
                    current: intent,
                    label: (intent.display_name()).into(),
                    width: 180.0,
                    options: schist_colormgmt::Intent::all()
                        .iter()
                        .map(|i| (SharedString::from(i.display_name()), *i))
                        .collect(),
                },
                |ws, value, _cx| {
                    ws.color.intent = value;
                    ws.rebuild_color_transforms();
                },
                cx,
            ),
        ))
        .child(ui::field_row(
            "Scrolling",
            ui::checkbox(
                "Zoom with scroll wheel",
                view.zoom_with_scroll,
                |ws, _cx| {
                    ws.view.zoom_with_scroll = !ws.view.zoom_with_scroll;
                    ws.save_view_options();
                },
                cx,
            ),
        ))
        .child(ui::field_row(
            "Diagnostics",
            ui::checkbox(
                "Write a local crash report on panic",
                view.crash_reports,
                |ws, _cx| {
                    ws.view.crash_reports = !ws.view.crash_reports;
                    ws.save_view_options();
                },
                cx,
            ),
        ))
        .child(
            div()
                .text_size(px(11.0))
                .text_color(gpui::rgb(ui::TEXT_DIM))
                .child(format!("Keyboard shortcuts: {keymap_path}")),
        )
        .child(
            div()
                .text_size(px(11.0))
                .text_color(gpui::rgb(ui::TEXT_DIM))
                .child(format!("Version {}", crate::crash::current_version())),
        );

    let actions = div().flex().flex_row().gap_2().child(ui::button(
        "Done",
        true,
        |ws, _w, cx| {
            ws.save_view_options();
            ws.close_modal(cx);
        },
        cx,
    ));
    ui::modal_frame("Preferences", 400.0, body, actions)
}

/// Layer Properties: rename a layer.
fn layer_properties(
    state: &DialogState,
    layer: schist_core::LayerId,
    name: String,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let focused = state.focused_field == Some("layer-name");
    let shown = if focused && !state.field_buffer.is_empty() {
        state.field_buffer.clone()
    } else {
        name.clone()
    };
    let body = ui::field_row(
        "Name",
        div()
            .w(px(200.0))
            .h(px(22.0))
            .px_1()
            .flex()
            .items_center()
            .rounded_sm()
            .bg(gpui::rgb(ui::FIELD_BG))
            .border_1()
            .border_color(gpui::rgb(if focused { ui::ACCENT } else { ui::FIELD_BG }))
            .text_size(px(12.0))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|ws, _e, _w, cx| {
                    ws.focus_field("layer-name");
                    cx.notify();
                }),
            )
            // A caret makes it obvious the field takes typing.
            .child(if focused {
                format!("{shown}|")
            } else {
                shown.clone()
            }),
    );

    let committed = name;
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
                let name = if ws.field_buffer.is_empty() {
                    committed.clone()
                } else {
                    ws.field_buffer.clone()
                };
                ws.rename_layer(layer, name, cx);
                ws.close_modal(cx);
            },
            cx,
        ));
    ui::modal_frame("Layer Properties", 340.0, body, actions)
}
