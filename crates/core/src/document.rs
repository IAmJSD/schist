//! The document: canvas, layer tree, selection, history, damage tracking.

use crate::geom::IntRect;
use crate::history::{Edit, EditOp, History, LayerProps};
use crate::layer::{Layer, LayerId, LayerMask, LayerPath, LayerTree};
use crate::selection::Selection;
use crate::tile::{TileBuf, TileCoord, TileMap, TILE_PIXELS};
use schist_color::{ColorMode, Depth};
use rustc_hash::FxHashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DocumentId(pub u64);

static NEXT_DOC_ID: AtomicU64 = AtomicU64::new(1);

/// A ruler guide: a full-canvas line at a fixed position.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Guide {
    /// True for a horizontal guide (constant y).
    pub horizontal: bool,
    /// Document-space position along the guide's axis.
    pub position: f32,
}

/// A PSD image resource preserved verbatim for round-trip.
#[derive(Debug, Clone)]
pub struct PreservedResource {
    pub id: u16,
    pub name: Vec<u8>,
    pub data: Vec<u8>,
}

#[derive(Debug)]
pub struct Document {
    pub id: DocumentId,
    pub title: String,
    pub path: Option<PathBuf>,
    pub width: u32,
    pub height: u32,
    pub resolution_dpi: f32,
    pub mode: ColorMode,
    pub depth: Depth,
    pub icc_profile: Option<Vec<u8>>,
    pub tree: LayerTree,
    pub selection: Selection,
    pub active_layer: Option<LayerId>,
    pub history: History,
    /// PSD image resources we preserve for round-trip fidelity.
    pub preserved_resources: Vec<PreservedResource>,
    /// Monotonic counter bumped on every visible change; views compare it
    /// to decide whether to recomposite.
    pub revision: u64,
    /// Ruler guides.
    pub guides: Vec<Guide>,
    /// The last non-empty selection, so Reselect can bring it back.
    pub last_selection: Option<Selection>,
    /// Named canvases within the document, each exportable on its own.
    pub artboards: Vec<crate::annotate::Artboard>,
    /// Regions marked for separate export.
    pub slices: Vec<crate::annotate::Slice>,
    /// Pinned annotations.
    pub notes: Vec<crate::annotate::Note>,
    /// Tally marks from the Count tool.
    pub counts: Vec<crate::annotate::CountGroup>,
    /// Named snapshots of layer visibility and appearance.
    pub layer_comps: Vec<crate::annotate::LayerComp>,
    /// Stored vector paths, as in Photoshop's Paths panel.
    pub paths: Vec<crate::path::VectorPath>,
    /// Index into `paths` of the one being edited.
    pub active_path: Option<usize>,
    /// Per-layer snapshot of the pixels the document was opened with,
    /// which is what the History Brush paints back from. Tile maps are
    /// copy-on-write, so this costs a handful of `Arc` clones.
    pub history_source: rustc_hash::FxHashMap<LayerId, crate::tile::TileMap>,
    /// Named selections stashed by Select ▸ Save Selection. Photoshop
    /// keeps these as alpha channels; we have no channels panel yet, so
    /// they live here and are not written to PSD.
    pub saved_selections: Vec<(String, Selection)>,
    /// Damaged document-space regions since the last `take_damage()`.
    damage: Vec<IntRect>,
    /// Unsaved changes?
    pub dirty: bool,
}

impl Document {
    pub fn new(title: impl Into<String>, width: u32, height: u32, depth: Depth) -> Document {
        Document {
            id: DocumentId(NEXT_DOC_ID.fetch_add(1, Ordering::Relaxed)),
            title: title.into(),
            path: None,
            width,
            height,
            resolution_dpi: 72.0,
            mode: ColorMode::Rgb,
            depth,
            icc_profile: None,
            tree: LayerTree::default(),
            selection: Selection::new(),
            active_layer: None,
            history: History::new(),
            preserved_resources: Vec::new(),
            revision: 0,
            guides: Vec::new(),
            last_selection: None,
            saved_selections: Vec::new(),
            history_source: Default::default(),
            paths: Vec::new(),
            artboards: Vec::new(),
            slices: Vec::new(),
            notes: Vec::new(),
            counts: Vec::new(),
            layer_comps: Vec::new(),
            active_path: None,
            damage: Vec::new(),
            dirty: false,
        }
    }

    pub fn canvas_rect(&self) -> IntRect {
        IntRect::from_size(self.width, self.height)
    }

    /// Record the current pixels as the History Brush's source. Called
    /// when a document is opened or created, matching Photoshop, whose
    /// default history source is the state the file was opened in.
    pub fn snapshot_history_source(&mut self) {
        self.history_source.clear();
        let mut stack: Vec<&crate::layer::Layer> = self.tree.layers.iter().collect();
        while let Some(layer) = stack.pop() {
            match &layer.kind {
                crate::layer::LayerKind::Raster(r) => {
                    self.history_source.insert(layer.id, r.tiles.clone());
                }
                crate::layer::LayerKind::Group(g) => stack.extend(g.children.iter()),
                crate::layer::LayerKind::Adjustment(_) => {}
            }
        }
    }

    /// How many frame layers the document has, for naming the next one.
    pub fn frame_count(&self) -> usize {
        self.tree.iter().filter(|l| l.is_frame).count()
    }

    pub fn add_damage(&mut self, rect: IntRect) {
        if !rect.is_empty() {
            self.damage.push(rect);
            self.revision += 1;
        }
    }

    pub fn damage_all(&mut self) {
        let all = self.canvas_rect();
        self.add_damage(all);
    }

    pub fn take_damage(&mut self) -> Vec<IntRect> {
        std::mem::take(&mut self.damage)
    }

    /// Begin a recorded edit. All mutations must go through the returned
    /// builder; `commit()` pushes the edit onto the history.
    pub fn begin_edit(&mut self, name: impl Into<String>) -> EditBuilder<'_> {
        EditBuilder {
            doc: self,
            name: name.into(),
            ops: Vec::new(),
            recorded_tiles: FxHashMap::default(),
            recorded_mask_tiles: FxHashMap::default(),
            damage: IntRect::EMPTY,
        }
    }

    pub fn undo(&mut self) -> Option<String> {
        let edit = self.history.pop_undo()?;
        for op in edit.ops.iter().rev() {
            self.apply_op(op, Direction::Undo);
        }
        self.dirty = true;
        Some(edit.name)
    }

    pub fn redo(&mut self) -> Option<String> {
        let edit = self.history.pop_redo()?;
        for op in edit.ops.iter() {
            self.apply_op(op, Direction::Redo);
        }
        self.dirty = true;
        Some(edit.name)
    }

    fn apply_op(&mut self, op: &EditOp, dir: Direction) {
        match op {
            EditOp::TileWrite {
                layer,
                coord,
                before,
                after,
            } => {
                let target = if dir == Direction::Undo {
                    before
                } else {
                    after
                };
                if let Some(l) = self.tree.find_mut(*layer) {
                    if let Some(raster) = l.as_raster_mut() {
                        match target {
                            Some(buf) => raster.tiles.insert(*coord, buf.clone()),
                            None => {
                                raster.tiles.remove(*coord);
                            }
                        }
                    }
                }
                self.add_damage(coord.rect());
            }
            EditOp::MaskTileWrite {
                layer,
                coord,
                before,
                after,
            } => {
                let target = if dir == Direction::Undo {
                    before
                } else {
                    after
                };
                if let Some(l) = self.tree.find_mut(*layer) {
                    if let Some(mask) = &mut l.mask {
                        match target {
                            Some(buf) => mask.tiles.insert(*coord, buf.clone()),
                            None => {
                                mask.tiles.prune_blank();
                            }
                        }
                    }
                }
                self.add_damage(coord.rect());
            }
            EditOp::LayerInsert { path, layer } => {
                match dir {
                    Direction::Redo => {
                        self.tree.insert_at(path, (**layer).clone());
                    }
                    Direction::Undo => {
                        self.tree.remove_at(path);
                        if self.active_layer == Some(layer.id) {
                            self.active_layer = None;
                        }
                    }
                }
                self.add_damage(layer.content_bounds());
                self.structure_changed();
            }
            EditOp::LayerRemove { path, layer } => {
                match dir {
                    Direction::Redo => {
                        self.tree.remove_at(path);
                        if self.active_layer == Some(layer.id) {
                            self.active_layer = None;
                        }
                    }
                    Direction::Undo => {
                        self.tree.insert_at(path, (**layer).clone());
                    }
                }
                self.add_damage(layer.content_bounds());
                self.structure_changed();
            }
            EditOp::LayerMove { from, to } => {
                let (src, dst) = if dir == Direction::Redo {
                    (from, to)
                } else {
                    (to, from)
                };
                if let Some(layer) = self.tree.remove_at(src) {
                    let bounds = layer.content_bounds();
                    self.tree.insert_at(dst, layer);
                    self.add_damage(bounds);
                }
                self.structure_changed();
            }
            EditOp::LayerTranslate { layer, dx, dy } => {
                let (dx, dy) = if dir == Direction::Undo {
                    (-dx, -dy)
                } else {
                    (*dx, *dy)
                };
                self.translate_layer_content(*layer, dx, dy);
            }
            EditOp::LayerProps {
                layer,
                before,
                after,
            } => {
                let props = if dir == Direction::Undo {
                    before
                } else {
                    after
                };
                let mut bounds = IntRect::EMPTY;
                if let Some(l) = self.tree.find_mut(*layer) {
                    props.apply_to(l);
                    bounds = l.content_bounds();
                }
                self.add_damage(bounds);
                self.structure_changed();
            }
            EditOp::MaskSet {
                layer,
                before,
                after,
            } => {
                let target = if dir == Direction::Undo {
                    before
                } else {
                    after
                };
                let mut bounds = IntRect::EMPTY;
                if let Some(l) = self.tree.find_mut(*layer) {
                    l.mask = target.as_deref().cloned();
                    bounds = l.content_bounds();
                }
                self.add_damage(bounds);
                self.structure_changed();
            }
            EditOp::SmartObjectSet {
                layer,
                before,
                after,
            } => {
                let want = if dir == Direction::Undo {
                    before
                } else {
                    after
                };
                if let Some(l) = self.tree.find_mut(*layer) {
                    l.smart = want.clone();
                }
                self.structure_changed();
            }
            EditOp::LayerStyleSet {
                layer,
                before,
                after,
            } => {
                let style = if dir == Direction::Undo {
                    before
                } else {
                    after
                };
                if let Some(l) = self.tree.find_mut(*layer) {
                    l.style = **style;
                    // The cached raster belongs to the old style.
                    l.styled = None;
                }
                self.damage_all();
                self.structure_changed();
            }
            EditOp::AdjustmentParams {
                layer,
                before,
                after,
            } => {
                let (json, raw) = if dir == Direction::Undo {
                    before
                } else {
                    after
                };
                if let Some(l) = self.tree.find_mut(*layer) {
                    if let crate::layer::LayerKind::Adjustment(data) = &mut l.kind {
                        data.params_json = json.clone();
                        data.raw = raw.clone();
                    }
                }
                self.damage_all();
                self.structure_changed();
            }
            EditOp::DocSize { before, after } => {
                let (w, h) = if dir == Direction::Undo {
                    *before
                } else {
                    *after
                };
                self.width = w;
                self.height = h;
                self.damage_all();
                self.structure_changed();
            }
            EditOp::SelectionSet { before, after } => {
                let target = if dir == Direction::Undo {
                    before
                } else {
                    after
                };
                self.selection = (**target).clone();
                self.revision += 1;
            }
        }
    }

    fn structure_changed(&mut self) {
        self.revision += 1;
    }

    fn translate_layer_content(&mut self, id: LayerId, dx: i32, dy: i32) {
        fn recurse(layer: &mut Layer, dx: i32, dy: i32, depth: Depth) {
            match &mut layer.kind {
                crate::layer::LayerKind::Raster(r) => {
                    r.tiles = r.tiles.translated(dx, dy, depth);
                }
                crate::layer::LayerKind::Group(g) => {
                    for child in &mut g.children {
                        recurse(child, dx, dy, depth);
                    }
                }
                crate::layer::LayerKind::Adjustment(_) => {}
            }
            if let Some(mask) = &mut layer.mask {
                if mask.linked {
                    mask.tiles = mask.tiles.translated(dx, dy);
                    mask.bounds = mask.bounds.translated(dx, dy);
                }
            }
        }
        let depth = self.depth;
        if let Some(layer) = self.tree.find_mut(id) {
            let before = layer.content_bounds();
            recurse(layer, dx, dy, depth);
            let after = layer.content_bounds();
            let damage = before.union(&after);
            self.add_damage(damage);
        }
    }

    /// Convenience for tests and importers: append a raster layer at the top
    /// level (top of stack) without recording history.
    pub fn push_layer(&mut self, layer: Layer) -> LayerId {
        let id = layer.id;
        let bounds = layer.content_bounds();
        self.tree.layers.push(layer);
        self.active_layer = Some(id);
        self.add_damage(bounds);
        self.structure_changed();
        id
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    Undo,
    Redo,
}

/// Records mutations as they happen, then commits them as one named edit.
pub struct EditBuilder<'a> {
    doc: &'a mut Document,
    name: String,
    ops: Vec<EditOp>,
    /// (layer, coord) -> index in `ops`, so repeated writes to one tile
    /// record only the first `before` and last `after`.
    recorded_tiles: FxHashMap<(LayerId, TileCoord), usize>,
    recorded_mask_tiles: FxHashMap<(LayerId, TileCoord), usize>,
    damage: IntRect,
}

impl<'a> EditBuilder<'a> {
    pub fn doc(&self) -> &Document {
        self.doc
    }

    /// Copy-on-write access to a layer tile, with undo capture.
    pub fn writable_tile(&mut self, layer_id: LayerId, coord: TileCoord) -> Option<&mut TileBuf> {
        let depth = self.doc.depth;
        let entry_key = (layer_id, coord);
        let layer = self.doc.tree.find_mut(layer_id)?;
        let raster = layer.as_raster_mut()?;
        if !self.recorded_tiles.contains_key(&entry_key) {
            let before = raster.tiles.snapshot(coord);
            self.ops.push(EditOp::TileWrite {
                layer: layer_id,
                coord,
                before,
                after: None,
            });
            self.recorded_tiles.insert(entry_key, self.ops.len() - 1);
        }
        self.damage = self.damage.union(&coord.rect());
        Some(raster.tiles.get_mut_or_insert(coord, depth))
    }

    /// Copy-on-write access to a layer-mask tile, with undo capture.
    pub fn writable_mask_tile(
        &mut self,
        layer_id: LayerId,
        coord: TileCoord,
    ) -> Option<&mut [u8; TILE_PIXELS]> {
        let entry_key = (layer_id, coord);
        let layer = self.doc.tree.find_mut(layer_id)?;
        let mask = layer.mask.as_mut()?;
        if !self.recorded_mask_tiles.contains_key(&entry_key) {
            let before = mask.tiles.get(coord).cloned();
            self.ops.push(EditOp::MaskTileWrite {
                layer: layer_id,
                coord,
                before,
                after: None,
            });
            self.recorded_mask_tiles
                .insert(entry_key, self.ops.len() - 1);
        }
        self.damage = self.damage.union(&coord.rect());
        Some(mask.tiles.get_mut_or_insert(coord))
    }

    /// Insert a layer (records op + performs it).
    pub fn insert_layer(&mut self, path: LayerPath, layer: Layer) -> LayerId {
        let id = layer.id;
        self.damage = self.damage.union(&layer.content_bounds());
        self.doc.tree.insert_at(&path, layer.clone());
        self.ops.push(EditOp::LayerInsert {
            path,
            layer: Box::new(layer),
        });
        id
    }

    pub fn remove_layer(&mut self, id: LayerId) -> bool {
        let Some((path, layer)) = self.doc.tree.remove(id) else {
            return false;
        };
        self.damage = self.damage.union(&layer.content_bounds());
        if self.doc.active_layer == Some(id) {
            self.doc.active_layer = None;
        }
        self.ops.push(EditOp::LayerRemove {
            path,
            layer: Box::new(layer),
        });
        true
    }

    /// Translate a layer's pixels (and linked mask) by an integer offset.
    pub fn translate_layer(&mut self, id: LayerId, dx: i32, dy: i32) {
        if dx == 0 && dy == 0 {
            return;
        }
        self.doc.translate_layer_content(id, dx, dy);
        if let Some(layer) = self.doc.tree.find(id) {
            self.damage = self.damage.union(&layer.content_bounds().inflated(1));
        }
        self.ops.push(EditOp::LayerTranslate { layer: id, dx, dy });
    }

    pub fn move_layer(&mut self, from: LayerPath, to: LayerPath) {
        if let Some(layer) = self.doc.tree.remove_at(&from) {
            self.damage = self.damage.union(&layer.content_bounds());
            self.doc.tree.insert_at(&to, layer);
            self.ops.push(EditOp::LayerMove { from, to });
        }
    }

    /// Replace a raster layer's tiles wholesale, recording every tile that
    /// changes (transforms, filters and resizes all go through this).
    pub fn replace_layer_tiles(&mut self, layer_id: LayerId, new_tiles: TileMap) {
        let Some(layer) = self.doc.tree.find(layer_id) else {
            return;
        };
        let Some(raster) = layer.as_raster() else {
            return;
        };
        let mut coords: Vec<TileCoord> = raster.tiles.coords().collect();
        for coord in new_tiles.coords() {
            if !coords.contains(&coord) {
                coords.push(coord);
            }
        }
        let mut damage = IntRect::EMPTY;
        for coord in coords {
            let before = self
                .doc
                .tree
                .find(layer_id)
                .and_then(|l| l.as_raster())
                .and_then(|r| r.tiles.snapshot(coord));
            let after = new_tiles.snapshot(coord);
            if before.as_ref().map(Arc::as_ptr) == after.as_ref().map(Arc::as_ptr) {
                continue;
            }
            self.ops.push(EditOp::TileWrite {
                layer: layer_id,
                coord,
                before,
                after: None,
            });
            self.recorded_tiles
                .insert((layer_id, coord), self.ops.len() - 1);
            damage = damage.union(&coord.rect());
        }
        if let Some(raster) = self
            .doc
            .tree
            .find_mut(layer_id)
            .and_then(|l| l.as_raster_mut())
        {
            raster.tiles = new_tiles;
        }
        self.damage = self.damage.union(&damage);
    }

    /// Apply an affine transform to a layer's pixels (and its mask).
    pub fn transform_layer(
        &mut self,
        layer_id: LayerId,
        matrix: &crate::resample::Affine,
        filter: crate::resample::Filter,
        clip: IntRect,
    ) {
        let depth = self.doc.depth;
        let Some(layer) = self.doc.tree.find(layer_id) else {
            return;
        };
        let Some(raster) = layer.as_raster() else {
            return;
        };
        let transformed =
            crate::resample::transform_tiles(&raster.tiles, matrix, depth, filter, clip);
        self.replace_layer_tiles(layer_id, transformed);
    }

    /// Record an adjustment layer's parameter change (the caller has
    /// already applied it, e.g. through a live dialog preview).
    /// Attach, replace or clear a layer's smart-object payload.
    pub fn set_smart_object(
        &mut self,
        layer: LayerId,
        after: Option<Box<crate::smart::SmartObject>>,
    ) {
        let before = self.doc.tree.find(layer).and_then(|l| l.smart.clone());
        if let Some(l) = self.doc.tree.find_mut(layer) {
            l.smart = after.clone();
        }
        self.ops.push(EditOp::SmartObjectSet {
            layer,
            before,
            after,
        });
    }

    /// Record a change to a layer's effects.
    pub fn record_layer_style(
        &mut self,
        layer: LayerId,
        before: crate::style::LayerStyle,
        after: crate::style::LayerStyle,
    ) {
        if before == after {
            return;
        }
        if let Some(l) = self.doc.tree.find_mut(layer) {
            l.style = after;
            l.styled = None;
        }
        self.ops.push(EditOp::LayerStyleSet {
            layer,
            before: Box::new(before),
            after: Box::new(after),
        });
        let canvas = self.doc.canvas_rect();
        self.damage = self.damage.union(&canvas);
    }

    pub fn record_adjustment_params(
        &mut self,
        layer: LayerId,
        before: (Option<String>, Vec<u8>),
        after: (Option<String>, Vec<u8>),
    ) {
        if before == after {
            return;
        }
        if let Some(l) = self.doc.tree.find_mut(layer) {
            if let crate::layer::LayerKind::Adjustment(data) = &mut l.kind {
                data.params_json = after.0.clone();
                data.raw = after.1.clone();
            }
        }
        self.ops.push(EditOp::AdjustmentParams {
            layer,
            before,
            after,
        });
        let canvas = self.doc.canvas_rect();
        self.damage = self.damage.union(&canvas);
    }

    /// Change the canvas size, optionally offsetting existing content
    /// (Canvas Size) or rescaling every layer (Image Size).
    pub fn set_canvas_size(&mut self, width: u32, height: u32) {
        let before = (self.doc.width, self.doc.height);
        if before == (width, height) {
            return;
        }
        self.doc.width = width;
        self.doc.height = height;
        self.ops.push(EditOp::DocSize {
            before,
            after: (width, height),
        });
        self.damage = self.damage.union(&IntRect::from_size(
            width.max(before.0),
            height.max(before.1),
        ));
    }

    /// Ids of every raster layer in the document, bottom-to-top.
    pub fn raster_layer_ids(&self) -> Vec<LayerId> {
        self.doc
            .tree
            .iter()
            .filter(|l| l.as_raster().is_some())
            .map(|l| l.id)
            .collect()
    }

    /// Change scalar properties via closure; captures before/after.
    pub fn change_props(&mut self, id: LayerId, f: impl FnOnce(&mut Layer)) {
        if let Some(layer) = self.doc.tree.find_mut(id) {
            let before = LayerProps::of(layer);
            f(layer);
            let after = LayerProps::of(layer);
            let bounds = layer.content_bounds();
            if before != after {
                self.damage = self.damage.union(&bounds);
                self.ops.push(EditOp::LayerProps {
                    layer: id,
                    before,
                    after,
                });
            }
        }
    }

    pub fn set_mask(&mut self, id: LayerId, mask: Option<LayerMask>) {
        if let Some(layer) = self.doc.tree.find_mut(id) {
            let before = layer.mask.take().map(Box::new);
            layer.mask = mask.clone();
            self.damage = self.damage.union(&layer.content_bounds());
            self.ops.push(EditOp::MaskSet {
                layer: id,
                before,
                after: mask.map(Box::new),
            });
        }
    }

    /// Replace the selection via closure; captures before/after.
    pub fn change_selection(&mut self, f: impl FnOnce(&mut Selection, IntRect)) {
        let canvas = self.doc.canvas_rect();
        let before = Box::new(self.doc.selection.clone());
        // Remember what we had, so Reselect can restore it after a deselect.
        if !before.is_empty() {
            self.doc.last_selection = Some((*before).clone());
        }
        f(&mut self.doc.selection, canvas);
        let after = Box::new(self.doc.selection.clone());
        self.ops.push(EditOp::SelectionSet { before, after });
        self.damage = self.damage.union(&canvas);
    }

    /// Finish: snapshot `after` states for tile writes and push to history.
    /// Returns false if the edit was empty (nothing recorded).
    pub fn commit(mut self) -> bool {
        if self.ops.is_empty() {
            return false;
        }
        // Fill in `after` snapshots for tile ops now that mutation is done.
        for op in &mut self.ops {
            match op {
                EditOp::TileWrite {
                    layer,
                    coord,
                    after,
                    ..
                } => {
                    *after = self
                        .doc
                        .tree
                        .find(*layer)
                        .and_then(|l| l.as_raster())
                        .and_then(|r| r.tiles.snapshot(*coord));
                }
                EditOp::MaskTileWrite {
                    layer,
                    coord,
                    after,
                    ..
                } => {
                    *after = self
                        .doc
                        .tree
                        .find(*layer)
                        .and_then(|l| l.mask.as_ref())
                        .and_then(|m| m.tiles.get(*coord).cloned());
                }
                _ => {}
            }
        }
        let damage = self.damage;
        self.doc.history.push(Edit {
            name: self.name,
            ops: self.ops,
        });
        self.doc.add_damage(damage);
        self.doc.revision += 1;
        self.doc.dirty = true;
        true
    }

    /// Abandon the edit, rolling back all recorded mutations.
    pub fn cancel(mut self) {
        let ops = std::mem::take(&mut self.ops);
        // Snapshot `after` first so tile rollback works.
        for op in ops.iter().rev() {
            self.doc.apply_op(op, Direction::Undo);
        }
    }
}

/// Records tile before-images across many mutations without borrowing the
/// document (unlike `EditBuilder`), so a brush stroke spanning dozens of
/// pointer events can mutate tiles directly and still commit as one edit.
#[derive(Debug, Default)]
pub struct StrokeEdit {
    name: String,
    befores: FxHashMap<(LayerId, TileCoord), Option<Arc2<TileBuf>>>,
    mask_befores: FxHashMap<(LayerId, TileCoord), Option<Arc2<[u8; TILE_PIXELS]>>>,
    damage: IntRect,
}

type Arc2<T> = std::sync::Arc<T>;

impl StrokeEdit {
    pub fn new(name: impl Into<String>) -> StrokeEdit {
        StrokeEdit {
            name: name.into(),
            ..Default::default()
        }
    }

    /// Call before (or while) mutating a layer tile; captures the pre-stroke
    /// snapshot once and returns COW-mutable access.
    pub fn writable_tile<'d>(
        &mut self,
        doc: &'d mut Document,
        layer_id: LayerId,
        coord: TileCoord,
    ) -> Option<&'d mut TileBuf> {
        let depth = doc.depth;
        let layer = doc.tree.find_mut(layer_id)?;
        let raster = layer.as_raster_mut()?;
        self.befores
            .entry((layer_id, coord))
            .or_insert_with(|| raster.tiles.snapshot(coord));
        self.damage = self.damage.union(&coord.rect());
        Some(raster.tiles.get_mut_or_insert(coord, depth))
    }

    pub fn writable_mask_tile<'d>(
        &mut self,
        doc: &'d mut Document,
        layer_id: LayerId,
        coord: TileCoord,
    ) -> Option<&'d mut [u8; TILE_PIXELS]> {
        let layer = doc.tree.find_mut(layer_id)?;
        let mask = layer.mask.as_mut()?;
        self.mask_befores
            .entry((layer_id, coord))
            .or_insert_with(|| mask.tiles.get(coord).cloned());
        self.damage = self.damage.union(&coord.rect());
        Some(mask.tiles.get_mut_or_insert(coord))
    }

    /// Extend visible damage without recording (e.g. live overlay updates).
    pub fn touch(&mut self, doc: &mut Document, rect: IntRect) {
        doc.add_damage(rect);
    }

    pub fn is_empty(&self) -> bool {
        self.befores.is_empty() && self.mask_befores.is_empty()
    }

    /// The tile's content as it was before this stroke touched it
    /// (`None` = tile was absent). Only valid after `writable_tile` was
    /// called for that tile; returns the live tile state otherwise.
    pub fn pre_stroke_tile(
        &self,
        doc: &Document,
        layer: LayerId,
        coord: TileCoord,
    ) -> Option<Arc2<TileBuf>> {
        match self.befores.get(&(layer, coord)) {
            Some(before) => before.clone(),
            None => doc
                .tree
                .find(layer)
                .and_then(|l| l.as_raster())
                .and_then(|r| r.tiles.snapshot(coord)),
        }
    }

    /// Commit the stroke as one history entry. Returns false if nothing
    /// was recorded.
    pub fn commit(self, doc: &mut Document) -> bool {
        if self.is_empty() {
            return false;
        }
        let mut ops = Vec::new();
        for ((layer, coord), before) in self.befores {
            let after = doc
                .tree
                .find(layer)
                .and_then(|l| l.as_raster())
                .and_then(|r| r.tiles.snapshot(coord));
            ops.push(EditOp::TileWrite {
                layer,
                coord,
                before,
                after,
            });
        }
        for ((layer, coord), before) in self.mask_befores {
            let after = doc
                .tree
                .find(layer)
                .and_then(|l| l.mask.as_ref())
                .and_then(|m| m.tiles.get(coord).cloned());
            ops.push(EditOp::MaskTileWrite {
                layer,
                coord,
                before,
                after,
            });
        }
        doc.history.push(Edit {
            name: self.name,
            ops,
        });
        doc.add_damage(self.damage);
        doc.dirty = true;
        true
    }

    /// Roll back everything this stroke touched.
    pub fn cancel(self, doc: &mut Document) {
        for ((layer, coord), before) in self.befores {
            if let Some(l) = doc.tree.find_mut(layer) {
                if let Some(r) = l.as_raster_mut() {
                    match before {
                        Some(buf) => r.tiles.insert(coord, buf),
                        None => {
                            r.tiles.remove(coord);
                        }
                    }
                }
            }
            doc.add_damage(coord.rect());
        }
        for ((layer, coord), before) in self.mask_befores {
            if let Some(l) = doc.tree.find_mut(layer) {
                if let Some(m) = &mut l.mask {
                    if let Some(buf) = before {
                        m.tiles.insert(coord, buf);
                    }
                }
            }
            doc.add_damage(coord.rect());
        }
    }
}

/// Fill a whole raster layer tilemap region from an RGBA8 buffer
/// (importer/test convenience; not undoable).
pub fn blit_rgba8(tiles: &mut TileMap, depth: Depth, rect: IntRect, rgba: &[u8]) {
    use crate::tile::TILE_SIZE;
    assert_eq!(
        rgba.len(),
        rect.width() as usize * rect.height() as usize * 4
    );
    let w = rect.width() as usize;
    for coord in TileCoord::covering(&rect) {
        let trect = coord.rect();
        let clip = trect.intersect(&rect);
        if clip.is_empty() {
            continue;
        }
        let buf = tiles.get_mut_or_insert(coord, depth);
        for y in clip.top..clip.bottom {
            let sy = (y - rect.top) as usize;
            let ly = (y - trect.top) as usize;
            for x in clip.left..clip.right {
                let sx = (x - rect.left) as usize;
                let lx = (x - trect.left) as usize;
                let s = (sy * w + sx) * 4;
                let px =
                    schist_color::Rgba::from_u8(rgba[s], rgba[s + 1], rgba[s + 2], rgba[s + 3]);
                buf.set(ly * TILE_SIZE as usize + lx, px);
            }
        }
    }
    tiles.prune_blank();
}

#[cfg(test)]
mod tests {
    use super::*;
    use schist_color::Rgba;

    fn doc_with_layer() -> (Document, LayerId) {
        let mut doc = Document::new("test", 512, 512, Depth::Eight);
        let id = doc.push_layer(Layer::new_raster("L1"));
        (doc, id)
    }

    #[test]
    fn tile_edit_undo_redo() {
        let (mut doc, id) = doc_with_layer();
        let coord = TileCoord { tx: 0, ty: 0 };

        let mut edit = doc.begin_edit("Paint");
        edit.writable_tile(id, coord).unwrap().set(0, Rgba::WHITE);
        assert!(edit.commit());

        let read = |doc: &Document| {
            doc.tree
                .find(id)
                .unwrap()
                .as_raster()
                .unwrap()
                .tiles
                .pixel(0, 0)
        };
        assert_eq!(read(&doc), Rgba::WHITE);
        assert_eq!(doc.undo().as_deref(), Some("Paint"));
        assert_eq!(read(&doc), Rgba::TRANSPARENT);
        assert_eq!(doc.redo().as_deref(), Some("Paint"));
        assert_eq!(read(&doc), Rgba::WHITE);
    }

    #[test]
    fn repeated_writes_one_op() {
        let (mut doc, id) = doc_with_layer();
        let coord = TileCoord { tx: 0, ty: 0 };
        let mut edit = doc.begin_edit("Paint");
        for i in 0..10 {
            edit.writable_tile(id, coord).unwrap().set(i, Rgba::BLACK);
        }
        assert_eq!(edit.ops.len(), 1);
        edit.commit();
    }

    #[test]
    fn layer_insert_remove_undo() {
        let (mut doc, _) = doc_with_layer();
        let mut edit = doc.begin_edit("New Layer");
        let new_id = edit.insert_layer(LayerPath(vec![1]), Layer::new_raster("L2"));
        edit.commit();
        assert!(doc.tree.find(new_id).is_some());

        doc.undo();
        assert!(doc.tree.find(new_id).is_none());
        doc.redo();
        assert!(doc.tree.find(new_id).is_some());

        let mut edit = doc.begin_edit("Delete Layer");
        edit.remove_layer(new_id);
        edit.commit();
        assert!(doc.tree.find(new_id).is_none());
        doc.undo();
        assert!(doc.tree.find(new_id).is_some());
    }

    #[test]
    fn props_change_undo() {
        let (mut doc, id) = doc_with_layer();
        let mut edit = doc.begin_edit("Opacity");
        edit.change_props(id, |l| l.opacity = 0.5);
        edit.commit();
        assert_eq!(doc.tree.find(id).unwrap().opacity, 0.5);
        doc.undo();
        assert_eq!(doc.tree.find(id).unwrap().opacity, 1.0);
    }

    #[test]
    fn cancel_rolls_back() {
        let (mut doc, id) = doc_with_layer();
        let coord = TileCoord { tx: 0, ty: 0 };
        let mut edit = doc.begin_edit("Paint");
        edit.writable_tile(id, coord).unwrap().set(0, Rgba::WHITE);
        edit.cancel();
        let px = doc
            .tree
            .find(id)
            .unwrap()
            .as_raster()
            .unwrap()
            .tiles
            .pixel(0, 0);
        assert_eq!(px, Rgba::TRANSPARENT);
        assert!(!doc.history.can_undo());
    }

    #[test]
    fn empty_edit_not_recorded() {
        let (mut doc, _) = doc_with_layer();
        let edit = doc.begin_edit("Nothing");
        assert!(!edit.commit());
        assert!(!doc.history.can_undo());
    }

    #[test]
    fn selection_edit_undo() {
        let (mut doc, _) = doc_with_layer();
        let mut edit = doc.begin_edit("Select");
        edit.change_selection(|sel, _| {
            sel.select_rect(
                IntRect::from_xywh(0, 0, 10, 10),
                crate::selection::SelectOp::Replace,
            )
        });
        edit.commit();
        assert!(!doc.selection.is_empty());
        doc.undo();
        assert!(doc.selection.is_empty());
    }
}
