//! Layer styles ("effects" in the PSD format, `lfx2`).
//!
//! These are pure description: how the effects are rasterized lives in
//! `schist-layer-fx`, which turns a layer's pixels plus its style into
//! a single styled raster that the compositor then blends normally. That
//! split keeps the kernel free of rendering policy and
//! matches Photoshop's own model, where Fill opacity scales the layer's
//! own pixels and Opacity scales the layer *and* its effects.

use schist_color::Rgba;
use serde::{Deserialize, Serialize};

use crate::blend::BlendMode;

/// Where a stroke sits relative to the layer's edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum StrokePosition {
    #[default]
    Outside,
    Inside,
    Center,
}

/// Glow shape: Photoshop's "Technique".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Technique {
    /// Gaussian falloff.
    #[default]
    Softer,
    /// Distance-based, giving a harder shoulder.
    Precise,
}

/// Drop Shadow and Inner Shadow share every control.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ShadowStyle {
    pub color: Rgba,
    pub blend: BlendMode,
    /// 0.0..=1.0
    pub opacity: f32,
    /// Degrees, counter-clockwise, 0 = light from the right.
    pub angle: f32,
    /// Offset from the layer, in pixels.
    pub distance: f32,
    /// Grows the shadow before blurring, 0.0..=1.0 of the size.
    pub spread: f32,
    /// Blur radius in pixels.
    pub size: f32,
    /// Hide the shadow where the layer itself is (drop shadow only).
    pub knockout: bool,
}

impl Default for ShadowStyle {
    fn default() -> Self {
        ShadowStyle {
            color: Rgba::new(0.0, 0.0, 0.0, 1.0),
            blend: BlendMode::Multiply,
            opacity: 0.75,
            angle: 120.0,
            distance: 5.0,
            spread: 0.0,
            size: 5.0,
            knockout: true,
        }
    }
}

/// Outer Glow and Inner Glow.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GlowStyle {
    pub color: Rgba,
    pub blend: BlendMode,
    pub opacity: f32,
    /// Grows the glow before blurring, 0.0..=1.0 of the size.
    pub spread: f32,
    pub size: f32,
    pub technique: Technique,
    /// Inner glow only: glow inwards from the edge rather than out from
    /// the centre.
    pub from_edge: bool,
}

impl Default for GlowStyle {
    fn default() -> Self {
        GlowStyle {
            color: Rgba::new(1.0, 1.0, 0.75, 1.0),
            blend: BlendMode::Screen,
            opacity: 0.75,
            spread: 0.0,
            size: 5.0,
            technique: Technique::Softer,
            from_edge: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct StrokeStyle {
    pub color: Rgba,
    pub blend: BlendMode,
    pub opacity: f32,
    pub size: f32,
    pub position: StrokePosition,
}

impl Default for StrokeStyle {
    fn default() -> Self {
        StrokeStyle {
            color: Rgba::new(0.0, 0.0, 0.0, 1.0),
            blend: BlendMode::Normal,
            opacity: 1.0,
            size: 3.0,
            position: StrokePosition::Outside,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ColorOverlayStyle {
    pub color: Rgba,
    pub blend: BlendMode,
    pub opacity: f32,
}

impl Default for ColorOverlayStyle {
    fn default() -> Self {
        ColorOverlayStyle {
            color: Rgba::new(1.0, 0.0, 0.0, 1.0),
            blend: BlendMode::Normal,
            opacity: 1.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum GradientShape {
    #[default]
    Linear,
    Radial,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GradientOverlayStyle {
    pub from: Rgba,
    pub to: Rgba,
    pub blend: BlendMode,
    pub opacity: f32,
    pub angle: f32,
    pub shape: GradientShape,
    pub reverse: bool,
    /// 0.1..=5.0, stretches the ramp across the layer.
    pub scale: f32,
}

impl Default for GradientOverlayStyle {
    fn default() -> Self {
        GradientOverlayStyle {
            from: Rgba::new(0.0, 0.0, 0.0, 1.0),
            to: Rgba::new(1.0, 1.0, 1.0, 1.0),
            blend: BlendMode::Normal,
            opacity: 1.0,
            angle: 90.0,
            shape: GradientShape::Linear,
            reverse: false,
            scale: 1.0,
        }
    }
}

/// Satin: the layer's own shape, blurred and offset both ways, differenced
/// against itself. Photoshop's "interior shading".
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SatinStyle {
    pub color: Rgba,
    pub blend: BlendMode,
    pub opacity: f32,
    pub angle: f32,
    pub distance: f32,
    pub size: f32,
    pub invert: bool,
}

impl Default for SatinStyle {
    fn default() -> Self {
        SatinStyle {
            color: Rgba::new(0.0, 0.0, 0.0, 1.0),
            blend: BlendMode::Multiply,
            opacity: 0.5,
            angle: 19.0,
            distance: 11.0,
            size: 14.0,
            invert: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum BevelStyle_ {
    /// Bevel the outside edge.
    OuterBevel,
    #[default]
    InnerBevel,
    /// Both, as if the layer were embossed into the surface.
    Emboss,
    /// Emboss the pillow way round.
    PillowEmboss,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BevelStyle {
    pub style: BevelStyle_,
    /// Degrees; the direction the light comes from.
    pub angle: f32,
    /// Degrees above the surface, 0..=90.
    pub altitude: f32,
    pub size: f32,
    /// Softens the shading.
    pub soften: f32,
    /// 0.0..=1.0. Negative depth inverts the bevel.
    pub depth: f32,
    pub highlight: Rgba,
    pub highlight_blend: BlendMode,
    pub highlight_opacity: f32,
    pub shadow: Rgba,
    pub shadow_blend: BlendMode,
    pub shadow_opacity: f32,
}

impl Default for BevelStyle {
    fn default() -> Self {
        BevelStyle {
            style: BevelStyle_::InnerBevel,
            angle: 120.0,
            altitude: 30.0,
            size: 5.0,
            soften: 0.0,
            depth: 1.0,
            highlight: Rgba::new(1.0, 1.0, 1.0, 1.0),
            highlight_blend: BlendMode::Screen,
            highlight_opacity: 0.75,
            shadow: Rgba::new(0.0, 0.0, 0.0, 1.0),
            shadow_blend: BlendMode::Multiply,
            shadow_opacity: 0.75,
        }
    }
}

/// Affinity's Gaussian Blur layer effect (`Gaus`): unlike every other
/// slot this one does not draw anything, it softens the layer's own
/// pixels before the rest of the stack sees them — so a shadow or
/// stroke follows the blurred shape, exactly as the app renders it.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BlurStyle {
    /// Gaussian radius in pixels.
    pub radius: f32,
    /// Blur the colour but keep the layer's own alpha (`PrAl`), so the
    /// shape stays crisp and only its contents smear.
    pub preserve_alpha: bool,
}

impl Default for BlurStyle {
    fn default() -> Self {
        BlurStyle {
            radius: 5.0,
            preserve_alpha: false,
        }
    }
}

/// Everything Photoshop's Layer Style dialog can turn on for one layer.
///
/// `None` means the effect is absent; `Some` with `enabled == false` in the
/// containing [`Effect`] means it is configured but switched off, which is
/// how the dialog remembers settings across a tick of the checkbox.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub struct LayerStyle {
    pub blur: Effect<BlurStyle>,
    pub drop_shadow: Effect<ShadowStyle>,
    pub inner_shadow: Effect<ShadowStyle>,
    pub outer_glow: Effect<GlowStyle>,
    pub inner_glow: Effect<GlowStyle>,
    pub bevel: Effect<BevelStyle>,
    pub satin: Effect<SatinStyle>,
    pub color_overlay: Effect<ColorOverlayStyle>,
    pub gradient_overlay: Effect<GradientOverlayStyle>,
    pub stroke: Effect<StrokeStyle>,
}

/// One effect slot: its settings, plus whether it is switched on.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Effect<T> {
    pub enabled: bool,
    pub settings: T,
}

impl<T: Default> Default for Effect<T> {
    fn default() -> Self {
        Effect {
            enabled: false,
            settings: T::default(),
        }
    }
}

impl<T> Effect<T> {
    /// The settings, or `None` when the effect is switched off.
    pub fn on(&self) -> Option<&T> {
        self.enabled.then_some(&self.settings)
    }
}

impl LayerStyle {
    /// True when nothing is switched on, i.e. the layer renders as-is.
    pub fn is_empty(&self) -> bool {
        !(self.blur.enabled
            || self.drop_shadow.enabled
            || self.inner_shadow.enabled
            || self.outer_glow.enabled
            || self.inner_glow.enabled
            || self.bevel.enabled
            || self.satin.enabled
            || self.color_overlay.enabled
            || self.gradient_overlay.enabled
            || self.stroke.enabled)
    }

    /// How far outside the layer's own pixels the effects can reach. The
    /// styled raster has to be grown by this much or shadows get clipped.
    pub fn outset(&self) -> i32 {
        let mut out = 0.0f32;
        // A blur is the one effect that grows the layer's own pixels,
        // and every later effect works off the grown alpha, so its
        // reach adds to the others rather than competing with them.
        // Three box passes of radius/sqrt(3) reach sqrt(3) times the
        // nominal radius, so a blur needs that much room, not its
        // radius, or the soft edge is clipped square.
        let blur = self.blur.on().map_or(0.0, |b| b.radius * 1.7321);
        if let Some(s) = self.drop_shadow.on() {
            out = out.max(s.distance + s.size + s.spread * s.size);
        }
        if let Some(g) = self.outer_glow.on() {
            out = out.max(g.size + g.spread * g.size);
        }
        if let Some(s) = self.stroke.on() {
            out = out.max(match s.position {
                StrokePosition::Outside => s.size,
                StrokePosition::Center => s.size / 2.0,
                StrokePosition::Inside => 0.0,
            });
        }
        // The inner glow and inner shadow blur the *outside* of the
        // shape inwards. They draw nothing beyond the layer, but the
        // buffer still has to hold that exterior out to the blur's
        // reach: with only the pixel of slack below, the first pass
        // smears a one-pixel border into nothing and the glow comes
        // back at two thirds strength along its own edge.
        for reach in [
            self.inner_glow.on().map(|g| g.size),
            self.inner_shadow.on().map(|s| s.size + s.distance),
        ]
        .into_iter()
        .flatten()
        {
            out = out.max(reach * 1.7321);
        }
        if let Some(b) = self.bevel.on() {
            if matches!(
                b.style,
                BevelStyle_::OuterBevel | BevelStyle_::Emboss | BevelStyle_::PillowEmboss
            ) {
                out = out.max(b.size + b.soften);
            }
        }
        // A pixel of slack so antialiased edges are not clipped.
        (out + blur).ceil() as i32 + 1
    }
}
