//! Schist — a plugin-first image editor on GPUI.

mod actions;
// The AI sidebar drives agent CLIs the user has installed; a browser tab
// can spawn no processes, so the whole subsystem stays off the web build
// (a stub keeps the bits of state the UI shares compiling).
#[cfg(not(target_arch = "wasm32"))]
mod ai;
#[cfg(target_arch = "wasm32")]
#[path = "ai_stub.rs"]
mod ai;
mod assets;
mod color_picker;
mod crash;
mod curve_editor;
mod dialogs;
// Dragging photos out of the window onto the desktop's file manager.
// Native-only: it is the platforms' own drag protocols, and a browser
// tab has no file manager to drop on.
#[cfg(not(target_arch = "wasm32"))]
mod drag_out;
mod fonts;
mod gallery;
mod keymap;
mod native_menu;
mod panels;
mod style_dialog;
mod ui;
#[cfg(not(target_arch = "wasm32"))]
mod update;
#[cfg(target_arch = "wasm32")]
#[path = "update_stub.rs"]
mod update;
// Linux renders through Vulkan and panics inside GPUI when there is no
// driver to render with. Nothing to check on macOS (Metal) or Windows.
#[cfg(target_os = "linux")]
mod vulkan;
// The browser build's shims: the in-memory file system behind open/save,
// the assets the loading page fetched, and the loading-page handoff.
#[cfg(target_arch = "wasm32")]
mod web;
mod workspace;

use actions::{HideApp, HideOthers, Quit, ShowAll};
use gpui::{
    px, size, App, AppContext as _, Application, AsyncApp, Bounds, TitlebarOptions, WindowBounds,
    WindowHandle, WindowOptions,
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

/// Whether an opt-in diagnostic is on: the preference, or the environment
/// variable that overrides it for one run.
#[cfg(not(target_arch = "wasm32"))]
fn opted_in(preference: bool, var: &str) -> bool {
    preference || std::env::var(var).is_ok_and(|v| v == "1")
}

/// Assemble the first-party plugin set. Every entry here is optional — the
/// app boots (to an empty shell) with any or all of them removed.
///
/// The two third-party hosts need what a browser tab lacks — a JIT for the
/// sandboxed wasm plugins, subprocesses and dlopen for the Photoshop ones —
/// so the web build assembles only the first-party registry.
#[cfg(not(target_arch = "wasm32"))]
type Hosts = (
    PluginRegistry,
    schist_plugin_host_wasm::PluginManager,
    schist_plugin_host_8bf::manager::PluginManager,
);
#[cfg(target_arch = "wasm32")]
type Hosts = PluginRegistry;

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
    #[cfg(not(target_arch = "wasm32"))]
    {
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
    #[cfg(target_arch = "wasm32")]
    registry
}

#[cfg(not(target_arch = "wasm32"))]
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
    #[cfg(not(target_arch = "wasm32"))]
    if let Some(callback) = std::env::args()
        .skip(1)
        .find(|arg| arg.starts_with("schist://"))
    {
        let dir = schist_gallery::state_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("schist/cloud");
        if let Err(error) = schist_cloud::auth::forward_callback(&callback, &dir) {
            eprintln!("Cloud sign-in callback failed: {error}");
            std::process::exit(1);
        }
        return;
    }

    // `schist --mcp-bridge <addr>` is not a GUI launch at all: it is the
    // stdio pump an agent harness spawns as its "MCP server", forwarding
    // into the running app's loopback endpoint. Handled before anything
    // else so no window, logger or driver probe gets in the way.
    #[cfg(not(target_arch = "wasm32"))]
    {
        let mut args = std::env::args().skip(1);
        if args.next().as_deref() == Some("--mcp-bridge") {
            let Some(addr) = args.next() else {
                eprintln!("usage: schist --mcp-bridge <addr>");
                std::process::exit(2);
            };
            ai::endpoint::run_bridge(&addr);
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    #[cfg(target_arch = "wasm32")]
    {
        // Panics land on the browser console instead of the void, and the
        // loading page is told so it can say something went wrong rather
        // than sit at a full progress bar forever.
        std::panic::set_hook(Box::new(|info| {
            console_error_panic_hook::hook(info);
            web::loading_failed(&info.to_string());
        }));
        console_log::init_with_level(log::Level::Info).ok();
    }
    // A Finder/desktop launch gets launchd's bare PATH, not the one the
    // AI panel's CLIs live on. Start asking the login shell for its PATH
    // now and collect the answer later: it is a shell startup, and the
    // window must not wait on it.
    #[cfg(not(target_arch = "wasm32"))]
    ai::path::start();
    // Before anything else: a system with no Vulkan driver cannot open a
    // window, and saying so plainly beats the panic that follows from
    // deep inside GPUI's renderer.
    #[cfg(target_os = "linux")]
    vulkan::check();
    // The browser exposes no fonts to scan, so the faces the loading page
    // fetched have to be registered before anything shapes text — the
    // text engine's for the type tool here, gpui's own inside `run`.
    #[cfg(target_arch = "wasm32")]
    web::install_fonts();
    let options = workspace::load_view_options();
    // Both diagnostics are opt-in, and separately so: writing a report to
    // this machine and sending one to ours are not the same decision.
    // SCHIST_CRASH_REPORTS=1 and SCHIST_CRASH_UPLOAD=1 turn them on for a
    // single run without touching preferences.
    //
    // Sentry goes first because it installs a panic hook and
    // `install_handler` chains in front of whichever hook it finds: the
    // local report is written before the upload is attempted, so a failing
    // network never costs the user the report on disk. `_reporter` has to
    // outlive the app -- dropping it is what flushes the queue.
    #[cfg(not(target_arch = "wasm32"))]
    let _reporter = crash::start_reporting(opted_in(options.crash_upload, "SCHIST_CRASH_UPLOAD"));
    #[cfg(not(target_arch = "wasm32"))]
    crash::install_handler(opted_in(options.crash_reports, "SCHIST_CRASH_REPORTS"));
    workspace::init_compositor_backend(options.gpu_compositing);
    #[cfg(not(target_arch = "wasm32"))]
    let (registry, plugin_manager, photoshop_plugins) = build_registry();
    #[cfg(target_arch = "wasm32")]
    let registry = build_registry();

    let requests: Rc<RefCell<OpenRequests>> = Rc::default();
    let app = Application::new().with_assets(assets::Assets);
    // Finder does not use argv: a double-clicked document arrives as an Apple
    // event, both at launch and while Schist is already running.
    app.on_open_urls({
        let requests = requests.clone();
        move |urls| {
            #[cfg(not(target_arch = "wasm32"))]
            for callback in urls.iter().filter(|url| url.starts_with("schist://")) {
                let dir = schist_gallery::state_dir()
                    .unwrap_or_else(std::env::temp_dir)
                    .join("schist/cloud");
                if let Err(error) = schist_cloud::auth::forward_callback(callback, &dir) {
                    log::error!("Cloud callback failed: {error}");
                }
            }
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
        // gpui's font database starts empty in a browser; feed it the same
        // faces the text engine got. Its default `.SystemUIFont` resolves
        // to "IBM Plex Sans" on the web backend, which the loading page
        // fetches for exactly that reason.
        #[cfg(target_arch = "wasm32")]
        {
            let faces = web::font_faces();
            if !faces.is_empty() {
                if let Err(err) = cx.text_system().add_fonts(faces) {
                    log::error!("failed to register fonts: {err:#}");
                }
            }
        }
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
                    titlebar: Some(TitlebarOptions {
                        title: Some("Schist".into()),
                        // AppKit keeps the traffic lights while allowing
                        // the workspace to paint a title bar matching its
                        // own light or dark theme.
                        appears_transparent: cfg!(target_os = "macos"),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                |_window, cx| {
                    cx.new(|cx| {
                        #[cfg(not(target_arch = "wasm32"))]
                        let mut ws =
                            Workspace::new(registry, plugin_manager, photoshop_plugins, cx);
                        #[cfg(target_arch = "wasm32")]
                        let ws = Workspace::new(registry, cx);
                        // Recovery runs whatever else is happening: opening
                        // a document from the shell or the file manager is
                        // not a reason to leave a previous session's
                        // unsaved work stranded on disk.
                        #[cfg(not(target_arch = "wasm32"))]
                        {
                            let recoveries = Workspace::pending_recoveries();
                            if !recoveries.is_empty() {
                                log::info!("recovering {} snapshot(s)", recoveries.len());
                                ws.recover_all(recoveries, cx);
                            }
                            if let Some(path) = std::env::args().nth(1) {
                                let path = path_from_url(&path).unwrap_or_else(|| path.into());
                                ws.load_file(path, cx);
                            } else if ws.tab_count() == 0 {
                                // Picasa boot: a launch with nothing to
                                // open lands in the gallery, empty or
                                // not. Recovered tabs win — unsaved
                                // work is more urgent than browsing.
                                // Through the toggle so the open does
                                // everything an interactive open does
                                // (rescan, camera discovery on macOS).
                                ws.toggle_gallery(cx);
                            }
                        }
                        ws
                    })
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
        // The window is open and will paint on the next animation frame;
        // fade the loading page out from over it.
        #[cfg(target_arch = "wasm32")]
        web::loading_done();
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
