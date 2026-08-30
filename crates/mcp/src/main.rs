//! `schist-mcp` — a Model Context Protocol server over stdio.
//!
//! Create a session (a blank document or an opened file), then drive it by
//! session id. Every registered canvas tool, menu command, filter and
//! adjustment — the same plugin registry the GPUI app assembles — is
//! published as its own MCP tool with its own documented parameters (see
//! `schist_mcp::catalog`), alongside document introspection and PNG
//! rendering. JSON-RPC 2.0, newline-delimited, on stdin/stdout; logs go to
//! stderr so they never corrupt the stream.
//!
//! What this binary owns is the session map; everything a session can do
//! lives in `schist_mcp::dispatch`, shared with the app's in-window AI
//! panel host.

use anyhow::{anyhow, bail, Result};
use schist_core::color::Depth;
use schist_mcp::catalog::Action;
use schist_mcp::dispatch::{arg_str, text, text_json};
use schist_mcp::{dispatch, Catalog, Session};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, Write};

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn"))
        .target(env_logger::Target::Stderr)
        .init();
    let stdin = std::io::stdin();
    let mut server = Server::default();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }
        let message: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                respond(&json!({
                    "jsonrpc": "2.0", "id": null,
                    "error": {"code": -32700, "message": format!("parse error: {e}")}
                }));
                continue;
            }
        };
        let id = message.get("id").cloned();
        let Some(method) = message.get("method").and_then(|m| m.as_str()) else {
            continue;
        };
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        // Notifications get no reply.
        let Some(id) = id else {
            continue;
        };
        let reply = match server.handle(method, &params) {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(e) => json!({
                "jsonrpc": "2.0", "id": id,
                "error": {"code": e.0, "message": e.1}
            }),
        };
        respond(&reply);
    }
}

fn respond(value: &Value) {
    let mut out = std::io::stdout().lock();
    let _ = serde_json::to_writer(&mut out, value);
    let _ = out.write_all(b"\n");
    let _ = out.flush();
}

/// JSON-RPC error: (code, message).
struct RpcError(i64, String);

#[derive(Default)]
struct Server {
    sessions: HashMap<String, Session>,
    next_id: u64,
    /// Built on the first `tools/list` or `tools/call` rather than at
    /// startup: assembling it scans the plugin folders, and a client that
    /// only pings should not pay for that.
    catalog: Option<Catalog>,
}

impl Server {
    fn handle(&mut self, method: &str, params: &Value) -> Result<Value, RpcError> {
        match method {
            "initialize" => {
                let requested = params
                    .get("protocolVersion")
                    .and_then(|v| v.as_str())
                    .unwrap_or("2025-03-26");
                Ok(json!({
                    "protocolVersion": requested,
                    "capabilities": {"tools": {}},
                    "serverInfo": {
                        "name": "schist-mcp",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                    "instructions": "Headless Schist image editor. Call create_session first \
                        (blank document or open a file) and pass the returned session id to every \
                        other tool. Everything the editor can do is its own tool: cmd_* are the \
                        menu commands (cmd_edit_undo, cmd_edit_redo…), tool_* select a canvas \
                        tool and set its options before you drive it with tool_stroke and \
                        tool_input, filter_* run filters, adjust_* apply adjustments. get_state \
                        gives the document and layer tree; render returns the canvas as a PNG. \
                        Edits go through the same plugin registry and undo history as the GUI.",
                }))
            }
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": self.catalog().defs()})),
            "tools/call" => {
                let name = params
                    .get("name")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| RpcError(-32602, "missing tool name".into()))?;
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                match self.call_tool(name, &args) {
                    Ok(content) => Ok(json!({"content": content})),
                    Err(e) => Ok(json!({
                        "content": [{"type": "text", "text": format!("{e:#}")}],
                        "isError": true,
                    })),
                }
            }
            _ => Err(RpcError(-32601, format!("method not found: {method}"))),
        }
    }

    /// Everything the server publishes is a name in the catalog. Session
    /// management is handled here; everything else resolves its session
    /// and goes through the shared dispatch.
    fn call_tool(&mut self, name: &str, args: &Value) -> Result<Value> {
        let action = self
            .catalog()
            .action(name)
            .cloned()
            .ok_or_else(|| anyhow!("unknown tool {name:?}"))?;
        match &action {
            Action::Builtin(b) if b == "create_session" => self.create_session(args),
            Action::Builtin(b) if b == "list_sessions" => self.list_sessions(),
            Action::Builtin(b) if b == "close_session" => {
                let id = arg_str(args, "session")?;
                self.sessions
                    .remove(id)
                    .ok_or_else(|| anyhow!("no session {id:?}"))?;
                text(format!("closed {id}"))
            }
            _ => {
                let id = arg_str(args, "session")?.to_string();
                let sess = self
                    .sessions
                    .get_mut(&id)
                    .ok_or_else(|| anyhow!("no session {id:?} — create_session first"))?;
                let out = dispatch::call_action(&mut sess.ctx(), &action, args, Some(&id));
                // There is no display cache here; drop whatever damage the
                // call queued rather than letting it pile up.
                sess.doc.take_damage();
                out
            }
        }
    }

    fn catalog(&mut self) -> &Catalog {
        self.catalog.get_or_insert_with(Catalog::build)
    }

    fn create_session(&mut self, args: &Value) -> Result<Value> {
        let session = match args.get("path").and_then(|v| v.as_str()) {
            Some(path) => Session::open(std::path::Path::new(path))?,
            None => {
                let width = args.get("width").and_then(|v| v.as_u64()).unwrap_or(1280) as u32;
                let height = args.get("height").and_then(|v| v.as_u64()).unwrap_or(800) as u32;
                let depth = match args.get("depth").and_then(|v| v.as_u64()).unwrap_or(8) {
                    8 => Depth::Eight,
                    16 => Depth::Sixteen,
                    32 => Depth::ThirtyTwo,
                    other => bail!("depth must be 8, 16 or 32, not {other}"),
                };
                let title = args
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Untitled");
                Session::new_blank(title, width, height, depth)?
            }
        };
        self.next_id += 1;
        let id = format!("s{}", self.next_id);
        self.sessions.insert(id.clone(), session);
        let sess = self.sessions.get_mut(&id).unwrap();
        text_json(dispatch::state_json(Some(&id), &sess.ctx()))
    }

    fn list_sessions(&mut self) -> Result<Value> {
        let mut sessions: Vec<Value> = self
            .sessions
            .iter()
            .map(|(id, s)| {
                json!({
                    "session": id,
                    "title": s.doc.title,
                    "path": s.doc.path.as_ref().map(|p| p.display().to_string()),
                    "size": [s.doc.width, s.doc.height],
                    "dirty": s.doc.dirty,
                })
            })
            .collect();
        sessions.sort_by(|a, b| a["session"].as_str().cmp(&b["session"].as_str()));
        text_json(json!({"sessions": sessions}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every name in the tool list has to lead somewhere. A builtin that
    /// is published but not implemented reports "unknown tool" to the
    /// caller and nothing at all to us.
    #[test]
    fn every_published_name_dispatches() {
        let mut server = Server::default();
        let names: Vec<String> = server
            .catalog()
            .defs()
            .iter()
            .map(|d| d["name"].as_str().unwrap().to_string())
            .collect();
        assert!(names.len() > 100, "only {} tools published", names.len());
        for name in names {
            if let Err(e) = server.call_tool(&name, &json!({"session": "s0"})) {
                let e = format!("{e:#}");
                assert!(!e.contains("unknown tool"), "{name}: {e}");
            }
        }
    }

    /// A filter's choice parameter reads as the name the dialog shows.
    #[test]
    fn a_choice_reaches_the_filter_as_its_index() {
        let mut server = Server::default();
        server
            .call_tool("create_session", &json!({"width": 32, "height": 32}))
            .expect("session");
        let args = json!({"session": "s1", "amount": 10, "distribution": "Gaussian"});
        server.call_tool("filter_add_noise", &args).expect("filter");
        let bad = json!({"session": "s1", "distribution": "Poisson"});
        let e = format!(
            "{:#}",
            server.call_tool("filter_add_noise", &bad).unwrap_err()
        );
        assert!(e.contains("no choice"), "{e}");
    }

    /// Selecting a tool and setting its options is one call, and the
    /// options that arrive with it are checked against that tool.
    #[test]
    fn a_tool_is_selected_and_configured_together() {
        let mut server = Server::default();
        server
            .call_tool("create_session", &json!({"width": 32, "height": 32}))
            .expect("session");
        let args = json!({"session": "s1", "marquee-feather": 3.0});
        server
            .call_tool("tool_marquee_rect", &args)
            .expect("select");
        let sess = server.sessions.get("s1").unwrap();
        assert_eq!(sess.state.active_tool, "marquee.rect");
        let feather = sess
            .registry
            .tools()
            .find(|t| t.id() == "marquee.rect")
            .unwrap()
            .options()
            .iter()
            .find(|o| o.key == "marquee-feather")
            .map(|o| o.value.num());
        assert_eq!(feather, Some(3.0));
        let e = format!(
            "{:#}",
            server
                .call_tool("tool_marquee_rect", &json!({"session": "s1", "nope": 1}))
                .unwrap_err()
        );
        assert!(e.contains("no option"), "{e}");
    }

    /// An adjustment takes its checkbox as a boolean and its sliders as
    /// numbers, and the two do not undo each other.
    #[test]
    fn an_adjustment_takes_flags_and_sliders_at_once() {
        let mut server = Server::default();
        server
            .call_tool("create_session", &json!({"width": 32, "height": 32}))
            .expect("session");
        let args = json!({"session": "s1", "monochrome": true, "r_r": 50.0});
        server
            .call_tool("adjust_channel_mixer", &args)
            .expect("adjustment");
        let e = format!(
            "{:#}",
            server
                .call_tool("adjust_levels", &json!({"session": "s1", "bogus": 1.0}))
                .unwrap_err()
        );
        assert!(e.contains("no parameter"), "{e}");
    }
}
