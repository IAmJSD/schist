//! The [`Params`] enum and the pixel math each variant applies.

use super::*;

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
