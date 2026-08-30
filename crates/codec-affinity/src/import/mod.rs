//! Interpret a parsed Affinity graph as a Schist document.
//!
//! The document graph is `Pers` (persona) → `DocR` (document node) →
//! `Chld` (spreads) → recursive layer tree. Layers carry `Desc` (name),
//! `Visi`, `Opac`, `Blnd`, and a 2×3 `Xfrm`. Raster layers ("Rstr")
//! carry a `Bitm` ("DyBm") whose channels are planar grids of 256-byte ×
//! 256-row tiles, `TWi<n>` of them across: a status byte per tile (0/1
//! empty, 2 all-0xFF, 3 all-1.0f, 4 stored) and, for stored tiles, a
//! `Blck` naming the archive entry with the 64 KiB tile plane plus its
//! valid sub-rect.
//!
//! Everything that isn't raster (live shapes, text, adjustments) has no
//! pixels in the file at all — Affinity re-renders them — so an import
//! can only recover them as structure, not pixels. The [`ImportReport`]
//! says exactly what was and wasn't recovered; callers use it to decide
//! whether the layered result or a flattened preview serves the user
//! better.

use crate::archive::Archive;
use crate::distort::Distort;
use crate::error::{malformed, AffinityError};
use crate::liveblur::LiveBlur;
use crate::vignette::Vignette;

/// Ceiling on the pixel count of one imported canvas or bitmap.
///
/// Each dimension is capped separately at `1 << 20`, but nothing capped
/// their product: 2^40 pixels is a 4 TiB RGBA buffer, requested from two
/// numbers read straight out of the file. 2^28 pixels is a 16384x16384
/// image, comfortably past anything real, and bounds the buffer at 1 GiB.
const MAX_PIXELS: u64 = 1 << 28;

fn check_pixel_count(width: u64, height: u64, what: &str) -> Result<(), AffinityError> {
    let pixels = width.saturating_mul(height);
    if pixels > MAX_PIXELS {
        return Err(malformed(format!(
            "implausible {what} {width}x{height}: {pixels} pixels is over the {MAX_PIXELS} limit"
        )));
    }
    Ok(())
}
use crate::graph::{self, tag_name, Graph, Node, Value};
use rayon::prelude::*;
use schist_color::Depth;
use schist_core::{blit_rgba8, Document, IntRect, Layer};

mod adjust;
mod bitmap;
mod dump;
mod effects;
mod layers;
mod livefx;
mod masks;
mod paint;
mod raster;
mod resample;
mod shapes;
mod text;
mod vector;

pub use dump::dump;

// The submodules reach each other through this module, so name their
// items here rather than importing sibling-by-sibling in each one.
use bitmap::*;
use paint::*;
use resample::*;
use shapes::*;

/// What a structural import managed to recover.
#[derive(Debug, Default, Clone)]
pub struct ImportReport {
    /// Raster layers whose pixels were fully recovered.
    pub raster_layers: usize,
    /// Groups recovered (structure only, no pixels of their own).
    pub groups: usize,
    /// Layer masks recovered and attached.
    pub masks: usize,
    /// Text layers re-rendered through the text engine.
    pub text_layers: usize,
    /// Shape and path layers rebuilt as live vectors.
    pub shapes: usize,
    /// Adjustment layers mapped onto our own adjustments.
    pub adjustments: usize,
    /// Layers present in the file but not recoverable as pixels:
    /// `(name, kind tag)` — shapes, text, adjustments…
    pub skipped: Vec<(String, String)>,
}

impl ImportReport {
    /// True when every leaf layer in the file became pixels — the
    /// layered import shows the same picture Affinity would.
    pub fn complete(&self) -> bool {
        self.skipped.is_empty()
    }

    /// Fold in the report of a subtree imported on another thread.
    fn absorb(&mut self, other: ImportReport) {
        self.raster_layers += other.raster_layers;
        self.groups += other.groups;
        self.masks += other.masks;
        self.text_layers += other.text_layers;
        self.shapes += other.shapes;
        self.adjustments += other.adjustments;
        self.skipped.extend(other.skipped);
    }
}

/// Read an Affinity file into a layered document, plus a report of what
/// could and couldn't be recovered.
pub fn read_affinity(bytes: &[u8]) -> Result<(Document, ImportReport), AffinityError> {
    let archive = Archive::parse(bytes)?;
    let entry = archive
        .head("doc.dat")
        .ok_or_else(|| malformed("container has no doc.dat"))?;
    let doc_bytes = archive.extract(entry)?;
    let graph = graph::parse(&doc_bytes)?;
    build(&archive, &graph)
}

/// A live filter we know how to run over a layer's own pixels: one that
/// moves them about, or one that mixes neighbours together.
enum LiveFilter {
    Geometry(Distort),
    Blur(LiveBlur),
    Vignette(Vignette),
}

fn f64s<'g>(node: &'g Node, name: &[u8; 4]) -> Option<&'g [f64]> {
    match node.field(name)? {
        Value::VecD(v) => Some(v),
        _ => None,
    }
}

fn f32_of(node: &Node, name: &[u8; 4]) -> Option<f32> {
    match node.field(name)? {
        Value::F32(v) => Some(*v),
        _ => None,
    }
}

fn i32_of(node: &Node, name: &[u8; 4]) -> Option<i32> {
    match node.field(name)? {
        Value::I32(v) => Some(*v),
        Value::U32(v) => Some(*v as i32),
        _ => None,
    }
}

fn bool_of(node: &Node, name: &[u8; 4]) -> Option<bool> {
    match node.field(name)? {
        Value::Bool(v) => Some(*v),
        _ => None,
    }
}

fn f64_of(node: &Node, name: &[u8; 4]) -> Option<f64> {
    match node.field(name)? {
        Value::F64(v) => Some(*v),
        Value::F32(v) => Some(*v as f64),
        _ => None,
    }
}

fn enum_of(node: &Node, name: &[u8; 4]) -> Option<u16> {
    match node.field(name)? {
        Value::Enum { id, .. } => Some(*id),
        _ => None,
    }
}

fn u16_of(node: &Node, name: &[u8; 4]) -> Option<u16> {
    match node.field(name)? {
        Value::U16(v) => Some(*v),
        Value::U32(v) => u16::try_from(*v).ok(),
        Value::I32(v) => u16::try_from(*v).ok(),
        _ => None,
    }
}

/// The *last* occurrence of a float field. Shape classes repeat a tag
/// across their base-class sections (a base default, then the derived
/// value); the last one is the one that renders.
fn f32_last(node: &Node, name: &[u8; 4]) -> Option<f32> {
    let t = graph::tag(name);
    node.fields
        .iter()
        .rev()
        .find(|(ft, _)| *ft == t)
        .and_then(|(_, v)| match v {
            Value::F32(f) => Some(*f),
            Value::F64(f) => Some(*f as f32),
            _ => None,
        })
}

fn str_of<'g>(node: &'g Node, name: &[u8; 4]) -> Option<&'g str> {
    match node.field(name)? {
        Value::Str(s) => Some(s),
        _ => None,
    }
}

fn build(archive: &Archive, graph: &Graph) -> Result<(Document, ImportReport), AffinityError> {
    let root = graph.node(graph::ROOT);
    let doc_node = graph
        .child(root, b"DocR")
        .ok_or_else(|| malformed("no document root (DocR)"))?;
    let spreads = graph.children(doc_node, b"Chld");
    let spread = *spreads
        .first()
        .ok_or_else(|| malformed("document has no spreads"))?;

    // Canvas: Designer spreads carry SprB bounds [x0, y0, x1, y1];
    // Photo documents instead put the size in the document node's DfSz.
    let (org_x, org_y, width, height) = match f64s(spread, b"SprB") {
        Some([x0, y0, x1, y1]) => (
            *x0,
            *y0,
            (x1 - x0).round().max(0.0) as u32,
            (y1 - y0).round().max(0.0) as u32,
        ),
        _ => match f64s(doc_node, b"DfSz") {
            Some([w, h]) => (
                0.0,
                0.0,
                w.round().max(0.0) as u32,
                h.round().max(0.0) as u32,
            ),
            _ => return Err(malformed("no spread bounds or document size")),
        },
    };
    if width == 0 || height == 0 || width > 1 << 20 || height > 1 << 20 {
        return Err(malformed(format!("implausible canvas {width}×{height}")));
    }
    check_pixel_count(width as u64, height as u64, "canvas")?;

    let mut doc = Document::new("Affinity import", width, height, Depth::Eight);
    let mut report = ImportReport::default();
    let mut walker = Walker {
        archive,
        graph,
        report: &mut report,
        ctm: Mat::translation(-org_x, -org_y),
    };

    if spreads.len() > 1 {
        log::warn!(
            "affinity: importing first of {} spreads/artboards",
            spreads.len()
        );
    }

    // A Photo-style spread stores its base pixels in a raster-spread
    // node; import it as the bottom layer when present. Photo 2 leaves
    // an evicted composite cache here (statuses all empty) — absence,
    // not content, so it must not spoil the report.
    if let Some(ras) = graph.child(spread, b"RasS") {
        let has_content = graph.child(ras, b"Bitm").is_some_and(bitmap_has_content);
        if has_content {
            if let Some(layer) = walker.raster_layer(ras, "Background") {
                doc.push_layer(layer);
            }
        }
    }

    let children = graph.children(spread, b"Chld");
    let (layers, subreport) = walker.layer_stacks_par(&children, walker.ctm);
    walker.report.absorb(subreport);
    for layer in layers {
        doc.push_layer(layer);
    }

    doc.damage_all();
    doc.mark_saved();
    Ok((doc, report))
}

struct Walker<'a> {
    archive: &'a Archive<'a>,
    graph: &'a Graph,
    report: &'a mut ImportReport,
    /// Current transform: the composition of every ancestor group's
    /// transform plus the canvas origin. Full affine — vector layers
    /// carry rotation/shear exactly, rasters resample through it.
    ctm: Mat,
}

/// Row-major 2×3 affine transform — exactly the layout of an Affinity
/// `Xfrm`: `[m00 m01 m02 m10 m11 m12]` maps (x, y) to
/// (m00·x + m01·y + m02, m10·x + m11·y + m12).
#[derive(Debug, Clone, Copy)]
struct Mat([f64; 6]);

impl Mat {
    fn translation(tx: f64, ty: f64) -> Mat {
        Mat([1.0, 0.0, tx, 0.0, 1.0, ty])
    }

    /// `self` applied after `other` (the matrix product self · other).
    fn then(&self, o: &Mat) -> Mat {
        let (a, b) = (&self.0, &o.0);
        Mat([
            a[0] * b[0] + a[1] * b[3],
            a[0] * b[1] + a[1] * b[4],
            a[0] * b[2] + a[1] * b[5] + a[2],
            a[3] * b[0] + a[4] * b[3],
            a[3] * b[1] + a[4] * b[4],
            a[3] * b[2] + a[4] * b[5] + a[5],
        ])
    }

    fn apply(&self, x: f64, y: f64) -> (f64, f64) {
        let m = &self.0;
        (m[0] * x + m[1] * y + m[2], m[3] * x + m[4] * y + m[5])
    }

    /// The linear part only — how direction vectors (Bezier handles)
    /// transform.
    fn apply_vec(&self, x: f64, y: f64) -> (f64, f64) {
        let m = &self.0;
        (m[0] * x + m[1] * y, m[3] * x + m[4] * y)
    }

    /// Length of the image of a unit x / y vector.
    fn scale_x(&self) -> f64 {
        self.0[0].hypot(self.0[3])
    }
    fn scale_y(&self) -> f64 {
        self.0[1].hypot(self.0[4])
    }

    /// No rotation or shear: axis-aligned scale and translation only.
    fn axis_aligned(&self) -> bool {
        let m = &self.0;
        let scale = m[0].abs().max(m[1].abs()).max(m[3].abs()).max(m[4].abs());
        m[1].abs() <= scale * 1e-9 && m[3].abs() <= scale * 1e-9
    }

    fn invert(&self) -> Option<Mat> {
        let m = &self.0;
        let det = m[0] * m[4] - m[1] * m[3];
        if det.abs() < 1e-12 {
            return None;
        }
        let (a, b, c, d) = (m[4] / det, -m[1] / det, -m[3] / det, m[0] / det);
        Some(Mat([
            a,
            b,
            -(a * m[2] + b * m[5]),
            c,
            d,
            -(c * m[2] + d * m[5]),
        ]))
    }
}

/// Push every anchor — points and handles — through an affine transform.
fn transform_path(path: &mut schist_core::VectorPath, m: &Mat) {
    for sub in &mut path.subpaths {
        for a in &mut sub.anchors {
            let p = m.apply(a.point.0 as f64, a.point.1 as f64);
            let hi = m.apply_vec(a.handle_in.0 as f64, a.handle_in.1 as f64);
            let ho = m.apply_vec(a.handle_out.0 as f64, a.handle_out.1 as f64);
            a.point = (p.0 as f32, p.1 as f32);
            a.handle_in = (hi.0 as f32, hi.1 as f32);
            a.handle_out = (ho.0 as f32, ho.1 as f32);
        }
    }
}

struct RgbaImage {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

/// A gradient fill ("FilG"): colour stops plus the 2×3 transform that
/// maps gradient space (t along the unit x axis) into path space.
struct GradientFill {
    stops: Vec<(f32, [u8; 4])>,
    /// Start and end of the gradient axis, in path space.
    start: (f64, f64),
    end: (f64, f64),
    radial: bool,
}

/// Scale an image to the placement rect's size (bilinear). Identity is
/// free; import-time quality matches what a one-off resample costs.
/// Flip an image in place about either axis.
/// A projective (3x3, `h8` = 1) map, as Affinity's Live Perspective
/// filter stores it: four source corners onto four destination corners.
#[derive(Clone, Copy, Debug)]
struct Homography([f64; 9]);

/// Affinity `Blnd` (enum id, enum version) → our blend mode.
///
/// Read out of `fixtures/affinity/layer_mode.afdesign`, whose layers are
/// named after the mode each carries. The version is part of the key:
/// Affinity added Darker/Lighter Colour and Linear Light later, reusing
/// ids under version 1 (2.1 vs Multiply 2.0, 15.1 vs Exclusion 15.0).
/// Average, Negation, Reflect, Glow and Erase have no Photoshop-model
/// equivalent and map to None.
fn blend_mode(id: u16, version: u16) -> Option<schist_core::BlendMode> {
    use schist_core::BlendMode::*;
    Some(match (id, version) {
        (0, _) => Normal,
        (1, _) => Darken,
        (2, 0) => Multiply,
        (2, _) => DarkerColor,
        (3, _) => ColorBurn,
        (4, _) => Lighten,
        (5, _) => Screen,
        (6, 0) => ColorDodge,
        (6, _) => LighterColor,
        (7, _) => LinearDodge, // "Add"
        (8, _) => Overlay,
        (9, _) => SoftLight,
        (10, _) => HardLight,
        (11, _) => VividLight,
        (12, _) => PinLight,
        (13, _) => HardMix,
        (14, _) => Difference,
        (15, 0) => Exclusion,
        (15, _) => LinearLight,
        (16, _) => Subtract,
        (17, _) => Hue,
        (18, _) => Saturation,
        (19, _) => Luminosity,
        (20, _) => Color,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A layer whose transform has a negative scale is still
    /// axis-aligned, so it takes the cheap scale-and-blit path; that
    /// path has to turn the pixels over itself.
    #[test]
    fn mirroring_flips_about_each_axis() {
        // 2x2, one byte per channel, distinct per pixel: a b / c d
        let px = |v: u8| [v, v, v, 255];
        let build = || RgbaImage {
            width: 2,
            height: 2,
            pixels: [px(1), px(2), px(3), px(4)].concat(),
        };
        let corners = |i: &RgbaImage| [i.pixels[0], i.pixels[4], i.pixels[8], i.pixels[12]];

        let mut h = build();
        mirror(&mut h, true, false);
        assert_eq!(corners(&h), [2, 1, 4, 3]);

        let mut v = build();
        mirror(&mut v, false, true);
        assert_eq!(corners(&v), [3, 4, 1, 2]);

        let mut both = build();
        mirror(&mut both, true, true);
        assert_eq!(corners(&both), [4, 3, 2, 1]);

        let mut none = build();
        mirror(&mut none, false, false);
        assert_eq!(corners(&none), [1, 2, 3, 4]);
    }

    /// A grid wider than the pixels need — Affinity allocates 32 tile
    /// columns for a 5848-byte row that only fills 23 — must still walk
    /// row by declared row, skipping the slack columns.
    #[test]
    fn tiles_walk_the_declared_grid_width() {
        let places: Vec<_> = tile_offsets(4, 512).collect();
        assert_eq!(
            places,
            [
                (0, 0),
                (256, 0),
                (512, 0),
                (768, 0),
                (0, 256),
                (256, 256),
                (512, 256),
                (768, 256),
            ]
        );
        // The walk stops at the last row holding pixels, however many
        // statuses the file lists.
        assert_eq!(tile_offsets(32, 3231).count(), 32 * 13);
        // A zero width would spin forever; treat it as one column.
        assert_eq!(tile_offsets(0, 512).count(), 2);
    }

    #[test]
    fn mat_compose_matches_manual_application() {
        let rot = Mat([0.0, -1.0, 3.0, 1.0, 0.0, 5.0]); // 90° + translate
        let scale = Mat([2.0, 0.0, 1.0, 0.0, 3.0, -1.0]);
        let m = rot.then(&scale);
        let (ax, ay) = scale.apply(7.0, 11.0);
        assert_eq!(m.apply(7.0, 11.0), rot.apply(ax, ay));
        assert!(!m.axis_aligned());
        assert!(scale.axis_aligned());
    }

    #[test]
    fn the_pixel_cap_bounds_the_product_not_just_each_side() {
        // Each dimension passes its own 1<<20 check, but the product is
        // 2^40 pixels: a 4 TiB rgba buffer from two numbers in the file.
        let err = check_pixel_count(1 << 20, 1 << 20, "bitmap").unwrap_err();
        assert!(matches!(err, AffinityError::Malformed(_)), "got {err:?}");

        // A very wide but short strip is fine, so the cap is not just a
        // proxy for either side being large.
        assert!(check_pixel_count(1 << 20, 16, "bitmap").is_ok());

        // Ordinary sizes are untouched.
        assert!(check_pixel_count(6000, 4000, "canvas").is_ok());
        assert!(check_pixel_count(16384, 16384, "canvas").is_ok());

        // And the multiply cannot overflow into a small number.
        assert!(check_pixel_count(u64::MAX, u64::MAX, "bitmap").is_err());
    }

    /// The cap only protects if the import path consults it: parse a
    /// real fixture, inflate its declared sizes past the limit, and
    /// watch `build` and `decode_bitmap` refuse. If a call site is
    /// dropped, this fails even while the unit test above stays green.
    #[test]
    fn the_import_path_consults_the_pixel_cap() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/affinity-probe/invert.af"
        );
        let bytes = std::fs::read(path).unwrap();
        let archive = Archive::parse(&bytes).unwrap();
        let doc = archive.extract(archive.head("doc.dat").unwrap()).unwrap();
        let mut graph = graph::parse(&doc).unwrap();

        // Untouched, the fixture imports.
        build(&archive, &graph).expect("the fixture imports as authored");

        // Canvas: each side passes its own 1<<20 check, so only the
        // product cap can refuse it.
        let side = (1u32 << 20) as f64;
        for node in &mut graph.nodes {
            for (t, value) in &mut node.fields {
                if *t == graph::tag(b"SprB") {
                    *value = Value::VecD(vec![0.0, 0.0, side, side]);
                } else if *t == graph::tag(b"DfSz") {
                    *value = Value::VecD(vec![side, side]);
                }
            }
        }
        let err = build(&archive, &graph).unwrap_err();
        assert!(err.to_string().contains("pixels"), "got {err}");

        // Bitmap: the same inflation on the declared bitmap size.
        let bitm = graph
            .nodes
            .iter()
            .position(|n| n.field(b"BmpW").is_some() && n.field(b"BmpH").is_some())
            .expect("the fixture holds a bitmap");
        for (t, value) in &mut graph.nodes[bitm].fields {
            if *t == graph::tag(b"BmpW") || *t == graph::tag(b"BmpH") {
                *value = Value::I32(1 << 20);
            }
        }
        let err = match decode_bitmap(&archive, &graph, graph.node(bitm)) {
            Ok(_) => panic!("an implausible bitmap decoded anyway"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("pixels"), "got {err}");
    }

    #[test]
    fn mat_inverts() {
        let m = Mat([1.5, -0.5, 3.0, 0.25, 2.0, -7.0]);
        let inv = m.invert().unwrap();
        let (x, y) = inv.apply(m.apply(4.0, -9.0).0, m.apply(4.0, -9.0).1);
        assert!((x - 4.0).abs() < 1e-9 && (y + 9.0).abs() < 1e-9);
        assert!(Mat([0.0; 6]).invert().is_none());
    }

    #[test]
    fn affine_resample_rotates_a_quarter_turn() {
        // A 4×2 image, red left half, blue right, rotated 90°
        // clockwise about the origin: x' = -y, y' = x.
        let mut pixels = vec![0u8; 4 * 2 * 4];
        for y in 0..2 {
            for x in 0..4 {
                let px = &mut pixels[(y * 4 + x) * 4..][..4];
                px[0] = if x < 2 { 255 } else { 0 };
                px[2] = if x < 2 { 0 } else { 255 };
                px[3] = 255;
            }
        }
        let img = RgbaImage {
            width: 4,
            height: 2,
            pixels,
        };
        let (rect, out) = affine_resample(&img, &Mat([0.0, -1.0, 0.0, 1.0, 0.0, 0.0])).unwrap();
        assert_eq!(
            (rect.left, rect.top, rect.right, rect.bottom),
            (-2, 0, 0, 4)
        );
        assert_eq!((out.width, out.height), (2, 4));
        // The red half (x<2) lands at the top (y'<2) of the rotated image.
        let px = |x: usize, y: usize| &out.pixels[(y * 2 + x) * 4..][..4];
        assert_eq!(px(1, 0)[..3], [255, 0, 0]);
        assert_eq!(px(1, 3)[..3], [0, 0, 255]);
        assert_eq!(px(1, 0)[3], 255);
    }

    #[test]
    fn subpath_records_read_on_curve_markers() {
        // start (1,0) · c-out (0,1) · c-in (0,2) · end (1,0)
        let records = [
            (0.0, 0.0, 1u8, 0u8),
            (1.0, 0.0, 0, 1),
            (2.0, 3.0, 0, 2),
            (3.0, 3.0, 1, 0),
        ];
        let sub = subpath_from_records(&records, false).unwrap();
        assert_eq!(sub.anchors.len(), 2);
        assert_eq!(sub.anchors[0].handle_out, (1.0, 0.0));
        assert_eq!(sub.anchors[1].handle_in, (-1.0, 0.0));
        // A lone point is not a path.
        assert!(subpath_from_records(&records[..1], false).is_none());
    }
}
