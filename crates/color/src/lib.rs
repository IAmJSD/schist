//! Color primitives shared by the whole application.
//!
//! Canonical interchange format for compositing is straight-alpha RGBA in
//! f32, one component per channel, 0.0..=1.0. Documents *store* pixels at
//! their native depth (8/16/32 bits per channel); conversion to and from f32
//! happens at tile granularity in the compositor and tools.

use serde::{Deserialize, Serialize};

/// Bits per channel of a document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Depth {
    Eight,
    Sixteen,
    ThirtyTwo,
}

impl Depth {
    pub fn bytes_per_channel(self) -> usize {
        match self {
            Depth::Eight => 1,
            Depth::Sixteen => 2,
            Depth::ThirtyTwo => 4,
        }
    }
}

/// Color mode of a document. V1 edits RGB and Grayscale natively; other PSD
/// modes are converted on import.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ColorMode {
    Rgb,
    Grayscale,
}

/// Straight-alpha RGBA, f32 components in 0.0..=1.0.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rgba {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Rgba {
    pub const TRANSPARENT: Rgba = Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.0 };
    pub const BLACK: Rgba = Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 };
    pub const WHITE: Rgba = Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 };

    pub fn new(r: f32, g: f32, b: f32, a: f32) -> Self {
        Rgba { r, g, b, a }
    }

    pub fn from_u8(r: u8, g: u8, b: u8, a: u8) -> Self {
        Rgba {
            r: r as f32 / 255.0,
            g: g as f32 / 255.0,
            b: b as f32 / 255.0,
            a: a as f32 / 255.0,
        }
    }

    pub fn to_u8(self) -> [u8; 4] {
        [
            (self.r.clamp(0.0, 1.0) * 255.0 + 0.5) as u8,
            (self.g.clamp(0.0, 1.0) * 255.0 + 0.5) as u8,
            (self.b.clamp(0.0, 1.0) * 255.0 + 0.5) as u8,
            (self.a.clamp(0.0, 1.0) * 255.0 + 0.5) as u8,
        ]
    }

    /// Source-over composite of `self` over `bottom`, straight alpha.
    pub fn over(self, bottom: Rgba) -> Rgba {
        let a = self.a + bottom.a * (1.0 - self.a);
        if a <= f32::EPSILON {
            return Rgba::TRANSPARENT;
        }
        let blend = |top_c: f32, bot_c: f32| {
            (top_c * self.a + bot_c * bottom.a * (1.0 - self.a)) / a
        };
        Rgba {
            r: blend(self.r, bottom.r),
            g: blend(self.g, bottom.g),
            b: blend(self.b, bottom.b),
            a,
        }
    }
}

#[inline]
pub fn u8_to_f32(v: u8) -> f32 {
    v as f32 / 255.0
}

#[inline]
pub fn f32_to_u8(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
}

#[inline]
pub fn u16_to_f32(v: u16) -> f32 {
    // PSD 16-bit uses 0..=32768 internally, but we normalize storage to the
    // full u16 range and convert at the codec boundary.
    v as f32 / 65535.0
}

#[inline]
pub fn f32_to_u16(v: f32) -> u16 {
    (v.clamp(0.0, 1.0) * 65535.0 + 0.5) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn over_opaque_top_wins() {
        let top = Rgba::new(1.0, 0.0, 0.0, 1.0);
        let bottom = Rgba::new(0.0, 1.0, 0.0, 1.0);
        assert_eq!(top.over(bottom), top);
    }

    #[test]
    fn over_transparent_top_is_identity() {
        let bottom = Rgba::new(0.25, 0.5, 0.75, 0.8);
        let out = Rgba::TRANSPARENT.over(bottom);
        assert!((out.a - bottom.a).abs() < 1e-6);
        assert!((out.r - bottom.r).abs() < 1e-6);
    }

    #[test]
    fn u8_round_trip() {
        for v in 0..=255u8 {
            assert_eq!(f32_to_u8(u8_to_f32(v)), v);
        }
    }

    #[test]
    fn u16_round_trip() {
        for v in [0u16, 1, 32768, 65534, 65535] {
            assert_eq!(f32_to_u16(u16_to_f32(v)), v);
        }
    }
}
