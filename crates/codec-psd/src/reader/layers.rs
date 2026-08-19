//! Layer & Mask Information section: layer records, additional-info blocks,
//! channel image data, group folding.

use super::cursor::Cursor;
use super::header::Header;
use super::pixels::{fill_mask_tiles, fill_tiles, plane_to_f32, ColorPlanes};
use super::rle::unpack_channel;
use crate::error::PsdError;
use photoslop_color::ColorMode;
use photoslop_core::{
    AdjustmentData, AdjustmentKind, BlendMode, GroupLayer, IntRect, Layer, LayerId, LayerKind,
    LayerMask, MaskTileMap, RasterLayer, RawBlock, TileMap,
};

/// Additional-layer-info keys whose length field widens to u64 in PSB files
/// (per the spec's "Photoshop Big" notes). All other keys stay u32 even in
/// PSB.
const PSB_U64_KEYS: [[u8; 4]; 13] = [
    *b"LMsk", *b"Lr16", *b"Lr32", *b"Layr", *b"Mt16", *b"Mt32", *b"Mtrn", *b"Alph", *b"FMsk",
    *b"lnk2", *b"FEid", *b"FXid", *b"PxSD",
];

/// Sanity cap on a single layer dimension — PSB canvases max out at 300,000
/// px; allow slack for layers extending past the canvas, but reject absurd
/// values that would make us allocate for corrupt bounds.
const MAX_LAYER_DIM: i32 = 400_000;

pub struct ParsedLayers {
    pub layers: Vec<Layer>,
    /// The layer count was negative: the first alpha channel of the merged
    /// composite is real transparency (matters only for flattened files).
    pub merged_alpha: bool,
}

/// One parsed layer record (before channel data is decoded).
struct Rec {
    rect: IntRect,
    /// (channel id, declared data length incl. 2-byte compression field).
    channels: Vec<(i16, u64)>,
    blend: [u8; 4],
    opacity: u8,
    clipping: bool,
    visible: bool,
    mask: Option<MaskInfo>,
    name: String,
    lsct: Option<Lsct>,
    adjustment: Option<AdjustmentData>,
    extras: Vec<RawBlock>,
}

struct MaskInfo {
    rect: IntRect,
    default_value: u8,
    disabled: bool,
}

struct Lsct {
    divider: u32,
    blend: Option<[u8; 4]>,
}

/// Parse the whole Layer & Mask Information section.
pub fn parse_layer_and_mask_info(
    cur: &mut Cursor,
    header: &Header,
) -> Result<ParsedLayers, PsdError> {
    let psb = header.psb;
    // Section length: u32 in PSD, u64 in PSB.
    let sec_len = cur.len_psb(psb)?;
    let sec_len = cur.checked_len(sec_len)?;
    let mut sec = cur.sub(sec_len)?;
    if sec_len == 0 {
        return Ok(ParsedLayers {
            layers: Vec::new(),
            merged_alpha: false,
        });
    }

    // Layer Info sub-section: its own length (u32/u64 for PSB) + contents.
    let li_len = sec.len_psb(psb)?;
    let li_len = sec.checked_len(li_len)?;
    let mut li = sec.sub(li_len)?;
    let (mut layers, mut merged_alpha) = if li_len > 0 {
        parse_layer_info(&mut li, header)?
    } else {
        (Vec::new(), false)
    };

    // Global Layer Mask Info: u32 length + data.
    // TODO(M6): preserve this block for round-trip; the writer will need it.
    if sec.remaining() >= 4 {
        let gl = sec.u32()? as usize;
        sec.skip(gl.min(sec.remaining()))?;
    }

    // Trailing document-level "additional layer information" blocks
    // ('Patt', 'FMsk', 'Txt2', ...). Spec quirk: at document level these are
    // padded to 4-byte boundaries (layer-record-level blocks pad to 2).
    //
    // TODO(M6): preserve these verbatim for round-trip. For now we only
    // *interpret* 'Lr16'/'Lr32'/'Layr', which is where Photoshop actually
    // stores the layer tree for 16/32-bit documents (Layer Info above is
    // empty in those files).
    while sec.remaining() >= 12 {
        let sig = sec.sig4()?;
        if &sig != b"8BIM" && &sig != b"8B64" {
            // Tolerate trailing zero padding / garbage after the last block.
            break;
        }
        let key = sec.sig4()?;
        let len = if psb && (sig == *b"8B64" || PSB_U64_KEYS.contains(&key)) {
            sec.u64()?
        } else {
            sec.u32()? as u64
        };
        let len = sec.checked_len(len)?;
        let mut block = sec.sub(len)?;
        let pad = (4 - len % 4) % 4;
        sec.skip(pad.min(sec.remaining()))?;

        if layers.is_empty() && matches!(&key, b"Lr16" | b"Lr32" | b"Layr") {
            let (l, ma) = parse_layer_info(&mut block, header)?;
            layers = l;
            merged_alpha |= ma;
        }
    }

    Ok(ParsedLayers {
        layers,
        merged_alpha,
    })
}

/// Layer Info: i16 layer count, layer records, then channel image data in
/// record order. Also the payload format of 'Lr16'/'Lr32'/'Layr' blocks.
fn parse_layer_info(cur: &mut Cursor, header: &Header) -> Result<(Vec<Layer>, bool), PsdError> {
    let count = cur.i16()?;
    // Negative layer count: the first alpha channel in the merged image data
    // holds real transparency for the composite. Use the magnitude.
    let merged_alpha = count < 0;
    let n = count.unsigned_abs() as usize;

    let mut recs = Vec::with_capacity(n.min(4096));
    for _ in 0..n {
        recs.push(parse_layer_record(cur, header)?);
    }

    // Channel image data follows all records, per layer in record order.
    let mut parsed = Vec::with_capacity(recs.len());
    for rec in recs {
        let (tiles, mask_tiles) = decode_layer_channels(cur, &rec, header)?;
        parsed.push((rec, tiles, mask_tiles));
    }

    Ok((fold_groups(parsed), merged_alpha))
}

fn check_rect(rect: IntRect, what: &str) -> Result<(), PsdError> {
    if rect.width() > MAX_LAYER_DIM || rect.height() > MAX_LAYER_DIM {
        return Err(PsdError::Corrupt(format!(
            "{what} rect {}x{} exceeds sanity limits",
            rect.width(),
            rect.height()
        )));
    }
    Ok(())
}

fn read_rect(cur: &mut Cursor) -> Result<IntRect, PsdError> {
    // PSD stores rects as top, left, bottom, right.
    let top = cur.i32()?;
    let left = cur.i32()?;
    let bottom = cur.i32()?;
    let right = cur.i32()?;
    Ok(IntRect::new(left, top, right, bottom))
}

fn parse_layer_record(cur: &mut Cursor, header: &Header) -> Result<Rec, PsdError> {
    let rect = read_rect(cur)?;
    check_rect(rect, "layer")?;

    let channel_count = cur.u16()?;
    if channel_count > 64 {
        return Err(PsdError::Corrupt(format!(
            "layer channel count {channel_count} too large"
        )));
    }
    let mut channels = Vec::with_capacity(channel_count as usize);
    for _ in 0..channel_count {
        let id = cur.i16()?;
        // Channel data length: u32 in PSD, u64 in PSB.
        let len = cur.len_psb(header.psb)?;
        channels.push((id, len));
    }

    let sig = cur.sig4()?;
    if &sig != b"8BIM" {
        return Err(PsdError::Corrupt(
            "layer blend mode signature is not 8BIM".into(),
        ));
    }
    let blend = cur.sig4()?;
    let opacity = cur.u8()?;
    let clipping = cur.u8()? != 0; // 0 = base, 1 = clipped to layer below
    let flags = cur.u8()?;
    let _filler = cur.u8()?;
    // Flags bit 1 is *inverted* visibility: set means the layer is hidden.
    let visible = flags & 0b10 == 0;

    let extra_len = cur.u32()? as usize;
    let mut extra = cur.sub(extra_len)?;

    // --- Layer mask / adjustment layer data block ---
    let mask = parse_mask_block(&mut extra)?;

    // --- Layer blending ranges ---
    // TODO(M6): regenerated as defaults by the writer for now; revisit if a
    // fixture shows customized ranges mattering.
    let ranges_len = extra.u32()? as usize;
    extra.skip(ranges_len)?;

    // --- Pascal layer name, padded to a multiple of 4 (incl. length byte) ---
    let name_len = extra.u8()? as usize;
    let name = String::from_utf8_lossy(extra.take(name_len)?).into_owned();
    let pad = (4 - (1 + name_len) % 4) % 4;
    extra.skip(pad)?;

    let mut rec = Rec {
        rect,
        channels,
        blend,
        opacity,
        clipping,
        visible,
        mask,
        name,
        lsct: None,
        adjustment: None,
        extras: Vec::new(),
    };

    parse_additional_blocks(&mut extra, header, &mut rec)?;
    Ok(rec)
}

fn parse_mask_block(extra: &mut Cursor) -> Result<Option<MaskInfo>, PsdError> {
    let mask_len = extra.u32()? as usize;
    let mut mcur = extra.sub(mask_len)?;
    if mask_len < 18 {
        // 0 = no mask. Sizes other than 0/20/36+ are out-of-spec; skip raw.
        return Ok(None);
    }
    let rect = read_rect(&mut mcur)?;
    check_rect(rect, "layer mask")?;
    let default_value = mcur.u8()?;
    let flags = mcur.u8()?;
    // Flag bit 1 = "layer mask disabled". (Bit 0 = position relative to
    // layer, bit 2 = invert-when-blending (obsolete), bit 4 = has
    // parameters.) The remainder of the block — real-user-mask flags,
    // background and rect, mask parameters — is consumed by the sub-cursor
    // bound above; the "real" mask channel (-3) is likewise skipped for now.
    let disabled = flags & 0b10 != 0;
    Ok(Some(MaskInfo {
        rect,
        default_value,
        disabled,
    }))
}

/// "Additional layer information" blocks at the end of a layer record.
fn parse_additional_blocks(
    extra: &mut Cursor,
    header: &Header,
    rec: &mut Rec,
) -> Result<(), PsdError> {
    while extra.remaining() >= 12 {
        let sig = extra.sig4()?;
        if &sig != b"8BIM" && &sig != b"8B64" {
            return Err(PsdError::Corrupt(format!(
                "additional layer info signature {sig:?} is not 8BIM/8B64"
            )));
        }
        let key = extra.sig4()?;
        // Only specific keys (and the '8B64' signature) widen to u64 in PSB.
        let len = if header.psb && (sig == *b"8B64" || PSB_U64_KEYS.contains(&key)) {
            extra.u64()?
        } else {
            extra.u32()? as u64
        };
        let len = extra.checked_len(len)?;
        let data = extra.take(len)?;
        // Quirk: within layer records blocks are padded to 2 bytes (document
        // -level trailing blocks pad to 4). Some writers omit the final pad
        // byte at the very end of the extra data, so only skip if present.
        if len % 2 == 1 && !extra.is_empty() {
            extra.skip(1)?;
        }

        match &key {
            // Unicode layer name overrides the pascal name. Not kept in
            // `extras`: the writer regenerates it from `Layer::name`.
            b"luni" => {
                if let Some(name) = parse_unicode_name(data) {
                    rec.name = name;
                }
            }
            // Section divider (group structure). Structural; not kept in
            // `extras`, the writer regenerates dividers from the tree.
            b"lsct" => {
                let mut c = Cursor::new(data);
                let divider = c.u32()?;
                // If the block is >= 12 bytes it carries its own
                // "8BIM" + blend key for the group.
                let blend = if data.len() >= 12 {
                    let _sig = c.sig4()?;
                    Some(c.sig4()?)
                } else {
                    None
                };
                rec.lsct = Some(Lsct { divider, blend });
            }
            _ => {
                // Adjustment layers: interpret the kind, keep the raw
                // payload. ('lyid' and every other key — interpreted or not
                // — is ALSO preserved verbatim below for M6 round-trip.)
                if let Some(kind) = AdjustmentKind::from_psd_key(&key) {
                    rec.adjustment = Some(AdjustmentData {
                        kind,
                        raw: data.to_vec(),
                    });
                }
                rec.extras.push(RawBlock {
                    key,
                    data: data.to_vec(),
                });
            }
        }
    }
    // 1..=11 remaining bytes are writer padding; ignore.
    Ok(())
}

/// 'luni' payload: u32 UTF-16 code unit count + UTF-16BE data.
fn parse_unicode_name(data: &[u8]) -> Option<String> {
    let mut c = Cursor::new(data);
    let count = c.u32().ok()? as usize;
    let bytes = c.take(count.checked_mul(2)?).ok()?;
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|p| u16::from_be_bytes([p[0], p[1]]))
        .collect();
    Some(String::from_utf16_lossy(&units))
}

/// Decode one layer's channel image data into color tiles + mask tiles.
fn decode_layer_channels(
    cur: &mut Cursor,
    rec: &Rec,
    header: &Header,
) -> Result<(TileMap, Option<MaskTileMap>), PsdError> {
    let mut planes = ColorPlanes::default();
    let mut mask_bytes: Option<Vec<u8>> = None;

    for &(id, declared_len) in &rec.channels {
        let declared_len = cur.checked_len(declared_len)?;
        let mut ch = cur.sub(declared_len)?;

        // Which plane does this channel land in, and at what geometry?
        // Color/alpha channels use the LAYER rect at document depth; the
        // user mask (-2) uses the MASK rect and is 8-bit regardless of
        // document depth. -3 (real user mask) and spot channels (>= base)
        // are skipped — their bytes are consumed by the declared length.
        enum Target {
            Color(u8),
            Alpha,
            Mask,
        }
        let target = match (id, header.mode) {
            (-1, _) => Target::Alpha,
            (-2, _) => Target::Mask,
            (0, ColorMode::Grayscale) => Target::Color(0),
            (0..=2, ColorMode::Rgb) => Target::Color(id as u8),
            _ => continue,
        };
        let (rect, sample_bytes) = match target {
            Target::Mask => match &rec.mask {
                Some(m) => (m.rect, 1usize),
                None => continue,
            },
            _ => (rec.rect, header.depth.bytes_per_channel()),
        };

        let rows = rect.height() as usize;
        let cols = rect.width() as usize;
        // Zero-area bounds (adjustment layers, group markers) have channels
        // with 0 rows of data — nothing to decode.
        if rows == 0 || cols == 0 {
            continue;
        }
        if ch.remaining() < 2 {
            continue; // no compression field ⇒ channel stored no data
        }
        let comp = ch.u16()?;
        let row_bytes = cols * sample_bytes;
        let bytes: Vec<u8> = match comp {
            0 => {
                let total = rows
                    .checked_mul(row_bytes)
                    .ok_or_else(|| PsdError::Corrupt("channel size overflows".into()))?;
                ch.take(total)?.to_vec()
            }
            1 => unpack_channel(&mut ch, rows, row_bytes, header.psb)?,
            // Note: for 32-bit docs RLE data is simply PackBits over the raw
            // big-endian f32 byte stream (Photoshop itself prefers zip with
            // prediction for 32-bit, which lands here as Unsupported).
            2 | 3 => {
                return Err(PsdError::Unsupported(format!(
                    "zip-compressed channel data (method {comp}) in layer \"{}\"",
                    rec.name
                )))
            }
            c => {
                return Err(PsdError::Corrupt(format!(
                    "unknown channel compression {c}"
                )))
            }
        };

        match target {
            Target::Mask => mask_bytes = Some(bytes),
            Target::Alpha => planes.a = Some(plane_to_f32(&bytes, header.depth)),
            Target::Color(0) => planes.r = Some(plane_to_f32(&bytes, header.depth)),
            Target::Color(1) => planes.g = Some(plane_to_f32(&bytes, header.depth)),
            Target::Color(_) => planes.b = Some(plane_to_f32(&bytes, header.depth)),
        }
    }

    // Grayscale documents: replicate gray into RGB.
    if header.mode == ColorMode::Grayscale {
        planes.g.clone_from(&planes.r);
        planes.b.clone_from(&planes.r);
    }

    let mut tiles = TileMap::new();
    fill_tiles(&mut tiles, header.depth, rec.rect, &planes);

    let mask_tiles = match (&rec.mask, mask_bytes) {
        (Some(m), Some(bytes)) => {
            let mut mt = MaskTileMap::new();
            fill_mask_tiles(&mut mt, m.rect, &bytes);
            Some(mt)
        }
        _ => None,
    };

    Ok((tiles, mask_tiles))
}

/// Fold the flat record list (file order = bottom-to-top, which matches
/// `LayerTree` order) into a tree using 'lsct' section dividers.
///
/// File order quirk: the *hidden* type-3 "bounded section divider" record
/// appears BELOW a group's children (i.e. first, since PSD stores bottom-to-
/// top), and the visible group header record (type 1 open / 2 closed) comes
/// AFTER them, closing the accumulated children into a folder.
fn fold_groups(parsed: Vec<(Rec, TileMap, Option<MaskTileMap>)>) -> Vec<Layer> {
    let mut root: Vec<Layer> = Vec::new();
    let mut stack: Vec<Vec<Layer>> = Vec::new();

    for (rec, tiles, mask_tiles) in parsed {
        match rec.lsct.as_ref().map(|l| l.divider) {
            Some(3) => {
                // Start of a group's children; the divider record itself
                // carries no user content and is dropped (the writer
                // regenerates it).
                stack.push(Vec::new());
            }
            Some(t @ (1 | 2)) => {
                // Group header: close the accumulated children. Tolerate
                // unbalanced files (missing type-3) with an empty group.
                let children = stack.pop().unwrap_or_default();
                let open = t == 1;
                // The group's blend key may live in the lsct block itself;
                // prefer it over the record's key.
                let key = rec.lsct.as_ref().and_then(|l| l.blend).unwrap_or(rec.blend);
                let blend = blend_from_key(&key, &rec.name);
                let mut group = make_layer(
                    rec,
                    mask_tiles,
                    LayerKind::Group(GroupLayer { children, open }),
                );
                group.blend = blend;
                push_to(&mut stack, &mut root, group);
            }
            _ => {
                let blend = blend_from_key(&rec.blend, &rec.name);
                let kind = match rec.adjustment.clone() {
                    Some(adj) => LayerKind::Adjustment(adj),
                    // Text layers ('TySh') and smart objects ('SoLd'/'PlLd')
                    // import as raster layers of their rasterization, with
                    // the raw blocks riding along in `extras` (PLAN.md §4).
                    None => LayerKind::Raster(RasterLayer { tiles }),
                };
                let mut layer = make_layer(rec, mask_tiles, kind);
                layer.blend = blend;
                push_to(&mut stack, &mut root, layer);
            }
        }
    }

    // Unbalanced (type-3 without a closing header): flush accumulators.
    for leftover in stack {
        root.extend(leftover);
    }
    root
}

fn push_to(stack: &mut [Vec<Layer>], root: &mut Vec<Layer>, layer: Layer) {
    match stack.last_mut() {
        Some(top) => top.push(layer),
        None => root.push(layer),
    }
}

fn blend_from_key(key: &[u8; 4], layer_name: &str) -> BlendMode {
    BlendMode::from_psd_key(key).unwrap_or_else(|| {
        log::warn!(
            "unknown blend mode key {:?} on layer \"{layer_name}\"; falling back to Normal",
            String::from_utf8_lossy(key)
        );
        BlendMode::Normal
    })
}

fn make_layer(rec: Rec, mask_tiles: Option<MaskTileMap>, kind: LayerKind) -> Layer {
    let mask = rec.mask.map(|m| LayerMask {
        tiles: mask_tiles.unwrap_or_default(),
        enabled: !m.disabled,
        linked: true,
        default_value: m.default_value,
        bounds: m.rect,
    });
    Layer {
        id: LayerId::next(),
        name: rec.name,
        visible: rec.visible,
        opacity: rec.opacity as f32 / 255.0,
        fill_opacity: 1.0,
        blend: BlendMode::Normal, // callers overwrite
        clipping: rec.clipping,
        locked: false,
        mask,
        kind,
        extras: rec.extras,
    }
}
