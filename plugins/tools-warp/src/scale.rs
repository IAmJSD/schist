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

use photoslop_color::{Depth, Rgba};
use photoslop_core::{IntRect, Selection, TileCoord, TileMap, TILE_SIZE};

/// A flat RGBA image, which is what the carving works on.
pub struct Image {
    pub width: usize,
    pub height: usize,
    pub px: Vec<Rgba>,
    /// Extra energy per pixel, keeping seams away from protected areas.
    pub protect: Vec<f32>,
}

impl Image {
    /// Pull a rect out of a tile map, with the selection as protection.
    pub fn from_tiles(tiles: &TileMap, rect: IntRect, protect: Option<&Selection>) -> Image {
        let (width, height) = (rect.width().max(0) as usize, rect.height().max(0) as usize);
        let mut px = Vec::with_capacity(width * height);
        let mut prot = Vec::with_capacity(width * height);
        for y in 0..height {
            for x in 0..width {
                let (dx, dy) = (rect.left + x as i32, rect.top + y as i32);
                px.push(tiles.pixel(dx, dy));
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
                    let i = (y - region.top) as usize * self.width + (x - region.left) as usize;
                    let ix = ((y - trect.top) * TILE_SIZE + (x - trect.left)) as usize;
                    buf.set(ix, self.px[i]);
                }
            }
        }
        out
    }

    #[inline]
    fn at(&self, x: usize, y: usize) -> Rgba {
        self.px[y * self.width + x]
    }

    /// Gradient magnitude plus protection.
    fn energy(&self) -> Vec<f32> {
        let (w, h) = (self.width, self.height);
        let mut out = vec![0.0f32; w * h];
        let lum = |p: Rgba| 0.299 * p.r + 0.587 * p.g + 0.114 * p.b;
        for y in 0..h {
            for x in 0..w {
                let l = lum(self.at(x.saturating_sub(1), y));
                let r = lum(self.at((x + 1).min(w - 1), y));
                let u = lum(self.at(x, y.saturating_sub(1)));
                let d = lum(self.at(x, (y + 1).min(h - 1)));
                // Fully transparent pixels are free to remove.
                let alpha = self.at(x, y).a;
                out[y * w + x] = ((r - l).abs() + (d - u).abs()) * alpha + self.protect[y * w + x];
            }
        }
        out
    }

    /// The lowest-energy top-to-bottom seam, as one x per row.
    fn vertical_seam(&self) -> Vec<usize> {
        let (w, h) = (self.width, self.height);
        if w == 0 || h == 0 {
            return Vec::new();
        }
        let energy = self.energy();
        // Cumulative cost, and which of the three pixels above we came
        // from, so the seam can be walked back.
        let mut cost = energy.clone();
        let mut from = vec![0i8; w * h];
        for y in 1..h {
            for x in 0..w {
                let mut best = cost[(y - 1) * w + x];
                let mut best_d = 0i8;
                if x > 0 && cost[(y - 1) * w + x - 1] < best {
                    best = cost[(y - 1) * w + x - 1];
                    best_d = -1;
                }
                if x + 1 < w && cost[(y - 1) * w + x + 1] < best {
                    best = cost[(y - 1) * w + x + 1];
                    best_d = 1;
                }
                cost[y * w + x] = energy[y * w + x] + best;
                from[y * w + x] = best_d;
            }
        }
        let mut x = (0..w)
            .min_by(|a, b| cost[(h - 1) * w + a].total_cmp(&cost[(h - 1) * w + b]))
            .unwrap_or(0);
        let mut seam = vec![0usize; h];
        for y in (0..h).rev() {
            seam[y] = x;
            let d = from[y * w + x];
            x = (x as isize + d as isize).clamp(0, w as isize - 1) as usize;
        }
        seam
    }

    /// Remove one vertical seam, narrowing the image by a pixel.
    fn carve_vertical(&mut self) {
        let seam = self.vertical_seam();
        if seam.is_empty() || self.width == 0 {
            return;
        }
        let (w, h) = (self.width, self.height);
        let mut px = Vec::with_capacity((w - 1) * h);
        let mut prot = Vec::with_capacity((w - 1) * h);
        for (y, cut) in seam.iter().enumerate() {
            for x in 0..w {
                if x == *cut {
                    continue;
                }
                px.push(self.px[y * w + x]);
                prot.push(self.protect[y * w + x]);
            }
        }
        self.px = px;
        self.protect = prot;
        self.width = w - 1;
    }

    /// Duplicate one vertical seam, widening the image by a pixel.
    fn grow_vertical(&mut self) {
        let seam = self.vertical_seam();
        if seam.is_empty() {
            return;
        }
        let (w, h) = (self.width, self.height);
        let mut px = Vec::with_capacity((w + 1) * h);
        let mut prot = Vec::with_capacity((w + 1) * h);
        for (y, cut) in seam.iter().enumerate() {
            for x in 0..w {
                px.push(self.px[y * w + x]);
                prot.push(self.protect[y * w + x]);
                if x == *cut {
                    // The inserted pixel is the average of its neighbours,
                    // so the duplicate does not read as a hard repeat.
                    let a = self.px[y * w + x];
                    let b = self.px[y * w + (x + 1).min(w - 1)];
                    px.push(Rgba::new(
                        (a.r + b.r) / 2.0,
                        (a.g + b.g) / 2.0,
                        (a.b + b.b) / 2.0,
                        (a.a + b.a) / 2.0,
                    ));
                    // Protect the inserted column too, or every later seam
                    // picks the same place and a crease forms.
                    prot.push(self.protect[y * w + x] + 200.0);
                }
            }
        }
        self.px = px;
        self.protect = prot;
        self.width = w + 1;
    }

    fn transpose(&mut self) {
        let (w, h) = (self.width, self.height);
        let mut px = Vec::with_capacity(w * h);
        let mut prot = Vec::with_capacity(w * h);
        for x in 0..w {
            for y in 0..h {
                px.push(self.px[y * w + x]);
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
        while self.width > width.max(1) {
            self.carve_vertical();
        }
        while self.width < width {
            self.grow_vertical();
        }
        if self.height != height {
            self.transpose();
            while self.width > height.max(1) {
                self.carve_vertical();
            }
            while self.width < height {
                self.grow_vertical();
            }
            self.transpose();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A vertical red bar on flat grey: carving should eat the grey and
    /// leave the bar alone.
    fn bar(width: usize, height: usize) -> Image {
        let mut px = Vec::with_capacity(width * height);
        for _y in 0..height {
            for x in 0..width {
                let inside = x + 3 >= width / 2 && x < width / 2 + 3;
                px.push(if inside {
                    Rgba::new(1.0, 0.0, 0.0, 1.0)
                } else {
                    Rgba::new(0.5, 0.5, 0.5, 1.0)
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
        img.px.iter().filter(|p| p.r > 0.8 && p.g < 0.2).count()
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
                img.px[y * 64 + x] = Rgba::new(0.0, 0.0, 1.0, 1.0);
            }
        }
        let before = img.px.iter().filter(|p| p.b > 0.8 && p.r < 0.2).count();
        img.content_aware_resize(48, 32);
        let after = img.px.iter().filter(|p| p.b > 0.8 && p.r < 0.2).count();
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
        assert_eq!(img.px.len(), 24 * 20);
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
