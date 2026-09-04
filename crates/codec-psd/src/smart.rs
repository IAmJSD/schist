//! Smart-object payloads, as a private additional-layer-info block.
//!
//! The README says "Smart objects keep their source pixels, so
//! transforming one repeatedly costs no more quality than transforming it
//! once". That was true inside a session and false across a save:
//! `LayerKind` has only `Raster`/`Group`/`Adjustment`, the payload rides
//! on `Layer::smart`, and the writer never serialized it. After
//! save-and-reopen the layer was a plain raster of its last
//! rasterization, and every further transform degraded it.
//!
//! Photoshop's own `SoLd`/`PlLd` descriptors point at pixels held
//! elsewhere in the file (`lnk2`), which we preserve verbatim but do not
//! author. Rather than pretend to write that graph, this stores the
//! source in a block of our own -- the same trick the type tool already
//! plays with `PsTx`. Photoshop ignores keys it does not know, so the
//! file stays valid there; Schist reads its own smart objects back.

use schist_color::Depth;
use schist_core::{
    blit_rgba8, blit_rgba_f32, Affine, Filter, IntRect, Layer, SmartObject, TileMap,
};

/// Private block key. Not an Adobe key: "Sc" for Schist, "So" for smart
/// object.
pub const SMART_BLOCK_KEY: [u8; 4] = *b"ScSo";

/// Format revision.
///
/// v1 stored 8-bit samples, which quantised a 16-bit source the moment it
/// was saved -- the exact loss the block exists to prevent. v2 stores
/// f32, so a deep source survives; v1 payloads are still read.
const VERSION: u32 = 2;
const VERSION_U8: u32 = 1;

/// Guard against a corrupt or hostile length claiming gigabytes.
const MAX_SOURCE_PIXELS: u64 = 200_000_000;

fn filter_code(f: Filter) -> u8 {
    match f {
        Filter::Nearest => 0,
        Filter::Bilinear => 1,
        Filter::Bicubic => 2,
    }
}

fn filter_from_code(v: u8) -> Filter {
    match v {
        0 => Filter::Nearest,
        2 => Filter::Bicubic,
        _ => Filter::Bilinear,
    }
}

/// Serialize a layer's smart object, or `None` if it has none.
pub fn write_smart(layer: &Layer) -> Option<Vec<u8>> {
    let smart = layer.smart.as_deref()?;
    let bounds = smart.source_bounds;
    if bounds.is_empty() {
        return None;
    }
    let (w, h) = (bounds.width() as usize, bounds.height() as usize);

    // f32 per channel: `to_u8()` here threw away 8 bits of a 16-bit
    // source on every save, which is what this block exists to avoid.
    let mut rgba = Vec::with_capacity(w * h * 16);
    for y in bounds.top..bounds.bottom {
        for x in bounds.left..bounds.right {
            let px = smart.source.pixel(x, y);
            for c in [px.r, px.g, px.b, px.a] {
                rgba.extend_from_slice(&c.to_be_bytes());
            }
        }
    }

    let mut out = Vec::new();
    out.extend_from_slice(&VERSION.to_be_bytes());
    let name = smart.name.as_bytes();
    out.extend_from_slice(&(name.len() as u32).to_be_bytes());
    out.extend_from_slice(name);
    for v in [
        smart.transform.a,
        smart.transform.b,
        smart.transform.c,
        smart.transform.d,
        smart.transform.tx,
        smart.transform.ty,
    ] {
        out.extend_from_slice(&v.to_be_bytes());
    }
    out.push(filter_code(smart.filter));
    for v in [bounds.left, bounds.top, bounds.right, bounds.bottom] {
        out.extend_from_slice(&v.to_be_bytes());
    }
    let packed = miniz_oxide::deflate::compress_to_vec_zlib(&rgba, 6);
    out.extend_from_slice(&(packed.len() as u32).to_be_bytes());
    out.extend_from_slice(&packed);
    Some(out)
}

/// Parse a `ScSo` payload back into a smart object.
///
/// Returns `None` for anything malformed: a layer that loses its smart
/// wrapper still has its rasterized pixels, so declining is always
/// better than failing the whole open.
pub fn read_smart(data: &[u8], depth: Depth) -> Option<SmartObject> {
    let mut c = Cursor { data, at: 0 };
    let version = c.u32()?;
    if version != VERSION && version != VERSION_U8 {
        return None;
    }
    let name_len = c.u32()? as usize;
    let name = String::from_utf8_lossy(c.take(name_len)?).into_owned();
    let transform = Affine {
        a: c.f32()?,
        b: c.f32()?,
        c: c.f32()?,
        d: c.f32()?,
        tx: c.f32()?,
        ty: c.f32()?,
    };
    let filter = filter_from_code(c.u8()?);
    let bounds = IntRect::new(c.i32()?, c.i32()?, c.i32()?, c.i32()?);
    if bounds.is_empty() {
        return None;
    }
    let pixels = bounds.width() as u64 * bounds.height() as u64;
    if pixels > MAX_SOURCE_PIXELS {
        log::warn!("smart object claims {pixels} source pixels; ignoring it");
        return None;
    }
    let packed_len = c.u32()? as usize;
    let packed = c.take(packed_len)?;
    let sample = if version == VERSION { 4 } else { 1 };
    let expected = pixels as usize * 4 * sample;
    // The decompression limit below is the real bound on host memory, so
    // it has to know how wide a sample is.
    let raw =
        miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(packed, expected.max(1)).ok()?;
    if raw.len() < expected {
        return None;
    }

    let mut source = TileMap::default();
    if version == VERSION_U8 {
        blit_rgba8(&mut source, depth, bounds, &raw[..expected]);
    } else {
        let floats: Vec<f32> = raw[..expected]
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| f32::from_be_bytes(*b))
            .collect();
        blit_rgba_f32(&mut source, depth, bounds, &floats);
    }
    Some(SmartObject {
        source,
        source_bounds: bounds,
        transform,
        filter,
        name,
    })
}

/// A big-endian reader that returns `None` rather than panicking on a
/// short buffer.
struct Cursor<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.at.checked_add(n)?;
        let out = self.data.get(self.at..end)?;
        self.at = end;
        Some(out)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_be_bytes(self.take(4)?.try_into().ok()?))
    }
    fn i32(&mut self) -> Option<i32> {
        Some(i32::from_be_bytes(self.take(4)?.try_into().ok()?))
    }
    fn f32(&mut self) -> Option<f32> {
        Some(f32::from_be_bytes(self.take(4)?.try_into().ok()?))
    }
}
