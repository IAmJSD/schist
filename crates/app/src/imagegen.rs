//! Image generation: File ▸ Generate Images, and everything behind it.
//!
//! Compiled only with the `imagegen` feature. The whole feature lives
//! here — the account on disk, the `schist://ig-callback` handoff, the
//! dialog, and the [`Workspace`] methods that drive them — so that
//! turning the flag off removes it rather than leaving inert branches
//! scattered through the workspace.
//!
//! The protocol itself is [`schist_imagegen`]; this module is the part
//! that knows about GPUI, the config directory and layers.

use crate::ui;
use crate::workspace::{Modal, Popup, Workspace};
use gpui::{
    div, px, Context, InteractiveElement as _, IntoElement, MouseButton, ParentElement as _,
    SharedString, StatefulInteractiveElement as _, Styled as _,
};
use schist_core::{blit_rgba8, IntRect, Layer, LayerPath};
use schist_imagegen as ig;
use std::cell::RefCell;
use std::path::PathBuf;
use std::time::Duration;

/// How often the browser callback and the generation stream are looked in
/// on. Both are waits on something outside the process, so this is a
/// courtesy poll rather than anything the UI is blocked behind.
const POLL_MS: u64 = 200;

/// How long to sit on the "waiting for the browser" step before giving
/// up. Long enough to sign up for an account mid-flow.
const AUTH_TIMEOUT: Duration = Duration::from_secs(600);

/// Quiet period after the last edit before the live preview is asked for.
/// The protocol wants the preview refreshed "whenever they change"; this
/// is what keeps that from meaning once per keystroke.
const PREVIEW_DEBOUNCE_MS: u64 = 600;

// ===== the account on disk =====

/// Where the signed-in account is kept.
///
/// Beside `preferences.json` rather than inside it: this is a credential,
/// and it is written with an owner-only mode that the preferences do not
/// need and that a shared file could not be given without also hiding the
/// preferences from everything else.
fn account_path() -> Option<PathBuf> {
    Some(crate::workspace::config_dir()?.join("imagegen-account.json"))
}

fn load_account() -> Option<ig::Account> {
    let text = std::fs::read_to_string(account_path()?).ok()?;
    match serde_json::from_str(&text) {
        Ok(account) => Some(account),
        Err(err) => {
            log::warn!("ignoring an unreadable image generation account: {err}");
            None
        }
    }
}

/// Persist `account`, owner-readable only. Called after anything that may
/// have renewed the token, which is most of the protocol.
fn save_account(account: &ig::Account) {
    let Some(path) = account_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let Ok(json) = serde_json::to_string_pretty(account) else {
        return;
    };
    if let Err(err) = std::fs::write(&path, json) {
        log::error!("could not save the image generation account: {err}");
        return;
    }
    restrict(&path);
}

fn forget_account() {
    if let Some(path) = account_path() {
        let _ = std::fs::remove_file(path);
    }
}

/// Take a file down to owner-only. A no-op where the platform has no
/// concept of it.
fn restrict(path: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    let _ = path;
}

// ===== the schist://ig-callback handoff =====

/// One `schist://ig-callback?state=…&code=…`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Callback {
    pub state: String,
    pub code: String,
}

/// Pull the two parameters out of a callback URL, or `None` if this is
/// not one.
pub fn parse_callback(url: &str) -> Option<Callback> {
    let rest = url.strip_prefix("schist://ig-callback")?;
    // Both the bare path and a trailing slash are the same callback.
    let query = rest.strip_prefix("/?").or_else(|| rest.strip_prefix('?'))?;
    let mut state = None;
    let mut code = None;
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=')?;
        let value = percent_decode(value)?;
        match key {
            "state" => state = Some(value),
            "code" => code = Some(value),
            _ => {}
        }
    }
    Some(Callback {
        state: state?,
        code: code?,
    })
}

/// Percent-decoding for the two parameters, which are opaque tokens: any
/// escape that is not a full hex pair means this is not a URL we
/// understand, so the callback is declined rather than guessed at.
fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hex = bytes.get(i + 1..i + 3)?;
                if !hex.iter().all(u8::is_ascii_hexdigit) {
                    return None;
                }
                out.push(u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// Where a callback waits to be picked up.
///
/// The browser hands the callback to whatever the OS has registered for
/// `schist://`, and on every platform but macOS that is a *new* Schist
/// process rather than the one sitting on the dialog. Passing it through
/// a file means both cases work the same way and neither needs Schist to
/// run a socket: the process that receives the URL drops it here and
/// exits, and the process that is waiting picks it up.
///
/// `XDG_RUNTIME_DIR` when there is one, since it is the directory the
/// system already guarantees is private; the config directory otherwise.
fn callback_path() -> Option<PathBuf> {
    let dir = std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .map(PathBuf::from)
        .or_else(crate::workspace::config_dir)?;
    Some(dir.join("schist-ig-callback.json"))
}

/// Drop a callback URL for the waiting process. Returns whether `url` was
/// a callback at all.
pub fn deliver_callback(url: &str) -> bool {
    let Some(callback) = parse_callback(url) else {
        return false;
    };
    let Some(path) = callback_path() else {
        return true;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(json) = serde_json::to_string(&callback) {
        if std::fs::write(&path, json).is_ok() {
            restrict(&path);
        }
    }
    true
}

/// Take the waiting callback, if there is one. It is consumed: a code is
/// good once, and leaving it on disk would make the next sign-in replay it.
fn take_callback() -> Option<Callback> {
    let path = callback_path()?;
    let text = std::fs::read_to_string(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    serde_json::from_str(&text).ok()
}

/// Hand `url` to the platform's browser.
fn open_in_browser(url: &str) -> std::io::Result<()> {
    use std::process::{Command, Stdio};
    let mut command = if cfg!(target_os = "macos") {
        let mut c = Command::new("open");
        c.arg(url);
        c
    } else if cfg!(target_os = "windows") {
        // `start` is a shell builtin, and its first quoted argument is
        // the window title -- hence the empty one before the URL.
        let mut c = Command::new("cmd");
        c.args(["/C", "start", "", url]);
        c
    } else {
        let mut c = Command::new("xdg-open");
        c.arg(url);
        c
    };
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(())
}

// ===== dialog state =====

/// Which step of the flow the dialog is showing.
#[derive(Debug, Clone, PartialEq)]
pub enum Stage {
    /// Nobody is signed in: asking which provider to use.
    SignIn,
    /// The browser is open and the callback has not arrived.
    Waiting {
        state: String,
        code_exchange_url: String,
    },
    /// Signed in, with the provider's form to fill in. The form itself
    /// is on the [`Dialog`], since a generation has to be able to come
    /// back to it.
    Form,
    /// A generation is streaming.
    Generating {
        /// Part names and slot counts, for the progress readout.
        layout: Vec<ig::LayoutPart>,
        /// Slots that have had their terminal status.
        done: usize,
        /// Total slots across the whole layout.
        total: usize,
        /// Finished images so far, which can exceed `total`.
        images: usize,
        /// Slots the provider refused, and why.
        rejected: Vec<(usize, String)>,
    },
}

/// The provider's form and what has been entered into it.
///
/// Kept across a generation rather than inside [`Stage::Form`]: a
/// generation that fails, or one the user wants to run again with a word
/// changed, comes straight back to the form it started from instead of
/// re-fetching it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Form {
    pub items: Vec<ig::FormItem>,
    pub values: ig::FormValues,
    /// The text the provider last rendered for a `live_text_preview`
    /// item, if the form has one and it has answered.
    pub preview: Option<String>,
    /// Set when a value changed and the preview has not caught up.
    preview_dirty: bool,
    /// Set while a preview request is out, so edits queue instead of
    /// piling requests on the provider.
    preview_pending: bool,
}

/// Everything the dialog is showing.
#[derive(Debug, Clone, PartialEq)]
pub struct Dialog {
    /// The provider's domain, as typed. Seeded with
    /// [`ig::DEFAULT_DOMAIN`] and editable until sign-in.
    pub domain: String,
    pub stage: Stage,
    /// The line under the title: what is happening, or what went wrong.
    pub status: SharedString,
    /// Set while a request is out, so the buttons can stand down.
    pub busy: bool,
    pub form: Form,
}

impl Dialog {
    fn new(domain: String, stage: Stage) -> Self {
        Dialog {
            domain,
            stage,
            status: SharedString::default(),
            busy: false,
            form: Form::default(),
        }
    }
}

/// The parts of the feature that outlive the dialog.
#[derive(Default)]
pub struct Session {
    /// The signed-in account, once read from disk.
    account: Option<ig::Account>,
    /// Whether the read has happened. Distinguishes "nobody is signed in"
    /// from "not looked yet", which otherwise both read as `None`.
    loaded: bool,
}

/// A stable `&'static str` id for the form field at `index`.
///
/// Focus and dropdown popups are keyed on `&'static str` ids, which every
/// built-in dialog has as a literal. This form comes from the provider,
/// so its ids are interned here instead: one leak the first time an index
/// is used, reused by every form afterwards. GPUI renders on one thread,
/// so the table needs no lock.
pub fn field_id(index: usize) -> &'static str {
    thread_local! {
        static IDS: RefCell<Vec<&'static str>> = const { RefCell::new(Vec::new()) };
    }
    IDS.with(|ids| {
        let mut ids = ids.borrow_mut();
        while ids.len() <= index {
            let id = format!("ig-field-{}", ids.len()).into_boxed_str();
            ids.push(Box::leak(id));
        }
        ids[index]
    })
}

/// The index [`field_id`] made this id for, or `None` for anything else.
pub fn field_index(id: &str) -> Option<usize> {
    id.strip_prefix("ig-field-")?.parse().ok()
}

/// A generated image, decoded and ready to become a layer.
struct DecodedImage {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

/// One layout part's images, decoded.
struct DecodedPart {
    part_name: String,
    images: Vec<DecodedImage>,
}

/// What the thread draining a generation sends back.
enum GenMsg {
    Event(ig::GenEvent),
    /// The drain is over: the account as it now stands (the token may
    /// have been renewed) and what came of it. `Ok(None)` means the
    /// dialog asked it to stop.
    Done(Box<ig::Account>, ig::Result<Option<Vec<ig::GeneratedPart>>>),
}

impl Workspace {
    /// File ▸ Generate Images.
    pub fn open_image_gen(&mut self, cx: &mut Context<Self>) {
        if !self.imagegen.loaded {
            self.imagegen.account = load_account();
            self.imagegen.loaded = true;
        }
        match &self.imagegen.account {
            Some(account) => {
                let domain = account.domain.clone();
                let mut dialog = Dialog::new(domain, Stage::Form);
                dialog.busy = true;
                dialog.status = "Asking the provider what to ask you\u{2026}".into();
                self.open_modal(Modal::ImageGen(Box::new(dialog)), cx);
                self.image_gen_fetch_form(cx);
            }
            None => {
                let dialog = Dialog::new(ig::DEFAULT_DOMAIN.to_string(), Stage::SignIn);
                self.open_modal(Modal::ImageGen(Box::new(dialog)), cx);
            }
        }
    }

    /// Run `f` over the open generation dialog, if that is what is open.
    pub fn with_image_gen(&mut self, f: impl FnOnce(&mut Dialog)) {
        self.update_modal(|modal| {
            if let Modal::ImageGen(dialog) = modal {
                f(dialog);
            }
        });
    }

    fn image_gen_dialog(&self) -> Option<&Dialog> {
        match &self.modal {
            Some(Modal::ImageGen(dialog)) => Some(dialog),
            _ => None,
        }
    }

    /// Show `message` on the dialog and let the buttons go again.
    ///
    /// A generation that ended badly goes back to the form it came from:
    /// the progress readout has nothing left to say, and everything the
    /// user typed is still there to try again with.
    fn image_gen_failed(&mut self, message: String, cx: &mut Context<Self>) {
        log::error!("image generation: {message}");
        self.with_image_gen(|dialog| {
            if matches!(dialog.stage, Stage::Generating { .. }) {
                dialog.stage = Stage::Form;
            }
            dialog.busy = false;
            dialog.status = message.into();
        });
        cx.notify();
    }

    /// The account is finished — the provider refused the token and a
    /// refresh could not save it. Drop it and go back to signing in.
    fn image_gen_signed_out(&mut self, message: String, cx: &mut Context<Self>) {
        self.imagegen.account = None;
        forget_account();
        let domain = self
            .image_gen_dialog()
            .map(|d| d.domain.clone())
            .unwrap_or_else(|| ig::DEFAULT_DOMAIN.to_string());
        self.with_image_gen(|dialog| {
            dialog.domain = domain;
            dialog.stage = Stage::SignIn;
            dialog.busy = false;
            dialog.status = message.into();
        });
        cx.notify();
    }

    /// Route a protocol failure: an unusable token ends the session,
    /// anything else is just a message.
    fn image_gen_error(&mut self, err: ig::Error, cx: &mut Context<Self>) {
        match err {
            ig::Error::Unauthorized => {
                self.image_gen_signed_out("Signed out by the provider. Sign in again.".into(), cx)
            }
            other => self.image_gen_failed(other.to_string(), cx),
        }
    }

    // --- signing in ---

    /// Ask the domain where its browser flow is, and open it.
    pub fn image_gen_connect(&mut self, cx: &mut Context<Self>) {
        let Some(domain) = self.image_gen_dialog().map(|d| d.domain.clone()) else {
            return;
        };
        let state = ig::random_state();
        if state.is_empty() {
            self.image_gen_failed("this machine has no source of randomness".into(), cx);
            return;
        }
        self.with_image_gen(|dialog| {
            dialog.busy = true;
            dialog.status = "Contacting the provider\u{2026}".into();
        });
        cx.notify();

        cx.spawn(async move |this, cx| {
            let lookup = {
                let domain = domain.clone();
                let state = state.clone();
                cx.background_executor()
                    .spawn(async move { ig::auth_urls(&domain, &state) })
                    .await
            };
            this.update(cx, |ws, cx| match lookup {
                Ok(urls) => {
                    if let Err(err) = open_in_browser(&urls.authentication_url) {
                        ws.image_gen_failed(format!("could not open a browser: {err}"), cx);
                        return;
                    }
                    ws.with_image_gen(|dialog| {
                        dialog.busy = false;
                        dialog.status = "Finish signing in in your browser\u{2026}".into();
                        dialog.stage = Stage::Waiting {
                            state: state.clone(),
                            code_exchange_url: urls.code_exchange_url.clone(),
                        };
                    });
                    ws.image_gen_await_callback(cx);
                    cx.notify();
                }
                Err(err) => ws.image_gen_error(err, cx),
            })
            .ok();
        })
        .detach();
    }

    /// Watch for the browser flow's callback while the dialog waits.
    fn image_gen_await_callback(&mut self, cx: &mut Context<Self>) {
        // A callback left over from an abandoned attempt would be
        // redeemed against this one's state and fail; clear it first.
        let _ = take_callback();
        cx.spawn(async move |this, cx| {
            let start = std::time::Instant::now();
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(POLL_MS))
                    .await;
                let Some(callback) = take_callback() else {
                    // The dialog moving on, or closing, ends the watch.
                    let waiting = this
                        .read_with(cx, |ws, _| {
                            matches!(
                                ws.image_gen_dialog().map(|d| &d.stage),
                                Some(Stage::Waiting { .. })
                            )
                        })
                        .unwrap_or(false);
                    if !waiting {
                        return;
                    }
                    if start.elapsed() < AUTH_TIMEOUT {
                        continue;
                    }
                    this.update(cx, |ws, cx| {
                        ws.image_gen_failed("the browser never came back".into(), cx)
                    })
                    .ok();
                    return;
                };
                this.update(cx, |ws, cx| ws.image_gen_exchange(callback, cx))
                    .ok();
                return;
            }
        })
        .detach();
    }

    /// Redeem the code the browser came back with.
    fn image_gen_exchange(&mut self, callback: Callback, cx: &mut Context<Self>) {
        let Some((expected, code_exchange_url, domain)) =
            self.image_gen_dialog()
                .and_then(|dialog| match &dialog.stage {
                    Stage::Waiting {
                        state,
                        code_exchange_url,
                    } => Some((
                        state.clone(),
                        code_exchange_url.clone(),
                        dialog.domain.clone(),
                    )),
                    _ => None,
                })
        else {
            return;
        };
        // The state is what ties this code to the flow we started. A
        // mismatch means it belongs to something else on this machine.
        if callback.state != expected {
            self.image_gen_failed("the browser came back with the wrong state".into(), cx);
            return;
        }
        self.with_image_gen(|dialog| {
            dialog.busy = true;
            dialog.status = "Signing in\u{2026}".into();
        });
        cx.notify();

        cx.spawn(async move |this, cx| {
            let exchanged = {
                let url = code_exchange_url.clone();
                cx.background_executor()
                    .spawn(async move { ig::exchange_code(&url, &callback.code, &callback.state) })
                    .await
            };
            this.update(cx, |ws, cx| match exchanged {
                Ok(tokens) => {
                    let account = ig::Account {
                        domain,
                        code_exchange_url,
                        tokens,
                    };
                    save_account(&account);
                    ws.imagegen.account = Some(account);
                    ws.imagegen.loaded = true;
                    ws.with_image_gen(|dialog| {
                        dialog.busy = true;
                        dialog.status = "Asking the provider what to ask you\u{2026}".into();
                        dialog.stage = Stage::Form;
                        dialog.form = Form::default();
                    });
                    ws.image_gen_fetch_form(cx);
                    cx.notify();
                }
                Err(err) => ws.image_gen_error(err, cx),
            })
            .ok();
        })
        .detach();
    }

    /// Sign out: tell the provider, then forget the account either way.
    pub fn image_gen_sign_out(&mut self, cx: &mut Context<Self>) {
        if let Some(account) = self.imagegen.account.clone() {
            cx.background_executor()
                .spawn(async move {
                    if let Err(err) = ig::logout(&account) {
                        // The account is going regardless: a provider
                        // that will not hear it is not a reason to stay
                        // signed in locally.
                        log::warn!("logout was refused: {err}");
                    }
                })
                .detach();
        }
        self.image_gen_signed_out("Signed out.".into(), cx);
    }

    // --- the form ---

    /// Fetch the form the provider wants drawn.
    fn image_gen_fetch_form(&mut self, cx: &mut Context<Self>) {
        let Some(mut account) = self.imagegen.account.clone() else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let fetched = cx
                .background_executor()
                .spawn(async move {
                    let items = ig::generation_structure(&mut account);
                    (account, items)
                })
                .await;
            let (account, items) = fetched;
            this.update(cx, |ws, cx| {
                // The fetch may have renewed the token on the way.
                save_account(&account);
                ws.imagegen.account = Some(account);
                match items {
                    Ok(items) => {
                        let values = default_values(&items);
                        let has_preview = items
                            .iter()
                            .any(|i| matches!(i, ig::FormItem::LiveTextPreview { .. }));
                        ws.with_image_gen(|dialog| {
                            dialog.busy = false;
                            dialog.status = SharedString::default();
                            dialog.stage = Stage::Form;
                            dialog.form = Form {
                                items,
                                values,
                                preview: None,
                                preview_dirty: has_preview,
                                preview_pending: false,
                            };
                        });
                        if has_preview {
                            ws.image_gen_watch_preview(cx);
                        }
                        cx.notify();
                    }
                    Err(err) => ws.image_gen_error(err, cx),
                }
            })
            .ok();
        })
        .detach();
    }

    /// Keep the live text preview in step with the form.
    ///
    /// One watcher for the life of the form, rather than a request per
    /// edit: the protocol asks for the preview to follow the values, and
    /// a quiet period between the last keystroke and the request is what
    /// keeps that to one request per pause instead of one per character.
    fn image_gen_watch_preview(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(PREVIEW_DEBOUNCE_MS))
                .await;
            let Ok(work) = this.read_with(cx, |ws, _| {
                // The dialog closing, or signing out, is the end of the
                // watch; a generation running is only a pause in it.
                let dialog = ws.image_gen_dialog()?;
                if !matches!(dialog.stage, Stage::Form | Stage::Generating { .. }) {
                    return None;
                }
                let form = &dialog.form;
                if !form.preview_dirty || form.preview_pending {
                    return Some(None);
                }
                let url = form.items.iter().find_map(|item| match item {
                    ig::FormItem::LiveTextPreview { live_preview_url } => {
                        Some(live_preview_url.clone())
                    }
                    _ => None,
                })?;
                Some(Some((url, form.values.clone())))
            }) else {
                return;
            };
            let Some(work) = work else { return };
            let Some((url, values)) = work else { continue };

            let Some(mut account) = this
                .read_with(cx, |ws, _| ws.imagegen.account.clone())
                .ok()
                .flatten()
            else {
                continue;
            };
            if this
                .update(cx, |ws, _| {
                    ws.with_image_gen(|dialog| {
                        dialog.form.preview_dirty = false;
                        dialog.form.preview_pending = true;
                    })
                })
                .is_err()
            {
                return;
            }

            let (account, text) = cx
                .background_executor()
                .spawn(async move {
                    let text = ig::live_text_preview(&mut account, &url, &values);
                    (account, text)
                })
                .await;
            if this
                .update(cx, |ws, cx| {
                    save_account(&account);
                    ws.imagegen.account = Some(account);
                    match text {
                        Ok(text) => ws.with_image_gen(|dialog| {
                            dialog.form.preview = Some(text);
                            dialog.form.preview_pending = false;
                        }),
                        Err(err) => {
                            // A preview that will not render is not worth
                            // interrupting the form over.
                            log::warn!("live preview failed: {err}");
                            ws.with_image_gen(|dialog| dialog.form.preview_pending = false);
                        }
                    }
                    cx.notify();
                })
                .is_err()
            {
                return;
            }
        })
        .detach();
    }

    /// Set one text field's value, from the shared field editor.
    pub fn image_gen_set_text(&mut self, index: usize, text: String) {
        self.with_image_gen(|dialog| {
            let form = &mut dialog.form;
            let Some(id) = form.items.get(index).and_then(|item| item.id()) else {
                return;
            };
            form.values
                .insert(id.to_string(), ig::FieldValue::Text(text));
            form.preview_dirty = true;
        });
    }

    /// Pick, or unpick, one option of a select.
    pub fn image_gen_toggle_choice(&mut self, index: usize, choice: String, multiple: bool) {
        self.with_image_gen(|dialog| {
            let form = &mut dialog.form;
            let Some(id) = form.items.get(index).and_then(|item| item.id()) else {
                return;
            };
            let mut chosen = match form.values.get(id) {
                Some(ig::FieldValue::Choices(chosen)) => chosen.clone(),
                _ => Vec::new(),
            };
            if multiple {
                match chosen.iter().position(|c| *c == choice) {
                    Some(at) => {
                        chosen.remove(at);
                    }
                    None => chosen.push(choice),
                }
            } else {
                chosen = vec![choice];
            }
            form.values
                .insert(id.to_string(), ig::FieldValue::Choices(chosen));
            form.preview_dirty = true;
        });
    }

    // --- generating ---

    /// Start a generation from the form as it stands.
    pub fn image_gen_start(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.image_gen_dialog() else {
            return;
        };
        let values = dialog.form.values.clone();
        if let Some(title) = ig::first_missing_required(&dialog.form.items, &values) {
            let title = title.to_string();
            self.with_image_gen(|dialog| {
                dialog.status = format!("{title} needs a value.").into();
            });
            cx.notify();
            return;
        }
        let Some(mut account) = self.imagegen.account.clone() else {
            return;
        };
        self.with_image_gen(|dialog| {
            dialog.busy = true;
            dialog.status = "Starting\u{2026}".into();
            dialog.stage = Stage::Generating {
                layout: Vec::new(),
                done: 0,
                total: 0,
                images: 0,
                rejected: Vec::new(),
            };
        });
        cx.notify();

        // A dedicated thread rather than the background executor: this
        // one blocks on a socket for as long as the provider takes to
        // draw, and a pool thread parked that long is a pool thread the
        // rest of the app has lost.
        let (tx, rx) = std::sync::mpsc::channel::<GenMsg>();
        std::thread::spawn(move || {
            let events = tx.clone();
            let result = ig::generate(&mut account, &values, &mut |event| {
                // A closed channel is the dialog saying it has gone away,
                // and is what stops the drain.
                events.send(GenMsg::Event(event)).is_ok()
            });
            let _ = tx.send(GenMsg::Done(Box::new(account), result));
        });
        self.image_gen_drain(rx, cx);
    }

    /// Pump the generation's messages into the dialog until it ends.
    fn image_gen_drain(&mut self, rx: std::sync::mpsc::Receiver<GenMsg>, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(POLL_MS))
                    .await;
                let mut messages = Vec::new();
                let mut hung_up = false;
                loop {
                    match rx.try_recv() {
                        Ok(message) => messages.push(message),
                        Err(std::sync::mpsc::TryRecvError::Empty) => break,
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                            hung_up = true;
                            break;
                        }
                    }
                }
                let mut done_seen = false;
                let updated = this.update(cx, |ws, cx| {
                    // The dialog closing drops `rx` with this task, which
                    // is what tells the drain thread to stop.
                    if !matches!(
                        ws.image_gen_dialog().map(|d| &d.stage),
                        Some(Stage::Generating { .. })
                    ) {
                        return false;
                    }
                    for message in messages {
                        match message {
                            GenMsg::Event(event) => ws.image_gen_event(event),
                            GenMsg::Done(account, result) => {
                                done_seen = true;
                                ws.image_gen_finished(*account, result, cx);
                            }
                        }
                    }
                    cx.notify();
                    true
                });
                // The dialog going away ends the pump, and dropping `rx`
                // with it is what stops the thread on the other end.
                if !updated.unwrap_or(false) || done_seen {
                    return;
                }
                if hung_up {
                    // The thread ended without a verdict, which it only
                    // does by panicking -- it sends one on every path.
                    this.update(cx, |ws, cx| {
                        ws.image_gen_failed("the generation stopped unexpectedly".into(), cx)
                    })
                    .ok();
                    return;
                }
            }
        })
        .detach();
    }

    /// Fold one streamed event into the progress readout.
    fn image_gen_event(&mut self, event: ig::GenEvent) {
        self.with_image_gen(|dialog| {
            let Stage::Generating {
                layout,
                done,
                total,
                images,
                rejected,
            } = &mut dialog.stage
            else {
                return;
            };
            match event {
                ig::GenEvent::Layout(parts) => {
                    *total = parts.iter().map(|p| p.children_count as usize).sum();
                    *layout = parts;
                    dialog.status = format!("Generating {total} image(s)\u{2026}").into();
                }
                ig::GenEvent::Image {
                    image, complete, ..
                } => {
                    if image.is_some() {
                        *images += 1;
                    }
                    if complete {
                        *done += 1;
                    }
                }
                ig::GenEvent::Rejected { index, reason } => {
                    *done += 1;
                    rejected.push((index, reason));
                }
            }
        });
    }

    /// The stream ended. Decode what came back and turn it into layers.
    fn image_gen_finished(
        &mut self,
        account: ig::Account,
        result: ig::Result<Option<Vec<ig::GeneratedPart>>>,
        cx: &mut Context<Self>,
    ) {
        save_account(&account);
        self.imagegen.account = Some(account);
        let parts = match result {
            // Stopped because the dialog went away; there is nothing left
            // to put anywhere.
            Ok(None) => return,
            Ok(Some(parts)) => parts,
            Err(err) => return self.image_gen_error(err, cx),
        };
        let rejections: Vec<String> = parts
            .iter()
            .flat_map(|p| p.children.iter())
            .filter_map(|c| c.rejected.clone())
            .collect();
        if parts
            .iter()
            .all(|p| p.children.iter().all(|c| c.images.is_empty()))
        {
            let message = match rejections.first() {
                Some(reason) => format!("The provider generated nothing: {reason}"),
                None => "The provider generated nothing.".to_string(),
            };
            self.image_gen_failed(message, cx);
            return;
        }

        self.with_image_gen(|dialog| {
            dialog.busy = true;
            dialog.status = "Decoding\u{2026}".into();
        });
        cx.notify();

        let codecs = self.registry.shared_codecs();
        cx.spawn(async move |this, cx| {
            let decoded = cx
                .background_executor()
                .spawn(async move { decode_parts(&codecs, parts) })
                .await;
            this.update(cx, |ws, cx| {
                ws.image_gen_insert(decoded, rejections, cx);
            })
            .ok();
        })
        .detach();
    }

    /// Put the decoded images into the document as layers, in one edit.
    fn image_gen_insert(
        &mut self,
        parts: Vec<DecodedPart>,
        rejections: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        let total: usize = parts.iter().map(|p| p.images.len()).sum();
        if total == 0 {
            self.image_gen_failed("nothing that arrived could be decoded".into(), cx);
            return;
        }
        // Generating into nothing makes a document to generate into,
        // sized to the largest image so none of them is cropped.
        if self.doc.is_none() {
            let width = parts
                .iter()
                .flat_map(|p| p.images.iter())
                .map(|i| i.width)
                .max()
                .unwrap_or(1024);
            let height = parts
                .iter()
                .flat_map(|p| p.images.iter())
                .map(|i| i.height)
                .max()
                .unwrap_or(1024);
            self.create_document(
                "Generated",
                width,
                height,
                72.0,
                schist_color::ColorMode::Rgb,
                schist_color::Depth::Eight,
                crate::workspace::NewDocBackground::Transparent,
            );
        }
        let Some(doc) = self.doc.as_mut() else { return };

        let depth = doc.depth;
        let canvas = (doc.width as i32, doc.height as i32);
        let mut new_layers = Vec::new();
        for part in parts {
            let name = if part.part_name.trim().is_empty() {
                "Generated".to_string()
            } else {
                part.part_name.clone()
            };
            // A part that produced one image is that image; only a part
            // with several needs a group to hold them together.
            if part.images.len() == 1 {
                new_layers.push(raster_layer(&name, &part.images[0], depth, canvas));
                continue;
            }
            let mut group = Layer::new_group(name.clone());
            let children: Vec<Layer> = part
                .images
                .iter()
                .enumerate()
                .map(|(n, image)| raster_layer(&format!("{name} {}", n + 1), image, depth, canvas))
                .collect();
            if let schist_core::LayerKind::Group(g) = &mut group.kind {
                g.children = children;
            }
            new_layers.push(group);
        }

        // Above the active layer, as a place does.
        let mut insert_at = match doc.active_layer.and_then(|a| doc.tree.path_of(a)) {
            Some(mut path) => {
                *path.0.last_mut().unwrap() += 1;
                path
            }
            None => LayerPath(vec![doc.tree.layers.len()]),
        };
        let mut last = None;
        let mut edit = doc.begin_edit("Generate Images");
        for layer in new_layers {
            last = Some(edit.insert_layer(insert_at.clone(), layer));
            *insert_at.0.last_mut().unwrap() += 1;
        }
        edit.commit();
        doc.active_layer = last;

        let status = match rejections.len() {
            0 => format!("Generated {total} layer(s)."),
            n => format!("Generated {total} layer(s); the provider refused {n}."),
        };
        self.status = status.into();
        self.close_modal(cx);
        self.after_change(cx);
    }
}

/// A raster layer holding `image`, centred on the canvas like a paste.
fn raster_layer(
    name: &str,
    image: &DecodedImage,
    depth: schist_color::Depth,
    canvas: (i32, i32),
) -> Layer {
    let mut layer = Layer::new_raster(name);
    let dest = IntRect::from_xywh(
        (canvas.0 - image.width as i32) / 2,
        (canvas.1 - image.height as i32) / 2,
        image.width,
        image.height,
    );
    if let Some(raster) = layer.as_raster_mut() {
        blit_rgba8(&mut raster.tiles, depth, dest, &image.rgba);
    }
    layer
}

/// Decode every generated image, dropping the ones no codec can read.
///
/// The protocol never says what an image is encoded as, so the codec is
/// chosen by probing the bytes, exactly as an opened file's is.
fn decode_parts(
    codecs: &[std::sync::Arc<dyn schist_plugin_api::CodecPlugin>],
    parts: Vec<ig::GeneratedPart>,
) -> Vec<DecodedPart> {
    parts
        .into_iter()
        .map(|part| DecodedPart {
            part_name: part.part_name,
            images: part
                .children
                .into_iter()
                .flat_map(|child| child.images)
                .filter_map(|bytes| match decode_image(codecs, &bytes) {
                    Ok(image) => Some(image),
                    Err(err) => {
                        log::error!("a generated image could not be decoded: {err:#}");
                        None
                    }
                })
                .collect(),
        })
        // A part every one of whose slots was refused, or whose images
        // no codec could read, would otherwise become an empty group.
        .filter(|part: &DecodedPart| !part.images.is_empty())
        .collect()
}

fn decode_image(
    codecs: &[std::sync::Arc<dyn schist_plugin_api::CodecPlugin>],
    bytes: &[u8],
) -> anyhow::Result<DecodedImage> {
    let codec = codecs
        .iter()
        .find(|c| c.probe(bytes))
        .ok_or_else(|| anyhow::anyhow!("no codec recognises it"))?;
    let doc = codec.import(bytes)?;
    let rect = doc.canvas_rect();
    Ok(DecodedImage {
        width: rect.width() as u32,
        height: rect.height() as u32,
        rgba: schist_compositor::composite_region_rgba8(&doc, rect),
    })
}

/// An empty value for every input the form has, so a field the user never
/// touches is still sent (empty) rather than missing.
fn default_values(items: &[ig::FormItem]) -> ig::FormValues {
    let mut values = ig::FormValues::new();
    for item in items {
        match item {
            ig::FormItem::Text(t) => {
                values.insert(t.id.clone(), ig::FieldValue::Text(String::new()));
            }
            ig::FormItem::Select(s) => {
                values.insert(s.id.clone(), ig::FieldValue::Choices(Vec::new()));
            }
            ig::FormItem::LiveTextPreview { .. } => {}
        }
    }
    values
}

// ===== rendering =====

/// Draw whichever step of the flow the dialog is on.
pub fn render(
    state: &crate::dialogs::DialogState,
    dialog: Dialog,
    cx: &mut Context<Workspace>,
) -> gpui::AnyElement {
    let status = dialog.status.clone();
    let busy = dialog.busy;
    let (title, width, body, actions) = match &dialog.stage {
        Stage::SignIn => (
            "Generate Images",
            420.0,
            sign_in_body(state, &dialog.domain, cx),
            sign_in_actions(busy, cx),
        ),
        Stage::Waiting { .. } => (
            "Generate Images",
            420.0,
            waiting_body(&dialog.domain),
            single_action("Cancel", cx),
        ),
        Stage::Form => (
            "Generate Images",
            480.0,
            form_body(state, &dialog.form, cx),
            form_actions(busy, cx),
        ),
        Stage::Generating {
            layout,
            done,
            total,
            images,
            rejected,
        } => (
            "Generating",
            420.0,
            generating_body(layout, *done, *total, *images, rejected),
            single_action("Cancel", cx),
        ),
    };
    let body = div()
        .flex()
        .flex_col()
        .gap_2()
        .child(body)
        .when(!status.is_empty(), |d| {
            d.child(
                div()
                    .text_size(px(11.0))
                    .text_color(gpui::rgb(ui::palette().text_dim))
                    .child(status),
            )
        });
    ui::modal_frame(title, width, body, actions).into_any_element()
}

fn sign_in_body(
    state: &crate::dialogs::DialogState,
    domain: &str,
    cx: &mut Context<Workspace>,
) -> gpui::AnyElement {
    div()
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .text_size(px(12.0))
                .text_color(gpui::rgb(ui::palette().text_dim))
                .child(
                    "Schist generates images through a provider you sign in to. \
                     Signing in opens your browser.",
                ),
        )
        .child(ui::field_row(
            "Provider",
            text_input("ig-domain", domain, 240.0, state, cx),
        ))
        .into_any_element()
}

fn sign_in_actions(busy: bool, cx: &mut Context<Workspace>) -> gpui::AnyElement {
    div()
        .flex()
        .flex_row()
        .gap_2()
        .child(ui::button(
            "Cancel",
            false,
            |ws, _window, cx| ws.close_modal(cx),
            cx,
        ))
        .child(ui::button(
            if busy {
                "Connecting\u{2026}"
            } else {
                "Sign In"
            },
            true,
            move |ws, _window, cx| {
                if !busy {
                    // Whatever is in the field is the domain, even if it
                    // was never committed with Enter.
                    ws.commit_focused_field();
                    ws.image_gen_connect(cx);
                }
            },
            cx,
        ))
        .into_any_element()
}

fn waiting_body(domain: &str) -> gpui::AnyElement {
    div()
        .text_size(px(12.0))
        .child(format!(
            "Finish signing in to {domain} in your browser. Schist picks up \
             where it leaves off; you can close the browser tab afterwards."
        ))
        .into_any_element()
}

fn form_body(
    state: &crate::dialogs::DialogState,
    form: &Form,
    cx: &mut Context<Workspace>,
) -> gpui::AnyElement {
    let Form {
        items,
        values,
        preview,
        ..
    } = form;
    if items.is_empty() {
        return div()
            .text_size(px(12.0))
            .text_color(gpui::rgb(ui::palette().text_dim))
            .child("Loading\u{2026}")
            .into_any_element();
    }
    let rows: Vec<gpui::AnyElement> = items
        .iter()
        .enumerate()
        .map(|(index, item)| match item {
            ig::FormItem::Text(text) => {
                let value = match values.get(&text.id) {
                    Some(ig::FieldValue::Text(v)) => v.as_str(),
                    _ => "",
                };
                labelled(
                    &text.title,
                    &text.description,
                    text.required,
                    text_input(field_id(index), value, 300.0, state, cx).into_any_element(),
                )
            }
            ig::FormItem::Select(select) => {
                let chosen = match values.get(&select.id) {
                    Some(ig::FieldValue::Choices(c)) => c.clone(),
                    _ => Vec::new(),
                };
                labelled(
                    &select.title,
                    &select.description,
                    select.required,
                    select_control(index, select, &chosen, state, cx),
                )
            }
            ig::FormItem::LiveTextPreview { .. } => preview_block(preview.as_deref()),
        })
        .collect();
    div()
        .id("ig-form")
        .flex()
        .flex_col()
        .gap_3()
        .max_h(px(380.0))
        .overflow_y_scroll()
        .children(rows)
        .into_any_element()
}

/// One form item: its title, its description, and whatever it is.
fn labelled(
    title: &str,
    description: &str,
    required: bool,
    control: gpui::AnyElement,
) -> gpui::AnyElement {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .flex()
                .flex_row()
                .gap_1()
                .text_size(px(12.0))
                .child(title.to_string())
                // Photoshop marks nothing; the protocol lets a provider
                // make a field mandatory, so the dialog has to say which.
                .when(required, |d| {
                    d.child(div().text_color(gpui::rgb(ui::palette().accent)).child("*"))
                }),
        )
        .when(!description.trim().is_empty(), |d| {
            d.child(
                div()
                    .text_size(px(11.0))
                    .text_color(gpui::rgb(ui::palette().text_dim))
                    .child(description.to_string()),
            )
        })
        .child(control)
        .into_any_element()
}

/// A single-choice select is a dropdown; a multiple-choice one is a list
/// of toggles, since a dropdown cannot show more than one answer.
fn select_control(
    index: usize,
    select: &ig::SelectBox,
    chosen: &[String],
    state: &crate::dialogs::DialogState,
    cx: &mut Context<Workspace>,
) -> gpui::AnyElement {
    if select.multiple {
        let rows: Vec<gpui::AnyElement> = select
            .values
            .iter()
            .map(|value| {
                let id = value.id.clone();
                ui::checkbox(
                    value.text.clone(),
                    chosen.contains(&value.id),
                    move |ws, _cx| ws.image_gen_toggle_choice(index, id.clone(), true),
                    cx,
                )
                .into_any_element()
            })
            .collect();
        return div()
            .flex()
            .flex_col()
            .gap_1()
            .children(rows)
            .into_any_element();
    }

    let id = field_id(index);
    let current = chosen.first().cloned().unwrap_or_default();
    let label = select
        .values
        .iter()
        .find(|v| v.id == current)
        .map(|v| v.text.clone())
        .unwrap_or_else(|| "Choose\u{2026}".to_string());
    ui::dropdown(
        ui::Dropdown {
            popup: Popup::Field(id),
            is_open: state.open_popup == Some(Popup::Field(id)),
            current,
            label: label.into(),
            width: 300.0,
            options: select
                .values
                .iter()
                .map(|v| (SharedString::from(v.text.clone()), v.id.clone()))
                .collect(),
        },
        move |ws, choice, _cx| ws.image_gen_toggle_choice(index, choice, false),
        cx,
    )
    .into_any_element()
}

/// The provider's rendering of what has been entered so far.
fn preview_block(preview: Option<&str>) -> gpui::AnyElement {
    div()
        .p_2()
        .rounded_sm()
        .bg(gpui::rgb(ui::palette().deep_bg))
        .border_1()
        .border_color(gpui::rgb(ui::palette().edge))
        .text_size(px(11.0))
        .text_color(gpui::rgb(if preview.is_some() {
            ui::palette().text
        } else {
            ui::palette().text_faint
        }))
        .child(preview.unwrap_or("\u{2026}").to_string())
        .into_any_element()
}

fn form_actions(busy: bool, cx: &mut Context<Workspace>) -> gpui::AnyElement {
    div()
        .flex()
        .flex_row()
        .gap_2()
        .child(ui::button(
            "Sign Out",
            false,
            |ws, _window, cx| ws.image_gen_sign_out(cx),
            cx,
        ))
        .child(ui::button(
            "Cancel",
            false,
            |ws, _window, cx| ws.close_modal(cx),
            cx,
        ))
        .child(ui::button(
            "Generate",
            true,
            move |ws, _window, cx| {
                if !busy {
                    ws.commit_focused_field();
                    ws.image_gen_start(cx);
                }
            },
            cx,
        ))
        .into_any_element()
}

fn generating_body(
    layout: &[ig::LayoutPart],
    done: usize,
    total: usize,
    images: usize,
    rejected: &[(usize, String)],
) -> gpui::AnyElement {
    let heading = if total == 0 {
        "Waiting for the provider\u{2026}".to_string()
    } else {
        format!("{done} of {total} finished, {images} image(s) so far")
    };
    let parts: Vec<gpui::AnyElement> = layout
        .iter()
        .map(|part| {
            div()
                .text_size(px(11.0))
                .text_color(gpui::rgb(ui::palette().text_dim))
                .child(format!(
                    "{} \u{2014} {} image(s)",
                    part.part_name, part.children_count
                ))
                .into_any_element()
        })
        .collect();
    let refusals: Vec<gpui::AnyElement> = rejected
        .iter()
        .map(|(index, reason)| {
            div()
                .text_size(px(11.0))
                .text_color(gpui::rgb(ui::palette().text_faint))
                .child(format!("Refused #{index}: {reason}"))
                .into_any_element()
        })
        .collect();
    div()
        .flex()
        .flex_col()
        .gap_2()
        .child(div().text_size(px(12.0)).child(heading))
        .child(progress_bar(done, total))
        .children(parts)
        .children(refusals)
        .into_any_element()
}

fn progress_bar(done: usize, total: usize) -> gpui::AnyElement {
    const WIDTH: f32 = 380.0;
    let ratio = if total == 0 {
        0.0
    } else {
        (done as f32 / total as f32).clamp(0.0, 1.0)
    };
    div()
        .w(px(WIDTH))
        .h(px(6.0))
        .rounded_sm()
        .bg(gpui::rgb(ui::palette().field_bg))
        .child(
            div()
                .h_full()
                .w(px(WIDTH * ratio))
                .rounded_sm()
                .bg(gpui::rgb(ui::palette().accent)),
        )
        .into_any_element()
}

fn single_action(label: &'static str, cx: &mut Context<Workspace>) -> gpui::AnyElement {
    div()
        .flex()
        .flex_row()
        .gap_2()
        .child(ui::button(
            label,
            false,
            |ws, _window, cx| ws.close_modal(cx),
            cx,
        ))
        .into_any_element()
}

/// A click-to-focus text field, as the layer and document name fields
/// are: GPUI ships no text input, and this dialog is not the place to
/// grow one.
fn text_input(
    id: &'static str,
    value: &str,
    width: f32,
    state: &crate::dialogs::DialogState,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let focused = state.focused_field == Some(id);
    let committed = value.to_string();
    let shown = if focused && !state.field_buffer.is_empty() {
        state.field_buffer.clone()
    } else {
        committed.clone()
    };
    div()
        .w(px(width))
        .h(px(22.0))
        .px_1()
        .flex()
        .items_center()
        .rounded_sm()
        .bg(gpui::rgb(ui::palette().field_bg))
        .border_1()
        .border_color(gpui::rgb(if focused {
            ui::palette().accent
        } else {
            ui::palette().field_bg
        }))
        .text_size(px(12.0))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |ws, _e, _w, cx| {
                ws.focus_field(id, committed.clone());
                cx.notify();
            }),
        )
        // A caret makes it obvious the field takes typing.
        .child(if focused { format!("{shown}|") } else { shown })
}

/// Take a committed field value if it belongs to this dialog.
///
/// The shared field editor keys on `&'static str` ids and dispatches by
/// matching them; these are the ones this module owns.
pub fn commit_field(ws: &mut Workspace, id: &str, buffer: String) -> bool {
    if id == "ig-domain" {
        ws.with_image_gen(|dialog| dialog.domain = buffer);
        return true;
    }
    let Some(index) = field_index(id) else {
        return false;
    };
    ws.image_gen_set_text(index, buffer);
    true
}

use gpui::prelude::FluentBuilder as _;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_callback_url_yields_its_state_and_code() {
        let callback = parse_callback("schist://ig-callback?state=abc&code=xyz").unwrap();
        assert_eq!(callback.state, "abc");
        assert_eq!(callback.code, "xyz");
        // Order is the provider's business, and a trailing slash before
        // the query is the same URL.
        let other = parse_callback("schist://ig-callback/?code=1&state=2").unwrap();
        assert_eq!((other.state.as_str(), other.code.as_str()), ("2", "1"));
    }

    #[test]
    fn callback_parameters_are_percent_decoded() {
        let callback =
            parse_callback("schist://ig-callback?state=a%2Fb&code=one%20two+three").unwrap();
        assert_eq!(callback.state, "a/b");
        assert_eq!(callback.code, "one two three");
    }

    #[test]
    fn anything_that_is_not_a_callback_is_declined() {
        assert!(parse_callback("file:///tmp/a.psd").is_none());
        assert!(parse_callback("schist://open?code=1&state=2").is_none());
        // Half a callback is not one: both parameters are required.
        assert!(parse_callback("schist://ig-callback?code=1").is_none());
        assert!(parse_callback("schist://ig-callback").is_none());
        // A truncated escape is malformed, not a literal '%'.
        assert!(parse_callback("schist://ig-callback?state=a%2&code=b").is_none());
    }

    #[test]
    fn field_ids_are_stable_and_round_trip() {
        // The same index has to give the same pointer every time, or a
        // focused field would lose focus on the next frame.
        assert!(std::ptr::eq(field_id(3), field_id(3)));
        assert_ne!(field_id(3), field_id(4));
        assert_eq!(field_index(field_id(7)), Some(7));
        assert_eq!(field_index("layer-name"), None);
        assert_eq!(field_index("ig-domain"), None);
    }

    #[test]
    fn every_input_starts_with_a_value() {
        let items: Vec<ig::FormItem> = serde_json::from_str(
            r#"[{"t":"text","title":"P","description":"","required":true,"id":"prompt"},
                {"t":"select","title":"S","description":"","required":false,"id":"style",
                 "multiple":false,"values":[]},
                {"t":"live_text_preview","live_preview_url":"https://schist.app/p"}]"#,
        )
        .unwrap();
        let values = default_values(&items);
        // The preview is not an input and contributes no key.
        assert_eq!(values.len(), 2);
        assert_eq!(
            values.get("prompt"),
            Some(&ig::FieldValue::Text(String::new()))
        );
        assert_eq!(
            values.get("style"),
            Some(&ig::FieldValue::Choices(Vec::new()))
        );
    }
}
