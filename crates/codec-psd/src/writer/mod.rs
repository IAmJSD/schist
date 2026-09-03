//! PSD/PSB writer.
//!
//! Emits the five sections a PSD file needs — header, color mode data,
//! image resources, layer & mask info, merged image data — from a
//! [`Document`]. The fidelity strategy lives here: every
//! block the reader preserved verbatim (unknown image resources, layer
//! effects, text engine data, smart object references, adjustment payloads)
//! is re-emitted byte-for-byte, so a file survives an open/edit/save cycle
//! even where we don't understand its contents.
//!
//! Group structure is regenerated from the layer tree: each group becomes a
//! hidden `lsct` type-3 divider below its children plus a header layer above
//! them, which is the inverse of what the reader folds up.

use crate::error::PsdError;
use crate::PSB_U64_KEYS;
use schist_color::{ColorMode, Depth};
use schist_core::{
    Document, IntRect, Layer, LayerKind, LayerMask, MaskTileMap, TileCoord, TileMap, TILE_SIZE,
};

mod buf;
mod rle;

use buf::Buf;

/// Largest dimension a PSD (version 1) file may declare; beyond this the
/// format requires PSB (version 2).
pub const PSD_MAX_DIM: u32 = 30_000;
/// Largest dimension PSB supports.
pub const PSB_MAX_DIM: u32 = 300_000;

/// Sentinel id under which the reader preserves the Color Mode Data section
/// inside `Document::preserved_resources`.
const COLOR_MODE_DATA_SENTINEL_ID: u16 = 0xFFFF;
const RES_RESOLUTION_INFO: u16 = 0x03ED;
const RES_ICC_PROFILE: u16 = 0x040F;

const MODE_GRAYSCALE: u16 = 1;
const MODE_INDEXED: u16 = 2;
const MODE_RGB: u16 = 3;
const MODE_CMYK: u16 = 4;
const MODE_LAB: u16 = 9;

/// Serialize a document to PSD, or PSB when its dimensions require it.
pub fn write_psd(doc: &Document) -> Result<Vec<u8>, PsdError> {
    let psb = doc.width > PSD_MAX_DIM || doc.height > PSD_MAX_DIM;
    write_psd_with(doc, psb)
}

/// Serialize forcing the container format (used by tests to exercise the
/// PSB length widenings on small documents).
pub fn write_psd_with(doc: &Document, psb: bool) -> Result<Vec<u8>, PsdError> {
    let max = if psb { PSB_MAX_DIM } else { PSD_MAX_DIM };
    if doc.width == 0 || doc.height == 0 {
        return Err(PsdError::Corrupt(
            "cannot write a zero-sized document".into(),
        ));
    }
    if doc.width > max || doc.height > max {
        return Err(PsdError::Unsupported(format!(
            "{}x{} exceeds the {} limit of {max}px",
            doc.width,
            doc.height,
            if psb { "PSB" } else { "PSD" }
        )));
    }
    if let Some(raw) = doc
        .tree
        .iter()
        .filter_map(|layer| layer.raw.as_deref())
        .find(|raw| raw.source.is_empty() || raw.source.len() > crate::raw::MAX_SOURCE_BYTES)
    {
        return Err(PsdError::Unsupported(format!(
            "camera-raw source of {} bytes cannot be embedded",
            raw.source.len()
        )));
    }

    let mode = match doc.mode {
        ColorMode::Rgb => MODE_RGB,
        ColorMode::Grayscale => MODE_GRAYSCALE,
        ColorMode::Cmyk => MODE_CMYK,
        ColorMode::Lab => MODE_LAB,
        ColorMode::Indexed => MODE_INDEXED,
    };
    // Channel count of the *merged* image: colour channels plus alpha.
    let channels: u16 = doc.mode.channels() as u16 + 1;

    let mut b = Buf::new();
    // --- File header ---
    b.bytes(b"8BPS");
    b.u16(if psb { 2 } else { 1 });
    b.bytes(&[0; 6]); // reserved
    b.u16(channels);
    b.u32(doc.height);
    b.u32(doc.width);
    b.u16(match doc.depth {
        Depth::Eight => 8,
        Depth::Sixteen => 16,
        Depth::ThirtyTwo => 32,
    });
    b.u16(mode);

    write_color_mode_data(&mut b, doc);
    write_image_resources(&mut b, doc);
    write_layer_and_mask_info(&mut b, doc, psb)?;
    write_merged_image(&mut b, doc, channels, psb);

    Ok(b.into_vec())
}

/// Color Mode Data: empty for RGB/Grayscale unless the reader preserved a
/// section (indexed palettes and duotone specs live here).
fn write_color_mode_data(b: &mut Buf, doc: &Document) {
    match doc
        .preserved_resources
        .iter()
        .find(|r| r.id == COLOR_MODE_DATA_SENTINEL_ID)
    {
        Some(res) => {
            b.u32(res.data.len() as u32);
            b.bytes(&res.data);
        }
        None => b.u32(0),
    }
}

/// Image Resources: preserved blocks in their original order, plus
/// synthesized resolution / ICC blocks when the document carries values the
/// preserved set doesn't already cover.
fn write_image_resources(b: &mut Buf, doc: &Document) {
    let at = b.reserve_u32();
    let mut seen_resolution = false;
    let mut seen_icc = false;

    for res in &doc.preserved_resources {
        if res.id == COLOR_MODE_DATA_SENTINEL_ID {
            continue; // written as its own section above
        }
        seen_resolution |= res.id == RES_RESOLUTION_INFO;
        seen_icc |= res.id == RES_ICC_PROFILE;
        b.bytes(b"8BIM");
        b.u16(res.id);
        // `name` holds the raw pascal bytes (length byte + content + pad)
        // exactly as they were on disk; fall back to an empty pascal string.
        if res.name.is_empty() {
            b.u16(0); // zero length byte + pad byte
        } else {
            b.bytes(&res.name);
            if res.name.len() % 2 == 1 {
                b.u8(0);
            }
        }
        b.u32(res.data.len() as u32);
        b.bytes(&res.data);
        b.pad_to(2);
    }

    if !seen_resolution {
        b.bytes(b"8BIM");
        b.u16(RES_RESOLUTION_INFO);
        b.u16(0); // empty pascal name (+ pad)
        b.u32(16);
        let fixed = fixed_16_16(doc.resolution_dpi);
        b.u32(fixed); // horizontal resolution
        b.u16(1); // display unit: pixels per inch
        b.u16(1); // width unit: inches
        b.u32(fixed); // vertical resolution
        b.u16(1);
        b.u16(1);
    }
    if !seen_icc {
        if let Some(icc) = &doc.icc_profile {
            b.bytes(b"8BIM");
            b.u16(RES_ICC_PROFILE);
            b.u16(0);
            b.u32(icc.len() as u32);
            b.bytes(icc);
            b.pad_to(2);
        }
    }
    b.patch_u32(at);
}

fn fixed_16_16(v: f32) -> u32 {
    (v.max(0.0) * 65536.0).round().min(u32::MAX as f32) as u32
}

// ===== layer & mask info =====

/// What a flattened tree entry represents in the file.
enum Entry<'a> {
    /// A pixel or adjustment layer.
    Leaf(&'a Layer),
    /// The header record that closes a group (carries its name/blend/mask).
    GroupHeader(&'a Layer, bool),
    /// The hidden `lsct` type-3 record that opens a group's children.
    Divider,
}

/// Flatten the tree into PSD file order (bottom-to-top, groups bracketed).
fn flatten<'a>(layers: &'a [Layer], out: &mut Vec<Entry<'a>>) {
    for layer in layers {
        match &layer.kind {
            LayerKind::Group(g) => {
                out.push(Entry::Divider);
                flatten(&g.children, out);
                out.push(Entry::GroupHeader(layer, g.open));
            }
            _ => out.push(Entry::Leaf(layer)),
        }
    }
}

/// One layer record's fully prepared bytes.
struct Prepared {
    bounds: IntRect,
    /// (channel id, encoded bytes including the leading compression word)
    channels: Vec<(i16, Vec<u8>)>,
    blend: [u8; 4],
    opacity: u8,
    clipping: u8,
    flags: u8,
    mask: Option<MaskOut>,
    name: String,
    /// Additional layer info blocks: (key, payload).
    extras: Vec<([u8; 4], Vec<u8>)>,
    /// The layer's blending-ranges block, verbatim.
    blending_ranges: Vec<u8>,
}

struct MaskOut {
    rect: IntRect,
    default_value: u8,
    flags: u8,
}

fn write_layer_and_mask_info(b: &mut Buf, doc: &Document, psb: bool) -> Result<(), PsdError> {
    let mut entries = Vec::new();
    flatten(&doc.tree.layers, &mut entries);

    if entries.is_empty() {
        b.len_psb(0, psb);
        return Ok(());
    }

    let prepared: Vec<Prepared> = entries
        .iter()
        .map(|e| prepare_entry(e, doc, psb))
        .collect::<Result<_, _>>()?;

    let section_at = b.reserve_len(psb);
    // --- Layer info ---
    let layer_info_at = b.reserve_len(psb);
    // A negative count declares that the merged image's alpha channel is
    // real transparency rather than a spot/alpha channel.
    //
    // `prepared` includes two extra records per group, so past 32767
    // entries this cast wrapped and the file declared a nonsense layer
    // count. Refuse instead of writing something unreadable.
    if prepared.len() > i16::MAX as usize {
        return Err(PsdError::Unsupported(format!(
            "{} layer records exceeds the {} the format can declare",
            prepared.len(),
            i16::MAX
        )));
    }
    b.i16(-(prepared.len() as i16));
    for p in &prepared {
        write_layer_record(b, p, psb);
    }
    for p in &prepared {
        for (_, data) in &p.channels {
            b.bytes(data);
        }
    }
    b.pad_to(2);
    b.patch_len(layer_info_at, psb);

    // --- Global layer mask info ---
    // Preserved verbatim; a zero length here dropped the block from
    // every file that had one.
    b.u32(doc.global_layer_mask.len() as u32);
    b.bytes(&doc.global_layer_mask);

    // --- Document-level additional layer information ---
    // Pattern definitions, linked smart objects and the rest, exactly as
    // they arrived. `Lr16`/`Lr32`/`Layr` are not in here: the layer tree
    // above is regenerated, so echoing the old copy back would write it
    // twice. Spec quirk: these pad to 4 bytes, not 2.
    for block in &doc.preserved_layer_info {
        // 8B64 marks a block whose length is u64 whatever the key, so a
        // block that arrived that way has to go back out that way: an
        // 8B64 block with a key outside `PSB_U64_KEYS` would otherwise be
        // read as u64 and rewritten as u32, and `as u32` would silently
        // truncate anything over 4 GiB.
        let wide =
            block.data.len() > u32::MAX as usize || (psb && PSB_U64_KEYS.contains(&block.key));
        b.bytes(if wide && !PSB_U64_KEYS.contains(&block.key) {
            b"8B64"
        } else {
            b"8BIM"
        });
        b.bytes(&block.key);
        if wide {
            b.u64(block.data.len() as u64);
        } else {
            b.u32(block.data.len() as u32);
        }
        b.bytes(&block.data);
        // Pad relative to the payload length, not to the absolute file
        // offset. `pad_to(4)` disagreed with the reader, which skips
        // `(4 - len % 4) % 4`, whenever a block did not happen to start
        // 4-aligned -- which depends on layer name lengths and channel
        // sizes, so real files hit it constantly and everything after the
        // first misaligned block was silently dropped.
        for _ in 0..(4 - block.data.len() % 4) % 4 {
            b.u8(0);
        }
    }
    b.patch_len(section_at, psb);
    Ok(())
}

fn write_layer_record(b: &mut Buf, p: &Prepared, psb: bool) {
    b.i32(p.bounds.top);
    b.i32(p.bounds.left);
    b.i32(p.bounds.bottom);
    b.i32(p.bounds.right);
    b.u16(p.channels.len() as u16);
    for (id, data) in &p.channels {
        b.i16(*id);
        b.len_psb(data.len() as u64, psb);
    }
    b.bytes(b"8BIM");
    b.bytes(&p.blend);
    b.u8(p.opacity);
    b.u8(p.clipping);
    b.u8(p.flags);
    b.u8(0); // filler

    let extra_at = b.reserve_u32();
    // Layer mask data.
    match &p.mask {
        Some(m) => {
            b.u32(20);
            b.i32(m.rect.top);
            b.i32(m.rect.left);
            b.i32(m.rect.bottom);
            b.i32(m.rect.right);
            b.u8(m.default_value);
            b.u8(m.flags);
            b.u16(0); // padding to the declared 20 bytes
        }
        None => b.u32(0),
    }
    // Layer blending ranges, verbatim. Emitting a zero length here lost
    // any "Blend If" the file arrived with.
    b.u32(p.blending_ranges.len() as u32);
    b.bytes(&p.blending_ranges);
    b.pascal(&p.name, 4);
    for (key, payload) in &p.extras {
        b.bytes(b"8BIM");
        b.bytes(key);
        // In PSB these keys carry a u64 length. `p.extras` includes blocks
        // preserved from the source file, so they really can appear here;
        // emitting u32 for one produced a file the reader (and Photoshop)
        // then read four bytes short.
        if psb && crate::PSB_U64_KEYS.contains(key) {
            b.u64(payload.len() as u64);
        } else {
            b.u32(payload.len() as u32);
        }
        b.bytes(payload);
        if payload.len() % 2 == 1 {
            b.u8(0);
        }
    }
    b.patch_u32(extra_at);
}

fn prepare_entry(entry: &Entry<'_>, doc: &Document, psb: bool) -> Result<Prepared, PsdError> {
    match entry {
        Entry::Divider => Ok(Prepared {
            bounds: IntRect::EMPTY,
            channels: empty_channels(doc),
            blend: *b"norm",
            opacity: 255,
            clipping: 0,
            // bit 3 set = flags are meaningful, bit 4 = pixel data irrelevant
            flags: 0b0001_1000,
            mask: None,
            name: "</Layer group>".into(),
            extras: vec![(*b"lsct", lsct_payload(3, None))],
            blending_ranges: Vec::new(),
        }),
        Entry::GroupHeader(layer, open) => {
            let mut p = prepare_common(layer, doc, psb, empty_channels(doc), IntRect::EMPTY);
            p.extras.insert(
                0,
                (
                    *b"lsct",
                    lsct_payload(if *open { 1 } else { 2 }, Some(layer.blend.psd_key())),
                ),
            );
            Ok(p)
        }
        Entry::Leaf(layer) => {
            let (bounds, channels) = match &layer.kind {
                LayerKind::Raster(r) => {
                    let bounds = r.tiles.content_bounds();
                    if bounds.is_empty() {
                        (IntRect::EMPTY, empty_channels(doc))
                    } else {
                        (bounds, encode_color_channels(&r.tiles, bounds, doc, psb))
                    }
                }
                // Adjustment layers carry no pixels; their parameters are
                // written as a settings block by `build_extras`.
                _ => (IntRect::EMPTY, empty_channels(doc)),
            };
            Ok(prepare_common(layer, doc, psb, channels, bounds))
        }
    }
}

fn prepare_common(
    layer: &Layer,
    doc: &Document,
    psb: bool,
    mut channels: Vec<(i16, Vec<u8>)>,
    bounds: IntRect,
) -> Prepared {
    let mask = layer.mask.as_ref().and_then(|m| {
        let rect = mask_bounds(m);
        if rect.is_empty() {
            return None;
        }
        channels.push((-2, encode_mask_channel(m, rect, psb)));
        Some(MaskOut {
            rect,
            default_value: m.default_value,
            // Bit 1 = "layer mask disabled".
            flags: if m.enabled { 0 } else { 0b10 },
        })
    });
    let _ = doc;
    Prepared {
        bounds,
        channels,
        blend: *layer.blend.psd_key(),
        opacity: (layer.opacity.clamp(0.0, 1.0) * 255.0).round() as u8,
        clipping: u8::from(layer.clipping),
        // Bit 1 is inverted: set means hidden.
        flags: 0b1000 | if layer.visible { 0 } else { 0b10 },
        mask,
        name: layer.name.clone(),
        extras: build_extras(layer, doc),
        blending_ranges: layer.blending_ranges.clone(),
    }
}

/// Preserved blocks plus a regenerated unicode name.
fn build_extras(layer: &Layer, doc: &Document) -> Vec<([u8; 4], Vec<u8>)> {
    let mut out = vec![(*b"luni", unicode_name_payload(&layer.name))];
    // An adjustment layer's settings live in its own block. Layers created
    // in Schist carry them only in `params_json`, which no PSD reader
    // understands, so without re-encoding here the layer was written as an
    // empty raster and every adjustment the user made was lost on save.
    // A preserved block is stale once the parameters have been edited, so
    // the encoded form wins and the old one is dropped below.
    let adjustment = match &layer.kind {
        LayerKind::Adjustment(data) => encode_adjustment(data),
        _ => None,
    };
    // Effects are re-encoded from the layer's own style, so any preserved
    // block is stale by definition. 'lrFX' is the pre-CS legacy form,
    // which we never write.
    let encoded = crate::effects::write_lfx2(&layer.style);
    // A live shape regenerates its own blocks, so preserved ones are stale.
    let vector = layer.shape.is_some();
    for block in &layer.extras {
        // 'luni'/'lsct' are regenerated, never echoed back.
        if &block.key == b"luni" || &block.key == b"lsct" {
            continue;
        }
        if &block.key == b"lfx2" || &block.key == b"lrFX" {
            continue;
        }
        // Regenerated from `Layer::smart` below.
        if block.key == crate::smart::SMART_BLOCK_KEY {
            continue;
        }
        // Regenerated from `Layer::raw` below. Keeping an old copy would
        // resurrect stale settings after the layer was edited.
        if block.key == crate::raw::RAW_BLOCK_KEY {
            continue;
        }
        // Fill opacity is regenerated from the layer below, so a
        // preserved copy is stale: echoing it back wrote the file's
        // original value over whatever the user set in Schist.
        if &block.key == b"iOpa" {
            continue;
        }
        if vector && (&block.key == b"vmsk" || &block.key == b"vsms" || &block.key == b"SoCo") {
            continue;
        }
        // Drop the stale settings block when we have re-encoded it.
        if let Some((key, _)) = &adjustment {
            if &block.key == key {
                continue;
            }
        }
        out.push((block.key, block.data.clone()));
    }
    if let Some(entry) = adjustment {
        out.push(entry);
    }
    if let Some(payload) = encoded {
        out.push((*b"lfx2", payload));
    }
    // The smart object's source pixels. Photoshop's own `SoLd`/`PlLd`
    // descriptors point at pixels held elsewhere in the file, which we
    // preserve but do not author; this is our own block, ignored by
    // readers that do not know it.
    if let Some(payload) = crate::smart::write_smart(layer) {
        out.push((crate::smart::SMART_BLOCK_KEY, payload));
    }
    // The immutable camera capture and its editable development settings.
    // Other PSD readers ignore this private block and use the raster pixels.
    if let Some(payload) = crate::raw::write_raw(layer) {
        out.push((crate::raw::RAW_BLOCK_KEY, payload));
    }
    // 'iOpa' is one byte of fill opacity plus three of padding. Photoshop
    // omits the block at 100%, which is what a reader assumes when it is
    // absent, so only write it when it says something.
    let fill = (layer.fill_opacity.clamp(0.0, 1.0) * 255.0).round() as u8;
    if fill != 255 {
        out.push((*b"iOpa", vec![fill, 0, 0, 0]));
    }
    out.extend(shape_blocks(layer, doc));
    out
}

/// The settings block for an adjustment layer, preferring the live
/// parameters over whatever the file arrived with.
///
/// Returns `None` when the parameters cannot be encoded, so the caller
/// keeps the preserved bytes rather than writing a block that would be
/// read back as something else.
fn encode_adjustment(data: &schist_core::AdjustmentData) -> Option<([u8; 4], Vec<u8>)> {
    let json = data.params_json.as_deref()?;
    let params: schist_adjustments::Params = serde_json::from_str(json).ok()?;
    let payload = schist_adjustments::encode_psd(data.kind, &params)?;
    Some((data.kind.psd_key(), payload))
}

/// Vector mask and fill blocks for a shape layer, so the shape survives as
/// a shape rather than as a picture of one.
fn shape_blocks(layer: &Layer, doc: &Document) -> Vec<([u8; 4], Vec<u8>)> {
    let Some(shape) = layer.shape.as_deref() else {
        return Vec::new();
    };
    let mut out = vec![(
        *b"vmsk",
        crate::vector::write_vector_mask(&shape.path, doc.width, doc.height),
    )];
    // The fill, as the solid-colour payload Photoshop expects.
    let mut b = schist_psd_descriptor::Builder::new("null");
    let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0) as f64;
    b.color("Clr ", q(shape.fill.r), q(shape.fill.g), q(shape.fill.b));
    out.push((*b"SoCo", b.finish_versioned()));
    out
}

fn unicode_name_payload(name: &str) -> Vec<u8> {
    let units: Vec<u16> = name.encode_utf16().collect();
    let mut out = Vec::with_capacity(4 + units.len() * 2);
    out.extend_from_slice(&(units.len() as u32).to_be_bytes());
    for u in units {
        out.extend_from_slice(&u.to_be_bytes());
    }
    out
}

fn lsct_payload(divider: u32, blend: Option<&[u8; 4]>) -> Vec<u8> {
    let mut out = divider.to_be_bytes().to_vec();
    if let Some(key) = blend {
        out.extend_from_slice(b"8BIM");
        out.extend_from_slice(key);
    }
    out
}

/// Zero-area colour channels (compression word only) for records with no
/// pixels: group headers, dividers, adjustment layers, empty layers.
fn empty_channels(doc: &Document) -> Vec<(i16, Vec<u8>)> {
    let mut ids: Vec<i16> = vec![-1];
    ids.extend(0..doc.mode.channels() as i16);
    ids.iter().map(|&id| (id, vec![0, 0])).collect()
}

// ===== channel data =====

fn mask_bounds(mask: &LayerMask) -> IntRect {
    if !mask.bounds.is_empty() {
        mask.bounds
    } else {
        mask.tiles.tile_bounds()
    }
}

/// Extract R,G,B,A planes for `rect` at the document's depth, big-endian.
/// The colour channels a document of this mode stores, converted from the
/// RGBA the tiles hold.
///
/// Everything is edited as RGBA; a CMYK or Lab document converts here on
/// the way out and in the reader on the way back, so the file is genuinely
/// in its mode even though the editing was not.
fn colour_planes(tiles: &TileMap, rect: IntRect, doc: &Document) -> Vec<Vec<u8>> {
    let depth = doc.depth;
    let w = rect.width().max(0) as usize;
    let h = rect.height().max(0) as usize;
    let bpc = depth.bytes_per_channel();
    let n = doc.mode.channels();
    let mut planes: Vec<Vec<u8>> = (0..n).map(|_| vec![0u8; w * h * bpc]).collect();
    if matches!(doc.mode, ColorMode::Rgb | ColorMode::Grayscale) {
        // The fast path: no conversion, just the first `n` of RGBA.
        let rgba = extract_planes(tiles, rect, depth);
        for (i, p) in planes.iter_mut().enumerate() {
            p.clone_from(&rgba[i]);
        }
        return planes;
    }
    for coord in TileCoord::covering(&rect) {
        let Some(buf) = tiles.get(coord) else {
            continue;
        };
        let trect = coord.rect();
        let clip = trect.intersect(&rect);
        if clip.is_empty() {
            continue;
        }
        for y in clip.top..clip.bottom {
            for x in clip.left..clip.right {
                let px = buf
                    .get((y - trect.top) as usize * TILE_SIZE as usize + (x - trect.left) as usize);
                let at = ((y - rect.top) as usize * w + (x - rect.left) as usize) * bpc;
                let values: Vec<f32> = match doc.mode {
                    ColorMode::Cmyk => {
                        // PSD stores CMYK inverted: 0 means full ink.
                        schist_color::convert::rgb_to_cmyk(px)
                            .iter()
                            .map(|v| 1.0 - v)
                            .collect()
                    }
                    ColorMode::Lab => {
                        let lab = schist_color::convert::rgb_to_lab(px);
                        vec![
                            lab[0] / 100.0,
                            (lab[1] + 128.0) / 255.0,
                            (lab[2] + 128.0) / 255.0,
                        ]
                    }
                    // Indexed has no palette here, so its single plane is
                    // the luminance -- the same thing the reader shows.
                    _ => vec![0.299 * px.r + 0.587 * px.g + 0.114 * px.b],
                };
                for (i, v) in values.into_iter().take(n).enumerate() {
                    write_sample(&mut planes[i][at..at + bpc], v.clamp(0.0, 1.0), depth);
                }
            }
        }
    }
    planes
}

fn extract_planes(tiles: &TileMap, rect: IntRect, depth: Depth) -> [Vec<u8>; 4] {
    let w = rect.width() as usize;
    let h = rect.height() as usize;
    let bpc = depth.bytes_per_channel();
    let mut planes = [
        vec![0u8; w * h * bpc],
        vec![0u8; w * h * bpc],
        vec![0u8; w * h * bpc],
        vec![0u8; w * h * bpc],
    ];
    for coord in TileCoord::covering(&rect) {
        let Some(buf) = tiles.get(coord) else {
            continue;
        };
        let trect = coord.rect();
        let clip = trect.intersect(&rect);
        if clip.is_empty() {
            continue;
        }
        for y in clip.top..clip.bottom {
            let ly = (y - trect.top) as usize;
            let oy = (y - rect.top) as usize;
            for x in clip.left..clip.right {
                let lx = (x - trect.left) as usize;
                let ox = (x - rect.left) as usize;
                let px = buf.get(ly * TILE_SIZE as usize + lx);
                let at = (oy * w + ox) * bpc;
                for (i, v) in [px.r, px.g, px.b, px.a].into_iter().enumerate() {
                    write_sample(&mut planes[i][at..at + bpc], v, depth);
                }
            }
        }
    }
    planes
}

fn write_sample(out: &mut [u8], v: f32, depth: Depth) {
    match depth {
        Depth::Eight => out[0] = schist_color::f32_to_u8(v),
        Depth::Sixteen => out.copy_from_slice(&schist_color::f32_to_u16(v).to_be_bytes()),
        Depth::ThirtyTwo => out.copy_from_slice(&v.to_be_bytes()),
    }
}

/// Encode a layer's colour channels: alpha first, then colour, matching the
/// order Photoshop writes.
fn encode_color_channels(
    tiles: &TileMap,
    bounds: IntRect,
    doc: &Document,
    psb: bool,
) -> Vec<(i16, Vec<u8>)> {
    let alpha = extract_planes(tiles, bounds, doc.depth)[3].clone();
    let colour = colour_planes(tiles, bounds, doc);
    let h = bounds.height() as usize;
    let row_bytes = bounds.width() as usize * doc.depth.bytes_per_channel();
    let mut out = Vec::with_capacity(colour.len() + 1);
    out.push((-1, encode_channel(&alpha, row_bytes, h, doc.depth, psb)));
    for (i, plane) in colour.iter().enumerate() {
        out.push((
            i as i16,
            encode_channel(plane, row_bytes, h, doc.depth, psb),
        ));
    }
    out
}

fn encode_mask_channel(mask: &LayerMask, rect: IntRect, psb: bool) -> Vec<u8> {
    let w = rect.width() as usize;
    let h = rect.height() as usize;
    let mut plane = vec![mask.default_value; w * h];
    fill_mask_plane(&mask.tiles, rect, &mut plane);
    // Mask channels are 8-bit regardless of document depth.
    encode_channel(&plane, w, h, Depth::Eight, psb)
}

fn fill_mask_plane(tiles: &MaskTileMap, rect: IntRect, plane: &mut [u8]) {
    let w = rect.width() as usize;
    for coord in TileCoord::covering(&rect) {
        let Some(buf) = tiles.get(coord) else {
            continue;
        };
        let trect = coord.rect();
        let clip = trect.intersect(&rect);
        for y in clip.top..clip.bottom {
            let ly = (y - trect.top) as usize;
            let oy = (y - rect.top) as usize;
            for x in clip.left..clip.right {
                let lx = (x - trect.left) as usize;
                let ox = (x - rect.left) as usize;
                plane[oy * w + ox] = buf[ly * TILE_SIZE as usize + lx];
            }
        }
    }
}

/// Encode one channel plane with its compression word.
///
/// 8-bit uses RLE (what Photoshop writes and every reader supports); deeper
/// formats use raw, since their alternative in the wild is zip, which we
/// don't emit.
fn encode_channel(plane: &[u8], row_bytes: usize, rows: usize, depth: Depth, psb: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(plane.len() + 2 + rows * 2);
    if depth != Depth::Eight || rows == 0 || row_bytes == 0 {
        out.extend_from_slice(&0u16.to_be_bytes()); // raw
        out.extend_from_slice(plane);
        return out;
    }
    out.extend_from_slice(&1u16.to_be_bytes()); // RLE
    let mut packed_rows = Vec::with_capacity(rows);
    for row in 0..rows {
        packed_rows.push(rle::pack_row(
            &plane[row * row_bytes..(row + 1) * row_bytes],
        ));
    }
    for r in &packed_rows {
        if psb {
            out.extend_from_slice(&(r.len() as u32).to_be_bytes());
        } else {
            out.extend_from_slice(&(r.len() as u16).to_be_bytes());
        }
    }
    for r in &packed_rows {
        out.extend_from_slice(r);
    }
    out
}

// ===== merged image =====

/// The flattened composite every PSD carries so simple viewers (and
/// Photoshop's own thumbnails) can display the file without interpreting
/// layers.
fn write_merged_image(b: &mut Buf, doc: &Document, channels: u16, psb: bool) {
    let rect = doc.canvas_rect();
    let composite = schist_compositor::composite_region_f32(doc, rect);
    let w = rect.width() as usize;
    let h = rect.height() as usize;
    let bpc = doc.depth.bytes_per_channel();
    let row_bytes = w * bpc;

    // Channel order in the merged section is colour first, then alpha.
    // Non-RGB modes convert here, the same way the layer channels do.
    let n = doc.mode.channels();
    let mut planes: Vec<Vec<u8>> = (0..n + 1).map(|_| vec![0u8; w * h * bpc]).collect();
    for i in 0..w * h {
        let px = schist_color::Rgba::new(
            composite[i * 4],
            composite[i * 4 + 1],
            composite[i * 4 + 2],
            composite[i * 4 + 3],
        );
        let values: Vec<f32> = match doc.mode {
            ColorMode::Rgb => vec![px.r, px.g, px.b],
            ColorMode::Grayscale | ColorMode::Indexed => {
                vec![0.299 * px.r + 0.587 * px.g + 0.114 * px.b]
            }
            ColorMode::Cmyk => schist_color::convert::rgb_to_cmyk(px)
                .iter()
                .map(|v| 1.0 - v)
                .collect(),
            ColorMode::Lab => {
                let lab = schist_color::convert::rgb_to_lab(px);
                vec![
                    lab[0] / 100.0,
                    (lab[1] + 128.0) / 255.0,
                    (lab[2] + 128.0) / 255.0,
                ]
            }
        };
        for (c, v) in values.into_iter().take(n).enumerate() {
            write_sample(
                &mut planes[c][i * bpc..(i + 1) * bpc],
                v.clamp(0.0, 1.0),
                doc.depth,
            );
        }
        write_sample(&mut planes[n][i * bpc..(i + 1) * bpc], px.a, doc.depth);
    }
    debug_assert_eq!(planes.len(), channels as usize);

    if doc.depth != Depth::Eight {
        b.u16(0); // raw
        for plane in &planes {
            b.bytes(plane);
        }
        return;
    }

    // RLE: the row-count table spans every channel before any row data.
    b.u16(1);
    let packed: Vec<Vec<Vec<u8>>> = planes
        .iter()
        .map(|plane| {
            (0..h)
                .map(|row| rle::pack_row(&plane[row * row_bytes..(row + 1) * row_bytes]))
                .collect()
        })
        .collect();
    for rows in &packed {
        for r in rows {
            if psb {
                b.u32(r.len() as u32);
            } else {
                b.u16(r.len() as u16);
            }
        }
    }
    for rows in &packed {
        for r in rows {
            b.bytes(r);
        }
    }
}
