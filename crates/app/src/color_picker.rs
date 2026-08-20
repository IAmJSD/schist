//! Photoshop's Color Picker: the saturation/brightness field, the rainbow
//! hue strip beside it, and the numeric readouts.
//!
//! The dialog holds HSB rather than RGB. That is not a stylistic choice --
//! hue and saturation are not recoverable from a black or grey RGB value,
//! so a picker that stored RGB would snap its hue strip to red the moment
//! you dragged brightness to zero, and lose your hue when you passed
//! through the neutral axis. Photoshop keeps them, and so does this.
//!
//! The square and the rainbows are painted pixel by pixel and handed to
//! GPUI as images. Building them from gradient quads is the obvious thing
//! and it is what this did first, but GPUI interpolates gradient stops in
//! a space that is not sRGB: the midpoint of a white-to-red gradient came
//! out at (255, 54, 54) where the colour a click there picks is
//! (255, 128, 128). A picker whose square disagrees with its own
//! arithmetic is worse than no picker at all.

use crate::ui;
use crate::workspace::{ColorTarget, Modal, PickerDrag, Workspace};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    canvas, div, img, px, rgb, Context, InteractiveElement as _, IntoElement, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement as _, Pixels, RenderImage,
    SharedString, Styled as _,
};
use photoslop_color::Rgba;
use smallvec::smallvec;
use std::sync::Arc;

/// The saturation/brightness square, in pixels.
const FIELD: f32 = 256.0;
/// Width of the hue strip beside it.
const STRIP: f32 = 22.0;

// ===== colour conversion =====

/// RGB from hue/saturation/brightness, every component 0..=1.
pub fn hsv_to_rgb(h: f32, s: f32, v: f32) -> (f32, f32, f32) {
    let h = h.rem_euclid(1.0) * 6.0;
    let sextant = h.floor();
    let f = h - sextant;
    let p = v * (1.0 - s);
    let q = v * (1.0 - s * f);
    let t = v * (1.0 - s * (1.0 - f));
    match sextant as i32 % 6 {
        0 => (v, t, p),
        1 => (q, v, p),
        2 => (p, v, t),
        3 => (p, q, v),
        4 => (t, p, v),
        _ => (v, p, q),
    }
}

/// Hue/saturation/brightness from RGB, every component 0..=1.
///
/// Grey has no hue to report, so this returns 0 for it. Callers that need
/// to keep the user's hue across a trip through grey must remember it
/// themselves -- which is exactly why the dialog stores HSB.
pub fn rgb_to_hsv(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let chroma = max - min;
    let hue = if chroma <= 0.0 {
        0.0
    } else if max == r {
        ((g - b) / chroma).rem_euclid(6.0) / 6.0
    } else if max == g {
        ((b - r) / chroma + 2.0) / 6.0
    } else {
        ((r - g) / chroma + 4.0) / 6.0
    };
    let saturation = if max > 0.0 { chroma / max } else { 0.0 };
    (hue, saturation, max)
}

/// The fully saturated, fully bright colour at a hue.
fn pure(hue: f32) -> Rgba {
    let (r, g, b) = hsv_to_rgb(hue, 1.0, 1.0);
    Rgba::new(r, g, b, 1.0)
}

fn to_hex(c: Rgba) -> u32 {
    let [r, g, b, _] = c.to_u8();
    ((r as u32) << 16) | ((g as u32) << 8) | b as u32
}

/// Parse `#rrggbb` or `rrggbb`, and the three-digit short form.
pub fn parse_hex(text: &str) -> Option<Rgba> {
    let t = text.trim().trim_start_matches('#');
    let (r, g, b) = match t.len() {
        6 => (
            u8::from_str_radix(&t[0..2], 16).ok()?,
            u8::from_str_radix(&t[2..4], 16).ok()?,
            u8::from_str_radix(&t[4..6], 16).ok()?,
        ),
        3 => {
            let d = |i: usize| -> Option<u8> {
                let v = u8::from_str_radix(&t[i..i + 1], 16).ok()?;
                Some(v * 17)
            };
            (d(0)?, d(1)?, d(2)?)
        }
        _ => return None,
    };
    Some(Rgba::from_u8(r, g, b, 255))
}

// ===== the rainbow =====

/// The saturation/brightness square for one hue.
///
/// Cached on the workspace by hue, because a hue drag rebuilds it every
/// frame; 65k conversions is nothing, but the sprite atlas churn is worth
/// avoiding.
fn field_image(ws: &mut Workspace, hue: f32) -> Arc<RenderImage> {
    // A thousandth of the wheel is finer than a 256-pixel strip can point
    // at, so this never rebuilds for a hue you could not have asked for.
    let key = (hue.rem_euclid(1.0) * 1024.0).round() as u32 % 1024;
    if let Some((cached, image)) = ws.picker_field.as_ref() {
        if *cached == key {
            return image.clone();
        }
    }
    let n = FIELD as usize;
    let image = build(n, n, |x, y| {
        hsv_to_rgb(
            hue,
            x as f32 / (n - 1) as f32,
            1.0 - y as f32 / (n - 1) as f32,
        )
    });
    if let Some((_, old)) = ws.picker_field.replace((key, image.clone())) {
        ws.retire_image(old);
    }
    image
}

/// The vertical rainbow beside the square: red at the top, once round the
/// wheel, red again at the bottom.
fn strip_image(ws: &mut Workspace) -> Arc<RenderImage> {
    if let Some(image) = ws.picker_strip.as_ref() {
        return image.clone();
    }
    let (w, h) = (STRIP as usize, FIELD as usize);
    let image = build(w, h, |_, y| hsv_to_rgb(y as f32 / (h - 1) as f32, 1.0, 1.0));
    ws.picker_strip = Some(image.clone());
    image
}

/// The Color panel's horizontal spectrum bar.
fn ramp_image(ws: &mut Workspace) -> Arc<RenderImage> {
    if let Some(image) = ws.picker_ramp.as_ref() {
        return image.clone();
    }
    let (w, h) = (256usize, 16usize);
    let image = build(w, h, |x, _| hsv_to_rgb(x as f32 / (w - 1) as f32, 1.0, 1.0));
    ws.picker_ramp = Some(image.clone());
    image
}

/// Rasterise `colour` over a `w` by `h` grid. GPUI wants BGRA.
fn build(w: usize, h: usize, colour: impl Fn(usize, usize) -> (f32, f32, f32)) -> Arc<RenderImage> {
    let mut bgra = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let (r, g, b) = colour(x, y);
            let d = (y * w + x) * 4;
            bgra[d] = (b * 255.0).round() as u8;
            bgra[d + 1] = (g * 255.0).round() as u8;
            bgra[d + 2] = (r * 255.0).round() as u8;
            bgra[d + 3] = 255;
        }
    }
    let buffer = image::RgbaImage::from_raw(w as u32, h as u32, bgra).expect("sized just above");
    Arc::new(RenderImage::new(smallvec![image::Frame::new(buffer)]))
}

/// The Color panel's spectrum bar: click or drag anywhere along it to take
/// that hue as the foreground colour.
pub fn hue_ramp(ws: &mut Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let entity = cx.entity();
    let ramp = ramp_image(ws);
    div()
        .relative()
        .h(px(16.0))
        .w_full()
        .border_1()
        .border_color(rgb(ui::EDGE))
        .child(img(ramp).absolute().size_full())
        .child(
            canvas(
                move |bounds, _window, cx| {
                    entity.update(cx, |ws, _| ws.record_slider_bounds("color-ramp", bounds));
                },
                |_, _: (), _, _| {},
            )
            .absolute()
            .size_full(),
        )
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|ws, ev: &MouseDownEvent, _w, cx| {
                ws.picker_drag = Some(PickerDrag::Ramp);
                take_ramp_hue(ws, ev.position, cx);
            }),
        )
        .on_mouse_move(cx.listener(|ws, ev: &MouseMoveEvent, _w, cx| {
            if ev.pressed_button == Some(MouseButton::Left)
                && ws.picker_drag == Some(PickerDrag::Ramp)
            {
                take_ramp_hue(ws, ev.position, cx);
            }
        }))
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(|ws, _e: &MouseUpEvent, _w, _cx| ws.picker_drag = None),
        )
}

fn take_ramp_hue(ws: &mut Workspace, at: gpui::Point<Pixels>, cx: &mut Context<Workspace>) {
    let Some((x, _)) = ws.box_position("color-ramp", at) else {
        return;
    };
    ws.editor.foreground = pure(x);
    cx.notify();
}

// ===== the dialog =====

pub fn render(
    ws: &mut Workspace,
    target: ColorTarget,
    hsv: (f32, f32, f32),
    original: Rgba,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let (h, s, v) = hsv;
    let (r, g, b) = hsv_to_rgb(h, s, v);
    let chosen = Rgba::new(r, g, b, 1.0);
    let focused = ws.focused_field;
    let buffer = ws.field_buffer.clone();

    let body = div()
        .flex()
        .flex_row()
        .gap_3()
        .child(field_square(ws, h, s, v, cx))
        .child(hue_strip(ws, h, cx))
        .child(
            div()
                .flex()
                .flex_col()
                .gap_2()
                .child(comparison(chosen, original))
                .child(components(hsv, focused, &buffer, cx))
                .child(hex_field(chosen, focused == Some("cp-hex"), &buffer, cx)),
        );

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
            move |ws, _w, cx| ws.commit_color_picker(cx),
            cx,
        ));

    ui::modal_frame(
        match target {
            ColorTarget::Foreground => "Color Picker (Foreground Color)",
            ColorTarget::Background => "Color Picker (Background Color)",
        },
        620.0,
        body,
        actions,
    )
}

/// The saturation/brightness square for the current hue.
fn field_square(
    ws: &mut Workspace,
    h: f32,
    s: f32,
    v: f32,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let entity = cx.entity();
    let field = field_image(ws, h);
    div()
        .relative()
        .w(px(FIELD))
        .h(px(FIELD))
        .flex_none()
        .border_1()
        .border_color(rgb(ui::EDGE))
        .child(img(field).absolute().size_full())
        .child(ring(s * FIELD, (1.0 - v) * FIELD))
        .child(
            canvas(
                move |bounds, _window, cx| {
                    entity.update(cx, |ws, _| ws.record_slider_bounds("cp-field", bounds));
                },
                |_, _: (), _, _| {},
            )
            .absolute()
            .size_full(),
        )
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|ws, ev: &MouseDownEvent, _w, cx| {
                ws.picker_drag = Some(PickerDrag::Field);
                take_field(ws, ev.position, cx);
            }),
        )
        .on_mouse_move(cx.listener(|ws, ev: &MouseMoveEvent, _w, cx| {
            if ev.pressed_button == Some(MouseButton::Left)
                && ws.picker_drag == Some(PickerDrag::Field)
            {
                take_field(ws, ev.position, cx);
            }
        }))
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(|ws, _e: &MouseUpEvent, _w, _cx| ws.picker_drag = None),
        )
}

fn take_field(ws: &mut Workspace, at: gpui::Point<Pixels>, cx: &mut Context<Workspace>) {
    let Some((x, y)) = ws.box_position("cp-field", at) else {
        return;
    };
    ws.update_modal(|m| {
        if let Modal::ColorPicker { hsv, .. } = m {
            // `box_position` measures y upwards, which is the direction
            // brightness runs in.
            hsv.1 = x;
            hsv.2 = y;
        }
    });
    cx.notify();
}

/// The rainbow strip: red at the top, round the wheel, red again at the
/// bottom, with a marker at the current hue.
fn hue_strip(ws: &mut Workspace, h: f32, cx: &mut Context<Workspace>) -> impl IntoElement {
    let entity = cx.entity();
    let strip = strip_image(ws);
    div()
        .relative()
        .w(px(STRIP))
        .h(px(FIELD))
        .flex_none()
        .border_1()
        .border_color(rgb(ui::EDGE))
        .child(img(strip).absolute().size_full())
        .child(
            // A bar rather than Photoshop's pair of arrows, because the
            // arrows would need room outside the strip.
            div()
                .absolute()
                .left_0()
                .right_0()
                .top(px((h * FIELD - 1.5).clamp(0.0, FIELD - 3.0)))
                .h(px(3.0))
                .bg(rgb(0xFFFFFF))
                .border_1()
                .border_color(rgb(0x000000)),
        )
        .child(
            canvas(
                move |bounds, _window, cx| {
                    entity.update(cx, |ws, _| ws.record_slider_bounds("cp-hue", bounds));
                },
                |_, _: (), _, _| {},
            )
            .absolute()
            .size_full(),
        )
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|ws, ev: &MouseDownEvent, _w, cx| {
                ws.picker_drag = Some(PickerDrag::Hue);
                take_hue(ws, ev.position, cx);
            }),
        )
        .on_mouse_move(cx.listener(|ws, ev: &MouseMoveEvent, _w, cx| {
            if ev.pressed_button == Some(MouseButton::Left)
                && ws.picker_drag == Some(PickerDrag::Hue)
            {
                take_hue(ws, ev.position, cx);
            }
        }))
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(|ws, _e: &MouseUpEvent, _w, _cx| ws.picker_drag = None),
        )
}

fn take_hue(ws: &mut Workspace, at: gpui::Point<Pixels>, cx: &mut Context<Workspace>) {
    let Some((_, y)) = ws.box_position("cp-hue", at) else {
        return;
    };
    ws.update_modal(|m| {
        if let Modal::ColorPicker { hsv, .. } = m {
            // The strip runs red at the top to red at the bottom, and
            // `box_position` measures upwards.
            hsv.0 = (1.0 - y).clamp(0.0, 1.0);
        }
    });
    cx.notify();
}

/// A small circle over the field, dark ringed in white so it stays visible
/// against both ends of the gradient.
fn ring(x: f32, y: f32) -> impl IntoElement {
    div()
        .absolute()
        .left(px(x - 6.0))
        .top(px(y - 6.0))
        .size(px(12.0))
        .rounded_full()
        .border_2()
        .border_color(rgb(0xFFFFFF))
        .child(
            div()
                .size_full()
                .rounded_full()
                .border_1()
                .border_color(rgb(0x000000)),
        )
}

/// Photoshop's new-over-current swatch.
fn comparison(chosen: Rgba, original: Rgba) -> impl IntoElement {
    let cell = |c: Rgba, label: &'static str| {
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .child(
                div()
                    .w(px(58.0))
                    .h(px(26.0))
                    .flex_none()
                    .bg(rgb(to_hex(c)))
                    .border_1()
                    .border_color(rgb(0x000000)),
            )
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(rgb(ui::TEXT_DIM))
                    .child(label),
            )
    };
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(cell(chosen, "new"))
        .child(cell(original, "current"))
}

/// One picker component's value in the units its field shows.
///
/// Rounded, because Photoshop's picker reads in whole degrees and whole
/// percent and there is no useful precision past that -- and because a
/// field that reads "145.99" is a field that will read "146.01" next
/// frame.
pub(crate) fn component_value(hsv: (f32, f32, f32), id: &str) -> f32 {
    let (h, s, v) = hsv;
    match id {
        "cp-h" => (h * 360.0).round(),
        "cp-s" => (s * 100.0).round(),
        "cp-b" => (v * 100.0).round(),
        "cp-r" | "cp-g" | "cp-bl" => {
            let (r, g, b) = hsv_to_rgb(h, s, v);
            let channel = match id {
                "cp-r" => r,
                "cp-g" => g,
                _ => b,
            };
            (channel * 255.0).round()
        }
        _ => 0.0,
    }
}

/// Write one component back, in the units its field shows.
///
/// Setting an RGB channel has to go out to RGB and return, which cannot
/// preserve a hue the result no longer has -- type your way to a grey and
/// there is no hue left to keep. Everything short of that survives.
pub(crate) fn set_component(hsv: &mut (f32, f32, f32), id: &str, value: f32) {
    match id {
        "cp-h" => hsv.0 = value.rem_euclid(360.0) / 360.0,
        "cp-s" => hsv.1 = (value / 100.0).clamp(0.0, 1.0),
        "cp-b" => hsv.2 = (value / 100.0).clamp(0.0, 1.0),
        "cp-r" | "cp-g" | "cp-bl" => {
            let (r, g, b) = hsv_to_rgb(hsv.0, hsv.1, hsv.2);
            let set = (value / 255.0).clamp(0.0, 1.0);
            let (r, g, b) = match id {
                "cp-r" => (set, g, b),
                "cp-g" => (r, set, b),
                _ => (r, g, set),
            };
            let typed = rgb_to_hsv(r, g, b);
            if typed.1 > 0.0 {
                hsv.0 = typed.0;
            }
            hsv.1 = typed.1;
            hsv.2 = typed.2;
        }
        _ => {}
    }
}

/// One press of a component's plus or minus button.
pub(crate) fn nudge(hsv: &mut (f32, f32, f32), id: &str, delta: f32) {
    set_component(hsv, id, component_value(*hsv, id) + delta);
}

/// One numeric component of the colour.
struct Component {
    id: &'static str,
    label: &'static str,
    /// Shown to the user, in the component's own units.
    value: f32,
    /// What one press of the ± buttons is worth, in those units.
    step: f32,
    suffix: &'static str,
}

fn component_specs(hsv: (f32, f32, f32)) -> [Component; 6] {
    let at = |id| component_value(hsv, id);
    [
        Component {
            id: "cp-h",
            label: "H",
            value: at("cp-h"),
            step: 1.0,
            suffix: "\u{b0}",
        },
        Component {
            id: "cp-s",
            label: "S",
            value: at("cp-s"),
            step: 1.0,
            suffix: "%",
        },
        Component {
            id: "cp-b",
            label: "B",
            value: at("cp-b"),
            step: 1.0,
            suffix: "%",
        },
        Component {
            id: "cp-r",
            label: "R",
            value: at("cp-r"),
            step: 1.0,
            suffix: "",
        },
        Component {
            id: "cp-g",
            label: "G",
            value: at("cp-g"),
            step: 1.0,
            suffix: "",
        },
        Component {
            id: "cp-bl",
            label: "B",
            value: at("cp-bl"),
            step: 1.0,
            suffix: "",
        },
    ]
}

fn components(
    hsv: (f32, f32, f32),
    focused: Option<&'static str>,
    buffer: &str,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let mut rows = div().flex().flex_col().gap_1();
    for (i, spec) in component_specs(hsv).into_iter().enumerate() {
        let id = spec.id;
        rows = rows.child(
            div()
                // HSB above, RGB below, with a gap between the groups the
                // way Photoshop separates its columns.
                .when(i == 3, |d| d.mt_2())
                .child(ui::field_row(
                    spec.label,
                    ui::num_field(
                        ui::NumField {
                            id,
                            value: spec.value,
                            suffix: spec.suffix,
                            step: spec.step,
                            focused: focused == Some(id),
                            buffer: buffer.to_string(),
                        },
                        move |ws, delta| ws.nudge_color_component(id, delta),
                        cx,
                    ),
                )),
        );
    }
    rows
}

fn hex_field(
    chosen: Rgba,
    focused: bool,
    buffer: &str,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let shown: SharedString = if focused {
        format!("#{buffer}").into()
    } else {
        format!("#{:06X}", to_hex(chosen)).into()
    };
    ui::field_row(
        "#",
        div()
            .flex()
            .items_center()
            .w(px(86.0))
            .h(px(20.0))
            .px_1()
            .rounded_sm()
            .bg(rgb(ui::FIELD_BG))
            .border_1()
            .border_color(rgb(if focused { ui::ACCENT } else { ui::FIELD_BG }))
            .text_size(px(11.0))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|ws, _e, _w, cx| {
                    ws.focus_field("cp-hex");
                    cx.notify();
                }),
            )
            .child(shown),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The six corners of the wheel, which every other hue interpolates
    /// between. If these are wrong the whole rainbow is wrong.
    #[test]
    fn the_primaries_land_where_they_should() {
        for (turn, want) in [
            (0.0, (1.0, 0.0, 0.0)),
            (1.0 / 6.0, (1.0, 1.0, 0.0)),
            (2.0 / 6.0, (0.0, 1.0, 0.0)),
            (3.0 / 6.0, (0.0, 1.0, 1.0)),
            (4.0 / 6.0, (0.0, 0.0, 1.0)),
            (5.0 / 6.0, (1.0, 0.0, 1.0)),
            (1.0, (1.0, 0.0, 0.0)),
        ] {
            let got = hsv_to_rgb(turn, 1.0, 1.0);
            let close = (got.0 - want.0).abs() < 1e-5
                && (got.1 - want.1).abs() < 1e-5
                && (got.2 - want.2).abs() < 1e-5;
            assert!(close, "hue {turn}: expected {want:?}, got {got:?}");
        }
    }

    #[test]
    fn hsv_and_rgb_are_inverses() {
        for h in 0..36 {
            for s in 1..=10 {
                for v in 1..=10 {
                    let (h, s, v) = (h as f32 / 36.0, s as f32 / 10.0, v as f32 / 10.0);
                    let (r, g, b) = hsv_to_rgb(h, s, v);
                    let back = rgb_to_hsv(r, g, b);
                    // Hue wraps, so compare on the circle.
                    let dh = (back.0 - h).abs().min(1.0 - (back.0 - h).abs());
                    assert!(dh < 1e-4, "hue {h} came back as {}", back.0);
                    assert!((back.1 - s).abs() < 1e-4, "sat {s} came back as {}", back.1);
                    assert!((back.2 - v).abs() < 1e-4, "val {v} came back as {}", back.2);
                }
            }
        }
    }

    /// Grey has no hue, and asking for one must not produce a wild value
    /// that the dialog would then adopt.
    #[test]
    fn neutrals_report_no_hue_and_no_saturation() {
        for level in [0.0, 0.25, 0.5, 1.0] {
            let (h, s, v) = rgb_to_hsv(level, level, level);
            assert_eq!(h, 0.0);
            assert_eq!(s, 0.0);
            assert!((v - level).abs() < 1e-6);
        }
    }

    #[test]
    fn hex_parses_both_lengths_and_rejects_junk() {
        let eq = |c: Rgba, want: [u8; 3]| {
            let [r, g, b, _] = c.to_u8();
            [r, g, b] == want
        };
        assert!(eq(parse_hex("#64B687").unwrap(), [0x64, 0xB6, 0x87]));
        assert!(eq(parse_hex("64b687").unwrap(), [0x64, 0xB6, 0x87]));
        // The short form repeats each digit, so #abc is #aabbcc.
        assert!(eq(parse_hex("#abc").unwrap(), [0xAA, 0xBB, 0xCC]));
        assert!(eq(parse_hex("  #FFF  ").unwrap(), [0xFF, 0xFF, 0xFF]));
        for junk in ["", "#", "12345", "#gggggg", "1234567"] {
            assert!(parse_hex(junk).is_none(), "{junk:?} should not parse");
        }
    }

    /// Typing into a component must not disturb the others, and a
    /// component driven to a neutral must leave the hue where it was.
    #[test]
    fn setting_a_component_keeps_the_rest() {
        let mut hsv = (0.4, 0.5, 0.6);
        set_component(&mut hsv, "cp-s", 80.0);
        assert!((hsv.0 - 0.4).abs() < 1e-6 && (hsv.2 - 0.6).abs() < 1e-6);
        assert!((hsv.1 - 0.8).abs() < 1e-6);

        // Pulling brightness to nothing is the case that matters: the
        // colour is black, so it has no hue of its own to report, and the
        // dialog has to keep showing the one you chose.
        let mut hsv = (0.4, 0.5, 0.6);
        set_component(&mut hsv, "cp-b", 0.0);
        assert_eq!(hsv.2, 0.0);
        assert!((hsv.0 - 0.4).abs() < 1e-6, "hue was lost: {}", hsv.0);
        assert!((hsv.1 - 0.5).abs() < 1e-6, "saturation was lost: {}", hsv.1);

        // Reaching black one RGB channel at a time is different: each of
        // those steps is a real colour change and is allowed to move the
        // hue. Only the final step, which leaves nothing to take a hue
        // from, has to preserve it.
        let mut hsv = (0.4, 0.5, 0.6);
        set_component(&mut hsv, "cp-r", 0.0);
        set_component(&mut hsv, "cp-g", 0.0);
        let last_real_hue = hsv.0;
        set_component(&mut hsv, "cp-bl", 0.0);
        assert_eq!(hsv.2, 0.0);
        assert!(
            (hsv.0 - last_real_hue).abs() < 1e-6,
            "hue was lost on the way to black: {} became {}",
            last_real_hue,
            hsv.0
        );
    }

    #[test]
    fn hue_nudges_wrap_rather_than_stick() {
        let mut hsv = (0.0, 1.0, 1.0);
        nudge(&mut hsv, "cp-h", -1.0);
        assert!((component_value(hsv, "cp-h") - 359.0).abs() < 1e-3);
        nudge(&mut hsv, "cp-h", 1.0);
        assert!((component_value(hsv, "cp-h")).abs() < 1e-3);
    }
}
