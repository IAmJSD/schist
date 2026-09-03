//! Documents and tabs: creating, opening, saving, closing.

use super::*;

impl Workspace {
    // ----- document lifecycle -----

    /// File ▸ New: the preset picker. A preset creates on the spot;
    /// Custom… goes on to the full dialog.
    pub fn open_new_file_picker(&mut self, cx: &mut Context<Self>) {
        self.open_modal(Modal::NewFilePicker, cx);
    }

    /// The full new-document dialog (the picker's Custom…). Photoshop
    /// asks for the settings before creating anything, so this only
    /// opens the dialog; `create_document` runs on Create.
    pub fn open_new_document_dialog(&mut self, cx: &mut Context<Self>) {
        self.open_modal(
            Modal::NewDocument {
                name: self.next_untitled_name(),
                width: 1280,
                height: 800,
                resolution: 72.0,
                mode: ColorMode::Rgb,
                depth: Depth::Eight,
                background: NewDocBackground::White,
            },
            cx,
        );
    }

    /// "Untitled-1", "Untitled-2", ... skipping names an open tab uses.
    pub(super) fn next_untitled_name(&self) -> String {
        let taken = self.tab_strip();
        (1..)
            .map(|n| format!("Untitled-{n}"))
            .find(|name| !taken.iter().any(|(title, _)| title.as_ref() == name))
            .unwrap()
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_document(
        &mut self,
        name: &str,
        width: u32,
        height: u32,
        resolution: f32,
        mode: ColorMode,
        depth: Depth,
        background: NewDocBackground,
    ) {
        self.rebuild_tool_groups();
        let width = width.clamp(1, 30000);
        let height = height.clamp(1, 30000);
        let name = name.trim();
        let mut doc = Document::new(
            if name.is_empty() { "Untitled" } else { name },
            width,
            height,
            depth,
        );
        doc.resolution_dpi = resolution.max(1.0);
        doc.mode = mode;
        let component = |v: f32| (v * 255.0).round().clamp(0.0, 255.0) as u8;
        let fill = match background {
            NewDocBackground::White => Some([255, 255, 255, 255]),
            NewDocBackground::Black => Some([0, 0, 0, 255]),
            NewDocBackground::BackgroundColor => {
                let c = self.editor.background;
                Some([component(c.r), component(c.g), component(c.b), 255])
            }
            NewDocBackground::Transparent => None,
        };
        // A filled start is "Background", a transparent one an ordinary
        // "Layer 1", as in Photoshop.
        let mut layer = Layer::new_raster(if fill.is_some() {
            "Background"
        } else {
            "Layer 1"
        });
        if let Some(rgba) = fill {
            let buf = rgba.repeat(width as usize * height as usize);
            blit_rgba8(
                &mut layer.as_raster_mut().unwrap().tiles,
                depth,
                IntRect::from_size(width, height),
                &buf,
            );
        }
        doc.push_layer(layer);
        doc.mark_saved();
        self.open_in_tab(doc, false);
    }

    /// Open `doc` in a new tab and switch to it. An untouched Untitled in
    /// the active slot is replaced instead, so opening a file right after
    /// launch doesn't leave a blank tab behind.
    pub fn install_document(&mut self, doc: Document) {
        self.open_in_tab(doc, true);
    }

    pub(super) fn open_in_tab(&mut self, mut doc: Document, replace_pristine: bool) {
        // A document arriving is what ends the gallery: whether it came
        // from File ▸ New, a gallery double-click or a crash recovery,
        // the editor is where it lives — and its memory goes with it.
        #[cfg(not(target_arch = "wasm32"))]
        if self.library.open {
            self.library.open = false;
            self.library.shed_memory();
        }
        // Photoshop's History Brush paints back from the state the file
        // was opened in, so that is what gets snapshotted here.
        doc.snapshot_history_source();
        let pristine = replace_pristine
            && self
                .doc
                .as_ref()
                .is_some_and(|d| d.path.is_none() && !d.dirty && !d.history.can_undo());
        if !pristine {
            self.stash_active_tab();
            self.active_tab = self.background_tabs.len();
        }
        self.doc = Some(doc);
        self.reset_per_document_caches();
        self.zoom = 1.0;
        self.offset = point(px(40.0), px(40.0));
        // Fit now when the canvas has a size, and again on the first
        // real paint regardless: opened from the gallery (or at boot)
        // the canvas has never laid out, and fitting against zero — or
        // stale — bounds left a 12-megapixel photo at 100%, top-left.
        self.fit_to_view();
        self.pending_fit = true;
    }

    // ----- tabs -----

    pub fn tab_count(&self) -> usize {
        self.background_tabs.len() + self.doc.is_some() as usize
    }

    pub fn active_tab(&self) -> usize {
        self.active_tab
    }

    /// Title and dirty flag of every tab, in tab-strip order.
    pub fn tab_strip(&self) -> Vec<(SharedString, bool)> {
        let mut out = Vec::with_capacity(self.tab_count());
        let mut parked = self.background_tabs.iter();
        for index in 0..self.tab_count() {
            let active_doc = (index == self.active_tab)
                .then_some(self.doc.as_ref())
                .flatten();
            let (title, dirty) = if let Some(doc) = active_doc {
                (doc.title.clone(), doc.dirty)
            } else if let Some(tab) = parked.next() {
                (tab.doc.title.clone(), tab.doc.dirty)
            } else {
                continue;
            };
            out.push((title.into(), dirty));
        }
        out
    }

    /// Park the active document, view transform and all, back into the
    /// tab list at its current position.
    pub(super) fn stash_active_tab(&mut self) {
        if let Some(doc) = self.doc.take() {
            let at = self.active_tab.min(self.background_tabs.len());
            self.background_tabs.insert(
                at,
                DocTab {
                    doc,
                    zoom: self.zoom,
                    offset: self.offset,
                    rotation: self.rotation,
                },
            );
        }
    }

    /// Check a parked tab out onto the canvas, restoring its view.
    pub(super) fn wake_tab(&mut self, tab: DocTab) {
        self.doc = Some(tab.doc);
        self.zoom = tab.zoom;
        self.editor.zoom = tab.zoom;
        self.offset = tab.offset;
        self.rotation = tab.rotation;
        self.reset_per_document_caches();
    }

    /// Drop every cache keyed by document state. Revision and selection
    /// generation counters restart per document, so caches tagged with
    /// them (nav thumbnail, selection outline) must go too or a collision
    /// would show the previous document's pixels.
    pub(super) fn reset_per_document_caches(&mut self) {
        self.rebuild_color_transforms();
        self.cache.invalidate_all();
        self.display_tiles.clear();
        self.prefetch_queue.clear();
        self.invalidate_viewport_image();
        if let Some(old) = self.preview.image.take() {
            self.retired_images.push(old);
        }
        self.preview = Preview::default();
        self.selection_outline = None;
        if let Some((_, old)) = self.nav_thumb.take() {
            self.retired_images.push(old);
        }
        self.filter_preview = None;
        self.dragging_guide = None;
    }

    /// Make the document at `index` the one on the canvas.
    pub fn select_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.doc.is_some() && index == self.active_tab {
            return;
        }
        self.stash_active_tab();
        if self.background_tabs.is_empty() {
            return;
        }
        let index = index.min(self.background_tabs.len() - 1);
        let tab = self.background_tabs.remove(index);
        self.active_tab = index;
        self.wake_tab(tab);
        cx.notify();
    }

    /// Step to an adjacent tab, wrapping at the ends.
    pub fn cycle_tab(&mut self, delta: isize, cx: &mut Context<Self>) {
        let count = self.tab_count();
        if count < 2 {
            return;
        }
        let next = (self.active_tab as isize + delta).rem_euclid(count as isize) as usize;
        self.select_tab(next, cx);
    }

    /// Close a tab, asking about unsaved changes first. A dirty tab is
    /// brought to the front so the prompt is about what's on screen.
    pub fn request_close_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        let dirty = self.tab_strip().get(index).is_some_and(|(_, dirty)| *dirty);
        if dirty {
            self.select_tab(index, cx);
            self.open_modal(Modal::ConfirmCloseTab, cx);
        } else {
            self.close_tab(index, cx);
        }
    }

    /// Index of the first tab with unsaved changes, if any.
    pub fn first_dirty_tab(&self) -> Option<usize> {
        self.tab_strip().iter().position(|(_, dirty)| *dirty)
    }

    /// Begin quitting: prompt for each dirty tab, then quit.
    ///
    /// The window's `should_close` hook is synchronous and the prompt is
    /// not, so quitting is vetoed and resumed here once the prompts are
    /// answered.
    pub fn request_quit(&mut self, cx: &mut Context<Self>) {
        match self.first_dirty_tab() {
            Some(index) => {
                self.pending_quit = true;
                self.select_tab(index, cx);
                self.open_modal(Modal::ConfirmCloseTab, cx);
            }
            None => {
                self.pending_quit = false;
                cx.quit();
            }
        }
    }

    /// The user backed out of one of the prompts, so the quit is off.
    pub fn cancel_quit(&mut self) {
        self.pending_quit = false;
    }

    /// Continue a quit after a tab was saved or discarded: prompt for the
    /// next dirty tab, or quit once none are left. A no-op when the user
    /// is just closing a tab.
    pub fn resume_quit(&mut self, cx: &mut Context<Self>) {
        if self.pending_quit {
            self.request_quit(cx);
        }
    }

    /// Close tab `index` outright, discarding any unsaved changes. Closing
    /// the last tab leaves an empty workspace.
    pub fn close_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.doc.is_none() {
            return;
        }
        if index == self.active_tab {
            if let Some(doc) = self.doc.take() {
                self.remove_recovery_for(doc.id);
                #[cfg(not(target_arch = "wasm32"))]
                self.forget_backing(doc.id);
            }
            if self.background_tabs.is_empty() {
                self.active_tab = 0;
            } else {
                // The tab to the right slides into the closed slot; at
                // the end, fall back to the new last tab.
                let next = index.min(self.background_tabs.len() - 1);
                let tab = self.background_tabs.remove(next);
                self.active_tab = next;
                self.wake_tab(tab);
            }
        } else if index < self.tab_count() {
            let parked = if index < self.active_tab {
                index
            } else {
                index - 1
            };
            let tab = self.background_tabs.remove(parked);
            self.remove_recovery_for(tab.doc.id);
            #[cfg(not(target_arch = "wasm32"))]
            self.forget_backing(tab.doc.id);
            if index < self.active_tab {
                self.active_tab -= 1;
            }
        } else {
            return;
        }
        // The last tab closing empties the editor; the gallery is home,
        // and it comes back exactly as it was left — search, selection,
        // filters and all live for the session on the library.
        #[cfg(not(target_arch = "wasm32"))]
        if self.doc.is_none() && self.background_tabs.is_empty() && !self.library.open {
            self.toggle_gallery(cx);
        }
        cx.notify();
    }

    /// Open `path` without blocking the window: the read and decode run
    /// on a background thread and the document is installed when ready.
    pub fn load_file(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.status = format!("Opening {}\u{2026}", path.display()).into();
        cx.notify();
        let codecs = self.registry.shared_codecs();
        cx.spawn(async move |this, cx| {
            let decode_path = path.clone();
            let result = cx
                .background_executor()
                .spawn(async move { decode_file(&codecs, &decode_path) })
                .await;
            this.update(cx, |ws, cx| {
                ws.finish_load(path, result, cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn finish_load(
        &mut self,
        path: PathBuf,
        result: anyhow::Result<Document>,
        cx: &mut Context<Self>,
    ) {
        // Only the HEIC retry arm reads the path, and that arm is
        // desktop-only.
        #[cfg(target_arch = "wasm32")]
        let _ = &path;
        match result {
            Ok(doc) => {
                self.status = match &doc.path {
                    Some(p) => format!("Opened {}", p.display()).into(),
                    None => format!("Opened {}", doc.title).into(),
                };
                self.install_document(doc);
                // Adopt a gallery edit's sidecar arrangement, or record
                // an ordinary open in the recents.
                #[cfg(not(target_arch = "wasm32"))]
                self.finish_load_bookkeeping(&path);
                self.offer_missing_fonts(cx);
            }
            // A HEIC on a machine with no libheif — or a libheif with
            // no HEVC decoder, as stock Ubuntu ships: downloading the
            // managed build fixes both, so offer that instead of failing.
            // (The web build has no dlopen and no HEIC codec at all, so
            // the plain error arm below is what a .heic gets there.)
            #[cfg(not(target_arch = "wasm32"))]
            Err(err)
                if schist_codecs_common::heif::download_would_help(&err)
                    && schist_codecs_common::heif::managed_library().is_some()
                    && self.modal.is_none() =>
            {
                self.status = "HEIC support is not installed".into();
                self.open_modal(Modal::HeifSupport { path }, cx);
            }
            // A raw Schist's own decoder declines needs LibRaw, and
            // there is none to load: say how to get one and offer a
            // retry. (The web build refuses these files outright with
            // a message of its own, through the plain arm below.)
            #[cfg(not(target_arch = "wasm32"))]
            Err(err)
                if schist_codecs_common::raw::is_missing_library_error(&err)
                    && self.modal.is_none() =>
            {
                log::info!("open needs LibRaw: {err:#}");
                self.status = "LibRaw is not installed".into();
                self.open_modal(Modal::LibRawSupport { path }, cx);
            }
            Err(err) => {
                log::error!("open failed: {err:#}");
                self.status = format!("Open failed: {err}").into();
            }
        }
    }

    /// Download the pinned decode-only libheif build and its license
    /// texts — only ever called from the consent dialog — then retry
    /// opening the file that needed it.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn download_heif_support(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.heif_download {
            return;
        }
        let Some(managed) = schist_codecs_common::heif::managed_library() else {
            return;
        };
        self.heif_download = true;
        self.status = format!(
            "Downloading HEIC support (libheif {})\u{2026}",
            managed.version
        )
        .into();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let installed = cx
                .background_executor()
                .spawn(async move {
                    // License texts first: the library must not land
                    // without them.
                    for file in managed.licenses.iter().chain([&managed.library]) {
                        // Nothing reads the byte count on this path --
                        // the status line says what it is doing and
                        // there is no per-file row to update -- so the
                        // counter is a sink.
                        let bytes = fetch_model(file.url, &AtomicU64::new(0))
                            .map_err(|e| anyhow::anyhow!("{}: {e}", file.name))?;
                        schist_codecs_common::heif::install(file, &bytes)?;
                    }
                    anyhow::Ok(())
                })
                .await;
            this.update(cx, |ws, cx| {
                ws.heif_download = false;
                match installed {
                    // From the gallery, the ask was thumbnails, not this
                    // one file in the editor: retry the failed thumbs
                    // and stay where the user is.
                    Ok(()) if ws.library.open => {
                        ws.library.retry_failed_thumbs();
                        ws.status = "HEIC support installed".into();
                        ws.library_rescan(cx);
                    }
                    Ok(()) => ws.load_file(path, cx),
                    Err(err) => {
                        log::error!("HEIC support download failed: {err:#}");
                        ws.status = format!("HEIC support download failed: {err}").into();
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Files dragged from the OS and dropped anywhere in the window.
    ///
    /// Layered documents always open in their own tabs. A flat image
    /// dropped onto an open document could mean "open it" or "place it",
    /// so that case asks; with several files, or nothing to place into,
    /// everything just opens.
    pub fn handle_dropped_paths(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        // Folders are a question, not a file: open what is inside, or
        // watch them in the gallery? Loose files dropped alongside open
        // as they always did.
        #[cfg(not(target_arch = "wasm32"))]
        {
            let (dirs, files): (Vec<PathBuf>, Vec<PathBuf>) =
                paths.into_iter().partition(|p| p.is_dir());
            if !dirs.is_empty() {
                let images = schist_gallery::scan_folders(&dirs, &self.codec_extensions())
                    .iter()
                    .map(|s| s.entries.len())
                    .sum();
                self.open_modal(Modal::DropFolders { dirs, images }, cx);
            }
            for path in files {
                self.load_file(path, cx);
            }
            return;
        }
        #[allow(unreachable_code)]
        if let [path] = paths.as_slice() {
            if self.doc.is_some() && self.is_flat_image(path) {
                self.open_modal(Modal::DropImage { path: path.clone() }, cx);
                return;
            }
        }
        for path in paths {
            self.load_file(path, cx);
        }
    }

    /// Read the open document's EXIF once per document, and reset the
    /// side panel's tab choice with it. The file is the original photo
    /// for a gallery edit (the sidecar is a PSD, which carries none)
    /// and the document's own file otherwise; an untitled document has
    /// nothing to read.
    pub fn refresh_exif(&mut self) {
        let Some(doc) = self.doc.as_ref() else {
            if self.exif.is_some() {
                self.exif = None;
                self.side_tab = None;
            }
            return;
        };
        if self.exif.as_ref().is_some_and(|(id, _)| *id == doc.id) {
            return;
        }
        let id = doc.id;
        #[cfg(not(target_arch = "wasm32"))]
        let source = self
            .library
            .edit_backings
            .get(&id)
            .cloned()
            .or_else(|| doc.path.clone());
        #[cfg(not(target_arch = "wasm32"))]
        let summary = source.as_deref().and_then(schist_gallery::exif_summary);
        // The web build has no file system: its opened files live in
        // memory under invented paths, so read the bytes back from there.
        #[cfg(target_arch = "wasm32")]
        let summary = doc
            .path
            .as_deref()
            .and_then(|path| crate::web::read_file(path).ok())
            .and_then(|bytes| schist_gallery::exif_summary_bytes(&bytes));
        #[cfg(not(target_arch = "wasm32"))]
        {
            // The info map opens on the spot, at street scale.
            let map = &mut self.info_map;
            map.markers.clear();
            if let Some((lat, lon)) = summary.as_ref().and_then(|s| s.gps) {
                map.markers.push((lat, lon));
                map.look_at(lat, lon, 15);
            }
        }
        self.exif = Some((id, summary));
        self.side_tab = None;
    }

    /// True when the extension belongs to a single-layer image format.
    /// Layered formats never make sense as one new layer.
    pub(super) fn is_flat_image(&self, path: &std::path::Path) -> bool {
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            return false;
        };
        let ext = ext.to_ascii_lowercase();
        self.registry
            .codecs()
            .find(|c| c.extensions().contains(&ext.as_str()))
            .is_some_and(|c| !matches!(c.id(), "codec.psd" | "codec.affinity"))
    }

    /// Decode `path` off the UI thread and insert it into the current
    /// document as a new raster layer, centered like a paste.
    pub fn place_image_as_layer(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.status = format!("Placing {}\u{2026}", path.display()).into();
        cx.notify();
        let codecs = self.registry.shared_codecs();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let doc = decode_file(&codecs, &path)?;
                    // The codec hands back a document; the layer wants
                    // pixels, so flatten it.
                    let rect = doc.canvas_rect();
                    let rgba = schist_compositor::composite_region_rgba8(&doc, rect);
                    anyhow::Ok((path, doc.title, rect, rgba))
                })
                .await;
            this.update(cx, |ws, cx| {
                ws.finish_place(result, cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn finish_place(
        &mut self,
        result: anyhow::Result<(PathBuf, String, IntRect, Vec<u8>)>,
        cx: &mut Context<Self>,
    ) {
        let (path, title, rect, rgba) = match result {
            Ok(r) => r,
            Err(err) => {
                log::error!("place failed: {err:#}");
                self.status = format!("Place failed: {err}").into();
                return;
            }
        };
        if self.doc.is_none() {
            // The tab closed while the file decoded; open it in its own
            // tab instead of dropping it on the floor.
            self.load_file(path, cx);
            return;
        }
        let doc = self.doc.as_mut().unwrap();
        // Centered, like paste with no selection.
        let cw = doc.width as i32;
        let ch = doc.height as i32;
        let dest = IntRect::from_xywh(
            (cw - rect.width()) / 2,
            (ch - rect.height()) / 2,
            rect.width() as u32,
            rect.height() as u32,
        );
        let mut layer = Layer::new_raster(title.clone());
        blit_rgba8(
            &mut layer.as_raster_mut().unwrap().tiles,
            doc.depth,
            dest,
            &rgba,
        );
        let id = layer.id;
        let insert_at = match doc.active_layer.and_then(|a| doc.tree.path_of(a)) {
            Some(mut p) => {
                *p.0.last_mut().unwrap() += 1;
                p
            }
            None => schist_core::LayerPath(vec![doc.tree.layers.len()]),
        };
        let mut edit = doc.begin_edit("Place Image");
        edit.insert_layer(insert_at, layer);
        edit.commit();
        doc.active_layer = Some(id);
        self.status = format!("Placed {title}").into();
        self.after_change(cx);
    }

    /// Serialize the document to `path`, choosing the codec by extension.
    pub fn save_file_as(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        // A save landing on a gallery sidecar keeps the previous state as
        // a version first — that is the gallery's automatic versioning.
        #[cfg(not(target_arch = "wasm32"))]
        self.pre_save_backing(&path);
        match self.write_document_to(&path) {
            Ok(()) => {
                if let Some(doc) = &mut self.doc {
                    doc.mark_saved();
                    doc.path = Some(path.clone());
                    if let Some(name) = path.file_name() {
                        doc.title = name.to_string_lossy().into_owned();
                    }
                }
                self.clear_recovery();
                // A sidecar save refreshes its photo's thumbnail instead
                // of joining the recents; anything else is a real file
                // the user may want back quickly.
                #[cfg(not(target_arch = "wasm32"))]
                if !self.post_save_backing(&path) {
                    self.note_recent(&path);
                }
                self.status = format!("Saved {}", path.display()).into();
                // Only if this is the document the close was asked for.
                // The Save As portal does not block the window on Linux,
                // so the user can switch tabs and save another one while
                // it is up, and a bare flag closed the wrong tab.
                let saved = self.doc.as_ref().map(|d| d.id);
                if self.close_after_save.take() == saved && saved.is_some() {
                    let index = self.active_tab();
                    self.close_tab(index, cx);
                }
            }
            Err(err) => {
                // The tab stays open on a failed save, whatever was asked.
                self.close_after_save = None;
                log::error!("save failed: {err:#}");
                self.status = format!("Save failed: {err}").into();
            }
        }
        cx.notify();
    }

    /// Ask for the active tab to close as soon as its save lands.
    pub fn close_tab_after_save(&mut self) {
        self.close_after_save = self.doc.as_ref().map(|d| d.id);
    }

    /// Whether a save is still outstanding with a close waiting on it.
    ///
    /// Save As is asynchronous: `save_current` returns before the file
    /// prompt resolves, so this is how a caller tells a synchronous save
    /// (already done, tab already closed) from one still in flight.
    pub fn has_pending_save(&self) -> bool {
        self.close_after_save.is_some()
    }

    /// The save never happened, so nothing is waiting on it.
    pub fn cancel_pending_save(&mut self) {
        self.close_after_save = None;
    }

    /// ⌘S: save over the document's existing path, or fall back to Save As
    /// when it has never been saved (or its format can't be written).
    pub fn save_current(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let path = self.doc.as_ref().and_then(|d| d.path.clone());
        match path {
            Some(path) if self.exporter_for(&path).is_some() => self.save_file_as(path, cx),
            _ => keymap::save_file_dialog(self, window, cx),
        }
    }

    pub(super) fn exporter_for(
        &self,
        path: &std::path::Path,
    ) -> Option<&dyn schist_plugin_api::CodecPlugin> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        self.registry
            .codecs()
            .find(|c| c.can_export() && c.extensions().contains(&ext.as_str()))
    }

    pub(super) fn write_document_to(&self, path: &std::path::Path) -> anyhow::Result<()> {
        let doc = self
            .doc
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no document"))?;
        self.write_doc_to(doc, path)
    }

    pub(super) fn write_doc_to(
        &self,
        doc: &Document,
        path: &std::path::Path,
    ) -> anyhow::Result<()> {
        let codec = self.exporter_for(path).ok_or_else(|| {
            anyhow::anyhow!(
                "no exporter for .{}",
                path.extension().and_then(|e| e.to_str()).unwrap_or("")
            )
        })?;
        let bytes = codec.export(doc)?;
        // Write to a sibling temp file and rename, so an interrupted save
        // can't truncate the user's existing file.
        #[cfg(not(target_arch = "wasm32"))]
        {
            let tmp = path.with_extension("schist-tmp");
            std::fs::write(&tmp, bytes)?;
            std::fs::rename(&tmp, path)?;
        }
        // The browser has no user-visible file system to write into: the
        // bytes go to the in-memory map (so the path reads back, and a
        // plain ⌘S keeps working on it) and out as a download.
        #[cfg(target_arch = "wasm32")]
        {
            crate::web::write_file(path, bytes.clone());
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "untitled.psd".into());
            crate::web::download_bytes(&name, &bytes)?;
        }
        Ok(())
    }
}
