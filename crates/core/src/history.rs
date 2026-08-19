//! Undo/redo.
//!
//! Every mutation of a document is recorded as an `Edit`: a named list of
//! reversible `EditOp`s. Tile-level ops store `Arc` snapshots of the tiles
//! they replaced — with copy-on-write tiles this costs memory proportional
//! to *changed* pixels only. Tools don't build ops by hand; they go through
//! `Document::begin_edit()` which records as they mutate.

use crate::layer::{Layer, LayerId, LayerMask, LayerPath};
use crate::selection::Selection;
use crate::tile::{TileBuf, TileCoord, TILE_PIXELS};
use crate::BlendMode;
use std::sync::Arc;

/// Snapshot of a layer's scalar properties (for property-change edits).
#[derive(Debug, Clone, PartialEq)]
pub struct LayerProps {
    pub name: String,
    pub visible: bool,
    pub opacity: f32,
    pub fill_opacity: f32,
    pub blend: BlendMode,
    pub clipping: bool,
    pub locked: bool,
}

impl LayerProps {
    pub fn of(layer: &Layer) -> LayerProps {
        LayerProps {
            name: layer.name.clone(),
            visible: layer.visible,
            opacity: layer.opacity,
            fill_opacity: layer.fill_opacity,
            blend: layer.blend,
            clipping: layer.clipping,
            locked: layer.locked,
        }
    }

    pub fn apply_to(&self, layer: &mut Layer) {
        layer.name = self.name.clone();
        layer.visible = self.visible;
        layer.opacity = self.opacity;
        layer.fill_opacity = self.fill_opacity;
        layer.blend = self.blend;
        layer.clipping = self.clipping;
        layer.locked = self.locked;
    }
}

#[derive(Debug, Clone)]
pub enum EditOp {
    /// A pixel tile changed. `None` = tile absent (transparent).
    TileWrite {
        layer: LayerId,
        coord: TileCoord,
        before: Option<Arc<TileBuf>>,
        after: Option<Arc<TileBuf>>,
    },
    /// A layer-mask tile changed.
    MaskTileWrite {
        layer: LayerId,
        coord: TileCoord,
        before: Option<Arc<[u8; TILE_PIXELS]>>,
        after: Option<Arc<[u8; TILE_PIXELS]>>,
    },
    /// Redo inserts `layer` at `path`; undo removes it.
    LayerInsert { path: LayerPath, layer: Box<Layer> },
    /// Redo removes the layer at `path`; undo re-inserts it.
    LayerRemove { path: LayerPath, layer: Box<Layer> },
    /// Layer moved in the tree (reorder / regroup).
    LayerMove { from: LayerPath, to: LayerPath },
    /// Layer content translated by an integer offset (Move tool).
    /// Losslessly invertible; undo translates by (-dx, -dy).
    LayerTranslate { layer: LayerId, dx: i32, dy: i32 },
    /// Scalar property change.
    LayerProps {
        layer: LayerId,
        before: LayerProps,
        after: LayerProps,
    },
    /// Whole-mask add/remove/replace on a layer.
    MaskSet {
        layer: LayerId,
        before: Option<Box<LayerMask>>,
        after: Option<Box<LayerMask>>,
    },
    /// Selection changed.
    SelectionSet {
        before: Box<Selection>,
        after: Box<Selection>,
    },
}

#[derive(Debug, Clone)]
pub struct Edit {
    pub name: String,
    pub ops: Vec<EditOp>,
}

/// Linear undo history. `undo_stack.last()` is the most recent edit.
#[derive(Debug, Default)]
pub struct History {
    undo_stack: Vec<Edit>,
    redo_stack: Vec<Edit>,
    /// Cap on retained edits; oldest are dropped past this.
    pub limit: usize,
}

impl History {
    pub fn new() -> History {
        History {
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            limit: 200,
        }
    }

    pub fn push(&mut self, edit: Edit) {
        self.redo_stack.clear();
        self.undo_stack.push(edit);
        if self.undo_stack.len() > self.limit {
            let excess = self.undo_stack.len() - self.limit;
            self.undo_stack.drain(0..excess);
        }
    }

    pub fn pop_undo(&mut self) -> Option<Edit> {
        let edit = self.undo_stack.pop()?;
        self.redo_stack.push(edit);
        self.redo_stack.last().cloned()
    }

    pub fn pop_redo(&mut self) -> Option<Edit> {
        let edit = self.redo_stack.pop()?;
        self.undo_stack.push(edit);
        self.undo_stack.last().cloned()
    }

    pub fn can_undo(&self) -> bool {
        !self.undo_stack.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo_stack.is_empty()
    }

    pub fn undo_name(&self) -> Option<&str> {
        self.undo_stack.last().map(|e| e.name.as_str())
    }

    pub fn redo_name(&self) -> Option<&str> {
        self.redo_stack.last().map(|e| e.name.as_str())
    }

    pub fn entries(&self) -> &[Edit] {
        &self.undo_stack
    }

    /// Redone-away entries, most-recently-undone LAST (so iterating in
    /// reverse yields the order they would be re-applied by `redo`).
    pub fn redo_entries(&self) -> &[Edit] {
        &self.redo_stack
    }
}
