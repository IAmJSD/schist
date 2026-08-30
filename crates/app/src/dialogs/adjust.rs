//! The adjustment-layer parameter dialog.

use super::*;

pub(super) fn adjustment_dialog(
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
