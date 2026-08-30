//! Walking the layer tree: a node's transform, its clipping stack,
//! and the layer it becomes.

use super::*;

impl Walker<'_> {
    /// This node's transform composed onto the current transform.
    pub(super) fn node_ctm(&self, node: &Node) -> Mat {
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
    pub(super) fn layer_stack(&mut self, node: &Node) -> Vec<Layer> {
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
    pub(super) fn layer_stacks_par(&self, nodes: &[&Node], ctm: Mat) -> (Vec<Layer>, ImportReport) {
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

    pub(super) fn layer(&mut self, node: &Node) -> Option<Layer> {
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
                    data.raw = crate::preserve::preserved_block(self.graph, &node.types, key, adj);
                    break;
                }
            }
        }

        self.apply_mask(node, &mut layer);
        self.report_live_filters(node, &layer);
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
}
