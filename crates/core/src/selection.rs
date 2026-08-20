//! Selections as anti-aliased coverage masks.
//!
//! A selection is a sparse single-channel mask over document space
//! (0 = unselected, 255 = fully selected). Every tool and filter consults it
//! as "where edits apply". An *empty* selection means "everything selected"
//! by Photoshop convention; callers use `coverage()` which encodes that.

use crate::geom::IntRect;
use crate::tile::{MaskTileMap, TileCoord, TILE_PIXELS, TILE_SIZE};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectOp {
    Replace,
    Add,
    Subtract,
    Intersect,
}

#[derive(Debug, Clone, Default)]
pub struct Selection {
    pub mask: MaskTileMap,
    /// Cached pixel-tight bounds of nonzero coverage.
    bounds: IntRect,
    /// False = no selection in effect (everything editable).
    active: bool,
    /// Bumped on every change, so views can cache derived data (the
    /// marching-ants outline) without diffing the mask.
    generation: u64,
}

impl Selection {
    pub fn new() -> Self {
        Self::default()
    }

    /// True when no selection is in effect (== everything editable).
    pub fn is_empty(&self) -> bool {
        !self.active
    }

    /// Mark the selection active (used by tools writing `mask` directly).
    pub fn activate(&mut self) {
        self.active = true;
    }

    /// Counter identifying this selection's contents.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn bounds(&self) -> IntRect {
        self.bounds
    }

    /// Effective coverage at a pixel: 255 everywhere when no selection.
    #[inline]
    pub fn coverage(&self, x: i32, y: i32) -> u8 {
        if self.is_empty() {
            255
        } else {
            self.mask.value(x, y)
        }
    }

    /// Recompute pixel-tight bounds by scanning tile contents.
    pub fn recompute_bounds(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.mask.prune_blank();
        let mut bounds = IntRect::EMPTY;
        for (coord, buf) in self.mask.iter() {
            let trect = coord.rect();
            for ly in 0..TILE_SIZE {
                let row = &buf[(ly * TILE_SIZE) as usize..((ly + 1) * TILE_SIZE) as usize];
                let Some(first) = row.iter().position(|&v| v != 0) else {
                    continue;
                };
                let last = row.iter().rposition(|&v| v != 0).unwrap();
                let y = trect.top + ly;
                bounds = bounds.union(&IntRect::new(
                    trect.left + first as i32,
                    y,
                    trect.left + last as i32 + 1,
                    y + 1,
                ));
            }
        }
        self.bounds = bounds;
    }

    /// Apply a shape (given as a per-pixel coverage function over `rect`)
    /// with a boolean op.
    pub fn apply_shape(
        &mut self,
        rect: IntRect,
        op: SelectOp,
        coverage_fn: impl Fn(i32, i32) -> u8,
    ) {
        self.active = true;
        if op == SelectOp::Replace {
            self.mask = MaskTileMap::new();
        }
        if op == SelectOp::Intersect {
            // Zero out everything outside rect, then intersect inside it.
            let coords: Vec<TileCoord> = self.mask.iter().map(|(c, _)| *c).collect();
            for coord in coords {
                let trect = coord.rect();
                let buf = self.mask.get_mut_or_insert(coord);
                for ly in 0..TILE_SIZE {
                    for lx in 0..TILE_SIZE {
                        let x = trect.left + lx;
                        let y = trect.top + ly;
                        let ix = (ly * TILE_SIZE + lx) as usize;
                        let shape = if rect.contains(x, y) {
                            coverage_fn(x, y)
                        } else {
                            0
                        };
                        let old = buf[ix];
                        buf[ix] = ((old as u16 * shape as u16) / 255) as u8;
                    }
                }
            }
            self.recompute_bounds();
            return;
        }
        for coord in TileCoord::covering(&rect) {
            let trect = coord.rect();
            let clip = trect.intersect(&rect);
            if clip.is_empty() {
                continue;
            }
            let buf = self.mask.get_mut_or_insert(coord);
            for y in clip.top..clip.bottom {
                let ly = (y - trect.top) as usize;
                for x in clip.left..clip.right {
                    let lx = (x - trect.left) as usize;
                    let ix = ly * TILE_SIZE as usize + lx;
                    let shape = coverage_fn(x, y);
                    if shape == 0 {
                        continue;
                    }
                    buf[ix] = match op {
                        SelectOp::Replace | SelectOp::Add => buf[ix].max(shape),
                        SelectOp::Subtract => buf[ix].saturating_sub(shape),
                        SelectOp::Intersect => unreachable!(),
                    };
                }
            }
        }
        self.recompute_bounds();
    }

    /// Rectangular selection (hard edges).
    pub fn select_rect(&mut self, rect: IntRect, op: SelectOp) {
        self.apply_shape(rect, op, |_, _| 255);
    }

    /// Elliptical selection inscribed in `rect`, anti-aliased with 4x4
    /// supersampling at the boundary.
    pub fn select_ellipse(&mut self, rect: IntRect, op: SelectOp) {
        if rect.is_empty() {
            return;
        }
        let cx = (rect.left + rect.right) as f64 / 2.0;
        let cy = (rect.top + rect.bottom) as f64 / 2.0;
        let rx = rect.width() as f64 / 2.0;
        let ry = rect.height() as f64 / 2.0;
        let inside = move |px: f64, py: f64| {
            let dx = (px - cx) / rx;
            let dy = (py - cy) / ry;
            dx * dx + dy * dy <= 1.0
        };
        self.apply_shape(rect, op, move |x, y| {
            // Fast paths: fully in/out at pixel-center distance beyond 1px.
            let mut hits = 0u32;
            for sy in 0..4 {
                for sx in 0..4 {
                    let px = x as f64 + (sx as f64 + 0.5) / 4.0;
                    let py = y as f64 + (sy as f64 + 0.5) / 4.0;
                    if inside(px, py) {
                        hits += 1;
                    }
                }
            }
            ((hits * 255) / 16) as u8
        });
    }

    /// Polygon (lasso) selection, even-odd fill, anti-aliased with 4x4
    /// supersampling. Points are document-space.
    pub fn select_polygon(&mut self, points: &[(f32, f32)], op: SelectOp) {
        if points.len() < 3 {
            return;
        }
        let mut rect = IntRect::EMPTY;
        for &(x, y) in points {
            rect = rect.union(&IntRect::from_xywh(
                x.floor() as i32,
                y.floor() as i32,
                2,
                2,
            ));
        }
        let pts: Vec<(f64, f64)> = points.iter().map(|&(x, y)| (x as f64, y as f64)).collect();
        let inside = move |px: f64, py: f64| {
            let mut winding = false;
            let n = pts.len();
            for i in 0..n {
                let (x1, y1) = pts[i];
                let (x2, y2) = pts[(i + 1) % n];
                if (y1 > py) != (y2 > py) {
                    let xint = x1 + (py - y1) / (y2 - y1) * (x2 - x1);
                    if px < xint {
                        winding = !winding;
                    }
                }
            }
            winding
        };
        self.apply_shape(rect, op, move |x, y| {
            let mut hits = 0u32;
            for sy in 0..4 {
                for sx in 0..4 {
                    let px = x as f64 + (sx as f64 + 0.5) / 4.0;
                    let py = y as f64 + (sy as f64 + 0.5) / 4.0;
                    if inside(px, py) {
                        hits += 1;
                    }
                }
            }
            ((hits * 255) / 16) as u8
        });
    }

    /// Invert within the canvas.
    pub fn invert(&mut self, canvas: IntRect) {
        let mut inverted = MaskTileMap::new();
        for coord in TileCoord::covering(&canvas) {
            let trect = coord.rect();
            let clip = trect.intersect(&canvas);
            let buf = inverted.get_mut_or_insert(coord);
            for y in clip.top..clip.bottom {
                let ly = (y - trect.top) as usize;
                for x in clip.left..clip.right {
                    let lx = (x - trect.left) as usize;
                    buf[ly * TILE_SIZE as usize + lx] = 255 - self.coverage(x, y);
                }
            }
        }
        self.mask = inverted;
        self.active = true;
        self.recompute_bounds();
    }

    pub fn select_all(&mut self, canvas: IntRect) {
        self.mask = MaskTileMap::new();
        self.select_rect(canvas, SelectOp::Replace);
    }

    pub fn deselect(&mut self) {
        self.mask = MaskTileMap::new();
        self.bounds = IntRect::EMPTY;
        self.active = false;
        self.generation = self.generation.wrapping_add(1);
    }

    /// Feather: approximate gaussian blur of the coverage mask using three
    /// box blurs. Radius in pixels.
    pub fn feather(&mut self, radius: f32) {
        if self.is_empty() || radius < 0.5 {
            return;
        }
        let work = self.bounds.inflated(radius.ceil() as i32 * 3 + 1);
        let w = work.width() as usize;
        let h = work.height() as usize;
        let mut a = vec![0f32; w * h];
        for y in 0..h {
            for x in 0..w {
                a[y * w + x] = self.mask.value(work.left + x as i32, work.top + y as i32) as f32;
            }
        }
        let r = (radius / (3f32).sqrt()).max(0.5);
        let mut b = vec![0f32; w * h];
        for _ in 0..3 {
            box_blur_h(&a, &mut b, w, h, r);
            box_blur_v(&b, &mut a, w, h, r);
        }
        let mut mask = MaskTileMap::new();
        for y in 0..h {
            for x in 0..w {
                let v = a[y * w + x].round().clamp(0.0, 255.0) as u8;
                if v > 0 {
                    let dx = work.left + x as i32;
                    let dy = work.top + y as i32;
                    let coord = TileCoord::containing(dx, dy);
                    let buf = mask.get_mut_or_insert(coord);
                    let lx = dx.rem_euclid(TILE_SIZE) as usize;
                    let ly = dy.rem_euclid(TILE_SIZE) as usize;
                    buf[ly * TILE_SIZE as usize + lx] = v;
                }
            }
        }
        self.mask = mask;
        self.recompute_bounds();
    }

    /// Trace the selection's boundary as polylines in document space.
    ///
    /// This is what "marching ants" actually are: the edges between
    /// selected and unselected pixels, not the bounding box. Runs of
    /// collinear edges are merged, so a rectangular selection comes back as
    /// four segments rather than thousands.
    pub fn outline(&self) -> Vec<Vec<(f32, f32)>> {
        if self.is_empty() || self.bounds.is_empty() {
            return Vec::new();
        }
        let bounds = self.bounds;
        let inside = |x: i32, y: i32| self.mask.value(x, y) >= 128;
        let mut segments: Vec<Vec<(f32, f32)>> = Vec::new();

        // Horizontal edges: scan each row boundary, merging runs.
        for y in bounds.top..=bounds.bottom {
            let mut run: Option<i32> = None;
            for x in bounds.left..=bounds.right {
                let edge = x < bounds.right && inside(x, y) != inside(x, y - 1);
                match (edge, run) {
                    (true, None) => run = Some(x),
                    (false, Some(start)) => {
                        segments.push(vec![(start as f32, y as f32), (x as f32, y as f32)]);
                        run = None;
                    }
                    _ => {}
                }
            }
        }
        // Vertical edges.
        for x in bounds.left..=bounds.right {
            let mut run: Option<i32> = None;
            for y in bounds.top..=bounds.bottom {
                let edge = y < bounds.bottom && inside(x, y) != inside(x - 1, y);
                match (edge, run) {
                    (true, None) => run = Some(y),
                    (false, Some(start)) => {
                        segments.push(vec![(x as f32, start as f32), (x as f32, y as f32)]);
                        run = None;
                    }
                    _ => {}
                }
            }
        }
        segments
    }

    /// Approximate fraction of the canvas selected, for status display.
    pub fn coverage_ratio(&self, canvas: IntRect) -> f64 {
        if self.is_empty() {
            return 1.0;
        }
        let mut sum = 0u64;
        for (_, buf) in self.mask.iter() {
            sum += buf.iter().map(|&v| v as u64).sum::<u64>();
        }
        let total = canvas.width() as u64 * canvas.height() as u64 * 255;
        if total == 0 {
            0.0
        } else {
            sum as f64 / total as f64
        }
    }

    pub const _TILE_PIXELS_CHECK: usize = TILE_PIXELS;
}

fn box_blur_h(src: &[f32], dst: &mut [f32], w: usize, h: usize, r: f32) {
    let ir = r as usize;
    let norm = 1.0 / (2 * ir + 1) as f32;
    for y in 0..h {
        let row = &src[y * w..(y + 1) * w];
        let mut acc: f32 = row[0] * (ir + 1) as f32;
        for &v in &row[..ir.min(w)] {
            acc += v;
        }
        for x in 0..w {
            let add = row[(x + ir).min(w - 1)];
            let sub = if x > ir { row[x - ir - 1] } else { row[0] };
            acc += add - sub;
            dst[y * w + x] = acc * norm;
        }
    }
}

fn box_blur_v(src: &[f32], dst: &mut [f32], w: usize, h: usize, r: f32) {
    let ir = r as usize;
    let norm = 1.0 / (2 * ir + 1) as f32;
    for x in 0..w {
        let mut acc: f32 = src[x] * (ir + 1) as f32;
        for y in 0..ir.min(h) {
            acc += src[y * w + x];
        }
        for y in 0..h {
            let add = src[(y + ir).min(h - 1) * w + x];
            let sub = if y > ir {
                src[(y - ir - 1) * w + x]
            } else {
                src[x]
            };
            acc += add - sub;
            dst[y * w + x] = acc * norm;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_selection_covers_everything() {
        let sel = Selection::new();
        assert!(sel.is_empty());
        assert_eq!(sel.coverage(12345, -678), 255);
    }

    #[test]
    fn rect_select_and_coverage() {
        let mut sel = Selection::new();
        sel.select_rect(IntRect::from_xywh(10, 10, 20, 20), SelectOp::Replace);
        assert_eq!(sel.coverage(15, 15), 255);
        assert_eq!(sel.coverage(5, 5), 0);
        assert_eq!(sel.coverage(29, 29), 255);
        assert_eq!(sel.coverage(30, 30), 0);
    }

    #[test]
    fn subtract_cuts_hole() {
        let mut sel = Selection::new();
        sel.select_rect(IntRect::from_xywh(0, 0, 100, 100), SelectOp::Replace);
        sel.select_rect(IntRect::from_xywh(25, 25, 50, 50), SelectOp::Subtract);
        assert_eq!(sel.coverage(50, 50), 0);
        assert_eq!(sel.coverage(10, 10), 255);
    }

    #[test]
    fn intersect_keeps_overlap_only() {
        let mut sel = Selection::new();
        sel.select_rect(IntRect::from_xywh(0, 0, 50, 50), SelectOp::Replace);
        sel.select_rect(IntRect::from_xywh(25, 25, 50, 50), SelectOp::Intersect);
        assert_eq!(sel.coverage(30, 30), 255);
        assert_eq!(sel.coverage(10, 10), 0);
        assert_eq!(sel.coverage(60, 60), 0);
    }

    #[test]
    fn ellipse_antialiases_edge() {
        let mut sel = Selection::new();
        sel.select_ellipse(IntRect::from_xywh(0, 0, 100, 100), SelectOp::Replace);
        assert_eq!(sel.coverage(50, 50), 255);
        assert_eq!(sel.coverage(2, 2), 0);
        // Cardinal edge pixel should be partially covered.
        let edge = sel.coverage(50, 0);
        assert!(edge > 0, "edge coverage = {edge}");
    }

    #[test]
    fn invert_flips_coverage() {
        let canvas = IntRect::from_xywh(0, 0, 64, 64);
        let mut sel = Selection::new();
        sel.select_rect(IntRect::from_xywh(0, 0, 32, 64), SelectOp::Replace);
        sel.invert(canvas);
        assert_eq!(sel.coverage(10, 10), 0);
        assert_eq!(sel.coverage(40, 10), 255);
    }

    #[test]
    fn feather_softens_edges() {
        let mut sel = Selection::new();
        sel.select_rect(IntRect::from_xywh(32, 32, 64, 64), SelectOp::Replace);
        sel.feather(4.0);
        let center = sel.coverage(64, 64);
        let edge = sel.coverage(32, 64);
        let outside = sel.coverage(20, 64);
        assert!(center > 240, "center = {center}");
        assert!(edge > 30 && edge < 220, "edge = {edge}");
        assert!(outside < 30, "outside = {outside}");
    }

    #[test]
    fn polygon_triangle() {
        let mut sel = Selection::new();
        sel.select_polygon(&[(0.0, 0.0), (40.0, 0.0), (0.0, 40.0)], SelectOp::Replace);
        assert!(sel.coverage(5, 5) > 200);
        assert_eq!(sel.coverage(35, 35), 0);
    }
}

#[cfg(test)]
mod outline_tests {
    use super::*;

    fn segments_of(sel: &Selection) -> Vec<((i32, i32), (i32, i32))> {
        sel.outline()
            .into_iter()
            .map(|run| {
                (
                    (run[0].0 as i32, run[0].1 as i32),
                    (run[1].0 as i32, run[1].1 as i32),
                )
            })
            .collect()
    }

    #[test]
    fn rectangle_traces_four_edges() {
        let mut sel = Selection::new();
        sel.select_rect(IntRect::from_xywh(2, 3, 5, 4), SelectOp::Replace);
        let segs = segments_of(&sel);
        // Merged runs, so a rectangle is exactly four segments.
        assert_eq!(segs.len(), 4, "{segs:?}");
        assert!(segs.contains(&((2, 3), (7, 3))), "top edge: {segs:?}");
        assert!(segs.contains(&((2, 7), (7, 7))), "bottom edge: {segs:?}");
        assert!(segs.contains(&((2, 3), (2, 7))), "left edge: {segs:?}");
        assert!(segs.contains(&((7, 3), (7, 7))), "right edge: {segs:?}");
    }

    #[test]
    fn outline_follows_a_hole_not_the_bounding_box() {
        let mut sel = Selection::new();
        sel.select_rect(IntRect::from_xywh(0, 0, 20, 20), SelectOp::Replace);
        sel.select_rect(IntRect::from_xywh(5, 5, 10, 10), SelectOp::Subtract);
        let segs = segments_of(&sel);
        // Four edges outside plus four around the hole.
        assert_eq!(segs.len(), 8, "{segs:?}");
        assert!(
            segs.contains(&((5, 5), (15, 5))),
            "hole's top edge: {segs:?}"
        );
    }

    #[test]
    fn diagonal_selection_traces_a_staircase_not_a_rectangle() {
        let mut sel = Selection::new();
        sel.select_polygon(&[(0.0, 0.0), (16.0, 0.0), (0.0, 16.0)], SelectOp::Replace);
        let segs = segments_of(&sel);
        // A triangle's hypotenuse becomes many short runs; a bounding box
        // would have produced four.
        assert!(
            segs.len() > 10,
            "expected a traced edge, got {}",
            segs.len()
        );
        // Nothing may appear in the far corner, which the bounds include.
        assert!(
            !segs
                .iter()
                .any(|(a, b)| a.0 > 14 && a.1 > 14 || b.0 > 14 && b.1 > 14),
            "outline stays off the empty corner: {segs:?}"
        );
    }

    #[test]
    fn empty_selection_has_no_outline() {
        assert!(Selection::new().outline().is_empty());
        let mut sel = Selection::new();
        sel.select_rect(IntRect::from_xywh(0, 0, 4, 4), SelectOp::Replace);
        sel.deselect();
        assert!(sel.outline().is_empty());
    }

    #[test]
    fn generation_changes_when_the_selection_does() {
        let mut sel = Selection::new();
        let start = sel.generation();
        sel.select_rect(IntRect::from_xywh(0, 0, 4, 4), SelectOp::Replace);
        let after_select = sel.generation();
        assert_ne!(start, after_select);
        sel.deselect();
        assert_ne!(after_select, sel.generation());
    }
}
