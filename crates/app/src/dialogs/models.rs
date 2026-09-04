//! The Neural Filters model manager.

use super::*;

/// Filter ▸ Neural Filters ▸ Manage Models.
///
/// The style-transfer, depth, face and segmentation networks are
/// somebody else's work and up to sixty-six megabytes of it, so they are
/// fetched here rather than shipped. The ones trained for this
/// application are small enough to live in the binary, and are listed as
/// built in rather than being hidden: which filter has which model is
/// exactly what somebody opens this dialog to find out.
///
/// Two of the models are not a filter's. Object Selection and
/// Content-Aware Fill are tools, and this is still where their networks
/// live, because one list of every model in the build is more useful
/// than a tidy division by which menu asked for it.
pub(super) fn model_manager(ws: &Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let downloading = ws.model_downloads.clone();
    let rows: Vec<gpui::AnyElement> = schist_neural::CATALOG
        .iter()
        .map(|spec| {
            let id = spec.id;
            let installed = schist_neural::installed(id);
            let busy = downloading.iter().find(|d| d.id == id);
            // Kilobytes below a megabyte: two of the built-in models
            // round to "0.2 MB", which reads as a rounding error rather
            // than as a size.
            let size_of = |bytes: f64| {
                if bytes < (1 << 20) as f64 {
                    format!("{:.0} KB", bytes / (1 << 10) as f64)
                } else {
                    format!("{:.1} MB", bytes / (1 << 20) as f64)
                }
            };
            let size = schist_neural::installed_size(spec)
                .map(|b| size_of(b as f64))
                .unwrap_or_else(|| size_of(spec.bytes as f64));
            // A "built-in" only ships in the binary natively; the web
            // build serves the same file beside the app and fetches it on
            // demand, so there it gets the download/remove row like any
            // other model.
            let carried = spec.built_in() && cfg!(not(target_arch = "wasm32"));
            let fetchable = schist_neural::download_url(spec).is_some();
            let state = if carried {
                format!("Built in \u{b7} {size}")
            } else if let Some(download) = busy {
                // Against the size this build expects rather than the
                // one the server declared: they are the same file, and
                // the hash check afterwards is what says so.
                let got = download.got.load(std::sync::atomic::Ordering::Relaxed);
                format!(
                    "Downloading\u{2026} {} of {}",
                    size_of(got as f64),
                    size_of(spec.bytes as f64)
                )
            } else if installed {
                format!("Installed \u{b7} {size}")
            } else if fetchable {
                format!("Not installed \u{b7} {size}")
            } else {
                // The externally-hosted models: their GitHub URLs redirect
                // through a host that sends no CORS headers, so a browser
                // cannot fetch them at all.
                format!("Not available in the browser \u{b7} {size}")
            };
            let action: gpui::AnyElement = if carried || (!installed && !fetchable) {
                div().into_any_element()
            } else if busy.is_some() {
                div()
                    .text_size(px(11.0))
                    .text_color(gpui::rgb(ui::palette().text_dim))
                    .child("\u{2026}")
                    .into_any_element()
            } else if installed {
                ui::button(
                    "Remove",
                    false,
                    move |ws, _w, cx| ws.remove_model(id, cx),
                    cx,
                )
                .into_any_element()
            } else {
                ui::button(
                    "Download",
                    true,
                    move |ws, _w, cx| ws.download_model(id, cx),
                    cx,
                )
                .into_any_element()
            };
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .py_1()
                .child(
                    // Fixed rather than flex-grown: the notes are long
                    // enough to need wrapping, and a grown column sizes to
                    // its content and gets clipped instead.
                    div()
                        .flex()
                        .flex_col()
                        .w(px(412.0))
                        .flex_none()
                        .child(
                            div()
                                .text_size(px(12.0))
                                .child(SharedString::from(spec.name)),
                        )
                        .child(
                            div()
                                .text_size(px(11.0))
                                .text_color(gpui::rgb(ui::palette().text_dim))
                                .child(SharedString::from(format!(
                                    "{state} \u{b7} {}",
                                    spec.license
                                ))),
                        )
                        .child(
                            div()
                                .text_size(px(11.0))
                                .text_color(gpui::rgb(ui::palette().text_dim))
                                .child(SharedString::from(spec.note)),
                        ),
                )
                .child(action)
                .into_any_element()
        })
        .collect();

    let body = div()
        .id("model-manager-body")
        .flex()
        .flex_col()
        .gap_1()
        .w(px(520.0))
        .max_h(px(360.0))
        .overflow_y_scroll()
        .children(rows)
        .child(
            div()
                .pt_2()
                .text_size(px(11.0))
                .text_color(gpui::rgb(ui::palette().text_dim))
                .child(SharedString::from(format!(
                    "Models are kept in {}. Filters that have no model fall \
                     back to signal processing and say so.",
                    schist_neural::model_dir().display()
                ))),
        );
    let actions = div().flex().flex_row().gap_2().child(ui::button(
        "Close",
        true,
        |ws, _w, cx| ws.close_modal(cx),
        cx,
    ));
    ui::modal_frame("Neural Filter Models", 560.0, body, actions)
}
