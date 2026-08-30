//! The Claude Code backend, through the Claude Agent SDK (Rust port).
//!
//! One worker thread per conversation, running a single-threaded tokio
//! runtime for the SDK's bidirectional client. The app's MCP tools are
//! served *in-process*: the SDK routes the CLI's `mcp_message` control
//! requests to [`PanelServer`], which queues them for the UI thread and
//! awaits the reply — no socket, no bridge, no second process.

use super::{path, AgentEvent, AiShared, Backend, CmdSender, ConvCmd, Conversation, SYSTEM_PROMPT};
use claude_agent_sdk_rs::types::mcp::McpSdkServerConfig;
use claude_agent_sdk_rs::{
    ClaudeAgentOptions, ClaudeClient, ClaudeError, McpServerConfig, McpServers, Message,
    PermissionMode, SdkMcpServer, SystemPrompt,
};
use futures::StreamExt as _;
use std::collections::HashMap;
use std::sync::Arc;

/// Serves the whole MCP conversation (initialize, tools/list, tools/call)
/// by forwarding each raw JSON-RPC message to the UI thread.
struct PanelServer {
    shared: AiShared,
}

#[async_trait::async_trait]
impl SdkMcpServer for PanelServer {
    async fn handle_message(
        &self,
        message: serde_json::Value,
    ) -> claude_agent_sdk_rs::Result<serde_json::Value> {
        // Notifications take no reply, matching the stdio server.
        if message.get("id").is_none() {
            return Ok(serde_json::Value::Null);
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.shared.ask(
            message,
            Box::new(move |reply| {
                let _ = tx.send(reply);
            }),
        );
        rx.await
            .map_err(|_| ClaudeError::Transport("the workspace went away".into()))
    }
}

/// Start a Claude Code conversation worker. `resume` continues an earlier
/// session by id, so a conversation survives the panel being closed.
pub fn start(shared: AiShared, resume: Option<String>) -> Conversation {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let worker = shared.clone();
    let spawned = std::thread::Builder::new()
        .name("ai-claude".into())
        .spawn(move || {
            match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime.block_on(run(&worker, rx, resume)),
                Err(e) => worker.error(format!("starting the agent runtime failed: {e}")),
            }
            worker.push(AgentEvent::Closed);
        });
    if let Err(e) = spawned {
        shared.error(format!("starting the Claude worker failed: {e}"));
        shared.push(AgentEvent::Closed);
    }
    Conversation {
        cmds: CmdSender::Tokio(tx),
        pid: Default::default(),
    }
}

fn options(shared: &AiShared, resume: Option<String>) -> ClaudeAgentOptions {
    let mut servers = HashMap::new();
    servers.insert(
        "schist".to_string(),
        McpServerConfig::Sdk(McpSdkServerConfig {
            name: "schist".to_string(),
            instance: Arc::new(PanelServer {
                shared: shared.clone(),
            }),
        }),
    );
    ClaudeAgentOptions {
        mcp_servers: McpServers::Dict(servers),
        allowed_tools: vec!["mcp__schist".to_string()],
        system_prompt: Some(SystemPrompt::Text(SYSTEM_PROMPT.to_string())),
        permission_mode: Some(PermissionMode::Default),
        include_partial_messages: true,
        resume,
        // The conversation is about the canvas, not about whatever
        // directory the app was launched from — and with no
        // setting_sources, no CLAUDE.md or filesystem settings leak in
        // either.
        cwd: std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(std::path::PathBuf::from),
        // Found on the login shell's PATH rather than launchd's, which is
        // all a Finder launch inherits; `env` hands that same PATH to
        // whatever the CLI spawns in turn. `None` lets the SDK run its
        // own search and say its own thing when it comes up empty.
        cli_path: Backend::Claude.locate(),
        env: path::child_env(),
        ..Default::default()
    }
}

async fn run(
    shared: &AiShared,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<ConvCmd>,
    resume: Option<String>,
) {
    let mut client = ClaudeClient::new(options(shared, resume));
    if let Err(e) = client.connect().await {
        shared.error(format!("Claude Code did not start: {e}"));
        return;
    }
    shared.push(AgentEvent::Info("Claude Code connected".into()));
    // The model applied to the live session; the CLI keeps it until told
    // otherwise, so it only needs saying when the selection changes.
    let mut applied_model: Option<String> = None;
    while let Some(cmd) = rx.recv().await {
        let (prompt, model) = match cmd {
            ConvCmd::Say { prompt, model } => (prompt, model),
            ConvCmd::Interrupt => continue,
            ConvCmd::Shutdown => break,
        };
        if model != applied_model {
            match client.set_model(model.as_deref()).await {
                Ok(()) => applied_model = model,
                Err(e) => shared.error(format!("switching model failed: {e}")),
            }
        }
        if let Err(e) = client.query(prompt).await {
            shared.error(format!("sending the prompt failed: {e}"));
            shared.push(AgentEvent::TurnDone);
            continue;
        }
        let mut stream = client.receive_response();
        loop {
            tokio::select! {
                item = stream.next() => match item {
                    None => break,
                    // The CLI grows message types (rate_limit_event, …)
                    // faster than the SDK's enum learns them; an unknown
                    // one is chatter, not a broken turn.
                    Some(Err(ClaudeError::MessageParse(e))) => {
                        log::debug!("skipping unparsed CLI message: {e}");
                    }
                    Some(Err(e)) => {
                        shared.error(format!("{e}"));
                        break;
                    }
                    Some(Ok(message)) => {
                        if forward(shared, message) {
                            break;
                        }
                    }
                },
                cmd = rx.recv() => match cmd {
                    Some(ConvCmd::Interrupt) => {
                        if let Err(e) = client.interrupt().await {
                            shared.error(format!("interrupt failed: {e}"));
                        }
                    }
                    Some(ConvCmd::Say { .. }) => {
                        // One turn at a time; the UI disables Send while
                        // running, so this is belt-and-braces.
                    }
                    Some(ConvCmd::Shutdown) | None => {
                        let _ = client.interrupt().await;
                        break;
                    }
                },
            }
        }
        drop(stream);
        shared.push(AgentEvent::TurnDone);
    }
    let _ = client.disconnect().await;
}

/// Map one SDK message onto transcript events. Returns true when the turn
/// is over.
fn forward(shared: &AiShared, message: Message) -> bool {
    match message {
        Message::StreamEvent(ev) => {
            let event = &ev.event;
            match event["type"].as_str() {
                Some("content_block_start") => {
                    let block = &event["content_block"];
                    if block["type"] == "tool_use" {
                        if let Some(name) = block["name"].as_str() {
                            shared.push(AgentEvent::ToolCall(display_tool(name)));
                        }
                    }
                }
                Some("content_block_delta") if event["delta"]["type"] == "text_delta" => {
                    if let Some(text) = event["delta"]["text"].as_str() {
                        shared.push(AgentEvent::Text(text.to_string()));
                    }
                }
                _ => {}
            }
            false
        }
        // Text and tool calls already arrived through the partial stream
        // events; the assembled message only confirms the session id.
        Message::Assistant(m) => {
            if let Some(id) = m.session_id {
                shared.push(AgentEvent::Session(id));
            }
            false
        }
        Message::System(m) => {
            if let Some(id) = m.session_id {
                shared.push(AgentEvent::Session(id));
            }
            false
        }
        Message::Result(r) => {
            shared.push(AgentEvent::Session(r.session_id));
            if r.is_error {
                shared.error(r.result.unwrap_or(r.subtype));
            }
            true
        }
        Message::User(_) | Message::ControlCancelRequest(_) => false,
    }
}

/// `mcp__schist__filter_gaussian_blur` reads better as its own name.
fn display_tool(name: &str) -> String {
    name.strip_prefix("mcp__schist__")
        .unwrap_or(name)
        .to_string()
}
