//! Adjustment parameters, their pixel math, and PSD payload parsing.
//!
//! An adjustment layer modifies whatever is beneath it rather than carrying
//! pixels of its own. The compositor asks this crate for a [`Params`] value
//! (parsed from the layer's preserved PSD payload, or from our own JSON when
//! the user created it) and applies it to the backdrop.
//!
//! Colour math runs on straight-alpha RGB in 0..1; alpha is never touched —
//! an adjustment recolours, it doesn't reshape coverage.

/// PSD descriptor decoding, re-exported so callers that already reach for
/// it through this crate keep working.
pub use schist_psd_descriptor as descriptor;

use schist_color::Rgba;
use schist_core::AdjustmentKind;

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
    /// Insert a control point, keeping the list sorted by x.
    ///
    /// Points nearer than `merge` in x replace each other rather than
    /// stacking, which is what stops a drag from leaving a pile of nearly
    /// coincident points behind.
    pub fn add_point(&mut self, x: f32, y: f32, merge: f32) -> usize {
        let x = x.clamp(0.0, 1.0);
        let y = y.clamp(0.0, 1.0);
        if let Some(i) = self.points.iter().position(|p| (p.0 - x).abs() <= merge) {
            self.points[i] = (x, y);
            return i;
        }
        let i = self
            .points
            .iter()
            .position(|p| p.0 > x)
            .unwrap_or(self.points.len());
        self.points.insert(i, (x, y));
        i
    }

    /// Move an existing point, keeping the order and the endpoints pinned
    /// to the ends of the range.
    pub fn move_point(&mut self, index: usize, x: f32, y: f32) {
        let n = self.points.len();
        if index >= n {
            return;
        }
        let y = y.clamp(0.0, 1.0);
        // The first and last points define the ends of the curve, so they
        // only move vertically.
        let x = if index == 0 {
            0.0
        } else if index + 1 == n {
            1.0
        } else {
            // Keep strictly between the neighbours, or the curve would
            // fold back on itself.
            let lo = self.points[index - 1].0 + 1e-3;
            let hi = self.points[index + 1].0 - 1e-3;
            x.clamp(lo.min(hi), hi.max(lo))
        };
        self.points[index] = (x, y);
    }

    /// Delete a point. The two endpoints cannot be removed.
    pub fn remove_point(&mut self, index: usize) {
        if index == 0 || index + 1 >= self.points.len() || self.points.len() <= 2 {
            return;
        }
        self.points.remove(index);
    }

    /// The point within `radius` of (x, y), if any.
    pub fn hit(&self, x: f32, y: f32, radius: f32) -> Option<usize> {
        self.points
            .iter()
            .enumerate()
            .filter(|(_, p)| (p.0 - x).hypot(p.1 - y) <= radius)
            .min_by(|a, b| {
                let da = (a.1 .0 - x).hypot(a.1 .1 - y);
                let db = (b.1 .0 - x).hypot(b.1 .1 - y);
                da.total_cmp(&db)
            })
            .map(|(i, _)| i)
    }

    pub fn reset(&mut self) {
        self.points = vec![(0.0, 0.0), (1.0, 1.0)];
    }

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

/// Which curve the editor is showing. Not part of the adjustment's
/// meaning, but it lives here so the dialog can round-trip it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CurveChannel {
    #[default]
    Rgb,
    Red,
    Green,
    Blue,
}

impl CurveChannel {
    pub const ALL: [CurveChannel; 4] = [
        CurveChannel::Rgb,
        CurveChannel::Red,
        CurveChannel::Green,
        CurveChannel::Blue,
    ];

    pub fn label(self) -> &'static str {
        match self {
            CurveChannel::Rgb => "RGB",
            CurveChannel::Red => "Red",
            CurveChannel::Green => "Green",
            CurveChannel::Blue => "Blue",
        }
    }

    /// The colour to draw this channel's curve in.
    pub fn tint(self) -> u32 {
        match self {
            CurveChannel::Rgb => 0xE0E0E0,
            CurveChannel::Red => 0xE05050,
            CurveChannel::Green => 0x50C050,
            CurveChannel::Blue => 0x5080E0,
        }
    }
}

impl Curves {
    pub fn channel(&self, ch: CurveChannel) -> &Curve {
        match ch {
            CurveChannel::Rgb => &self.rgb,
            CurveChannel::Red => &self.red,
            CurveChannel::Green => &self.green,
            CurveChannel::Blue => &self.blue,
        }
    }

    pub fn channel_mut(&mut self, ch: CurveChannel) -> &mut Curve {
        match ch {
            CurveChannel::Rgb => &mut self.rgb,
            CurveChannel::Red => &mut self.red,
            CurveChannel::Green => &mut self.green,
            CurveChannel::Blue => &mut self.blue,
        }
    }
}

/// One of Affinity's six per-hue-range tweaks on an HSL adjustment.
///
/// The range is a trapezoid over the hue circle: weight 0 outside
/// `bounds[0]`..`bounds[3]`, ramping up to 1 across `bounds[0]`..
/// `bounds[1]`, flat until `bounds[2]`, ramping back down by
/// `bounds[3]` (degrees, wrapping — reds are 315, 345, 15, 45). The
/// six defaults overlap on their ramps, so the weights of the whole
/// set sum to 1 at every hue. Each range's shifts are added to the
/// master ones, weighted, before the sliders' transfer curves run.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct HueRange {
    /// Trapezoid corners in degrees, ascending around the circle.
    pub bounds: [f32; 4],
    /// -180..180 degrees.
    pub hue: f32,
    /// -100..100.
    pub saturation: f32,
    /// -100..100.
    pub lightness: f32,
}

impl HueRange {
    /// How much this range claims of a pixel at hue `h` (degrees).
    pub fn weight(&self, h: f32) -> f32 {
        let [b0, b1, b2, b3] = self.bounds;
        let span = |a: f32, b: f32| (b - a).rem_euclid(360.0);
        let (d, up, flat, down) = (span(b0, h), span(b0, b1), span(b0, b2), span(b0, b3));
        if d >= down {
            0.0
        } else if d < up {
            if up > 0.0 {
                d / up
            } else {
                1.0
            }
        } else if d <= flat {
            1.0
        } else if down > flat {
            (down - d) / (down - flat)
        } else {
            1.0
        }
    }
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
        /// Affinity's lightness slider also scales saturation by
        /// 1 − |lightness| (a lift toward white flattens colour);
        /// Photoshop's does not. Imports from Affinity set this.
        #[serde(default)]
        lightness_desaturates: bool,
        /// Affinity's saturation slider boosts reciprocally
        /// (s / (1 − A) for positive amounts — +40 is a ×1.67 boost,
        /// measured across the probe fixture); ours scales by 1 + A.
        /// Imports from Affinity set this too.
        #[serde(default)]
        reciprocal_saturation: bool,
        /// Affinity's per-hue-range tweaks, added to the master
        /// sliders in proportion to each range's weight at the
        /// pixel's own hue. Empty for a master-only adjustment.
        #[serde(default)]
        ranges: Vec<HueRange>,
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
    /// Shifts colour balance separately in shadows, midtones and
    /// highlights. Each triple is cyan/red, magenta/green, yellow/blue in
    /// -100..=100.
    ColorBalance {
        shadows: [f32; 3],
        midtones: [f32; 3],
        highlights: [f32; 3],
        /// Keep the pixel's luminance while the hue moves.
        preserve_luminosity: bool,
    },
    /// Saturation that leans on the least saturated pixels, so skin tones
    /// and already-vivid colours move least.
    Vibrance {
        vibrance: f32,
        saturation: f32,
    },
    /// Linear exposure in stops, plus offset and gamma.
    Exposure {
        exposure: f32,
        offset: f32,
        gamma: f32,
    },
    /// A coloured filter over the image, as if screwed onto the lens.
    PhotoFilter {
        color: [f32; 3],
        /// 0..=100.
        density: f32,
        preserve_luminosity: bool,
    },
    /// Remaps luminance through a two-colour ramp.
    GradientMap {
        from: [f32; 3],
        to: [f32; 3],
        reverse: bool,
        /// A full multi-stop ramp as (position, colour) pairs, sorted by
        /// position. When at least two are present they replace
        /// `from`/`to` (which stay as the ends for older documents and
        /// the dialog). Imported Affinity gradient maps carry their
        /// whole ramp here.
        #[serde(default)]
        stops: Vec<(f32, [f32; 3])>,
    },
    /// Per-colour-range CMYK tweaks. `ranges` is indexed by
    /// [`SelectiveRange`], each holding cyan/magenta/yellow/black in
    /// -100..=100.
    SelectiveColor {
        ranges: [[f32; 4]; 6],
        /// Relative scales by what is already there; absolute does not.
        relative: bool,
    },
    /// Each output channel as a weighted sum of the inputs, in percent.
    ChannelMixer {
        red: [f32; 3],
        green: [f32; 3],
        blue: [f32; 3],
        constant: [f32; 3],
        monochrome: bool,
    },
    /// Photographic white balance: a Bradford chromatic adaptation in
    /// linear light, warmth and tint in -100..=100. The per-channel
    /// grey gains are calibrated against Affinity's renders; matching
    /// them under Bradford also reproduces its saturated colours to
    /// about 1/255 (measured across seven colour patches).
    WhiteBalance {
        warmth: f32,
        tint: f32,
    },
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
                lightness_desaturates: false,
                reciprocal_saturation: false,
                ranges: Vec::new(),
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
            AdjustmentKind::ColorBalance => Params::ColorBalance {
                shadows: [0.0; 3],
                midtones: [0.0; 3],
                highlights: [0.0; 3],
                preserve_luminosity: true,
            },
            AdjustmentKind::Vibrance => Params::Vibrance {
                vibrance: 0.0,
                saturation: 0.0,
            },
            AdjustmentKind::Exposure => Params::Exposure {
                exposure: 0.0,
                offset: 0.0,
                gamma: 1.0,
            },
            AdjustmentKind::PhotoFilter => Params::PhotoFilter {
                // Photoshop's "Warming Filter (85)".
                color: [0.929, 0.510, 0.208],
                density: 25.0,
                preserve_luminosity: true,
            },
            AdjustmentKind::GradientMap => Params::GradientMap {
                from: [0.0, 0.0, 0.0],
                to: [1.0, 1.0, 1.0],
                reverse: false,
                stops: Vec::new(),
            },
            AdjustmentKind::SelectiveColor => Params::SelectiveColor {
                ranges: [[0.0; 4]; 6],
                relative: true,
            },
            AdjustmentKind::ChannelMixer => Params::ChannelMixer {
                red: [100.0, 0.0, 0.0],
                green: [0.0, 100.0, 0.0],
                blue: [0.0, 0.0, 100.0],
                constant: [0.0; 3],
                monochrome: false,
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
            Params::ColorBalance { .. } => AdjustmentKind::ColorBalance,
            Params::Vibrance { .. } => AdjustmentKind::Vibrance,
            Params::Exposure { .. } => AdjustmentKind::Exposure,
            Params::PhotoFilter { .. } => AdjustmentKind::PhotoFilter,
            Params::GradientMap { .. } => AdjustmentKind::GradientMap,
            Params::SelectiveColor { .. } => AdjustmentKind::SelectiveColor,
            Params::ChannelMixer { .. } => AdjustmentKind::ChannelMixer,
            Params::WhiteBalance { .. } => AdjustmentKind::Other(*b"WhBl"),
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
                lightness_desaturates,
                reciprocal_saturation,
                ranges,
            } => {
                let (h, s, l) = rgb_to_hsl(px.r, px.g, px.b);
                // Per-range hue and saturation tweaks simply add to the
                // master sliders, weighted by the pixel's own
                // (unshifted) hue. Per-range *lightness* does not: it is
                // a separate, hue-preserving pull of every channel
                // toward the brightest one, applied to the result below
                // (fixtures/affinity-probe/hsl_range_green_lum.af).
                let desat = if *lightness_desaturates {
                    (1.0 - lightness.abs() / 100.0).clamp(0.0, 1.0)
                } else {
                    1.0
                };
                let (mut hue, mut saturation, mut range_lightness) = (*hue, *saturation, 0.0f32);
                for range in ranges {
                    let w = range.weight(h);
                    if w > 0.0 {
                        hue += w * range.hue;
                        saturation += w * range.saturation;
                        range_lightness += w * range.lightness;
                    }
                }
                let (hue, saturation) = (&hue, &saturation);
                let shifted = if *reciprocal_saturation && *saturation > 0.0 {
                    s / (1.0 - saturation / 100.0).max(0.02)
                } else {
                    s * (1.0 + saturation / 100.0)
                };
                let (nh, ns, nl) = if *colorize {
                    (
                        hue.rem_euclid(360.0),
                        (saturation / 100.0).clamp(0.0, 1.0),
                        adjust_lightness(l, *lightness),
                    )
                } else {
                    (
                        (h + hue).rem_euclid(360.0),
                        (shifted * desat).clamp(0.0, 1.0),
                        adjust_lightness(l, *lightness),
                    )
                };
                let (r, g, b) = hsl_to_rgb(nh, ns, nl);
                let (r, g, b) = pull_to_extreme(r, g, b, range_lightness / 100.0);
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
                // Equal input bands, outputs spread over the full range —
                // floor into n bands, not round to n-1 lattice points,
                // which is the convention Photoshop and Affinity share
                // (verified against an Affinity fixture's own render).
                let n = (*levels).clamp(2, 255) as f32;
                let f = |v: f32| ((v * n).floor().min(n - 1.0) / (n - 1.0)).clamp(0.0, 1.0);
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
            Params::ColorBalance {
                shadows,
                midtones,
                highlights,
                preserve_luminosity,
            } => {
                let lum = luma(px);
                // Weight each band by how much of the pixel falls in it;
                // the midtone bell peaks at 0.5 and the two ends ramp away
                // from it, which is what stops a shadow tweak bleaching
                // the highlights.
                let sh = (1.0 - lum * 2.0).clamp(0.0, 1.0);
                let hi = ((lum - 0.5) * 2.0).clamp(0.0, 1.0);
                let mid = 1.0 - sh - hi;
                let shift =
                    |c: usize| (shadows[c] * sh + midtones[c] * mid + highlights[c] * hi) / 100.0;
                let out = Rgba {
                    r: (px.r + shift(0)).clamp(0.0, 1.0),
                    g: (px.g + shift(1)).clamp(0.0, 1.0),
                    b: (px.b + shift(2)).clamp(0.0, 1.0),
                    a: px.a,
                };
                if *preserve_luminosity {
                    set_lum(out, lum)
                } else {
                    out
                }
            }
            Params::Vibrance {
                vibrance,
                saturation,
            } => {
                // Both sliders scale CIELAB chroma; Saturation does it
                // flat, Vibrance through the weighting measured below.
                let sat_k = 1.0 + saturation / 100.0;
                let t = (vibrance / 100.0).clamp(-1.0, 1.0);
                scale_chroma_by(px, |chroma, hue| sat_k * vibrance_gain(t, chroma, hue))
            }
            Params::Exposure {
                exposure,
                offset,
                gamma,
            } => {
                let scale = 2f32.powf(*exposure);
                let g = (1.0 / gamma.max(0.01)).clamp(0.01, 100.0);
                let f = |v: f32| (((v * scale) + offset).max(0.0)).powf(g).clamp(0.0, 1.0);
                Rgba {
                    r: f(px.r),
                    g: f(px.g),
                    b: f(px.b),
                    a: px.a,
                }
            }
            Params::PhotoFilter {
                color,
                density,
                preserve_luminosity,
            } => {
                // Probed with the RGB cube at density 50 and 100, with
                // Preserve Luminosity both ways
                // (fixtures/affinity-probe/cube_lens*.af): the filter
                // is a per-channel multiply in *encoded* sRGB towards
                // the filter colour, and the panel's density is not the
                // blend fraction — that is 0.9 x density squared, which
                // reproduces all four probes to under 1/255 RMS.
                let d = (density / 100.0).clamp(0.0, 1.0);
                let k = 0.9 * d * d;
                let mix = |v: f32, c: f32| v * (1.0 - k + k * c);
                let out = Rgba {
                    r: mix(px.r, color[0]),
                    g: mix(px.g, color[1]),
                    b: mix(px.b, color[2]),
                    a: px.a,
                };
                let out = Rgba {
                    r: out.r.clamp(0.0, 1.0),
                    g: out.g.clamp(0.0, 1.0),
                    b: out.b.clamp(0.0, 1.0),
                    a: out.a,
                };
                if *preserve_luminosity {
                    set_lum(out, luma(px))
                } else {
                    out
                }
            }
            Params::GradientMap {
                from,
                to,
                reverse,
                stops,
            } => {
                let mut t = luma(px);
                if *reverse {
                    t = 1.0 - t;
                }
                let c = if stops.len() >= 2 {
                    let mut lo = &stops[0];
                    let mut hi = &stops[stops.len() - 1];
                    for pair in stops.windows(2) {
                        if t >= pair[0].0 && t <= pair[1].0 {
                            lo = &pair[0];
                            hi = &pair[1];
                            break;
                        }
                    }
                    let span = (hi.0 - lo.0).max(1e-6);
                    let u = ((t - lo.0) / span).clamp(0.0, 1.0);
                    [
                        lo.1[0] + (hi.1[0] - lo.1[0]) * u,
                        lo.1[1] + (hi.1[1] - lo.1[1]) * u,
                        lo.1[2] + (hi.1[2] - lo.1[2]) * u,
                    ]
                } else {
                    [
                        from[0] + (to[0] - from[0]) * t,
                        from[1] + (to[1] - from[1]) * t,
                        from[2] + (to[2] - from[2]) * t,
                    ]
                };
                Rgba {
                    r: c[0],
                    g: c[1],
                    b: c[2],
                    a: px.a,
                }
            }
            Params::SelectiveColor { ranges, relative } => selective_color(px, ranges, *relative),
            Params::WhiteBalance { warmth, tint } => white_balance(px, *warmth, *tint),
            Params::ChannelMixer {
                red,
                green,
                blue,
                constant,
                monochrome,
            } => {
                let mix = |w: &[f32; 3], k: f32| {
                    ((px.r * w[0] + px.g * w[1] + px.b * w[2]) / 100.0 + k / 100.0).clamp(0.0, 1.0)
                };
                if *monochrome {
                    // Monochrome uses the red row for every output channel,
                    // matching Photoshop.
                    let v = mix(red, constant[0]);
                    Rgba {
                        r: v,
                        g: v,
                        b: v,
                        a: px.a,
                    }
                } else {
                    Rgba {
                        r: mix(red, constant[0]),
                        g: mix(green, constant[1]),
                        b: mix(blue, constant[2]),
                        a: px.a,
                    }
                }
            }
            Params::Unsupported => px,
        }
    }

    /// Apply across a straight-alpha f32 RGBA buffer in place.
    pub fn apply_buffer(&self, pixels: &mut [f32]) {
        if matches!(self, Params::Unsupported) {
            return;
        }
        for px in pixels.as_chunks_mut::<4>().0 {
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

/// Affinity's per-hue-range luminosity shift: a hue-preserving pull of
/// every channel toward the brightest one (`amount` > 0, lightening) or
/// toward the darkest one (`amount` < 0), by `|amount|` of the way.
/// Both extremes stay put, so the hue and the HSV value or saturation —
/// whichever the move doesn't touch — survive exactly. Measured against
/// hsl_range_green_lum.af and hsl_range_green_lumneg.af, which pin both
/// directions to the last unit in 8 bits.
fn pull_to_extreme(r: f32, g: f32, b: f32, amount: f32) -> (f32, f32, f32) {
    if amount == 0.0 {
        return (r, g, b);
    }
    let target = if amount > 0.0 {
        r.max(g).max(b)
    } else {
        r.min(g).min(b)
    };
    let k = amount.abs().clamp(0.0, 1.0);
    let pull = |c: f32| c + (target - c) * k;
    (pull(r), pull(g), pull(b))
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

/// Encode `params` back into the PSD block payload for `kind`.
///
/// Returns `None` for kinds this crate cannot round-trip; the caller then
/// falls back to whatever raw bytes the file arrived with.
///
/// Adjustment layers created in Schist carry their settings only in
/// `params_json`, which no PSD reader understands. Without an encoder the
/// writer emitted an empty raster layer, so every adjustment layer a user
/// made was destroyed by saving. Each encoder here is the exact inverse of
/// the parser above it, which is what the round-trip tests check.
pub fn encode_psd(kind: AdjustmentKind, params: &Params) -> Option<Vec<u8>> {
    match (kind, params) {
        (AdjustmentKind::Invert, Params::Invert) => Some(Vec::new()),
        (AdjustmentKind::Posterize, Params::Posterize { levels }) => {
            Some((*levels as u16).to_be_bytes().to_vec())
        }
        (AdjustmentKind::Threshold, Params::Threshold { level }) => {
            let v = (level * 255.0).round().clamp(0.0, 65535.0) as u16;
            Some(v.to_be_bytes().to_vec())
        }
        (
            AdjustmentKind::BrightnessContrast,
            Params::BrightnessContrast {
                brightness,
                contrast,
            },
        ) => {
            let mut out = Vec::with_capacity(7);
            out.extend_from_slice(&(brightness.round() as i16).to_be_bytes());
            out.extend_from_slice(&(contrast.round() as i16).to_be_bytes());
            out.extend_from_slice(&0i16.to_be_bytes()); // mean
            out.push(0); // lab flag
            Some(out)
        }
        (AdjustmentKind::Levels, Params::Levels(l)) => {
            let mut out = Vec::with_capacity(2 + 29 * 10);
            out.extend_from_slice(&2u16.to_be_bytes()); // version
            let mut record = |c: &LevelsChannel| {
                let q = |v: f32| (v * 255.0).round().clamp(0.0, 255.0) as u16;
                out.extend_from_slice(&q(c.input_black).to_be_bytes());
                out.extend_from_slice(&q(c.input_white).to_be_bytes());
                out.extend_from_slice(&q(c.output_black).to_be_bytes());
                out.extend_from_slice(&q(c.output_white).to_be_bytes());
                let gamma = (c.gamma * 100.0).round().clamp(1.0, 1000.0) as u16;
                out.extend_from_slice(&gamma.to_be_bytes());
            };
            record(&l.rgb);
            record(&l.red);
            record(&l.green);
            record(&l.blue);
            // Photoshop always stores 29 records; the rest are identity.
            let identity = LevelsChannel::default();
            for _ in 4..29 {
                record(&identity);
            }
            Some(out)
        }
        (
            AdjustmentKind::HueSaturation,
            Params::HueSaturation {
                hue,
                saturation,
                lightness,
                colorize,
                ..
            },
        ) => {
            let mut out = Vec::with_capacity(10);
            out.extend_from_slice(&2u16.to_be_bytes()); // version
            out.extend_from_slice(&u16::from(*colorize).to_be_bytes());
            out.extend_from_slice(&(hue.round() as i16).to_be_bytes());
            out.extend_from_slice(&(saturation.round() as i16).to_be_bytes());
            out.extend_from_slice(&(lightness.round() as i16).to_be_bytes());
            Some(out)
        }
        (AdjustmentKind::Curves, Params::Curves(c)) => {
            let channels = [&c.rgb, &c.red, &c.green, &c.blue];
            let mut bitmap = 0u32;
            for (i, curve) in channels.iter().enumerate() {
                if curve.points.len() >= 2 {
                    bitmap |= 1 << i;
                }
            }
            let mut out = Vec::new();
            out.push(0); // padding
            out.extend_from_slice(&1u16.to_be_bytes()); // version
            out.extend_from_slice(&bitmap.to_be_bytes());
            for curve in channels {
                if curve.points.len() < 2 {
                    continue;
                }
                let points: Vec<_> = curve.points.iter().take(32).collect();
                out.extend_from_slice(&(points.len() as u16).to_be_bytes());
                for (inp, outp) in points {
                    let q = |v: f32| (v * 255.0).round().clamp(0.0, 255.0) as u16;
                    out.extend_from_slice(&q(*outp).to_be_bytes());
                    out.extend_from_slice(&q(*inp).to_be_bytes());
                }
            }
            Some(out)
        }
        (
            AdjustmentKind::BlackWhite,
            Params::BlackWhite {
                reds,
                yellows,
                greens,
                cyans,
                blues,
                magentas,
            },
        ) => {
            let mut b = descriptor::Builder::new("null");
            b.double("Rd  ", *reds as f64)
                .double("Yllw", *yellows as f64)
                .double("Grn ", *greens as f64)
                .double("Cyn ", *cyans as f64)
                .double("Bl  ", *blues as f64)
                .double("Mgnt", *magentas as f64);
            Some(versioned(b.finish()))
        }
        (AdjustmentKind::SolidColor, Params::SolidColor { rgba }) => {
            let mut b = descriptor::Builder::new("null");
            b.color(
                "Clr ",
                rgba[0] as f64 * 255.0,
                rgba[1] as f64 * 255.0,
                rgba[2] as f64 * 255.0,
            );
            Some(versioned(b.finish()))
        }
        _ => None,
    }
}

/// A descriptor with the prefix `descriptor::parse_versioned` expects: a
/// u16 layer-block version then a u32 descriptor version, six bytes in all.
fn versioned(body: Vec<u8>) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 6);
    out.extend_from_slice(&16u16.to_be_bytes());
    out.extend_from_slice(&16u32.to_be_bytes());
    out.extend_from_slice(&body);
    out
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
            lightness_desaturates: false,
            reciprocal_saturation: false,
            ranges: Vec::new(),
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
            lightness_desaturates: false,
            reciprocal_saturation: false,
            ranges: Vec::new(),
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
            lightness_desaturates: false,
            reciprocal_saturation: false,
            ranges: Vec::new(),
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
            Params::ColorBalance {
                shadows,
                midtones,
                highlights,
                ..
            } => {
                // Three bands of three, named the way Photoshop labels the
                // ends of each slider.
                let mut out = Vec::new();
                for (band, vals) in [("sh", shadows), ("mid", midtones), ("hi", highlights)] {
                    for (c, label) in [
                        ("cr", "Cyan/Red"),
                        ("mg", "Magenta/Green"),
                        ("yb", "Yellow/Blue"),
                    ]
                    .iter()
                    .enumerate()
                    {
                        out.push(spec(
                            balance_key(band, label.0),
                            balance_label(band, label.1),
                            -100.0,
                            100.0,
                            vals[c],
                            "",
                        ));
                    }
                }
                out
            }
            Params::Vibrance {
                vibrance,
                saturation,
            } => vec![
                spec("vibrance", "Vibrance", -100.0, 100.0, *vibrance, ""),
                spec("saturation", "Saturation", -100.0, 100.0, *saturation, ""),
            ],
            Params::Exposure {
                exposure,
                offset,
                gamma,
            } => vec![
                spec("exposure", "Exposure", -20.0, 20.0, *exposure, " EV"),
                spec("offset", "Offset", -0.5, 0.5, *offset, ""),
                spec("gamma", "Gamma", 0.1, 9.99, *gamma, ""),
            ],
            Params::PhotoFilter { density, .. } => {
                vec![spec("density", "Density", 0.0, 100.0, *density, "%")]
            }
            Params::GradientMap { .. } => Vec::new(),
            Params::WhiteBalance { warmth, tint } => vec![
                spec("warmth", "Warmth", -100.0, 100.0, *warmth, ""),
                spec("tint", "Tint", -100.0, 100.0, *tint, ""),
            ],
            Params::SelectiveColor { ranges, .. } => {
                let mut out = Vec::new();
                for (i, r) in SelectiveRange::ALL.iter().enumerate() {
                    for (c, label) in ["Cyan", "Magenta", "Yellow", "Black"].iter().enumerate() {
                        out.push(spec(
                            selective_key(i, c),
                            selective_label(*r, label),
                            -100.0,
                            100.0,
                            ranges[i][c],
                            "%",
                        ));
                    }
                }
                out
            }
            Params::ChannelMixer {
                red, green, blue, ..
            } => vec![
                spec("r_r", "Red \u{2192} Red", -200.0, 200.0, red[0], "%"),
                spec("r_g", "Green \u{2192} Red", -200.0, 200.0, red[1], "%"),
                spec("r_b", "Blue \u{2192} Red", -200.0, 200.0, red[2], "%"),
                spec("g_r", "Red \u{2192} Green", -200.0, 200.0, green[0], "%"),
                spec("g_g", "Green \u{2192} Green", -200.0, 200.0, green[1], "%"),
                spec("g_b", "Blue \u{2192} Green", -200.0, 200.0, green[2], "%"),
                spec("b_r", "Red \u{2192} Blue", -200.0, 200.0, blue[0], "%"),
                spec("b_g", "Green \u{2192} Blue", -200.0, 200.0, blue[1], "%"),
                spec("b_b", "Blue \u{2192} Blue", -200.0, 200.0, blue[2], "%"),
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
            Params::ColorBalance {
                shadows,
                midtones,
                highlights,
                ..
            } => {
                let v = value.clamp(-100.0, 100.0);
                let (band, c) = match key {
                    "sh_cr" => (0, 0),
                    "sh_mg" => (0, 1),
                    "sh_yb" => (0, 2),
                    "mid_cr" => (1, 0),
                    "mid_mg" => (1, 1),
                    "mid_yb" => (1, 2),
                    "hi_cr" => (2, 0),
                    "hi_mg" => (2, 1),
                    "hi_yb" => (2, 2),
                    _ => return,
                };
                match band {
                    0 => shadows[c] = v,
                    1 => midtones[c] = v,
                    _ => highlights[c] = v,
                }
            }
            Params::Vibrance {
                vibrance,
                saturation,
            } => match key {
                "vibrance" => *vibrance = value.clamp(-100.0, 100.0),
                "saturation" => *saturation = value.clamp(-100.0, 100.0),
                _ => {}
            },
            Params::Exposure {
                exposure,
                offset,
                gamma,
            } => match key {
                "exposure" => *exposure = value.clamp(-20.0, 20.0),
                "offset" => *offset = value.clamp(-0.5, 0.5),
                "gamma" => *gamma = value.clamp(0.1, 9.99),
                _ => {}
            },
            Params::PhotoFilter { density, .. } => {
                if key == "density" {
                    *density = value.clamp(0.0, 100.0);
                }
            }
            Params::WhiteBalance { warmth, tint } => match key {
                "warmth" => *warmth = value.clamp(-100.0, 100.0),
                "tint" => *tint = value.clamp(-100.0, 100.0),
                _ => {}
            },
            Params::SelectiveColor { ranges, .. } => {
                for (r, range) in ranges.iter_mut().enumerate() {
                    for (c, slot) in range.iter_mut().enumerate() {
                        if key == selective_key(r, c) {
                            *slot = value.clamp(-100.0, 100.0);
                            return;
                        }
                    }
                }
            }
            Params::ChannelMixer {
                red, green, blue, ..
            } => {
                let v = value.clamp(-200.0, 200.0);
                match key {
                    "r_r" => red[0] = v,
                    "r_g" => red[1] = v,
                    "r_b" => red[2] = v,
                    "g_r" => green[0] = v,
                    "g_g" => green[1] = v,
                    "g_b" => green[2] = v,
                    "b_r" => blue[0] = v,
                    "b_g" => blue[1] = v,
                    "b_b" => blue[2] = v,
                    _ => {}
                }
            }
            Params::GradientMap { .. }
            | Params::Curves(_)
            | Params::Invert
            | Params::Unsupported => {}
        }
    }

    /// Adjustment kinds the user can create from the menu.
    pub fn creatable() -> &'static [AdjustmentKind] {
        &[
            AdjustmentKind::Levels,
            // Curves needed a graph editor before it could be offered;
            // now that there is one, it belongs here.
            AdjustmentKind::Curves,
            AdjustmentKind::BrightnessContrast,
            AdjustmentKind::HueSaturation,
            AdjustmentKind::BlackWhite,
            AdjustmentKind::Invert,
            AdjustmentKind::Posterize,
            AdjustmentKind::Threshold,
            AdjustmentKind::ColorBalance,
            AdjustmentKind::Vibrance,
            AdjustmentKind::Exposure,
            AdjustmentKind::PhotoFilter,
            AdjustmentKind::GradientMap,
            AdjustmentKind::SelectiveColor,
            AdjustmentKind::ChannelMixer,
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

/// Affinity's warmth slider, measured: the linear-light grey log-gains
/// it applies at every tenth of its range, from -100 (cool) to +100
/// (warm). Solved per fixture by fitting the gains of the Bradford
/// adaptation below to Affinity's own render of the probe test card —
/// each fit lands within 0.3/255 RMS, so the adaptation *is* the
/// operation and only these gains were ever in question. The curve is
/// markedly asymmetric (+10 moves nearly three times as far as -10, and
/// warming saturates while cooling runs away), which is why the earlier
/// mirrored-quadratic fit missed the cooling half.
const WARMTH_LOG_GAINS: [[f32; 3]; 21] = [
    [-1.066_140_8, 0.124_861_58, 1.085_302_5],
    [-0.771_848_3, 0.101_186_51, 0.903_488_3],
    [-0.579_433_7, 0.081_628_76, 0.747_089],
    [-0.441_393_9, 0.064_658_24, 0.610_910_9],
    [-0.335_628_2, 0.051_187_27, 0.491_378_4],
    [-0.251_684_2, 0.038_791_68, 0.385_404],
    [-0.182_746_1, 0.028_956_54, 0.291_194_9],
    [-0.126_031_4, 0.019_863_86, 0.207_869_5],
    [-0.078_470_71, 0.011_565_35, 0.131_245_84],
    [-0.036_664_08, 0.005_242_18, 0.062_403_7],
    [0.0, 0.0, 0.0],
    [0.115_865_47, -0.017_772_31, -0.221_109_93],
    [0.188_446_02, -0.028_601_73, -0.377_630_32],
    [0.238_142_49, -0.034_957_6, -0.494_250_24],
    [0.273_799_57, -0.038_461_82, -0.584_949_9],
    [0.300_928_86, -0.040_617_4, -0.655_367_1],
    [0.321_281_37, -0.042_763_06, -0.713_570_9],
    [0.337_165_5, -0.043_341_38, -0.763_241_7],
    [0.349_603_6, -0.044_069_05, -0.803_047_2],
    [0.359_788_2, -0.043_880_67, -0.838_493_2],
    [0.369_014, -0.043_403_02, -0.867_859_36],
];

/// The tint slider's grey log-gains, measured the same way at every
/// fifth of its range. Note the sign: `WBTi` on disk is the *negation*
/// of the Tint field Affinity shows, so this table is indexed by the
/// stored value (positive = the UI's green direction), matching what
/// the importer hands us. Like warmth, it is neither linear nor
/// symmetric.
const TINT_LOG_GAINS: [[f32; 3]; 11] = [
    [0.188_686_56, -0.094_765_3, 0.348_664_34],
    [0.154_797_69, -0.076_550_72, 0.272_947_67],
    [0.119_324_28, -0.056_674_22, 0.200_040_82],
    [0.082_791_54, -0.037_263_72, 0.131_661_3],
    [0.041_722_29, -0.019_361_82, 0.064_003_38],
    [0.0, 0.0, 0.0],
    [-0.044_928_24, 0.020_042_06, -0.062_391_93],
    [-0.094_376_62, 0.039_050_96, -0.123_218_58],
    [-0.145_803_21, 0.059_505_64, -0.182_253_42],
    [-0.203_076_29, 0.078_741_57, -0.240_934_16],
    [-0.263_430_95, 0.100_587_59, -0.298_225_22],
];

/// Read one of the measured tables at `pos` in -1..=1, interpolating
/// linearly between its knots.
fn slider_log_gains(table: &[[f32; 3]], pos: f32) -> [f32; 3] {
    let last = table.len() - 1;
    let x = ((pos.clamp(-1.0, 1.0) + 1.0) / 2.0) * last as f32;
    let i = (x.floor() as usize).min(last - 1);
    let f = x - i as f32;
    let (a, b) = (table[i], table[i + 1]);
    [
        a[0] + (b[0] - a[0]) * f,
        a[1] + (b[1] - a[1]) * f,
        a[2] + (b[2] - a[2]) * f,
    ]
}

/// Rec. 601 luminance, which is what Photoshop's adjustments weight by.
/// Bradford chromatic adaptation for [`Params::WhiteBalance`]. The
/// diagonal cone gains are chosen so the grey axis moves by the
/// calibrated per-channel linear gains for this warmth/tint.
/// How much the two White Balance sliders pull each other, as log
/// cone-gain corrections on a 5x5 grid of (warmth, tint) at -100, -50,
/// 0, 50, 100 — measured from `wbgrid` probes over a full grey and
/// primary ramp, one document per cell, with the clipped samples
/// dropped so the extreme gains are not dragged in.
///
/// Zero all along both axes, which is where the one-dimensional tables
/// above were calibrated and are exact. The corner that matters is
/// cool-and-magenta: at (-100, +100) Affinity's S-cone gain is over
/// three times the product of the two sliders taken separately.
const WB_INTERACTION: [[[f32; 3]; 5]; 5] = [
    [
        [-0.028_16, -0.020_48, -0.330_81],
        [-0.011_48, -0.009_38, -0.200_25],
        [0.0, 0.0, 0.0],
        [0.005_43, 0.006_48, 0.339_99],
        [0.003_97, 0.009_83, 1.129_74],
    ],
    [
        [-0.002_86, -0.011_16, -0.089_41],
        [-0.000_30, -0.005_07, -0.052_52],
        [0.0, 0.0, 0.0],
        [-0.001_05, 0.004_63, 0.073_44],
        [-0.004_17, 0.007_44, 0.179_94],
    ],
    [
        [0.0, 0.0, 0.0],
        [0.0, 0.0, 0.0],
        [0.0, 0.0, 0.0],
        [0.0, 0.0, 0.0],
        [0.0, 0.0, 0.0],
    ],
    [
        [-0.016_25, 0.025_91, 0.107_43],
        [-0.010_82, 0.010_77, 0.057_62],
        [0.0, 0.0, 0.0],
        [0.013_21, -0.011_90, -0.072_17],
        [0.030_10, -0.021_23, -0.160_45],
    ],
    [
        [-0.027_59, 0.035_72, 0.137_72],
        [-0.016_19, 0.017_59, 0.073_46],
        [0.0, 0.0, 0.0],
        [0.022_30, -0.015_61, -0.089_02],
        [0.049_94, -0.028_37, -0.194_16],
    ],
];

/// Read [`WB_INTERACTION`] at an arbitrary warmth and tint, bilinearly.
/// The table's tint axis is the *panel's*, and the file (so our
/// parameter) stores that negated, hence the flip.
fn white_balance_interaction(warmth: f32, tint: f32) -> [f32; 3] {
    let tint = -tint;
    let at = |v: f32| {
        let x = ((v.clamp(-100.0, 100.0) + 100.0) / 50.0).clamp(0.0, 4.0);
        let i = (x.floor() as usize).min(3);
        (i, x - i as f32)
    };
    let ((iw, fw), (it, ft)) = (at(warmth), at(tint));
    let mut out = [0.0f32; 3];
    for (k, o) in out.iter_mut().enumerate() {
        let a = WB_INTERACTION[iw][it][k] * (1.0 - ft) + WB_INTERACTION[iw][it + 1][k] * ft;
        let b =
            WB_INTERACTION[iw + 1][it][k] * (1.0 - ft) + WB_INTERACTION[iw + 1][it + 1][k] * ft;
        *o = a * (1.0 - fw) + b * fw;
    }
    out
}

fn white_balance(px: Rgba, warmth: f32, tint: f32) -> Rgba {
    // Each slider's measured grey gains, multiplied together.
    let (kw, kt) = (
        slider_log_gains(&WARMTH_LOG_GAINS, warmth / 100.0),
        slider_log_gains(&TINT_LOG_GAINS, tint / 100.0),
    );
    let g = [
        (kw[0] + kt[0]).exp(),
        (kw[1] + kt[1]).exp(),
        (kw[2] + kt[2]).exp(),
    ];
    // Bradford cone matrix times sRGB->XYZ (D65), and its inverse.
    const B: [[f32; 3]; 3] = [
        [0.8951, 0.2664, -0.1614],
        [-0.7502, 1.7135, 0.0367],
        [0.0389, -0.0685, 1.0296],
    ];
    const R2X: [[f32; 3]; 3] = [
        [0.412_456, 0.357_576, 0.180_437],
        [0.212_673, 0.715_152, 0.072_175],
        [0.019_334, 0.119_192, 0.950_304],
    ];
    fn matmul(a: &[[f32; 3]; 3], b: &[[f32; 3]; 3]) -> [[f32; 3]; 3] {
        let mut out = [[0.0f32; 3]; 3];
        for (i, row) in out.iter_mut().enumerate() {
            for (j, v) in row.iter_mut().enumerate() {
                *v = (0..3).map(|k| a[i][k] * b[k][j]).sum();
            }
        }
        out
    }
    fn matvec(a: &[[f32; 3]; 3], v: [f32; 3]) -> [f32; 3] {
        [
            a[0][0] * v[0] + a[0][1] * v[1] + a[0][2] * v[2],
            a[1][0] * v[0] + a[1][1] * v[1] + a[1][2] * v[2],
            a[2][0] * v[0] + a[2][1] * v[1] + a[2][2] * v[2],
        ]
    }
    fn inv3(m: &[[f32; 3]; 3]) -> [[f32; 3]; 3] {
        let c = |r: usize, cc: usize| m[r][cc];
        let det = c(0, 0) * (c(1, 1) * c(2, 2) - c(1, 2) * c(2, 1))
            - c(0, 1) * (c(1, 0) * c(2, 2) - c(1, 2) * c(2, 0))
            + c(0, 2) * (c(1, 0) * c(2, 1) - c(1, 1) * c(2, 0));
        let d = 1.0 / det;
        [
            [
                (c(1, 1) * c(2, 2) - c(1, 2) * c(2, 1)) * d,
                (c(0, 2) * c(2, 1) - c(0, 1) * c(2, 2)) * d,
                (c(0, 1) * c(1, 2) - c(0, 2) * c(1, 1)) * d,
            ],
            [
                (c(1, 2) * c(2, 0) - c(1, 0) * c(2, 2)) * d,
                (c(0, 0) * c(2, 2) - c(0, 2) * c(2, 0)) * d,
                (c(0, 2) * c(1, 0) - c(0, 0) * c(1, 2)) * d,
            ],
            [
                (c(1, 0) * c(2, 1) - c(1, 1) * c(2, 0)) * d,
                (c(0, 1) * c(2, 0) - c(0, 0) * c(2, 1)) * d,
                (c(0, 0) * c(1, 1) - c(0, 1) * c(1, 0)) * d,
            ],
        ]
    }
    let bm = matmul(&B, &R2X);
    let u = matvec(&bm, [1.0, 1.0, 1.0]);
    let bg = matvec(&bm, g);
    // The two sliders are not independent: Affinity moves one white
    // point in two dimensions rather than adapting twice, so the
    // product of the two tables is only right on the axes. Correct it
    // by the measured interaction (zero along both axes by
    // construction).
    let c = white_balance_interaction(warmth, tint);
    let d = [
        bg[0] / u[0] * c[0].exp(),
        bg[1] / u[1] * c[1].exp(),
        bg[2] / u[2] * c[2].exp(),
    ];
    let dec = |v: f32| {
        let v = v.clamp(0.0, 1.0);
        if v <= 0.04045 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    let enc = |v: f32| {
        let v = v.clamp(0.0, 1.0);
        if v <= 0.003_130_8 {
            12.92 * v
        } else {
            1.055 * v.powf(1.0 / 2.4) - 0.055
        }
    };
    let lin = [dec(px.r), dec(px.g), dec(px.b)];
    let lms = matvec(&bm, lin);
    let adapted = [lms[0] * d[0], lms[1] * d[1], lms[2] * d[2]];
    let out = matvec(&inv3(&bm), adapted);
    Rgba {
        r: enc(out[0]),
        g: enc(out[1]),
        b: enc(out[2]),
        a: px.a,
    }
}

fn luma(px: Rgba) -> f32 {
    0.299 * px.r + 0.587 * px.g + 0.114 * px.b
}

/// Move a pixel onto a target luma without changing its colour, the way
/// Photoshop's `SetLum`/`ClipColor` pair does — and, as the lens-filter
/// probes show, the way Affinity's "Preserve Luminosity" does too.
///
/// Adding the luma difference to all three channels keeps the hue and
/// saturation exactly; where that pushes a channel out of range the
/// triple is pulled back towards its own luma instead of being clipped
/// per channel, which is what stops a filtered white from turning
/// orange (a straight rescale gets that pixel badly wrong).
fn set_lum(px: Rgba, target: f32) -> Rgba {
    let d = target - luma(px);
    let mut c = [px.r + d, px.g + d, px.b + d];
    let l = 0.299 * c[0] + 0.587 * c[1] + 0.114 * c[2];
    let lo = c[0].min(c[1]).min(c[2]);
    if lo < 0.0 && l - lo > 1e-6 {
        for v in c.iter_mut() {
            *v = l + (*v - l) * l / (l - lo);
        }
    }
    let hi = c[0].max(c[1]).max(c[2]);
    if hi > 1.0 && hi - l > 1e-6 {
        for v in c.iter_mut() {
            *v = l + (*v - l) * (1.0 - l) / (hi - l);
        }
    }
    Rgba {
        r: c[0].clamp(0.0, 1.0),
        g: c[1].clamp(0.0, 1.0),
        b: c[2].clamp(0.0, 1.0),
        a: px.a,
    }
}


/// Scale a pixel's CIELAB chroma about its own lightness, keeping hue,
/// and clip back into sRGB. Affinity saturates this way.
/// Scale a pixel's CIELAB chroma by a factor the caller picks from that
/// chroma and the Lab hue angle in degrees, keeping L* and the hue.
fn scale_chroma_by(px: Rgba, gain: impl Fn(f32, f32) -> f32) -> Rgba {
    const M: [[f32; 3]; 3] = [
        [0.4124, 0.3576, 0.1805],
        [0.2126, 0.7152, 0.0722],
        [0.0193, 0.1192, 0.9505],
    ];
    const MI: [[f32; 3]; 3] = [
        [3.240_97, -1.537_383, -0.498_611],
        [-0.969_244, 1.875_968, 0.041_555],
        [0.055_63, -0.203_977, 1.056_972],
    ];
    let dec = |v: f32| {
        let v = v.clamp(0.0, 1.0);
        if v <= 0.04045 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    let enc = |v: f32| {
        let v = v.clamp(0.0, 1.0);
        if v <= 0.003_130_8 {
            12.92 * v
        } else {
            1.055 * v.powf(1.0 / 2.4) - 0.055
        }
    };
    let mul = |m: &[[f32; 3]; 3], v: [f32; 3]| {
        [
            m[0][0] * v[0] + m[0][1] * v[1] + m[0][2] * v[2],
            m[1][0] * v[0] + m[1][1] * v[1] + m[1][2] * v[2],
            m[2][0] * v[0] + m[2][1] * v[1] + m[2][2] * v[2],
        ]
    };
    const D: f32 = 6.0 / 29.0;
    let f = |t: f32| {
        if t > D * D * D {
            t.cbrt()
        } else {
            t / (3.0 * D * D) + 4.0 / 29.0
        }
    };
    let fi = |t: f32| {
        if t > D {
            t * t * t
        } else {
            3.0 * D * D * (t - 4.0 / 29.0)
        }
    };
    let white = mul(&M, [1.0, 1.0, 1.0]);
    let xyz = mul(&M, [dec(px.r), dec(px.g), dec(px.b)]);
    let fv = [
        f(xyz[0] / white[0]),
        f(xyz[1] / white[1]),
        f(xyz[2] / white[2]),
    ];
    let (l, a, b) = (
        116.0 * fv[1] - 16.0,
        500.0 * (fv[0] - fv[1]),
        200.0 * (fv[1] - fv[2]),
    );
    let k = gain(a.hypot(b), b.atan2(a).to_degrees().rem_euclid(360.0));
    let (a, b) = (a * k, b * k);
    let fy = (l + 16.0) / 116.0;
    let back = [
        fi(fy + a / 500.0) * white[0],
        fi(fy) * white[1],
        fi(fy - b / 200.0) * white[2],
    ];
    let rgb = mul(&MI, back);
    Rgba {
        r: enc(rgb[0]),
        g: enc(rgb[1]),
        b: enc(rgb[2]),
        a: px.a,
    }
}

/// Affinity's Vibrance weighting, as probed with the 64^3 RGB cube in
/// fixtures/affinity-probe/cube_vib100.af, cube_vib50.af and
/// cube_vibneg100.af (whose embedded thumbnails are byte-exact renders,
/// so each file is a complete transfer function).
///
/// Turned *down* it is nothing but a weaker Saturation: at -100 every
/// pixel comes back at exactly half its chroma, whatever its hue, so
/// the gain there is a flat `1 + t/2`.
///
/// Turned *up* it is a chroma gain shaped in two independent ways:
///
/// * by hue, a protection window over the skin tones — the gain is
///   exactly 1 between Lab hue 30 deg and 45 deg, and ramps to full
///   over the next 45 deg either side, so oranges and reds stay put
///   while everything else moves;
/// * by chroma, [`VIBRANCE_BOOST`], rising from nothing at grey to
///   +47 % near chroma 58 and easing off again past it.
///
/// The slider does not simply scale the result: at half strength the
/// curve is read at a *lower* chroma as well, which is what makes the
/// 50 % fixture land where it does (residual 1.4 % of the gain, against
/// 2.4 % for a plain `1 + t*A(C)`). The 0.7 exponent on that is fitted.
fn vibrance_gain(t: f32, chroma: f32, hue: f32) -> f32 {
    if t <= 0.0 {
        return 1.0 + t * 0.5;
    }
    // The window straddles 0 deg, so measure hue on (-135, 225].
    let h = if hue > 225.0 { hue - 360.0 } else { hue };
    let protect = if h < 30.0 {
        ((30.0 - h) / 45.0).clamp(0.0, 1.0)
    } else if h > 45.0 {
        ((h - 45.0) / 45.0).clamp(0.0, 1.0)
    } else {
        0.0
    };
    if protect <= 0.0 {
        return 1.0;
    }
    1.0 + t * protect * vibrance_boost(t.powf(0.7) * chroma)
}

/// The full-strength Vibrance boost against CIELAB chroma, on 5-unit
/// centres; grey is untouched and the tail holds at the last measurement.
const VIBRANCE_BOOST: [f32; 20] = [
    0.0, 0.0424, 0.1367, 0.2183, 0.2804, 0.3330, 0.3748, 0.4061, 0.4294, 0.4468, 0.4587, 0.4657,
    0.4684, 0.4676, 0.4639, 0.4559, 0.4410, 0.4163, 0.3934, 0.3670,
];

fn vibrance_boost(chroma: f32) -> f32 {
    // Knots at chroma 0, then 2.5 and every 5 after it.
    if chroma <= 0.0 {
        return 0.0;
    }
    if chroma <= 2.5 {
        return VIBRANCE_BOOST[1] * (chroma / 2.5);
    }
    let x = (chroma - 2.5) / 5.0;
    let i = x.floor() as usize + 1;
    if i + 1 >= VIBRANCE_BOOST.len() {
        return VIBRANCE_BOOST[VIBRANCE_BOOST.len() - 1];
    }
    let f = x - x.floor();
    VIBRANCE_BOOST[i] * (1.0 - f) + VIBRANCE_BOOST[i + 1] * f
}

/// The colour ranges Selective Color works on, in `ranges` order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectiveRange {
    Reds = 0,
    Yellows = 1,
    Greens = 2,
    Cyans = 3,
    Blues = 4,
    Magentas = 5,
}

impl SelectiveRange {
    pub const ALL: [SelectiveRange; 6] = [
        SelectiveRange::Reds,
        SelectiveRange::Yellows,
        SelectiveRange::Greens,
        SelectiveRange::Cyans,
        SelectiveRange::Blues,
        SelectiveRange::Magentas,
    ];

    pub fn label(self) -> &'static str {
        match self {
            SelectiveRange::Reds => "Reds",
            SelectiveRange::Yellows => "Yellows",
            SelectiveRange::Greens => "Greens",
            SelectiveRange::Cyans => "Cyans",
            SelectiveRange::Blues => "Blues",
            SelectiveRange::Magentas => "Magentas",
        }
    }
}

/// Selective Color: CMYK nudges applied only to the colour ranges a pixel
/// belongs to.
///
/// A pixel's membership of a range is how much of its saturation that
/// range accounts for, so a pure red is entirely in Reds while an orange
/// is split between Reds and Yellows. That is what makes the adjustment
/// selective rather than global.
fn selective_color(px: Rgba, ranges: &[[f32; 4]; 6], relative: bool) -> Rgba {
    let max = px.r.max(px.g).max(px.b);
    let min = px.r.min(px.g).min(px.b);
    let mid = px.r + px.g + px.b - max - min;
    let sat = max - min;
    if sat <= 1e-5 {
        return px;
    }
    // Weight of each of the six ranges for this pixel.
    let mut w = [0f32; 6];
    let (primary, secondary) = if max == px.r {
        if px.g >= px.b {
            (0usize, 1usize) // red -> yellow
        } else {
            (0, 5) // red -> magenta
        }
    } else if max == px.g {
        if px.b >= px.r {
            (2, 3) // green -> cyan
        } else {
            (2, 1) // green -> yellow
        }
    } else if px.r >= px.g {
        (4, 5) // blue -> magenta
    } else {
        (4, 3) // blue -> cyan
    };
    // How far the middle channel sits between the min and max decides the
    // split between the primary and the neighbouring secondary.
    let t = ((mid - min) / sat).clamp(0.0, 1.0);
    w[primary] = 1.0 - t;
    w[secondary] = t;

    // Convert to CMYK properly: pull the black out first, then normalise
    // the remaining ink. Skipping the normalisation makes the round trip
    // lossy, so an untouched Selective Color would darken every pixel.
    let k0 = 1.0 - max;
    let denom = 1.0 - k0;
    let mut cmy = if denom > 1e-5 {
        [
            (1.0 - px.r - k0) / denom,
            (1.0 - px.g - k0) / denom,
            (1.0 - px.b - k0) / denom,
        ]
    } else {
        [0.0; 3]
    };
    let mut k = k0;
    for (i, weight) in w.iter().enumerate() {
        if *weight <= 0.0 {
            continue;
        }
        let adj = ranges[i];
        for c in 0..3 {
            let d = adj[c] / 100.0 * weight;
            cmy[c] = if relative {
                cmy[c] + cmy[c] * d
            } else {
                cmy[c] + d
            };
        }
        let dk = adj[3] / 100.0 * weight;
        k = if relative { k + k * dk } else { k + dk };
    }
    let k = k.clamp(0.0, 1.0);
    let f = |c: f32| ((1.0 - c.clamp(0.0, 1.0)) * (1.0 - k)).clamp(0.0, 1.0);
    // (1 - C)(1 - K) is the exact inverse of the extraction above.
    Rgba {
        r: f(cmy[0]),
        g: f(cmy[1]),
        b: f(cmy[2]),
        a: px.a,
    }
}

/// Slider keys for Color Balance, e.g. "sh_cr" for shadows cyan/red.
/// Interned so `ParamSpec` can stay `&'static str`.
fn balance_key(band: &str, channel: &str) -> &'static str {
    match (band, channel) {
        ("sh", "cr") => "sh_cr",
        ("sh", "mg") => "sh_mg",
        ("sh", "yb") => "sh_yb",
        ("mid", "cr") => "mid_cr",
        ("mid", "mg") => "mid_mg",
        ("mid", "yb") => "mid_yb",
        ("hi", "cr") => "hi_cr",
        ("hi", "mg") => "hi_mg",
        _ => "hi_yb",
    }
}

fn balance_label(band: &str, channel: &str) -> &'static str {
    match (band, channel) {
        ("sh", "Cyan/Red") => "Shadows C/R",
        ("sh", "Magenta/Green") => "Shadows M/G",
        ("sh", "Yellow/Blue") => "Shadows Y/B",
        ("mid", "Cyan/Red") => "Midtones C/R",
        ("mid", "Magenta/Green") => "Midtones M/G",
        ("mid", "Yellow/Blue") => "Midtones Y/B",
        ("hi", "Cyan/Red") => "Highlights C/R",
        ("hi", "Magenta/Green") => "Highlights M/G",
        _ => "Highlights Y/B",
    }
}

fn selective_key(range: usize, channel: usize) -> &'static str {
    const KEYS: [[&str; 4]; 6] = [
        ["r_c", "r_m", "r_y", "r_k"],
        ["y_c", "y_m", "y_y", "y_k"],
        ["g_c", "g_m", "g_y", "g_k"],
        ["c_c", "c_m", "c_y", "c_k"],
        ["b_c", "b_m", "b_y", "b_k"],
        ["m_c", "m_m", "m_y", "m_k"],
    ];
    KEYS[range.min(5)][channel.min(3)]
}

fn selective_label(range: SelectiveRange, channel: &str) -> &'static str {
    const LABELS: [[&str; 4]; 6] = [
        ["Reds: Cyan", "Reds: Magenta", "Reds: Yellow", "Reds: Black"],
        [
            "Yellows: Cyan",
            "Yellows: Magenta",
            "Yellows: Yellow",
            "Yellows: Black",
        ],
        [
            "Greens: Cyan",
            "Greens: Magenta",
            "Greens: Yellow",
            "Greens: Black",
        ],
        [
            "Cyans: Cyan",
            "Cyans: Magenta",
            "Cyans: Yellow",
            "Cyans: Black",
        ],
        [
            "Blues: Cyan",
            "Blues: Magenta",
            "Blues: Yellow",
            "Blues: Black",
        ],
        [
            "Magentas: Cyan",
            "Magentas: Magenta",
            "Magentas: Yellow",
            "Magentas: Black",
        ],
    ];
    let c = ["Cyan", "Magenta", "Yellow", "Black"]
        .iter()
        .position(|x| *x == channel)
        .unwrap_or(0);
    LABELS[range as usize][c]
}

#[cfg(test)]
mod new_adjustment_tests {
    use super::*;

    fn px(r: f32, g: f32, b: f32) -> Rgba {
        Rgba::new(r, g, b, 1.0)
    }

    #[test]
    fn defaults_are_identity() {
        // Opening an adjustment and touching nothing must not change a
        // single pixel.
        let sample = [
            px(0.0, 0.0, 0.0),
            px(1.0, 1.0, 1.0),
            px(0.2, 0.6, 0.9),
            px(0.8, 0.3, 0.1),
            px(0.5, 0.5, 0.5),
        ];
        for kind in [
            AdjustmentKind::ColorBalance,
            AdjustmentKind::Vibrance,
            AdjustmentKind::Exposure,
            AdjustmentKind::SelectiveColor,
            AdjustmentKind::ChannelMixer,
        ] {
            let p = Params::default_for(kind);
            for c in sample {
                let out = p.apply(c);
                assert!(
                    (out.r - c.r).abs() < 1e-3
                        && (out.g - c.g).abs() < 1e-3
                        && (out.b - c.b).abs() < 1e-3,
                    "{kind:?} default is not identity: {c:?} -> {out:?}"
                );
            }
        }
    }

    #[test]
    fn color_balance_moves_the_band_it_is_told_to() {
        let mut p = Params::default_for(AdjustmentKind::ColorBalance);
        if let Params::ColorBalance {
            preserve_luminosity,
            ..
        } = &mut p
        {
            *preserve_luminosity = false;
        }
        p.set_param("hi_cr", 100.0);
        // A highlight goes redder...
        let hi = p.apply(px(0.9, 0.9, 0.9));
        assert!(hi.r > hi.g, "highlight did not go red: {hi:?}");
        // ...while a shadow is left alone.
        let sh = p.apply(px(0.05, 0.05, 0.05));
        assert!(
            (sh.r - sh.g).abs() < 0.02,
            "shadow moved with a highlights slider: {sh:?}"
        );
    }

    #[test]
    fn vibrance_spares_already_saturated_colours() {
        let mut p = Params::default_for(AdjustmentKind::Vibrance);
        p.set_param("vibrance", 100.0);
        let dull = px(0.5, 0.45, 0.4);
        let vivid = px(1.0, 0.0, 0.0);
        let dull_gain = {
            let o = p.apply(dull);
            (o.r - o.b).abs() - (dull.r - dull.b).abs()
        };
        let vivid_gain = {
            let o = p.apply(vivid);
            (o.r - o.b).abs() - (vivid.r - vivid.b).abs()
        };
        assert!(
            dull_gain > vivid_gain,
            "vibrance should push the dull colour harder ({dull_gain} vs {vivid_gain})"
        );
    }

    #[test]
    fn exposure_is_a_stop_per_unit() {
        let mut p = Params::default_for(AdjustmentKind::Exposure);
        p.set_param("exposure", 1.0);
        let out = p.apply(px(0.25, 0.25, 0.25));
        assert!(
            (out.r - 0.5).abs() < 1e-3,
            "one stop should double the value: {out:?}"
        );
    }

    #[test]
    fn gradient_map_replaces_colour_with_the_ramp() {
        let p = Params::GradientMap {
            from: [1.0, 0.0, 0.0],
            to: [0.0, 0.0, 1.0],
            reverse: false,
            stops: Vec::new(),
        };
        let dark = p.apply(px(0.0, 0.0, 0.0));
        assert!(
            dark.r > 0.9 && dark.b < 0.1,
            "black did not map to the low end"
        );
        let light = p.apply(px(1.0, 1.0, 1.0));
        assert!(
            light.b > 0.9 && light.r < 0.1,
            "white did not map to the high end"
        );
    }

    #[test]
    fn photo_filter_warms_without_changing_brightness() {
        let p = Params::default_for(AdjustmentKind::PhotoFilter);
        let before = px(0.5, 0.5, 0.5);
        let after = p.apply(before);
        assert!(after.r > after.b, "warming filter did not warm: {after:?}");
        let (lb, la) = (
            0.299 * before.r + 0.587 * before.g + 0.114 * before.b,
            0.299 * after.r + 0.587 * after.g + 0.114 * after.b,
        );
        assert!(
            (la - lb).abs() < 0.02,
            "Preserve Luminosity did not hold brightness: {lb} -> {la}"
        );
    }

    #[test]
    fn selective_color_touches_only_the_named_range() {
        let mut p = Params::default_for(AdjustmentKind::SelectiveColor);
        // Add magenta to the reds, which pulls green out of them.
        p.set_param("r_m", 100.0);
        let red = p.apply(px(0.8, 0.3, 0.3));
        let blue = p.apply(px(0.3, 0.3, 0.8));
        assert!(red.g < 0.25, "reds were not affected: {red:?}");
        assert!(
            (blue.b - 0.8).abs() < 0.02 && (blue.g - 0.3).abs() < 0.02,
            "blues moved with a reds slider: {blue:?}"
        );
    }

    #[test]
    fn channel_mixer_monochrome_flattens_to_grey() {
        let mut p = Params::default_for(AdjustmentKind::ChannelMixer);
        if let Params::ChannelMixer { monochrome, .. } = &mut p {
            *monochrome = true;
        }
        let out = p.apply(px(1.0, 0.0, 0.0));
        assert!(
            (out.r - out.g).abs() < 1e-4 && (out.g - out.b).abs() < 1e-4,
            "monochrome mix is not grey: {out:?}"
        );
    }

    #[test]
    fn every_creatable_adjustment_has_a_name_and_survives_a_round_trip() {
        for kind in Params::creatable() {
            let p = Params::default_for(*kind);
            assert_eq!(p.kind(), *kind, "kind() disagrees with default_for");
            assert!(!p.display_name().is_empty(), "{kind:?} has no display name");
            let json = serde_json::to_string(&p).expect("serialise");
            let back: Params = serde_json::from_str(&json).expect("deserialise");
            assert_eq!(back, p, "{kind:?} did not survive a JSON round trip");
        }
    }
}

#[cfg(test)]
mod curve_editing_tests {
    use super::*;

    #[test]
    fn adding_points_keeps_the_list_sorted() {
        let mut c = Curve::default();
        c.add_point(0.5, 0.7, 0.02);
        c.add_point(0.25, 0.2, 0.02);
        let xs: Vec<f32> = c.points.iter().map(|p| p.0).collect();
        assert_eq!(xs, vec![0.0, 0.25, 0.5, 1.0]);
    }

    #[test]
    fn a_click_near_an_existing_point_replaces_it() {
        // Otherwise a drag leaves a pile of nearly coincident points.
        let mut c = Curve::default();
        c.add_point(0.5, 0.5, 0.02);
        let before = c.points.len();
        c.add_point(0.505, 0.9, 0.02);
        assert_eq!(c.points.len(), before, "a duplicate point was added");
        assert_eq!(c.points[1], (0.505, 0.9));
    }

    #[test]
    fn moving_a_point_cannot_reorder_the_curve() {
        let mut c = Curve::default();
        c.add_point(0.3, 0.3, 0.02);
        c.add_point(0.7, 0.7, 0.02);
        // Drag the middle point far past its right-hand neighbour.
        c.move_point(1, 0.99, 0.4);
        let xs: Vec<f32> = c.points.iter().map(|p| p.0).collect();
        assert!(
            xs.windows(2).all(|w| w[0] < w[1]),
            "curve folded back on itself: {xs:?}"
        );
        assert!(xs[1] < xs[2], "point passed its neighbour");
    }

    #[test]
    fn the_endpoints_only_move_vertically() {
        let mut c = Curve::default();
        c.move_point(0, 0.4, 0.2);
        assert_eq!(c.points[0], (0.0, 0.2), "left endpoint slid inwards");
        c.move_point(1, 0.4, 0.8);
        assert_eq!(c.points[1], (1.0, 0.8), "right endpoint slid inwards");
    }

    #[test]
    fn the_endpoints_cannot_be_removed() {
        let mut c = Curve::default();
        c.add_point(0.5, 0.9, 0.02);
        c.remove_point(0);
        c.remove_point(2);
        assert_eq!(c.points.len(), 3, "an endpoint was removed");
        c.remove_point(1);
        assert_eq!(c.points.len(), 2, "the middle point survived");
    }

    #[test]
    fn hit_testing_picks_the_nearest_point() {
        let mut c = Curve::default();
        c.add_point(0.3, 0.3, 0.02);
        c.add_point(0.6, 0.6, 0.02);
        assert_eq!(c.hit(0.31, 0.29, 0.05), Some(1));
        assert_eq!(c.hit(0.62, 0.61, 0.05), Some(2));
        assert_eq!(c.hit(0.45, 0.1, 0.05), None, "hit nothing nearby");
    }

    #[test]
    fn a_lifted_midpoint_brightens_the_midtones_only() {
        let mut c = Curve::default();
        c.add_point(0.5, 0.75, 0.02);
        assert!((c.eval(0.0) - 0.0).abs() < 1e-4, "black point moved");
        assert!((c.eval(1.0) - 1.0).abs() < 1e-4, "white point moved");
        assert!(c.eval(0.5) > 0.7, "midtones were not lifted");
    }

    #[test]
    fn reset_returns_the_identity() {
        let mut c = Curve::default();
        c.add_point(0.5, 0.9, 0.02);
        assert!(!c.is_identity());
        c.reset();
        assert!(c.is_identity());
    }

    #[test]
    fn channels_are_addressed_independently() {
        let mut curves = Curves::default();
        curves
            .channel_mut(CurveChannel::Red)
            .add_point(0.5, 0.9, 0.02);
        assert!(!curves.channel(CurveChannel::Red).is_identity());
        assert!(curves.channel(CurveChannel::Blue).is_identity());
        assert!(curves.channel(CurveChannel::Rgb).is_identity());

        let p = Params::Curves(curves);
        let out = p.apply(Rgba::new(0.5, 0.5, 0.5, 1.0));
        assert!(out.r > 0.7, "red curve did not apply");
        assert!((out.b - 0.5).abs() < 0.01, "blue was changed too");
    }
    /// Every kind the reader understands must survive encode -> parse.
    #[test]
    fn adjustment_params_round_trip_through_psd_bytes() {
        let cases: Vec<(AdjustmentKind, Params)> = vec![
            (AdjustmentKind::Invert, Params::Invert),
            (AdjustmentKind::Posterize, Params::Posterize { levels: 7 }),
            (
                AdjustmentKind::Threshold,
                Params::Threshold {
                    level: 100.0 / 255.0,
                },
            ),
            (
                AdjustmentKind::BrightnessContrast,
                Params::BrightnessContrast {
                    brightness: 25.0,
                    contrast: -40.0,
                },
            ),
            (
                AdjustmentKind::HueSaturation,
                Params::HueSaturation {
                    hue: 30.0,
                    saturation: -20.0,
                    lightness: 15.0,
                    colorize: true,
                    lightness_desaturates: false,
                    reciprocal_saturation: false,
                    // PSD's own per-range tweaks are a separate slot we
                    // don't parse, so an Affinity import's `ranges`
                    // simply don't survive a PSD round-trip.
                    ranges: Vec::new(),
                },
            ),
            (
                AdjustmentKind::BlackWhite,
                Params::BlackWhite {
                    reds: 10.0,
                    yellows: 20.0,
                    greens: 30.0,
                    cyans: 40.0,
                    blues: 50.0,
                    magentas: 60.0,
                },
            ),
        ];
        for (kind, params) in cases {
            let raw =
                encode_psd(kind, &params).unwrap_or_else(|| panic!("{kind:?} has no encoder"));
            let back = parse_psd(kind, &raw);
            assert_eq!(back, params, "{kind:?} did not round-trip");
        }
    }

    #[test]
    fn levels_round_trip_through_psd_bytes() {
        let levels = Levels {
            rgb: LevelsChannel {
                input_black: 10.0 / 255.0,
                input_white: 240.0 / 255.0,
                output_black: 5.0 / 255.0,
                output_white: 250.0 / 255.0,
                gamma: 1.25,
            },
            ..Levels::default()
        };
        let params = Params::Levels(levels);
        let raw = encode_psd(AdjustmentKind::Levels, &params).unwrap();
        assert_eq!(raw.len(), 2 + 29 * 10, "photoshop expects 29 records");
        assert_eq!(parse_psd(AdjustmentKind::Levels, &raw), params);
    }

    #[test]
    fn curves_round_trip_through_psd_bytes() {
        let curves = Curves {
            rgb: Curve {
                points: vec![(0.0, 0.0), (128.0 / 255.0, 200.0 / 255.0), (1.0, 1.0)],
            },
            ..Curves::default()
        };
        let params = Params::Curves(curves);
        let raw = encode_psd(AdjustmentKind::Curves, &params).unwrap();
        assert_eq!(parse_psd(AdjustmentKind::Curves, &raw), params);
    }

    #[test]
    fn solid_colour_round_trips_through_psd_bytes() {
        let params = Params::SolidColor {
            rgba: [64.0 / 255.0, 128.0 / 255.0, 192.0 / 255.0, 1.0],
        };
        let raw = encode_psd(AdjustmentKind::SolidColor, &params).unwrap();
        match parse_psd(AdjustmentKind::SolidColor, &raw) {
            Params::SolidColor { rgba } => {
                for (a, b) in rgba
                    .iter()
                    .zip([64.0 / 255.0, 128.0 / 255.0, 192.0 / 255.0, 1.0])
                {
                    assert!((a - b).abs() < 1.0 / 255.0, "{rgba:?}");
                }
            }
            other => panic!("parsed back as {other:?}"),
        }
    }

    #[test]
    fn kinds_without_an_encoder_say_so() {
        // Better an honest None, so the caller keeps whatever raw bytes the
        // file arrived with, than a wrong block.
        assert!(encode_psd(AdjustmentKind::Vibrance, &Params::Unsupported).is_none());
        assert!(encode_psd(AdjustmentKind::GradientMap, &Params::Unsupported).is_none());
    }
}
