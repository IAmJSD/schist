//! The Codex backend, through its app-server protocol (`codex-codes`).
//!
//! One worker thread per conversation, holding a synchronous client on a
//! long-lived `codex app-server` process. Codex spawns MCP servers as
//! child processes, so it is configured to spawn this same binary in
//! `--mcp-bridge` mode, which pipes into the app's loopback endpoint.
//!
//! The protocol read is blocking, so a mid-turn stop kills the app-server
//! process (the pid is shared with [`Conversation`]); the thread id
//! survives and the next conversation resumes it.

use super::{path, AgentEvent, AiShared, Backend, CmdSender, ConvCmd, Conversation};
use anyhow::{Context as _, Result};
use codex_codes::cli::AppServerBuilder;
use codex_codes::client_sync::SyncClient;
use codex_codes::{
    AskForApproval, ClientInfo, CommandExecutionApprovalDecision,
    CommandExecutionRequestApprovalResponse, FileChangeApprovalDecision,
    FileChangeRequestApprovalResponse, InitializeParams, McpServerElicitationAction,
    McpServerElicitationRequestResponse, Notification, SandboxMode, ServerMessage, ServerRequest,
    ThreadItem, ThreadResumeParams, ThreadStartParams, TurnStartParams, UserInput,
};
use std::sync::{Arc, Mutex};

/// Start a Codex conversation worker. `resume` continues an earlier
/// thread by id.
pub fn start(
    shared: AiShared,
    addr: String,
    token: String,
    resume: Option<String>,
    system_prompt: String,
) -> Conversation {
    let (tx, rx) = std::sync::mpsc::channel();
    let pid: Arc<Mutex<Option<u32>>> = Default::default();
    let worker_pid = pid.clone();
    let worker = shared.clone();
    let spawned = std::thread::Builder::new()
        .name("ai-codex".into())
        .spawn(move || {
            if let Err(e) = run(
                &worker,
                rx,
                &worker_pid,
                &addr,
                &token,
                resume,
                &system_prompt,
            ) {
                worker.error(format!("{e:#}"));
            }
            worker.push(AgentEvent::Closed);
        });
    if let Err(e) = spawned {
        shared.error(format!("starting the Codex worker failed: {e}"));
        shared.push(AgentEvent::Closed);
    }
    Conversation {
        cmds: CmdSender::Std(tx),
        pid,
    }
}

fn home_dir() -> Option<String> {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()
}

fn run(
    shared: &AiShared,
    rx: std::sync::mpsc::Receiver<ConvCmd>,
    pid: &Arc<Mutex<Option<u32>>>,
    addr: &str,
    token: &str,
    resume: Option<String>,
    system_prompt: &str,
) -> Result<()> {
    let exe = std::env::current_exe().context("locating the schist binary")?;
    // The CLI is located on the login shell's PATH, not launchd's — the
    // builder's own `which` would only see what a Finder launch inherits
    // — and that PATH is passed on to the app-server's own children.
    let mut builder = AppServerBuilder::new().env("PATH", path::resolved());
    if let Some(codex) = Backend::Codex.locate() {
        builder = builder.command(codex);
    }
    // The bridge command and its token, as codex config: the value side of
    // each `-c` is TOML.
    builder = builder
        // A canvas conversation is deliberately self-contained. The
        // user's Codex memories belong to their coding work, and feeding
        // them into this embedded assistant both confuses its role and
        // can disclose context from an unrelated project. Do not consume
        // those memories, and do not turn photo-editing conversations into
        // inputs for future global memories either.
        .config_override("features.memories", "false")
        .config_override("memories.use_memories", "false")
        .config_override("memories.generate_memories", "false")
        // Requests in this panel authorize the app's editor tools. With
        // `never` below, Codex rejects any tool that still needs consent
        // before our request handler can see it. Pre-approve this server
        // specifically; other MCP servers keep their own policies.
        .config_override(
            "mcp_servers.schist.default_tools_approval_mode",
            "\"approve\"",
        )
        .config_override(
            "mcp_servers.schist.command",
            toml_string(&exe.display().to_string()),
        )
        .config_override(
            "mcp_servers.schist.args",
            format!("[\"--mcp-bridge\", {}]", toml_string(addr)),
        )
        .config_override(
            "mcp_servers.schist.env",
            format!("{{ SCHIST_MCP_TOKEN = {} }}", toml_string(token)),
        );
    if let Some(home) = home_dir() {
        builder = builder.working_directory(home);
    }
    let child = builder.spawn_sync().context("starting codex app-server")?;
    if let Ok(mut slot) = pid.lock() {
        *slot = Some(child.id());
    }
    let mut client = SyncClient::new(child).context("attaching to codex app-server")?;
    client
        .initialize(&InitializeParams {
            client_info: ClientInfo {
                name: "schist".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                title: Some("Schist".to_string()),
            },
            capabilities: None,
        })
        .context("initializing codex app-server")?;

    // Read-only sandbox and no approval prompts: the agent's hands are
    // the schist MCP tools, and there is no dialog here to answer an
    // approval with — anything that would need one is declined below.
    let thread_id = match resume {
        Some(id) => {
            client
                .thread_resume(&ThreadResumeParams {
                    thread_id: id.clone(),
                    cwd: home_dir(),
                    approval_policy: Some(AskForApproval::Never),
                    sandbox: Some(SandboxMode::Read_only),
                    developer_instructions: Some(system_prompt.to_string()),
                    ..Default::default()
                })
                .context("resuming codex thread")?;
            id
        }
        None => {
            let started = client
                .thread_start(&ThreadStartParams {
                    cwd: home_dir(),
                    approval_policy: Some(AskForApproval::Never),
                    sandbox: Some(SandboxMode::Read_only),
                    developer_instructions: Some(system_prompt.to_string()),
                    ..Default::default()
                })
                .context("starting codex thread")?;
            started.thread.id
        }
    };
    shared.push(AgentEvent::Session(thread_id.clone()));
    shared.push(AgentEvent::Info("Codex connected".into()));

    while let Ok(cmd) = rx.recv() {
        let (prompt, model) = match cmd {
            ConvCmd::Say { prompt, model } => (prompt, model),
            ConvCmd::Interrupt => continue,
            ConvCmd::Shutdown => break,
        };
        if let Err(e) = client.turn_start(&TurnStartParams {
            thread_id: thread_id.clone(),
            input: vec![UserInput::Text {
                text: prompt,
                text_elements: None,
            }],
            // Per-turn override; None inherits the thread's model.
            model,
            ..Default::default()
        }) {
            shared.error(format!("sending the prompt failed: {e}"));
            shared.push(AgentEvent::TurnDone);
            continue;
        }
        loop {
            match client.next_message() {
                // EOF: killed by the stop button, or crashed. Either way
                // the conversation is over; the thread id survives for a
                // resume.
                Ok(None) => {
                    shared.push(AgentEvent::TurnDone);
                    return Ok(());
                }
                Err(e) => {
                    shared.error(format!("{e}"));
                    shared.push(AgentEvent::TurnDone);
                    return Ok(());
                }
                Ok(Some(ServerMessage::Notification(n))) => {
                    if forward(shared, n) {
                        break;
                    }
                }
                Ok(Some(ServerMessage::Request { id, request })) => {
                    respond_declining(&mut client, id, request);
                }
            }
        }
        shared.push(AgentEvent::TurnDone);
    }
    let _ = client.shutdown();
    Ok(())
}

/// Map one notification onto transcript events. Returns true when the
/// turn is over.
fn forward(shared: &AiShared, notification: Notification) -> bool {
    match notification {
        Notification::AgentMessageDelta(d) => {
            shared.push(AgentEvent::Text(d.delta));
            false
        }
        Notification::ItemStarted(item) => {
            match item.item {
                ThreadItem::McpToolCall { tool, .. } => {
                    shared.push(AgentEvent::ToolCall(tool));
                }
                ThreadItem::CommandExecution { .. } => {
                    shared.push(AgentEvent::ToolCall("shell command".into()));
                }
                _ => {}
            }
            false
        }
        Notification::TurnCompleted(_) => true,
        Notification::Error(e) => {
            shared.error(e.error.message);
            // An error notification mid-turn may still be followed by
            // turn/completed; when it is not, the next read's EOF or the
            // user's stop ends things.
            false
        }
        _ => false,
    }
}

/// Editor tools are pre-approved in the Schist server configuration.
/// Decline any remaining approval requests, including elicitations from
/// other MCP servers inherited from the user's config. Anything else
/// gets an error so the server is never left waiting.
fn respond_declining(client: &mut SyncClient, id: codex_codes::RequestId, request: ServerRequest) {
    let result = match request {
        ServerRequest::McpServerElicitationRequest(_) => client.respond(
            id,
            &McpServerElicitationRequestResponse {
                _meta: None,
                action: McpServerElicitationAction::Decline,
                content: None,
            },
        ),
        ServerRequest::CmdExecApproval(_) => client.respond(
            id,
            &CommandExecutionRequestApprovalResponse {
                decision: CommandExecutionApprovalDecision::Decline,
            },
        ),
        ServerRequest::FileChangeApproval(_) => client.respond(
            id,
            &FileChangeRequestApprovalResponse {
                decision: FileChangeApprovalDecision::Decline,
            },
        ),
        other => client.respond_error(
            id,
            -32601,
            &format!("{} is not supported in the Schist AI panel", other.method()),
        ),
    };
    if let Err(e) = result {
        log::warn!("codex approval response failed: {e}");
    }
}

/// A string as a TOML literal, for `-c key=value` overrides.
fn toml_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}
