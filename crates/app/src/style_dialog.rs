//! The Layer Style dialog.
//!
//! Laid out like Photoshop's: the effects list down the left with a
//! checkbox each, the selected effect's settings on the right. Edits write
//! straight onto the layer so the canvas previews them live, and OK records
//! one history entry for the whole session (Cancel puts the old style
//! back), the same shape the adjustment dialogs use.

use crate::dialogs::{param_slider, SliderSpec};
use crate::ui;
use crate::workspace::{ColorTarget, Modal, Popup, Workspace};
use gpui::{
    div, px, Context, InteractiveElement as _, IntoElement, MouseButton, MouseDownEvent,
    ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled,
};
use schist_color::Rgba;
use schist_core::{
    BevelStyle_, BlendMode, GradientShape, LayerId, LayerStyle, StrokePosition, Technique,
};

/// The effects, in the order Photoshop lists them.
pub const EFFECTS: &[(&str, &str)] = &[
    ("bevel", "Bevel & Emboss"),
    ("stroke", "Stroke"),
    ("inner_shadow", "Inner Shadow"),
    ("inner_glow", "Inner Glow"),
    ("satin", "Satin"),
    ("color_overlay", "Color Overlay"),
    ("gradient_overlay", "Gradient Overlay"),
    ("outer_glow", "Outer Glow"),
    ("drop_shadow", "Drop Shadow"),
];

fn enabled(style: &LayerStyle, key: &str) -> bool {
    match key {
        "bevel" => style.bevel.enabled,
        "stroke" => style.stroke.enabled,
        "inner_shadow" => style.inner_shadow.enabled,
        "inner_glow" => style.inner_glow.enabled,
        "satin" => style.satin.enabled,
        "color_overlay" => style.color_overlay.enabled,
        "gradient_overlay" => style.gradient_overlay.enabled,
        "outer_glow" => style.outer_glow.enabled,
        "drop_shadow" => style.drop_shadow.enabled,
        _ => false,
    }
}

fn set_enabled(style: &mut LayerStyle, key: &str, on: bool) {
    match key {
        "bevel" => style.bevel.enabled = on,
        "stroke" => style.stroke.enabled = on,
        "inner_shadow" => style.inner_shadow.enabled = on,
        "inner_glow" => style.inner_glow.enabled = on,
        "satin" => style.satin.enabled = on,
        "color_overlay" => style.color_overlay.enabled = on,
        "gradient_overlay" => style.gradient_overlay.enabled = on,
        "outer_glow" => style.outer_glow.enabled = on,
        "drop_shadow" => style.drop_shadow.enabled = on,
        _ => {}
    }
}

/// Every numeric control in the dialog, addressed by `<effect>.<field>`.
/// Keeping them in one table means the widgets, the setter and the layout
/// cannot drift apart.
struct Field {
    key: &'static str,
    label: &'static str,
    min: f32,
    max: f32,
    suffix: &'static str,
}

const fn f(
    key: &'static str,
    label: &'static str,
    min: f32,
    max: f32,
    suffix: &'static str,
) -> Field {
    Field {
        key,
        label,
        min,
        max,
        suffix,
    }
}

fn fields(effect: &str) -> &'static [Field] {
    const SHADOW: &[Field] = &[
        f("opacity", "Opacity", 0.0, 100.0, "%"),
        f("angle", "Angle", -180.0, 180.0, "\u{b0}"),
        f("distance", "Distance", 0.0, 250.0, " px"),
        f("spread", "Spread", 0.0, 100.0, "%"),
        f("size", "Size", 0.0, 250.0, " px"),
    ];
    const GLOW: &[Field] = &[
        f("opacity", "Opacity", 0.0, 100.0, "%"),
        f("spread", "Spread", 0.0, 100.0, "%"),
        f("size", "Size", 0.0, 250.0, " px"),
    ];
    const STROKE: &[Field] = &[
        f("size", "Size", 1.0, 250.0, " px"),
        f("opacity", "Opacity", 0.0, 100.0, "%"),
    ];
    const OVERLAY: &[Field] = &[f("opacity", "Opacity", 0.0, 100.0, "%")];
    const GRADIENT: &[Field] = &[
        f("opacity", "Opacity", 0.0, 100.0, "%"),
        f("angle", "Angle", -180.0, 180.0, "\u{b0}"),
        f("scale", "Scale", 0.1, 5.0, "\u{d7}"),
    ];
    const SATIN: &[Field] = &[
        f("opacity", "Opacity", 0.0, 100.0, "%"),
        f("angle", "Angle", -180.0, 180.0, "\u{b0}"),
        f("distance", "Distance", 0.0, 250.0, " px"),
        f("size", "Size", 0.0, 250.0, " px"),
    ];
    const BEVEL: &[Field] = &[
        f("size", "Size", 0.0, 250.0, " px"),
        f("soften", "Soften", 0.0, 16.0, " px"),
        f("depth", "Depth", 0.0, 10.0, "\u{d7}"),
        f("angle", "Angle", -180.0, 180.0, "\u{b0}"),
        f("altitude", "Altitude", 0.0, 90.0, "\u{b0}"),
        f("highlight_opacity", "Highlight", 0.0, 100.0, "%"),
        f("shadow_opacity", "Shadow", 0.0, 100.0, "%"),
    ];
    match effect {
        "drop_shadow" | "inner_shadow" => SHADOW,
        "outer_glow" | "inner_glow" => GLOW,
        "stroke" => STROKE,
        "color_overlay" => OVERLAY,
        "gradient_overlay" => GRADIENT,
        "satin" => SATIN,
        "bevel" => BEVEL,
        _ => &[],
    }
}

/// Percentages are stored 0..=1 but shown 0..=100.
fn is_percent(suffix: &str) -> bool {
    suffix == "%"
}

fn get(style: &LayerStyle, effect: &str, field: &str) -> f32 {
    match (effect, field) {
        ("drop_shadow", f) => shadow_get(&style.drop_shadow.settings, f),
        ("inner_shadow", f) => shadow_get(&style.inner_shadow.settings, f),
        ("outer_glow", f) => glow_get(&style.outer_glow.settings, f),
        ("inner_glow", f) => glow_get(&style.inner_glow.settings, f),
        ("stroke", "size") => style.stroke.settings.size,
        ("stroke", "opacity") => style.stroke.settings.opacity,
        ("color_overlay", "opacity") => style.color_overlay.settings.opacity,
        ("gradient_overlay", "opacity") => style.gradient_overlay.settings.opacity,
        ("gradient_overlay", "angle") => style.gradient_overlay.settings.angle,
        ("gradient_overlay", "scale") => style.gradient_overlay.settings.scale,
        ("satin", "opacity") => style.satin.settings.opacity,
        ("satin", "angle") => style.satin.settings.angle,
        ("satin", "distance") => style.satin.settings.distance,
        ("satin", "size") => style.satin.settings.size,
        ("bevel", "size") => style.bevel.settings.size,
        ("bevel", "soften") => style.bevel.settings.soften,
        ("bevel", "depth") => style.bevel.settings.depth,
        ("bevel", "angle") => style.bevel.settings.angle,
        ("bevel", "altitude") => style.bevel.settings.altitude,
        ("bevel", "highlight_opacity") => style.bevel.settings.highlight_opacity,
        ("bevel", "shadow_opacity") => style.bevel.settings.shadow_opacity,
        _ => 0.0,
    }
}

fn shadow_get(s: &schist_core::ShadowStyle, field: &str) -> f32 {
    match field {
        "opacity" => s.opacity,
        "angle" => s.angle,
        "distance" => s.distance,
        "spread" => s.spread,
        "size" => s.size,
        _ => 0.0,
    }
}

fn glow_get(g: &schist_core::GlowStyle, field: &str) -> f32 {
    match field {
        "opacity" => g.opacity,
        "spread" => g.spread,
        "size" => g.size,
        _ => 0.0,
    }
}

fn set(style: &mut LayerStyle, effect: &str, field: &str, v: f32) {
    match effect {
        "drop_shadow" => shadow_set(&mut style.drop_shadow.settings, field, v),
        "inner_shadow" => shadow_set(&mut style.inner_shadow.settings, field, v),
        "outer_glow" => glow_set(&mut style.outer_glow.settings, field, v),
        "inner_glow" => glow_set(&mut style.inner_glow.settings, field, v),
        "stroke" => match field {
            "size" => style.stroke.settings.size = v,
            "opacity" => style.stroke.settings.opacity = v,
            _ => {}
        },
        "color_overlay" => {
            if field == "opacity" {
                style.color_overlay.settings.opacity = v;
            }
        }
        "gradient_overlay" => match field {
            "opacity" => style.gradient_overlay.settings.opacity = v,
            "angle" => style.gradient_overlay.settings.angle = v,
            "scale" => style.gradient_overlay.settings.scale = v,
            _ => {}
        },
        "satin" => match field {
            "opacity" => style.satin.settings.opacity = v,
            "angle" => style.satin.settings.angle = v,
            "distance" => style.satin.settings.distance = v,
            "size" => style.satin.settings.size = v,
            _ => {}
        },
        "bevel" => match field {
            "size" => style.bevel.settings.size = v,
            "soften" => style.bevel.settings.soften = v,
            "depth" => style.bevel.settings.depth = v,
            "angle" => style.bevel.settings.angle = v,
            "altitude" => style.bevel.settings.altitude = v,
            "highlight_opacity" => style.bevel.settings.highlight_opacity = v,
            "shadow_opacity" => style.bevel.settings.shadow_opacity = v,
            _ => {}
        },
        _ => {}
    }
}

fn shadow_set(s: &mut schist_core::ShadowStyle, field: &str, v: f32) {
    match field {
        "opacity" => s.opacity = v,
        "angle" => s.angle = v,
        "distance" => s.distance = v,
        "spread" => s.spread = v,
        "size" => s.size = v,
        _ => {}
    }
}

fn glow_set(g: &mut schist_core::GlowStyle, field: &str, v: f32) {
    match field {
        "opacity" => g.opacity = v,
        "spread" => g.spread = v,
        "size" => g.size = v,
        _ => {}
    }
}

fn color_of(style: &LayerStyle, effect: &str) -> Option<Rgba> {
    Some(match effect {
        "drop_shadow" => style.drop_shadow.settings.color,
        "inner_shadow" => style.inner_shadow.settings.color,
        "outer_glow" => style.outer_glow.settings.color,
        "inner_glow" => style.inner_glow.settings.color,
        "stroke" => style.stroke.settings.color,
        "color_overlay" => style.color_overlay.settings.color,
        "satin" => style.satin.settings.color,
        _ => return None,
    })
}

/// The display name of a colour this dialog owns, for the Color Picker's
/// title bar.
pub fn effect_label(effect: &str) -> &'static str {
    match effect {
        "gradient_overlay.from" => "Gradient Overlay From",
        "gradient_overlay.to" => "Gradient Overlay To",
        _ => EFFECTS
            .iter()
            .find(|(key, _)| *key == effect)
            .map(|(_, label)| *label)
            .unwrap_or("Layer Style"),
    }
}

pub fn set_color(style: &mut LayerStyle, effect: &str, c: Rgba) {
    match effect {
        "drop_shadow" => style.drop_shadow.settings.color = c,
        "inner_shadow" => style.inner_shadow.settings.color = c,
        "outer_glow" => style.outer_glow.settings.color = c,
        "inner_glow" => style.inner_glow.settings.color = c,
        "stroke" => style.stroke.settings.color = c,
        "color_overlay" => style.color_overlay.settings.color = c,
        "satin" => style.satin.settings.color = c,
        // The gradient's two ends are not "the effect's colour", so they
        // get their own keys rather than a slot in `color_of`.
        "gradient_overlay.from" => style.gradient_overlay.settings.from = c,
        "gradient_overlay.to" => style.gradient_overlay.settings.to = c,
        _ => {}
    }
}

/// A colour swatch that opens the Color Picker on `key`. The Layer Style
/// dialog stays open underneath it and takes the colour on OK.
fn color_swatch(
    id: &'static str,
    key: &'static str,
    c: Rgba,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    div()
        .id(id)
        .size(px(18.0))
        .flex_none()
        .rounded_sm()
        .border_1()
        .border_color(gpui::rgb(ui::palette().edge))
        .bg(gpui::rgb(rgb_of(c)))
        .cursor_pointer()
        .hover(|s| s.border_color(gpui::rgb(ui::palette().text)))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |ws, _e: &MouseDownEvent, _w, cx| {
                ws.open_color_picker_on(ColorTarget::StyleEffect(key), c, cx);
            }),
        )
}

fn blend_of(style: &LayerStyle, effect: &str) -> Option<BlendMode> {
    Some(match effect {
        "drop_shadow" => style.drop_shadow.settings.blend,
        "inner_shadow" => style.inner_shadow.settings.blend,
        "outer_glow" => style.outer_glow.settings.blend,
        "inner_glow" => style.inner_glow.settings.blend,
        "stroke" => style.stroke.settings.blend,
        "color_overlay" => style.color_overlay.settings.blend,
        "gradient_overlay" => style.gradient_overlay.settings.blend,
        "satin" => style.satin.settings.blend,
        _ => return None,
    })
}

fn set_blend(style: &mut LayerStyle, effect: &str, m: BlendMode) {
    match effect {
        "drop_shadow" => style.drop_shadow.settings.blend = m,
        "inner_shadow" => style.inner_shadow.settings.blend = m,
        "outer_glow" => style.outer_glow.settings.blend = m,
        "inner_glow" => style.inner_glow.settings.blend = m,
        "stroke" => style.stroke.settings.blend = m,
        "color_overlay" => style.color_overlay.settings.blend = m,
        "gradient_overlay" => style.gradient_overlay.settings.blend = m,
        "satin" => style.satin.settings.blend = m,
        _ => {}
    }
}

/// Mutate the open dialog's style and push it onto the layer so the canvas
/// updates on the next frame.
fn edit(ws: &mut Workspace, cx: &mut Context<Workspace>, f: impl FnOnce(&mut LayerStyle)) {
    let mut next = None;
    ws.update_modal(|m| {
        if let Modal::LayerStyle { style, layer, .. } = m {
            f(style);
            next = Some((*layer, **style));
        }
    });
    if let Some((layer, style)) = next {
        ws.preview_layer_style(layer, style, cx);
    }
}

pub fn render(
    ws: &mut Workspace,
    layer: LayerId,
    style: LayerStyle,
    active: &'static str,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let _ = layer;
    let list = div()
        .flex()
        .flex_col()
        .w(px(150.0))
        .flex_none()
        .gap(px(1.0))
        .children(EFFECTS.iter().map(|(key, label)| {
            let key = *key;
            let on = enabled(&style, key);
            let selected = key == active;
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .px_2()
                .h(px(22.0))
                .rounded_sm()
                .text_size(px(12.0))
                .when_selected(selected)
                .child(ui::checkbox(
                    "",
                    on,
                    move |ws, cx| {
                        edit(ws, cx, |s| set_enabled(s, key, !on));
                    },
                    cx,
                ))
                .child(div().flex_grow().child(SharedString::from(*label)))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |ws, _e: &MouseDownEvent, _w, cx| {
                        ws.update_modal(|m| {
                            if let Modal::LayerStyle { active, .. } = m {
                                *active = key;
                            }
                        });
                        cx.notify();
                    }),
                )
        }));

    let mut settings = div().flex().flex_col().gap_1().flex_grow();
    let title = EFFECTS
        .iter()
        .find(|(k, _)| *k == active)
        .map(|(_, l)| *l)
        .unwrap_or("");
    settings = settings.child(
        div()
            .text_size(px(12.0))
            .pb_1()
            .child(SharedString::from(title)),
    );

    if let Some(mode) = blend_of(&style, active) {
        settings = settings.child(ui::field_row(
            "Blend",
            ui::dropdown(
                ui::Dropdown {
                    popup: Popup::Field("fx-blend"),
                    is_open: ws.open_popup == Some(Popup::Field("fx-blend")),
                    current: mode,
                    label: mode.display_name().into(),
                    width: 150.0,
                    options: BlendMode::layer_modes()
                        .iter()
                        .map(|m| (SharedString::from(m.display_name()), *m))
                        .collect(),
                },
                move |ws, m, _cx| {
                    // Dropdowns fire without a context; the preview catches
                    // up on the next edit or on OK.
                    ws.update_modal(|md| {
                        if let Modal::LayerStyle { style, .. } = md {
                            set_blend(style, active, m);
                        }
                    });
                    ws.restyle_from_modal();
                },
                cx,
            ),
        ));
    }

    if let Some(c) = color_of(&style, active) {
        settings = settings.child(ui::field_row(
            "Color",
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(color_swatch("style-color-swatch", active, c, cx))
                .child(ui::button(
                    "Use Foreground",
                    false,
                    move |ws, _w, cx| {
                        let fg = ws.editor.foreground;
                        edit(ws, cx, |s| set_color(s, active, fg));
                    },
                    cx,
                )),
        ));
    }

    for field in fields(active) {
        let pct = is_percent(field.suffix);
        let raw = get(&style, active, field.key);
        let key = field.key;
        settings = settings.child(param_slider(
            SliderSpec {
                id: key,
                label: field.label,
                value: if pct { raw * 100.0 } else { raw },
                min: field.min,
                max: field.max,
                suffix: field.suffix,
                ..Default::default()
            },
            move |ws, v, cx| {
                let v = if pct { v / 100.0 } else { v };
                edit(ws, cx, |s| set(s, active, key, v));
            },
            cx,
        ));
    }

    // The handful of controls that are not numeric.
    settings = match active {
        "stroke" => {
            let pos = style.stroke.settings.position;
            settings.child(ui::field_row(
                "Position",
                ui::dropdown(
                    ui::Dropdown {
                        popup: Popup::Field("fx-stroke-pos"),
                        is_open: ws.open_popup == Some(Popup::Field("fx-stroke-pos")),
                        current: pos,
                        label: match pos {
                            StrokePosition::Outside => "Outside",
                            StrokePosition::Inside => "Inside",
                            StrokePosition::Center => "Center",
                        }
                        .into(),
                        width: 150.0,
                        options: vec![
                            ("Outside".into(), StrokePosition::Outside),
                            ("Inside".into(), StrokePosition::Inside),
                            ("Center".into(), StrokePosition::Center),
                        ],
                    },
                    move |ws, p, _cx| {
                        ws.update_modal(|md| {
                            if let Modal::LayerStyle { style, .. } = md {
                                style.stroke.settings.position = p;
                            }
                        });
                        ws.restyle_from_modal();
                    },
                    cx,
                ),
            ))
        }
        "bevel" => {
            let st = style.bevel.settings.style;
            settings.child(ui::field_row(
                "Style",
                ui::dropdown(
                    ui::Dropdown {
                        popup: Popup::Field("fx-bevel-style"),
                        is_open: ws.open_popup == Some(Popup::Field("fx-bevel-style")),
                        current: st,
                        label: match st {
                            BevelStyle_::OuterBevel => "Outer Bevel",
                            BevelStyle_::InnerBevel => "Inner Bevel",
                            BevelStyle_::Emboss => "Emboss",
                            BevelStyle_::PillowEmboss => "Pillow Emboss",
                        }
                        .into(),
                        width: 150.0,
                        options: vec![
                            ("Outer Bevel".into(), BevelStyle_::OuterBevel),
                            ("Inner Bevel".into(), BevelStyle_::InnerBevel),
                            ("Emboss".into(), BevelStyle_::Emboss),
                            ("Pillow Emboss".into(), BevelStyle_::PillowEmboss),
                        ],
                    },
                    move |ws, v, _cx| {
                        ws.update_modal(|md| {
                            if let Modal::LayerStyle { style, .. } = md {
                                style.bevel.settings.style = v;
                            }
                        });
                        ws.restyle_from_modal();
                    },
                    cx,
                ),
            ))
        }
        "gradient_overlay" => {
            let shape = style.gradient_overlay.settings.shape;
            let rev = style.gradient_overlay.settings.reverse;
            settings
                .child(ui::field_row(
                    "Shape",
                    ui::dropdown(
                        ui::Dropdown {
                            popup: Popup::Field("fx-grad-shape"),
                            is_open: ws.open_popup == Some(Popup::Field("fx-grad-shape")),
                            current: shape,
                            label: match shape {
                                GradientShape::Linear => "Linear",
                                GradientShape::Radial => "Radial",
                            }
                            .into(),
                            width: 150.0,
                            options: vec![
                                ("Linear".into(), GradientShape::Linear),
                                ("Radial".into(), GradientShape::Radial),
                            ],
                        },
                        move |ws, v, _cx| {
                            ws.update_modal(|md| {
                                if let Modal::LayerStyle { style, .. } = md {
                                    style.gradient_overlay.settings.shape = v;
                                }
                            });
                            ws.restyle_from_modal();
                        },
                        cx,
                    ),
                ))
                .child(ui::checkbox(
                    "Reverse",
                    rev,
                    move |ws, cx| {
                        edit(ws, cx, |s| {
                            s.gradient_overlay.settings.reverse = !rev;
                        });
                    },
                    cx,
                ))
                .child(ui::field_row(
                    "From / To",
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .child(color_swatch(
                            "style-gradient-from",
                            "gradient_overlay.from",
                            style.gradient_overlay.settings.from,
                            cx,
                        ))
                        .child(ui::button(
                            "From = FG",
                            false,
                            move |ws, _w, cx| {
                                let fg = ws.editor.foreground;
                                edit(ws, cx, |s| s.gradient_overlay.settings.from = fg);
                            },
                            cx,
                        ))
                        .child(color_swatch(
                            "style-gradient-to",
                            "gradient_overlay.to",
                            style.gradient_overlay.settings.to,
                            cx,
                        ))
                        .child(ui::button(
                            "To = FG",
                            false,
                            move |ws, _w, cx| {
                                let fg = ws.editor.foreground;
                                edit(ws, cx, |s| s.gradient_overlay.settings.to = fg);
                            },
                            cx,
                        )),
                ))
        }
        "outer_glow" | "inner_glow" => {
            let inner = active == "inner_glow";
            let tech = if inner {
                style.inner_glow.settings.technique
            } else {
                style.outer_glow.settings.technique
            };
            let precise = tech == Technique::Precise;
            settings.child(ui::checkbox(
                "Precise",
                precise,
                move |ws, cx| {
                    let next = if precise {
                        Technique::Softer
                    } else {
                        Technique::Precise
                    };
                    edit(ws, cx, |s| {
                        if inner {
                            s.inner_glow.settings.technique = next;
                        } else {
                            s.outer_glow.settings.technique = next;
                        }
                    });
                },
                cx,
            ))
        }
        "drop_shadow" => {
            let knock = style.drop_shadow.settings.knockout;
            settings.child(ui::checkbox(
                "Layer knocks out drop shadow",
                knock,
                move |ws, cx| {
                    edit(ws, cx, |s| s.drop_shadow.settings.knockout = !knock);
                },
                cx,
            ))
        }
        "satin" => {
            let inv = style.satin.settings.invert;
            settings.child(ui::checkbox(
                "Invert",
                inv,
                move |ws, cx| {
                    edit(ws, cx, |s| s.satin.settings.invert = !inv);
                },
                cx,
            ))
        }
        _ => settings,
    };

    let body = div()
        .id("layer-style-body")
        .flex()
        .flex_row()
        .gap_3()
        .h(px(330.0))
        .overflow_y_scroll()
        .child(list)
        .child(settings);

    let actions = div()
        .flex()
        .flex_row()
        .gap_2()
        .child(ui::button(
            "Cancel",
            false,
            |ws, _w, cx| ws.close_modal(cx),
            cx,
        ))
        .child(ui::button(
            "OK",
            true,
            move |ws, _w, cx| ws.commit_layer_style(cx),
            cx,
        ));
    ui::modal_frame("Layer Style", 620.0, body, actions)
}

fn rgb_of(c: Rgba) -> u32 {
    let q = |v: f32| ((v.clamp(0.0, 1.0) * 255.0).round() as u32) & 0xFF;
    (q(c.r) << 16) | (q(c.g) << 8) | q(c.b)
}

/// Highlight for the selected row in the effects list.
trait Selected: Styled + Sized {
    fn when_selected(self, on: bool) -> Self {
        if on {
            self.bg(gpui::rgb(ui::palette().selection_bg))
        } else {
            self
        }
    }
}

impl<T: Styled + Sized> Selected for T {}
