//! Stable keys and display labels for the multi-part adjustments.

use super::*;

/// Slider keys for Color Balance, e.g. "sh_cr" for shadows cyan/red.
/// Interned so `ParamSpec` can stay `&'static str`.
pub(crate) fn balance_key(band: &str, channel: &str) -> &'static str {
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

pub(crate) fn balance_label(band: &str, channel: &str) -> &'static str {
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

pub(crate) fn selective_key(range: usize, channel: usize) -> &'static str {
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

pub(crate) fn selective_label(range: SelectiveRange, channel: &str) -> &'static str {
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
