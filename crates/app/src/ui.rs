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
use std::cell::RefCell;
use std::rc::Rc;

/// The chrome colours for one theme. Everything that isn't document
/// content draws from here; the active set is swapped by [`set_light`].
pub struct Palette {
    /// The window shell behind the panels.
    pub window_bg: u32,
    /// The area surrounding the document canvas.
    pub canvas_bg: u32,
    pub panel_bg: u32,
    /// Recessed strips: the document tab bar, the curve editor well.
    pub deep_bg: u32,
    pub status_bg: u32,
    pub ruler_bg: u32,
    pub field_bg: u32,
    pub popup_bg: u32,
    /// Small inline controls: step buttons, the active tab, badges.
    pub control_bg: u32,
    pub button_bg: u32,
    pub button_hover: u32,
    /// Row hover inside menus, popups and panels.
    pub hover: u32,
    /// Hairlines inside a panel (separators, section borders).
    pub divider: u32,
    /// Borders around fields, popups and modals.
    pub edge: u32,
    /// The border between panels and the shell.
    pub panel_edge: u32,
    /// Grid lines drawn on `deep_bg` (curve editor).
    pub grid: u32,
    pub text: u32,
    pub text_dim: u32,
    pub text_faint: u32,
    pub accent: u32,
    pub accent_hover: u32,
    /// Text and icons drawn on top of `accent`.
    pub accent_text: u32,
    /// Selected rows that keep their own text colour (lists, tiles).
    pub selection_bg: u32,
}

pub const DARK: Palette = Palette {
    window_bg: 0x1E1E1E,
    canvas_bg: 0x262626,
    panel_bg: 0x1A1A1A,
    deep_bg: 0x141414,
    status_bg: 0x161616,
    ruler_bg: 0x202020,
    field_bg: 0x0E0E0E,
    popup_bg: 0x242424,
    control_bg: 0x2A2A2A,
    button_bg: 0x333333,
    button_hover: 0x3E3E3E,
    hover: 0x2E2E2E,
    divider: 0x2A2A2A,
    edge: 0x3A3A3A,
    panel_edge: 0x111111,
    grid: 0x262626,
    text: 0xD8D8D8,
    text_dim: 0x9A9A9A,
    text_faint: 0x666666,
    accent: 0x3A6EA5,
    accent_hover: 0x4A80BC,
    accent_text: 0xFFFFFF,
    selection_bg: 0x2F5B8C,
};

pub const LIGHT: Palette = Palette {
    window_bg: 0xE8E8E8,
    canvas_bg: 0xB4B4B4,
    panel_bg: 0xF0F0F0,
    deep_bg: 0xE0E0E0,
    status_bg: 0xE4E4E4,
    ruler_bg: 0xE6E6E6,
    field_bg: 0xFFFFFF,
    popup_bg: 0xFAFAFA,
    control_bg: 0xD6D6D6,
    button_bg: 0xD0D0D0,
    button_hover: 0xC2C2C2,
    hover: 0xDCDCDC,
    divider: 0xD4D4D4,
    edge: 0xB8B8B8,
    panel_edge: 0xC4C4C4,
    grid: 0xC8C8C8,
    text: 0x1C1C1C,
    text_dim: 0x5A5A5A,
    text_faint: 0x9E9E9E,
    accent: 0x3A6EA5,
    accent_hover: 0x2E5E95,
    accent_text: 0xFFFFFF,
    selection_bg: 0xB8D2EE,
};

static LIGHT_THEME: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Select the palette that [`palette`] returns. `Workspace::render` calls
/// this every frame from the persisted preference, so widgets built during
/// that render (and canvas paint callbacks after it) all agree.
pub fn set_light(light: bool) {
    LIGHT_THEME.store(light, std::sync::atomic::Ordering::Relaxed);
}

pub fn palette() -> &'static Palette {
    if LIGHT_THEME.load(std::sync::atomic::Ordering::Relaxed) {
        &LIGHT
    } else {
        &DARK
    }
}

/// A labelled push button.
/// What a dialog does when the user presses Enter.
pub type DialogAction = Rc<dyn Fn(&mut Workspace, &mut gpui::Window, &mut Context<Workspace>)>;

thread_local! {
    /// The primary button built most recently.
    ///
    /// A dialog's default action is, by definition, whatever its primary
    /// button does, and the buttons are built as plain closures deep
    /// inside each dialog body with no path back to the workspace. Rather
    /// than thread an out-parameter through every dialog function, the
    /// primary button leaves its handler here and `dialogs::render` --
    /// which brackets the whole build and does hold `&mut Workspace` --
    /// picks it up. GPUI renders on one thread, synchronously, so the
    /// slot is only ever live for the duration of one dialog build.
    static DEFAULT_ACTION: RefCell<Option<DialogAction>> = const { RefCell::new(None) };
}

/// Start a dialog build: forget any previous dialog's default action.
pub fn reset_default_action() {
    DEFAULT_ACTION.with(|slot| *slot.borrow_mut() = None);
}

/// End a dialog build: take whatever its primary button registered.
pub fn take_default_action() -> Option<DialogAction> {
    DEFAULT_ACTION.with(|slot| slot.borrow_mut().take())
}

pub fn button(
    label: impl Into<SharedString>,
    primary: bool,
    on_click: impl Fn(&mut Workspace, &mut gpui::Window, &mut Context<Workspace>) + 'static,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let on_click: DialogAction = Rc::new(on_click);
    if primary {
        let action = on_click.clone();
        DEFAULT_ACTION.with(|slot| *slot.borrow_mut() = Some(action));
    }
    div()
        .flex()
        .items_center()
        .justify_center()
        .h(px(24.0))
        .px_3()
        .rounded_sm()
        .text_size(px(12.0))
        .cursor_pointer()
        .bg(gpui::rgb(if primary {
            palette().accent
        } else {
            palette().button_bg
        }))
        .text_color(gpui::rgb(if primary {
            palette().accent_text
        } else {
            palette().text
        }))
        .hover(|s| {
            s.bg(gpui::rgb(if primary {
                palette().accent_hover
            } else {
                palette().button_hover
            }))
        })
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |ws, _e, window, cx| on_click(ws, window, cx)),
        )
        .child(label.into())
}

/// Everything a [`num_field`] needs to draw itself.
///
/// State is passed in rather than read from the entity: these render
/// *during* `Workspace::render`, where reading the entity panics on the
/// outstanding mutable borrow.
pub struct NumField {
    pub id: &'static str,
    pub value: f32,
    pub suffix: &'static str,
    pub step: f32,
    pub focused: bool,
    pub buffer: String,
}

/// A numeric field: click to focus and type digits, or use the ± buttons.
pub fn num_field(
    field: NumField,
    on_change: impl Fn(&mut Workspace, f32) + Clone + 'static,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let NumField {
        id,
        value,
        suffix,
        step,
        focused,
        buffer,
    } = field;
    let committed = if value.fract().abs() < 0.01 {
        format!("{value:.0}")
    } else {
        format!("{value:.2}")
    };
    let shown = if focused && !buffer.is_empty() {
        buffer
    } else {
        committed.clone()
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
                .bg(gpui::rgb(palette().field_bg))
                .border_1()
                .border_color(gpui::rgb(if focused {
                    palette().accent
                } else {
                    palette().field_bg
                }))
                .text_size(px(11.0))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |ws, _e, _w, cx| {
                        ws.focus_field(id, committed.clone());
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
        .bg(gpui::rgb(palette().control_bg))
        .hover(|s| s.bg(gpui::rgb(palette().button_hover)))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |ws, _e, _w, cx| {
                on_click(ws);
                cx.notify();
            }),
        )
        .child(crate::panels::icon(icon_name, 11.0, palette().text))
}

/// A checkbox with a label to its right.
pub fn checkbox(
    label: impl Into<SharedString>,
    checked: bool,
    on_toggle: impl Fn(&mut Workspace, &mut Context<Workspace>) + 'static,
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
                on_toggle(ws, cx);
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
                .bg(gpui::rgb(if checked {
                    palette().accent
                } else {
                    palette().field_bg
                }))
                .border_1()
                .border_color(gpui::rgb(palette().edge))
                .when_some(checked.then_some(()), |d, _| {
                    d.child(crate::panels::icon("check", 10.0, palette().accent_text))
                }),
        )
        .child(label.into())
}

/// Placement and state for a [`dropdown`].
pub struct Dropdown<T> {
    pub popup: Popup,
    pub is_open: bool,
    pub current: T,
    pub label: SharedString,
    pub width: f32,
    pub options: Vec<(SharedString, T)>,
}

/// A dropdown button that opens its popup with the given options.
pub fn dropdown<T: Clone + PartialEq + 'static>(
    spec: Dropdown<T>,
    on_select: impl Fn(&mut Workspace, T, &mut Context<Workspace>) + Clone + 'static,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let Dropdown {
        popup,
        is_open,
        current,
        label,
        width,
        options,
    } = spec;
    let current = &current;
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
        .bg(gpui::rgb(palette().field_bg))
        .text_size(px(11.0))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |ws, _e, _w, cx| ws.toggle_popup(popup, cx)),
        )
        .child(label)
        .child(crate::panels::icon(
            "chevron-down",
            11.0,
            palette().text_dim,
        ));
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
                    .bg(gpui::rgb(if selected {
                        palette().accent
                    } else {
                        palette().popup_bg
                    }))
                    .text_color(gpui::rgb(if selected {
                        palette().accent_text
                    } else {
                        palette().text
                    }))
                    .hover(move |s| {
                        if selected {
                            s
                        } else {
                            s.bg(gpui::rgb(palette().hover))
                        }
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |ws, _e, _w, cx| {
                            ws.close_popup(cx);
                            on_select(ws, value.clone(), cx);
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
                .bg(gpui::rgb(palette().popup_bg))
                .text_color(gpui::rgb(palette().text))
                .border_1()
                .border_color(gpui::rgb(palette().edge))
                .rounded_sm()
                .shadow_lg()
                .occlude()
                .on_mouse_down_out(cx.listener(|ws, _e, _w, cx| ws.close_popup(cx)))
                .children(rows),
        ));
    }
    root
}

/// A bare slider track that reports a 0..1 ratio while dragged. Panels and
/// dialogs both build on this.
pub fn slider_track(
    id: &'static str,
    ratio: f32,
    width: f32,
    on_change: impl Fn(&mut Workspace, f32, &mut Context<Workspace>) + Clone + 'static,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let entity = cx.entity();
    let down = on_change.clone();
    let moved = on_change;
    div()
        .relative()
        .w(px(width))
        .h(px(12.0))
        .flex_none()
        .rounded_sm()
        .bg(gpui::rgb(palette().field_bg))
        .child(
            div()
                .absolute()
                .left_0()
                .top_0()
                .bottom_0()
                .w(px(width * ratio.clamp(0.0, 1.0)))
                .rounded_sm()
                .bg(gpui::rgb(palette().accent)),
        )
        .child(
            gpui::canvas(
                move |bounds, _window, cx| {
                    entity.update(cx, |ws, _| ws.record_slider_bounds(id, bounds));
                },
                |_, _, _, _| {},
            )
            .absolute()
            .size_full(),
        )
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |ws, ev: &gpui::MouseDownEvent, _w, cx| {
                ws.begin_slider(id, ratio);
                if let Some(r) = ws.slider_ratio(id, ev.position) {
                    down(ws, r, cx);
                }
                cx.notify();
            }),
        )
        .on_mouse_move(cx.listener(move |ws, ev: &gpui::MouseMoveEvent, _w, cx| {
            if ev.pressed_button == Some(MouseButton::Left) && ws.dragging_slider(id) {
                if let Some(r) = ws.slider_ratio(id, ev.position) {
                    moved(ws, r, cx);
                    cx.notify();
                }
            }
        }))
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(move |ws, _ev: &gpui::MouseUpEvent, _w, _cx| {
                ws.end_slider(id);
            }),
        )
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
                .text_color(gpui::rgb(palette().text_dim))
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
        // The backdrop must swallow the pointer, or the canvas underneath
        // keeps its hit box and the active tool edits the document while
        // the dialog is open -- dragging a slider would also drag the layer.
        .occlude()
        .child(
            div()
                .flex()
                .flex_col()
                .w(px(width))
                .p_3()
                .gap_2()
                .rounded_md()
                .bg(gpui::rgb(palette().panel_bg))
                .border_1()
                .border_color(gpui::rgb(palette().edge))
                .shadow_lg()
                .text_color(gpui::rgb(palette().text))
                .child(
                    div()
                        .text_size(px(13.0))
                        .pb_1()
                        .border_b_1()
                        .border_color(gpui::rgb(palette().divider))
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
