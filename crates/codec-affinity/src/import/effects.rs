//! Layer effects (`FiEf`) mapped onto our layer style.

use super::*;

impl Walker<'_> {
    /// Map a layer's `FiEf` effects onto our layer style.
    ///
    /// Each entry is a `FilE`-derived class sharing `Enab`, `BlnM` (the
    /// layer blend table), `Opac` (0..1), `SclO` (scale with object)
    /// and usually `Radi` (blur/width) and a `Colr`:
    /// `Shad`/`InnS` shadows add `Offs` (distance) and `Angl` — the
    /// *offset direction* in radians, y-down, so 45° points down-right —
    /// plus `Knck`; `OutG`/`InnG` glows; `ColO` colour overlay; `GrdO`
    /// gradient overlay (`GrFl`, a fill descriptor); `Strk` outline
    /// stroke (`Radi` width, `Alig` position); `BevE` bevel
    /// (`Azim`/`Elev` light direction in radians, `Dept`, `Sftn`);
    /// `Gaus` gaussian blur (`Radi` radius, `PrAl` preserve alpha); and
    /// `PhgB` (the 3D effect), which has no equivalent and is reported.
    ///
    /// Shadows and glows share `Comp`, the panel's Intensity slider
    /// stored *inverted* — 0% intensity writes 1.0 and 100% writes 0.0
    /// — which is our `spread` (probed with fx_shadow_spread.af,
    /// fx_glow_outer.af, fx_inner_shadow.af, fx_inner_glow.af).
    pub(super) fn apply_effects(&mut self, node: &Node, layer: &mut Layer) {
        use schist_core::style;
        let scale = ((self.node_ctm(node).scale_x() + self.node_ctm(node).scale_y()) * 0.5) as f32;
        for fx in self.graph.children(node, b"FiEf") {
            if !matches!(bool_of(fx, b"Enab"), Some(true)) {
                continue;
            }
            let tag = fx.type_tag().to_be_bytes();
            let opacity = f64_of(fx, b"Opac").unwrap_or(1.0) as f32;
            let blend = match fx.field(b"BlnM") {
                Some(Value::Enum { id, version }) => blend_mode(*id, *version),
                _ => None,
            };
            let color = self
                .graph
                .child(fx, b"Colr")
                .and_then(color_bytes)
                .map(|c| schist_color::Rgba::from_u8(c[0], c[1], c[2], c[3]));
            // "Scale with object" bakes the layer transform into the
            // effect's pixel measures; otherwise they are canvas-absolute.
            let s = if bool_of(fx, b"SclO") == Some(true) {
                scale
            } else {
                1.0
            };
            let radius = f64_of(fx, b"Radi").unwrap_or(0.0) as f32 * s;
            // For the effects whose `Radi` is a *blur*, it means the
            // same thing it does on `Gaus`: a standard deviation of
            // about 0.35 x Radi, probed on a hard-edged square at
            // radius 20, 40 and 80 (`ig_r*_i0.af`, sigma 8.02, 13.16
            // and 27.49). Our own radius is one where sigma =
            // radius/sqrt(3), so the conversion is sqrt(3) x that. The
            // hard-square probes put it at 0.57-0.60 and the test-card
            // ones a little higher, which says a residual remains in
            // the falloff *shape*; 0.58 is where the two sets agree
            // best. A stroke's `Radi` is a width, not a blur, and keeps
            // its pixels.
            let blur_radius = radius * crate::BLUR_RADI;
            fn on<T>(settings: T) -> style::Effect<T> {
                style::Effect {
                    enabled: true,
                    settings,
                }
            }
            /// The panel's Intensity slider, stored inverted in `Comp`.
            fn intensity(fx: &Node) -> f32 {
                (1.0 - f64_of(fx, b"Comp").unwrap_or(1.0) as f32).clamp(0.0, 1.0)
            }
            match &tag {
                b"Shad" | b"InnS" => {
                    let settings = style::ShadowStyle {
                        color: color.unwrap_or(schist_color::Rgba::new(0.0, 0.0, 0.0, 1.0)),
                        blend: blend.unwrap_or(schist_core::BlendMode::Multiply),
                        opacity,
                        // Ours is where the light comes from (the shadow
                        // falls opposite); Affinity stores the offset
                        // direction itself.
                        angle: 180.0
                            - f64_of(fx, b"Angl")
                                .unwrap_or(std::f64::consts::FRAC_PI_4)
                                .to_degrees() as f32,
                        distance: f64_of(fx, b"Offs").unwrap_or(0.0) as f32 * s,
                        spread: intensity(fx),
                        size: blur_radius,
                        knockout: bool_of(fx, b"Knck").unwrap_or(true),
                    };
                    if &tag == b"Shad" {
                        layer.style.drop_shadow = on(settings);
                    } else {
                        layer.style.inner_shadow = on(settings);
                    }
                }
                b"OutG" | b"InnG" => {
                    let settings = style::GlowStyle {
                        color: color.unwrap_or(schist_color::Rgba::new(1.0, 1.0, 0.75, 1.0)),
                        blend: blend.unwrap_or(schist_core::BlendMode::Screen),
                        opacity,
                        spread: intensity(fx),
                        size: blur_radius,
                        technique: style::Technique::Softer,
                        from_edge: true,
                    };
                    if &tag == b"OutG" {
                        layer.style.outer_glow = on(settings);
                    } else {
                        layer.style.inner_glow = on(settings);
                    }
                }
                b"ColO" => {
                    layer.style.color_overlay = on(style::ColorOverlayStyle {
                        color: color.unwrap_or(schist_color::Rgba::new(1.0, 0.0, 0.0, 1.0)),
                        blend: blend.unwrap_or(schist_core::BlendMode::Normal),
                        opacity,
                    });
                }
                b"Strk" => {
                    let color = color.or_else(|| {
                        // Gradient-filled strokes approximate to their
                        // first stop.
                        let fill = self
                            .graph
                            .child(fx, b"GrFl")
                            .and_then(|g| self.graph.child(g, b"FDeF"))?;
                        let grad = gradient_fill(self.graph, fill, None).or_else(|| {
                            let c = fill_color(self.graph, fill)?;
                            Some(GradientFill {
                                stops: vec![(0.0, c), (1.0, c)],
                                start: (0.0, 0.0),
                                end: (1.0, 0.0),
                                radial: false,
                            })
                        })?;
                        let c = grad.stops.first()?.1;
                        Some(schist_color::Rgba::from_u8(c[0], c[1], c[2], c[3]))
                    });
                    let Some(color) = color else {
                        continue;
                    };
                    layer.style.stroke = on(style::StrokeStyle {
                        color,
                        blend: blend.unwrap_or(schist_core::BlendMode::Normal),
                        opacity,
                        size: radius,
                        // Probed one fixture per setting: 0 outside,
                        // 1 centre, 2 inside.
                        position: match enum_of(fx, b"Alig") {
                            Some(1) => style::StrokePosition::Center,
                            Some(2) => style::StrokePosition::Inside,
                            _ => style::StrokePosition::Outside,
                        },
                    });
                }
                b"BevE" => {
                    // The Type popup writes `Beve` 0 inner · 1 outer ·
                    // 2 emboss · 3 pillow — one probe fixture each.
                    // `Dept` is a depth in *pixels* beside `Radi`, not a
                    // factor; ours is a 0..1 fraction of the radius.
                    // `Invt` flips the bevel, and the highlight and
                    // shadow each carry their own blend, opacity and
                    // colour (`BlnM`/`Opac`/`HiCl` and
                    // `ShBM`/`ShOp`/`ShCl`).
                    let depth = f64_of(fx, b"Dept").unwrap_or(5.0) as f32 * s;
                    let sign = if bool_of(fx, b"Invt") == Some(true) {
                        -1.0
                    } else {
                        1.0
                    };
                    let default = style::BevelStyle::default();
                    layer.style.bevel = on(style::BevelStyle {
                        style: match enum_of(fx, b"Beve") {
                            Some(1) => style::BevelStyle_::OuterBevel,
                            Some(2) => style::BevelStyle_::Emboss,
                            Some(3) => style::BevelStyle_::PillowEmboss,
                            _ => style::BevelStyle_::InnerBevel,
                        },
                        angle: f64_of(fx, b"Azim").unwrap_or(2.356).to_degrees() as f32,
                        altitude: f64_of(fx, b"Elev").unwrap_or(0.785).to_degrees() as f32,
                        size: blur_radius,
                        soften: f64_of(fx, b"Sftn").unwrap_or(0.0) as f32 * s,
                        depth: sign * (depth / radius.max(1e-3)).clamp(0.0, 1.0),
                        highlight: self
                            .graph
                            .child(fx, b"HiCl")
                            .and_then(color_bytes)
                            .map(|c| schist_color::Rgba::from_u8(c[0], c[1], c[2], c[3]))
                            .unwrap_or(default.highlight),
                        highlight_blend: blend.unwrap_or(default.highlight_blend),
                        highlight_opacity: opacity,
                        shadow: self
                            .graph
                            .child(fx, b"ShCl")
                            .and_then(color_bytes)
                            .map(|c| schist_color::Rgba::from_u8(c[0], c[1], c[2], c[3]))
                            .unwrap_or(default.shadow),
                        shadow_blend: match fx.field(b"ShBM") {
                            Some(Value::Enum { id, version }) => blend_mode(*id, *version),
                            _ => None,
                        }
                        .unwrap_or(default.shadow_blend),
                        shadow_opacity: f64_of(fx, b"ShOp").unwrap_or(0.75) as f32,
                    });
                }
                b"GrdO" => {
                    // The gradient lives in a fill descriptor, the same
                    // shape a shape layer's `BFFl` uses.
                    let fill = self
                        .graph
                        .child(fx, b"GrFl")
                        .and_then(|g| self.graph.child(g, b"FDeF"));
                    let Some(stops) = fill.and_then(|f| gradient_stops(self.graph, f)) else {
                        self.report
                            .skipped
                            .push((format!("{} (effect)", layer.name), tag_name(fx.type_tag())));
                        continue;
                    };
                    let rgba = |c: [u8; 4]| schist_color::Rgba::from_u8(c[0], c[1], c[2], c[3]);
                    let radial = matches!(
                        fill.and_then(|f| f.field(b"Type")),
                        Some(Value::Enum { id: 2.., .. })
                    );
                    layer.style.gradient_overlay = on(style::GradientOverlayStyle {
                        from: stops.first().map(|s| rgba(s.1)).unwrap_or_default(),
                        to: stops.last().map(|s| rgba(s.1)).unwrap_or_default(),
                        blend: blend.unwrap_or(schist_core::BlendMode::Normal),
                        opacity,
                        // No `FDeX`: the panel's own scale, offset and
                        // angle controls are absent at their defaults,
                        // and the ramp runs left to right across the
                        // layer's bounds.
                        angle: 0.0,
                        shape: if radial {
                            style::GradientShape::Radial
                        } else {
                            style::GradientShape::Linear
                        },
                        ..style::GradientOverlayStyle::default()
                    });
                }
                b"Gaus" => {
                    layer.style.blur = on(style::BlurStyle {
                        // `Radi` is not a pixel radius: probed against
                        // blur_r10/r30/r60.af (a hard-edged square whose
                        // blurred alpha fits an error function almost
                        // exactly), Affinity's Gaussian has a standard
                        // deviation of 0.373 x Radi (Radi 90 comes out 6%
                        // under that line, the rest are on it). Our own
                        // radius is one where sigma = radius / sqrt(3),
                        // putting the conversion at 0.646; the three box
                        // passes approximating it run a few percent wide
                        // and quantise to an integer box, and 0.60 is
                        // where the three probes actually agree best.
                        radius: radius * 0.60,
                        // `PrAl`, the panel's "Preserve alpha": blur the
                        // colour inside an unchanged silhouette.
                        preserve_alpha: bool_of(fx, b"PrAl").unwrap_or(false),
                    });
                }
                _ => {
                    // The 3D effect and anything else we can't restyle
                    // changes what the layer looks like — record the gap.
                    self.report
                        .skipped
                        .push((format!("{} (effect)", layer.name), tag_name(fx.type_tag())));
                }
            }
        }
    }
}
