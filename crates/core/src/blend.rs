//! Blend mode identifiers (the math lives in `photoslop-pixel-ops`).
//!
//! The set and the PSD 4-byte keys come from the PSD specification's blend
//! mode keys ('norm', 'mul ', …). `PassThrough` is only valid on groups.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BlendMode {
    PassThrough,
    Normal,
    Dissolve,
    Darken,
    Multiply,
    ColorBurn,
    LinearBurn,
    DarkerColor,
    Lighten,
    Screen,
    ColorDodge,
    LinearDodge,
    LighterColor,
    Overlay,
    SoftLight,
    HardLight,
    VividLight,
    LinearLight,
    PinLight,
    HardMix,
    Difference,
    Exclusion,
    Subtract,
    Divide,
    Hue,
    Saturation,
    Color,
    Luminosity,
}

impl BlendMode {
    /// PSD blend mode signature (the 4 bytes following '8BIM' in a layer
    /// record).
    pub fn psd_key(self) -> &'static [u8; 4] {
        use BlendMode::*;
        match self {
            PassThrough => b"pass",
            Normal => b"norm",
            Dissolve => b"diss",
            Darken => b"dark",
            Multiply => b"mul ",
            ColorBurn => b"idiv",
            LinearBurn => b"lbrn",
            DarkerColor => b"dkCl",
            Lighten => b"lite",
            Screen => b"scrn",
            ColorDodge => b"div ",
            LinearDodge => b"lddg",
            LighterColor => b"lgCl",
            Overlay => b"over",
            SoftLight => b"sLit",
            HardLight => b"hLit",
            VividLight => b"vLit",
            LinearLight => b"lLit",
            PinLight => b"pLit",
            HardMix => b"hMix",
            Difference => b"diff",
            Exclusion => b"smud",
            Subtract => b"fsub",
            Divide => b"fdiv",
            Hue => b"hue ",
            Saturation => b"sat ",
            Color => b"colr",
            Luminosity => b"lum ",
        }
    }

    pub fn from_psd_key(key: &[u8; 4]) -> Option<BlendMode> {
        use BlendMode::*;
        Some(match key {
            b"pass" => PassThrough,
            b"norm" => Normal,
            b"diss" => Dissolve,
            b"dark" => Darken,
            b"mul " => Multiply,
            b"idiv" => ColorBurn,
            b"lbrn" => LinearBurn,
            b"dkCl" => DarkerColor,
            b"lite" => Lighten,
            b"scrn" => Screen,
            b"div " => ColorDodge,
            b"lddg" => LinearDodge,
            b"lgCl" => LighterColor,
            b"over" => Overlay,
            b"sLit" => SoftLight,
            b"hLit" => HardLight,
            b"vLit" => VividLight,
            b"lLit" => LinearLight,
            b"pLit" => PinLight,
            b"hMix" => HardMix,
            b"diff" => Difference,
            b"smud" => Exclusion,
            b"fsub" => Subtract,
            b"fdiv" => Divide,
            b"hue " => Hue,
            b"sat " => Saturation,
            b"colr" => Color,
            b"lum " => Luminosity,
            _ => return None,
        })
    }

    pub fn display_name(self) -> &'static str {
        use BlendMode::*;
        match self {
            PassThrough => "Pass Through",
            Normal => "Normal",
            Dissolve => "Dissolve",
            Darken => "Darken",
            Multiply => "Multiply",
            ColorBurn => "Color Burn",
            LinearBurn => "Linear Burn",
            DarkerColor => "Darker Color",
            Lighten => "Lighten",
            Screen => "Screen",
            ColorDodge => "Color Dodge",
            LinearDodge => "Linear Dodge (Add)",
            LighterColor => "Lighter Color",
            Overlay => "Overlay",
            SoftLight => "Soft Light",
            HardLight => "Hard Light",
            VividLight => "Vivid Light",
            LinearLight => "Linear Light",
            PinLight => "Pin Light",
            HardMix => "Hard Mix",
            Difference => "Difference",
            Exclusion => "Exclusion",
            Subtract => "Subtract",
            Divide => "Divide",
            Hue => "Hue",
            Saturation => "Saturation",
            Color => "Color",
            Luminosity => "Luminosity",
        }
    }

    /// All modes selectable on a normal (non-group) layer, in Photoshop's
    /// menu order.
    pub fn layer_modes() -> &'static [BlendMode] {
        use BlendMode::*;
        &[
            Normal,
            Dissolve,
            Darken,
            Multiply,
            ColorBurn,
            LinearBurn,
            DarkerColor,
            Lighten,
            Screen,
            ColorDodge,
            LinearDodge,
            LighterColor,
            Overlay,
            SoftLight,
            HardLight,
            VividLight,
            LinearLight,
            PinLight,
            HardMix,
            Difference,
            Exclusion,
            Subtract,
            Divide,
            Hue,
            Saturation,
            Color,
            Luminosity,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn psd_key_round_trip() {
        for &mode in BlendMode::layer_modes() {
            assert_eq!(BlendMode::from_psd_key(mode.psd_key()), Some(mode));
        }
        assert_eq!(
            BlendMode::from_psd_key(BlendMode::PassThrough.psd_key()),
            Some(BlendMode::PassThrough)
        );
        assert_eq!(BlendMode::from_psd_key(b"????"), None);
    }
}
