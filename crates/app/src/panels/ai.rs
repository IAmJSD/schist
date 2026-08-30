//! The AI sidebar: a transcript of the conversation with an agent
//! harness, streamed as it happens, and a prompt box.
//!
//! Rendering only — the conversation lives in `crate::ai` workers and the
//! workspace's `ai_*` methods. The prompt box follows the notes panel's
//! text-entry pattern: a plain buffer, a drawn `|` caret, one child div
//! per line. Harness and model are picked together in one popup — a
//! search box over the live catalogs each installed CLI reported, with a
//! rail to flip between harnesses — opened from the chip beside Send.

use super::*;
use crate::ai::{AiEntryKind, Backend};
use gpui::AnyElement;

/// Errors get a colour of their own; the palette has no failure red
/// because nothing else in the chrome fails inline.
const ERROR_TEXT: u32 = 0xC0605A;

pub fn ai_sidebar(ws: &mut Workspace, cx: &mut Context<Workspace>) -> Option<AnyElement> {
    if !ws.view.ai_panel {
        return None;
    }
    let panel = div()
        .flex()
        .flex_col()
        .w(px(300.0))
        .flex_none()
        .h_full()
        .bg(gpui::rgb(palette().panel_bg))
        .border_l_1()
        .border_color(gpui::rgb(palette().panel_edge))
        .child(header(cx))
        .child(transcript(ws))
        .child(prompt_box(ws, cx));
    Some(panel.into_any_element())
}

fn header(cx: &mut Context<Workspace>) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .p_2()
        .child(panel_title("AI"))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .child(
                    div()
                        .id("ai-clear")
                        .flex()
                        .items_center()
                        .justify_center()
                        .size(px(20.0))
                        .rounded_sm()
                        .cursor_pointer()
                        .hover(|s| s.bg(gpui::rgb(palette().hover)))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|ws, _e, _w, cx| ws.ai_new_conversation(cx)),
                        )
                        .child(icon("trash", 13.0, palette().text_dim)),
                )
                // Closes the sidebar; whether it was open is a saved
                // preference, so it comes back on the next launch.
                .child(
                    div()
                        .id("ai-close")
                        .flex()
                        .items_center()
                        .justify_center()
                        .size(px(20.0))
                        .rounded_sm()
                        .cursor_pointer()
                        .hover(|s| s.bg(gpui::rgb(palette().hover)))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|ws, _e, _w, cx| ws.toggle_ai_panel(cx)),
                        )
                        .child(icon("close", 13.0, palette().text_dim)),
                ),
        )
}

fn transcript(ws: &mut Workspace) -> impl IntoElement {
    let running = ws.ai.running;
    let last = ws.ai.transcript.len().saturating_sub(1);
    let entries: Vec<AnyElement> = ws
        .ai
        .transcript
        .iter()
        .enumerate()
        .map(|(i, e)| {
            // The streamed reply gets a caret while it is still arriving.
            let streaming = running && i == last && e.kind == AiEntryKind::Assistant;
            entry(e.kind, &e.text, streaming)
        })
        .collect();
    let empty = entries.is_empty();
    div()
        .id("ai-scroll")
        .flex()
        .flex_col()
        .flex_grow()
        .min_h(px(0.0))
        .p_2()
        .gap_2()
        .overflow_y_scroll()
        .track_scroll(&ws.ai.scroll)
        .children(entries)
        .children(empty.then(|| {
            div()
                .text_size(px(11.0))
                .text_color(gpui::rgb(palette().text_faint))
                .child(
                    "Ask for an edit and watch it happen on the canvas. \
                     The agent drives the same tools, filters and commands \
                     as the menus, one undo step each.",
                )
        }))
        .children((running && ws.ai.transcript.last().map(|e| e.kind) != Some(AiEntryKind::Assistant)).then(|| {
            div()
                .text_size(px(11.0))
                .text_color(gpui::rgb(palette().text_faint))
                .child("thinking…")
        }))
}

fn entry(kind: AiEntryKind, text: &str, streaming: bool) -> AnyElement {
    let lines = |text: &str| {
        text.split('\n')
            .map(|l| SharedString::from(l.to_string()))
            .collect::<Vec<_>>()
    };
    match kind {
        AiEntryKind::User => div()
            .p_1p5()
            .rounded_sm()
            .bg(gpui::rgb(palette().field_bg))
            .border_1()
            .border_color(gpui::rgb(palette().divider))
            .text_size(px(12.0))
            .text_color(gpui::rgb(palette().text))
            .flex()
            .flex_col()
            .children(lines(text))
            .into_any_element(),
        AiEntryKind::Assistant => {
            let mut text = text.to_string();
            if streaming {
                text.push('▌');
            }
            div()
                .text_size(px(12.0))
                .text_color(gpui::rgb(palette().text))
                .flex()
                .flex_col()
                .gap_0p5()
                .children(lines(&text))
                .into_any_element()
        }
        AiEntryKind::Tool => div()
            .text_size(px(11.0))
            .text_color(gpui::rgb(palette().text_dim))
            .child(SharedString::from(format!("\u{25B8} {text}")))
            .into_any_element(),
        AiEntryKind::Info => div()
            .text_size(px(10.0))
            .text_color(gpui::rgb(palette().text_faint))
            .child(SharedString::from(text.to_string()))
            .into_any_element(),
        AiEntryKind::Error => div()
            .text_size(px(11.0))
            .text_color(gpui::rgb(ERROR_TEXT))
            .flex()
            .flex_col()
            .children(lines(text))
            .into_any_element(),
    }
}

fn prompt_box(ws: &Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let editing = ws.ai.input_active;
    let empty = ws.ai.input.is_empty();
    let shown = if editing {
        format!("{}|", ws.ai.input)
    } else if empty {
        "Ask about or edit this document".to_string()
    } else {
        ws.ai.input.clone()
    };
    let running = ws.ai.running;
    div()
        .relative()
        .flex()
        .flex_col()
        .flex_none()
        .p_2()
        .gap_1()
        .border_t_1()
        .border_color(gpui::rgb(palette().panel_edge))
        .children(ws.ai.model_menu.then(|| model_menu(ws, cx)))
        .child(
            div()
                .id("ai-input")
                .min_h(px(44.0))
                .p_1()
                .rounded_sm()
                .cursor_text()
                .bg(gpui::rgb(palette().field_bg))
                .border_1()
                .border_color(gpui::rgb(if editing {
                    palette().accent
                } else {
                    palette().panel_edge
                }))
                .text_size(px(12.0))
                .text_color(gpui::rgb(if !editing && empty {
                    palette().text_faint
                } else {
                    palette().text
                }))
                .flex()
                .flex_col()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|ws, _e, _w, cx| {
                        ws.ai.input_active = true;
                        cx.notify();
                    }),
                )
                .children(
                    shown
                        .split('\n')
                        .map(|l| SharedString::from(l.to_string())),
                ),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .child(model_chip(ws, cx))
                .child(if running {
                    ui::button("Stop", false, |ws, _w, cx| ws.ai_stop(cx), cx)
                        .into_any_element()
                } else {
                    ui::button("Send", true, |ws, _w, cx| ws.ai_send(cx), cx)
                        .into_any_element()
                }),
        )
}

/// The chip that opens the picker, in the corner where its choice acts.
fn model_chip(ws: &Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    div()
        .id("ai-model-chip")
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .h(px(22.0))
        .px_1p5()
        .rounded_sm()
        .cursor_pointer()
        .bg(gpui::rgb(palette().control_bg))
        .hover(|s| s.bg(gpui::rgb(palette().button_hover)))
        .text_size(px(11.0))
        .text_color(gpui::rgb(palette().text))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|ws, _e, _w, cx| ws.open_ai_model_menu(cx)),
        )
        .child(icon(ws.ai.backend.icon(), 11.0, palette().text_dim))
        .child(SharedString::from(ws.ai_model_name()))
        .child(icon("chevron-down", 10.0, palette().text_dim))
}

/// The picker: search over the live catalogs, a rail per installed
/// harness, and rows named the way the harness names them.
fn model_menu(ws: &Workspace, cx: &mut Context<Workspace>) -> AnyElement {
    let search = &ws.ai.model_search;
    let searching = !search.is_empty();
    let fetching = match ws.ai.menu_backend {
        Backend::Claude => ws.ai.fetching_claude && ws.ai.models_claude.is_none(),
        Backend::Codex => ws.ai.fetching_codex && ws.ai.models_codex.is_none(),
    };
    let current_backend = ws.ai.backend;
    let current_slug = match current_backend {
        Backend::Claude => ws.view.ai_model_claude.clone(),
        Backend::Codex => ws.view.ai_model_codex.clone(),
    };

    let entries = ws.ai_menu_entries();
    let rows: Vec<AnyElement> = if fetching && entries.is_empty() {
        vec![menu_note("Asking the CLI for its models…")]
    } else if entries.is_empty() {
        vec![menu_note("No models match")]
    } else {
        entries
            .into_iter()
            .map(|(backend, entry)| {
                let picked = backend == current_backend && entry.slug == current_slug;
                let slug = entry.slug.clone();
                let sub = if entry.detail.is_empty() {
                    backend.label().to_string()
                } else {
                    format!("{} · {}", backend.label(), entry.detail)
                };
                // gpui's ellipsis needs a real text engine pass this UI
                // kit doesn't do; a character budget sized to the row is
                // how the rest of the chrome truncates.
                let name = ellipsize(&entry.name, if picked { 27 } else { 31 });
                let sub = ellipsize(&sub, 41);
                div()
                    .px_2()
                    .py_1()
                    .flex_none()
                    .flex()
                    .flex_col()
                    .cursor_pointer()
                    .hover(|s| s.bg(gpui::rgb(palette().hover)))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |ws, _e, _w, cx| {
                            ws.ai_pick_model(backend, slug.clone(), cx);
                        }),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_1()
                            .overflow_hidden()
                            .text_size(px(12.0))
                            .text_color(gpui::rgb(palette().text))
                            .child(
                                div()
                                    .flex_grow()
                                    .min_w(px(0.0))
                                    .truncate()
                                    .child(SharedString::from(name)),
                            )
                            .children(picked.then(|| {
                                div()
                                    .flex_none()
                                    .child(icon("check", 12.0, palette().accent_hover))
                            })),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_1()
                            .overflow_hidden()
                            .text_size(px(10.0))
                            .text_color(gpui::rgb(palette().text_faint))
                            .child(div().flex_none().child(icon(
                                backend.icon(),
                                9.0,
                                palette().text_faint,
                            )))
                            .child(
                                div()
                                    .flex_grow()
                                    .min_w(px(0.0))
                                    .truncate()
                                    .child(SharedString::from(sub)),
                            ),
                    )
                    .into_any_element()
            })
            .collect()
    };

    let mut rail = div()
        .flex()
        .flex_col()
        .flex_none()
        .w(px(34.0))
        .items_center()
        .pt_1()
        .gap_1()
        .border_r_1()
        .border_color(gpui::rgb(palette().divider));
    for backend in [Backend::Claude, Backend::Codex] {
        let installed = match backend {
            Backend::Claude => ws.ai.available.0,
            Backend::Codex => ws.ai.available.1,
        };
        if !installed {
            continue;
        }
        // A search spans every harness, so the rail stands down.
        let selected = !searching && ws.ai.menu_backend == backend;
        rail = rail.child(
            div()
                .id(backend.icon())
                .flex()
                .items_center()
                .justify_center()
                .size(px(24.0))
                .rounded_sm()
                .cursor_pointer()
                .when(selected, |d| d.bg(gpui::rgb(palette().selection_bg)))
                .hover(|s| s.bg(gpui::rgb(palette().hover)))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |ws, _e, _w, cx| {
                        ws.ai.menu_backend = backend;
                        ws.ai.model_search.clear();
                        ws.ensure_ai_models(cx);
                        cx.notify();
                    }),
                )
                .child(icon(
                    backend.icon(),
                    13.0,
                    if selected {
                        palette().text
                    } else {
                        palette().text_dim
                    },
                )),
        );
    }

    gpui::deferred(
        div()
            .absolute()
            .bottom(px(34.0))
            .left(px(8.0))
            .w(px(276.0))
            .flex()
            .flex_col()
            .rounded_md()
            .bg(gpui::rgb(palette().popup_bg))
            .border_1()
            .border_color(gpui::rgb(palette().edge))
            .shadow_lg()
            .occlude()
            .on_mouse_down_out(cx.listener(|ws, _e, _w, cx| ws.close_ai_model_menu(cx)))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1p5()
                    .h(px(30.0))
                    .px_2()
                    .border_b_1()
                    .border_color(gpui::rgb(palette().accent))
                    .child(div().flex_none().child(icon("search", 12.0, palette().text_faint)))
                    .child(
                        div()
                            .flex_grow()
                            .min_w(px(0.0))
                            .truncate()
                            .text_size(px(11.0))
                            .text_color(gpui::rgb(if searching {
                                palette().text
                            } else {
                                palette().text_faint
                            }))
                            .child(SharedString::from(if searching {
                                format!("{search}|")
                            } else {
                                "Search models…|".to_string()
                            })),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .min_h(px(60.0))
                    .child(rail)
                    .child(
                        div()
                            .id("ai-models")
                            .flex()
                            .flex_col()
                            .flex_grow()
                            .max_h(px(280.0))
                            .py_1()
                            .overflow_y_scroll()
                            .children(rows),
                    ),
            ),
    )
    .into_any_element()
}

/// Trim to a character budget with an ellipsis, on a char boundary.
fn ellipsize(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let kept: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", kept.trim_end())
}

fn menu_note(text: &'static str) -> AnyElement {
    div()
        .px_2()
        .py_2()
        .text_size(px(11.0))
        .text_color(gpui::rgb(palette().text_faint))
        .child(text)
        .into_any_element()
}
