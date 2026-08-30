//! Assembling the visible tiles into one image for the canvas element.

use super::*;

impl Workspace {
    /// Which document pixels can land on screen at the current zoom, pan
    /// and rotation, clipped to the canvas. `width`/`height` are the
    /// canvas element's size in device pixels.
    pub(super) fn visible_doc_rect(
        &self,
        width: usize,
        height: usize,
        scale_factor: f32,
        canvas_rect: IntRect,
    ) -> IntRect {
        let sf = scale_factor.max(0.01);
        let inv_zoom = 1.0 / self.zoom;
        let origin = (f32::from(self.offset.x) * sf, f32::from(self.offset.y) * sf);
        // Rotation is about the middle of the viewport, which is what
        // makes spinning the view feel like turning a sheet of paper.
        let centre = (width as f32 / 2.0, height as f32 / 2.0);
        let (rs, rc) = (-self.rotation).sin_cos();
        let doc_at = |dx: f32, dy: f32| -> (f32, f32) {
            let (ox, oy) = (dx - centre.0, dy - centre.1);
            let (rx, ry) = (ox * rc - oy * rs + centre.0, ox * rs + oy * rc + centre.1);
            (
                (rx - origin.0) * inv_zoom / sf,
                (ry - origin.1) * inv_zoom / sf,
            )
        };
        // With rotation, the visible region is the union of all four
        // corners rather than the span between two of them.
        let mut span = IntRect::EMPTY;
        for (cx, cy) in [
            (0.0, 0.0),
            (width as f32, 0.0),
            (width as f32, height as f32),
            (0.0, height as f32),
        ] {
            let (x, y) = doc_at(cx, cy);
            span = span.union(&IntRect::new(
                x.floor() as i32 - 1,
                y.floor() as i32 - 1,
                x.ceil() as i32 + 1,
                y.ceil() as i32 + 1,
            ));
        }
        span.intersect(&canvas_rect)
    }

    /// Assemble everything visible into one BGRA image the size of the
    /// canvas element, resampling on the way.
    ///
    /// Integer zooms stay nearest-neighbour so pixels stay crisp (what an
    /// image editor wants); fractional and rotated views interpolate, and
    /// zooming out averages the whole pixel footprint to damp aliasing.
    /// Transparency is checkered at a fixed *screen* size, like Photoshop.
    pub(super) fn assemble_viewport(
        &mut self,
        bounds: Bounds<Pixels>,
        scale_factor: f32,
    ) -> Option<Arc<RenderImage>> {
        let sf = scale_factor.max(0.01);
        let width = (f32::from(bounds.size.width) * sf).round().max(1.0) as usize;
        let height = (f32::from(bounds.size.height) * sf).round().max(1.0) as usize;
        // A sanity cap: a hostile window size shouldn't allocate gigabytes.
        if width * height > 64 << 20 {
            return None;
        }
        let doc = self.doc.as_ref()?;
        let revision = doc.revision;
        let canvas_rect = doc.canvas_rect();
        let key = ViewportKey {
            revision,
            zoom: self.zoom.to_bits(),
            offset: (
                (f32::from(self.offset.x) * sf).round() as i32,
                (f32::from(self.offset.y) * sf).round() as i32,
            ),
            size: (width as u32, height as u32),
            color_epoch: self.color_epoch,
            rotation: self.rotation.to_bits(),
            surround: crate::ui::palette().canvas_bg,
        };
        if let Some((cached_key, image)) = &self.viewport_image {
            if *cached_key == key {
                return Some(image.clone());
            }
        }

        // Which document pixels can land on screen?
        let zoom = self.zoom;
        let origin = (f32::from(self.offset.x) * sf, f32::from(self.offset.y) * sf);
        let visible = self.visible_doc_rect(width, height, sf, canvas_rect);
        if visible.is_empty() {
            // Nothing but background: a 1x1 image keeps the paint path simple.
            let s = (key.surround & 0xFF) as u8;
            let buffer = image::RgbaImage::from_raw(1, 1, vec![s, s, s, 255])?;
            let img = Arc::new(RenderImage::new(smallvec![image::Frame::new(buffer)]));
            self.viewport_image = Some((key, img.clone()));
            return Some(img);
        }

        // Composite the visible tiles, then index them by grid position so
        // sampling is an array lookup rather than a hash per pixel.
        let coords: Vec<TileCoord> = TileCoord::covering(&visible).collect();
        if let Some(doc) = self.doc.as_ref() {
            self.cache.prewarm(doc, &coords);
        }
        let (tx0, ty0) = (
            visible.left.div_euclid(TILE_SIZE),
            visible.top.div_euclid(TILE_SIZE),
        );
        let cols = ((visible.right - 1).div_euclid(TILE_SIZE) - tx0 + 1).max(1) as usize;
        let rows = ((visible.bottom - 1).div_euclid(TILE_SIZE) - ty0 + 1).max(1) as usize;
        let mut grid: Vec<Option<Arc<Vec<u8>>>> = vec![None; cols * rows];
        for coord in coords {
            let ix = (coord.ty - ty0) as usize * cols + (coord.tx - tx0) as usize;
            if let Some(slot) = grid.get_mut(ix) {
                *slot = self.display_tile(coord);
            }
        }

        // The rest of the document renders during idle time, nearest tiles
        // first, so scrolling lands on warm caches instead of popping in.
        self.rebuild_prefetch_queue(canvas_rect, visible, false);

        // Resample on the active backend (GPU when installed and the grid
        // fits its buffers), with the CPU reference as the always-correct
        // fallback. Both implement the same contract — see
        // `schist_compositor::viewport`.
        let params = schist_compositor::viewport::ViewportParams {
            width,
            height,
            origin,
            zoom,
            scale_factor: sf,
            rotation: self.rotation,
            canvas: canvas_rect,
            grid_origin: (tx0, ty0),
            grid_cols: cols,
            grid_rows: rows,
            surround: key.surround,
        };
        let bgra = schist_compositor::backend()
            .viewport(&params, &grid)
            .unwrap_or_else(|| schist_compositor::viewport::render_viewport_cpu(&params, &grid));

        let buffer = image::RgbaImage::from_raw(width as u32, height as u32, bgra)?;
        let img = Arc::new(RenderImage::new(smallvec![image::Frame::new(buffer)]));
        // Release the previous frame's atlas slot.
        if let Some((_, old)) = self.viewport_image.replace((key, img.clone())) {
            self.retired_images.push(old);
        }
        Some(img)
    }

    /// Mid-gesture stand-in for `assemble_viewport`: the previous frame's
    /// image, positioned so GPUI stretches it to the current zoom and pan
    /// on its GPU. Resampling the document and uploading a fresh
    /// full-viewport texture on every wheel tick is what made zooming lag;
    /// a slightly soft frame is invisible while the view is in motion, and
    /// the settle timer rebuilds it crisp the moment the hand stops.
    ///
    /// Old and new transforms share the rotation about the viewport
    /// centre, and uniform scaling commutes with rotation, so old-screen
    /// to new-screen composes to scale-plus-translate:
    /// `s1 = k*s0 + (1-k)*c + R((k-1)*c - k*o0 + o1)`. One axis-aligned
    /// quad is therefore exact even for a turned view.
    pub(super) fn gesture_viewport_quad(
        &self,
        bounds: Bounds<Pixels>,
        scale_factor: f32,
    ) -> Option<(Bounds<Pixels>, Arc<RenderImage>)> {
        if !self.view_gesture_active {
            return None;
        }
        let (key, image) = self.viewport_image.as_ref()?;
        let doc = self.doc.as_ref()?;
        let sf = scale_factor.max(0.01);
        let width = (f32::from(bounds.size.width) * sf).round().max(1.0) as u32;
        let height = (f32::from(bounds.size.height) * sf).round().max(1.0) as u32;
        // Reuse is only sound when everything but zoom and pan matches
        // what the image was built for.
        if key.revision != doc.revision
            || key.size != (width, height)
            || key.color_epoch != self.color_epoch
            || key.rotation != self.rotation.to_bits()
            || key.surround != crate::ui::palette().canvas_bg
        {
            return None;
        }
        let z0 = f32::from_bits(key.zoom);
        if !z0.is_finite() || z0 <= 0.0 {
            return None;
        }
        let k = self.zoom / z0;
        let c = (
            f32::from(bounds.size.width) / 2.0,
            f32::from(bounds.size.height) / 2.0,
        );
        let o0 = (key.offset.0 as f32 / sf, key.offset.1 as f32 / sf);
        let o1 = (f32::from(self.offset.x), f32::from(self.offset.y));
        let (ux, uy) = (
            (k - 1.0) * c.0 - k * o0.0 + o1.0,
            (k - 1.0) * c.1 - k * o0.1 + o1.1,
        );
        let (rs, rc) = self.rotation.sin_cos();
        let t = (
            (1.0 - k) * c.0 + ux * rc - uy * rs,
            (1.0 - k) * c.1 + ux * rs + uy * rc,
        );
        Some((
            Bounds {
                origin: point(bounds.origin.x + px(t.0), bounds.origin.y + px(t.1)),
                size: size(
                    px(f32::from(bounds.size.width) * k),
                    px(f32::from(bounds.size.height) * k),
                ),
            },
            image.clone(),
        ))
    }

    /// Can the real frame be rebuilt mid-gesture without stalling? True
    /// when the gesture is a pure pan — zoom and rotation match the last
    /// full frame — and every visible tile is already composited and
    /// colour-managed, so `assemble_viewport` is just a resample. Panning
    /// then renders crisp on every tick and fills ground the stale image
    /// never covered, instead of flashing surround until the hand stops;
    /// the stale-quad path remains for zooming (where every tick
    /// invalidates the whole frame) and for scrolls that outrun the
    /// prefetch into cold tiles.
    pub(super) fn warm_pan_frame_ready(
        &self,
        bounds: Bounds<Pixels>,
        scale_factor: f32,
        canvas_rect: IntRect,
    ) -> bool {
        let Some((key, _)) = &self.viewport_image else {
            return false;
        };
        if key.zoom != self.zoom.to_bits() || key.rotation != self.rotation.to_bits() {
            return false;
        }
        let sf = scale_factor.max(0.01);
        let width = (f32::from(bounds.size.width) * sf).round().max(1.0) as usize;
        let height = (f32::from(bounds.size.height) * sf).round().max(1.0) as usize;
        let visible = self.visible_doc_rect(width, height, sf, canvas_rect);
        TileCoord::covering(&visible)
            .all(|c| self.cache.contains(c) && self.display_tiles.contains_key(&c))
    }

    pub(super) fn refresh_preview(&mut self) -> Option<Arc<RenderImage>> {
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
            let rgba = schist_compositor::composite_region_rgba8(doc, rect);
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
}
