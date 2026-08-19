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
    pub fn new(registry: PluginRegistry, cx: &mut Context<Self>) -> Self {
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
        };
        ws.new_document();
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

    pub fn save_file_as(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let result = (|| -> anyhow::Result<()> {
            let doc = self
                .doc
                .as_mut()
                .ok_or_else(|| anyhow::anyhow!("no document"))?;
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("png")
                .to_ascii_lowercase();
            let codec = self
                .registry
                .codecs()
                .find(|c| c.can_export() && c.extensions().contains(&ext.as_str()))
                .ok_or_else(|| anyhow::anyhow!("no exporter for .{ext} (PSD save lands in M6)"))?;
            let bytes = codec.export(doc)?;
            std::fs::write(&path, bytes)?;
            doc.dirty = false;
            doc.path = Some(path.clone());
            Ok(())
        })();
        self.status = match result {
            Ok(()) => format!("Saved {}", path.display()).into(),
            Err(err) => {
                log::error!("save failed: {err:#}");
                format!("Save failed: {err}").into()
            }
        };
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
            self.editor.active_tool = id;
            self.status = format!("Tool: {}", tool.name()).into();
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

    pub fn cancel_gesture(&mut self, cx: &mut Context<Self>) {
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
    fn prepare_paint(&mut self, bounds: Bounds<Pixels>) -> PaintJob {
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

        if zoom <= PREVIEW_ZOOM_CUTOFF {
            if let Some(img) = self.refresh_preview() {
                let origin = to_screen(0.0, 0.0);
                let sz = size(
                    px(canvas_rect.width() as f32 * zoom),
                    px(canvas_rect.height() as f32 * zoom),
                );
                job.tiles.push((Bounds { origin, size: sz }, img));
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
                    let trect = coord.rect();
                    let origin = to_screen(trect.left as f32, trect.top as f32);
                    let sz = size(px(TILE_SIZE as f32 * zoom), px(TILE_SIZE as f32 * zoom));
                    job.tiles.push((Bounds { origin, size: sz }, img));
                }
            }
        }

        // Document border.
        job.outlines.push((
            Bounds {
                origin: to_screen(0.0, 0.0),
                size: size(
                    px(canvas_rect.width() as f32 * zoom),
                    px(canvas_rect.height() as f32 * zoom),
                ),
            },
            gpui::rgb(0x000000).into(),
        ));

        // Selection marching-ants (static dashes for now: alternating
        // black/white nested outlines).
        if let Some(b) = sel_bounds {
            let bounds_px = Bounds {
                origin: to_screen(b.left as f32, b.top as f32),
                size: size(px(b.width() as f32 * zoom), px(b.height() as f32 * zoom)),
            };
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
                    move |bounds, _window, cx| entity.update(cx, |ws, _| ws.prepare_paint(bounds)),
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
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(gpui::rgb(0x1E1E1E))
            .text_color(gpui::rgb(0xD8D8D8))
            .text_size(px(12.0))
            .key_context("Workspace")
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
            .on_action(|_: &Quit, _w, cx| cx.quit())
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
    }
}
