//! Photoslop kernel: document model, tiles, layers, selection, history.
//!
//! This crate deliberately contains **no user-facing features** — those live
//! in plugins (see `photoslop-plugin-api`). If every plugin were removed the
//! app would boot to an empty workspace that can do nothing.

pub mod blend;
pub mod document;
pub mod geom;
pub mod history;
pub mod layer;
pub mod path;
pub mod resample;
pub mod selection;
pub mod smart;
pub mod style;
pub mod tile;

pub use blend::BlendMode;
pub use document::{
    blit_rgba8, Document, DocumentId, EditBuilder, Guide, PreservedResource, StrokeEdit,
};
pub use geom::IntRect;
pub use history::{Edit, EditOp, History, LayerProps};
pub use layer::{
    AdjustmentData, AdjustmentKind, GroupLayer, Layer, LayerId, LayerKind, LayerMask, LayerPath,
    LayerTree, RasterLayer, RawBlock, StyledRaster,
};
pub use path::{Anchor, SubPath, VectorPath};
pub use resample::{Affine, Filter};
pub use selection::{SelectOp, Selection};
pub use smart::SmartObject;
pub use style::{
    BevelStyle, BevelStyle_, ColorOverlayStyle, Effect, GlowStyle, GradientOverlayStyle,
    GradientShape, LayerStyle, SatinStyle, ShadowStyle, StrokePosition, StrokeStyle, Technique,
};
pub use tile::{MaskTileMap, TileBuf, TileCoord, TileMap, TILE_PIXELS, TILE_SIZE};

pub use photoslop_color as color;
