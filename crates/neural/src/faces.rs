//! Face detection, for Skin Smoothing and the gallery's People — and
//! face recognition, for telling the gallery's people apart.
//!
//! UltraFace is a single-shot detector: it scores a few thousand fixed
//! boxes at once and hands back all of them, so the work here is choosing
//! which ones to believe. That is a confidence threshold and
//! non-maximum suppression -- keep the best box, drop everything that
//! overlaps it, repeat -- because a detector fires several times on one
//! face and a filter wants the face, not the firings.
//!
//! Boxes come back in the model's own frame, which the image was
//! letterboxed into; they are mapped back to image pixels here so nothing
//! downstream has to know that happened.

use anyhow::{bail, Context as _, Result};

use crate::{frame, Model};

/// Length of the vector [`embed_face`] hands back.
pub const FACE_EMBED_DIM: usize = 128;

/// A detected face, in image pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Face {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    /// How sure the detector is, 0..=1.
    pub score: f32,
}

impl Face {
    fn overlap(&self, other: &Face) -> f32 {
        let x = (self.x + self.width).min(other.x + other.width) - self.x.max(other.x);
        let y = (self.y + self.height).min(other.y + other.height) - self.y.max(other.y);
        if x <= 0.0 || y <= 0.0 {
            return 0.0;
        }
        let inter = x * y;
        let union = self.width * self.height + other.width * other.height - inter;
        if union <= 0.0 {
            0.0
        } else {
            inter / union
        }
    }
}

/// Below this the detector is guessing. High enough that a hand or a
/// patterned cushion does not become a face, low enough to keep a face
/// turned away from the camera.
const CONFIDENCE: f32 = 0.7;
/// Two boxes overlapping by more than this are the same face.
const MERGE: f32 = 0.4;

/// Find the faces in an image. `rgb` is interleaved RGB in 0..=1.
pub fn faces(model: &Model, rgb: &[f32], width: usize, height: usize) -> Result<Vec<Face>> {
    if width == 0 || height == 0 || rgb.len() < width * height * 3 {
        bail!("image is {width}x{height} but has {} floats", rgb.len());
    }
    let (input, framing) = frame(model.spec, rgb, width, height);
    let out = model.run(&input)?;

    // Two outputs of the same length: the one with two numbers a box is
    // the confidence, the one with four is the box. Matching on the shape
    // rather than the order means a re-exported graph that swaps them
    // still works.
    let mut scores = None;
    let mut boxes = None;
    for value in out.iter() {
        let view = value.to_plain_array_view::<f32>()?;
        match view.shape() {
            [1, _, 2] => scores = Some(view),
            [1, _, 4] => boxes = Some(view),
            _ => {}
        }
    }
    let (Some(scores), Some(boxes)) = (scores, boxes) else {
        bail!("model does not look like a face detector");
    };
    let n = scores.shape()[1].min(boxes.shape()[1]);
    let scores = scores.as_slice().context("non-contiguous scores")?;
    let boxes = boxes.as_slice().context("non-contiguous boxes")?;

    let (fw, fh) = model.spec.input.dims();
    let (sx, sy) = framing.scale;
    let (ox, oy) = framing.offset;
    let mut found: Vec<Face> = Vec::new();
    for i in 0..n {
        // Column 1 is "face"; column 0 is "background".
        let score = scores[i * 2 + 1];
        if score < CONFIDENCE {
            continue;
        }
        // Corners, as a fraction of the frame.
        let b = &boxes[i * 4..i * 4 + 4];
        let x0 = (b[0] * fw as f32 - ox) / sx;
        let y0 = (b[1] * fh as f32 - oy) / sy;
        let x1 = (b[2] * fw as f32 - ox) / sx;
        let y1 = (b[3] * fh as f32 - oy) / sy;
        if !(x0.is_finite() && y0.is_finite() && x1.is_finite() && y1.is_finite())
            || x1 <= x0
            || y1 <= y0
        {
            continue;
        }
        found.push(Face {
            x: x0,
            y: y0,
            width: x1 - x0,
            height: y1 - y0,
            score,
        });
    }

    // Non-maximum suppression, best first.
    found.sort_by(|a, b| b.score.total_cmp(&a.score));
    let mut kept: Vec<Face> = Vec::new();
    for face in found {
        if kept.iter().all(|k| k.overlap(&face) < MERGE) {
            kept.push(face);
        }
    }
    Ok(kept)
}

/// Describe a face so it can be compared with others: `rgb` is one
/// crop, interleaved RGB in 0..=1, sized exactly as the recogniser's
/// spec says (112x112). The result is unit length, so the dot product
/// of two is their cosine — above ~0.36 they are the same person, by
/// the threshold the model's own authors use.
///
/// The model was trained on OpenCV images, which are BGR, so the
/// channels swap on the way in; the caller need not know.
pub fn embed_face(model: &Model, rgb: &[f32]) -> Result<Vec<f32>> {
    let (w, h) = model.spec.input.dims();
    if rgb.len() != w * h * 3 {
        bail!("expected a {w}x{h} crop, got {} floats", rgb.len());
    }
    let bgr: Vec<f32> = rgb
        .as_chunks::<3>()
        .0
        .iter()
        .flat_map(|px| [px[2], px[1], px[0]])
        .collect();
    let mut out = model.run_scores(&bgr)?;
    if out.len() != FACE_EMBED_DIM {
        bail!("expected {FACE_EMBED_DIM} numbers, got {}", out.len());
    }
    let norm = out.iter().map(|v| v * v).sum::<f32>().sqrt();
    if norm > 0.0 {
        for v in &mut out {
            *v /= norm;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn face(x: f32, y: f32, s: f32) -> Face {
        Face {
            x,
            y,
            width: 10.0,
            height: 10.0,
            score: s,
        }
    }

    #[test]
    fn overlap_is_intersection_over_union() {
        let a = face(0.0, 0.0, 1.0);
        assert_eq!(a.overlap(&a), 1.0);
        assert_eq!(a.overlap(&face(20.0, 0.0, 1.0)), 0.0);
        // Half of each box, so a third of their union.
        assert!((a.overlap(&face(5.0, 0.0, 1.0)) - 1.0 / 3.0).abs() < 1e-6);
    }
}
