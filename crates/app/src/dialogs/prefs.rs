//! Preferences.

use super::*;

/// Application preferences (⌘K).
pub(super) fn preferences(
    ws: &mut Workspace,
    state: &DialogState,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let view = ws.view.clone();
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
                &ws.dropdown_scroll,
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
                &ws.dropdown_scroll,
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
        .children(
            // The gallery is compiled out of the web build, so a switch
            // for it would be furniture there.
            (!cfg!(target_arch = "wasm32")).then(|| {
                ui::field_row(
                    "Gallery",
                    ui::checkbox(
                        "Hide photos the content filter flags as explicit",
                        view.gallery_hide_nsfw,
                        |ws, _cx| {
                            ws.view.gallery_hide_nsfw = !ws.view.gallery_hide_nsfw;
                            ws.save_view_options();
                        },
                        cx,
                    ),
                )
            }),
        )
        .children((!cfg!(target_arch = "wasm32")).then(|| {
            div()
                .pl(px(110.0))
                .text_size(px(10.0))
                .text_color(gpui::rgb(ui::palette().text_faint))
                .child(
                    "Judged by the Content (NSFW Filter) model — fetch it under \
                     Filter ▸ Neural Filters ▸ Manage Models. Without it, nothing \
                     is flagged.",
                )
        }))
        .child(ui::field_row(
            "Rendering",
            ui::checkbox(
                "GPU compositing",
                view.gpu_compositing,
                |ws, cx| {
                    ws.view.gpu_compositing = !ws.view.gpu_compositing;
                    ws.save_view_options();
                    crate::workspace::init_compositor_backend(ws.view.gpu_compositing);
                    // Cached tiles and the viewport image were composited
                    // by the old backend; rebuild them on the new one.
                    ws.rebuild_after_backend_change(cx);
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
        // Only the official releases are built with a DSN. Everywhere else
        // this would be a checkbox that sends reports nowhere, so it is not
        // offered -- and the preference it would set stays false.
        .children(crate::crash::reporting_available().then(|| {
            ui::field_row(
                "",
                ui::checkbox(
                    "Also send it to the developers",
                    view.crash_upload,
                    |ws, _cx| {
                        ws.view.crash_upload = !ws.view.crash_upload;
                        ws.save_view_options();
                    },
                    cx,
                ),
            )
        }))
        .child(
            div()
                .text_size(px(11.0))
                .text_color(gpui::rgb(ui::palette().text_dim))
                .child("Diagnostics take effect when Schist next starts."),
        )
        .child(ui::field_row(
            "Updates",
            ui::checkbox(
                "Check for new releases at launch",
                view.check_updates,
                |ws, _cx| {
                    ws.view.check_updates = !ws.view.check_updates;
                    ws.save_view_options();
                },
                cx,
            ),
        ))
        .child(
            div()
                .text_size(px(11.0))
                .text_color(gpui::rgb(ui::palette().text_dim))
                .child(format!("Keyboard shortcuts: {keymap_path}")),
        )
        .child(
            div()
                .text_size(px(11.0))
                .text_color(gpui::rgb(ui::palette().text_dim))
                .child(format!("Version {}", crate::update::current_version())),
        );

    let actions = div()
        .flex()
        .flex_row()
        .gap_2()
        .child(ui::button(
            "Cancel",
            false,
            |ws, _w, cx| {
                ws.revert_preferences(cx);
                ws.close_modal(cx);
            },
            cx,
        ))
        .child(ui::button(
            "Done",
            true,
            |ws, _w, cx| {
                ws.keep_preferences();
                ws.close_modal(cx);
            },
            cx,
        ));
    ui::modal_frame("Preferences", 400.0, body, actions)
}
