//! ICC colour management.
//!
//! Three jobs:
//!
//! * **Display transform** — composited pixels are in the *document's*
//!   space; the screen has its own. Everything drawn goes through a
//!   document→display transform so a wide-gamut file looks right on an
//!   sRGB panel and vice versa.
//! * **Assign vs. convert** — assigning a profile reinterprets the same
//!   numbers, converting rewrites the pixels to preserve appearance. They
//!   are different operations and the UI keeps them separate.
//! * **Soft proof** — preview how the document will look on some other
//!   device by routing document→proof→display.
//!
//! Profiles are parsed by `moxcms` (pure Rust, no C toolchain).

use anyhow::{anyhow, Result};
use moxcms::{
    CicpColorPrimaries, CicpProfile, ColorProfile, Layout, MatrixCoefficients, RenderingIntent,
    TransferCharacteristics, TransformExecutor, TransformOptions,
};
use std::sync::Arc;

/// How out-of-gamut colours are handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum Intent {
    #[default]
    Perceptual,
    RelativeColorimetric,
    Saturation,
    AbsoluteColorimetric,
}

impl Intent {
    pub fn display_name(self) -> &'static str {
        match self {
            Intent::Perceptual => "Perceptual",
            Intent::RelativeColorimetric => "Relative Colorimetric",
            Intent::Saturation => "Saturation",
            Intent::AbsoluteColorimetric => "Absolute Colorimetric",
        }
    }

    pub fn all() -> &'static [Intent] {
        &[
            Intent::Perceptual,
            Intent::RelativeColorimetric,
            Intent::Saturation,
            Intent::AbsoluteColorimetric,
        ]
    }

    fn to_mox(self) -> RenderingIntent {
        match self {
            Intent::Perceptual => RenderingIntent::Perceptual,
            Intent::RelativeColorimetric => RenderingIntent::RelativeColorimetric,
            Intent::Saturation => RenderingIntent::Saturation,
            Intent::AbsoluteColorimetric => RenderingIntent::AbsoluteColorimetric,
        }
    }
}

/// A named constructor for one of the built-in profiles.
pub type BuiltinProfile = (&'static str, fn() -> Profile);

/// A parsed ICC profile plus the bytes it came from (documents store the
/// bytes so a file round-trips with the exact profile it arrived with).
#[derive(Clone)]
pub struct Profile {
    profile: Arc<ColorProfile>,
    bytes: Option<Arc<Vec<u8>>>,
    name: String,
}

impl std::fmt::Debug for Profile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Profile").field("name", &self.name).finish()
    }
}

impl Profile {
    /// Parse an embedded ICC profile.
    pub fn from_bytes(bytes: &[u8]) -> Result<Profile> {
        let profile = ColorProfile::new_from_slice(bytes)
            .map_err(|e| anyhow!("unreadable ICC profile: {e:?}"))?;
        // moxcms doesn't surface the profile description tag, so embedded
        // profiles are shown generically until it does.
        let name = "Embedded profile".to_string();
        Ok(Profile {
            profile: Arc::new(profile),
            bytes: Some(Arc::new(bytes.to_vec())),
            name,
        })
    }

    pub fn srgb() -> Profile {
        Profile::builtin(ColorProfile::new_srgb(), "sRGB")
    }

    pub fn display_p3() -> Profile {
        Profile::builtin(ColorProfile::new_display_p3(), "Display P3")
    }

    /// A built-in profile, serialized so it can be embedded on save.
    ///
    /// These used to carry `bytes: None`, which made them unusable as
    /// assignment targets: `icc_bytes()` returned `None`, so assigning
    /// either of the two profiles the UI offers *untagged* the document
    /// instead of tagging it, and the next open reinterpreted the pixels
    /// against whatever the working space happened to be.
    fn builtin(profile: ColorProfile, name: &str) -> Profile {
        let bytes = profile.encode().ok().map(Arc::new);
        if bytes.is_none() {
            log::warn!("could not serialize the built-in {name} profile");
        }
        Profile {
            profile: Arc::new(profile),
            bytes,
            name: name.into(),
        }
    }

    /// Built-in profiles offered in the UI.
    pub fn builtins() -> Vec<BuiltinProfile> {
        vec![("sRGB", Profile::srgb), ("Display P3", Profile::display_p3)]
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn with_name(mut self, name: impl Into<String>) -> Profile {
        self.name = name.into();
        self
    }

    /// The bytes to embed when saving, if this profile came from a file.
    pub fn icc_bytes(&self) -> Option<&[u8]> {
        self.bytes.as_ref().map(|b| b.as_slice())
    }
}

/// A compiled document→display (or document→proof→display) transform over
/// straight-alpha f32 RGBA buffers.
pub struct ColorTransform {
    executor: Arc<dyn TransformExecutor<f32> + Send + Sync>,
    /// True when source and destination match, so callers can skip work.
    identity: bool,
}

impl std::fmt::Debug for ColorTransform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ColorTransform")
            .field("identity", &self.identity)
            .finish()
    }
}

impl ColorTransform {
    /// Build a transform between two profiles.
    pub fn new(src: &Profile, dst: &Profile, intent: Intent) -> Result<ColorTransform> {
        let options = TransformOptions {
            rendering_intent: intent.to_mox(),
            ..Default::default()
        };
        let executor = src
            .profile
            .create_transform_f32(Layout::Rgba, &dst.profile, Layout::Rgba, options)
            .map_err(|e| anyhow!("cannot build colour transform: {e:?}"))?;
        Ok(ColorTransform {
            executor,
            identity: false,
        })
    }

    /// A transform that does nothing (source and destination agree).
    pub fn identity() -> ColorTransform {
        struct Noop;
        impl TransformExecutor<f32> for Noop {
            fn transform(&self, src: &[f32], dst: &mut [f32]) -> Result<(), moxcms::CmsError> {
                dst.copy_from_slice(src);
                Ok(())
            }
        }
        ColorTransform {
            executor: Arc::new(Noop),
            identity: true,
        }
    }

    pub fn is_identity(&self) -> bool {
        self.identity
    }

    /// Convert a straight-alpha f32 RGBA buffer in place.
    ///
    /// Alpha is carried through untouched: it is coverage, not colour.
    /// Transform a straight-alpha f32 RGBA buffer in place.
    ///
    /// Note this cannot preserve extended range even on the editing path:
    /// moxcms clamps to 0..1 while evaluating the transfer curves
    /// (`gamma.rs`), so a 32-bit document's out-of-range highlights are
    /// clipped by the CMS before we see the output. Removing the clamp
    /// below would not change that; it needs either CMS support or a
    /// matrix-only path that skips the curves.
    pub fn apply(&self, pixels: &mut [f32]) {
        if self.identity || pixels.is_empty() {
            return;
        }
        let src = pixels.to_vec();
        if let Err(err) = self.executor.transform(&src, pixels) {
            log::warn!("colour transform failed: {err:?}");
            pixels.copy_from_slice(&src);
            return;
        }
        // Clamp for display and restore alpha, which a matrix transform
        // may have touched.
        for (out, inp) in pixels
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .zip(src.as_chunks::<4>().0)
        {
            out[0] = out[0].clamp(0.0, 1.0);
            out[1] = out[1].clamp(0.0, 1.0);
            out[2] = out[2].clamp(0.0, 1.0);
            out[3] = inp[3];
        }
    }
}

/// The colour pipeline a document is displayed through.
#[derive(Debug, Clone)]
pub struct ColorSettings {
    /// Profile assigned to documents that carry none.
    pub working: Profile,
    /// The monitor's profile.
    pub display: Profile,
    pub intent: Intent,
    /// When set, preview through this device before hitting the display.
    pub proof: Option<Profile>,
}

impl Default for ColorSettings {
    fn default() -> Self {
        ColorSettings {
            working: Profile::srgb(),
            display: Profile::srgb(),
            intent: Intent::Perceptual,
            proof: None,
        }
    }
}

impl ColorSettings {
    /// Build the transform for a document with the given embedded profile.
    ///
    /// Soft proofing runs document→proof→display; the two hops are baked
    /// into one executor chain by applying them in sequence.
    pub fn transform_for(&self, document_icc: Option<&[u8]>) -> ColorTransform {
        let source = match document_icc {
            Some(bytes) => match Profile::from_bytes(bytes) {
                Ok(p) => p,
                Err(err) => {
                    log::warn!("{err:#}; falling back to the working space");
                    self.working.clone()
                }
            },
            None => self.working.clone(),
        };
        match ColorTransform::new(&source, &self.display, self.intent) {
            Ok(t) => t,
            Err(err) => {
                log::warn!("{err:#}; displaying without conversion");
                ColorTransform::identity()
            }
        }
    }

    /// The proofing hop, if soft proofing is on.
    pub fn proof_transform(&self, document_icc: Option<&[u8]>) -> Option<ColorTransform> {
        let proof = self.proof.as_ref()?;
        let source = match document_icc {
            Some(bytes) => Profile::from_bytes(bytes).unwrap_or_else(|_| self.working.clone()),
            None => self.working.clone(),
        };
        // Proofing is colorimetric by definition: it must show the target's
        // gamut clipping rather than re-map it pleasingly.
        ColorTransform::new(&source, proof, Intent::RelativeColorimetric).ok()
    }
}

/// Bake BT.2100 HDR pixels (PQ or HLG signal, straight-alpha RGBA f32)
/// down to sRGB in place.
///
/// `primaries` and `transfer` are H.273/cICP code points; `transfer` must
/// be PQ (16) or HLG (18). Graphic ("diffuse") white — 203 nits, per
/// BT.2408 — maps to 1.0, and the specular range above it rolls off
/// through an exponential shoulder rather than clipping, approximating
/// the SDR rendition cameras bake for HDR captures.
pub fn bake_hdr_to_srgb(pixels: &mut [f32], primaries: u8, transfer: u8) -> Result<()> {
    const REF_WHITE_NITS: f32 = 203.0;
    /// Where the shoulder starts, in diffuse-white-relative linear light.
    const KNEE: f32 = 0.9;

    let signal_to_nits: fn(f32) -> f32 = match transfer {
        16 => |v: f32| pq_eotf(v) * 10_000.0,
        // Per-channel HLG approximation: 1000-nit nominal display, with
        // the BT.2100 OOTF's system gamma of 1.2 applied channel-wise
        // rather than to luminance.
        18 => |v: f32| hlg_inverse_oetf(v).powf(1.2) * 1_000.0,
        other => return Err(anyhow!("cICP transfer {other} is not PQ or HLG")),
    };
    let primaries = CicpColorPrimaries::try_from(primaries)
        .map_err(|e| anyhow!("bad cICP primaries: {e:?}"))?;
    let source = Profile {
        profile: Arc::new(ColorProfile::new_from_cicp(CicpProfile {
            color_primaries: primaries,
            transfer_characteristics: TransferCharacteristics::Linear,
            matrix_coefficients: MatrixCoefficients::Identity,
            full_range: true,
        })),
        bytes: None,
        name: "HDR source".into(),
    };
    for px in pixels.as_chunks_mut::<4>().0 {
        for c in px.iter_mut().take(3) {
            let s = signal_to_nits(c.clamp(0.0, 1.0)) / REF_WHITE_NITS;
            *c = if s <= KNEE {
                s
            } else {
                KNEE + (1.0 - KNEE) * (1.0 - (-(s - KNEE) / (1.0 - KNEE)).exp())
            };
        }
    }
    ColorTransform::new(&source, &Profile::srgb(), Intent::RelativeColorimetric)?.apply(pixels);
    Ok(())
}

/// BT.2100 PQ EOTF: signal 0..1 to display light as a fraction of the
/// 10 000-nit peak.
fn pq_eotf(v: f32) -> f32 {
    const M1: f32 = 1305.0 / 8192.0;
    const M2: f32 = 2523.0 / 32.0;
    const C1: f32 = 107.0 / 128.0;
    const C2: f32 = 2413.0 / 128.0;
    const C3: f32 = 2392.0 / 128.0;
    let p = v.max(0.0).powf(1.0 / M2);
    ((p - C1).max(0.0) / (C2 - C3 * p).max(f32::EPSILON)).powf(1.0 / M1)
}

/// BT.2100 HLG inverse OETF: signal 0..1 to scene light 0..1.
fn hlg_inverse_oetf(v: f32) -> f32 {
    const A: f32 = 0.178_832_77;
    const B: f32 = 0.284_668_92;
    const C: f32 = 0.559_910_7;
    let v = v.max(0.0);
    if v <= 0.5 {
        v * v / 3.0
    } else {
        (((v - C) / A).exp() + B) / 12.0
    }
}

/// Convert pixels from one profile to another, preserving appearance
/// (Image ▸ Convert to Profile).
pub fn convert_pixels(
    pixels: &mut [f32],
    from: &Profile,
    to: &Profile,
    intent: Intent,
) -> Result<()> {
    let transform = ColorTransform::new(from, to, intent)?;
    transform.apply(pixels);
    Ok(())
}

/// Reduce a buffer to a lower bit depth with ordered dithering.
///
/// Straight truncation bands smooth gradients badly at 8 bits; a 4x4 Bayer
/// matrix costs nothing and hides it.
pub fn dither_to_depth(pixels: &mut [f32], width: usize, levels: u32) {
    if levels < 2 || width == 0 {
        return;
    }
    const BAYER: [[f32; 4]; 4] = [
        [0.0, 8.0, 2.0, 10.0],
        [12.0, 4.0, 14.0, 6.0],
        [3.0, 11.0, 1.0, 9.0],
        [15.0, 7.0, 13.0, 5.0],
    ];
    let steps = (levels - 1) as f32;
    for (i, px) in pixels.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let x = i % width;
        let y = i / width;
        // Centre the threshold on zero so dithering doesn't shift levels.
        let threshold = (BAYER[y % 4][x % 4] / 16.0) - 0.5;
        for channel in px.iter_mut().take(3) {
            let scaled = *channel * steps + threshold;
            *channel = (scaled.round() / steps).clamp(0.0, 1.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgba(r: f32, g: f32, b: f32) -> Vec<f32> {
        vec![r, g, b, 1.0]
    }

    #[test]
    fn builtin_profiles_load() {
        assert_eq!(Profile::srgb().name(), "sRGB");
        assert_eq!(Profile::display_p3().name(), "Display P3");
        assert_eq!(Profile::builtins().len(), 2);
    }

    #[test]
    fn srgb_to_srgb_is_a_no_op() {
        let t = ColorTransform::new(&Profile::srgb(), &Profile::srgb(), Intent::Perceptual)
            .expect("transform builds");
        let mut px = rgba(0.2, 0.5, 0.9);
        let before = px.clone();
        t.apply(&mut px);
        for (a, b) in px.iter().zip(before.iter()) {
            assert!((a - b).abs() < 0.01, "{px:?} vs {before:?}");
        }
    }

    #[test]
    fn identity_transform_short_circuits() {
        let t = ColorTransform::identity();
        assert!(t.is_identity());
        let mut px = rgba(0.1, 0.2, 0.3);
        t.apply(&mut px);
        assert_eq!(px, rgba(0.1, 0.2, 0.3));
    }

    #[test]
    fn p3_to_srgb_expands_saturated_colors() {
        // Pure red in P3 is outside sRGB's gamut, so converting it into
        // sRGB must push the channel up (and clip), not leave it alone.
        let t = ColorTransform::new(
            &Profile::display_p3(),
            &Profile::srgb(),
            Intent::RelativeColorimetric,
        )
        .expect("transform builds");
        let mut px = rgba(1.0, 0.0, 0.0);
        t.apply(&mut px);
        assert!(px[0] > 0.9, "red stays red: {px:?}");
        assert!(
            px[1] < 0.35 && px[2] < 0.35,
            "P3 red maps to a saturated sRGB red: {px:?}"
        );
        assert_eq!(px[3], 1.0, "alpha untouched");
    }

    #[test]
    fn round_tripping_through_p3_returns_close_to_the_original() {
        let to_p3 = ColorTransform::new(
            &Profile::srgb(),
            &Profile::display_p3(),
            Intent::RelativeColorimetric,
        )
        .unwrap();
        let back = ColorTransform::new(
            &Profile::display_p3(),
            &Profile::srgb(),
            Intent::RelativeColorimetric,
        )
        .unwrap();
        let original = rgba(0.35, 0.6, 0.8);
        let mut px = original.clone();
        to_p3.apply(&mut px);
        assert_ne!(px, original, "the conversion changed the numbers");
        back.apply(&mut px);
        for (a, b) in px.iter().zip(original.iter()) {
            assert!((a - b).abs() < 0.02, "round trip: {px:?} vs {original:?}");
        }
    }

    #[test]
    fn grey_stays_neutral_across_profiles() {
        let t = ColorTransform::new(
            &Profile::display_p3(),
            &Profile::srgb(),
            Intent::RelativeColorimetric,
        )
        .unwrap();
        let mut px = rgba(0.5, 0.5, 0.5);
        t.apply(&mut px);
        assert!(
            (px[0] - px[1]).abs() < 0.01 && (px[1] - px[2]).abs() < 0.01,
            "neutral stays neutral: {px:?}"
        );
    }

    #[test]
    fn settings_without_an_embedded_profile_use_the_working_space() {
        let settings = ColorSettings {
            working: Profile::display_p3(),
            display: Profile::srgb(),
            ..Default::default()
        };
        let t = settings.transform_for(None);
        assert!(!t.is_identity());
        let mut px = rgba(1.0, 0.0, 0.0);
        t.apply(&mut px);
        assert!(px[1] < 0.4, "treated as P3 source: {px:?}");
    }

    #[test]
    fn a_corrupt_embedded_profile_falls_back_instead_of_failing() {
        let settings = ColorSettings::default();
        let t = settings.transform_for(Some(b"not an icc profile"));
        let mut px = rgba(0.3, 0.3, 0.3);
        t.apply(&mut px); // must not panic
        assert!(px.iter().all(|c| c.is_finite()));
    }

    #[test]
    fn soft_proof_transform_only_exists_when_proofing() {
        let mut settings = ColorSettings::default();
        assert!(settings.proof_transform(None).is_none());
        settings.proof = Some(Profile::display_p3());
        assert!(settings.proof_transform(None).is_some());
    }

    #[test]
    fn dithering_keeps_the_average_but_breaks_up_banding() {
        // A flat value halfway between two 4-level steps must dither into
        // both neighbours rather than snapping everything one way.
        let width = 4;
        let mut pixels: Vec<f32> = (0..16).flat_map(|_| [0.5f32, 0.5, 0.5, 1.0]).collect();
        dither_to_depth(&mut pixels, width, 4);
        let values: Vec<f32> = pixels.as_chunks::<4>().0.iter().map(|p| p[0]).collect();
        let distinct: Vec<f32> = {
            let mut v = values.clone();
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
            v.dedup();
            v
        };
        assert!(
            distinct.len() >= 2,
            "dither produced variation: {distinct:?}"
        );
        let mean = values.iter().sum::<f32>() / values.len() as f32;
        assert!((mean - 0.5).abs() < 0.12, "mean preserved: {mean}");
    }

    #[test]
    fn dithering_leaves_exact_levels_alone() {
        let mut pixels = vec![0.0f32, 0.0, 0.0, 1.0, 1.0, 1.0, 1.0, 1.0];
        dither_to_depth(&mut pixels, 2, 256);
        assert_eq!(pixels[0], 0.0);
        assert_eq!(pixels[4], 1.0);
    }

    #[test]
    fn pq_reference_white_bakes_near_srgb_white() {
        // PQ signal for 203 nits — HDR graphics white — must land close
        // to full white, not the murky grey a naive 10 000-nit-relative
        // transform would produce.
        let mut px = rgba(0.5806, 0.5806, 0.5806);
        bake_hdr_to_srgb(&mut px, 9, 16).unwrap();
        assert!(px[0] > 0.93, "reference white stays white: {px:?}");
        assert!((px[0] - px[1]).abs() < 0.01 && (px[1] - px[2]).abs() < 0.01);
        assert_eq!(px[3], 1.0, "alpha untouched");
    }

    #[test]
    fn pq_blacks_stay_black_and_speculars_stay_bounded() {
        let mut px = vec![0.0f32, 0.0, 0.0, 1.0, 0.9, 0.9, 0.9, 1.0];
        bake_hdr_to_srgb(&mut px, 9, 16).unwrap();
        assert!(px[0] < 0.02, "black stays black: {}", px[0]);
        // A ~1000-nit specular compresses into the shoulder above
        // reference white but never exceeds 1.0.
        assert!(
            px[4] > 0.95 && px[4] <= 1.0,
            "specular rolls off: {}",
            px[4]
        );
    }

    #[test]
    fn pq_bake_is_monotone() {
        let mut px: Vec<f32> = (0..64)
            .flat_map(|i| [i as f32 / 63.0, i as f32 / 63.0, i as f32 / 63.0, 1.0])
            .collect();
        bake_hdr_to_srgb(&mut px, 9, 16).unwrap();
        let greys: Vec<f32> = px.as_chunks::<4>().0.iter().map(|p| p[0]).collect();
        assert!(
            greys.windows(2).all(|w| w[1] >= w[0]),
            "monotone: {greys:?}"
        );
    }

    #[test]
    fn hlg_mid_grey_bakes_sensibly() {
        // HLG 0.5 signal is scene light 1/12 → ~26 nits after the OOTF,
        // well below reference white but clearly not black.
        let mut px = rgba(0.5, 0.5, 0.5);
        bake_hdr_to_srgb(&mut px, 9, 18).unwrap();
        assert!(px[0] > 0.2 && px[0] < 0.6, "HLG mid-grey: {px:?}");
    }

    #[test]
    fn bake_rejects_sdr_transfers() {
        let mut px = rgba(0.5, 0.5, 0.5);
        assert!(bake_hdr_to_srgb(&mut px, 9, 1).is_err());
        assert!(bake_hdr_to_srgb(&mut px, 9, 13).is_err());
    }

    #[test]
    fn convert_pixels_matches_a_manual_transform() {
        let mut a = rgba(0.4, 0.2, 0.7);
        let mut b = a.clone();
        convert_pixels(
            &mut a,
            &Profile::srgb(),
            &Profile::display_p3(),
            Intent::Perceptual,
        )
        .unwrap();
        ColorTransform::new(&Profile::srgb(), &Profile::display_p3(), Intent::Perceptual)
            .unwrap()
            .apply(&mut b);
        assert_eq!(a, b);
    }
    #[test]
    fn builtin_profiles_can_be_embedded() {
        // `bytes: None` made these unusable as assignment targets:
        // `assign_profile` writes `icc_bytes()` onto the document, so
        // assigning either of the two profiles the UI offers untagged the
        // document rather than tagging it.
        for p in [Profile::srgb(), Profile::display_p3()] {
            let bytes = p
                .icc_bytes()
                .unwrap_or_else(|| panic!("{} has no bytes", p.name));
            assert!(bytes.len() > 128, "{} icc is implausibly small", p.name);
            assert_eq!(&bytes[36..40], b"acsp", "{} is not an icc profile", p.name);
            // And it must round-trip back through the parser.
            assert!(
                Profile::from_bytes(bytes).is_ok(),
                "{} did not parse back",
                p.name
            );
        }
    }

    #[test]
    fn conversion_still_leaves_alpha_alone() {
        let mut px = vec![0.4f32, 0.2, 0.7, 0.33];
        convert_pixels(
            &mut px,
            &Profile::srgb(),
            &Profile::display_p3(),
            Intent::Perceptual,
        )
        .unwrap();
        assert!((px[3] - 0.33).abs() < 1e-6, "alpha changed: {}", px[3]);
    }
}
