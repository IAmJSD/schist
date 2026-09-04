//! Whole-image and selection operations: Select ▸ Modify, Color
//! Range, destructive adjustments, Auto Tone/Contrast/Color, canvas
//! rotation, Trim, and colour mode.

use super::*;

impl Workspace {
    /// Run a Select ▸ Modify operation as one history entry.
    pub fn apply_select_modify(&mut self, kind: ModifyKind, amount: f32, cx: &mut Context<Self>) {
        let Some(doc) = self.doc.as_mut() else { return };
        if doc.selection.is_empty() {
            self.status = "Select something first".into();
            cx.notify();
            return;
        }
        let n = amount.round().max(0.0) as i32;
        let mut edit = doc.begin_edit(kind.title());
        edit.change_selection(|sel, canvas| match kind {
            ModifyKind::Expand => sel.expand(n, canvas),
            ModifyKind::Contract => sel.contract(n, canvas),
            ModifyKind::Border => sel.border(n, canvas),
            ModifyKind::Smooth => sel.smooth(n, canvas),
            ModifyKind::Feather => sel.feather(amount.max(0.0)),
        });
        edit.commit();
        self.status = kind.title().into();
        self.after_change(cx);
    }

    /// Select ▸ Color Range: every pixel within `tolerance` of `target`.
    pub fn apply_color_range(&mut self, tolerance: f32, target: Rgba, cx: &mut Context<Self>) {
        let Some(doc) = self.doc.as_mut() else { return };
        let Some(raster) = doc
            .active_layer
            .and_then(|id| doc.tree.find(id))
            .and_then(|l| l.as_raster())
        else {
            self.status = "Color Range needs a pixel layer".into();
            cx.notify();
            return;
        };
        let canvas = doc.canvas_rect();
        let tol = tolerance / 255.0;
        // Coverage falls off across the tolerance band rather than
        // cutting hard, which is what makes Photoshop's Fuzziness feather
        // the edges of a colour selection.
        let mut cov = vec![0u8; (canvas.width() * canvas.height()) as usize];
        let w = canvas.width() as usize;
        for y in canvas.top..canvas.bottom {
            for x in canvas.left..canvas.right {
                let c = raster.tiles.pixel(x, y);
                let d = (c.r - target.r)
                    .abs()
                    .max((c.g - target.g).abs())
                    .max((c.b - target.b).abs());
                let v = if tol <= 0.0 {
                    if d == 0.0 {
                        1.0
                    } else {
                        0.0
                    }
                } else {
                    (1.0 - d / tol).clamp(0.0, 1.0)
                };
                cov[(y - canvas.top) as usize * w + (x - canvas.left) as usize] =
                    (v * 255.0).round() as u8;
            }
        }
        let mut edit = doc.begin_edit("Color Range");
        edit.change_selection(|sel, canvas| {
            sel.deselect();
            sel.activate();
            sel.apply_shape(canvas, schist_core::SelectOp::Replace, |x, y| {
                cov[(y - canvas.top) as usize * w + (x - canvas.left) as usize]
            });
        });
        edit.commit();
        self.status = "Color Range".into();
        self.after_change(cx);
    }

    /// Image ▸ Adjustments: apply an adjustment straight onto the active
    /// layer's pixels, rather than adding a layer for it.
    ///
    /// Opens the same dialog as the adjustment layers do, but previewing
    /// writes pixels; that is what "destructive" means here.
    pub fn apply_adjustment_destructive(
        &mut self,
        kind: schist_core::AdjustmentKind,
        cx: &mut Context<Self>,
    ) {
        let params = schist_adjustments::Params::default_for(kind);
        if !self.begin_filter_preview() {
            cx.notify();
            return;
        }
        self.open_modal(
            Modal::DestructiveAdjustment {
                kind,
                params: Box::new(params),
                preview: true,
            },
            cx,
        );
    }

    /// Re-run a destructive adjustment's preview from the snapshot.
    pub fn preview_destructive_adjustment(
        &mut self,
        params: Option<&schist_adjustments::Params>,
        cx: &mut Context<Self>,
    ) {
        let Some(preview) = self.filter_preview.clone() else {
            return;
        };
        let mut buf = preview.original.clone();
        if let Some(params) = params {
            params.apply_buffer(&mut buf);
        }
        self.write_region(
            preview.layer,
            preview.region,
            &preview.original,
            &buf,
            "",
            false,
        );
        self.after_change(cx);
    }

    /// Commit a destructive adjustment as one history entry.
    pub fn commit_destructive_adjustment(
        &mut self,
        kind: schist_core::AdjustmentKind,
        params: &schist_adjustments::Params,
        cx: &mut Context<Self>,
    ) {
        // Put the previewed pixels back first so the edit records the
        // right "before".
        self.preview_destructive_adjustment(None, cx);
        let Some(preview) = self.filter_preview.take() else {
            return;
        };
        let mut buf = preview.original.clone();
        params.apply_buffer(&mut buf);
        let name = kind.display_name().to_string();
        self.write_region(
            preview.layer,
            preview.region,
            &preview.original,
            &buf,
            &name,
            true,
        );
        self.status = name.into();
        self.after_change(cx);
    }

    /// Image ▸ Auto Tone / Auto Contrast / Auto Color.
    ///
    /// All three stretch the histogram to fill the range; they differ in
    /// whether the channels are stretched together (contrast, preserving
    /// the colour cast) or apart (tone and colour, removing it), and
    /// whether the midpoint is re-centred (colour).
    pub fn auto_adjust(&mut self, mode: AutoMode, cx: &mut Context<Self>) {
        if !self.begin_filter_preview() {
            cx.notify();
            return;
        }
        let Some(preview) = self.filter_preview.take() else {
            return;
        };
        let mut buf = preview.original.clone();
        // Photoshop clips half a percent off each end so a handful of
        // stray pixels cannot flatten the whole stretch.
        const CLIP: f32 = 0.005;
        let mut lo = [1.0f32; 3];
        let mut hi = [0.0f32; 3];
        for ch in 0..3 {
            let mut vals: Vec<f32> = buf
                .as_chunks::<4>()
                .0
                .iter()
                .filter(|p| p[3] > 0.0)
                .map(|p| p[ch])
                .collect();
            if vals.is_empty() {
                self.status = "Nothing to adjust".into();
                cx.notify();
                return;
            }
            vals.sort_by(|a, b| a.total_cmp(b));
            let n = vals.len();
            lo[ch] = vals[((n as f32 * CLIP) as usize).min(n - 1)];
            hi[ch] = vals[((n as f32 * (1.0 - CLIP)) as usize).min(n - 1)];
        }
        if mode == AutoMode::Contrast {
            // One stretch for all three channels keeps the colour cast.
            let l = lo[0].min(lo[1]).min(lo[2]);
            let h = hi[0].max(hi[1]).max(hi[2]);
            lo = [l; 3];
            hi = [h; 3];
        }
        // Auto Color additionally pulls each channel's midtone to neutral
        // grey, which is the only thing distinguishing it from Auto Tone.
        // This used to be `v.powf(1.0)`, the identity, so the two menu
        // items produced byte-identical results.
        let mut gamma = [1.0f32; 3];
        if mode == AutoMode::Color {
            let mut sum = [0.0f64; 3];
            let mut n = 0u64;
            for p in buf.as_chunks::<4>().0 {
                if p[3] <= 0.0 {
                    continue;
                }
                for ch in 0..3 {
                    let span = (hi[ch] - lo[ch]).max(1e-4);
                    sum[ch] += f64::from(((p[ch] - lo[ch]) / span).clamp(0.0, 1.0));
                }
                n += 1;
            }
            if n > 0 {
                for ch in 0..3 {
                    let mean = (sum[ch] / n as f64) as f32;
                    // Solve mean^gamma = 0.5 for gamma, so the channel's
                    // midtone lands on neutral grey. Clamped so a nearly
                    // black or white channel cannot explode.
                    if mean > 1e-3 && mean < 1.0 - 1e-3 {
                        gamma[ch] = (0.5f32.ln() / mean.ln()).clamp(0.2, 5.0);
                    }
                }
            }
        }
        for p in buf.as_chunks_mut::<4>().0 {
            if p[3] <= 0.0 {
                continue;
            }
            for ch in 0..3 {
                let span = (hi[ch] - lo[ch]).max(1e-4);
                let mut v = ((p[ch] - lo[ch]) / span).clamp(0.0, 1.0);
                if gamma[ch] != 1.0 {
                    v = v.powf(gamma[ch]);
                }
                p[ch] = v;
            }
        }
        let name = mode.title();
        self.write_region(
            preview.layer,
            preview.region,
            &preview.original,
            &buf,
            name,
            true,
        );
        self.status = name.into();
        self.after_change(cx);
    }

    /// Image ▸ Image Rotation, and the flip entries under Edit ▸ Transform.
    pub fn transform_canvas(&mut self, op: CanvasTransform, cx: &mut Context<Self>) {
        let Some(doc) = self.doc.as_mut() else { return };
        transform_document(doc, op);
        self.status = op.title().into();
        self.fit_to_view();
        self.after_change(cx);
    }

    /// Image ▸ Trim: crop away uniform borders.
    pub fn trim(&mut self, cx: &mut Context<Self>) {
        let Some(doc) = self.doc.as_ref() else { return };
        let canvas = doc.canvas_rect();
        // What counts as "border" is the colour of the top-left pixel of
        // the composited image, or transparency where there is none.
        let flat = schist_compositor::composite_region_rgba8(doc, canvas);
        let w = canvas.width() as usize;
        let at = |x: i32, y: i32| -> [u8; 4] {
            let i = (y as usize * w + x as usize) * 4;
            [flat[i], flat[i + 1], flat[i + 2], flat[i + 3]]
        };
        let key = at(0, 0);
        let same = |p: [u8; 4]| p == key || (p[3] == 0 && key[3] == 0);
        let mut keep = IntRect::EMPTY;
        for y in 0..canvas.height() {
            for x in 0..canvas.width() {
                if !same(at(x, y)) {
                    keep = keep.union(&IntRect::new(x, y, x + 1, y + 1));
                }
            }
        }
        if keep.is_empty() || keep == canvas {
            self.status = "Nothing to trim".into();
            cx.notify();
            return;
        }
        self.resize_canvas_to(keep, cx);
    }

    /// Crop the document to `rect`, moving every layer with it.
    pub fn resize_canvas_to(&mut self, rect: IntRect, cx: &mut Context<Self>) {
        let Some(doc) = self.doc.as_mut() else { return };
        schist_tools_transform::crop_to(doc, rect);
        self.status = "Trimmed".into();
        self.fit_to_view();
        self.after_change(cx);
    }

    /// Image ▸ Mode: switch the document between RGB and Grayscale.
    pub fn set_color_mode(&mut self, mode: schist_color::ColorMode, cx: &mut Context<Self>) {
        let Some(doc) = self.doc.as_mut() else { return };
        if doc.mode == mode {
            return;
        }
        // Indexed Color needs palette quantisation, which does not exist.
        // It used to fall into the greyscale branch below, desaturating
        // the image and labelling the history entry "Grayscale", so the
        // menu item claimed to do something it had never implemented.
        if mode == schist_color::ColorMode::Indexed {
            self.status = "Indexed Color is not supported yet".into();
            cx.notify();
            return;
        }
        if mode == schist_color::ColorMode::Grayscale {
            // Flatten colour out of every layer, which is what the mode
            // change actually means for the pixels.
            let ids: Vec<schist_core::LayerId> = doc.tree.iter().map(|l| l.id).collect();
            let coords_by_layer: Vec<(schist_core::LayerId, Vec<TileCoord>)> = ids
                .iter()
                .filter_map(|id| {
                    doc.tree
                        .find(*id)
                        .and_then(|l| l.as_raster())
                        .map(|r| (*id, r.tiles.iter().map(|(c, _)| *c).collect()))
                })
                .collect();
            let mut edit = doc.begin_edit("Grayscale");
            for (id, coords) in coords_by_layer {
                for coord in coords {
                    let Some(tile) = edit.writable_tile(id, coord) else {
                        break;
                    };
                    for ix in 0..schist_core::TILE_PIXELS {
                        let p = tile.get(ix);
                        let l = 0.299 * p.r + 0.587 * p.g + 0.114 * p.b;
                        tile.set(ix, schist_color::Rgba::new(l, l, l, p.a));
                    }
                }
            }
            edit.set_color_mode(mode);
            edit.commit();
        } else {
            // CMYK/Lab/RGB change nothing but the mode, which still has
            // to be undoable: it used to produce no history entry at all.
            let mut edit = doc.begin_edit(mode.display_name().to_string());
            edit.set_color_mode(mode);
            edit.commit();
        }
        if let Some(doc) = self.doc.as_mut() {
            doc.damage_all();
        }
        self.status = mode.display_name().into();
        self.after_change(cx);
    }
}

/// Turn or flip a whole document, every raster layer with it, as one
/// history entry. The gallery's batch run uses this on documents that
/// never reach the editor.
pub(super) fn transform_document(doc: &mut Document, op: CanvasTransform) {
    let (w, h) = (doc.width, doc.height);
    let swaps = matches!(op, CanvasTransform::Cw90 | CanvasTransform::Ccw90);
    let (nw, nh) = if swaps { (h, w) } else { (w, h) };
    // Read every layer's pixels first: the mapping reads from the old
    // geometry while writing the new one.
    let ids: Vec<schist_core::LayerId> = doc.tree.iter().map(|l| l.id).collect();
    let sources: Vec<(schist_core::LayerId, schist_core::TileMap)> = ids
        .iter()
        .filter_map(|id| {
            doc.tree
                .find(*id)
                .and_then(|l| l.as_raster())
                .map(|r| (*id, r.tiles.clone()))
        })
        .collect();
    let mut edit = doc.begin_edit(op.title());
    edit.set_canvas_size(nw, nh);
    // Document furniture is not layer content, so rotate and flip it with
    // the pixels. Quarter turns also swap horizontal and vertical guides.
    let (fw, fh) = (w as f32, h as f32);
    edit.map_geometry(
        |x, y| match op {
            CanvasTransform::Cw90 => (fh - y, x),
            CanvasTransform::Ccw90 => (y, fw - x),
            CanvasTransform::Rotate180 => (fw - x, fh - y),
            CanvasTransform::FlipH => (fw - x, y),
            CanvasTransform::FlipV => (x, fh - y),
        },
        swaps,
    );
    for (id, src) in &sources {
        for coord in TileCoord::covering(&IntRect::from_size(nw, nh)) {
            let trect = coord.rect();
            let Some(tile) = edit.writable_tile(*id, coord) else {
                break;
            };
            for y in trect.top..trect.bottom {
                for x in trect.left..trect.right {
                    // Where this destination pixel came from.
                    let (sx, sy) = match op {
                        CanvasTransform::Cw90 => (y, nw as i32 - 1 - x),
                        CanvasTransform::Ccw90 => (nh as i32 - 1 - y, x),
                        CanvasTransform::Rotate180 => (w as i32 - 1 - x, h as i32 - 1 - y),
                        CanvasTransform::FlipH => (w as i32 - 1 - x, y),
                        CanvasTransform::FlipV => (x, h as i32 - 1 - y),
                    };
                    let ix = ((y - trect.top) * TILE_SIZE + (x - trect.left)) as usize;
                    tile.set(ix, src.pixel(sx, sy));
                }
            }
        }
    }
    edit.commit();
}
