//! Vector layers: live shapes, free paths, and the fill/stroke
//! lookup they share.

use super::*;

impl Walker<'_> {
    /// Rebuild a shape layer as a live vector layer.
    ///
    /// Geometry comes from the `Shpe` class over the layer's `ShpB`
    /// local bounds, built in local space and pushed through the full
    /// layer transform — so rotated and sheared shapes import exactly.
    /// Kinds whose geometry we can't rebuild are reported, not guessed.
    pub(super) fn shape_layer(&mut self, node: &Node, name: &str) -> Option<Layer> {
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
        let preserved = crate::preserve::preserved_block(graph, &node.types, b"Shpe", shpe);
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
    pub(super) fn path_layer(&mut self, node: &Node, name: &str) -> Option<Layer> {
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
    pub(super) fn vector_layer(
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
}
