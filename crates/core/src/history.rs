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
    /// An adjustment layer's parameters changed. Holds the canonical JSON
    /// and the PSD payload, since editing parameters invalidates the
    /// preserved bytes.
    AdjustmentParams {
        layer: LayerId,
        before: (Option<String>, Vec<u8>),
        after: (Option<String>, Vec<u8>),
    },
    /// A layer's smart-object payload changed (converted, rasterized, or
    /// re-placed by a transform).
    SmartObjectSet {
        layer: LayerId,
        before: Option<Box<crate::smart::SmartObject>>,
        after: Option<Box<crate::smart::SmartObject>>,
    },
    /// A layer's effects changed.
    LayerStyleSet {
        layer: LayerId,
        before: Box<crate::style::LayerStyle>,
        after: Box<crate::style::LayerStyle>,
    },
    /// Canvas dimensions changed (Image Size / Canvas Size). Pixel moves
    /// and rescales ride along as ordinary tile writes in the same edit.
    DocSize {
        before: (u32, u32),
        after: (u32, u32),
    },
    /// Selection changed.
    SelectionSet {
        before: Box<Selection>,
        after: Box<Selection>,
    },
    /// The document's embedded ICC profile changed (Assign / Convert to
    /// Profile). Rides in the same edit as the pixel rewrite so undo puts
    /// the pixels and their tag back together; undoing only the pixels
    /// left the old numbers interpreted under the new profile.
    IccProfileSet {
        before: Option<Vec<u8>>,
        after: Option<Vec<u8>>,
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

    /// Drop the newest two entries when they are exactly this pair, and
    /// report whether they were.
    ///
    /// For two edits that cancel each other out, where leaving them would
    /// put two no-op steps in the History panel. Nothing is dropped unless
    /// both are still on top: an edit committed in between is real work,
    /// and collapsing across it would leave the history describing a
    /// document that no longer matches.
    ///
    /// Note this is not `pop_redo`. That is the redo primitive: it moves
    /// an entry from the redo stack *onto* the undo stack rather than
    /// discarding anything.
    pub fn drop_cancelling_pair(&mut self, newest: &str, older: &str) -> bool {
        let n = self.undo_stack.len();
        if n < 2 {
            return false;
        }
        if self.undo_stack[n - 1].name != newest || self.undo_stack[n - 2].name != older {
            return false;
        }
        self.undo_stack.truncate(n - 2);
        true
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
