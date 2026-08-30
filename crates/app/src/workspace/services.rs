//! Things fetched or installed from outside the app: neural models,
//! update checks, fonts, and plug-ins.

use super::*;

impl Workspace {
    /// Download a Neural Filters model and install it.
    ///
    /// Runs off the UI thread: these are megabytes over the network --
    /// sixty-six of them for the depth model -- and the window should
    /// stay usable while one arrives.
    pub fn download_model(&mut self, id: &'static str, cx: &mut Context<Self>) {
        let Some(spec) = schist_neural::spec(id) else {
            return;
        };
        let Some(url) = spec.url else { return };
        if self.model_downloads.iter().any(|d| d.id == id) {
            return;
        }
        let got = Arc::new(AtomicU64::new(0));
        self.model_downloads.push(ModelDownload {
            id,
            got: got.clone(),
        });
        self.status = format!("Downloading {}\u{2026}", spec.name).into();
        cx.notify();
        // A repaint every so often while it runs, so the dialog's count
        // climbs. The fetch itself cannot ask for one: it is on a
        // background thread and has no handle on the view.
        cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(250))
                .await;
            let running = this.update(cx, |ws, cx| {
                let running = ws.model_downloads.iter().any(|d| d.id == id);
                if running {
                    cx.notify();
                }
                running
            });
            if !matches!(running, Ok(true)) {
                break;
            }
        })
        .detach();
        cx.spawn(async move |this, cx| {
            let fetched = cx
                .background_executor()
                .spawn(async move { fetch_model(url, &got) })
                .await;
            this.update(cx, |ws, cx| {
                ws.model_downloads.retain(|d| d.id != id);
                let Some(spec) = schist_neural::spec(id) else {
                    return;
                };
                ws.status = match fetched.and_then(|bytes| {
                    schist_neural::install(spec, &bytes).map_err(|e| e.to_string())
                }) {
                    Ok(path) => format!("Installed {} to {}", spec.name, path.display()).into(),
                    Err(e) => format!("{}: {e}", spec.name).into(),
                };
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub fn remove_model(&mut self, id: &'static str, cx: &mut Context<Self>) {
        let Some(spec) = schist_neural::spec(id) else {
            return;
        };
        self.status = match schist_neural::uninstall(spec) {
            Ok(()) => format!("Removed {}", spec.name).into(),
            Err(e) => format!("{e}").into(),
        };
        cx.notify();
    }

    /// Ask upstream whether a newer release exists. Opt-in and
    /// user-initiated, like the font fetch — the app makes no other
    /// network requests.
    pub fn check_for_update(&mut self, cx: &mut Context<Self>) {
        self.status = "Checking for updates…".into();
        cx.notify();
        self.run_update_check(false, cx);
    }

    /// The launch-time check. Silent unless there is something to say:
    /// nobody opening a document wants to be told their editor is
    /// current, and a machine that is offline at login should not be
    /// shown a failure it never asked for.
    pub fn check_for_update_quietly(&mut self, cx: &mut Context<Self>) {
        self.run_update_check(true, cx);
    }

    pub(super) fn run_update_check(&mut self, quiet: bool, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let status = cx
                .background_executor()
                .spawn(async {
                    let status = crate::update::check();
                    // A check that never reached GitHub is not one, so a
                    // machine that was offline at launch tries again at
                    // the next one rather than in a day.
                    if !matches!(status, crate::update::UpdateStatus::Failed(_)) {
                        crate::update::mark_checked();
                    }
                    status
                })
                .await;
            this.update(cx, |ws, cx| {
                match status {
                    crate::update::UpdateStatus::Available(update) => {
                        log::info!("update {} available at {}", update.version, update.page);
                        ws.status = format!("Schist {} is available", update.version).into();
                        // The launch-time check lands five seconds in,
                        // by which time the user may be in a dialog of
                        // their own. Theirs wins; the status line still
                        // says an update is there, and File ▸ Check for
                        // Updates brings this back.
                        if !quiet || ws.modal.is_none() {
                            ws.open_modal(Modal::UpdateAvailable { update }, cx);
                        }
                    }
                    crate::update::UpdateStatus::UpToDate if !quiet => {
                        ws.status =
                            format!("Schist {} is up to date", crate::update::current_version())
                                .into();
                    }
                    crate::update::UpdateStatus::Failed(err) if !quiet => {
                        ws.status = format!("Update check failed: {err}").into();
                    }
                    _ => {}
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Download the release and install it over this copy, then quit so
    /// the relauncher can start the new build.
    ///
    /// The dialog stays up throughout: it is what shows the progress,
    /// and there is nothing useful to do in an editor whose executable
    /// is being replaced underneath it.
    pub fn start_update(&mut self, update: crate::update::Update, cx: &mut Context<Self>) {
        let Some(installer) = update.install.clone() else {
            return;
        };
        if self.update_progress.is_some() {
            return;
        }
        self.update_progress = Some(UpdateProgress::Downloading {
            received: 0,
            total: installer.size,
        });
        self.status = format!("Downloading Schist {}\u{2026}", update.version).into();
        cx.notify();

        // The download runs on a background thread and counts bytes into
        // this; the dialog reads it on a timer, rather than that thread
        // reaching into the entity once per 64 KiB.
        let received = Arc::new(std::sync::atomic::AtomicU64::new(0));
        cx.spawn({
            let received = received.clone();
            async move |this, cx| loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(200))
                    .await;
                let downloading = this.update(cx, |ws, cx| {
                    let Some(UpdateProgress::Downloading { received: got, .. }) =
                        ws.update_progress.as_mut()
                    else {
                        return false;
                    };
                    *got = received.load(std::sync::atomic::Ordering::Relaxed);
                    cx.notify();
                    true
                });
                if !matches!(downloading, Ok(true)) {
                    break;
                }
            }
        })
        .detach();

        cx.spawn(async move |this, cx| {
            let fetched = {
                let received = received.clone();
                cx.background_executor()
                    .spawn(async move { crate::update::download(&installer, &received) })
                    .await
            };
            let file = match fetched {
                Ok(file) => file,
                Err(err) => {
                    this.update(cx, |ws, cx| ws.update_failed(&format!("{err:#}"), cx))
                        .ok();
                    return;
                }
            };
            // Nothing is installed if the user cancelled while it was
            // coming down: `update_progress` is cleared by Cancel, and
            // that is what says the download was still wanted.
            let wanted = this.update(cx, |ws, cx| {
                if ws.update_progress.is_none() {
                    return false;
                }
                ws.update_progress = Some(UpdateProgress::Installing);
                ws.status = "Installing the update\u{2026}".into();
                cx.notify();
                true
            });
            if !matches!(wanted, Ok(true)) {
                crate::update::clean_downloads();
                return;
            }
            let installed = cx
                .background_executor()
                .spawn(async move { crate::update::install_and_restart(&file) })
                .await;
            this.update(cx, |ws, cx| {
                ws.update_progress = None;
                match installed {
                    Ok(()) => {
                        ws.close_modal(cx);
                        // Quitting takes the usual route, so unsaved work
                        // is still asked about. Backing out of one of
                        // those prompts leaves the update staged rather
                        // than lost: it lands whenever this process does
                        // exit.
                        ws.status =
                            format!("Restarting into Schist {}\u{2026}", update.version).into();
                        ws.request_quit(cx);
                    }
                    Err(err) => ws.update_failed(&format!("{err:#}"), cx),
                }
            })
            .ok();
        })
        .detach();
    }

    /// Abandon a download in progress.
    ///
    /// The transfer itself is left to run out into the temporary
    /// directory, since a blocking read cannot be interrupted, but its
    /// result is dropped and nothing is installed.
    pub fn cancel_update(&mut self, cx: &mut Context<Self>) {
        self.update_progress = None;
        self.status = "Update cancelled".into();
        self.close_modal(cx);
        cx.notify();
    }

    /// Give up on an update, leaving the user where they were.
    pub(super) fn update_failed(&mut self, err: &str, cx: &mut Context<Self>) {
        log::error!("update failed: {err}");
        crate::update::clean_downloads();
        self.update_progress = None;
        self.status = format!("Update failed: {err}").into();
        self.close_modal(cx);
        cx.notify();
    }

    /// Fetch a font family and set every layer that wanted it.
    ///
    /// `family` is what the document asked for; `target` is what we may
    /// legally install, which for a proprietary name is its
    /// metric-compatible twin. Both are needed: we download the twin but
    /// re-render the layers that named the original.
    pub fn download_font(&mut self, family: String, target: String, cx: &mut Context<Self>) {
        if self.font_downloads.contains(&family) {
            return;
        }
        self.font_downloads.push(family.clone());
        self.status = format!("Downloading {target}\u{2026}").into();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let fetch_target = target.clone();
            let fetched = cx
                .background_executor()
                .spawn(async move { crate::fonts::fetch_family(&fetch_target) })
                .await;
            this.update(cx, |ws, cx| {
                ws.font_downloads.retain(|f| *f != family);
                ws.status = match fetched.and_then(|faces| install_faces(&faces)) {
                    Ok(n) => {
                        // The engine caches parsed faces and the family
                        // list; both are stale until it re-scans.
                        schist_text_engine::refresh();
                        let redrawn = ws
                            .doc
                            .as_mut()
                            .map(|doc| schist_tools_type::rerender_family(doc, &family))
                            .unwrap_or(0);
                        ws.refresh_missing_fonts();
                        format!(
                            "Installed {target} ({n} faces) \u{b7} re-set {redrawn} text layer(s)"
                        )
                        .into()
                    }
                    Err(e) => format!("{target}: {e}").into(),
                };
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Show what the open document is missing, on demand. Unlike the
    /// prompt on open this ignores what has already been offered, and
    /// opens even when the answer is "nothing" — the user asked, so
    /// answer.
    pub fn show_missing_fonts(&mut self, cx: &mut Context<Self>) {
        let fonts = self
            .doc
            .as_ref()
            .map(crate::fonts::missing_in)
            .unwrap_or_default();
        for font in &fonts {
            self.fonts_offered.insert(font.family.clone());
        }
        // Always open, even with nothing to list. A menu item that asks a
        // question is entitled to answer "nothing is missing" — leaving
        // the screen unchanged is indistinguishable from a dead command.
        self.open_modal(Modal::MissingFonts { fonts }, cx);
    }

    /// Drop rows from the open Missing Fonts dialog that are now
    /// satisfied, and close it once nothing is left to install.
    pub(super) fn refresh_missing_fonts(&mut self) {
        let Some(Modal::MissingFonts { .. }) = &self.modal else {
            return;
        };
        let remaining = self
            .doc
            .as_ref()
            .map(crate::fonts::missing_in)
            .unwrap_or_default();
        // Keep it open once the last font lands: the empty state is the
        // confirmation that the job finished. Closing the window out from
        // under the pointer reads as a glitch.
        self.modal = Some(Modal::MissingFonts { fonts: remaining });
    }

    /// Offer to fetch any font the freshly opened document names but this
    /// system lacks. Local-only: nothing is requested until a button is
    /// pressed.
    pub(super) fn offer_missing_fonts(&mut self, cx: &mut Context<Self>) {
        // Never interrupt something the user already has open.
        if self.modal.is_some() {
            return;
        }
        let Some(doc) = &self.doc else { return };
        let fonts: Vec<_> = crate::fonts::missing_in(doc)
            .into_iter()
            .filter(|f| !self.fonts_offered.contains(&f.family))
            .collect();
        if fonts.is_empty() {
            return;
        }
        for font in &fonts {
            self.fonts_offered.insert(font.family.clone());
        }
        self.open_modal(Modal::MissingFonts { fonts }, cx);
    }

    /// Enable or disable a third-party plugin. The id says which host it
    /// belongs to: Photoshop plug-ins are the ones the 8BF host found.
    pub fn set_plugin_enabled(&mut self, id: String, enabled: bool, cx: &mut Context<Self>) {
        if id.starts_with("8bf.") {
            let Some(dir) = schist_plugin_host_8bf::manager::PluginManager::plugin_dir() else {
                return;
            };
            self.photoshop_plugins.set_enabled(&id, enabled, &dir);
            self.status = format!(
                "{} {} — restart to apply",
                id,
                if enabled { "enabled" } else { "disabled" }
            )
            .into();
            cx.notify();
            return;
        }
        let Some(dir) = schist_plugin_host_wasm::PluginManager::plugin_dir() else {
            return;
        };
        self.plugins.set_enabled(&id, enabled, &dir);
        self.status = format!(
            "{} {} — restart to apply",
            id,
            if enabled { "enabled" } else { "disabled" }
        )
        .into();
        cx.notify();
    }

    /// Install a plugin file into the plugin directory. A `.8bf` or a
    /// `.plugin` bundle goes to the Photoshop folder, anything else to
    /// the WebAssembly one, so one Install button serves both.
    pub fn install_plugin(&mut self, source: PathBuf, cx: &mut Context<Self>) {
        let photoshop = source
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| {
                schist_plugin_host_8bf::FILTER_EXTENSIONS
                    .iter()
                    .any(|w| e.eq_ignore_ascii_case(w))
            });
        self.status = if photoshop {
            match schist_plugin_host_8bf::manager::PluginManager::plugin_dir() {
                Some(dir) => {
                    match schist_plugin_host_8bf::manager::PluginManager::install(&source, &dir) {
                        Ok(path) => {
                            format!("Installed {} — restart to load", path.display()).into()
                        }
                        Err(err) => format!("Plug-in rejected: {err}").into(),
                    }
                }
                None => return,
            }
        } else {
            let Some(dir) = schist_plugin_host_wasm::PluginManager::plugin_dir() else {
                return;
            };
            match schist_plugin_host_wasm::PluginManager::install(&source, &dir) {
                Ok(path) => format!("Installed {} — restart to load", path.display()).into(),
                Err(err) => format!("Plugin rejected: {err}").into(),
            }
        };
        cx.notify();
    }
}
