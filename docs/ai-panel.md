# The AI panel

View ▸ AI Panel (`Cmd/Ctrl+Shift+A`) opens a sidebar that puts a coding
agent's harness behind the document you have open. You type a request;
the agent edits *this* document, live: every change appears on the
canvas as it lands, goes through the same plugin registry the menus use,
and arrives as ordinary entries in the History panel — one undo step per
edit, exactly as if you had done it yourself.

Schist talks to the agents through their own SDKs and ships no model
access of its own: there is no API key to paste and no new account. The
panel drives the CLI you already have installed and logged in.

| backend | CLI it drives | driven through |
|---|---|---|
| Claude Code | `claude` | the Claude Agent SDK (Rust port, `claude-agent-sdk-rs`) |
| Codex | `codex` | the Codex app-server protocol (`codex-codes`) |

If neither CLI is on `PATH` the panel says so instead of sending
anything.

A model picker sits under the header. "Default" is whatever the CLI's
own configuration chooses; the other entries are per-harness — the
Claude aliases (Opus, Sonnet, Haiku) resolve to the newest model of each
tier, and the Codex list is its SDK's bundled catalog. The choice is
remembered per harness and applies from the next message without
restarting the conversation: Claude Code switches its live session,
Codex takes it as a per-turn override.

## What the agent sees

The agent's hands are the same ~250 MCP tools `schist-mcp` publishes
(see [mcp.md](mcp.md)) with one difference: there are no sessions. The
document open in the window is implicit in every call, so the `session`
parameter and `create_session`/`list_sessions`/`close_session` do not
exist in this catalog (`schist_mcp::catalog::Scope::Active`). `get_state`
describes the open document; `render` returns the canvas the user is
looking at; switching tabs mid-conversation points the next call at the
newly active tab, because "the current document" is the whole contract.

Tool calls are serviced on the UI thread between frames, so the agent
can never race a brush stroke, and every mutating call repaints through
the same `after_change` path as any menu command.

## How each backend connects

**Claude Code** needs no plumbing at all: the Agent SDK supports
in-process MCP servers, so the CLI's tool calls arrive over its control
channel and are answered inside the app. The panel starts the CLI with
an isolated configuration — no filesystem settings, no CLAUDE.md, `HOME`
as its working directory — with only the `schist` server allowed.

**Codex** spawns MCP servers as child processes, so the app listens on
an ephemeral loopback TCP port and Codex is configured to spawn
`schist --mcp-bridge <addr>` — this same binary in a mode that just
pumps stdio into that socket. The port is guarded by a per-launch random
token (any local process can reach loopback; only children the app
configured know the token). Codex runs with a read-only sandbox and
approvals set to `never`; anything that would have needed an approval
dialog is declined.

The bridge mode is not specific to Codex: any stdio MCP client can be
pointed at a running app the same way, if it is handed the address and
`SCHIST_MCP_TOKEN` the app generated for that launch.

## Conversations

A conversation keeps its context until the trash button starts a new
one. The backend's own session survives even the panel closing: the id
(`claude --resume`'s session id, Codex's thread id) is kept and the next
prompt resumes it. Stop interrupts the turn in flight — gracefully over
the control channel for Claude Code; by ending the app-server process
for Codex, whose protocol read cannot be interrupted (the thread id
survives for the next resume).

The transcript streams: assistant text arrives token by token, and each
tool call is shown by name as it starts. Enter sends, Shift+Enter breaks
the line, Escape hands the keyboard back to the canvas.
