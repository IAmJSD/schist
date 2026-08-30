//! Dialogs raised while opening a file: what to do with a dropped
//! image, and the HEIC decoder consent prompt.

use super::*;

/// An image was dropped on the window while a document is open: its own
/// tab, or a new layer in the current document?
pub(super) fn drop_image(
    path: std::path::PathBuf,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    let tab_path = path.clone();
    ui::modal_frame(
        "Open Image",
        380.0,
        div().text_size(px(12.0)).child(format!(
            "Open \u{201C}{name}\u{201D} in a new tab, or add it to the current document as a new layer?"
        )),
        div()
            .flex()
            .flex_row()
            .gap_2()
            .child(ui::button(
                "Cancel",
                false,
                |ws, _window, cx| ws.close_modal(cx),
                cx,
            ))
            .child(ui::button(
                "New Tab",
                false,
                move |ws, _window, cx| {
                    ws.close_modal(cx);
                    ws.load_file(tab_path.clone(), cx);
                },
                cx,
            ))
            .child(ui::button(
                "New Layer",
                true,
                move |ws, _window, cx| {
                    ws.close_modal(cx);
                    ws.place_image_as_layer(path.clone(), cx);
                },
                cx,
            )),
    )
}

/// A HEIC file needs an HEVC decoder this machine doesn't have: ask
/// before downloading one. Consent matters here — it is a network fetch
/// of executable code (hash-pinned to a schist release) and the
/// libraries carry their own (LGPL-3.0) licenses, which are installed
/// alongside.
pub(super) fn heif_support(
    ws: &Workspace,
    path: std::path::PathBuf,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let managed = schist_codecs_common::heif::managed_library()
        .expect("dialog only opens when a download exists for this platform");
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    let downloading = ws.heif_download;
    let source_url = managed.source_url;
    ui::modal_frame(
        "HEIC Support",
        420.0,
        div()
            .flex()
            .flex_col()
            .gap_2()
            .text_size(px(12.0))
            .child(format!(
                "Opening \u{201C}{name}\u{201D} needs an HEVC decoder, which is not \
                 installed on this system."
            ))
            .child(format!(
                "Schist can download a decode-only build of libheif {} with the \
                 libde265 HEVC decoder (\u{2248}5 MB). Both are LGPL-3.0 licensed; \
                 their license texts are installed next to the library, and the \
                 source is available at the project page.",
                managed.version
            )),
        div()
            .flex()
            .flex_row()
            .gap_2()
            .child(ui::button(
                "Cancel",
                false,
                |ws, _window, cx| ws.close_modal(cx),
                cx,
            ))
            .child(ui::button(
                "Licenses & Source",
                false,
                move |_ws, _window, cx| cx.open_url(source_url),
                cx,
            ))
            .child(ui::button(
                if downloading {
                    "Downloading\u{2026}"
                } else {
                    "Download"
                },
                true,
                move |ws, _window, cx| {
                    if !ws.heif_download {
                        ws.close_modal(cx);
                        ws.download_heif_support(path.clone(), cx);
                    }
                },
                cx,
            )),
    )
}
