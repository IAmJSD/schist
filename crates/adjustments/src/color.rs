//! Shared colour math: HSL round trips, luminance, and chroma.

use super::*;

/// Affinity's per-hue-range luminosity shift: a hue-preserving pull of
/// every channel toward the brightest one (`amount` > 0, lightening) or
/// toward the darkest one (`amount` < 0), by `|amount|` of the way.
/// Both extremes stay put, so the hue and the HSV value or saturation —
/// whichever the move doesn't touch — survive exactly. Measured against
/// hsl_range_green_lum.af and hsl_range_green_lumneg.af, which pin both
/// directions to the last unit in 8 bits.
pub(crate) fn pull_to_extreme(r: f32, g: f32, b: f32, amount: f32) -> (f32, f32, f32) {
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

pub(crate) fn adjust_lightness(l: f32, amount: f32) -> f32 {
    if amount >= 0.0 {
        l + (1.0 - l) * (amount / 100.0)
    } else {
        l * (1.0 + amount / 100.0)
    }
    .clamp(0.0, 1.0)
}

pub(crate) fn rgb_to_hsl(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
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

pub(crate) fn hsl_to_rgb(h: f32, s: f32, l: f32) -> (f32, f32, f32) {
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

pub(crate) fn luma(px: Rgba) -> f32 {
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
pub(crate) fn set_lum(px: Rgba, target: f32) -> Rgba {
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
pub(crate) fn scale_chroma_by(px: Rgba, gain: impl Fn(f32, f32) -> f32) -> Rgba {
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
pub(crate) fn vibrance_gain(t: f32, chroma: f32, hue: f32) -> f32 {
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
pub(crate) const VIBRANCE_BOOST: [f32; 20] = [
    0.0, 0.0424, 0.1367, 0.2183, 0.2804, 0.3330, 0.3748, 0.4061, 0.4294, 0.4468, 0.4587, 0.4657,
    0.4684, 0.4676, 0.4639, 0.4559, 0.4410, 0.4163, 0.3934, 0.3670,
];

pub(crate) fn vibrance_boost(chroma: f32) -> f32 {
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
