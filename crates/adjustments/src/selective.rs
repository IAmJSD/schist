//! Selective Colour: the six ranges and the per-range CMYK tweaks.

use super::*;

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
pub(crate) fn selective_color(px: Rgba, ranges: &[[f32; 4]; 6], relative: bool) -> Rgba {
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
