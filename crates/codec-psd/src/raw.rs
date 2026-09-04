//! Schist camera-raw payloads in a private additional-layer-info block.
//!
//! PSD has no portable representation for an editable camera capture.
//! `ScRw` keeps the original file and Schist's development settings beside
//! the rendered layer pixels. Other PSD readers ignore the private block;
//! Schist can reopen it and render from the sensor data again.

use schist_core::{Layer, RawDevelopment, RawSettings};
use std::sync::Arc;

/// Private block key: "Sc" for Schist, "Rw" for camera raw.
pub const RAW_BLOCK_KEY: [u8; 4] = *b"ScRw";

const VERSION: u32 = 1;
const SETTINGS_LEN: usize = 15;
/// A malformed block must not be able to make the reader allocate without
/// limit. This is well above the largest current still-camera capture.
pub(crate) const MAX_SOURCE_BYTES: usize = 1 << 30;

/// Serialize a RAW-backed layer, or `None` for an ordinary layer.
pub fn write_raw(layer: &Layer) -> Option<Vec<u8>> {
    let raw = layer.raw.as_deref()?;
    if raw.source.is_empty() || raw.source.len() > MAX_SOURCE_BYTES {
        return None;
    }
    let source_len = u32::try_from(raw.source.len()).ok()?;
    let mut out = Vec::with_capacity(8 + SETTINGS_LEN * 4 + raw.source.len());
    out.extend_from_slice(&VERSION.to_be_bytes());
    out.extend_from_slice(&source_len.to_be_bytes());
    for value in settings_values(raw.settings.sanitized()) {
        out.extend_from_slice(&value.to_be_bytes());
    }
    out.extend_from_slice(&raw.source);
    Some(out)
}

/// Parse a private RAW payload. A malformed payload is ignored because the
/// PSD still contains the last rendered pixels for the layer.
pub fn read_raw(data: &[u8]) -> Option<RawDevelopment> {
    let mut cursor = Cursor { data, at: 0 };
    if cursor.u32()? != VERSION {
        return None;
    }
    let source_len = cursor.u32()? as usize;
    if source_len == 0 || source_len > MAX_SOURCE_BYTES {
        return None;
    }
    let mut values = [0.0f32; SETTINGS_LEN];
    for value in &mut values {
        *value = cursor.f32()?;
        if !value.is_finite() {
            return None;
        }
    }
    let source = cursor.take(source_len)?;
    if cursor.at != data.len() {
        return None;
    }
    Some(RawDevelopment {
        source: Arc::from(source),
        settings: settings_from_values(values).sanitized(),
    })
}

fn settings_values(s: RawSettings) -> [f32; SETTINGS_LEN] {
    [
        s.temperature,
        s.tint,
        s.exposure,
        s.contrast,
        s.highlights,
        s.shadows,
        s.whites,
        s.blacks,
        s.clarity,
        s.dehaze,
        s.vibrance,
        s.saturation,
        s.sharpening,
        s.noise,
        s.vignette,
    ]
}

fn settings_from_values(v: [f32; SETTINGS_LEN]) -> RawSettings {
    RawSettings {
        temperature: v[0],
        tint: v[1],
        exposure: v[2],
        contrast: v[3],
        highlights: v[4],
        shadows: v[5],
        whites: v[6],
        blacks: v[7],
        clarity: v[8],
        dehaze: v[9],
        vibrance: v[10],
        saturation: v[11],
        sharpening: v[12],
        noise: v[13],
        vignette: v[14],
    }
}

struct Cursor<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, count: usize) -> Option<&'a [u8]> {
        let end = self.at.checked_add(count)?;
        let bytes = self.data.get(self.at..end)?;
        self.at = end;
        Some(bytes)
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_be_bytes(self.take(4)?.try_into().ok()?))
    }

    fn f32(&mut self) -> Option<f32> {
        Some(f32::from_be_bytes(self.take(4)?.try_into().ok()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_payload_round_trips_source_and_settings() {
        let mut layer = Layer::new_raster("capture");
        let settings = RawSettings {
            temperature: 18.0,
            tint: -7.0,
            exposure: 1.25,
            highlights: -32.0,
            shadows: 41.0,
            sharpening: 63.0,
            ..RawSettings::default()
        };
        layer.raw = Some(Box::new(RawDevelopment {
            source: Arc::from(&b"raw camera bytes"[..]),
            settings,
        }));

        let payload = write_raw(&layer).expect("RAW payload");
        let decoded = read_raw(&payload).expect("valid RAW payload");
        assert_eq!(decoded.source.as_ref(), b"raw camera bytes");
        assert_eq!(decoded.settings, settings);
    }

    #[test]
    fn malformed_raw_payload_is_ignored() {
        let mut layer = Layer::new_raster("capture");
        layer.raw = Some(Box::new(RawDevelopment {
            source: Arc::from(&b"source"[..]),
            settings: RawSettings::default(),
        }));
        let payload = write_raw(&layer).unwrap();

        assert!(read_raw(&payload[..payload.len() - 1]).is_none());
        let mut trailing = payload.clone();
        trailing.push(0);
        assert!(read_raw(&trailing).is_none());
        let mut nan = payload;
        nan[8..12].copy_from_slice(&f32::NAN.to_be_bytes());
        assert!(read_raw(&nan).is_none());
    }
}
