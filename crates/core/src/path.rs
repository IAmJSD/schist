//! Stored vector paths.
//!
//! Photoshop keeps paths in their own panel, separate from layers: the pen
//! draws one, the path selection tools edit it, and it can then be filled,
//! stroked or turned into a selection. Before this the pen tool built a
//! path and immediately rasterized it, so there was nothing to edit.
//!
//! Rasterizing lives in `schist-vector`; this is only the model.

use serde::{Deserialize, Serialize};

/// One point on a path, with its two Bezier handles stored as offsets from
/// the point itself so moving the anchor moves its handles with it.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Anchor {
    pub point: (f32, f32),
    /// Control handle for the segment arriving at this anchor.
    pub handle_in: (f32, f32),
    /// Control handle for the segment leaving it.
    pub handle_out: (f32, f32),
}

impl Anchor {
    /// A corner point: no curvature either side.
    pub fn corner(x: f32, y: f32) -> Anchor {
        Anchor {
            point: (x, y),
            handle_in: (0.0, 0.0),
            handle_out: (0.0, 0.0),
        }
    }

    /// A smooth point whose handles mirror each other.
    pub fn smooth(x: f32, y: f32, hx: f32, hy: f32) -> Anchor {
        Anchor {
            point: (x, y),
            handle_in: (-hx, -hy),
            handle_out: (hx, hy),
        }
    }

    pub fn translated(self, dx: f32, dy: f32) -> Anchor {
        Anchor {
            point: (self.point.0 + dx, self.point.1 + dy),
            ..self
        }
    }
}

/// A named path: one or more subpaths of anchors.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VectorPath {
    pub name: String,
    pub subpaths: Vec<SubPath>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubPath {
    pub anchors: Vec<Anchor>,
    pub closed: bool,
}

impl VectorPath {
    pub fn new(name: impl Into<String>) -> VectorPath {
        VectorPath {
            name: name.into(),
            subpaths: Vec::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.subpaths.iter().all(|s| s.anchors.is_empty())
    }

    /// Append an open subpath of anchors.
    pub fn push_open_anchors(&mut self, anchors: Vec<Anchor>) {
        self.subpaths.push(SubPath {
            anchors,
            closed: false,
        });
    }

    pub fn translate(&mut self, dx: f32, dy: f32) {
        for sub in &mut self.subpaths {
            for a in &mut sub.anchors {
                *a = a.translated(dx, dy);
            }
        }
    }

    /// Every anchor, as (subpath index, anchor index, anchor).
    pub fn anchors(&self) -> impl Iterator<Item = (usize, usize, &Anchor)> {
        self.subpaths.iter().enumerate().flat_map(|(s, sub)| {
            sub.anchors
                .iter()
                .enumerate()
                .map(move |(a, an)| (s, a, an))
        })
    }

    /// The nearest anchor to a point, within `radius`.
    pub fn hit_anchor(&self, x: f32, y: f32, radius: f32) -> Option<(usize, usize)> {
        let mut best: Option<(usize, usize, f32)> = None;
        for (s, a, an) in self.anchors() {
            let d = (an.point.0 - x).hypot(an.point.1 - y);
            if d <= radius && best.is_none_or(|(_, _, bd)| d < bd) {
                best = Some((s, a, d));
            }
        }
        best.map(|(s, a, _)| (s, a))
    }

    /// Axis-aligned bounds of the anchors, ignoring the handles.
    pub fn bounds(&self) -> crate::geom::IntRect {
        let mut out = crate::geom::IntRect::EMPTY;
        for (_, _, a) in self.anchors() {
            out = out.union(&crate::geom::IntRect::new(
                a.point.0.floor() as i32,
                a.point.1.floor() as i32,
                a.point.0.ceil() as i32 + 1,
                a.point.1.ceil() as i32 + 1,
            ));
        }
        out
    }

    /// Set every anchor's handles from its neighbours, giving a smooth
    /// curve through all of them. This is what the curvature pen does
    /// after every click.
    pub fn smooth_all(&mut self) {
        for sub in &mut self.subpaths {
            let n = sub.anchors.len();
            if n < 2 {
                continue;
            }
            let points: Vec<(f32, f32)> = sub.anchors.iter().map(|a| a.point).collect();
            for i in 0..n {
                let prev = if i == 0 {
                    if sub.closed {
                        points[n - 1]
                    } else {
                        points[0]
                    }
                } else {
                    points[i - 1]
                };
                let next = if i + 1 == n {
                    if sub.closed {
                        points[0]
                    } else {
                        points[n - 1]
                    }
                } else {
                    points[i + 1]
                };
                // Catmull-Rom tangent: a sixth of the span between the
                // neighbours, which is the standard conversion to Bezier.
                let hx = (next.0 - prev.0) / 6.0;
                let hy = (next.1 - prev.1) / 6.0;
                sub.anchors[i].handle_in = (-hx, -hy);
                sub.anchors[i].handle_out = (hx, hy);
            }
        }
    }
}

/// A live vector shape: the artwork a shape layer is generated from.
///
/// Photoshop models these as a fill layer plus a vector mask, which is
/// also how they are written to PSD. Here the layer keeps its shape and
/// re-rasterizes whenever the path, fill or stroke changes -- so a shape
/// stays editable and stays sharp at any size, which is the whole reason
/// to have one rather than a pile of pixels.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VectorShape {
    pub path: VectorPath,
    pub fill: schist_color::Rgba,
    /// Stroke colour and width, when the shape has one.
    pub stroke: Option<(schist_color::Rgba, f32)>,
    /// Even-odd rather than nonzero, for shapes with holes.
    pub even_odd: bool,
}

impl VectorShape {
    pub fn new(path: VectorPath, fill: schist_color::Rgba) -> VectorShape {
        VectorShape {
            path,
            fill,
            stroke: None,
            even_odd: false,
        }
    }

    /// A cheap fingerprint of everything the rasterization depends on, so
    /// a re-render can be skipped when nothing has moved.
    pub fn key(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = rustc_hash::FxHasher::default();
        for (_, _, a) in self.path.anchors() {
            a.point.0.to_bits().hash(&mut h);
            a.point.1.to_bits().hash(&mut h);
            a.handle_in.0.to_bits().hash(&mut h);
            a.handle_in.1.to_bits().hash(&mut h);
            a.handle_out.0.to_bits().hash(&mut h);
            a.handle_out.1.to_bits().hash(&mut h);
        }
        for sub in &self.path.subpaths {
            sub.anchors.len().hash(&mut h);
            sub.closed.hash(&mut h);
        }
        for v in [self.fill.r, self.fill.g, self.fill.b, self.fill.a] {
            v.to_bits().hash(&mut h);
        }
        match self.stroke {
            Some((c, w)) => {
                for v in [c.r, c.g, c.b, c.a, w] {
                    v.to_bits().hash(&mut h);
                }
            }
            None => 0u8.hash(&mut h),
        }
        self.even_odd.hash(&mut h);
        h.finish()
    }
}
