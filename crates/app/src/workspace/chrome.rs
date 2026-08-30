//! Small pieces of window chrome: popups, sliders, thumbnails, blend
//! mode and opacity, and history navigation.

use super::*;

impl Workspace {
    // ----- UI support: popups, sliders, thumbnails, history -----

    pub fn toggle_popup(&mut self, popup: Popup, cx: &mut Context<Self>) {
        self.open_submenu.clear();
        self.open_popup = if self.open_popup == Some(popup) {
            None
        } else {
            Some(popup)
        };
        cx.notify();
    }

    pub fn close_popup(&mut self, cx: &mut Context<Self>) {
        self.open_submenu.clear();
        if self.open_popup.take().is_some() {
            cx.notify();
        }
    }

    pub fn record_slider_bounds(&mut self, id: &'static str, bounds: Bounds<Pixels>) {
        self.slider_bounds.insert(id, bounds);
    }

    /// Position within a recorded box as a 0..=1 pair, with y measured
    /// upwards so it matches how a curve is drawn.
    /// Hand back an image that has just been replaced, so its atlas slot
    /// is freed once the frame that still references it has been painted.
    pub fn retire_image(&mut self, image: Arc<RenderImage>) {
        self.retired_images.push(image);
    }

    pub fn box_position(&self, id: &'static str, window_pos: Point<Pixels>) -> Option<(f32, f32)> {
        let b = self.slider_bounds.get(id)?;
        let (w, h) = (f32::from(b.size.width), f32::from(b.size.height));
        if w <= 0.0 || h <= 0.0 {
            return None;
        }
        let x = (f32::from(window_pos.x) - f32::from(b.origin.x)) / w;
        let y = (f32::from(window_pos.y) - f32::from(b.origin.y)) / h;
        Some((x.clamp(0.0, 1.0), (1.0 - y).clamp(0.0, 1.0)))
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
    pub fn set_layer_opacity_live(&mut self, id: schist_core::LayerId, value: f32) {
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
        id: schist_core::LayerId,
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
        id: schist_core::LayerId,
        mode: schist_core::BlendMode,
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
    pub fn layer_thumbnail(&mut self, id: schist_core::LayerId) -> Option<Arc<RenderImage>> {
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

    /// Set an absolute zoom level about the viewport centre.
    pub fn set_zoom(&mut self, zoom: f32) {
        let factor = zoom / self.zoom.max(1e-6);
        self.zoom_by(factor, None);
    }

    /// A small thumbnail of the whole document for the navigator, cached
    /// against the document revision.
    pub fn document_thumbnail(&mut self) -> Option<Arc<RenderImage>> {
        const MAX: u32 = 220;
        let doc = self.doc.as_ref()?;
        let revision = doc.revision;
        if let Some((rev, img)) = &self.nav_thumb {
            if *rev == revision {
                return Some(img.clone());
            }
        }
        let scale = (doc.width as f32 / MAX as f32).max(doc.height as f32 / 84.0);
        let (w, h) = (
            ((doc.width as f32 / scale) as u32).clamp(1, MAX),
            ((doc.height as f32 / scale) as u32).clamp(1, 84),
        );
        // Point-sample through the shared tile cache rather than
        // compositing the whole canvas at full resolution: tiles the
        // viewport has already composited are reused, an edit
        // recomposites only the tiles it damaged, and no canvas-sized
        // full-resolution buffer ever exists.
        let sxs: Vec<u32> = (0..w)
            .map(|tx| (((tx as f32 + 0.5) * scale) as u32).min(doc.width - 1))
            .collect();
        let sys: Vec<u32> = (0..h)
            .map(|ty| (((ty as f32 + 0.5) * scale) as u32).min(doc.height - 1))
            .collect();
        let dedup = |v: &[u32]| {
            let mut t: Vec<i32> = v
                .iter()
                .map(|&s| (s as i32).div_euclid(TILE_SIZE))
                .collect();
            t.dedup();
            t
        };
        let (tcols, trows) = (dedup(&sxs), dedup(&sys));
        let coords: Vec<TileCoord> = trows
            .iter()
            .flat_map(|&ty| tcols.iter().map(move |&tx| TileCoord { tx, ty }))
            .collect();
        self.cache.prewarm(doc, &coords);
        let mut bgra = vec![0u8; (w * h * 4) as usize];
        for ty in 0..h {
            for tx in 0..w {
                let (sx, sy) = (sxs[tx as usize] as i32, sys[ty as usize] as i32);
                let tile = self.cache.get(
                    doc,
                    TileCoord {
                        tx: sx.div_euclid(TILE_SIZE),
                        ty: sy.div_euclid(TILE_SIZE),
                    },
                );
                let s = ((sy.rem_euclid(TILE_SIZE) * TILE_SIZE + sx.rem_euclid(TILE_SIZE)) * 4)
                    as usize;
                let (r, g, b, a) = (
                    tile[s] as u32,
                    tile[s + 1] as u32,
                    tile[s + 2] as u32,
                    tile[s + 3] as u32,
                );
                let bg = if ((tx >> 2) + (ty >> 2)) & 1 == 0 {
                    0xE0
                } else {
                    0xB0
                };
                let inv = 255 - a;
                let d = ((ty * w + tx) * 4) as usize;
                bgra[d] = ((b * a + bg * inv) / 255) as u8;
                bgra[d + 1] = ((g * a + bg * inv) / 255) as u8;
                bgra[d + 2] = ((r * a + bg * inv) / 255) as u8;
                bgra[d + 3] = 255;
            }
        }
        let buffer = image::RgbaImage::from_raw(w, h, bgra)?;
        let img = Arc::new(RenderImage::new(smallvec![image::Frame::new(buffer)]));
        self.nav_thumb = Some((revision, img.clone()));
        Some(img)
    }

    /// Toggle a group's expanded state (pure UI state, not undoable).
    pub fn toggle_group_open(&mut self, id: schist_core::LayerId, cx: &mut Context<Self>) {
        if let Some(doc) = &mut self.doc {
            if let Some(layer) = doc.tree.find_mut(id) {
                if let schist_core::LayerKind::Group(g) = &mut layer.kind {
                    g.open = !g.open;
                }
            }
        }
        cx.notify();
    }
}
