//! Save one gallery photo as a flat image, in a chosen format and at a
//! chosen size.

use super::*;
use std::path::PathBuf;

#[allow(clippy::too_many_arguments)]
pub(super) fn save_image_dialog(
    ws: &mut Workspace,
    state: &DialogState,
    path: PathBuf,
    codec_id: &'static str,
    options: schist_plugin_api::ExportOptions,
    scale: f32,
    size: Option<(u32, u32)>,
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
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();

    let mut body = div()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_size(px(11.0))
                .text_color(gpui::rgb(ui::palette().text_dim))
                .pb_1()
                .child(SharedString::from(format!(
                    "{name} — the edit, if it has one; the original is never touched."
                ))),
        )
        .child(ui::field_row(
            "Format",
            ui::dropdown(
                &ws.dropdown_scroll,
                ui::Dropdown {
                    popup: Popup::Field("save-image-format"),
                    is_open: state.open_popup == Some(Popup::Field("save-image-format")),
                    current: codec_id,
                    label: current_name,
                    width: 150.0,
                    options: codecs,
                },
                |ws, value, _cx| {
                    ws.update_modal(|m| {
                        if let Modal::SaveImageAs { codec, .. } = m {
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
                id: "save-image-quality",
                label: "Quality",
                value: options.quality as f32,
                min: 1.0,
                max: 100.0,
                suffix: "",
                ..Default::default()
            },
            |ws, v, _cx| {
                ws.update_modal(|m| {
                    if let Modal::SaveImageAs { options, .. } = m {
                        options.quality = v.clamp(1.0, 100.0) as u8;
                    }
                });
            },
            cx,
        ));
    }
    // The scale, said in pixels where the source size is known: "50%"
    // means little; "1512 × 2016" is a decision.
    let percent = (scale * 100.0).round();
    let scale_label = match size {
        Some((w, h)) => format!(
            "{}% \u{2192} {} \u{d7} {}",
            percent,
            ((w as f32 * scale).round() as u32).max(1),
            ((h as f32 * scale).round() as u32).max(1)
        ),
        None => format!("{percent}%"),
    };
    body = body
        .child(param_slider(
            SliderSpec {
                id: "save-image-scale",
                label: "Scale",
                value: percent,
                min: 10.0,
                max: 100.0,
                suffix: "%",
                ..Default::default()
            },
            |ws, v, _cx| {
                ws.update_modal(|m| {
                    if let Modal::SaveImageAs { scale, .. } = m {
                        *scale = (v.round() / 100.0).clamp(0.1, 1.0);
                    }
                });
            },
            cx,
        ))
        .child(
            div()
                .pl(px(96.0))
                .text_size(px(11.0))
                .text_color(gpui::rgb(ui::palette().text_dim))
                .child(SharedString::from(scale_label)),
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
            "Save…",
            true,
            move |ws, window, cx| {
                let Some(Modal::SaveImageAs {
                    path,
                    codec,
                    options,
                    scale,
                    ..
                }) = ws.modal.clone()
                else {
                    return;
                };
                ws.close_modal(cx);
                ws.save_photo_as(path, codec, options, scale, window, cx);
            },
            cx,
        ));
    ui::modal_frame("Save Image As", 380.0, body, actions)
}
