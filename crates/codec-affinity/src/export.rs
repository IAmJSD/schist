//! Document → .af: build an Affinity file from a
//! [`schist_core::Document`].
//!
//! Strategy: never guess at boilerplate. A real Affinity 3.1 document
//! (the vendored probe fixture) is embedded as a template; its object
//! graph is parsed, the canvas fields are patched, the template's own
//! layers are replaced with freshly built nodes, and the graph is
//! re-serialized by [`crate::emit`] — which reproduces parsed graphs
//! byte-exactly, so everything not deliberately changed is exactly what
//! Affinity itself wrote. Layer nodes are built from field layouts
//! transcribed from real documents (see `docs/affinity-format.md`);
//! adjustments and shapes re-emit the native subtrees the importer
//! preserved (`AFJ1` blocks).
//!
//! What exports today: raster layers (any schist depth, written as
//! RGBA8 tiles), groups (nested, pass-through or isolated), layer
//! masks, clipping layers (as Affinity clipped children), opacity /
//! fill opacity / blend modes / visibility, and adjustment layers that
//! carry a preserved native parameter block. Text and vector layers
//! export their rasterized pixels. Anything dropped lands in
//! [`ExportReport::skipped`].

use crate::archive::Archive;
use crate::container::{write_container, EntryData};
use crate::emit;
use crate::error::{malformed, AffinityError};
use crate::graph::{self, tag, ChainEnd, Graph, Node, Value};
use schist_core::{BlendMode, Document, IntRect, Layer, LayerKind, LayerMask};
use schist_core::{TileCoord, TILE_SIZE};

/// A complete, minimal Affinity 3.1 document; the boilerplate donor.
const TEMPLATE: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../fixtures/affinity-probe/invert.af"
));

/// What could not be expressed in the written file.
#[derive(Debug, Default)]
pub struct ExportReport {
    /// (layer name, why).
    pub skipped: Vec<(String, String)>,
}

/// Serialize `doc` as a version-12 unified Affinity document.
///
/// `thumbnail_png` becomes the file's embedded preview (Affinity shows
/// it in its browser, and it is the fallback other importers read);
/// callers with a compositor should render one.
pub fn write_affinity(
    doc: &Document,
    thumbnail_png: Option<&[u8]>,
) -> Result<(Vec<u8>, ExportReport), AffinityError> {
    let archive = Archive::parse(TEMPLATE)?;
    let entry = archive
        .head("doc.dat")
        .ok_or_else(|| malformed("template has no doc.dat"))?;
    let plain = archive.extract(entry)?;
    let g = graph::parse(&plain)?;

    let next_id = g.nodes.iter().map(|n| n.id).max().unwrap_or(0) + 1;
    let mut ex = Exporter {
        g,
        entries: Vec::new(),
        report: ExportReport::default(),
        next_id,
        canvas: (doc.width, doc.height),
        rng: 0x9E37_79B9_7F4A_7C15 ^ ((doc.width as u64) << 32 | doc.height as u64),
    };
    ex.patch_document(doc)?;

    // Patching breaks the two stream-order invariants Affinity's reader
    // enforces; restore declare-once type chains and 0,1,2… object ids.
    emit::normalize_declarations(&mut ex.g);
    emit::renumber_ids(&mut ex.g);
    let doc_dat = emit::serialize(&ex.g)?;
    let mut entries = vec![EntryData {
        name: "doc.dat".into(),
        plain: doc_dat,
    }];
    entries.append(&mut ex.entries);

    let created = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Ok((write_container(&entries, thumbnail_png, created), ex.report))
}

struct Exporter {
    g: Graph,
    /// Tile entries accumulated while building bitmaps ("d/1"…).
    entries: Vec<EntryData>,
    report: ExportReport,
    next_id: u32,
    canvas: (u32, u32),
    rng: u64,
}

/// A field under construction: (tag, wire type, aux, value).
type Field = (u32, u8, u64, Value);

fn f(name: &[u8; 4], wire: u8, value: Value) -> Field {
    (tag(name), wire, 0, value)
}

fn f_aux(name: &[u8; 4], wire: u8, aux: u64, value: Value) -> Field {
    (tag(name), wire, aux, value)
}

impl Exporter {
    // ------------------------------------------------------------------
    // Graph plumbing

    fn next_id(&mut self) -> u32 {
        self.next_id += 1;
        self.next_id - 1
    }

    /// splitmix64 — deterministic per-document ids for GooP fields.
    fn rand(&mut self) -> u64 {
        self.rng = self.rng.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.rng;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn goop(&mut self) -> Value {
        let (a, b) = (self.rand(), self.rand());
        Value::VecI(vec![a as i32, (a >> 32) as i32, b as i32, (b >> 32) as i32])
    }

    /// A fresh 0x31-framed node: type chain sections (all field-less,
    /// the way Affinity writes layer nodes), lone root tag, flat fields.
    fn push_node(&mut self, chain: &[(&[u8; 4], u32)], fields: Vec<Field>) -> usize {
        let id = self.next_id();
        self.push_node_framed(0x31, chain, ChainEnd::LoneTag, id, fields)
    }

    /// A 0x31 node whose chain ends with flag 2 instead of a lone tag
    /// (single-type helper classes: DyBm, Blck).
    fn push_node_closed(&mut self, chain: &[(&[u8; 4], u32)], fields: Vec<Field>) -> usize {
        let id = self.next_id();
        self.push_node_framed(0x31, chain, ChainEnd::Closed, id, fields)
    }

    /// A 0x32-framed helper (Matx, Per*, Quad): single type, no id.
    fn push_tagged(&mut self, ty: (&[u8; 4], u32), fields: Vec<Field>) -> usize {
        self.push_node_framed(0x32, &[ty], ChainEnd::None, 0, fields)
    }

    fn push_node_framed(
        &mut self,
        framing: u8,
        chain: &[(&[u8; 4], u32)],
        chain_end: ChainEnd,
        id: u32,
        fields: Vec<Field>,
    ) -> usize {
        let types: Vec<(u32, u32)> = chain.iter().map(|(t, v)| (tag(t), *v)).collect();
        let section_lens = match (framing, chain_end) {
            // Every section before the lone tag is empty.
            (0x31, ChainEnd::LoneTag) => vec![0; types.len().saturating_sub(1)],
            (0x31, ChainEnd::Closed) => vec![0; types.len()],
            _ => Vec::new(),
        };
        let mut node = Node {
            types,
            id,
            framing,
            section_lens,
            chain_end,
            ..Node::default()
        };
        for (t, wire, aux, value) in fields {
            node.fields.push((t, value));
            node.wire.push(wire);
            node.aux.push(aux);
        }
        self.g.nodes.push(node);
        self.g.nodes.len() - 1
    }

    fn class(idx: usize) -> Value {
        Value::Class(Some(idx))
    }

    fn find_child(&self, node: usize, name: &[u8; 4]) -> Option<usize> {
        match self.g.nodes[node].field(name)? {
            Value::Class(Some(i)) => Some(*i),
            _ => None,
        }
    }

    /// Replace an existing field's value in place.
    fn set_field(&mut self, node: usize, name: &[u8; 4], value: Value) {
        let t = tag(name);
        if let Some(at) = self.g.nodes[node]
            .fields
            .iter()
            .position(|(ft, _)| *ft == t)
        {
            self.g.nodes[node].fields[at].1 = value;
        }
    }

    // ------------------------------------------------------------------
    // Document skeleton

    fn patch_document(&mut self, doc: &Document) -> Result<(), AffinityError> {
        let (w, h) = (doc.width as f64, doc.height as f64);
        let doc_r = self
            .find_child(graph::ROOT, b"DocR")
            .ok_or_else(|| malformed("template has no DocR"))?;
        self.set_field(doc_r, b"DfSz", Value::VecD(vec![w, h]));

        let sprd = match self.g.nodes[doc_r].field(b"Chld") {
            Some(Value::Array(items)) => match items.first() {
                Some(Value::Class(Some(i))) => *i,
                _ => return Err(malformed("template has no spread")),
            },
            _ => return Err(malformed("template has no spread")),
        };

        // A fresh spread identity.
        let mi_id = format!(
            "{:08X}-{:04X}-{:04X}-{:04X}-{:012X}",
            self.rand() as u32,
            self.rand() as u16,
            self.rand() as u16,
            self.rand() as u16,
            self.rand() & 0xFFFF_FFFF_FFFF
        );
        self.set_field(sprd, b"MiID", Value::Str(mi_id));

        // Affinity sizes the opened canvas from the spread's page
        // geometry, not DfSz: the slice persona's spread rect and the
        // spread-metadata page rects. Left at the template's values the
        // document opens as a 512×512 square.
        if let Some(slcp) = self.find_child(sprd, b"SlcP") {
            self.set_field(slcp, b"SRct", Value::VecI(vec![0, 0, w as i32, h as i32]));
        }
        if let Some(spmd) = self.find_child(sprd, b"SpMd") {
            let pages: Vec<usize> = match self.g.nodes[spmd].field(b"PagR") {
                Some(Value::Array(items)) => items
                    .iter()
                    .filter_map(|v| match v {
                        Value::Class(Some(i)) => Some(*i),
                        _ => None,
                    })
                    .collect(),
                _ => Vec::new(),
            };
            for page in pages {
                self.set_field(page, b"rctp", Value::VecD(vec![0.0, 0.0, w, h]));
            }
        }

        // The spread's base rasters are evicted composite caches; they
        // just need the right dimensions.
        for name in [b"RasS", b"Ras2"] {
            if let Some(srst) = self.find_child(sprd, name) {
                self.set_field(srst, b"BitI", Value::VecI(vec![0, 0, w as i32, h as i32]));
                let bitm = self.build_evicted_bitmap(doc.width, doc.height);
                self.set_field(srst, b"Bitm", Self::class(bitm));
            }
        }

        // The layer stack.
        let layers = self.build_stack(&doc.tree.layers, (0.0, 0.0));
        self.set_field(sprd, b"Chld", Value::Array(layers));

        // The template's selection points into its replaced layer stack;
        // real writers store an empty Itms for "nothing selected".
        if let Some(csel) = self.find_child(graph::ROOT, b"CSel") {
            self.set_field(csel, b"Itms", Value::Array(Vec::new()));
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Layers

    /// Build a sibling stack (bottom-to-top), folding clipping layers
    /// into their base layer's child list the way Affinity nests them.
    fn build_stack(&mut self, layers: &[Layer], origin: (f64, f64)) -> Vec<Value> {
        let mut out = Vec::new();
        let mut at = 0usize;
        while at < layers.len() {
            let base = &layers[at];
            let mut clips: Vec<&Layer> = Vec::new();
            while at + 1 < layers.len() && layers[at + 1].clipping {
                clips.push(&layers[at + 1]);
                at += 1;
            }
            if let Some(idx) = self.build_layer(base, &clips, origin) {
                out.push(Self::class(idx));
            }
            at += 1;
        }
        out
    }

    fn build_layer(
        &mut self,
        layer: &Layer,
        clips: &[&Layer],
        origin: (f64, f64),
    ) -> Option<usize> {
        match &layer.kind {
            LayerKind::Group(group) => Some(self.group_node(layer, &group.children, origin)),
            LayerKind::Adjustment(_) => self.adjustment_node(layer, clips, origin),
            _ => self.raster_node(layer, clips, origin),
        }
    }

    /// The field block every layer node starts with, transcribed from
    /// real 3.1 documents. `xfrm` places the node in its parent's space.
    fn common_fields(
        &mut self,
        layer: &Layer,
        xfrm: Option<[f64; 6]>,
        pass_through: Option<bool>,
    ) -> Vec<Field> {
        let zeros4 = || Value::VecD(vec![0.0; 4]);
        let enum0 = || Value::Enum { id: 0, version: 0 };
        let goop = self.goop();
        let mut out = vec![
            f(b"TrCn", 0x07, Value::I32(18)),
            f(b"TrAn", 0x2a, enum0()),
            f(b"TrFP", 0x24, Value::VecD(vec![0.0, 0.0])),
            f(b"TrFV", 0x29, Value::Bool(false)),
        ];
        if let Some((id, version)) = blend_enum(layer.blend) {
            if layer.blend != BlendMode::Normal && layer.blend != BlendMode::PassThrough {
                out.push(f(b"Blnd", 0x2a, Value::Enum { id, version }));
            }
        } else {
            self.report.skipped.push((
                layer.name.clone(),
                format!("blend mode {:?} has no Affinity equivalent", layer.blend),
            ));
        }
        if let Some(m) = xfrm {
            if m != [1.0, 0.0, 0.0, 0.0, 1.0, 0.0] {
                out.push(f(b"Xfrm", 0x28, Value::VecD(m.to_vec())));
            }
        }
        out.push(f(b"SrBx", 0x26, zeros4()));
        out.push(f(b"SrPB", 0x26, zeros4()));
        if pass_through == Some(false) {
            out.push(f(b"PasT", 0x29, Value::Bool(false)));
        }
        out.extend([
            f(b"Desc", 0x2b, Value::Str(layer.name.clone())),
            f(b"TagC", 0x31, Value::Class(None)),
            f_aux(b"Visi", 0x29, 1, Value::Bool(layer.visible)),
            f(b"Opac", 0x09, Value::F32(layer.opacity.clamp(0.0, 1.0))),
            f(
                b"FOpc",
                0x09,
                Value::F32(layer.fill_opacity.clamp(0.0, 1.0)),
            ),
        ]);
        let effects = self.effect_nodes(layer);
        out.extend([
            f(b"FiEf", 0xb1, Value::Array(effects)),
            f_aux(b"Edtb", 0x29, 1, Value::Bool(true)),
            f_aux(b"MEtb", 0x29, 1, Value::Bool(true)),
            f(b"Data", 0x31, Value::Class(None)),
            f(b"AncD", 0x31, Value::Class(None)),
            f(b"GooP", 0x17, goop),
            f(b"deco", 0x29, Value::Bool(false)),
            f(b"Frst", 0xab, Value::Array(Vec::new())),
            f(b"Scnd", 0xab, Value::Array(Vec::new())),
            f(b"TWSt", 0x2a, enum0()),
            f(b"TWBo", 0x26, zeros4()),
            f(b"CLiT", 0x2a, enum0()),
            f(b"avin", 0x03, Value::U32(1_073_741_823)),
        ]);
        out
    }

    /// Layer effects as `FilE`-derived nodes, the inverse of the
    /// import's `apply_effects` mapping. Only enabled effects are
    /// written; effects with no Affinity equivalent are reported.
    fn effect_nodes(&mut self, layer: &Layer) -> Vec<Value> {
        let style = &layer.style;
        let mut out = Vec::new();

        let shadow = |ex: &mut Self, tag: &[u8; 4], s: &schist_core::style::ShadowStyle| {
            let colr = ex.rgba_node(s.color);
            let (bid, bver) = blend_enum(s.blend).unwrap_or((2, 0));
            let idx = ex.push_node(
                &[(tag, 1), (b"FilE", 0)],
                vec![
                    f_aux(b"Enab", 0x29, 1, Value::Bool(true)),
                    f(
                        b"BlnM",
                        0x2a,
                        Value::Enum {
                            id: bid,
                            version: bver,
                        },
                    ),
                    f(b"Opac", 0x0a, Value::F64(s.opacity as f64)),
                    f(b"SclO", 0x29, Value::Bool(false)),
                    f(b"Radi", 0x0a, Value::F64(s.size as f64)),
                    f(b"Offs", 0x0a, Value::F64(s.distance as f64)),
                    // Ours is where the light comes from; Affinity
                    // stores the offset direction itself.
                    f(
                        b"Angl",
                        0x0a,
                        Value::F64(((180.0 - s.angle) as f64).to_radians()),
                    ),
                    // The panel's Intensity slider, stored inverted.
                    f(
                        b"Comp",
                        0x0a,
                        Value::F64(1.0 - s.spread.clamp(0.0, 1.0) as f64),
                    ),
                    f_aux(b"Knck", 0x29, 1, Value::Bool(s.knockout)),
                    f(b"Colr", 0x31, Self::class(colr)),
                ],
            );
            Self::class(idx)
        };
        if style.drop_shadow.enabled {
            out.push(shadow(self, b"Shad", &style.drop_shadow.settings));
        }
        if style.inner_shadow.enabled {
            // "InnS", not the "InSh" an earlier reading assumed.
            out.push(shadow(self, b"InnS", &style.inner_shadow.settings));
        }

        let glow = |ex: &mut Self, tag: &[u8; 4], s: &schist_core::style::GlowStyle| {
            let colr = ex.rgba_node(s.color);
            let (bid, bver) = blend_enum(s.blend).unwrap_or((5, 0));
            let idx = ex.push_node(
                &[(tag, 1), (b"FilE", 0)],
                vec![
                    f_aux(b"Enab", 0x29, 1, Value::Bool(true)),
                    f(
                        b"BlnM",
                        0x2a,
                        Value::Enum {
                            id: bid,
                            version: bver,
                        },
                    ),
                    f(b"Opac", 0x0a, Value::F64(s.opacity as f64)),
                    f(b"SclO", 0x29, Value::Bool(false)),
                    f(b"Radi", 0x0a, Value::F64(s.size as f64)),
                    f(
                        b"Comp",
                        0x0a,
                        Value::F64(1.0 - s.spread.clamp(0.0, 1.0) as f64),
                    ),
                    f(b"Colr", 0x31, Self::class(colr)),
                ],
            );
            Self::class(idx)
        };
        if style.outer_glow.enabled {
            out.push(glow(self, b"OutG", &style.outer_glow.settings));
        }
        if style.inner_glow.enabled {
            out.push(glow(self, b"InnG", &style.inner_glow.settings));
        }

        if style.color_overlay.enabled {
            let s = &style.color_overlay.settings;
            let colr = self.rgba_node(s.color);
            let (bid, bver) = blend_enum(s.blend).unwrap_or((0, 0));
            let idx = self.push_node(
                &[(b"ColO", 1), (b"FilE", 0)],
                vec![
                    f_aux(b"Enab", 0x29, 1, Value::Bool(true)),
                    f(
                        b"BlnM",
                        0x2a,
                        Value::Enum {
                            id: bid,
                            version: bver,
                        },
                    ),
                    f(b"Opac", 0x0a, Value::F64(s.opacity as f64)),
                    f(b"SclO", 0x29, Value::Bool(false)),
                    f(b"Colr", 0x31, Self::class(colr)),
                ],
            );
            out.push(Self::class(idx));
        }

        if style.stroke.enabled {
            let s = &style.stroke.settings;
            let colr = self.rgba_node(s.color);
            let (bid, bver) = blend_enum(s.blend).unwrap_or((0, 0));
            // Probed one fixture per setting: 0 outside, 1 centre,
            // 2 inside.
            let align = match s.position {
                schist_core::style::StrokePosition::Outside => 0,
                schist_core::style::StrokePosition::Center => 1,
                schist_core::style::StrokePosition::Inside => 2,
            };
            let idx = self.push_node(
                &[(b"Strk", 1), (b"FilE", 0)],
                vec![
                    f_aux(b"Enab", 0x29, 1, Value::Bool(true)),
                    f(
                        b"BlnM",
                        0x2a,
                        Value::Enum {
                            id: bid,
                            version: bver,
                        },
                    ),
                    f(b"Opac", 0x0a, Value::F64(s.opacity as f64)),
                    f(b"SclO", 0x29, Value::Bool(false)),
                    f(b"Radi", 0x0a, Value::F64(s.size as f64)),
                    f(
                        b"Alig",
                        0x2a,
                        Value::Enum {
                            id: align,
                            version: 0,
                        },
                    ),
                    f(b"Ftyp", 0x2a, Value::Enum { id: 0, version: 0 }),
                    f(b"Colr", 0x31, Self::class(colr)),
                    f(b"GrFl", 0x31, Value::Class(None)),
                ],
            );
            out.push(Self::class(idx));
        }

        for (enabled, what) in [
            (style.bevel.enabled, "bevel"),
            (style.satin.enabled, "satin"),
            (style.gradient_overlay.enabled, "gradient overlay"),
        ] {
            if enabled {
                self.report.skipped.push((
                    layer.name.clone(),
                    format!("{what} effect has no verified Affinity mapping"),
                ));
            }
        }
        out
    }

    /// An RGBA colour class: four little-endian f32s in a `_col` struct.
    fn rgba_node(&mut self, c: schist_color::Rgba) -> usize {
        let mut bytes = Vec::with_capacity(16);
        for v in [c.r, c.g, c.b, c.a] {
            bytes.extend_from_slice(&v.clamp(0.0, 1.0).to_le_bytes());
        }
        self.push_node_closed(
            &[(b"RGBA", 1)],
            vec![f(b"_col", 0x44, Value::Struct(bytes))],
        )
    }

    /// The perspective/rotation tail real raster-backed nodes carry.
    fn raster_tail_fields(&mut self, ext_e: bool) -> Vec<Field> {
        let matx = self.push_tagged(
            (b"Matx", 1),
            vec![
                f(b"Rows", 0x07, Value::I32(3)),
                f(b"Cols", 0x07, Value::I32(3)),
                f(
                    b"Data",
                    0x8a,
                    Value::Array(
                        [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0]
                            .iter()
                            .map(|v| Value::F64(*v))
                            .collect(),
                    ),
                ),
            ],
        );
        let quad = self.push_tagged(
            (b"Quad", 1),
            [
                (b"X0  ", 0.0),
                (b"X1  ", 1.0),
                (b"X2  ", 1.0),
                (b"X3  ", 0.0),
                (b"Y0  ", 0.0),
                (b"Y1  ", 0.0),
                (b"Y2  ", 1.0),
                (b"Y3  ", 1.0),
            ]
            .iter()
            .map(|(t, v)| f(t, 0x0a, Value::F64(*v)))
            .collect(),
        );
        let pers = self.push_tagged(
            (b"Per*", 1),
            vec![
                f_aux(
                    b"Pers",
                    0xb2,
                    ((tag(b"Quad") as u64) << 16) | 1,
                    Value::Array(vec![Self::class(quad)]),
                ),
                f(b"Curr", 0x07, Value::I32(0)),
            ],
        );
        vec![
            f(b"CMsk", 0x07, Value::I32(31)),
            f(b"ProT", 0x2a, Value::Enum { id: 0, version: 0 }),
            f(b"Unpr", 0x31, Value::Class(None)),
            f(b"WRot", 0x32, Self::class(matx)),
            f(b"Lati", 0x0a, Value::F64(90.0)),
            f(b"Long", 0x0a, Value::F64(90.0)),
            f(b"Roll", 0x0a, Value::F64(0.0)),
            f(b"FOV ", 0x0a, Value::F64(75.0)),
            f(b"Psp*", 0x32, Self::class(pers)),
            f_aux(b"ExtE", 0x29, u64::from(ext_e), Value::Bool(ext_e)),
        ]
    }

    fn group_node(&mut self, layer: &Layer, children: &[Layer], origin: (f64, f64)) -> usize {
        let pass = layer.blend == BlendMode::PassThrough;
        let child_values = self.build_stack(children, origin);
        let mut fields = self.common_fields(layer, None, Some(pass).filter(|p| !p));
        fields.push(self.mask_field(layer, origin));
        fields.push(f(b"Chld", 0xb1, Value::Array(child_values)));
        fields.push(f(b"ComO", 0x2a, Value::Enum { id: 0, version: 0 }));
        let fields = fields.into_iter().filter(non_empty_mask_slot).collect();
        self.push_node(&[(b"Grup", 1), (b"LogN", 0)], fields)
    }

    fn raster_node(
        &mut self,
        layer: &Layer,
        clips: &[&Layer],
        origin: (f64, f64),
    ) -> Option<usize> {
        let raster = layer.as_raster()?;
        if layer.shape.is_some() {
            self.report.skipped.push((
                layer.name.clone(),
                "live vector shape written as pixels (stays visually intact)".into(),
            ));
        } else if layer.extras.iter().any(|b| &b.key == b"PsTx") {
            self.report.skipped.push((
                layer.name.clone(),
                "text written as pixels (stays visually intact)".into(),
            ));
        }
        let mut bounds = raster.tiles.content_bounds();
        if bounds.is_empty() {
            // A pixel-less layer still keeps its slot in the stack.
            bounds = IntRect::new(0, 0, 1, 1);
        }
        let (w, h) = (bounds.width() as u32, bounds.height() as u32);
        let planes = rgba_planes(&raster.tiles, &bounds);
        let bitm = self.build_bitmap(0, w, h, &planes, true);

        let xfrm = [
            1.0,
            0.0,
            bounds.left as f64 - origin.0,
            0.0,
            1.0,
            bounds.top as f64 - origin.1,
        ];
        let mut fields = self.common_fields(layer, Some(xfrm), None);
        let child_origin = (bounds.left as f64, bounds.top as f64);
        if !clips.is_empty() {
            // Clipped children live in this layer's coordinate space.
            let clip_values = self.build_stack_refs(clips, child_origin);
            fields.push(f(b"Chld", 0xb1, Value::Array(clip_values)));
        }
        fields.push(self.mask_field(layer, child_origin));
        fields.push(f(b"Bitm", 0x31, Self::class(bitm)));
        fields.push(f(
            b"BitR",
            0x17,
            Value::VecI(vec![0, 0, w as i32, h as i32]),
        ));
        fields.push(f(
            b"BitI",
            0x17,
            Value::VecI(vec![0, 0, w as i32, h as i32]),
        ));
        fields.extend(self.raster_tail_fields(true));
        let fields = fields.into_iter().filter(non_empty_mask_slot).collect();
        Some(self.push_node(&[(b"Rstr", 1), (b"Node", 0)], fields))
    }

    fn build_stack_refs(&mut self, layers: &[&Layer], origin: (f64, f64)) -> Vec<Value> {
        // Clipping chains never nest further clipping chains.
        layers
            .iter()
            .filter_map(|l| self.build_layer(l, &[], origin))
            .map(Self::class)
            .collect()
    }

    fn adjustment_node(
        &mut self,
        layer: &Layer,
        _clips: &[&Layer],
        origin: (f64, f64),
    ) -> Option<usize> {
        let LayerKind::Adjustment(data) = &layer.kind else {
            return None;
        };
        let mut next = {
            let mut n = self.next_id;
            move || {
                n += 1;
                n - 1
            }
        };
        let decoded = crate::preserve::decode(&data.raw, &mut self.g, &mut next);
        self.next_id = self
            .g
            .nodes
            .iter()
            .map(|n| n.id)
            .max()
            .unwrap_or(self.next_id)
            + 1;
        // Invert is the one adjustment with no parameter class at all —
        // a bare node is the complete representation.
        let param_free = matches!(data.kind, schist_core::AdjustmentKind::Invert);
        if decoded.is_none() && !param_free {
            self.report.skipped.push((
                layer.name.clone(),
                "adjustment carries no native Affinity parameters".into(),
            ));
            return None;
        }

        // The layer's own class chain: preserved when the block recorded
        // it with versions, else the standard adjustment chain.
        let preserved_chain = decoded
            .as_ref()
            .map(|d| d.layer_types.clone())
            .unwrap_or_default();
        let chain: Vec<(u32, u32)> = if preserved_chain.len() > 1 {
            preserved_chain
        } else {
            let kind_tag = preserved_chain
                .first()
                .map(|(t, _)| *t)
                .unwrap_or_else(|| tag(b"InRA"));
            vec![
                (kind_tag, 1),
                (tag(b"AdjR"), 0),
                (tag(b"EncR"), 0),
                (tag(b"Rstr"), 1),
                (tag(b"Node"), 0),
            ]
        };

        let (cw, ch) = self.canvas;
        let coverage = self.build_coverage_bitmap(cw, ch);
        let mut fields = Vec::new();
        if let Some(decoded) = &decoded {
            let param_wire = match self.g.nodes[decoded.root].framing {
                0x31 => 0x31,
                _ => 0x32,
            };
            fields.push((decoded.key, param_wire, 0u64, Self::class(decoded.root)));
        }
        fields.extend(self.common_fields(layer, None, None));
        fields.push(self.mask_field(layer, origin));
        fields.push(f(b"Bitm", 0x31, Self::class(coverage)));
        fields.push(f(
            b"BitR",
            0x17,
            Value::VecI(vec![0, 0, cw as i32, ch as i32]),
        ));
        fields.push(f(b"BitI", 0x17, Value::VecI(vec![i32::MIN + 1; 4])));
        fields.extend(self.raster_tail_fields(false));
        let fields = fields.into_iter().filter(non_empty_mask_slot).collect();

        let chain_named: Vec<([u8; 4], u32)> =
            chain.iter().map(|(t, v)| (t.to_be_bytes(), *v)).collect();
        let chain_refs: Vec<(&[u8; 4], u32)> = chain_named.iter().map(|(t, v)| (t, *v)).collect();
        Some(self.push_node(&chain_refs, fields))
    }

    // ------------------------------------------------------------------
    // Masks

    /// The AdCh field holding this layer's mask, or a sentinel skipped
    /// by `non_empty_mask_slot` when there is none.
    fn mask_field(&mut self, layer: &Layer, layer_origin: (f64, f64)) -> Field {
        let Some(mask) = &layer.mask else {
            return f(b"AdCh", 0x00, Value::Array(Vec::new()));
        };
        let idx = self.mask_node(mask, layer_origin);
        f(b"AdCh", 0xb1, Value::Array(vec![Self::class(idx)]))
    }

    fn mask_node(&mut self, mask: &LayerMask, layer_origin: (f64, f64)) -> usize {
        // Outside its bitmap a mask reveals (255). A hiding default
        // must cover the whole canvas to mean the same thing.
        let mut rect = mask.bounds;
        if mask.default_value == 0 {
            let (w, h) = self.canvas;
            rect = rect.union(&IntRect::new(0, 0, w as i32, h as i32));
        }
        if rect.is_empty() {
            rect = IntRect::new(0, 0, 1, 1);
        }
        let (w, h) = (rect.width() as u32, rect.height() as u32);
        let plane = mask_plane(mask, &rect);
        let bitm = self.build_bitmap(6, w, h, &[plane], false);

        let xfrm = [
            1.0,
            0.0,
            rect.left as f64 - layer_origin.0,
            0.0,
            1.0,
            rect.top as f64 - layer_origin.1,
        ];
        let mask_layer = Layer {
            visible: mask.enabled,
            ..Layer::new_raster("")
        };
        let mut fields = vec![f(b"ComO", 0x2a, Value::Enum { id: 0, version: 0 })];
        fields.extend(self.common_fields(&mask_layer, Some(xfrm), None));
        fields.push(f(b"Bitm", 0x31, Self::class(bitm)));
        fields.push(f(
            b"BitR",
            0x17,
            Value::VecI(vec![0, 0, w as i32, h as i32]),
        ));
        fields.push(f(
            b"BitI",
            0x17,
            Value::VecI(vec![0, 0, w as i32, h as i32]),
        ));
        fields.extend(self.raster_tail_fields(false));
        self.push_node(
            &[(b"MRst", 1), (b"EncR", 0), (b"Rstr", 1), (b"Node", 0)],
            fields,
        )
    }

    // ------------------------------------------------------------------
    // Bitmaps and tiles

    /// An evicted single-channel bitmap: dimensions and an all-empty
    /// status grid, no pixel data — the shape Affinity leaves spread
    /// composite caches in.
    fn build_evicted_bitmap(&mut self, w: u32, h: u32) -> usize {
        let (tw, th) = (w.div_ceil(256) as usize, h.div_ceil(256) as usize);
        let mut fields = bitmap_header_fields(6, w, h);
        fields.push(f(b"TWi1", 0x07, Value::I32(tw as i32)));
        fields.push(f(b"THi1", 0x07, Value::I32(th as i32)));
        fields.push(f(b"Idx1", 0xb1, Value::Array(Vec::new())));
        fields.push(f(b"Sta1", 0x81, Value::Array(vec![Value::U8(0); tw * th])));
        self.push_node_closed(&[(b"DyBm", 1)], fields)
    }

    /// A full-coverage single-channel bitmap (adjustment extent): every
    /// tile is a solid 0xFF fill, no data entries.
    fn build_coverage_bitmap(&mut self, w: u32, h: u32) -> usize {
        let (tw, th) = (w.div_ceil(256) as usize, h.div_ceil(256) as usize);
        let mut fields = bitmap_header_fields(6, w, h);
        fields.push(f(b"TWi1", 0x07, Value::I32(tw as i32)));
        fields.push(f(b"THi1", 0x07, Value::I32(th as i32)));
        fields.push(f(b"Idx1", 0xb1, Value::Array(Vec::new())));
        fields.push(f(b"Sta1", 0x81, Value::Array(vec![Value::U8(2); tw * th])));
        self.push_node_closed(&[(b"DyBm", 1)], fields)
    }

    /// A pixel bitmap from 8-bit channel planes (each `pitch × rows`,
    /// pitch = ceil(w/256)*256). `format` 0 = RGBA8 (4 planes),
    /// 6 = single channel.
    fn build_bitmap(
        &mut self,
        format: u16,
        w: u32,
        h: u32,
        planes: &[Vec<u8>],
        mips: bool,
    ) -> usize {
        let mut fields = bitmap_header_fields(format, w, h);
        let (tw, th) = (w.div_ceil(256) as usize, h.div_ceil(256) as usize);
        for (c, plane) in planes.iter().enumerate() {
            let (idx, sta) = self.channel_tiles(plane, w, h);
            let d = b'1' + c as u8;
            fields.push(f(&[b'T', b'W', b'i', d], 0x07, Value::I32(tw as i32)));
            fields.push(f(&[b'T', b'H', b'i', d], 0x07, Value::I32(th as i32)));
            fields.push(f(&[b'I', b'd', b'x', d], 0xb1, Value::Array(idx)));
            fields.push(f(&[b'S', b't', b'a', d], 0x81, Value::Array(sta)));
        }
        if mips {
            let mut level_planes: Vec<Vec<u8>> = planes.to_vec();
            let (mut lw, mut lh) = (w, h);
            let mut level = 1u8;
            while lw.max(lh) > 256 && level <= 4 {
                let (nw, nh) = (lw.div_ceil(2).max(1), lh.div_ceil(2).max(1));
                level_planes = level_planes
                    .iter()
                    .map(|p| downsample_plane(p, lw, lh, nw, nh))
                    .collect();
                let (mw, mh) = (nw.div_ceil(256) as usize, nh.div_ceil(256) as usize);
                for (c, plane) in level_planes.iter().enumerate() {
                    let (idx, sta) = self.channel_tiles(plane, nw, nh);
                    let d = b'1' + c as u8;
                    fields.push(f(&[b'M', b'W', level, d], 0x07, Value::I32(mw as i32)));
                    fields.push(f(&[b'M', b'H', level, d], 0x07, Value::I32(mh as i32)));
                    fields.push(f(&[b'M', b'I', level, d], 0xb1, Value::Array(idx)));
                    fields.push(f(&[b'M', b'T', level, d], 0x81, Value::Array(sta)));
                }
                (lw, lh) = (nw, nh);
                level += 1;
            }
        }
        self.push_node_closed(&[(b"DyBm", 1)], fields)
    }

    /// Classify one channel's tiles: empty (1), solid 0xFF fill (2), or
    /// stored (4, with a container entry and a `Blck` node).
    fn channel_tiles(&mut self, plane: &[u8], w: u32, h: u32) -> (Vec<Value>, Vec<Value>) {
        let (tw, th) = (w.div_ceil(256) as usize, h.div_ceil(256) as usize);
        let pitch = tw * 256;
        let mut statuses = Vec::with_capacity(tw * th);
        let mut blocks = Vec::new();
        for ty in 0..th {
            for tx in 0..tw {
                let (x0, y0) = (tx * 256, ty * 256);
                let valid_w = (w as usize - x0).min(256);
                let valid_h = (h as usize - y0).min(256);
                let mut all0 = true;
                let mut all255 = true;
                let mut tile = vec![0u8; 0x10000];
                for row in 0..256usize {
                    let src = (y0 + row) * pitch + x0;
                    let dst = &mut tile[row * 256..row * 256 + 256];
                    dst.copy_from_slice(&plane[src..src + 256]);
                    if row < valid_h {
                        for &b in &dst[..valid_w] {
                            all0 &= b == 0;
                            all255 &= b == 0xFF;
                        }
                    }
                }
                if all0 {
                    statuses.push(Value::U8(1));
                } else if all255 {
                    statuses.push(Value::U8(2));
                } else {
                    statuses.push(Value::U8(4));
                    let name = format!("d/{}", self.entries.len() + 1);
                    self.entries.push(EntryData {
                        name: name.clone(),
                        plain: tile,
                    });
                    let mut block_fields = Vec::new();
                    if valid_w < 256 || valid_h < 256 {
                        block_fields.push(f(
                            b"Rect",
                            0x17,
                            Value::VecI(vec![0, 0, valid_w as i32, valid_h as i32]),
                        ));
                    }
                    block_fields.push(f(
                        b"Data",
                        0x33,
                        Value::Embedded {
                            tag: tag(b"DatI"),
                            name,
                        },
                    ));
                    let idx = self.push_node_closed(&[(b"Blck", 1)], block_fields);
                    blocks.push(Self::class(idx));
                }
            }
        }
        (blocks, statuses)
    }
}

/// Filter for the mask-field sentinel (wire 0 = no mask).
fn non_empty_mask_slot(field: &Field) -> bool {
    field.1 != 0x00
}

fn bitmap_header_fields(format: u16, w: u32, h: u32) -> Vec<Field> {
    vec![
        f(
            b"Frmt",
            0x2a,
            Value::Enum {
                id: format,
                version: 0,
            },
        ),
        f(b"BmpW", 0x07, Value::I32(w as i32)),
        f(b"BmpH", 0x07, Value::I32(h as i32)),
        f_aux(b"DelA", 0x29, 1, Value::Bool(true)),
        f(b"MipM", 0x2a, Value::Enum { id: 4, version: 0 }),
        f(b"LInf", 0x07, Value::I32(0)),
        f(b"TInf", 0x07, Value::I32(0)),
    ]
}

/// Extract straight-alpha 8-bit channel planes (R, G, B, A) for `rect`
/// from a schist tile map. Plane pitch is the 256-byte tile grid.
fn rgba_planes(tiles: &schist_core::TileMap, rect: &IntRect) -> Vec<Vec<u8>> {
    let (w, h) = (rect.width() as usize, rect.height() as usize);
    let tw = w.div_ceil(256);
    let pitch = tw * 256;
    let rows = h.div_ceil(256) * 256;
    let mut planes = vec![vec![0u8; pitch * rows]; 4];

    for coord in TileCoord::covering(rect) {
        let Some(buf) = tiles.get(coord) else {
            continue;
        };
        let trect = coord.rect();
        let clip = trect.intersect(rect);
        if clip.is_empty() {
            continue;
        }
        for y in clip.top..clip.bottom {
            let ly = (y - trect.top) as usize;
            let dst_row = (y - rect.top) as usize * pitch;
            match &**buf {
                schist_core::TileBuf::U8(d) => {
                    for x in clip.left..clip.right {
                        let s = (ly * TILE_SIZE as usize + (x - trect.left) as usize) * 4;
                        let dst = dst_row + (x - rect.left) as usize;
                        for (c, plane) in planes.iter_mut().enumerate() {
                            plane[dst] = d[s + c];
                        }
                    }
                }
                schist_core::TileBuf::U16(d) => {
                    for x in clip.left..clip.right {
                        let s = (ly * TILE_SIZE as usize + (x - trect.left) as usize) * 4;
                        let dst = dst_row + (x - rect.left) as usize;
                        for (c, plane) in planes.iter_mut().enumerate() {
                            plane[dst] = (d[s + c] >> 8) as u8;
                        }
                    }
                }
                schist_core::TileBuf::F32(d) => {
                    for x in clip.left..clip.right {
                        let s = (ly * TILE_SIZE as usize + (x - trect.left) as usize) * 4;
                        let dst = dst_row + (x - rect.left) as usize;
                        for (c, plane) in planes.iter_mut().enumerate() {
                            plane[dst] = (d[s + c].clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
                        }
                    }
                }
            }
        }
    }
    planes
}

/// One 8-bit plane for a mask over `rect`, honouring its default value
/// outside stored tiles.
fn mask_plane(mask: &LayerMask, rect: &IntRect) -> Vec<u8> {
    let (w, h) = (rect.width() as usize, rect.height() as usize);
    let tw = w.div_ceil(256);
    let pitch = tw * 256;
    let rows = h.div_ceil(256) * 256;
    let mut plane = vec![0u8; pitch * rows];
    // Fill the in-bounds region with the default first.
    for y in 0..h {
        plane[y * pitch..y * pitch + w].fill(mask.default_value);
    }
    for (coord, buf) in mask.tiles.iter() {
        let trect = coord.rect();
        let clip = trect.intersect(rect);
        if clip.is_empty() {
            continue;
        }
        for y in clip.top..clip.bottom {
            let ly = (y - trect.top) as usize;
            let dst_row = (y - rect.top) as usize * pitch;
            for x in clip.left..clip.right {
                plane[dst_row + (x - rect.left) as usize] =
                    buf[ly * TILE_SIZE as usize + (x - trect.left) as usize];
            }
        }
    }
    plane
}

/// 2×2 box downsample of a channel plane (tile-grid pitched in and out).
fn downsample_plane(plane: &[u8], w: u32, h: u32, nw: u32, nh: u32) -> Vec<u8> {
    let src_pitch = (w.div_ceil(256) * 256) as usize;
    let dst_pitch = (nw.div_ceil(256) * 256) as usize;
    let dst_rows = (nh.div_ceil(256) * 256) as usize;
    let mut out = vec![0u8; dst_pitch * dst_rows];
    for y in 0..nh as usize {
        for x in 0..nw as usize {
            let (sx, sy) = (x * 2, y * 2);
            let mut acc = 0u32;
            let mut n = 0u32;
            for dy in 0..2usize {
                for dx in 0..2usize {
                    if sx + dx < w as usize && sy + dy < h as usize {
                        acc += plane[(sy + dy) * src_pitch + sx + dx] as u32;
                        n += 1;
                    }
                }
            }
            out[y * dst_pitch + x] = (acc / n.max(1)) as u8;
        }
    }
    out
}

/// schist blend mode → Affinity's (id, version) pair — the inverse of
/// the import table read from `layer_mode.afdesign`.
fn blend_enum(mode: BlendMode) -> Option<(u16, u16)> {
    use BlendMode::*;
    Some(match mode {
        PassThrough | Normal => (0, 0),
        Darken => (1, 0),
        Multiply => (2, 0),
        DarkerColor => (2, 1),
        ColorBurn => (3, 0),
        Lighten => (4, 0),
        Screen => (5, 0),
        ColorDodge => (6, 0),
        LighterColor => (6, 1),
        LinearDodge => (7, 0),
        Overlay => (8, 0),
        SoftLight => (9, 0),
        HardLight => (10, 0),
        VividLight => (11, 0),
        PinLight => (12, 0),
        HardMix => (13, 0),
        Difference => (14, 0),
        Exclusion => (15, 0),
        LinearLight => (15, 1),
        Subtract => (16, 0),
        Hue => (17, 0),
        Saturation => (18, 0),
        Color => (20, 0),
        Luminosity => (19, 0),
        Dissolve | LinearBurn | Divide => return None,
    })
}
