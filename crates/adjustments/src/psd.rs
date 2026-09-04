//! Reading and writing the PSD payloads adjustment layers preserve.

use super::*;

pub(crate) fn be_u16(d: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(d.get(at..at + 2)?.try_into().ok()?))
}

pub(crate) fn be_i16(d: &[u8], at: usize) -> Option<i16> {
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
pub(crate) fn versioned(body: Vec<u8>) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 6);
    out.extend_from_slice(&16u16.to_be_bytes());
    out.extend_from_slice(&16u32.to_be_bytes());
    out.extend_from_slice(&body);
    out
}

pub(crate) fn parse_brightness(raw: &[u8]) -> Params {
    // Legacy 'brit': brightness i16, contrast i16, mean i16, lab u8.
    match (be_i16(raw, 0), be_i16(raw, 2)) {
        (Some(b), Some(c)) => Params::BrightnessContrast {
            brightness: b as f32,
            contrast: c as f32,
        },
        _ => Params::Unsupported,
    }
}

pub(crate) fn parse_levels(raw: &[u8]) -> Params {
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

pub(crate) fn parse_hue_sat(raw: &[u8]) -> Params {
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

pub(crate) fn parse_curves(raw: &[u8]) -> Params {
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

pub(crate) fn parse_black_white(raw: &[u8]) -> Params {
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

pub(crate) fn parse_solid_color(raw: &[u8]) -> Params {
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
