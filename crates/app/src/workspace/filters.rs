//! Running filters: the region they see, the context they get, and
//! the preview/apply cycle.

use super::*;

const CAMERA_RAW_FILTER: &str = "filter.camera_raw";
const RAW_PREVIEW_DEBOUNCE_MS: u64 = 120;

fn settings_from_values(values: &schist_plugin_api::FilterValues) -> schist_core::RawSettings {
    schist_core::RawSettings {
        temperature: values.get("temperature"),
        tint: values.get("tint"),
        exposure: values.get("exposure"),
        contrast: values.get("contrast"),
        highlights: values.get("highlights"),
        shadows: values.get("shadows"),
        whites: values.get("whites"),
        blacks: values.get("blacks"),
        clarity: values.get("clarity"),
        dehaze: values.get("dehaze"),
        vibrance: values.get("vibrance"),
        saturation: values.get("saturation"),
        sharpening: values.get("sharpening"),
        noise: values.get("noise"),
        vignette: values.get("vignette"),
    }
    .sanitized()
}

fn values_from_settings(
    settings: schist_core::RawSettings,
    values: &mut schist_plugin_api::FilterValues,
) {
    let settings = settings.sanitized();
    for (key, value) in [
        ("temperature", settings.temperature),
        ("tint", settings.tint),
        ("exposure", settings.exposure),
        ("contrast", settings.contrast),
        ("highlights", settings.highlights),
        ("shadows", settings.shadows),
        ("whites", settings.whites),
        ("blacks", settings.blacks),
        ("clarity", settings.clarity),
        ("dehaze", settings.dehaze),
        ("vibrance", settings.vibrance),
        ("saturation", settings.saturation),
        ("sharpening", settings.sharpening),
        ("noise", settings.noise),
        ("vignette", settings.vignette),
    ] {
        values.set(key, value);
    }
}

/// Develop the sensor-domain controls and then run the remaining Camera Raw
/// controls over that fresh render. The three controls already consumed by
/// the RAW pipeline are zeroed so they are not applied twice.
fn render_raw_capture(
    source: Arc<[u8]>,
    settings: schist_core::RawSettings,
    quality: schist_codecs_common::raw::RawQuality,
    filter: Arc<dyn schist_plugin_api::FilterPlugin>,
    mut values: schist_plugin_api::FilterValues,
) -> anyhow::Result<schist_codecs_common::raw::DevelopedRaw> {
    let mut developed = schist_codecs_common::raw::develop_rgba(&source, settings, quality)?;
    values.set("temperature", 0.0);
    values.set("tint", 0.0);
    values.set("exposure", 0.0);
    filter.apply(
        &mut developed.rgba,
        developed.width,
        developed.height,
        &values,
    );
    Ok(developed)
}

impl Workspace {
    // ----- filters and adjustments -----

    /// Fill Camera Raw controls from the development attached to the active
    /// layer. Ordinary pixel layers retain the filter's declared defaults.
    pub(super) fn seed_raw_filter_values(
        &self,
        id: &str,
        values: &mut schist_plugin_api::FilterValues,
    ) {
        if id != CAMERA_RAW_FILTER {
            return;
        }
        let settings = self
            .doc
            .as_ref()
            .and_then(|doc| doc.active_layer.and_then(|id| doc.tree.find(id)))
            .and_then(|layer| layer.raw.as_deref())
            .map(|raw| raw.settings);
        if let Some(settings) = settings {
            values_from_settings(settings, values);
        }
    }

    /// Whether Camera Raw means re-developing the active layer's original
    /// capture rather than destructively filtering its current pixels.
    pub(crate) fn is_raw_redevelopment(&self, id: &str) -> bool {
        id == CAMERA_RAW_FILTER
            && self
                .doc
                .as_ref()
                .and_then(|doc| doc.active_layer.and_then(|id| doc.tree.find(id)))
                .is_some_and(|layer| layer.raw.is_some())
    }

    /// Run a registered filter over the active layer, confined to the
    /// selection, as one undoable edit.
    /// The pixels a filter would touch: the layer's content clipped to the
    /// canvas, or to the selection when there is one.
    pub(super) fn filter_region(&self, layer_id: schist_core::LayerId) -> IntRect {
        let Some(doc) = self.doc.as_ref() else {
            return IntRect::EMPTY;
        };
        let canvas = doc.canvas_rect();
        if doc.selection.is_empty() {
            doc.tree
                .find(layer_id)
                .map(|l| l.content_bounds())
                .unwrap_or(IntRect::EMPTY)
                .intersect(&canvas)
        } else {
            doc.selection.bounds().intersect(&canvas)
        }
    }

    /// Pull `region` out of a raster layer into a flat straight-alpha
    /// f32 RGBA buffer, the shape every filter works on.
    pub(super) fn read_region(
        &self,
        layer_id: schist_core::LayerId,
        region: IntRect,
    ) -> Option<Vec<f32>> {
        let raster = self
            .doc
            .as_ref()?
            .tree
            .find(layer_id)
            .and_then(|l| l.as_raster())?;
        let (w, h) = (region.width() as usize, region.height() as usize);
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
        Some(buf)
    }

    /// Pick an image for a filter that takes one, and re-preview with it.
    ///
    /// Decoded through the same codecs that open documents -- so a map
    /// can be a PSD, a PNG, or anything else this build reads -- and
    /// composited flat, because a filter wants pixels and not a layer
    /// tree.
    pub fn choose_filter_map(
        &mut self,
        id: &'static str,
        window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        let rx = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Choose".into()),
        });
        let codecs = self.registry.shared_codecs();
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(mut paths))) = rx.await else {
                return;
            };
            let Some(path) = paths.pop() else { return };
            let decoded = cx
                .background_executor()
                .spawn(async move {
                    let doc = decode_file(&codecs, &path)?;
                    let rect = doc.canvas_rect();
                    anyhow::Ok(schist_plugin_api::FilterImage {
                        width: rect.width().max(0) as usize,
                        height: rect.height().max(0) as usize,
                        pixels: schist_compositor::composite_region_f32(&doc, rect),
                    })
                })
                .await;
            this.update_in(cx, |ws, _window, cx| {
                match decoded {
                    Ok(image) => {
                        let image = Arc::new(image);
                        let mut values = None;
                        ws.update_modal(|m| {
                            if let Modal::Filter {
                                map,
                                values: v,
                                preview,
                                ..
                            } = m
                            {
                                *map = Some(image.clone());
                                if *preview {
                                    values = Some(v.clone());
                                }
                            }
                        });
                        if let Some(values) = values {
                            ws.preview_filter(id, Some(&values), cx);
                        }
                    }
                    Err(e) => ws.status = format!("{e}").into(),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The image the open filter dialog has been given, if any.
    pub(super) fn filter_map(&self) -> Option<Arc<schist_plugin_api::FilterImage>> {
        match &self.modal {
            Some(Modal::Filter { map, .. }) => map.clone(),
            _ => None,
        }
    }

    /// Everything a filter asked for, gathered.
    ///
    /// Each piece costs something -- compositing the layers below,
    /// flattening a path -- so nothing is fetched for a filter that did
    /// not ask. The colours are free and are always passed: they are two
    /// numbers the toolbox already has, and half the Sketch group is
    /// wrong without them.
    pub(super) fn filter_context<'a>(
        &mut self,
        filter: &dyn schist_plugin_api::FilterPlugin,
        layer: schist_core::LayerId,
        region: IntRect,
        backdrop: &'a mut Option<Vec<f32>>,
        path: &'a mut Option<Vec<(f32, f32)>>,
        map: Option<&'a schist_plugin_api::FilterImage>,
    ) -> schist_plugin_api::FilterContext<'a> {
        if filter.wants_backdrop() {
            *backdrop = self.read_backdrop(layer, region);
        }
        if filter.wants_path() {
            *path = self.active_path_points(region);
        }
        schist_plugin_api::FilterContext {
            backdrop: backdrop.as_deref(),
            foreground: self.editor.foreground,
            background: self.editor.background,
            map,
            path: path.as_deref(),
        }
    }

    /// The document's active path, flattened and moved into the filter's
    /// own coordinates.
    ///
    /// A filter works on a region of a layer and knows nothing about
    /// where that region sits, so the points arrive already relative to
    /// it -- and points outside it are kept rather than clipped, because
    /// a curve that leaves the selection and comes back is still one
    /// curve.
    pub(super) fn active_path_points(&self, region: IntRect) -> Option<Vec<(f32, f32)>> {
        let doc = self.doc.as_ref()?;
        let path = doc.active_path.and_then(|i| doc.paths.get(i))?;
        let flat = schist_tools_vector::paths::flatten(path);
        // Subpaths run together: a filter drawing along the path wants a
        // list of points, and where one subpath ends and the next begins
        // is a distinction none of them make.
        let points: Vec<(f32, f32)> = flat
            .subpaths
            .iter()
            .flat_map(|sub| sub.iter())
            .map(|(x, y)| (x - region.left as f32, y - region.top as f32))
            .collect();
        (points.len() >= 2).then_some(points)
    }

    /// What the document composites to *under* a layer, over a region.
    ///
    /// Produced by hiding the layer and everything above it, compositing,
    /// and putting the visibility back -- which is cheaper than it
    /// sounds and much cheaper than the alternative, since the
    /// compositor already knows how to skip an invisible layer. Only
    /// filters that ask for it pay for it.
    pub(super) fn read_backdrop(
        &mut self,
        layer_id: schist_core::LayerId,
        region: IntRect,
    ) -> Option<Vec<f32>> {
        let doc = self.doc.as_mut()?;
        let path = doc.tree.path_of(layer_id)?;
        // Hide the layer itself and every sibling above it, at every
        // level of the path: "above" in a nested group means later in
        // that group *and* later in each of its ancestors.
        let mut hidden: Vec<(schist_core::LayerId, bool)> = Vec::new();
        {
            let mut layers: &mut Vec<schist_core::Layer> = &mut doc.tree.layers;
            for (depth, &ix) in path.0.iter().enumerate() {
                let last = depth + 1 == path.0.len();
                let from = if last { ix } else { ix + 1 };
                for layer in layers.iter_mut().skip(from) {
                    hidden.push((layer.id, layer.visible));
                    layer.visible = false;
                }
                if last {
                    break;
                }
                match &mut layers.get_mut(ix)?.kind {
                    schist_core::LayerKind::Group(g) => layers = &mut g.children,
                    _ => break,
                }
            }
        }
        let under = schist_compositor::composite_region_f32(doc, region);
        for (id, was) in hidden {
            if let Some(layer) = doc.tree.find_mut(id) {
                layer.visible = was;
            }
        }
        Some(under)
    }

    /// Blend `filtered` back over `original` through the selection, so
    /// partial coverage feathers the result.
    ///
    /// With `record` the write becomes one history entry; without it the
    /// pixels change but the history does not, which is what a live
    /// preview needs.
    pub(super) fn write_region(
        &mut self,
        layer_id: schist_core::LayerId,
        region: IntRect,
        original: &[f32],
        filtered: &[f32],
        label: &str,
        record: bool,
    ) {
        self.write_region_inner(layer_id, region, original, filtered, label, record, true);
    }

    #[allow(clippy::too_many_arguments)]
    fn write_region_inner(
        &mut self,
        layer_id: schist_core::LayerId,
        region: IntRect,
        original: &[f32],
        filtered: &[f32],
        label: &str,
        record: bool,
        respect_selection: bool,
    ) {
        let Some(doc) = self.doc.as_mut() else { return };
        let selection = respect_selection.then(|| doc.selection.clone());
        let depth = doc.depth;
        let coords: Vec<TileCoord> = TileCoord::covering(&region).collect();

        if record {
            let mut edit = doc.begin_edit(label.to_string());
            for coord in coords {
                let clip = coord.rect().intersect(&region);
                if clip.is_empty() {
                    continue;
                }
                let Some(tile) = edit.writable_tile(layer_id, coord) else {
                    break;
                };
                blend_region_tile(
                    tile,
                    coord,
                    clip,
                    region,
                    original,
                    filtered,
                    selection.as_ref(),
                );
            }
            edit.commit();
        } else {
            let Some(raster) = doc.tree.find_mut(layer_id).and_then(|l| l.as_raster_mut()) else {
                return;
            };
            for coord in coords {
                let clip = coord.rect().intersect(&region);
                if clip.is_empty() {
                    continue;
                }
                let tile = raster.tiles.get_mut_or_insert(coord, depth);
                blend_region_tile(
                    tile,
                    coord,
                    clip,
                    region,
                    original,
                    filtered,
                    selection.as_ref(),
                );
            }
            doc.add_damage(region);
        }
    }

    /// Snapshot what a filter dialog is about to change, so the preview can
    /// be re-run from the original on every slider tick and undone on
    /// cancel. Returns false when there is nothing to filter.
    pub fn begin_filter_preview(&mut self) -> bool {
        self.begin_filter_preview_for(false)
    }

    pub(super) fn begin_raw_filter_preview(&mut self) -> bool {
        self.begin_filter_preview_for(true)
    }

    fn begin_filter_preview_for(&mut self, whole_layer: bool) -> bool {
        self.filter_preview = None;
        let Some(layer_id) = self.doc.as_ref().and_then(|d| d.active_layer) else {
            self.status = "Select a layer first".into();
            return false;
        };
        if self
            .doc
            .as_ref()
            .and_then(|d| d.tree.find(layer_id))
            .and_then(|l| l.as_raster())
            .is_none()
        {
            self.status = "Filters need a pixel layer".into();
            return false;
        }
        // A RAW development is the whole capture. A selection still applies
        // when Camera Raw is used as an ordinary destructive pixel filter,
        // but cannot sensibly crop the sensor pipeline itself.
        let region = if whole_layer {
            self.doc.as_ref().unwrap().canvas_rect()
        } else {
            self.filter_region(layer_id)
        };
        if region.is_empty() {
            self.status = "Nothing to filter".into();
            return false;
        }
        let Some(original) = self.read_region(layer_id, region) else {
            return false;
        };
        self.filter_preview = Some(FilterPreview {
            layer: layer_id,
            region,
            original,
            whole_layer,
        });
        true
    }

    fn preview_raw_filter(
        &mut self,
        values: &schist_plugin_api::FilterValues,
        cx: &mut Context<Self>,
    ) {
        let Some(preview) = self.filter_preview.clone() else {
            return;
        };
        let Some(raw) = self
            .doc
            .as_ref()
            .and_then(|doc| doc.tree.find(preview.layer))
            .and_then(|layer| layer.raw.as_deref())
            .cloned()
        else {
            return;
        };
        let Some(filter) = self.registry.shared_filter(CAMERA_RAW_FILTER) else {
            return;
        };
        let settings = settings_from_values(values);
        let values = values.clone();
        self.raw_preview_seq = self.raw_preview_seq.wrapping_add(1);
        let sequence = self.raw_preview_seq;
        self.status = "Developing RAW preview…".into();
        cx.notify();

        cx.spawn(async move |this, cx| {
            // Slider drags emit many positions. Only start the expensive
            // sensor decode once a position has remained current briefly.
            cx.background_executor()
                .timer(std::time::Duration::from_millis(RAW_PREVIEW_DEBOUNCE_MS))
                .await;
            let current = this
                .update(cx, |ws, _cx| {
                    ws.raw_preview_seq == sequence
                        && ws
                            .filter_preview
                            .as_ref()
                            .is_some_and(|p| p.layer == preview.layer)
                })
                .unwrap_or(false);
            if !current {
                return;
            }

            let rendered = cx
                .background_executor()
                .spawn(async move {
                    render_raw_capture(
                        raw.source,
                        settings,
                        schist_codecs_common::raw::RawQuality::Fast,
                        filter,
                        values,
                    )
                })
                .await;
            this.update(cx, |ws, cx| {
                if ws.raw_preview_seq != sequence {
                    return;
                }
                let Some(current) = ws.filter_preview.clone() else {
                    return;
                };
                if current.layer != preview.layer {
                    return;
                }
                match rendered {
                    Ok(developed)
                        if developed.width == current.region.width() as usize
                            && developed.height == current.region.height() as usize =>
                    {
                        ws.write_region_inner(
                            current.layer,
                            current.region,
                            &current.original,
                            &developed.rgba,
                            "",
                            false,
                            false,
                        );
                        ws.status = "RAW preview (fast demosaic)".into();
                        ws.after_change(cx);
                    }
                    Ok(developed) => {
                        ws.status = format!(
                            "RAW preview size changed: {} × {}",
                            developed.width, developed.height
                        )
                        .into();
                        cx.notify();
                    }
                    Err(err) => {
                        ws.status = format!("RAW preview failed: {err}").into();
                        cx.notify();
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    /// Re-run the filter on the canvas from the snapshot, without touching
    /// history. `None` values restore the untouched pixels.
    pub fn preview_filter(
        &mut self,
        id: &str,
        values: Option<&schist_plugin_api::FilterValues>,
        cx: &mut Context<Self>,
    ) {
        let Some(preview) = self.filter_preview.clone() else {
            return;
        };
        if let Some(values) = values {
            if id == CAMERA_RAW_FILTER
                && self
                    .doc
                    .as_ref()
                    .and_then(|doc| doc.tree.find(preview.layer))
                    .is_some_and(|layer| layer.raw.is_some())
            {
                self.preview_raw_filter(values, cx);
                return;
            }
        }
        let mut buf = preview.original.clone();
        if let Some(values) = values {
            let (w, h) = (
                preview.region.width() as usize,
                preview.region.height() as usize,
            );
            let Some(filter) = self.registry.shared_filter(id) else {
                return;
            };
            let map = self.filter_map();
            let (mut backdrop, mut path) = (None, None);
            let context = self.filter_context(
                filter.as_ref(),
                preview.layer,
                preview.region,
                &mut backdrop,
                &mut path,
                map.as_deref(),
            );
            filter.apply_with(&mut buf, w, h, values, &context);
        }
        self.write_region_inner(
            preview.layer,
            preview.region,
            &preview.original,
            &buf,
            "",
            false,
            !preview.whole_layer,
        );
        self.after_change(cx);
    }

    /// Drop a preview, restoring the pixels it was drawn over.
    pub fn cancel_filter_preview(&mut self, cx: &mut Context<Self>) {
        self.raw_preview_seq = self.raw_preview_seq.wrapping_add(1);
        if self.filter_preview.is_none() {
            return;
        }
        self.preview_filter("", None, cx);
        self.filter_preview = None;
    }

    pub fn apply_filter(
        &mut self,
        id: &str,
        values: &schist_plugin_api::FilterValues,
        cx: &mut Context<Self>,
    ) {
        if self.is_raw_redevelopment(id) {
            // Calls outside the dialog may still have a live preview. Put
            // its pixels back and invalidate every in-flight preview before
            // the Best-quality render starts.
            self.cancel_filter_preview(cx);
            self.apply_raw_development(values, cx);
            return;
        }
        // A live preview has already changed these pixels; put them back so
        // the recorded edit has the right "before".
        if self.filter_preview.is_some() {
            self.preview_filter("", None, cx);
        }
        let preview = self.filter_preview.take();
        let Some(layer_id) = self.doc.as_ref().and_then(|d| d.active_layer) else {
            self.status = "Select a layer first".into();
            return;
        };
        let Some(filter) = self.registry.filters().find(|f| f.id() == id) else {
            log::warn!("unknown filter {id}");
            return;
        };
        let name = filter.name().to_string();
        if self
            .doc
            .as_ref()
            .and_then(|d| d.tree.find(layer_id))
            .and_then(|l| l.as_raster())
            .is_none()
        {
            self.status = "Filters need a pixel layer".into();
            return;
        }
        // Reuse the preview's region so what was previewed is what lands.
        let region = preview
            .filter(|p| p.layer == layer_id)
            .map(|p| p.region)
            .unwrap_or_else(|| self.filter_region(layer_id));
        if region.is_empty() {
            self.status = "Nothing to filter".into();
            return;
        }
        let Some(original) = self.read_region(layer_id, region) else {
            return;
        };
        let mut buf = original.clone();
        // Looked up a second time because the first borrow ended; an
        // `unwrap` on registry state in a UI path is a panic waiting for
        // someone to add an early return between the two lookups.
        let Some(filter) = self.registry.filters().find(|f| f.id() == id) else {
            self.status = "Filter went away".into();
            return;
        };
        // A filter that runs outside this process blocks for as long as
        // its own dialog is open — which is until someone answers it. Run
        // it on a background thread so the window keeps painting, behind
        // a modal that holds the document still meanwhile.
        if filter.runs_out_of_process() {
            let Some(filter) = self.registry.shared_filter(id) else {
                return;
            };
            let values = values.clone();
            let (w, h) = (region.width() as usize, region.height() as usize);
            self.open_modal(
                Modal::Busy {
                    title: "Photoshop plug-in".into(),
                    what: format!("Running {name}"),
                    note: "The plug-in runs in its own process. If it opens a \
                           window, answer that to continue."
                        .into(),
                },
                cx,
            );
            cx.spawn(async move |this, cx| {
                let (buf, failure) = cx
                    .background_executor()
                    .spawn(async move {
                        filter.apply(&mut buf, w, h, &values);
                        let failure = filter.last_error();
                        (buf, failure)
                    })
                    .await;
                this.update(cx, |ws, cx| {
                    ws.modal = None;
                    ws.finish_external_filter(layer_id, region, original, buf, name, failure, cx);
                })
                .ok();
            })
            .detach();
            return;
        }
        let Some(filter) = self.registry.shared_filter(id) else {
            return;
        };
        let map = self.filter_map();
        let (mut backdrop, mut path) = (None, None);
        let context = self.filter_context(
            filter.as_ref(),
            layer_id,
            region,
            &mut backdrop,
            &mut path,
            map.as_deref(),
        );
        filter.apply_with(
            &mut buf,
            region.width() as usize,
            region.height() as usize,
            values,
            &context,
        );
        // A Photoshop plug-in runs in another process and can refuse, or
        // be cancelled from its own dialog. Recording an edit for a run
        // that did nothing would put an entry in the history that undoes
        // nothing.
        if let Some(err) = filter.last_error() {
            self.status = format!("{name}: {err}").into();
            cx.notify();
            return;
        }
        self.write_region(layer_id, region, &original, &buf, &name, true);
        self.status = name.into();
        self.after_change(cx);
    }

    fn apply_raw_development(
        &mut self,
        values: &schist_plugin_api::FilterValues,
        cx: &mut Context<Self>,
    ) {
        let Some(doc) = self.doc.as_ref() else { return };
        let Some(layer_id) = doc.active_layer else {
            return;
        };
        let Some(raw) = doc
            .tree
            .find(layer_id)
            .and_then(|layer| layer.raw.as_deref())
            .cloned()
        else {
            return;
        };
        let Some(filter) = self.registry.shared_filter(CAMERA_RAW_FILTER) else {
            return;
        };
        let document_id = doc.id;
        let settings = settings_from_values(values);
        let values = values.clone();
        let source = raw.source.clone();
        self.raw_preview_seq = self.raw_preview_seq.wrapping_add(1);
        self.open_modal(
            Modal::Busy {
                title: "Camera Raw".into(),
                what: "Developing the original capture…".into(),
                note: "Using the best demosaic path. The original RAW and these settings remain editable after saving as PSD or PSB."
                    .into(),
            },
            cx,
        );

        cx.spawn(async move |this, cx| {
            let rendered = cx
                .background_executor()
                .spawn(async move {
                    render_raw_capture(
                        source,
                        settings,
                        schist_codecs_common::raw::RawQuality::Best,
                        filter,
                        values,
                    )
                })
                .await;
            this.update(cx, |ws, cx| {
                ws.modal = None;
                let Some(doc) = ws.doc.as_mut() else {
                    return;
                };
                if doc.id != document_id || doc.tree.find(layer_id).is_none() {
                    ws.status = "RAW development finished after its document changed".into();
                    cx.notify();
                    return;
                }
                let developed = match rendered {
                    Ok(developed) => developed,
                    Err(err) => {
                        ws.status = format!("RAW development failed: {err}").into();
                        cx.notify();
                        return;
                    }
                };
                let Ok(width) = u32::try_from(developed.width) else {
                    ws.status = "RAW development is too wide".into();
                    cx.notify();
                    return;
                };
                let Ok(height) = u32::try_from(developed.height) else {
                    ws.status = "RAW development is too tall".into();
                    cx.notify();
                    return;
                };
                if (width, height) != (doc.width, doc.height) {
                    ws.status = format!(
                        "RAW development size changed to {width} × {height}; keeping the current layer"
                    )
                    .into();
                    cx.notify();
                    return;
                }

                let mut tiles = schist_core::TileMap::default();
                schist_core::blit_rgba_f32(
                    &mut tiles,
                    doc.depth,
                    IntRect::from_size(width, height),
                    &developed.rgba,
                );
                let mut after = raw;
                after.settings = settings;
                let mut edit = doc.begin_edit("Camera Raw Development");
                edit.replace_layer_tiles(layer_id, tiles);
                edit.set_raw_development(layer_id, Some(Box::new(after)));
                edit.commit();
                ws.status = "Camera Raw development applied (best demosaic)".into();
                ws.after_change(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Image Size through a neural upscaler.
    ///
    /// The network costs seconds per input megapixel, so it runs on a
    /// background thread behind a modal that holds the document still --
    /// the same arrangement an out-of-process filter gets, and for the
    /// same reason.
    pub fn resize_image_neural(
        &mut self,
        width: u32,
        height: u32,
        id: &'static str,
        cx: &mut Context<Self>,
    ) {
        let name = schist_tools_transform::Resample::Neural(id).display_name();
        let done = format!("Image size: {width} × {height}");
        let Some(doc) = self.doc.as_ref() else { return };
        if width == 0 || height == 0 || (width == doc.width && height == doc.height) {
            self.close_modal(cx);
            return;
        }
        let plan = schist_tools_transform::plan_neural(doc, width, height, id);
        // Both classical outcomes are quick enough to do inline; only the
        // network needs to get off this thread.
        let bicubic = |ws: &mut Self, status: String, cx: &mut Context<Self>| {
            if let Some(doc) = ws.doc.as_mut() {
                schist_tools_transform::resize_image(
                    doc,
                    width,
                    height,
                    schist_core::Filter::Bicubic,
                );
            }
            ws.status = status.into();
            ws.close_modal(cx);
            ws.after_change(cx);
            ws.fit_to_view();
        };
        match plan {
            schist_tools_transform::Plan::NoModel => bicubic(
                self,
                format!("{done} — the {name} model would not load, so bicubic stood in"),
                cx,
            ),
            schist_tools_transform::Plan::Classical => bicubic(self, done, cx),
            schist_tools_transform::Plan::Neural(plan) => {
                let mp = plan.megapixels();
                self.open_modal(
                    Modal::Busy {
                        title: "Image Size".into(),
                        what: format!("Upscaling with {name}"),
                        note: format!(
                            "{mp:.1} megapixels through the network, at a few \
                             seconds each. The document is held until it finishes."
                        ),
                    },
                    cx,
                );
                cx.spawn(async move |this, cx| {
                    let up = cx
                        .background_executor()
                        .spawn(async move { plan.run() })
                        .await;
                    this.update(cx, |ws, cx| {
                        ws.modal = None;
                        if let Some(doc) = ws.doc.as_mut() {
                            schist_tools_transform::apply_upscaled(doc, up);
                        }
                        ws.status = done.into();
                        ws.after_change(cx);
                        ws.fit_to_view();
                    })
                    .ok();
                })
                .detach();
            }
        }
    }

    /// Land the result of a filter that ran off the main thread.
    ///
    /// The layer is looked up again rather than assumed: the run took as
    /// long as someone took to answer a dialog, and the document it
    /// started against may not be the one in front of us now.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn finish_external_filter(
        &mut self,
        layer_id: schist_core::LayerId,
        region: IntRect,
        original: Vec<f32>,
        buf: Vec<f32>,
        name: String,
        failure: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if let Some(err) = failure {
            self.status = format!("{name}: {err}").into();
            cx.notify();
            return;
        }
        let still_there = self
            .doc
            .as_ref()
            .and_then(|d| d.tree.find(layer_id))
            .and_then(|l| l.as_raster())
            .is_some();
        if !still_there {
            self.status = format!("{name}: the layer it filtered is gone").into();
            cx.notify();
            return;
        }
        self.write_region(layer_id, region, &original, &buf, &name, true);
        self.status = name.into();
        self.after_change(cx);
    }
}
