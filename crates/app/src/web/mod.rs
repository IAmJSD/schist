//! What the browser build stands in for the operating system.
//!
//! Four jobs live here, all of them small and all of them load-bearing:
//!
//! * **The boot payload.** The loading page (`web/loader.js`) fetches the
//!   wasm chunks, the icons and the fonts with one progress bar, parks the
//!   asset bytes on `window.__schist_boot`, and only then instantiates the
//!   module. This module reads that payload back once, so everything after
//!   `main` starts is synchronous — no async asset plumbing in the app.
//! * **Files.** A browser has no paths, but the whole open/save flow is
//!   built on them, so picked files land in an in-memory map under
//!   invented paths (`/web/open/1/moss.psd`) and reads consult the map.
//!   Saves run the codec as ever and hand the bytes to the browser as a
//!   download.
//! * **Fetching.** Model downloads go through `fetch` with streamed
//!   progress, since there is no thread to park a blocking read on.
//! * **The loading page handoff.** `loading_done` fades the overlay out
//!   once the window exists; `loading_failed` turns it into an error
//!   card instead of leaving a full bar sitting there forever.

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast as _, JsValue};

/// Everything the loading page fetched before instantiating the module.
struct Boot {
    /// Asset path (`icons/brush.svg`) to file bytes.
    assets: HashMap<String, Vec<u8>>,
    /// Font files, registered with both text systems at startup.
    fonts: Vec<Vec<u8>>,
}

fn boot() -> &'static Boot {
    static BOOT: OnceLock<Boot> = OnceLock::new();
    BOOT.get_or_init(|| {
        let mut boot = Boot {
            assets: HashMap::new(),
            fonts: Vec::new(),
        };
        let Some(window) = web_sys::window() else {
            return boot;
        };
        let Ok(payload) = js_sys::Reflect::get(&window, &"__schist_boot".into()) else {
            return boot;
        };
        if payload.is_undefined() || payload.is_null() {
            log::error!("no __schist_boot payload; was the app served without its loader?");
            return boot;
        }
        let bytes_of = |v: &JsValue| -> Option<Vec<u8>> {
            v.dyn_ref::<js_sys::Uint8Array>().map(|a| a.to_vec())
        };
        if let Ok(assets) = js_sys::Reflect::get(&payload, &"assets".into()) {
            for key in js_sys::Object::keys(assets.unchecked_ref::<js_sys::Object>()).iter() {
                let Some(name) = key.as_string() else {
                    continue;
                };
                let Ok(value) = js_sys::Reflect::get(&assets, &key) else {
                    continue;
                };
                if let Some(bytes) = bytes_of(&value) {
                    boot.assets.insert(name, bytes);
                }
            }
        }
        if let Ok(fonts) = js_sys::Reflect::get(&payload, &"fonts".into()) {
            if let Some(fonts) = fonts.dyn_ref::<js_sys::Array>() {
                boot.fonts.extend(fonts.iter().filter_map(|f| bytes_of(&f)));
            }
        }
        boot
    })
}

/// A fetched asset's bytes, for the [`gpui::AssetSource`] in
/// `crate::assets`.
pub fn asset(path: &str) -> Option<&'static [u8]> {
    boot().assets.get(path).map(|b| b.as_slice())
}

/// Register the fetched fonts with the text engine (the type tool's side).
/// gpui's own text system gets the same faces inside `run`, where a
/// context exists to hand them to.
pub fn install_fonts() {
    for bytes in &boot().fonts {
        schist_text_engine::add_font_data(bytes.clone());
    }
}

/// The fetched fonts again, in the shape `TextSystem::add_fonts` takes.
pub fn font_faces() -> Vec<Cow<'static, [u8]>> {
    boot()
        .fonts
        .iter()
        .map(|b| Cow::Borrowed(b.as_slice()))
        .collect()
}

/// Tell the loading page the window is up. It owns the fade-out; the
/// fallback removal covers a page that embeds the app without the loader.
pub fn loading_done() {
    call_loader("__schistLoadingDone", None);
}

/// Turn the loading page into an error card. Wired into the panic hook,
/// so "it crashed" never looks like "it is still loading".
pub fn loading_failed(message: &str) {
    call_loader("__schistLoadingFailed", Some(message));
}

fn call_loader(name: &str, message: Option<&str>) {
    let Some(window) = web_sys::window() else {
        return;
    };
    if let Ok(hook) = js_sys::Reflect::get(&window, &name.into()) {
        if let Some(hook) = hook.dyn_ref::<js_sys::Function>() {
            let _ = match message {
                Some(msg) => hook.call1(&JsValue::NULL, &msg.into()),
                None => hook.call0(&JsValue::NULL),
            };
            return;
        }
    }
    // No loader on the page: at least get the overlay out of the way.
    if let Some(el) = window
        .document()
        .and_then(|d| d.get_element_by_id("schist-loading"))
    {
        el.remove();
    }
}

/// The in-memory files behind every path the browser build hands out.
mod files {
    use super::*;

    fn map() -> &'static Mutex<HashMap<PathBuf, Arc<Vec<u8>>>> {
        static FILES: OnceLock<Mutex<HashMap<PathBuf, Arc<Vec<u8>>>>> = OnceLock::new();
        FILES.get_or_init(Default::default)
    }

    /// Park `bytes` under a fresh path carrying `name`. The counter keeps
    /// two picks of files with one name apart — their edit histories are
    /// different documents.
    pub fn store(name: &str, bytes: Vec<u8>) -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(1);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = PathBuf::from(format!("/web/open/{n}")).join(name);
        if let Ok(mut map) = map().lock() {
            map.insert(path.clone(), Arc::new(bytes));
        }
        path
    }

    /// Overwrite (or create) the file at exactly `path` — what a save
    /// does, so a re-opened path reads back what was last written.
    pub fn write(path: &Path, bytes: Vec<u8>) {
        if let Ok(mut map) = map().lock() {
            map.insert(path.to_path_buf(), Arc::new(bytes));
        }
    }

    pub fn read(path: &Path) -> anyhow::Result<Arc<Vec<u8>>> {
        map()
            .lock()
            .ok()
            .and_then(|map| map.get(path).cloned())
            .ok_or_else(|| anyhow::anyhow!("no such file: {}", path.display()))
    }
}

pub use files::{read as read_file, write as write_file};

/// Ask the user for a file via a transient `<input type=file>`.
///
/// Resolves to the invented path the picked file was stored under, or
/// `None` when the picker is dismissed. The receiver also just goes quiet
/// on browsers that fire no `cancel` event; the caller's task leaks
/// nothing worse than itself.
pub fn pick_file(accept: &str) -> futures::channel::oneshot::Receiver<Option<PathBuf>> {
    let (tx, rx) = futures::channel::oneshot::channel();
    let Some(document) = web_sys::window().and_then(|w| w.document()) else {
        return rx;
    };
    let Ok(input) = document.create_element("input") else {
        return rx;
    };
    let Ok(input) = input.dyn_into::<web_sys::HtmlInputElement>() else {
        return rx;
    };
    input.set_type("file");
    input.set_accept(accept);
    let _ = input.style().set_property("display", "none");
    if let Some(body) = document.body() {
        let _ = body.append_child(&input);
    }

    let tx = std::rc::Rc::new(std::cell::RefCell::new(Some(tx)));
    let on_change = Closure::<dyn FnMut(web_sys::Event)>::new({
        let tx = tx.clone();
        move |event: web_sys::Event| {
            let Some(input) = event
                .target()
                .and_then(|t| t.dyn_into::<web_sys::HtmlInputElement>().ok())
            else {
                return;
            };
            input.remove();
            let Some(file) = input.files().and_then(|files| files.item(0)) else {
                if let Some(tx) = tx.borrow_mut().take() {
                    let _ = tx.send(None);
                }
                return;
            };
            let name = file.name();
            let tx = tx.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let sent = match wasm_bindgen_futures::JsFuture::from(file.array_buffer()).await {
                    Ok(buffer) => {
                        let bytes = js_sys::Uint8Array::new(&buffer).to_vec();
                        Some(files::store(&name, bytes))
                    }
                    Err(_) => None,
                };
                if let Some(tx) = tx.borrow_mut().take() {
                    let _ = tx.send(sent);
                }
            });
        }
    });
    let on_cancel = Closure::<dyn FnMut(web_sys::Event)>::new({
        let tx = tx.clone();
        move |event: web_sys::Event| {
            if let Some(input) = event
                .target()
                .and_then(|t| t.dyn_into::<web_sys::HtmlInputElement>().ok())
            {
                input.remove();
            }
            if let Some(tx) = tx.borrow_mut().take() {
                let _ = tx.send(None);
            }
        }
    });
    let _ = input.add_event_listener_with_callback("change", on_change.as_ref().unchecked_ref());
    let _ = input.add_event_listener_with_callback("cancel", on_cancel.as_ref().unchecked_ref());
    // The closures outlive this frame on purpose; one picker's worth of
    // leak per dialog open, bounded by how often a person can click Open.
    on_change.forget();
    on_cancel.forget();
    input.click();
    rx
}

/// Hand `bytes` to the browser as a download named `file_name`.
pub fn download_bytes(file_name: &str, bytes: &[u8]) -> anyhow::Result<()> {
    let err = |what: &str| anyhow::anyhow!("browser download failed: {what}");
    let document = web_sys::window()
        .and_then(|w| w.document())
        .ok_or_else(|| err("no document"))?;
    let array = js_sys::Array::new();
    array.push(&js_sys::Uint8Array::from(bytes));
    let options = web_sys::BlobPropertyBag::new();
    options.set_type("application/octet-stream");
    let blob = web_sys::Blob::new_with_u8_array_sequence_and_options(&array, &options)
        .map_err(|_| err("Blob"))?;
    let url = web_sys::Url::create_object_url_with_blob(&blob).map_err(|_| err("object URL"))?;
    let anchor = document
        .create_element("a")
        .ok()
        .and_then(|a| a.dyn_into::<web_sys::HtmlAnchorElement>().ok())
        .ok_or_else(|| err("anchor"))?;
    anchor.set_href(&url);
    anchor.set_download(file_name);
    anchor.click();
    let _ = web_sys::Url::revoke_object_url(&url);
    Ok(())
}

/// `window.prompt`, for the one question a save still has to ask (what to
/// call the file) now that there is no path to pick.
pub fn prompt_string(message: &str, default: &str) -> Option<String> {
    web_sys::window()?
        .prompt_with_message_and_default(message, default)
        .ok()
        .flatten()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Fetch `url`, bumping `got` as the body streams in so a progress line
/// has something to say.
pub async fn fetch_bytes(url: String, got: Arc<AtomicU64>) -> Result<Vec<u8>, String> {
    let window = web_sys::window().ok_or("no window")?;
    let response = wasm_bindgen_futures::JsFuture::from(window.fetch_with_str(&url))
        .await
        .map_err(|_| format!("fetch failed: {url}"))?;
    let response: web_sys::Response = response.dyn_into().map_err(|_| "not a Response")?;
    if !response.ok() {
        return Err(format!("HTTP {} for {url}", response.status()));
    }
    let Some(body) = response.body() else {
        return Err("response has no body".into());
    };
    let reader = body
        .get_reader()
        .dyn_into::<web_sys::ReadableStreamDefaultReader>()
        .map_err(|_| "no stream reader")?;
    let mut bytes = Vec::new();
    loop {
        let chunk = wasm_bindgen_futures::JsFuture::from(reader.read())
            .await
            .map_err(|_| "read failed")?;
        let done = js_sys::Reflect::get(&chunk, &"done".into())
            .ok()
            .and_then(|d| d.as_bool())
            .unwrap_or(true);
        if done {
            break;
        }
        if let Ok(value) = js_sys::Reflect::get(&chunk, &"value".into()) {
            if let Some(array) = value.dyn_ref::<js_sys::Uint8Array>() {
                bytes.extend(array.to_vec());
                got.store(bytes.len() as u64, Ordering::Relaxed);
            }
        }
    }
    Ok(bytes)
}

/// Preferences persistence: localStorage stands in for the config file.
pub const PREFS_KEY: &str = "schist.preferences";

pub fn local_get(key: &str) -> Option<String> {
    web_sys::window()?
        .local_storage()
        .ok()??
        .get_item(key)
        .ok()?
}

pub fn local_set(key: &str, value: &str) {
    if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        let _ = storage.set_item(key, value);
    }
}
