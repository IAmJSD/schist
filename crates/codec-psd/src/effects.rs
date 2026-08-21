//! Layer effects (`lfx2`), read and written as PSD descriptors.
//!
//! Before this the block was preserved byte-for-byte but never
//! interpreted, so a file's drop shadows were invisible in Schist and
//! editing a layer's effects meant throwing the block away on save. Both
//! directions now go through `schist-psd-descriptor`.
//!
//! Photoshop's key names are terse and mostly undocumented; the mapping
//! below is the one the format actually uses, with the meaning spelled out
//! in comments rather than guessed at from the name.

use schist_color::Rgba;
use schist_core::{
    BevelStyle, BevelStyle_, BlendMode, ColorOverlayStyle, Effect, GlowStyle, GradientOverlayStyle,
    GradientShape, LayerStyle, SatinStyle, ShadowStyle, StrokePosition, StrokeStyle, Technique,
};
use schist_psd_descriptor::{parse, Builder, Descriptor, Value};

/// `lfx2` prefixes its descriptor with two u32s: an object-effects version
/// (0) and the descriptor version (16). That is not the u16+u32 shape the
/// adjustment blocks use, so it gets its own handling rather than
/// `parse_versioned`.
const LFX2_PREFIX: usize = 8;

/// PSD blend-mode keys, as they appear inside an effects descriptor.
fn blend_to_key(mode: BlendMode) -> &'static str {
    // `BlendMode::psd_key` already knows these; effects use the same set.
    match std::str::from_utf8(mode.psd_key()) {
        Ok(k) => match k {
            "norm" => "Nrml",
            "diss" => "Dslv",
            "dark" => "Drkn",
            "mul " => "Mltp",
            "idiv" => "CBrn",
            "lbrn" => "linearBurn",
            "dkCl" => "darkerColor",
            "lite" => "Lghn",
            "scrn" => "Scrn",
            "div " => "CDdg",
            "lddg" => "linearDodge",
            "lgCl" => "lighterColor",
            "over" => "Ovrl",
            "sLit" => "SftL",
            "hLit" => "HrdL",
            "vLit" => "vividLight",
            "lLit" => "linearLight",
            "pLit" => "pinLight",
            "hMix" => "hardMix",
            "diff" => "Dfrn",
            "smud" => "Xclu",
            "fsub" => "blendSubtraction",
            "fdiv" => "blendDivide",
            "hue " => "H   ",
            "sat " => "Strt",
            "colr" => "Clr ",
            "lum " => "Lmns",
            _ => "Nrml",
        },
        Err(_) => "Nrml",
    }
}

fn key_to_blend(key: &str) -> BlendMode {
    let psd = match key {
        "Nrml" => b"norm",
        "Dslv" => b"diss",
        "Drkn" => b"dark",
        "Mltp" => b"mul ",
        "CBrn" => b"idiv",
        "linearBurn" => b"lbrn",
        "darkerColor" => b"dkCl",
        "Lghn" => b"lite",
        "Scrn" => b"scrn",
        "CDdg" => b"div ",
        "linearDodge" => b"lddg",
        "lighterColor" => b"lgCl",
        "Ovrl" => b"over",
        "SftL" => b"sLit",
        "HrdL" => b"hLit",
        "vividLight" => b"vLit",
        "linearLight" => b"lLit",
        "pinLight" => b"pLit",
        "hardMix" => b"hMix",
        "Dfrn" => b"diff",
        "Xclu" => b"smud",
        "blendSubtraction" => b"fsub",
        "blendDivide" => b"fdiv",
        "H   " => b"hue ",
        "Strt" => b"sat ",
        "Clr " => b"colr",
        "Lmns" => b"lum ",
        _ => b"norm",
    };
    BlendMode::from_psd_key(psd).unwrap_or(BlendMode::Normal)
}

/// Colours in effects descriptors are 0..=255 doubles.
fn read_color(d: &Descriptor, key: &str) -> Option<Rgba> {
    let c = d.get(key)?.as_object()?;
    Some(Rgba::new(
        (c.number("Rd  ")? / 255.0) as f32,
        (c.number("Grn ")? / 255.0) as f32,
        (c.number("Bl  ")? / 255.0) as f32,
        1.0,
    ))
}

fn write_color(b: &mut Builder, key: &str, c: Rgba) {
    b.color(
        key,
        (c.r.clamp(0.0, 1.0) * 255.0) as f64,
        (c.g.clamp(0.0, 1.0) * 255.0) as f64,
        (c.b.clamp(0.0, 1.0) * 255.0) as f64,
    );
}

fn enabled(d: &Descriptor) -> bool {
    d.get("enab").and_then(|v| v.as_bool()).unwrap_or(false)
}

fn blend_of(d: &Descriptor, key: &str) -> BlendMode {
    match d.get(key) {
        Some(Value::Enum(_, v)) => key_to_blend(v),
        _ => BlendMode::Normal,
    }
}

/// Percentages are stored 0..=100; the model keeps them 0..=1.
fn pct(d: &Descriptor, key: &str, default: f32) -> f32 {
    d.number(key).map(|v| v as f32 / 100.0).unwrap_or(default)
}

fn num(d: &Descriptor, key: &str, default: f32) -> f32 {
    d.number(key).map(|v| v as f32).unwrap_or(default)
}

/// Decode an `lfx2` payload into a layer style.
pub fn read_lfx2(raw: &[u8]) -> Option<LayerStyle> {
    let d = parse(raw.get(LFX2_PREFIX..)?)?;
    let mut style = LayerStyle::default();

    if let Some(s) = d.get("DrSh").and_then(|v| v.as_object()) {
        style.drop_shadow = Effect {
            enabled: enabled(s),
            settings: read_shadow(s, true),
        };
    }
    if let Some(s) = d.get("IrSh").and_then(|v| v.as_object()) {
        style.inner_shadow = Effect {
            enabled: enabled(s),
            settings: read_shadow(s, false),
        };
    }
    if let Some(g) = d.get("OrGl").and_then(|v| v.as_object()) {
        style.outer_glow = Effect {
            enabled: enabled(g),
            settings: read_glow(g, false),
        };
    }
    if let Some(g) = d.get("IrGl").and_then(|v| v.as_object()) {
        style.inner_glow = Effect {
            enabled: enabled(g),
            settings: read_glow(g, true),
        };
    }
    if let Some(b) = d.get("ebbl").and_then(|v| v.as_object()) {
        style.bevel = Effect {
            enabled: enabled(b),
            settings: read_bevel(b),
        };
    }
    if let Some(s) = d.get("ChFX").and_then(|v| v.as_object()) {
        style.satin = Effect {
            enabled: enabled(s),
            settings: SatinStyle {
                color: read_color(s, "Clr ").unwrap_or(Rgba::new(0.0, 0.0, 0.0, 1.0)),
                blend: blend_of(s, "Md  "),
                opacity: pct(s, "Opct", 0.5),
                angle: num(s, "lagl", 19.0),
                distance: num(s, "Dstn", 11.0),
                size: num(s, "blur", 14.0),
                invert: s.get("InvT").and_then(|v| v.as_bool()).unwrap_or(true),
            },
        };
    }
    if let Some(o) = d.get("SoFi").and_then(|v| v.as_object()) {
        style.color_overlay = Effect {
            enabled: enabled(o),
            settings: ColorOverlayStyle {
                color: read_color(o, "Clr ").unwrap_or(Rgba::new(1.0, 0.0, 0.0, 1.0)),
                blend: blend_of(o, "Md  "),
                opacity: pct(o, "Opct", 1.0),
            },
        };
    }
    if let Some(o) = d.get("GrFl").and_then(|v| v.as_object()) {
        style.gradient_overlay = Effect {
            enabled: enabled(o),
            settings: read_gradient(o),
        };
    }
    if let Some(s) = d.get("FrFX").and_then(|v| v.as_object()) {
        style.stroke = Effect {
            enabled: enabled(s),
            settings: StrokeStyle {
                color: read_color(s, "Clr ").unwrap_or(Rgba::new(0.0, 0.0, 0.0, 1.0)),
                blend: blend_of(s, "Md  "),
                opacity: pct(s, "Opct", 1.0),
                size: num(s, "Sz  ", 3.0),
                position: match s.get("Styl") {
                    Some(Value::Enum(_, v)) => match v.as_str() {
                        "InsF" => StrokePosition::Inside,
                        "CtrF" => StrokePosition::Center,
                        _ => StrokePosition::Outside,
                    },
                    _ => StrokePosition::Outside,
                },
            },
        };
    }
    Some(style)
}

fn read_shadow(d: &Descriptor, drop: bool) -> ShadowStyle {
    ShadowStyle {
        color: read_color(d, "Clr ").unwrap_or(Rgba::new(0.0, 0.0, 0.0, 1.0)),
        blend: blend_of(d, "Md  "),
        opacity: pct(d, "Opct", 0.75),
        angle: num(d, "lagl", 120.0),
        distance: num(d, "Dstn", 5.0),
        // "Ckmt" is Choke/Spread, stored as a percentage.
        spread: pct(d, "Ckmt", 0.0),
        size: num(d, "blur", 5.0),
        knockout: if drop {
            d.get("layerConceals")
                .and_then(|v| v.as_bool())
                .unwrap_or(true)
        } else {
            true
        },
    }
}

fn read_glow(d: &Descriptor, inner: bool) -> GlowStyle {
    GlowStyle {
        color: read_color(d, "Clr ").unwrap_or(Rgba::new(1.0, 1.0, 0.75, 1.0)),
        blend: blend_of(d, "Md  "),
        opacity: pct(d, "Opct", 0.75),
        spread: pct(d, "Ckmt", 0.0),
        size: num(d, "blur", 5.0),
        technique: match d.get("GlwT") {
            Some(Value::Enum(_, v)) if v == "PrBL" => Technique::Precise,
            _ => Technique::Softer,
        },
        from_edge: if inner {
            !matches!(d.get("glwS"), Some(Value::Enum(_, v)) if v == "SrcC")
        } else {
            true
        },
    }
}

fn read_bevel(d: &Descriptor) -> BevelStyle {
    BevelStyle {
        style: match d.get("bvlS") {
            Some(Value::Enum(_, v)) => match v.as_str() {
                "OtrB" => BevelStyle_::OuterBevel,
                "Embs" => BevelStyle_::Emboss,
                "PlEb" => BevelStyle_::PillowEmboss,
                _ => BevelStyle_::InnerBevel,
            },
            _ => BevelStyle_::InnerBevel,
        },
        angle: num(d, "lagl", 120.0),
        altitude: num(d, "Lald", 30.0),
        size: num(d, "blur", 5.0),
        soften: num(d, "Sftn", 0.0),
        // "srgR" is Depth, a percentage where 100 means 1x.
        depth: pct(d, "srgR", 1.0),
        highlight: read_color(d, "hglC").unwrap_or(Rgba::new(1.0, 1.0, 1.0, 1.0)),
        highlight_blend: blend_of(d, "hglM"),
        highlight_opacity: pct(d, "hglO", 0.75),
        shadow: read_color(d, "sdwC").unwrap_or(Rgba::new(0.0, 0.0, 0.0, 1.0)),
        shadow_blend: blend_of(d, "sdwM"),
        shadow_opacity: pct(d, "sdwO", 0.75),
    }
}

fn read_gradient(d: &Descriptor) -> GradientOverlayStyle {
    // Photoshop stores the ramp as a list of colour stops in a "Grad"
    // object; we model two ends, so take the first and last.
    let mut from = Rgba::new(0.0, 0.0, 0.0, 1.0);
    let mut to = Rgba::new(1.0, 1.0, 1.0, 1.0);
    if let Some(stops) = d
        .get("Grad")
        .and_then(|v| v.as_object())
        .and_then(|g| g.get("Clrs"))
        .and_then(|v| v.as_list())
    {
        if let Some(first) = stops.first().and_then(|v| v.as_object()) {
            if let Some(c) = read_color(first, "Clr ") {
                from = c;
            }
        }
        if let Some(last) = stops.last().and_then(|v| v.as_object()) {
            if let Some(c) = read_color(last, "Clr ") {
                to = c;
            }
        }
    }
    GradientOverlayStyle {
        from,
        to,
        blend: blend_of(d, "Md  "),
        opacity: pct(d, "Opct", 1.0),
        angle: num(d, "Angl", 90.0),
        shape: match d.get("Type") {
            Some(Value::Enum(_, v)) if v == "Rdl " => GradientShape::Radial,
            _ => GradientShape::Linear,
        },
        reverse: d.get("Rvrs").and_then(|v| v.as_bool()).unwrap_or(false),
        scale: d.number("Scl ").map(|v| v as f32 / 100.0).unwrap_or(1.0),
    }
}

/// Encode a layer style as an `lfx2` payload, or `None` when nothing is
/// switched on and the block should simply be omitted.
pub fn write_lfx2(style: &LayerStyle) -> Option<Vec<u8>> {
    if style.is_empty() {
        return None;
    }
    let mut root = Builder::new("null");
    root.percent("Scl ", 100.0).bool("masterFXSwitch", true);

    if style.drop_shadow.enabled {
        root.object("DrSh", shadow_builder(&style.drop_shadow.settings, true));
    }
    if style.inner_shadow.enabled {
        root.object("IrSh", shadow_builder(&style.inner_shadow.settings, false));
    }
    if style.outer_glow.enabled {
        root.object("OrGl", glow_builder(&style.outer_glow.settings, false));
    }
    if style.inner_glow.enabled {
        root.object("IrGl", glow_builder(&style.inner_glow.settings, true));
    }
    if style.bevel.enabled {
        root.object("ebbl", bevel_builder(&style.bevel.settings));
    }
    if style.satin.enabled {
        let s = &style.satin.settings;
        let mut b = Builder::new("ChFX");
        b.bool("enab", true);
        b.enumerated("Md  ", "BlnM", blend_to_key(s.blend));
        write_color(&mut b, "Clr ", s.color);
        b.percent("Opct", (s.opacity * 100.0) as f64)
            .angle("lagl", s.angle as f64)
            .pixels("Dstn", s.distance as f64)
            .pixels("blur", s.size as f64)
            .bool("InvT", s.invert)
            .bool("AntA", true);
        root.object("ChFX", b);
    }
    if style.color_overlay.enabled {
        let o = &style.color_overlay.settings;
        let mut b = Builder::new("SoFi");
        b.bool("enab", true);
        b.enumerated("Md  ", "BlnM", blend_to_key(o.blend));
        write_color(&mut b, "Clr ", o.color);
        b.percent("Opct", (o.opacity * 100.0) as f64);
        root.object("SoFi", b);
    }
    if style.gradient_overlay.enabled {
        root.object("GrFl", gradient_builder(&style.gradient_overlay.settings));
    }
    if style.stroke.enabled {
        let s = &style.stroke.settings;
        let mut b = Builder::new("FrFX");
        b.bool("enab", true);
        b.enumerated(
            "Styl",
            "FStl",
            match s.position {
                StrokePosition::Outside => "OutF",
                StrokePosition::Inside => "InsF",
                StrokePosition::Center => "CtrF",
            },
        );
        // Solid colour, as opposed to a gradient or pattern stroke.
        b.enumerated("PntT", "FrFl", "SClr");
        b.enumerated("Md  ", "BlnM", blend_to_key(s.blend));
        b.percent("Opct", (s.opacity * 100.0) as f64)
            .pixels("Sz  ", s.size as f64);
        write_color(&mut b, "Clr ", s.color);
        root.object("FrFX", b);
    }
    let mut out = Vec::new();
    // Object effects version, then descriptor version.
    out.extend_from_slice(&0u32.to_be_bytes());
    out.extend_from_slice(&16u32.to_be_bytes());
    out.extend_from_slice(&root.finish());
    Some(out)
}

fn shadow_builder(s: &ShadowStyle, drop: bool) -> Builder {
    let mut b = Builder::new(if drop { "DrSh" } else { "IrSh" });
    b.bool("enab", true);
    b.enumerated("Md  ", "BlnM", blend_to_key(s.blend));
    write_color(&mut b, "Clr ", s.color);
    b.percent("Opct", (s.opacity * 100.0) as f64)
        // Global light off: each effect keeps its own angle, which is what
        // the model stores.
        .bool("uglg", false)
        .angle("lagl", s.angle as f64)
        .pixels("Dstn", s.distance as f64)
        .percent("Ckmt", (s.spread * 100.0) as f64)
        .pixels("blur", s.size as f64)
        .percent("Nose", 0.0)
        .bool("AntA", false);
    if drop {
        b.bool("layerConceals", s.knockout);
    }
    b
}

fn glow_builder(g: &GlowStyle, inner: bool) -> Builder {
    let mut b = Builder::new(if inner { "IrGl" } else { "OrGl" });
    b.bool("enab", true);
    b.enumerated("Md  ", "BlnM", blend_to_key(g.blend));
    write_color(&mut b, "Clr ", g.color);
    b.percent("Opct", (g.opacity * 100.0) as f64)
        .percent("Nose", 0.0)
        .percent("Ckmt", (g.spread * 100.0) as f64)
        .pixels("blur", g.size as f64);
    b.enumerated(
        "GlwT",
        "BETE",
        match g.technique {
            Technique::Softer => "SfBL",
            Technique::Precise => "PrBL",
        },
    );
    if inner {
        b.enumerated("glwS", "IGSr", if g.from_edge { "SrcE" } else { "SrcC" });
    }
    b.bool("AntA", false).percent("ShdN", 0.0);
    b
}

fn bevel_builder(v: &BevelStyle) -> Builder {
    let mut b = Builder::new("ebbl");
    b.bool("enab", true);
    b.enumerated(
        "bvlS",
        "BESl",
        match v.style {
            BevelStyle_::OuterBevel => "OtrB",
            BevelStyle_::InnerBevel => "InrB",
            BevelStyle_::Emboss => "Embs",
            BevelStyle_::PillowEmboss => "PlEb",
        },
    );
    b.enumerated("bvlT", "bvlT", "SfBL");
    b.enumerated("bvlD", "BESs", "In  ");
    b.bool("uglg", false)
        .angle("lagl", v.angle as f64)
        .angle("Lald", v.altitude as f64)
        .pixels("blur", v.size as f64)
        .pixels("Sftn", v.soften as f64)
        .percent("srgR", (v.depth * 100.0) as f64);
    b.enumerated("hglM", "BlnM", blend_to_key(v.highlight_blend));
    write_color(&mut b, "hglC", v.highlight);
    b.percent("hglO", (v.highlight_opacity * 100.0) as f64);
    b.enumerated("sdwM", "BlnM", blend_to_key(v.shadow_blend));
    write_color(&mut b, "sdwC", v.shadow);
    b.percent("sdwO", (v.shadow_opacity * 100.0) as f64);
    b
}

fn gradient_builder(o: &GradientOverlayStyle) -> Builder {
    let mut b = Builder::new("GrFl");
    b.bool("enab", true);
    b.enumerated("Md  ", "BlnM", blend_to_key(o.blend));
    b.percent("Opct", (o.opacity * 100.0) as f64);

    // The ramp: two colour stops at either end. Photoshop measures stop
    // locations in 0..=4096, not 0..=1.
    let stop = |c: Rgba, at: f64| {
        let mut s = Builder::new("Clrt");
        s.color(
            "Clr ",
            (c.r.clamp(0.0, 1.0) * 255.0) as f64,
            (c.g.clamp(0.0, 1.0) * 255.0) as f64,
            (c.b.clamp(0.0, 1.0) * 255.0) as f64,
        );
        s.enumerated("Type", "Clry", "UsrS")
            .integer("Lctn", at as i32)
            .integer("Mdpn", 50);
        s
    };
    let mut grad = Builder::new("Grdn");
    grad.text("Nm  ", "Custom")
        .enumerated("GrdF", "GrdF", "CstS")
        .double("Intr", 4096.0);
    grad.object_list("Clrs", vec![stop(o.from, 0.0), stop(o.to, 4096.0)]);
    let mut opacity_stop = Builder::new("TrnS");
    opacity_stop
        .percent("Opct", 100.0)
        .integer("Lctn", 0)
        .integer("Mdpn", 50);
    let mut opacity_end = Builder::new("TrnS");
    opacity_end
        .percent("Opct", 100.0)
        .integer("Lctn", 4096)
        .integer("Mdpn", 50);
    grad.object_list("Trns", vec![opacity_stop, opacity_end]);
    b.object("Grad", grad);

    b.angle("Angl", o.angle as f64);
    b.enumerated(
        "Type",
        "GrdT",
        match o.shape {
            GradientShape::Linear => "Lnr ",
            GradientShape::Radial => "Rdl ",
        },
    );
    b.bool("Rvrs", o.reverse)
        .bool("Algn", true)
        .percent("Scl ", (o.scale * 100.0) as f64);
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_style() -> LayerStyle {
        let mut s = LayerStyle::default();
        s.drop_shadow.enabled = true;
        s.drop_shadow.settings = ShadowStyle {
            color: Rgba::new(0.2, 0.4, 0.6, 1.0),
            blend: BlendMode::Multiply,
            opacity: 0.6,
            angle: 45.0,
            distance: 12.0,
            spread: 0.25,
            size: 8.0,
            knockout: false,
        };
        s.stroke.enabled = true;
        s.stroke.settings = StrokeStyle {
            color: Rgba::new(1.0, 0.0, 0.5, 1.0),
            blend: BlendMode::Overlay,
            opacity: 0.8,
            size: 6.0,
            position: StrokePosition::Inside,
        };
        s.inner_glow.enabled = true;
        s.inner_glow.settings.technique = Technique::Precise;
        s.inner_glow.settings.from_edge = false;
        s.inner_glow.settings.size = 17.0;
        s.bevel.enabled = true;
        s.bevel.settings.style = BevelStyle_::PillowEmboss;
        s.bevel.settings.altitude = 55.0;
        s.gradient_overlay.enabled = true;
        s.gradient_overlay.settings.shape = GradientShape::Radial;
        s.gradient_overlay.settings.from = Rgba::new(1.0, 0.0, 0.0, 1.0);
        s.gradient_overlay.settings.to = Rgba::new(0.0, 0.0, 1.0, 1.0);
        s.gradient_overlay.settings.reverse = true;
        s.satin.enabled = true;
        s.color_overlay.enabled = true;
        s.color_overlay.settings.color = Rgba::new(0.0, 1.0, 0.0, 1.0);
        s
    }

    fn close(a: f32, b: f32, what: &str) {
        assert!((a - b).abs() < 0.01, "{what}: {a} != {b}");
    }

    #[test]
    fn effects_survive_a_round_trip() {
        let before = sample_style();
        let bytes = write_lfx2(&before).expect("something is enabled");
        let after = read_lfx2(&bytes).expect("reads back");

        assert!(after.drop_shadow.enabled);
        let (a, b) = (after.drop_shadow.settings, before.drop_shadow.settings);
        assert_eq!(a.blend, b.blend);
        close(a.opacity, b.opacity, "shadow opacity");
        close(a.angle, b.angle, "shadow angle");
        close(a.distance, b.distance, "shadow distance");
        close(a.spread, b.spread, "shadow spread");
        close(a.size, b.size, "shadow size");
        close(a.color.r, b.color.r, "shadow red");
        close(a.color.b, b.color.b, "shadow blue");
        assert_eq!(a.knockout, b.knockout, "knockout");

        let (a, b) = (after.stroke.settings, before.stroke.settings);
        assert_eq!(a.position, b.position, "stroke position");
        assert_eq!(a.blend, b.blend, "stroke blend");
        close(a.size, b.size, "stroke size");
        close(a.color.g, b.color.g, "stroke green");

        let (a, b) = (after.inner_glow.settings, before.inner_glow.settings);
        assert_eq!(a.technique, b.technique, "glow technique");
        assert_eq!(a.from_edge, b.from_edge, "glow source");
        close(a.size, b.size, "glow size");

        let (a, b) = (after.bevel.settings, before.bevel.settings);
        assert_eq!(a.style, b.style, "bevel style");
        close(a.altitude, b.altitude, "bevel altitude");
        close(a.depth, b.depth, "bevel depth");

        let (a, b) = (
            after.gradient_overlay.settings,
            before.gradient_overlay.settings,
        );
        assert_eq!(a.shape, b.shape, "gradient shape");
        assert_eq!(a.reverse, b.reverse, "gradient reverse");
        close(a.from.r, b.from.r, "gradient from");
        close(a.to.b, b.to.b, "gradient to");

        close(
            after.color_overlay.settings.color.g,
            before.color_overlay.settings.color.g,
            "overlay green",
        );
        assert!(after.satin.enabled, "satin lost");
    }

    #[test]
    fn an_empty_style_writes_no_block() {
        assert!(write_lfx2(&LayerStyle::default()).is_none());
    }

    #[test]
    fn a_disabled_effect_is_not_written() {
        let mut s = LayerStyle::default();
        s.drop_shadow.enabled = true;
        // Configured but switched off: the block should carry the shadow
        // and not the glow.
        s.outer_glow.enabled = false;
        s.outer_glow.settings.size = 40.0;
        let after = read_lfx2(&write_lfx2(&s).unwrap()).unwrap();
        assert!(after.drop_shadow.enabled);
        assert!(!after.outer_glow.enabled);
    }

    #[test]
    fn every_blend_mode_survives_the_key_mapping() {
        for mode in BlendMode::layer_modes() {
            let round = key_to_blend(blend_to_key(*mode));
            assert_eq!(round, *mode, "{mode:?} did not survive");
        }
    }

    #[test]
    fn garbage_is_none_not_a_panic() {
        assert!(read_lfx2(&[]).is_none());
        assert!(read_lfx2(&[0, 1, 2, 3, 4, 5, 6, 7]).is_none());
        // A valid prefix followed by nonsense.
        let mut bytes = write_lfx2(&sample_style()).unwrap();
        bytes.truncate(bytes.len() / 2);
        let _ = read_lfx2(&bytes);
    }
}
