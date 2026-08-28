//! Colour conversion for the `colorServices` callback.
//!
//! Plug-ins ask the host to convert between colour spaces because in
//! Photoshop the host is the one that knows the document's profile. This
//! host has no colour management yet, so these are the plain textbook
//! conversions against sRGB and D65: exact for RGB, HSB, HSL and
//! greyscale, and an approximation for CMYK, Lab and XYZ, which really
//! want a profile. That is worth knowing before trusting a CMYK number
//! that came back through here.
//!
//! Component ranges are Adobe's, from API Guide table A-4, and one of
//! them is a trap: **CMYK is stored inverted**, 0 meaning 100% ink and
//! 255 meaning none.
//!
//! Everything is deliberately self-contained rather than reaching for
//! `schist-color`, because this crate stays dependency-free for the
//! out-of-process helper that stage 3 turns it into.

use crate::abi::color_space;

/// The four `colorComponents` of a [`crate::abi::ColorServicesInfo`].
pub type Components = [i16; 4];

/// Convert `c` from one space to another, both [`color_space`] values.
///
/// `None` for a space this does not know, which the caller reports to
/// the plug-in rather than returning a wrong colour.
pub fn convert(from: i16, to: i16, c: Components) -> Option<Components> {
    let rgb = to_rgb(from, c)?;
    from_rgb(to, rgb)
}

/// Linear 0.0..=1.0 sRGB, the hub every conversion passes through.
type Rgb = (f32, f32, f32);

fn b(v: i16) -> f32 {
    (v.clamp(0, 255) as f32) / 255.0
}

fn q(v: f32) -> i16 {
    (v * 255.0).round().clamp(0.0, 255.0) as i16
}

fn to_rgb(space: i16, c: Components) -> Option<Rgb> {
    Some(match space {
        color_space::RGB => (b(c[0]), b(c[1]), b(c[2])),
        color_space::GRAY => {
            let g = b(c[0]);
            (g, g, g)
        }
        color_space::HSB => hsb_to_rgb(c[0] as f32, b(c[1]), b(c[2])),
        color_space::HSL => hsl_to_rgb(c[0] as f32, b(c[1]), b(c[2])),
        color_space::CMYK => {
            // Stored inverted: 0 is full ink.
            let ink = |v: i16| 1.0 - b(v);
            let (cy, m, y, k) = (ink(c[0]), ink(c[1]), ink(c[2]), ink(c[3]));
            (
                (1.0 - cy) * (1.0 - k),
                (1.0 - m) * (1.0 - k),
                (1.0 - y) * (1.0 - k),
            )
        }
        color_space::XYZ => xyz_to_rgb(b(c[0]), b(c[1]), b(c[2])),
        color_space::LAB => {
            let l = b(c[0]) * 100.0;
            let a = c[1].clamp(0, 255) as f32 - 128.0;
            let bb = c[2].clamp(0, 255) as f32 - 128.0;
            let (x, y, z) = lab_to_xyz(l, a, bb);
            xyz_to_rgb_linear(x, y, z)
        }
        _ => return None,
    })
}

fn from_rgb(space: i16, (r, g, bl): Rgb) -> Option<Components> {
    Some(match space {
        color_space::RGB => [q(r), q(g), q(bl), 0],
        color_space::GRAY => {
            // Rec.601 luma, which is what an 8-bit greyscale conversion
            // conventionally means.
            let y = 0.299 * r + 0.587 * g + 0.114 * bl;
            [q(y), 0, 0, 0]
        }
        color_space::HSB => {
            let (h, s, v) = rgb_to_hsb(r, g, bl);
            [h.round() as i16, q(s), q(v), 0]
        }
        color_space::HSL => {
            let (h, s, l) = rgb_to_hsl(r, g, bl);
            [h.round() as i16, q(s), q(l), 0]
        }
        color_space::CMYK => {
            let k = 1.0 - r.max(g).max(bl);
            let f = |v: f32| {
                if k >= 1.0 {
                    0.0
                } else {
                    (1.0 - v - k) / (1.0 - k)
                }
            };
            // Back to Adobe's inverted storage.
            let store = |ink: f32| q(1.0 - ink.clamp(0.0, 1.0));
            [store(f(r)), store(f(g)), store(f(bl)), store(k)]
        }
        color_space::XYZ => {
            let (x, y, z) = rgb_to_xyz(r, g, bl);
            [q(x), q(y), q(z), 0]
        }
        color_space::LAB => {
            let (x, y, z) = rgb_to_xyz_linear(r, g, bl);
            let (l, a, bb) = xyz_to_lab(x, y, z);
            [
                q(l / 100.0),
                (a.round() + 128.0).clamp(0.0, 255.0) as i16,
                (bb.round() + 128.0).clamp(0.0, 255.0) as i16,
                0,
            ]
        }
        _ => return None,
    })
}

fn rgb_to_hsb(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let d = max - min;
    let h = hue(r, g, b, max, d);
    let s = if max <= 0.0 { 0.0 } else { d / max };
    (h, s, max)
}

fn rgb_to_hsl(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let d = max - min;
    let l = (max + min) / 2.0;
    let s = if d == 0.0 {
        0.0
    } else {
        d / (1.0 - (2.0 * l - 1.0).abs())
    };
    (hue(r, g, b, max, d), s.clamp(0.0, 1.0), l)
}

fn hue(r: f32, g: f32, b: f32, max: f32, d: f32) -> f32 {
    if d == 0.0 {
        return 0.0;
    }
    let h = if max == r {
        ((g - b) / d).rem_euclid(6.0)
    } else if max == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    };
    (h * 60.0).rem_euclid(360.0)
}

fn hsb_to_rgb(h: f32, s: f32, v: f32) -> Rgb {
    let c = v * s;
    let (r, g, b) = hue_ramp(h, c);
    let m = v - c;
    (r + m, g + m, b + m)
}

fn hsl_to_rgb(h: f32, s: f32, l: f32) -> Rgb {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let (r, g, b) = hue_ramp(h, c);
    let m = l - c / 2.0;
    (r + m, g + m, b + m)
}

fn hue_ramp(h: f32, c: f32) -> Rgb {
    let h = h.rem_euclid(360.0) / 60.0;
    let x = c * (1.0 - (h.rem_euclid(2.0) - 1.0).abs());
    match h as i32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    }
}

fn srgb_to_linear(v: f32) -> f32 {
    if v <= 0.04045 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

fn linear_to_srgb(v: f32) -> f32 {
    if v <= 0.003_130_8 {
        v * 12.92
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    }
}

/// Linear-light XYZ, D65, normalised so Y of white is 1.
fn rgb_to_xyz_linear(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let (r, g, b) = (srgb_to_linear(r), srgb_to_linear(g), srgb_to_linear(b));
    (
        0.412_456 * r + 0.357_576 * g + 0.180_437 * b,
        0.212_673 * r + 0.715_152 * g + 0.072_175 * b,
        0.019_334 * r + 0.119_192 * g + 0.950_304 * b,
    )
}

fn xyz_to_rgb_linear(x: f32, y: f32, z: f32) -> Rgb {
    let r = 3.240_454 * x - 1.537_139 * y - 0.498_531 * z;
    let g = -0.969_266 * x + 1.876_011 * y + 0.041_556 * z;
    let b = 0.055_643 * x - 0.204_026 * y + 1.057_225 * z;
    (
        linear_to_srgb(r).clamp(0.0, 1.0),
        linear_to_srgb(g).clamp(0.0, 1.0),
        linear_to_srgb(b).clamp(0.0, 1.0),
    )
}

/// Table A-4 gives XYZ as 0..255 per component, so the linear values are
/// simply scaled into that range and back.
fn rgb_to_xyz(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    rgb_to_xyz_linear(r, g, b)
}

fn xyz_to_rgb(x: f32, y: f32, z: f32) -> Rgb {
    xyz_to_rgb_linear(x, y, z)
}

const WHITE: (f32, f32, f32) = (0.950_47, 1.0, 1.088_83);

fn xyz_to_lab(x: f32, y: f32, z: f32) -> (f32, f32, f32) {
    let f = |t: f32| {
        if t > 0.008_856 {
            t.cbrt()
        } else {
            7.787 * t + 16.0 / 116.0
        }
    };
    let (fx, fy, fz) = (f(x / WHITE.0), f(y / WHITE.1), f(z / WHITE.2));
    (116.0 * fy - 16.0, 500.0 * (fx - fy), 200.0 * (fy - fz))
}

fn lab_to_xyz(l: f32, a: f32, b: f32) -> (f32, f32, f32) {
    let fy = (l + 16.0) / 116.0;
    let fx = fy + a / 500.0;
    let fz = fy - b / 200.0;
    let g = |t: f32| {
        if t.powi(3) > 0.008_856 {
            t.powi(3)
        } else {
            (t - 16.0 / 116.0) / 7.787
        }
    };
    (g(fx) * WHITE.0, g(fy) * WHITE.1, g(fz) * WHITE.2)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn near(a: Components, b: Components, tol: i16) {
        for i in 0..3 {
            assert!(
                (a[i] - b[i]).abs() <= tol,
                "component {i}: {a:?} vs {b:?} (tolerance {tol})"
            );
        }
    }

    #[test]
    fn rgb_survives_a_round_trip_through_every_space() {
        // Eight-bit components make these lossy, so the tolerances are
        // for quantisation, not for sloppiness. XYZ gets a wide one on
        // purpose: table A-4 stores it in bytes, and a byte of *linear*
        // light throws away most of the shadows — RGB 10 comes back as
        // 25 not because the maths is wrong but because 8-bit linear
        // cannot represent it. Photoshop's own XYZ has the same floor.
        for (space, tol) in [
            (color_space::HSB, 3),
            (color_space::HSL, 3),
            (color_space::CMYK, 3),
            (color_space::LAB, 4),
            (color_space::XYZ, 16),
        ] {
            for rgb in [
                [10, 200, 90, 0],
                [255, 0, 0, 0],
                [0, 0, 0, 0],
                [128, 128, 128, 0],
            ] {
                let there = convert(color_space::RGB, space, rgb).unwrap();
                let back = convert(space, color_space::RGB, there).unwrap();
                near(rgb, back, tol);
            }
        }
    }

    #[test]
    fn the_spaces_that_are_meant_to_be_exact_are_exact() {
        // HSB, HSL and CMYK are algebraic rearrangements of RGB, so
        // mid-tones should survive essentially untouched. Anything worse
        // than a rounding step here is a real error, not quantisation.
        for space in [color_space::HSB, color_space::HSL, color_space::CMYK] {
            for rgb in [[40, 160, 220, 0], [200, 100, 50, 0], [128, 128, 128, 0]] {
                let there = convert(color_space::RGB, space, rgb).unwrap();
                let back = convert(space, color_space::RGB, there).unwrap();
                near(rgb, back, 1);
            }
        }
    }

    #[test]
    fn cmyk_is_stored_inverted() {
        // Adobe: "cyan from 0...255 representing 100%...0%". Pure cyan
        // ink is therefore 0, not 255 — getting this backwards makes
        // every CMYK colour its own opposite.
        let cyan = convert(color_space::RGB, color_space::CMYK, [0, 255, 255, 0]).unwrap();
        assert_eq!(cyan[0], 0, "full cyan ink should store as 0");
        assert_eq!(cyan[3], 255, "no black ink should store as 255");

        let white = convert(color_space::RGB, color_space::CMYK, [255, 255, 255, 0]).unwrap();
        assert_eq!(white, [255, 255, 255, 255], "white is no ink at all");

        let black = convert(color_space::RGB, color_space::CMYK, [0, 0, 0, 0]).unwrap();
        assert_eq!(black[3], 0, "black is full black ink");
    }

    #[test]
    fn hue_is_in_degrees_and_the_rest_in_bytes() {
        // Table A-4: hue 0...359, saturation and brightness 0...255.
        let red = convert(color_space::RGB, color_space::HSB, [255, 0, 0, 0]).unwrap();
        assert_eq!(red, [0, 255, 255, 0]);
        let green = convert(color_space::RGB, color_space::HSB, [0, 255, 0, 0]).unwrap();
        assert_eq!(green[0], 120);
        let blue = convert(color_space::RGB, color_space::HSB, [0, 0, 255, 0]).unwrap();
        assert_eq!(blue[0], 240);
    }

    #[test]
    fn grey_is_luma_not_an_average() {
        let g = convert(color_space::RGB, color_space::GRAY, [0, 255, 0, 0]).unwrap();
        assert_eq!(g[0], 150, "Rec.601 puts green at 0.587");
    }

    #[test]
    fn lab_lightness_spans_its_documented_range() {
        // L is 0...255 standing for 0...100.
        let white = convert(color_space::RGB, color_space::LAB, [255, 255, 255, 0]).unwrap();
        assert_eq!(white[0], 255);
        assert!((white[1] - 128).abs() <= 1 && (white[2] - 128).abs() <= 1);
        let black = convert(color_space::RGB, color_space::LAB, [0, 0, 0, 0]).unwrap();
        assert_eq!(black[0], 0);
    }

    #[test]
    fn an_unknown_space_is_refused() {
        assert!(convert(99, color_space::RGB, [0; 4]).is_none());
        assert!(convert(color_space::RGB, 99, [0; 4]).is_none());
    }
}
