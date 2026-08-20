//! Path construction and antialiased rasterization (M7).
//!
//! Paths are built from lines and cubic Béziers, flattened to polylines,
//! then filled by a scanline rasterizer that supersamples vertically and
//! computes exact horizontal span coverage. That gives clean antialiasing
//! in both axes without pulling in a tessellation dependency, and it feeds
//! the same 8-bit coverage masks the selection system already uses.

use photoslop_core::IntRect;

/// Winding rule used when filling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FillRule {
    /// Odd-even: overlapping subpaths punch holes.
    EvenOdd,
    /// Nonzero: overlapping subpaths in the same direction merge.
    NonZero,
}

/// A path flattened to polylines, in document coordinates.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Path {
    /// Filling always treats a subpath as closed; this is the geometry.
    pub subpaths: Vec<Vec<(f32, f32)>>,
    /// Whether each subpath was explicitly closed. Only stroking reads
    /// this -- filling always treats a subpath as closed -- so a `Path`
    /// assembled by hand fills exactly as it always did. Entries beyond
    /// the end of this vector count as open, which for stroking means
    /// caps rather than a phantom segment back to the first point.
    pub closed: Vec<bool>,
}

impl Path {
    pub fn is_empty(&self) -> bool {
        self.subpaths.iter().all(|s| s.len() < 3)
    }

    /// Whether subpath `i` is closed. Unknown subpaths count as open.
    pub fn is_closed(&self, i: usize) -> bool {
        self.closed.get(i).copied().unwrap_or(false)
    }

    /// Append an open subpath (a polyline with two distinct ends).
    pub fn push_open(&mut self, points: Vec<(f32, f32)>) {
        self.closed.resize(self.subpaths.len(), false);
        self.subpaths.push(points);
        self.closed.push(false);
    }

    /// Append a closed subpath (a ring).
    pub fn push_closed(&mut self, points: Vec<(f32, f32)>) {
        self.closed.resize(self.subpaths.len(), false);
        self.subpaths.push(points);
        self.closed.push(true);
    }

    /// Bounding box, rounded outwards.
    pub fn bounds(&self) -> IntRect {
        let mut out = IntRect::EMPTY;
        for sub in &self.subpaths {
            for &(x, y) in sub {
                let r = IntRect::new(
                    x.floor() as i32,
                    y.floor() as i32,
                    x.ceil() as i32 + 1,
                    y.ceil() as i32 + 1,
                );
                out = out.union(&r);
            }
        }
        out
    }

    pub fn translated(&self, dx: f32, dy: f32) -> Path {
        Path {
            subpaths: self
                .subpaths
                .iter()
                .map(|s| s.iter().map(|&(x, y)| (x + dx, y + dy)).collect())
                .collect(),
            closed: self.closed.clone(),
        }
    }
}

/// One drawing command, kept unflattened so paths stay editable.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Segment {
    MoveTo(f32, f32),
    LineTo(f32, f32),
    /// Cubic Bézier: two control points and an endpoint.
    CubicTo(f32, f32, f32, f32, f32, f32),
    Close,
}

/// Builds a [`Path`] from segments, flattening curves on demand.
#[derive(Debug, Clone, Default)]
pub struct PathBuilder {
    pub segments: Vec<Segment>,
}

impl PathBuilder {
    pub fn new() -> PathBuilder {
        PathBuilder::default()
    }

    pub fn move_to(&mut self, x: f32, y: f32) -> &mut Self {
        self.segments.push(Segment::MoveTo(x, y));
        self
    }

    pub fn line_to(&mut self, x: f32, y: f32) -> &mut Self {
        self.segments.push(Segment::LineTo(x, y));
        self
    }

    pub fn cubic_to(
        &mut self,
        c1x: f32,
        c1y: f32,
        c2x: f32,
        c2y: f32,
        x: f32,
        y: f32,
    ) -> &mut Self {
        self.segments
            .push(Segment::CubicTo(c1x, c1y, c2x, c2y, x, y));
        self
    }

    pub fn close(&mut self) -> &mut Self {
        self.segments.push(Segment::Close);
        self
    }

    pub fn rect(&mut self, r: IntRect) -> &mut Self {
        self.move_to(r.left as f32, r.top as f32)
            .line_to(r.right as f32, r.top as f32)
            .line_to(r.right as f32, r.bottom as f32)
            .line_to(r.left as f32, r.bottom as f32)
            .close()
    }

    /// Ellipse inscribed in `r`, drawn as four cubic arcs.
    pub fn ellipse(&mut self, r: IntRect) -> &mut Self {
        const K: f32 = 0.552_284_8; // circle-to-cubic magic constant
        let (cx, cy) = (
            (r.left + r.right) as f32 / 2.0,
            (r.top + r.bottom) as f32 / 2.0,
        );
        let (rx, ry) = (r.width() as f32 / 2.0, r.height() as f32 / 2.0);
        let (ox, oy) = (rx * K, ry * K);
        self.move_to(cx - rx, cy)
            .cubic_to(cx - rx, cy - oy, cx - ox, cy - ry, cx, cy - ry)
            .cubic_to(cx + ox, cy - ry, cx + rx, cy - oy, cx + rx, cy)
            .cubic_to(cx + rx, cy + oy, cx + ox, cy + ry, cx, cy + ry)
            .cubic_to(cx - ox, cy + ry, cx - rx, cy + oy, cx - rx, cy)
            .close()
    }

    /// Regular polygon with `sides` vertices inscribed in `r`.
    pub fn polygon(&mut self, r: IntRect, sides: u32) -> &mut Self {
        let sides = sides.max(3);
        let (cx, cy) = (
            (r.left + r.right) as f32 / 2.0,
            (r.top + r.bottom) as f32 / 2.0,
        );
        let (rx, ry) = (r.width() as f32 / 2.0, r.height() as f32 / 2.0);
        for i in 0..sides {
            let a = -std::f32::consts::FRAC_PI_2 + i as f32 * std::f32::consts::TAU / sides as f32;
            let (x, y) = (cx + rx * a.cos(), cy + ry * a.sin());
            if i == 0 {
                self.move_to(x, y);
            } else {
                self.line_to(x, y);
            }
        }
        self.close()
    }

    /// Flatten to polylines. `tolerance` is the maximum deviation in pixels.
    pub fn build(&self, tolerance: f32) -> Path {
        let tol = tolerance.max(0.01);
        let mut subpaths: Vec<Vec<(f32, f32)>> = Vec::new();
        let mut closed: Vec<bool> = Vec::new();
        let mut current: Vec<(f32, f32)> = Vec::new();
        let mut cursor = (0.0f32, 0.0f32);
        for seg in &self.segments {
            match *seg {
                Segment::MoveTo(x, y) => {
                    if current.len() >= 2 {
                        subpaths.push(std::mem::take(&mut current));
                        closed.push(false);
                    } else {
                        current.clear();
                    }
                    cursor = (x, y);
                    current.push(cursor);
                }
                Segment::LineTo(x, y) => {
                    cursor = (x, y);
                    current.push(cursor);
                }
                Segment::CubicTo(c1x, c1y, c2x, c2y, x, y) => {
                    flatten_cubic(cursor, (c1x, c1y), (c2x, c2y), (x, y), tol, &mut current);
                    cursor = (x, y);
                }
                Segment::Close => {
                    if current.len() >= 2 {
                        subpaths.push(std::mem::take(&mut current));
                        closed.push(true);
                    } else {
                        current.clear();
                    }
                }
            }
        }
        if current.len() >= 2 {
            subpaths.push(current);
            closed.push(false);
        }
        Path { subpaths, closed }
    }
}

/// Adaptive-ish cubic flattening: subdivision count from the control
/// polygon's deviation from a straight line.
fn flatten_cubic(
    p0: (f32, f32),
    p1: (f32, f32),
    p2: (f32, f32),
    p3: (f32, f32),
    tolerance: f32,
    out: &mut Vec<(f32, f32)>,
) {
    let d1 = (p1.0 - p0.0).hypot(p1.1 - p0.1);
    let d2 = (p2.0 - p1.0).hypot(p2.1 - p1.1);
    let d3 = (p3.0 - p2.0).hypot(p3.1 - p2.1);
    let len = d1 + d2 + d3;
    let steps = ((len / tolerance).sqrt().ceil() as usize).clamp(1, 256);
    for i in 1..=steps {
        let t = i as f32 / steps as f32;
        let mt = 1.0 - t;
        let (a, b, c, d) = (mt * mt * mt, 3.0 * mt * mt * t, 3.0 * mt * t * t, t * t * t);
        out.push((
            a * p0.0 + b * p1.0 + c * p2.0 + d * p3.0,
            a * p0.1 + b * p1.1 + c * p2.1 + d * p3.1,
        ));
    }
}

/// Vertical supersampling factor of the rasterizer.
const SUB: usize = 5;

/// Rasterize `path` into an 8-bit coverage mask covering `rect`
/// (`rect.width() * rect.height()` bytes, row-major).
pub fn rasterize(path: &Path, rect: IntRect, rule: FillRule) -> Vec<u8> {
    let w = rect.width().max(0) as usize;
    let h = rect.height().max(0) as usize;
    let mut cov = vec![0f32; w * h];
    if w == 0 || h == 0 || path.is_empty() {
        return vec![0; w * h];
    }

    // Edges as (x0, y0, x1, y1) with winding direction preserved.
    let mut edges: Vec<(f32, f32, f32, f32, i32)> = Vec::new();
    for sub in &path.subpaths {
        if sub.len() < 3 {
            continue;
        }
        for i in 0..sub.len() {
            let (x0, y0) = sub[i];
            let (x1, y1) = sub[(i + 1) % sub.len()];
            if (y0 - y1).abs() < f32::EPSILON {
                continue; // horizontal edges never cross a scanline
            }
            let dir = if y1 > y0 { 1 } else { -1 };
            edges.push((x0, y0, x1, y1, dir));
        }
    }
    if edges.is_empty() {
        return vec![0; w * h];
    }

    let mut crossings: Vec<(f32, i32)> = Vec::with_capacity(edges.len());
    for row in 0..h {
        let py = rect.top as f32 + row as f32;
        for s in 0..SUB {
            let sy = py + (s as f32 + 0.5) / SUB as f32;
            crossings.clear();
            for &(x0, y0, x1, y1, dir) in &edges {
                let (ymin, ymax) = if y0 < y1 { (y0, y1) } else { (y1, y0) };
                if sy < ymin || sy >= ymax {
                    continue;
                }
                let t = (sy - y0) / (y1 - y0);
                crossings.push((x0 + t * (x1 - x0), dir));
            }
            if crossings.len() < 2 {
                continue;
            }
            crossings.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

            // Walk crossings, emitting inside spans per the fill rule.
            let mut winding = 0;
            for i in 0..crossings.len() - 1 {
                winding += match rule {
                    FillRule::NonZero => crossings[i].1,
                    FillRule::EvenOdd => 1,
                };
                let inside = match rule {
                    FillRule::NonZero => winding != 0,
                    FillRule::EvenOdd => winding % 2 != 0,
                };
                if inside {
                    add_span(
                        &mut cov[row * w..(row + 1) * w],
                        crossings[i].0 - rect.left as f32,
                        crossings[i + 1].0 - rect.left as f32,
                        1.0 / SUB as f32,
                        w,
                    );
                }
            }
        }
    }
    cov.into_iter()
        .map(|c| (c.clamp(0.0, 1.0) * 255.0 + 0.5) as u8)
        .collect()
}

/// Accumulate a horizontal span with exact partial coverage at both ends.
fn add_span(row: &mut [f32], x0: f32, x1: f32, weight: f32, w: usize) {
    let (x0, x1) = (x0.max(0.0), x1.min(w as f32));
    if x1 <= x0 {
        return;
    }
    let first = x0.floor() as usize;
    let last = (x1.ceil() as usize).min(w);
    for (px, slot) in row.iter_mut().enumerate().take(last).skip(first) {
        let left = (px as f32).max(x0);
        let right = ((px + 1) as f32).min(x1);
        if right > left {
            *slot += (right - left) * weight;
        }
    }
}

/// Convert a stroke into fillable outline polygons: one quad per segment
/// plus a disc at each joint, filled with the nonzero rule so overlaps
/// merge. Good enough for shape outlines and pen strokes; it does not do
/// miter joins or dashes.
/// How a stroke terminates at the free end of an open subpath.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LineCap {
    /// Stop dead on the endpoint. Photoshop's Line tool default.
    #[default]
    Butt,
    /// Semicircle centred on the endpoint.
    Round,
    /// Half-square extending half the stroke width past the endpoint.
    Square,
}

/// How a stroke turns a corner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LineJoin {
    #[default]
    Round,
    Miter,
    Bevel,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StrokeStyle {
    pub width: f32,
    pub cap: LineCap,
    pub join: LineJoin,
    /// Miter joins longer than this multiple of the width fall back to bevel.
    pub miter_limit: f32,
}

impl Default for StrokeStyle {
    fn default() -> Self {
        StrokeStyle {
            width: 1.0,
            cap: LineCap::default(),
            join: LineJoin::default(),
            miter_limit: 4.0,
        }
    }
}

impl StrokeStyle {
    pub fn new(width: f32) -> StrokeStyle {
        StrokeStyle {
            width,
            ..Default::default()
        }
    }

    pub fn with_cap(mut self, cap: LineCap) -> StrokeStyle {
        self.cap = cap;
        self
    }

    pub fn with_join(mut self, join: LineJoin) -> StrokeStyle {
        self.join = join;
        self
    }
}

/// Expand a path's outline into a fillable path, round caps and joins.
///
/// Kept for callers that only care about width. Equivalent to
/// [`stroke_path`] with a round cap and join.
pub fn stroke_to_path(path: &Path, width: f32) -> Path {
    stroke_path(
        path,
        StrokeStyle::new(width)
            .with_cap(LineCap::Round)
            .with_join(LineJoin::Round),
    )
}

/// Expand a path's outline into a path that can be filled with
/// [`FillRule::NonZero`] to draw the stroke.
///
/// Every piece emitted here -- segment quads, join wedges, caps -- is wound
/// the same way, because under the nonzero rule opposite windings cancel
/// and punch holes where pieces overlap.
pub fn stroke_path(path: &Path, style: StrokeStyle) -> Path {
    let hw = (style.width / 2.0).max(0.05);
    let mut out = Path::default();
    for (i, sub) in path.subpaths.iter().enumerate() {
        let pts = dedup(sub);
        if pts.len() < 2 {
            // A degenerate subpath still draws a dot, but only for caps
            // that have any area of their own.
            if let (1, LineCap::Round) = (pts.len(), style.cap) {
                push_wound(&mut out, disc(pts[0].0, pts[0].1, hw));
            }
            continue;
        }
        let closed = path.is_closed(i) && pts.len() >= 3;
        let n = pts.len();
        let last = if closed { n } else { n - 1 };

        for k in 0..last {
            let (x0, y0) = pts[k];
            let (x1, y1) = pts[(k + 1) % n];
            let (dx, dy) = (x1 - x0, y1 - y0);
            let len = dx.hypot(dy);
            if len < 1e-6 {
                continue;
            }
            let (nx, ny) = (-dy / len * hw, dx / len * hw);
            push_wound(
                &mut out,
                vec![
                    (x0 + nx, y0 + ny),
                    (x1 + nx, y1 + ny),
                    (x1 - nx, y1 - ny),
                    (x0 - nx, y0 - ny),
                ],
            );
        }

        // Joins at every interior vertex, and at the seam when closed.
        let joins: Vec<usize> = if closed {
            (0..n).collect()
        } else {
            (1..n - 1).collect()
        };
        for &j in &joins {
            let prev = pts[(j + n - 1) % n];
            let here = pts[j];
            let next = pts[(j + 1) % n];
            push_join(&mut out, prev, here, next, hw, style);
        }

        if !closed {
            push_cap(&mut out, pts[1], pts[0], hw, style.cap);
            push_cap(&mut out, pts[n - 2], pts[n - 1], hw, style.cap);
        }
    }
    out
}

/// Drop consecutive duplicate points, which would otherwise produce
/// zero-length segments and undefined normals.
fn dedup(pts: &[(f32, f32)]) -> Vec<(f32, f32)> {
    let mut out: Vec<(f32, f32)> = Vec::with_capacity(pts.len());
    for &p in pts {
        if out
            .last()
            .is_none_or(|l| (l.0 - p.0).abs() > 1e-6 || (l.1 - p.1).abs() > 1e-6)
        {
            out.push(p);
        }
    }
    // A closed ring may repeat its first point at the end.
    if out.len() > 1 {
        let (f, l) = (out[0], out[out.len() - 1]);
        if (f.0 - l.0).abs() <= 1e-6 && (f.1 - l.1).abs() <= 1e-6 {
            out.pop();
        }
    }
    out
}

fn push_join(
    out: &mut Path,
    prev: (f32, f32),
    here: (f32, f32),
    next: (f32, f32),
    hw: f32,
    style: StrokeStyle,
) {
    match style.join {
        LineJoin::Round => push_wound(out, disc(here.0, here.1, hw)),
        LineJoin::Bevel | LineJoin::Miter => {
            let Some(a) = unit(here.0 - prev.0, here.1 - prev.1) else {
                return;
            };
            let Some(b) = unit(next.0 - here.0, next.1 - here.1) else {
                return;
            };
            // Outer side of the turn: the side the normals point away from.
            let cross = a.0 * b.1 - a.1 * b.0;
            if cross.abs() < 1e-6 {
                return; // collinear, nothing to fill
            }
            let sign = if cross > 0.0 { -1.0 } else { 1.0 };
            let na = (-a.1 * hw * sign, a.0 * hw * sign);
            let nb = (-b.1 * hw * sign, b.0 * hw * sign);
            let p1 = (here.0 + na.0, here.1 + na.1);
            let p2 = (here.0 + nb.0, here.1 + nb.1);
            let mut wedge = vec![here, p1, p2];
            if style.join == LineJoin::Miter {
                // Miter tip sits where the two offset edges meet.
                let cos_half = ((1.0 + (a.0 * b.0 + a.1 * b.1)) / 2.0).max(1e-6).sqrt();
                if 1.0 / cos_half <= style.miter_limit {
                    let Some(m) = unit(na.0 + nb.0, na.1 + nb.1) else {
                        return;
                    };
                    let tip = (here.0 + m.0 * hw / cos_half, here.1 + m.1 * hw / cos_half);
                    wedge = vec![here, p1, tip, p2];
                }
            }
            push_wound(out, wedge);
        }
    }
}

/// Cap the free end at `end`, given the neighbouring point `from`.
fn push_cap(out: &mut Path, from: (f32, f32), end: (f32, f32), hw: f32, cap: LineCap) {
    match cap {
        LineCap::Butt => {}
        LineCap::Round => push_wound(out, disc(end.0, end.1, hw)),
        LineCap::Square => {
            let Some(d) = unit(end.0 - from.0, end.1 - from.1) else {
                return;
            };
            let n = (-d.1 * hw, d.0 * hw);
            let e = (d.0 * hw, d.1 * hw);
            push_wound(
                out,
                vec![
                    (end.0 + n.0, end.1 + n.1),
                    (end.0 + n.0 + e.0, end.1 + n.1 + e.1),
                    (end.0 - n.0 + e.0, end.1 - n.1 + e.1),
                    (end.0 - n.0, end.1 - n.1),
                ],
            );
        }
    }
}

fn unit(x: f32, y: f32) -> Option<(f32, f32)> {
    let len = x.hypot(y);
    (len > 1e-6).then(|| (x / len, y / len))
}

/// Signed area, positive for one winding direction and negative for the other.
fn signed_area(poly: &[(f32, f32)]) -> f32 {
    let n = poly.len();
    let mut acc = 0.0;
    for i in 0..n {
        let (x0, y0) = poly[i];
        let (x1, y1) = poly[(i + 1) % n];
        acc += x0 * y1 - x1 * y0;
    }
    acc / 2.0
}

/// Push a polygon, flipping it if needed so that every piece of a stroke
/// shares one winding direction. Mixed windings cancel under the nonzero
/// rule and leave holes where pieces overlap.
fn push_wound(out: &mut Path, mut poly: Vec<(f32, f32)>) {
    if signed_area(&poly) < 0.0 {
        poly.reverse();
    }
    out.subpaths.push(poly);
}

fn disc(cx: f32, cy: f32, r: f32) -> Vec<(f32, f32)> {
    let steps = ((r * 2.0) as usize).clamp(8, 48);
    (0..steps)
        .map(|i| {
            let a = i as f32 * std::f32::consts::TAU / steps as f32;
            (cx + r * a.cos(), cy + r * a.sin())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cov_at(mask: &[u8], rect: IntRect, x: i32, y: i32) -> u8 {
        let w = rect.width() as usize;
        mask[(y - rect.top) as usize * w + (x - rect.left) as usize]
    }

    #[test]
    fn fills_a_rectangle_exactly() {
        let mut b = PathBuilder::new();
        b.rect(IntRect::from_xywh(2, 2, 6, 6));
        let path = b.build(0.25);
        let rect = IntRect::from_size(12, 12);
        let mask = rasterize(&path, rect, FillRule::NonZero);
        assert_eq!(cov_at(&mask, rect, 4, 4), 255, "interior");
        assert_eq!(cov_at(&mask, rect, 0, 0), 0, "outside");
        assert_eq!(cov_at(&mask, rect, 7, 7), 255, "last inside pixel");
        assert_eq!(cov_at(&mask, rect, 8, 8), 0, "first outside pixel");
    }

    #[test]
    fn antialiases_a_diagonal() {
        let mut b = PathBuilder::new();
        b.move_to(0.0, 0.0)
            .line_to(16.0, 16.0)
            .line_to(0.0, 16.0)
            .close();
        let path = b.build(0.25);
        let rect = IntRect::from_size(16, 16);
        let mask = rasterize(&path, rect, FillRule::NonZero);
        // Well inside the triangle.
        assert!(cov_at(&mask, rect, 2, 12) > 240);
        // Well outside.
        assert_eq!(cov_at(&mask, rect, 12, 2), 0);
        // On the diagonal: partial coverage, not 0 or 255.
        let edge = cov_at(&mask, rect, 8, 8);
        assert!(edge > 20 && edge < 235, "diagonal edge coverage = {edge}");
    }

    #[test]
    fn ellipse_is_round_and_centered() {
        let mut b = PathBuilder::new();
        b.ellipse(IntRect::from_size(32, 32));
        let path = b.build(0.2);
        let rect = IntRect::from_size(32, 32);
        let mask = rasterize(&path, rect, FillRule::NonZero);
        assert_eq!(cov_at(&mask, rect, 16, 16), 255, "centre filled");
        assert_eq!(cov_at(&mask, rect, 0, 0), 0, "corner empty");
        assert_eq!(cov_at(&mask, rect, 31, 31), 0, "opposite corner empty");
        // Cardinal points sit on the boundary.
        assert!(cov_at(&mask, rect, 16, 1) > 0);
    }

    #[test]
    fn even_odd_punches_holes_where_nonzero_does_not() {
        // Outer square with an inner square wound the same direction.
        let mut b = PathBuilder::new();
        b.rect(IntRect::from_xywh(0, 0, 20, 20));
        b.rect(IntRect::from_xywh(5, 5, 10, 10));
        let path = b.build(0.25);
        let rect = IntRect::from_size(20, 20);

        let eo = rasterize(&path, rect, FillRule::EvenOdd);
        assert_eq!(cov_at(&eo, rect, 10, 10), 0, "even-odd leaves a hole");
        assert_eq!(cov_at(&eo, rect, 2, 2), 255);

        let nz = rasterize(&path, rect, FillRule::NonZero);
        assert_eq!(cov_at(&nz, rect, 10, 10), 255, "nonzero fills through");
    }

    #[test]
    fn cubic_flattening_tracks_the_curve() {
        let mut b = PathBuilder::new();
        b.move_to(0.0, 0.0)
            .cubic_to(0.0, 10.0, 10.0, 10.0, 10.0, 0.0)
            .close();
        let path = b.build(0.1);
        let pts = &path.subpaths[0];
        assert!(pts.len() > 8, "curve flattened into {} points", pts.len());
        // The curve's apex is at t=0.5 => y = 7.5 for this control net.
        let apex = pts.iter().map(|p| p.1).fold(f32::MIN, f32::max);
        assert!((apex - 7.5).abs() < 0.3, "apex y = {apex}");
    }

    #[test]
    fn stroke_covers_the_line_and_its_width() {
        let mut b = PathBuilder::new();
        b.move_to(2.0, 8.0).line_to(14.0, 8.0);
        let line = b.build(0.25);
        let stroked = stroke_to_path(&line, 4.0);
        let rect = IntRect::from_size(16, 16);
        let mask = rasterize(&stroked, rect, FillRule::NonZero);
        assert_eq!(cov_at(&mask, rect, 8, 8), 255, "on the line");
        assert!(cov_at(&mask, rect, 8, 7) > 200, "within half-width");
        assert_eq!(cov_at(&mask, rect, 8, 13), 0, "beyond the stroke");
    }

    #[test]
    fn empty_and_degenerate_paths_are_safe() {
        let rect = IntRect::from_size(4, 4);
        assert_eq!(
            rasterize(&Path::default(), rect, FillRule::NonZero),
            vec![0; 16]
        );
        let mut b = PathBuilder::new();
        b.move_to(1.0, 1.0).line_to(2.0, 2.0).close(); // 2 points: no area
        let mask = rasterize(&b.build(0.25), rect, FillRule::NonZero);
        assert!(mask.iter().all(|&v| v == 0));
    }

    #[test]
    fn bounds_cover_all_points() {
        let mut b = PathBuilder::new();
        b.ellipse(IntRect::from_xywh(-4, 10, 20, 8));
        let path = b.build(0.2);
        let bounds = path.bounds();
        assert!(bounds.left <= -4 && bounds.right >= 16);
        assert!(bounds.top <= 10 && bounds.bottom >= 18);
    }
}
