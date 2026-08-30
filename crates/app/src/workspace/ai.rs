//! The AI sidebar's workspace side: conversations, the drain ticker, and
//! answering the agents' MCP requests against the open document.
//!
//! The worker threads (see `crate::ai`) never touch the workspace; they
//! queue events and MCP requests on shared queues, and a ticker task
//! drains both here on the UI thread. Tool calls run through the same
//! `schist_mcp::dispatch` the headless server uses, against this window's
//! document, registry and editor state — so every edit lands in the undo
//! history and repaints through the ordinary `after_change` path.

use super::*;
use crate::ai::{self, AgentEvent, AiEntry, AiEntryKind, Backend, ConvCmd, ModelEntry};
use serde_json::{json, Value};

/// How often the queues are drained while a conversation is talking.
/// Fast enough that streamed text feels live; the tick is a no-op when
/// both queues are empty.
const AI_TICK_MS: u64 = 33;

impl Workspace {
    pub fn toggle_ai_panel(&mut self, cx: &mut Context<Self>) {
        self.view.ai_panel = !self.view.ai_panel;
        self.status = if self.view.ai_panel {
            "AI panel shown".into()
        } else {
            // The panel's own close button lands here too; say how to
            // get it back.
            format!(
                "AI panel hidden — View ▸ AI Panel ({}) brings it back",
                if cfg!(target_os = "macos") {
                    "Cmd+Shift+A"
                } else {
                    "Ctrl+Shift+A"
                }
            )
            .into()
        };
        if self.view.ai_panel {
            self.ensure_ai_models(cx);
        }
        self.save_view_options();
        cx.notify();
    }

    /// Kick off the model-catalog probes for whichever CLIs are
    /// installed, once per run. The lists arrive through the event queue.
    pub fn ensure_ai_models(&mut self, cx: &mut Context<Self>) {
        if self.ai.available.0 && self.ai.models_claude.is_none() && !self.ai.fetching_claude {
            self.ai.fetching_claude = true;
            ai::models::fetch(Backend::Claude, self.ai.shared.clone());
        }
        if self.ai.available.1 && self.ai.models_codex.is_none() && !self.ai.fetching_codex {
            self.ai.fetching_codex = true;
            ai::models::fetch(Backend::Codex, self.ai.shared.clone());
        }
        if self.ai.fetching_claude || self.ai.fetching_codex {
            self.ensure_ai_ticker(cx);
        }
    }

    /// Pick up the login-shell PATH once its probe answers.
    ///
    /// Asking the shell costs a shell startup, so it runs on a thread
    /// while the window opens (see [`crate::ai::path`]) — which means the
    /// availability this workspace was built with can say the CLIs are
    /// missing when they are merely off launchd's PATH. Poll for the
    /// answer and redo it. The probe gives up on its own, so the loop
    /// always ends.
    pub fn watch_agent_path(&mut self, cx: &mut Context<Self>) {
        if ai::path::ready() {
            return;
        }
        cx.spawn(async move |this, cx| {
            while !ai::path::ready() {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(100))
                    .await;
            }
            this.update(cx, |ws, cx| {
                let found = (Backend::Claude.available(), Backend::Codex.available());
                if found == ws.ai.available {
                    return;
                }
                ws.ai.available = found;
                if ws.view.ai_panel {
                    ws.ensure_ai_models(cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The fetched (or fallback) catalog for a harness.
    pub fn ai_models_for(&self, backend: Backend) -> Vec<ModelEntry> {
        let cached = match backend {
            Backend::Claude => &self.ai.models_claude,
            Backend::Codex => &self.ai.models_codex,
        };
        cached.clone().unwrap_or_else(|| backend.fallback_models())
    }

    /// What the model chip says: the display name of the current pick.
    pub fn ai_model_name(&self) -> String {
        let slug = match self.ai.backend {
            Backend::Claude => &self.view.ai_model_claude,
            Backend::Codex => &self.view.ai_model_codex,
        };
        self.ai_models_for(self.ai.backend)
            .into_iter()
            .find(|m| m.slug == *slug)
            .map(|m| m.name)
            .unwrap_or_else(|| {
                // Nothing used here yet and the catalog hasn't arrived to
                // seed a pick.
                if slug.is_empty() {
                    "…".to_string()
                } else {
                    slug.clone()
                }
            })
    }

    pub fn open_ai_model_menu(&mut self, cx: &mut Context<Self>) {
        if self.ai.model_menu {
            self.close_ai_model_menu(cx);
            return;
        }
        self.ai.model_menu = true;
        self.ai.menu_backend = self.ai.backend;
        self.ai.model_search.clear();
        self.ai.input_active = false;
        self.ensure_ai_models(cx);
        cx.notify();
    }

    pub fn close_ai_model_menu(&mut self, cx: &mut Context<Self>) {
        self.ai.model_menu = false;
        cx.notify();
    }

    /// The rows the picker is showing: the rail's harness normally, every
    /// installed harness once a search narrows things down.
    pub fn ai_menu_entries(&self) -> Vec<(Backend, ModelEntry)> {
        let wanted = self.ai.model_search.to_lowercase();
        let backends: Vec<Backend> = if wanted.is_empty() {
            vec![self.ai.menu_backend]
        } else {
            let mut all = Vec::new();
            if self.ai.available.0 {
                all.push(Backend::Claude);
            }
            if self.ai.available.1 {
                all.push(Backend::Codex);
            }
            all
        };
        let mut out = Vec::new();
        for backend in backends {
            for entry in self.ai_models_for(backend) {
                let matches = wanted.is_empty()
                    || entry.name.to_lowercase().contains(&wanted)
                    || entry.slug.to_lowercase().contains(&wanted)
                    || backend.label().to_lowercase().contains(&wanted);
                if matches {
                    out.push((backend, entry));
                }
            }
        }
        out
    }

    /// Commit a pick: harness and model together, as one gesture.
    pub fn ai_pick_model(&mut self, backend: Backend, slug: String, cx: &mut Context<Self>) {
        if backend != self.ai.backend {
            self.set_ai_backend(backend, cx);
        }
        self.set_ai_model(slug, cx);
        self.close_ai_model_menu(cx);
    }

    /// Keystrokes while the picker is open: type to search, Enter takes
    /// the top match, Escape closes.
    pub fn ai_model_menu_key(&mut self, ev: &gpui::KeyDownEvent, cx: &mut Context<Self>) -> bool {
        if !self.ai.model_menu {
            return false;
        }
        match ev.keystroke.key.as_str() {
            "escape" => self.close_ai_model_menu(cx),
            "enter" => {
                if let Some((backend, entry)) = self.ai_menu_entries().into_iter().next() {
                    self.ai_pick_model(backend, entry.slug, cx);
                }
            }
            "backspace" => {
                self.ai.model_search.pop();
            }
            "space" => self.ai.model_search.push(' '),
            _ => {
                if let Some(t) = ev.keystroke.key_char.as_deref() {
                    if !t.is_empty() && !t.chars().any(char::is_control) {
                        self.ai.model_search.push_str(t);
                    }
                }
            }
        }
        cx.notify();
        true
    }

    /// Switch harness. Ends the current conversation — its session id
    /// belongs to the other backend and cannot travel.
    pub fn set_ai_backend(&mut self, backend: Backend, cx: &mut Context<Self>) {
        if self.ai.backend == backend {
            return;
        }
        self.ai_end_conversation();
        self.ai.session = None;
        self.ai.backend = backend;
        self.view.ai_backend = backend.pref_key().to_string();
        self.save_view_options();
        cx.notify();
    }

    /// The current backend's model override; empty preference means the
    /// CLI's own default.
    pub fn ai_model(&self) -> Option<String> {
        let slug = match self.ai.backend {
            Backend::Claude => &self.view.ai_model_claude,
            Backend::Codex => &self.view.ai_model_codex,
        };
        (!slug.is_empty()).then(|| slug.clone())
    }

    /// Pick a model for the current backend. Takes effect from the next
    /// turn — Claude switches its live session, Codex overrides per turn
    /// — so the conversation keeps going either way.
    pub fn set_ai_model(&mut self, slug: String, cx: &mut Context<Self>) {
        match self.ai.backend {
            Backend::Claude => self.view.ai_model_claude = slug,
            Backend::Codex => self.view.ai_model_codex = slug,
        }
        self.save_view_options();
        cx.notify();
    }

    /// Clear the transcript and forget the session; the next prompt
    /// starts fresh.
    pub fn ai_new_conversation(&mut self, cx: &mut Context<Self>) {
        self.ai_end_conversation();
        self.ai.transcript.clear();
        self.ai.session = None;
        self.ai.input_active = false;
        cx.notify();
    }

    fn ai_end_conversation(&mut self) {
        if let Some(conversation) = self.ai.conversation.take() {
            if self.ai.running {
                conversation.stop();
            }
            conversation.cmds.send(ConvCmd::Shutdown);
        }
        self.ai.running = false;
    }

    /// The Stop button: interrupt the turn in flight.
    pub fn ai_stop(&mut self, cx: &mut Context<Self>) {
        if let Some(conversation) = &self.ai.conversation {
            conversation.stop();
        }
        cx.notify();
    }

    /// Send the typed prompt, starting a conversation worker if none is
    /// live.
    pub fn ai_send(&mut self, cx: &mut Context<Self>) {
        let prompt = self.ai.input.trim().to_string();
        if prompt.is_empty() || self.ai.running {
            return;
        }
        let backend = self.ai.backend;
        if !backend.available() {
            self.ai.transcript.push(AiEntry {
                kind: AiEntryKind::Error,
                text: format!(
                    "The {} CLI ({:?}) is not on PATH. Install and log into it first.",
                    backend.label(),
                    backend.binary()
                ),
            });
            cx.notify();
            return;
        }
        // The catalog is built from this window's registry, so the tool
        // list the agent sees carries the options bar's current values.
        if self.ai.catalog.is_none() {
            self.ai.catalog = Some(schist_mcp::Catalog::from_registry_scoped(
                &self.registry,
                schist_mcp::Scope::Active,
            ));
        }
        if self.ai.conversation.is_none() {
            let resume = self.ai.session.clone();
            let shared = self.ai.shared.clone();
            let conversation = match backend {
                Backend::Claude => ai::claude::start(shared, resume),
                Backend::Codex => {
                    if self.ai.endpoint.is_none() {
                        match ai::endpoint::Endpoint::start(self.ai.shared.clone()) {
                            Ok(endpoint) => self.ai.endpoint = Some(endpoint),
                            Err(e) => {
                                self.ai.transcript.push(AiEntry {
                                    kind: AiEntryKind::Error,
                                    text: format!("starting the MCP endpoint failed: {e:#}"),
                                });
                                cx.notify();
                                return;
                            }
                        }
                    }
                    let endpoint = self.ai.endpoint.as_ref().unwrap();
                    ai::codex::start(
                        shared,
                        endpoint.addr.clone(),
                        endpoint.token.clone(),
                        resume,
                    )
                }
            };
            self.ai.conversation = Some(conversation);
        }
        self.ai.input.clear();
        self.ai.transcript.push(AiEntry {
            kind: AiEntryKind::User,
            text: prompt.clone(),
        });
        self.ai.running = true;
        let model = self.ai_model();
        if let Some(conversation) = &self.ai.conversation {
            conversation.cmds.send(ConvCmd::Say { prompt, model });
        }
        self.ensure_ai_ticker(cx);
        self.ai.scroll.scroll_to_bottom();
        cx.notify();
    }

    /// Feed a keystroke to the prompt box. Consumes every key while it is
    /// focused, so tool shortcuts can't fire mid-sentence.
    pub fn ai_input_key(&mut self, ev: &gpui::KeyDownEvent, cx: &mut Context<Self>) -> bool {
        if !self.ai.input_active {
            return false;
        }
        match ev.keystroke.key.as_str() {
            "escape" => self.ai.input_active = false,
            // Enter sends; a paragraph break is Shift+Enter, as in every
            // chat box.
            "enter" if ev.keystroke.modifiers.shift => self.ai.input.push('\n'),
            "enter" => self.ai_send(cx),
            "backspace" => {
                self.ai.input.pop();
            }
            "space" => self.ai.input.push(' '),
            _ => {
                if let Some(t) = ev.keystroke.key_char.as_deref() {
                    if !t.is_empty() && !t.chars().any(char::is_control) {
                        self.ai.input.push_str(t);
                    }
                }
            }
        }
        cx.notify();
        true
    }

    /// One drain task at a time; it stands down once the turn is over and
    /// the queues have gone quiet, and `ai_send` starts it again.
    fn ensure_ai_ticker(&mut self, cx: &mut Context<Self>) {
        if self.ai.ticker {
            return;
        }
        self.ai.ticker = true;
        cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(AI_TICK_MS))
                .await;
            match this.update(cx, |ws, cx| ws.ai_tick(cx)) {
                Ok(true) => {}
                _ => break,
            }
        })
        .detach();
    }

    /// Drain both queues. Returns whether the ticker should keep running.
    fn ai_tick(&mut self, cx: &mut Context<Self>) -> bool {
        let requests: Vec<ai::McpRequest> = match self.ai.shared.mcp.lock() {
            Ok(mut q) => q.drain(..).collect(),
            Err(_) => Vec::new(),
        };
        let had_requests = !requests.is_empty();
        for request in requests {
            log::debug!(
                "mcp answering: {}",
                request
                    .message
                    .get("method")
                    .and_then(|m| m.as_str())
                    .unwrap_or("?")
            );
            let reply = self.ai_mcp_reply(&request.message, cx);
            (request.reply)(reply);
        }
        let events: Vec<AgentEvent> = match self.ai.shared.events.lock() {
            Ok(mut q) => q.drain(..).collect(),
            Err(_) => Vec::new(),
        };
        let had_events = !events.is_empty();
        for event in events {
            self.ai_apply_event(event);
        }
        if had_events {
            self.ai.scroll.scroll_to_bottom();
        }
        if had_events || had_requests {
            cx.notify();
        }
        let keep = self.ai.running
            || self.ai.fetching_claude
            || self.ai.fetching_codex
            || had_events
            || had_requests;
        if !keep {
            self.ai.ticker = false;
        }
        keep
    }

    fn ai_apply_event(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::Text(text) => match self.ai.transcript.last_mut() {
                Some(last) if last.kind == AiEntryKind::Assistant => last.text.push_str(&text),
                _ => self.ai.transcript.push(AiEntry {
                    kind: AiEntryKind::Assistant,
                    text,
                }),
            },
            AgentEvent::ToolCall(name) => self.ai.transcript.push(AiEntry {
                kind: AiEntryKind::Tool,
                text: name,
            }),
            AgentEvent::Session(id) => self.ai.session = Some(id),
            AgentEvent::Models(backend, models) => {
                // The picker holds concrete models only; if nothing has
                // been used in this app yet (or the remembered slug is no
                // longer offered), seed from the harness's recommendation
                // — from then on it's simply what was last used here.
                let slot = match backend {
                    Backend::Claude => &mut self.view.ai_model_claude,
                    Backend::Codex => &mut self.view.ai_model_codex,
                };
                if !models.iter().any(|m| m.slug == *slot) {
                    let seed = models
                        .iter()
                        .find(|m| m.recommended)
                        .or_else(|| models.first());
                    if let Some(seed) = seed {
                        *slot = seed.slug.clone();
                        self.save_view_options();
                    }
                }
                match backend {
                    Backend::Claude => {
                        self.ai.models_claude = Some(models);
                        self.ai.fetching_claude = false;
                    }
                    Backend::Codex => {
                        self.ai.models_codex = Some(models);
                        self.ai.fetching_codex = false;
                    }
                }
            }
            AgentEvent::Info(text) => self.ai.transcript.push(AiEntry {
                kind: AiEntryKind::Info,
                text,
            }),
            AgentEvent::Error(text) => self.ai.transcript.push(AiEntry {
                kind: AiEntryKind::Error,
                text,
            }),
            AgentEvent::TurnDone => self.ai.running = false,
            AgentEvent::Closed => {
                self.ai.conversation = None;
                self.ai.running = false;
            }
        }
    }

    /// Answer one MCP JSON-RPC request against the open document. The
    /// reply is a full envelope; tool failures are `isError` content, and
    /// only protocol misuse is a JSON-RPC error — matching the stdio
    /// server.
    fn ai_mcp_reply(&mut self, message: &Value, cx: &mut Context<Self>) -> Value {
        let id = message.get("id").cloned().unwrap_or(Value::Null);
        let method = message.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let envelope =
            |id: Value, result: Value| json!({"jsonrpc": "2.0", "id": id, "result": result});
        let rpc_error = |id: Value, code: i64, message: String| json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}});
        match method {
            "initialize" => {
                let requested = params
                    .get("protocolVersion")
                    .and_then(|v| v.as_str())
                    .unwrap_or("2025-03-26");
                envelope(
                    id,
                    json!({
                        "protocolVersion": requested,
                        "capabilities": {"tools": {}},
                        "serverInfo": {"name": "schist", "version": env!("CARGO_PKG_VERSION")},
                        "instructions": "The Schist image editor, operating on the document \
                            currently open in the app. There are no sessions to manage: every \
                            tool acts on the current document, edits appear on the user's \
                            canvas immediately, and each lands as one entry in the undo \
                            history. get_state describes the document; render shows it.",
                    }),
                )
            }
            "ping" => envelope(id, json!({})),
            "tools/list" => {
                if self.ai.catalog.is_none() {
                    self.ai.catalog = Some(schist_mcp::Catalog::from_registry_scoped(
                        &self.registry,
                        schist_mcp::Scope::Active,
                    ));
                }
                envelope(
                    id,
                    json!({"tools": self.ai.catalog.as_ref().unwrap().defs()}),
                )
            }
            "tools/call" => {
                let Some(name) = params.get("name").and_then(|v| v.as_str()) else {
                    return rpc_error(id, -32602, "missing tool name".into());
                };
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                let action = self
                    .ai
                    .catalog
                    .as_ref()
                    .and_then(|c| c.action(name))
                    .cloned();
                let outcome = match action {
                    None => Err(anyhow::anyhow!("unknown tool {name:?}")),
                    Some(action) => match self.doc.as_mut() {
                        None => Err(anyhow::anyhow!("no document is open in Schist")),
                        Some(doc) => {
                            let mut sess = schist_mcp::SessionCtx {
                                doc,
                                state: &mut self.editor,
                                registry: &mut self.registry,
                                photoshop: Some(&self.photoshop_plugins),
                            };
                            schist_mcp::dispatch::call_action(&mut sess, &action, &args, None)
                        }
                    },
                };
                self.status = format!("AI: {name}").into();
                // The canvas, panels and history must reflect whatever the
                // call did before the agent's next look at the document.
                self.after_change(cx);
                match outcome {
                    Ok(content) => envelope(id, json!({"content": content})),
                    Err(e) => envelope(
                        id,
                        json!({
                            "content": [{"type": "text", "text": format!("{e:#}")}],
                            "isError": true,
                        }),
                    ),
                }
            }
            other => rpc_error(id, -32601, format!("method not found: {other}")),
        }
    }
}
