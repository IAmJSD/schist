//! Smart objects.
//!
//! A smart object keeps its *source* pixels alongside the transform that
//! places them on the canvas, and re-renders from the source every time the
//! transform changes. That is the whole point: scaling a smart object down
//! to a tenth and back up again returns the original, where doing the same
//! to an ordinary layer would leave you with a tenth of the detail
//! resampled up.
//!
//! Photoshop's version can also embed a whole nested document; this embeds
//! a raster, which covers what the feature is actually used for and is what
//! `Convert to Smart Object` produces.

use crate::geom::IntRect;
use crate::resample::{Affine, Filter};
use crate::tile::TileMap;
use photoslop_color::Depth;

/// The source artwork of a smart object, plus where it sits.
#[derive(Debug, Clone)]
pub struct SmartObject {
    /// Untouched source pixels, in their own coordinate space.
    pub source: TileMap,
    /// Pixel-tight bounds of `source`, cached so a re-render need not
    /// rescan the tiles.
    pub source_bounds: IntRect,
    /// Maps source space onto the canvas.
    pub transform: Affine,
    /// Resampling filter used when rendering.
    pub filter: Filter,
    /// Name of the embedded source, shown in the layers panel and written
    /// to PSD as the linked file's name.
    pub name: String,
}

impl SmartObject {
    /// Wrap existing pixels, with the identity transform.
    pub fn wrap(source: TileMap, name: impl Into<String>) -> SmartObject {
        let source_bounds = source.content_bounds();
        SmartObject {
            source,
            source_bounds,
            transform: Affine::IDENTITY,
            filter: Filter::Bicubic,
            name: name.into(),
        }
    }

    /// Render the source through the transform.
    ///
    /// Always from the *source*, never from the last render, which is what
    /// keeps repeated transforms lossless.
    pub fn render(&self, depth: Depth, clip: IntRect) -> TileMap {
        if self.source_bounds.is_empty() {
            return TileMap::new();
        }
        crate::resample::transform_tiles(&self.source, &self.transform, depth, self.filter, clip)
    }

    /// Where the transformed artwork lands on the canvas.
    pub fn placed_bounds(&self) -> IntRect {
        let b = self.source_bounds;
        if b.is_empty() {
            return IntRect::EMPTY;
        }
        let corners = [
            (b.left as f32, b.top as f32),
            (b.right as f32, b.top as f32),
            (b.right as f32, b.bottom as f32),
            (b.left as f32, b.bottom as f32),
        ];
        let mut out = IntRect::EMPTY;
        for (x, y) in corners {
            let (tx, ty) = self.transform.apply(x, y);
            out = out.union(&IntRect::new(
                tx.floor() as i32,
                ty.floor() as i32,
                tx.ceil() as i32 + 1,
                ty.ceil() as i32 + 1,
            ));
        }
        out
    }

    /// Apply a further transform, composed onto the existing one so the
    /// source is only ever resampled once.
    ///
    /// `m` is in canvas space -- it is the gesture the user just made, not
    /// something in the source's coordinates -- so it composes *after* the
    /// existing placement. (`Affine::then` reads right to left: `x.then(y)`
    /// applies `y` first.)
    pub fn apply(&mut self, m: &Affine) {
        self.transform = m.then(&self.transform);
    }
}
