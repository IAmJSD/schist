//! Turning a published tool name back into work on a session.
//!
//! Everything here operates on a [`SessionCtx`] — borrowed views of a
//! document, editor state and registry — so the stdio server and the
//! app's in-process host share one implementation. Session management
//! (create/list/close) is the one thing that stays with the host, since
//! only the stdio server has more than one session to manage.

use crate::catalog::{self, Action};
use crate::session::SessionCtx;
use anyhow::{anyhow, bail, Result};
use base64::Engine as _;
use schist_core::color::Rgba;
use schist_core::{AdjustmentKind, BlendMode, IntRect, Layer, LayerId, LayerKind};
use schist_plugin_api::{ExportOptions, Modifiers, OptionKind, OptionValue, ToolOption};
use serde_json::{json, Value};

/// Run one published tool against a session. `session_id` is echoed into
/// state reports when the host addresses sessions by id; the app's
/// single-document host passes `None` and the field is simply absent.
pub fn call_action(
    sess: &mut SessionCtx,
    action: &Action,
    args: &Value,
    session_id: Option<&str>,
) -> Result<Value> {
    match action {
        Action::Builtin(name) => call_builtin(sess, name, args, session_id),
        Action::Tool(id) => select_tool(sess, id, args),
        Action::Command(id) => {
            let title = sess.run_command(id)?;
            text(title)
        }
        Action::Filter(id) => apply_filter(sess, id, args),
        Action::Adjustment(kind) => apply_adjustment(sess, *kind, args),
    }
}

/// The server's own tools: the ones that are about state and files rather
/// than about something in the registry. The session-management builtins
/// (`create_session`, `list_sessions`, `close_session`) belong to the
/// stdio server and are not handled here.
fn call_builtin(
    sess: &mut SessionCtx,
    name: &str,
    args: &Value,
    session_id: Option<&str>,
) -> Result<Value> {
    match name {
        "get_state" => text_json(state_json(session_id, sess)),
        "tool_stroke" => tool_stroke(sess, args),
        "tool_input" => {
            let modifiers = parse_modifiers(args.get("modifiers"));
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
        "set_layer_props" => set_layer_props(sess, args),
        "set_editor" => set_editor(sess, args),
        "render" => render(sess, args),
        "save" => {
            let path = args.get("path").and_then(|v| v.as_str()).map(String::from);
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
            sess.export(&path, &options)?;
            text(format!("exported {}", path.display()))
        }
        "photoshop_plugins" => text_json(photoshop_json(sess)),
        other => bail!("unknown tool {other:?}"),
    }
}

/// Make a canvas tool active and apply the options passed with it.
///
/// Options are read fresh between writes because a tool's options bar
/// can change shape as it is set: the move tool only offers its
/// auto-select target once auto-select is on.
fn select_tool(sess: &mut SessionCtx, id: &str, args: &Value) -> Result<Value> {
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

fn apply_filter(sess: &mut SessionCtx, id: &str, args: &Value) -> Result<Value> {
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

fn apply_adjustment(sess: &mut SessionCtx, kind: AdjustmentKind, args: &Value) -> Result<Value> {
    let mut params = match args.get("params") {
        Some(v) if !v.is_null() => serde_json::from_value::<schist_adjustments::Params>(v.clone())
            .map_err(|e| anyhow!("bad adjustment params: {e}"))?,
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
    let name = sess.apply_adjustment(kind, Some(params))?;
    text(format!("applied {name}"))
}

fn tool_stroke(sess: &mut SessionCtx, args: &Value) -> Result<Value> {
    let points: Vec<(f32, f32)> =
        args.get("points")
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
    let tool = sess.state.active_tool;
    sess.stroke(&points, pressure, modifiers)?;
    text(format!("{tool}: stroke of {} point(s)", points.len()))
}

fn set_layer_props(sess: &mut SessionCtx, args: &Value) -> Result<Value> {
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
    sess.refresh_caches();
    text(format!("updated layer {}", id.0))
}

fn set_editor(sess: &mut SessionCtx, args: &Value) -> Result<Value> {
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

fn render(sess: &mut SessionCtx, args: &Value) -> Result<Value> {
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
    let note = match &out_path {
        Some(path) => format!(
            "rendered {w}x{h} at ({}, {}); full resolution written to {path}, preview {}x{}",
            region.left,
            region.top,
            shown.width(),
            shown.height()
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

// ----- argument helpers -----

pub fn arg_str<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("missing required argument {key:?}"))
}

pub fn text(s: String) -> Result<Value> {
    Ok(json!([{"type": "text", "text": s}]))
}

pub fn text_json(v: Value) -> Result<Value> {
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

pub const BLEND_MODES: [BlendMode; 28] = [
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

pub fn state_json(id: Option<&str>, sess: &SessionCtx) -> Value {
    let doc = &sess.doc;
    let tool_options: Vec<Value> = sess
        .registry
        .tools()
        .find(|t| t.id() == sess.state.active_tool)
        .map(|t| t.options().iter().map(option_json).collect())
        .unwrap_or_default();
    let mut out = json!({
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
    });
    if let Some(id) = id {
        out.as_object_mut()
            .unwrap()
            .insert("session".into(), json!(id));
    }
    out
}

/// What the Photoshop plug-in scan found.
///
/// The plug-ins that loaded are published as filter tools like any
/// other, so this is really about the ones that did not: which folders
/// were searched, and what stopped each entry.
fn photoshop_json(sess: &SessionCtx) -> Value {
    let Some(photoshop) = sess.photoshop else {
        return json!({"folders": [], "plugins": []});
    };
    json!({
        "folders": photoshop
            .dirs
            .iter()
            .map(|d| d.display().to_string())
            .collect::<Vec<_>>(),
        "plugins": photoshop
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
        use serde_json::json;
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
