//! The Info tab: what the open photo's EXIF says, and where it was
//! taken.

use super::*;
use crate::workspace::SideTab;
use schist_gallery::ExifSummary;

/// The tallest the EXIF rows get before they scroll: about three rows.
const INFO_ROWS_MAX_H: f32 = 88.0;
/// The side panel is 260 px wide with 8 px of padding each side; the
/// map spans that.
const SIDE_PANEL_CONTENT_W: f32 = 260.0 - 16.0;

/// The side panel's top slot: a tab row when the open file has EXIF —
/// Info first and by default, Color beside it — and the plain colour
/// panel when it has none, exactly as before.
pub(super) fn top_panel(ws: &mut Workspace, cx: &mut Context<Workspace>) -> gpui::AnyElement {
    ws.refresh_exif();
    let Some(exif) = ws.exif.as_ref().and_then(|(_, e)| e.clone()) else {
        return color_panel(ws, cx).into_any_element();
    };
    let tab = ws.side_tab.unwrap_or(SideTab::Info);
    let tab_chip = |label: &'static str, which: SideTab, cx: &mut Context<Workspace>| {
        let on = tab == which;
        div()
            .px_2()
            .py(px(3.0))
            .rounded_sm()
            .text_size(px(11.0))
            .cursor_pointer()
            .bg(gpui::rgb(if on {
                palette().hover
            } else {
                palette().panel_bg
            }))
            .text_color(gpui::rgb(if on {
                palette().text
            } else {
                palette().text_dim
            }))
            .hover(|s| s.bg(gpui::rgb(palette().hover)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |ws, _e: &MouseDownEvent, _w, cx| {
                    ws.side_tab = Some(which);
                    cx.notify();
                }),
            )
            .child(label.to_uppercase())
    };
    let tabs = div()
        .flex()
        .flex_row()
        .gap_1()
        .px_2()
        .pt_2()
        .child(tab_chip("Info", SideTab::Info, cx))
        .child(tab_chip("Color", SideTab::Color, cx));
    let body = match tab {
        SideTab::Info => info_panel(ws, &exif, cx).into_any_element(),
        SideTab::Color => color_panel(ws, cx).into_any_element(),
    };
    div()
        .flex()
        .flex_col()
        .child(tabs)
        .child(body)
        .into_any_element()
}

/// The thumb beside the EXIF rows, so it shows there is more below the
/// fold; it reads the scroll handle's own extents from the last frame
/// and is absent while everything fits.
fn rows_thumb(handle: &gpui::ScrollHandle) -> Option<gpui::AnyElement> {
    let view_h = f32::from(handle.bounds().size.height);
    let max_y = f32::from(handle.max_offset().height);
    if view_h <= 0.0 || max_y <= 1.0 {
        return None;
    }
    let thumb_h = (view_h * view_h / (view_h + max_y)).clamp(20.0, view_h);
    let travel = (view_h - thumb_h).max(1.0);
    let scroll_y = (-f32::from(handle.offset().y)).clamp(0.0, max_y);
    let thumb_top = scroll_y / max_y * travel;
    Some(
        div()
            .absolute()
            .top(px(thumb_top))
            .right_0()
            .w(px(3.0))
            .h(px(thumb_h))
            .rounded_sm()
            .bg(gpui::rgb(palette().text_dim))
            .opacity(0.5)
            .into_any_element(),
    )
}

/// The camera, the exposure, when, and — on a map with a blip — where.
fn info_panel(
    ws: &mut Workspace,
    exif: &ExifSummary,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let row = |label: &'static str, value: String| {
        div()
            .flex()
            .flex_row()
            .items_baseline()
            .gap_2()
            .child(
                div()
                    .w(px(64.0))
                    .flex_none()
                    .text_size(px(10.0))
                    .text_color(gpui::rgb(palette().text_dim))
                    .child(label),
            )
            .child(
                div()
                    .flex_grow()
                    .min_w(px(0.0))
                    .text_size(px(11.0))
                    .text_color(gpui::rgb(palette().text))
                    .child(SharedString::from(value)),
            )
    };
    let mut rows: Vec<gpui::AnyElement> = Vec::new();
    if let Some(camera) = exif.camera() {
        rows.push(row("Camera", camera).into_any_element());
    }
    if let Some(lens) = &exif.lens {
        rows.push(row("Lens", lens.clone()).into_any_element());
    }
    // The exposure triangle on one line, as a camera's own display
    // puts it.
    let exposure: Vec<String> = [
        exif.exposure.clone(),
        exif.aperture.clone(),
        exif.iso.map(|iso| format!("ISO {iso}")),
        exif.focal_length.clone(),
    ]
    .into_iter()
    .flatten()
    .collect();
    if !exposure.is_empty() {
        rows.push(row("Exposure", exposure.join(" \u{b7} ")).into_any_element());
    }
    if let Some(bias) = &exif.exposure_bias {
        rows.push(row("Bias", bias.clone()).into_any_element());
    }
    let mut extras: Vec<String> = Vec::new();
    if let Some(flash) = exif.flash {
        extras.push(if flash {
            "flash fired".into()
        } else {
            "no flash".into()
        });
    }
    if let Some(wb) = &exif.white_balance {
        extras.push(format!("WB {}", wb.to_lowercase()));
    }
    if let Some(metering) = &exif.metering {
        extras.push(format!("{} metering", metering.to_lowercase()));
    }
    if !extras.is_empty() {
        rows.push(row("Settings", extras.join(", ")).into_any_element());
    }
    if let Some(taken) = &exif.taken {
        rows.push(row("Taken", taken.clone()).into_any_element());
    }
    if let (Some(w), Some(h)) = (exif.width, exif.height) {
        let mp = (w as f64 * h as f64) / 1_000_000.0;
        let orient = match exif.orientation {
            Some(o) if o > 1 => format!(" \u{b7} orientation {o}"),
            _ => String::new(),
        };
        rows.push(
            row("Size", format!("{w} \u{d7} {h} \u{b7} {mp:.1} MP{orient}")).into_any_element(),
        );
    }
    if let Some(software) = &exif.software {
        rows.push(row("Software", software.clone()).into_any_element());
    }
    if let Some((lat, lon)) = exif.gps {
        let place = schist_gallery::nearest_city(lat, lon);
        let mut text = match place {
            Some(place) => format!("{place} \u{b7} {lat:.4}, {lon:.4}"),
            None => format!("{lat:.4}, {lon:.4}"),
        };
        if let Some(alt) = exif.altitude_m {
            text.push_str(&format!(" \u{b7} {alt:.0} m"));
        }
        rows.push(row("Where", text).into_any_element());
    }
    // The rows are their own scrolling region, bounded so the map
    // beneath stays put whatever a camera wrote (some write a lot).
    let panel = div()
        .flex()
        .flex_col()
        .p_2()
        .gap_1()
        .child(panel_title("Info"))
        .child(
            div()
                .relative()
                .child(
                    div()
                        .id("info-rows")
                        .flex()
                        .flex_col()
                        .gap_1()
                        .max_h(px(INFO_ROWS_MAX_H))
                        .overflow_y_scroll()
                        .track_scroll(&ws.info_scroll)
                        .children(rows),
                )
                .children(rows_thumb(&ws.info_scroll)),
        );
    #[cfg(not(target_arch = "wasm32"))]
    let panel = if exif.gps.is_some() {
        // The map, with the blip on it, at 16:9 across the panel's
        // content width. Wheel to zoom, drag to pan; it opens on the
        // spot at street scale.
        let map_h = (SIDE_PANEL_CONTENT_W * 9.0 / 16.0).round();
        panel.child(div().pt_1().child(crate::workspace::map_element(
            ws,
            crate::workspace::MapSlot::Info,
            map_h,
            cx,
        )))
    } else {
        panel
    };
    #[cfg(target_arch = "wasm32")]
    {
        let _ = (ws, cx);
    }
    panel
}
