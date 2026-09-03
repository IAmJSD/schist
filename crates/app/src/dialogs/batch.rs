//! The gallery's batch dialog: one recipe — turn, upscale, colour —
//! run over every selected photo (or a whole bucket), written as
//! versioned gallery edits or as flat copies.

use super::*;
use crate::workspace::{BatchRecipe, BatchTarget, CanvasTransform};
use schist_core::AdjustmentKind;
use std::path::PathBuf;

/// The adjustments on offer, in the order Photoshop's menu lists the
/// ones it has. Curves needs its editor and the fills paint over the
/// photo, so neither belongs in a batch.
const KINDS: &[AdjustmentKind] = &[
    AdjustmentKind::BrightnessContrast,
    AdjustmentKind::Levels,
    AdjustmentKind::Exposure,
    AdjustmentKind::Vibrance,
    AdjustmentKind::HueSaturation,
    AdjustmentKind::ColorBalance,
    AdjustmentKind::BlackWhite,
    AdjustmentKind::PhotoFilter,
    AdjustmentKind::ChannelMixer,
    AdjustmentKind::Invert,
    AdjustmentKind::Posterize,
    AdjustmentKind::Threshold,
];

/// Popup ids for each adjustment step's kind dropdown — a popup is
/// keyed by a static string, so the steps are capped at this many.
const STEP_POPUPS: [&str; 6] = [
    "batch-adj-0",
    "batch-adj-1",
    "batch-adj-2",
    "batch-adj-3",
    "batch-adj-4",
    "batch-adj-5",
];

/// The upscalers on offer: the two that ship inside the binary, so a
/// batch never stalls on a download.
const UPSCALERS: &[&str] = &["waifu2x-photo", "waifu2x-art"];

fn section(title: &'static str) -> impl IntoElement {
    div()
        .pt_1()
        .text_size(px(11.0))
        .text_color(gpui::rgb(ui::palette().text_dim))
        .child(title)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn batch_dialog(
    ws: &mut Workspace,
    state: &DialogState,
    photos: Vec<PathBuf>,
    recipe: BatchRecipe,
    target: BatchTarget,
    codec_id: &'static str,
    options: schist_plugin_api::ExportOptions,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let n = photos.len();
    let edited = photos
        .iter()
        .filter(|p| schist_gallery::backing_psd(p).is_some_and(|s| s.exists()))
        .count();
    let note = match (target, edited) {
        (BatchTarget::Edit, 0) => format!(
            "{n} photo{} — saved as gallery edits; the originals are never touched.",
            if n == 1 { "" } else { "s" }
        ),
        (BatchTarget::Edit, _) => format!(
            "{n} photo{}, {edited} already edited — the recipe goes on top of the edit, \
             and the previous edit is kept as a version.",
            if n == 1 { "" } else { "s" }
        ),
        (_, 0) => format!(
            "{n} photo{} — flat copies; the originals are never touched.",
            if n == 1 { "" } else { "s" }
        ),
        (_, _) => format!(
            "{n} photo{}, {edited} already edited — the copies are made from the edits.",
            if n == 1 { "" } else { "s" }
        ),
    };

    let mut body = div().flex().flex_col().gap_1().child(
        div()
            .text_size(px(11.0))
            .text_color(gpui::rgb(ui::palette().text_dim))
            .pb_1()
            .child(SharedString::from(note)),
    );

    // Turn.
    let rotations: Vec<(SharedString, Option<CanvasTransform>)> = vec![
        ("None".into(), None),
        ("90\u{b0} Clockwise".into(), Some(CanvasTransform::Cw90)),
        (
            "90\u{b0} Counter Clockwise".into(),
            Some(CanvasTransform::Ccw90),
        ),
        ("180\u{b0}".into(), Some(CanvasTransform::Rotate180)),
    ];
    let rotate_label = rotations
        .iter()
        .find(|(_, r)| *r == recipe.rotate)
        .map(|(l, _)| l.clone())
        .unwrap_or_else(|| "None".into());
    body = body
        .child(section("Turn"))
        .child(ui::field_row(
            "Rotate",
            ui::dropdown(
                &ws.dropdown,
                ui::Dropdown {
                    popup: Popup::Field("batch-rotate"),
                    is_open: state.open_popup == Some(Popup::Field("batch-rotate")),
                    current: recipe.rotate,
                    label: rotate_label,
                    width: 190.0,
                    options: rotations,
                },
                |ws, value, _cx| {
                    ws.update_modal(|m| {
                        if let Modal::BatchProcess { recipe, .. } = m {
                            recipe.rotate = value;
                        }
                    });
                },
                cx,
            ),
        ))
        .child(ui::field_row(
            "Flip",
            div()
                .flex()
                .flex_row()
                .gap_4()
                .child(ui::checkbox(
                    "Horizontal",
                    recipe.flip_h,
                    |ws, _cx| {
                        ws.update_modal(|m| {
                            if let Modal::BatchProcess { recipe, .. } = m {
                                recipe.flip_h = !recipe.flip_h;
                            }
                        });
                    },
                    cx,
                ))
                .child(ui::checkbox(
                    "Vertical",
                    recipe.flip_v,
                    |ws, _cx| {
                        ws.update_modal(|m| {
                            if let Modal::BatchProcess { recipe, .. } = m {
                                recipe.flip_v = !recipe.flip_v;
                            }
                        });
                    },
                    cx,
                )),
        ));

    // Size.
    let mut upscalers: Vec<(SharedString, Option<&'static str>)> = vec![("None".into(), None)];
    upscalers.extend(UPSCALERS.iter().map(|id| {
        (
            SharedString::from(schist_tools_transform::Resample::Neural(id).display_name()),
            Some(*id),
        )
    }));
    let upscale_label = upscalers
        .iter()
        .find(|(_, u)| *u == recipe.upscale)
        .map(|(l, _)| l.clone())
        .unwrap_or_else(|| "None".into());
    body = body.child(section("Size")).child(ui::field_row(
        "Upscale",
        ui::dropdown(
            &ws.dropdown,
            ui::Dropdown {
                popup: Popup::Field("batch-upscale"),
                is_open: state.open_popup == Some(Popup::Field("batch-upscale")),
                current: recipe.upscale,
                label: upscale_label,
                width: 190.0,
                options: upscalers,
            },
            |ws, value, _cx| {
                ws.update_modal(|m| {
                    if let Modal::BatchProcess { recipe, .. } = m {
                        recipe.upscale = value;
                    }
                });
            },
            cx,
        ),
    ));

    // Colour: each step is an adjustment layer, its sliders under it.
    body = body.child(section("Colour"));
    let kind_options: Vec<(SharedString, AdjustmentKind)> = KINDS
        .iter()
        .map(|k| (SharedString::from(k.display_name()), *k))
        .collect();
    for (i, params) in recipe.adjustments.iter().enumerate() {
        let Some(popup) = STEP_POPUPS.get(i).copied() else {
            break;
        };
        let kind = params.kind();
        body = body.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(ui::dropdown(
                    &ws.dropdown,
                    ui::Dropdown {
                        popup: Popup::Field(popup),
                        is_open: state.open_popup == Some(Popup::Field(popup)),
                        current: kind,
                        label: kind.display_name().into(),
                        width: 190.0,
                        options: kind_options.clone(),
                    },
                    move |ws, value, _cx| {
                        ws.update_modal(|m| {
                            if let Modal::BatchProcess { recipe, .. } = m {
                                if let Some(step) = recipe.adjustments.get_mut(i) {
                                    if step.kind() != value {
                                        *step = schist_adjustments::Params::default_for(value);
                                    }
                                }
                            }
                        });
                    },
                    cx,
                ))
                .child(ui::button(
                    "Remove",
                    false,
                    move |ws, _w, _cx| {
                        ws.update_modal(|m| {
                            if let Modal::BatchProcess { recipe, .. } = m {
                                if i < recipe.adjustments.len() {
                                    recipe.adjustments.remove(i);
                                }
                            }
                        });
                    },
                    cx,
                )),
        );
        for spec in params.param_specs() {
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
                    ws.update_modal(|m| {
                        if let Modal::BatchProcess { recipe, .. } = m {
                            if let Some(step) = recipe.adjustments.get_mut(i) {
                                step.set_param(key, v);
                            }
                        }
                    });
                },
                cx,
            ));
        }
    }
    if recipe.adjustments.len() < STEP_POPUPS.len() {
        body = body.child(ui::field_row(
            "Add",
            ui::dropdown(
                &ws.dropdown,
                ui::Dropdown {
                    popup: Popup::Field("batch-adj-add"),
                    is_open: state.open_popup == Some(Popup::Field("batch-adj-add")),
                    current: None,
                    label: "Adjustment\u{2026}".into(),
                    width: 190.0,
                    options: kind_options
                        .iter()
                        .map(|(l, k)| (l.clone(), Some(*k)))
                        .collect(),
                },
                |ws, value, _cx| {
                    let Some(kind) = value else { return };
                    ws.update_modal(|m| {
                        if let Modal::BatchProcess { recipe, .. } = m {
                            recipe
                                .adjustments
                                .push(schist_adjustments::Params::default_for(kind));
                        }
                    });
                },
                cx,
            ),
        ));
    }

    // Output.
    let targets: Vec<(SharedString, BatchTarget)> =
        [BatchTarget::Edit, BatchTarget::Beside, BatchTarget::Folder]
            .into_iter()
            .map(|t| (SharedString::from(t.label()), t))
            .collect();
    body = body.child(section("Output")).child(ui::field_row(
        "Save as",
        ui::dropdown(
            &ws.dropdown,
            ui::Dropdown {
                popup: Popup::Field("batch-target"),
                is_open: state.open_popup == Some(Popup::Field("batch-target")),
                current: target,
                label: target.label().into(),
                width: 190.0,
                options: targets,
            },
            |ws, value, _cx| {
                ws.update_modal(|m| {
                    if let Modal::BatchProcess { target, .. } = m {
                        *target = value;
                    }
                });
            },
            cx,
        ),
    ));
    if target != BatchTarget::Edit {
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
        body = body.child(ui::field_row(
            "Format",
            ui::dropdown(
                &ws.dropdown,
                ui::Dropdown {
                    popup: Popup::Field("batch-format"),
                    is_open: state.open_popup == Some(Popup::Field("batch-format")),
                    current: codec_id,
                    label: current_name,
                    width: 190.0,
                    options: codecs,
                },
                |ws, value, _cx| {
                    ws.update_modal(|m| {
                        if let Modal::BatchProcess { codec, .. } = m {
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
                    id: "batch-quality",
                    label: "Quality",
                    value: options.quality as f32,
                    min: 1.0,
                    max: 100.0,
                    suffix: "",
                    ..Default::default()
                },
                |ws, v, _cx| {
                    ws.update_modal(|m| {
                        if let Modal::BatchProcess { options, .. } = m {
                            options.quality = v.clamp(1.0, 100.0) as u8;
                        }
                    });
                },
                cx,
            ));
        }
    }

    let run_label = if n == 1 {
        "Process".to_string()
    } else {
        format!("Process {n}")
    };
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
            run_label,
            true,
            move |ws, window, cx| {
                let Some(Modal::BatchProcess {
                    photos,
                    recipe,
                    target,
                    codec,
                    options,
                }) = ws.modal.clone()
                else {
                    return;
                };
                if recipe.is_empty() {
                    ws.status =
                        "Nothing to do: pick a turn, an upscale or an adjustment first".into();
                    cx.notify();
                    return;
                }
                ws.close_modal(cx);
                ws.run_batch(photos, recipe, target, codec, options, window, cx);
            },
            cx,
        ));
    ui::modal_frame("Process Photos", 440.0, body, actions)
}
