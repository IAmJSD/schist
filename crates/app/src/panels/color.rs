//! The colour panel: foreground/background wells and the swatch palette.

use super::*;

pub(super) fn color_wells(ws: &Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    div()
        .relative()
        .size(px(30.0))
        .mt_2()
        .child(
            div()
                .absolute()
                .bottom_0()
                .right_0()
                .size(px(18.0))
                .bg(swatch_hex(ws.editor.background))
                .border_1()
                .border_color(gpui::rgb(palette().text_faint))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|ws, _ev, _w, cx| {
                        ws.open_color_picker(ColorTarget::Background, cx)
                    }),
                ),
        )
        .child(
            div()
                .absolute()
                .top_0()
                .left_0()
                .size(px(18.0))
                .bg(swatch_hex(ws.editor.foreground))
                .border_1()
                .border_color(gpui::rgb(palette().text))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|ws, _ev, _w, cx| {
                        ws.open_color_picker(ColorTarget::Foreground, cx)
                    }),
                ),
        )
        // The empty corner between the two wells, which is where
        // Photoshop puts the swap arrows too.
        .child(
            div()
                .absolute()
                .top(px(-1.0))
                .right(px(-1.0))
                .size(px(11.0))
                .child(icon("swap", 11.0, palette().text_dim))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|ws, _ev, _w, cx| {
                        std::mem::swap(&mut ws.editor.foreground, &mut ws.editor.background);
                        cx.notify();
                    }),
                ),
        )
}

// ===== side panels =====

pub(super) const PALETTE: [u32; 16] = [
    0x000000, 0xFFFFFF, 0x808080, 0xC0C0C0, 0xE81E25, 0xFF7F27, 0xFFF200, 0x22B14C, 0x00A2E8,
    0x3F48CC, 0xA349A4, 0xB97A57, 0xFFAEC9, 0xFFC90E, 0xB5E61D, 0x99D9EA,
];

pub(super) fn color_panel(ws: &mut Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let fg = ws.editor.foreground.to_u8();
    div()
        .flex()
        .flex_col()
        .p_2()
        .gap_1()
        .child(panel_title("Color"))
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(|ws, ev: &MouseDownEvent, _w, cx| {
                ws.open_context_menu(ContextTarget::Color, ev.position, cx);
            }),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .gap_1()
                .children(PALETTE.map(|hex| {
                    div()
                        .size(px(18.0))
                        .bg(gpui::rgb(hex))
                        .border_1()
                        .border_color(gpui::rgb(palette().divider))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |ws, ev: &MouseDownEvent, _w, cx| {
                                let color = Rgba::from_u8(
                                    ((hex >> 16) & 0xFF) as u8,
                                    ((hex >> 8) & 0xFF) as u8,
                                    (hex & 0xFF) as u8,
                                    255,
                                );
                                if ev.modifiers.alt {
                                    ws.editor.background = color;
                                } else {
                                    ws.editor.foreground = color;
                                }
                                cx.notify();
                            }),
                        )
                })),
        )
        .child(slider(
            "col-r",
            "R",
            format!("{}", fg[0]),
            SliderTarget::ForegroundR,
            ws,
            cx,
        ))
        .child(slider(
            "col-g",
            "G",
            format!("{}", fg[1]),
            SliderTarget::ForegroundG,
            ws,
            cx,
        ))
        .child(slider(
            "col-b",
            "B",
            format!("{}", fg[2]),
            SliderTarget::ForegroundB,
            ws,
            cx,
        ))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .child(
                    div()
                        .text_size(px(10.0))
                        .text_color(gpui::rgb(palette().text_dim))
                        .child(format!("#{:02X}{:02X}{:02X}", fg[0], fg[1], fg[2])),
                )
                .child(
                    div()
                        .text_size(px(10.0))
                        .text_color(gpui::rgb(palette().text_dim))
                        .hover(|s| s.text_color(gpui::rgb(palette().text)))
                        .child("Picker\u{2026}")
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|ws, _e, _w, cx| {
                                ws.open_color_picker(ColorTarget::Foreground, cx)
                            }),
                        ),
                ),
        )
        // Photoshop's spectrum bar: drag along it to take a hue directly.
        .child(crate::color_picker::hue_ramp(ws, cx))
}
