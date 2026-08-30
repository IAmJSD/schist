//! What each adjustment exposes to the UI: sliders, ranges, labels.

use super::*;

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
