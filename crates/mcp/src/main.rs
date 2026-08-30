//! `schist-mcp` — a Model Context Protocol server over stdio.
//!
//! Create a session (a blank document or an opened file), then drive it by
//! session id. Every registered canvas tool, menu command, filter and
//! adjustment — the same plugin registry the GPUI app assembles — is
//! published as its own MCP tool with its own documented parameters (see
//! `catalog`), alongside document introspection and PNG rendering.
//! JSON-RPC 2.0, newline-delimited, on stdin/stdout; logs go to stderr so
//! they never corrupt the stream.

mod catalog;
mod session;

use anyhow::{anyhow, bail, Result};
use base64::Engine as _;
use catalog::{Action, Catalog};
use schist_core::color::{Depth, Rgba};
use schist_core::{AdjustmentKind, BlendMode, IntRect, Layer, LayerId, LayerKind};
use schist_plugin_api::{ExportOptions, Modifiers, OptionKind, OptionValue, ToolOption};
use serde_json::{json, Value};
use session::Session;
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

    /// Everything the server publishes is a name in the catalog, so this
    /// is the only place a name turns back into work.
    fn call_tool(&mut self, name: &str, args: &Value) -> Result<Value> {
        let action = self
            .catalog()
            .action(name)
            .cloned()
            .ok_or_else(|| anyhow!("unknown tool {name:?}"))?;
        match action {
            Action::Builtin(name) => self.call_builtin(&name, args),
            Action::Tool(id) => self.select_tool(&id, args),
            Action::Command(id) => {
                let sess = self.session(args)?;
                let title = sess.run_command(&id)?;
                text(title)
            }
            Action::Filter(id) => self.apply_filter(&id, args),
            Action::Adjustment(kind) => self.apply_adjustment(kind, args),
        }
    }

    fn catalog(&mut self) -> &Catalog {
        self.catalog.get_or_insert_with(Catalog::build)
    }

    /// The server's own tools: the ones that are about sessions, state and
    /// files rather than about something in the registry.
    fn call_builtin(&mut self, name: &str, args: &Value) -> Result<Value> {
        match name {
            "create_session" => self.create_session(args),
            "list_sessions" => self.list_sessions(),
            "close_session" => {
                let id = arg_str(args, "session")?;
                self.sessions
                    .remove(id)
                    .ok_or_else(|| anyhow!("no session {id:?}"))?;
                text(format!("closed {id}"))
            }
            "get_state" => {
                let id = arg_str(args, "session")?.to_string();
                let sess = self.session(args)?;
                text_json(state_json(&id, sess))
            }
            "tool_stroke" => self.tool_stroke(args),
            "tool_input" => {
                let modifiers = parse_modifiers(args.get("modifiers"));
                let sess = self.session(args)?;
                let consumed = sess.tool_input(
                    arg_str(args, "action")?,
                    args.get("key").and_then(|v| v.as_str()),
                    args.get("text").and_then(|v| v.as_str()),
                    modifiers,
                )?;
                text(if consumed { "consumed" } else { "not consumed" }.to_string())
            }
            "set_active_layer" => {
                let id = args
                    .get("id")
                    .and_then(|v| v.as_u64())
                    .ok_or_else(|| anyhow!("missing layer id"))?;
                let sess = self.session(args)?;
                let id = LayerId(id);
                let name = sess
                    .doc
                    .tree
                    .find(id)
                    .map(|l| l.name.clone())
                    .ok_or_else(|| anyhow!("no layer {}", id.0))?;
                sess.doc.active_layer = Some(id);
                sess.doc.selected = vec![id];
                text(format!("active layer: {name}"))
            }
            "set_layer_props" => self.set_layer_props(args),
            "set_editor" => self.set_editor(args),
            "render" => self.render(args),
            "save" => {
                let path = args.get("path").and_then(|v| v.as_str()).map(String::from);
                let sess = self.session(args)?;
                let path = path
                    .map(std::path::PathBuf::from)
                    .or_else(|| sess.doc.path.clone())
                    .ok_or_else(|| anyhow!("document has no path; pass one"))?;
                sess.save(&path)?;
                text(format!("saved {}", path.display()))
            }
            "export" => {
                let path = std::path::PathBuf::from(arg_str(args, "path")?);
                let options = ExportOptions {
                    quality: args
                        .get("quality")
                        .and_then(|v| v.as_u64())
                        .map(|q| q.clamp(1, 100) as u8)
                        .unwrap_or(ExportOptions::default().quality),
                    bit_depth: args
                        .get("bit_depth")
                        .and_then(|v| v.as_u64())
                        .map(|b| b as u8)
                        .unwrap_or(ExportOptions::default().bit_depth),
                    dither: args
                        .get("dither")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(ExportOptions::default().dither),
                };
                let sess = self.session(args)?;
                sess.export(&path, &options)?;
                text(format!("exported {}", path.display()))
            }
            "photoshop_plugins" => {
                let sess = self.session(args)?;
                text_json(photoshop_json(sess))
            }
            other => bail!("unknown tool {other:?}"),
        }
    }

    /// Make a canvas tool active and apply the options passed with it.
    ///
    /// Options are read fresh between writes because a tool's options bar
    /// can change shape as it is set: the move tool only offers its
    /// auto-select target once auto-select is on.
    fn select_tool(&mut self, id: &str, args: &Value) -> Result<Value> {
        let sess = self.session(args)?;
        let id = sess.activate_tool(id)?;
        for (key, value) in catalog::parameters(args) {
            let declared = sess
                .registry
                .tools()
                .find(|t| t.id() == id)
                .map(|t| t.options())
                .unwrap_or_default();
            let kind = declared
                .iter()
                .find(|o| o.key == key)
                .map(|o| o.kind)
                .ok_or_else(|| {
                    anyhow!(
                        "tool {id:?} has no option {key:?} (has: {:?})",
                        declared.iter().map(|o| o.key).collect::<Vec<_>>()
                    )
                })?;
            sess.set_tool_option(&key, coerce_option(&value, kind)?)?;
        }
        let options: Vec<Value> = sess
            .registry
            .tools()
            .find(|t| t.id() == id)
            .map(|t| t.options().iter().map(option_json).collect())
            .unwrap_or_default();
        text_json(json!({"active_tool": id, "options": options}))
    }

    fn apply_filter(&mut self, id: &str, args: &Value) -> Result<Value> {
        let sess = self.session(args)?;
        let params = sess
            .registry
            .filters()
            .find(|f| f.id() == id)
            .map(|f| f.params())
            .ok_or_else(|| anyhow!("unknown filter {id:?}"))?;
        let mut values: Vec<(String, f64)> = Vec::new();
        for (key, value) in catalog::parameters(args) {
            let param = params.iter().find(|p| p.key == key).ok_or_else(|| {
                anyhow!(
                    "filter {id:?} has no parameter {key:?} (has: {:?})",
                    params.iter().map(|p| p.key).collect::<Vec<_>>()
                )
            })?;
            // A choice reads as its name here, the way the dialog shows
            // it, but the filter only ever sees the index.
            let number = match &value {
                Value::String(name) if !param.choices.is_empty() => param
                    .choices
                    .iter()
                    .position(|c| c.eq_ignore_ascii_case(name))
                    .map(|i| i as f64)
                    .ok_or_else(|| {
                        anyhow!(
                            "no choice {name:?} for {key:?} (choices: {:?})",
                            param.choices
                        )
                    })?,
                other => other
                    .as_f64()
                    .ok_or_else(|| anyhow!("parameter {key:?} must be a number"))?,
            };
            values.push((key, number));
        }
        let name = sess.apply_filter(id, &values)?;
        text(format!("applied {name}"))
    }

    fn apply_adjustment(&mut self, kind: AdjustmentKind, args: &Value) -> Result<Value> {
        let mut params = match args.get("params") {
            Some(v) if !v.is_null() => {
                serde_json::from_value::<schist_adjustments::Params>(v.clone())
                    .map_err(|e| anyhow!("bad adjustment params: {e}"))?
            }
            _ => schist_adjustments::Params::default_for(kind),
        };
        let mut arguments = catalog::parameters(args);
        arguments.remove("params");
        // Flags first: setting one round-trips through serde, which would
        // undo slider values written before it.
        for (key, value) in &arguments {
            let Some(flag) = value.as_bool() else {
                continue;
            };
            params = catalog::set_flag(&params, key, flag).ok_or_else(|| {
                anyhow!(
                    "{} has no flag {key:?} (has: {:?})",
                    kind.display_name(),
                    catalog::flags(&params)
                        .iter()
                        .map(|(k, _)| k.clone())
                        .collect::<Vec<_>>()
                )
            })?;
        }
        let specs = params.param_specs();
        for (key, value) in &arguments {
            if value.is_boolean() {
                continue;
            }
            let number = value
                .as_f64()
                .ok_or_else(|| anyhow!("parameter {key:?} must be a number"))?;
            if !specs.iter().any(|s| s.key == key) {
                bail!(
                    "{} has no parameter {key:?} (has: {:?})",
                    kind.display_name(),
                    specs.iter().map(|s| s.key).collect::<Vec<_>>()
                );
            }
            params.set_param(key, number as f32);
        }
        let sess = self.session(args)?;
        let name = sess.apply_adjustment(kind, Some(params))?;
        text(format!("applied {name}"))
    }

    fn session(&mut self, args: &Value) -> Result<&mut Session> {
        let id = arg_str(args, "session")?;
        self.sessions
            .get_mut(id)
            .ok_or_else(|| anyhow!("no session {id:?} — create_session first"))
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
        text_json(state_json(&id, sess))
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

    fn tool_stroke(&mut self, args: &Value) -> Result<Value> {
        let points: Vec<(f32, f32)> = args
            .get("points")
            .and_then(|v| v.as_array())
            .ok_or_else(|| anyhow!("missing points array"))?
            .iter()
            .map(|p| {
                let pair = p.as_array().filter(|a| a.len() == 2).ok_or_else(|| {
                    anyhow!("each point must be an [x, y] pair in document pixels")
                })?;
                Ok((
                    pair[0].as_f64().unwrap_or(0.0) as f32,
                    pair[1].as_f64().unwrap_or(0.0) as f32,
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        let pressure = args
            .get("pressure")
            .and_then(|v| v.as_f64())
            .unwrap_or(1.0)
            .clamp(0.0, 1.0) as f32;
        let modifiers = parse_modifiers(args.get("modifiers"));
        let sess = self.session(args)?;
        let tool = sess.state.active_tool;
        sess.stroke(&points, pressure, modifiers)?;
        text(format!("{tool}: stroke of {} point(s)", points.len()))
    }

    fn set_layer_props(&mut self, args: &Value) -> Result<Value> {
        let id = args
            .get("id")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| anyhow!("missing layer id"))?;
        let name = args.get("name").and_then(|v| v.as_str()).map(String::from);
        let visible = args.get("visible").and_then(|v| v.as_bool());
        let locked = args.get("locked").and_then(|v| v.as_bool());
        let clipping = args.get("clipping").and_then(|v| v.as_bool());
        let opacity = args
            .get("opacity")
            .and_then(|v| v.as_f64())
            .map(|v| v.clamp(0.0, 1.0) as f32);
        let fill_opacity = args
            .get("fill_opacity")
            .and_then(|v| v.as_f64())
            .map(|v| v.clamp(0.0, 1.0) as f32);
        let blend = args
            .get("blend")
            .and_then(|v| v.as_str())
            .map(parse_blend_mode)
            .transpose()?;
        let sess = self.session(args)?;
        let id = LayerId(id);
        if sess.doc.tree.find(id).is_none() {
            bail!("no layer {}", id.0);
        }
        let mut edit = sess.doc.begin_edit("Layer Properties");
        edit.change_props(id, |layer| {
            if let Some(name) = name {
                layer.name = name;
            }
            if let Some(v) = visible {
                layer.visible = v;
            }
            if let Some(v) = locked {
                layer.locked = v;
            }
            if let Some(v) = clipping {
                layer.clipping = v;
            }
            if let Some(v) = opacity {
                layer.opacity = v;
            }
            if let Some(v) = fill_opacity {
                layer.fill_opacity = v;
            }
            if let Some(v) = blend {
                layer.blend = v;
            }
        });
        edit.commit();
        sess.after_change();
        text(format!("updated layer {}", id.0))
    }

    fn set_editor(&mut self, args: &Value) -> Result<Value> {
        let foreground = args
            .get("foreground")
            .and_then(|v| v.as_str())
            .map(parse_color)
            .transpose()?;
        let background = args
            .get("background")
            .and_then(|v| v.as_str())
            .map(parse_color)
            .transpose()?;
        let resample = args
            .get("resample")
            .and_then(|v| v.as_str())
            .map(|s| match s.to_ascii_lowercase().as_str() {
                "nearest" => Ok(schist_core::Filter::Nearest),
                "bilinear" => Ok(schist_core::Filter::Bilinear),
                "bicubic" => Ok(schist_core::Filter::Bicubic),
                other => Err(anyhow!(
                    "resample must be nearest, bilinear or bicubic, not {other:?}"
                )),
            })
            .transpose()?;
        let sess = self.session(args)?;
        if let Some(c) = foreground {
            sess.state.foreground = c;
        }
        if let Some(c) = background {
            sess.state.background = c;
        }
        if let Some(v) = args.get("brush_size").and_then(|v| v.as_f64()) {
            sess.state.brush_size = (v as f32).max(1.0);
        }
        if let Some(v) = args.get("brush_hardness").and_then(|v| v.as_f64()) {
            sess.state.brush_hardness = (v as f32).clamp(0.0, 1.0);
        }
        if let Some(v) = args.get("tool_opacity").and_then(|v| v.as_f64()) {
            sess.state.tool_opacity = (v as f32).clamp(0.0, 1.0);
        }
        if let Some(v) = args.get("tolerance").and_then(|v| v.as_u64()) {
            sess.state.tolerance = v.min(255) as u8;
        }
        if let Some(f) = resample {
            sess.state.resample = f;
        }
        text("editor state updated".to_string())
    }

    fn render(&mut self, args: &Value) -> Result<Value> {
        let region = match (
            args.get("x").and_then(|v| v.as_i64()),
            args.get("y").and_then(|v| v.as_i64()),
            args.get("width").and_then(|v| v.as_u64()),
            args.get("height").and_then(|v| v.as_u64()),
        ) {
            (Some(x), Some(y), Some(w), Some(h)) => {
                Some(IntRect::from_xywh(x as i32, y as i32, w as u32, h as u32))
            }
            (None, None, None, None) => None,
            _ => bail!("pass all of x, y, width, height for a region, or none for the canvas"),
        };
        let max_dim = args
            .get("max_dim")
            .and_then(|v| v.as_u64())
            .unwrap_or(1024)
            .clamp(16, 8192) as u32;
        let out_path = args.get("path").and_then(|v| v.as_str()).map(String::from);
        let sess = self.session(args)?;
        let (region, pixels) = sess.render(region)?;
        let (w, h) = (region.width() as u32, region.height() as u32);
        let img = image::RgbaImage::from_raw(w, h, pixels)
            .ok_or_else(|| anyhow!("composited buffer had the wrong size"))?;
        let img = image::DynamicImage::ImageRgba8(img);
        if let Some(path) = &out_path {
            img.save_with_format(path, image::ImageFormat::Png)?;
        }
        let shown = if w.max(h) > max_dim {
            img.thumbnail(max_dim, max_dim)
        } else {
            img
        };
        let mut png = Vec::new();
        shown.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)?;
        let note =
            match &out_path {
                Some(path) => format!(
                "rendered {w}x{h} at ({}, {}); full resolution written to {path}, preview {}x{}",
                region.left, region.top, shown.width(), shown.height()
            ),
                None => format!(
                    "rendered {w}x{h} at ({}, {}), shown at {}x{}",
                    region.left,
                    region.top,
                    shown.width(),
                    shown.height()
                ),
            };
        Ok(json!([
            {
                "type": "image",
                "data": base64::engine::general_purpose::STANDARD.encode(&png),
                "mimeType": "image/png",
            },
            {"type": "text", "text": note},
        ]))
    }
}

// ----- argument helpers -----

fn arg_str<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("missing required argument {key:?}"))
}

fn text(s: String) -> Result<Value> {
    Ok(json!([{"type": "text", "text": s}]))
}

fn text_json(v: Value) -> Result<Value> {
    text(serde_json::to_string_pretty(&v)?)
}

fn parse_modifiers(v: Option<&Value>) -> Modifiers {
    let get = |key: &str| {
        v.and_then(|m| m.get(key))
            .and_then(|b| b.as_bool())
            .unwrap_or(false)
    };
    Modifiers {
        shift: get("shift"),
        alt: get("alt"),
        ctrl_or_cmd: get("ctrl") || get("cmd") || get("ctrl_or_cmd"),
    }
}

fn coerce_option(value: &Value, kind: OptionKind) -> Result<OptionValue> {
    match (value, kind) {
        (Value::Bool(b), _) => Ok(OptionValue::Bool(*b)),
        (Value::Number(n), OptionKind::Choice(_)) => {
            Ok(OptionValue::Choice(n.as_f64().unwrap_or(0.0) as usize))
        }
        (Value::Number(n), _) => Ok(OptionValue::Num(n.as_f64().unwrap_or(0.0) as f32)),
        (Value::String(s), OptionKind::Choice(names)) => names
            .iter()
            .position(|n| n.eq_ignore_ascii_case(s))
            .map(OptionValue::Choice)
            .ok_or_else(|| anyhow!("no choice {s:?} (choices: {names:?})")),
        (Value::String(s), _) => s
            .parse::<f32>()
            .map(OptionValue::Num)
            .map_err(|_| anyhow!("option value {s:?} is not a number")),
        (other, _) => bail!("unsupported option value {other}"),
    }
}

fn parse_color(s: &str) -> Result<Rgba> {
    let hex = s.trim().trim_start_matches('#');
    // The slices below are byte ranges and `len()` is a byte count, so a
    // multi-byte char would both pick the wrong arm and split a codepoint,
    // which panics. Reject anything that is not ASCII hex up front.
    if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("colour must be hex digits, not {s:?}");
    }
    let channel = |at: usize, width: usize| -> Result<f32> {
        let raw = u8::from_str_radix(&hex[at * width..(at + 1) * width], 16)
            .map_err(|_| anyhow!("bad hex colour {s:?}"))?;
        let raw = if width == 1 { raw * 17 } else { raw };
        Ok(raw as f32 / 255.0)
    };
    match hex.len() {
        3 => Ok(Rgba::new(
            channel(0, 1)?,
            channel(1, 1)?,
            channel(2, 1)?,
            1.0,
        )),
        6 => Ok(Rgba::new(
            channel(0, 2)?,
            channel(1, 2)?,
            channel(2, 2)?,
            1.0,
        )),
        8 => Ok(Rgba::new(
            channel(0, 2)?,
            channel(1, 2)?,
            channel(2, 2)?,
            channel(3, 2)?,
        )),
        _ => bail!("colour must be #rgb, #rrggbb or #rrggbbaa, not {s:?}"),
    }
}

fn color_hex(c: Rgba) -> String {
    let byte = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!(
        "#{:02x}{:02x}{:02x}{:02x}",
        byte(c.r),
        byte(c.g),
        byte(c.b),
        byte(c.a)
    )
}

/// Names accepted case-insensitively with spaces, slashes and dashes
/// ignored, so "Hue/Saturation", "hue-saturation" and "huesaturation" all
/// land on the same kind.
fn normalize(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase()
}

const BLEND_MODES: [BlendMode; 28] = [
    BlendMode::PassThrough,
    BlendMode::Normal,
    BlendMode::Dissolve,
    BlendMode::Darken,
    BlendMode::Multiply,
    BlendMode::ColorBurn,
    BlendMode::LinearBurn,
    BlendMode::DarkerColor,
    BlendMode::Lighten,
    BlendMode::Screen,
    BlendMode::ColorDodge,
    BlendMode::LinearDodge,
    BlendMode::LighterColor,
    BlendMode::Overlay,
    BlendMode::SoftLight,
    BlendMode::HardLight,
    BlendMode::VividLight,
    BlendMode::LinearLight,
    BlendMode::PinLight,
    BlendMode::HardMix,
    BlendMode::Difference,
    BlendMode::Exclusion,
    BlendMode::Subtract,
    BlendMode::Divide,
    BlendMode::Hue,
    BlendMode::Saturation,
    BlendMode::Color,
    BlendMode::Luminosity,
];

fn parse_blend_mode(name: &str) -> Result<BlendMode> {
    let wanted = normalize(name);
    BLEND_MODES
        .iter()
        .find(|m| normalize(&format!("{m:?}")) == wanted)
        .copied()
        .ok_or_else(|| anyhow!("unknown blend mode {name:?}"))
}

// ----- state and capability reporting -----

fn rect_json(r: IntRect) -> Value {
    json!({"x": r.left, "y": r.top, "width": r.width(), "height": r.height()})
}

fn layer_json(layer: &Layer, active: Option<LayerId>) -> Value {
    let kind = match &layer.kind {
        LayerKind::Raster(_) if layer.shape.is_some() => "shape".to_string(),
        LayerKind::Raster(_) if layer.smart.is_some() => "smart_object".to_string(),
        LayerKind::Raster(_) => "raster".to_string(),
        LayerKind::Group(_) => "group".to_string(),
        LayerKind::Adjustment(a) => format!("adjustment:{}", a.kind.display_name()),
    };
    let mut out = json!({
        "id": layer.id.0,
        "name": layer.name,
        "kind": kind,
        "visible": layer.visible,
        "opacity": layer.opacity,
        "blend": format!("{:?}", layer.blend),
        "bounds": rect_json(layer.content_bounds()),
    });
    let obj = out.as_object_mut().unwrap();
    if layer.fill_opacity != 1.0 {
        obj.insert("fill_opacity".into(), json!(layer.fill_opacity));
    }
    if layer.clipping {
        obj.insert("clipping".into(), json!(true));
    }
    if layer.locked {
        obj.insert("locked".into(), json!(true));
    }
    if layer.mask.is_some() {
        obj.insert("has_mask".into(), json!(true));
    }
    if !layer.style.is_empty() {
        obj.insert("has_effects".into(), json!(true));
    }
    if active == Some(layer.id) {
        obj.insert("active".into(), json!(true));
    }
    if let Some(children) = layer.children() {
        obj.insert(
            "children".into(),
            Value::Array(children.iter().map(|c| layer_json(c, active)).collect()),
        );
    }
    out
}

fn option_json(option: &ToolOption) -> Value {
    let (kind, extra) = match option.kind {
        OptionKind::Slider { min, max, suffix } => {
            ("slider", json!({"min": min, "max": max, "suffix": suffix}))
        }
        OptionKind::Toggle => ("toggle", json!({})),
        OptionKind::Choice(names) => ("choice", json!({"choices": names})),
    };
    let value = match option.value {
        OptionValue::Num(v) => json!(v),
        OptionValue::Bool(b) => json!(b),
        OptionValue::Choice(i) => match option.kind {
            OptionKind::Choice(names) => json!(names.get(i).copied().unwrap_or("?")),
            _ => json!(i),
        },
    };
    let mut out = json!({
        "key": option.key,
        "label": option.label,
        "kind": kind,
        "value": value,
    });
    out.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    out
}

fn state_json(id: &str, sess: &Session) -> Value {
    let doc = &sess.doc;
    let tool_options: Vec<Value> = sess
        .registry
        .tools()
        .find(|t| t.id() == sess.state.active_tool)
        .map(|t| t.options().iter().map(option_json).collect())
        .unwrap_or_default();
    json!({
        "session": id,
        "document": {
            "title": doc.title,
            "path": doc.path.as_ref().map(|p| p.display().to_string()),
            "width": doc.width,
            "height": doc.height,
            "depth_bits": doc.depth.bytes_per_channel() * 8,
            "mode": doc.mode.display_name(),
            "resolution_dpi": doc.resolution_dpi,
            "dirty": doc.dirty,
        },
        "layers": doc
            .tree
            .layers
            .iter()
            .map(|l| layer_json(l, doc.active_layer))
            .collect::<Vec<_>>(),
        "selection": if doc.selection.is_empty() {
            json!({"empty": true})
        } else {
            json!({"empty": false, "bounds": rect_json(doc.selection.bounds())})
        },
        "history": {
            "can_undo": doc.history.can_undo(),
            "undo": doc.history.undo_name(),
            "can_redo": doc.history.can_redo(),
            "redo": doc.history.redo_name(),
        },
        "editor": {
            "active_tool": sess.state.active_tool,
            "tool_options": tool_options,
            "foreground": color_hex(sess.state.foreground),
            "background": color_hex(sess.state.background),
            "brush_size": sess.state.brush_size,
            "brush_hardness": sess.state.brush_hardness,
            "tool_opacity": sess.state.tool_opacity,
            "tolerance": sess.state.tolerance,
            "resample": sess.state.resample.display_name(),
        },
    })
}

/// What the Photoshop plug-in scan found.
///
/// The plug-ins that loaded are published as filter tools like any
/// other, so this is really about the ones that did not: which folders
/// were searched, and what stopped each entry.
fn photoshop_json(sess: &Session) -> Value {
    json!({
        "folders": sess
            .photoshop
            .dirs
            .iter()
            .map(|d| d.display().to_string())
            .collect::<Vec<_>>(),
        "plugins": sess
            .photoshop
            .entries
            .iter()
            .map(|e| {
                json!({
                    "id": e.id,
                    "name": e.name,
                    "file": e.container.display().to_string(),
                    "architecture": e.architecture,
                    "enabled": e.enabled,
                    "available": e.blocker.is_none() && e.enabled,
                    "unavailable_because": e.blocker,
                })
            })
            .collect::<Vec<_>>(),
    })
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
            if let Err(e) = server.call_tool(&name, &json!({})) {
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

    #[test]
    fn colours_parse_in_every_hex_form() {
        assert_eq!(parse_color("#fff").unwrap(), Rgba::new(1.0, 1.0, 1.0, 1.0));
        assert_eq!(
            parse_color("#ff0000").unwrap(),
            Rgba::new(1.0, 0.0, 0.0, 1.0)
        );
        assert_eq!(
            parse_color("00ff0080").unwrap(),
            Rgba::new(0.0, 1.0, 0.0, 128.0 / 255.0)
        );
        assert!(parse_color("#zzz").is_err());
        assert!(parse_color("#ffff").is_err());
        assert_eq!(color_hex(Rgba::new(1.0, 0.0, 0.0, 1.0)), "#ff0000ff");
    }

    #[test]
    fn non_ascii_colours_are_rejected_not_panicked_on() {
        // Byte length, not char count, picks the arm: "é4" is three bytes,
        // so it took the #rgb path and sliced through the middle of 'é'.
        for s in ["#é4", "é4", "#ééé", "#日本語", "#ff00é0", "#—", "#ﬀﬀﬀ"] {
            assert!(
                parse_color(s).is_err(),
                "expected an error for {s:?}, not a panic"
            );
        }
    }

    #[test]
    fn names_normalize_to_modes() {
        assert_eq!(
            parse_blend_mode("soft light").unwrap(),
            BlendMode::SoftLight
        );
        assert_eq!(parse_blend_mode("MULTIPLY").unwrap(), BlendMode::Multiply);
        assert!(parse_blend_mode("bogus").is_err());
    }

    #[test]
    fn option_values_coerce_by_kind() {
        let choice = OptionKind::Choice(&["Mosaic", "Crystals"]);
        assert_eq!(
            coerce_option(&json!("crystals"), choice).unwrap(),
            OptionValue::Choice(1)
        );
        assert_eq!(
            coerce_option(&json!(1), choice).unwrap(),
            OptionValue::Choice(1)
        );
        assert!(coerce_option(&json!("nope"), choice).is_err());
        let slider = OptionKind::Slider {
            min: 0.0,
            max: 10.0,
            suffix: "",
        };
        assert_eq!(
            coerce_option(&json!(4.5), slider).unwrap(),
            OptionValue::Num(4.5)
        );
        assert_eq!(
            coerce_option(&json!(true), OptionKind::Toggle).unwrap(),
            OptionValue::Bool(true)
        );
    }
}
