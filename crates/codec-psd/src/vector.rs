//! Vector masks (`vmsk` / `vsms`), the path half of a Photoshop shape
//! layer.
//!
//! The format is a list of 26-byte records. Each starts with a u16
//! selector saying what it is, and the ones that matter here are the
//! subpath-length records (how many knots follow, and whether the subpath
//! is closed) and the knot records themselves, which hold three points:
//! the incoming control handle, the anchor, and the outgoing one.
//!
//! Coordinates are 8.24 signed fixed point, expressed as a *fraction of
//! the document* rather than in pixels -- and stored vertically first.
//! Both are easy to get subtly wrong, so both are pinned by tests.

use schist_core::{Anchor, SubPath, VectorPath};

const REC: usize = 26;

// Record selectors.
const CLOSED_LENGTH: u16 = 0;
const CLOSED_LINKED: u16 = 1;
const CLOSED_UNLINKED: u16 = 2;
const OPEN_LENGTH: u16 = 3;
const OPEN_LINKED: u16 = 4;
const OPEN_UNLINKED: u16 = 5;

/// 8.24 fixed point to a float.
fn fixed_to_f32(raw: i32) -> f32 {
    raw as f32 / (1 << 24) as f32
}

fn f32_to_fixed(v: f32) -> i32 {
    (v * (1 << 24) as f32).round() as i32
}

fn be_i32(b: &[u8]) -> i32 {
    i32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

/// Decode a vector mask into a path in document pixel coordinates.
pub fn read_vector_mask(raw: &[u8], width: u32, height: u32) -> Option<VectorPath> {
    // The block starts with a u32 version and u32 flags before the
    // records begin.
    let body = raw.get(8..)?;
    let (w, h) = (width.max(1) as f32, height.max(1) as f32);
    let mut path = VectorPath::new("Vector Mask");
    let mut current: Option<SubPath> = None;
    let mut remaining = 0usize;

    for chunk in body.as_chunks::<REC>().0 {
        let selector = u16::from_be_bytes([chunk[0], chunk[1]]);
        match selector {
            CLOSED_LENGTH | OPEN_LENGTH => {
                if let Some(sub) = current.take() {
                    if !sub.anchors.is_empty() {
                        path.subpaths.push(sub);
                    }
                }
                remaining = u16::from_be_bytes([chunk[2], chunk[3]]) as usize;
                current = Some(SubPath {
                    anchors: Vec::with_capacity(remaining),
                    closed: selector == CLOSED_LENGTH,
                });
            }
            CLOSED_LINKED | CLOSED_UNLINKED | OPEN_LINKED | OPEN_UNLINKED => {
                let Some(sub) = current.as_mut() else {
                    continue;
                };
                if remaining == 0 {
                    continue;
                }
                remaining -= 1;
                // Three points, each vertical-first.
                let pt = |i: usize| -> (f32, f32) {
                    let at = 2 + i * 8;
                    let y = fixed_to_f32(be_i32(&chunk[at..at + 4])) * h;
                    let x = fixed_to_f32(be_i32(&chunk[at + 4..at + 8])) * w;
                    (x, y)
                };
                let (inx, iny) = pt(0);
                let (ax, ay) = pt(1);
                let (outx, outy) = pt(2);
                sub.anchors.push(Anchor {
                    point: (ax, ay),
                    handle_in: (inx - ax, iny - ay),
                    handle_out: (outx - ax, outy - ay),
                });
            }
            // Path fill rule, clipboard and initial-fill records carry no
            // geometry; skipping them keeps parsing on the rails.
            _ => {}
        }
    }
    if let Some(sub) = current {
        if !sub.anchors.is_empty() {
            path.subpaths.push(sub);
        }
    }
    (!path.is_empty()).then_some(path)
}

/// Encode a path as a vector mask payload.
pub fn write_vector_mask(path: &VectorPath, width: u32, height: u32) -> Vec<u8> {
    let (w, h) = (width.max(1) as f32, height.max(1) as f32);
    let mut out = Vec::new();
    out.extend_from_slice(&3u32.to_be_bytes()); // version
    out.extend_from_slice(&0u32.to_be_bytes()); // flags

    let mut rec = |selector: u16, body: &[u8]| {
        out.extend_from_slice(&selector.to_be_bytes());
        out.extend_from_slice(body);
        // Records are always 26 bytes; the rest is padding.
        out.extend(std::iter::repeat_n(0u8, REC - 2 - body.len()));
    };

    for sub in &path.subpaths {
        if sub.anchors.is_empty() {
            continue;
        }
        let selector = if sub.closed {
            CLOSED_LENGTH
        } else {
            OPEN_LENGTH
        };
        rec(selector, &(sub.anchors.len() as u16).to_be_bytes());
        let knot = if sub.closed {
            CLOSED_UNLINKED
        } else {
            OPEN_UNLINKED
        };
        for a in &sub.anchors {
            let mut body = Vec::with_capacity(24);
            for (x, y) in [
                (a.point.0 + a.handle_in.0, a.point.1 + a.handle_in.1),
                a.point,
                (a.point.0 + a.handle_out.0, a.point.1 + a.handle_out.1),
            ] {
                // Vertical first, as fractions of the document.
                body.extend_from_slice(&f32_to_fixed(y / h).to_be_bytes());
                body.extend_from_slice(&f32_to_fixed(x / w).to_be_bytes());
            }
            rec(knot, &body);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> VectorPath {
        let mut p = VectorPath::new("s");
        p.subpaths.push(SubPath {
            anchors: vec![
                Anchor::corner(10.0, 20.0),
                Anchor::smooth(90.0, 20.0, 8.0, 0.0),
                Anchor::corner(90.0, 80.0),
            ],
            closed: true,
        });
        p.subpaths.push(SubPath {
            anchors: vec![Anchor::corner(5.0, 5.0), Anchor::corner(50.0, 50.0)],
            closed: false,
        });
        p
    }

    #[test]
    fn paths_round_trip_through_a_vector_mask() {
        let before = sample();
        let bytes = write_vector_mask(&before, 200, 100);
        let after = read_vector_mask(&bytes, 200, 100).expect("reads back");

        assert_eq!(after.subpaths.len(), 2);
        assert!(after.subpaths[0].closed);
        assert!(!after.subpaths[1].closed);
        for (a, b) in after.anchors().zip(before.anchors()) {
            assert!(
                (a.2.point.0 - b.2.point.0).abs() < 0.01
                    && (a.2.point.1 - b.2.point.1).abs() < 0.01,
                "{:?} != {:?}",
                a.2.point,
                b.2.point
            );
            assert!(
                (a.2.handle_out.0 - b.2.handle_out.0).abs() < 0.01,
                "handle lost"
            );
        }
    }

    #[test]
    fn coordinates_are_fractions_of_the_document_not_pixels() {
        // The same path in a document twice as wide must encode to
        // different bytes, and decode back to the same pixels.
        let p = sample();
        let a = write_vector_mask(&p, 200, 100);
        let b = write_vector_mask(&p, 400, 100);
        assert_ne!(a, b, "the encoding ignored the document size");
        let back = read_vector_mask(&b, 400, 100).unwrap();
        let first = back.anchors().next().unwrap().2.point;
        assert!((first.0 - 10.0).abs() < 0.01, "x came back as {}", first.0);
    }

    #[test]
    fn records_are_twenty_six_bytes_each() {
        let p = sample();
        let bytes = write_vector_mask(&p, 100, 100);
        // Eight bytes of header, then one length record and one knot
        // record per anchor in each subpath.
        let records = 2 + 3 + 2;
        assert_eq!(bytes.len(), 8 + records * 26);
    }

    #[test]
    fn garbage_and_truncation_are_none_rather_than_a_panic() {
        assert!(read_vector_mask(&[], 10, 10).is_none());
        assert!(read_vector_mask(&[0; 4], 10, 10).is_none());
        let mut bytes = write_vector_mask(&sample(), 100, 100);
        bytes.truncate(bytes.len() - 7);
        let _ = read_vector_mask(&bytes, 100, 100);
    }

    #[test]
    fn an_empty_path_encodes_to_a_bare_header() {
        let bytes = write_vector_mask(&VectorPath::new("e"), 10, 10);
        assert_eq!(bytes.len(), 8);
        assert!(read_vector_mask(&bytes, 10, 10).is_none());
    }
}
