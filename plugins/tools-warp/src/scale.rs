//! Content-Aware Scale: seam carving.
//!
//! Resizing an image normally squashes everything equally. Seam carving
//! instead repeatedly removes (or duplicates) the single connected path of
//! pixels through the image that costs the least -- the "seam" -- so flat
//! sky gives way before a face does.
//!
//! Energy is the gradient magnitude, and a protect mask (Photoshop's
//! "Protect" channel, here the selection) adds enough energy that seams
//! route around it.
//!
//! The carving itself lives in `schist_fx`, which is where the GPU seam
//! is: one command runs the whole loop, hundreds of full-image passes, and
//! that is the shape a second device pays for. What stays here is the tile
//! plumbing and the transpose that turns a width carve into a height one.

use schist_color::{Depth, Rgba};
use schist_core::{IntRect, Selection, TileCoord, TileMap, TILE_SIZE};

/// A flat RGBA image, which is what the carving works on.
pub struct Image {
    pub width: usize,
    pub height: usize,
    /// Straight-alpha RGBA f32, four floats per pixel, row major.
    pub px: Vec<f32>,
    /// Extra energy per pixel, keeping seams away from protected areas.
    pub protect: Vec<f32>,
}

impl Image {
    /// Pull a rect out of a tile map, with the selection as protection.
    pub fn from_tiles(tiles: &TileMap, rect: IntRect, protect: Option<&Selection>) -> Image {
        let (width, height) = (rect.width().max(0) as usize, rect.height().max(0) as usize);
        let mut px = Vec::with_capacity(width * height * 4);
        let mut prot = Vec::with_capacity(width * height);
        for y in 0..height {
            for x in 0..width {
                let (dx, dy) = (rect.left + x as i32, rect.top + y as i32);
                let p = tiles.pixel(dx, dy);
                px.extend_from_slice(&[p.r, p.g, p.b, p.a]);
                let p = protect
                    .filter(|s| !s.is_empty())
                    .map(|s| s.coverage(dx, dy) as f32 / 255.0)
                    .unwrap_or(0.0);
                // A large but finite bonus: seams will still cross a
                // protected area if there is genuinely nowhere else.
                prot.push(p * 1000.0);
            }
        }
        Image {
            width,
            height,
            px,
            protect: prot,
        }
    }

    pub fn into_tiles(self, rect: IntRect, depth: Depth) -> TileMap {
        let mut out = TileMap::new();
        let region = IntRect::new(
            rect.left,
            rect.top,
            rect.left + self.width as i32,
            rect.top + self.height as i32,
        );
        if region.is_empty() {
            return out;
        }
        for coord in TileCoord::covering(&region) {
            let trect = coord.rect();
            let clip = trect.intersect(&region);
            if clip.is_empty() {
                continue;
            }
            let buf = out.get_mut_or_insert(coord, depth);
            for y in clip.top..clip.bottom {
                for x in clip.left..clip.right {
                    let i =
                        ((y - region.top) as usize * self.width + (x - region.left) as usize) * 4;
                    let ix = ((y - trect.top) * TILE_SIZE + (x - trect.left)) as usize;
                    buf.set(
                        ix,
                        Rgba::new(self.px[i], self.px[i + 1], self.px[i + 2], self.px[i + 3]),
                    );
                }
            }
        }
        out
    }

    fn transpose(&mut self) {
        let (w, h) = (self.width, self.height);
        let mut px = Vec::with_capacity(w * h * 4);
        let mut prot = Vec::with_capacity(w * h);
        for x in 0..w {
            for y in 0..h {
                let i = (y * w + x) * 4;
                px.extend_from_slice(&self.px[i..i + 4]);
                prot.push(self.protect[y * w + x]);
            }
        }
        self.px = px;
        self.protect = prot;
        self.width = h;
        self.height = w;
    }

    /// Resize to `(width, height)` by carving or growing seams.
    pub fn content_aware_resize(&mut self, width: usize, height: usize) {
        // Width first, then height via a transpose, which is the standard
        // way to avoid writing every routine twice.
        self.resize_width(width);
        if self.height != height {
            self.transpose();
            self.resize_width(height);
            self.transpose();
        }
    }

    fn resize_width(&mut self, target: usize) {
        if self.width == target || self.width == 0 || self.height == 0 {
            return;
        }
        let carved = schist_fx::carve(&schist_fx::CarveJob {
            px: &self.px,
            protect: &self.protect,
            width: self.width,
            height: self.height,
            target_width: target,
        });
        self.px = carved.px;
        self.protect = carved.protect;
        self.width = carved.width;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A vertical red bar on flat grey: carving should eat the grey and
    /// leave the bar alone.
    fn bar(width: usize, height: usize) -> Image {
        let mut px = Vec::with_capacity(width * height * 4);
        for _y in 0..height {
            for x in 0..width {
                let inside = x + 3 >= width / 2 && x < width / 2 + 3;
                px.extend_from_slice(if inside {
                    &[1.0, 0.0, 0.0, 1.0]
                } else {
                    &[0.5, 0.5, 0.5, 1.0]
                });
            }
        }
        Image {
            width,
            height,
            px,
            protect: vec![0.0; width * height],
        }
    }

    fn count_red(img: &Image) -> usize {
        img.px
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|p| p[0] > 0.8 && p[1] < 0.2)
            .count()
    }

    #[test]
    fn carving_removes_the_flat_areas_and_spares_the_subject() {
        let mut img = bar(64, 32);
        let before = count_red(&img);
        img.content_aware_resize(40, 32);
        assert_eq!(img.width, 40);
        assert_eq!(img.height, 32);
        let after = count_red(&img);
        // A plain squash would have lost 40/64 of the bar.
        assert!(
            after as f32 > before as f32 * 0.85,
            "the subject was carved away: {before} -> {after}"
        );
    }

    #[test]
    fn growing_widens_without_stretching_the_subject() {
        let mut img = bar(48, 24);
        let before = count_red(&img);
        img.content_aware_resize(72, 24);
        assert_eq!(img.width, 72);
        let after = count_red(&img);
        assert!(
            (after as f32) < before as f32 * 1.5,
            "the subject was stretched: {before} -> {after}"
        );
    }

    #[test]
    fn protection_keeps_seams_off_the_marked_area() {
        // Protect the left third, which is otherwise the cheapest place
        // to carve because it is flat.
        let mut img = bar(64, 32);
        for y in 0..32 {
            for x in 0..20 {
                img.protect[y * 64 + x] = 1000.0;
            }
        }
        // Mark the protected region so it can be counted afterwards.
        for y in 0..32 {
            for x in 0..20 {
                img.px[(y * 64 + x) * 4..(y * 64 + x) * 4 + 4]
                    .copy_from_slice(&[0.0, 0.0, 1.0, 1.0]);
            }
        }
        let count_blue = |img: &Image| {
            img.px
                .as_chunks::<4>()
                .0
                .iter()
                .filter(|p| p[2] > 0.8 && p[0] < 0.2)
                .count()
        };
        let before = count_blue(&img);
        img.content_aware_resize(48, 32);
        let after = count_blue(&img);
        assert!(
            after as f32 > before as f32 * 0.9,
            "seams cut through the protected area: {before} -> {after}"
        );
    }

    #[test]
    fn height_is_carved_as_well_as_width() {
        let mut img = bar(32, 32);
        img.content_aware_resize(24, 20);
        assert_eq!((img.width, img.height), (24, 20));
        assert_eq!(img.px.len(), 24 * 20 * 4);
    }

    #[test]
    fn degenerate_sizes_do_not_hang_or_panic() {
        let mut img = bar(8, 8);
        img.content_aware_resize(1, 1);
        assert_eq!((img.width, img.height), (1, 1));
        let mut img = bar(4, 4);
        img.content_aware_resize(0, 0);
        assert!(img.width >= 1 && img.height >= 1);
    }
}
