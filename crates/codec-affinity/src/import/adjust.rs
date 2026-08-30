//! Adjustment layers: curves, HSL, and the parametric kinds.

use super::*;

impl Walker<'_> {
    /// Rebuild a curves adjustment layer ("CrRA").
    ///
    /// `AdjP` → "CrvP" holds one spline per channel: `Mast` master and
    /// `C1Sp`/`C2Sp`/`C3Sp` for R/G/B. A spline is `Cnt` control points
    /// with `Vals` laid out as xs, then ys, then tangents (which our
    /// Catmull-Rom evaluation approximates well enough to drop).
    pub(super) fn curves_adjustment(&mut self, node: &Node, name: &str) -> Option<Layer> {
        let adj = self.graph.child(node, b"AdjP")?;
        if &adj.type_tag().to_be_bytes() != b"CrvP" {
            return None;
        }
        let curve_of = |tag: &[u8; 4]| -> schist_adjustments::Curve {
            let mut curve = schist_adjustments::Curve::default();
            let Some(spline) = self.graph.child(adj, tag) else {
                return curve;
            };
            let count = match spline.field(b"Cnt ") {
                Some(Value::I32(n)) => *n as usize,
                _ => return curve,
            };
            let Some(Value::Array(vals)) = spline.field(b"Vals") else {
                return curve;
            };
            if count < 2 || vals.len() < count * 2 {
                return curve;
            }
            let v = |i: usize| match vals.get(i) {
                Some(Value::F64(f)) => *f as f32,
                _ => 0.0,
            };
            curve.points = (0..count.min(16))
                .map(|i| (v(i).clamp(0.0, 1.0), v(count + i).clamp(0.0, 1.0)))
                .collect();
            curve
        };
        let params = schist_adjustments::Params::Curves(schist_adjustments::Curves {
            rgb: curve_of(b"Mast"),
            red: curve_of(b"C1Sp"),
            green: curve_of(b"C2Sp"),
            blue: curve_of(b"C3Sp"),
        });

        let mut layer = Layer::new_raster(if name.is_empty() { "Curves" } else { name });
        layer.kind = schist_core::LayerKind::Adjustment(schist_core::AdjustmentData {
            kind: schist_core::AdjustmentKind::Curves,
            raw: Vec::new(),
            params_json: serde_json::to_string(&params).ok(),
        });
        self.report.adjustments += 1;
        Some(layer)
    }

    /// The per-hue-range tweaks of an "HSSP" HSL adjustment.
    ///
    /// `RngC` is six trapezoids, four boundary angles each in degrees
    /// (reds are 315, 345, 15, 45 — the ramps overlap their
    /// neighbours', so the six weights sum to 1 at every hue), and
    /// `HueC`/`SatC`/`LumC` are the six shifts in the same units and
    /// with the same sign convention as the master `HueA`/`SatA`/
    /// `LumA`. Ranges left at zero are dropped.
    pub(super) fn hsl_ranges(adj: &Node) -> Vec<schist_adjustments::HueRange> {
        let floats = |t: &[u8; 4]| match adj.field(t) {
            Some(Value::Array(v)) => v
                .iter()
                .map(|x| match x {
                    Value::F32(f) => *f,
                    Value::F64(f) => *f as f32,
                    _ => 0.0,
                })
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        };
        let (bounds, hue, sat, lum) = (
            floats(b"RngC"),
            floats(b"HueC"),
            floats(b"SatC"),
            floats(b"LumC"),
        );
        let count = [hue.len(), sat.len(), lum.len(), bounds.len() / 4]
            .into_iter()
            .min()
            .unwrap_or(0);
        (0..count)
            .filter(|&i| hue[i] != 0.0 || sat[i] != 0.0 || lum[i] != 0.0)
            .map(|i| schist_adjustments::HueRange {
                bounds: [
                    bounds[4 * i],
                    bounds[4 * i + 1],
                    bounds[4 * i + 2],
                    bounds[4 * i + 3],
                ],
                hue: (hue[i] * 360.0).clamp(-180.0, 180.0),
                saturation: (sat[i] * 100.0).clamp(-100.0, 100.0),
                lightness: (lum[i] * 100.0).clamp(-100.0, 100.0),
            })
            .collect()
    }

    /// Rebuild an HSL adjustment layer ("HsRA").
    ///
    /// `AdjP` → "HSSP": master shifts `HueA` (a fraction of the full
    /// turn), `SatA` and `LumA` (fractions of full range), an `HSV`
    /// mode flag, and six per-hue-range tweak arrays (`HueC`/`SatC`/
    /// `LumC` over the `RngC` boundaries) that our adjustment doesn't
    /// model — kept master-only with a warning when they're in use.
    pub(super) fn hsl_adjustment(&mut self, node: &Node, name: &str) -> Option<Layer> {
        let adj = self.graph.child(node, b"AdjP")?;
        if &adj.type_tag().to_be_bytes() != b"HSSP" {
            return None;
        }
        let f = |t: &[u8; 4]| f32_of(adj, t).unwrap_or(0.0);
        if matches!(adj.field(b"HSV "), Some(Value::Bool(true))) {
            log::warn!("affinity: HSL adjustment {name:?} uses HSV mode; applying as HSL");
        }
        let params = schist_adjustments::Params::HueSaturation {
            hue: (f(b"HueA") * 360.0).clamp(-180.0, 180.0),
            saturation: (f(b"SatA") * 100.0).clamp(-100.0, 100.0),
            lightness: (f(b"LumA") * 100.0).clamp(-100.0, 100.0),
            colorize: false,
            lightness_desaturates: true,
            reciprocal_saturation: true,
            ranges: Self::hsl_ranges(adj),
        };

        let mut layer = Layer::new_raster(if name.is_empty() { "HSL" } else { name });
        layer.kind = schist_core::LayerKind::Adjustment(schist_core::AdjustmentData {
            kind: schist_core::AdjustmentKind::HueSaturation,
            raw: Vec::new(),
            params_json: serde_json::to_string(&params).ok(),
        });
        self.report.adjustments += 1;
        Some(layer)
    }

    /// Rebuild the parametric adjustment layers whose field layouts were
    /// probed with fixture files drawn in Affinity itself
    /// (fixtures/affinity-probe): one document per adjustment, each with
    /// distinctive slider values, read back through afdump. The class
    /// behind `AdjP` (or `NAjP`, gradient map's spelling) names the type;
    /// values are fractions of the UI's percentages unless noted.
    pub(super) fn parametric_adjustment(&mut self, node: &Node, name: &str) -> Option<Layer> {
        use schist_adjustments::Params;
        let graph = self.graph;
        let tag_bytes = node.type_tag().to_be_bytes();
        let adj = match graph
            .child(node, b"AdjP")
            .or_else(|| graph.child(node, b"NAjP"))
        {
            Some(adj) => adj,
            // Invert has no parameters — and no params class.
            None if &tag_bytes == b"InRA" => node,
            None => return None,
        };
        let f = |t: &[u8; 4]| f32_of(adj, t).unwrap_or(0.0);
        let fd = |t: &[u8; 4], d: f32| f32_of(adj, t).unwrap_or(d);
        let tag = node.type_tag().to_be_bytes();
        let (kind, params, default_name) = match &tag {
            // LevP: Blac/Whit input levels, Gamm, OutB/OutW outputs, all
            // 0..1 fractions of the UI's percents; the C-arrays hold
            // per-channel variants our Levels doesn't model.
            b"LeRA" => {
                let per_channel = [b"BlkC", b"GamC", b"OBlC"].into_iter().any(|t| {
                    matches!(adj.field(t), Some(Value::Array(v)) if v.iter().any(|x| !matches!(x, Value::F32(f) if *f == 0.0 || *f == 1.0)))
                });
                if per_channel {
                    log::warn!(
                        "affinity: levels {name:?} has per-channel values; keeping master only"
                    );
                }
                let master = schist_adjustments::LevelsChannel {
                    input_black: f(b"Blac"),
                    input_white: fd(b"Whit", 1.0),
                    gamma: fd(b"Gamm", 1.0).max(0.01),
                    output_black: f(b"OutB"),
                    output_white: fd(b"OutW", 1.0),
                };
                let params = Params::Levels(schist_adjustments::Levels {
                    rgb: master,
                    ..Default::default()
                });
                (schist_core::AdjustmentKind::Levels, params, "Levels")
            }
            // ExpP: Expo is in stops applied in a power-law space whose
            // exponent is the Gamm field (2.2). Our exposure multiplies
            // the encoded value directly, so dividing the stops by that
            // gamma reproduces it exactly: (v^g * 2^E)^(1/g) = v*2^(E/g).
            b"ExRA" => (
                schist_core::AdjustmentKind::Exposure,
                Params::Exposure {
                    exposure: f(b"Expo") / fd(b"Gamm", 2.2).max(0.1),
                    offset: 0.0,
                    gamma: 1.0,
                },
                "Exposure",
            ),
            // B&CP: Brig is the percentage as a fraction; Ctrs stores
            // 1 + contrast/100. Affinity's sliders drive smooth
            // endpoint-preserving curves, not a linear remap; the
            // tables below are its actual transfer curves, read off
            // isolated probe fixtures (brightness +40%, contrast −50%
            // and +60%), and other amounts scale/blend against them.
            // The import is therefore a sampled curves adjustment.
            b"BCRA" => {
                const BRIGHT40: [f32; 17] = [
                    0.0118, 0.1015, 0.1956, 0.2887, 0.3755, 0.4576, 0.5358, 0.61, 0.6765, 0.739,
                    0.7975, 0.851, 0.8951, 0.9341, 0.9652, 0.9882, 1.0,
                ];
                const CONTRAST_N50: [f32; 17] = [
                    0.0314, 0.1252, 0.1995, 0.262, 0.3167, 0.3674, 0.4142, 0.4571, 0.5, 0.5429,
                    0.5858, 0.6326, 0.6833, 0.738, 0.8005, 0.8748, 1.0,
                ];
                const CONTRAST_P60: [f32; 17] = [
                    0.0, 0.0194, 0.0544, 0.1051, 0.1637, 0.2341, 0.3147, 0.4022, 0.498, 0.5978,
                    0.6853, 0.7659, 0.8363, 0.8949, 0.9456, 0.9806, 1.0,
                ];
                let interp = |table: &[f32; 17], v: f32| -> f32 {
                    let x = v.clamp(0.0, 1.0) * 16.0;
                    let i = (x.floor() as usize).min(15);
                    let t = x - i as f32;
                    table[i] + (table[i + 1] - table[i]) * t
                };
                if matches!(adj.field(b"Linr"), Some(Value::Bool(true))) {
                    log::warn!("affinity: brightness/contrast {name:?} is linear; applying gamma");
                }
                let bright = f(b"Brig");
                let contrast = fd(b"Ctrs", 1.0) - 1.0;
                let rgb = schist_adjustments::Curve {
                    points: (0..=16)
                        .map(|k| {
                            let v = k as f32 / 16.0;
                            let vb = v + (bright / 0.4) * (BRIGHT40[k] - v);
                            let vc = if contrast < 0.0 {
                                vb + (-contrast / 0.5) * (interp(&CONTRAST_N50, vb) - vb)
                            } else {
                                vb + (contrast / 0.6) * (interp(&CONTRAST_P60, vb) - vb)
                            };
                            (v, vc.clamp(0.0, 1.0))
                        })
                        .collect(),
                };
                let params = Params::Curves(schist_adjustments::Curves {
                    rgb,
                    ..Default::default()
                });
                (
                    schist_core::AdjustmentKind::Curves,
                    params,
                    "Brightness/Contrast",
                )
            }
            b"BWRA" => (
                schist_core::AdjustmentKind::BlackWhite,
                Params::BlackWhite {
                    reds: f(b"RedC") * 100.0,
                    yellows: f(b"Yell") * 100.0,
                    greens: f(b"Gree") * 100.0,
                    cyans: f(b"Cyan") * 100.0,
                    blues: f(b"Blue") * 100.0,
                    magentas: f(b"Mage") * 100.0,
                },
                "Black and White",
            ),
            b"CBRA" => (
                schist_core::AdjustmentKind::ColorBalance,
                Params::ColorBalance {
                    // Affinity's slider moves the channel about a tenth
                    // as far as ours per percent (fit against the probe
                    // fixture's thumbnail).
                    shadows: [f(b"ShCR") * 11.0, f(b"ShMG") * 11.0, f(b"ShYB") * 11.0],
                    midtones: [f(b"MiCR") * 11.0, f(b"MiMG") * 11.0, f(b"MiYB") * 11.0],
                    highlights: [f(b"HiCR") * 11.0, f(b"HiMG") * 11.0, f(b"HiYB") * 11.0],
                    preserve_luminosity: matches!(adj.field(b"PeLu"), Some(Value::Bool(true))),
                },
                "Colour Balance",
            ),
            // VibP: `Satu` is a plain fraction, but `Vibr` is an i32
            // on a **0..127 scale**, not a percentage — the panel's
            // 50 % writes 64 and 100 % writes 127.
            b"VbRA" => (
                schist_core::AdjustmentKind::Vibrance,
                Params::Vibrance {
                    vibrance: i32_of(adj, b"Vibr").unwrap_or(0) as f32 * 100.0 / 127.0,
                    saturation: f(b"Satu") * 100.0,
                },
                "Vibrance",
            ),
            b"InRA" => (
                schist_core::AdjustmentKind::Invert,
                Params::Invert,
                "Invert",
            ),
            b"PoRA" => (
                schist_core::AdjustmentKind::Posterize,
                Params::Posterize {
                    levels: i32_of(adj, b"Post").unwrap_or(4).clamp(2, 255) as u32,
                },
                "Posterise",
            ),
            b"ThRA" => (
                schist_core::AdjustmentKind::Threshold,
                Params::Threshold {
                    level: fd(b"Thre", 0.5),
                },
                "Threshold",
            ),
            // CnMP: Weig is five rows of six — [offset, R, G, B, A, x]
            // for the R, G, B, A and composite outputs (the probe file's
            // typed weights landed at rows[0][1..5], identity rows carry
            // their 1.0 on the moving diagonal).
            b"CMRA" => {
                let Some(Value::Array(w)) = adj.field(b"Weig") else {
                    return None;
                };
                let g = |i: usize| match w.get(i) {
                    Some(Value::F32(v)) => *v * 100.0,
                    _ => 0.0,
                };
                let row = |r: usize| [g(r * 6 + 1), g(r * 6 + 2), g(r * 6 + 3)];
                // The alpha weight contributes a flat term on opaque
                // pixels, so it folds into the constant with the offset.
                let constant = |r: usize| g(r * 6) + g(r * 6 + 4);
                (
                    schist_core::AdjustmentKind::ChannelMixer,
                    Params::ChannelMixer {
                        red: row(0),
                        green: row(1),
                        blue: row(2),
                        constant: [constant(0), constant(1), constant(2)],
                        monochrome: false,
                    },
                    "Channel Mixer",
                )
            }
            // SCoP: Weig is nine ranges of [C, M, Y, K] — the six
            // Photoshop-model ranges first, then whites/neutrals/blacks,
            // which our adjustment doesn't have.
            b"SCRA" => {
                let Some(Value::Array(w)) = adj.field(b"Weig") else {
                    return None;
                };
                let g = |i: usize| match w.get(i) {
                    Some(Value::F32(v)) => *v * 100.0,
                    _ => 0.0,
                };
                if (24..36).any(|i| g(i) != 0.0) {
                    log::warn!(
                        "affinity: selective colour {name:?} tweaks whites/neutrals/blacks; \
                         importing the six colour ranges only"
                    );
                }
                let mut ranges = [[0.0f32; 4]; 6];
                for (r, out) in ranges.iter_mut().enumerate() {
                    for (c, v) in out.iter_mut().enumerate() {
                        *v = g(r * 4 + c);
                    }
                }
                (
                    schist_core::AdjustmentKind::SelectiveColor,
                    Params::SelectiveColor {
                        ranges,
                        relative: matches!(adj.field(b"Rela"), Some(Value::Bool(true))),
                    },
                    "Selective Colour",
                )
            }
            // GraP (behind NAjP): a Grad class of stops. Our gradient map
            // is a two-colour ramp, so the first and last stops speak.
            b"GrRA" => {
                let grad = graph.child(adj, b"Grad")?;
                let cols = graph.children(grad, b"Cols");
                let rgb = |n: &&Node| -> [f32; 3] {
                    let c = color_bytes(n).unwrap_or([0, 0, 0, 255]);
                    [
                        c[0] as f32 / 255.0,
                        c[1] as f32 / 255.0,
                        c[2] as f32 / 255.0,
                    ]
                };
                // Posn pairs are (position, midpoint); the whole ramp
                // goes into the multi-stop form.
                let positions: Vec<f32> = match grad.field(b"Posn") {
                    Some(Value::Array(v)) => v
                        .iter()
                        .filter_map(|p| match p {
                            Value::VecD(d) => d.first().map(|x| *x as f32),
                            _ => None,
                        })
                        .collect(),
                    _ => Vec::new(),
                };
                let mut stops: Vec<(f32, [f32; 3])> = positions
                    .iter()
                    .zip(cols.iter())
                    .map(|(p, c)| (p.clamp(0.0, 1.0), rgb(c)))
                    .collect();
                stops.sort_by(|a, b| a.0.total_cmp(&b.0));
                let (first, last) = (cols.first()?, cols.last()?);
                (
                    schist_core::AdjustmentKind::GradientMap,
                    Params::GradientMap {
                        from: rgb(first),
                        to: rgb(last),
                        reverse: false,
                        stops,
                    },
                    "Gradient Map",
                )
            }
            // LeFP: the filter colour as three u16 Lab components (the
            // first three u16 fields, in L, a, b order — their tags
            // carry unprintable bytes), plus Dens and Pres.
            b"PfRA" => {
                let labs: Vec<u16> = adj
                    .fields
                    .iter()
                    .filter_map(|(_, v)| match v {
                        Value::U16(u) => Some(*u),
                        _ => None,
                    })
                    .take(3)
                    .collect();
                let [l, a, b] = labs.as_slice() else {
                    return None;
                };
                let color = lab_to_rgb(
                    *l as f32 / 65535.0 * 100.0,
                    *a as f32 / 65535.0 * 255.0 - 128.0,
                    *b as f32 / 65535.0 * 255.0 - 128.0,
                );
                (
                    schist_core::AdjustmentKind::PhotoFilter,
                    Params::PhotoFilter {
                        color,
                        density: (f(b"Dens") * 100.0).clamp(0.0, 100.0),
                        preserve_luminosity: matches!(adj.field(b"Pres"), Some(Value::Bool(true))),
                    },
                    "Lens Filter",
                )
            }
            // WhBP: WhBa is warmth in -100..100 (an i32), WBTi tint as
            // a fraction. A real white-balance adjustment on our side —
            // a Bradford chromatic adaptation whose grey-axis gains are
            // calibrated against warmth-only and tint-only fixtures.
            b"WBRA" => (
                schist_core::AdjustmentKind::Other(*b"WhBl"),
                Params::WhiteBalance {
                    warmth: i32_of(adj, b"WhBa").unwrap_or(0) as f32,
                    tint: f(b"WBTi") * 100.0,
                },
                "White Balance",
            ),
            // RecP: hue as a fraction of the turn, saturation and
            // lightness as fractions — a colorize in our model, whose
            // lightness is an offset about the 50% midpoint.
            b"RcRA" => (
                schist_core::AdjustmentKind::HueSaturation,
                Params::HueSaturation {
                    // Colorize reads hue as an absolute 0..360 angle.
                    // Affinity's lightness L lifts towards white as
                    // l + (1 - l) * L — exactly our positive lightness
                    // offset.
                    hue: f(b"RecH") * 360.0,
                    saturation: (f(b"RecS") * 100.0).clamp(0.0, 100.0),
                    lightness: (f(b"RecL") * 100.0).clamp(0.0, 100.0),
                    colorize: true,
                    lightness_desaturates: false,
                    reciprocal_saturation: false,
                    ranges: Vec::new(),
                },
                "Recolour",
            ),
            _ => return None,
        };
        let mut layer = Layer::new_raster(if name.is_empty() {
            format!("{default_name} Adjustment")
        } else {
            name.to_string()
        });
        layer.kind = schist_core::LayerKind::Adjustment(schist_core::AdjustmentData {
            kind,
            raw: Vec::new(),
            params_json: serde_json::to_string(&params).ok(),
        });
        self.report.adjustments += 1;
        Some(layer)
    }
}
