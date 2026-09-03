//! Non-destructive camera-raw source and development settings.
//!
//! The kernel does not decode camera files. It only carries the immutable
//! capture and the settings used to render a raster layer from it, in the
//! same way [`crate::SmartObject`] carries the source behind rendered smart
//! object pixels. Codecs and the app own the actual development pipeline.

use std::sync::Arc;

/// The editable controls for a camera-raw development.
///
/// Values use the ranges presented by the Camera Raw dialog: exposure is
/// measured in EV, sharpening is 0..=150, and the other controls are
/// generally -100..=100 (noise reduction is 0..=100). Keeping a typed,
/// fixed layout makes the document format stable even if the UI is moved or
/// the filter plug-in is unavailable.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct RawSettings {
    pub temperature: f32,
    pub tint: f32,
    pub exposure: f32,
    pub contrast: f32,
    pub highlights: f32,
    pub shadows: f32,
    pub whites: f32,
    pub blacks: f32,
    pub clarity: f32,
    pub dehaze: f32,
    pub vibrance: f32,
    pub saturation: f32,
    pub sharpening: f32,
    pub noise: f32,
    pub vignette: f32,
}

impl RawSettings {
    /// Replace non-finite values with neutral settings and constrain every
    /// control to the range the editor exposes. PSD blocks are untrusted
    /// input, and public callers need the same guarantee as the UI.
    pub fn sanitized(self) -> RawSettings {
        let signed = |value: f32| {
            if value.is_finite() {
                value.clamp(-100.0, 100.0)
            } else {
                0.0
            }
        };
        let unsigned = |value: f32, max: f32| {
            if value.is_finite() {
                value.clamp(0.0, max)
            } else {
                0.0
            }
        };
        RawSettings {
            temperature: signed(self.temperature),
            tint: signed(self.tint),
            exposure: if self.exposure.is_finite() {
                self.exposure.clamp(-5.0, 5.0)
            } else {
                0.0
            },
            contrast: signed(self.contrast),
            highlights: signed(self.highlights),
            shadows: signed(self.shadows),
            whites: signed(self.whites),
            blacks: signed(self.blacks),
            clarity: signed(self.clarity),
            dehaze: signed(self.dehaze),
            vibrance: signed(self.vibrance),
            saturation: signed(self.saturation),
            sharpening: unsigned(self.sharpening, 150.0),
            noise: unsigned(self.noise, 100.0),
            vignette: signed(self.vignette),
        }
    }
}

/// The original capture behind a rendered RAW layer.
///
/// `Arc` makes history snapshots and live previews cheap: all of them share
/// one immutable copy of a file that may be hundreds of megabytes.
#[derive(Debug, Clone)]
pub struct RawDevelopment {
    pub source: Arc<[u8]>,
    pub settings: RawSettings,
}

impl PartialEq for RawDevelopment {
    fn eq(&self, other: &Self) -> bool {
        // Settings almost always differ during an edit, so compare them
        // before considering a potentially enormous byte slice. History
        // snapshots normally share the Arc and take the pointer-fast path.
        self.settings == other.settings
            && (Arc::ptr_eq(&self.source, &other.source) || self.source == other.source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_are_finite_and_bounded_at_the_model_boundary() {
        let settings = RawSettings {
            temperature: f32::NAN,
            exposure: 90.0,
            contrast: -500.0,
            sharpening: 500.0,
            noise: -4.0,
            ..RawSettings::default()
        }
        .sanitized();
        assert_eq!(settings.temperature, 0.0);
        assert_eq!(settings.exposure, 5.0);
        assert_eq!(settings.contrast, -100.0);
        assert_eq!(settings.sharpening, 150.0);
        assert_eq!(settings.noise, 0.0);
    }
}
