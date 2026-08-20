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

/// Colour mode of a document.
///
/// Pixels are always held as RGBA f32 -- one pipeline rather than five --
/// and converted at the boundaries: on open, on Image ▸ Mode, and on save.
/// So a CMYK file opens, edits and saves as CMYK, but the editing itself
/// happens in RGB. See `convert`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ColorMode {
    Rgb,
    Grayscale,
    /// Four-ink separation. Pixels are still edited as RGBA; the mode
    /// records how the file is stored and how it converts on save.
    Cmyk,
    /// CIELAB. Same arrangement as CMYK: converted at the boundaries.
    Lab,
    /// A palette of at most 256 colours.
    Indexed,
}

impl ColorMode {
    pub fn display_name(self) -> &'static str {
        match self {
            ColorMode::Rgb => "RGB Color",
            ColorMode::Grayscale => "Grayscale",
            ColorMode::Cmyk => "CMYK Color",
            ColorMode::Lab => "Lab Color",
            ColorMode::Indexed => "Indexed Color",
        }
    }

    /// How many colour channels a file in this mode stores.
    pub fn channels(self) -> usize {
        match self {
            ColorMode::Rgb | ColorMode::Lab => 3,
            ColorMode::Grayscale | ColorMode::Indexed => 1,
            ColorMode::Cmyk => 4,
        }
    }
}

/// Straight-alpha RGBA, f32 components in 0.0..=1.0.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Rgba {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Rgba {
    pub const TRANSPARENT: Rgba = Rgba {
        r: 0.0,
        g: 0.0,
        b: 0.0,
        a: 0.0,
    };
    pub const BLACK: Rgba = Rgba {
        r: 0.0,
        g: 0.0,
        b: 0.0,
        a: 1.0,
    };
    pub const WHITE: Rgba = Rgba {
        r: 1.0,
        g: 1.0,
        b: 1.0,
        a: 1.0,
    };

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
        let blend =
            |top_c: f32, bot_c: f32| (top_c * self.a + bot_c * bottom.a * (1.0 - self.a)) / a;
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

/// Conversions between the working RGB space and the other document
/// modes.
///
/// Photoslop edits in RGBA f32 whatever the document's mode says, and
/// converts at the boundaries: on open, on Image ▸ Mode, and on save. That
/// keeps one pipeline rather than four, at the cost of not editing CMYK
/// channels individually -- which is stated in the docs rather than
/// implied by silence.
pub mod convert {
    use super::Rgba;

    /// Naive CMYK, the same transform Photoshop calls "U.S. Web Coated"
    /// only in the loosest sense: no ink limits, no dot gain, no profile.
    /// Good enough to round-trip a file and to preview; not a substitute
    /// for a real separation, which is what the ICC path is for.
    pub fn rgb_to_cmyk(px: Rgba) -> [f32; 4] {
        let k = 1.0 - px.r.max(px.g).max(px.b);
        if k >= 1.0 - 1e-6 {
            return [0.0, 0.0, 0.0, 1.0];
        }
        let d = 1.0 - k;
        [
            (1.0 - px.r - k) / d,
            (1.0 - px.g - k) / d,
            (1.0 - px.b - k) / d,
            k,
        ]
    }

    pub fn cmyk_to_rgb(c: [f32; 4], alpha: f32) -> Rgba {
        let k = c[3].clamp(0.0, 1.0);
        let f = |v: f32| ((1.0 - v.clamp(0.0, 1.0)) * (1.0 - k)).clamp(0.0, 1.0);
        Rgba::new(f(c[0]), f(c[1]), f(c[2]), alpha)
    }

    /// sRGB companding, needed because Lab is defined on linear light.
    fn to_linear(v: f32) -> f32 {
        if v <= 0.04045 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    }

    fn from_linear(v: f32) -> f32 {
        if v <= 0.003_130_8 {
            v * 12.92
        } else {
            1.055 * v.powf(1.0 / 2.4) - 0.055
        }
    }

    /// D65 white point in XYZ.
    const WHITE: [f32; 3] = [0.950_47, 1.0, 1.088_83];

    fn lab_f(t: f32) -> f32 {
        const D: f32 = 6.0 / 29.0;
        if t > D * D * D {
            t.cbrt()
        } else {
            t / (3.0 * D * D) + 4.0 / 29.0
        }
    }

    fn lab_f_inv(t: f32) -> f32 {
        const D: f32 = 6.0 / 29.0;
        if t > D {
            t * t * t
        } else {
            3.0 * D * D * (t - 4.0 / 29.0)
        }
    }

    /// sRGB to CIELAB. `L` is 0..=100, `a` and `b` roughly -128..=127.
    pub fn rgb_to_lab(px: Rgba) -> [f32; 3] {
        let (r, g, b) = (to_linear(px.r), to_linear(px.g), to_linear(px.b));
        let x = (0.4124 * r + 0.3576 * g + 0.1805 * b) / WHITE[0];
        let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
        let z = (0.0193 * r + 0.1192 * g + 0.9505 * b) / WHITE[2];
        let (fx, fy, fz) = (lab_f(x), lab_f(y), lab_f(z));
        [116.0 * fy - 16.0, 500.0 * (fx - fy), 200.0 * (fy - fz)]
    }

    pub fn lab_to_rgb(lab: [f32; 3], alpha: f32) -> Rgba {
        let fy = (lab[0] + 16.0) / 116.0;
        let fx = fy + lab[1] / 500.0;
        let fz = fy - lab[2] / 200.0;
        let x = lab_f_inv(fx) * WHITE[0];
        let y = lab_f_inv(fy);
        let z = lab_f_inv(fz) * WHITE[2];
        let r = 3.2406 * x - 1.5372 * y - 0.4986 * z;
        let g = -0.9689 * x + 1.8758 * y + 0.0415 * z;
        let b = 0.0557 * x - 0.2040 * y + 1.0570 * z;
        Rgba::new(
            from_linear(r).clamp(0.0, 1.0),
            from_linear(g).clamp(0.0, 1.0),
            from_linear(b).clamp(0.0, 1.0),
            alpha,
        )
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn close(a: f32, b: f32, tol: f32, what: &str) {
            assert!((a - b).abs() < tol, "{what}: {a} != {b}");
        }

        #[test]
        fn cmyk_round_trips() {
            for px in [
                Rgba::new(0.0, 0.0, 0.0, 1.0),
                Rgba::new(1.0, 1.0, 1.0, 1.0),
                Rgba::new(0.2, 0.6, 0.9, 1.0),
                Rgba::new(0.8, 0.1, 0.4, 1.0),
            ] {
                let back = cmyk_to_rgb(rgb_to_cmyk(px), 1.0);
                close(back.r, px.r, 1e-4, "red");
                close(back.g, px.g, 1e-4, "green");
                close(back.b, px.b, 1e-4, "blue");
            }
        }

        #[test]
        fn lab_round_trips() {
            for px in [
                Rgba::new(0.0, 0.0, 0.0, 1.0),
                Rgba::new(1.0, 1.0, 1.0, 1.0),
                Rgba::new(0.2, 0.6, 0.9, 1.0),
                Rgba::new(0.75, 0.25, 0.05, 1.0),
            ] {
                let back = lab_to_rgb(rgb_to_lab(px), 1.0);
                close(back.r, px.r, 2e-3, "red");
                close(back.g, px.g, 2e-3, "green");
                close(back.b, px.b, 2e-3, "blue");
            }
        }

        #[test]
        fn lab_lightness_matches_expectations() {
            // Black is 0, white is 100, mid grey lands near 53 -- the
            // usual sanity check on a Lab implementation.
            close(
                rgb_to_lab(Rgba::new(0.0, 0.0, 0.0, 1.0))[0],
                0.0,
                0.01,
                "black",
            );
            close(
                rgb_to_lab(Rgba::new(1.0, 1.0, 1.0, 1.0))[0],
                100.0,
                0.01,
                "white",
            );
            close(
                rgb_to_lab(Rgba::new(0.5, 0.5, 0.5, 1.0))[0],
                53.4,
                1.0,
                "mid grey",
            );
        }

        #[test]
        fn a_neutral_colour_has_no_chroma() {
            let lab = rgb_to_lab(Rgba::new(0.5, 0.5, 0.5, 1.0));
            close(lab[1], 0.0, 0.01, "a");
            close(lab[2], 0.0, 0.01, "b");
        }

        #[test]
        fn pure_black_is_all_key() {
            assert_eq!(
                rgb_to_cmyk(Rgba::new(0.0, 0.0, 0.0, 1.0)),
                [0.0, 0.0, 0.0, 1.0]
            );
        }
    }
}
