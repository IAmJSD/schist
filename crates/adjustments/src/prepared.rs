//! Precomputing an adjustment into LUTs before a run over pixels.

use super::*;

/// A [`Params`] compiled for fast repeated application.
///
/// Channel-independent adjustments (levels, curves, invert, posterize,
/// threshold, brightness/contrast) collapse into per-channel lookup tables,
/// which turns a spline evaluation per pixel into two loads and a lerp.
/// Everything else keeps evaluating directly.
#[derive(Debug, Clone)]
pub enum Prepared {
    /// Three 256-entry tables (R, G, B) over the 0..1 input range.
    Lut(Box<[[f32; LUT_SIZE]; 3]>),
    Direct(Params),
    Fill(Rgba),
    Identity,
}

pub const LUT_SIZE: usize = 256;

impl Params {
    /// Compile for repeated use. Call once per composite pass, not per
    /// pixel.
    pub fn prepare(&self) -> Prepared {
        match self {
            Params::Unsupported => Prepared::Identity,
            Params::SolidColor { rgba } => {
                Prepared::Fill(Rgba::new(rgba[0], rgba[1], rgba[2], rgba[3]))
            }
            // Anything that mixes channels — reads luma, hue, or one
            // channel to write another — cannot be a per-channel LUT: a
            // LUT built from the grey ramp is exact on grey and wrong on
            // colour (a gradient map would send pure red to the two ends
            // of the ramp at once). Step functions (posterize, threshold)
            // must not be interpolated either, or the LUT rounds their
            // edges off.
            Params::HueSaturation { .. }
            | Params::BlackWhite { .. }
            | Params::Threshold { .. }
            | Params::Posterize { .. }
            | Params::ColorBalance { .. }
            | Params::Vibrance { .. }
            | Params::PhotoFilter { .. }
            | Params::GradientMap { .. }
            | Params::SelectiveColor { .. }
            | Params::ChannelMixer { .. }
            | Params::WhiteBalance { .. } => Prepared::Direct(self.clone()),
            _ => {
                let mut lut = Box::new([[0.0f32; LUT_SIZE]; 3]);
                for i in 0..LUT_SIZE {
                    let v = i as f32 / (LUT_SIZE - 1) as f32;
                    let out = self.apply(Rgba::new(v, v, v, 1.0));
                    lut[0][i] = out.r;
                    lut[1][i] = out.g;
                    lut[2][i] = out.b;
                }
                Prepared::Lut(lut)
            }
        }
    }
}

impl Prepared {
    pub fn is_identity(&self) -> bool {
        matches!(self, Prepared::Identity)
    }

    pub fn is_fill(&self) -> bool {
        matches!(self, Prepared::Fill(_))
    }

    /// Fill colour, for fill layers.
    pub fn fill_color(&self) -> Option<Rgba> {
        match self {
            Prepared::Fill(c) => Some(*c),
            _ => None,
        }
    }

    /// Apply to one pixel.
    #[inline]
    pub fn apply(&self, px: Rgba) -> Rgba {
        match self {
            Prepared::Identity => px,
            Prepared::Fill(c) => Rgba { a: px.a, ..*c },
            Prepared::Direct(p) => p.apply(px),
            Prepared::Lut(lut) => Rgba {
                r: sample_lut(&lut[0], px.r),
                g: sample_lut(&lut[1], px.g),
                b: sample_lut(&lut[2], px.b),
                a: px.a,
            },
        }
    }
}

#[inline]
pub(crate) fn sample_lut(lut: &[f32; LUT_SIZE], v: f32) -> f32 {
    let x = v.clamp(0.0, 1.0) * (LUT_SIZE - 1) as f32;
    let i = x as usize;
    if i >= LUT_SIZE - 1 {
        return lut[LUT_SIZE - 1];
    }
    // Linear interpolation keeps 16/32-bit inputs from banding.
    let f = x - i as f32;
    lut[i] + (lut[i + 1] - lut[i]) * f
}

#[cfg(test)]
mod prepared_tests {
    use super::*;

    #[test]
    fn prepared_matches_direct_application() {
        let cases = [
            Params::Invert,
            Params::Posterize { levels: 5 },
            Params::BrightnessContrast {
                brightness: 20.0,
                contrast: -30.0,
            },
            Params::Levels(Levels {
                rgb: LevelsChannel {
                    input_black: 0.1,
                    input_white: 0.9,
                    gamma: 1.4,
                    ..Default::default()
                },
                ..Default::default()
            }),
            Params::Curves(Curves {
                rgb: Curve {
                    points: vec![(0.0, 0.1), (0.5, 0.4), (1.0, 1.0)],
                },
                ..Default::default()
            }),
        ];
        for params in cases {
            let prepared = params.prepare();
            for i in 0..=20 {
                let v = i as f32 / 20.0;
                let px = Rgba::new(v, v * 0.5, 1.0 - v, 0.75);
                let direct = params.apply(px);
                let fast = prepared.apply(px);
                for (a, b) in [
                    (direct.r, fast.r),
                    (direct.g, fast.g),
                    (direct.b, fast.b),
                    (direct.a, fast.a),
                ] {
                    assert!((a - b).abs() < 0.01, "{params:?} at {v}: {a} vs {b}");
                }
            }
        }
    }

    #[test]
    fn step_functions_stay_direct_so_their_edges_stay_hard() {
        assert!(matches!(
            Params::Posterize { levels: 5 }.prepare(),
            Prepared::Direct(_)
        ));
        assert!(matches!(
            Params::Threshold { level: 0.5 }.prepare(),
            Prepared::Direct(_)
        ));
    }

    #[test]
    fn channel_mixing_adjustments_stay_direct() {
        let hue = Params::HueSaturation {
            hue: 40.0,
            saturation: 10.0,
            lightness: 0.0,
            colorize: false,
            lightness_desaturates: false,
            reciprocal_saturation: false,
            ranges: Vec::new(),
        };
        assert!(matches!(hue.prepare(), Prepared::Direct(_)));
        let px = Rgba::new(0.8, 0.2, 0.4, 1.0);
        assert_eq!(hue.prepare().apply(px), hue.apply(px));
    }

    #[test]
    fn unsupported_prepares_to_identity() {
        assert!(Params::Unsupported.prepare().is_identity());
    }

    #[test]
    fn fill_layers_expose_their_colour() {
        let fill = Params::SolidColor {
            rgba: [0.2, 0.4, 0.6, 1.0],
        }
        .prepare();
        assert!(fill.is_fill());
        assert_eq!(fill.fill_color().unwrap().g, 0.4);
    }
}
