//! The AI sidebar's plumbing: agent harnesses driven through their SDKs,
//! feeding a transcript, with the app's own MCP server as their hands.
//!
//! Two worker shapes, one contract. A conversation runs on its own thread
//! (Claude Code through the Claude Agent SDK on a small tokio runtime,
//! Codex through its app-server protocol client, synchronously) and talks
//! back exclusively through [`AiShared`]: agent events queue up for the
//! workspace's ticker to drain, and MCP requests queue up for the
//! workspace to answer against the live document. Nothing here touches
//! the UI or the document directly — the UI thread owns both.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

pub mod claude;
pub mod codex;
pub mod endpoint;

/// Which agent harness a conversation runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Claude,
    Codex,
}

impl Backend {
    pub fn label(self) -> &'static str {
        match self {
            Backend::Claude => "Claude Code",
            Backend::Codex => "Codex",
        }
    }

    /// The CLI the backend drives; the SDKs spawn it themselves.
    pub fn binary(self) -> &'static str {
        match self {
            Backend::Claude => "claude",
            Backend::Codex => "codex",
        }
    }

    /// Whether the CLI is installed, by the same lookup the SDKs use.
    pub fn available(self) -> bool {
        let path = std::env::var_os("PATH").unwrap_or_default();
        std::env::split_paths(&path).any(|dir| {
            let file = dir.join(self.binary());
            #[cfg(windows)]
            let file = file.with_extension("exe");
            file.is_file()
        })
    }

    /// The preference-file spelling.
    pub fn pref_key(self) -> &'static str {
        match self {
            Backend::Claude => "claude",
            Backend::Codex => "codex",
        }
    }

    pub fn from_pref(key: &str) -> Backend {
        match key {
            "codex" => Backend::Codex,
            _ => Backend::Claude,
        }
    }

    /// The models the sidebar offers, as (display name, slug). An empty
    /// slug means the CLI's own default. Claude Code resolves the aliases
    /// to the latest model of each tier itself; the Codex list is the
    /// SDK's bundled catalog, so it tracks the crate rather than us.
    pub fn models(self) -> Vec<(&'static str, String)> {
        match self {
            Backend::Claude => vec![
                ("Default", String::new()),
                ("Opus", "opus".to_string()),
                ("Sonnet", "sonnet".to_string()),
                ("Haiku", "haiku".to_string()),
            ],
            Backend::Codex => {
                let mut out = vec![("Default", String::new())];
                out.extend(
                    codex_codes::CodexModel::known()
                        .iter()
                        // Hidden from Codex's own picker; same here.
                        .filter(|m| !matches!(m, codex_codes::CodexModel::CodexAutoReview))
                        .map(|m| (m.display_name(), m.cli_arg().to_string())),
                );
                out
            }
        }
    }
}

/// What a conversation worker reports back, drained by the UI ticker.
pub enum AgentEvent {
    /// A streamed piece of assistant text.
    Text(String),
    /// A tool call started, by display name.
    ToolCall(String),
    /// The backend's session/thread id, for resuming.
    Session(String),
    /// A status line worth a dim row in the transcript.
    Info(String),
    Error(String),
    /// The turn finished (successfully or not — an Error precedes if not).
    TurnDone,
    /// The worker exited; the conversation is over.
    Closed,
}

/// What the UI sends a conversation worker.
pub enum ConvCmd {
    Say {
        prompt: String,
        /// Model slug for this turn; `None` is the CLI's own default.
        model: Option<String>,
    },
    Interrupt,
    Shutdown,
}

/// One MCP request awaiting the UI thread: the raw JSON-RPC message and a
/// completion to hand the raw JSON-RPC reply.
pub struct McpRequest {
    pub message: serde_json::Value,
    pub reply: Box<dyn FnOnce(serde_json::Value) + Send>,
}

/// The queues a worker shares with the workspace.
#[derive(Clone, Default)]
pub struct AiShared {
    pub events: Arc<Mutex<VecDeque<AgentEvent>>>,
    pub mcp: Arc<Mutex<VecDeque<McpRequest>>>,
}

impl AiShared {
    pub fn push(&self, event: AgentEvent) {
        if let Ok(mut q) = self.events.lock() {
            q.push_back(event);
        }
    }

    pub fn error(&self, message: impl Into<String>) {
        self.push(AgentEvent::Error(message.into()));
    }

    /// Queue an MCP request for the UI thread; the reply lands in `reply`.
    pub fn ask(&self, message: serde_json::Value, reply: Box<dyn FnOnce(serde_json::Value) + Send>) {
        if let Ok(mut q) = self.mcp.lock() {
            q.push_back(McpRequest { message, reply });
        }
    }
}

/// A sender that reaches whichever channel flavour the worker listens on.
pub enum CmdSender {
    Std(std::sync::mpsc::Sender<ConvCmd>),
    Tokio(tokio::sync::mpsc::UnboundedSender<ConvCmd>),
}

impl CmdSender {
    pub fn send(&self, cmd: ConvCmd) {
        match self {
            CmdSender::Std(tx) => {
                let _ = tx.send(cmd);
            }
            CmdSender::Tokio(tx) => {
                let _ = tx.send(cmd);
            }
        }
    }
}

/// A live conversation: the handle the workspace holds.
pub struct Conversation {
    pub cmds: CmdSender,
    /// The harness process id, filled in by the worker once it has
    /// spawned. Killing it is the only way to stop Codex mid-turn; Claude
    /// interrupts over its control channel and leaves this empty.
    pub pid: Arc<Mutex<Option<u32>>>,
}

impl Conversation {
    /// Stop whatever is in flight. Claude gets a graceful interrupt; the
    /// Codex worker sits in a blocking protocol read, so its app-server
    /// process is killed and the read's EOF ends the worker.
    pub fn stop(&self) {
        self.cmds.send(ConvCmd::Interrupt);
        if let Some(pid) = self.pid.lock().ok().and_then(|p| *p) {
            kill_process(pid);
        }
    }
}

fn kill_process(pid: u32) {
    #[cfg(unix)]
    let mut cmd = {
        let mut c = std::process::Command::new("kill");
        c.arg("-9").arg(pid.to_string());
        c
    };
    #[cfg(windows)]
    let mut cmd = {
        let mut c = std::process::Command::new("taskkill");
        c.args(["/PID", &pid.to_string(), "/T", "/F"]);
        c
    };
    let _ = cmd
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// The system prompt both harnesses run under. They are coding agents by
/// upbringing; this points them at the canvas instead.
pub const SYSTEM_PROMPT: &str = "You are the AI assistant panel inside Schist, an image editor. \
    The `schist` MCP server operates directly on the document the user has open: every edit you \
    make appears on their canvas immediately and lands in the undo history, and there is no \
    session to create — the current document is implicit in every tool. Use get_state to see \
    the document, layers and selection; use render to look at the canvas. Prefer the editor's \
    own tools (cmd_*, tool_*, filter_*, adjust_*) over describing steps for the user to take. \
    Keep prose brief — you are in a narrow sidebar.";

/// One entry in the transcript.
pub struct AiEntry {
    pub kind: AiEntryKind,
    pub text: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum AiEntryKind {
    User,
    Assistant,
    Tool,
    Info,
    Error,
}

/// Everything the workspace holds for the sidebar.
pub struct AiState {
    pub backend: Backend,
    /// The prompt being typed.
    pub input: String,
    /// Whether the input box is capturing keys.
    pub input_active: bool,
    pub transcript: Vec<AiEntry>,
    /// A turn is in flight.
    pub running: bool,
    pub shared: AiShared,
    pub conversation: Option<Conversation>,
    /// The backend's id for this conversation, once it has said.
    pub session: Option<String>,
    /// The published tool list, built from the app's registry on first
    /// use. Also serves `tools/list` for the workers.
    pub catalog: Option<schist_mcp::Catalog>,
    /// Whether the drain ticker task is live (only ever one at a time).
    pub ticker: bool,
    /// Loopback endpoint for harnesses that spawn their MCP servers as
    /// processes (Codex); started on first use.
    pub endpoint: Option<endpoint::Endpoint>,
    pub scroll: gpui::ScrollHandle,
    /// (claude, codex) CLIs found on PATH, probed once at startup.
    pub available: (bool, bool),
}

impl AiState {
    pub fn new(backend: Backend) -> AiState {
        AiState {
            backend,
            input: String::new(),
            input_active: false,
            transcript: Vec::new(),
            running: false,
            shared: AiShared::default(),
            conversation: None,
            session: None,
            catalog: None,
            ticker: false,
            endpoint: None,
            scroll: gpui::ScrollHandle::new(),
            available: (Backend::Claude.available(), Backend::Codex.available()),
        }
    }
}
