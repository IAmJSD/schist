//! `schist-mcp` — a Model Context Protocol server over stdio.
//!
//! Create a session (a blank document or an opened file), then drive it by
//! session id: every registered tool, menu command, filter, adjustment and
//! codec — the same plugin registry the GPUI app assembles — plus document
//! introspection and PNG rendering. JSON-RPC 2.0, newline-delimited, on
//! stdin/stdout; logs go to stderr so they never corrupt the stream.

mod session;

use anyhow::{anyhow, bail, Result};
use base64::Engine as _;
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
        let method = message.get("method").and_then(|m| m.as_str());
        // Notifications carry no id and get no reply.
        let Some(id) = id else {
            continue;
        };
        // A request *with* an id and no usable method still needs an
        // answer, or the client blocks forever waiting for one. This used
        // to `continue` before the id was even considered.
        let Some(method) = method else {
            let reply = json!({
                "jsonrpc": "2.0", "id": id,
                "error": {"code": -32600, "message": "Invalid Request: missing or non-string method"}
            });
            respond(&reply);
            continue;
        };
        let params = message.get("params").cloned().unwrap_or(Value::Null);
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
}

/// Protocol revisions this server implements, newest first.
const SUPPORTED_PROTOCOLS: &[&str] = &["2025-06-18", "2025-03-26"];

/// Reject arguments the handler does not understand.
///
/// Every field used to be read with `get(k).and_then(as_TYPE)`, so an
/// absent, misspelled or wrongly-typed key yielded `None` and was
/// skipped -- and the handler then reported success regardless. A model
/// sending `{"brushsize": 40}` was told the editor state was updated and
/// went on to reason over state it believed it had set.
fn reject_unknown_keys(args: &Value, known: &[&str]) -> Result<()> {
    let Some(object) = args.as_object() else {
        return Ok(());
    };
    for key in object.keys() {
        // `session` addresses the call rather than the state it changes,
        // and every handler accepts it.
        if key == "session" || known.contains(&key.as_str()) {
            continue;
        }
        bail!("unknown argument {key:?} (accepts: {})", known.join(", "));
    }
    Ok(())
}

/// A typed argument, or an error naming what was wrong with it.
fn typed<'a, T>(
    args: &'a Value,
    key: &str,
    kind: &str,
    f: impl Fn(&'a Value) -> Option<T>,
) -> Result<Option<T>> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => f(value)
            .map(Some)
            .ok_or_else(|| anyhow!("{key} must be {kind}, not {value}")),
    }
}

impl Server {
    fn handle(&mut self, method: &str, params: &Value) -> Result<Value, RpcError> {
        match method {
            "initialize" => {
                let requested = params
                    .get("protocolVersion")
                    .and_then(|v| v.as_str())
                    .unwrap_or(SUPPORTED_PROTOCOLS[0]);
                // Echoing the request back told a client speaking a
                // future or bogus revision that the server speaks it too,
                // so the mismatch surfaced later as confusing tool-level
                // failures instead of at the handshake.
                let agreed = if SUPPORTED_PROTOCOLS.contains(&requested) {
                    requested
                } else {
                    SUPPORTED_PROTOCOLS[0]
                };
                Ok(json!({
                    "protocolVersion": agreed,
                    "capabilities": {"tools": {}},
                    "serverInfo": {
                        "name": "schist-mcp",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                    "instructions": "Headless Schist image editor. Call create_session first \
                        (blank document or open a file) and pass the returned session id to every \
                        other tool. Use describe to enumerate the session's canvas tools, menu \
                        commands, filters, adjustments and codecs; get_state for the document and \
                        layer tree; render to see the canvas as a PNG. Edits go through the same \
                        plugin registry and undo history as the GUI (undo/redo are the edit.undo \
                        and edit.redo commands).",
                }))
            }
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": tool_defs()})),
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

    fn call_tool(&mut self, name: &str, args: &Value) -> Result<Value> {
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
            "describe" => {
                let sess = self.session(args)?;
                let what = args.get("what").and_then(|v| v.as_str()).unwrap_or("all");
                text_json(describe(sess, what)?)
            }
            "get_state" => {
                let id = arg_str(args, "session")?.to_string();
                let sess = self.session(args)?;
                text_json(state_json(&id, sess))
            }
            "run_command" => {
                let sess = self.session(args)?;
                let title = sess.run_command(arg_str(args, "id")?)?;
                text(title)
            }
            "select_tool" => {
                let sess = self.session(args)?;
                let id = sess.activate_tool(arg_str(args, "id")?)?;
                text(format!("active tool: {id}"))
            }
            "set_tool_options" => self.set_tool_options(args),
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
            "apply_filter" => {
                let values: Vec<(String, f64)> = args
                    .get("params")
                    .and_then(|v| v.as_object())
                    .map(|m| {
                        m.iter()
                            .map(|(k, v)| {
                                v.as_f64()
                                    .map(|n| (k.clone(), n))
                                    .ok_or_else(|| anyhow!("parameter {k:?} must be a number"))
                            })
                            .collect::<Result<Vec<_>>>()
                    })
                    .transpose()?
                    .unwrap_or_default();
                let sess = self.session(args)?;
                let name = sess.apply_filter(arg_str(args, "id")?, &values)?;
                text(format!("applied {name}"))
            }
            "apply_adjustment" => {
                let kind = parse_adjustment_kind(arg_str(args, "kind")?)?;
                let params = match args.get("params") {
                    Some(v) if !v.is_null() => Some(
                        serde_json::from_value::<schist_adjustments::Params>(v.clone())
                            .map_err(|e| anyhow!("bad adjustment params: {e}"))?,
                    ),
                    _ => None,
                };
                let sess = self.session(args)?;
                let name = sess.apply_adjustment(kind, params)?;
                text(format!("applied {name}"))
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
            _ => bail!("unknown tool {name:?}"),
        }
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

    fn set_tool_options(&mut self, args: &Value) -> Result<Value> {
        let options = args
            .get("options")
            .and_then(|v| v.as_object())
            .ok_or_else(|| anyhow!("missing options object"))?
            .clone();
        let sess = self.session(args)?;
        let active = sess.state.active_tool;
        let declared: Vec<ToolOption> = sess
            .registry
            .tools()
            .find(|t| t.id() == active)
            .map(|t| t.options())
            .unwrap_or_default();
        for (key, value) in &options {
            let kind = declared
                .iter()
                .find(|o| o.key == key.as_str())
                .map(|o| o.kind)
                .ok_or_else(|| {
                    anyhow!(
                        "tool {active:?} has no option {key:?} (has: {:?})",
                        declared.iter().map(|o| o.key).collect::<Vec<_>>()
                    )
                })?;
            let value = coerce_option(value, kind)?;
            sess.set_tool_option(key, value)?;
        }
        text(format!("set {} option(s) on {active}", options.len()))
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
        reject_unknown_keys(
            args,
            &[
                "id",
                "name",
                "visible",
                "locked",
                "clipping",
                "opacity",
                "fill_opacity",
                "blend",
            ],
        )?;
        let id = args
            .get("id")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| anyhow!("missing layer id"))?;
        let name = typed(args, "name", "a string", |v| v.as_str())?.map(String::from);
        let visible = typed(args, "visible", "a boolean", |v| v.as_bool())?;
        let locked = typed(args, "locked", "a boolean", |v| v.as_bool())?;
        let clipping = typed(args, "clipping", "a boolean", |v| v.as_bool())?;
        let opacity = typed(args, "opacity", "a number 0..=1", |v| v.as_f64())?
            .map(|v| v.clamp(0.0, 1.0) as f32);
        let fill_opacity = typed(args, "fill_opacity", "a number 0..=1", |v| v.as_f64())?
            .map(|v| v.clamp(0.0, 1.0) as f32);
        let blend = typed(args, "blend", "a blend-mode name", |v| v.as_str())?
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
        reject_unknown_keys(
            args,
            &[
                "foreground",
                "background",
                "resample",
                "brush_size",
                "brush_hardness",
                "tool_opacity",
                "tolerance",
            ],
        )?;
        let foreground = typed(args, "foreground", "a colour string", |v| v.as_str())?
            .map(parse_color)
            .transpose()?;
        let background = typed(args, "background", "a colour string", |v| v.as_str())?
            .map(parse_color)
            .transpose()?;
        let resample = typed(args, "resample", "a string", |v| v.as_str())?
            .map(|s| match s.to_ascii_lowercase().as_str() {
                "nearest" => Ok(schist_core::Filter::Nearest),
                "bilinear" => Ok(schist_core::Filter::Bilinear),
                "bicubic" => Ok(schist_core::Filter::Bicubic),
                other => Err(anyhow!(
                    "resample must be nearest, bilinear or bicubic, not {other:?}"
                )),
            })
            .transpose()?;
        let brush_size = typed(args, "brush_size", "a number", |v| v.as_f64())?;
        let brush_hardness = typed(args, "brush_hardness", "a number", |v| v.as_f64())?;
        let tool_opacity = typed(args, "tool_opacity", "a number", |v| v.as_f64())?;
        let tolerance = typed(args, "tolerance", "an integer 0..=255", |v| v.as_u64())?;
        let sess = self.session(args)?;
        if let Some(c) = foreground {
            sess.state.foreground = c;
        }
        if let Some(c) = background {
            sess.state.background = c;
        }
        if let Some(v) = brush_size {
            sess.state.brush_size = (v as f32).max(1.0);
        }
        if let Some(v) = brush_hardness {
            sess.state.brush_hardness = (v as f32).clamp(0.0, 1.0);
        }
        if let Some(v) = tool_opacity {
            sess.state.tool_opacity = (v as f32).clamp(0.0, 1.0);
        }
        if let Some(v) = tolerance {
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

const ADJUSTMENT_KINDS: [AdjustmentKind; 18] = [
    AdjustmentKind::Levels,
    AdjustmentKind::Curves,
    AdjustmentKind::HueSaturation,
    AdjustmentKind::BrightnessContrast,
    AdjustmentKind::BlackWhite,
    AdjustmentKind::SolidColor,
    AdjustmentKind::GradientFill,
    AdjustmentKind::PatternFill,
    AdjustmentKind::Invert,
    AdjustmentKind::Posterize,
    AdjustmentKind::Threshold,
    AdjustmentKind::ColorBalance,
    AdjustmentKind::Vibrance,
    AdjustmentKind::Exposure,
    AdjustmentKind::PhotoFilter,
    AdjustmentKind::GradientMap,
    AdjustmentKind::SelectiveColor,
    AdjustmentKind::ChannelMixer,
];

fn parse_adjustment_kind(name: &str) -> Result<AdjustmentKind> {
    let wanted = normalize(name);
    ADJUSTMENT_KINDS
        .iter()
        .find(|k| normalize(k.display_name()) == wanted)
        .copied()
        .ok_or_else(|| {
            anyhow!(
                "unknown adjustment {name:?} (kinds: {})",
                ADJUSTMENT_KINDS.map(|k| k.display_name()).join(", ")
            )
        })
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

fn describe(sess: &Session, what: &str) -> Result<Value> {
    let tools = || -> Value {
        sess.registry
            .tools()
            .map(|t| {
                json!({
                    "id": t.id(),
                    "name": t.name(),
                    "group": t.group(),
                    "shortcut": t.shortcut(),
                    "in_toolbar": t.in_toolbar(),
                    "options": t.options().iter().map(option_json).collect::<Vec<_>>(),
                })
            })
            .collect()
    };
    let commands = || -> Value {
        sess.registry
            .commands()
            .iter()
            .map(|c| json!({"id": c.id, "title": c.title, "keybind": c.keybind}))
            .collect()
    };
    let filters = || -> Value {
        sess.registry
            .filters()
            .map(|f| {
                json!({
                    "id": f.id(),
                    "name": f.name(),
                    "category": f.category(),
                    "info": f.info(),
                    "params": f
                        .params()
                        .iter()
                        .map(|p| {
                            json!({
                                "key": p.key,
                                "label": p.label,
                                "min": p.min,
                                "max": p.max,
                                "default": p.default,
                                "suffix": p.suffix,
                                "choices": p.choices,
                            })
                        })
                        .collect::<Vec<_>>(),
                })
            })
            .collect()
    };
    let codecs = || -> Value {
        sess.registry
            .codecs()
            .map(|c| {
                json!({
                    "id": c.id(),
                    "name": c.name(),
                    "extensions": c.extensions(),
                    "can_export": c.can_export(),
                    "supports_quality": c.supports_quality(),
                })
            })
            .collect()
    };
    let adjustments = || -> Value {
        ADJUSTMENT_KINDS
            .iter()
            .map(|k| {
                json!({
                    "kind": k.display_name(),
                    "default_params": serde_json::to_value(
                        schist_adjustments::Params::default_for(*k)
                    )
                    .unwrap_or(Value::Null),
                })
            })
            .collect()
    };
    let blend_modes = || -> Value {
        BLEND_MODES
            .iter()
            .map(|m| json!(format!("{m:?}")))
            .collect()
    };
    Ok(match what {
        "tools" => json!({"tools": tools()}),
        "commands" => json!({"commands": commands()}),
        "filters" => json!({"filters": filters()}),
        "codecs" => json!({"codecs": codecs()}),
        "adjustments" => json!({"adjustments": adjustments()}),
        "blend_modes" => json!({"blend_modes": blend_modes()}),
        "all" => json!({
            "tools": tools(),
            "commands": commands(),
            "filters": filters(),
            "codecs": codecs(),
            "adjustments": adjustments(),
            "blend_modes": blend_modes(),
        }),
        other => bail!(
            "unknown section {other:?} (tools, commands, filters, codecs, adjustments, \
             blend_modes or all)"
        ),
    })
}

// ----- tool definitions -----

fn tool_defs() -> Value {
    let session_prop = json!({"type": "string", "description": "Session id from create_session"});
    let modifiers_prop = json!({
        "type": "object",
        "description": "Held modifier keys",
        "properties": {
            "shift": {"type": "boolean"},
            "alt": {"type": "boolean"},
            "ctrl": {"type": "boolean", "description": "Ctrl/Cmd"},
        },
    });
    let def = |name: &str, description: &str, props: Value, required: &[&str]| {
        json!({
            "name": name,
            "description": description,
            "inputSchema": {
                "type": "object",
                "properties": props,
                "required": required,
            },
        })
    };
    json!([
        def(
            "create_session",
            "Create an editing session: open an image file (PSD/PSB, PNG, JPEG, WebP, TIFF, \
             Affinity .af/.afphoto/.afdesign/.afpub) or start a blank document with a white \
             Background layer. Returns the session id and initial state.",
            json!({
                "path": {"type": "string", "description": "File to open; omit for a blank document"},
                "width": {"type": "integer", "description": "Blank document width (default 1280)"},
                "height": {"type": "integer", "description": "Blank document height (default 800)"},
                "depth": {"type": "integer", "enum": [8, 16, 32], "description": "Bits per channel (default 8)"},
                "title": {"type": "string"},
            }),
            &[],
        ),
        def("list_sessions", "List open sessions.", json!({}), &[]),
        def(
            "close_session",
            "Close a session, discarding unsaved changes.",
            json!({"session": session_prop}),
            &["session"],
        ),
        def(
            "describe",
            "Enumerate what the session can do: canvas tools (with their options), menu \
             commands (run with run_command), filters, destructive adjustments (with default \
             parameter shapes), codecs and blend modes.",
            json!({
                "session": session_prop,
                "what": {
                    "type": "string",
                    "enum": ["tools", "commands", "filters", "codecs", "adjustments", "blend_modes", "all"],
                    "description": "Section to list (default all)",
                },
            }),
            &["session"],
        ),
        def(
            "get_state",
            "Document info, full layer tree (with layer ids), selection, undo/redo state, and \
             editor state including the active tool's options.",
            json!({"session": session_prop}),
            &["session"],
        ),
        def(
            "run_command",
            "Run a menu command by id (see describe): edit.undo, edit.redo, select.all, \
             layer.new, layer.duplicate and everything else the registry declares. \
             Note that image sizing, canvas sizing, transforms and adjustment layers \
             are not registry commands and cannot be reached this way.",
            json!({
                "session": session_prop,
                "id": {"type": "string", "description": "Command id, e.g. \"layer.duplicate\""},
            }),
            &["session", "id"],
        ),
        def(
            "select_tool",
            "Activate a canvas tool by id (see describe): brush, eraser, marquee, lasso, wand, \
             gradient, move, crop, transform, text, shapes, pen…",
            json!({
                "session": session_prop,
                "id": {"type": "string", "description": "Tool id, e.g. \"brush\""},
            }),
            &["session", "id"],
        ),
        def(
            "set_tool_options",
            "Set options-bar values on the active tool. Numbers for sliders, booleans for \
             toggles, and either an index or the choice's name for dropdowns.",
            json!({
                "session": session_prop,
                "options": {
                    "type": "object",
                    "description": "Option key to value, e.g. {\"wand-tolerance\": 40}",
                    "additionalProperties": true,
                },
            }),
            &["session", "options"],
        ),
        def(
            "tool_stroke",
            "Drive the active tool through one pointer gesture in document pixels: down on the \
             first point, drag through the rest, up on the last. One point clicks. A brush \
             stroke, a marquee drag, a transform-handle drag and a text-layer click are all one \
             call. Modal tools (crop, transform, text) keep a pending state afterwards — finish \
             with tool_input.",
            json!({
                "session": session_prop,
                "points": {
                    "type": "array",
                    "items": {"type": "array", "items": {"type": "number"}, "minItems": 2, "maxItems": 2},
                    "description": "[[x, y], …] in document pixels",
                },
                "pressure": {"type": "number", "description": "Stylus pressure 0..1 (default 1)"},
                "modifiers": modifiers_prop,
            }),
            &["session", "points"],
        ),
        def(
            "tool_input",
            "Non-pointer input for the active tool: commit (Enter) or cancel (Escape) a pending \
             crop/transform/text gesture, or send a raw key — the type tool takes text through \
             action \"key\" with the character in \"text\".",
            json!({
                "session": session_prop,
                "action": {"type": "string", "enum": ["key", "commit", "cancel"]},
                "key": {"type": "string", "description": "Physical key name for action \"key\", e.g. \"a\", \"enter\", \"backspace\""},
                "text": {"type": "string", "description": "Character the key types, when it types one"},
                "modifiers": modifiers_prop,
            }),
            &["session", "action"],
        ),
        def(
            "apply_filter",
            "Run a destructive filter on the active pixel layer (through the selection when one \
             exists), as one undoable edit. Omitted parameters use their defaults (see describe).",
            json!({
                "session": session_prop,
                "id": {"type": "string", "description": "Filter id, e.g. \"filter.gaussian-blur\""},
                "params": {
                    "type": "object",
                    "description": "Parameter key to number",
                    "additionalProperties": {"type": "number"},
                },
            }),
            &["session", "id"],
        ),
        def(
            "apply_adjustment",
            "Image ▸ Adjustments: apply an adjustment destructively to the active layer's \
             pixels. Non-destructive adjustment layers live only in the gui and cannot \
             be created from here. params follows the shape shown by \
             describe(adjustments); omit for defaults.",
            json!({
                "session": session_prop,
                "kind": {"type": "string", "description": "e.g. \"Levels\", \"Hue/Saturation\", \"Brightness/Contrast\""},
                "params": {"type": "object", "description": "Serde form of the adjustment's parameters"},
            }),
            &["session", "kind"],
        ),
        def(
            "set_active_layer",
            "Make a layer the target of tools, filters and layer commands (layer ids come from \
             get_state).",
            json!({
                "session": session_prop,
                "id": {"type": "integer", "description": "Layer id"},
            }),
            &["session", "id"],
        ),
        def(
            "set_layer_props",
            "Change a layer's name, visibility, opacity, fill opacity, blend mode, lock or \
             clipping flag, as one undoable edit.",
            json!({
                "session": session_prop,
                "id": {"type": "integer"},
                "name": {"type": "string"},
                "visible": {"type": "boolean"},
                "opacity": {"type": "number", "description": "0..1"},
                "fill_opacity": {"type": "number", "description": "0..1"},
                "blend": {"type": "string", "description": "Blend mode name, e.g. \"Multiply\""},
                "locked": {"type": "boolean"},
                "clipping": {"type": "boolean"},
            }),
            &["session", "id"],
        ),
        def(
            "set_editor",
            "Set shared editor state: foreground/background colours, brush size and hardness, \
             tool opacity, magic-wand tolerance, transform resampling.",
            json!({
                "session": session_prop,
                "foreground": {"type": "string", "description": "#rrggbb or #rrggbbaa"},
                "background": {"type": "string"},
                "brush_size": {"type": "number", "description": "Pixels"},
                "brush_hardness": {"type": "number", "description": "0 soft .. 1 hard"},
                "tool_opacity": {"type": "number", "description": "0..1"},
                "tolerance": {"type": "integer", "description": "0..255"},
                "resample": {"type": "string", "enum": ["nearest", "bilinear", "bicubic"]},
            }),
            &["session"],
        ),
        def(
            "render",
            "Composite the document (or a region) and return it as a PNG image, downscaled to \
             max_dim for viewing. Pass path to also write the full-resolution PNG to disk.",
            json!({
                "session": session_prop,
                "x": {"type": "integer"},
                "y": {"type": "integer"},
                "width": {"type": "integer"},
                "height": {"type": "integer"},
                "max_dim": {"type": "integer", "description": "Longest edge of the returned preview (default 1024)"},
                "path": {"type": "string", "description": "Also write full-resolution PNG here"},
            }),
            &["session"],
        ),
        def(
            "save",
            "Save the document, codec chosen by extension (.psd/.psb keeps layers; raster \
             formats flatten). Defaults to the path it was opened from.",
            json!({
                "session": session_prop,
                "path": {"type": "string", "description": "Target path; optional when the document already has one"},
            }),
            &["session"],
        ),
        def(
            "export",
            "Export a flattened copy with encoder settings, leaving the document's own path \
             untouched.",
            json!({
                "session": session_prop,
                "path": {"type": "string"},
                "quality": {"type": "integer", "description": "1..100 for lossy formats (default 90)"},
                "bit_depth": {"type": "integer", "description": "Bits per channel where the format supports a choice (default 8)"},
                "dither": {"type": "boolean", "description": "Dither when reducing depth (default true)"},
            }),
            &["session", "path"],
        ),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn names_normalize_to_kinds_and_modes() {
        assert_eq!(
            parse_adjustment_kind("Hue/Saturation").unwrap(),
            AdjustmentKind::HueSaturation
        );
        assert_eq!(
            parse_adjustment_kind("brightness-contrast").unwrap(),
            AdjustmentKind::BrightnessContrast
        );
        assert!(parse_adjustment_kind("sepia").is_err());
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

    /// Every field used to be read with `get(k).and_then(as_TYPE)`, so a
    /// misspelled or wrongly-typed key was skipped and the handler
    /// reported success anyway — and the model went on reasoning over
    /// state it believed it had set.
    #[test]
    fn misspelled_and_mistyped_arguments_are_rejected() {
        let known = ["brush_size", "tolerance"];
        assert!(reject_unknown_keys(&json!({"brush_size": 40}), &known).is_ok());
        // The session key addresses the call, not the state.
        assert!(reject_unknown_keys(&json!({"session": "a", "tolerance": 3}), &known).is_ok());

        let err = reject_unknown_keys(&json!({"brushsize": 40}), &known).unwrap_err();
        assert!(err.to_string().contains("brushsize"), "{err}");

        // Wrong types are named rather than silently dropped.
        let args = json!({"brush_size": "forty"});
        let err = typed(&args, "brush_size", "a number", |v| v.as_f64()).unwrap_err();
        assert!(err.to_string().contains("must be a number"), "{err}");

        // Absent and explicitly null both mean "leave it alone".
        let args = json!({"tolerance": null});
        assert!(typed(&args, "tolerance", "an integer", |v| v.as_u64())
            .unwrap()
            .is_none());
        assert!(typed(&json!({}), "tolerance", "an integer", |v| v.as_u64())
            .unwrap()
            .is_none());
    }

    /// Echoing the requested revision back told a client speaking a
    /// future or bogus one that the server speaks it too, so the mismatch
    /// surfaced later as confusing tool-level failures.
    #[test]
    fn an_unsupported_protocol_version_is_negotiated_down() {
        fn agreed(asked: &str) -> &'static str {
            SUPPORTED_PROTOCOLS
                .iter()
                .copied()
                .find(|v| *v == asked)
                .unwrap_or(SUPPORTED_PROTOCOLS[0])
        }
        assert_eq!(agreed("2025-03-26"), "2025-03-26");
        assert_eq!(agreed("2099-01-01"), SUPPORTED_PROTOCOLS[0]);
        assert_eq!(agreed("nonsense"), SUPPORTED_PROTOCOLS[0]);
    }

    /// The tool descriptions named commands the registry has never had.
    #[test]
    fn the_tool_descriptions_do_not_promise_missing_commands() {
        let text = serde_json::to_string(&tool_defs()).unwrap();
        for missing in ["image.crop", "layer.adjustment."] {
            assert!(
                !text.contains(missing),
                "the descriptions still advertise {missing}"
            );
        }
    }
}
