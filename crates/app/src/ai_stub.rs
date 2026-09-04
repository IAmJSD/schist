//! The web build's stand-in for `crate::ai`.
//!
//! The AI sidebar works by spawning the user's `claude`/`codex` CLIs, and a
//! browser tab cannot spawn anything, so the whole subsystem is compiled out
//! on wasm32. This stub keeps just the two flags the shared UI code reads
//! (the escape-key chain and the key-context selection in render), so those
//! files compile unchanged; both stay false forever.

#[derive(Default)]
pub struct AiState {
    pub input_active: bool,
    pub model_menu: bool,
}
