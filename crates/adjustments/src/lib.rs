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

mod color;
mod curves;
mod labels;
mod params;
mod prepared;
mod psd;
mod selective;
mod spec;
mod white_balance;

use color::*;
pub use curves::*;
use labels::*;
pub use params::*;
pub use prepared::*;
pub use psd::*;
pub use selective::*;
pub use spec::*;
use white_balance::*;

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
