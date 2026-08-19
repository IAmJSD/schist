//! Docked panels: toolbar, layers, history, color, status bar.
//!
//! These render directly from the Workspace (panel-as-plugin trait objects
//! arrive when panels need to come from third parties; the registry shape
//! in plugin-api already reserves the seam).

use crate::workspace::Workspace;
use gpui::{
    div, px, Context, InteractiveElement as _, IntoElement, MouseButton, ParentElement as _,
    StatefulInteractiveElement as _, Styled,
};
use photoslop_color::Rgba;
use photoslop_core::{Layer, LayerId, LayerKind};

fn swatch_hex(c: Rgba) -> gpui::Rgba {
    let [r, g, b, _] = c.to_u8();
    gpui::rgb(((r as u32) << 16) | ((g as u32) << 8) | b as u32)
}

pub fn toolbar(ws: &mut Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let active = ws.editor.active_tool;
    let tools: Vec<(&'static str, &'static str, &'static str)> = ws
        .registry
        .tools()
        .map(|t| (t.id(), t.icon(), t.name()))
        .collect();
    div()
        .flex()
        .flex_col()
        .w(px(44.0))
        .flex_none()
        .bg(gpui::rgb(0x1A1A1A))
        .border_r_1()
        .border_color(gpui::rgb(0x111111))
        .children(tools.into_iter().map(|(id, icon, _name)| {
            let is_active = id == active;
            div()
                .flex()
                .items_center()
                .justify_center()
                .w_full()
                .h(px(36.0))
                .text_size(px(16.0))
                .when_active(is_active)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |ws, _ev, _w, cx| ws.activate_tool(id, cx)),
                )
                .child(icon)
        }))
        .child(color_wells(ws, cx))
}

trait ActiveExt: Styled + Sized {
    fn when_active(self, active: bool) -> Self {
        if active {
            self.bg(gpui::rgb(0x3A6EA5))
        } else {
            self
        }
    }
}
impl<T: Styled> ActiveExt for T {}

fn color_wells(ws: &Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .items_center()
        .gap_1()
        .pt_2()
        .child(
            div()
                .size(px(24.0))
                .bg(swatch_hex(ws.editor.foreground))
                .border_1()
                .border_color(gpui::rgb(0xFFFFFF)),
        )
        .child(
            div()
                .size(px(24.0))
                .bg(swatch_hex(ws.editor.background))
                .border_1()
                .border_color(gpui::rgb(0x888888))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|ws, _ev, _w, cx| {
                        std::mem::swap(&mut ws.editor.foreground, &mut ws.editor.background);
                        cx.notify();
                    }),
                ),
        )
}

pub fn side_panels(ws: &mut Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .w(px(260.0))
        .flex_none()
        .bg(gpui::rgb(0x1A1A1A))
        .border_l_1()
        .border_color(gpui::rgb(0x111111))
        .child(color_palette(cx))
        .child(layers_panel(ws, cx))
        .child(history_panel(ws, cx))
}

const PALETTE: [u32; 16] = [
    0x000000, 0xFFFFFF, 0x808080, 0xC0C0C0, 0xE81E25, 0xFF7F27, 0xFFF200, 0x22B14C, 0x00A2E8,
    0x3F48CC, 0xA349A4, 0xB97A57, 0xFFAEC9, 0xFFC90E, 0xB5E61D, 0x99D9EA,
];

fn color_palette(cx: &mut Context<Workspace>) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .p_2()
        .gap_1()
        .child(panel_title("Color"))
        .child(
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .gap_1()
                .children(PALETTE.map(|hex| {
                    div()
                        .size(px(22.0))
                        .bg(gpui::rgb(hex))
                        .border_1()
                        .border_color(gpui::rgb(0x333333))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |ws, ev: &gpui::MouseDownEvent, _w, cx| {
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
}

fn panel_title(name: &'static str) -> impl IntoElement {
    div()
        .text_size(px(11.0))
        .text_color(gpui::rgb(0x9A9A9A))
        .pb_1()
        .child(name.to_uppercase())
}

/// Flattened view of the layer tree: (layer id, depth, is_group, name,
/// visible, is_active), TOP-first for display.
fn flatten_layers(
    layers: &[Layer],
    depth: usize,
    active: Option<LayerId>,
    out: &mut Vec<(LayerId, usize, bool, String, bool, bool)>,
) {
    for layer in layers.iter().rev() {
        out.push((
            layer.id,
            depth,
            layer.is_group(),
            layer.name.clone(),
            layer.visible,
            Some(layer.id) == active,
        ));
        if let LayerKind::Group(g) = &layer.kind {
            if g.open {
                flatten_layers(&g.children, depth + 1, active, out);
            }
        }
    }
}

fn layers_panel(ws: &mut Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let mut rows = Vec::new();
    if let Some(doc) = &ws.doc {
        flatten_layers(&doc.tree.layers, 0, doc.active_layer, &mut rows);
    }
    let opacity_line = ws
        .doc
        .as_ref()
        .and_then(|d| d.active_layer.and_then(|id| d.tree.find(id)))
        .map(|l| format!("{} · {:.0}%", l.blend.display_name(), l.opacity * 100.0))
        .unwrap_or_default();

    div()
        .flex()
        .flex_col()
        .flex_grow()
        .min_h(px(0.0))
        .p_2()
        .gap_1()
        .child(panel_title("Layers"))
        .child(
            div()
                .text_size(px(11.0))
                .text_color(gpui::rgb(0x9A9A9A))
                .child(opacity_line),
        )
        .child(
            div()
                .id("layers-scroll")
                .flex()
                .flex_col()
                .flex_grow()
                .overflow_y_scroll()
                .children(
                    rows.into_iter()
                        .map(|(id, depth, is_group, name, visible, active)| {
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap_1()
                                .px_1()
                                .h(px(24.0))
                                .when_active(active)
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |ws, _e, _w, cx| {
                                        if let Some(doc) = &mut ws.doc {
                                            doc.active_layer = Some(id);
                                        }
                                        cx.notify();
                                    }),
                                )
                                .child(
                                    // Visibility eye.
                                    div()
                                        .w(px(18.0))
                                        .text_size(px(12.0))
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(move |ws, _e, _w, cx| {
                                                if let Some(doc) = &mut ws.doc {
                                                    let mut edit =
                                                        doc.begin_edit("Toggle Visibility");
                                                    edit.change_props(id, |l| {
                                                        l.visible = !l.visible
                                                    });
                                                    edit.commit();
                                                }
                                                ws.after_change(cx);
                                                cx.stop_propagation();
                                            }),
                                        )
                                        .child(if visible { "👁" } else { " " }),
                                )
                                .child(
                                    div().pl(px(depth as f32 * 12.0)).text_size(px(12.0)).child(
                                        format!("{}{}", if is_group { "▸ " } else { "" }, name),
                                    ),
                                )
                        }),
                ),
        )
}

fn history_panel(ws: &mut Workspace, _cx: &mut Context<Workspace>) -> impl IntoElement {
    let entries: Vec<String> = ws
        .doc
        .as_ref()
        .map(|d| d.history.entries().iter().map(|e| e.name.clone()).collect())
        .unwrap_or_default();
    let n = entries.len();
    div()
        .flex()
        .flex_col()
        .h(px(160.0))
        .flex_none()
        .p_2()
        .gap_1()
        .border_t_1()
        .border_color(gpui::rgb(0x111111))
        .child(panel_title("History"))
        .child(
            div()
                .id("history-scroll")
                .flex()
                .flex_col()
                .overflow_y_scroll()
                .flex_grow()
                .children(entries.into_iter().enumerate().map(move |(i, name)| {
                    div()
                        .px_1()
                        .h(px(20.0))
                        .text_size(px(12.0))
                        .when_active(i + 1 == n)
                        .child(name)
                })),
        )
}

pub fn status_bar(ws: &Workspace) -> impl IntoElement {
    let title = ws
        .doc
        .as_ref()
        .map(|d| {
            format!(
                "{}{}  {}×{}",
                d.title,
                if d.dirty { " •" } else { "" },
                d.width,
                d.height
            )
        })
        .unwrap_or_else(|| "No document".into());
    let zoom = format!("{:.0}%", ws.zoom * 100.0);
    let brush = format!("⌀{:.0}px", ws.editor.brush_size);
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_4()
        .h(px(24.0))
        .flex_none()
        .px_2()
        .bg(gpui::rgb(0x161616))
        .border_t_1()
        .border_color(gpui::rgb(0x111111))
        .text_size(px(11.0))
        .text_color(gpui::rgb(0x9A9A9A))
        .child(title)
        .child(zoom)
        .child(brush)
        .child(div().flex_grow())
        .child(ws.status.clone())
}
