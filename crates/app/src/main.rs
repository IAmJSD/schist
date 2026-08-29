//! Schist — a plugin-first image editor on GPUI.

mod actions;
mod assets;
mod color_picker;
mod crash;
mod curve_editor;
mod dialogs;
mod fonts;
mod gallery;
mod keymap;
mod native_menu;
mod panels;
mod style_dialog;
mod ui;
mod update;
mod workspace;

use actions::{HideApp, HideOthers, Quit, ShowAll};
use gpui::{
    px, size, App, AppContext as _, Application, AsyncApp, Bounds, WindowBounds, WindowHandle,
    WindowOptions,
};
use schist_plugin_api::{CodecPlugin, PluginManifest, PluginRegistry};
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use workspace::Workspace;

/// PSD/PSB import and export via `schist-codec-psd`.
struct PsdCodec;

impl CodecPlugin for PsdCodec {
    fn id(&self) -> &'static str {
        "codec.psd"
    }
    fn name(&self) -> &'static str {
        "Photoshop PSD"
    }
    fn extensions(&self) -> &'static [&'static str] {
        &["psd", "psb"]
    }
    fn probe(&self, bytes: &[u8]) -> bool {
        schist_codec_psd::is_psd(bytes)
    }
    fn import(&self, bytes: &[u8]) -> anyhow::Result<schist_core::Document> {
        Ok(schist_codec_psd::read_psd(bytes)?)
    }
    fn can_export(&self) -> bool {
        true
    }
    fn export(&self, doc: &schist_core::Document) -> anyhow::Result<Vec<u8>> {
        Ok(schist_codec_psd::write_psd(doc)?)
    }
}

struct PsdPlugin;

impl PluginManifest for PsdPlugin {
    fn id(&self) -> &'static str {
        "schist.codec-psd"
    }
    fn register(&self, registry: &mut PluginRegistry) {
        registry.register_codec(Box::new(PsdCodec));
    }
}

/// Crash reporting stays off unless the user opts in.
fn crash_reports_enabled() -> bool {
    if std::env::var("SCHIST_CRASH_REPORTS").is_ok_and(|v| v == "1") {
        return true;
    }
    std::env::var("XDG_CONFIG_HOME")
        .ok()
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| std::path::PathBuf::from(h).join(".config"))
        })
        .map(|d| d.join("schist/preferences.json"))
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|v| v.get("crash_reports").and_then(|b| b.as_bool()))
        .unwrap_or(false)
}

/// Assemble the first-party plugin set. Every entry here is optional — the
/// app boots (to an empty shell) with any or all of them removed.
type Hosts = (
    PluginRegistry,
    schist_plugin_host_wasm::PluginManager,
    schist_plugin_host_8bf::manager::PluginManager,
);

fn build_registry() -> Hosts {
    let mut registry = PluginRegistry::new();
    let manifests: Vec<Box<dyn PluginManifest>> = vec![
        Box::new(schist_tools_basic::BasicToolsPlugin),
        Box::new(schist_tools_paint::PaintToolsPlugin),
        Box::new(schist_tools_retouch::RetouchToolsPlugin),
        Box::new(schist_tools_warp::WarpToolsPlugin),
        Box::new(schist_tools_doc::DocToolsPlugin),
        Box::new(schist_tools_select::SelectToolsPlugin),
        Box::new(schist_tools_transform::TransformToolsPlugin),
        Box::new(schist_tools_vector::VectorToolsPlugin),
        Box::new(schist_tools_type::TypeToolsPlugin),
        Box::new(schist_commands_core::CoreCommandsPlugin),
        Box::new(schist_filters_core::CoreFiltersPlugin),
        Box::new(schist_codecs_common::CommonCodecsPlugin),
        Box::new(PsdPlugin),
    ];
    for manifest in manifests {
        log::info!("loading plugin {}", manifest.id());
        manifest.register(&mut registry);
    }
    // Third-party WebAssembly plugins, sandboxed.
    let manager = match schist_plugin_host_wasm::PluginManager::plugin_dir() {
        Some(dir) => schist_plugin_host_wasm::PluginManager::load_dir(&dir, &mut registry),
        None => schist_plugin_host_wasm::PluginManager::default(),
    };
    log_8bf_support();
    // Photoshop plug-ins, each run in a helper process. `Interactive::Yes`
    // because a `.8bf` carries its own dialog, and in the app there is
    // someone to answer it — which is the whole of its parameter UI.
    let photoshop = schist_plugin_host_8bf::manager::PluginManager::load_dirs(
        &schist_plugin_host_8bf::manager::PluginManager::search_dirs(),
        &mut registry,
        schist_plugin_host_8bf::manager::Interactive::Yes,
    );
    (registry, manager, photoshop)
}

/// Report which Photoshop plug-in helpers this build carries.
///
/// They are binaries for architectures other than this one, so they ride
/// inside the executable and are unpacked to the cache the first time a
/// plug-in actually needs one — which for most people is never. Nothing
/// is written here; this only says what is available, because that is
/// the first question when a `.8bf` will not load.
fn log_8bf_support() {
    use schist_plugin_host_8bf::bundled;
    let carried: Vec<&str> = bundled::names().collect();
    if carried.is_empty() {
        log::info!(
            "no Photoshop plug-in helpers bundled; .8bf support needs them \
             installed beside the binary"
        );
    } else {
        log::info!("Photoshop plug-in helpers carried: {}", carried.join(", "));
    }
}

/// Documents the platform hands over outside argv, and the window they are
/// destined for. The handler has to be installed before the app runs, so at
/// launch the window may not exist yet; anything that arrives that early
/// waits here until it does.
#[derive(Default)]
struct OpenRequests {
    window: Option<(AsyncApp, WindowHandle<Workspace>)>,
    queued: Vec<PathBuf>,
}

/// A `file://` URL as a local path, or `None` for anything that is not one.
///
/// macOS opens documents by handing the app percent-encoded URLs; every
/// other platform Schist is associated on passes a plain path on argv.
fn path_from_url(url: &str) -> Option<PathBuf> {
    let rest = url.strip_prefix("file://")?;
    // An empty authority and "localhost" both mean this machine; anything
    // else is a remote file we cannot open by path anyway.
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    if !rest.starts_with('/') {
        return None;
    }
    // "file:///C:/..." -- the drive letter, not the root, starts the path.
    // Only when a drive letter really follows: "file:///tmp/a.psb" is a
    // rooted path, and dropping its slash would silently make it relative.
    #[cfg(windows)]
    let rest = match rest.as_bytes() {
        [b'/', drive, b':', ..] if drive.is_ascii_alphabetic() => &rest[1..],
        _ => rest,
    };

    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            // A truncated or non-hex escape means this is not a URL we
            // understand, so decline it rather than guessing at the bytes.
            let hex = bytes.get(i + 1..i + 3)?;
            if !hex.iter().all(u8::is_ascii_hexdigit) {
                return None;
            }
            out.push(u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Some(PathBuf::from(String::from_utf8(out).ok()?))
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    // Opt-in: SCHIST_CRASH_REPORTS=1, or the preference file's
    // crash_reports flag once the user enables it in Preferences.
    crash::install_handler(crash_reports_enabled());
    workspace::init_compositor_backend(workspace::load_view_options().gpu_compositing);
    let (registry, plugin_manager, photoshop_plugins) = build_registry();

    let requests: Rc<RefCell<OpenRequests>> = Rc::default();
    let app = Application::new().with_assets(assets::Assets);
    // Finder does not use argv: a double-clicked document arrives as an Apple
    // event, both at launch and while Schist is already running.
    app.on_open_urls({
        let requests = requests.clone();
        move |urls| {
            let paths: Vec<PathBuf> = urls.iter().filter_map(|u| path_from_url(u)).collect();
            if paths.is_empty() {
                return;
            }
            let window = requests.borrow().window.clone();
            match window {
                Some((async_cx, window)) => {
                    let opened = async_cx.update(|cx| open_all(paths, window, cx));
                    if let Err(err) = opened {
                        log::error!("open failed: {err:#}");
                    }
                }
                None => requests.borrow_mut().queued.extend(paths),
            }
        }
    });

    app.run(move |cx: &mut App| {
        cx.bind_keys(keymap::build_bindings(&registry));
        // The macOS application menu, which is reachable with no window
        // open, so these are global rather than on the workspace.
        cx.on_action(|_: &HideApp, cx: &mut App| cx.hide());
        cx.on_action(|_: &HideOthers, cx: &mut App| cx.hide_other_apps());
        cx.on_action(|_: &ShowAll, cx: &mut App| cx.unhide_other_apps());
        let bounds = Bounds::centered(None, size(px(1440.0), px(900.0)), cx);
        let window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                |_window, cx| {
                    let workspace = cx.new(|cx| {
                        let mut ws =
                            Workspace::new(registry, plugin_manager, photoshop_plugins, cx);
                        // Recovery runs whatever else is happening: opening
                        // a document from the shell or the file manager is
                        // not a reason to leave a previous session's
                        // unsaved work stranded on disk.
                        let recoveries = Workspace::pending_recoveries();
                        if !recoveries.is_empty() {
                            log::info!("recovering {} snapshot(s)", recoveries.len());
                            ws.recover_all(recoveries, cx);
                        }
                        if let Some(path) = std::env::args().nth(1) {
                            ws.load_file(path.into(), cx);
                        }
                        ws
                    });
                    workspace
                },
            )
            .expect("failed to open window");

        // Quit routes through the workspace so unsaved documents get a
        // prompt. Registered here rather than before `open_window` because
        // it needs the window to ask. With no window there is nothing to
        // lose, so quitting outright is correct.
        cx.on_action(move |_: &Quit, cx: &mut App| {
            if window
                .update(cx, |ws, _window, cx| ws.request_quit(cx))
                .is_err()
            {
                cx.quit();
            }
        });
        // The platform close button and the window manager come through
        // here. The hook is synchronous, so a dirty workspace vetoes the
        // close and `request_quit` drives the prompts.
        let _ = window.update(cx, |_ws, win, cx| {
            win.on_window_should_close(cx, move |_win, cx| {
                window
                    .update(cx, |ws, _window, cx| {
                        if ws.first_dirty_tab().is_some() {
                            ws.request_quit(cx);
                            false
                        } else {
                            true
                        }
                    })
                    .unwrap_or(true)
            });
        });

        let queued = {
            let mut requests = requests.borrow_mut();
            requests.window = Some((cx.to_async(), window));
            std::mem::take(&mut requests.queued)
        };
        open_all(queued, window, cx);
        cx.activate(true);
        // Closing the last window ends the session. The X11, Wayland and
        // Windows backends already stop themselves once no window is left;
        // AppKit instead keeps a window-less app sitting in the dock, and
        // Schist has nothing to offer in that state — no menu item opens a
        // second window. Quitting from anywhere but macOS would panic
        // besides: `Platform::quit` is synchronous on Linux and re-enters
        // the client state this callback is already dispatching from.
        if cfg!(target_os = "macos") {
            cx.on_window_closed(|cx| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
        }
    });
}

fn open_all(paths: Vec<PathBuf>, window: WindowHandle<Workspace>, cx: &mut App) {
    for path in paths {
        if let Err(err) = window.update(cx, |ws, _window, cx| ws.load_file(path, cx)) {
            log::error!("open failed: {err:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::path_from_url;
    use std::path::PathBuf;

    #[test]
    fn finder_urls_become_paths() {
        assert_eq!(
            path_from_url("file:///Users/astrid/Pictures/moss.afphoto"),
            Some(PathBuf::from("/Users/astrid/Pictures/moss.afphoto"))
        );
        // Spaces and non-ASCII arrive percent-encoded.
        assert_eq!(
            path_from_url("file:///tmp/two%20words/gr%C3%BCn.afdesign"),
            Some(PathBuf::from("/tmp/two words/grün.afdesign"))
        );
        assert_eq!(
            path_from_url("file://localhost/tmp/a.psb"),
            Some(PathBuf::from("/tmp/a.psb"))
        );
    }

    // Explorer hands over "file:///C:/...", where the root belongs to the
    // drive rather than the path.
    #[cfg(windows)]
    #[test]
    fn drive_letters_lose_the_leading_slash() {
        assert_eq!(
            path_from_url("file:///C:/Users/astrid/Pictures/moss.afphoto"),
            Some(PathBuf::from(r"C:/Users/astrid/Pictures/moss.afphoto"))
        );
    }

    #[test]
    fn anything_that_is_not_a_local_file_is_declined() {
        assert_eq!(path_from_url("https://example.com/a.psd"), None);
        assert_eq!(path_from_url("schist://open"), None);
        assert_eq!(path_from_url("file://server/share/a.psd"), None);
        // A truncated escape is malformed, not a path with a stray '%'.
        assert_eq!(path_from_url("file:///tmp/a%2"), None);
    }
}
