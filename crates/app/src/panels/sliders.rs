//! The panel sliders and what each one reads and writes.

use super::*;

#[derive(Clone, Copy, PartialEq)]
pub enum SliderTarget {
    /// A control the active tool declared for itself, mapped from the
    /// slider's 0..=1 ratio into the option's own range.
    ToolOption {
        key: &'static str,
        min: f32,
        max: f32,
    },
    BrushSize,
    BrushHardness,
    ToolOpacity,
    LayerOpacity(LayerId),
    ForegroundR,
    ForegroundG,
    ForegroundB,
}

pub(super) fn slider_get(ws: &Workspace, target: SliderTarget) -> f32 {
    match target {
        SliderTarget::ToolOption { key, min, max } => {
            let v = ws
                .registry
                .tools()
                .find(|t| t.id() == ws.editor.active_tool)
                .and_then(|t| t.options().into_iter().find(|o| o.key == key))
                .map(|o| o.value.num())
                .unwrap_or(min);
            ((v - min) / (max - min).max(1e-6)).clamp(0.0, 1.0)
        }
        SliderTarget::BrushSize => ((ws.editor.brush_size - 1.0) / 299.0).clamp(0.0, 1.0),
        SliderTarget::BrushHardness => ws.editor.brush_hardness,
        SliderTarget::ToolOpacity => ws.editor.tool_opacity,
        SliderTarget::LayerOpacity(id) => ws
            .doc
            .as_ref()
            .and_then(|d| d.tree.find(id))
            .map(|l| l.opacity)
            .unwrap_or(1.0),
        SliderTarget::ForegroundR => ws.editor.foreground.r,
        SliderTarget::ForegroundG => ws.editor.foreground.g,
        SliderTarget::ForegroundB => ws.editor.foreground.b,
    }
}

pub(super) fn slider_set(
    ws: &mut Workspace,
    target: SliderTarget,
    ratio: f32,
    cx: &mut Context<Workspace>,
) {
    match target {
        SliderTarget::ToolOption { key, min, max } => ws.set_tool_option(
            key,
            schist_plugin_api::OptionValue::Num(min + ratio * (max - min)),
            cx,
        ),
        SliderTarget::BrushSize => ws.editor.brush_size = 1.0 + ratio * 299.0,
        SliderTarget::BrushHardness => ws.editor.brush_hardness = ratio,
        SliderTarget::ToolOpacity => ws.editor.tool_opacity = ratio,
        SliderTarget::LayerOpacity(id) => ws.set_layer_opacity_live(id, ratio),
        SliderTarget::ForegroundR => ws.editor.foreground.r = ratio,
        SliderTarget::ForegroundG => ws.editor.foreground.g = ratio,
        SliderTarget::ForegroundB => ws.editor.foreground.b = ratio,
    }
    if matches!(target, SliderTarget::LayerOpacity(_)) {
        ws.after_change(cx);
    } else {
        cx.notify();
    }
}

/// A horizontal slider. The track's live bounds are recorded via a nested
/// canvas so mouse positions can be mapped back to a 0..=1 ratio.
pub(super) fn slider(
    id: &'static str,
    label: &'static str,
    display: String,
    target: SliderTarget,
    ws: &Workspace,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let ratio = slider_get(ws, target);
    let entity = cx.entity();
    let track = div()
        .relative()
        .w(px(72.0))
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
                .w(px(72.0 * ratio))
                .rounded_sm()
                .bg(gpui::rgb(palette().accent)),
        )
        .child(
            canvas(
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
            cx.listener(move |ws, ev: &MouseDownEvent, _w, cx| {
                ws.begin_slider(id, slider_get(ws, target));
                if let Some(r) = ws.slider_ratio(id, ev.position) {
                    slider_set(ws, target, r, cx);
                }
            }),
        )
        .on_mouse_move(cx.listener(move |ws, ev: &MouseMoveEvent, _w, cx| {
            if ev.pressed_button == Some(MouseButton::Left) && ws.dragging_slider(id) {
                if let Some(r) = ws.slider_ratio(id, ev.position) {
                    slider_set(ws, target, r, cx);
                }
            }
        }))
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(move |ws, _ev: &MouseUpEvent, _w, cx| {
                if let Some(before) = ws.end_slider(id) {
                    if let SliderTarget::LayerOpacity(layer) = target {
                        ws.commit_layer_opacity(layer, before, cx);
                    }
                }
            }),
        );
    let mut row = div().flex().flex_row().items_center().gap_1();
    if !label.is_empty() {
        row = row.child(
            div()
                .text_size(px(11.0))
                .text_color(gpui::rgb(palette().text_dim))
                .child(label),
        );
    }
    row.child(track).child(
        div()
            .w(px(34.0))
            .flex_none()
            .text_size(px(11.0))
            .child(display),
    )
}

// ===== document tabs =====
