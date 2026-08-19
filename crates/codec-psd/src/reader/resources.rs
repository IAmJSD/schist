//! Image Resources section: a list of "8BIM" resource blocks.
//!
//! Every block is preserved verbatim (id + raw pascal-name bytes + data)
//! into `Document::preserved_resources` in file order so the M6 writer can
//! re-emit them; on top of that we *interpret* the resolution (0x03ED) and
//! ICC profile (0x040F).

use super::cursor::Cursor;
use crate::error::PsdError;
use photoslop_core::PreservedResource;

pub const RES_RESOLUTION_INFO: u16 = 0x03ED;
pub const RES_ICC_PROFILE: u16 = 0x040F;

#[derive(Debug, Default)]
pub struct ParsedResources {
    pub preserved: Vec<PreservedResource>,
    pub resolution_dpi: Option<f32>,
    pub icc_profile: Option<Vec<u8>>,
}

pub fn parse_image_resources(cur: &mut Cursor) -> Result<ParsedResources, PsdError> {
    // Section length is always u32, even in PSB.
    let len = cur.u32()? as usize;
    let mut sec = cur.sub(len)?;
    let mut out = ParsedResources::default();

    // Each block: "8BIM", u16 id, pascal name (length byte + bytes, padded
    // so the whole name field is an even size), u32 data length, data
    // (padded to even — the *declared* length is the unpadded size).
    while sec.remaining() >= 4 {
        let sig = sec.sig4()?;
        if &sig != b"8BIM" {
            return Err(PsdError::Corrupt(format!(
                "image resource signature {:?} is not 8BIM",
                sig
            )));
        }
        let id = sec.u16()?;
        let name_len = sec.u8()? as usize;
        let mut name = Vec::with_capacity(name_len + 2);
        name.push(name_len as u8);
        name.extend_from_slice(sec.take(name_len)?);
        // Pad the (length byte + content) to an even total; we keep the pad
        // byte in `name` so the stored bytes are exactly what was on disk.
        if (1 + name_len) % 2 == 1 {
            name.push(sec.u8()?);
        }
        let data_len = sec.u32()? as usize;
        let data = sec.take(data_len)?.to_vec();
        if data_len % 2 == 1 && !sec.is_empty() {
            sec.skip(1)?; // pad byte (tolerate writers that omit the final one)
        }

        match id {
            RES_RESOLUTION_INFO if data.len() >= 4 => {
                // ResolutionInfo: hres as fixed-point 16.16 pixels-per-inch.
                let fixed = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
                out.resolution_dpi = Some(fixed as f32 / 65536.0);
            }
            RES_ICC_PROFILE => out.icc_profile = Some(data.clone()),
            _ => {}
        }

        out.preserved.push(PreservedResource { id, name, data });
    }
    // 1..=3 trailing bytes would be malformed padding; ignore them.
    Ok(out)
}
