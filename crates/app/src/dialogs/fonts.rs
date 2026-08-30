//! The missing-fonts prompt.

use super::*;

/// Fonts the open document names that this system doesn't have.
///
/// Substituting silently would keep the file readable while quietly
/// changing every glyph width and line break, so we say what is missing
/// and offer to fetch it. Nothing is requested until a button is pressed.
pub(super) fn missing_fonts(
    ws: &Workspace,
    fonts: &[crate::fonts::MissingFont],
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let busy = ws.font_downloads.clone();
    let rows: Vec<gpui::AnyElement> = fonts
        .iter()
        .map(|font| {
            let family = font.family.clone();
            let target = font.target().to_string();
            let downloading = busy.contains(&family);
            let action: gpui::AnyElement = if downloading {
                div()
                    .text_size(px(11.0))
                    .text_color(gpui::rgb(ui::palette().text_dim))
                    .child("\u{2026}")
                    .into_any_element()
            } else {
                let label = match font.substitute {
                    Some(sub) => format!("Install {sub}"),
                    None => "Download".to_string(),
                };
                let (f, t) = (family.clone(), target.clone());
                ui::button(
                    SharedString::from(label),
                    true,
                    move |ws, _w, cx| ws.download_font(f.clone(), t.clone(), cx),
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
                    // Fixed rather than flex-grown, like the model rows:
                    // a grown column sizes to its content and clips.
                    div()
                        .flex()
                        .flex_col()
                        .w(px(392.0))
                        .flex_none()
                        .child(
                            div()
                                .text_size(px(12.0))
                                .child(SharedString::from(font.family.clone())),
                        )
                        .child(
                            div()
                                .text_size(px(11.0))
                                .text_color(gpui::rgb(ui::palette().text_dim))
                                .child(SharedString::from(font.detail())),
                        ),
                )
                .child(action)
                .into_any_element()
        })
        .collect();

    // What the dialog says when it has nothing to offer: no document, or
    // a document whose every font is already here.
    let preamble: SharedString = if ws.doc.is_none() {
        "No document is open, so there is nothing to check.".into()
    } else if fonts.is_empty() {
        "Every font this document uses is installed. Its text is set in \
         the fonts it was laid out with."
            .into()
    } else {
        "This document is set in fonts you don't have. Text is readable in \
         a substitute, but its widths and line breaks are not the ones it \
         was laid out with."
            .into()
    };

    let body = div()
        .id("missing-fonts-body")
        .flex()
        .flex_col()
        .gap_1()
        .w(px(520.0))
        .max_h(px(360.0))
        .overflow_y_scroll()
        .child(
            div()
                .pb_1()
                .text_size(px(11.0))
                .text_color(gpui::rgb(ui::palette().text_dim))
                .child(preamble),
        )
        .children(rows)
        .when(!fonts.is_empty(), |body| {
            body.child(
                div()
                    .pt_2()
                    .text_size(px(11.0))
                    .text_color(gpui::rgb(ui::palette().text_dim))
                    .child(SharedString::from(format!(
                        "Downloads come from the Google Fonts open catalogue and are kept in {}. \
                     A font that isn't openly licensed is never fetched — where a \
                     metric-compatible libre design exists it is offered instead, which \
                     restores the layout because the advance widths match.",
                        schist_text_engine::font_dir()
                            .map(|d| d.display().to_string())
                            .unwrap_or_else(|| "your user font directory".into())
                    ))),
            )
        });
    let actions = div().flex().flex_row().gap_2().child(ui::button(
        "Close",
        true,
        |ws, _w, cx| ws.close_modal(cx),
        cx,
    ));
    ui::modal_frame("Missing Fonts", 560.0, body, actions)
}
