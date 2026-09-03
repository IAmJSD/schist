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
#[cfg(not(target_arch = "wasm32"))]
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

/// A raw file Schist's own decoder declines needs LibRaw, and this
/// machine has none. There is no download offer yet (no prebuilt to
/// pin, unlike libheif), so this says where the library comes from
/// and retries the open once it is there.
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn libraw_support(
    path: std::path::PathBuf,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    let how = if cfg!(target_os = "macos") {
        "Install it with Homebrew: brew install libraw".to_string()
    } else if cfg!(target_os = "linux") {
        "Install your distribution's LibRaw package (Debian/Ubuntu: libraw23t64 or \
         libraw23; Fedora: LibRaw; Arch: libraw)."
            .to_string()
    } else {
        format!(
            "Put a LibRaw build (libraw.dll) in {}",
            schist_codecs_common::raw::managed_dir().display()
        )
    };
    ui::modal_frame(
        "LibRaw Needed",
        420.0,
        div()
            .flex()
            .flex_col()
            .gap_2()
            .text_size(px(12.0))
            .child(format!(
                "Schist\u{2019}s own raw decoder does not read \u{201C}{name}\u{201D} \
                 yet (Canon CR3, Fuji compressed RAF and a few other codecs), and \
                 the LibRaw library it falls back to for those is not installed."
            ))
            .child(how)
            .child("Then choose Try Again."),
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
                "Try Again",
                true,
                move |ws, _window, cx| {
                    ws.close_modal(cx);
                    ws.load_file(path.clone(), cx);
                },
                cx,
            )),
    )
}

/// Folders dropped on the window: every image in them as tabs, or the
/// folders watched in the gallery. The gallery is the answer for
/// anything bigger than a handful — which is why it is the primary
/// button — and the tab count is capped so a camera roll cannot open
/// five thousand tabs by accident.
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn drop_folders(
    dirs: Vec<std::path::PathBuf>,
    images: usize,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let what = if dirs.len() == 1 {
        format!(
            "\u{201C}{}\u{201D}",
            dirs[0]
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| dirs[0].display().to_string())
        )
    } else {
        format!("{} folders", dirs.len())
    };
    let cap = crate::workspace::DROP_OPEN_CAP;
    let open_label = if images == 0 {
        "Open in Tabs".to_string()
    } else if images > cap {
        format!("Open First {cap} in Tabs")
    } else if images == 1 {
        "Open 1 in a Tab".to_string()
    } else {
        format!("Open {images} in Tabs")
    };
    let body = div()
        .flex()
        .flex_col()
        .gap_1()
        .child(div().text_size(px(12.0)).child(format!(
            "{what} holds {images} image{} Schist can open (sub-folders included).",
            if images == 1 { "" } else { "s" }
        )))
        .child(
            div()
                .text_size(px(11.0))
                .text_color(gpui::rgb(ui::palette().text_dim))
                .child(
                    "Add to Gallery watches the folders in place — nothing is copied, \
                     and edits are versioned beside each photo. Opening in tabs loads \
                     every image into the editor now.",
                ),
        );
    let open_dirs = dirs.clone();
    ui::modal_frame(
        "Dropped Folders",
        420.0,
        body,
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
                open_label,
                false,
                move |ws, _window, cx| {
                    ws.close_modal(cx);
                    ws.open_folder_images(open_dirs.clone(), cx);
                },
                cx,
            ))
            .child(ui::button(
                "Add to Gallery",
                true,
                move |ws, _window, cx| {
                    ws.close_modal(cx);
                    ws.add_gallery_folders(dirs.clone(), cx);
                },
                cx,
            )),
    )
}
