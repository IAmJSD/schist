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
    /// A layer's preserved blocks changed.
    ///
    /// The type tool keeps a text layer's spec in one of them, and it
    /// has to undo together with the pixels rendered from it: undoing
    /// the pixels alone left the glyphs and the text they were set from
    /// disagreeing.
    LayerExtrasSet {
        layer: LayerId,
        before: Vec<crate::layer::RawBlock>,
        after: Vec<crate::layer::RawBlock>,
    },
    /// A layer's original camera capture or development settings changed.
    RawDevelopmentSet {
        layer: LayerId,
        before: Option<Box<crate::raw::RawDevelopment>>,
        after: Option<Box<crate::raw::RawDevelopment>>,
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
    /// The document's notes changed -- placed, moved, retyped, deleted or
    /// cleared.
    ///
    /// The whole list rather than a per-note delta: notes are a handful of
    /// short strings, so a snapshot costs less than the machinery to
    /// describe which one moved, and it makes Clear Notes one op like
    /// every other.
    NotesSet {
        before: Vec<crate::annotate::Note>,
        after: Vec<crate::annotate::Note>,
    },
    /// The document's colour mode changed (Image > Mode).
    ///
    /// Set outside the edit, so undo restored the pixels and left the new
    /// mode: a document converted to greyscale and undone still reported
    /// (and saved as) greyscale. The CMYK/Lab/RGB cases changed nothing
    /// else at all, so they produced no history entry whatsoever.
    ColorModeSet {
        before: schist_color::ColorMode,
        after: schist_color::ColorMode,
    },
}

impl EditOp {
    /// Rough heap bytes this op keeps alive, for the history byte budget.
    /// Only the bulky pixel payloads are counted, and shared `Arc`s are
    /// counted at full size, so this overestimates what evicting the op
    /// actually frees — the safe direction for a cap.
    fn retained_bytes(&self) -> usize {
        let tile = |t: &Option<Arc<TileBuf>>| t.as_ref().map_or(0, |t| t.byte_len());
        match self {
            EditOp::TileWrite { before, after, .. } => tile(before) + tile(after),
            EditOp::MaskTileWrite { before, after, .. } => {
                (before.is_some() as usize + after.is_some() as usize) * TILE_PIXELS
            }
            _ => 0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Edit {
    pub name: String,
    pub ops: Vec<EditOp>,
}

impl Edit {
    fn retained_bytes(&self) -> usize {
        self.ops.iter().map(EditOp::retained_bytes).sum()
    }
}

/// Linear undo history. `undo_stack.last()` is the most recent edit.
#[derive(Debug, Default)]
pub struct History {
    undo_stack: Vec<Edit>,
    redo_stack: Vec<Edit>,
    /// Cap on retained edits; oldest are dropped past this.
    pub limit: usize,
    /// Cap on retained pixel bytes across the undo stack; oldest edits are
    /// dropped past this. One large stroke can hold tens of megabytes of
    /// tiles, so an edit-count cap alone lets a painting session retain
    /// gigabytes. The newest edit always stays, however large.
    pub byte_limit: usize,
    /// How deep the undo stack was when the document was last saved, so
    /// undoing back to that point can clear the dirty flag again. `None`
    /// once that edit has aged out past `limit`, since we can no longer
    /// tell whether we are standing on it.
    saved_depth: Option<usize>,
}

impl History {
    pub fn new() -> History {
        History {
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            limit: 200,
            byte_limit: 1 << 30,
            saved_depth: Some(0),
        }
    }

    pub fn push(&mut self, edit: Edit) {
        // A new edit discards the redo branch. If the save point was in
        // that branch it can never be reached again, so it has to go:
        // otherwise draw A, save, undo A, draw B, undo B, redo B leaves
        // `undo_stack.len() == saved_depth` and redo clears `dirty` while
        // the document holds B and the disk holds A. With the quit
        // confirmation in front of it, that loses B silently.
        if self.saved_depth.is_some_and(|d| d > self.undo_stack.len()) {
            self.saved_depth = None;
        }
        self.redo_stack.clear();
        self.undo_stack.push(edit);
        let mut excess = 0;
        if self.undo_stack.len() > self.limit {
            excess = self.undo_stack.len() - self.limit;
        }
        let mut retained: usize = self.undo_stack[excess..]
            .iter()
            .map(Edit::retained_bytes)
            .sum();
        while retained > self.byte_limit && excess + 1 < self.undo_stack.len() {
            retained -= self.undo_stack[excess].retained_bytes();
            excess += 1;
        }
        if excess > 0 {
            self.undo_stack.drain(0..excess);
            // Dropping the oldest edits shifts every depth down; a save
            // point that falls off the bottom is gone for good.
            self.saved_depth = self.saved_depth.and_then(|d| d.checked_sub(excess));
        }
    }

    /// Record that the document as it stands right now is what is on disk.
    pub fn mark_saved(&mut self) {
        self.saved_depth = Some(self.undo_stack.len());
    }

    /// True when the current state is the one that was last saved.
    pub fn at_saved(&self) -> bool {
        self.saved_depth == Some(self.undo_stack.len())
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
