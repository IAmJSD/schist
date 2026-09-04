//! The cheap ways to look at a PSD without decoding one: the canvas size
//! from the 26-byte header, and the thumbnail Photoshop embeds in an
//! image resource.
//!
//! Both exist for callers that need a picture of a file *now* — macOS
//! Quick Look gets seconds, and a layered 2 GB PSB does not open in
//! seconds. `read_psd` remains the accurate path; this one skips the
//! layer and merged-image sections entirely, touching only the header
//! and the resource block's own lengths.

use super::cursor::Cursor;
use super::header;
use crate::error::PsdError;

/// Photoshop 5.0 and later: a JPEG thumbnail in RGB order.
const RES_THUMBNAIL: u16 = 0x040C;
/// Photoshop 4.0: the same block, but the JPEG's channels are BGR.
const RES_THUMBNAIL_PS4: u16 = 0x0409;

/// The `kJpegRGB` value of the resource's format field. The alternative,
/// `kRawRGB` (0), is not known to have shipped in a real file.
const FORMAT_JPEG: u32 = 1;

/// A file's embedded thumbnail, still JPEG-encoded: decoding it is the
/// caller's business, and this crate deliberately owns no image decoder.
#[derive(Debug, Clone, Copy)]
pub struct Thumbnail<'a> {
    /// Width the resource declares. Photoshop fits the thumbnail in a
    /// 256-pixel box, so this is small, and it is what the JPEG holds.
    pub width: u32,
    pub height: u32,
    /// The JPEG's channels are in BGR order rather than RGB — true only
    /// for the Photoshop 4.0 resource (0x0409).
    pub bgr: bool,
    pub jpeg: &'a [u8],
}

/// The canvas size, from the header alone.
pub fn read_dimensions(bytes: &[u8]) -> Result<(u32, u32), PsdError> {
    let mut cur = Cursor::new(bytes);
    let h = header::parse_header(&mut cur)?;
    Ok((h.width, h.height))
}

/// The embedded thumbnail, if the file carries one.
///
/// Prefers the modern RGB resource over the Photoshop 4.0 one when a
/// file (unusually) carries both. Returns `None` rather than an error
/// for anything malformed: a missing thumbnail and an unreadable one
/// call for the same fallback.
pub fn read_thumbnail(bytes: &[u8]) -> Option<Thumbnail<'_>> {
    let mut cur = Cursor::new(bytes);
    header::parse_header(&mut cur).ok()?;

    // Color Mode Data, skipped: its length is u32 even in PSB.
    let cm_len = cur.u32().ok()? as usize;
    cur.skip(cm_len).ok()?;

    let res_len = cur.u32().ok()? as usize;
    let mut sec = cur.sub(res_len).ok()?;

    let mut ps4 = None;
    while sec.remaining() >= 4 {
        if &sec.sig4().ok()? != b"8BIM" {
            break;
        }
        let id = sec.u16().ok()?;
        // Pascal name: length byte plus content, the pair padded to even.
        let name_len = sec.u8().ok()? as usize;
        sec.skip(name_len + (1 + name_len) % 2).ok()?;
        let data_len = sec.u32().ok()? as usize;
        let data = sec.take(data_len).ok()?;
        if data_len % 2 == 1 && !sec.is_empty() {
            sec.skip(1).ok()?;
        }

        match id {
            RES_THUMBNAIL => return parse_thumbnail(data, false),
            RES_THUMBNAIL_PS4 => ps4 = parse_thumbnail(data, true),
            _ => {}
        }
    }
    ps4
}

/// The resource body: a 28-byte header, then the JPEG.
///
/// ```text
/// u32 format   u32 width       u32 height
/// u32 widthbytes (padded row stride)       u32 total size
/// u32 compressed size   u16 bits per pixel   u16 planes
/// ```
fn parse_thumbnail(data: &[u8], bgr: bool) -> Option<Thumbnail<'_>> {
    let head = data.get(..28)?;
    let u32_at = |i: usize| u32::from_be_bytes(head[i..i + 4].try_into().unwrap());
    if u32_at(0) != FORMAT_JPEG {
        return None;
    }
    let width = u32_at(4);
    let height = u32_at(8);
    let jpeg = &data[28..];
    // The declared compressed size is advisory: trust it only when it
    // fits, since a truncated resource is likelier than a padded one.
    let declared = u32_at(20) as usize;
    let jpeg = if declared > 0 && declared <= jpeg.len() {
        &jpeg[..declared]
    } else {
        jpeg
    };
    if width == 0 || height == 0 || jpeg.is_empty() {
        return None;
    }
    Some(Thumbnail {
        width,
        height,
        bgr,
        jpeg,
    })
}
