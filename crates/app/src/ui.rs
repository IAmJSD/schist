//! Small widget kit shared by the panels and dialogs.
//!
//! Deliberately minimal: GPUI ships no widget library, and a full text-input
//! implementation needs an IME handler, so numeric fields here are edited by
//! click-to-focus plus digit keys (see `Workspace::field_key`) rather than
//! by a general-purpose text editor.

use crate::workspace::{Popup, Workspace};
use gpui::{
    div, px, Context, InteractiveElement as _, IntoElement, MouseButton, ParentElement as _,
    SharedString, StatefulInteractiveElement as _, Styled as _,
};

pub const TEXT: u32 = 0xD8D8D8;
pub const TEXT_DIM: u32 = 0x9A9A9A;
pub const ACCENT: u32 = 0x3A6EA5;
pub const HOVER: u32 = 0x2E2E2E;
pub const FIELD_BG: u32 = 0x0E0E0E;
pub const PANEL_BG: u32 = 0x1A1A1A;
pub const POPUP_BG: u32 = 0x242424;
pub const EDGE: u32 = 0x3A3A3A;

/// A labelled push button.
pub fn button(
    label: impl Into<SharedString>,
    primary: bool,
    on_click: impl Fn(&mut Workspace, &mut gpui::Window, &mut Context<Workspace>) + 'static,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .justify_center()
        .h(px(24.0))
        .px_3()
        .rounded_sm()
        .text_size(px(12.0))
        .bg(gpui::rgb(if primary { ACCENT } else { 0x333333 }))
        .hover(|s| s.bg(gpui::rgb(if primary { 0x4A80BC } else { 0x3E3E3E })))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |ws, _e, window, cx| on_click(ws, window, cx)),
        )
        .child(label.into())
}

/// A numeric field: click to focus and type digits, or use the ± buttons.
pub fn num_field(
    id: &'static str,
    value: f32,
    suffix: &'static str,
    step: f32,
    on_change: impl Fn(&mut Workspace, f32) + Clone + 'static,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let focused = cx.entity().read(cx).focused_field == Some(id);
    let buffer = cx.entity().read(cx).field_buffer.clone();
    let shown = if focused && !buffer.is_empty() {
        buffer
    } else if value.fract().abs() < 0.01 {
        format!("{value:.0}")
    } else {
        format!("{value:.2}")
    };
    let dec = on_change.clone();
    let inc = on_change.clone();
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .child(
            div()
                .flex()
                .items_center()
                .justify_end()
                .w(px(62.0))
                .h(px(20.0))
                .px_1()
                .rounded_sm()
                .bg(gpui::rgb(FIELD_BG))
                .border_1()
                .border_color(gpui::rgb(if focused { ACCENT } else { FIELD_BG }))
                .text_size(px(11.0))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |ws, _e, _w, cx| {
                        ws.focus_field(id);
                        cx.notify();
                    }),
                )
                .child(format!("{shown}{suffix}")),
        )
        .child(step_button("minus", move |ws| dec(ws, -step), cx))
        .child(step_button("plus", move |ws| inc(ws, step), cx))
}

fn step_button(
    icon_name: &'static str,
    on_click: impl Fn(&mut Workspace) + 'static,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .justify_center()
        .size(px(18.0))
        .rounded_sm()
        .bg(gpui::rgb(0x2A2A2A))
        .hover(|s| s.bg(gpui::rgb(HOVER)))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |ws, _e, _w, cx| {
                on_click(ws);
                cx.notify();
            }),
        )
        .child(crate::panels::icon(icon_name, 11.0, TEXT))
}

/// A checkbox with a label to its right.
pub fn checkbox(
    label: impl Into<SharedString>,
    checked: bool,
    on_toggle: impl Fn(&mut Workspace) + 'static,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .text_size(px(12.0))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |ws, _e, _w, cx| {
                on_toggle(ws);
                cx.notify();
            }),
        )
        .child(
            div()
                .flex()
                .items_center()
                .justify_center()
                .size(px(14.0))
                .rounded_sm()
                .bg(gpui::rgb(if checked { ACCENT } else { FIELD_BG }))
                .border_1()
                .border_color(gpui::rgb(EDGE))
                .when_some(checked.then_some(()), |d, _| {
                    d.child(crate::panels::icon("check", 10.0, 0xFFFFFF))
                }),
        )
        .child(label.into())
}

/// A dropdown button that opens `popup` with the given options.
pub fn dropdown<T: Clone + PartialEq + 'static>(
    popup: Popup,
    current: &T,
    label: impl Into<SharedString>,
    width: f32,
    options: Vec<(SharedString, T)>,
    on_select: impl Fn(&mut Workspace, T) + Clone + 'static,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let is_open = cx.entity().read(cx).open_popup == Some(popup);
    let mut root = div()
        .relative()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .w(px(width))
        .h(px(20.0))
        .px_1()
        .rounded_sm()
        .bg(gpui::rgb(FIELD_BG))
        .text_size(px(11.0))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |ws, _e, _w, cx| ws.toggle_popup(popup, cx)),
        )
        .child(label.into())
        .child(crate::panels::icon("chevron-down", 11.0, TEXT_DIM));
    if is_open {
        let rows: Vec<gpui::AnyElement> = options
            .into_iter()
            .map(|(text, value)| {
                let selected = value == *current;
                let on_select = on_select.clone();
                div()
                    .px_2()
                    .h(px(20.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .text_size(px(11.0))
                    .bg(gpui::rgb(if selected { ACCENT } else { POPUP_BG }))
                    .hover(move |s| if selected { s } else { s.bg(gpui::rgb(HOVER)) })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |ws, _e, _w, cx| {
                            ws.close_popup(cx);
                            on_select(ws, value.clone());
                            cx.notify();
                        }),
                    )
                    .child(text)
                    .into_any_element()
            })
            .collect();
        root = root.child(gpui::deferred(
            div()
                .id("dropdown-items")
                .absolute()
                .top(px(22.0))
                .left_0()
                .w(px(width.max(140.0)))
                .max_h(px(300.0))
                .overflow_y_scroll()
                .py_1()
                .bg(gpui::rgb(POPUP_BG))
                .border_1()
                .border_color(gpui::rgb(EDGE))
                .rounded_sm()
                .shadow_lg()
                .occlude()
                .on_mouse_down_out(cx.listener(|ws, _e, _w, cx| ws.close_popup(cx)))
                .children(rows),
        ));
    }
    root
}

/// A labelled row inside a dialog.
pub fn field_row(label: impl Into<SharedString>, control: impl IntoElement) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap_3()
        .h(px(26.0))
        .child(
            div()
                .w(px(110.0))
                .flex_none()
                .text_size(px(12.0))
                .text_color(gpui::rgb(TEXT_DIM))
                .child(label.into()),
        )
        .child(control)
}

/// Centred modal frame with a title bar and an action row.
pub fn modal_frame(
    title: impl Into<SharedString>,
    width: f32,
    body: impl IntoElement,
    actions: impl IntoElement,
) -> impl IntoElement {
    div()
        .absolute()
        .top_0()
        .left_0()
        .right_0()
        .bottom_0()
        .flex()
        .items_center()
        .justify_center()
        .bg(gpui::rgba(0x00000080))
        .child(
            div()
                .flex()
                .flex_col()
                .w(px(width))
                .p_3()
                .gap_2()
                .rounded_md()
                .bg(gpui::rgb(PANEL_BG))
                .border_1()
                .border_color(gpui::rgb(EDGE))
                .shadow_lg()
                .text_color(gpui::rgb(TEXT))
                .child(
                    div()
                        .text_size(px(13.0))
                        .pb_1()
                        .border_b_1()
                        .border_color(gpui::rgb(0x2A2A2A))
                        .child(title.into()),
                )
                .child(div().flex().flex_col().gap_1().child(body))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .justify_end()
                        .gap_2()
                        .pt_2()
                        .child(actions),
                ),
        )
}

use gpui::prelude::FluentBuilder as _;
