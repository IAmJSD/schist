//! The Filter Gallery.
//!
//! Photoshop's gallery is the place you stack several filters and see the
//! result of the lot before committing. That stacking is the point: the
//! filters themselves are all reachable from the Filter menu individually,
//! but only here can you run three in sequence and change the first one's
//! settings while watching the third.

use crate::dialogs::{param_slider, SliderSpec};
use crate::ui;
use crate::workspace::{GalleryEntry, Modal, Popup, Workspace};
use gpui::{
    div, px, Context, InteractiveElement as _, IntoElement, MouseButton, MouseDownEvent,
    ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled,
};

/// Mutate the open gallery and re-run its preview.
fn edit(
    ws: &mut Workspace,
    cx: &mut Context<Workspace>,
    f: impl FnOnce(&mut Vec<GalleryEntry>, &mut usize),
) {
    let mut next = None;
    ws.update_modal(|m| {
        if let Modal::FilterGallery {
            stack,
            selected,
            preview,
        } = m
        {
            f(stack, selected);
            *selected = (*selected).min(stack.len().saturating_sub(1));
            if *preview {
                next = Some(stack.clone());
            }
        }
    });
    if let Some(stack) = next {
        ws.preview_gallery(&stack, cx);
    }
    cx.notify();
}

pub fn render(
    ws: &mut Workspace,
    stack: Vec<GalleryEntry>,
    selected: usize,
    preview: bool,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    // Every filter, grouped by category, to pick from.
    let mut categories: Vec<(&'static str, Vec<(&'static str, String)>)> = Vec::new();
    for f in ws.registry.filters() {
        let entry = (f.id(), f.name().to_string());
        match categories.iter_mut().find(|(c, _)| *c == f.category()) {
            Some((_, list)) => list.push(entry),
            None => categories.push((f.category(), vec![entry])),
        }
    }

    let browser = div()
        .id("gallery-browser")
        .flex()
        .flex_col()
        .w(px(190.0))
        .flex_none()
        .h(px(360.0))
        .overflow_y_scroll()
        .children(categories.into_iter().map(|(name, list)| {
            div()
                .flex()
                .flex_col()
                .child(
                    div()
                        .px_1()
                        .pt_2()
                        .text_size(px(11.0))
                        .text_color(gpui::rgb(ui::TEXT_DIM))
                        .child(SharedString::from(name)),
                )
                .children(list.into_iter().map(|(id, label)| {
                    div()
                        .px_2()
                        .h(px(20.0))
                        .rounded_sm()
                        .text_size(px(12.0))
                        .hover(|s| s.bg(gpui::rgb(0x2A2A2A)))
                        .child(SharedString::from(label))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |ws, _e: &MouseDownEvent, _w, cx| {
                                // Adding puts the new filter on top of the
                                // stack and selects it, as Photoshop does.
                                let values = ws
                                    .registry
                                    .filters()
                                    .find(|f| f.id() == id)
                                    .map(|f| {
                                        photoslop_plugin_api::FilterValues::defaults(&f.params())
                                    })
                                    .unwrap_or_default();
                                edit(ws, cx, |stack, selected| {
                                    stack.push(GalleryEntry {
                                        id,
                                        values,
                                        enabled: true,
                                    });
                                    *selected = stack.len() - 1;
                                });
                            }),
                        )
                }))
        }));

    // The stack, drawn top-first so it reads the way it is applied.
    let names: Vec<(usize, String, bool)> = stack
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let name = ws
                .registry
                .filters()
                .find(|f| f.id() == e.id)
                .map(|f| f.name().to_string())
                .unwrap_or_else(|| e.id.to_string());
            (i, name, e.enabled)
        })
        .collect();
    let stack_panel = div()
        .flex()
        .flex_col()
        .gap(px(1.0))
        .children(names.into_iter().rev().map(|(i, name, on)| {
            let is_selected = i == selected;
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .px_2()
                .h(px(22.0))
                .rounded_sm()
                .text_size(px(12.0))
                .bg(gpui::rgb(if is_selected { 0x2F5B8C } else { 0x1E1E1E }))
                .child(ui::checkbox(
                    "",
                    on,
                    move |ws, cx| {
                        edit(ws, cx, |stack, _| {
                            if let Some(e) = stack.get_mut(i) {
                                e.enabled = !on;
                            }
                        });
                    },
                    cx,
                ))
                .child(div().flex_grow().child(SharedString::from(name)))
                .child(
                    div()
                        .px_1()
                        .text_size(px(11.0))
                        .text_color(gpui::rgb(ui::TEXT_DIM))
                        .child("\u{2715}")
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |ws, _e: &MouseDownEvent, _w, cx| {
                                edit(ws, cx, |stack, _| {
                                    if i < stack.len() {
                                        stack.remove(i);
                                    }
                                });
                            }),
                        ),
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |ws, _e: &MouseDownEvent, _w, cx| {
                        edit(ws, cx, |_, selected| *selected = i);
                    }),
                )
        }));

    // Parameters of whichever entry is selected.
    let mut params = div().flex().flex_col().gap_1();
    if let Some(entry) = stack.get(selected) {
        let specs = ws
            .registry
            .filters()
            .find(|f| f.id() == entry.id)
            .map(|f| f.params())
            .unwrap_or_default();
        if specs.is_empty() {
            params = params.child(
                div()
                    .text_size(px(11.0))
                    .text_color(gpui::rgb(ui::TEXT_DIM))
                    .child("This filter has no settings."),
            );
        }
        for spec in specs {
            let key = spec.key;
            params = params.child(param_slider(
                SliderSpec {
                    id: key,
                    label: spec.label,
                    value: entry.values.get(key),
                    min: spec.min,
                    max: spec.max,
                    suffix: spec.suffix,
                    choices: spec.choices,
                },
                move |ws, v, cx| {
                    edit(ws, cx, |stack, selected| {
                        if let Some(e) = stack.get_mut(*selected) {
                            e.values.set(key, v);
                        }
                    });
                },
                cx,
            ));
        }
    }

    let right = div()
        .flex()
        .flex_col()
        .gap_2()
        .flex_grow()
        .child(
            div()
                .text_size(px(11.0))
                .text_color(gpui::rgb(ui::TEXT_DIM))
                .child("Stack (applied bottom to top)"),
        )
        .child(stack_panel)
        .child(div().h(px(1.0)).bg(gpui::rgb(0x2A2A2A)))
        .child(params)
        .child(ui::checkbox(
            "Preview",
            preview,
            move |ws, cx| {
                let mut next = None;
                ws.update_modal(|m| {
                    if let Modal::FilterGallery { stack, preview, .. } = m {
                        *preview = !*preview;
                        next = Some((*preview, stack.clone()));
                    }
                });
                match next {
                    Some((true, stack)) => ws.preview_gallery(&stack, cx),
                    Some((false, _)) => ws.preview_gallery(&[], cx),
                    None => {}
                }
            },
            cx,
        ));

    let body = div().flex().flex_row().gap_3().child(browser).child(right);
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
            |ws, _w, cx| {
                let mut run = None;
                ws.update_modal(|m| {
                    if let Modal::FilterGallery { stack, .. } = m {
                        run = Some(stack.clone());
                    }
                });
                ws.modal = None;
                if let Some(stack) = run {
                    ws.commit_gallery(&stack, cx);
                }
                cx.notify();
            },
            cx,
        ));
    let _ = Popup::BlendModes;
    ui::modal_frame("Filter Gallery", 620.0, body, actions)
}
