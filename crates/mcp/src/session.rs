//! One editing session: a document plus the same plugin registry and
//! editor state the GPUI shell would hold for a window.
//!
//! The methods here are the headless counterparts of `Workspace`'s
//! document-editing paths — same registry, same history semantics, same
//! selection-feathered filter writes — minus everything that needs a
//! window (view transforms, previews, system clipboard, dialogs).

use anyhow::{anyhow, bail, Context as _, Result};
use schist_core::color::{Depth, Rgba};
use schist_core::{
    blit_rgba8, AdjustmentKind, Document, IntRect, Layer, LayerId, LayerKind, TileCoord, TILE_SIZE,
};
use schist_plugin_api::{
    CodecPlugin, CommandCtx, EditorState, ExportOptions, FilterValues, Modifiers, OptionValue,
    PluginManifest, PluginRegistry, PointerInput, ToolCtx,
};
use std::path::Path;

/// PSD/PSB import and export via `schist-codec-psd` — the same wrapper the
/// app shell registers.
struct PsdCodec;

impl CodecPlugin for PsdCodec {
    fn id(&self) -> &'static str {
        "codec.psd"
    }
    fn name(&self) -> &'static str {
        "Photoshop PSD"
    }
    fn extensions(&self) -> &'static [&'static str] {
        &["psd", "psb"]
    }
    fn probe(&self, bytes: &[u8]) -> bool {
        schist_codec_psd::is_psd(bytes)
    }
    fn import(&self, bytes: &[u8]) -> Result<Document> {
        Ok(schist_codec_psd::read_psd(bytes)?)
    }
    fn can_export(&self) -> bool {
        true
    }
    fn export(&self, doc: &Document) -> Result<Vec<u8>> {
        Ok(schist_codec_psd::write_psd(doc)?)
    }
}

struct PsdPlugin;

impl PluginManifest for PsdPlugin {
    fn id(&self) -> &'static str {
        "schist.codec-psd"
    }
    fn register(&self, registry: &mut PluginRegistry) {
        registry.register_codec(Box::new(PsdCodec));
    }
}

/// The same first-party plugin set `schist-app` assembles, plus any
/// installed third-party WebAssembly plugins. Each session gets its own
/// registry because tools carry per-gesture state.
fn build_registry() -> (PluginRegistry, schist_plugin_host_wasm::PluginManager) {
    let mut registry = PluginRegistry::new();
    let manifests: Vec<Box<dyn PluginManifest>> = vec![
        Box::new(schist_tools_basic::BasicToolsPlugin),
        Box::new(schist_tools_paint::PaintToolsPlugin),
        Box::new(schist_tools_retouch::RetouchToolsPlugin),
        Box::new(schist_tools_warp::WarpToolsPlugin),
        Box::new(schist_tools_doc::DocToolsPlugin),
        Box::new(schist_tools_select::SelectToolsPlugin),
        Box::new(schist_tools_transform::TransformToolsPlugin),
        Box::new(schist_tools_vector::VectorToolsPlugin),
        Box::new(schist_tools_type::TypeToolsPlugin),
        Box::new(schist_commands_core::CoreCommandsPlugin),
        Box::new(schist_filters_core::CoreFiltersPlugin),
        Box::new(schist_codecs_common::CommonCodecsPlugin),
        Box::new(PsdPlugin),
    ];
    for manifest in manifests {
        manifest.register(&mut registry);
    }
    let manager = match schist_plugin_host_wasm::PluginManager::plugin_dir() {
        Some(dir) => schist_plugin_host_wasm::PluginManager::load_dir(&dir, &mut registry),
        None => schist_plugin_host_wasm::PluginManager::default(),
    };
    (registry, manager)
}

pub struct Session {
    pub doc: Document,
    pub state: EditorState,
    pub registry: PluginRegistry,
    /// Keeps loaded WASM plugins alive for the session's lifetime.
    _wasm: schist_plugin_host_wasm::PluginManager,
}

/// Largest side a new document may have.
const MAX_DIMENSION: u32 = 30_000;

/// Largest total area, which is the limit that actually bounds memory:
/// 200 megapixels is 800 MB of RGBA8 and comfortably past any real
/// document.
const MAX_PIXELS: u64 = 200_000_000;

impl Session {
    /// A blank document with a white Background layer, like File ▸ New.
    pub fn new_blank(title: &str, width: u32, height: u32, depth: Depth) -> Result<Session> {
        if width == 0 || height == 0 || width > MAX_DIMENSION || height > MAX_DIMENSION {
            bail!("document size must be 1..={MAX_DIMENSION} in each dimension");
        }
        // Per-dimension limits alone allowed 30000x30000, and the white
        // fill below is one `vec![255u8; w * h * 4]` -- 3.6 GB in a single
        // allocation, before any tiling. `create_session
        // {"width":30000,"height":30000}` is a plausible typo for
        // 3000x3000 and it aborted the process, taking every other
        // session's unsaved work with it.
        let pixels = width as u64 * height as u64;
        if pixels > MAX_PIXELS {
            bail!(
                "document is {pixels} pixels, over the {MAX_PIXELS} limit \
                 ({width}x{height})"
            );
        }
        let mut doc = Document::new(title, width, height, depth);
        let mut bg = Layer::new_raster("Background");
        // A row at a time rather than the whole canvas at once, so the
        // peak allocation is bounded by the width.
        let row = vec![255u8; width as usize * 4];
        for y in 0..height {
            blit_rgba8(
                &mut bg.as_raster_mut().unwrap().tiles,
                depth,
                IntRect::from_xywh(0, y as i32, width, 1),
                &row,
            );
        }
        doc.push_layer(bg);
        doc.dirty = false;
        Ok(Session::install(doc))
    }

    /// Open a file through the codec registry, like File ▸ Open.
    pub fn open(path: &Path) -> Result<Session> {
        let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let (registry, wasm) = build_registry();
        let ext = path.extension().and_then(|e| e.to_str());
        let codec = registry
            .codec_for(&bytes, ext)
            .ok_or_else(|| anyhow!("no codec for {}", path.display()))?;
        let mut doc = codec.import(&bytes)?;
        doc.title = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Untitled".into());
        doc.path = Some(path.to_path_buf());
        doc.snapshot_history_source();
        let mut session = Session {
            doc,
            state: EditorState::default(),
            registry,
            _wasm: wasm,
        };
        session.after_change();
        Ok(session)
    }

    fn install(mut doc: Document) -> Session {
        doc.snapshot_history_source();
        let (registry, wasm) = build_registry();
        Session {
            doc,
            state: EditorState::default(),
            registry,
            _wasm: wasm,
        }
    }

    // ----- commands -----

    pub fn run_command(&mut self, id: &str) -> Result<String> {
        // Grow, Similar and Color Range take their tolerance from the
        // magic wand, exactly as the app does.
        self.sync_wand_tolerance();
        let command = self
            .registry
            .command(id)
            .ok_or_else(|| anyhow!("unknown command {id:?}"))?;
        let mut ctx = CommandCtx {
            doc: &mut self.doc,
            state: &mut self.state,
        };
        (command.run)(&mut ctx);
        let title = command.title.to_string();
        self.after_change();
        Ok(title)
    }

    fn sync_wand_tolerance(&mut self) {
        if let Some(option) = self
            .registry
            .tools()
            .find(|t| t.id() == "wand")
            .and_then(|t| t.options().into_iter().find(|o| o.key == "wand-tolerance"))
        {
            self.state.tolerance = option.value.num().round().clamp(0.0, 255.0) as u8;
        }
    }

    // ----- tools -----

    pub fn activate_tool(&mut self, id: &str) -> Result<&'static str> {
        let previous = self.state.active_tool;
        if previous != id {
            if let Some(tool) = self.registry.tool_mut(previous) {
                let mut ctx = ToolCtx {
                    doc: &mut self.doc,
                    state: &mut self.state,
                };
                tool.on_deactivate(&mut ctx);
            }
        }
        let static_id = {
            let tool = self
                .registry
                .tool_mut(id)
                .ok_or_else(|| anyhow!("unknown tool {id:?}"))?;
            tool.id()
        };
        self.state.active_tool = static_id;
        let tool = self.registry.tool_mut(static_id).unwrap();
        let mut ctx = ToolCtx {
            doc: &mut self.doc,
            state: &mut self.state,
        };
        tool.on_activate(&mut ctx);
        self.after_change();
        Ok(static_id)
    }

    pub fn set_tool_option(&mut self, key: &str, value: OptionValue) -> Result<()> {
        let tool_id = self.state.active_tool;
        let tool = self
            .registry
            .tool_mut(tool_id)
            .ok_or_else(|| anyhow!("no active tool"))?;
        let key = tool
            .options()
            .iter()
            .map(|o| o.key)
            .find(|k| *k == key)
            .ok_or_else(|| {
                anyhow!(
                    "tool {tool_id:?} has no option {key:?} (has: {:?})",
                    tool.options().iter().map(|o| o.key).collect::<Vec<_>>()
                )
            })?;
        tool.set_option(key, value);
        let tool = self.registry.tool_mut(tool_id).unwrap();
        let mut ctx = ToolCtx {
            doc: &mut self.doc,
            state: &mut self.state,
        };
        tool.on_option_changed(&mut ctx, key);
        self.after_change();
        Ok(())
    }

    /// Drive the active tool through a full gesture: pointer down on the
    /// first point, moves through the rest, up on the last.
    pub fn stroke(
        &mut self,
        points: &[(f32, f32)],
        pressure: f32,
        modifiers: Modifiers,
    ) -> Result<()> {
        if points.is_empty() {
            bail!("stroke needs at least one point");
        }
        let tool_id = self.state.active_tool;
        if self.registry.tool_mut(tool_id).is_none() {
            bail!("no active tool");
        }
        let input = |&(x, y): &(f32, f32)| PointerInput {
            x,
            y,
            pressure,
            modifiers,
        };
        let tool = self.registry.tool_mut(tool_id).unwrap();
        let mut ctx = ToolCtx {
            doc: &mut self.doc,
            state: &mut self.state,
        };
        tool.on_pointer_down(&mut ctx, input(&points[0]));
        for point in &points[1..] {
            tool.on_pointer_move(&mut ctx, input(point));
        }
        tool.on_pointer_up(&mut ctx, input(points.last().unwrap()));
        self.after_change();
        Ok(())
    }

    /// Enter / Escape / raw keys for modal tools (free transform, crop,
    /// the type tool's text entry).
    pub fn tool_input(
        &mut self,
        action: &str,
        key: Option<&str>,
        text: Option<&str>,
        modifiers: Modifiers,
    ) -> Result<bool> {
        let tool_id = self.state.active_tool;
        let tool = self
            .registry
            .tool_mut(tool_id)
            .ok_or_else(|| anyhow!("no active tool"))?;
        let mut ctx = ToolCtx {
            doc: &mut self.doc,
            state: &mut self.state,
        };
        let consumed = match action {
            "commit" => {
                tool.on_commit(&mut ctx);
                true
            }
            "cancel" => {
                tool.on_cancel(&mut ctx);
                true
            }
            "key" => {
                let key = key.ok_or_else(|| anyhow!("action \"key\" needs a key name"))?;
                tool.on_key(&mut ctx, key, text, modifiers)
            }
            other => bail!("unknown action {other:?} (expected key, commit or cancel)"),
        };
        self.after_change();
        Ok(consumed)
    }

    // ----- filters and destructive adjustments -----

    pub fn apply_filter(&mut self, id: &str, values: &[(String, f64)]) -> Result<String> {
        let filter = self
            .registry
            .filters()
            .find(|f| f.id() == id)
            .ok_or_else(|| anyhow!("unknown filter {id:?}"))?;
        let params = filter.params();
        let mut resolved = FilterValues::defaults(&params);
        for (key, value) in values {
            let key = params
                .iter()
                .map(|p| p.key)
                .find(|k| k == key)
                .ok_or_else(|| {
                    anyhow!(
                        "filter {id:?} has no parameter {key:?} (has: {:?})",
                        params.iter().map(|p| p.key).collect::<Vec<_>>()
                    )
                })?;
            resolved.set(key, *value as f32);
        }
        let name = filter.name().to_string();
        let (layer_id, region) = self.filter_target()?;
        let original = self
            .read_region(layer_id, region)
            .ok_or_else(|| anyhow!("layer pixels unreadable"))?;
        let mut buf = original.clone();
        let filter = self
            .registry
            .filters()
            .find(|f| f.id() == id)
            .ok_or_else(|| anyhow!("no filter {id}"))?;
        // A sandboxed filter can trap or run out of fuel; recording the
        // edit anyway would report success over unchanged pixels.
        filter
            .try_apply(
                &mut buf,
                region.width() as usize,
                region.height() as usize,
                &resolved,
            )
            .map_err(|err| anyhow!("{name} failed: {err}"))?;
        self.write_region(layer_id, region, &original, &buf, &name);
        self.after_change();
        Ok(name)
    }

    /// Image ▸ Adjustments: apply an adjustment straight onto the active
    /// layer's pixels, as one history entry.
    pub fn apply_adjustment(
        &mut self,
        kind: AdjustmentKind,
        params: Option<schist_adjustments::Params>,
    ) -> Result<String> {
        let params = params.unwrap_or_else(|| schist_adjustments::Params::default_for(kind));
        let (layer_id, region) = self.filter_target()?;
        let original = self
            .read_region(layer_id, region)
            .ok_or_else(|| anyhow!("layer pixels unreadable"))?;
        let mut buf = original.clone();
        params.apply_buffer(&mut buf);
        let name = kind.display_name().to_string();
        self.write_region(layer_id, region, &original, &buf, &name);
        self.after_change();
        Ok(name)
    }

    /// The active raster layer and the region a filter would touch —
    /// the selection's bounds when there is one, the layer's otherwise.
    fn filter_target(&self) -> Result<(LayerId, IntRect)> {
        let layer_id = self
            .doc
            .active_layer
            .ok_or_else(|| anyhow!("select a layer first"))?;
        if self
            .doc
            .tree
            .find(layer_id)
            .and_then(|l| l.as_raster())
            .is_none()
        {
            bail!("filters need a pixel layer");
        }
        let canvas = self.doc.canvas_rect();
        let region = if self.doc.selection.is_empty() {
            self.doc
                .tree
                .find(layer_id)
                .map(|l| l.content_bounds())
                .unwrap_or(IntRect::EMPTY)
                .intersect(&canvas)
        } else {
            self.doc.selection.bounds().intersect(&canvas)
        };
        if region.is_empty() {
            bail!("nothing to filter");
        }
        Ok((layer_id, region))
    }

    /// Pull `region` out of a raster layer into a flat straight-alpha
    /// f32 RGBA buffer, the shape every filter works on.
    fn read_region(&self, layer_id: LayerId, region: IntRect) -> Option<Vec<f32>> {
        let raster = self.doc.tree.find(layer_id).and_then(|l| l.as_raster())?;
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

    /// Blend `filtered` back over `original` through the selection, as one
    /// history entry, so partial coverage feathers the result.
    fn write_region(
        &mut self,
        layer_id: LayerId,
        region: IntRect,
        original: &[f32],
        filtered: &[f32],
        label: &str,
    ) {
        let selection = self.doc.selection.clone();
        let coords: Vec<TileCoord> = TileCoord::covering(&region).collect();
        let mut edit = self.doc.begin_edit(label.to_string());
        for coord in coords {
            let clip = coord.rect().intersect(&region);
            if clip.is_empty() {
                continue;
            }
            let Some(tile) = edit.writable_tile(layer_id, coord) else {
                break;
            };
            let trect = coord.rect();
            let w = region.width() as usize;
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
                        Rgba::new(
                            mix(original[src], filtered[src]),
                            mix(original[src + 1], filtered[src + 1]),
                            mix(original[src + 2], filtered[src + 2]),
                            mix(original[src + 3], filtered[src + 3]),
                        ),
                    );
                }
            }
        }
        edit.commit();
    }

    // ----- rendering -----

    /// Composite `region` (or the whole canvas) to straight-alpha RGBA8,
    /// with shape and style caches refreshed first.
    pub fn render(&mut self, region: Option<IntRect>) -> Result<(IntRect, Vec<u8>)> {
        self.after_change();
        let canvas = self.doc.canvas_rect();
        let region = region.map(|r| r.intersect(&canvas)).unwrap_or(canvas);
        if region.is_empty() {
            bail!("render region is empty");
        }
        let pixels = schist_compositor::composite_region_rgba8(&self.doc, region);
        Ok((region, pixels))
    }

    // ----- saving -----

    fn exporter_for(&self, path: &Path) -> Option<&dyn CodecPlugin> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        self.registry
            .codecs()
            .find(|c| c.can_export() && c.extensions().contains(&ext.as_str()))
    }

    /// File ▸ Save (As): serialize by extension and adopt the path.
    pub fn save(&mut self, path: &Path) -> Result<()> {
        // Raster formats flatten through the compositor, which reads the
        // styled-raster caches — bring them up to date first.
        self.after_change();
        let codec = self.exporter_for(path).ok_or_else(|| {
            anyhow!(
                "no exporter for .{} — exportable: {}",
                path.extension().and_then(|e| e.to_str()).unwrap_or(""),
                self.export_extensions().join(", ")
            )
        })?;
        let bytes = codec.export(&self.doc)?;
        write_atomically(path, &bytes)?;
        self.doc.dirty = false;
        self.doc.path = Some(path.to_path_buf());
        if let Some(name) = path.file_name() {
            self.doc.title = name.to_string_lossy().into_owned();
        }
        Ok(())
    }

    /// File ▸ Export: flattened export with encoder settings, leaving the
    /// document's own path alone.
    pub fn export(&mut self, path: &Path, options: &ExportOptions) -> Result<()> {
        self.after_change();
        let codec = self.exporter_for(path).ok_or_else(|| {
            anyhow!(
                "no exporter for .{} — exportable: {}",
                path.extension().and_then(|e| e.to_str()).unwrap_or(""),
                self.export_extensions().join(", ")
            )
        })?;
        let bytes = codec.export_with(&self.doc, options)?;
        write_atomically(path, &bytes)
    }

    fn export_extensions(&self) -> Vec<String> {
        self.registry
            .codecs()
            .filter(|c| c.can_export())
            .flat_map(|c| c.extensions().iter().map(|e| e.to_string()))
            .collect()
    }

    // ----- derived caches -----

    /// The headless counterpart of the shell's `after_change`: re-rasterize
    /// stale vector shapes, rebuild stale layer-effect rasters, and drop
    /// the damage (there is no display cache here to invalidate).
    pub fn after_change(&mut self) {
        let depth = self.doc.depth;
        let canvas = self.doc.canvas_rect();
        let mut grew = Vec::new();
        reshape_layers(&mut self.doc.tree.layers, depth, canvas, &mut grew);
        restyle_layers(&mut self.doc.tree.layers, &mut grew);
        for rect in grew {
            self.doc.add_damage(rect);
        }
        self.doc.take_damage();
    }
}

fn write_atomically(path: &Path, bytes: &[u8]) -> Result<()> {
    // The parent has to exist already. Creating it meant a mistyped
    // destination silently scattered empty directory trees instead of
    // saying the path is wrong.
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() && !parent.is_dir() {
            bail!("{} is not a directory", parent.display());
        }
    }
    // Append rather than replace the extension, and include the pid.
    // `with_extension` *replaces* it, so saving `photo.psd` wrote and then
    // renamed `photo.schist-tmp`, destroying any real file of that name;
    // two processes saving at once would also collide.
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.schist-tmp", std::process::id()));
    let tmp = path.with_file_name(name);
    // Flush before the rename: `rename` is atomic against a process
    // crash, but not against power loss, and it can otherwise reach the
    // disk ahead of the data.
    {
        let mut file = std::fs::File::create(&tmp)?;
        std::io::Write::write_all(&mut file, bytes)?;
        file.sync_all()?;
    }
    if let Err(err) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(err.into());
    }
    Ok(())
}

/// Re-rasterize any shape layer whose path, fill or stroke has moved.
fn reshape_layers(layers: &mut [Layer], depth: Depth, canvas: IntRect, damage: &mut Vec<IntRect>) {
    for layer in layers.iter_mut() {
        if let LayerKind::Group(g) = &mut layer.kind {
            reshape_layers(&mut g.children, depth, canvas, damage);
        }
        let Some(shape) = layer.shape.as_deref() else {
            continue;
        };
        let key = shape.key();
        if layer.shape_key == key {
            continue;
        }
        let before = layer.content_bounds();
        let tiles = schist_tools_vector::render_shape(shape, depth, canvas);
        if let Some(raster) = layer.as_raster_mut() {
            raster.tiles = tiles;
        }
        layer.shape_key = key;
        layer.styled = None;
        damage.push(before);
        damage.push(layer.content_bounds());
    }
}

/// Rebuild stale styled rasters (layer effects), collecting changed areas.
fn restyle_layers(layers: &mut [Layer], damage: &mut Vec<IntRect>) {
    for layer in layers.iter_mut() {
        if let LayerKind::Group(g) = &mut layer.kind {
            restyle_layers(&mut g.children, damage);
        }
        if layer.style.is_empty() {
            if let Some(old) = layer.styled.take() {
                damage.push(old.bounds);
            }
            continue;
        }
        let key = fx_key(layer);
        if layer.styled.as_ref().map(|s| s.key) == Some(key) {
            continue;
        }
        let before = layer.styled.as_ref().map(|s| s.bounds);
        layer.styled = schist_compositor::render_styled(layer).map(|mut r| {
            r.key = key;
            std::sync::Arc::new(r)
        });
        if let Some(b) = before {
            damage.push(b);
        }
        if let Some(s) = &layer.styled {
            damage.push(s.bounds);
        }
    }
}

/// Fingerprint of everything a styled raster depends on.
fn fx_key(layer: &Layer) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = rustc_hash::FxHasher::default();
    format!("{:?}", layer.style).hash(&mut h);
    layer.fill_opacity.to_bits().hash(&mut h);
    if let Some(r) = layer.as_raster() {
        r.tiles.fingerprint().hash(&mut h);
    }
    // A group's styled raster renders from its flattened children;
    // children restyle first, so their styled keys are fresh here.
    if let LayerKind::Group(g) = &layer.kind {
        fx_key_children(&g.children, &mut h);
    }
    h.finish()
}

fn fx_key_children(layers: &[Layer], h: &mut rustc_hash::FxHasher) {
    use std::hash::Hash;
    for l in layers {
        l.visible.hash(h);
        l.opacity.to_bits().hash(h);
        l.fill_opacity.to_bits().hash(h);
        format!("{:?}", l.blend).hash(h);
        l.render_offset.hash(h);
        l.clipping.hash(h);
        if let Some(r) = l.as_raster() {
            r.tiles.fingerprint().hash(h);
        }
        if let Some(s) = l.styled.as_ref() {
            s.key.hash(h);
        }
        if let Some(m) = &l.mask {
            m.enabled.hash(h);
        }
        if let LayerKind::Group(g) = &l.kind {
            fx_key_children(&g.children, h);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blank() -> Session {
        Session::new_blank("test", 64, 48, Depth::Eight).unwrap()
    }

    #[test]
    fn a_brush_stroke_paints_and_undo_restores() {
        let mut session = blank();
        session.state.foreground = Rgba::new(1.0, 0.0, 0.0, 1.0);
        session.activate_tool("brush").unwrap();
        session
            .stroke(&[(10.0, 10.0), (50.0, 30.0)], 1.0, Modifiers::default())
            .unwrap();
        let (_, painted) = session.render(None).unwrap();
        assert!(
            painted
                .as_chunks::<4>()
                .0
                .iter()
                .any(|p| p[0] > 200 && p[1] < 100),
            "the stroke left no red pixels"
        );
        session.run_command("edit.undo").unwrap();
        let (_, restored) = session.render(None).unwrap();
        assert!(
            restored
                .as_chunks::<4>()
                .0
                .iter()
                .all(|p| p[0] > 240 && p[1] > 240),
            "undo did not put the white background back"
        );
    }

    #[test]
    fn commands_and_filters_run_by_registry_id() {
        let mut session = blank();
        session.run_command("layer.duplicate").unwrap();
        assert_eq!(session.doc.tree.layers.len(), 2);
        assert!(session.run_command("no.such.command").is_err());

        // A blur over a painted layer changes pixels; an unknown filter or
        // parameter is refused by name.
        session.state.foreground = Rgba::new(0.0, 0.0, 1.0, 1.0);
        session.activate_tool("pencil").unwrap();
        session
            .stroke(&[(0.0, 24.0), (63.0, 24.0)], 1.0, Modifiers::default())
            .unwrap();
        let (_, before) = session.render(None).unwrap();
        session
            .apply_filter("filter.gaussian_blur", &[("radius".into(), 4.0)])
            .unwrap();
        let (_, after) = session.render(None).unwrap();
        assert_ne!(before, after);
        assert!(session.apply_filter("filter.nope", &[]).is_err());
        assert!(session
            .apply_filter("filter.gaussian_blur", &[("nope".into(), 1.0)])
            .is_err());
    }

    #[test]
    fn adjustments_apply_destructively_as_one_edit() {
        let mut session = blank();
        let (_, before) = session.render(None).unwrap();
        session
            .apply_adjustment(AdjustmentKind::Invert, None)
            .unwrap();
        let (_, inverted) = session.render(None).unwrap();
        assert!(
            inverted
                .as_chunks::<4>()
                .0
                .iter()
                .all(|p| p[0] < 15 && p[1] < 15),
            "inverting white did not give black"
        );
        session.run_command("edit.undo").unwrap();
        let (_, restored) = session.render(None).unwrap();
        assert_eq!(before, restored);
    }

    #[test]
    fn a_selection_confines_a_filter() {
        let mut session = blank();
        session.activate_tool("marquee.rect").unwrap();
        session
            .stroke(&[(0.0, 0.0), (32.0, 48.0)], 1.0, Modifiers::default())
            .unwrap();
        assert!(!session.doc.selection.is_empty());
        session
            .apply_adjustment(AdjustmentKind::Invert, None)
            .unwrap();
        let (_, pixels) = session.render(None).unwrap();
        let px = |x: usize, y: usize| &pixels[(y * 64 + x) * 4..(y * 64 + x) * 4 + 4];
        assert!(px(10, 24)[0] < 15, "inside the selection stays white");
        assert!(px(50, 24)[0] > 240, "outside the selection went dark");
    }
    #[test]
    fn an_atomic_write_does_not_clobber_a_sibling() {
        // `with_extension` *replaces* the extension, so saving `photo.psd`
        // wrote and renamed `photo.schist-tmp`, destroying any real file
        // of that name.
        let dir = std::env::temp_dir().join(format!("schist-atomic-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let victim = dir.join("photo.schist-tmp");
        std::fs::write(&victim, b"precious").unwrap();

        let target = dir.join("photo.psd");
        write_atomically(&target, b"new contents").unwrap();

        assert_eq!(
            std::fs::read(&victim).unwrap(),
            b"precious",
            "the sibling file must survive"
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"new contents");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_write_to_a_missing_directory_fails_without_creating_it() {
        // `create_dir_all` here meant a mistyped destination silently
        // built the whole tree instead of saying the path was wrong.
        let dir = std::env::temp_dir().join(format!("schist-atomic2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let missing = dir.join("no/such/place/doc.psd");
        assert!(
            write_atomically(&missing, b"x").is_err(),
            "a missing parent must be refused"
        );
        assert!(!dir.exists(), "nothing should have been created");
    }

    #[test]
    fn a_successful_write_leaves_no_temp_file() {
        let dir = std::env::temp_dir().join(format!("schist-atomic3-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let target = dir.join("doc.psd");
        write_atomically(&target, b"x").unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains("schist-tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp file left behind: {leftovers:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Per-dimension limits alone allowed 30000x30000, whose white fill
    /// was one 3.6 GB allocation — a plausible typo for 3000x3000 that
    /// aborted the process and took every other session's unsaved work
    /// with it.
    #[test]
    fn an_absurd_canvas_is_refused_rather_than_allocated() {
        let err = match Session::new_blank("t", 30_000, 30_000, Depth::Eight) {
            Ok(_) => panic!("30000x30000 must be refused"),
            Err(err) => err,
        };
        assert!(
            err.to_string().contains("pixels"),
            "unexpected message: {err}"
        );
        // Each side is still individually allowed; it is the area that
        // is out of bounds.
        assert!(Session::new_blank("t", 30_000, 100, Depth::Eight).is_ok());
        // And an ordinary document is untouched.
        assert!(Session::new_blank("t", 1920, 1080, Depth::Eight).is_ok());
    }

    #[test]
    fn a_reasonable_canvas_is_white_all_the_way_across() {
        // The fill is per-row now, so check both ends and the far corner.
        let sess = Session::new_blank("t", 600, 400, Depth::Eight).unwrap();
        let tiles = &sess.doc.tree.layers[0].as_raster().unwrap().tiles;
        for (x, y) in [(0, 0), (599, 0), (0, 399), (599, 399), (300, 200)] {
            assert_eq!(
                tiles.pixel(x, y).to_u8(),
                [255, 255, 255, 255],
                "at ({x}, {y})"
            );
        }
    }
}
