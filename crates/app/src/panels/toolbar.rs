//! The toolbar, its flyouts, and the tool options bar above the canvas.

use super::*;

/// One toolbar slot: its group, the icon of the tool currently showing,
/// whether that tool is active, whether the group has more than one tool,
/// and the name and shortcut for its hover label.
pub(super) type ToolSlot = (
    &'static str,
    &'static str,
    bool,
    bool,
    String,
    Option<SharedString>,
);

pub fn tool_options_bar(ws: &mut Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let tool_id = ws.editor.active_tool;
    let (tool_icon, tool_name) = ws
        .registry
        .tools()
        .find(|t| t.id() == tool_id)
        .map(|t| (t.icon(), t.name()))
        .unwrap_or(("move", "Move"));
    let is_paint = matches!(tool_id, "brush" | "pencil" | "eraser");

    let mut bar = div()
        .flex()
        .flex_row()
        .items_center()
        .gap_4()
        .h(px(32.0))
        .flex_none()
        .px_3()
        .bg(gpui::rgb(palette().panel_bg))
        .border_b_1()
        .border_color(gpui::rgb(palette().panel_edge))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .w(px(130.0))
                .flex_none()
                .child(icon(tool_icon, 15.0, palette().text))
                .child(div().text_size(px(12.0)).child(tool_name)),
        );
    if is_paint {
        bar = bar
            .child(slider(
                "opt-size",
                "Size",
                format!("{:.0}px", ws.editor.brush_size),
                SliderTarget::BrushSize,
                ws,
                cx,
            ))
            .child(slider(
                "opt-hard",
                "Hardness",
                format!("{:.0}%", ws.editor.brush_hardness * 100.0),
                SliderTarget::BrushHardness,
                ws,
                cx,
            ));
    }
    if tool_id == "note" {
        bar = bar.child(note_options(ws, cx));
    }
    bar = bar.child(slider(
        "opt-opacity",
        "Opacity",
        format!("{:.0}%", ws.editor.tool_opacity * 100.0),
        SliderTarget::ToolOpacity,
        ws,
        cx,
    ));
    // Whatever else the active tool asked for.
    for opt in ws
        .registry
        .tools()
        .find(|t| t.id() == tool_id)
        .map(|t| t.options())
        .unwrap_or_default()
    {
        bar = bar.child(tool_option_control(ws, opt, cx));
    }
    bar
}

/// Render one plugin-declared option. The shell knows the three kinds, not
/// the tools.
pub(super) fn tool_option_control(
    ws: &Workspace,
    opt: schist_plugin_api::ToolOption,
    cx: &mut Context<Workspace>,
) -> gpui::AnyElement {
    use schist_plugin_api::OptionKind;
    let key = opt.key;
    match opt.kind {
        OptionKind::Slider { min, max, suffix } => {
            let v = opt.value.num();
            // Coarse ranges read better without decimals.
            let display = if max - min > 20.0 {
                format!("{v:.0}{suffix}")
            } else {
                format!("{v:.1}{suffix}")
            };
            slider(
                key,
                opt.label,
                display,
                SliderTarget::ToolOption { key, min, max },
                ws,
                cx,
            )
            .into_any_element()
        }
        OptionKind::Toggle => {
            let on = opt.value.bool();
            ui::checkbox(
                opt.label,
                on,
                move |ws, cx| {
                    ws.set_tool_option(key, schist_plugin_api::OptionValue::Bool(!on), cx)
                },
                cx,
            )
            .into_any_element()
        }
        OptionKind::Choice(labels) => {
            let current = opt.value.index().min(labels.len().saturating_sub(1));
            // Wide enough for the longest thing it can say, so a dropdown
            // never has to truncate its own value.
            let longest = labels.iter().map(|l| l.chars().count()).max().unwrap_or(0);
            let width = (longest as f32 * 6.2 + 34.0).clamp(80.0, 210.0);
            let spec = ui::Dropdown {
                popup: Popup::Field(key),
                is_open: ws.open_popup == Some(Popup::Field(key)),
                current,
                label: labels.get(current).copied().unwrap_or("").into(),
                width,
                options: labels
                    .iter()
                    .enumerate()
                    .map(|(i, l)| (SharedString::from(*l), i))
                    .collect(),
            };
            let on_select = move |ws: &mut Workspace, i, cx: &mut Context<Workspace>| {
                ws.set_tool_option(key, schist_plugin_api::OptionValue::Choice(i), cx)
            };
            // The font menu's rows are font names, so show each in itself.
            let control = if key == "type-family" {
                ui::font_dropdown(&ws.dropdown_scroll, spec, on_select, cx).into_any_element()
            } else {
                ui::dropdown(&ws.dropdown_scroll, spec, on_select, cx).into_any_element()
            };
            // Sliders carry their own label; a dropdown does not, and an
            // unlabelled one reading "Point Sample" does not say what it
            // is choosing.
            if opt.label.is_empty() {
                return control.into_any_element();
            }
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(gpui::rgb(palette().text_dim))
                        .child(opt.label),
                )
                .child(control)
                .into_any_element()
        }
    }
}

pub fn toolbar(ws: &mut Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let active = ws.editor.active_tool;
    // One slot per group, showing whichever tool that group last used —
    // Photoshop's nested tools, so twenty tools take eleven slots.
    let slots: Vec<ToolSlot> = ws
        .tool_groups
        .clone()
        .into_iter()
        .map(|(group, tools)| {
            let shown = ws.group_tool(group);
            let (icon, name, key) = ws
                .registry
                .tool_mut(shown)
                .map(|t| (t.icon(), t.name().to_string(), t.shortcut()))
                .unwrap_or(("move", "Move".into(), None));
            let hint = key.map(|k| SharedString::from(k.to_uppercase()));
            (
                group,
                icon,
                tools.contains(&active),
                tools.len() > 1,
                name,
                hint,
            )
        })
        .collect();

    div()
        .flex()
        .flex_col()
        .w(px(40.0))
        .flex_none()
        .items_center()
        .bg(gpui::rgb(palette().panel_bg))
        .border_r_1()
        .border_color(gpui::rgb(palette().panel_edge))
        .pt_1()
        .children(slots.into_iter().map(
            |(group, icon_name, is_active, has_siblings, name, hint)| {
                div()
                    .id(SharedString::from(format!("tool-slot-{group}")))
                    .relative()
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(30.0))
                    .my(px(1.0))
                    .rounded_sm()
                    .cursor_pointer()
                    .tooltip(ui::tip(name, hint))
                    .when_active(is_active)
                    .hover(move |s| {
                        if is_active {
                            s
                        } else {
                            s.bg(gpui::rgb(palette().hover))
                        }
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |ws, ev: &MouseDownEvent, _w, cx| {
                            ws.press_tool_group(group, ev.position, cx);
                        }),
                    )
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(move |ws, _ev, _w, cx| {
                            ws.release_tool_group(group, cx);
                        }),
                    )
                    // Right-click opens the flyout immediately, for
                    // people who don't want to wait out the hold.
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |ws, ev: &MouseDownEvent, _w, cx| {
                            ws.open_tool_flyout(group, ev.position, cx);
                        }),
                    )
                    .child(icon(
                        icon_name,
                        16.0,
                        if is_active {
                            palette().accent_text
                        } else {
                            palette().text
                        },
                    ))
                    .children(has_siblings.then(|| {
                        // The corner mark that means "more tools here".
                        div()
                            .absolute()
                            .right(px(2.0))
                            .bottom(px(2.0))
                            .size(px(4.0))
                            .bg(gpui::rgb(if is_active {
                                palette().accent_text
                            } else {
                                palette().text_dim
                            }))
                    }))
            },
        ))
        .child(color_wells(ws, cx))
}

/// The flyout listing a group's tools, opened by holding or right-clicking
/// its toolbar slot.
pub fn tool_flyout(ws: &mut Workspace, cx: &mut Context<Workspace>) -> Option<gpui::AnyElement> {
    let (group, position) = ws.tool_flyout?;
    let active = ws.editor.active_tool;
    let shortcut = ws
        .group_shortcut(group)
        .map(|s| s.to_uppercase())
        .unwrap_or_default();
    let tools: Vec<&'static str> = ws
        .tool_groups
        .iter()
        .find(|(g, _)| *g == group)
        .map(|(_, t)| t.clone())
        .unwrap_or_default();
    let rows: Vec<gpui::AnyElement> = tools
        .into_iter()
        .map(|id| {
            let (name, icon_name) = ws
                .registry
                .tool_mut(id)
                .map(|t| (t.name(), t.icon()))
                .unwrap_or((id, "move"));
            let selected = id == active;
            let shortcut = shortcut.clone();
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .px_2()
                .h(px(24.0))
                .when_active(selected)
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
                        ws.close_tool_flyout(cx);
                        ws.activate_tool(id, cx);
                    }),
                )
                .child(icon(icon_name, 14.0, palette().text))
                .child(div().flex_grow().text_size(px(12.0)).child(name))
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(gpui::rgb(palette().text_dim))
                        .child(shortcut),
                )
                .into_any_element()
        })
        .collect();

    Some(
        deferred(
            div()
                .absolute()
                // Sits just right of the toolbar, level with the slot.
                .left(px(42.0))
                .top(px(f32::from(position.y) - 12.0))
                .w(px(200.0))
                .py_1()
                .bg(gpui::rgb(palette().popup_bg))
                .text_color(gpui::rgb(palette().text))
                .border_1()
                .border_color(gpui::rgb(palette().edge))
                .rounded_sm()
                .shadow_lg()
                .occlude()
                .on_mouse_down_out(cx.listener(|ws, _e, _w, cx| ws.close_tool_flyout(cx)))
                .children(rows),
        )
        .into_any_element(),
    )
}
