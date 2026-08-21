//! Interpret a parsed Affinity graph as a Schist document.
//!
//! The document graph is `Pers` (persona) → `DocR` (document node) →
//! `Chld` (spreads) → recursive layer tree. Layers carry `Desc` (name),
//! `Visi`, `Opac`, `Blnd`, and a 2×3 `Xfrm`. Raster layers ("Rstr")
//! carry a `Bitm` ("DyBm") whose channels are planar grids of 256-byte ×
//! 256-row tiles: a status byte per tile (0/1 empty, 2 all-0xFF, 3
//! all-1.0f, 4 stored) and, for stored tiles, a `Blck` naming the
//! archive entry with the 64 KiB tile plane plus its valid sub-rect.
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

fn enum_of(node: &Node, name: &[u8; 4]) -> Option<u16> {
    match node.field(name)? {
        Value::Enum { id, .. } => Some(*id),
        _ => None,
    }
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
        ctm: [1.0, 1.0, -org_x, -org_y],
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

    for child in graph.children(spread, b"Chld") {
        if let Some(layer) = walker.layer(child) {
            doc.push_layer(layer);
        }
    }

    doc.damage_all();
    doc.dirty = false;
    Ok((doc, report))
}

struct Walker<'a> {
    archive: &'a Archive<'a>,
    graph: &'a Graph,
    report: &'a mut ImportReport,
    /// Current transform: (sx, sy, tx, ty), the composition of every
    /// ancestor group's transform plus the canvas origin. Rotation and
    /// shear are dropped (with a warning) — axis-aligned import.
    ctm: [f64; 4],
}

/// Compose an axis-aligned CTM with a node's 2×3 row-major transform.
fn compose(ctm: [f64; 4], xf: [f64; 6]) -> [f64; 4] {
    [
        ctm[0] * xf[0],
        ctm[1] * xf[4],
        ctm[0] * xf[2] + ctm[2],
        ctm[1] * xf[5] + ctm[3],
    ]
}

impl Walker<'_> {
    /// This node's transform composed onto the current transform.
    fn node_ctm(&self, node: &Node, what: &str) -> [f64; 4] {
        match f64s(node, b"Xfrm").and_then(|v| v.first_chunk::<6>().copied()) {
            Some(xf) => {
                if xf[1] != 0.0 || xf[3] != 0.0 {
                    log::warn!("affinity: {what} is rotated/sheared; importing axis-aligned");
                }
                compose(self.ctm, xf)
            }
            None => self.ctm,
        }
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
                let saved = self.ctm;
                self.ctm = self.node_ctm(node, "group");
                let children: Vec<Layer> = self
                    .graph
                    .children(node, b"Chld")
                    .into_iter()
                    .filter_map(|c| self.layer(c))
                    .collect();
                self.ctm = saved;
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
            // Curves adjustment layers map onto our own curves
            // adjustment; the compositor applies them non-destructively.
            b"CrRA" => match self.curves_adjustment(node, &display) {
                Some(layer) => layer,
                None => {
                    self.report.skipped.push((skip_label, tag_name(kind)));
                    return None;
                }
            },
            // A live filter node warps the content below it between two
            // quads. When source and destination coincide the filter is
            // configured but inert — absence, not content.
            b"FlRN" if self.filter_is_identity(node) => return None,
            _ => {
                // No pixels exist in the file for other live layer
                // kinds — only their parameters. Record the gap.
                self.report.skipped.push((skip_label, tag_name(kind)));
                return None;
            }
        };

        self.apply_mask(node, &mut layer);
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
    /// Supported geometry: rectangles with per-corner rounding ("ShNR" /
    /// "ShRR", radii in `ShCR` as fractions of half the shorter side
    /// unless `AbSz`) and ellipses ("ShCE"), over the layer's `ShpB`
    /// bounds through its transform. Fill from `BFFl`, stroke from
    /// `LIFl`/`LILn`. Anything fancier (stars, hearts, polygons) is
    /// reported instead of guessed.
    fn shape_layer(&mut self, node: &Node, name: &str) -> Option<Layer> {
        let graph = self.graph;
        let shpe = graph.child(node, b"Shpe")?;
        let b = f64s(node, b"ShpB").filter(|b| b.len() == 4)?;
        let c = self.node_ctm(node, name);
        let (sx, sy) = (c[0].abs(), c[1].abs());
        let x0 = (b[0] * c[0] + c[2]) as f32;
        let y0 = (b[1] * c[1] + c[3]) as f32;
        let x1 = (b[2] * c[0] + c[2]) as f32;
        let y1 = (b[3] * c[1] + c[3]) as f32;
        let (x0, x1) = (x0.min(x1), x0.max(x1));
        let (y0, y1) = (y0.min(y1), y0.max(y1));
        let (w, h) = (x1 - x0, y1 - y0);
        if w < 0.5 || h < 0.5 || w > 1e6 || h > 1e6 {
            return None;
        }

        let square =
            matches!(shpe.field(b"ShCR"), Some(Value::VecF(r)) if r.iter().all(|v| *v == 0.0));
        let kind_name = match &shpe.type_tag().to_be_bytes() {
            b"ShNR" | b"ShRR" if square => "Rectangle",
            b"ShNR" | b"ShRR" => "Rounded Rectangle",
            b"ShCE" | b"ShpE" => "Ellipse",
            _ => "Shape",
        };
        let name = if name.is_empty() { kind_name } else { name };
        let anchors = match &shpe.type_tag().to_be_bytes() {
            b"ShNR" | b"ShRR" => {
                let mut radius = match shpe.field(b"ShCR") {
                    Some(Value::VecF(r)) if !r.is_empty() => r[0],
                    _ => 0.0,
                };
                if !matches!(shpe.field(b"AbSz"), Some(Value::Bool(true))) {
                    radius *= w.min(h) * 0.5;
                } else {
                    radius *= ((sx + sy) / 2.0) as f32;
                }
                rounded_rect_anchors(x0, y0, x1, y1, radius.clamp(0.0, w.min(h) * 0.5))
            }
            b"ShCE" | b"ShpE" => ellipse_anchors(x0, y0, x1, y1),
            _ => return None,
        };

        let mut path = schist_core::VectorPath::new(name);
        path.subpaths.push(schist_core::SubPath {
            anchors,
            closed: true,
        });
        self.vector_layer(node, name, path, false)
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

        let c = self.node_ctm(node, name);
        let to_doc = |x: f64, y: f64| ((x * c[0] + c[2]) as f32, (y * c[1] + c[3]) as f32);

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
        self.vector_layer(node, name, path, true)
    }

    /// Shared tail for vector layers: fill/stroke lookup, live shape,
    /// rasterization.
    fn vector_layer(
        &mut self,
        node: &Node,
        name: &str,
        path: schist_core::VectorPath,
        even_odd: bool,
    ) -> Option<Layer> {
        use schist_color::Rgba;
        let graph = self.graph;
        let c = self.node_ctm(node, name);
        let fill_desc = graph.children(node, b"BFFl");
        let fill = fill_desc.first().and_then(|f| descriptor_color(graph, f));
        let mut gradient = match fill {
            Some(_) => None,
            None => fill_desc
                .first()
                .and_then(|f| descriptor_gradient(graph, f)),
        };
        // Gradient axis into document space.
        if let Some(g) = &mut gradient {
            let map = |p: (f64, f64)| (p.0 * c[0] + c[2], p.1 * c[1] + c[3]);
            g.start = map(g.start);
            g.end = map(g.end);
        }
        let stroke_color = graph
            .children(node, b"LIFl")
            .first()
            .and_then(|f| descriptor_color(graph, f));
        let stroke_width = graph
            .children(node, b"LILn")
            .first()
            .and_then(|l| graph.child(l, b"LDeL"))
            .and_then(|s| match s.field(b"Wght") {
                Some(Value::F64(v)) => Some((*v * (c[0].abs() + c[1].abs()) / 2.0) as f32),
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

    /// Attach the layer's mask, if it carries one: a "MRst" (mask
    /// raster) node in the `AdCh` list, holding a single-channel bitmap
    /// where white reveals. It becomes a real, editable [`LayerMask`].
    fn apply_mask(&mut self, node: &Node, layer: &mut Layer) {
        let masks: Vec<&Node> = self
            .graph
            .children(node, b"AdCh")
            .into_iter()
            .filter(|n| n.types.iter().any(|(t, _)| *t == graph::tag(b"MRst")))
            .collect();
        let Some(&mask_node) = masks.first() else {
            return;
        };
        if masks.len() > 1 {
            log::warn!(
                "affinity: layer {:?} has {} masks; applying the first",
                layer.name,
                masks.len()
            );
        }
        let Some(bitm) = self.graph.child(mask_node, b"Bitm") else {
            return;
        };
        let gray = match self.decode_bitmap(bitm) {
            Ok(g) => g,
            Err(e) => {
                log::warn!("affinity: mask of {:?}: {e}", layer.name);
                self.report
                    .skipped
                    .push((format!("{} (mask)", layer.name), "MRst".to_string()));
                return;
            }
        };
        let rect = self.placement(
            mask_node,
            &format!("{} (mask)", layer.name),
            gray.width,
            gray.height,
        );
        let gray = resample_to(gray, rect.width() as u32, rect.height() as u32);
        if rect.is_empty()
            || gray.pixels.len() != rect.width() as usize * rect.height() as usize * 4
        {
            return;
        }

        let mut mask = schist_core::LayerMask::new_revealing();
        mask.bounds = rect;
        mask.enabled = bool_of(mask_node, b"Visi").unwrap_or(true);
        blit_mask(&mut mask.tiles, rect, &gray);
        layer.mask = Some(mask);
        self.report.masks += 1;
    }

    /// Rebuild a text layer by re-setting its type.
    ///
    /// The graph stores the string (`StSt` story → `Blok` → `Glyp`
    /// `Utf8`), the first run's font size (`Doub[0]`) and resolved font
    /// (`RFnt`/`DFnt` PostScript + family names), the fill colour (run
    /// `Objs` → `FDsc.FDeF` → `Colr`), and the frame box `FrmB` whose
    /// transformed bottom edge is the first baseline. The rendered
    /// layer carries the same `PsTx` extras block the type tool writes,
    /// so imported text stays editable.
    fn text_layer(&mut self, node: &Node, name: &str) -> Option<Layer> {
        let graph = self.graph;
        let story = graph.child(node, b"StSt")?;
        let blocks = graph.children(story, b"Blok");
        let text = blocks
            .iter()
            .filter_map(|b| graph.child(b, b"Glyp"))
            .filter_map(|g| str_of(g, b"Utf8"))
            .map(|s| s.trim_end_matches('\0'))
            .collect::<Vec<_>>()
            .join("\n");
        if text.trim().is_empty() {
            return None;
        }

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
        let font = graph
            .child(run_item, b"RFnt")
            .or_else(|| graph.child(run_item, b"DFnt"));
        let (family, post) = font
            .map(|f| {
                (
                    str_of(f, b"Famy").unwrap_or_default().to_string(),
                    str_of(f, b"Post").unwrap_or_default().to_string(),
                )
            })
            .unwrap_or_default();
        let color = run_color(graph, run_item).unwrap_or([0, 0, 0, 255]);

        // Placement: the frame box, through the layer transform. Its
        // bottom edge is the first baseline (the box spans the visual
        // cap height), which is exactly what the rasterizer's layout
        // origin means.
        let frame = graph.child(node, b"TxtH")?;
        let frmb = f64s(frame, b"FrmB").filter(|b| b.len() == 4)?;
        let c = self.node_ctm(node, name);
        let frame_left = (frmb[0] * c[0] + c[2]).round() as i32;
        let frame_top = (frmb[1] * c[1] + c[3]).round() as i32;
        let frame_width = ((frmb[2] - frmb[0]) * c[0].abs()).round() as i32;
        let eff_size = (size * c[1].abs()) as f32;
        if !(0.5..=10_000.0).contains(&eff_size) {
            return None;
        }

        let mut spec = schist_text_engine::TextSpec {
            text,
            family,
            bold: post.contains("Bold"),
            italic: post.contains("Italic") || post.contains("Oblique"),
            size: eff_size,
            align: schist_text_engine::Align::Left,
            line_height: 1.0,
            tracking: 0.0,
            wrap_width: None,
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
        // The frame box records exactly how wide Affinity set this text.
        // When our face's metrics disagree (usually a substituted font),
        // scale the size so the line still fills its box.
        if frame_width > 8 && !spec.text.contains('\n') {
            let ratio = frame_width as f32 / raster.bounds.width() as f32;
            if (ratio - 1.0).abs() > 0.02 {
                spec.size *= ratio;
                raster = schist_text_engine::rasterize(&spec)?;
                if raster.is_empty() {
                    return None;
                }
            }
        }

        // Anchor our ink box to the frame box: ink-to-ink placement is
        // immune to differing ascent conventions between engines.
        let origin = (
            frame_left - raster.bounds.left,
            frame_top - raster.bounds.top,
        );
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
        blit_rgba8(
            &mut layer.as_raster_mut().unwrap().tiles,
            Depth::Eight,
            bounds,
            &rgba,
        );

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

        let mut rect = self.placement(node, name, rgba.width, rgba.height);
        if rect.is_empty() {
            rect = IntRect::from_size(rgba.width, rgba.height);
        }
        let rgba = resample_to(rgba, rect.width() as u32, rect.height() as u32);
        if rgba.pixels.len() != rect.width() as usize * rect.height() as usize * 4 {
            return None;
        }

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

    /// Where a bitmap lands on the canvas.
    ///
    /// Preference order: the pixel rect `BitR` when set (Photo writes an
    /// `i32::MIN+1` sentinel when it isn't); else the layer transform —
    /// row-major 2×3, `[sx, kx, tx, ky, sy, ty]` — applied to the bitmap
    /// rect, dropping rotation/shear; else the origin at natural size.
    fn placement(&self, node: &Node, name: &str, w: u32, h: u32) -> IntRect {
        if let Some(Value::VecI(r)) = node.field(b"BitR") {
            if let [x0, y0, x1, y1] = r[..] {
                let sane = |v: i32| v.abs() < 1 << 24;
                if x1 > x0 && y1 > y0 && sane(x0) && sane(y0) && sane(x1) && sane(y1) {
                    let c = self.ctm;
                    let ax = (x0 as f64 * c[0] + c[2]).round() as i32;
                    let ay = (y0 as f64 * c[1] + c[3]).round() as i32;
                    let bx = (x1 as f64 * c[0] + c[2]).round() as i32;
                    let by = (y1 as f64 * c[1] + c[3]).round() as i32;
                    let rect = IntRect::new(ax.min(bx), ay.min(by), ax.max(bx), ay.max(by));
                    if !rect.is_empty() {
                        return rect;
                    }
                }
            }
        }
        let c = self.node_ctm(node, name);
        let (dw, dh) = (
            (w as f64 * c[0].abs()).round(),
            (h as f64 * c[1].abs()).round(),
        );
        if dw >= 1.0 && dh >= 1.0 && dw < (1 << 24) as f64 && dh < (1 << 24) as f64 {
            return IntRect::from_xywh(
                c[2].round() as i32,
                c[3].round() as i32,
                dw as u32,
                dh as u32,
            );
        }
        IntRect::from_size(w, h)
    }

    fn decode_bitmap(&self, bitm: &Node) -> Result<RgbaImage, AffinityError> {
        decode_bitmap(self.archive, self.graph, bitm)
    }
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
    // pull from the original file, carried in the Bckg entry.
    let source = if all_statuses(bitm).contains(&5) {
        Some(source_image(archive, bitm, width, height)?)
    } else {
        None
    };

    let sta_names: [&[u8; 4]; 5] = [b"Sta1", b"Sta2", b"Sta3", b"Sta4", b"Sta5"];
    let idx_names: [&[u8; 4]; 5] = [b"Idx1", b"Idx2", b"Idx3", b"Idx4", b"Idx5"];
    let mut planes = Vec::with_capacity(fmt.channels);
    for channel in 0..fmt.channels {
        planes.push(load_plane(PlaneJob {
            archive,
            graph,
            bitm,
            sta: sta_names[channel],
            idx: idx_names[channel],
            pitch,
            rows,
            row_bytes,
            height,
            bytes_per_sample: fmt.bytes_per_sample,
            source: source.as_ref().map(|s| (s, channel)),
        })?);
    }

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
    for y in 0..height {
        for x in 0..width {
            let out = &mut pixels[(y * width + x) * 4..][..4];
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
    }
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
    pitch: usize,
    rows: usize,
    row_bytes: usize,
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
    let (mut x, mut y) = (0usize, 0usize);
    for &status in &statuses {
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
                let entry = job
                    .archive
                    .head(name)
                    .ok_or_else(|| malformed(format!("missing tile entry {name:?}")))?;
                let tile = tile_payload(job.archive.extract(entry)?)
                    .ok_or_else(|| malformed(format!("tile {name:?} has no 64 KiB payload")))?;
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
        x += 256;
        if x >= job.row_bytes.max(1) {
            x = 0;
            y += 256;
            if y >= job.height {
                break;
            }
        }
    }
    Ok(plane)
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

/// The colour behind a fill descriptor (`FDsc.FDeF`): solid fills give
/// their colour; "none" fills and gradients give nothing.
fn descriptor_color(graph: &Graph, fdsc: &Node) -> Option<[u8; 4]> {
    let fill = graph.child(fdsc, b"FDeF")?;
    let colr = graph.child(fill, b"Colr")?;
    color_bytes(colr)
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

fn descriptor_gradient(graph: &Graph, fdsc: &Node) -> Option<GradientFill> {
    let fill = graph.child(fdsc, b"FDeF")?;
    if &fill.type_tag().to_be_bytes() != b"FilG" {
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
    let m: [f64; 6] = match fdsc.field(b"FDeX").or_else(|| fill.field(b"FDeX")) {
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

/// Assemble a subpath from PCrv records: cubic segments as
/// (control₁, control₂, endpoint), each starting at the previous
/// endpoint. A closed path begins mid-cycle — its first point is the
/// stream's final endpoint.
fn subpath_from_records(
    records: &[(f32, f32, u8, u8)],
    closed: bool,
) -> Option<schist_core::SubPath> {
    use schist_core::Anchor;
    // Split into (endpoint, incoming c1, incoming c2) per segment.
    let mut points: Vec<(f32, f32)> = Vec::new();
    let mut c1s: Vec<Option<(f32, f32)>> = Vec::new();
    let mut c2s: Vec<Option<(f32, f32)>> = Vec::new();
    let (mut c1, mut c2) = (None, None);
    for &(x, y, m0, m1) in records {
        match (m0, m1) {
            (1, 0) => c1 = Some((x, y)),
            (0, 1) => c2 = Some((x, y)),
            _ => {
                points.push((x, y));
                c1s.push(c1.take());
                c2s.push(c2.take());
            }
        }
    }
    // Trailing controls belong to the segment closing back to the first
    // recorded endpoint (closed paths start mid-cycle).
    if closed && (c1.is_some() || c2.is_some()) && !points.is_empty() {
        points.rotate_right(1);
        c1s.rotate_right(1);
        c2s.rotate_right(1);
        c1s[0] = c1;
        c2s[0] = c2;
    }
    if points.len() < 2 {
        return None;
    }

    let n = points.len();
    let anchors = (0..n)
        .map(|i| {
            let p = points[i];
            // Incoming control₂ of segment i; outgoing control₁ of the
            // next segment.
            let hin = c2s[i].map(|c| (c.0 - p.0, c.1 - p.1)).unwrap_or((0.0, 0.0));
            let hout = c1s[(i + 1) % n]
                .filter(|_| closed || i + 1 < n)
                .map(|c| (c.0 - p.0, c.1 - p.1))
                .unwrap_or((0.0, 0.0));
            Anchor {
                point: p,
                handle_in: hin,
                handle_out: hout,
            }
        })
        .collect();
    Some(schist_core::SubPath { anchors, closed })
}

/// Circle-to-Bezier handle length as a fraction of the radius.
const KAPPA: f32 = 0.552_284_8;

/// Anchors for a clockwise rounded rectangle (plain corners at r = 0).
fn rounded_rect_anchors(x0: f32, y0: f32, x1: f32, y1: f32, r: f32) -> Vec<schist_core::Anchor> {
    use schist_core::Anchor;
    if r < 0.25 {
        return vec![
            Anchor::corner(x0, y0),
            Anchor::corner(x1, y0),
            Anchor::corner(x1, y1),
            Anchor::corner(x0, y1),
        ];
    }
    let k = KAPPA * r;
    let a = |px, py, ix, iy, ox, oy| Anchor {
        point: (px, py),
        handle_in: (ix, iy),
        handle_out: (ox, oy),
    };
    vec![
        a(x0 + r, y0, -k, 0.0, 0.0, 0.0),
        a(x1 - r, y0, 0.0, 0.0, k, 0.0),
        a(x1, y0 + r, 0.0, -k, 0.0, 0.0),
        a(x1, y1 - r, 0.0, 0.0, 0.0, k),
        a(x1 - r, y1, k, 0.0, 0.0, 0.0),
        a(x0 + r, y1, 0.0, 0.0, -k, 0.0),
        a(x0, y1 - r, 0.0, k, 0.0, 0.0),
        a(x0, y0 + r, 0.0, 0.0, 0.0, -k),
    ]
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

/// Write a decoded mask image (channel 0) into mask tiles at `rect`.
fn blit_mask(tiles: &mut schist_core::MaskTileMap, rect: IntRect, gray: &RgbaImage) {
    use schist_core::{TileCoord, TILE_SIZE};
    let w = gray.width as usize;
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
                buf[ly * TILE_SIZE as usize + lx] = gray.pixels[(sy * w + sx) * 4];
            }
        }
    }
}

/// Scale an image to the placement rect's size (bilinear). Identity is
/// free; import-time quality matches what a one-off resample costs.
fn resample_to(img: RgbaImage, dw: u32, dh: u32) -> RgbaImage {
    if (img.width, img.height) == (dw, dh) || dw == 0 || dh == 0 {
        return img;
    }
    let (sw, sh) = (img.width as usize, img.height as usize);
    let mut pixels = vec![0u8; dw as usize * dh as usize * 4];
    for y in 0..dh as usize {
        let fy = (y as f32 + 0.5) * sh as f32 / dh as f32 - 0.5;
        let y0 = (fy.floor().max(0.0) as usize).min(sh - 1);
        let y1 = (y0 + 1).min(sh - 1);
        let wy = (fy - y0 as f32).clamp(0.0, 1.0);
        for x in 0..dw as usize {
            let fx = (x as f32 + 0.5) * sw as f32 / dw as f32 - 0.5;
            let x0 = (fx.floor().max(0.0) as usize).min(sw - 1);
            let x1 = (x0 + 1).min(sw - 1);
            let wx = (fx - x0 as f32).clamp(0.0, 1.0);
            let at = |px: usize, py: usize, c: usize| img.pixels[(py * sw + px) * 4 + c] as f32;
            let out = &mut pixels[(y * dw as usize + x) * 4..][..4];
            for (c, slot) in out.iter_mut().enumerate() {
                let top = at(x0, y0, c) * (1.0 - wx) + at(x1, y0, c) * wx;
                let bot = at(x0, y1, c) * (1.0 - wx) + at(x1, y1, c) * wx;
                *slot = (top * (1.0 - wy) + bot * wy + 0.5) as u8;
            }
        }
    }
    RgbaImage {
        width: dw,
        height: dh,
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
