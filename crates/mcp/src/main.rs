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

mod gallery {
    //! The gallery tools of the stdio server, over [`schist_gallery::headless::Gallery`].
    use anyhow::{anyhow, bail, Result};
    use schist_gallery::headless::Gallery;
    use serde_json::{json, Value};
    use std::path::PathBuf;

    fn def(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
        json!({"name": name, "description": description,
               "inputSchema": {"type": "object", "properties": properties, "required": required}})
    }

    pub fn tool_defs() -> Vec<Value> {
        let paths = json!({"type": "array", "items": {"type": "string"}, "description": "Photo paths, as other gallery tools return them."});
        vec![
            def("gallery_state",
                "Describe the photo gallery from its files on disk: watched folders, photo count, \
                 buckets (with their rules), how much of the library the app has indexed, and the \
                 content filter's counts. Selection and grouping belong to the app's window and are \
                 not visible here.",
                json!({}), &[]),
            def("gallery_list",
                "List photos — the whole library, one watched folder (by path prefix), or one \
                 bucket (by name). Each row: path, when taken, place, whether it has an edit, and \
                 the content filter's verdict (null while unscored).",
                json!({"folder": {"type": "string"}, "bucket": {"type": "string"},
                       "offset": {"type": "integer", "minimum": 0, "default": 0},
                       "limit": {"type": "integer", "minimum": 1, "maximum": 500, "default": 50}}),
                &[]),
            def("gallery_search",
                "Search photos by what is in them (\"dog on a beach\"), by where they were taken \
                 (\"taken in nyc\"), or both, ranked best first, over the embeddings the app has \
                 indexed. With a bucket, the bucket filters first and the query ranks its \
                 photos. Content search needs the Search models installed; places work from \
                 EXIF alone.",
                json!({"query": {"type": "string"},
                       "bucket": {"type": "string",
                                  "description": "Search within this bucket (by name) only."},
                       "limit": {"type": "integer", "minimum": 1, "maximum": 200, "default": 20}}),
                &["query"]),
            def("gallery_thumbnail",
                "Look at one photo: its cached gallery thumbnail (up to 256 px) as an image, when \
                 the app has rendered one.",
                json!({"path": {"type": "string"}}), &["path"]),
            def("gallery_flagged",
                "List photos by the content filter's verdict — flagged, clean, or unscored.",
                json!({"verdict": {"type": "string", "enum": ["flagged", "clean", "unscored"], "default": "flagged"},
                       "offset": {"type": "integer", "minimum": 0, "default": 0},
                       "limit": {"type": "integer", "minimum": 1, "maximum": 500, "default": 50}}),
                &[]),
            def("gallery_bucket_create",
                "Create a bucket in the library file, optionally with a query the app keeps \
                 matching once it runs. The app reads the file at launch.",
                json!({"name": {"type": "string"}, "query": {"type": "string"}, "paths": paths}),
                &["name"]),
            def("gallery_people",
                "The People album: every named person with their photo and face counts, and how \
                 many detected faces are still unnamed. Given a name, lists that person's photos \
                 instead. Names are given in the app by clicking a face in a photo.",
                json!({"person": {"type": "string", "description": "A person's name, for their photos."},
                       "offset": {"type": "integer", "minimum": 0, "default": 0},
                       "limit": {"type": "integer", "minimum": 1, "maximum": 500, "default": 50}}),
                &[]),
            def("gallery_bucket_add",
                "Add photos to a bucket by name, in the library file.",
                json!({"bucket": {"type": "string"}, "paths": paths}), &["bucket", "paths"]),
            def("gallery_open",
                "Open a photo as a new editing session — its edit sidecar when it has one — and \
                 return the session's state; the session tools then apply.",
                json!({"path": {"type": "string"}}), &["path"]),
        ]
    }

    fn text(value: Value) -> Result<Value> {
        Ok(json!([{"type": "text", "text": value.to_string()}]))
    }

    pub fn str_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
        args.get(key)
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| anyhow!("{key} is required"))
    }

    fn paths_arg(args: &Value, key: &str) -> Vec<PathBuf> {
        args.get(key)
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .map(PathBuf::from)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn page(args: &Value) -> (usize, usize) {
        let offset = args.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(50)
            .clamp(1, 500) as usize;
        (offset, limit)
    }

    fn base64(bytes: &[u8]) -> String {
        const TABLE: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
        for chunk in bytes.chunks(3) {
            let b = [
                chunk[0],
                *chunk.get(1).unwrap_or(&0),
                *chunk.get(2).unwrap_or(&0),
            ];
            let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
            out.push(TABLE[(n >> 18) as usize & 63] as char);
            out.push(TABLE[(n >> 12) as usize & 63] as char);
            out.push(if chunk.len() > 1 {
                TABLE[(n >> 6) as usize & 63] as char
            } else {
                '='
            });
            out.push(if chunk.len() > 2 {
                TABLE[n as usize & 63] as char
            } else {
                '='
            });
        }
        out
    }

    pub fn call(gallery: &mut Gallery, name: &str, args: &Value) -> Result<Value> {
        match name {
            "gallery_state" => text(gallery.state_json()),
            "gallery_list" => {
                let (offset, limit) = page(args);
                let folder = args.get("folder").and_then(|v| v.as_str());
                let bucket = args.get("bucket").and_then(|v| v.as_str());
                let entries = gallery.list(folder, bucket);
                let rows: Vec<Value> = entries
                    .iter()
                    .skip(offset)
                    .take(limit)
                    .map(|e| gallery.entry_json(e))
                    .collect();
                text(json!({"total": entries.len(), "offset": offset, "photos": rows}))
            }
            "gallery_search" => {
                let query = str_arg(args, "query")?;
                let limit = args
                    .get("limit")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(20)
                    .clamp(1, 200) as usize;
                let bucket = args.get("bucket").and_then(|v| v.as_str());
                let (ranked, place) = gallery.search(query, bucket)?;
                let rows: Vec<Value> = ranked
                    .iter()
                    .take(limit)
                    .filter_map(|(p, score)| {
                        let mut row = gallery.entry_json(gallery.entry(p)?);
                        row["score"] = json!((score * 1000.0).round() / 1000.0);
                        Some(row)
                    })
                    .collect();
                text(json!({
                    "query": query,
                    "bucket": bucket,
                    "place": place,
                    "matches": ranked.len(),
                    "photos": rows,
                }))
            }
            "gallery_thumbnail" => {
                let path = PathBuf::from(str_arg(args, "path")?);
                let png = gallery.thumbnail_png(&path).ok_or_else(|| {
                    anyhow!(
                        "no thumbnail for {} — the app renders them as photos come on screen",
                        path.display()
                    )
                })?;
                Ok(json!([{"type": "image", "data": base64(&png), "mimeType": "image/png"}]))
            }
            "gallery_flagged" => {
                let verdict = args
                    .get("verdict")
                    .and_then(|v| v.as_str())
                    .unwrap_or("flagged");
                if !["flagged", "clean", "unscored"].contains(&verdict) {
                    bail!("verdict must be flagged, clean or unscored");
                }
                let (offset, limit) = page(args);
                let entries: Vec<_> = gallery
                    .entries()
                    .filter(|e| gallery.verdict(&e.path) == verdict)
                    .cloned()
                    .collect();
                let rows: Vec<Value> = entries
                    .iter()
                    .skip(offset)
                    .take(limit)
                    .map(|e| gallery.entry_json(e))
                    .collect();
                text(
                    json!({"verdict": verdict, "total": entries.len(), "offset": offset, "photos": rows}),
                )
            }
            "gallery_bucket_create" => {
                let name = str_arg(args, "name")?;
                let query = args
                    .get("query")
                    .and_then(|v| v.as_str())
                    .filter(|q| !q.trim().is_empty());
                let index = gallery.create_bucket(name, query, paths_arg(args, "paths"))?;
                text(json!({"bucket": name, "index": index}))
            }
            "gallery_bucket_add" => {
                let name = str_arg(args, "bucket")?;
                let count = gallery.add_to_bucket(name, &paths_arg(args, "paths"))?;
                text(json!({"bucket": name, "photos": count}))
            }
            "gallery_people" => match args.get("person").and_then(|v| v.as_str()) {
                Some(person) => {
                    let (offset, limit) = page(args);
                    let entries = gallery
                        .person_photos(person)
                        .ok_or_else(|| anyhow!("nobody named {person:?}"))?;
                    let rows: Vec<Value> = entries
                        .iter()
                        .skip(offset)
                        .take(limit)
                        .map(|e| gallery.entry_json(e))
                        .collect();
                    text(
                        json!({"person": person, "total": entries.len(), "offset": offset, "photos": rows}),
                    )
                }
                None => text(gallery.people_json()),
            },
            other => bail!("unknown gallery tool {other:?}"),
        }
    }
}

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
            "tools/list" => {
                let mut tools: Vec<Value> = self.catalog().defs().to_vec();
                tools.extend(gallery::tool_defs());
                Ok(json!({"tools": tools}))
            }
            "tools/call" => {
                let name = params
                    .get("name")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| RpcError(-32602, "missing tool name".into()))?;
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                // The gallery's tools are not in the registry's catalog
                // (the gallery is not a plugin); they dispatch by prefix.
                let result = if name.starts_with("gallery_") {
                    self.gallery_tool(name, &args)
                } else {
                    self.call_tool(name, &args)
                };
                match result {
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

    /// One `gallery_*` tool, over the gallery's files on disk.
    fn gallery_tool(&mut self, name: &str, args: &Value) -> Result<Value> {
        let exts: Vec<String> = {
            let (registry, _wasm, _photoshop) = schist_mcp::session::build_registry();
            registry
                .codecs()
                .flat_map(|c| c.extensions().iter().map(|e| e.to_string()))
                .collect()
        };
        let mut gallery = schist_gallery::headless::Gallery::open(&exts);
        if name == "gallery_open" {
            // Open in a new session: the edit sidecar when there is one,
            // the way the app's double-click does.
            let path = std::path::PathBuf::from(gallery::str_arg(args, "path")?);
            let source = schist_gallery::backing_psd(&path)
                .filter(|p| p.exists())
                .unwrap_or(path);
            return self.create_session(&json!({"path": source.display().to_string()}));
        }
        gallery::call(&mut gallery, name, args)
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
