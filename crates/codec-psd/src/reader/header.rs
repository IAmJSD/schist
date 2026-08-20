//! File header section (26 bytes).

use super::cursor::Cursor;
use crate::error::PsdError;
use photoslop_color::{ColorMode, Depth};

/// PSD colour mode numbers.
pub const MODE_BITMAP: u16 = 0;
pub const MODE_GRAYSCALE: u16 = 1;
pub const MODE_INDEXED: u16 = 2;
pub const MODE_RGB: u16 = 3;
pub const MODE_CMYK: u16 = 4;
pub const MODE_MULTICHANNEL: u16 = 7;
pub const MODE_DUOTONE: u16 = 8;
pub const MODE_LAB: u16 = 9;

#[derive(Debug, Clone)]
pub struct Header {
    /// True for PSB (version 2) files. PSB differs from PSD *only* in the
    /// header version, larger dimension limits, and certain length fields
    /// widening from 4 to 8 bytes (see `Cursor::len_psb` / `len_rle_row`).
    pub psb: bool,
    pub channels: u16,
    pub width: u32,
    pub height: u32,
    pub depth: Depth,
    pub mode: ColorMode,
}

impl Header {
    /// Number of color channels (before any alpha) for the color mode.
    pub fn base_channels(&self) -> u16 {
        self.mode.channels() as u16
    }
}

pub fn parse_header(cur: &mut Cursor) -> Result<Header, PsdError> {
    if cur.remaining() < 4 || cur.take(4)? != b"8BPS".as_slice() {
        return Err(PsdError::BadSignature);
    }
    let version = cur.u16()?;
    let psb = match version {
        1 => false,
        2 => true,
        v => return Err(PsdError::Corrupt(format!("unknown PSD version {v}"))),
    };
    // 6 reserved bytes; the spec says zero, but some writers don't comply,
    // so we only warn.
    let reserved = cur.take(6)?;
    if reserved.iter().any(|&b| b != 0) {
        log::warn!("PSD header reserved bytes are non-zero");
    }
    let channels = cur.u16()?;
    let height = cur.u32()?;
    let width = cur.u32()?;
    let depth_bits = cur.u16()?;
    let mode_num = cur.u16()?;

    if !(1..=56).contains(&channels) {
        return Err(PsdError::Corrupt(format!(
            "channel count {channels} out of range 1..=56"
        )));
    }
    // PSD caps dimensions at 30_000, PSB at 300_000.
    let max_dim: u32 = if psb { 300_000 } else { 30_000 };
    if width == 0 || height == 0 || width > max_dim || height > max_dim {
        return Err(PsdError::Corrupt(format!(
            "dimensions {width}x{height} out of range 1..={max_dim}"
        )));
    }

    let depth = match depth_bits {
        8 => Depth::Eight,
        16 => Depth::Sixteen,
        32 => Depth::ThirtyTwo,
        1 => {
            return Err(PsdError::Unsupported(
                "1-bit (Bitmap-depth) PSD files are not supported yet".into(),
            ))
        }
        d => return Err(PsdError::Corrupt(format!("invalid bit depth {d}"))),
    };

    let mode = match mode_num {
        MODE_RGB => ColorMode::Rgb,
        MODE_GRAYSCALE => ColorMode::Grayscale,
        MODE_CMYK => ColorMode::Cmyk,
        MODE_LAB => ColorMode::Lab,
        MODE_INDEXED => ColorMode::Indexed,
        // Duotone files store a single ink plane plus a colour table; the
        // plane on its own reads exactly like greyscale, which is what
        // Photoshop shows for a duotone with no table applied.
        MODE_DUOTONE => ColorMode::Grayscale,
        MODE_BITMAP => return Err(unsupported_mode("Bitmap")),
        MODE_MULTICHANNEL => return Err(unsupported_mode("Multichannel")),
        m => return Err(PsdError::Corrupt(format!("unknown color mode {m}"))),
    };

    Ok(Header {
        psb,
        channels,
        width,
        height,
        depth,
        mode,
    })
}

fn unsupported_mode(name: &str) -> PsdError {
    PsdError::Unsupported(format!(
        "{name} color mode is not supported yet (only RGB and Grayscale)"
    ))
}
