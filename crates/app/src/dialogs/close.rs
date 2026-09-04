//! Confirming a close with unsaved changes.

use super::*;

/// "Save changes before closing?" for the active tab. Save falls back to
/// the Save As dialog for never-saved documents; the tab then stays open
/// (now clean) rather than chaining a close onto an async file prompt.
pub(super) fn confirm_close_tab(
    ws: &mut Workspace,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
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
                    ws.resume_quit(cx);
                },
                cx,
            ))
            .child(ui::button(
                "Cancel",
                false,
                |ws, _window, cx| {
                    ws.cancel_quit();
                    ws.close_modal(cx);
                },
                cx,
            ))
            .child(ui::button(
                "Save…",
                true,
                |ws, window, cx| {
                    ws.close_modal(cx);
                    // The Save As prompt is async: it returns with the
                    // document still dirty and finishes later, so the
                    // close has to be pending rather than conditional.
                    // Answering "Save…" used to save an Untitled document
                    // and leave its tab open.
                    ws.close_tab_after_save();
                    ws.save_current(window, cx);
                    if ws.has_pending_save() {
                        #[cfg(not(target_arch = "wasm32"))]
                        if ws
                            .doc
                            .as_ref()
                            .is_some_and(|doc| ws.cloud.docs.contains_key(&doc.id))
                        {
                            // The cloud acknowledgement resumes the pending quit.
                            return;
                        }
                        // Still waiting on a file prompt. Do not hold a
                        // quit open across it; the tab closes when the
                        // save lands.
                        ws.cancel_quit();
                    } else {
                        // Saved synchronously, so the tab has gone.
                        ws.resume_quit(cx);
                    }
                },
                cx,
            )),
    )
}
