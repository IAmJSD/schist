//! File ▸ New: presets and the new-document dialog.

use super::*;

/// (label, width, height, ppi) rows of the File ▸ New preset dropdown,
/// matched against the dialog's current values to show which is selected.
pub(super) const NEW_DOC_PRESETS: &[(&str, u32, u32, f32)] = &[
    ("Default (1280 × 800)", 1280, 800, 72.0),
    ("HD (1920 × 1080)", 1920, 1080, 72.0),
    ("4K UHD (3840 × 2160)", 3840, 2160, 72.0),
    ("Square (1080 × 1080)", 1080, 1080, 72.0),
    ("A4, 300 ppi", 2480, 3508, 300.0),
    ("US Letter, 300 ppi", 2550, 3300, 300.0),
];

/// File ▸ New: the preset picker. One card per common size — a click
/// creates the document on the spot — and Custom… opens the full
/// dialog below for everything else.
pub(super) fn new_file_picker(cx: &mut Context<Workspace>) -> impl IntoElement {
    let mut cards = div().flex().flex_row().flex_wrap().gap_2();
    for &(label, width, height, ppi) in NEW_DOC_PRESETS {
        cards = cards.child(preset_card(label, width, height, ppi, cx));
    }
    cards = cards.child(
        div()
            .flex()
            .items_center()
            .justify_center()
            .w(px(150.0))
            .h(px(56.0))
            .rounded_md()
            .border_1()
            .border_color(gpui::rgb(ui::palette().edge))
            .text_size(px(12.0))
            .text_color(gpui::rgb(ui::palette().text_dim))
            .cursor_pointer()
            .hover(|s| s.border_color(gpui::rgb(ui::palette().accent)))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|ws, _e, _w, cx| {
                    ws.open_new_document_dialog(cx);
                }),
            )
            .child("Custom…"),
    );
    let actions = div().flex().flex_row().gap_2().child(ui::button(
        "Cancel",
        false,
        |ws, _w, cx| ws.close_modal(cx),
        cx,
    ));
    ui::modal_frame("New File", 520.0, cards, actions)
}

/// A preset card: click it and the document exists.
fn preset_card(
    label: &'static str,
    width: u32,
    height: u32,
    ppi: f32,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .justify_center()
        .w(px(150.0))
        .h(px(56.0))
        .px_3()
        .rounded_md()
        .bg(gpui::rgb(ui::palette().control_bg))
        .border_1()
        .border_color(gpui::rgb(ui::palette().edge))
        .cursor_pointer()
        .hover(|s| s.border_color(gpui::rgb(ui::palette().accent)))
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(move |ws, _e, _w, cx| {
                ws.close_modal(cx);
                ws.create_document(
                    "",
                    width,
                    height,
                    ppi,
                    ColorMode::Rgb,
                    Depth::Eight,
                    crate::workspace::NewDocBackground::White,
                );
                cx.notify();
            }),
        )
        .child(div().text_size(px(12.0)).child(label))
        .child(
            div()
                .text_size(px(10.0))
                .text_color(gpui::rgb(ui::palette().text_dim))
                .child(format!("{width} × {height} px")),
        )
}

/// The full dialog, asked before anything is created, as Photoshop does.
pub(super) fn new_document_dialog(
    state: &DialogState,
    modal: Modal,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let Modal::NewDocument {
        name,
        width,
        height,
        resolution,
        mode,
        depth,
        background,
    } = modal
    else {
        unreachable!("render dispatches only Modal::NewDocument here");
    };

    let name_focused = state.focused_field == Some("new-doc-name");
    let committed = name.clone();
    let shown_name = if name_focused && !state.field_buffer.is_empty() {
        state.field_buffer.clone()
    } else {
        name.clone()
    };

    let preset = NEW_DOC_PRESETS
        .iter()
        .position(|(_, w, h, r)| *w == width && *h == height && (*r - resolution).abs() < 0.5);
    let preset_label: SharedString = preset
        .map(|i| NEW_DOC_PRESETS[i].0.into())
        .unwrap_or_else(|| "Custom".into());
    let preset_options: Vec<(SharedString, usize)> = NEW_DOC_PRESETS
        .iter()
        .enumerate()
        .map(|(i, (label, ..))| (SharedString::from(*label), i))
        .collect();

    let depth_label = |d: Depth| match d {
        Depth::Eight => "8 bit",
        Depth::Sixteen => "16 bit",
        Depth::ThirtyTwo => "32 bit",
    };
    let mode_options: Vec<(SharedString, ColorMode)> = [
        ColorMode::Rgb,
        ColorMode::Grayscale,
        ColorMode::Cmyk,
        ColorMode::Lab,
    ]
    .into_iter()
    .map(|m| (SharedString::from(m.display_name()), m))
    .collect();
    let background_options: Vec<(SharedString, NewDocBackground)> = [
        NewDocBackground::White,
        NewDocBackground::BackgroundColor,
        NewDocBackground::Black,
        NewDocBackground::Transparent,
    ]
    .into_iter()
    .map(|b| (SharedString::from(b.display_name()), b))
    .collect();

    // Uncompressed pixel size, the way Photoshop's dialog reports it.
    let bytes = width as u64
        * height as u64
        * (mode.channels() as u64 + 1)
        * depth.bytes_per_channel() as u64;
    let size = if bytes < 1 << 20 {
        format!("{:.0} KB", bytes as f64 / (1u64 << 10) as f64)
    } else if bytes < 1 << 30 {
        format!("{:.1} MB", bytes as f64 / (1u64 << 20) as f64)
    } else {
        format!("{:.2} GB", bytes as f64 / (1u64 << 30) as f64)
    };

    let body = div()
        .flex()
        .flex_col()
        .gap_1()
        .child(ui::field_row(
            "Name",
            div()
                .w(px(200.0))
                .h(px(22.0))
                .px_1()
                .flex()
                .items_center()
                .rounded_sm()
                .bg(gpui::rgb(ui::palette().field_bg))
                .border_1()
                .border_color(gpui::rgb(if name_focused {
                    ui::palette().accent
                } else {
                    ui::palette().field_bg
                }))
                .text_size(px(12.0))
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(move |ws, _e, _w, cx| {
                        ws.focus_field("new-doc-name", committed.clone());
                        cx.notify();
                    }),
                )
                // A caret makes it obvious the field takes typing; it
                // blinks, and the arrows move it.
                .child(if name_focused {
                    let (before, after) = if state.field_buffer.is_empty() {
                        (shown_name.clone(), String::new())
                    } else {
                        let at = state.field_cursor.min(shown_name.len());
                        (shown_name[..at].to_string(), shown_name[at..].to_string())
                    };
                    ui::caret_run(before, after, state.caret_on, ui::palette().text)
                        .into_any_element()
                } else {
                    div().child(shown_name.clone()).into_any_element()
                }),
        ))
        .child(ui::field_row(
            "Preset",
            ui::dropdown(
                &state.dropdown_scroll,
                ui::Dropdown {
                    popup: Popup::Field("new-doc-preset"),
                    is_open: state.open_popup == Some(Popup::Field("new-doc-preset")),
                    current: preset.unwrap_or(usize::MAX),
                    label: preset_label,
                    width: 200.0,
                    options: preset_options,
                },
                |ws, index, _cx| {
                    let (_, w, h, r) = NEW_DOC_PRESETS[index];
                    ws.update_modal(|m| {
                        if let Modal::NewDocument {
                            width,
                            height,
                            resolution,
                            ..
                        } = m
                        {
                            *width = w;
                            *height = h;
                            *resolution = r;
                        }
                    });
                },
                cx,
            ),
        ))
        .child(ui::field_row(
            "Width",
            ui::num_field(
                ui::NumField {
                    id: "new-doc-w",
                    value: width as f32,
                    suffix: " px",
                    step: 10.0,
                    focused: state.focused_field == Some("new-doc-w"),
                    buffer: state.field_buffer.clone(),
                },
                |ws, delta| {
                    ws.update_modal(|m| {
                        if let Modal::NewDocument { width, .. } = m {
                            *width = (*width as f32 + delta).clamp(1.0, 30000.0) as u32;
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
                    id: "new-doc-h",
                    value: height as f32,
                    suffix: " px",
                    step: 10.0,
                    focused: state.focused_field == Some("new-doc-h"),
                    buffer: state.field_buffer.clone(),
                },
                |ws, delta| {
                    ws.update_modal(|m| {
                        if let Modal::NewDocument { height, .. } = m {
                            *height = (*height as f32 + delta).clamp(1.0, 30000.0) as u32;
                        }
                    });
                },
                cx,
            ),
        ))
        .child(ui::field_row(
            "Resolution",
            ui::num_field(
                ui::NumField {
                    id: "new-doc-dpi",
                    value: resolution,
                    suffix: " ppi",
                    step: 1.0,
                    focused: state.focused_field == Some("new-doc-dpi"),
                    buffer: state.field_buffer.clone(),
                },
                |ws, delta| {
                    ws.update_modal(|m| {
                        if let Modal::NewDocument { resolution, .. } = m {
                            *resolution = (*resolution + delta).max(1.0);
                        }
                    });
                },
                cx,
            ),
        ))
        .child(ui::field_row(
            "Color Mode",
            ui::dropdown(
                &state.dropdown_scroll,
                ui::Dropdown {
                    popup: Popup::Field("new-doc-mode"),
                    is_open: state.open_popup == Some(Popup::Field("new-doc-mode")),
                    current: mode,
                    label: (mode.display_name()).into(),
                    width: 150.0,
                    options: mode_options,
                },
                |ws, value, _cx| {
                    ws.update_modal(|m| {
                        if let Modal::NewDocument { mode, .. } = m {
                            *mode = value;
                        }
                    });
                },
                cx,
            ),
        ))
        .child(ui::field_row(
            "Bit Depth",
            ui::dropdown(
                &state.dropdown_scroll,
                ui::Dropdown {
                    popup: Popup::Field("new-doc-depth"),
                    is_open: state.open_popup == Some(Popup::Field("new-doc-depth")),
                    current: depth,
                    label: (depth_label(depth)).into(),
                    width: 150.0,
                    options: [Depth::Eight, Depth::Sixteen, Depth::ThirtyTwo]
                        .into_iter()
                        .map(|d| (SharedString::from(depth_label(d)), d))
                        .collect(),
                },
                |ws, value, _cx| {
                    ws.update_modal(|m| {
                        if let Modal::NewDocument { depth, .. } = m {
                            *depth = value;
                        }
                    });
                },
                cx,
            ),
        ))
        .child(ui::field_row(
            "Background",
            ui::dropdown(
                &state.dropdown_scroll,
                ui::Dropdown {
                    popup: Popup::Field("new-doc-bg"),
                    is_open: state.open_popup == Some(Popup::Field("new-doc-bg")),
                    current: background,
                    label: (background.display_name()).into(),
                    width: 150.0,
                    options: background_options,
                },
                |ws, value, _cx| {
                    ws.update_modal(|m| {
                        if let Modal::NewDocument { background, .. } = m {
                            *background = value;
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
                .child(format!(
                    "{width} × {height} px @ {resolution:.0} ppi · {size}"
                )),
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
            "Create",
            true,
            |ws, _w, cx| {
                let Some(Modal::NewDocument {
                    name,
                    width,
                    height,
                    resolution,
                    mode,
                    depth,
                    background,
                }) = ws.modal.clone()
                else {
                    return;
                };
                ws.close_modal(cx);
                ws.create_document(&name, width, height, resolution, mode, depth, background);
                ws.status = format!("New document: {width} × {height} px").into();
                cx.notify();
            },
            cx,
        ));

    ui::modal_frame("New Document", 400.0, body, actions)
}
