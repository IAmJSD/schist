//! The layer tree.
//!
//! Children are stored bottom-to-top (index 0 composites first), matching
//! PSD file order; UIs display the reverse. Text layers and smart objects
//! import as raster layers carrying their original PSD blocks in `extras`
//! so they round-trip unharmed (see PLAN.md §4).

use crate::geom::IntRect;
use crate::tile::{MaskTileMap, TileMap};
use crate::BlendMode;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LayerId(pub u64);

static NEXT_LAYER_ID: AtomicU64 = AtomicU64::new(1);

impl LayerId {
    pub fn next() -> LayerId {
        LayerId(NEXT_LAYER_ID.fetch_add(1, Ordering::Relaxed))
    }
}

/// A preserved-verbatim PSD "additional layer information" block (or image
/// resource) that we don't interpret. Re-emitted byte-for-byte on save.
#[derive(Debug, Clone, PartialEq)]
pub struct RawBlock {
    pub key: [u8; 4],
    pub data: Vec<u8>,
}

/// Raster layer mask. 255 = fully revealed, 0 = fully hidden.
/// `default_value` applies outside the stored tiles (PSD masks are bounded).
#[derive(Debug, Clone)]
pub struct LayerMask {
    pub tiles: MaskTileMap,
    pub enabled: bool,
    pub linked: bool,
    pub default_value: u8,
    pub bounds: IntRect,
}

impl LayerMask {
    pub fn new_revealing() -> Self {
        LayerMask {
            tiles: MaskTileMap::new(),
            enabled: true,
            linked: true,
            default_value: 255,
            bounds: IntRect::EMPTY,
        }
    }

    pub fn value(&self, x: i32, y: i32) -> u8 {
        if self.bounds.contains(x, y) {
            self.tiles.value(x, y)
        } else {
            self.default_value
        }
    }
}

/// Which adjustment a non-destructive adjustment layer performs. Parameter
/// *parsing* and rendering land in M8; until then `raw` preserves the exact
/// PSD payload for round-trip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdjustmentKind {
    Levels,
    Curves,
    HueSaturation,
    BrightnessContrast,
    BlackWhite,
    SolidColor,
    GradientFill,
    PatternFill,
    Invert,
    Posterize,
    Threshold,
    Other([u8; 4]),
}

impl AdjustmentKind {
    pub fn from_psd_key(key: &[u8; 4]) -> Option<AdjustmentKind> {
        use AdjustmentKind::*;
        Some(match key {
            b"levl" => Levels,
            b"curv" => Curves,
            b"hue2" | b"hue " => HueSaturation,
            b"brit" => BrightnessContrast,
            b"blwh" => BlackWhite,
            b"SoCo" => SolidColor,
            b"GdFl" => GradientFill,
            b"PtFl" => PatternFill,
            b"nvrt" => Invert,
            b"post" => Posterize,
            b"thrs" => Threshold,
            _ => return None,
        })
    }

    pub fn display_name(&self) -> &'static str {
        use AdjustmentKind::*;
        match self {
            Levels => "Levels",
            Curves => "Curves",
            HueSaturation => "Hue/Saturation",
            BrightnessContrast => "Brightness/Contrast",
            BlackWhite => "Black & White",
            SolidColor => "Solid Color",
            GradientFill => "Gradient Fill",
            PatternFill => "Pattern Fill",
            Invert => "Invert",
            Posterize => "Posterize",
            Threshold => "Threshold",
            Other(_) => "Adjustment",
        }
    }
}

#[derive(Debug, Clone)]
pub struct AdjustmentData {
    pub kind: AdjustmentKind,
    /// The verbatim PSD additional-info payload for this adjustment.
    pub raw: Vec<u8>,
}

#[derive(Debug, Clone, Default)]
pub struct RasterLayer {
    pub tiles: TileMap,
}

#[derive(Debug, Clone)]
pub struct GroupLayer {
    pub children: Vec<Layer>,
    pub open: bool,
}

impl Default for GroupLayer {
    fn default() -> Self {
        GroupLayer {
            children: Vec::new(),
            open: true,
        }
    }
}

#[derive(Debug, Clone)]
pub enum LayerKind {
    Raster(RasterLayer),
    Group(GroupLayer),
    Adjustment(AdjustmentData),
}

#[derive(Debug, Clone)]
pub struct Layer {
    pub id: LayerId,
    pub name: String,
    pub visible: bool,
    /// 0.0..=1.0
    pub opacity: f32,
    /// PSD fill opacity ("Fill" in the layers panel), 0.0..=1.0.
    pub fill_opacity: f32,
    pub blend: BlendMode,
    /// True if this layer is clipped to the first non-clipping layer below.
    pub clipping: bool,
    pub locked: bool,
    pub mask: Option<LayerMask>,
    pub kind: LayerKind,
    /// Preserved PSD blocks (text engine data, effects, smart object refs…).
    pub extras: Vec<RawBlock>,
}

impl Layer {
    pub fn new_raster(name: impl Into<String>) -> Layer {
        Layer {
            id: LayerId::next(),
            name: name.into(),
            visible: true,
            opacity: 1.0,
            fill_opacity: 1.0,
            blend: BlendMode::Normal,
            clipping: false,
            locked: false,
            mask: None,
            kind: LayerKind::Raster(RasterLayer::default()),
            extras: Vec::new(),
        }
    }

    pub fn new_group(name: impl Into<String>) -> Layer {
        Layer {
            blend: BlendMode::PassThrough,
            kind: LayerKind::Group(GroupLayer::default()),
            ..Layer::new_raster(name)
        }
    }

    pub fn is_group(&self) -> bool {
        matches!(self.kind, LayerKind::Group(_))
    }

    pub fn as_raster(&self) -> Option<&RasterLayer> {
        match &self.kind {
            LayerKind::Raster(r) => Some(r),
            _ => None,
        }
    }

    pub fn as_raster_mut(&mut self) -> Option<&mut RasterLayer> {
        match &mut self.kind {
            LayerKind::Raster(r) => Some(r),
            _ => None,
        }
    }

    pub fn children(&self) -> Option<&[Layer]> {
        match &self.kind {
            LayerKind::Group(g) => Some(&g.children),
            _ => None,
        }
    }

    /// Content bounds in document space (tile-granular for raster).
    pub fn content_bounds(&self) -> IntRect {
        match &self.kind {
            LayerKind::Raster(r) => r.tiles.tile_bounds(),
            LayerKind::Group(g) => {
                let mut b = IntRect::EMPTY;
                for child in &g.children {
                    b = b.union(&child.content_bounds());
                }
                b
            }
            LayerKind::Adjustment(_) => IntRect::EMPTY,
        }
    }

    fn find(&self, id: LayerId) -> Option<&Layer> {
        if self.id == id {
            return Some(self);
        }
        self.children()?.iter().find_map(|c| c.find(id))
    }

    fn find_mut(&mut self, id: LayerId) -> Option<&mut Layer> {
        if self.id == id {
            return Some(self);
        }
        match &mut self.kind {
            LayerKind::Group(g) => g.children.iter_mut().find_map(|c| c.find_mut(id)),
            _ => None,
        }
    }
}

/// Path from the root to a layer: indices into successive `children` vecs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayerPath(pub Vec<usize>);

/// The root of a document's layer stack. A thin wrapper over an implicit
/// pass-through group that can't be selected or deleted.
#[derive(Debug, Clone, Default)]
pub struct LayerTree {
    pub layers: Vec<Layer>,
}

impl LayerTree {
    pub fn find(&self, id: LayerId) -> Option<&Layer> {
        self.layers.iter().find_map(|l| l.find(id))
    }

    pub fn find_mut(&mut self, id: LayerId) -> Option<&mut Layer> {
        self.layers.iter_mut().find_map(|l| l.find_mut(id))
    }

    pub fn path_of(&self, id: LayerId) -> Option<LayerPath> {
        fn walk(layers: &[Layer], id: LayerId, prefix: &mut Vec<usize>) -> bool {
            for (i, layer) in layers.iter().enumerate() {
                prefix.push(i);
                if layer.id == id {
                    return true;
                }
                if let Some(children) = layer.children() {
                    if walk(children, id, prefix) {
                        return true;
                    }
                }
                prefix.pop();
            }
            false
        }
        let mut path = Vec::new();
        walk(&self.layers, id, &mut path).then_some(LayerPath(path))
    }

    fn siblings_mut(&mut self, path: &LayerPath) -> Option<&mut Vec<Layer>> {
        let (last, parents) = path.0.split_last()?;
        let _ = last;
        let mut vec = &mut self.layers;
        for &ix in parents {
            match &mut vec.get_mut(ix)?.kind {
                LayerKind::Group(g) => vec = &mut g.children,
                _ => return None,
            }
        }
        Some(vec)
    }

    /// Insert a layer at an explicit path position.
    pub fn insert_at(&mut self, path: &LayerPath, layer: Layer) -> bool {
        let Some(&ix) = path.0.last() else {
            return false;
        };
        match self.siblings_mut(path) {
            Some(vec) if ix <= vec.len() => {
                vec.insert(ix, layer);
                true
            }
            _ => false,
        }
    }

    pub fn remove_at(&mut self, path: &LayerPath) -> Option<Layer> {
        let &ix = path.0.last()?;
        let vec = self.siblings_mut(path)?;
        if ix < vec.len() {
            Some(vec.remove(ix))
        } else {
            None
        }
    }

    pub fn remove(&mut self, id: LayerId) -> Option<(LayerPath, Layer)> {
        let path = self.path_of(id)?;
        let layer = self.remove_at(&path)?;
        Some((path, layer))
    }

    /// Iterate all layers depth-first, bottom-to-top.
    pub fn iter(&self) -> impl Iterator<Item = &Layer> {
        fn push<'a>(layers: &'a [Layer], out: &mut Vec<&'a Layer>) {
            for l in layers {
                out.push(l);
                if let Some(c) = l.children() {
                    push(c, out);
                }
            }
        }
        let mut out = Vec::new();
        push(&self.layers, &mut out);
        out.into_iter()
    }

    pub fn len(&self) -> usize {
        self.iter().count()
    }

    pub fn is_empty(&self) -> bool {
        self.layers.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree_with_group() -> (LayerTree, LayerId, LayerId, LayerId) {
        let bottom = Layer::new_raster("bottom");
        let inner = Layer::new_raster("inner");
        let mut group = Layer::new_group("group");
        let (b_id, i_id, g_id) = (bottom.id, inner.id, group.id);
        if let LayerKind::Group(g) = &mut group.kind {
            g.children.push(inner);
        }
        let tree = LayerTree {
            layers: vec![bottom, group],
        };
        (tree, b_id, i_id, g_id)
    }

    #[test]
    fn path_of_nested_layer() {
        let (tree, b, i, g) = tree_with_group();
        assert_eq!(tree.path_of(b), Some(LayerPath(vec![0])));
        assert_eq!(tree.path_of(g), Some(LayerPath(vec![1])));
        assert_eq!(tree.path_of(i), Some(LayerPath(vec![1, 0])));
    }

    #[test]
    fn remove_and_reinsert_round_trips() {
        let (mut tree, _, i, _) = tree_with_group();
        let (path, layer) = tree.remove(i).unwrap();
        assert!(tree.find(i).is_none());
        assert!(tree.insert_at(&path, layer));
        assert_eq!(tree.find(i).unwrap().name, "inner");
    }

    #[test]
    fn iter_is_depth_first_bottom_up() {
        let (tree, ..) = tree_with_group();
        let names: Vec<_> = tree.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(names, vec!["bottom", "group", "inner"]);
    }
}
