//! Document furniture: artboards, slices, notes, counts and layer comps.
//!
//! None of these are pixels. They are things a document carries alongside
//! its layers -- regions to export, places to remember, states to restore
//! -- and they are grouped here because they share that nature rather than
//! any implementation.

use serde::{Deserialize, Serialize};

use crate::blend::BlendMode;
use crate::geom::IntRect;
use crate::layer::LayerId;
use crate::style::LayerStyle;

/// A named canvas within the document.
///
/// Photoshop implements artboards as groups with bounds; the practical
/// difference that matters is that each exports on its own, which is what
/// this supports.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Artboard {
    pub name: String,
    pub rect: IntRect,
}

/// A rectangle marked for separate export.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Slice {
    pub name: String,
    pub rect: IntRect,
    /// Slices Photoshop generated from guides are "auto"; ones the user
    /// drew are "user". Only the latter survive a re-slice.
    pub user: bool,
}

/// A pinned annotation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Note {
    pub at: (f32, f32),
    pub author: String,
    pub text: String,
}

/// A set of tally marks, as the Count tool leaves behind.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CountGroup {
    pub name: String,
    pub points: Vec<(f32, f32)>,
}

/// One layer's state inside a layer comp.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LayerCompState {
    pub layer: LayerId,
    pub visible: bool,
    pub opacity: f32,
    pub fill_opacity: f32,
    pub blend: BlendMode,
    pub style: LayerStyle,
}

/// A named snapshot of every layer's visibility, opacity, blend and
/// effects -- the three things Photoshop's Layer Comps panel offers.
///
/// Pixels are deliberately not included: a comp is a way of showing the
/// same artwork several ways, not a second copy of it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LayerComp {
    pub name: String,
    pub states: Vec<LayerCompState>,
    /// Which of the three aspects this comp restores.
    pub apply_visibility: bool,
    pub apply_appearance: bool,
}

impl LayerComp {
    pub fn new(name: impl Into<String>) -> LayerComp {
        LayerComp {
            name: name.into(),
            states: Vec::new(),
            apply_visibility: true,
            apply_appearance: true,
        }
    }
}
