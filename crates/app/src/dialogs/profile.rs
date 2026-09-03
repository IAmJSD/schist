//! Assigning and converting colour profiles.

use super::*;

pub(super) fn profile_dialog(
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
                &state.dropdown,
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
                .text_color(gpui::rgb(ui::palette().text_dim))
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
