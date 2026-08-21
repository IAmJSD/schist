//! Interpret a parsed Affinity graph as a Photoslop document.
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
use photoslop_color::Depth;
use photoslop_core::{blit_rgba8, Document, IntRect, Layer};

/// What a structural import managed to recover.
#[derive(Debug, Default, Clone)]
pub struct ImportReport {
    /// Raster layers whose pixels were fully recovered.
    pub raster_layers: usize,
    /// Groups recovered (structure only, no pixels of their own).
    pub groups: usize,
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

    // Canvas: the spread bounds, [x0, y0, x1, y1] in document points
    // (pixels for raster documents).
    let bounds = f64s(spread, b"SprB").ok_or_else(|| malformed("spread has no bounds"))?;
    if bounds.len() != 4 {
        return Err(malformed("spread bounds are not a rect"));
    }
    let (org_x, org_y) = (bounds[0], bounds[1]);
    let width = (bounds[2] - bounds[0]).round().max(0.0) as u32;
    let height = (bounds[3] - bounds[1]).round().max(0.0) as u32;
    if width == 0 || height == 0 || width > 1 << 20 || height > 1 << 20 {
        return Err(malformed(format!("implausible canvas {width}×{height}")));
    }

    let mut doc = Document::new("Affinity import", width, height, Depth::Eight);
    let mut report = ImportReport::default();
    let mut walker = Walker {
        archive,
        graph,
        report: &mut report,
        origin: (org_x, org_y),
    };

    if spreads.len() > 1 {
        log::warn!(
            "affinity: importing first of {} spreads/artboards",
            spreads.len()
        );
    }

    // A Photo-style spread stores its base pixels in a raster-spread
    // node; import it as the bottom layer when present.
    if let Some(ras) = graph.child(spread, b"RasS") {
        if let Some(layer) = walker.raster_layer(ras, "Background") {
            doc.push_layer(layer);
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
    /// Canvas origin: layer transforms are in spread space.
    origin: (f64, f64),
}

impl Walker<'_> {
    fn layer(&mut self, node: &Node) -> Option<Layer> {
        let kind = node.type_tag();
        let name = str_of(node, b"Desc").unwrap_or_default().to_string();
        let display = if name.is_empty() {
            tag_name(kind)
        } else {
            name.clone()
        };

        let mut layer = match &kind.to_be_bytes() {
            // "Grup" is a group in both apps; "Scop" is Designer's layer
            // container (every layers-panel "Layer" wraps its content in
            // one). Both are groups to us.
            b"Grup" | b"Scop" => {
                self.report.groups += 1;
                let mut group = Layer::new_group(display);
                let children: Vec<Layer> = self
                    .graph
                    .children(node, b"Chld")
                    .into_iter()
                    .filter_map(|c| self.layer(c))
                    .collect();
                if let photoslop_core::LayerKind::Group(g) = &mut group.kind {
                    g.children = children;
                }
                group
            }
            b"Rstr" => self.raster_layer(node, &display)?,
            _ => {
                // No pixels exist in the file for live shapes, text or
                // adjustments — only their parameters. Record the gap.
                self.report.skipped.push((display, tag_name(kind)));
                return None;
            }
        };

        if let Some(v) = bool_of(node, b"Visi") {
            layer.visible = v;
        }
        if let Some(o) = f32_of(node, b"Opac") {
            layer.opacity = o.clamp(0.0, 1.0);
        }
        // Groups pass through by default; PasT=false switches a group to
        // isolated (Normal) compositing. An explicit Blnd overrides both.
        if layer.is_group() && bool_of(node, b"PasT") == Some(false) {
            layer.blend = photoslop_core::BlendMode::Normal;
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

        // Placement: the layer transform's translation, relative to the
        // spread origin. Rotation/shear (b, c) can't be represented
        // without resampling; import unrotated and let the user fix up.
        let (mut x, mut y) = (0.0f64, 0.0f64);
        if let Some(xf) = f64s(node, b"Xfrm") {
            if xf.len() == 6 {
                x = xf[4] - self.origin.0;
                y = xf[5] - self.origin.1;
                if xf[1] != 0.0 || xf[2] != 0.0 {
                    log::warn!("affinity: layer {name:?} is rotated; importing unrotated");
                }
            }
        }

        let mut layer = Layer::new_raster(name);
        let rect = IntRect::from_xywh(x.round() as i32, y.round() as i32, rgba.width, rgba.height);
        blit_rgba8(
            &mut layer.as_raster_mut().unwrap().tiles,
            Depth::Eight,
            rect,
            &rgba.pixels,
        );
        self.report.raster_layers += 1;
        Some(layer)
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
}

fn format(id: u16) -> Option<Format> {
    let (bytes_per_sample, channels, kind) = match id {
        0 => (1, 4, FormatKind::Rgba),
        1 => (2, 4, FormatKind::Rgba),
        2 => (1, 2, FormatKind::Gray),
        3 => (2, 2, FormatKind::Gray),
        4 => (1, 5, FormatKind::Cmyk),
        5 => (2, 4, FormatKind::Lab),
        9 => (4, 4, FormatKind::Rgba),
        _ => return None,
    };
    Some(Format {
        bytes_per_sample,
        channels,
        kind,
    })
}

fn decode_bitmap(archive: &Archive, graph: &Graph, bitm: &Node) -> Result<RgbaImage, AffinityError> {
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

    let sta_names: [&[u8; 4]; 5] = [b"Sta1", b"Sta2", b"Sta3", b"Sta4", b"Sta5"];
    let idx_names: [&[u8; 4]; 5] = [b"Idx1", b"Idx2", b"Idx3", b"Idx4", b"Idx5"];
    let mut planes = Vec::with_capacity(fmt.channels);
    for channel in 0..fmt.channels {
        planes.push(load_plane(
            archive,
            graph,
            bitm,
            sta_names[channel],
            idx_names[channel],
            pitch,
            rows,
            row_bytes,
            height,
        )?);
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
                FormatKind::Cmyk => {
                    let (c, m, yl, k) = (
                        sample(&planes[0], x, y),
                        sample(&planes[1], x, y),
                        sample(&planes[2], x, y),
                        sample(&planes[3], x, y),
                    );
                    ((1.0 - c) * (1.0 - k), (1.0 - m) * (1.0 - k), (1.0 - yl) * (1.0 - k), sample(&planes[4], x, y))
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

/// Rebuild one channel plane from its tile status list and blocks.
#[allow(clippy::too_many_arguments)]
fn load_plane(
    archive: &Archive,
    graph: &Graph,
    bitm: &Node,
    sta: &[u8; 4],
    idx: &[u8; 4],
    pitch: usize,
    rows: usize,
    row_bytes: usize,
    height: usize,
) -> Result<Vec<u8>, AffinityError> {
    let statuses: Vec<u8> = match bitm.field(sta) {
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| match v {
                Value::U8(b) => Ok(*b),
                _ => Err(malformed("tile status is not a byte")),
            })
            .collect::<Result<_, _>>()?,
        _ => return Err(malformed(format!("bitmap has no {}", tag_name(graph::tag(sta))))),
    };
    let blocks = graph.children(bitm, idx);
    let mut next_block = blocks.iter();

    let mut plane = vec![0u8; pitch * rows];
    let (mut x, mut y) = (0usize, 0usize);
    for &status in &statuses {
        match status {
            0 | 1 => {}
            2 => fill_tile(&mut plane, pitch, x, y, &[0xFF]),
            3 => fill_tile(&mut plane, pitch, x, y, &0x3F80_0000u32.to_le_bytes()),
            4 => {
                let block = next_block
                    .next()
                    .ok_or_else(|| malformed("more stored tiles than blocks"))?;
                let rect = match block.field(b"Rect") {
                    Some(Value::VecI(v)) if v.len() == 4 => v,
                    _ => return Err(malformed("block has no rect")),
                };
                let name = match block.field(b"Data") {
                    Some(Value::Embedded { name, .. }) => name,
                    _ => return Err(malformed("block has no data reference")),
                };
                let entry = archive
                    .head(name)
                    .ok_or_else(|| malformed(format!("missing tile entry {name:?}")))?;
                let tile = tile_payload(archive.extract(entry)?)
                    .ok_or_else(|| malformed(format!("tile {name:?} has no 64 KiB payload")))?;
                let (x0, y0) = (rect[0].clamp(0, 256) as usize, rect[1].clamp(0, 256) as usize);
                let (x1, y1) = (rect[2].clamp(0, 256) as usize, rect[3].clamp(0, 256) as usize);
                for ty in y0..y1 {
                    if y + ty >= rows {
                        break;
                    }
                    let dst = (y + ty) * pitch + x + x0;
                    let src = ty * 256 + x0;
                    let n = x1.saturating_sub(x0).min(pitch - (x + x0)) ;
                    plane[dst..dst + n].copy_from_slice(&tile[src..src + n]);
                }
            }
            other => return Err(malformed(format!("unknown tile status {other}"))),
        }
        x += 256;
        if x >= row_bytes.max(1) {
            x = 0;
            y += 256;
            if y >= height {
                break;
            }
        }
    }
    Ok(plane)
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
fn blend_mode(id: u16, version: u16) -> Option<photoslop_core::BlendMode> {
    use photoslop_core::BlendMode::*;
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
    let _ = writeln!(out, "entries: {}", archive.names().collect::<Vec<_>>().join(", "));
    let entry = archive
        .head("doc.dat")
        .ok_or_else(|| malformed("no doc.dat"))?;
    let doc = archive.extract(entry)?;
    let graph = graph::parse(&doc)?;
    dump_node(&graph, graph::ROOT, 0, &mut out, &mut vec![false; graph.nodes.len()]);
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
