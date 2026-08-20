//! Keymap assembly and file dialogs.
//!
//! Defaults come from the plugin registry (each command/tool declares its
//! own binding, PLAN.md §5); a user keymap file overlays them. "cmd-" in
//! plugin bindings means the platform primary modifier and is rewritten to
//! "ctrl-" on Linux/Windows.

use crate::actions::*;
use crate::workspace::Workspace;
use gpui::{Context, KeyBinding, PathPromptOptions, Window};
use photoslop_plugin_api::PluginRegistry;
use std::path::PathBuf;

const CONTEXT: Option<&str> = Some("Workspace");
/// Context for bindings without modifiers: suppressed while a tool is
/// capturing typing (see `Workspace::tool_captures_keys`).
const TYPING_SAFE: Option<&str> = Some("Workspace && editable");

fn translate(binding: &str) -> String {
    if cfg!(target_os = "macos") {
        binding.to_string()
    } else {
        binding.replace("cmd-", "ctrl-")
    }
}

pub fn build_bindings(registry: &PluginRegistry) -> Vec<KeyBinding> {
    let mut bindings = Vec::new();

    // Plugin-declared command keybinds.
    for command in registry.commands() {
        if let Some(kb) = command.keybind {
            bindings.push(KeyBinding::new(
                &translate(kb),
                RunCommand {
                    id: command.id.to_string(),
                },
                CONTEXT,
            ));
        }
    }
    // Tool activation keys.
    for tool in registry.tools() {
        if let Some(key) = tool.shortcut() {
            bindings.push(KeyBinding::new(
                key,
                ActivateTool {
                    id: tool.id().to_string(),
                },
                TYPING_SAFE,
            ));
        }
    }
    // App-level bindings.
    bindings.extend([
        KeyBinding::new(&translate("cmd-n"), NewFile, CONTEXT),
        KeyBinding::new(&translate("cmd-o"), OpenFile, CONTEXT),
        KeyBinding::new(&translate("cmd-shift-s"), SaveFileAs, CONTEXT),
        KeyBinding::new(&translate("cmd-s"), SaveFile, CONTEXT),
        KeyBinding::new(&translate("cmd-="), ZoomIn, CONTEXT),
        KeyBinding::new(&translate("cmd--"), ZoomOut, CONTEXT),
        KeyBinding::new(&translate("cmd-0"), ZoomFit, CONTEXT),
        KeyBinding::new(&translate("cmd-1"), ZoomActual, CONTEXT),
        KeyBinding::new("[", BrushSmaller, TYPING_SAFE),
        KeyBinding::new("]", BrushLarger, TYPING_SAFE),
        KeyBinding::new("x", SwapColors, TYPING_SAFE),
        KeyBinding::new("d", DefaultColors, TYPING_SAFE),
        KeyBinding::new("escape", CancelGesture, CONTEXT),
        KeyBinding::new("enter", CommitGesture, TYPING_SAFE),
        KeyBinding::new(
            &translate("cmd-t"),
            ActivateTool {
                id: "transform".into(),
            },
            CONTEXT,
        ),
        KeyBinding::new(&translate("cmd-q"), Quit, CONTEXT),
        KeyBinding::new(&translate("cmd-alt-i"), ShowImageSize, CONTEXT),
        KeyBinding::new(&translate("cmd-alt-c"), ShowCanvasSize, CONTEXT),
        KeyBinding::new(&translate("cmd-k"), ShowPreferences, CONTEXT),
        KeyBinding::new(&translate("cmd-r"), ToggleRulers, CONTEXT),
        KeyBinding::new(&translate("cmd-'"), ToggleGrid, CONTEXT),
        KeyBinding::new(&translate("cmd-;"), ToggleGuides, CONTEXT),
        KeyBinding::new(&translate("cmd-h"), ToggleExtras, CONTEXT),
        KeyBinding::new(&translate("cmd-shift-;"), ToggleSnap, CONTEXT),
        KeyBinding::new(&translate("cmd-alt-;"), ClearGuides, CONTEXT),
        KeyBinding::new("tab", TogglePanels, TYPING_SAFE),
        KeyBinding::new("f", CycleScreenMode, TYPING_SAFE),
        // Adjustment layers, matching Photoshop's Image ▸ Adjustments keys.
        KeyBinding::new(
            &translate("cmd-l"),
            AddAdjustment {
                kind: "levels".into(),
            },
            CONTEXT,
        ),
        KeyBinding::new(
            &translate("cmd-m"),
            AddAdjustment {
                kind: "curves".into(),
            },
            CONTEXT,
        ),
        KeyBinding::new(
            &translate("cmd-u"),
            AddAdjustment {
                kind: "hue_saturation".into(),
            },
            CONTEXT,
        ),
        KeyBinding::new(
            &translate("cmd-i"),
            AddAdjustment {
                kind: "invert".into(),
            },
            CONTEXT,
        ),
    ]);
    // Digit keys -> tool opacity (1 = 10% … 0 = 100%).
    for digit in 0..=9u32 {
        let percent = if digit == 0 { 100 } else { digit * 10 };
        bindings.push(KeyBinding::new(
            &digit.to_string(),
            SetToolOpacity { percent },
            TYPING_SAFE,
        ));
    }

    // User overrides: ~/.config/photoslop/keymap.json
    // Format: { "<keystroke>": "command:<id>" | "tool:<id>" }
    if let Some(user) = load_user_keymap() {
        for (keystroke, target) in user {
            if let Some(id) = target.strip_prefix("command:") {
                bindings.push(KeyBinding::new(
                    &keystroke,
                    RunCommand { id: id.to_string() },
                    CONTEXT,
                ));
            } else if let Some(id) = target.strip_prefix("tool:") {
                bindings.push(KeyBinding::new(
                    &keystroke,
                    ActivateTool { id: id.to_string() },
                    CONTEXT,
                ));
            } else {
                log::warn!("keymap: unknown target {target:?} for {keystroke:?}");
            }
        }
    }
    bindings
}

/// Where user keybinding overrides live.
pub fn user_keymap_path() -> Option<PathBuf> {
    Some(dirs_config()?.join("photoslop/keymap.json"))
}

fn load_user_keymap() -> Option<Vec<(String, String)>> {
    let path = user_keymap_path()?;
    let text = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str::<std::collections::BTreeMap<String, String>>(&text) {
        Ok(map) => Some(map.into_iter().collect()),
        Err(err) => {
            log::error!("invalid user keymap: {err}");
            None
        }
    }
}

fn dirs_config() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(dir));
    }
    std::env::var("HOME")
        .ok()
        .map(|h| PathBuf::from(h).join(".config"))
}

pub fn open_file_dialog(_ws: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let rx = cx.prompt_for_paths(PathPromptOptions {
        files: true,
        directories: false,
        multiple: false,
        prompt: Some("Open".into()),
    });
    cx.spawn_in(window, async move |this, cx| {
        if let Ok(Ok(Some(mut paths))) = rx.await {
            if let Some(path) = paths.pop() {
                this.update_in(cx, |ws, _window, cx| ws.load_file(path, cx))
                    .ok();
            }
        }
    })
    .detach();
}

/// Pick a `.wasm` plugin to install.
pub fn install_plugin_dialog(
    _ws: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let rx = cx.prompt_for_paths(PathPromptOptions {
        files: true,
        directories: false,
        multiple: false,
        prompt: Some("Install Plugin".into()),
    });
    cx.spawn_in(window, async move |this, cx| {
        if let Ok(Ok(Some(mut paths))) = rx.await {
            if let Some(path) = paths.pop() {
                this.update_in(cx, |ws, _window, cx| ws.install_plugin(path, cx))
                    .ok();
            }
        }
    })
    .detach();
}

pub fn save_file_dialog(ws: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let dir = ws
        .doc
        .as_ref()
        .and_then(|d| d.path.as_ref())
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .or_else(|| std::env::var("HOME").ok().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."));
    // PSD is the native save format (PLAN.md §2); keep an existing
    // extension when the document already has a writable one.
    let suggested = ws
        .doc
        .as_ref()
        .map(|d| {
            let stem = std::path::Path::new(&d.title)
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "untitled".into());
            let ext = d
                .path
                .as_ref()
                .and_then(|p| p.extension())
                .and_then(|e| e.to_str())
                .filter(|e| ["psd", "psb", "png", "jpg", "jpeg", "webp", "tif", "tiff"].contains(e))
                .unwrap_or("psd");
            format!("{stem}.{ext}")
        })
        .unwrap_or_else(|| "untitled.psd".into());
    let rx = cx.prompt_for_new_path(&dir, Some(&suggested));
    cx.spawn_in(window, async move |this, cx| {
        if let Ok(Ok(Some(path))) = rx.await {
            this.update_in(cx, |ws, _window, cx| ws.save_file_as(path, cx))
                .ok();
        }
    })
    .detach();
}
