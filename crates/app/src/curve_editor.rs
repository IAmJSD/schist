//! The Curves graph editor.
//!
//! Curves was the one adjustment with no UI of its own: it parsed,
//! rendered and round-tripped, but the only way to change it was to edit
//! the JSON. It needs a graph rather than sliders, which is what this is.
//!
//! Drag a point to move it, click empty space to add one, alt-click a
//! point to remove it. The diagonal, the grid and the histogram of what is
//! underneath are all drawn so the curve can be read against the image it
//! is acting on.

use crate::ui;
use crate::workspace::{Modal, Popup, Workspace};
use gpui::{
    canvas, div, px, Bounds, Context, InteractiveElement as _, IntoElement, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement as _, PathBuilder, Pixels, Point,
    SharedString, Styled,
};
use schist_adjustments::{CurveChannel, Curves};

/// Side of the square graph, in pixels.
const SIZE: f32 = 256.0;
/// How close a click has to be to grab a point, in curve units.
const GRAB: f32 = 0.035;

/// Which curve dialog is open, so the editor can write back to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CurveTarget {
    /// An adjustment layer's parameters.
    Layer,
    /// Image ▸ Adjustments ▸ Curves, applied to pixels.
    Destructive,
}

/// Read the curves out of whichever modal is open.
fn current(ws: &Workspace) -> Option<(CurveTarget, Curves, CurveChannel)> {
    match ws.modal.as_ref()? {
        Modal::Adjustment {
            params: schist_adjustments::Params::Curves(c),
            ..
        } => Some((CurveTarget::Layer, c.clone(), ws.curve_channel)),
        Modal::DestructiveAdjustment { params, .. } => match &**params {
            schist_adjustments::Params::Curves(c) => {
                Some((CurveTarget::Destructive, c.clone(), ws.curve_channel))
            }
            _ => None,
        },
        _ => None,
    }
}

/// Write curves back into the open modal and refresh the preview.
fn commit(ws: &mut Workspace, curves: Curves, cx: &mut Context<Workspace>) {
    let mut layer_preview = None;
    let mut destructive_preview = None;
    ws.update_modal(|m| match m {
        Modal::Adjustment { params, layer, .. } => {
            *params = schist_adjustments::Params::Curves(curves.clone());
            layer_preview = Some((*layer, params.clone()));
        }
        Modal::DestructiveAdjustment {
            params, preview, ..
        } => {
            **params = schist_adjustments::Params::Curves(curves.clone());
            if *preview {
                destructive_preview = Some((**params).clone());
            }
        }
        _ => {}
    });
    if let Some((layer, params)) = layer_preview {
        ws.preview_adjustment(layer, &params);
        ws.after_change(cx);
    }
    if let Some(params) = destructive_preview {
        ws.preview_destructive_adjustment(Some(&params), cx);
    }
}

/// Histogram of the layer under the adjustment, 64 buckets, normalised.
fn histogram(ws: &Workspace) -> Vec<f32> {
    let mut buckets = vec![0f32; 64];
    let Some(doc) = ws.doc.as_ref() else {
        return buckets;
    };
    let Some(raster) = doc
        .active_layer
        .and_then(|id| doc.tree.find(id))
        .and_then(|l| l.as_raster())
        .or_else(|| doc.tree.iter().last().and_then(|l| l.as_raster()))
    else {
        return buckets;
    };
    let rect = doc.canvas_rect();
    // Sample rather than scan: a histogram sketch does not need every
    // pixel of a 100-megapixel document.
    let step = ((rect.width() * rect.height()) as f32 / 20_000.0)
        .sqrt()
        .max(1.0) as i32;
    let mut y = rect.top;
    while y < rect.bottom {
        let mut x = rect.left;
        while x < rect.right {
            let p = raster.tiles.pixel(x, y);
            if p.a > 0.0 {
                let l = 0.299 * p.r + 0.587 * p.g + 0.114 * p.b;
                let b = ((l * 63.0).round() as usize).min(63);
                buckets[b] += 1.0;
            }
            x += step;
        }
        y += step;
    }
    let peak = buckets.iter().cloned().fold(0.0f32, f32::max);
    if peak > 0.0 {
        for b in buckets.iter_mut() {
            *b /= peak;
        }
    }
    buckets
}

/// The graph, as a dialog body element.
pub fn render(ws: &mut Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let Some((_, curves, channel)) = current(ws) else {
        return div();
    };
    let curve = curves.channel(channel).clone();
    let bars = histogram(ws);
    let tint = channel.tint();
    let entity = cx.entity();

    // Sampled curve, in 0..=1 graph coordinates.
    let plotted: Vec<(f32, f32)> = (0..=64)
        .map(|i| {
            let x = i as f32 / 64.0;
            (x, curve.eval(x).clamp(0.0, 1.0))
        })
        .collect();
    let points = curve.points.clone();

    let graph = div()
        .relative()
        .w(px(SIZE))
        .h(px(SIZE))
        .flex_none()
        .bg(gpui::rgb(ui::palette().deep_bg))
        .border_1()
        .border_color(gpui::rgb(ui::palette().edge))
        .rounded_sm()
        .child(
            canvas(
                move |bounds, _window, cx| {
                    entity.update(cx, |ws, _| ws.record_slider_bounds("curve-graph", bounds));
                    bounds
                },
                move |_, bounds: Bounds<Pixels>, window, _cx| {
                    paint_graph(bounds, window, &bars, &plotted, &points, tint);
                },
            )
            .absolute()
            .size_full(),
        )
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |ws, ev: &MouseDownEvent, _w, cx| {
                let Some((_, mut curves, channel)) = current(ws) else {
                    return;
                };
                let Some((x, y)) = ws.box_position("curve-graph", ev.position) else {
                    return;
                };
                let c = curves.channel_mut(channel);
                match c.hit(x, y, GRAB) {
                    // Alt-click removes; the two endpoints refuse.
                    Some(i) if ev.modifiers.alt => {
                        c.remove_point(i);
                        ws.curve_drag = None;
                    }
                    Some(i) => ws.curve_drag = Some(i),
                    None => ws.curve_drag = Some(c.add_point(x, y, GRAB)),
                }
                commit(ws, curves, cx);
                cx.notify();
            }),
        )
        .on_mouse_move(cx.listener(move |ws, ev: &MouseMoveEvent, _w, cx| {
            if ev.pressed_button != Some(MouseButton::Left) {
                return;
            }
            let Some(index) = ws.curve_drag else { return };
            let Some((_, mut curves, channel)) = current(ws) else {
                return;
            };
            let Some((x, y)) = ws.box_position("curve-graph", ev.position) else {
                return;
            };
            curves.channel_mut(channel).move_point(index, x, y);
            commit(ws, curves, cx);
            cx.notify();
        }))
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(|ws, _ev: &MouseUpEvent, _w, _cx| {
                ws.curve_drag = None;
            }),
        );

    div()
        .flex()
        .flex_col()
        .gap_2()
        .child(ui::field_row(
            "Channel",
            ui::dropdown(
                ui::Dropdown {
                    popup: Popup::Field("curve-channel"),
                    is_open: ws.open_popup == Some(Popup::Field("curve-channel")),
                    current: channel,
                    label: channel.label().into(),
                    width: 130.0,
                    options: CurveChannel::ALL
                        .iter()
                        .map(|c| (SharedString::from(c.label()), *c))
                        .collect(),
                },
                |ws, c, _cx| ws.curve_channel = c,
                cx,
            ),
        ))
        .child(graph)
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(ui::button(
                    "Reset Channel",
                    false,
                    move |ws, _w, cx| {
                        let Some((_, mut curves, channel)) = current(ws) else {
                            return;
                        };
                        curves.channel_mut(channel).reset();
                        commit(ws, curves, cx);
                    },
                    cx,
                ))
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(gpui::rgb(ui::palette().text_dim))
                        .child("Drag to shape · click to add · alt-click to remove"),
                ),
        )
}

fn paint_graph(
    bounds: Bounds<Pixels>,
    window: &mut gpui::Window,
    bars: &[f32],
    plotted: &[(f32, f32)],
    points: &[(f32, f32)],
    tint: u32,
) {
    let (ox, oy) = (f32::from(bounds.origin.x), f32::from(bounds.origin.y));
    let (w, h) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
    // Graph space (0..1, y up) to screen.
    let to_screen = |x: f32, y: f32| Point {
        x: px(ox + x * w),
        y: px(oy + (1.0 - y) * h),
    };

    // Histogram behind everything, so the curve reads on top of it.
    let n = bars.len().max(1);
    for (i, v) in bars.iter().enumerate() {
        if *v <= 0.0 {
            continue;
        }
        let x0 = ox + i as f32 / n as f32 * w;
        let bw = (w / n as f32).max(1.0);
        let bh = v.clamp(0.0, 1.0) * h;
        window.paint_quad(gpui::fill(
            Bounds {
                origin: Point {
                    x: px(x0),
                    y: px(oy + h - bh),
                },
                size: gpui::size(px(bw), px(bh)),
            },
            gpui::rgba((ui::palette().edge << 8) | 0x80),
        ));
    }

    // Quarter grid, and the identity diagonal it is read against.
    for i in 1..4 {
        let t = i as f32 / 4.0;
        window.paint_quad(gpui::fill(
            Bounds {
                origin: Point {
                    x: px(ox + t * w),
                    y: px(oy),
                },
                size: gpui::size(px(1.0), px(h)),
            },
            gpui::rgb(ui::palette().grid),
        ));
        window.paint_quad(gpui::fill(
            Bounds {
                origin: Point {
                    x: px(ox),
                    y: px(oy + t * h),
                },
                size: gpui::size(px(w), px(1.0)),
            },
            gpui::rgb(ui::palette().grid),
        ));
    }
    stroke_polyline(
        window,
        &[to_screen(0.0, 0.0), to_screen(1.0, 1.0)],
        1.0,
        gpui::rgb(ui::palette().edge),
    );

    let screen: Vec<Point<Pixels>> = plotted.iter().map(|(x, y)| to_screen(*x, *y)).collect();
    stroke_polyline(window, &screen, 1.6, gpui::rgb(tint));

    // Control points on top.
    for (x, y) in points {
        let c = to_screen(*x, *y);
        window.paint_quad(gpui::quad(
            Bounds {
                origin: Point {
                    x: c.x - px(3.5),
                    y: c.y - px(3.5),
                },
                size: gpui::size(px(7.0), px(7.0)),
            },
            px(1.0),
            gpui::rgb(ui::palette().field_bg),
            px(1.0),
            gpui::rgb(tint),
            gpui::BorderStyle::Solid,
        ));
    }
}

/// Stroke a polyline as a filled path of quads, which is all the canvas
/// element offers.
fn stroke_polyline(
    window: &mut gpui::Window,
    pts: &[Point<Pixels>],
    width: f32,
    color: gpui::Rgba,
) {
    if pts.len() < 2 {
        return;
    }
    let hw = width / 2.0;
    let mut builder = PathBuilder::stroke(px(width));
    builder.move_to(pts[0]);
    for p in &pts[1..] {
        builder.line_to(*p);
    }
    if let Ok(path) = builder.build() {
        window.paint_path(path, color);
    }
    let _ = hw;
}
