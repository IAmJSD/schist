//! No-op stand-ins for the AI-sidebar entry points the shared UI calls.
//!
//! The sidebar itself is compiled out on the web (see `crate::ai_stub`);
//! the stub `AiState`'s flags are never set, so these can never be
//! reached with anything to do — they exist so the escape-key chain and
//! the action table compile unchanged.

use super::Workspace;
use gpui::Context;

impl Workspace {
    pub fn toggle_ai_panel(&mut self, _cx: &mut Context<Self>) {}
    pub fn close_ai_model_menu(&mut self, _cx: &mut Context<Self>) {}
    pub fn ai_model_menu_key(&mut self, _ev: &gpui::KeyDownEvent, _cx: &mut Context<Self>) -> bool {
        false
    }
    pub fn ai_input_key(&mut self, _ev: &gpui::KeyDownEvent, _cx: &mut Context<Self>) -> bool {
        false
    }
}
