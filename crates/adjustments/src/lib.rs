//! Adjustment parameters, their pixel math, and PSD payload parsing (M8).
//!
//! An adjustment layer modifies whatever is beneath it rather than carrying
//! pixels of its own. The compositor asks this crate for a [`Params`] value
//! (parsed from the layer's preserved PSD payload, or from our own JSON when
//! the user created it) and applies it to the backdrop.
//!
//! Colour math runs on straight-alpha RGB in 0..1; alpha is never touched —
//! an adjustment recolours, it doesn't reshape coverage.

pub mod descriptor;

use photoslop_color::Rgba;
use photoslop_core::AdjustmentKind;

/// A tone curve as up to 16 control points in 0..1, evaluated with
/// monotone-ish Catmull-Rom interpolation and cached into a LUT.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Curve {
    pub points: Vec<(f32, f32)>,
}

impl Default for Curve {
    fn default() -> Self {
        Curve {
            points: vec![(0.0, 0.0), (1.0, 1.0)],
        }
    }
}

impl Curve {
    pub fn is_identity(&self) -> bool {
        self.points.len() == 2
            && (self.points[0].0 - self.points[0].1).abs() < 1e-4
            && (self.points[1].0 - self.points[1].1).abs() < 1e-4
    }

    /// Evaluate at `x` (0..1) with linear interpolation between the sorted
    /// control points, clamped outside their range.
    pub fn eval(&self, x: f32) -> f32 {
        if self.points.is_empty() {
            return x;
        }
        let x = x.clamp(0.0, 1.0);
        let pts = &self.points;
        if x <= pts[0].0 {
            return pts[0].1.clamp(0.0, 1.0);
        }
        for i in 0..pts.len() - 1 {
            let (x0, y0) = pts[i];
            let (x1, y1) = pts[i + 1];
            if x > x1 {
                continue;
            }
            let t = if (x1 - x0).abs() < 1e-6 {
                0.0
            } else {
                (x - x0) / (x1 - x0)
            };
            // Two points describe a straight ramp — interpolate linearly so
            // the default curve is exactly the identity. Longer curves get
            // Catmull-Rom through the control points for a smooth shape.
            if pts.len() == 2 {
                return (y0 + (y1 - y0) * t).clamp(0.0, 1.0);
            }
            let ym1 = pts[i.saturating_sub(1)].1;
            let y2 = pts[(i + 2).min(pts.len() - 1)].1;
            let t2 = t * t;
            let t3 = t2 * t;
            let y = 0.5
                * ((2.0 * y0)
                    + (-ym1 + y1) * t
                    + (2.0 * ym1 - 5.0 * y0 + 4.0 * y1 - y2) * t2
                    + (-ym1 + 3.0 * y0 - 3.0 * y1 + y2) * t3);
            return y.clamp(0.0, 1.0);
        }
        pts[pts.len() - 1].1.clamp(0.0, 1.0)
    }
}

/// Per-channel levels: input black/white with gamma, remapped to an output
/// range.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LevelsChannel {
    pub input_black: f32,
    pub input_white: f32,
    pub gamma: f32,
    pub output_black: f32,
    pub output_white: f32,
}

impl Default for LevelsChannel {
    fn default() -> Self {
        LevelsChannel {
            input_black: 0.0,
            input_white: 1.0,
            gamma: 1.0,
            output_black: 0.0,
            output_white: 1.0,
        }
    }
}

impl LevelsChannel {
    pub fn is_identity(&self) -> bool {
        self.input_black == 0.0
            && self.input_white == 1.0
            && (self.gamma - 1.0).abs() < 1e-4
            && self.output_black == 0.0
            && self.output_white == 1.0
    }

    pub fn apply(&self, v: f32) -> f32 {
        let span = (self.input_white - self.input_black).max(1e-4);
        let t = ((v - self.input_black) / span).clamp(0.0, 1.0);
        let t = if (self.gamma - 1.0).abs() < 1e-4 {
            t
        } else {
            t.powf(1.0 / self.gamma.max(1e-3))
        };
        (self.output_black + t * (self.output_white - self.output_black)).clamp(0.0, 1.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub struct Levels {
    pub rgb: LevelsChannel,
    pub red: LevelsChannel,
    pub green: LevelsChannel,
    pub blue: LevelsChannel,
}

#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub struct Curves {
    pub rgb: Curve,
    pub red: Curve,
    pub green: Curve,
    pub blue: Curve,
}

/// Everything an adjustment layer can do.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Params {
    Levels(Levels),
    Curves(Curves),
    HueSaturation {
        /// -180..180 degrees.
        hue: f32,
        /// -100..100.
        saturation: f32,
        /// -100..100.
        lightness: f32,
        colorize: bool,
    },
    BrightnessContrast {
        /// -100..100.
        brightness: f32,
        /// -100..100.
        contrast: f32,
    },
    BlackWhite {
        reds: f32,
        yellows: f32,
        greens: f32,
        cyans: f32,
        blues: f32,
        magentas: f32,
    },
    Invert,
    Posterize {
        levels: u32,
    },
    Threshold {
        /// 0..1.
        level: f32,
    },
    SolidColor {
        rgba: [f32; 4],
    },
    /// Recognised kind whose payload we couldn't parse: renders as a no-op
    /// but keeps its raw bytes for round-trip.
    Unsupported,
}

impl Params {
    /// A sensible starting point when the user adds this adjustment.
    pub fn default_for(kind: AdjustmentKind) -> Params {
        match kind {
            AdjustmentKind::Levels => Params::Levels(Levels::default()),
            AdjustmentKind::Curves => Params::Curves(Curves::default()),
            AdjustmentKind::HueSaturation => Params::HueSaturation {
                hue: 0.0,
                saturation: 0.0,
                lightness: 0.0,
                colorize: false,
            },
            AdjustmentKind::BrightnessContrast => Params::BrightnessContrast {
                brightness: 0.0,
                contrast: 0.0,
            },
            AdjustmentKind::BlackWhite => Params::BlackWhite {
                reds: 40.0,
                yellows: 60.0,
                greens: 40.0,
                cyans: 60.0,
                blues: 20.0,
                magentas: 80.0,
            },
            AdjustmentKind::Invert => Params::Invert,
            AdjustmentKind::Posterize => Params::Posterize { levels: 4 },
            AdjustmentKind::Threshold => Params::Threshold { level: 0.5 },
            AdjustmentKind::SolidColor => Params::SolidColor {
                rgba: [0.0, 0.0, 0.0, 1.0],
            },
            _ => Params::Unsupported,
        }
    }

    pub fn kind(&self) -> AdjustmentKind {
        match self {
            Params::Levels(_) => AdjustmentKind::Levels,
            Params::Curves(_) => AdjustmentKind::Curves,
            Params::HueSaturation { .. } => AdjustmentKind::HueSaturation,
            Params::BrightnessContrast { .. } => AdjustmentKind::BrightnessContrast,
            Params::BlackWhite { .. } => AdjustmentKind::BlackWhite,
            Params::Invert => AdjustmentKind::Invert,
            Params::Posterize { .. } => AdjustmentKind::Posterize,
            Params::Threshold { .. } => AdjustmentKind::Threshold,
            Params::SolidColor { .. } => AdjustmentKind::SolidColor,
            Params::Unsupported => AdjustmentKind::Other(*b"____"),
        }
    }

    /// Adjustments that ignore the backdrop entirely (fill layers).
    pub fn is_fill(&self) -> bool {
        matches!(self, Params::SolidColor { .. })
    }

    /// Apply to one pixel. Alpha passes through unchanged.
    pub fn apply(&self, px: Rgba) -> Rgba {
        match self {
            Params::Levels(l) => {
                let f = |v: f32, ch: &LevelsChannel| l.rgb.apply(ch.apply(v));
                Rgba {
                    r: f(px.r, &l.red),
                    g: f(px.g, &l.green),
                    b: f(px.b, &l.blue),
                    a: px.a,
                }
            }
            Params::Curves(c) => Rgba {
                r: c.rgb.eval(c.red.eval(px.r)),
                g: c.rgb.eval(c.green.eval(px.g)),
                b: c.rgb.eval(c.blue.eval(px.b)),
                a: px.a,
            },
            Params::HueSaturation {
                hue,
                saturation,
                lightness,
                colorize,
            } => {
                let (h, s, l) = rgb_to_hsl(px.r, px.g, px.b);
                let (nh, ns, nl) = if *colorize {
                    (
                        hue.rem_euclid(360.0),
                        (saturation / 100.0).clamp(0.0, 1.0),
                        adjust_lightness(l, *lightness),
                    )
                } else {
                    (
                        (h + hue).rem_euclid(360.0),
                        (s * (1.0 + saturation / 100.0)).clamp(0.0, 1.0),
                        adjust_lightness(l, *lightness),
                    )
                };
                let (r, g, b) = hsl_to_rgb(nh, ns, nl);
                Rgba { r, g, b, a: px.a }
            }
            Params::BrightnessContrast {
                brightness,
                contrast,
            } => {
                let b = brightness / 100.0;
                // Photoshop's contrast slider is roughly a pivoted scale
                // about mid-grey, steepening sharply near +100.
                let c = if *contrast >= 0.0 {
                    1.0 / (1.0 - (contrast / 100.0) * 0.99).max(1e-3)
                } else {
                    1.0 + contrast / 100.0
                };
                let f = |v: f32| (((v + b) - 0.5) * c + 0.5).clamp(0.0, 1.0);
                Rgba {
                    r: f(px.r),
                    g: f(px.g),
                    b: f(px.b),
                    a: px.a,
                }
            }
            Params::BlackWhite {
                reds,
                yellows,
                greens,
                cyans,
                blues,
                magentas,
            } => {
                // Weight each colour region by how much of it the pixel
                // contains, following Photoshop's six-slider model.
                let (r, g, b) = (px.r, px.g, px.b);
                let max = r.max(g).max(b);
                let min = r.min(g).min(b);
                let mid = r + g + b - max - min;
                let w = |v: f32| v / 100.0;
                // Which region dominates depends on the channel ordering.
                let gray = if max <= min + 1e-6 {
                    max
                } else if r >= g && g >= b {
                    // red -> yellow
                    let t = (mid - min) / (max - min);
                    min + (max - min) * (w(*reds) * (1.0 - t) + w(*yellows) * t)
                } else if g >= r && r >= b {
                    let t = (mid - min) / (max - min);
                    min + (max - min) * (w(*greens) * (1.0 - t) + w(*yellows) * t)
                } else if g >= b && b >= r {
                    let t = (mid - min) / (max - min);
                    min + (max - min) * (w(*greens) * (1.0 - t) + w(*cyans) * t)
                } else if b >= g && g >= r {
                    let t = (mid - min) / (max - min);
                    min + (max - min) * (w(*blues) * (1.0 - t) + w(*cyans) * t)
                } else if b >= r && r >= g {
                    let t = (mid - min) / (max - min);
                    min + (max - min) * (w(*blues) * (1.0 - t) + w(*magentas) * t)
                } else {
                    let t = (mid - min) / (max - min);
                    min + (max - min) * (w(*reds) * (1.0 - t) + w(*magentas) * t)
                };
                let v = gray.clamp(0.0, 1.0);
                Rgba {
                    r: v,
                    g: v,
                    b: v,
                    a: px.a,
                }
            }
            Params::Invert => Rgba {
                r: 1.0 - px.r,
                g: 1.0 - px.g,
                b: 1.0 - px.b,
                a: px.a,
            },
            Params::Posterize { levels } => {
                let n = (*levels).clamp(2, 255) as f32;
                let f = |v: f32| ((v * (n - 1.0)).round() / (n - 1.0)).clamp(0.0, 1.0);
                Rgba {
                    r: f(px.r),
                    g: f(px.g),
                    b: f(px.b),
                    a: px.a,
                }
            }
            Params::Threshold { level } => {
                let lum = 0.3 * px.r + 0.59 * px.g + 0.11 * px.b;
                let v = if lum >= *level { 1.0 } else { 0.0 };
                Rgba {
                    r: v,
                    g: v,
                    b: v,
                    a: px.a,
                }
            }
            Params::SolidColor { rgba } => Rgba {
                r: rgba[0],
                g: rgba[1],
                b: rgba[2],
                a: px.a,
            },
            Params::Unsupported => px,
        }
    }

    /// Apply across a straight-alpha f32 RGBA buffer in place.
    pub fn apply_buffer(&self, pixels: &mut [f32]) {
        if matches!(self, Params::Unsupported) {
            return;
        }
        for px in pixels.chunks_exact_mut(4) {
            let out = self.apply(Rgba::new(px[0], px[1], px[2], px[3]));
            px[0] = out.r;
            px[1] = out.g;
            px[2] = out.b;
            px[3] = out.a;
        }
    }

    pub fn display_name(&self) -> &'static str {
        self.kind().display_name()
    }
}

fn adjust_lightness(l: f32, amount: f32) -> f32 {
    if amount >= 0.0 {
        l + (1.0 - l) * (amount / 100.0)
    } else {
        l * (1.0 + amount / 100.0)
    }
    .clamp(0.0, 1.0)
}

fn rgb_to_hsl(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    if (max - min).abs() < 1e-6 {
        return (0.0, 0.0, l);
    }
    let d = max - min;
    let s = if l > 0.5 {
        d / (2.0 - max - min)
    } else {
        d / (max + min)
    };
    let h = if max == r {
        60.0 * (((g - b) / d) % 6.0)
    } else if max == g {
        60.0 * ((b - r) / d + 2.0)
    } else {
        60.0 * ((r - g) / d + 4.0)
    };
    (h.rem_euclid(360.0), s, l)
}

fn hsl_to_rgb(h: f32, s: f32, l: f32) -> (f32, f32, f32) {
    if s <= 1e-6 {
        return (l, l, l);
    }
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = h.rem_euclid(360.0) / 60.0;
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r1, g1, b1) = match hp as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    (
        (r1 + m).clamp(0.0, 1.0),
        (g1 + m).clamp(0.0, 1.0),
        (b1 + m).clamp(0.0, 1.0),
    )
}

// ===== PSD payload parsing =====

fn be_u16(d: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(d.get(at..at + 2)?.try_into().ok()?))
}

fn be_i16(d: &[u8], at: usize) -> Option<i16> {
    Some(i16::from_be_bytes(d.get(at..at + 2)?.try_into().ok()?))
}

/// Decode an adjustment layer's PSD payload into parameters.
///
/// Returns `Params::Unsupported` for kinds whose payload we can't read yet,
/// which renders as a no-op while the raw bytes stay preserved for saving.
pub fn parse_psd(kind: AdjustmentKind, raw: &[u8]) -> Params {
    match kind {
        AdjustmentKind::Invert => Params::Invert,
        AdjustmentKind::Posterize => be_u16(raw, 0)
            .map(|levels| Params::Posterize {
                levels: levels as u32,
            })
            .unwrap_or(Params::Unsupported),
        AdjustmentKind::Threshold => be_u16(raw, 0)
            .map(|level| Params::Threshold {
                level: level as f32 / 255.0,
            })
            .unwrap_or(Params::Unsupported),
        AdjustmentKind::BrightnessContrast => parse_brightness(raw),
        AdjustmentKind::Levels => parse_levels(raw),
        AdjustmentKind::HueSaturation => parse_hue_sat(raw),
        AdjustmentKind::Curves => parse_curves(raw),
        AdjustmentKind::BlackWhite => parse_black_white(raw),
        AdjustmentKind::SolidColor => parse_solid_color(raw),
        _ => Params::Unsupported,
    }
}

fn parse_brightness(raw: &[u8]) -> Params {
    // Legacy 'brit': brightness i16, contrast i16, mean i16, lab u8.
    match (be_i16(raw, 0), be_i16(raw, 2)) {
        (Some(b), Some(c)) => Params::BrightnessContrast {
            brightness: b as f32,
            contrast: c as f32,
        },
        _ => Params::Unsupported,
    }
}

fn parse_levels(raw: &[u8]) -> Params {
    // Version u16, then 29 records of 5 u16s: input black/white, output
    // black/white, gamma*100.
    if be_u16(raw, 0).is_none() {
        return Params::Unsupported;
    }
    let record = |i: usize| -> Option<LevelsChannel> {
        let at = 2 + i * 10;
        Some(LevelsChannel {
            input_black: be_u16(raw, at)? as f32 / 255.0,
            input_white: be_u16(raw, at + 2)? as f32 / 255.0,
            output_black: be_u16(raw, at + 4)? as f32 / 255.0,
            output_white: be_u16(raw, at + 6)? as f32 / 255.0,
            gamma: (be_u16(raw, at + 8)? as f32 / 100.0).max(0.01),
        })
    };
    match (record(0), record(1), record(2), record(3)) {
        (Some(rgb), Some(red), Some(green), Some(blue)) => Params::Levels(Levels {
            rgb,
            red,
            green,
            blue,
        }),
        _ => Params::Unsupported,
    }
}

fn parse_hue_sat(raw: &[u8]) -> Params {
    // 'hue2': version u16, colorize u16, then master hue/sat/lightness i16.
    let colorize = be_u16(raw, 2).map(|v| v != 0).unwrap_or(false);
    match (be_i16(raw, 4), be_i16(raw, 6), be_i16(raw, 8)) {
        (Some(h), Some(s), Some(l)) => Params::HueSaturation {
            hue: h as f32,
            saturation: s as f32,
            lightness: l as f32,
            colorize,
        },
        _ => Params::Unsupported,
    }
}

fn parse_curves(raw: &[u8]) -> Params {
    // Legacy 'curv': u8 padding, u16 version, u32 channel bitmap, then per
    // channel: u16 point count and (output, input) u16 pairs.
    let version = be_u16(raw, 1).unwrap_or(0);
    if version != 1 && version != 4 {
        // Modern files store curves in a descriptor after the legacy block;
        // reading that is future work, so leave the layer a no-op.
        return Params::Unsupported;
    }
    let bitmap = match raw.get(3..7) {
        Some(b) => u32::from_be_bytes([b[0], b[1], b[2], b[3]]),
        None => return Params::Unsupported,
    };
    let mut at = 7;
    let mut curves = Curves::default();
    for channel in 0..4u32 {
        if bitmap & (1 << channel) == 0 {
            continue;
        }
        let Some(count) = be_u16(raw, at) else {
            break;
        };
        at += 2;
        let mut points = Vec::new();
        for _ in 0..count.min(32) {
            let (Some(out), Some(inp)) = (be_u16(raw, at), be_u16(raw, at + 2)) else {
                break;
            };
            at += 4;
            points.push((inp as f32 / 255.0, out as f32 / 255.0));
        }
        points.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        if points.len() < 2 {
            continue;
        }
        let curve = Curve { points };
        match channel {
            0 => curves.rgb = curve,
            1 => curves.red = curve,
            2 => curves.green = curve,
            _ => curves.blue = curve,
        }
    }
    Params::Curves(curves)
}

fn parse_black_white(raw: &[u8]) -> Params {
    let Some(d) = descriptor::parse_versioned(raw).or_else(|| descriptor::parse(raw)) else {
        return Params::Unsupported;
    };
    let get = |k: &str, default: f32| d.number(k).map(|v| v as f32).unwrap_or(default);
    Params::BlackWhite {
        reds: get("Rd  ", 40.0),
        yellows: get("Yllw", 60.0),
        greens: get("Grn ", 40.0),
        cyans: get("Cyn ", 60.0),
        blues: get("Bl  ", 20.0),
        magentas: get("Mgnt", 80.0),
    }
}

fn parse_solid_color(raw: &[u8]) -> Params {
    let Some(d) = descriptor::parse_versioned(raw).or_else(|| descriptor::parse(raw)) else {
        return Params::Unsupported;
    };
    let Some(color) = d.get("Clr ").and_then(|v| v.as_object()) else {
        return Params::Unsupported;
    };
    // RGB colours are stored 0..255 per channel.
    let ch = |k: &str| color.number(k).unwrap_or(0.0) as f32 / 255.0;
    Params::SolidColor {
        rgba: [ch("Rd  "), ch("Grn "), ch("Bl  "), 1.0],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px(r: f32, g: f32, b: f32) -> Rgba {
        Rgba::new(r, g, b, 1.0)
    }

    #[test]
    fn invert_flips_channels_not_alpha() {
        let out = Params::Invert.apply(Rgba::new(0.25, 0.5, 1.0, 0.4));
        assert!((out.r - 0.75).abs() < 1e-5);
        assert!((out.b - 0.0).abs() < 1e-5);
        assert_eq!(out.a, 0.4, "alpha untouched");
    }

    #[test]
    fn levels_black_and_white_points_stretch_contrast() {
        let params = Params::Levels(Levels {
            rgb: LevelsChannel {
                input_black: 0.25,
                input_white: 0.75,
                ..Default::default()
            },
            ..Default::default()
        });
        assert!(
            params.apply(px(0.25, 0.25, 0.25)).r.abs() < 1e-4,
            "black point"
        );
        assert!(
            (params.apply(px(0.75, 0.75, 0.75)).r - 1.0).abs() < 1e-4,
            "white point"
        );
        let mid = params.apply(px(0.5, 0.5, 0.5)).r;
        assert!((mid - 0.5).abs() < 0.02, "midpoint stays mid: {mid}");
    }

    #[test]
    fn levels_gamma_lifts_midtones() {
        let params = Params::Levels(Levels {
            rgb: LevelsChannel {
                gamma: 2.0,
                ..Default::default()
            },
            ..Default::default()
        });
        let out = params.apply(px(0.5, 0.5, 0.5)).r;
        assert!(out > 0.6, "gamma 2.0 brightens midtones: {out}");
        assert!(
            params.apply(px(0.0, 0.0, 0.0)).r.abs() < 1e-5,
            "black stays"
        );
        assert!(
            (params.apply(px(1.0, 1.0, 1.0)).r - 1.0).abs() < 1e-5,
            "white stays"
        );
    }

    #[test]
    fn curves_identity_is_a_no_op() {
        let params = Params::Curves(Curves::default());
        for v in [0.0, 0.25, 0.5, 1.0] {
            assert!((params.apply(px(v, v, v)).r - v).abs() < 1e-3, "v={v}");
        }
    }

    #[test]
    fn curves_lift_darkens_or_brightens() {
        let params = Params::Curves(Curves {
            rgb: Curve {
                points: vec![(0.0, 0.2), (1.0, 1.0)],
            },
            ..Default::default()
        });
        assert!(params.apply(px(0.0, 0.0, 0.0)).r > 0.15, "blacks lifted");
        assert!((params.apply(px(1.0, 1.0, 1.0)).r - 1.0).abs() < 1e-3);
    }

    #[test]
    fn hue_rotation_moves_red_toward_green() {
        let params = Params::HueSaturation {
            hue: 120.0,
            saturation: 0.0,
            lightness: 0.0,
            colorize: false,
        };
        let out = params.apply(px(1.0, 0.0, 0.0));
        assert!(out.g > 0.9 && out.r < 0.1, "{out:?}");
    }

    #[test]
    fn saturation_minus_100_is_grayscale() {
        let params = Params::HueSaturation {
            hue: 0.0,
            saturation: -100.0,
            lightness: 0.0,
            colorize: false,
        };
        let out = params.apply(px(0.8, 0.2, 0.4));
        assert!(
            (out.r - out.g).abs() < 1e-4 && (out.g - out.b).abs() < 1e-4,
            "{out:?}"
        );
    }

    #[test]
    fn brightness_and_contrast_move_the_expected_way() {
        let bright = Params::BrightnessContrast {
            brightness: 50.0,
            contrast: 0.0,
        };
        assert!(bright.apply(px(0.5, 0.5, 0.5)).r > 0.9);

        let contrast = Params::BrightnessContrast {
            brightness: 0.0,
            contrast: 50.0,
        };
        assert!(contrast.apply(px(0.6, 0.6, 0.6)).r > 0.6, "lights lighten");
        assert!(contrast.apply(px(0.4, 0.4, 0.4)).r < 0.4, "darks darken");
        assert!(
            (contrast.apply(px(0.5, 0.5, 0.5)).r - 0.5).abs() < 1e-4,
            "mid-grey is the pivot"
        );
    }

    #[test]
    fn posterize_quantizes_to_the_requested_levels() {
        let params = Params::Posterize { levels: 2 };
        for v in [0.0, 0.2, 0.49] {
            assert_eq!(params.apply(px(v, v, v)).r, 0.0, "v={v}");
        }
        for v in [0.51, 0.8, 1.0] {
            assert_eq!(params.apply(px(v, v, v)).r, 1.0, "v={v}");
        }
    }

    #[test]
    fn threshold_splits_on_luminance() {
        let params = Params::Threshold { level: 0.5 };
        assert_eq!(params.apply(px(0.9, 0.9, 0.9)).r, 1.0);
        assert_eq!(params.apply(px(0.1, 0.1, 0.1)).r, 0.0);
    }

    #[test]
    fn black_white_is_gray_and_weights_colors_differently() {
        let params = Params::BlackWhite {
            reds: 40.0,
            yellows: 60.0,
            greens: 40.0,
            cyans: 60.0,
            blues: 20.0,
            magentas: 80.0,
        };
        let red = params.apply(px(1.0, 0.0, 0.0));
        let blue = params.apply(px(0.0, 0.0, 1.0));
        assert!(
            (red.r - red.g).abs() < 1e-5 && (red.g - red.b).abs() < 1e-5,
            "gray"
        );
        assert!(red.r > blue.r, "reds map lighter than blues by default");
    }

    #[test]
    fn solid_color_replaces_rgb_but_keeps_alpha() {
        let params = Params::SolidColor {
            rgba: [1.0, 0.0, 0.5, 1.0],
        };
        let out = params.apply(Rgba::new(0.2, 0.2, 0.2, 0.5));
        assert_eq!((out.r, out.g, out.b), (1.0, 0.0, 0.5));
        assert_eq!(out.a, 0.5);
    }

    #[test]
    fn apply_buffer_matches_per_pixel() {
        let params = Params::Invert;
        let mut buf = vec![0.25f32, 0.5, 0.75, 1.0, 0.0, 0.0, 0.0, 0.5];
        params.apply_buffer(&mut buf);
        assert!((buf[0] - 0.75).abs() < 1e-5);
        assert_eq!(buf[3], 1.0);
        assert!((buf[4] - 1.0).abs() < 1e-5);
        assert_eq!(buf[7], 0.5);
    }

    // --- PSD payload parsing ---

    #[test]
    fn parses_posterize_threshold_and_invert() {
        assert_eq!(
            parse_psd(AdjustmentKind::Posterize, &6u16.to_be_bytes()),
            Params::Posterize { levels: 6 }
        );
        match parse_psd(AdjustmentKind::Threshold, &128u16.to_be_bytes()) {
            Params::Threshold { level } => assert!((level - 0.502).abs() < 0.01),
            other => panic!("{other:?}"),
        }
        assert_eq!(parse_psd(AdjustmentKind::Invert, &[]), Params::Invert);
    }

    #[test]
    fn parses_brightness_contrast() {
        let mut raw = Vec::new();
        raw.extend_from_slice(&(-20i16).to_be_bytes());
        raw.extend_from_slice(&30i16.to_be_bytes());
        raw.extend_from_slice(&0i16.to_be_bytes());
        raw.push(0);
        assert_eq!(
            parse_psd(AdjustmentKind::BrightnessContrast, &raw),
            Params::BrightnessContrast {
                brightness: -20.0,
                contrast: 30.0
            }
        );
    }

    #[test]
    fn parses_levels_records() {
        let mut raw = 2u16.to_be_bytes().to_vec(); // version
        for (ib, iw, ob, ow, gamma) in [
            (10u16, 245u16, 0u16, 255u16, 120u16),
            (0, 255, 0, 255, 100),
            (0, 255, 0, 255, 100),
            (0, 255, 0, 255, 100),
        ] {
            for v in [ib, iw, ob, ow, gamma] {
                raw.extend_from_slice(&v.to_be_bytes());
            }
        }
        match parse_psd(AdjustmentKind::Levels, &raw) {
            Params::Levels(l) => {
                assert!((l.rgb.input_black - 10.0 / 255.0).abs() < 1e-4);
                assert!((l.rgb.gamma - 1.2).abs() < 1e-4);
                assert!(l.red.is_identity());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn parses_legacy_curves() {
        // padding, version 1, bitmap = RGB only, 2 points.
        let mut raw = vec![0u8];
        raw.extend_from_slice(&1u16.to_be_bytes());
        raw.extend_from_slice(&1u32.to_be_bytes());
        raw.extend_from_slice(&2u16.to_be_bytes());
        for (out, inp) in [(50u16, 0u16), (255, 255)] {
            raw.extend_from_slice(&out.to_be_bytes());
            raw.extend_from_slice(&inp.to_be_bytes());
        }
        match parse_psd(AdjustmentKind::Curves, &raw) {
            Params::Curves(c) => {
                assert_eq!(c.rgb.points.len(), 2);
                assert!((c.rgb.points[0].1 - 50.0 / 255.0).abs() < 1e-4);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn unparseable_payloads_become_no_ops() {
        let params = parse_psd(AdjustmentKind::Levels, &[1, 2]);
        assert_eq!(params, Params::Unsupported);
        let px_in = px(0.3, 0.6, 0.9);
        assert_eq!(params.apply(px_in), px_in, "no-op keeps pixels intact");
    }

    #[test]
    fn truncated_payloads_never_panic() {
        let kinds = [
            AdjustmentKind::Levels,
            AdjustmentKind::Curves,
            AdjustmentKind::HueSaturation,
            AdjustmentKind::BrightnessContrast,
            AdjustmentKind::BlackWhite,
            AdjustmentKind::SolidColor,
            AdjustmentKind::Posterize,
            AdjustmentKind::Threshold,
        ];
        let blob: Vec<u8> = (0..64u8).collect();
        for kind in kinds {
            for cut in 0..blob.len() {
                let _ = parse_psd(kind, &blob[..cut]);
            }
        }
    }
}

/// A tunable exposed to the UI. Mirrors `plugin_api::FilterParam` so the
/// shell can render adjustments and filters with the same dialog code.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ParamSpec {
    pub key: &'static str,
    pub label: &'static str,
    pub min: f32,
    pub max: f32,
    pub value: f32,
    pub suffix: &'static str,
}

impl Params {
    /// Editable controls for this adjustment. Curves return nothing: they
    /// need a curve editor rather than sliders (deliberately deferred).
    pub fn param_specs(&self) -> Vec<ParamSpec> {
        let spec = |key, label, min, max, value, suffix| ParamSpec {
            key,
            label,
            min,
            max,
            value,
            suffix,
        };
        match self {
            Params::Levels(l) => vec![
                spec("in_black", "Input Black", 0.0, 1.0, l.rgb.input_black, ""),
                spec("gamma", "Gamma", 0.1, 9.99, l.rgb.gamma, ""),
                spec("in_white", "Input White", 0.0, 1.0, l.rgb.input_white, ""),
                spec(
                    "out_black",
                    "Output Black",
                    0.0,
                    1.0,
                    l.rgb.output_black,
                    "",
                ),
                spec(
                    "out_white",
                    "Output White",
                    0.0,
                    1.0,
                    l.rgb.output_white,
                    "",
                ),
            ],
            Params::HueSaturation {
                hue,
                saturation,
                lightness,
                ..
            } => vec![
                spec("hue", "Hue", -180.0, 180.0, *hue, "°"),
                spec("saturation", "Saturation", -100.0, 100.0, *saturation, ""),
                spec("lightness", "Lightness", -100.0, 100.0, *lightness, ""),
            ],
            Params::BrightnessContrast {
                brightness,
                contrast,
            } => vec![
                spec("brightness", "Brightness", -100.0, 100.0, *brightness, ""),
                spec("contrast", "Contrast", -100.0, 100.0, *contrast, ""),
            ],
            Params::BlackWhite {
                reds,
                yellows,
                greens,
                cyans,
                blues,
                magentas,
            } => vec![
                spec("reds", "Reds", -200.0, 300.0, *reds, "%"),
                spec("yellows", "Yellows", -200.0, 300.0, *yellows, "%"),
                spec("greens", "Greens", -200.0, 300.0, *greens, "%"),
                spec("cyans", "Cyans", -200.0, 300.0, *cyans, "%"),
                spec("blues", "Blues", -200.0, 300.0, *blues, "%"),
                spec("magentas", "Magentas", -200.0, 300.0, *magentas, "%"),
            ],
            Params::Posterize { levels } => {
                vec![spec("levels", "Levels", 2.0, 255.0, *levels as f32, "")]
            }
            Params::Threshold { level } => {
                vec![spec("level", "Threshold", 0.0, 1.0, *level, "")]
            }
            Params::SolidColor { rgba } => vec![
                spec("r", "Red", 0.0, 1.0, rgba[0], ""),
                spec("g", "Green", 0.0, 1.0, rgba[1], ""),
                spec("b", "Blue", 0.0, 1.0, rgba[2], ""),
            ],
            Params::Curves(_) | Params::Invert | Params::Unsupported => Vec::new(),
        }
    }

    /// Update one control by key. Unknown keys are ignored.
    pub fn set_param(&mut self, key: &str, value: f32) {
        match self {
            Params::Levels(l) => match key {
                "in_black" => l.rgb.input_black = value.clamp(0.0, 1.0),
                "in_white" => l.rgb.input_white = value.clamp(0.0, 1.0),
                "gamma" => l.rgb.gamma = value.clamp(0.1, 9.99),
                "out_black" => l.rgb.output_black = value.clamp(0.0, 1.0),
                "out_white" => l.rgb.output_white = value.clamp(0.0, 1.0),
                _ => {}
            },
            Params::HueSaturation {
                hue,
                saturation,
                lightness,
                ..
            } => match key {
                "hue" => *hue = value.clamp(-180.0, 180.0),
                "saturation" => *saturation = value.clamp(-100.0, 100.0),
                "lightness" => *lightness = value.clamp(-100.0, 100.0),
                _ => {}
            },
            Params::BrightnessContrast {
                brightness,
                contrast,
            } => match key {
                "brightness" => *brightness = value.clamp(-100.0, 100.0),
                "contrast" => *contrast = value.clamp(-100.0, 100.0),
                _ => {}
            },
            Params::BlackWhite {
                reds,
                yellows,
                greens,
                cyans,
                blues,
                magentas,
            } => {
                let v = value.clamp(-200.0, 300.0);
                match key {
                    "reds" => *reds = v,
                    "yellows" => *yellows = v,
                    "greens" => *greens = v,
                    "cyans" => *cyans = v,
                    "blues" => *blues = v,
                    "magentas" => *magentas = v,
                    _ => {}
                }
            }
            Params::Posterize { levels } => {
                if key == "levels" {
                    *levels = value.clamp(2.0, 255.0) as u32;
                }
            }
            Params::Threshold { level } => {
                if key == "level" {
                    *level = value.clamp(0.0, 1.0);
                }
            }
            Params::SolidColor { rgba } => {
                let v = value.clamp(0.0, 1.0);
                match key {
                    "r" => rgba[0] = v,
                    "g" => rgba[1] = v,
                    "b" => rgba[2] = v,
                    _ => {}
                }
            }
            Params::Curves(_) | Params::Invert | Params::Unsupported => {}
        }
    }

    /// Adjustment kinds the user can create from the menu.
    pub fn creatable() -> &'static [AdjustmentKind] {
        &[
            AdjustmentKind::Levels,
            AdjustmentKind::BrightnessContrast,
            AdjustmentKind::HueSaturation,
            AdjustmentKind::BlackWhite,
            AdjustmentKind::Invert,
            AdjustmentKind::Posterize,
            AdjustmentKind::Threshold,
            AdjustmentKind::SolidColor,
        ]
    }
}

#[cfg(test)]
mod param_ui_tests {
    use super::*;

    #[test]
    fn every_creatable_kind_has_defaults_and_round_trips_json() {
        for &kind in Params::creatable() {
            let params = Params::default_for(kind);
            assert_ne!(params, Params::Unsupported, "{kind:?}");
            let json = serde_json::to_string(&params).unwrap();
            let back: Params = serde_json::from_str(&json).unwrap();
            assert_eq!(params, back, "{kind:?} JSON round trip");
        }
    }

    #[test]
    fn set_param_moves_the_matching_control() {
        for &kind in Params::creatable() {
            let mut params = Params::default_for(kind);
            let specs = params.param_specs();
            for spec in specs {
                let target = (spec.value + (spec.max - spec.value) * 0.5).min(spec.max);
                params.set_param(spec.key, target);
                let after = params
                    .param_specs()
                    .into_iter()
                    .find(|s| s.key == spec.key)
                    .expect("control still present");
                // Integer-valued controls (posterize levels) quantize, so
                // allow a one-step difference.
                assert!(
                    (after.value - target).abs() <= 1.0,
                    "{kind:?}/{} did not take {target}: {}",
                    spec.key,
                    after.value
                );
            }
        }
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let mut params = Params::default_for(AdjustmentKind::Posterize);
        let before = params.clone();
        params.set_param("nonexistent", 99.0);
        assert_eq!(params, before);
    }
}

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
            // Channel-mixing adjustments can't be expressed per channel;
            // step functions (posterize, threshold) must not be
            // interpolated, or the LUT would round their edges off.
            Params::HueSaturation { .. }
            | Params::BlackWhite { .. }
            | Params::Threshold { .. }
            | Params::Posterize { .. } => Prepared::Direct(self.clone()),
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
fn sample_lut(lut: &[f32; LUT_SIZE], v: f32) -> f32 {
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
