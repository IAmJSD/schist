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
use crate::error::{malformed, AffinityError};
use crate::graph::{self, tag_name, Graph, Node, Value};
use rayon::prelude::*;
use schist_color::Depth;
use schist_core::{blit_rgba8, Document, IntRect, Layer};

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
    doc.dirty = false;
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

impl Walker<'_> {
    /// This node's transform composed onto the current transform.
    fn node_ctm(&self, node: &Node) -> Mat {
        match f64s(node, b"Xfrm").and_then(|v| v.first_chunk::<6>().copied()) {
            Some(xf) => self.ctm.then(&Mat(xf)),
            None => self.ctm,
        }
    }

    /// A layer plus the clipped children Affinity nests inside it.
    /// Any layer kind — pixels, shapes, adjustments, whole groups —
    /// can sit in a non-group layer's `Chld` list; each is confined to
    /// the parent's alpha, which is exactly a clipping run in our
    /// model: the base layer followed by `clipping` layers above it.
    fn layer_stack(&mut self, node: &Node) -> Vec<Layer> {
        let mut out = Vec::new();
        let Some(layer) = self.layer(node) else {
            return out;
        };
        out.push(layer);
        if matches!(&node.type_tag().to_be_bytes(), b"Grup" | b"Scop") {
            return out; // group children were walked as real layers
        }
        let children = self.graph.children(node, b"Chld");
        if children.is_empty() {
            return out;
        }
        // Clipped children live in the parent's coordinate space, like
        // group members.
        let saved = self.ctm;
        self.ctm = self.node_ctm(node);
        for child in children {
            let mut stack = self.layer_stack(child);
            match stack.len() {
                0 => {}
                1 => {
                    let mut clipped = stack.pop().unwrap();
                    clipped.clipping = true;
                    out.push(clipped);
                }
                // The child carries clips of its own; our flat clipping
                // run can't nest, but a clipping group can.
                _ => {
                    let mut group = Layer::new_group(stack[0].name.clone());
                    group.clipping = true;
                    group.blend = stack[0].blend;
                    if let schist_core::LayerKind::Group(g) = &mut group.kind {
                        g.children = stack;
                    }
                    out.push(group);
                }
            }
        }
        self.ctm = saved;
        out
    }

    /// Walk sibling subtrees in parallel — decoding a layer's pixels is
    /// the bulk of an import, and siblings are independent. Each worker
    /// gets its own report; the merged result keeps file order.
    fn layer_stacks_par(&self, nodes: &[&Node], ctm: Mat) -> (Vec<Layer>, ImportReport) {
        let results: Vec<(Vec<Layer>, ImportReport)> = nodes
            .par_iter()
            .map(|node| {
                let mut report = ImportReport::default();
                let mut walker = Walker {
                    archive: self.archive,
                    graph: self.graph,
                    report: &mut report,
                    ctm,
                };
                let layers = walker.layer_stack(node);
                (layers, report)
            })
            .collect();
        let mut layers = Vec::new();
        let mut report = ImportReport::default();
        for (subtree, subreport) in results {
            layers.extend(subtree);
            report.absorb(subreport);
        }
        (layers, report)
    }

    fn layer(&mut self, node: &Node) -> Option<Layer> {
        let kind = node.type_tag();
        let name = str_of(node, b"Desc").unwrap_or_default().to_string();
        // Unnamed layers get the same kind-based names Affinity's own
        // layers panel shows, not internal tags.
        let display = if name.is_empty() {
            match &kind.to_be_bytes() {
                b"Scop" => "Layer".to_string(),
                b"Grup" => "Group".to_string(),
                b"Rstr" | b"FRst" | b"MRst" => "Pixel".to_string(),
                b"ImgN" => "Image".to_string(),
                b"PCrv" => "Curve".to_string(),
                b"TxtA" | b"TxtF" => String::new(), // named from the text below
                b"CrRA" => "Curves Adjustment".to_string(),
                b"HsRA" => "HSL Adjustment".to_string(),
                // Parametric adjustments name themselves on import.
                b"LeRA" | b"ExRA" | b"BCRA" | b"BWRA" | b"CBRA" | b"VbRA" | b"InRA" | b"PoRA"
                | b"ThRA" | b"CMRA" | b"SCRA" | b"GrRA" | b"PfRA" | b"RcRA" | b"WBRA" => {
                    String::new()
                }
                b"ShpN" => String::new(), // named from the shape kind below
                _ => tag_name(kind),
            }
        } else {
            name.clone()
        };

        let skip_label = if display.is_empty() {
            tag_name(kind)
        } else {
            display.clone()
        };
        let mut layer = match &kind.to_be_bytes() {
            // "Grup" is a group in both apps; "Scop" is Designer's layer
            // container (every layers-panel "Layer" wraps its content in
            // one). Both are groups to us.
            b"Grup" | b"Scop" => {
                self.report.groups += 1;
                let mut group = Layer::new_group(display);
                // Children live in the group's coordinate space.
                let nodes = self.graph.children(node, b"Chld");
                let (children, subreport) = self.layer_stacks_par(&nodes, self.node_ctm(node));
                self.report.absorb(subreport);
                if let schist_core::LayerKind::Group(g) = &mut group.kind {
                    g.children = children;
                }
                group
            }
            // "Rstr" is a pixel layer. "ImgN" is a placed image, which
            // conveniently also carries its rendered pixels as a DyBm —
            // the original file rides along, but the bitmap is enough.
            b"Rstr" | b"ImgN" => {
                let display = if display.is_empty() || display == "Image" {
                    str_of(node, b"IRFN")
                        .filter(|s| !s.is_empty())
                        .unwrap_or("Image")
                        .to_string()
                } else {
                    display
                };
                self.raster_layer(node, &display)?
            }
            // Text stores no pixels, but it stores everything needed to
            // set it again: string, font, size, colour, and the frame
            // box that places it. Re-render through our text engine.
            b"TxtA" | b"TxtF" => match self.text_layer(node, &display) {
                Some(layer) => layer,
                None => {
                    self.report.skipped.push((skip_label, tag_name(kind)));
                    return None;
                }
            },
            // Simple geometric shapes carry their bounds, corner radii
            // and fill/stroke; rebuild them as live vector layers.
            b"ShpN" => match self.shape_layer(node, &display) {
                Some(layer) => layer,
                None => {
                    self.report.skipped.push((skip_label, tag_name(kind)));
                    return None;
                }
            },
            // Free bezier paths (pen tool, traced outlines).
            b"PCrv" => match self.path_layer(node, &display) {
                Some(layer) => layer,
                None => {
                    self.report.skipped.push((skip_label, tag_name(kind)));
                    return None;
                }
            },
            // Adjustment layers map onto our own adjustments; the
            // compositor applies them non-destructively.
            b"CrRA" => match self.curves_adjustment(node, &display) {
                Some(layer) => layer,
                None => {
                    self.report.skipped.push((skip_label, tag_name(kind)));
                    return None;
                }
            },
            b"HsRA" => match self.hsl_adjustment(node, &display) {
                Some(layer) => layer,
                None => {
                    self.report.skipped.push((skip_label, tag_name(kind)));
                    return None;
                }
            },
            // The parametric adjustments probed from fixture files
            // (fixtures/affinity-probe) — one class each behind AdjP/NAjP.
            b"LeRA" | b"ExRA" | b"BCRA" | b"BWRA" | b"CBRA" | b"VbRA" | b"InRA" | b"PoRA"
            | b"ThRA" | b"CMRA" | b"SCRA" | b"GrRA" | b"PfRA" | b"RcRA" | b"WBRA" => {
                match self.parametric_adjustment(node, &display) {
                    Some(layer) => layer,
                    None => {
                        self.report.skipped.push((skip_label, tag_name(kind)));
                        return None;
                    }
                }
            }
            // A live filter node warps the content below it between two
            // quads. When source and destination coincide the filter is
            // configured but inert — absence, not content.
            b"FlRN" if self.filter_is_identity(node) => return None,
            // Any other adjustment (split toning, soft proof, LUT,
            // OCIO…) has no equivalent on our side: import it as a
            // no-op adjustment layer that keeps the native parameters,
            // instead of dropping it — a future .af export can then
            // round-trip it, and the user keeps the layer in the stack.
            _ if node.types.iter().any(|(t, _)| t.to_be_bytes() == *b"AdjR") => {
                log::warn!(
                    "affinity: adjustment {} has no equivalent; keeping it as a no-op",
                    tag_name(kind)
                );
                let mut l = Layer::new_raster(if display.is_empty() {
                    format!("{} Adjustment", tag_name(kind))
                } else {
                    display.clone()
                });
                l.kind = schist_core::LayerKind::Adjustment(schist_core::AdjustmentData {
                    kind: schist_core::AdjustmentKind::Other(kind.to_be_bytes()),
                    raw: Vec::new(),
                    params_json: serde_json::to_string(&schist_adjustments::Params::Unsupported)
                        .ok(),
                });
                self.report.adjustments += 1;
                l
            }
            _ => {
                // No pixels exist in the file for other live layer
                // kinds — only their parameters. Record the gap.
                self.report.skipped.push((skip_label, tag_name(kind)));
                return None;
            }
        };

        // Whatever kind of adjustment this was, keep its native
        // parameter block so nothing is lost on a future .af export.
        if let schist_core::LayerKind::Adjustment(data) = &mut layer.kind {
            for key in [b"AdjP", b"NAjP"] {
                if let Some(adj) = self.graph.child(node, key) {
                    data.raw = crate::preserve::preserved_block(self.graph, kind, key, adj);
                    break;
                }
            }
        }

        self.apply_mask(node, &mut layer);
        self.apply_effects(node, &mut layer);
        if let Some(v) = bool_of(node, b"Visi") {
            layer.visible = v;
        }
        if let Some(o) = f32_of(node, b"Opac") {
            layer.opacity = o.clamp(0.0, 1.0);
        }
        // Groups pass through by default; PasT=false switches a group to
        // isolated (Normal) compositing. An explicit Blnd overrides both.
        if layer.is_group() && bool_of(node, b"PasT") == Some(false) {
            layer.blend = schist_core::BlendMode::Normal;
        }
        if let Some((id, version)) = match node.field(b"Blnd") {
            Some(Value::Enum { id, version }) => Some((*id, *version)),
            _ => None,
        } {
            match blend_mode(id, version) {
                Some(mode) => layer.blend = mode,
                None => log::warn!(
                    "affinity: blend mode {id}.{version} has no equivalent; using Normal"
                ),
            }
        }
        Some(layer)
    }

    /// Rebuild a shape layer as a live vector layer.
    ///
    /// Geometry comes from the `Shpe` class over the layer's `ShpB`
    /// local bounds, built in local space and pushed through the full
    /// layer transform — so rotated and sheared shapes import exactly.
    /// Kinds whose geometry we can't rebuild are reported, not guessed.
    fn shape_layer(&mut self, node: &Node, name: &str) -> Option<Layer> {
        let graph = self.graph;
        let shpe = graph.child(node, b"Shpe")?;
        let b = f64s(node, b"ShpB").filter(|b| b.len() == 4)?;
        let ctm = self.node_ctm(node);
        let (x0, y0) = (b[0].min(b[2]) as f32, b[1].min(b[3]) as f32);
        let (x1, y1) = (b[0].max(b[2]) as f32, b[1].max(b[3]) as f32);
        let (dw, dh) = (
            (x1 - x0) as f64 * ctm.scale_x(),
            (y1 - y0) as f64 * ctm.scale_y(),
        );
        if dw < 0.5 || dh < 0.5 || dw > 1e6 || dh > 1e6 {
            return None;
        }

        let (kind_name, subpaths) = shape_geometry(shpe, x0, y0, x1, y1)?;
        let name = if name.is_empty() { kind_name } else { name };
        let mut path = schist_core::VectorPath::new(name);
        let preserved = crate::preserve::preserved_block(graph, node.type_tag(), b"Shpe", shpe);
        // Holes (a donut's ring, a cog's bore) are extra subpaths under
        // even-odd fill; single outlines fill identically either way.
        let even_odd = subpaths.len() > 1;
        for (anchors, closed) in subpaths {
            path.subpaths.push(schist_core::SubPath { anchors, closed });
        }
        transform_path(&mut path, &ctm);
        let mut layer = self.vector_layer(node, name, path, even_odd, &ctm)?;
        layer.extras.push(schist_core::RawBlock {
            key: *b"AfSh",
            data: preserved,
        });
        Some(layer)
    }

    /// Rebuild a free path layer ("PCrv") from its curve data.
    ///
    /// `Crvs` → "PCvD" → an untagged record: a subpath count, then per
    /// subpath a closed flag and a stream of 18-byte records — f64 x,
    /// f64 y, and a marker: (1,0) control₁, (0,1) control₂, (0,2)
    /// on-curve endpoint. Each cubic segment starts at the previous
    /// endpoint; a closed path's first point is its final endpoint.
    fn path_layer(&mut self, node: &Node, name: &str) -> Option<Layer> {
        let graph = self.graph;
        let data = graph
            .child(graph.child(node, b"Crvs")?, b"Data")
            .filter(|d| !d.fields.is_empty())?;

        let ctm = self.node_ctm(node);
        let to_doc = |x: f64, y: f64| {
            let (px, py) = ctm.apply(x, y);
            (px as f32, py as f32)
        };

        let mut path = schist_core::VectorPath::new(name);
        let mut closed = true;
        for (_, value) in &data.fields {
            match value {
                Value::Bool(c) => closed = *c,
                Value::Array(items) if items.iter().any(|v| matches!(v, Value::Curve(_))) => {
                    let mut records = Vec::new();
                    for item in items {
                        let Value::Curve(raw) = item else { continue };
                        if raw.len() != 18 {
                            return None;
                        }
                        let x = f64::from_le_bytes(raw[0..8].try_into().unwrap());
                        let y = f64::from_le_bytes(raw[8..16].try_into().unwrap());
                        let (px, py) = to_doc(x, y);
                        records.push((px, py, raw[16], raw[17]));
                    }
                    if let Some(sub) = subpath_from_records(&records, closed) {
                        path.subpaths.push(sub);
                    }
                    closed = true;
                }
                _ => {}
            }
        }
        if path.is_empty() {
            return None;
        }
        // Traced outlines carry holes as counter-subpaths; even-odd
        // renders them correctly regardless of winding.
        self.vector_layer(node, name, path, true, &ctm)
    }

    /// Shared tail for vector layers: fill/stroke lookup, live shape,
    /// rasterization.
    ///
    /// Two paint conventions coexist. Photo 2 hangs *descriptors* off
    /// the layer (`BFFl` fill, `LIFl` line fill, `LILn` line style),
    /// each wrapping the actual class behind `FDeF`. Designer 1.x
    /// stores the classes directly: `BFil` (fill), `PFil` (pen/line
    /// fill) and `LSty` (line style). A line style's 12-byte `Data`
    /// record ends `… cap join style 0`; style 0 means no line is
    /// drawn (1 solid, 2 dashed, 3 textured — both render as solid).
    fn vector_layer(
        &mut self,
        node: &Node,
        name: &str,
        path: schist_core::VectorPath,
        even_odd: bool,
        ctm: &Mat,
    ) -> Option<Layer> {
        use schist_color::Rgba;
        let graph = self.graph;
        let fill_node = graph
            .children(node, b"BFFl")
            .first()
            .and_then(|f| graph.child(f, b"FDeF"))
            .or_else(|| graph.child(node, b"BFil"));
        let fill = fill_node.and_then(|f| fill_color(graph, f));
        let mut gradient = match fill {
            Some(_) => None,
            None => {
                let host = graph.children(node, b"BFFl").first().copied();
                fill_node.and_then(|f| gradient_fill(graph, f, host))
            }
        };
        // Gradient axis into document space.
        if let Some(g) = &mut gradient {
            g.start = ctm.apply(g.start.0, g.start.1);
            g.end = ctm.apply(g.end.0, g.end.1);
        }
        let stroke_color = graph
            .children(node, b"LIFl")
            .first()
            .and_then(|f| graph.child(f, b"FDeF"))
            .or_else(|| graph.child(node, b"PFil"))
            .and_then(|f| fill_color(graph, f));
        let stroke_width = graph
            .children(node, b"LILn")
            .first()
            .copied()
            .or_else(|| graph.child(node, b"LSty"))
            .and_then(|l| graph.child(l, b"LDeL"))
            .filter(|s| match s.field(b"Data") {
                Some(Value::Curve(d)) => d.get(10).is_none_or(|style| *style != 0),
                _ => true,
            })
            .and_then(|s| match s.field(b"Wght") {
                Some(Value::F64(v)) => Some((*v * (ctm.scale_x() + ctm.scale_y()) / 2.0) as f32),
                _ => None,
            })
            .unwrap_or(0.0);
        let stroke = stroke_color.filter(|_| stroke_width > 0.05);
        if fill.is_none() && gradient.is_none() && stroke.is_none() {
            return None; // invisible: nothing to import
        }

        let to_rgba = |c: [u8; 4]| Rgba::from_u8(c[0], c[1], c[2], c[3]);
        let shape = schist_core::VectorShape {
            path,
            fill: fill.map(to_rgba).unwrap_or(Rgba::TRANSPARENT),
            stroke: stroke.map(|c| (to_rgba(c), stroke_width)),
            even_odd,
        };

        let mut layer = Layer::new_raster(name);
        rasterize_shape(&mut layer, &shape, gradient.as_ref());
        if gradient.is_none() {
            // Solid shapes stay live vectors; a gradient has no home in
            // our shape model yet, so those keep their pixels only.
            layer.shape_key = shape.key();
            layer.shape = Some(Box::new(shape));
        }
        self.report.shapes += 1;
        Some(layer)
    }

    /// True when a live filter node's warp maps every source quad onto
    /// itself — it changes nothing on screen.
    fn filter_is_identity(&self, node: &Node) -> bool {
        let Some(filt) = self.graph.child(node, b"Filt") else {
            return false;
        };
        // A quad's corners are its eight F64 fields, in file order.
        let corners = |name: &[u8; 4]| -> Option<Vec<f64>> {
            let q = self.graph.child(filt, name)?;
            let out: Vec<f64> = q
                .fields
                .iter()
                .filter_map(|(_, v)| match v {
                    Value::F64(f) => Some(*f),
                    _ => None,
                })
                .collect();
            (out.len() >= 8).then_some(out)
        };
        for (src, dst) in [(b"DSrA", b"DDsA"), (b"DSrB", b"DDsB"), (b"Src ", b"Dst ")] {
            match (corners(src), corners(dst)) {
                (Some(a), Some(b)) => {
                    if a.iter().zip(&b).any(|(x, y)| (x - y).abs() > 1e-6) {
                        return false;
                    }
                }
                (None, None) => {}
                _ => return false,
            }
        }
        true
    }

    /// Rebuild a curves adjustment layer ("CrRA").
    ///
    /// `AdjP` → "CrvP" holds one spline per channel: `Mast` master and
    /// `C1Sp`/`C2Sp`/`C3Sp` for R/G/B. A spline is `Cnt` control points
    /// with `Vals` laid out as xs, then ys, then tangents (which our
    /// Catmull-Rom evaluation approximates well enough to drop).
    fn curves_adjustment(&mut self, node: &Node, name: &str) -> Option<Layer> {
        let adj = self.graph.child(node, b"AdjP")?;
        if &adj.type_tag().to_be_bytes() != b"CrvP" {
            return None;
        }
        let curve_of = |tag: &[u8; 4]| -> schist_adjustments::Curve {
            let mut curve = schist_adjustments::Curve::default();
            let Some(spline) = self.graph.child(adj, tag) else {
                return curve;
            };
            let count = match spline.field(b"Cnt ") {
                Some(Value::I32(n)) => *n as usize,
                _ => return curve,
            };
            let Some(Value::Array(vals)) = spline.field(b"Vals") else {
                return curve;
            };
            if count < 2 || vals.len() < count * 2 {
                return curve;
            }
            let v = |i: usize| match vals.get(i) {
                Some(Value::F64(f)) => *f as f32,
                _ => 0.0,
            };
            curve.points = (0..count.min(16))
                .map(|i| (v(i).clamp(0.0, 1.0), v(count + i).clamp(0.0, 1.0)))
                .collect();
            curve
        };
        let params = schist_adjustments::Params::Curves(schist_adjustments::Curves {
            rgb: curve_of(b"Mast"),
            red: curve_of(b"C1Sp"),
            green: curve_of(b"C2Sp"),
            blue: curve_of(b"C3Sp"),
        });

        let mut layer = Layer::new_raster(if name.is_empty() { "Curves" } else { name });
        layer.kind = schist_core::LayerKind::Adjustment(schist_core::AdjustmentData {
            kind: schist_core::AdjustmentKind::Curves,
            raw: Vec::new(),
            params_json: serde_json::to_string(&params).ok(),
        });
        self.report.adjustments += 1;
        Some(layer)
    }

    /// Rebuild an HSL adjustment layer ("HsRA").
    ///
    /// `AdjP` → "HSSP": master shifts `HueA` (a fraction of the full
    /// turn), `SatA` and `LumA` (fractions of full range), an `HSV`
    /// mode flag, and six per-hue-range tweak arrays (`HueC`/`SatC`/
    /// `LumC` over the `RngC` boundaries) that our adjustment doesn't
    /// model — kept master-only with a warning when they're in use.
    fn hsl_adjustment(&mut self, node: &Node, name: &str) -> Option<Layer> {
        let adj = self.graph.child(node, b"AdjP")?;
        if &adj.type_tag().to_be_bytes() != b"HSSP" {
            return None;
        }
        let f = |t: &[u8; 4]| f32_of(adj, t).unwrap_or(0.0);
        let ranged = [b"HueC", b"SatC", b"LumC"].into_iter().any(|t| {
            matches!(adj.field(t), Some(Value::Array(v)) if v.iter().any(|x| match x {
                Value::F32(f) => *f != 0.0,
                _ => false,
            }))
        });
        if ranged {
            log::warn!(
                "affinity: HSL adjustment {name:?} has per-range tweaks; keeping master only"
            );
        }
        if matches!(adj.field(b"HSV "), Some(Value::Bool(true))) {
            log::warn!("affinity: HSL adjustment {name:?} uses HSV mode; applying as HSL");
        }
        let params = schist_adjustments::Params::HueSaturation {
            hue: (f(b"HueA") * 360.0).clamp(-180.0, 180.0),
            saturation: (f(b"SatA") * 100.0).clamp(-100.0, 100.0),
            lightness: (f(b"LumA") * 100.0).clamp(-100.0, 100.0),
            colorize: false,
            lightness_desaturates: true,
            reciprocal_saturation: true,
        };

        let mut layer = Layer::new_raster(if name.is_empty() { "HSL" } else { name });
        layer.kind = schist_core::LayerKind::Adjustment(schist_core::AdjustmentData {
            kind: schist_core::AdjustmentKind::HueSaturation,
            raw: Vec::new(),
            params_json: serde_json::to_string(&params).ok(),
        });
        self.report.adjustments += 1;
        Some(layer)
    }

    /// Rebuild the parametric adjustment layers whose field layouts were
    /// probed with fixture files drawn in Affinity itself
    /// (fixtures/affinity-probe): one document per adjustment, each with
    /// distinctive slider values, read back through afdump. The class
    /// behind `AdjP` (or `NAjP`, gradient map's spelling) names the type;
    /// values are fractions of the UI's percentages unless noted.
    fn parametric_adjustment(&mut self, node: &Node, name: &str) -> Option<Layer> {
        use schist_adjustments::Params;
        let graph = self.graph;
        let tag_bytes = node.type_tag().to_be_bytes();
        let adj = match graph
            .child(node, b"AdjP")
            .or_else(|| graph.child(node, b"NAjP"))
        {
            Some(adj) => adj,
            // Invert has no parameters — and no params class.
            None if &tag_bytes == b"InRA" => node,
            None => return None,
        };
        let f = |t: &[u8; 4]| f32_of(adj, t).unwrap_or(0.0);
        let fd = |t: &[u8; 4], d: f32| f32_of(adj, t).unwrap_or(d);
        let tag = node.type_tag().to_be_bytes();
        let (kind, params, default_name) = match &tag {
            // LevP: Blac/Whit input levels, Gamm, OutB/OutW outputs, all
            // 0..1 fractions of the UI's percents; the C-arrays hold
            // per-channel variants our Levels doesn't model.
            b"LeRA" => {
                let per_channel = [b"BlkC", b"GamC", b"OBlC"].into_iter().any(|t| {
                    matches!(adj.field(t), Some(Value::Array(v)) if v.iter().any(|x| !matches!(x, Value::F32(f) if *f == 0.0 || *f == 1.0)))
                });
                if per_channel {
                    log::warn!(
                        "affinity: levels {name:?} has per-channel values; keeping master only"
                    );
                }
                let master = schist_adjustments::LevelsChannel {
                    input_black: f(b"Blac"),
                    input_white: fd(b"Whit", 1.0),
                    gamma: fd(b"Gamm", 1.0).max(0.01),
                    output_black: f(b"OutB"),
                    output_white: fd(b"OutW", 1.0),
                };
                let params = Params::Levels(schist_adjustments::Levels {
                    rgb: master,
                    ..Default::default()
                });
                (schist_core::AdjustmentKind::Levels, params, "Levels")
            }
            // ExpP: Expo is in stops applied in a power-law space whose
            // exponent is the Gamm field (2.2). Our exposure multiplies
            // the encoded value directly, so dividing the stops by that
            // gamma reproduces it exactly: (v^g * 2^E)^(1/g) = v*2^(E/g).
            b"ExRA" => (
                schist_core::AdjustmentKind::Exposure,
                Params::Exposure {
                    exposure: f(b"Expo") / fd(b"Gamm", 2.2).max(0.1),
                    offset: 0.0,
                    gamma: 1.0,
                },
                "Exposure",
            ),
            // B&CP: Brig is the percentage as a fraction; Ctrs stores
            // 1 + contrast/100. Affinity's sliders drive smooth
            // endpoint-preserving curves, not a linear remap; the
            // tables below are its actual transfer curves, read off
            // isolated probe fixtures (brightness +40%, contrast −50%
            // and +60%), and other amounts scale/blend against them.
            // The import is therefore a sampled curves adjustment.
            b"BCRA" => {
                const BRIGHT40: [f32; 17] = [
                    0.0118, 0.1015, 0.1956, 0.2887, 0.3755, 0.4576, 0.5358, 0.61, 0.6765, 0.739,
                    0.7975, 0.851, 0.8951, 0.9341, 0.9652, 0.9882, 1.0,
                ];
                const CONTRAST_N50: [f32; 17] = [
                    0.0314, 0.1252, 0.1995, 0.262, 0.3167, 0.3674, 0.4142, 0.4571, 0.5, 0.5429,
                    0.5858, 0.6326, 0.6833, 0.738, 0.8005, 0.8748, 1.0,
                ];
                const CONTRAST_P60: [f32; 17] = [
                    0.0, 0.0194, 0.0544, 0.1051, 0.1637, 0.2341, 0.3147, 0.4022, 0.498, 0.5978,
                    0.6853, 0.7659, 0.8363, 0.8949, 0.9456, 0.9806, 1.0,
                ];
                let interp = |table: &[f32; 17], v: f32| -> f32 {
                    let x = v.clamp(0.0, 1.0) * 16.0;
                    let i = (x.floor() as usize).min(15);
                    let t = x - i as f32;
                    table[i] + (table[i + 1] - table[i]) * t
                };
                if matches!(adj.field(b"Linr"), Some(Value::Bool(true))) {
                    log::warn!("affinity: brightness/contrast {name:?} is linear; applying gamma");
                }
                let bright = f(b"Brig");
                let contrast = fd(b"Ctrs", 1.0) - 1.0;
                let rgb = schist_adjustments::Curve {
                    points: (0..=16)
                        .map(|k| {
                            let v = k as f32 / 16.0;
                            let vb = v + (bright / 0.4) * (BRIGHT40[k] - v);
                            let vc = if contrast < 0.0 {
                                vb + (-contrast / 0.5) * (interp(&CONTRAST_N50, vb) - vb)
                            } else {
                                vb + (contrast / 0.6) * (interp(&CONTRAST_P60, vb) - vb)
                            };
                            (v, vc.clamp(0.0, 1.0))
                        })
                        .collect(),
                };
                let params = Params::Curves(schist_adjustments::Curves {
                    rgb,
                    ..Default::default()
                });
                (
                    schist_core::AdjustmentKind::Curves,
                    params,
                    "Brightness/Contrast",
                )
            }
            b"BWRA" => (
                schist_core::AdjustmentKind::BlackWhite,
                Params::BlackWhite {
                    reds: f(b"RedC") * 100.0,
                    yellows: f(b"Yell") * 100.0,
                    greens: f(b"Gree") * 100.0,
                    cyans: f(b"Cyan") * 100.0,
                    blues: f(b"Blue") * 100.0,
                    magentas: f(b"Mage") * 100.0,
                },
                "Black and White",
            ),
            b"CBRA" => (
                schist_core::AdjustmentKind::ColorBalance,
                Params::ColorBalance {
                    // Affinity's slider moves the channel about a tenth
                    // as far as ours per percent (fit against the probe
                    // fixture's thumbnail).
                    shadows: [f(b"ShCR") * 11.0, f(b"ShMG") * 11.0, f(b"ShYB") * 11.0],
                    midtones: [f(b"MiCR") * 11.0, f(b"MiMG") * 11.0, f(b"MiYB") * 11.0],
                    highlights: [f(b"HiCR") * 11.0, f(b"HiMG") * 11.0, f(b"HiYB") * 11.0],
                    preserve_luminosity: matches!(adj.field(b"PeLu"), Some(Value::Bool(true))),
                },
                "Colour Balance",
            ),
            // VibP: Vibr is an i32 percentage, Satu a fraction.
            b"VbRA" => (
                schist_core::AdjustmentKind::Vibrance,
                Params::Vibrance {
                    vibrance: i32_of(adj, b"Vibr").unwrap_or(0) as f32,
                    saturation: f(b"Satu") * 100.0,
                },
                "Vibrance",
            ),
            b"InRA" => (
                schist_core::AdjustmentKind::Invert,
                Params::Invert,
                "Invert",
            ),
            b"PoRA" => (
                schist_core::AdjustmentKind::Posterize,
                Params::Posterize {
                    levels: i32_of(adj, b"Post").unwrap_or(4).clamp(2, 255) as u32,
                },
                "Posterise",
            ),
            b"ThRA" => (
                schist_core::AdjustmentKind::Threshold,
                Params::Threshold {
                    level: fd(b"Thre", 0.5),
                },
                "Threshold",
            ),
            // CnMP: Weig is five rows of six — [offset, R, G, B, A, x]
            // for the R, G, B, A and composite outputs (the probe file's
            // typed weights landed at rows[0][1..5], identity rows carry
            // their 1.0 on the moving diagonal).
            b"CMRA" => {
                let Some(Value::Array(w)) = adj.field(b"Weig") else {
                    return None;
                };
                let g = |i: usize| match w.get(i) {
                    Some(Value::F32(v)) => *v * 100.0,
                    _ => 0.0,
                };
                let row = |r: usize| [g(r * 6 + 1), g(r * 6 + 2), g(r * 6 + 3)];
                // The alpha weight contributes a flat term on opaque
                // pixels, so it folds into the constant with the offset.
                let constant = |r: usize| g(r * 6) + g(r * 6 + 4);
                (
                    schist_core::AdjustmentKind::ChannelMixer,
                    Params::ChannelMixer {
                        red: row(0),
                        green: row(1),
                        blue: row(2),
                        constant: [constant(0), constant(1), constant(2)],
                        monochrome: false,
                    },
                    "Channel Mixer",
                )
            }
            // SCoP: Weig is nine ranges of [C, M, Y, K] — the six
            // Photoshop-model ranges first, then whites/neutrals/blacks,
            // which our adjustment doesn't have.
            b"SCRA" => {
                let Some(Value::Array(w)) = adj.field(b"Weig") else {
                    return None;
                };
                let g = |i: usize| match w.get(i) {
                    Some(Value::F32(v)) => *v * 100.0,
                    _ => 0.0,
                };
                if (24..36).any(|i| g(i) != 0.0) {
                    log::warn!(
                        "affinity: selective colour {name:?} tweaks whites/neutrals/blacks; \
                         importing the six colour ranges only"
                    );
                }
                let mut ranges = [[0.0f32; 4]; 6];
                for (r, out) in ranges.iter_mut().enumerate() {
                    for (c, v) in out.iter_mut().enumerate() {
                        *v = g(r * 4 + c);
                    }
                }
                (
                    schist_core::AdjustmentKind::SelectiveColor,
                    Params::SelectiveColor {
                        ranges,
                        relative: matches!(adj.field(b"Rela"), Some(Value::Bool(true))),
                    },
                    "Selective Colour",
                )
            }
            // GraP (behind NAjP): a Grad class of stops. Our gradient map
            // is a two-colour ramp, so the first and last stops speak.
            b"GrRA" => {
                let grad = graph.child(adj, b"Grad")?;
                let cols = graph.children(grad, b"Cols");
                let rgb = |n: &&Node| -> [f32; 3] {
                    let c = color_bytes(n).unwrap_or([0, 0, 0, 255]);
                    [
                        c[0] as f32 / 255.0,
                        c[1] as f32 / 255.0,
                        c[2] as f32 / 255.0,
                    ]
                };
                // Posn pairs are (position, midpoint); the whole ramp
                // goes into the multi-stop form.
                let positions: Vec<f32> = match grad.field(b"Posn") {
                    Some(Value::Array(v)) => v
                        .iter()
                        .filter_map(|p| match p {
                            Value::VecD(d) => d.first().map(|x| *x as f32),
                            _ => None,
                        })
                        .collect(),
                    _ => Vec::new(),
                };
                let mut stops: Vec<(f32, [f32; 3])> = positions
                    .iter()
                    .zip(cols.iter())
                    .map(|(p, c)| (p.clamp(0.0, 1.0), rgb(c)))
                    .collect();
                stops.sort_by(|a, b| a.0.total_cmp(&b.0));
                let (first, last) = (cols.first()?, cols.last()?);
                (
                    schist_core::AdjustmentKind::GradientMap,
                    Params::GradientMap {
                        from: rgb(first),
                        to: rgb(last),
                        reverse: false,
                        stops,
                    },
                    "Gradient Map",
                )
            }
            // LeFP: the filter colour as three u16 Lab components (the
            // first three u16 fields, in L, a, b order — their tags
            // carry unprintable bytes), plus Dens and Pres.
            b"PfRA" => {
                let labs: Vec<u16> = adj
                    .fields
                    .iter()
                    .filter_map(|(_, v)| match v {
                        Value::U16(u) => Some(*u),
                        _ => None,
                    })
                    .take(3)
                    .collect();
                let [l, a, b] = labs.as_slice() else {
                    return None;
                };
                let color = lab_to_rgb(
                    *l as f32 / 65535.0 * 100.0,
                    *a as f32 / 65535.0 * 255.0 - 128.0,
                    *b as f32 / 65535.0 * 255.0 - 128.0,
                );
                (
                    schist_core::AdjustmentKind::PhotoFilter,
                    Params::PhotoFilter {
                        color,
                        // Affinity's filter tints far more gently per
                        // percent than our multiply-toward-the-colour
                        // (fit against the probe fixture's thumbnail).
                        density: (f(b"Dens") * 100.0 * 0.2).clamp(0.0, 100.0),
                        preserve_luminosity: matches!(adj.field(b"Pres"), Some(Value::Bool(true))),
                    },
                    "Lens Filter",
                )
            }
            // WhBP: WhBa is warmth in -100..100 (an i32), WBTi tint as
            // a fraction. A real white-balance adjustment on our side —
            // a Bradford chromatic adaptation whose grey-axis gains are
            // calibrated against warmth-only and tint-only fixtures.
            b"WBRA" => (
                schist_core::AdjustmentKind::Other(*b"WhBl"),
                Params::WhiteBalance {
                    warmth: i32_of(adj, b"WhBa").unwrap_or(0) as f32,
                    tint: f(b"WBTi") * 100.0,
                },
                "White Balance",
            ),
            // RecP: hue as a fraction of the turn, saturation and
            // lightness as fractions — a colorize in our model, whose
            // lightness is an offset about the 50% midpoint.
            b"RcRA" => (
                schist_core::AdjustmentKind::HueSaturation,
                Params::HueSaturation {
                    // Colorize reads hue as an absolute 0..360 angle.
                    // Affinity's lightness L lifts towards white as
                    // l + (1 - l) * L — exactly our positive lightness
                    // offset.
                    hue: f(b"RecH") * 360.0,
                    saturation: (f(b"RecS") * 100.0).clamp(0.0, 100.0),
                    lightness: (f(b"RecL") * 100.0).clamp(0.0, 100.0),
                    colorize: true,
                    lightness_desaturates: false,
                    reciprocal_saturation: false,
                },
                "Recolour",
            ),
            _ => return None,
        };
        let mut layer = Layer::new_raster(if name.is_empty() {
            format!("{default_name} Adjustment")
        } else {
            name.to_string()
        });
        layer.kind = schist_core::LayerKind::Adjustment(schist_core::AdjustmentData {
            kind,
            raw: Vec::new(),
            params_json: serde_json::to_string(&params).ok(),
        });
        self.report.adjustments += 1;
        Some(layer)
    }

    /// Map a layer's `FiEf` effects onto our layer style.
    ///
    /// Each entry is a `FilE`-derived class sharing `Enab`, `BlnM` (the
    /// layer blend table), `Opac` (0..1), `SclO` (scale with object)
    /// and usually `Radi` (blur/width) and a `Colr`:
    /// `Shad`/`InSh` shadows add `Offs` (distance) and `Angl` — the
    /// *offset direction* in radians, y-down, so 45° points down-right —
    /// plus `Knck`; `OutG`/`InnG` glows; `ColO` colour overlay; `Strk`
    /// outline stroke (`Radi` width, `Alig` position); `BevE` bevel
    /// (`Azim`/`Elev` light direction in radians, `Dept`, `Sftn`);
    /// `Gaus` gaussian blur, which has no equivalent and is reported.
    fn apply_effects(&mut self, node: &Node, layer: &mut Layer) {
        use schist_core::style;
        let scale = ((self.node_ctm(node).scale_x() + self.node_ctm(node).scale_y()) * 0.5) as f32;
        for fx in self.graph.children(node, b"FiEf") {
            if !matches!(bool_of(fx, b"Enab"), Some(true)) {
                continue;
            }
            let tag = fx.type_tag().to_be_bytes();
            let opacity = f64_of(fx, b"Opac").unwrap_or(1.0) as f32;
            let blend = match fx.field(b"BlnM") {
                Some(Value::Enum { id, version }) => blend_mode(*id, *version),
                _ => None,
            };
            let color = self
                .graph
                .child(fx, b"Colr")
                .and_then(color_bytes)
                .map(|c| schist_color::Rgba::from_u8(c[0], c[1], c[2], c[3]));
            // "Scale with object" bakes the layer transform into the
            // effect's pixel measures; otherwise they are canvas-absolute.
            let s = if bool_of(fx, b"SclO") == Some(true) {
                scale
            } else {
                1.0
            };
            let radius = f64_of(fx, b"Radi").unwrap_or(0.0) as f32 * s;
            fn on<T>(settings: T) -> style::Effect<T> {
                style::Effect {
                    enabled: true,
                    settings,
                }
            }
            match &tag {
                b"Shad" | b"InSh" => {
                    let settings = style::ShadowStyle {
                        color: color.unwrap_or(schist_color::Rgba::new(0.0, 0.0, 0.0, 1.0)),
                        blend: blend.unwrap_or(schist_core::BlendMode::Multiply),
                        opacity,
                        // Ours is where the light comes from (the shadow
                        // falls opposite); Affinity stores the offset
                        // direction itself.
                        angle: 180.0
                            - f64_of(fx, b"Angl")
                                .unwrap_or(std::f64::consts::FRAC_PI_4)
                                .to_degrees() as f32,
                        distance: f64_of(fx, b"Offs").unwrap_or(0.0) as f32 * s,
                        spread: 0.0,
                        size: radius,
                        knockout: bool_of(fx, b"Knck").unwrap_or(true),
                    };
                    if &tag == b"Shad" {
                        layer.style.drop_shadow = on(settings);
                    } else {
                        layer.style.inner_shadow = on(settings);
                    }
                }
                b"OutG" | b"InnG" => {
                    let settings = style::GlowStyle {
                        color: color.unwrap_or(schist_color::Rgba::new(1.0, 1.0, 0.75, 1.0)),
                        blend: blend.unwrap_or(schist_core::BlendMode::Screen),
                        opacity,
                        spread: 0.0,
                        size: radius,
                        technique: style::Technique::Softer,
                        from_edge: true,
                    };
                    if &tag == b"OutG" {
                        layer.style.outer_glow = on(settings);
                    } else {
                        layer.style.inner_glow = on(settings);
                    }
                }
                b"ColO" => {
                    layer.style.color_overlay = on(style::ColorOverlayStyle {
                        color: color.unwrap_or(schist_color::Rgba::new(1.0, 0.0, 0.0, 1.0)),
                        blend: blend.unwrap_or(schist_core::BlendMode::Normal),
                        opacity,
                    });
                }
                b"Strk" => {
                    let color = color.or_else(|| {
                        // Gradient-filled strokes approximate to their
                        // first stop.
                        let fill = self
                            .graph
                            .child(fx, b"GrFl")
                            .and_then(|g| self.graph.child(g, b"FDeF"))?;
                        let grad = gradient_fill(self.graph, fill, None).or_else(|| {
                            let c = fill_color(self.graph, fill)?;
                            Some(GradientFill {
                                stops: vec![(0.0, c), (1.0, c)],
                                start: (0.0, 0.0),
                                end: (1.0, 0.0),
                                radial: false,
                            })
                        })?;
                        let c = grad.stops.first()?.1;
                        Some(schist_color::Rgba::from_u8(c[0], c[1], c[2], c[3]))
                    });
                    let Some(color) = color else {
                        continue;
                    };
                    layer.style.stroke = on(style::StrokeStyle {
                        color,
                        blend: blend.unwrap_or(schist_core::BlendMode::Normal),
                        opacity,
                        size: radius,
                        position: match enum_of(fx, b"Alig") {
                            Some(1) => style::StrokePosition::Inside,
                            Some(0) => style::StrokePosition::Center,
                            _ => style::StrokePosition::Outside,
                        },
                    });
                }
                b"BevE" => {
                    // The Beve subtype enum has only been seen disabled;
                    // the order below is a guess.
                    log::warn!(
                        "affinity: bevel on {:?} imported with an unverified subtype mapping",
                        layer.name
                    );
                    layer.style.bevel = on(style::BevelStyle {
                        style: match enum_of(fx, b"Beve") {
                            Some(0) => style::BevelStyle_::OuterBevel,
                            Some(2) => style::BevelStyle_::Emboss,
                            Some(3) => style::BevelStyle_::PillowEmboss,
                            _ => style::BevelStyle_::InnerBevel,
                        },
                        angle: f64_of(fx, b"Azim").unwrap_or(2.356).to_degrees() as f32,
                        altitude: f64_of(fx, b"Elev").unwrap_or(0.785).to_degrees() as f32,
                        size: radius,
                        soften: f64_of(fx, b"Sftn").unwrap_or(0.0) as f32 * s,
                        depth: (f64_of(fx, b"Dept").unwrap_or(1.0) as f32 / 10.0).clamp(0.0, 1.0),
                        ..style::BevelStyle::default()
                    });
                }
                _ => {
                    // Gaussian blur and anything else we can't restyle
                    // changes what the layer looks like — record the gap.
                    self.report
                        .skipped
                        .push((format!("{} (effect)", layer.name), tag_name(fx.type_tag())));
                }
            }
        }
    }

    /// Attach the layer's masks: "MRst" (mask raster) nodes in the
    /// `AdCh` list, each a full layer node with its own transform and a
    /// single-channel bitmap where white reveals. Several masks
    /// multiply, exactly as Affinity stacks them; the product becomes
    /// one real, editable [`LayerMask`].
    fn apply_mask(&mut self, node: &Node, layer: &mut Layer) {
        let masks: Vec<&Node> = self
            .graph
            .children(node, b"AdCh")
            .into_iter()
            .filter(|n| n.types.iter().any(|(t, _)| *t == graph::tag(b"MRst")))
            .collect();
        if masks.is_empty() {
            return;
        }
        // A lone mask imports whole, keeping its enabled toggle; of
        // several, only the visible ones join the product.
        let visible: Vec<&Node> = masks
            .iter()
            .copied()
            .filter(|m| bool_of(m, b"Visi").unwrap_or(true))
            .collect();
        let solo = masks.len() == 1;
        let used: Vec<&Node> = if solo || visible.is_empty() {
            vec![masks[0]]
        } else {
            visible
        };

        // A mask hangs off the layer and is stored in the layer's own
        // space, so it has to be placed through the layer's transform
        // as well as its own — exactly like a clipped child. Placing it
        // with only its own transform leaves it wherever the untransformed
        // mask happened to sit, which for a rotated or scaled layer is
        // nowhere near the pixels it is supposed to be cutting.
        let saved = self.ctm;
        self.ctm = self.node_ctm(node);
        let mut placed: Vec<(IntRect, RgbaImage)> = Vec::new();
        for mask_node in &used {
            let Some(bitm) = self.graph.child(mask_node, b"Bitm") else {
                continue;
            };
            let gray = match self.decode_bitmap(bitm) {
                Ok(g) => g,
                Err(e) => {
                    log::warn!("affinity: mask of {:?}: {e}", layer.name);
                    self.report
                        .skipped
                        .push((format!("{} (mask)", layer.name), "MRst".to_string()));
                    continue;
                }
            };
            if let Some((rect, gray)) = self.place_raster(mask_node, gray) {
                placed.push((rect, gray));
            }
        }
        self.ctm = saved;
        if placed.is_empty() {
            return;
        }

        // Union extent; outside its own rect a mask contributes the
        // revealing default (255), so only stored pixels multiply.
        let mut bounds = IntRect::EMPTY;
        for (r, _) in &placed {
            bounds = bounds.union(r);
        }
        let (w, h) = (bounds.width() as usize, bounds.height() as usize);
        if bounds.is_empty() || w * h > (1 << 28) {
            return;
        }
        let mut buf = vec![255u8; w * h];
        for (r, img) in &placed {
            let rw = r.width() as usize;
            for y in r.top..r.bottom {
                for x in r.left..r.right {
                    let v =
                        img.pixels[((y - r.top) as usize * rw + (x - r.left) as usize) * 4] as u16;
                    let px = &mut buf[(y - bounds.top) as usize * w + (x - bounds.left) as usize];
                    *px = (*px as u16 * v / 255) as u8;
                }
            }
        }

        let mut mask = schist_core::LayerMask::new_revealing();
        mask.bounds = bounds;
        mask.enabled = if solo {
            bool_of(used[0], b"Visi").unwrap_or(true)
        } else {
            true
        };
        blit_mask(&mut mask.tiles, bounds, &buf);
        layer.mask = Some(mask);
        self.report.masks += used.len();
    }

    /// Rebuild a text layer by re-setting its type.
    ///
    /// The graph stores the string (`StSt` story → `Blok` → `Glyp`
    /// `Utf8`, with U+2029 paragraph breaks), the paragraph alignment
    /// (block `PAtt` → `Ints[0]`: 0 left · 1 centre · 2 right), the
    /// first run's font size (`Doub[0]`) and resolved font
    /// (`RFnt`/`DFnt` PostScript + family names, `Wegt` weight, `Ital`),
    /// the fill colour (run `Objs` → `FDsc.FDeF` → `Colr`), and the
    /// frame box `FrmB` whose transformed bottom edge is the first
    /// baseline. The rendered layer carries the same `PsTx` extras
    /// block the type tool writes, so imported text stays editable.
    fn text_layer(&mut self, node: &Node, name: &str) -> Option<Layer> {
        let graph = self.graph;
        let frame_text = &node.type_tag().to_be_bytes() == b"TxtF";
        let story = graph.child(node, b"StSt")?;
        let blocks = graph.children(story, b"Blok");
        let text = blocks
            .iter()
            .filter_map(|b| graph.child(b, b"Glyp"))
            .filter_map(|g| str_of(g, b"Utf8"))
            .map(|s| s.trim_end_matches('\0'))
            .collect::<Vec<_>>()
            .join("\n")
            // Affinity breaks lines with the Unicode paragraph and line
            // separators (and a vertical tab for soft returns).
            .replace(['\u{2028}', '\u{2029}', '\u{000B}'], "\n")
            .replace("\r\n", "\n")
            .replace('\r', "\n");
        if text.trim().is_empty() {
            return None;
        }

        // First block's paragraph attributes give the alignment. Like
        // the character attributes, they sit behind a run list:
        // `PAtt` → `Runs` → `Item` → `Ints[0]`.
        let align = blocks
            .iter()
            .find_map(|b| {
                let runs = graph.children(graph.child(b, b"PAtt")?, b"Runs");
                let item = graph.child(*runs.first()?, b"Item")?;
                match item.field(b"Ints") {
                    Some(Value::Array(v)) => match v.first() {
                        Some(Value::I32(a)) => Some(*a),
                        _ => None,
                    },
                    _ => None,
                }
            })
            .unwrap_or(0);
        let align = match align {
            1 => schist_text_engine::Align::Center,
            2 => schist_text_engine::Align::Right,
            _ => schist_text_engine::Align::Left,
        };

        // First run's character attributes speak for the whole layer.
        let run_item = blocks.iter().find_map(|b| {
            let runs = graph.children(graph.child(b, b"GAtt")?, b"Runs");
            graph.child(*runs.first()?, b"Item")
        })?;
        let size = match run_item.field(b"Doub") {
            Some(Value::Array(d)) => match d.first() {
                Some(Value::F64(s)) => *s,
                _ => return None,
            },
            _ => return None,
        };
        // Two descriptors name the font: `DFnt` is what the document was
        // designed with, `RFnt` what the *writing* machine resolved it
        // to — stale advice on any other machine (real corpus files
        // carry RFnt = Helvetica for a document set in Geist). Affinity
        // re-resolves on open, preferring the document font whenever it
        // — or its family stripped of a trailing parenthetical, since
        // "Geist (Beta)" installs as "Geist" — is present; only a
        // document font this machine lacks falls back to RFnt's advice.
        let installed_name = |f: &&Node| -> Option<String> {
            let fam = str_of(f, b"Famy").filter(|s| !s.is_empty())?;
            if schist_text_engine::has_family(fam) {
                return Some(fam.to_string());
            }
            let head = fam.rsplit_once(" (").map(|(head, _)| head.trim())?;
            (!head.is_empty() && schist_text_engine::has_family(head)).then(|| head.to_string())
        };
        let dfnt = graph.child(run_item, b"DFnt");
        let rfnt = graph.child(run_item, b"RFnt");
        let (font, family, family_installed) = match [dfnt, rfnt]
            .into_iter()
            .flatten()
            .find_map(|f| installed_name(&f).map(|fam| (f, fam)))
        {
            Some((f, fam)) => (Some(f), fam, true),
            // Neither is installed: keep the writing app's resolution if
            // it names anything, and let the engine substitute from there.
            None => {
                let f = [rfnt, dfnt]
                    .into_iter()
                    .flatten()
                    .find(|f| str_of(f, b"Famy").is_some_and(|s| !s.is_empty()));
                let fam = f
                    .and_then(|f| str_of(f, b"Famy"))
                    .unwrap_or_default()
                    .to_string();
                (f, fam, false)
            }
        };
        let post = font
            .and_then(|f| str_of(f, b"Post"))
            .unwrap_or_default()
            .to_string();
        let weight = font.and_then(|f| i32_of(f, b"Wegt")).unwrap_or(400);
        let italic = font.and_then(|f| bool_of(f, b"Ital")).unwrap_or(false);
        let color = run_color(graph, run_item).unwrap_or([0, 0, 0, 255]);

        // Placement: the frame box, through the layer transform. Its
        // bottom edge is the first baseline (the box spans the visual
        // cap height), which is exactly what the rasterizer's layout
        // origin means.
        let frame = graph.child(node, b"TxtH")?;
        let frmb = f64s(frame, b"FrmB").filter(|b| b.len() == 4)?;
        let ctm = self.node_ctm(node);
        // Rotated or sheared text lays out in a de-rotated frame space
        // — the local frame box at document scale, origin at its top
        // left — and the finished raster is pushed through the rotation
        // at the end. Axis-aligned text keeps the direct path.
        let rotated = !ctm.axis_aligned();
        let doc_scale = ctm.scale_y().abs().max(1e-6);
        let (frame_left, frame_top, frame_width, frame_height) = if rotated {
            (
                0,
                0,
                ((frmb[2] - frmb[0]).abs() * doc_scale).round() as i32,
                ((frmb[3] - frmb[1]).abs() * doc_scale).round() as i32,
            )
        } else {
            // The frame box's bounding box through the transform; in
            // the axis-aligned case this is the box itself.
            let corners = [
                ctm.apply(frmb[0], frmb[1]),
                ctm.apply(frmb[2], frmb[1]),
                ctm.apply(frmb[0], frmb[3]),
                ctm.apply(frmb[2], frmb[3]),
            ];
            let min =
                |f: fn(&(f64, f64)) -> f64| corners.iter().map(f).fold(f64::INFINITY, f64::min);
            let max =
                |f: fn(&(f64, f64)) -> f64| corners.iter().map(f).fold(f64::NEG_INFINITY, f64::max);
            (
                min(|c| c.0).round() as i32,
                min(|c| c.1).round() as i32,
                (max(|c| c.0) - min(|c| c.0)).round() as i32,
                (max(|c| c.1) - min(|c| c.1)).round() as i32,
            )
        };
        let eff_size = (size * ctm.scale_y()) as f32;
        if !(0.5..=10_000.0).contains(&eff_size) {
            return None;
        }

        let mut spec = schist_text_engine::TextSpec {
            text,
            family,
            bold: weight >= 600
                || post.contains("Bold")
                || post.contains("Black")
                || post.contains("Heavy"),
            italic: italic || post.contains("Italic") || post.contains("Oblique"),
            size: eff_size,
            align,
            line_height: 1.0,
            tracking: 0.0,
            // Frame text reflows to its box; artistic text never wraps.
            wrap_width: (frame_text && frame_width > 8).then_some(frame_width as f32),
        };
        let mut raster = match schist_text_engine::rasterize(&spec) {
            Some(r) => r,
            None => {
                let fallback = schist_text_engine::default_family();
                log::warn!(
                    "affinity: font {:?} not installed; setting {name:?} in {fallback:?}",
                    spec.family
                );
                spec.family = fallback;
                schist_text_engine::rasterize(&spec)?
            }
        };
        if raster.is_empty() {
            return None;
        }
        // An artistic-text frame box records exactly how wide the
        // *writing* machine set this text — as a pen (advance) box, side
        // bearings and all, not an ink box (corpus files' own exports
        // adjudicated this: their ink starts one left side bearing
        // inside the frame). With the real family installed the natural
        // layout is already right and the frame may reflect someone
        // else's substitute, so leave it alone; when we substitute,
        // scale the size so our advance width still fills the recorded
        // box. Frame text keeps its size and reflows instead.
        if !frame_text && !family_installed && frame_width > 8 && raster.layout_width > 1.0 {
            let ratio = frame_width as f32 / raster.layout_width;
            if (ratio - 1.0).abs() > 0.002 {
                spec.size *= ratio;
                raster = schist_text_engine::rasterize(&spec)?;
                if raster.is_empty() {
                    return None;
                }
            }
        }

        // The frame box of multi-line artistic text runs from the first
        // line's cap down to the last line's baseline, so what it
        // records beyond that cap is exactly the leading Affinity used.
        // Our own leading is whatever the face's line metrics say, which
        // is a different number in every font and in no way the one this
        // document was set with — so solve for it.
        let lines = spec.text.lines().count();
        if !frame_text && lines > 1 && raster.line_advance > 0.0 {
            // The face's declared capital height, not the first line's
            // ink top: ascenders overshoot the cap, and the frame box is
            // measured from the cap.
            let cap = raster
                .cap_height
                .unwrap_or(raster.first_baseline - raster.bounds.top as f32);
            let wanted = (frame_height as f32 - cap) / (lines - 1) as f32;
            let scale = wanted / raster.line_advance;
            // A box that implies a collapsed or wildly stretched leading
            // is one we have misread; keep the face's own spacing.
            if (0.25..=4.0).contains(&scale) && (scale - 1.0).abs() > 0.01 {
                spec.line_height *= scale;
                raster = schist_text_engine::rasterize(&spec)?;
                if raster.is_empty() {
                    return None;
                }
            }
        }

        // Anchor our pen box to the frame box: the layout already put
        // the widest line's pen at 0 and aligned the rest, so the frame
        // left is the origin for every alignment, and the ink lands one
        // side bearing inside it exactly as Affinity draws it. (When the
        // engine measured no advances, fall back to ink anchoring.)
        let origin_x = if raster.layout_width > 1.0 {
            match spec.align {
                schist_text_engine::Align::Center if frame_width > 8 => {
                    frame_left + (frame_width - raster.layout_width.round() as i32) / 2
                }
                schist_text_engine::Align::Right if frame_width > 8 => {
                    frame_left + frame_width - raster.layout_width.round() as i32
                }
                _ => frame_left,
            }
        } else {
            frame_left - raster.bounds.left
        };
        // Vertically, artistic text anchors by baseline: the frame's
        // bottom edge is the last line's baseline (the first line's, for
        // one line), which no ascender-vs-cap disagreement can move.
        // Frame text keeps ink-top anchoring to its frame.
        let origin_y = if !frame_text && raster.first_baseline > 0.0 {
            let last_baseline =
                raster.first_baseline + (lines.max(1) - 1) as f32 * raster.line_advance;
            frame_top + frame_height - last_baseline.round() as i32
        } else {
            frame_top - raster.bounds.top
        };
        let origin = (origin_x, origin_y);
        // Affinity's panel names text layers after their content.
        let display_name = if name.is_empty() {
            let first_line = spec.text.lines().next().unwrap_or("Text").trim();
            let mut label: String = first_line.chars().take(32).collect();
            if label.len() < first_line.len() {
                label.push('…');
            }
            if label.is_empty() {
                label = "Text".to_string();
            }
            label
        } else {
            name.to_string()
        };
        let mut layer = Layer::new_raster(display_name);
        let bounds = raster.bounds.translated(origin.0, origin.1);
        let mut rgba = vec![0u8; raster.coverage.len() * 4];
        for (px, &cov) in rgba.as_chunks_mut::<4>().0.iter_mut().zip(&raster.coverage) {
            px[0] = color[0];
            px[1] = color[1];
            px[2] = color[2];
            px[3] = (cov as u16 * color[3] as u16 / 255) as u8;
        }
        if rotated {
            // Map layout space back through the rotation: a layout
            // pixel p sits at ctm · (frame_local_origin + p/scale).
            let c0 = ctm.apply(frmb[0].min(frmb[2]), frmb[1].min(frmb[3]));
            let lin = Mat([
                ctm.0[0] / doc_scale,
                ctm.0[1] / doc_scale,
                0.0,
                ctm.0[3] / doc_scale,
                ctm.0[4] / doc_scale,
                0.0,
            ]);
            let o = lin.apply(bounds.left as f64, bounds.top as f64);
            let map = Mat([
                lin.0[0],
                lin.0[1],
                o.0 + c0.0,
                lin.0[3],
                lin.0[4],
                o.1 + c0.1,
            ]);
            let img = RgbaImage {
                width: raster.bounds.width() as u32,
                height: raster.bounds.height() as u32,
                pixels: rgba,
            };
            let (rect, out) = affine_resample(&img, &map)?;
            blit_rgba8(
                &mut layer.as_raster_mut().unwrap().tiles,
                Depth::Eight,
                rect,
                &out.pixels,
            );
        } else {
            blit_rgba8(
                &mut layer.as_raster_mut().unwrap().tiles,
                Depth::Eight,
                bounds,
                &rgba,
            );
        }

        // The type tool's persistence block, so double-clicking with T
        // reopens this text for editing.
        #[derive(serde::Serialize)]
        struct StoredText<'a> {
            spec: &'a schist_text_engine::TextSpec,
            origin: (i32, i32),
            color: [u8; 4],
        }
        if let Ok(data) = serde_json::to_vec(&StoredText {
            spec: &spec,
            origin,
            color,
        }) {
            layer.extras.push(schist_core::RawBlock {
                key: *b"PsTx",
                data,
            });
        }
        self.report.text_layers += 1;
        Some(layer)
    }

    /// Build a raster layer from a node holding a `Bitm` bitmap.
    fn raster_layer(&mut self, node: &Node, name: &str) -> Option<Layer> {
        let bitm = self.graph.child(node, b"Bitm")?;
        if &bitm.type_tag().to_be_bytes() != b"DyBm" {
            self.report
                .skipped
                .push((name.to_string(), tag_name(bitm.type_tag())));
            return None;
        }
        let rgba = match self.decode_bitmap(bitm) {
            Ok(d) => d,
            Err(e) => {
                log::warn!("affinity: bitmap of {name:?}: {e}");
                self.report
                    .skipped
                    .push((name.to_string(), format!("Rstr: {e}")));
                return None;
            }
        };

        let (rect, rgba) = self.place_raster(node, rgba)?;
        let mut layer = Layer::new_raster(name);
        blit_rgba8(
            &mut layer.as_raster_mut().unwrap().tiles,
            Depth::Eight,
            rect,
            &rgba.pixels,
        );
        self.report.raster_layers += 1;
        Some(layer)
    }

    /// The map from a bitmap's pixel space onto the canvas: the layer
    /// transform chain, exactly as for vectors. `BitR` (the content's
    /// bounding rect, usually in bitmap space) plays no part in
    /// placement — treating it as a destination squashes any layer
    /// whose transform isn't the identity, which real Photo documents
    /// (scaled brush strokes, rotated placed images with pre-rotated
    /// pixel caches) proved wrong against their own thumbnails.
    fn raster_map(&self, node: &Node) -> Mat {
        self.node_ctm(node)
    }

    /// Place a decoded bitmap on the canvas. Axis-aligned placements
    /// scale bilinearly (identity is free); rotated or sheared ones go
    /// through a full affine resample.
    fn place_raster(&self, node: &Node, img: RgbaImage) -> Option<(IntRect, RgbaImage)> {
        let map = self.raster_map(node);
        if !map.axis_aligned() {
            return affine_resample(&img, &map);
        }
        let (ax, ay) = map.apply(0.0, 0.0);
        let (bx, by) = map.apply(img.width as f64, img.height as f64);
        let sane = |v: f64| v.is_finite() && v.abs() < (1 << 24) as f64;
        let rect = if sane(ax) && sane(ay) && sane(bx) && sane(by) {
            IntRect::new(
                ax.min(bx).round() as i32,
                ay.min(by).round() as i32,
                ax.max(bx).round() as i32,
                ay.max(by).round() as i32,
            )
        } else {
            IntRect::EMPTY
        };
        let rect = if rect.is_empty() {
            IntRect::from_size(img.width, img.height)
        } else {
            rect
        };
        let mut img = resample_to(img, rect.width() as u32, rect.height() as u32);
        // A mirror is axis-aligned too — zero shear, negative scale — so
        // it lands here rather than in the resampler. The rect above is
        // normalised, which puts the box in the right place but leaves
        // the pixels facing the wrong way; turn them over.
        mirror(&mut img, map.0[0] < 0.0, map.0[4] < 0.0);
        (img.pixels.len() == rect.width() as usize * rect.height() as usize * 4)
            .then_some((rect, img))
    }

    fn decode_bitmap(&self, bitm: &Node) -> Result<RgbaImage, AffinityError> {
        decode_bitmap(self.archive, self.graph, bitm)
    }
}

/// Resample an image through a full affine map (bitmap pixel space →
/// canvas space): the destination is the transformed rect's bounding
/// box; every destination pixel centre inverse-maps into the source and
/// samples bilinearly, transparent outside it.
fn affine_resample(img: &RgbaImage, map: &Mat) -> Option<(IntRect, RgbaImage)> {
    let inv = map.invert()?;
    let (sw, sh) = (img.width as f64, img.height as f64);
    let corners = [
        map.apply(0.0, 0.0),
        map.apply(sw, 0.0),
        map.apply(0.0, sh),
        map.apply(sw, sh),
    ];
    let mut lo = (f64::INFINITY, f64::INFINITY);
    let mut hi = (f64::NEG_INFINITY, f64::NEG_INFINITY);
    for (x, y) in corners {
        if !x.is_finite() || !y.is_finite() {
            return None;
        }
        lo = (lo.0.min(x), lo.1.min(y));
        hi = (hi.0.max(x), hi.1.max(y));
    }
    if lo.0.abs().max(lo.1.abs()).max(hi.0.abs()).max(hi.1.abs()) > (1 << 24) as f64 {
        return None;
    }
    let rect = IntRect::new(
        lo.0.floor() as i32,
        lo.1.floor() as i32,
        hi.0.ceil() as i32,
        hi.1.ceil() as i32,
    );
    let (dw, dh) = (rect.width() as usize, rect.height() as usize);
    if rect.is_empty() || dw * dh > (1 << 28) {
        return None;
    }

    let (iw, ih) = (img.width as i64, img.height as i64);
    // Taps are premultiplied (so transparent neighbours don't drag
    // colour in) but kept on the 0–255 scale; the unpremultiply ratio
    // and the alpha write-out below are scale-invariant.
    let fetch = |x: i64, y: i64| -> [f32; 4] {
        if x < 0 || y < 0 || x >= iw || y >= ih {
            return [0.0; 4];
        }
        let at = ((y as usize * iw as usize) + x as usize) * 4;
        let p = &img.pixels[at..at + 4];
        let a = p[3] as f32;
        [p[0] as f32 * a, p[1] as f32 * a, p[2] as f32 * a, a]
    };
    let mut pixels = vec![0u8; dw * dh * 4];
    let m = &inv.0;
    // Fully opaque sources (photos, most pasted images) need no
    // premultiply/unpremultiply when the whole 2×2 neighbourhood is
    // inside the image: the taps are opaque, so a straight lerp of the
    // raw channels gives the same result without twelve multiplies and
    // a divide per pixel.
    let opaque = img
        .pixels
        .as_chunks::<4>()
        .0
        .par_iter()
        .all(|p| p[3] == 0xFF);
    pixels
        .par_chunks_exact_mut(dw * 4)
        .enumerate()
        .for_each(|(y, row)| {
            // The inverse map is affine, so along a row the source point
            // advances by a constant (m[0], m[3]) per pixel.
            let py = rect.top as f64 + y as f64 + 0.5;
            let (row_sx, row_sy) = (
                m[1] * py + m[2] + m[0] * (rect.left as f64 + 0.5),
                m[4] * py + m[5] + m[3] * (rect.left as f64 + 0.5),
            );
            for (x, out) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                let (sx, sy) = (row_sx + m[0] * x as f64, row_sy + m[3] * x as f64);
                let (fx, fy) = (sx - 0.5, sy - 0.5);
                if fx < -1.0 || fy < -1.0 || fx > sw || fy > sh {
                    continue;
                }
                // Branch-free floor: `as` truncates toward zero, so
                // adjust down for negative non-integers. (Landing one
                // texel low on a negative integer just moves all the
                // weight to the other tap — same sample.)
                let (tx, ty) = (fx as i64, fy as i64);
                let x0 = tx - (tx as f64 > fx) as i64;
                let y0 = ty - (ty as f64 > fy) as i64;
                let (wx, wy) = ((fx - x0 as f64) as f32, (fy - y0 as f64) as f32);
                if opaque && x0 >= 0 && y0 >= 0 && x0 + 1 < iw && y0 + 1 < ih {
                    let row0 = &img.pixels[(y0 as usize * iw as usize + x0 as usize) * 4..][..8];
                    let row1 =
                        &img.pixels[((y0 + 1) as usize * iw as usize + x0 as usize) * 4..][..8];
                    for c in 0..3 {
                        let top = row0[c] as f32 * (1.0 - wx) + row0[c + 4] as f32 * wx;
                        let bot = row1[c] as f32 * (1.0 - wx) + row1[c + 4] as f32 * wx;
                        out[c] = (top * (1.0 - wy) + bot * wy + 0.5) as u8;
                    }
                    out[3] = 0xFF;
                    continue;
                }
                let acc = if x0 >= 0 && y0 >= 0 && x0 + 1 < iw && y0 + 1 < ih {
                    // Whole 2×2 neighbourhood inside: read the two row
                    // pairs directly, skipping the per-tap bounds test.
                    let at = |px: i64, py: i64| -> [f32; 4] {
                        let p = &img.pixels[((py as usize * iw as usize) + px as usize) * 4..][..4];
                        let a = p[3] as f32;
                        [p[0] as f32 * a, p[1] as f32 * a, p[2] as f32 * a, a]
                    };
                    let (p00, p10) = (at(x0, y0), at(x0 + 1, y0));
                    let (p01, p11) = (at(x0, y0 + 1), at(x0 + 1, y0 + 1));
                    let mut acc = [0.0f32; 4];
                    for c in 0..4 {
                        let top = p00[c] * (1.0 - wx) + p10[c] * wx;
                        let bot = p01[c] * (1.0 - wx) + p11[c] * wx;
                        acc[c] = top * (1.0 - wy) + bot * wy;
                    }
                    acc
                } else {
                    let mut acc = [0.0f32; 4];
                    for (dxy, wgt) in [
                        ((0, 0), (1.0 - wx) * (1.0 - wy)),
                        ((1, 0), wx * (1.0 - wy)),
                        ((0, 1), (1.0 - wx) * wy),
                        ((1, 1), wx * wy),
                    ] {
                        let p = fetch(x0 + dxy.0, y0 + dxy.1);
                        for (a, v) in acc.iter_mut().zip(p) {
                            *a += v * wgt;
                        }
                    }
                    acc
                };
                if acc[3] > f32::EPSILON {
                    let unpremul = 1.0 / acc[3];
                    for i in 0..3 {
                        out[i] = (acc[i] * unpremul + 0.5).clamp(0.0, 255.0) as u8;
                    }
                    out[3] = (acc[3] + 0.5).clamp(0.0, 255.0) as u8;
                }
            }
        });
    Some((
        rect,
        RgbaImage {
            width: dw as u32,
            height: dh as u32,
            pixels,
        },
    ))
}

struct RgbaImage {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

/// Channel layout of a "DyBm" by its `Frmt` enum id.
struct Format {
    bytes_per_sample: usize,
    channels: usize,
    kind: FormatKind,
}

enum FormatKind {
    Rgba,
    Gray,
    Cmyk,
    Lab,
    /// One channel; decodes to (v, v, v, opaque).
    Mask,
}

fn format(id: u16) -> Option<Format> {
    let (bytes_per_sample, channels, kind) = match id {
        0 => (1, 4, FormatKind::Rgba),
        1 => (2, 4, FormatKind::Rgba),
        2 => (1, 2, FormatKind::Gray),
        3 => (2, 2, FormatKind::Gray),
        4 => (1, 5, FormatKind::Cmyk),
        5 => (2, 4, FormatKind::Lab),
        // Single 8-bit channel: layer masks, and Photo 2's (usually
        // evicted) composite caches.
        6 => (1, 1, FormatKind::Mask),
        9 => (4, 4, FormatKind::Rgba),
        _ => return None,
    };
    Some(Format {
        bytes_per_sample,
        channels,
        kind,
    })
}

/// Every tile status of every channel, flattened; missing `Sta` fields
/// read as empty.
fn all_statuses(bitm: &Node) -> Vec<u8> {
    let mut out = Vec::new();
    for sta in [b"Sta1", b"Sta2", b"Sta3", b"Sta4", b"Sta5"] {
        if let Some(Value::Array(items)) = bitm.field(sta) {
            out.extend(items.iter().filter_map(|v| match v {
                Value::U8(b) => Some(*b),
                _ => None,
            }));
        }
    }
    out
}

/// True when a bitmap stores or synthesizes any pixel at all. Photo 2
/// keeps evicted composite caches around (all statuses 0) — those are
/// absence, not content.
fn bitmap_has_content(bitm: &Node) -> bool {
    all_statuses(bitm).iter().any(|&s| s > 1)
}

fn decode_bitmap(
    archive: &Archive,
    graph: &Graph,
    bitm: &Node,
) -> Result<RgbaImage, AffinityError> {
    let frmt = enum_of(bitm, b"Frmt").ok_or_else(|| malformed("bitmap has no format"))?;
    let fmt = format(frmt).ok_or_else(|| malformed(format!("unknown pixel format {frmt}")))?;
    let width = i32_of(bitm, b"BmpW").unwrap_or(0);
    let height = i32_of(bitm, b"BmpH").unwrap_or(0);
    if width <= 0 || height <= 0 || width > 1 << 20 || height > 1 << 20 {
        return Err(malformed(format!("implausible bitmap {width}×{height}")));
    }
    let (width, height) = (width as usize, height as usize);

    let row_bytes = width * fmt.bytes_per_sample;
    let pitch = row_bytes.div_ceil(256) * 256;
    let rows = height.div_ceil(256) * 256;

    // Placed images don't duplicate their pixels: tiles with status 5
    // pull from the original file, carried in the Bckg entry. A fully
    // evicted bitmap (no status arrays at all) that still has its
    // source *is* the source, wholesale.
    let statuses = all_statuses(bitm);
    if statuses.is_empty() && bitm.field(b"Bckg").is_some() {
        return source_image(archive, bitm, width, height);
    }
    let source = if statuses.contains(&5) {
        Some(source_image(archive, bitm, width, height)?)
    } else {
        None
    };

    let sta_names: [&[u8; 4]; 5] = [b"Sta1", b"Sta2", b"Sta3", b"Sta4", b"Sta5"];
    let idx_names: [&[u8; 4]; 5] = [b"Idx1", b"Idx2", b"Idx3", b"Idx4", b"Idx5"];
    let twi_names: [&[u8; 4]; 5] = [b"TWi1", b"TWi2", b"TWi3", b"TWi4", b"TWi5"];
    let planes = (0..fmt.channels)
        .into_par_iter()
        .map(|channel| {
            load_plane(PlaneJob {
                archive,
                graph,
                bitm,
                sta: sta_names[channel],
                idx: idx_names[channel],
                // Affinity rounds the tile grid up past the pixels it
                // needs, so the status array's row stride is the declared
                // `TWi`, not `ceil(row_bytes / 256)`. Reading it as the
                // tight grid shears every row after the first.
                grid_width: i32_of(bitm, twi_names[channel])
                    .filter(|w| *w > 0)
                    .map_or(row_bytes.div_ceil(256), |w| w as usize),
                pitch,
                rows,
                height,
                bytes_per_sample: fmt.bytes_per_sample,
                source: source.as_ref().map(|s| (s, channel)),
            })
        })
        .collect::<Result<Vec<_>, _>>()?;

    // Interleave planes into straight-alpha RGBA8. Higher depths are
    // reduced to 8 bits here; precision, not placement, is what's lost.
    let sample = |plane: &[u8], x: usize, y: usize| -> f32 {
        let at = y * pitch + x * fmt.bytes_per_sample;
        match fmt.bytes_per_sample {
            1 => plane[at] as f32 / 255.0,
            2 => u16::from_le_bytes([plane[at], plane[at + 1]]) as f32 / 65535.0,
            _ => f32::from_le_bytes(plane[at..at + 4].try_into().unwrap()).clamp(0.0, 1.0),
        }
    };

    let mut pixels = vec![0u8; width * height * 4];
    match (fmt.bytes_per_sample, &fmt.kind) {
        // 8-bit samples map to output bytes unchanged; interleave the
        // planes directly instead of round-tripping through f32.
        (1, FormatKind::Rgba) => {
            pixels
                .par_chunks_exact_mut(width * 4)
                .enumerate()
                .for_each(|(y, out_row)| {
                    let at = y * pitch;
                    let (r, g, b, a) = (
                        &planes[0][at..at + width],
                        &planes[1][at..at + width],
                        &planes[2][at..at + width],
                        &planes[3][at..at + width],
                    );
                    for (x, px) in out_row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                        *px = [r[x], g[x], b[x], a[x]];
                    }
                });
            return Ok(RgbaImage {
                width: width as u32,
                height: height as u32,
                pixels,
            });
        }
        (1, FormatKind::Gray) => {
            pixels
                .par_chunks_exact_mut(width * 4)
                .enumerate()
                .for_each(|(y, out_row)| {
                    let at = y * pitch;
                    let (g, a) = (&planes[0][at..at + width], &planes[1][at..at + width]);
                    for (x, px) in out_row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                        *px = [g[x], g[x], g[x], a[x]];
                    }
                });
            return Ok(RgbaImage {
                width: width as u32,
                height: height as u32,
                pixels,
            });
        }
        (1, FormatKind::Mask) => {
            pixels
                .par_chunks_exact_mut(width * 4)
                .enumerate()
                .for_each(|(y, out_row)| {
                    let at = y * pitch;
                    let v = &planes[0][at..at + width];
                    for (x, px) in out_row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                        *px = [v[x], v[x], v[x], 0xFF];
                    }
                });
            return Ok(RgbaImage {
                width: width as u32,
                height: height as u32,
                pixels,
            });
        }
        _ => {}
    }
    pixels
        .par_chunks_exact_mut(width * 4)
        .enumerate()
        .for_each(|(y, out_row)| {
            for (x, out) in out_row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                let (r, g, b, a) = match fmt.kind {
                    FormatKind::Rgba => (
                        sample(&planes[0], x, y),
                        sample(&planes[1], x, y),
                        sample(&planes[2], x, y),
                        sample(&planes[3], x, y),
                    ),
                    FormatKind::Gray => {
                        let g = sample(&planes[0], x, y);
                        (g, g, g, sample(&planes[1], x, y))
                    }
                    FormatKind::Mask => {
                        let v = sample(&planes[0], x, y);
                        (v, v, v, 1.0)
                    }
                    FormatKind::Cmyk => {
                        let (c, m, yl, k) = (
                            sample(&planes[0], x, y),
                            sample(&planes[1], x, y),
                            sample(&planes[2], x, y),
                            sample(&planes[3], x, y),
                        );
                        (
                            (1.0 - c) * (1.0 - k),
                            (1.0 - m) * (1.0 - k),
                            (1.0 - yl) * (1.0 - k),
                            sample(&planes[4], x, y),
                        )
                    }
                    FormatKind::Lab => {
                        let l = sample(&planes[0], x, y) * 100.0;
                        let a_c = sample(&planes[1], x, y) * 255.0 - 128.0;
                        let b_c = sample(&planes[2], x, y) * 255.0 - 128.0;
                        let (r, g, b) = lab_to_srgb(l, a_c, b_c);
                        (r, g, b, sample(&planes[3], x, y))
                    }
                };
                out[0] = (r * 255.0 + 0.5) as u8;
                out[1] = (g * 255.0 + 0.5) as u8;
                out[2] = (b * 255.0 + 0.5) as u8;
                out[3] = (a * 255.0 + 0.5) as u8;
            }
        });
    Ok(RgbaImage {
        width: width as u32,
        height: height as u32,
        pixels,
    })
}

struct PlaneJob<'a> {
    archive: &'a Archive<'a>,
    graph: &'a Graph,
    bitm: &'a Node,
    sta: &'a [u8; 4],
    idx: &'a [u8; 4],
    /// Tiles per row of the status array, as the file declares it.
    grid_width: usize,
    pitch: usize,
    rows: usize,
    height: usize,
    bytes_per_sample: usize,
    /// The bitmap's original file and which of its channels this plane
    /// is, when any tile is source-backed (status 5).
    source: Option<(&'a RgbaImage, usize)>,
}

/// Rebuild one channel plane from its tile status list and blocks.
fn load_plane(job: PlaneJob) -> Result<Vec<u8>, AffinityError> {
    let statuses: Vec<u8> = match job.bitm.field(job.sta) {
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| match v {
                Value::U8(b) => Ok(*b),
                _ => Err(malformed("tile status is not a byte")),
            })
            .collect::<Result<_, _>>()?,
        // Fully evicted bitmaps drop their status arrays entirely.
        _ => Vec::new(),
    };
    let blocks = job.graph.children(job.bitm, job.idx);
    let mut next_block = blocks.iter();

    let mut plane = vec![0u8; job.pitch * job.rows];
    let places = tile_offsets(job.grid_width, job.height);
    // Pair each stored tile with its destination first; decompressing
    // and CRC-checking the payloads is the bulk of the work and every
    // tile is independent, so that part fans out across cores.
    let mut stored: Vec<(usize, usize, [i32; 4], &str)> = Vec::new();
    for (&status, (x, y)) in statuses.iter().zip(places) {
        match status {
            0 | 1 => {}
            2 => fill_tile(&mut plane, job.pitch, x, y, &[0xFF]),
            3 => fill_tile(&mut plane, job.pitch, x, y, &0x3F80_0000u32.to_le_bytes()),
            4 => {
                let block = next_block
                    .next()
                    .ok_or_else(|| malformed("more stored tiles than blocks"))?;
                // Photo 2 omits the rect on full tiles; the copy below
                // clips to the plane, so the full default is safe.
                let rect: [i32; 4] = match block.field(b"Rect").or_else(|| block.field(b"IRct")) {
                    Some(Value::VecI(v)) if v.len() == 4 => [v[0], v[1], v[2], v[3]],
                    _ => [0, 0, 256, 256],
                };
                let name = match block.field(b"Data") {
                    Some(Value::Embedded { name, .. }) => name,
                    _ => return Err(malformed("block has no data reference")),
                };
                stored.push((x, y, rect, name));
            }
            // Source-backed: the pixels live in the bitmap's original
            // file (Bckg), not in tile entries.
            5 => {
                let Some((source, channel)) = job.source else {
                    return Err(malformed("source-backed tile without a source image"));
                };
                copy_source_tile(&mut plane, &job, source, channel, x, y)?;
            }
            other => return Err(malformed(format!("unknown tile status {other}"))),
        }
    }
    let tiles = stored
        .par_iter()
        .map(|&(_, _, _, name)| {
            let entry = job
                .archive
                .head(name)
                .ok_or_else(|| malformed(format!("missing tile entry {name:?}")))?;
            tile_payload(job.archive.extract(entry)?)
                .ok_or_else(|| malformed(format!("tile {name:?} has no 64 KiB payload")))
        })
        .collect::<Result<Vec<_>, _>>()?;
    for ((x, y, rect, _), tile) in stored.iter().zip(&tiles) {
        let (x0, y0) = (
            rect[0].clamp(0, 256) as usize,
            rect[1].clamp(0, 256) as usize,
        );
        let (x1, y1) = (
            rect[2].clamp(0, 256) as usize,
            rect[3].clamp(0, 256) as usize,
        );
        for ty in y0..y1 {
            if y + ty >= job.rows {
                break;
            }
            let dst = (y + ty) * job.pitch + x + x0;
            let src = ty * 256 + x0;
            let n = x1.saturating_sub(x0).min(job.pitch - (x + x0));
            plane[dst..dst + n].copy_from_slice(&tile[src..src + n]);
        }
    }
    Ok(plane)
}

/// Where each entry of a `Sta` array lands in its plane: `x` is a byte
/// offset, `y` a row. The grid is `grid_width` tiles across — which is
/// not always the tight `ceil(row_bytes / 256)`, because Affinity
/// rounds the allocation up (a 5848-byte row can sit in a 32-tile
/// grid). Walking it at the tight width shears every row but the first.
fn tile_offsets(grid_width: usize, height: usize) -> impl Iterator<Item = (usize, usize)> {
    let width = grid_width.max(1);
    (0..)
        .map(move |i| ((i % width) * 256, (i / width) * 256))
        .take_while(move |&(_, y)| y < height)
}

/// Fill one tile of a channel plane from the decoded source image.
/// `x` is a byte offset into the plane; the source is always RGBA8, so
/// wider formats spread each 8-bit sample across the sample width.
fn copy_source_tile(
    plane: &mut [u8],
    job: &PlaneJob,
    source: &RgbaImage,
    channel: usize,
    x: usize,
    y: usize,
) -> Result<(), AffinityError> {
    if channel >= 4 {
        return Err(malformed("source-backed tile in a >4 channel format"));
    }
    let px0 = x / job.bytes_per_sample;
    let per_tile = 256 / job.bytes_per_sample;
    let sw = source.width as usize;
    for ty in 0..256usize {
        let sy = y + ty;
        if sy >= source.height as usize || sy >= job.rows {
            break;
        }
        for tx in 0..per_tile {
            let sx = px0 + tx;
            if sx >= sw {
                break;
            }
            let v = source.pixels[(sy * sw + sx) * 4 + channel];
            let at = sy * job.pitch + x + tx * job.bytes_per_sample;
            match job.bytes_per_sample {
                1 => plane[at] = v,
                2 => plane[at..at + 2].copy_from_slice(&(v as u16 * 257).to_le_bytes()),
                _ => plane[at..at + 4].copy_from_slice(&(v as f32 / 255.0).to_le_bytes()),
            }
        }
    }
    Ok(())
}

/// Decode a bitmap's original file: the Bckg entry is a tiny "Blck"
/// graph document whose Data blob is the file bytes (PNG, JPEG…).
fn source_image(
    archive: &Archive,
    bitm: &Node,
    width: usize,
    height: usize,
) -> Result<RgbaImage, AffinityError> {
    let name = match bitm.field(b"Bckg") {
        Some(Value::Embedded { name, .. }) => name,
        _ => return Err(malformed("source-backed bitmap has no Bckg entry")),
    };
    let entry = archive
        .head(name)
        .ok_or_else(|| malformed(format!("missing source entry {name:?}")))?;
    let data = archive.extract(entry)?;
    let graph = graph::parse(&data)?;
    let file = graph
        .node(graph::ROOT)
        .field(b"Data")
        .and_then(|v| match v {
            Value::Blob(b) => Some(b),
            _ => None,
        })
        .ok_or_else(|| malformed("source entry has no file data"))?;
    let img = image::load_from_memory(file)
        .map_err(|e| malformed(format!("decoding source image: {e}")))?
        .to_rgba8();
    if (img.width() as usize, img.height() as usize) != (width, height) {
        return Err(malformed(format!(
            "source image is {}×{}, bitmap says {width}×{height}",
            img.width(),
            img.height()
        )));
    }
    Ok(RgbaImage {
        width: img.width(),
        height: img.height(),
        pixels: img.into_raw(),
    })
}

/// The colour of a fill class ("FilS" solid): its `Colr` child as RGBA
/// bytes. "None" fills and gradients give nothing.
fn fill_color(graph: &Graph, fill: &Node) -> Option<[u8; 4]> {
    let colr = graph.child(fill, b"Colr")?;
    color_bytes(colr)
}

/// The colour behind a fill descriptor (`FDsc.FDeF`): solid fills give
/// their colour; "none" fills and gradients give nothing.
fn descriptor_color(graph: &Graph, fdsc: &Node) -> Option<[u8; 4]> {
    fill_color(graph, graph.child(fdsc, b"FDeF")?)
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

/// Read a gradient off a fill class ("FilG"), with `host` the fill
/// descriptor when one wraps it (newer files hang the gradient's
/// transform there; older ones put it on the fill itself).
fn gradient_fill(graph: &Graph, fill: &Node, host: Option<&Node>) -> Option<GradientFill> {
    if fill.types.iter().all(|(t, _)| *t != graph::tag(b"FilG")) {
        return None;
    }
    let radial = matches!(fill.field(b"Type"), Some(Value::Enum { id: 2.., .. }));
    let grad = graph.child(fill, b"Grad")?;
    let positions: Vec<f32> = match grad.field(b"Posn") {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|v| match v {
                Value::VecD(p) => p.first().map(|x| *x as f32),
                _ => None,
            })
            .collect(),
        _ => return None,
    };
    let colors: Vec<[u8; 4]> = graph
        .children(grad, b"Cols")
        .iter()
        .filter_map(|c| color_bytes(c))
        .collect();
    if positions.len() != colors.len() || positions.len() < 2 {
        return None;
    }
    // The gradient transform hangs off the descriptor in newer files
    // and off the fill itself in older ones.
    let m: [f64; 6] = match host
        .and_then(|h| h.field(b"FDeX"))
        .or_else(|| fill.field(b"FDeX"))
    {
        Some(Value::VecD(v)) => v.first_chunk().copied()?,
        _ => return None,
    };
    Some(GradientFill {
        stops: positions.into_iter().zip(colors).collect(),
        start: (m[2], m[5]),
        end: (m[0] + m[2], m[3] + m[5]),
        radial,
    })
}

impl GradientFill {
    fn color_at(&self, t: f32) -> [u8; 4] {
        let t = t.clamp(0.0, 1.0);
        let mut prev = self.stops[0];
        for &(pos, col) in &self.stops {
            if t <= pos {
                let span = pos - prev.0;
                let f = if span <= f32::EPSILON {
                    1.0
                } else {
                    (t - prev.0) / span
                };
                let mix = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * f + 0.5) as u8;
                return [
                    mix(prev.1[0], col[0]),
                    mix(prev.1[1], col[1]),
                    mix(prev.1[2], col[2]),
                    mix(prev.1[3], col[3]),
                ];
            }
            prev = (pos, col);
        }
        self.stops.last().unwrap().1
    }
}

/// The fill colour of a text run: the first `Objs` descriptor whose
/// `FDeF` fill carries a `Colr` class, converted to RGBA bytes.
fn run_color(graph: &Graph, run_item: &Node) -> Option<[u8; 4]> {
    let objs = graph.children(run_item, b"Objs");
    objs.iter().find_map(|obj| descriptor_color(graph, obj))
}

/// Convert a colour class (`RGBA`/`HSLA`/`GRAY`/`CMYK`) to RGBA bytes.
/// CIELAB (D50, the ICC connection space) to sRGB, for the lens filter's
/// stored colour. `l` 0..100, `a`/`b` about -128..127.
fn lab_to_rgb(l: f32, a: f32, b: f32) -> [f32; 3] {
    let fy = (l + 16.0) / 116.0;
    let fx = fy + a / 500.0;
    let fz = fy - b / 200.0;
    let inv = |t: f32| {
        if t > 6.0 / 29.0 {
            t * t * t
        } else {
            3.0 * (6.0f32 / 29.0).powi(2) * (t - 4.0 / 29.0)
        }
    };
    // D50 white point.
    let (xn, yn, zn) = (0.9642, 1.0, 0.8249);
    let (x, y, z) = (xn * inv(fx), yn * inv(fy), zn * inv(fz));
    // XYZ(D50) -> linear sRGB (Bradford-adapted matrix).
    let rl = 3.133_856 * x - 1.616_867 * y - 0.490_615 * z;
    let gl = -0.978_768 * x + 1.916_142 * y + 0.033_454 * z;
    let bl = 0.071_945 * x - 0.228_991 * y + 1.405_243 * z;
    let enc = |v: f32| {
        let v = v.clamp(0.0, 1.0);
        if v <= 0.0031308 {
            12.92 * v
        } else {
            1.055 * v.powf(1.0 / 2.4) - 0.055
        }
    };
    [enc(rl), enc(gl), enc(bl)]
}

fn color_bytes(colr: &Node) -> Option<[u8; 4]> {
    {
        let Value::Struct(raw) = colr.field(b"_col")? else {
            return None;
        };
        let f = |i: usize| f32::from_le_bytes(raw[i * 4..i * 4 + 4].try_into().unwrap());
        let to = |v: f32| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
        match (&colr.type_tag().to_be_bytes(), raw.len()) {
            (b"RGBA", 16) => Some([to(f(0)), to(f(1)), to(f(2)), to(f(3))]),
            (b"HSLA", 16) => {
                let (r, g, b) = hsl_to_rgb(f(0), f(1), f(2));
                Some([to(r), to(g), to(b), to(f(3))])
            }
            (b"GRAY", 8) => Some([to(f(0)), to(f(0)), to(f(0)), to(f(1))]),
            (b"CMYK", 20) => {
                let k = f(3);
                Some([
                    to((1.0 - f(0)) * (1.0 - k)),
                    to((1.0 - f(1)) * (1.0 - k)),
                    to((1.0 - f(2)) * (1.0 - k)),
                    to(f(4)),
                ])
            }
            (_, 4) => Some([raw[0], raw[1], raw[2], raw[3]]),
            _ => None,
        }
    }
}

fn hsl_to_rgb(h: f32, s: f32, l: f32) -> (f32, f32, f32) {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = (h.rem_euclid(1.0)) * 6.0;
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r, g, b) = match hp as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    (r + m, g + m, b + m)
}

/// Assemble a subpath from PCrv records.
///
/// The marker pair classifies each record: (1,0) and (2,0) are
/// on-curve points (terminal and interior respectively), (0,1) is the
/// previous point's outgoing control and (0,2) the next point's
/// incoming control. A closed path's trailing controls belong to the
/// segment joining back to the first point.
fn subpath_from_records(
    records: &[(f32, f32, u8, u8)],
    closed: bool,
) -> Option<schist_core::SubPath> {
    use schist_core::Anchor;
    let mut anchors: Vec<Anchor> = Vec::new();
    let mut incoming: Option<(f32, f32)> = None;
    for &(x, y, m0, m1) in records {
        match (m0, m1) {
            (0, 1) => {
                if let Some(prev) = anchors.last_mut() {
                    prev.handle_out = (x - prev.point.0, y - prev.point.1);
                }
            }
            (0, 2) => incoming = Some((x, y)),
            _ => {
                let handle_in = incoming
                    .take()
                    .map(|c| (c.0 - x, c.1 - y))
                    .unwrap_or((0.0, 0.0));
                anchors.push(Anchor {
                    point: (x, y),
                    handle_in,
                    handle_out: (0.0, 0.0),
                });
            }
        }
    }
    if closed {
        if let (Some(c), Some(first)) = (incoming, anchors.first_mut()) {
            first.handle_in = (c.0 - first.point.0, c.1 - first.point.1);
        }
    }
    if anchors.len() < 2 {
        return None;
    }
    Some(schist_core::SubPath { anchors, closed })
}

/// Circle-to-Bezier handle length as a fraction of the radius.
const KAPPA: f32 = 0.552_284_8;

/// Build a shape's outline in its local `ShpB` box, from its `Shpe`
/// parameters. Returns the display name Affinity's own panel would show
/// and the anchors of one closed subpath — or `None` for kinds whose
/// geometry we can't rebuild (those are reported, not guessed).
type ShapeSubPaths = Vec<(Vec<schist_core::Anchor>, bool)>;

fn shape_geometry(
    shpe: &Node,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
) -> Option<(&'static str, ShapeSubPaths)> {
    use schist_core::Anchor;
    use std::f32::consts::{FRAC_PI_2, PI, TAU};
    let (w, h) = (x1 - x0, y1 - y0);
    let (cx, cy) = ((x0 + x1) * 0.5, (y0 + y1) * 0.5);
    let closed = |a: Vec<Anchor>| vec![(a, true)];
    let f = |t: &[u8; 4], d: f32| f32_last(shpe, t).unwrap_or(d);
    let tag_bytes = shpe.type_tag().to_be_bytes();
    match &tag_bytes {
        b"ShNR" | b"ShRR" => {
            let mut radii = match shpe.field(b"ShCR") {
                Some(Value::VecF(r)) if r.len() >= 4 => [r[0], r[1], r[2], r[3]],
                Some(Value::VecF(r)) if !r.is_empty() => [r[0]; 4],
                _ => [0.0; 4],
            };
            // Designer's rectangle tool keeps default radii in ShCR but
            // only shapes corners whose type (`CTyp`, one enum per
            // corner) says so: 0 rounded · 1 straight (chamfer) ·
            // 2 concave · 3 cutout, probed with one fixture per type.
            // A CTyp-less ShNR renders sharp in Affinity's own
            // thumbnail, radii notwithstanding.
            let mut corner_types = [0u16; 4];
            let has_ctyp = matches!(shpe.field(b"CTyp"), Some(Value::Array(_)));
            match shpe.field(b"CTyp") {
                Some(Value::Array(types)) => {
                    for (i, r) in radii.iter_mut().enumerate() {
                        match types.get(i) {
                            Some(Value::Enum { id, .. }) if *id <= 3 => {
                                corner_types[i] = *id;
                            }
                            _ => *r = 0.0,
                        }
                    }
                }
                _ if &tag_bytes == b"ShNR" => radii = [0.0; 4],
                _ => {}
            }
            // "Single radius" locks every corner to the first one's
            // radius and treatment.
            if matches!(shpe.field(b"Lock"), Some(Value::Bool(true))) {
                radii = [radii[0]; 4];
                corner_types = [corner_types[0]; 4];
            }
            let scale = if matches!(shpe.field(b"AbSz"), Some(Value::Bool(true))) {
                1.0 // absolute: already in local units
            } else if has_ctyp {
                // The unified app's writer (which also writes CTyp)
                // scales radii by the full shorter side — a 25% radius
                // chamfers 33px off a 132px-tall rect — where Designer
                // 1.x scaled by half of it.
                w.min(h)
            } else {
                w.min(h) * 0.5 // fraction of half the shorter side
            };
            let radii = radii.map(|r| (r * scale).clamp(0.0, w.min(h) * 0.5));
            let kind = if radii.iter().all(|r| *r < 0.25) {
                "Rectangle"
            } else {
                "Rounded Rectangle"
            };
            Some((
                kind,
                closed(cornered_rect_anchors(x0, y0, x1, y1, radii, corner_types)),
            ))
        }
        b"ShpE" => Some(("Ellipse", closed(ellipse_anchors(x0, y0, x1, y1)))),
        b"ShSt" => {
            // Star: `Pnts` points alternating between the ellipse
            // inscribed in the box and `IRad` of it; the first point is
            // up. `CrvL`/`CrvR` bow each spike's left (notch→tip) and
            // right (tip→notch) edges sideways — positive bows to the
            // left of the direction of travel, i.e. outward.
            let cl = f32_last(shpe, b"CrvL").unwrap_or(0.0).clamp(-1.0, 1.0);
            let cr = f32_last(shpe, b"CrvR").unwrap_or(0.0).clamp(-1.0, 1.0);
            let points = u16_of(shpe, b"Pnts").unwrap_or(5).clamp(3, 100) as usize;
            let inner = f32_last(shpe, b"IRad").unwrap_or(0.5).clamp(0.0, 1.0);
            let mut anchors: Vec<Anchor> = (0..points * 2)
                .map(|i| {
                    let r = if i % 2 == 0 { 1.0 } else { inner };
                    let ang = -FRAC_PI_2 + PI * i as f32 / points as f32;
                    unit_anchor(ang.cos() * r, ang.sin() * r, x0, y0, x1, y1)
                })
                .collect();
            let n = anchors.len();
            for i in 0..n {
                // Edge i runs anchors[i] → anchors[i+1]: tips sit at
                // even indices, so even i is tip→notch (right edge).
                let c = if i % 2 == 0 { cr } else { cl };
                if c.abs() < 0.005 {
                    continue;
                }
                let (a, b) = (anchors[i].point, anchors[(i + 1) % n].point);
                let (dx, dy) = (b.0 - a.0, b.1 - a.1);
                // Left of travel in screen space (y down).
                let (nx, ny) = (dy, -dx);
                let len = (nx * nx + ny * ny).sqrt().max(1e-6);
                let sag = c * (dx * dx + dy * dy).sqrt() * 0.22;
                let (mx, my) = (
                    (a.0 + b.0) * 0.5 + nx / len * 2.0 * sag,
                    (a.1 + b.1) * 0.5 + ny / len * 2.0 * sag,
                );
                // The quadratic through that sagitta, as a cubic.
                anchors[i].handle_out = ((mx - a.0) * (2.0 / 3.0), (my - a.1) * (2.0 / 3.0));
                anchors[(i + 1) % n].handle_in =
                    ((mx - b.0) * (2.0 / 3.0), (my - b.1) * (2.0 / 3.0));
            }
            Some(("Star", closed(anchors)))
        }
        b"ShSS" => Some((
            "Square Star",
            closed(square_star_anchors(shpe, x0, y0, x1, y1)),
        )),
        b"ShCl" => Some(("Cloud", closed(cloud_anchors(shpe, x0, y0, x1, y1)))),
        b"ShHt" => {
            let spread = f32_last(shpe, b"Sprd").unwrap_or(0.2).clamp(0.0, 1.0);
            Some(("Heart", closed(heart_anchors(x0, y0, x1, y1, spread))))
        }
        // The kinds below were each drawn once in Affinity itself
        // (fixtures/affinity-probe/shp_*.af) and their geometry verified
        // against those files' embedded thumbnails.
        // Triangle: apex `Pos` of the way along the top edge.
        b"ShpT" => {
            let pos = f(b"Pos ", 0.5).clamp(0.0, 1.0);
            Some((
                "Triangle",
                closed(vec![
                    Anchor::corner(x0 + pos * w, y0),
                    Anchor::corner(x1, y1),
                    Anchor::corner(x0, y1),
                ]),
            ))
        }
        // Diamond: widest at `Pos` of the height.
        b"ShpD" => {
            let pos = f(b"Pos ", 0.5).clamp(0.0, 1.0);
            let ym = y0 + pos * h;
            Some((
                "Diamond",
                closed(vec![
                    Anchor::corner(cx, y0),
                    Anchor::corner(x1, ym),
                    Anchor::corner(cx, y1),
                    Anchor::corner(x0, ym),
                ]),
            ))
        }
        // Trapezoid: the top edge runs from `PosL` to `PosR`.
        b"ShTz" => {
            let l = f(b"PosL", 0.25).clamp(0.0, 1.0);
            let r = f(b"PosR", 0.75).clamp(0.0, 1.0);
            Some((
                "Trapezoid",
                closed(vec![
                    Anchor::corner(x0 + l * w, y0),
                    Anchor::corner(x0 + r * w, y0),
                    Anchor::corner(x1, y1),
                    Anchor::corner(x0, y1),
                ]),
            ))
        }
        // Polygon: `Side` vertices on the inscribed ellipse, first up.
        // `Curv` bends the edges — unmapped, so only straight rebuilds.
        b"ShPy" => {
            if f(b"Curv", 0.0).abs() > 0.01 {
                return None;
            }
            let sides = u16_of(shpe, b"Side").unwrap_or(5).clamp(3, 100) as usize;
            let anchors = (0..sides)
                .map(|i| {
                    let ang = -FRAC_PI_2 + TAU * i as f32 / sides as f32;
                    unit_anchor(ang.cos(), ang.sin(), x0, y0, x1, y1)
                })
                .collect();
            Some(("Polygon", closed(anchors)))
        }
        // Double star: `Pnts` major tips at the rim with minor tips
        // (`PRad`) between them and notches (`IRad`) between every tip.
        b"ShDS" => {
            let points = u16_of(shpe, b"Pnts").unwrap_or(5).clamp(2, 100) as usize;
            let inner = f(b"IRad", 0.5).clamp(0.0, 1.0);
            let mid = f(b"PRad", 0.8).clamp(0.0, 1.0);
            let radii = [1.0, inner, mid, inner];
            let anchors = (0..points * 4)
                .map(|i| {
                    let r = radii[i % 4];
                    let ang = -FRAC_PI_2 + TAU * i as f32 / (points * 4) as f32;
                    unit_anchor(ang.cos() * r, ang.sin() * r, x0, y0, x1, y1)
                })
                .collect();
            Some(("Double Star", closed(anchors)))
        }
        // Pie and donut share a class: a ring sector from `AngS` to
        // `AngE` (visual angles, anticlockwise from +x) with an inner
        // radius. Equal angles mean the full ring; zero `IRad` a wedge.
        b"ShPi" => {
            let ang_s = f(b"AngS", 0.0);
            let ang_e = f(b"AngE", 0.0);
            let inner = f(b"IRad", 0.0).clamp(0.0, 0.999);
            let (rx, ry) = (w * 0.5, h * 0.5);
            let full = (ang_s - ang_e).abs() < 1e-4;
            if full {
                let mut subs = vec![(ellipse_anchors(x0, y0, x1, y1), true)];
                if inner > 0.001 {
                    subs.push((
                        ellipse_anchors(
                            cx - rx * inner,
                            cy - ry * inner,
                            cx + rx * inner,
                            cy + ry * inner,
                        ),
                        true,
                    ));
                }
                return Some((if inner > 0.001 { "Donut" } else { "Ellipse" }, subs));
            }
            // Screen space runs y-down, so a visual angle t is -t here.
            let t0 = -ang_s;
            let t1 = -(ang_e + if ang_e <= ang_s { TAU } else { 0.0 });
            let mut anchors = arc_anchors(cx, cy, rx, ry, t0, t1);
            if inner > 0.001 {
                anchors.extend(arc_anchors(cx, cy, rx * inner, ry * inner, t1, t0));
            } else {
                anchors.push(Anchor::corner(cx, cy));
            }
            Some(("Pie", closed(anchors)))
        }
        // Segment: the inscribed ellipse above a chord `Pos0` of the way
        // up (and below one at `Pos1`, when it cuts).
        b"ShSg" => {
            let pos0 = f(b"Pos0", 0.25).clamp(0.0, 1.0);
            let pos1 = f(b"Pos1", 1.0).clamp(0.0, 1.0);
            if pos1 < 0.999 {
                log::warn!("affinity: segment with a second chord; importing the first only");
            }
            let uy = (1.0 - 2.0 * pos0).clamp(-1.0, 1.0);
            let a = uy.asin();
            let (rx, ry) = (w * 0.5, h * 0.5);
            // From the left intersection, over the top, to the right.
            let anchors = arc_anchors(cx, cy, rx, ry, PI - a, TAU + a);
            Some(("Segment", closed(anchors)))
        }
        // Crescent: tips at the top and bottom centre; each boundary is
        // a circular arc (in the box's unit space) bowing sideways with
        // a sagitta of half the `ArcL`/`ArcR` value — negative bows
        // left, so the default −1 outer arc is the inscribed ellipse's
        // own left half.
        b"ShCr" => {
            let mut unit = bow_arc_unit(f(b"ArcL", -1.0), true);
            let up = bow_arc_unit(f(b"ArcR", -0.3), false);
            unit.extend(up);
            let anchors = unit
                .into_iter()
                .map(|mut a| {
                    a.point = (x0 + a.point.0 * w, y0 + a.point.1 * h);
                    a.handle_in = (a.handle_in.0 * w, a.handle_in.1 * h);
                    a.handle_out = (a.handle_out.0 * w, a.handle_out.1 * h);
                    a
                })
                .collect();
            Some(("Crescent", closed(anchors)))
        }
        // Arrow: a `Thck`-of-the-height shaft with a head at either end
        // when its style enum says so; head length is `LPr1`/`RPr1` of
        // the height.
        b"ShDA" => {
            let sh = f(b"Thck", 0.35).clamp(0.0, 1.0) * h * 0.5;
            let head = |style: Option<&Value>| match style {
                Some(Value::Enum { id, .. }) => *id != 0,
                _ => true,
            };
            let l_head = head(shpe.field(b"LSty"));
            let r_head = head(shpe.field(b"RSty"));
            let lw = if l_head {
                (f(b"LPr1", 0.5) * h).min(w * 0.45)
            } else {
                0.0
            };
            let rw = if r_head {
                (f(b"RPr1", 0.5) * h).min(w * 0.45)
            } else {
                0.0
            };
            let mut a = Vec::new();
            if l_head {
                a.push(Anchor::corner(x0, cy));
                a.push(Anchor::corner(x0 + lw, y0));
                a.push(Anchor::corner(x0 + lw, cy - sh));
            } else {
                a.push(Anchor::corner(x0, cy - sh));
            }
            if r_head {
                a.push(Anchor::corner(x1 - rw, cy - sh));
                a.push(Anchor::corner(x1 - rw, y0));
                a.push(Anchor::corner(x1, cy));
                a.push(Anchor::corner(x1 - rw, y1));
                a.push(Anchor::corner(x1 - rw, cy + sh));
            } else {
                a.push(Anchor::corner(x1, cy - sh));
                a.push(Anchor::corner(x1, cy + sh));
            }
            if l_head {
                a.push(Anchor::corner(x0 + lw, cy + sh));
                a.push(Anchor::corner(x0 + lw, y1));
            } else {
                a.push(Anchor::corner(x0, cy + sh));
            }
            Some(("Arrow", closed(a)))
        }
        // Cog: `Teth` teeth from `IRad` out to the rim — each tooth's
        // top spans `TtSz` of its period, the root gap `NtSz` — plus a
        // `Hole` bore. `Curv` bends the flanks; only straight rebuilds.
        b"ShCg" => {
            if f(b"Curv", 0.0).abs() > 0.01 {
                return None;
            }
            let teeth = u16_of(shpe, b"Teth").unwrap_or(12).clamp(3, 200) as usize;
            let root = f(b"IRad", 0.85).clamp(0.0, 1.0);
            let hole = f(b"Hole", 0.2).clamp(0.0, 0.999);
            let ts = f(b"TtSz", 0.37).clamp(0.0, 1.0);
            let ns = f(b"NtSz", 0.42).clamp(0.0, 1.0);
            let step = TAU / teeth as f32;
            let mut a = Vec::with_capacity(teeth * 4);
            for k in 0..teeth {
                let c = -FRAC_PI_2 + step * k as f32;
                let g = c + step * 0.5; // the gap between this tooth and the next
                for (r, ang) in [
                    (1.0, c - ts * step * 0.5),
                    (1.0, c + ts * step * 0.5),
                    (root, g - ns * step * 0.5),
                    (root, g + ns * step * 0.5),
                ] {
                    a.push(unit_anchor(ang.cos() * r, ang.sin() * r, x0, y0, x1, y1));
                }
            }
            let mut subs = vec![(a, true)];
            if hole > 0.001 {
                let (rx, ry) = (w * 0.5 * hole, h * 0.5 * hole);
                subs.push((ellipse_anchors(cx - rx, cy - ry, cx + rx, cy + ry), true));
            }
            Some(("Cog", subs))
        }
        // Callout (rounded rectangle): the balloon over the top
        // 1 − `TlHg` of the box, its corner radii in `ShCR`, with a tail
        // `TlWd` wide rooted at `TlRP` of the width pointing to its tip
        // at `TlEP` on the bottom edge.
        b"ShCR" => {
            let tail_h = f(b"TlHg", 0.3).clamp(0.0, 0.95);
            let tail_w = f(b"TlWd", 0.15).clamp(0.0, 1.0);
            let root = f(b"TlRP", 0.4).clamp(0.0, 1.0);
            let tip = f(b"TlEP", 0.2).clamp(0.0, 1.0);
            let yr = y1 - tail_h * h;
            let radii = match shpe.field(b"ShCR") {
                Some(Value::VecF(r)) if r.len() >= 4 => [r[0], r[1], r[2], r[3]],
                _ => [0.25; 4],
            };
            // Unlike the plain rounded rectangle, the callout's radii
            // scale by the full shorter side of the balloon (measured
            // off the fixture's own render: 0.25 → 23.9px on a 92px
            // balloon), not half of it.
            let scale = if matches!(shpe.field(b"AbSz"), Some(Value::Bool(true))) {
                1.0
            } else {
                w.min(yr - y0)
            };
            let radii = radii.map(|r| (r * scale).clamp(0.0, w.min(yr - y0) * 0.5));
            let mut a = rounded_rect_anchors(x0, y0, x1, yr, radii);
            // The bottom edge runs right-to-left; splice the tail in
            // after the bottom-right corner's anchors.
            let after_br = a
                .iter()
                .position(|an| an.point.1 >= yr - 0.01 && an.point.0 > cx)
                .map(|i| i + 1)
                .unwrap_or(a.len());
            let half = tail_w * w * 0.5;
            let rc = x0 + root * w;
            a.splice(
                after_br..after_br,
                [
                    Anchor::corner((rc + half).min(x1), yr),
                    Anchor::corner(x0 + tip * w, y1),
                    Anchor::corner((rc - half).max(x0), yr),
                ],
            );
            Some(("Callout", closed(a)))
        }
        // Callout (ellipse): the balloon over the top 1 − `TlHg`, its
        // tail rooted where the centre-to-tip direction meets the
        // ellipse, `TlAn` of parametric angle wide, tip at `TlEP` on
        // the bottom edge.
        b"ShCE" => {
            let tail_h = f(b"TlHg", 0.2).clamp(0.0, 0.95);
            let tip_x = x0 + f(b"TlEP", 0.15).clamp(0.0, 1.0) * w;
            let half_ang = (f(b"TlAn", 0.35) * 0.5).clamp(0.02, 1.5);
            let yr = y1 - tail_h * h;
            let (rx, ry) = (w * 0.5, (yr - y0) * 0.5);
            let cey = (y0 + yr) * 0.5;
            let t_dir = ((y1 - cey) / ry).atan2((tip_x - cx) / rx);
            let mut a = arc_anchors(cx, cey, rx, ry, t_dir + half_ang, t_dir - half_ang + TAU);
            a.push(Anchor::corner(tip_x, y1));
            Some(("Callout", closed(a)))
        }
        // Tear: an apex over a bulb. The geometry below reproduces the
        // default (Ball 0.25, Curv 0.3, Bend 0, Tail 0.5) exactly —
        // convex sides fitted numerically to Affinity's own render,
        // widest at 51.5% of the height, an elliptical bulb below —
        // and scales the cone with `Tail`; the other parameters warn.
        b"ShTr" => {
            let tail = f(b"Tail", 0.5).clamp(0.05, 0.95);
            if (f(b"Ball", 0.25) - 0.25).abs() > 0.02
                || (f(b"Curv", 0.3) - 0.3).abs() > 0.02
                || f(b"Bend", 0.0).abs() > 0.02
            {
                log::warn!("affinity: tear with non-default ball/curve/bend; shape approximate");
            }
            let ym = y0 + (tail * 1.03).min(0.9) * h;
            let hw = w * 0.5;
            let (dx, dy) = (0.410 * hw, 0.159 * h);
            let v = 0.161 * h;
            let mut a = vec![Anchor {
                point: (cx, y0),
                handle_in: (dx, dy),
                handle_out: (-dx, dy),
            }];
            let mut bottom = arc_anchors(cx, ym, hw, y1 - ym, PI, 0.0);
            if let Some(first) = bottom.first_mut() {
                first.handle_in = (0.0, -v);
            }
            if let Some(last) = bottom.last_mut() {
                last.handle_out = (0.0, -v);
            }
            a.extend(bottom);
            Some(("Tear", closed(a)))
        }
        _ => None,
    }
}

/// A circular arc in unit space from the top tip (0.5, 0) to the
/// bottom tip (0.5, 1) — reversed when `downward` is false — bowing
/// sideways with sagitta `bow`/2 (negative bows left). Nearly-zero bows
/// degenerate to the straight chord.
fn bow_arc_unit(bow: f32, downward: bool) -> Vec<schist_core::Anchor> {
    use schist_core::Anchor;
    use std::f32::consts::PI;
    let s = (bow.abs() * 0.5).min(0.5);
    if s < 0.005 {
        let mut pts = vec![Anchor::corner(0.5, 0.0), Anchor::corner(0.5, 1.0)];
        if !downward {
            pts.reverse();
        }
        return pts;
    }
    let r = (s * s + 0.25) / (2.0 * s);
    // Bulge left: the circle's centre sits right of the chord.
    let cxu = 0.5 + (r - s);
    let phi = (0.5f32).atan2(r - s);
    let (t0, t1) = if downward {
        (PI + phi, PI - phi)
    } else {
        (PI - phi, PI + phi)
    };
    let mut a = arc_anchors(cxu, 0.5, r, r, t0, t1);
    if bow > 0.0 {
        for an in &mut a {
            an.point.0 = 1.0 - an.point.0;
            an.handle_in.0 = -an.handle_in.0;
            an.handle_out.0 = -an.handle_out.0;
        }
    }
    a
}

/// Anchors tracing the elliptical arc `t0`→`t1` (radians in screen
/// space, so y grows downward: point = centre + (rx·cos t, ry·sin t)),
/// split into ≤90° cubic segments. The endpoints carry only their
/// arc-side handle, so a straight edge can follow either end.
fn arc_anchors(cx: f32, cy: f32, rx: f32, ry: f32, t0: f32, t1: f32) -> Vec<schist_core::Anchor> {
    use schist_core::Anchor;
    let n = ((t1 - t0).abs() / std::f32::consts::FRAC_PI_2)
        .ceil()
        .max(1.0) as usize;
    let dt = (t1 - t0) / n as f32;
    let k = 4.0 / 3.0 * (dt / 4.0).tan();
    (0..=n)
        .map(|i| {
            let t = t0 + dt * i as f32;
            let (px, py) = (cx + rx * t.cos(), cy + ry * t.sin());
            let (dx, dy) = (-rx * t.sin() * k, ry * t.cos() * k);
            let mut a = Anchor::corner(px, py);
            if i > 0 {
                a.handle_in = (-dx, -dy);
            }
            if i < n {
                a.handle_out = (dx, dy);
            }
            a
        })
        .collect()
}

/// A corner anchor at unit-circle coordinates (centre 0, radius 1)
/// mapped onto the ellipse inscribed in the box.
fn unit_anchor(ux: f32, uy: f32, x0: f32, y0: f32, x1: f32, y1: f32) -> schist_core::Anchor {
    schist_core::Anchor::corner(
        (x0 + x1) * 0.5 + ux * (x1 - x0) * 0.5,
        (y0 + y1) * 0.5 + uy * (y1 - y0) * 0.5,
    )
}

/// Square star ("ShSS"): `Side` rectangular arms radiating from the
/// centre, the first pointing down. The arms are the middle `COut` of
/// each edge of the regular `Side`-gon whose vertices sit on the
/// inscribed ellipse — flat tips at the polygon's apothem, sides
/// perpendicular to them, adjacent arms meeting in a V notch at `COut`
/// of the radius. (All measured off Affinity's own thumbnail render.)
fn square_star_anchors(
    shpe: &Node,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
) -> Vec<schist_core::Anchor> {
    use std::f32::consts::{FRAC_PI_2, PI, TAU};
    let sides = u16_of(shpe, b"Side").unwrap_or(4).clamp(3, 100) as usize;
    let cut = f32_last(shpe, b"COut").unwrap_or(0.5).clamp(0.01, 0.99);
    let step = TAU / sides as f32;
    let tip = (PI / sides as f32).cos(); // the polygon's apothem
    let hw = cut * (PI / sides as f32).sin(); // arm half-width
    let mut out = Vec::with_capacity(sides * 3);
    for k in 0..sides {
        let ang = FRAC_PI_2 + step * k as f32;
        let (ux, uy) = (ang.cos(), ang.sin());
        let (px, py) = (-uy, ux); // toward the next arm
        out.push(unit_anchor(
            ux * tip - px * hw,
            uy * tip - py * hw,
            x0,
            y0,
            x1,
            y1,
        ));
        out.push(unit_anchor(
            ux * tip + px * hw,
            uy * tip + py * hw,
            x0,
            y0,
            x1,
            y1,
        ));
        let na = ang + step * 0.5;
        out.push(unit_anchor(na.cos() * cut, na.sin() * cut, x0, y0, x1, y1));
    }
    out
}

/// Cloud ("ShCl"): `Bubl` circular arcs bulging outward around the
/// inscribed ellipse, meeting each other at `IRad` of the radius.
fn cloud_anchors(shpe: &Node, x0: f32, y0: f32, x1: f32, y1: f32) -> Vec<schist_core::Anchor> {
    use std::f32::consts::{FRAC_PI_2, TAU};
    let bubbles = u16_of(shpe, b"Bubl").unwrap_or(12).clamp(3, 100) as usize;
    let meet = f32_last(shpe, b"IRad").unwrap_or(0.8).clamp(0.1, 0.999);
    let step = TAU / bubbles as f32;

    // Anchors alternate meet points and bubble peaks; handles come from
    // the exact circular arc through (meet, peak, meet) in unit space.
    let mut out: Vec<schist_core::Anchor> = (0..bubbles * 2)
        .map(|i| {
            let ang = -FRAC_PI_2 + step * 0.5 * i as f32 - step * 0.5;
            let r = if i % 2 == 0 { meet } else { 1.0 };
            schist_core::Anchor::corner(ang.cos() * r, ang.sin() * r)
        })
        .collect();
    let n = out.len();
    for k in 0..bubbles {
        let p0 = out[2 * k].point;
        let p1 = out[2 * k + 1].point;
        let p2 = out[(2 * k + 2) % n].point;
        if let Some((c, r)) = circle_through(p0, p1, p2) {
            let ang = |p: (f32, f32)| (p.1 - c.1).atan2(p.0 - c.0);
            let (a0, mut a1, mut a2) = (ang(p0), ang(p1), ang(p2));
            // Walk monotonically a0 → a1 → a2 the short way round.
            while a1 - a0 > std::f32::consts::PI {
                a1 -= TAU;
            }
            while a1 - a0 < -std::f32::consts::PI {
                a1 += TAU;
            }
            while a2 - a1 > std::f32::consts::PI {
                a2 -= TAU;
            }
            while a2 - a1 < -std::f32::consts::PI {
                a2 += TAU;
            }
            let tangent = |a: f32, k: f32| (-a.sin() * k * r, a.cos() * k * r);
            let k01 = (4.0 / 3.0) * ((a1 - a0) / 4.0).tan();
            let k12 = (4.0 / 3.0) * ((a2 - a1) / 4.0).tan();
            out[2 * k].handle_out = tangent(a0, k01);
            out[2 * k + 1].handle_in = {
                let t = tangent(a1, k01);
                (-t.0, -t.1)
            };
            out[2 * k + 1].handle_out = tangent(a1, k12);
            out[(2 * k + 2) % n].handle_in = {
                let t = tangent(a2, k12);
                (-t.0, -t.1)
            };
        }
    }
    for a in &mut out {
        let mapped = unit_anchor(a.point.0, a.point.1, x0, y0, x1, y1);
        let (sx, sy) = ((x1 - x0) * 0.5, (y1 - y0) * 0.5);
        a.point = mapped.point;
        a.handle_in = (a.handle_in.0 * sx, a.handle_in.1 * sy);
        a.handle_out = (a.handle_out.0 * sx, a.handle_out.1 * sy);
    }
    out
}

/// The circle through three points, unless they are collinear.
fn circle_through(p0: (f32, f32), p1: (f32, f32), p2: (f32, f32)) -> Option<((f32, f32), f32)> {
    let d = 2.0 * (p0.0 * (p1.1 - p2.1) + p1.0 * (p2.1 - p0.1) + p2.0 * (p0.1 - p1.1));
    if d.abs() < 1e-9 {
        return None;
    }
    let sq = |p: (f32, f32)| p.0 * p.0 + p.1 * p.1;
    let cx = (sq(p0) * (p1.1 - p2.1) + sq(p1) * (p2.1 - p0.1) + sq(p2) * (p0.1 - p1.1)) / d;
    let cy = (sq(p0) * (p2.0 - p1.0) + sq(p1) * (p0.0 - p2.0) + sq(p2) * (p1.0 - p0.0)) / d;
    Some(((cx, cy), (p0.0 - cx).hypot(p0.1 - cy)))
}

/// Heart ("ShHt"): two Bezier lobes over a point, the notch between
/// them deepening as `Sprd` grows. Proportions traced from Affinity's
/// own thumbnail render at the default spread of 0.2: lobes touch the
/// top, flanks stay near-vertical to mid-height, sides sweep gently
/// convex into the tip.
fn heart_anchors(x0: f32, y0: f32, x1: f32, y1: f32, spread: f32) -> Vec<schist_core::Anchor> {
    use schist_core::Anchor;
    let (w, h) = (x1 - x0, y1 - y0);
    let p = |ux: f32, uy: f32| (x0 + ux * w, y0 + uy * h);
    let v = |ux: f32, uy: f32| (ux * w, uy * h);
    let notch = (spread * 0.82).clamp(0.0, 0.6);
    let a = |pt: (f32, f32), hin: (f32, f32), hout: (f32, f32)| Anchor {
        point: pt,
        handle_in: hin,
        handle_out: hout,
    };
    vec![
        // Bottom tip; sides sweep out symmetrically.
        a(p(0.5, 1.0), v(0.26, -0.14), v(-0.26, -0.14)),
        // Left flank.
        a(p(0.0, 0.35), v(0.0, 0.25), v(0.0, -0.20)),
        // Left lobe top.
        a(p(0.25, 0.0), v(-0.14, 0.0), v(0.14, 0.0)),
        // Notch.
        a(p(0.5, notch), v(-0.045, -0.10), v(0.045, -0.10)),
        // Right lobe top.
        a(p(0.75, 0.0), v(-0.14, 0.0), v(0.14, 0.0)),
        // Right flank.
        a(p(1.0, 0.35), v(0.0, -0.20), v(0.0, 0.25)),
    ]
}

/// Anchors for a clockwise rounded rectangle with per-corner radii
/// `[top-left, top-right, bottom-right, bottom-left]` (a plain corner
/// at r = 0).
fn rounded_rect_anchors(
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    r: [f32; 4],
) -> Vec<schist_core::Anchor> {
    cornered_rect_anchors(x0, y0, x1, y1, r, [0; 4])
}

/// Rectangle anchors with per-corner treatments (`CTyp`): 0 rounded ·
/// 1 straight chamfer · 2 concave (the arc bends inward, centred on
/// the corner) · 3 cutout (a square bite through the inner point).
fn cornered_rect_anchors(
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    r: [f32; 4],
    types: [u16; 4],
) -> Vec<schist_core::Anchor> {
    use schist_core::Anchor;
    let [tl, tr, br, bl] = r.map(|v| if v < 0.25 { 0.0 } else { v });
    let a = |px, py, ix, iy, ox, oy| Anchor {
        point: (px, py),
        handle_in: (ix, iy),
        handle_out: (ox, oy),
    };
    let mut out = Vec::with_capacity(8);
    // Each corner contributes its arc's entry and exit anchors — or,
    // when square, the corner point itself.
    for (ci, (radius, corner, entry, exit)) in [
        (tl, (x0, y0), (0.0f32, 1.0f32), (1.0f32, 0.0f32)),
        (tr, (x1, y0), (-1.0, 0.0), (0.0, 1.0)),
        (br, (x1, y1), (0.0, -1.0), (-1.0, 0.0)),
        (bl, (x0, y1), (1.0, 0.0), (0.0, -1.0)),
    ]
    .into_iter()
    .enumerate()
    {
        if radius == 0.0 {
            out.push(Anchor::corner(corner.0, corner.1));
            continue;
        }
        let ctype = types[ci];
        let (en, ex) = (
            (corner.0 + entry.0 * radius, corner.1 + entry.1 * radius),
            (corner.0 + exit.0 * radius, corner.1 + exit.1 * radius),
        );
        match ctype {
            1 => {
                // Straight: chamfer between the two radius points.
                out.push(Anchor::corner(en.0, en.1));
                out.push(Anchor::corner(ex.0, ex.1));
                continue;
            }
            2 => {
                // Concave: the arc's centre is the corner itself.
                let k = KAPPA * radius;
                out.push(a(en.0, en.1, 0.0, 0.0, exit.0 * k, exit.1 * k));
                out.push(a(ex.0, ex.1, entry.0 * k, entry.1 * k, 0.0, 0.0));
                continue;
            }
            3 => {
                // Cutout: a square bite via the inner point.
                out.push(Anchor::corner(en.0, en.1));
                out.push(Anchor::corner(
                    corner.0 + (entry.0 + exit.0) * radius,
                    corner.1 + (entry.1 + exit.1) * radius,
                ));
                out.push(Anchor::corner(ex.0, ex.1));
                continue;
            }
            _ => {}
        }
        // The straight edges either side keep zero handles; the arc
        // between the two anchors bends toward the corner point.
        let k = KAPPA * radius;
        out.push(a(
            corner.0 + entry.0 * radius,
            corner.1 + entry.1 * radius,
            0.0,
            0.0,
            -entry.0 * k,
            -entry.1 * k,
        ));
        out.push(a(
            corner.0 + exit.0 * radius,
            corner.1 + exit.1 * radius,
            -exit.0 * k,
            -exit.1 * k,
            0.0,
            0.0,
        ));
    }
    out
}

/// Anchors for a clockwise ellipse filling the box.
fn ellipse_anchors(x0: f32, y0: f32, x1: f32, y1: f32) -> Vec<schist_core::Anchor> {
    use schist_core::Anchor;
    let (cx, cy) = ((x0 + x1) / 2.0, (y0 + y1) / 2.0);
    let (kx, ky) = (KAPPA * (x1 - x0) / 2.0, KAPPA * (y1 - y0) / 2.0);
    vec![
        Anchor::smooth(cx, y0, kx, 0.0),
        Anchor::smooth(x1, cy, 0.0, ky),
        Anchor::smooth(cx, y1, -kx, 0.0),
        Anchor::smooth(x0, cy, 0.0, -ky),
    ]
}

/// Rasterize a vector shape into the layer's tiles: fill (solid or
/// gradient-shaded), then stroke composited over it, blitted once.
fn rasterize_shape(
    layer: &mut Layer,
    shape: &schist_core::VectorShape,
    gradient: Option<&GradientFill>,
) {
    let mut builder = schist_vector::PathBuilder::new();
    for sub in &shape.path.subpaths {
        let Some(first) = sub.anchors.first() else {
            continue;
        };
        builder.move_to(first.point.0, first.point.1);
        let n = sub.anchors.len();
        for i in 1..=n {
            let from = &sub.anchors[i - 1];
            let to = &sub.anchors[i % n];
            if i == n && !sub.closed {
                break;
            }
            builder.cubic_to(
                from.point.0 + from.handle_out.0,
                from.point.1 + from.handle_out.1,
                to.point.0 + to.handle_in.0,
                to.point.1 + to.handle_in.1,
                to.point.0,
                to.point.1,
            );
        }
        if sub.closed {
            builder.close();
        }
    }
    let flat = builder.build(0.25);

    let pad = shape
        .stroke
        .map(|(_, w)| w / 2.0 + 1.0)
        .unwrap_or(1.0)
        .ceil() as i32;
    let mut rect = shape.path.bounds();
    rect.left -= pad;
    rect.top -= pad;
    rect.right += pad;
    rect.bottom += pad;
    let (w, h) = (rect.width() as usize, rect.height() as usize);
    if w == 0 || h == 0 || w * h > (1 << 28) {
        return;
    }

    // Straight-alpha source-over of one coverage pixel.
    fn over(px: &mut [u8], c: [u8; 4], cov: u8) {
        let sa = cov as u32 * c[3] as u32 / 255;
        let da = px[3] as u32;
        let out_a = sa + da * (255 - sa) / 255;
        if out_a == 0 {
            return;
        }
        for i in 0..3 {
            px[i] = ((c[i] as u32 * sa + px[i] as u32 * da * (255 - sa) / 255) / out_a) as u8;
        }
        px[3] = out_a as u8;
    }

    let mut rgba = vec![0u8; w * h * 4];
    fn layer_in(rgba: &mut [u8], cov: Vec<u8>, color: schist_color::Rgba) {
        let c = color.to_u8();
        for (px, &a) in rgba.as_chunks_mut::<4>().0.iter_mut().zip(&cov) {
            if a > 0 {
                over(px, c, a);
            }
        }
    }
    let rule = if shape.even_odd {
        schist_vector::FillRule::EvenOdd
    } else {
        schist_vector::FillRule::NonZero
    };
    if let Some(g) = gradient {
        let cov = schist_vector::rasterize(&flat, rect, rule);
        let (dx, dy) = (g.end.0 - g.start.0, g.end.1 - g.start.1);
        let len2 = (dx * dx + dy * dy).max(1e-9);
        for y in 0..h {
            for x in 0..w {
                let a = cov[y * w + x];
                if a == 0 {
                    continue;
                }
                let px = (rect.left + x as i32) as f64 + 0.5 - g.start.0;
                let py = (rect.top + y as i32) as f64 + 0.5 - g.start.1;
                let t = if g.radial {
                    ((px * px + py * py) / len2).sqrt()
                } else {
                    (px * dx + py * dy) / len2
                };
                over(&mut rgba[(y * w + x) * 4..][..4], g.color_at(t as f32), a);
            }
        }
    } else if shape.fill.a > 0.0 {
        layer_in(
            &mut rgba,
            schist_vector::rasterize(&flat, rect, rule),
            shape.fill,
        );
    }
    if let Some((color, width)) = shape.stroke {
        let stroked = schist_vector::stroke_path(
            &flat,
            schist_vector::StrokeStyle::new(width)
                .with_cap(schist_vector::LineCap::Round)
                .with_join(schist_vector::LineJoin::Round),
        );
        layer_in(
            &mut rgba,
            schist_vector::rasterize(&stroked, rect, schist_vector::FillRule::NonZero),
            color,
        );
    }
    blit_rgba8(
        &mut layer.as_raster_mut().unwrap().tiles,
        Depth::Eight,
        rect,
        &rgba,
    );
}

/// Write a gray coverage buffer (one byte per pixel, rect-sized) into
/// mask tiles at `rect`.
fn blit_mask(tiles: &mut schist_core::MaskTileMap, rect: IntRect, gray: &[u8]) {
    use schist_core::{TileCoord, TILE_SIZE};
    let w = rect.width() as usize;
    for coord in TileCoord::covering(&rect) {
        let trect = coord.rect();
        let clip = trect.intersect(&rect);
        if clip.is_empty() {
            continue;
        }
        let buf = tiles.get_mut_or_insert(coord);
        for y in clip.top..clip.bottom {
            let sy = (y - rect.top) as usize;
            let ly = (y - trect.top) as usize;
            for x in clip.left..clip.right {
                let sx = (x - rect.left) as usize;
                let lx = (x - trect.left) as usize;
                buf[ly * TILE_SIZE as usize + lx] = gray[sy * w + sx];
            }
        }
    }
}

/// Scale an image to the placement rect's size (bilinear). Identity is
/// free; import-time quality matches what a one-off resample costs.
/// Flip an image in place about either axis.
fn mirror(img: &mut RgbaImage, horizontal: bool, vertical: bool) {
    let (w, h) = (img.width as usize, img.height as usize);
    if horizontal {
        for row in img.pixels.chunks_exact_mut(w * 4) {
            let (mut a, mut b) = (0, w - 1);
            while a < b {
                for c in 0..4 {
                    row.swap(a * 4 + c, b * 4 + c);
                }
                a += 1;
                b -= 1;
            }
        }
    }
    if vertical {
        let stride = w * 4;
        let (mut top, mut bottom) = (0, h - 1);
        while top < bottom {
            for i in 0..stride {
                img.pixels.swap(top * stride + i, bottom * stride + i);
            }
            top += 1;
            bottom -= 1;
        }
    }
}

fn resample_to(img: RgbaImage, dw: u32, dh: u32) -> RgbaImage {
    if (img.width, img.height) == (dw, dh) || dw == 0 || dh == 0 {
        return img;
    }
    let (sw, sh) = (img.width as usize, img.height as usize);
    let (dw, dh) = (dw as usize, dh as usize);
    // The horizontal taps are the same for every row; compute them once
    // as byte offsets into a source row.
    let xtaps: Vec<(usize, usize, f32)> = (0..dw)
        .map(|x| {
            let fx = (x as f32 + 0.5) * sw as f32 / dw as f32 - 0.5;
            let x0 = (fx.floor().max(0.0) as usize).min(sw - 1);
            let x1 = (x0 + 1).min(sw - 1);
            let wx = (fx - x0 as f32).clamp(0.0, 1.0);
            (x0 * 4, x1 * 4, wx)
        })
        .collect();
    let mut pixels = vec![0u8; dw * dh * 4];
    pixels
        .par_chunks_exact_mut(dw * 4)
        .enumerate()
        .for_each(|(y, out_row)| {
            let fy = (y as f32 + 0.5) * sh as f32 / dh as f32 - 0.5;
            let y0 = (fy.floor().max(0.0) as usize).min(sh - 1);
            let y1 = (y0 + 1).min(sh - 1);
            let wy = (fy - y0 as f32).clamp(0.0, 1.0);
            let row0 = &img.pixels[y0 * sw * 4..][..sw * 4];
            let row1 = &img.pixels[y1 * sw * 4..][..sw * 4];
            for (out, &(x0, x1, wx)) in out_row.as_chunks_mut::<4>().0.iter_mut().zip(&xtaps) {
                for c in 0..4 {
                    let top = row0[x0 + c] as f32 * (1.0 - wx) + row0[x1 + c] as f32 * wx;
                    let bot = row1[x0 + c] as f32 * (1.0 - wx) + row1[x1 + c] as f32 * wx;
                    out[c] = (top * (1.0 - wy) + bot * wy + 0.5) as u8;
                }
            }
        });
    RgbaImage {
        width: dw as u32,
        height: dh as u32,
        pixels,
    }
}

/// A tile entry is either the bare 64 KiB plane, or (older files) a tiny
/// graph document of type "Data" whose one blob field holds the plane.
fn tile_payload(data: Vec<u8>) -> Option<Vec<u8>> {
    if data.len() == 0x10000 {
        return Some(data);
    }
    let graph = graph::parse(&data).ok()?;
    graph
        .node(graph::ROOT)
        .fields
        .iter()
        .find_map(|(_, v)| match v {
            Value::Blob(b) if b.len() == 0x10000 => Some(b.clone()),
            _ => None,
        })
}

fn fill_tile(plane: &mut [u8], pitch: usize, x: usize, y: usize, pattern: &[u8]) {
    for row in 0..256 {
        let base = (y + row) * pitch + x;
        if base + 256 > plane.len() {
            break;
        }
        for (i, byte) in plane[base..base + 256].iter_mut().enumerate() {
            *byte = pattern[i % pattern.len()];
        }
    }
}

/// D50 Lab → sRGB, matching how Affinity displays Lab documents.
fn lab_to_srgb(l: f32, a: f32, b: f32) -> (f32, f32, f32) {
    let fy = (l + 16.0) / 116.0;
    let fx = fy + a / 500.0;
    let fz = fy - b / 200.0;
    let finv = |t: f32| {
        if t > 6.0 / 29.0 {
            t * t * t
        } else {
            3.0 * (6.0f32 / 29.0).powi(2) * (t - 4.0 / 29.0)
        }
    };
    // D50 white point.
    let (xn, yn, zn) = (0.9642f32, 1.0, 0.8251);
    let (x, y, z) = (xn * finv(fx), yn * finv(fy), zn * finv(fz));
    // XYZ (D50) → linear sRGB (Bradford-adapted matrix).
    let r = 3.133_856 * x - 1.616_867 * y - 0.490_615 * z;
    let g = -0.978_768 * x + 1.916_141 * y + 0.033_454 * z;
    let bl = 0.071_945 * x - 0.228_991 * y + 1.405_243 * z;
    let enc = |c: f32| {
        let c = c.clamp(0.0, 1.0);
        if c <= 0.0031308 {
            12.92 * c
        } else {
            1.055 * c.powf(1.0 / 2.4) - 0.055
        }
    };
    (enc(r), enc(g), enc(bl))
}

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

/// Render the parsed graph as an indented outline — the debugging view
/// used while reverse engineering, kept for `--features`-free forensics.
pub fn dump(bytes: &[u8]) -> Result<String, AffinityError> {
    use std::fmt::Write as _;
    let archive = Archive::parse(bytes)?;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "container v{} class {}",
        archive.version,
        tag_name(archive.class_tag)
    );
    let _ = writeln!(
        out,
        "entries: {}",
        archive.names().collect::<Vec<_>>().join(", ")
    );
    let entry = archive
        .head("doc.dat")
        .ok_or_else(|| malformed("no doc.dat"))?;
    let doc = archive.extract(entry)?;
    let graph = graph::parse(&doc)?;
    dump_node(
        &graph,
        graph::ROOT,
        0,
        &mut out,
        &mut vec![false; graph.nodes.len()],
    );
    Ok(out)
}

fn dump_node(graph: &Graph, index: usize, depth: usize, out: &mut String, seen: &mut Vec<bool>) {
    use std::fmt::Write as _;
    let node = graph.node(index);
    let pad = "  ".repeat(depth);
    let types: Vec<String> = node.types.iter().map(|(t, _)| tag_name(*t)).collect();
    let _ = writeln!(out, "{pad}[{}] id={}", types.join("<"), node.id);
    if seen[index] {
        let _ = writeln!(out, "{pad}  (already shown)");
        return;
    }
    seen[index] = true;
    for (tag, value) in &node.fields {
        let _ = write!(out, "{pad}  {} = ", tag_name(*tag));
        dump_value(graph, value, depth, out, seen);
    }
}

fn dump_value(graph: &Graph, value: &Value, depth: usize, out: &mut String, seen: &mut Vec<bool>) {
    use std::fmt::Write as _;
    match value {
        Value::Class(Some(i)) => {
            let _ = writeln!(out, "class:");
            dump_node(graph, *i, depth + 2, out, seen);
        }
        Value::Class(None) => {
            let _ = writeln!(out, "null");
        }
        Value::Array(items) => {
            let _ = writeln!(out, "array[{}]:", items.len());
            let scalar = !items
                .iter()
                .any(|v| matches!(v, Value::Class(_) | Value::Array(_)));
            if scalar {
                let mut line = String::new();
                for v in items.iter().take(64) {
                    let _ = write!(line, "{v:?} ");
                }
                let _ = writeln!(out, "{}    {}", "  ".repeat(depth), line);
            } else {
                for v in items.iter().take(32) {
                    let _ = write!(out, "{}    - ", "  ".repeat(depth));
                    dump_value(graph, v, depth + 2, out, seen);
                }
                if items.len() > 32 {
                    let _ = writeln!(out, "{}    …", "  ".repeat(depth));
                }
            }
        }
        other => {
            let _ = writeln!(out, "{other:?}");
        }
    }
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
