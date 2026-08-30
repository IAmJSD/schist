//! Every installed feature published as its own MCP tool.
//!
//! The server used to expose a handful of generic invokers -- `run_command`,
//! `select_tool`, `apply_filter` -- plus a `describe` call that listed what
//! they could be pointed at. That put a caller two round trips away from
//! doing anything, and left a filter's parameters undiscoverable until it
//! had asked for them by name: nothing in the tool list said that
//! `apply_filter` would take a `radius` between 0 and 250 for one id and
//! `cell_size` for another.
//!
//! The registry is already a catalog of everything installed, so this
//! projects it straight into MCP: one tool per canvas tool, menu command,
//! filter and adjustment, each carrying its own typed parameters with the
//! label, range, default and choices the options bar would have shown. A
//! third-party plugin dropped into the plugins folder is published the same
//! way, because it goes through the same registry.
//!
//! It is a big tool list -- a couple of hundred entries -- which is the
//! honest size of the application.

use crate::session;
use schist_adjustments::Params;
use schist_core::AdjustmentKind;
use schist_plugin_api::{FilterParam, OptionKind, PluginRegistry, ToolOption, ToolPlugin};
use serde_json::{json, Map, Value};
use std::collections::HashMap;

/// The adjustments Image ▸ Adjustments offers, in menu order.
pub const ADJUSTMENT_KINDS: [AdjustmentKind; 18] = [
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

/// How the host addresses documents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// The stdio server: many sessions, every tool takes a session id and
    /// `create_session`/`list_sessions`/`close_session` manage them.
    Sessions,
    /// The app's in-process host: exactly one document — whichever is
    /// open in the window — so there is no session id to pass and no
    /// session management to publish.
    Active,
}

/// What calling one published tool actually does.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// A server-level tool implemented directly in `main`.
    Builtin(String),
    /// Activate a canvas tool and apply any options passed with it.
    Tool(String),
    /// Run a menu command.
    Command(String),
    /// Run a filter on the active layer.
    Filter(String),
    /// Apply an adjustment destructively to the active layer.
    Adjustment(AdjustmentKind),
}

/// The published tool list, and what each name maps back to.
pub struct Catalog {
    defs: Vec<Value>,
    actions: HashMap<String, Action>,
}

impl Catalog {
    /// Assemble the list from a registry built the way a session builds
    /// one. The registry itself is dropped afterwards: everything the
    /// catalog needs has been copied into owned JSON, and a session
    /// builds its own anyway (tools carry per-gesture state).
    pub fn build() -> Catalog {
        let (registry, _wasm, _photoshop) = session::build_registry();
        Catalog::from_registry(&registry)
    }

    pub fn from_registry(registry: &PluginRegistry) -> Catalog {
        Catalog::from_registry_scoped(registry, Scope::Sessions)
    }

    /// Assemble the list for a host that addresses documents its own way.
    ///
    /// The definitions are built with their session id in place and
    /// stripped afterwards for [`Scope::Active`]: one construction path,
    /// so the two scopes cannot drift apart in what they publish.
    pub fn from_registry_scoped(registry: &PluginRegistry, scope: Scope) -> Catalog {
        let mut catalog = Catalog::assemble(registry);
        if scope == Scope::Active {
            let managers = ["create_session", "list_sessions", "close_session"];
            catalog.defs.retain(|def| {
                !managers
                    .iter()
                    .any(|m| def["name"].as_str() == Some(m))
            });
            for name in managers {
                catalog.actions.remove(name);
            }
            for def in &mut catalog.defs {
                let schema = &mut def["inputSchema"];
                if let Some(props) = schema["properties"].as_object_mut() {
                    props.remove("session");
                }
                if let Some(required) = schema["required"].as_array_mut() {
                    required.retain(|r| r.as_str() != Some("session"));
                }
            }
        }
        catalog
    }

    fn assemble(registry: &PluginRegistry) -> Catalog {
        let mut b = Builder::default();
        for (name, def, action) in builtins(registry) {
            b.push(name, def, action);
        }
        for tool in registry.tools() {
            b.push(
                format!("tool_{}", slug(tool.id())),
                tool_def(tool),
                Action::Tool(tool.id().to_string()),
            );
        }
        for command in registry.commands() {
            b.push(
                format!("cmd_{}", slug(command.id)),
                json!({
                    "description": format!(
                        "{}{} Menu command {:?}{}.",
                        command.title,
                        end(command.description),
                        command.id,
                        match command.keybind {
                            Some(k) => format!(", bound to {k} in the app"),
                            None => String::new(),
                        },
                    ),
                    "inputSchema": schema(json!({"session": session_prop()}), &["session"]),
                }),
                Action::Command(command.id.to_string()),
            );
        }
        for filter in registry.filters() {
            let params = filter.params();
            let mut props = json!({"session": session_prop()});
            let object = props.as_object_mut().unwrap();
            for param in &params {
                object.insert(param.key.to_string(), filter_param_prop(param));
            }
            b.push(
                format!("filter_{}", slug(filter.id().trim_start_matches("filter."))),
                json!({
                    "description": format!(
                        "{} — Filter ▸ {}. Runs on the active raster layer, feathered through \
                         the selection when there is one, as one undoable edit.{}{}",
                        filter.name(),
                        filter.category(),
                        if params.is_empty() {
                            String::new()
                        } else {
                            " Omitted parameters keep their defaults.".to_string()
                        },
                        match filter.info() {
                            Some(info) => format!(" {info}"),
                            None => String::new(),
                        },
                    ),
                    "inputSchema": schema(props, &["session"]),
                }),
                Action::Filter(filter.id().to_string()),
            );
        }
        for kind in ADJUSTMENT_KINDS {
            b.push(
                format!("adjust_{}", slug(kind.display_name())),
                adjustment_def(kind),
                Action::Adjustment(kind),
            );
        }
        Catalog {
            defs: b.defs,
            actions: b.actions,
        }
    }

    /// The `tools/list` payload.
    pub fn defs(&self) -> &[Value] {
        &self.defs
    }

    pub fn action(&self, name: &str) -> Option<&Action> {
        self.actions.get(name)
    }
}

#[derive(Default)]
struct Builder {
    defs: Vec<Value>,
    actions: HashMap<String, Action>,
}

impl Builder {
    /// Publish one tool, under a name nothing else has taken. Two
    /// plugins are free to pick ids that sanitize to the same thing;
    /// silently publishing one of them and dropping the other would make
    /// a feature disappear, so the loser is numbered instead.
    fn push(&mut self, mut name: String, mut def: Value, action: Action) {
        if self.actions.contains_key(&name) {
            let base = name.clone();
            let mut n = 2;
            while self.actions.contains_key(&name) {
                name = format!("{base}_{n}");
                n += 1;
            }
        }
        def.as_object_mut()
            .unwrap()
            .insert("name".into(), json!(name.clone()));
        self.defs.push(def);
        self.actions.insert(name, action);
    }
}

/// A registry id as an MCP tool name: `marquee.rect` → `marquee_rect`.
fn slug(id: &str) -> String {
    let mut out = String::with_capacity(id.len());
    for c in id.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('_') {
            out.push('_');
        }
    }
    out.trim_matches('_').to_string()
}

/// A sentence, spaced off from whatever precedes it, or nothing.
fn end(description: &str) -> String {
    if description.is_empty() {
        String::new()
    } else {
        format!(" — {description}")
    }
}

fn session_prop() -> Value {
    json!({"type": "string", "description": "Session id from create_session"})
}

fn schema(properties: Value, required: &[&str]) -> Value {
    json!({"type": "object", "properties": properties, "required": required})
}

fn modifiers_prop() -> Value {
    json!({
        "type": "object",
        "description": "Held modifier keys",
        "properties": {
            "shift": {"type": "boolean"},
            "alt": {"type": "boolean"},
            "ctrl": {"type": "boolean", "description": "Ctrl/Cmd"},
        },
    })
}

/// One canvas tool: selecting it, and its options bar as parameters.
fn tool_def(tool: &dyn ToolPlugin) -> Value {
    let options = tool.options();
    let mut props = json!({"session": session_prop()});
    let object = props.as_object_mut().unwrap();
    for option in &options {
        object.insert(option.key.to_string(), tool_option_prop(option));
    }
    let description = format!(
        "{} tool{} Makes it the active tool for this session and applies any options passed \
         with it; drive it afterwards with tool_stroke (pointer gestures in document pixels) \
         and tool_input (Enter, Escape or raw keys).{}",
        tool.name(),
        end(tool.description()),
        match tool.shortcut() {
            Some(key) => format!(" Its shortcut in the app is {key:?}."),
            None => String::new(),
        },
    );
    json!({
        "description": description,
        "inputSchema": schema(props, &["session"]),
    })
}

/// An options-bar control as a JSON Schema property.
fn tool_option_prop(option: &ToolOption) -> Value {
    let label = if option.label.is_empty() {
        option.key
    } else {
        option.label
    };
    match option.kind {
        OptionKind::Slider { min, max, suffix } => json!({
            "type": "number",
            "minimum": min,
            "maximum": max,
            "description": format!(
                "{label}. {min}..{max}{}, currently {}",
                suffix.trim_end(),
                option.value.num(),
            ),
        }),
        OptionKind::Toggle => json!({
            "type": "boolean",
            "description": format!("{label}, currently {}", option.value.bool()),
        }),
        OptionKind::Choice(names) => json!({
            "type": "string",
            "enum": names,
            "description": format!(
                "{label}, currently {:?}",
                names.get(option.value.index()).copied().unwrap_or_default(),
            ),
        }),
    }
}

/// A filter tunable as a JSON Schema property.
fn filter_param_prop(param: &FilterParam) -> Value {
    if param.choices.is_empty() {
        json!({
            "type": "number",
            "minimum": param.min,
            "maximum": param.max,
            "description": format!(
                "{}. {}..{}{} (default {})",
                param.label,
                param.min,
                param.max,
                param.suffix.trim_end(),
                param.default,
            ),
        })
    } else {
        json!({
            "type": "string",
            "enum": param.choices,
            "description": format!(
                "{} (default {:?})",
                param.label,
                param
                    .choices
                    .get(param.default.max(0.0) as usize)
                    .copied()
                    .unwrap_or_default(),
            ),
        })
    }
}

/// One adjustment: its sliders, its flags, and a raw escape hatch.
///
/// The sliders come from the same `param_specs` the adjustment dialog
/// renders. The flags -- Monochrome, Preserve Luminosity, Colorize --
/// are not in that list because the dialog draws them itself, so they
/// are read out of the serialized default instead; that also keeps them
/// working for whatever an adjustment grows next.
fn adjustment_def(kind: AdjustmentKind) -> Value {
    let defaults = Params::default_for(kind);
    let mut props = json!({"session": session_prop()});
    let object = props.as_object_mut().unwrap();
    for spec in defaults.param_specs() {
        object.insert(
            spec.key.to_string(),
            json!({
                "type": "number",
                "minimum": spec.min,
                "maximum": spec.max,
                "description": format!(
                    "{}. {}..{}{} (default {})",
                    spec.label,
                    spec.min,
                    spec.max,
                    spec.suffix.trim_end(),
                    spec.value,
                ),
            }),
        );
    }
    for (key, value) in flags(&defaults) {
        object.insert(
            key.clone(),
            json!({
                "type": "boolean",
                "description": format!("{} (default {value})", label(&key)),
            }),
        );
    }
    // A unit variant (Invert) serializes as a bare string: there is
    // nothing to pass, so it gets no escape hatch either.
    let serialized = serde_json::to_value(&defaults).unwrap_or_default();
    if serialized.is_object() {
        object.insert(
            "params".into(),
            json!({
                "type": "object",
                "description": format!(
                    "The whole parameter set at once, in the adjustment's serde form: {}. \
                     Anything the properties above cannot reach -- curve points, gradient \
                     stops, per-range tables -- goes here, and the properties are applied \
                     on top of it.",
                    serialized,
                ),
            }),
        );
    }
    json!({
        "description": format!(
            "Image ▸ Adjustments ▸ {}: applied destructively to the active layer's pixels, \
             feathered through the selection when there is one, as one undoable edit.",
            kind.display_name(),
        ),
        "inputSchema": schema(props, &["session"]),
    })
}

/// A payload field name as something to read: `preserve_luminosity`
/// becomes "Preserve luminosity".
fn label(key: &str) -> String {
    let spaced = key.replace('_', " ");
    let mut chars = spaced.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => spaced,
    }
}

/// The boolean fields of an adjustment's payload, by name.
pub fn flags(params: &Params) -> Vec<(String, bool)> {
    let Ok(Value::Object(outer)) = serde_json::to_value(params) else {
        return Vec::new();
    };
    // Externally tagged: {"ChannelMixer": {...}}. A unit variant (Invert)
    // serializes as a bare string and never reaches here.
    let Some(Value::Object(fields)) = outer.values().next() else {
        return Vec::new();
    };
    fields
        .iter()
        .filter_map(|(k, v)| v.as_bool().map(|b| (k.clone(), b)))
        .collect()
}

/// Set one boolean field in an adjustment's payload.
///
/// `Params` is an enum of a dozen shapes with no field-level setter, and
/// adding one per flag would mean touching the adjustments crate every
/// time an adjustment grows a checkbox. Round-tripping through the
/// serialized form reaches all of them and keeps working.
pub fn set_flag(params: &Params, key: &str, value: bool) -> Option<Params> {
    let Ok(Value::Object(mut outer)) = serde_json::to_value(params) else {
        return None;
    };
    let variant = outer.keys().next()?.clone();
    let Some(Value::Object(fields)) = outer.get_mut(&variant) else {
        return None;
    };
    if !fields.get(key).is_some_and(Value::is_boolean) {
        return None;
    }
    fields.insert(key.to_string(), json!(value));
    serde_json::from_value(Value::Object(outer)).ok()
}

/// The server's own tools: sessions, state, rendering and files.
fn builtins(registry: &PluginRegistry) -> Vec<(String, Value, Action)> {
    let readable: Vec<&str> = registry
        .codecs()
        .flat_map(|c| c.extensions().iter().copied())
        .collect();
    let writable: Vec<&str> = registry
        .codecs()
        .filter(|c| c.can_export())
        .flat_map(|c| c.extensions().iter().copied())
        .collect();
    let blend_modes: Vec<String> = crate::dispatch::BLEND_MODES
        .iter()
        .map(|m| format!("{m:?}"))
        .collect();
    let def = |name: &str, description: String, props: Value, required: &[&str]| {
        (
            name.to_string(),
            json!({"description": description, "inputSchema": schema(props, required)}),
            Action::Builtin(name.to_string()),
        )
    };
    vec![
        def(
            "create_session",
            format!(
                "Create an editing session: open an image file ({}) or start a blank \
                 document with a white Background layer. Returns the session id every other \
                 tool takes, and the document's initial state.",
                readable.join(", "),
            ),
            json!({
                "path": {"type": "string", "description": "File to open; omit for a blank document"},
                "width": {"type": "integer", "description": "Blank document width (default 1280)"},
                "height": {"type": "integer", "description": "Blank document height (default 800)"},
                "depth": {"type": "integer", "enum": [8, 16, 32], "description": "Bits per channel (default 8)"},
                "title": {"type": "string"},
            }),
            &[],
        ),
        def(
            "list_sessions",
            "List the open sessions: id, title, path, size, and whether each has unsaved \
             changes."
                .to_string(),
            json!({}),
            &[],
        ),
        def(
            "close_session",
            "Close a session, discarding unsaved changes.".to_string(),
            json!({"session": session_prop()}),
            &["session"],
        ),
        def(
            "get_state",
            "Document info, the full layer tree with layer ids, the selection, undo/redo \
             state, and editor state including the active tool's current options."
                .to_string(),
            json!({"session": session_prop()}),
            &["session"],
        ),
        def(
            "tool_stroke",
            "Drive the active tool through one pointer gesture in document pixels: down on \
             the first point, drag through the rest, up on the last. One point clicks. A \
             brush stroke, a marquee drag, a transform-handle drag and a text-layer click \
             are all one call. Modal tools (crop, transform, type) keep a pending state \
             afterwards — finish with tool_input."
                .to_string(),
            json!({
                "session": session_prop(),
                "points": {
                    "type": "array",
                    "items": {"type": "array", "items": {"type": "number"}, "minItems": 2, "maxItems": 2},
                    "description": "[[x, y], …] in document pixels",
                },
                "pressure": {"type": "number", "description": "Stylus pressure 0..1 (default 1)"},
                "modifiers": modifiers_prop(),
            }),
            &["session", "points"],
        ),
        def(
            "tool_input",
            "Non-pointer input for the active tool: commit (Enter) or cancel (Escape) a \
             pending crop/transform/text gesture, or send a raw key — the type tool takes \
             text through action \"key\" with the character in \"text\"."
                .to_string(),
            json!({
                "session": session_prop(),
                "action": {"type": "string", "enum": ["key", "commit", "cancel"]},
                "key": {"type": "string", "description": "Physical key name for action \"key\", e.g. \"a\", \"enter\", \"backspace\""},
                "text": {"type": "string", "description": "Character the key types, when it types one"},
                "modifiers": modifiers_prop(),
            }),
            &["session", "action"],
        ),
        def(
            "set_active_layer",
            "Make a layer the target of tools, filters and layer commands (layer ids come \
             from get_state)."
                .to_string(),
            json!({
                "session": session_prop(),
                "id": {"type": "integer", "description": "Layer id"},
            }),
            &["session", "id"],
        ),
        def(
            "set_layer_props",
            "Change a layer's name, visibility, opacity, fill opacity, blend mode, lock or \
             clipping flag, as one undoable edit."
                .to_string(),
            json!({
                "session": session_prop(),
                "id": {"type": "integer"},
                "name": {"type": "string"},
                "visible": {"type": "boolean"},
                "opacity": {"type": "number", "description": "0..1"},
                "fill_opacity": {"type": "number", "description": "0..1"},
                "blend": {"type": "string", "enum": blend_modes},
                "locked": {"type": "boolean"},
                "clipping": {"type": "boolean"},
            }),
            &["session", "id"],
        ),
        def(
            "set_editor",
            "Set shared editor state: foreground/background colours, brush size and \
             hardness, tool opacity, magic-wand tolerance, transform resampling."
                .to_string(),
            json!({
                "session": session_prop(),
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
            "Composite the document (or a region) and return it as a PNG image, downscaled \
             to max_dim for viewing. Pass path to also write the full-resolution PNG to disk."
                .to_string(),
            json!({
                "session": session_prop(),
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
            format!(
                "Save the document, codec chosen by the extension ({}; .psd/.psb keep \
                 layers, raster formats flatten). Defaults to the path it was opened from.",
                writable.join(", "),
            ),
            json!({
                "session": session_prop(),
                "path": {"type": "string", "description": "Target path; optional when the document already has one"},
            }),
            &["session"],
        ),
        def(
            "export",
            "Export a flattened copy with encoder settings, leaving the document's own path \
             untouched."
                .to_string(),
            json!({
                "session": session_prop(),
                "path": {"type": "string"},
                "quality": {"type": "integer", "description": "1..100 for lossy formats (default 90)"},
                "bit_depth": {"type": "integer", "description": "Bits per channel where the format supports a choice (default 8)"},
                "dither": {"type": "boolean", "description": "Dither when reducing depth (default true)"},
            }),
            &["session", "path"],
        ),
        def(
            "photoshop_plugins",
            "Which Photoshop `.8bf` plug-ins were found, and why any of them is unavailable. \
             The ones that loaded are published as filter tools like any other; this is for \
             the ones that did not."
                .to_string(),
            json!({"session": session_prop()}),
            &["session"],
        ),
    ]
}

/// The arguments of a call that are not the session, i.e. the ones a
/// published tool's own parameters were built from.
pub fn parameters(args: &Value) -> Map<String, Value> {
    let mut out = args.as_object().cloned().unwrap_or_default();
    out.remove("session");
    out.retain(|_, v| !v.is_null());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find<'a>(catalog: &'a Catalog, name: &str) -> &'a Value {
        catalog
            .defs()
            .iter()
            .find(|d| d["name"] == name)
            .unwrap_or_else(|| panic!("{name} was not published"))
    }

    /// A name a client cannot call is a feature that is not there.
    #[test]
    fn names_are_unique_and_callable() {
        let catalog = Catalog::build();
        let mut seen = std::collections::HashSet::new();
        for def in catalog.defs() {
            let name = def["name"].as_str().expect("every tool has a name");
            assert!(seen.insert(name), "{name} was published twice");
            assert!(
                !name.is_empty() && name.len() <= 64,
                "{name} is a bad length"
            );
            assert!(
                name.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
                "{name} has characters a tool name cannot have"
            );
            assert!(
                catalog.action(name).is_some(),
                "{name} maps back to nothing"
            );
            assert!(
                def["description"].as_str().is_some_and(|d| d.len() > 20),
                "{name} says nothing about itself"
            );
        }
    }

    /// The whole point: a filter's tunables are in the tool list, not
    /// behind a second call.
    #[test]
    fn a_filter_publishes_its_parameters() {
        let catalog = Catalog::build();
        let blur = find(&catalog, "filter_gaussian_blur");
        let radius = &blur["inputSchema"]["properties"]["radius"];
        assert_eq!(radius["type"], "number");
        assert!(radius["maximum"].as_f64().unwrap() > 0.0);
        assert!(radius["description"].as_str().unwrap().contains("default"));
        assert_eq!(
            catalog.action("filter_gaussian_blur"),
            Some(&Action::Filter("filter.gaussian_blur".into()))
        );
        // A choice reads as its names, not as an index.
        let noise = find(&catalog, "filter_add_noise");
        assert_eq!(
            noise["inputSchema"]["properties"]["distribution"]["enum"],
            json!(["Uniform", "Gaussian"])
        );
    }

    #[test]
    fn a_tool_publishes_its_options_bar() {
        let catalog = Catalog::build();
        let marquee = find(&catalog, "tool_marquee_rect");
        let props = &marquee["inputSchema"]["properties"];
        assert_eq!(props["marquee-feather"]["type"], "number");
        assert!(props["marquee-mode"]["enum"].is_array());
        assert_eq!(
            catalog.action("tool_marquee_rect"),
            Some(&Action::Tool("marquee.rect".into()))
        );
    }

    /// "Grow" is a menu label; on its own it tells a caller nothing.
    #[test]
    fn a_command_describes_itself_beyond_its_title() {
        let catalog = Catalog::build();
        let grow = find(&catalog, "cmd_select_grow");
        let text = grow["description"].as_str().unwrap();
        assert!(text.contains("tolerance"), "{text}");
        assert_eq!(
            catalog.action("cmd_select_grow"),
            Some(&Action::Command("select.grow".into()))
        );
    }

    #[test]
    fn an_adjustment_publishes_sliders_flags_and_an_escape_hatch() {
        let catalog = Catalog::build();
        let mixer = find(&catalog, "adjust_channel_mixer");
        let props = &mixer["inputSchema"]["properties"];
        assert_eq!(props["r_r"]["type"], "number");
        assert_eq!(props["monochrome"]["type"], "boolean");
        assert_eq!(props["params"]["type"], "object");
        // Curves has no sliders at all, so the escape hatch is the whole
        // of its interface.
        let curves = find(&catalog, "adjust_curves");
        assert!(curves["inputSchema"]["properties"]["params"].is_object());
        // Invert has no parameters in any form.
        let invert = find(&catalog, "adjust_invert");
        assert_eq!(
            invert["inputSchema"]["properties"]
                .as_object()
                .unwrap()
                .len(),
            1,
            "invert should take nothing but the session"
        );
    }

    #[test]
    fn flags_are_read_and_written_through_the_serialized_form() {
        let params = Params::default_for(AdjustmentKind::ChannelMixer);
        assert_eq!(flags(&params), vec![("monochrome".to_string(), false)]);
        let flipped = set_flag(&params, "monochrome", true).expect("set monochrome");
        assert_eq!(flags(&flipped), vec![("monochrome".to_string(), true)]);
        // Sliders survive the round trip the flag takes.
        assert_eq!(flipped.param_specs(), params.param_specs());
        assert!(set_flag(&params, "nonexistent", true).is_none());
        assert!(flags(&Params::default_for(AdjustmentKind::Invert)).is_empty());
    }

    #[test]
    fn ids_become_names_a_client_can_type() {
        assert_eq!(slug("marquee.rect"), "marquee_rect");
        assert_eq!(slug("layer.add_mask"), "layer_add_mask");
        assert_eq!(slug("Hue/Saturation"), "hue_saturation");
        assert_eq!(slug("Brightness/Contrast"), "brightness_contrast");
        assert_eq!(slug("  odd  id  "), "odd_id");
    }

    /// The app-hosted catalog is about one document: nothing takes a
    /// session id and nothing manages sessions.
    #[test]
    fn the_active_scope_has_no_sessions_anywhere() {
        let (registry, _wasm, _photoshop) = session::build_registry();
        let catalog = Catalog::from_registry_scoped(&registry, Scope::Active);
        assert!(catalog.defs().len() > 100);
        for name in ["create_session", "list_sessions", "close_session"] {
            assert!(catalog.action(name).is_none(), "{name} was published");
        }
        for def in catalog.defs() {
            let name = def["name"].as_str().unwrap();
            assert!(
                def["inputSchema"]["properties"]["session"].is_null(),
                "{name} still takes a session id"
            );
            let required = def["inputSchema"]["required"].as_array().unwrap();
            assert!(
                !required.iter().any(|r| r == "session"),
                "{name} still requires a session id"
            );
        }
        // The sibling scope built from the same registry still has them.
        let sessions = Catalog::from_registry_scoped(&registry, Scope::Sessions);
        assert!(sessions.action("create_session").is_some());
        let state = find(&sessions, "get_state");
        assert!(state["inputSchema"]["properties"]["session"].is_object());
    }

    /// Two ids that sanitize alike must both stay reachable.
    #[test]
    fn colliding_names_are_numbered_rather_than_dropped() {
        let mut b = Builder::default();
        b.push("filter_x".into(), json!({}), Action::Filter("a".into()));
        b.push("filter_x".into(), json!({}), Action::Filter("b".into()));
        assert_eq!(b.defs[0]["name"], "filter_x");
        assert_eq!(b.defs[1]["name"], "filter_x_2");
        assert_eq!(b.actions.len(), 2);
    }
}
