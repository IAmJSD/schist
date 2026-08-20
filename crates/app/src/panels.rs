//! UI chrome: menu bar, tool options bar, toolbar, layers/history/color
//! panels, status bar.
//!
//! These render directly from the Workspace (third-party panel plugins get
//! their seam in M9; the registry shape in plugin-api reserves it). Icons
//! are monochrome SVGs from the embedded asset source, tinted by text
//! color — no emoji.

use crate::workspace::{Modal, Popup, Workspace};
use gpui::{
    canvas, deferred, div, img, px, svg, Context, InteractiveElement as _, IntoElement,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement as _, RenderImage,
    StatefulInteractiveElement as _, Styled, Window,
};
use photoslop_color::Rgba;
use photoslop_core::{BlendMode, Layer, LayerId, LayerKind};
use std::sync::Arc;

const PANEL_BG: u32 = 0x1A1A1A;
const PANEL_EDGE: u32 = 0x111111;
const TEXT_DIM: u32 = 0x9A9A9A;
const TEXT: u32 = 0xD8D8D8;
const ACCENT: u32 = 0x3A6EA5;
const HOVER: u32 = 0x2E2E2E;
const POPUP_BG: u32 = 0x242424;

fn swatch_hex(c: Rgba) -> gpui::Rgba {
    let [r, g, b, _] = c.to_u8();
    gpui::rgb(((r as u32) << 16) | ((g as u32) << 8) | b as u32)
}

pub fn icon(name: &str, size: f32, color: u32) -> impl IntoElement {
    svg()
        .path(format!("icons/{name}.svg"))
        .size(px(size))
        .text_color(gpui::rgb(color))
}

trait ActiveExt: Styled + Sized {
    fn when_active(self, active: bool) -> Self {
        if active {
            self.bg(gpui::rgb(ACCENT))
        } else {
            self
        }
    }
}
impl<T: Styled> ActiveExt for T {}

// ===== menu bar =====

enum MenuEntry {
    /// A registered plugin command (label + keybind resolved from registry).
    Cmd(&'static str),
    /// An app-level item handled by the shell.
    App(&'static str, AppItem, Option<&'static str>),
    /// Create an adjustment layer of this kind.
    Adjustment(photoslop_core::AdjustmentKind),
    /// Open a registered filter's dialog.
    Filter(&'static str),
    Sep,
}

#[derive(Clone, Copy)]
enum AppItem {
    New,
    Open,
    Save,
    SaveAs,
    Quit,
    ZoomIn,
    ZoomOut,
    ZoomFit,
    ZoomActual,
    ImageSize,
    CanvasSize,
    FreeTransform,
    Crop,
}

fn menus() -> Vec<(&'static str, Vec<MenuEntry>)> {
    use AppItem::*;
    use MenuEntry::*;
    vec![
        (
            "File",
            vec![
                App("New", New, Some("cmd-n")),
                App("Open…", Open, Some("cmd-o")),
                App("Save", Save, Some("cmd-s")),
                App("Save As…", SaveAs, Some("cmd-shift-s")),
                Sep,
                App("Quit", Quit, Some("cmd-q")),
            ],
        ),
        (
            "Edit",
            vec![
                Cmd("edit.undo"),
                Cmd("edit.redo"),
                Sep,
                Cmd("edit.cut"),
                Cmd("edit.copy"),
                Cmd("edit.copy_merged"),
                Cmd("edit.paste"),
                Cmd("edit.paste_in_place"),
                Sep,
                Cmd("edit.fill_foreground"),
                Cmd("edit.fill_background"),
                Sep,
                App("Free Transform", FreeTransform, Some("cmd-t")),
            ],
        ),
        (
            "Image",
            vec![
                App("Image Size…", ImageSize, Some("cmd-alt-i")),
                App("Canvas Size…", CanvasSize, Some("cmd-alt-c")),
                Sep,
                App("Crop to Selection", Crop, None),
            ],
        ),
        (
            "Select",
            vec![
                Cmd("select.all"),
                Cmd("select.deselect"),
                Cmd("select.inverse"),
            ],
        ),
        (
            "Layer",
            vec![
                Cmd("layer.new"),
                Cmd("layer.duplicate"),
                Cmd("layer.delete"),
                Sep,
                Cmd("layer.group"),
                Cmd("layer.merge_down"),
                Cmd("layer.merge_visible"),
            ],
        ),
        (
            "Adjust",
            photoslop_adjustments::Params::creatable()
                .iter()
                .map(|&k| Adjustment(k))
                .collect(),
        ),
        ("Filter", filter_menu_entries()),
        (
            "View",
            vec![
                App("Zoom In", ZoomIn, Some("cmd-=")),
                App("Zoom Out", ZoomOut, Some("cmd--")),
                App("Fit on Screen", ZoomFit, Some("cmd-0")),
                App("100%", ZoomActual, Some("cmd-1")),
            ],
        ),
    ]
}

/// Filters grouped by category, in registration order.
fn filter_menu_entries() -> Vec<MenuEntry> {
    // The ids are static strings owned by the plugins; the menu resolves
    // names from the registry at render time.
    let mut out = Vec::new();
    for (i, (_, ids)) in FILTER_GROUPS.iter().enumerate() {
        if i > 0 {
            out.push(MenuEntry::Sep);
        }
        out.extend(ids.iter().map(|id| MenuEntry::Filter(id)));
    }
    out
}

/// Menu grouping for the built-in filters.
const FILTER_GROUPS: &[(&str, &[&str])] = &[
    (
        "Blur",
        &[
            "filter.gaussian_blur",
            "filter.box_blur",
            "filter.motion_blur",
        ],
    ),
    ("Sharpen", &["filter.sharpen", "filter.unsharp_mask"]),
    ("Noise", &["filter.add_noise", "filter.median"]),
];

fn keybind_hint(kb: Option<&str>) -> String {
    let Some(kb) = kb else { return String::new() };
    let kb = if cfg!(target_os = "macos") {
        kb.to_string()
    } else {
        kb.replace("cmd-", "ctrl-")
    };
    kb.split('-')
        .map(|part| match part {
            "cmd" => "Cmd".to_string(),
            "ctrl" => "Ctrl".to_string(),
            "shift" => "Shift".to_string(),
            "alt" => "Alt".to_string(),
            other if other.len() == 1 => other.to_uppercase(),
            other => {
                let mut c = other.chars();
                match c.next() {
                    Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                    None => String::new(),
                }
            }
        })
        .collect::<Vec<_>>()
        .join("+")
}

fn run_app_item(
    ws: &mut Workspace,
    item: AppItem,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    match item {
        AppItem::New => ws.new_document(),
        AppItem::Open => crate::keymap::open_file_dialog(ws, window, cx),
        AppItem::Save => ws.save_current(window, cx),
        AppItem::SaveAs => crate::keymap::save_file_dialog(ws, window, cx),
        AppItem::Quit => cx.quit(),
        AppItem::ZoomIn => ws.zoom_by(1.25, None),
        AppItem::ZoomOut => ws.zoom_by(0.8, None),
        AppItem::ZoomFit => ws.fit_to_view(),
        AppItem::ZoomActual => {
            ws.zoom = 1.0;
            ws.editor.zoom = 1.0;
        }
        AppItem::ImageSize => {
            if let Some(doc) = ws.doc.as_ref() {
                let modal = Modal::ImageSize {
                    width: doc.width,
                    height: doc.height,
                    filter: ws.editor.resample,
                    link: true,
                };
                ws.open_modal(modal, cx);
            }
        }
        AppItem::CanvasSize => {
            if let Some(doc) = ws.doc.as_ref() {
                let modal = Modal::CanvasSize {
                    width: doc.width,
                    height: doc.height,
                    anchor: (0.5, 0.5),
                };
                ws.open_modal(modal, cx);
            }
        }
        AppItem::FreeTransform => ws.activate_tool("transform", cx),
        AppItem::Crop => {
            let rect = ws
                .doc
                .as_ref()
                .filter(|d| !d.selection.is_empty())
                .map(|d| d.selection.bounds().intersect(&d.canvas_rect()));
            match rect {
                Some(rect) if !rect.is_empty() => {
                    if let Some(doc) = ws.doc.as_mut() {
                        photoslop_tools_transform::crop_to(doc, rect);
                    }
                    ws.after_change(cx);
                    ws.fit_to_view();
                }
                _ => ws.status = "Crop to Selection needs a selection".into(),
            }
        }
    }
    cx.notify();
}

fn menu_row(
    label: String,
    hint: String,
    on_click: impl Fn(&mut Workspace, &MouseDownEvent, &mut Window, &mut Context<Workspace>) + 'static,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .px_2()
        .h(px(24.0))
        .hover(|s| s.bg(gpui::rgb(ACCENT)))
        .on_mouse_down(MouseButton::Left, cx.listener(on_click))
        .child(div().text_size(px(12.0)).child(label))
        .child(
            div()
                .text_size(px(10.0))
                .text_color(gpui::rgb(TEXT_DIM))
                .child(hint),
        )
}

pub fn menu_bar(ws: &mut Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let open = ws.open_popup;
    div()
        .flex()
        .flex_row()
        .items_center()
        .h(px(28.0))
        .flex_none()
        .px_1()
        .bg(gpui::rgb(PANEL_BG))
        .border_b_1()
        .border_color(gpui::rgb(PANEL_EDGE))
        .children(
            menus()
                .into_iter()
                .enumerate()
                .map(|(i, (title, entries))| {
                    let is_open = open == Some(Popup::Menu(i));
                    let mut button = div()
                        .relative()
                        .flex()
                        .items_center()
                        .px_2()
                        .h(px(22.0))
                        .rounded_sm()
                        .text_size(px(12.0))
                        .when_active(is_open)
                        .hover(|s| s.bg(gpui::rgb(HOVER)))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |ws, _e, _w, cx| ws.toggle_popup(Popup::Menu(i), cx)),
                        )
                        .child(title);
                    if is_open {
                        let rows: Vec<gpui::AnyElement> = entries
                            .into_iter()
                            .map(|entry| match entry {
                                MenuEntry::Sep => div()
                                    .h(px(1.0))
                                    .my_1()
                                    .bg(gpui::rgb(0x3A3A3A))
                                    .into_any_element(),
                                MenuEntry::Cmd(id) => {
                                    let (label, hint) = ws
                                        .registry
                                        .command(id)
                                        .map(|c| (c.title.to_string(), keybind_hint(c.keybind)))
                                        .unwrap_or_else(|| (id.to_string(), String::new()));
                                    menu_row(
                                        label,
                                        hint,
                                        move |ws, _e, _w, cx| {
                                            ws.close_popup(cx);
                                            ws.run_command(id, cx);
                                        },
                                        cx,
                                    )
                                    .into_any_element()
                                }
                                MenuEntry::Adjustment(kind) => menu_row(
                                    kind.display_name().to_string(),
                                    String::new(),
                                    move |ws, _e, _w, cx| {
                                        ws.close_popup(cx);
                                        ws.add_adjustment(kind, cx);
                                    },
                                    cx,
                                )
                                .into_any_element(),
                                MenuEntry::Filter(id) => {
                                    let name = ws
                                        .registry
                                        .filters()
                                        .find(|f| f.id() == id)
                                        .map(|f| format!("{}…", f.name()))
                                        .unwrap_or_else(|| id.to_string());
                                    menu_row(
                                        name,
                                        String::new(),
                                        move |ws, _e, _w, cx| {
                                            ws.close_popup(cx);
                                            ws.open_filter_dialog(id, cx);
                                        },
                                        cx,
                                    )
                                    .into_any_element()
                                }
                                MenuEntry::App(label, item, kb) => menu_row(
                                    label.to_string(),
                                    keybind_hint(kb),
                                    move |ws, _e, window, cx| {
                                        ws.close_popup(cx);
                                        run_app_item(ws, item, window, cx);
                                    },
                                    cx,
                                )
                                .into_any_element(),
                            })
                            .collect();
                        button = button.child(deferred(
                            div()
                                .absolute()
                                .top(px(24.0))
                                .left_0()
                                .w(px(230.0))
                                .py_1()
                                .bg(gpui::rgb(POPUP_BG))
                                .border_1()
                                .border_color(gpui::rgb(0x3A3A3A))
                                .rounded_sm()
                                .shadow_lg()
                                .occlude()
                                .on_mouse_down_out(cx.listener(|ws, _e, _w, cx| ws.close_popup(cx)))
                                .children(rows),
                        ));
                    }
                    button
                }),
        )
}

// ===== sliders =====

#[derive(Clone, Copy, PartialEq)]
pub enum SliderTarget {
    BrushSize,
    BrushHardness,
    ToolOpacity,
    LayerOpacity(LayerId),
    ForegroundR,
    ForegroundG,
    ForegroundB,
}

fn slider_get(ws: &Workspace, target: SliderTarget) -> f32 {
    match target {
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

fn slider_set(ws: &mut Workspace, target: SliderTarget, ratio: f32, cx: &mut Context<Workspace>) {
    match target {
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
fn slider(
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
        .bg(gpui::rgb(0x0E0E0E))
        .child(
            div()
                .absolute()
                .left_0()
                .top_0()
                .bottom_0()
                .w(px(72.0 * ratio))
                .rounded_sm()
                .bg(gpui::rgb(ACCENT)),
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
                .text_color(gpui::rgb(TEXT_DIM))
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

// ===== tool options bar =====

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
        .bg(gpui::rgb(PANEL_BG))
        .border_b_1()
        .border_color(gpui::rgb(PANEL_EDGE))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .w(px(130.0))
                .flex_none()
                .child(icon(tool_icon, 15.0, TEXT))
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
    bar.child(slider(
        "opt-opacity",
        "Opacity",
        format!("{:.0}%", ws.editor.tool_opacity * 100.0),
        SliderTarget::ToolOpacity,
        ws,
        cx,
    ))
}

// ===== toolbar =====

pub fn toolbar(ws: &mut Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let active = ws.editor.active_tool;
    let tools: Vec<(&'static str, &'static str)> =
        ws.registry.tools().map(|t| (t.id(), t.icon())).collect();
    div()
        .flex()
        .flex_col()
        .w(px(40.0))
        .flex_none()
        .items_center()
        .bg(gpui::rgb(PANEL_BG))
        .border_r_1()
        .border_color(gpui::rgb(PANEL_EDGE))
        .pt_1()
        .children(tools.into_iter().map(|(id, icon_name)| {
            let is_active = id == active;
            div()
                .flex()
                .items_center()
                .justify_center()
                .size(px(30.0))
                .my(px(1.0))
                .rounded_sm()
                .when_active(is_active)
                .hover(move |s| if is_active { s } else { s.bg(gpui::rgb(HOVER)) })
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |ws, _ev, _w, cx| ws.activate_tool(id, cx)),
                )
                .child(icon(
                    icon_name,
                    16.0,
                    if is_active { 0xFFFFFF } else { TEXT },
                ))
        }))
        .child(color_wells(ws, cx))
}

fn color_wells(ws: &Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
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
                .border_color(gpui::rgb(0x777777)),
        )
        .child(
            div()
                .absolute()
                .top_0()
                .left_0()
                .size(px(18.0))
                .bg(swatch_hex(ws.editor.foreground))
                .border_1()
                .border_color(gpui::rgb(0xEEEEEE)),
        )
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|ws, _ev, _w, cx| {
                std::mem::swap(&mut ws.editor.foreground, &mut ws.editor.background);
                cx.notify();
            }),
        )
}

// ===== side panels =====

pub fn side_panels(ws: &mut Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .w(px(260.0))
        .flex_none()
        .bg(gpui::rgb(PANEL_BG))
        .border_l_1()
        .border_color(gpui::rgb(PANEL_EDGE))
        .child(color_panel(ws, cx))
        .child(layers_panel(ws, cx))
        .child(history_panel(ws, cx))
}

const PALETTE: [u32; 16] = [
    0x000000, 0xFFFFFF, 0x808080, 0xC0C0C0, 0xE81E25, 0xFF7F27, 0xFFF200, 0x22B14C, 0x00A2E8,
    0x3F48CC, 0xA349A4, 0xB97A57, 0xFFAEC9, 0xFFC90E, 0xB5E61D, 0x99D9EA,
];

fn color_panel(ws: &mut Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let fg = ws.editor.foreground.to_u8();
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
                        .size(px(18.0))
                        .bg(gpui::rgb(hex))
                        .border_1()
                        .border_color(gpui::rgb(0x333333))
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
                .text_size(px(10.0))
                .text_color(gpui::rgb(TEXT_DIM))
                .child(format!("#{:02X}{:02X}{:02X}", fg[0], fg[1], fg[2])),
        )
}

fn panel_title(name: &'static str) -> impl IntoElement {
    div()
        .text_size(px(11.0))
        .text_color(gpui::rgb(TEXT_DIM))
        .pb_1()
        .child(name.to_uppercase())
}

// ===== layers panel =====

struct LayerRow {
    id: LayerId,
    depth: usize,
    kind: RowKind,
    name: String,
    visible: bool,
    active: bool,
    open: bool,
}

enum RowKind {
    Raster,
    Group,
    Adjustment,
}

fn flatten_layers(
    layers: &[Layer],
    depth: usize,
    active: Option<LayerId>,
    out: &mut Vec<LayerRow>,
) {
    for layer in layers.iter().rev() {
        let (kind, open) = match &layer.kind {
            LayerKind::Group(g) => (RowKind::Group, g.open),
            LayerKind::Adjustment(_) => (RowKind::Adjustment, false),
            LayerKind::Raster(_) => (RowKind::Raster, false),
        };
        out.push(LayerRow {
            id: layer.id,
            depth,
            kind,
            name: layer.name.clone(),
            visible: layer.visible,
            active: Some(layer.id) == active,
            open,
        });
        if let LayerKind::Group(g) = &layer.kind {
            if g.open {
                flatten_layers(&g.children, depth + 1, active, out);
            }
        }
    }
}

fn icon_button(
    icon_name: &'static str,
    command: &'static str,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .justify_center()
        .size(px(22.0))
        .rounded_sm()
        .hover(|s| s.bg(gpui::rgb(HOVER)))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |ws, _e, _w, cx| ws.run_command(command, cx)),
        )
        .child(icon(icon_name, 14.0, TEXT))
}

fn blend_mode_control(ws: &mut Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let active_layer = ws.doc.as_ref().and_then(|d| d.active_layer);
    let current = active_layer
        .and_then(|id| ws.doc.as_ref().and_then(|d| d.tree.find(id)))
        .map(|l| l.blend)
        .unwrap_or(BlendMode::Normal);
    let is_open = ws.open_popup == Some(Popup::BlendModes);
    let mut button = div()
        .relative()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .flex_grow()
        .h(px(20.0))
        .px_1()
        .rounded_sm()
        .bg(gpui::rgb(0x0E0E0E))
        .text_size(px(11.0))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|ws, _e, _w, cx| ws.toggle_popup(Popup::BlendModes, cx)),
        )
        .child(current.display_name())
        .child(icon("chevron-down", 11.0, TEXT_DIM));
    if is_open {
        if let Some(layer_id) = active_layer {
            let rows: Vec<gpui::AnyElement> = BlendMode::layer_modes()
                .iter()
                .map(|&mode| {
                    let selected = mode == current;
                    div()
                        .px_2()
                        .h(px(20.0))
                        .flex_none()
                        .flex()
                        .items_center()
                        .text_size(px(11.0))
                        .when_active(selected)
                        .hover(move |s| if selected { s } else { s.bg(gpui::rgb(HOVER)) })
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |ws, _e, _w, cx| {
                                ws.close_popup(cx);
                                ws.set_blend_mode(layer_id, mode, cx);
                            }),
                        )
                        .child(mode.display_name())
                        .into_any_element()
                })
                .collect();
            button = button.child(deferred(
                div()
                    .id("blend-modes")
                    .absolute()
                    .top(px(22.0))
                    .left_0()
                    .w(px(150.0))
                    .max_h(px(320.0))
                    .overflow_y_scroll()
                    .py_1()
                    .bg(gpui::rgb(POPUP_BG))
                    .border_1()
                    .border_color(gpui::rgb(0x3A3A3A))
                    .rounded_sm()
                    .shadow_lg()
                    .occlude()
                    .on_mouse_down_out(cx.listener(|ws, _e, _w, cx| ws.close_popup(cx)))
                    .children(rows),
            ));
        }
    }
    button
}

fn layers_panel(ws: &mut Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let mut rows = Vec::new();
    let active_layer = ws.doc.as_ref().and_then(|d| d.active_layer);
    if let Some(doc) = &ws.doc {
        flatten_layers(&doc.tree.layers, 0, doc.active_layer, &mut rows);
    }
    let thumbs: Vec<Option<Arc<RenderImage>>> =
        rows.iter().map(|r| ws.layer_thumbnail(r.id)).collect();
    let opacity_display = active_layer
        .map(|id| slider_get(ws, SliderTarget::LayerOpacity(id)))
        .map(|v| format!("{:.0}%", v * 100.0))
        .unwrap_or_default();

    div()
        .flex()
        .flex_col()
        .flex_grow()
        .min_h(px(0.0))
        .p_2()
        .gap_1()
        .border_t_1()
        .border_color(gpui::rgb(PANEL_EDGE))
        .child(panel_title("Layers"))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(blend_mode_control(ws, cx))
                .child(match active_layer {
                    Some(id) => slider(
                        "layer-opacity",
                        "",
                        opacity_display,
                        SliderTarget::LayerOpacity(id),
                        ws,
                        cx,
                    )
                    .into_any_element(),
                    None => div().into_any_element(),
                }),
        )
        .child(
            div()
                .id("layers-scroll")
                .flex()
                .flex_col()
                .flex_grow()
                .min_h(px(0.0))
                .overflow_y_scroll()
                .children(rows.into_iter().zip(thumbs).map(|(row, thumb)| {
                    let id = row.id;
                    let is_active_row = row.active;
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_1()
                        .px_1()
                        .h(px(34.0))
                        .flex_none()
                        .rounded_sm()
                        .when_active(row.active)
                        .hover(move |s| {
                            if is_active_row {
                                s
                            } else {
                                s.bg(gpui::rgb(HOVER))
                            }
                        })
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
                                .flex()
                                .items_center()
                                .justify_center()
                                .size(px(20.0))
                                .flex_none()
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |ws, _e, _w, cx| {
                                        if let Some(doc) = &mut ws.doc {
                                            let mut edit = doc.begin_edit("Toggle Visibility");
                                            edit.change_props(id, |l| l.visible = !l.visible);
                                            edit.commit();
                                        }
                                        ws.after_change(cx);
                                        cx.stop_propagation();
                                    }),
                                )
                                .child(icon(
                                    if row.visible { "eye" } else { "eye-off" },
                                    13.0,
                                    if row.visible { TEXT } else { TEXT_DIM },
                                )),
                        )
                        .child(div().w(px(row.depth as f32 * 12.0)).flex_none())
                        .child(match &row.kind {
                            RowKind::Group => div()
                                .flex()
                                .items_center()
                                .justify_center()
                                .size(px(16.0))
                                .flex_none()
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |ws, _e, _w, cx| {
                                        ws.toggle_group_open(id, cx);
                                        cx.stop_propagation();
                                    }),
                                )
                                .child(icon(
                                    if row.open {
                                        "chevron-down"
                                    } else {
                                        "chevron-right"
                                    },
                                    11.0,
                                    TEXT_DIM,
                                ))
                                .into_any_element(),
                            _ => div().w(px(0.0)).into_any_element(),
                        })
                        .child(
                            // Thumbnail (raster) or type icon.
                            div()
                                .flex()
                                .items_center()
                                .justify_center()
                                .w(px(38.0))
                                .h(px(30.0))
                                .flex_none()
                                .bg(gpui::rgb(0x0E0E0E))
                                .rounded_sm()
                                // Adjustment layers open their settings
                                // from the thumbnail, like Photoshop.
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |ws, _e, _w, cx| ws.edit_adjustment(id, cx)),
                                )
                                .child(match (&row.kind, thumb) {
                                    (RowKind::Raster, Some(t)) => {
                                        img(t).max_w(px(36.0)).max_h(px(28.0)).into_any_element()
                                    }
                                    (RowKind::Group, _) => {
                                        icon("folder", 14.0, TEXT_DIM).into_any_element()
                                    }
                                    _ => icon("adjust", 13.0, TEXT_DIM).into_any_element(),
                                }),
                        )
                        .child(div().text_size(px(12.0)).overflow_hidden().child(row.name))
                })),
        )
        .child(
            // Action buttons.
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_end()
                .gap_1()
                .pt_1()
                .border_t_1()
                .border_color(gpui::rgb(0x2A2A2A))
                .child(icon_button("layer-new", "layer.new", cx))
                .child(icon_button("group-new", "layer.group", cx))
                .child(icon_button("duplicate", "layer.duplicate", cx))
                .child(icon_button("merge-down", "layer.merge_down", cx))
                .child(icon_button("trash", "layer.delete", cx)),
        )
}

// ===== history panel =====

fn history_panel(ws: &mut Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let (undo_entries, redo_entries): (Vec<String>, Vec<String>) = ws
        .doc
        .as_ref()
        .map(|d| {
            (
                d.history.entries().iter().map(|e| e.name.clone()).collect(),
                // Most-recently-undone first == next redo first.
                d.history
                    .redo_entries()
                    .iter()
                    .rev()
                    .map(|e| e.name.clone())
                    .collect(),
            )
        })
        .unwrap_or_default();
    let n_undo = undo_entries.len() as i32;

    div()
        .flex()
        .flex_col()
        .h(px(150.0))
        .flex_none()
        .p_2()
        .gap_1()
        .border_t_1()
        .border_color(gpui::rgb(PANEL_EDGE))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .child(panel_title("History"))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap_1()
                        .child(icon_button("undo", "edit.undo", cx))
                        .child(icon_button("redo", "edit.redo", cx)),
                ),
        )
        .child(
            div()
                .id("history-scroll")
                .flex()
                .flex_col()
                .overflow_y_scroll()
                .flex_grow()
                .min_h(px(0.0))
                .children(undo_entries.into_iter().enumerate().map(|(i, name)| {
                    // Jump so entry i becomes the last applied edit.
                    let steps = (i as i32 + 1) - n_undo;
                    let is_current = i as i32 + 1 == n_undo;
                    div()
                        .px_1()
                        .h(px(19.0))
                        .flex_none()
                        .text_size(px(11.0))
                        .rounded_sm()
                        .when_active(is_current)
                        .hover(move |s| {
                            if is_current {
                                s
                            } else {
                                s.bg(gpui::rgb(HOVER))
                            }
                        })
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |ws, _e, _w, cx| ws.history_jump(steps, cx)),
                        )
                        .child(name)
                }))
                .children(redo_entries.into_iter().enumerate().map(|(j, name)| {
                    let steps = j as i32 + 1;
                    div()
                        .px_1()
                        .h(px(19.0))
                        .flex_none()
                        .text_size(px(11.0))
                        .rounded_sm()
                        .text_color(gpui::rgb(0x666666))
                        .hover(|s| s.bg(gpui::rgb(HOVER)))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |ws, _e, _w, cx| ws.history_jump(steps, cx)),
                        )
                        .child(name)
                })),
        )
}

// ===== status bar =====

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
    let brush = format!("{:.0}px", ws.editor.brush_size);
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
        .border_color(gpui::rgb(PANEL_EDGE))
        .text_size(px(11.0))
        .text_color(gpui::rgb(TEXT_DIM))
        .child(title)
        .child(zoom)
        .child(brush)
        .child(div().flex_grow())
        .child(ws.status.clone())
}
