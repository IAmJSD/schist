//! The update-available prompt and its download progress.

use super::*;

/// A newer release than this build.
///
/// Where Schist can replace itself — a macOS bundle, a Windows install —
/// it offers to, and restarts into the new one. Where it cannot, the
/// copy belongs to whatever installed it, so the dialog says where the
/// release is and gets out of the way.
pub(super) fn update_available(
    ws: &Workspace,
    update: crate::update::Update,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let progress = ws.update_progress.clone();
    let page = update.page.clone();
    let installer = update.install.clone();

    let body = div()
        .flex()
        .flex_col()
        .gap_2()
        .text_size(px(12.0))
        .child(format!(
            "Schist {} is available. This copy is {}.",
            update.version,
            crate::update::current_version()
        ));
    let body = match (&installer, &progress) {
        (_, Some(UpdateProgress::Downloading { received, total })) => body
            .child(format!(
                "Downloading\u{2026} {} of {}",
                megabytes(*received),
                megabytes(*total)
            ))
            .child(progress_bar(*received as f32 / (*total).max(1) as f32)),
        (_, Some(UpdateProgress::Installing)) => body
            .child("Installing the update\u{2026}".to_string())
            .child(progress_bar(1.0)),
        // The buttons already make the available choices clear. Details
        // about replacing the executable and restarting are implementation
        // noise in what should be a quick update prompt.
        (Some(_), None) => body,
        (None, None) => body.child(
            "This copy came from somewhere that owns it \u{2014} a package \
             manager, an AppImage, a build of your own \u{2014} so it updates \
             the way it was installed."
                .to_string(),
        ),
    };

    let actions = div().flex().flex_row().gap_2();
    let actions = match progress {
        // Nothing to press while the bundle is being swapped: it takes a
        // moment and there is no half of it to back out to.
        Some(UpdateProgress::Installing) => actions,
        Some(UpdateProgress::Downloading { .. }) => actions.child(ui::button(
            "Cancel",
            false,
            |ws, _window, cx| ws.cancel_update(cx),
            cx,
        )),
        None => {
            let actions = actions.child(ui::button(
                "Later",
                false,
                |ws, _window, cx| ws.close_modal(cx),
                cx,
            ));
            match installer {
                Some(_) => actions
                    .child(ui::button(
                        "Release Notes",
                        false,
                        move |_ws, _window, cx| cx.open_url(&page),
                        cx,
                    ))
                    .child(ui::button(
                        "Update and Restart",
                        true,
                        move |ws, _window, cx| ws.start_update(update.clone(), cx),
                        cx,
                    )),
                None => actions.child(ui::button(
                    "Open Release Page",
                    true,
                    move |_ws, _window, cx| cx.open_url(&page),
                    cx,
                )),
            }
        }
    };
    ui::modal_frame("Update Available", UPDATE_DIALOG_WIDTH, body, actions)
}

/// The update dialog's width, which its progress bar has to match.
pub(super) const UPDATE_DIALOG_WIDTH: f32 = 420.0;

/// A download's size, in the megabytes a release page would quote.
pub(super) fn megabytes(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1_000_000.0)
}

/// A filled bar, `fraction` of the way across the dialog.
pub(super) fn progress_bar(fraction: f32) -> impl IntoElement {
    // The frame's width less its padding, which is `p_3` on both sides.
    let width = UPDATE_DIALOG_WIDTH - 24.0;
    div()
        .w(px(width))
        .h(px(6.0))
        .rounded_sm()
        .bg(gpui::rgb(ui::palette().button_bg))
        .child(
            div()
                .h_full()
                .w(px(width * fraction.clamp(0.0, 1.0)))
                .rounded_sm()
                .bg(gpui::rgb(ui::palette().accent)),
        )
}
