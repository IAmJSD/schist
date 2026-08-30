//! The AI sidebar: a transcript of the conversation with an agent
//! harness, streamed as it happens, and a prompt box.
//!
//! Rendering only — the conversation lives in `crate::ai` workers and the
//! workspace's `ai_*` methods. The prompt box follows the notes panel's
//! text-entry pattern: a plain buffer, a drawn `|` caret, one child div
//! per line.

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
        .child(header(ws, cx))
        .child(transcript(ws))
        .child(prompt_box(ws, cx));
    Some(panel.into_any_element())
}

fn header(ws: &Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
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
                .child(backend_button(ws, Backend::Claude, cx))
                .child(backend_button(ws, Backend::Codex, cx))
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
                ),
        )
}

fn backend_button(ws: &Workspace, backend: Backend, cx: &mut Context<Workspace>) -> impl IntoElement {
    let selected = ws.ai.backend == backend;
    let installed = match backend {
        Backend::Claude => ws.ai.available.0,
        Backend::Codex => ws.ai.available.1,
    };
    let label = match backend {
        Backend::Claude => "Claude",
        Backend::Codex => "Codex",
    };
    div()
        .id(match backend {
            Backend::Claude => "ai-backend-claude",
            Backend::Codex => "ai-backend-codex",
        })
        .px_1p5()
        .h(px(20.0))
        .flex()
        .items_center()
        .rounded_sm()
        .text_size(px(11.0))
        .text_color(gpui::rgb(if installed {
            palette().text
        } else {
            palette().text_faint
        }))
        .when_active(selected)
        .cursor_pointer()
        .hover(|s| s.bg(gpui::rgb(palette().hover)))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |ws, _e, _w, cx| ws.set_ai_backend(backend, cx)),
        )
        .child(label)
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
        .flex()
        .flex_col()
        .flex_none()
        .p_2()
        .gap_1()
        .border_t_1()
        .border_color(gpui::rgb(palette().panel_edge))
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
                .child(
                    div()
                        .text_size(px(10.0))
                        .text_color(gpui::rgb(palette().text_faint))
                        .child(if editing { "Enter sends · Shift+Enter breaks" } else { "" }),
                )
                .child(if running {
                    ui::button("Stop", false, |ws, _w, cx| ws.ai_stop(cx), cx)
                        .into_any_element()
                } else {
                    ui::button("Send", true, |ws, _w, cx| ws.ai_send(cx), cx)
                        .into_any_element()
                }),
        )
}
