//! Edit-menu operations that write pixels: paths to fill/stroke/
//! selection, Fill, Stroke, Content-Aware Fill and Scale, and the
//! Filter Gallery.

use super::*;

impl Workspace {
    /// Rasterize the active path: fill it, stroke it, or turn it into a
    /// selection. The three things Photoshop's Paths panel buttons do.
    pub fn use_active_path(&mut self, op: PathOp, cx: &mut Context<Self>) {
        let Some(doc) = self.doc.as_ref() else { return };
        let Some(path) = doc.active_path.and_then(|i| doc.paths.get(i)).cloned() else {
            self.status = "No path to use".into();
            cx.notify();
            return;
        };
        if path.is_empty() {
            self.status = "The path is empty".into();
            cx.notify();
            return;
        }
        let flat = schist_tools_vector::paths::flatten(&path);
        let colour = self.editor.foreground;
        let width = self.editor.brush_size.max(1.0);
        let Some(doc) = self.doc.as_mut() else { return };
        match op {
            PathOp::Fill => {
                schist_tools_vector::fill_path(
                    doc,
                    &flat,
                    colour,
                    schist_vector::FillRule::NonZero,
                    "Fill Path",
                );
            }
            PathOp::Stroke => {
                let stroked = schist_vector::stroke_path(
                    &flat,
                    schist_vector::StrokeStyle::new(width)
                        .with_cap(schist_vector::LineCap::Round)
                        .with_join(schist_vector::LineJoin::Round),
                );
                schist_tools_vector::fill_path(
                    doc,
                    &stroked,
                    colour,
                    schist_vector::FillRule::NonZero,
                    "Stroke Path",
                );
            }
            PathOp::Select => {
                let rect = flat.bounds();
                let mask = schist_vector::rasterize(&flat, rect, schist_vector::FillRule::NonZero);
                let w = rect.width().max(0) as usize;
                let mut edit = doc.begin_edit("Make Selection");
                edit.change_selection(|sel, _| {
                    sel.deselect();
                    sel.activate();
                    sel.apply_shape(rect, schist_core::SelectOp::Replace, |x, y| {
                        mask[(y - rect.top) as usize * w + (x - rect.left) as usize]
                    });
                });
                edit.commit();
            }
            PathOp::Delete => {
                if let Some(i) = doc.active_path {
                    doc.paths.remove(i);
                    doc.active_path = if doc.paths.is_empty() {
                        None
                    } else {
                        Some(i.min(doc.paths.len() - 1))
                    };
                }
                doc.damage_all();
            }
        }
        self.status = op.title().into();
        self.after_change(cx);
    }

    /// Edit ▸ Stroke: paint a band along the selection's edge.
    pub fn stroke_selection(
        &mut self,
        width: f32,
        position: schist_core::StrokePosition,
        cx: &mut Context<Self>,
    ) {
        let colour = self.editor.foreground;
        let Some(doc) = self.doc.as_mut() else { return };
        if doc.selection.is_empty() {
            self.status = "Stroke needs a selection".into();
            cx.notify();
            return;
        }
        let Some(layer) = doc.active_layer else {
            return;
        };
        // Border() already builds the band; asking it for the right
        // position is a matter of which side of the edge to take.
        let canvas = doc.canvas_rect();
        let mut band = doc.selection.clone();
        let w = width.round().max(1.0) as i32;
        match position {
            schist_core::StrokePosition::Inside => {
                let mut inner = band.clone();
                inner.contract(w, canvas);
                subtract_into(&mut band, &inner, canvas);
            }
            schist_core::StrokePosition::Outside => {
                let mut outer = band.clone();
                outer.expand(w, canvas);
                let inner = band.clone();
                subtract_into(&mut outer, &inner, canvas);
                band = outer;
            }
            schist_core::StrokePosition::Center => band.border(w, canvas),
        }
        let rect = band.bounds().intersect(&canvas);
        if rect.is_empty() {
            self.status = "Nothing to stroke".into();
            cx.notify();
            return;
        }
        let mut edit = doc.begin_edit("Stroke");
        for coord in TileCoord::covering(&rect) {
            let trect = coord.rect();
            let clip = trect.intersect(&rect);
            if clip.is_empty() {
                continue;
            }
            let Some(tile) = edit.writable_tile(layer, coord) else {
                break;
            };
            for y in clip.top..clip.bottom {
                for x in clip.left..clip.right {
                    let cov = band.coverage(x, y) as f32 / 255.0;
                    if cov <= 0.0 {
                        continue;
                    }
                    let ix = ((y - trect.top) * TILE_SIZE + (x - trect.left)) as usize;
                    let under = tile.get(ix);
                    tile.set(
                        ix,
                        Rgba {
                            a: colour.a * cov,
                            ..colour
                        }
                        .over(under),
                    );
                }
            }
        }
        edit.commit();
        self.status = "Stroke".into();
        self.after_change(cx);
    }

    /// Edit ▸ Fill.
    pub fn fill_selection(&mut self, source: FillSource, opacity: f32, cx: &mut Context<Self>) {
        let colour = match source {
            FillSource::Foreground => self.editor.foreground,
            FillSource::Background => self.editor.background,
            FillSource::Black => Rgba::new(0.0, 0.0, 0.0, 1.0),
            FillSource::White => Rgba::new(1.0, 1.0, 1.0, 1.0),
            FillSource::Gray => Rgba::new(0.5, 0.5, 0.5, 1.0),
            FillSource::ContentAware => {
                self.content_aware_fill(cx);
                return;
            }
        };
        let Some(doc) = self.doc.as_mut() else { return };
        let Some(layer) = doc.active_layer else {
            return;
        };
        let canvas = doc.canvas_rect();
        let rect = if doc.selection.is_empty() {
            canvas
        } else {
            doc.selection.bounds().intersect(&canvas)
        };
        if rect.is_empty() {
            return;
        }
        let selection = doc.selection.clone();
        let mut edit = doc.begin_edit("Fill");
        for coord in TileCoord::covering(&rect) {
            let trect = coord.rect();
            let clip = trect.intersect(&rect);
            if clip.is_empty() {
                continue;
            }
            let Some(tile) = edit.writable_tile(layer, coord) else {
                break;
            };
            for y in clip.top..clip.bottom {
                for x in clip.left..clip.right {
                    let cov = selection.coverage(x, y) as f32 / 255.0 * opacity;
                    if cov <= 0.0 {
                        continue;
                    }
                    let ix = ((y - trect.top) * TILE_SIZE + (x - trect.left)) as usize;
                    let under = tile.get(ix);
                    tile.set(
                        ix,
                        Rgba {
                            a: colour.a * cov,
                            ..colour
                        }
                        .over(under),
                    );
                }
            }
        }
        edit.commit();
        self.status = "Fill".into();
        self.after_change(cx);
    }

    /// Edit ▸ Content-Aware Fill: grow the surroundings over the selection.
    pub fn content_aware_fill(&mut self, cx: &mut Context<Self>) {
        let Some(doc) = self.doc.as_mut() else { return };
        if doc.selection.is_empty() {
            self.status = "Content-Aware Fill needs a selection".into();
            cx.notify();
            return;
        }
        let Some(layer) = doc.active_layer else {
            return;
        };
        let canvas = doc.canvas_rect();
        let sel_rect = doc.selection.bounds().intersect(&canvas);
        // Work over a margin so the fill has surroundings to grow from.
        let rect = IntRect::new(
            sel_rect.left - 16,
            sel_rect.top - 16,
            sel_rect.right + 16,
            sel_rect.bottom + 16,
        )
        .intersect(&canvas);
        let Some(tiles) = doc
            .tree
            .find(layer)
            .and_then(|l| l.as_raster())
            .map(|r| r.tiles.clone())
        else {
            self.status = "Content-Aware Fill needs a pixel layer".into();
            cx.notify();
            return;
        };
        let selection = doc.selection.clone();
        let (w, h) = (rect.width().max(0) as usize, rect.height().max(0) as usize);
        let mut hole = vec![false; w * h];
        for y in 0..h {
            for x in 0..w {
                hole[y * w + x] =
                    selection.coverage(rect.left + x as i32, rect.top + y as i32) >= 128;
            }
        }
        let filled = schist_tools_retouch::inpaint(&tiles, rect, &hole);
        let mut edit = doc.begin_edit("Content-Aware Fill");
        for coord in TileCoord::covering(&rect) {
            let trect = coord.rect();
            let clip = trect.intersect(&rect);
            if clip.is_empty() {
                continue;
            }
            let Some(tile) = edit.writable_tile(layer, coord) else {
                break;
            };
            for y in clip.top..clip.bottom {
                for x in clip.left..clip.right {
                    let cov = selection.coverage(x, y) as f32 / 255.0;
                    if cov <= 0.0 {
                        continue;
                    }
                    let ix = ((y - trect.top) * TILE_SIZE + (x - trect.left)) as usize;
                    let under = tile.get(ix);
                    let patch = filled[(y - rect.top) as usize * w + (x - rect.left) as usize];
                    tile.set(
                        ix,
                        Rgba {
                            r: under.r + (patch.r - under.r) * cov,
                            g: under.g + (patch.g - under.g) * cov,
                            b: under.b + (patch.b - under.b) * cov,
                            a: under.a + (patch.a - under.a) * cov,
                        },
                    );
                }
            }
        }
        edit.commit();
        self.status = "Content-Aware Fill".into();
        self.after_change(cx);
    }

    /// Edit ▸ Content-Aware Scale: resize the canvas by carving seams
    /// rather than squashing everything equally.
    pub fn content_aware_scale(&mut self, width: u32, height: u32, cx: &mut Context<Self>) {
        let Some(doc) = self.doc.as_mut() else { return };
        if width == 0 || height == 0 || (width, height) == (doc.width, doc.height) {
            return;
        }
        let canvas = doc.canvas_rect();
        // The selection marks what to protect, as Photoshop's Protect
        // channel does.
        let protect = (!doc.selection.is_empty()).then(|| doc.selection.clone());
        let depth = doc.depth;
        let ids: Vec<schist_core::LayerId> = doc.tree.iter().map(|l| l.id).collect();
        let mut carved: Vec<(schist_core::LayerId, schist_core::TileMap)> = Vec::new();
        for id in &ids {
            let Some(raster) = doc.tree.find(*id).and_then(|l| l.as_raster()) else {
                continue;
            };
            let mut img = schist_tools_warp::scale::Image::from_tiles(
                &raster.tiles,
                canvas,
                protect.as_ref(),
            );
            img.content_aware_resize(width as usize, height as usize);
            carved.push((
                *id,
                img.into_tiles(IntRect::from_size(width, height), depth),
            ));
        }
        let mut edit = doc.begin_edit("Content-Aware Scale");
        for (id, tiles) in carved {
            edit.replace_layer_tiles(id, tiles);
        }
        edit.set_canvas_size(width, height);
        edit.change_selection(|sel, _| sel.deselect());
        edit.commit();
        self.status = "Content-Aware Scale".into();
        self.fit_to_view();
        self.after_change(cx);
    }

    /// Open the Filter Gallery.
    pub fn show_filter_gallery(&mut self, cx: &mut Context<Self>) {
        if !self.begin_filter_preview() {
            cx.notify();
            return;
        }
        // Start with one filter so the panel has something to show.
        let first = self
            .registry
            .filters()
            .find(|f| f.category() == "Stylize")
            .or_else(|| self.registry.filters().find(|f| !f.runs_out_of_process()));
        let stack = first
            .map(|f| {
                vec![GalleryEntry {
                    id: f.id(),
                    values: schist_plugin_api::FilterValues::defaults(&f.params()),
                    enabled: true,
                }]
            })
            .unwrap_or_default();
        self.preview_gallery(&stack, cx);
        self.open_modal(
            Modal::FilterGallery {
                stack,
                selected: 0,
                preview: true,
            },
            cx,
        );
    }

    /// Run a gallery stack over the preview snapshot, bottom to top.
    pub(super) fn run_gallery(&self, stack: &[GalleryEntry], buf: &mut [f32], w: usize, h: usize) {
        for entry in stack {
            if !entry.enabled {
                continue;
            }
            let Some(filter) = self.registry.filters().find(|f| f.id() == entry.id) else {
                continue;
            };
            // The gallery hands over the toolbox colours and nothing
            // else: a stack has no one layer to read a backdrop for, and
            // choosing a map per entry would need a picker per entry.
            let context = schist_plugin_api::FilterContext {
                foreground: self.editor.foreground,
                background: self.editor.background,
                ..Default::default()
            };
            filter.apply_with(buf, w, h, &entry.values, &context);
        }
    }

    pub fn preview_gallery(&mut self, stack: &[GalleryEntry], cx: &mut Context<Self>) {
        let Some(preview) = self.filter_preview.clone() else {
            return;
        };
        let (w, h) = (
            preview.region.width() as usize,
            preview.region.height() as usize,
        );
        let mut buf = preview.original.clone();
        self.run_gallery(stack, &mut buf, w, h);
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

    /// Bake the stack in as one history entry.
    pub fn commit_gallery(&mut self, stack: &[GalleryEntry], cx: &mut Context<Self>) {
        // Restore first so the recorded edit has the right "before".
        self.preview_gallery(&[], cx);
        let Some(preview) = self.filter_preview.take() else {
            return;
        };
        let (w, h) = (
            preview.region.width() as usize,
            preview.region.height() as usize,
        );
        let mut buf = preview.original.clone();
        self.run_gallery(stack, &mut buf, w, h);
        self.write_region(
            preview.layer,
            preview.region,
            &preview.original,
            &buf,
            "Filter Gallery",
            true,
        );
        self.status = "Filter Gallery".into();
        self.after_change(cx);
    }
}
