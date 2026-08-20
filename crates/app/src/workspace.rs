//! The single-window workspace: canvas viewport + docked panels.
//!
//! The kernel and plugins are GPUI-free; this file is the boundary where
//! GPUI events become `PointerInput`s for the active tool and where
//! composited tiles become GPU textures.

use crate::actions::*;
use crate::keymap;
use crate::panels;
use gpui::{
    canvas, div, point, px, size, App, Bounds, Context, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, ParentElement as _, PathBuilder, Pixels, Point, Render, RenderImage,
    ScrollWheelEvent, SharedString, Styled as _, Window,
};
use photoslop_color::Depth;
use photoslop_compositor::TileCache;
use photoslop_core::{blit_rgba8, Document, IntRect, Layer, TileCoord, TILE_SIZE};
use photoslop_plugin_api::{
    CommandCtx, EditorState, Modifiers, Overlay, PluginRegistry, PointerInput, ToolCtx,
};
use rustc_hash::FxHashMap;
use smallvec::smallvec;
use std::path::PathBuf;
use std::sync::Arc;

/// Zoom below which the canvas paints one downscaled preview image instead
/// of hundreds of tile quads.
const PREVIEW_ZOOM_CUTOFF: f32 = 0.35;
const PREVIEW_SHIFT: u32 = 3; // preview at 1/8 scale
/// How often a dirty document is snapshotted for crash recovery.
const AUTOSAVE_SECS: u64 = 30;

pub struct Workspace {
    pub registry: PluginRegistry,
    pub editor: EditorState,
    pub doc: Option<Document>,
    cache: TileCache,
    tile_images: FxHashMap<TileCoord, Arc<RenderImage>>,
    preview: Preview,
    pub zoom: f32,
    /// Screen offset of document origin within the canvas element.
    pub offset: Point<Pixels>,
    canvas_bounds: Bounds<Pixels>,
    focus: FocusHandle,
    pan_last: Option<Point<Pixels>>,
    space_held: bool,
    pointer_down: bool,
    pub status: SharedString,
    /// Which transient popup (menu / dropdown) is open.
    pub open_popup: Option<Popup>,
    /// Live bounds of slider tracks, recorded each frame by their canvases.
    slider_bounds: FxHashMap<&'static str, Bounds<Pixels>>,
    /// Slider drag in progress: (slider id, value before the drag) — used
    /// to commit layer-opacity drags as one undo step on release.
    active_slider: Option<(&'static str, f32)>,
    /// Layer thumbnails keyed by layer id, tagged with the doc revision
    /// they were rendered at.
    thumbs: FxHashMap<photoslop_core::LayerId, (u64, Arc<RenderImage>)>,
    /// The open modal dialog, if any.
    pub modal: Option<Modal>,
    /// Numeric field currently accepting digits, and its edit buffer.
    pub focused_field: Option<&'static str>,
    pub field_buffer: String,
    /// Third-party plugin registry state (M9).
    pub plugins: photoslop_plugin_host_wasm::PluginManager,
    /// Plugin enable/disable requested from the manager UI, applied on the
    /// next render pass (the checkbox callback has no context to do it).
    pub pending_plugin_toggle: Option<(String, bool)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Popup {
    Menu(usize),
    BlendModes,
    /// A dropdown inside a dialog, keyed by field id.
    Field(&'static str),
}

/// Which modal dialog is open.
#[derive(Debug, Clone, PartialEq)]
pub enum Modal {
    ImageSize {
        width: u32,
        height: u32,
        filter: photoslop_core::Filter,
        link: bool,
    },
    CanvasSize {
        width: u32,
        height: u32,
        anchor: (f32, f32),
    },
    /// A destructive filter with live parameters.
    Filter {
        id: &'static str,
        values: photoslop_plugin_api::FilterValues,
    },
    /// The third-party plugin manager.
    PluginManager,
    /// Editing an existing adjustment layer's parameters. `original` is
    /// what the layer held before the dialog opened, so Cancel can put it
    /// back exactly.
    Adjustment {
        layer: photoslop_core::LayerId,
        params: photoslop_adjustments::Params,
        original: (Option<String>, Vec<u8>),
    },
}

#[derive(Default)]
struct Preview {
    /// RGBA8 straight, (doc.width >> PREVIEW_SHIFT) x (doc.height >> ...).
    buf: Vec<u8>,
    w: u32,
    h: u32,
    image: Option<Arc<RenderImage>>,
    dirty: Vec<IntRect>,
    valid: bool,
}

impl Workspace {
    pub fn new(
        registry: PluginRegistry,
        plugins: photoslop_plugin_host_wasm::PluginManager,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut ws = Workspace {
            registry,
            editor: EditorState::default(),
            doc: None,
            cache: TileCache::new(),
            tile_images: FxHashMap::default(),
            preview: Preview::default(),
            zoom: 1.0,
            offset: point(px(0.0), px(0.0)),
            canvas_bounds: Bounds::default(),
            focus: cx.focus_handle(),
            pan_last: None,
            space_held: false,
            pointer_down: false,
            status: "Ready".into(),
            open_popup: None,
            slider_bounds: FxHashMap::default(),
            active_slider: None,
            thumbs: FxHashMap::default(),
            modal: None,
            focused_field: None,
            field_buffer: String::new(),
            plugins,
            pending_plugin_toggle: None,
        };
        ws.new_document();
        // Periodic crash-recovery snapshot; the task ends with the entity.
        cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(std::time::Duration::from_secs(AUTOSAVE_SECS))
                .await;
            if this.update(cx, |ws, _| ws.autosave()).is_err() {
                break;
            }
        })
        .detach();
        ws
    }

    // ----- document lifecycle -----

    pub fn new_document(&mut self) {
        let mut doc = Document::new("Untitled", 1280, 800, Depth::Eight);
        let mut bg = Layer::new_raster("Background");
        let white = vec![255u8; 1280 * 800 * 4];
        blit_rgba8(
            &mut bg.as_raster_mut().unwrap().tiles,
            Depth::Eight,
            IntRect::from_size(1280, 800),
            &white,
        );
        doc.push_layer(bg);
        doc.dirty = false;
        self.install_document(doc);
    }

    pub fn install_document(&mut self, doc: Document) {
        self.doc = Some(doc);
        self.cache.invalidate_all();
        self.tile_images.clear();
        self.preview = Preview::default();
        self.zoom = 1.0;
        self.offset = point(px(40.0), px(40.0));
        self.fit_to_view();
    }

    pub fn load_file(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let result = (|| -> anyhow::Result<Document> {
            let bytes = std::fs::read(&path)?;
            let ext = path.extension().and_then(|e| e.to_str());
            let codec = self
                .registry
                .codec_for(&bytes, ext)
                .ok_or_else(|| anyhow::anyhow!("no codec for {}", path.display()))?;
            let mut doc = codec.import(&bytes)?;
            doc.title = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "Untitled".into());
            doc.path = Some(path.clone());
            Ok(doc)
        })();
        match result {
            Ok(doc) => {
                self.status = format!("Opened {}", path.display()).into();
                self.install_document(doc);
            }
            Err(err) => {
                log::error!("open failed: {err:#}");
                self.status = format!("Open failed: {err}").into();
            }
        }
        cx.notify();
    }

    /// Serialize the document to `path`, choosing the codec by extension.
    pub fn save_file_as(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        match self.write_document_to(&path) {
            Ok(()) => {
                if let Some(doc) = &mut self.doc {
                    doc.dirty = false;
                    doc.path = Some(path.clone());
                    if let Some(name) = path.file_name() {
                        doc.title = name.to_string_lossy().into_owned();
                    }
                }
                self.clear_recovery();
                self.status = format!("Saved {}", path.display()).into();
            }
            Err(err) => {
                log::error!("save failed: {err:#}");
                self.status = format!("Save failed: {err}").into();
            }
        }
        cx.notify();
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

    fn exporter_for(
        &self,
        path: &std::path::Path,
    ) -> Option<&dyn photoslop_plugin_api::CodecPlugin> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        self.registry
            .codecs()
            .find(|c| c.can_export() && c.extensions().contains(&ext.as_str()))
    }

    fn write_document_to(&self, path: &std::path::Path) -> anyhow::Result<()> {
        let doc = self
            .doc
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no document"))?;
        let codec = self.exporter_for(path).ok_or_else(|| {
            anyhow::anyhow!(
                "no exporter for .{}",
                path.extension().and_then(|e| e.to_str()).unwrap_or("")
            )
        })?;
        let bytes = codec.export(doc)?;
        // Write to a sibling temp file and rename, so an interrupted save
        // can't truncate the user's existing file.
        let tmp = path.with_extension("photoslop-tmp");
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    // ----- crash recovery -----

    /// Directory holding autosaved recovery snapshots.
    pub fn recovery_dir() -> Option<PathBuf> {
        let base = std::env::var("XDG_STATE_HOME")
            .ok()
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var("HOME")
                    .ok()
                    .map(|h| PathBuf::from(h).join(".local/state"))
            })?;
        Some(base.join("photoslop/recovery"))
    }

    fn recovery_path(&self) -> Option<PathBuf> {
        Some(Self::recovery_dir()?.join(format!("session-{}.psd", std::process::id())))
    }

    /// Write a recovery snapshot if the document has unsaved changes.
    /// Returns true when a snapshot was written.
    pub fn autosave(&mut self) -> bool {
        let dirty = self.doc.as_ref().map(|d| d.dirty).unwrap_or(false);
        if !dirty {
            return false;
        }
        let Some(path) = self.recovery_path() else {
            return false;
        };
        let Some(dir) = path.parent() else {
            return false;
        };
        if let Err(err) = std::fs::create_dir_all(dir) {
            log::warn!("autosave: cannot create {dir:?}: {err}");
            return false;
        }
        match self.write_document_to(&path) {
            Ok(()) => {
                log::debug!("autosaved recovery snapshot to {path:?}");
                true
            }
            Err(err) => {
                log::warn!("autosave failed: {err:#}");
                false
            }
        }
    }

    fn clear_recovery(&self) {
        if let Some(path) = self.recovery_path() {
            let _ = std::fs::remove_file(path);
        }
    }

    /// Newest recovery snapshot left behind by a previous run, if any.
    pub fn pending_recovery() -> Option<PathBuf> {
        let dir = Self::recovery_dir()?;
        let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
        for entry in std::fs::read_dir(dir).ok()?.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("psd") {
                continue;
            }
            // Skip a snapshot this very process owns.
            if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n == format!("session-{}.psd", std::process::id()))
            {
                continue;
            }
            let Ok(modified) = entry.metadata().and_then(|m| m.modified()) else {
                continue;
            };
            if best.as_ref().is_none_or(|(t, _)| modified > *t) {
                best = Some((modified, path));
            }
        }
        best.map(|(_, p)| p)
    }

    /// Load a recovery snapshot and mark it dirty (it has no real path).
    pub fn recover_from(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.load_file(path.clone(), cx);
        if let Some(doc) = &mut self.doc {
            doc.path = None;
            doc.dirty = true;
            doc.title = format!("{} (recovered)", doc.title);
        }
        let _ = std::fs::remove_file(&path);
        self.status = "Recovered unsaved work from a previous session".into();
        cx.notify();
    }

    // ----- viewport -----

    pub fn fit_to_view(&mut self) {
        let Some(doc) = &self.doc else { return };
        let avail = self.canvas_bounds.size;
        if avail.width <= px(0.0) || avail.height <= px(0.0) {
            return;
        }
        let margin = 40.0;
        let zx = (f32::from(avail.width) - margin) / doc.width as f32;
        let zy = (f32::from(avail.height) - margin) / doc.height as f32;
        self.zoom = zx.min(zy).clamp(0.005, 32.0);
        self.editor.zoom = self.zoom;
        self.center();
    }

    fn center(&mut self) {
        let Some(doc) = &self.doc else { return };
        let avail = self.canvas_bounds.size;
        self.offset = point(
            px((f32::from(avail.width) - doc.width as f32 * self.zoom) / 2.0),
            px((f32::from(avail.height) - doc.height as f32 * self.zoom) / 2.0),
        );
    }

    pub fn zoom_by(&mut self, factor: f32, around: Option<Point<Pixels>>) {
        let old = self.zoom;
        let new = (old * factor).clamp(0.005, 32.0);
        let pivot = around.unwrap_or_else(|| {
            point(
                px(f32::from(self.canvas_bounds.size.width) / 2.0),
                px(f32::from(self.canvas_bounds.size.height) / 2.0),
            )
        });
        // Keep the document point under `pivot` fixed.
        let scale = new / old;
        self.offset = point(
            px(f32::from(pivot.x) - (f32::from(pivot.x) - f32::from(self.offset.x)) * scale),
            px(f32::from(pivot.y) - (f32::from(pivot.y) - f32::from(self.offset.y)) * scale),
        );
        self.zoom = new;
        self.editor.zoom = new;
    }

    fn doc_pos(&self, canvas_local: Point<Pixels>) -> (f32, f32) {
        (
            (f32::from(canvas_local.x) - f32::from(self.offset.x)) / self.zoom,
            (f32::from(canvas_local.y) - f32::from(self.offset.y)) / self.zoom,
        )
    }

    fn to_local(&self, window_pos: Point<Pixels>) -> Point<Pixels> {
        point(
            window_pos.x - self.canvas_bounds.origin.x,
            window_pos.y - self.canvas_bounds.origin.y,
        )
    }

    // ----- change propagation -----

    pub fn after_change(&mut self, cx: &mut Context<Self>) {
        if let Some(doc) = &mut self.doc {
            let damage = doc.take_damage();
            for rect in &damage {
                self.cache.invalidate(rect);
                for coord in TileCoord::covering(rect) {
                    self.tile_images.remove(&coord);
                }
                self.preview.dirty.push(*rect);
            }
        }
        cx.notify();
    }

    pub fn run_command(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(doc) = self.doc.as_mut() else { return };
        if let Some(command) = self.registry.command(id) {
            let mut ctx = CommandCtx {
                doc,
                state: &mut self.editor,
            };
            (command.run)(&mut ctx);
            self.status = command.title.into();
        } else {
            log::warn!("unknown command {id}");
        }
        self.after_change(cx);
    }

    pub fn activate_tool(&mut self, id: &str, cx: &mut Context<Self>) {
        let previous = self.editor.active_tool;
        if previous != id {
            if let (Some(doc), Some(tool)) = (self.doc.as_mut(), self.registry.tool_mut(previous)) {
                let mut ctx = ToolCtx {
                    doc,
                    state: &mut self.editor,
                };
                tool.on_deactivate(&mut ctx);
            }
        }
        if let Some(tool) = self.registry.tool_mut(id) {
            let id = tool.id();
            let name = tool.name();
            self.editor.active_tool = id;
            self.status = format!("Tool: {name}").into();
            if let (Some(doc), Some(tool)) = (self.doc.as_mut(), self.registry.tool_mut(id)) {
                let mut ctx = ToolCtx {
                    doc,
                    state: &mut self.editor,
                };
                tool.on_activate(&mut ctx);
            }
        }
        self.after_change(cx);
    }

    // ----- input routing -----

    fn panning_tool(&self) -> bool {
        self.space_held || self.editor.active_tool == "hand"
    }

    fn tool_input(&self, local: Point<Pixels>, m: gpui::Modifiers, pressure: f32) -> PointerInput {
        let (x, y) = self.doc_pos(local);
        PointerInput {
            x,
            y,
            pressure,
            modifiers: Modifiers {
                shift: m.shift,
                alt: m.alt,
                ctrl_or_cmd: m.control || m.platform,
            },
        }
    }

    fn on_mouse_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus);
        let local = self.to_local(ev.position);
        if ev.button == MouseButton::Middle || self.panning_tool() {
            self.pan_last = Some(ev.position);
            return;
        }
        if self.editor.active_tool == "zoom" {
            let factor = if ev.modifiers.alt { 1.0 / 1.5 } else { 1.5 };
            self.zoom_by(factor, Some(local));
            cx.notify();
            return;
        }
        if ev.button != MouseButton::Left {
            return;
        }
        self.pointer_down = true;
        let input = self.tool_input(local, ev.modifiers, 1.0);
        let tool_id = self.editor.active_tool;
        if let (Some(doc), Some(tool)) = (self.doc.as_mut(), self.registry.tool_mut(tool_id)) {
            let mut ctx = ToolCtx {
                doc,
                state: &mut self.editor,
            };
            tool.on_pointer_down(&mut ctx, input);
        }
        self.after_change(cx);
    }

    fn on_mouse_move(&mut self, ev: &MouseMoveEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(last) = self.pan_last {
            self.offset = point(
                self.offset.x + (ev.position.x - last.x),
                self.offset.y + (ev.position.y - last.y),
            );
            self.pan_last = Some(ev.position);
            cx.notify();
            return;
        }
        let local = self.to_local(ev.position);
        let input = self.tool_input(local, ev.modifiers, 1.0);
        let tool_id = self.editor.active_tool;
        if let (Some(doc), Some(tool)) = (self.doc.as_mut(), self.registry.tool_mut(tool_id)) {
            let mut ctx = ToolCtx {
                doc,
                state: &mut self.editor,
            };
            tool.on_pointer_move(&mut ctx, input);
        }
        self.after_change(cx);
    }

    fn on_mouse_up(&mut self, ev: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if self.pan_last.take().is_some() {
            return;
        }
        if !self.pointer_down {
            return;
        }
        self.pointer_down = false;
        let local = self.to_local(ev.position);
        let input = self.tool_input(local, ev.modifiers, 1.0);
        let tool_id = self.editor.active_tool;
        if let (Some(doc), Some(tool)) = (self.doc.as_mut(), self.registry.tool_mut(tool_id)) {
            let mut ctx = ToolCtx {
                doc,
                state: &mut self.editor,
            };
            tool.on_pointer_up(&mut ctx, input);
        }
        self.after_change(cx);
    }

    fn on_scroll(&mut self, ev: &ScrollWheelEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let delta = ev.delta.pixel_delta(px(30.0));
        if ev.modifiers.control || ev.modifiers.platform {
            let factor = 1.0 + f32::from(delta.y) / 200.0;
            let local = self.to_local(ev.position);
            self.zoom_by(factor.clamp(0.5, 2.0), Some(local));
        } else {
            self.offset = point(self.offset.x + delta.x, self.offset.y + delta.y);
        }
        cx.notify();
    }

    // ----- modals and numeric fields -----

    pub fn open_modal(&mut self, modal: Modal, cx: &mut Context<Self>) {
        self.modal = Some(modal);
        self.focused_field = None;
        self.field_buffer.clear();
        self.open_popup = None;
        cx.notify();
    }

    pub fn close_modal(&mut self, cx: &mut Context<Self>) {
        self.modal = None;
        self.focused_field = None;
        self.field_buffer.clear();
        self.open_popup = None;
        cx.notify();
    }

    /// Mutate the open modal's state in place.
    pub fn update_modal(&mut self, f: impl FnOnce(&mut Modal)) {
        if let Some(modal) = &mut self.modal {
            f(modal);
        }
    }

    pub fn focus_field(&mut self, id: &'static str) {
        self.focused_field = Some(id);
        self.field_buffer.clear();
    }

    /// Feed a keystroke to the focused numeric field. Returns true when the
    /// field consumed it.
    fn field_key(&mut self, key: &str, text: Option<&str>) -> bool {
        let Some(id) = self.focused_field else {
            return false;
        };
        match key {
            "backspace" => {
                self.field_buffer.pop();
            }
            "escape" => {
                self.focused_field = None;
                self.field_buffer.clear();
                return true;
            }
            "enter" | "tab" => {
                self.commit_field(id);
                return true;
            }
            _ => match text {
                Some(t) if t.chars().all(|c| c.is_ascii_digit()) && !t.is_empty() => {
                    self.field_buffer.push_str(t)
                }
                _ => return false,
            },
        }
        // Apply as you type so the dialog stays live.
        self.commit_field_value(id);
        true
    }

    fn commit_field(&mut self, id: &'static str) {
        self.commit_field_value(id);
        self.focused_field = None;
        self.field_buffer.clear();
    }

    fn commit_field_value(&mut self, id: &'static str) {
        let Ok(value) = self.field_buffer.parse::<f32>() else {
            return;
        };
        let value = value.max(1.0);
        let aspect = self
            .doc
            .as_ref()
            .map(|d| d.width as f32 / d.height.max(1) as f32)
            .unwrap_or(1.0);
        self.update_modal(|m| match m {
            Modal::ImageSize {
                width,
                height,
                link,
                ..
            } => {
                if id == "image-size-w" {
                    *width = value as u32;
                    if *link {
                        *height = (value / aspect).round().max(1.0) as u32;
                    }
                } else if id == "image-size-h" {
                    *height = value as u32;
                    if *link {
                        *width = (value * aspect).round().max(1.0) as u32;
                    }
                }
            }
            Modal::CanvasSize { width, height, .. } => {
                if id == "canvas-size-w" {
                    *width = value as u32;
                } else if id == "canvas-size-h" {
                    *height = value as u32;
                }
            }
            // These dialogs have no typed fields.
            Modal::Filter { .. } | Modal::Adjustment { .. } | Modal::PluginManager => {}
        });
    }

    /// True when the active tool is capturing raw typing.
    pub fn tool_captures_keys(&mut self) -> bool {
        let id = self.editor.active_tool;
        self.registry
            .tool_mut(id)
            .map(|t| t.captures_keys())
            .unwrap_or(false)
    }

    /// Feed a keystroke to the active tool. Returns true if it consumed it.
    fn tool_key(&mut self, ev: &gpui::KeyDownEvent) -> bool {
        let tool_id = self.editor.active_tool;
        let key = ev.keystroke.key.clone();
        let text = ev.keystroke.key_char.clone();
        let modifiers = Modifiers {
            shift: ev.keystroke.modifiers.shift,
            alt: ev.keystroke.modifiers.alt,
            ctrl_or_cmd: ev.keystroke.modifiers.control || ev.keystroke.modifiers.platform,
        };
        let (Some(doc), Some(tool)) = (self.doc.as_mut(), self.registry.tool_mut(tool_id)) else {
            return false;
        };
        let mut ctx = ToolCtx {
            doc,
            state: &mut self.editor,
        };
        tool.on_key(&mut ctx, &key, text.as_deref(), modifiers)
    }

    /// Enter: let the active tool commit its pending gesture.
    pub fn commit_gesture(&mut self, cx: &mut Context<Self>) {
        let tool_id = self.editor.active_tool;
        if let (Some(doc), Some(tool)) = (self.doc.as_mut(), self.registry.tool_mut(tool_id)) {
            let mut ctx = ToolCtx {
                doc,
                state: &mut self.editor,
            };
            tool.on_commit(&mut ctx);
        }
        self.after_change(cx);
    }

    pub fn cancel_gesture(&mut self, cx: &mut Context<Self>) {
        if self.modal.is_some() {
            self.close_modal(cx);
            return;
        }
        if self.open_popup.is_some() {
            self.close_popup(cx);
            return;
        }
        self.pointer_down = false;
        self.pan_last = None;
        let tool_id = self.editor.active_tool;
        if let (Some(doc), Some(tool)) = (self.doc.as_mut(), self.registry.tool_mut(tool_id)) {
            let mut ctx = ToolCtx {
                doc,
                state: &mut self.editor,
            };
            tool.on_cancel(&mut ctx);
        }
        self.after_change(cx);
    }

    // ----- UI support: popups, sliders, thumbnails, history -----

    pub fn toggle_popup(&mut self, popup: Popup, cx: &mut Context<Self>) {
        self.open_popup = if self.open_popup == Some(popup) {
            None
        } else {
            Some(popup)
        };
        cx.notify();
    }

    pub fn close_popup(&mut self, cx: &mut Context<Self>) {
        if self.open_popup.take().is_some() {
            cx.notify();
        }
    }

    pub fn record_slider_bounds(&mut self, id: &'static str, bounds: Bounds<Pixels>) {
        self.slider_bounds.insert(id, bounds);
    }

    /// 0..=1 ratio of a window position along a slider's recorded track.
    pub fn slider_ratio(&self, id: &'static str, window_pos: Point<Pixels>) -> Option<f32> {
        let b = self.slider_bounds.get(id)?;
        let w = f32::from(b.size.width);
        if w <= 0.0 {
            return None;
        }
        Some(((f32::from(window_pos.x) - f32::from(b.origin.x)) / w).clamp(0.0, 1.0))
    }

    pub fn begin_slider(&mut self, id: &'static str, before: f32) {
        self.active_slider = Some((id, before));
    }

    pub fn dragging_slider(&self, id: &'static str) -> bool {
        matches!(self.active_slider, Some((s, _)) if s == id)
    }

    /// End a slider drag, returning the value it started at.
    pub fn end_slider(&mut self, id: &'static str) -> Option<f32> {
        match self.active_slider {
            Some((s, before)) if s == id => {
                self.active_slider = None;
                Some(before)
            }
            _ => None,
        }
    }

    /// Live (history-free) layer opacity update during a slider drag; the
    /// drag commits one undo step on release via `commit_layer_opacity`.
    pub fn set_layer_opacity_live(&mut self, id: photoslop_core::LayerId, value: f32) {
        if let Some(doc) = &mut self.doc {
            let mut bounds = IntRect::EMPTY;
            if let Some(layer) = doc.tree.find_mut(id) {
                layer.opacity = value;
                bounds = layer.content_bounds();
            }
            doc.add_damage(bounds);
        }
    }

    pub fn commit_layer_opacity(
        &mut self,
        id: photoslop_core::LayerId,
        before: f32,
        cx: &mut Context<Self>,
    ) {
        if let Some(doc) = &mut self.doc {
            let after = doc.tree.find(id).map(|l| l.opacity).unwrap_or(before);
            if (after - before).abs() < 1e-4 {
                return;
            }
            // Rewind silently so the edit records the true before state.
            if let Some(layer) = doc.tree.find_mut(id) {
                layer.opacity = before;
            }
            let mut edit = doc.begin_edit("Layer Opacity");
            edit.change_props(id, |l| l.opacity = after);
            edit.commit();
        }
        self.after_change(cx);
    }

    pub fn set_blend_mode(
        &mut self,
        id: photoslop_core::LayerId,
        mode: photoslop_core::BlendMode,
        cx: &mut Context<Self>,
    ) {
        if let Some(doc) = &mut self.doc {
            let mut edit = doc.begin_edit("Blend Mode");
            edit.change_props(id, |l| l.blend = mode);
            edit.commit();
        }
        self.after_change(cx);
    }

    /// Jump in history: negative = undo n steps, positive = redo n steps.
    pub fn history_jump(&mut self, steps: i32, cx: &mut Context<Self>) {
        if let Some(doc) = &mut self.doc {
            if steps < 0 {
                for _ in 0..(-steps) {
                    if doc.undo().is_none() {
                        break;
                    }
                }
            } else {
                for _ in 0..steps {
                    if doc.redo().is_none() {
                        break;
                    }
                }
            }
        }
        self.after_change(cx);
    }

    /// 36x28 thumbnail of a raster layer over a checkerboard, cached per
    /// document revision.
    pub fn layer_thumbnail(&mut self, id: photoslop_core::LayerId) -> Option<Arc<RenderImage>> {
        const TW: usize = 36;
        const TH: usize = 28;
        let doc = self.doc.as_ref()?;
        if let Some((rev, img)) = self.thumbs.get(&id) {
            if *rev == doc.revision {
                return Some(img.clone());
            }
        }
        let layer = doc.tree.find(id)?;
        let raster = layer.as_raster()?;
        let bounds = {
            let b = layer.content_bounds().intersect(&doc.canvas_rect());
            if b.is_empty() {
                doc.canvas_rect()
            } else {
                b
            }
        };
        if bounds.is_empty() {
            return None;
        }
        let scale = (bounds.width() as f32 / TW as f32).max(bounds.height() as f32 / TH as f32);
        let (w, h) = (
            ((bounds.width() as f32 / scale) as usize).clamp(1, TW),
            ((bounds.height() as f32 / scale) as usize).clamp(1, TH),
        );
        let mut bgra = vec![0u8; w * h * 4];
        for ty in 0..h {
            for tx in 0..w {
                let sx = bounds.left + ((tx as f32 + 0.5) * scale) as i32;
                let sy = bounds.top + ((ty as f32 + 0.5) * scale) as i32;
                let px = raster.tiles.pixel(sx, sy).to_u8();
                let (r, g, b, a) = (px[0] as u32, px[1] as u32, px[2] as u32, px[3] as u32);
                let bg = if ((tx >> 2) + (ty >> 2)) & 1 == 0 {
                    0xE0u32
                } else {
                    0xB0u32
                };
                let inv = 255 - a;
                let d = (ty * w + tx) * 4;
                bgra[d] = ((b * a + bg * inv) / 255) as u8;
                bgra[d + 1] = ((g * a + bg * inv) / 255) as u8;
                bgra[d + 2] = ((r * a + bg * inv) / 255) as u8;
                bgra[d + 3] = 255;
            }
        }
        let buffer = image::RgbaImage::from_raw(w as u32, h as u32, bgra)?;
        let img = Arc::new(RenderImage::new(smallvec![image::Frame::new(buffer)]));
        let rev = doc.revision;
        self.thumbs.insert(id, (rev, img.clone()));
        // Drop cache entries for layers that no longer exist.
        if self.thumbs.len() > 64 {
            if let Some(doc) = self.doc.as_ref() {
                self.thumbs.retain(|lid, _| doc.tree.find(*lid).is_some());
            }
        }
        Some(img)
    }

    /// Toggle a group's expanded state (pure UI state, not undoable).
    pub fn toggle_group_open(&mut self, id: photoslop_core::LayerId, cx: &mut Context<Self>) {
        if let Some(doc) = &mut self.doc {
            if let Some(layer) = doc.tree.find_mut(id) {
                if let photoslop_core::LayerKind::Group(g) = &mut layer.kind {
                    g.open = !g.open;
                }
            }
        }
        cx.notify();
    }

    // ----- filters and adjustments -----

    /// Run a registered filter over the active layer, confined to the
    /// selection, as one undoable edit.
    pub fn apply_filter(
        &mut self,
        id: &str,
        values: &photoslop_plugin_api::FilterValues,
        cx: &mut Context<Self>,
    ) {
        let Some(doc) = self.doc.as_mut() else { return };
        let Some(layer_id) = doc.active_layer else {
            self.status = "Select a layer first".into();
            return;
        };
        let Some(filter) = self.registry.filters().find(|f| f.id() == id) else {
            log::warn!("unknown filter {id}");
            return;
        };
        let canvas = doc.canvas_rect();
        let content = doc
            .tree
            .find(layer_id)
            .map(|l| l.content_bounds())
            .unwrap_or(IntRect::EMPTY);
        let region = if doc.selection.is_empty() {
            content.intersect(&canvas)
        } else {
            doc.selection.bounds().intersect(&canvas)
        };
        if region.is_empty() {
            self.status = "Nothing to filter".into();
            return;
        }
        let Some(raster) = doc.tree.find(layer_id).and_then(|l| l.as_raster()) else {
            self.status = "Filters need a pixel layer".into();
            return;
        };

        // Pull the region into a flat buffer, filter it, then blend back
        // through the selection so partial coverage feathers the result.
        let w = region.width() as usize;
        let h = region.height() as usize;
        let mut buf = vec![0.0f32; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let px = raster
                    .tiles
                    .pixel(region.left + x as i32, region.top + y as i32);
                let at = (y * w + x) * 4;
                buf[at] = px.r;
                buf[at + 1] = px.g;
                buf[at + 2] = px.b;
                buf[at + 3] = px.a;
            }
        }
        let original = buf.clone();
        filter.apply(&mut buf, w, h, values);
        let name = filter.name().to_string();
        let selection = doc.selection.clone();

        let mut edit = doc.begin_edit(name.clone());
        for coord in TileCoord::covering(&region) {
            let trect = coord.rect();
            let clip = trect.intersect(&region);
            if clip.is_empty() {
                continue;
            }
            let Some(tile) = edit.writable_tile(layer_id, coord) else {
                break;
            };
            for y in clip.top..clip.bottom {
                for x in clip.left..clip.right {
                    let cov = selection.coverage(x, y) as f32 / 255.0;
                    if cov <= 0.0 {
                        continue;
                    }
                    let src = ((y - region.top) as usize * w + (x - region.left) as usize) * 4;
                    let ix = ((y - trect.top) * TILE_SIZE + (x - trect.left)) as usize;
                    let mix = |a: f32, b: f32| a + (b - a) * cov;
                    tile.set(
                        ix,
                        photoslop_color::Rgba::new(
                            mix(original[src], buf[src]),
                            mix(original[src + 1], buf[src + 1]),
                            mix(original[src + 2], buf[src + 2]),
                            mix(original[src + 3], buf[src + 3]),
                        ),
                    );
                }
            }
        }
        edit.commit();
        self.status = name.into();
        self.after_change(cx);
    }

    /// Enable or disable a third-party plugin.
    pub fn set_plugin_enabled(&mut self, id: String, enabled: bool, cx: &mut Context<Self>) {
        let Some(dir) = photoslop_plugin_host_wasm::PluginManager::plugin_dir() else {
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

    /// Install a plugin file into the plugin directory.
    pub fn install_plugin(&mut self, source: PathBuf, cx: &mut Context<Self>) {
        let Some(dir) = photoslop_plugin_host_wasm::PluginManager::plugin_dir() else {
            return;
        };
        self.status = match photoslop_plugin_host_wasm::PluginManager::install(&source, &dir) {
            Ok(path) => format!("Installed {} — restart to load", path.display()).into(),
            Err(err) => format!("Plugin rejected: {err}").into(),
        };
        cx.notify();
    }

    /// Open a filter's parameter dialog, pre-filled with its defaults.
    pub fn open_filter_dialog(&mut self, id: &'static str, cx: &mut Context<Self>) {
        let Some(filter) = self.registry.filters().find(|f| f.id() == id) else {
            return;
        };
        let values = photoslop_plugin_api::FilterValues::defaults(&filter.params());
        // Filters with no parameters just run.
        if values.0.is_empty() {
            self.apply_filter(id, &values, cx);
            return;
        }
        self.open_modal(Modal::Filter { id, values }, cx);
    }

    /// Insert an adjustment layer above the active layer.
    pub fn add_adjustment(&mut self, kind: photoslop_core::AdjustmentKind, cx: &mut Context<Self>) {
        let params = photoslop_adjustments::Params::default_for(kind);
        let Some(doc) = self.doc.as_mut() else { return };
        let mut layer = Layer::new_raster(kind.display_name());
        layer.kind = photoslop_core::LayerKind::Adjustment(photoslop_core::AdjustmentData {
            kind,
            raw: Vec::new(),
            params_json: serde_json::to_string(&params).ok(),
        });
        let id = layer.id;
        let path = match doc.active_layer.and_then(|a| doc.tree.path_of(a)) {
            Some(mut p) => {
                *p.0.last_mut().unwrap() += 1;
                p
            }
            None => photoslop_core::LayerPath(vec![doc.tree.layers.len()]),
        };
        let mut edit = doc.begin_edit(format!("New {} Layer", kind.display_name()));
        edit.insert_layer(path, layer);
        edit.commit();
        doc.active_layer = Some(id);
        self.status = format!("Added {}", kind.display_name()).into();
        self.after_change(cx);
        // Anything with controls opens its dialog straight away.
        if !params.param_specs().is_empty() {
            let original = (serde_json::to_string(&params).ok(), Vec::new());
            self.open_modal(
                Modal::Adjustment {
                    layer: id,
                    params,
                    original,
                },
                cx,
            );
        }
    }

    /// Write edited parameters back onto an adjustment layer (live, without
    /// a history entry per slider tick).
    pub fn preview_adjustment(
        &mut self,
        layer: photoslop_core::LayerId,
        params: &photoslop_adjustments::Params,
    ) {
        let Some(doc) = self.doc.as_mut() else { return };
        if let Some(photoslop_core::LayerKind::Adjustment(data)) =
            doc.tree.find_mut(layer).map(|l| &mut l.kind)
        {
            data.params_json = serde_json::to_string(params).ok();
        }
        doc.damage_all();
    }

    /// Commit edited adjustment parameters as one history entry.
    pub fn commit_adjustment(
        &mut self,
        layer: photoslop_core::LayerId,
        params: &photoslop_adjustments::Params,
        original: (Option<String>, Vec<u8>),
        cx: &mut Context<Self>,
    ) {
        let after = (serde_json::to_string(params).ok(), Vec::new());
        if let Some(doc) = self.doc.as_mut() {
            // Put the pre-dialog state back first so the recorded edit has
            // the right "before"; the live preview already moved the layer.
            if let Some(photoslop_core::LayerKind::Adjustment(data)) =
                doc.tree.find_mut(layer).map(|l| &mut l.kind)
            {
                data.params_json = original.0.clone();
                data.raw = original.1.clone();
            }
            let mut edit = doc.begin_edit(format!("{} Settings", params.display_name()));
            // Editing parameters supersedes the preserved PSD payload, so
            // the writer emits our values rather than stale bytes.
            edit.record_adjustment_params(layer, original, after);
            edit.commit();
        }
        self.status = format!("{} updated", params.display_name()).into();
        self.after_change(cx);
    }

    /// Discard a live adjustment preview (dialog cancelled).
    pub fn revert_adjustment(
        &mut self,
        layer: photoslop_core::LayerId,
        original: (Option<String>, Vec<u8>),
        cx: &mut Context<Self>,
    ) {
        if let Some(doc) = self.doc.as_mut() {
            if let Some(photoslop_core::LayerKind::Adjustment(data)) =
                doc.tree.find_mut(layer).map(|l| &mut l.kind)
            {
                data.params_json = original.0;
                data.raw = original.1;
            }
            doc.damage_all();
        }
        self.after_change(cx);
    }

    /// Open the parameter dialog for an existing adjustment layer.
    pub fn edit_adjustment(&mut self, layer: photoslop_core::LayerId, cx: &mut Context<Self>) {
        let Some(doc) = self.doc.as_ref() else { return };
        let Some(photoslop_core::LayerKind::Adjustment(data)) =
            doc.tree.find(layer).map(|l| &l.kind)
        else {
            return;
        };
        let params = data
            .params_json
            .as_deref()
            .and_then(|j| serde_json::from_str(j).ok())
            .unwrap_or_else(|| photoslop_adjustments::parse_psd(data.kind, &data.raw));
        if params.param_specs().is_empty() {
            self.status = format!("{} has no editable settings", params.display_name()).into();
            return;
        }
        let original = (data.params_json.clone(), data.raw.clone());
        self.open_modal(
            Modal::Adjustment {
                layer,
                params,
                original,
            },
            cx,
        );
    }

    // ----- painting -----

    /// Convert a straight-alpha RGBA tile to GPUI's premultiplied BGRA,
    /// compositing transparency over the checkerboard inside the canvas and
    /// over the app background outside it.
    fn tile_to_render_image(rgba: &[u8], coord: TileCoord, canvas_rect: IntRect) -> RenderImage {
        let trect = coord.rect();
        let mut bgra = vec![0u8; rgba.len()];
        for p in 0..(TILE_SIZE * TILE_SIZE) as usize {
            let x = trect.left + (p as i32 % TILE_SIZE);
            let y = trect.top + (p as i32 / TILE_SIZE);
            let s = &rgba[p * 4..p * 4 + 4];
            let (r, g, b, a) = (s[0] as u32, s[1] as u32, s[2] as u32, s[3] as u32);
            let bg = if canvas_rect.contains(x, y) {
                // 8px checkerboard.
                if ((x >> 3) + (y >> 3)) & 1 == 0 {
                    0xFFu32
                } else {
                    0xCCu32
                }
            } else {
                0x26u32 // outside the document: app background
            };
            let inv = 255 - a;
            let d = &mut bgra[p * 4..p * 4 + 4];
            d[0] = ((b * a + bg * inv) / 255) as u8;
            d[1] = ((g * a + bg * inv) / 255) as u8;
            d[2] = ((r * a + bg * inv) / 255) as u8;
            d[3] = 255;
        }
        let buffer = image::RgbaImage::from_raw(TILE_SIZE as u32, TILE_SIZE as u32, bgra).unwrap();
        RenderImage::new(smallvec![image::Frame::new(buffer)])
    }

    fn tile_image(&mut self, coord: TileCoord) -> Option<Arc<RenderImage>> {
        if let Some(img) = self.tile_images.get(&coord) {
            return Some(img.clone());
        }
        let doc = self.doc.as_ref()?;
        let canvas_rect = doc.canvas_rect();
        let rgba = self.cache.get(doc, coord);
        let img = Arc::new(Self::tile_to_render_image(&rgba, coord, canvas_rect));
        self.tile_images.insert(coord, img.clone());
        Some(img)
    }

    fn refresh_preview(&mut self) -> Option<Arc<RenderImage>> {
        let doc = self.doc.as_ref()?;
        let (w, h) = (
            (doc.width >> PREVIEW_SHIFT).max(1),
            (doc.height >> PREVIEW_SHIFT).max(1),
        );
        let full = IntRect::from_size(doc.width, doc.height);
        if !self.preview.valid || self.preview.w != w || self.preview.h != h {
            self.preview.buf = vec![0u8; (w * h * 4) as usize];
            self.preview.w = w;
            self.preview.h = h;
            self.preview.dirty = vec![full];
            self.preview.valid = true;
        }
        let dirty = std::mem::take(&mut self.preview.dirty);
        if dirty.is_empty() {
            if let Some(img) = &self.preview.image {
                return Some(img.clone());
            }
        }
        let step = 1i32 << PREVIEW_SHIFT;
        for rect in dirty {
            let rect = rect.intersect(&full);
            if rect.is_empty() {
                continue;
            }
            let rgba = photoslop_compositor::composite_region_rgba8(doc, rect);
            let rw = rect.width() as usize;
            // Point-sample the full-res composite into the preview buffer.
            let px0 = rect.left.div_euclid(step).max(0);
            let py0 = rect.top.div_euclid(step).max(0);
            let px1 = ((rect.right - 1).div_euclid(step) + 1).min(w as i32);
            let py1 = ((rect.bottom - 1).div_euclid(step) + 1).min(h as i32);
            for py in py0..py1 {
                let sy = (py * step + step / 2).clamp(rect.top, rect.bottom - 1);
                for pxx in px0..px1 {
                    let sx = (pxx * step + step / 2).clamp(rect.left, rect.right - 1);
                    let s = (((sy - rect.top) as usize * rw) + (sx - rect.left) as usize) * 4;
                    let (r, g, b, a) = (
                        rgba[s] as u32,
                        rgba[s + 1] as u32,
                        rgba[s + 2] as u32,
                        rgba[s + 3] as u32,
                    );
                    let bg = if ((sx >> 3) + (sy >> 3)) & 1 == 0 {
                        0xFFu32
                    } else {
                        0xCCu32
                    };
                    let inv = 255 - a;
                    let d = ((py as u32 * w + pxx as u32) * 4) as usize;
                    self.preview.buf[d] = ((b * a + bg * inv) / 255) as u8;
                    self.preview.buf[d + 1] = ((g * a + bg * inv) / 255) as u8;
                    self.preview.buf[d + 2] = ((r * a + bg * inv) / 255) as u8;
                    self.preview.buf[d + 3] = 255;
                }
            }
        }
        let buffer = image::RgbaImage::from_raw(w, h, self.preview.buf.clone())?;
        let img = Arc::new(RenderImage::new(smallvec![image::Frame::new(buffer)]));
        self.preview.image = Some(img.clone());
        Some(img)
    }

    /// Everything the paint closure needs, computed with &mut self.
    fn prepare_paint(&mut self, bounds: Bounds<Pixels>, scale_factor: f32) -> PaintJob {
        self.canvas_bounds = bounds;
        let mut job = PaintJob::default();
        let Some(doc) = self.doc.as_ref() else {
            return job;
        };
        let canvas_rect = doc.canvas_rect();
        let zoom = self.zoom;
        // Immutable-phase data, captured by value so the mutable phase
        // below (preview/tile-image builds) doesn't fight the borrows.
        let sel_bounds = (!doc.selection.is_empty() && !doc.selection.bounds().is_empty())
            .then(|| doc.selection.bounds());
        let tool_id = self.editor.active_tool;
        let overlays = self
            .registry
            .tool_mut(tool_id)
            .map(|t| t.overlays(doc, &self.editor))
            .unwrap_or_default();
        let origin = (
            f32::from(bounds.origin.x) + f32::from(self.offset.x),
            f32::from(bounds.origin.y) + f32::from(self.offset.y),
        );
        let to_screen = move |x: f32, y: f32| -> Point<Pixels> {
            point(px(origin.0 + x * zoom), px(origin.1 + y * zoom))
        };
        // Snap a document-space coordinate to the device-pixel grid.
        // Abutting quads MUST share bit-identical edges or fractional
        // zoom/pan leaves sub-pixel gaps between tiles that show through
        // as hairline seams; computing every tile's edges through this one
        // function (same input -> same output) guarantees they meet.
        let sf = scale_factor.max(0.01);
        let snap_x = move |x: f32| ((origin.0 + x * zoom) * sf).round() / sf;
        let snap_y = move |y: f32| ((origin.1 + y * zoom) * sf).round() / sf;
        let snapped_bounds = move |rect: IntRect| -> Bounds<Pixels> {
            let x0 = snap_x(rect.left as f32);
            let x1 = snap_x(rect.right as f32);
            let y0 = snap_y(rect.top as f32);
            let y1 = snap_y(rect.bottom as f32);
            Bounds {
                origin: point(px(x0), px(y0)),
                size: size(px(x1 - x0), px(y1 - y0)),
            }
        };

        if zoom <= PREVIEW_ZOOM_CUTOFF {
            if let Some(img) = self.refresh_preview() {
                job.tiles.push((snapped_bounds(canvas_rect), img));
            }
        } else {
            // Visible document rect.
            let (dx0, dy0) = self.doc_pos(point(px(0.0), px(0.0)));
            let (dx1, dy1) = self.doc_pos(point(bounds.size.width, bounds.size.height));
            let visible = IntRect::new(
                dx0.floor() as i32 - 1,
                dy0.floor() as i32 - 1,
                dx1.ceil() as i32 + 1,
                dy1.ceil() as i32 + 1,
            )
            .intersect(&canvas_rect);
            let coords: Vec<TileCoord> = TileCoord::covering(&visible).collect();
            if let Some(doc) = self.doc.as_ref() {
                self.cache.prewarm(doc, &coords);
            }
            for coord in coords {
                if let Some(img) = self.tile_image(coord) {
                    job.tiles.push((snapped_bounds(coord.rect()), img));
                }
            }
        }

        // Document border.
        job.outlines
            .push((snapped_bounds(canvas_rect), gpui::rgb(0x000000).into()));

        // Selection marching-ants (static dashes for now: alternating
        // black/white nested outlines).
        if let Some(b) = sel_bounds {
            let bounds_px = snapped_bounds(b);
            job.outlines.push((bounds_px, gpui::rgb(0xFFFFFF).into()));
            job.outlines.push((
                Bounds {
                    origin: point(bounds_px.origin.x - px(1.0), bounds_px.origin.y - px(1.0)),
                    size: size(
                        bounds_px.size.width + px(2.0),
                        bounds_px.size.height + px(2.0),
                    ),
                },
                gpui::rgb(0x000000).into(),
            ));
        }

        // Active tool overlays.
        for overlay in overlays {
            match overlay {
                Overlay::Rect(r) | Overlay::AntsRect(r) => {
                    job.outlines.push((
                        Bounds {
                            origin: to_screen(r.left as f32, r.top as f32),
                            size: size(px(r.width() as f32 * zoom), px(r.height() as f32 * zoom)),
                        },
                        gpui::rgb(0x44AAFF).into(),
                    ));
                }
                Overlay::AntsPolygon(points) => {
                    let pts: Vec<Point<Pixels>> =
                        points.iter().map(|&(x, y)| to_screen(x, y)).collect();
                    job.polylines.push(pts);
                }
                Overlay::Line { x1, y1, x2, y2 } => {
                    job.polylines
                        .push(vec![to_screen(x1, y1), to_screen(x2, y2)]);
                }
                Overlay::Circle { cx: ccx, cy, r } => {
                    let d = r * 2.0 * zoom;
                    job.circles.push(Bounds {
                        origin: to_screen(ccx - r, cy - r),
                        size: size(px(d), px(d)),
                    });
                }
            }
        }
        job
    }

    pub fn render_canvas(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        div()
            .id("canvas")
            .flex_grow()
            .size_full()
            .overflow_hidden()
            .bg(gpui::rgb(0x262626))
            .track_focus(&self.focus)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|ws, ev, w, cx| ws.on_mouse_down(ev, w, cx)),
            )
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(|ws, ev, w, cx| ws.on_mouse_down(ev, w, cx)),
            )
            .on_mouse_move(cx.listener(|ws, ev, w, cx| ws.on_mouse_move(ev, w, cx)))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|ws, ev, w, cx| ws.on_mouse_up(ev, w, cx)),
            )
            .on_mouse_up(
                MouseButton::Middle,
                cx.listener(|ws, ev, w, cx| ws.on_mouse_up(ev, w, cx)),
            )
            .on_scroll_wheel(cx.listener(|ws, ev, w, cx| ws.on_scroll(ev, w, cx)))
            .on_key_down(cx.listener(|ws, ev: &gpui::KeyDownEvent, _w, cx| {
                if ws.field_key(&ev.keystroke.key, ev.keystroke.key_char.as_deref()) {
                    cx.notify();
                    cx.stop_propagation();
                    return;
                }
                if ws.tool_key(ev) {
                    ws.after_change(cx);
                    cx.stop_propagation();
                    return;
                }
                if ev.keystroke.key == "space" && !ev.is_held {
                    ws.space_held = true;
                    cx.notify();
                }
            }))
            .on_key_up(cx.listener(|ws, ev: &gpui::KeyUpEvent, _w, cx| {
                if ev.keystroke.key == "space" {
                    ws.space_held = false;
                    ws.pan_last = None;
                    cx.notify();
                }
            }))
            .child(
                canvas(
                    move |bounds, window, cx| {
                        let scale = window.scale_factor();
                        entity.update(cx, |ws, _| ws.prepare_paint(bounds, scale))
                    },
                    move |_bounds, job: PaintJob, window, _cx| {
                        for (bounds, img) in job.tiles {
                            let _ =
                                window.paint_image(bounds, gpui::Corners::default(), img, 0, false);
                        }
                        for (bounds, color) in job.outlines {
                            window.paint_quad(gpui::outline(
                                bounds,
                                color,
                                gpui::BorderStyle::Solid,
                            ));
                        }
                        for pts in job.polylines {
                            if pts.len() < 2 {
                                continue;
                            }
                            let mut pb = PathBuilder::stroke(px(1.0));
                            pb.move_to(pts[0]);
                            for p in &pts[1..] {
                                pb.line_to(*p);
                            }
                            if let Ok(path) = pb.build() {
                                window.paint_path(path, gpui::rgb(0xFFFFFF));
                            }
                        }
                        for bounds in job.circles {
                            let r = bounds.size.width / 2.0;
                            window.paint_quad(gpui::quad(
                                bounds,
                                r,
                                gpui::transparent_black(),
                                px(1.0),
                                gpui::rgb(0xEEEEEE),
                                gpui::BorderStyle::Solid,
                            ));
                        }
                    },
                )
                .size_full(),
            )
    }
}

#[derive(Default)]
pub struct PaintJob {
    tiles: Vec<(Bounds<Pixels>, Arc<RenderImage>)>,
    outlines: Vec<(Bounds<Pixels>, gpui::Hsla)>,
    polylines: Vec<Vec<Point<Pixels>>>,
    circles: Vec<Bounds<Pixels>>,
}

impl Focusable for Workspace {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some((id, enabled)) = self.pending_plugin_toggle.take() {
            self.set_plugin_enabled(id, enabled, cx);
        }
        let captures_keys = self.tool_captures_keys() || self.modal.is_some();
        let modal = crate::dialogs::render(self, cx);
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(gpui::rgb(0x1E1E1E))
            .text_color(gpui::rgb(0xD8D8D8))
            .text_size(px(12.0))
            // While a tool is capturing typing the context loses "editable",
            // which is what single-letter shortcuts are bound against — so
            // letters reach the tool instead of switching tools.
            .key_context(if captures_keys {
                "Workspace"
            } else {
                "Workspace editable"
            })
            .on_action(cx.listener(|ws, action: &RunCommand, _w, cx| {
                ws.run_command(&action.id.clone(), cx);
            }))
            .on_action(cx.listener(|ws, action: &ActivateTool, _w, cx| {
                ws.activate_tool(&action.id.clone(), cx);
            }))
            .on_action(cx.listener(|ws, action: &SetToolOpacity, _w, cx| {
                ws.editor.tool_opacity = action.percent as f32 / 100.0;
                ws.status = format!("Opacity: {}%", action.percent).into();
                cx.notify();
            }))
            .on_action(cx.listener(|ws, _: &NewFile, _w, cx| {
                ws.new_document();
                cx.notify();
            }))
            .on_action(cx.listener(|ws, _: &OpenFile, window, cx| {
                keymap::open_file_dialog(ws, window, cx);
            }))
            .on_action(cx.listener(|ws, _: &SaveFile, window, cx| {
                ws.save_current(window, cx);
            }))
            .on_action(cx.listener(|ws, _: &SaveFileAs, window, cx| {
                keymap::save_file_dialog(ws, window, cx);
            }))
            .on_action(cx.listener(|ws, _: &ZoomIn, _w, cx| {
                ws.zoom_by(1.25, None);
                cx.notify();
            }))
            .on_action(cx.listener(|ws, _: &ZoomOut, _w, cx| {
                ws.zoom_by(0.8, None);
                cx.notify();
            }))
            .on_action(cx.listener(|ws, _: &ZoomFit, _w, cx| {
                ws.fit_to_view();
                cx.notify();
            }))
            .on_action(cx.listener(|ws, _: &ZoomActual, _w, cx| {
                ws.zoom = 1.0;
                ws.editor.zoom = 1.0;
                ws.center();
                cx.notify();
            }))
            .on_action(cx.listener(|ws, _: &BrushSmaller, _w, cx| {
                ws.editor.brush_size = (ws.editor.brush_size / 1.25).max(1.0);
                cx.notify();
            }))
            .on_action(cx.listener(|ws, _: &BrushLarger, _w, cx| {
                ws.editor.brush_size = (ws.editor.brush_size * 1.25).min(500.0);
                cx.notify();
            }))
            .on_action(cx.listener(|ws, _: &SwapColors, _w, cx| {
                std::mem::swap(&mut ws.editor.foreground, &mut ws.editor.background);
                cx.notify();
            }))
            .on_action(cx.listener(|ws, _: &DefaultColors, _w, cx| {
                ws.editor.foreground = photoslop_color::Rgba::BLACK;
                ws.editor.background = photoslop_color::Rgba::WHITE;
                cx.notify();
            }))
            .on_action(cx.listener(|ws, _: &CancelGesture, _w, cx| {
                ws.cancel_gesture(cx);
            }))
            .on_action(cx.listener(|ws, _: &CommitGesture, _w, cx| {
                ws.commit_gesture(cx);
            }))
            .on_action(cx.listener(|ws, _: &ShowImageSize, _w, cx| {
                if let Some(doc) = ws.doc.as_ref() {
                    let modal = Modal::ImageSize {
                        width: doc.width,
                        height: doc.height,
                        filter: ws.editor.resample,
                        link: true,
                    };
                    ws.open_modal(modal, cx);
                }
            }))
            .on_action(cx.listener(|ws, _: &ShowCanvasSize, _w, cx| {
                if let Some(doc) = ws.doc.as_ref() {
                    let modal = Modal::CanvasSize {
                        width: doc.width,
                        height: doc.height,
                        anchor: (0.5, 0.5),
                    };
                    ws.open_modal(modal, cx);
                }
            }))
            .on_action(|_: &Quit, _w, cx| cx.quit())
            .child(panels::menu_bar(self, cx))
            .child(panels::tool_options_bar(self, cx))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_grow()
                    .min_h(px(0.0))
                    .child(panels::toolbar(self, cx))
                    .child(self.render_canvas(cx))
                    .child(panels::side_panels(self, cx)),
            )
            .child(panels::status_bar(self))
            .children(modal)
    }
}
