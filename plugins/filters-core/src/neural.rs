//! Photoshop's Neural Filters.
//!
//! Twelve of them, nine of which run a real network. Super Zoom, JPEG
//! Artifact Removal, Colorize and Photo Restoration use models trained
//! for this application and shipped inside the binary (`tools/train/`);
//! Style Transfer uses the fast neural-style networks from the ONNX
//! Model Zoo, Depth Blur and Landscape Mixer use MiDaS, and Skin
//! Smoothing and Face to Caricature use UltraFace, all downloaded on
//! demand. Everything runs through `schist-neural`, which is `tract` --
//! pure Rust, so there is no runtime to install.
//!
//! The three that are not networks are not networks for different
//! reasons. Skin Smoothing's *smoothing* is frequency separation, which
//! is what a retoucher does by hand; the network's job there is to say
//! where the faces are, which is the part that needs to know what a face
//! is. Colour Transfer and Harmonization move one image's colour
//! distribution onto another's, which is arithmetic -- what Adobe's
//! networks add is matching the two *by subject*, so that a reference's
//! sky lands on your sky rather than on your whole picture.
//!
//! Three of these need a second image, and the one a filter can be
//! handed without a file picker is the layer underneath: Colour
//! Transfer, Harmonization and Landscape Mixer take their reference from
//! whatever the document composites to below this layer, which is asked
//! for with [`FilterPlugin::wants_backdrop`].
//!
//! Every model-backed filter also works without its model, falling back
//! to the classical path and saying so in its dialog. Nothing here is a
//! stub that stops working when a download fails.

use std::sync::{Arc, Mutex};

use crate::util::{at, gaussian_rgba, luma, put, sample, warp};
use crate::{choice, param, simple_filter};
use schist_neural::Face;
use schist_plugin_api::{FilterContext, FilterParam, FilterPlugin, FilterValues};

/// Copy the RGB of a filter buffer out, run `f` on it, and blend the
/// result back. Models work on RGB; the filter buffer is RGBA.
fn through_rgb(px: &mut [f32], f: impl FnOnce(&mut Vec<f32>)) {
    let mut rgb = rgb_of(px);
    f(&mut rgb);
    for (i, p) in px.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        p[..3].copy_from_slice(&rgb[i * 3..i * 3 + 3]);
    }
}

/// The RGB of a filter buffer, for a model that only reads it.
fn rgb_of(px: &[f32]) -> Vec<f32> {
    let mut rgb = Vec::with_capacity(px.len() / 4 * 3);
    for p in px.as_chunks::<4>().0.iter() {
        rgb.extend_from_slice(&p[..3]);
    }
    rgb
}

/// The note a model-backed filter shows in its dialog.
fn model_note(id: &str, fallback: &str) -> Option<String> {
    match schist_neural::spec(id) {
        Some(spec) if schist_neural::installed(id) => {
            // The dot rather than nested brackets: the names already have
            // brackets in them.
            Some(format!("Using {} \u{b7} {}.", spec.name, spec.license))
        }
        Some(spec) => Some(format!(
            "{} is not installed \u{2014} {fallback} Get it from \
             Filter \u{25b8} Neural Filters \u{25b8} Manage Models.",
            spec.name
        )),
        None => None,
    }
}

/// A memo of something a network worked out about the picture.
///
/// The expensive part of these filters does not depend on their sliders:
/// where the faces are, how far away things are and what colour they
/// ought to be are all facts about the image, and the image does not
/// change while somebody drags Strength. Inference happens once and the
/// answer is kept until the pixels underneath it change.
struct Memo<T> {
    kept: Mutex<Option<(u64, Arc<T>)>>,
}

impl<T> Memo<T> {
    const fn new() -> Memo<T> {
        Memo {
            kept: Mutex::new(None),
        }
    }

    fn get(&self, key: u64, compute: impl FnOnce() -> Option<T>) -> Option<Arc<T>> {
        if let Ok(kept) = self.kept.lock() {
            if let Some((k, v)) = kept.as_ref() {
                if *k == key {
                    return Some(v.clone());
                }
            }
        }
        let value = Arc::new(compute()?);
        if let Ok(mut kept) = self.kept.lock() {
            *kept = Some((key, value.clone()));
        }
        Some(value)
    }
}

/// What the memo is keyed on: the shape of the pixels and a sample of
/// them.
///
/// A sample rather than all of them, because reading a hundred megabytes
/// on every keystroke to discover that nothing changed costs more than
/// some of the models do. Sixty-four thousand pixels spread evenly over
/// the image, all four channels of each: for this to go wrong an edit
/// would have to miss every one of them *and* land between two runs of
/// the same filter, and the thing making those runs is a preview loop
/// handing back the identical buffer.
fn fingerprint(px: &[f32], width: usize, height: usize) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64 ^ ((width as u64) << 32) ^ height as u64;
    let pixels = px.len() / 4;
    let stride = (pixels / (1 << 16)).max(1);
    for p in px.as_chunks::<4>().0.iter().step_by(stride) {
        for v in p {
            h ^= v.to_bits() as u64;
            h = h.wrapping_mul(0x1000_0000_01b3);
        }
    }
    h ^ pixels as u64
}

/// How much a colour looks like skin, 0..=1.
///
/// Skin tones sit in a narrow wedge: red leads, green follows, blue
/// trails, and the whole thing is reasonably bright and not very
/// saturated. That is enough to separate a face from a blue shirt, which
/// is the separation this filter needs.
fn skinness(p: &[f32]) -> f32 {
    let (r, g, b) = (p[0], p[1], p[2]);
    if r <= g || g < b || r < 0.2 {
        return 0.0;
    }
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let sat = if max > 0.0 { (max - min) / max } else { 0.0 };
    // Within the wedge, prefer moderate saturation and mid-to-high
    // brightness, falling off smoothly at the edges so there is no seam.
    let sat_fit = 1.0 - ((sat - 0.3).abs() / 0.35).clamp(0.0, 1.0);
    let lum_fit = 1.0 - ((luma(p) - 0.55).abs() / 0.45).clamp(0.0, 1.0);
    let hue_fit = ((r - b) / r.max(1e-4)).clamp(0.0, 1.0);
    (sat_fit * lum_fit * hue_fit).clamp(0.0, 1.0)
}

/// Skin Smoothing: frequency separation, on skin, on a face.
///
/// The smoothing is not the interesting part -- separate the colour from
/// the texture, blur the colour, mix a chosen amount of the texture back,
/// which is what a retoucher does by hand and is why it looks like skin
/// rather than like plastic. The interesting part is *where*, and that is
/// where the network earns its place: without it this is a skin-colour
/// test, which cannot tell a cheek from a hand, a leather chair or a
/// tanned wooden door.
pub struct SkinSmoothing {
    faces: Memo<Vec<Face>>,
}

impl SkinSmoothing {
    pub const fn new() -> SkinSmoothing {
        SkinSmoothing { faces: Memo::new() }
    }
}

impl Default for SkinSmoothing {
    fn default() -> Self {
        Self::new()
    }
}

impl FilterPlugin for SkinSmoothing {
    fn id(&self) -> &'static str {
        "filter.neural.skin_smoothing"
    }
    fn name(&self) -> &'static str {
        "Skin Smoothing"
    }
    fn category(&self) -> &'static str {
        "Neural Filters"
    }
    fn params(&self) -> Vec<FilterParam> {
        vec![
            param("blur", "Smoothness", 0.0, 100.0, 50.0, ""),
            param("detail", "Keep Detail", 0.0, 100.0, 40.0, ""),
        ]
    }

    fn info(&self) -> Option<String> {
        model_note(
            "face",
            "smoothing anything skin-coloured instead of the faces.",
        )
    }

    fn apply(&self, px: &mut [f32], width: usize, height: usize, values: &FilterValues) {
        if width == 0 || height == 0 {
            return;
        }
        let amount = values.get("blur") / 100.0;
        let detail = values.get("detail") / 100.0;
        if amount <= 0.0 {
            return;
        }

        // Faces, if there is a model to find them with. A model that runs
        // and finds nobody leaves `faces` empty, which is the same as no
        // model at all: smooth whatever is skin-coloured. Doing nothing
        // instead would be defensible and is what Photoshop does, but a
        // filter that silently declines is a worse thing to debug than
        // one that over-reaches.
        let faces = schist_neural::get("face").and_then(|model| {
            let key = fingerprint(px, width, height);
            self.faces.get(key, || {
                let rgb = rgb_of(px);
                schist_neural::faces(&model, &rgb, width, height)
                    .map_err(|e| log::warn!("face detection: {e:#}"))
                    .ok()
            })
        });
        let mask = faces
            .as_deref()
            .filter(|f| !f.is_empty())
            .map(|f| face_mask(f, width, height));

        // A big face needs a bigger blur than a small one to look the
        // same amount smoothed, so the radius follows the subject rather
        // than the pixel grid. 240 pixels across is the face this was
        // tuned on.
        let widest = faces
            .as_deref()
            .map_or(0.0, |f| f.iter().map(|f| f.width).fold(0.0, f32::max));
        let scale = if widest > 0.0 {
            (widest / 240.0).clamp(0.4, 4.0)
        } else {
            1.0
        };

        let src = px.to_vec();
        let mut low = px.to_vec();
        gaussian_rgba(&mut low, width, height, (3.0 + amount * 9.0) * scale);
        for i in 0..px.len() / 4 {
            let p = &src[i * 4..i * 4 + 4];
            let s = skinness(p) * amount * mask.as_ref().map_or(1.0, |m| m[i]);
            if s <= 0.0 {
                continue;
            }
            for c in 0..3 {
                let smoothed = low[i * 4 + c];
                let texture = p[c] - smoothed;
                let target = smoothed + texture * detail;
                px[i * 4 + c] = (p[c] + (target - p[c]) * s).clamp(0.0, 1.0);
            }
        }
    }
}

/// Where the faces are, as a mask that fades out at their edges.
///
/// An ellipse rather than the detector's box, because a face is one and
/// the corners of the box are hair, collar and background. The fade
/// matters more than the shape: a hard-edged mask would leave a visible
/// oval of smoothed skin with sharp skin around it.
fn face_mask(faces: &[Face], width: usize, height: usize) -> Vec<f32> {
    let mut mask = vec![0.0f32; width * height];
    for f in faces {
        // Slightly wider than the box: detectors crop tight, and the jaw
        // and forehead are skin too.
        let (rx, ry) = ((f.width * 0.72).max(1.0), (f.height * 0.80).max(1.0));
        let (cx, cy) = (f.x + f.width / 2.0, f.y + f.height / 2.0);
        let x0 = ((cx - rx).floor().max(0.0)) as usize;
        let y0 = ((cy - ry).floor().max(0.0)) as usize;
        let x1 = ((cx + rx).ceil().min(width as f32)) as usize;
        let y1 = ((cy + ry).ceil().min(height as f32)) as usize;
        for y in y0..y1 {
            for x in x0..x1 {
                let dx = (x as f32 + 0.5 - cx) / rx;
                let dy = (y as f32 + 0.5 - cy) / ry;
                let d = dx.hypot(dy);
                // Solid to seven tenths of the way out, gone by the edge.
                let v = ((1.0 - d) / 0.3).clamp(0.0, 1.0);
                let m = &mut mask[y * width + x];
                *m = m.max(v);
            }
        }
    }
    mask
}

/// JPEG Artifact Removal.
///
/// The network is the whole filter when it is there: `dejpeg.onnx` was
/// trained on the Kodak suite compressed at every quality from 10 to 60,
/// with the patches cut at unaligned offsets so it has to find the block
/// grid rather than assume where it is. The fallback below knows where
/// the grid *usually* is instead, which is most of the difference between
/// them.
pub struct JpegArtifactRemoval;

impl FilterPlugin for JpegArtifactRemoval {
    fn id(&self) -> &'static str {
        "filter.neural.jpeg_artifacts"
    }
    fn name(&self) -> &'static str {
        "JPEG Artifact Removal"
    }
    fn category(&self) -> &'static str {
        "Neural Filters"
    }
    fn params(&self) -> Vec<FilterParam> {
        vec![param("strength", "Strength", 0.0, 100.0, 60.0, "")]
    }

    fn info(&self) -> Option<String> {
        model_note("dejpeg", "smoothing the 8-pixel grid instead.")
    }

    fn apply(&self, px: &mut [f32], width: usize, height: usize, values: &FilterValues) {
        if width == 0 || height == 0 {
            return;
        }
        let strength = (values.get("strength") / 100.0).clamp(0.0, 1.0);
        if strength <= 0.0 {
            return;
        }
        if let Some(model) = schist_neural::get("dejpeg") {
            through_rgb(px, |rgb| {
                schist_neural::run_tiled(&model, rgb, width, height, strength);
            });
            return;
        }
        deblock(px, width, height, strength);
    }
}

/// The fallback: smooth *across the block boundaries* specifically.
///
/// JPEG artefacts are blocky, and the blocks land on the 8-pixel grid and
/// nowhere else, so pulling neighbours together only where they straddle
/// that grid leaves real edges alone wherever they happen to fall.
fn deblock(px: &mut [f32], w: usize, h: usize, strength: f32) {
    let src = px.to_vec();
    for y in 0..h {
        for x in 0..w {
            let on_v = x % 8 == 0 && x > 0;
            let on_h = y % 8 == 0 && y > 0;
            if !on_v && !on_h {
                continue;
            }
            let here = at(&src, w, h, x as i32, y as i32);
            let left = at(&src, w, h, x as i32 - 1, y as i32);
            let above = at(&src, w, h, x as i32, y as i32 - 1);
            let mut out = here;
            for (c, o) in out.iter_mut().enumerate().take(3) {
                let mut acc = 0.0;
                let mut n = 0.0;
                if on_v {
                    acc += left[c] + here[c];
                    n += 2.0;
                }
                if on_h {
                    acc += above[c] + here[c];
                    n += 2.0;
                }
                let mean = acc / n;
                // Only pull towards the mean when the step is small
                // enough to be an artefact rather than an edge.
                if (*o - mean).abs() < 0.12 {
                    *o += (mean - *o) * strength;
                }
            }
            put(px, w, x, y, out);
        }
    }
    // A gentle ringing clean-up inside the blocks.
    let mut low = px.to_vec();
    gaussian_rgba(&mut low, w, h, 0.8);
    for (p, l) in px
        .as_chunks_mut::<4>()
        .0
        .iter_mut()
        .zip(low.as_chunks::<4>().0.iter())
    {
        for c in 0..3 {
            if (p[c] - l[c]).abs() < 0.05 {
                p[c] += (l[c] - p[c]) * strength * 0.5;
            }
        }
    }
}

/// Colorize: put colour back into a photograph that has none.
///
/// The one filter here that cannot be done without a network, because
/// nothing in a greyscale photograph says the grass is green. The model
/// is given luminance and predicts chroma; the luminance is then
/// recombined with it untouched, so this cannot soften a picture even
/// when it is wrong about the colour.
///
/// It is 320k parameters trained on 20,000 photographs, so it is not
/// DeOldify. Expect it to be confident about sky, foliage, wood and skin,
/// and cautious -- which reads as desaturated -- about anything whose
/// colour is genuinely a choice, like a painted wall or a car.
pub struct Colorize {
    chroma: Memo<Vec<f32>>,
}

impl Colorize {
    pub const fn new() -> Colorize {
        Colorize {
            chroma: Memo::new(),
        }
    }
}

impl Default for Colorize {
    fn default() -> Self {
        Self::new()
    }
}

impl FilterPlugin for Colorize {
    fn id(&self) -> &'static str {
        "filter.neural.colorize"
    }
    fn name(&self) -> &'static str {
        "Colorize"
    }
    fn category(&self) -> &'static str {
        "Neural Filters"
    }
    fn params(&self) -> Vec<FilterParam> {
        vec![
            param("warmth", "Warmth", -100.0, 100.0, 0.0, ""),
            param("strength", "Strength", 0.0, 100.0, 70.0, ""),
        ]
    }

    fn info(&self) -> Option<String> {
        model_note("colorize", "tinting by luminance instead.")
    }

    fn apply(&self, px: &mut [f32], width: usize, height: usize, values: &FilterValues) {
        if width == 0 || height == 0 {
            return;
        }
        let warmth = values.get("warmth") / 100.0;
        let strength = (values.get("strength") / 100.0).clamp(0.0, 1.0);
        if strength <= 0.0 {
            return;
        }
        let predicted = schist_neural::get("colorize").and_then(|model| {
            let key = fingerprint(px, width, height);
            self.chroma.get(key, || {
                let rgb = rgb_of(px);
                schist_neural::chroma(&model, &rgb, width, height)
                    .map_err(|e| log::warn!("colorisation: {e:#}"))
                    .ok()
            })
        });
        let Some(chroma) = predicted else {
            tint_by_luminance(px, warmth, strength);
            return;
        };
        for (i, p) in px.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            // Warmth tilts the answer along the amber/blue axis, which is
            // the axis a photograph's white balance moves along and the
            // one the eye forgives being wrong about.
            let c = [
                chroma[i * 2] + warmth * 0.06,
                chroma[i * 2 + 1] - warmth * 0.06,
            ];
            let target = schist_neural::recolour(&[p[0], p[1], p[2]], c);
            for c in 0..3 {
                p[c] = (p[c] + (target[c] - p[c]) * strength).clamp(0.0, 1.0);
            }
        }
    }
}

/// The fallback: a luminance ramp, cool in the shadows and warm in the
/// highlights, which is what most daylight scenes actually do.
///
/// It does not recognise anything, so it will not make grass green. What
/// it will do is stop a greyscale photograph looking grey, which on a
/// portrait or a landscape at golden hour is a surprising amount of the
/// way there.
fn tint_by_luminance(px: &mut [f32], warmth: f32, strength: f32) {
    for p in px.as_chunks_mut::<4>().0.iter_mut() {
        let l = luma(p);
        let t = l * 2.0 - 1.0;
        let target = [
            (l + t * 0.10 * (1.0 + warmth)).clamp(0.0, 1.0),
            (l + t * 0.03).clamp(0.0, 1.0),
            (l - t * 0.10 * (1.0 + warmth)).clamp(0.0, 1.0),
        ];
        for c in 0..3 {
            p[c] = (p[c] + (target[c] - p[c]) * strength).clamp(0.0, 1.0);
        }
    }
}

/// Super Zoom: restore the detail an enlargement loses.
///
/// A filter cannot resize its own buffer, so this is the second half of
/// an upscale -- enlarge with Image Size, then run this to put the high
/// frequencies back.
pub struct SuperZoom;

impl FilterPlugin for SuperZoom {
    fn id(&self) -> &'static str {
        "filter.neural.super_zoom"
    }
    fn name(&self) -> &'static str {
        "Super Zoom"
    }
    fn category(&self) -> &'static str {
        "Neural Filters"
    }
    fn params(&self) -> Vec<FilterParam> {
        vec![param("detail", "Detail", 0.0, 100.0, 60.0, "")]
    }

    fn info(&self) -> Option<String> {
        model_note("detail", "using edge-directed sharpening instead.")
    }

    fn apply(&self, px: &mut [f32], width: usize, height: usize, values: &FilterValues) {
        if width == 0 || height == 0 {
            return;
        }
        let detail = (values.get("detail") / 100.0).clamp(0.0, 1.0);
        if detail <= 0.0 {
            return;
        }
        if let Some(model) = schist_neural::get("detail") {
            through_rgb(px, |rgb| {
                schist_neural::run_tiled(&model, rgb, width, height, detail);
            });
            return;
        }
        edge_directed_sharpen(px, width, height, detail);
    }
}

/// The fallback: sharpen along the gradient rather than across it, which
/// avoids the halos plain sharpening leaves on an already-soft image.
fn edge_directed_sharpen(px: &mut [f32], w: usize, h: usize, detail: f32) {
    let src = px.to_vec();
    for y in 0..h as i32 {
        for x in 0..w as i32 {
            let c0 = at(&src, w, h, x, y);
            let gx = luma(&at(&src, w, h, x + 1, y)) - luma(&at(&src, w, h, x - 1, y));
            let gy = luma(&at(&src, w, h, x, y + 1)) - luma(&at(&src, w, h, x, y - 1));
            let mag = gx.hypot(gy);
            if mag < 1e-4 {
                continue;
            }
            let (ux, uy) = (gx / mag, gy / mag);
            let a = sample(&src, w, h, x as f32 - ux, y as f32 - uy);
            let b = sample(&src, w, h, x as f32 + ux, y as f32 + uy);
            let mut out = c0;
            for c in 0..3 {
                let mid = (a[c] + b[c]) / 2.0;
                out[c] = (c0[c] + (c0[c] - mid) * detail * 1.5).clamp(0.0, 1.0);
            }
            put(px, w, x as usize, y as usize, out);
        }
    }
}

/// The styles this build knows about, in catalogue order.
const STYLES: &[&str] = &["Mosaic", "Candy", "Udnie"];
const STYLE_IDS: &[&str] = &["style-mosaic", "style-candy", "style-udnie"];

/// Style Transfer: repaint the image in a learned style.
///
/// There is no signal-processing stand-in for a brushstroke. Without the
/// model it does the colour half only, which is honest but is not the
/// same thing.
pub struct StyleTransfer;

impl FilterPlugin for StyleTransfer {
    fn id(&self) -> &'static str {
        "filter.neural.style_transfer"
    }
    fn name(&self) -> &'static str {
        "Style Transfer"
    }
    fn category(&self) -> &'static str {
        "Neural Filters"
    }
    fn params(&self) -> Vec<FilterParam> {
        vec![
            choice("style", "Style", STYLES, 0),
            param("strength", "Strength", 0.0, 100.0, 100.0, ""),
        ]
    }

    fn info(&self) -> Option<String> {
        // Report on whichever styles are present, since they install
        // separately.
        let ready: Vec<&str> = STYLE_IDS
            .iter()
            .enumerate()
            .filter(|(_, id)| schist_neural::installed(id))
            .map(|(i, _)| STYLES[i])
            .collect();
        Some(if ready.is_empty() {
            "No style models installed \u{2014} transferring colour only. \
             Get them from Filter \u{25b8} Neural Filters \u{25b8} Manage Models."
                .to_string()
        } else {
            format!(
                "Installed: {} (ONNX Model Zoo, Apache-2.0).",
                ready.join(", ")
            )
        })
    }

    fn apply(&self, px: &mut [f32], width: usize, height: usize, values: &FilterValues) {
        if width == 0 || height == 0 {
            return;
        }
        let strength = (values.get("strength") / 100.0).clamp(0.0, 1.0);
        if strength <= 0.0 {
            return;
        }
        let pick = (values.get("style").round().max(0.0) as usize).min(STYLE_IDS.len() - 1);
        if let Some(model) = schist_neural::get(STYLE_IDS[pick]) {
            through_rgb(px, |rgb| {
                schist_neural::run_tiled(&model, rgb, width, height, strength);
            });
            return;
        }
        // Colour-only fallback: push the image towards the style's
        // dominant hue. It is not style transfer and does not pretend to
        // be; `info` says so.
        let hue = [30.0f32, 340.0, 210.0][pick.min(2)].to_radians();
        colour_shift(px, hue, strength * 0.6);
    }
}

/// Push an image's mean chroma towards a hue.
fn colour_shift(px: &mut [f32], hue: f32, strength: f32) {
    let n = (px.len() / 4).max(1) as f32;
    let (mut ma, mut mb) = (0.0f32, 0.0f32);
    for p in px.as_chunks::<4>().0.iter() {
        let l = luma(p);
        ma += p[0] - l;
        mb += p[2] - l;
    }
    ma /= n;
    mb /= n;
    let (ta, tb) = (hue.cos() * 0.18, hue.sin() * 0.18);
    for p in px.as_chunks_mut::<4>().0.iter_mut() {
        let l = luma(p);
        let (ca, cb) = (p[0] - l, p[2] - l);
        let target = [
            (l + ca - ma + ta).clamp(0.0, 1.0),
            (l - ((ca - ma + ta) + (cb - mb + tb)) * 0.3).clamp(0.0, 1.0),
            (l + cb - mb + tb).clamp(0.0, 1.0),
        ];
        for c in 0..3 {
            p[c] += (target[c] - p[c]) * strength;
        }
    }
}

/// Colour Transfer: take another photograph's palette.
///
/// Photoshop's reads the palette from a reference image you choose. This
/// reads it from the layer underneath -- the one second image a filter
/// can be handed without a file picker -- and falls back to a hue you
/// pick when there is nothing under it.
///
/// The transfer itself is Reinhard: match the mean and spread of tone
/// and chroma. What Adobe's network adds is matching them *by subject*,
/// so that the reference's sky lands on your sky; this moves the whole
/// distribution at once, which is right for a mood and wrong for a
/// scene. Landscape Mixer is the one that splits it up.
pub struct ColorTransfer;

impl FilterPlugin for ColorTransfer {
    fn id(&self) -> &'static str {
        "filter.neural.color_transfer"
    }
    fn name(&self) -> &'static str {
        "Color Transfer"
    }
    fn category(&self) -> &'static str {
        "Neural Filters"
    }
    fn params(&self) -> Vec<FilterParam> {
        vec![
            param("hue", "Target Hue", 0.0, 360.0, 30.0, "\u{b0}"),
            param("strength", "Strength", 0.0, 100.0, 60.0, ""),
            param("contrast", "Match Contrast", 0.0, 100.0, 50.0, ""),
        ]
    }

    fn wants_backdrop(&self) -> bool {
        true
    }

    fn info(&self) -> Option<String> {
        Some(
            "Takes its palette from the layer underneath. With nothing \
             underneath it aims at the Target Hue instead."
                .to_string(),
        )
    }

    fn apply(&self, px: &mut [f32], width: usize, height: usize, values: &FilterValues) {
        self.apply_with(px, width, height, values, &FilterContext::default());
    }

    fn apply_with(
        &self,
        px: &mut [f32],
        width: usize,
        height: usize,
        values: &FilterValues,
        context: &FilterContext,
    ) {
        let backdrop = context.backdrop;
        if width == 0 || height == 0 {
            return;
        }
        let strength = (values.get("strength") / 100.0).clamp(0.0, 1.0);
        let match_contrast = (values.get("contrast") / 100.0).clamp(0.0, 1.0);
        if strength <= 0.0 {
            return;
        }
        if let Some(reference) = backdrop {
            if let (Some(mine), Some(theirs)) =
                (stats_of(px, |_| true), stats_of(reference, |_| true))
            {
                // Match Contrast decides whether the *spread* of tone
                // comes across as well as its middle: at zero the
                // picture keeps its own contrast and only borrows the
                // colour.
                let theirs = Stats {
                    mean: theirs.mean,
                    sd: [
                        mine.sd[0] + (theirs.sd[0] - mine.sd[0]) * match_contrast,
                        theirs.sd[1],
                        theirs.sd[2],
                    ],
                };
                for p in px.as_chunks_mut::<4>().0.iter_mut() {
                    restat(p, mine, theirs, strength);
                }
                return;
            }
        }
        // No reference: aim at the hue instead. Shift the image's mean
        // chroma towards it and optionally normalise its spread.
        let hue = values.get("hue").to_radians();
        let n = (px.len() / 4).max(1) as f32;
        let (mut mean_l, mut mean_a, mut mean_b) = (0.0f32, 0.0f32, 0.0f32);
        for p in px.as_chunks::<4>().0.iter() {
            let l = luma(p);
            mean_l += l;
            mean_a += p[0] - l;
            mean_b += p[2] - l;
        }
        mean_l /= n;
        mean_a /= n;
        mean_b /= n;
        let mut var_l = 0.0f32;
        for p in px.as_chunks::<4>().0.iter() {
            var_l += (luma(p) - mean_l).powi(2);
        }
        let sd_l = (var_l / n).sqrt().max(1e-4);
        let (ta, tb) = (hue.cos() * 0.18, hue.sin() * 0.18);
        let gain = 1.0 + match_contrast * (0.25 / sd_l - 1.0).clamp(-0.5, 0.5);
        for p in px.as_chunks_mut::<4>().0.iter_mut() {
            let l = luma(p);
            let (ca, cb) = (p[0] - l, p[2] - l);
            let l2 = mean_l + (l - mean_l) * gain;
            let na = ca - mean_a + ta;
            let nb = cb - mean_b + tb;
            let target = [
                (l2 + na).clamp(0.0, 1.0),
                (l2 - (na + nb) * 0.3).clamp(0.0, 1.0),
                (l2 + nb).clamp(0.0, 1.0),
            ];
            for c in 0..3 {
                p[c] = (p[c] + (target[c] - p[c]) * strength).clamp(0.0, 1.0);
            }
        }
    }
}

/// Depth Blur: throw the background out of focus.
///
/// With the model this is a real defocus: MiDaS estimates how far away
/// everything in the photograph is, and the blur follows how far each
/// region sits from the focal distance -- so a face stays sharp while the
/// street behind it goes, and the near foreground goes too, which is what
/// a wide aperture actually does and what a background-only blur cannot.
pub struct DepthBlur {
    depth: Memo<Vec<f32>>,
}

impl DepthBlur {
    pub const fn new() -> DepthBlur {
        DepthBlur { depth: Memo::new() }
    }
}

impl Default for DepthBlur {
    fn default() -> Self {
        Self::new()
    }
}

impl FilterPlugin for DepthBlur {
    fn id(&self) -> &'static str {
        "filter.neural.depth_blur"
    }
    fn name(&self) -> &'static str {
        "Depth Blur"
    }
    fn category(&self) -> &'static str {
        "Neural Filters"
    }
    fn params(&self) -> Vec<FilterParam> {
        vec![
            param("focus", "Focal Distance", 0.0, 100.0, 50.0, ""),
            param("blur", "Blur Strength", 0.0, 100.0, 50.0, ""),
        ]
    }

    fn info(&self) -> Option<String> {
        model_note("depth", "focusing by local sharpness instead.")
    }

    fn apply(&self, px: &mut [f32], width: usize, height: usize, values: &FilterValues) {
        if width == 0 || height == 0 {
            return;
        }
        // The slider reads as a distance, so 0 is the nearest thing in
        // the picture; the map reads the other way round, 1 being near.
        let focus = 1.0 - values.get("focus") / 100.0;
        let strength = (values.get("blur") / 100.0).clamp(0.0, 1.0);
        if strength <= 0.0 {
            return;
        }
        let estimated = schist_neural::get("depth").and_then(|model| {
            let key = fingerprint(px, width, height);
            self.depth.get(key, || {
                let rgb = rgb_of(px);
                schist_neural::depth_map(&model, &rgb, width, height)
                    .map_err(|e| log::warn!("depth estimation: {e:#}"))
                    .ok()
            })
        });
        let plane = estimated.unwrap_or_else(|| Arc::new(acuity(px, width, height)));
        defocus(px, width, height, &plane, focus, strength);
    }
}

/// The fallback for a depth map: local sharpness.
///
/// Areas that are already detailed read as near and flat ones as far,
/// which is true of a photograph taken with a shallow depth of field and
/// a guess anywhere else. It is a photographic effect rather than a depth
/// map, and it behaves like one.
fn acuity(px: &[f32], w: usize, h: usize) -> Vec<f32> {
    let mut low = px.to_vec();
    gaussian_rgba(&mut low, w, h, 6.0);
    let mut near = vec![0.0f32; w * h];
    for (i, n) in near.iter_mut().enumerate() {
        let d = (0..3)
            .map(|c| (px[i * 4 + c] - low[i * 4 + c]).abs())
            .fold(0.0f32, f32::max);
        *n = (d * 12.0).clamp(0.0, 1.0);
    }
    // Smooth it, so the blur varies over regions rather than pixels.
    let mut plane: Vec<f32> = near.iter().flat_map(|a| [*a, *a, *a, 1.0]).collect();
    gaussian_rgba(&mut plane, w, h, 20.0);
    plane.as_chunks::<4>().0.iter().map(|p| p[0]).collect()
}

/// Blur each pixel by how far its own distance is from the focal one.
///
/// One blurred copy, mixed in per pixel, rather than a different radius
/// everywhere: a true variable-radius blur costs the largest radius over
/// the whole image and this is a lens effect, not a measurement.
fn defocus(px: &mut [f32], w: usize, h: usize, plane: &[f32], focus: f32, strength: f32) {
    let src = px.to_vec();
    let mut blurred = src.clone();
    gaussian_rgba(&mut blurred, w, h, 2.0 + strength * 14.0);
    for i in 0..w * h {
        let k = ((plane[i] - focus).abs() * 2.0).clamp(0.0, 1.0) * strength;
        for c in 0..3 {
            px[i * 4 + c] = src[i * 4 + c] + (blurred[i * 4 + c] - src[i * 4 + c]) * k;
        }
    }
}

/// Mean and spread of a buffer's luminance and chroma.
///
/// The currency of every "make this look like that" filter: two images
/// match when these six numbers match, which is Reinhard et al.'s
/// observation and is why colour transfer is arithmetic rather than
/// magic.
#[derive(Clone, Copy)]
struct Stats {
    mean: [f32; 3],
    sd: [f32; 3],
}

fn stats_of(px: &[f32], mask: impl Fn(usize) -> bool) -> Option<Stats> {
    let (mut sum, mut n) = ([0.0f32; 3], 0.0f32);
    for (i, p) in px.as_chunks::<4>().0.iter().enumerate() {
        if p[3] <= 0.01 || !mask(i) {
            continue;
        }
        let l = luma(p);
        sum[0] += l;
        sum[1] += p[0] - l;
        sum[2] += p[2] - l;
        n += 1.0;
    }
    if n < 8.0 {
        return None;
    }
    let mean = [sum[0] / n, sum[1] / n, sum[2] / n];
    let mut var = [0.0f32; 3];
    for (i, p) in px.as_chunks::<4>().0.iter().enumerate() {
        if p[3] <= 0.01 || !mask(i) {
            continue;
        }
        let l = luma(p);
        let v = [l - mean[0], (p[0] - l) - mean[1], (p[2] - l) - mean[2]];
        for c in 0..3 {
            var[c] += v[c] * v[c];
        }
    }
    Some(Stats {
        mean,
        sd: [
            (var[0] / n).sqrt().max(1e-4),
            (var[1] / n).sqrt().max(1e-4),
            (var[2] / n).sqrt().max(1e-4),
        ],
    })
}

/// Move a pixel from one distribution to another.
fn restat(p: &mut [f32], from: Stats, to: Stats, amount: f32) {
    let l = luma(p);
    let (ca, cb) = (p[0] - l, p[2] - l);
    let moved = [
        (l - from.mean[0]) / from.sd[0] * to.sd[0] + to.mean[0],
        (ca - from.mean[1]) / from.sd[1] * to.sd[1] + to.mean[1],
        (cb - from.mean[2]) / from.sd[2] * to.sd[2] + to.mean[2],
    ];
    let target = schist_neural::recolour(&[moved[0], moved[0], moved[0]], [moved[1], moved[2]]);
    for c in 0..3 {
        p[c] = (p[c] + (target[c] - p[c]) * amount).clamp(0.0, 1.0);
    }
}

/// Harmonization: make this layer look like it belongs on the one below.
///
/// Photoshop's asks you to pick the layer to match; this takes the one
/// thing a filter can be handed without a file picker -- what the
/// document composites to underneath -- which is the same answer for the
/// case the filter exists for, a cut-out pasted onto a background.
///
/// The matching itself is Reinhard: move the layer's tone and colour
/// distribution onto the backdrop's. What Adobe's network adds is knowing
/// *which parts* correspond, which is the part this cannot do and does
/// not claim to.
pub struct Harmonization;

impl FilterPlugin for Harmonization {
    fn id(&self) -> &'static str {
        "filter.neural.harmonization"
    }
    fn name(&self) -> &'static str {
        "Harmonization"
    }
    fn category(&self) -> &'static str {
        "Neural Filters"
    }
    fn params(&self) -> Vec<FilterParam> {
        vec![
            param("strength", "Strength", 0.0, 100.0, 75.0, ""),
            param("tone", "Match Tone", 0.0, 100.0, 100.0, ""),
        ]
    }

    fn wants_backdrop(&self) -> bool {
        true
    }

    fn info(&self) -> Option<String> {
        Some(
            "Matches this layer to whatever is underneath it. With nothing \
             underneath there is nothing to match to and this does nothing."
                .to_string(),
        )
    }

    fn apply(&self, px: &mut [f32], width: usize, height: usize, values: &FilterValues) {
        self.apply_with(px, width, height, values, &FilterContext::default());
    }

    fn apply_with(
        &self,
        px: &mut [f32],
        width: usize,
        height: usize,
        values: &FilterValues,
        context: &FilterContext,
    ) {
        let backdrop = context.backdrop;
        if width == 0 || height == 0 {
            return;
        }
        let strength = (values.get("strength") / 100.0).clamp(0.0, 1.0);
        let tone = (values.get("tone") / 100.0).clamp(0.0, 1.0);
        let Some(backdrop) = backdrop else { return };
        let (Some(mine), Some(theirs)) = (stats_of(px, |_| true), stats_of(backdrop, |_| true))
        else {
            return;
        };
        // Match Tone at zero leaves the luminance alone and moves only
        // the colour, which is what you want when the cut-out is lit
        // correctly and merely the wrong temperature.
        let theirs = Stats {
            mean: [
                mine.mean[0] + (theirs.mean[0] - mine.mean[0]) * tone,
                theirs.mean[1],
                theirs.mean[2],
            ],
            sd: [
                mine.sd[0] + (theirs.sd[0] - mine.sd[0]) * tone,
                theirs.sd[1],
                theirs.sd[2],
            ],
        };
        for p in px.as_chunks_mut::<4>().0.iter_mut() {
            restat(p, mine, theirs, strength);
        }
    }
}

/// Landscape Mixer: take the season, the hour and the weather from
/// another photograph.
///
/// Photoshop's generates the new landscape outright. This one moves
/// colour, and moves it *by distance*: sky matched to sky, ground matched
/// to ground, because a landscape's palette is stratified by depth and a
/// single global match turns the grass the colour of the sky. The depth
/// model is what splits the bands; without it the match is global, and
/// the dialog says so.
pub struct LandscapeMixer {
    depth: Memo<Vec<f32>>,
    reference: Memo<Vec<f32>>,
}

impl LandscapeMixer {
    pub const fn new() -> LandscapeMixer {
        LandscapeMixer {
            depth: Memo::new(),
            reference: Memo::new(),
        }
    }
}

impl Default for LandscapeMixer {
    fn default() -> Self {
        Self::new()
    }
}

impl FilterPlugin for LandscapeMixer {
    fn id(&self) -> &'static str {
        "filter.neural.landscape_mixer"
    }
    fn name(&self) -> &'static str {
        "Landscape Mixer"
    }
    fn category(&self) -> &'static str {
        "Neural Filters"
    }
    fn params(&self) -> Vec<FilterParam> {
        vec![
            param("strength", "Strength", 0.0, 100.0, 70.0, ""),
            param("bands", "Depth Bands", 1.0, 5.0, 3.0, ""),
        ]
    }

    fn wants_backdrop(&self) -> bool {
        true
    }

    fn info(&self) -> Option<String> {
        Some(if schist_neural::installed("depth") {
            "Takes its palette from the layer underneath, matched band by \
             band using Depth (Depth Blur)."
                .to_string()
        } else {
            "Takes its palette from the layer underneath. Install Depth \
             (Depth Blur) to match sky to sky and ground to ground rather \
             than the picture as a whole."
                .to_string()
        })
    }

    fn apply(&self, px: &mut [f32], width: usize, height: usize, values: &FilterValues) {
        self.apply_with(px, width, height, values, &FilterContext::default());
    }

    fn apply_with(
        &self,
        px: &mut [f32],
        width: usize,
        height: usize,
        values: &FilterValues,
        context: &FilterContext,
    ) {
        let backdrop = context.backdrop;
        if width == 0 || height == 0 {
            return;
        }
        let strength = (values.get("strength") / 100.0).clamp(0.0, 1.0);
        let bands = (values.get("bands").round().max(1.0) as usize).min(5);
        let Some(backdrop) = backdrop else { return };

        // Depth for both pictures, if there is a model. Two memos: the
        // reference does not change while the sliders move either.
        let model = schist_neural::get("depth");
        let depth_of = |memo: &Memo<Vec<f32>>, buf: &[f32]| -> Option<Arc<Vec<f32>>> {
            let model = model.clone()?;
            let key = fingerprint(buf, width, height);
            memo.get(key, || {
                let rgb = rgb_of(buf);
                schist_neural::depth_map(&model, &rgb, width, height)
                    .map_err(|e| log::warn!("landscape depth: {e:#}"))
                    .ok()
            })
        };
        let mine = depth_of(&self.depth, px);
        let theirs = depth_of(&self.reference, backdrop);

        for band in 0..bands {
            let lo = band as f32 / bands as f32;
            let hi = (band + 1) as f32 / bands as f32;
            // Without depth there is one band covering everything, which
            // is an ordinary colour transfer.
            let in_band = |map: &Option<Arc<Vec<f32>>>, i: usize| -> bool {
                match map {
                    Some(d) => d[i] >= lo && (d[i] < hi || hi >= 1.0),
                    None => band == 0,
                }
            };
            let (Some(from), Some(to)) = (
                stats_of(px, |i| in_band(&mine, i)),
                stats_of(backdrop, |i| in_band(&theirs, i)),
            ) else {
                continue;
            };
            for (i, p) in px.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                if !in_band(&mine, i) {
                    continue;
                }
                restat(p, from, to, strength);
            }
        }
    }
}

/// Photo Restoration: an old photograph, cleaned up.
///
/// Adobe's is one network doing everything. This is the set of things
/// that network is doing, done separately and in the order a restorer
/// would: take the scratches out, take the grain and the compression
/// out, put the detail back, and open the tones up again. Two of those
/// steps are models this build already ships, which is why this filter
/// exists here at all -- it is mostly composition.
pub struct PhotoRestoration;

impl FilterPlugin for PhotoRestoration {
    fn id(&self) -> &'static str {
        "filter.neural.photo_restoration"
    }
    fn name(&self) -> &'static str {
        "Photo Restoration"
    }
    fn category(&self) -> &'static str {
        "Neural Filters"
    }
    fn params(&self) -> Vec<FilterParam> {
        vec![
            param("enhance", "Photo Enhancement", 0.0, 100.0, 50.0, ""),
            param("scratches", "Scratch Reduction", 0.0, 100.0, 30.0, ""),
            param("tone", "Restore Tone", 0.0, 100.0, 60.0, ""),
        ]
    }

    fn info(&self) -> Option<String> {
        let mut have: Vec<&str> = Vec::new();
        if schist_neural::installed("dejpeg") {
            have.push("Deblock");
        }
        if schist_neural::installed("detail") {
            have.push("Detail");
        }
        Some(if have.is_empty() {
            "Cleaning up without a model.".to_string()
        } else {
            format!("Using {} \u{b7} trained for Schist.", have.join(" and "))
        })
    }

    fn apply(&self, px: &mut [f32], width: usize, height: usize, values: &FilterValues) {
        if width == 0 || height == 0 {
            return;
        }
        let enhance = (values.get("enhance") / 100.0).clamp(0.0, 1.0);
        let scratches = (values.get("scratches") / 100.0).clamp(0.0, 1.0);
        let tone = (values.get("tone") / 100.0).clamp(0.0, 1.0);

        if scratches > 0.0 {
            despeckle(px, width, height, scratches);
        }
        if enhance > 0.0 {
            // Grain first, then the deblocker for what a scan of a print
            // puts in, then the detail model to put back the edges that
            // removing them cost. In that order: sharpening before
            // denoising sharpens the noise.
            denoise(px, width, height, enhance);
            if let Some(model) = schist_neural::get("dejpeg") {
                through_rgb(px, |rgb| {
                    schist_neural::run_tiled(&model, rgb, width, height, enhance);
                });
            }
            if let Some(model) = schist_neural::get("detail") {
                through_rgb(px, |rgb| {
                    schist_neural::run_tiled(&model, rgb, width, height, enhance * 0.5);
                });
            } else {
                edge_directed_sharpen(px, width, height, enhance * 0.5);
            }
        }
        if tone > 0.0 {
            restore_tone(px, tone);
        }
    }
}

/// Take out the specks and hairline scratches a print picks up.
///
/// A pixel that disagrees with the *median* of its neighbours is damage;
/// one that disagrees with their mean might merely be an edge. The
/// distinction matters here more than anywhere else in the filter set,
/// because a scratch is a thin bright line and so is a highlight on a
/// wire. The radius follows the slider: a speck needs one pixel of
/// context to be outvoted, a hairline scratch needs three.
fn despeckle(px: &mut [f32], w: usize, h: usize, amount: f32) {
    let src = px.to_vec();
    let radius = 1 + (amount * 2.5) as i32;
    let threshold = 0.16 - amount * 0.1;
    let mut ring: Vec<f32> = Vec::with_capacity(((2 * radius + 1) * (2 * radius + 1)) as usize);
    for y in 0..h as i32 {
        for x in 0..w as i32 {
            let here = at(&src, w, h, x, y);
            let mut out = here;
            let mut damaged = 0.0f32;
            for c in 0..3 {
                ring.clear();
                for dy in -radius..=radius {
                    for dx in -radius..=radius {
                        if dx == 0 && dy == 0 {
                            continue;
                        }
                        ring.push(at(&src, w, h, x + dx, y + dy)[c]);
                    }
                }
                ring.sort_by(f32::total_cmp);
                let median = ring[ring.len() / 2];
                damaged = damaged.max((here[c] - median).abs());
                out[c] = median;
            }
            if damaged > threshold {
                let mix = ((damaged - threshold) * 8.0).clamp(0.0, 1.0) * amount;
                for c in 0..3 {
                    out[c] = here[c] + (out[c] - here[c]) * mix;
                }
                put(px, w, x as usize, y as usize, out);
            }
        }
    }
}

/// Take the grain out without taking the picture with it.
///
/// A print that has been scanned carries the film's grain, the paper's
/// texture and the scanner's own noise, none of which the detail model
/// should be asked to sharpen. Smoothing only where the difference is
/// small enough to be noise is the cheapest thing that works.
fn denoise(px: &mut [f32], w: usize, h: usize, amount: f32) {
    let mut soft = px.to_vec();
    gaussian_rgba(&mut soft, w, h, 0.6 + amount * 1.2);
    let threshold = 0.04 + amount * 0.06;
    for (p, s) in px
        .as_chunks_mut::<4>()
        .0
        .iter_mut()
        .zip(soft.as_chunks::<4>().0.iter())
    {
        for c in 0..3 {
            let d = (p[c] - s[c]).abs();
            if d < threshold {
                // Fully towards the smooth version for the finest
                // differences, tapering off as they start to look like
                // something that was in the room.
                let mix = (1.0 - d / threshold) * amount;
                p[c] += (s[c] - p[c]) * mix;
            }
        }
    }
}

/// Open the tones back up: an old print has faded towards its middle.
fn restore_tone(px: &mut [f32], amount: f32) {
    let n = (px.len() / 4).max(1) as f32;
    let (mut lo, mut hi, mut mean) = (1.0f32, 0.0f32, 0.0f32);
    for p in px.as_chunks::<4>().0.iter() {
        let l = luma(p);
        lo = lo.min(l);
        hi = hi.max(l);
        mean += l;
    }
    mean /= n;
    let span = (hi - lo).max(1e-3);
    for p in px.as_chunks_mut::<4>().0.iter_mut() {
        for v in p.iter_mut().take(3) {
            // Stretch to the full range, and take the sepia out by
            // nudging each channel towards where the middle should be.
            let stretched = ((*v - lo) / span).clamp(0.0, 1.0);
            let neutral = stretched + (mean - (lo + hi) / 2.0) * 0.15;
            *v = (*v + (neutral - *v) * amount).clamp(0.0, 1.0);
        }
    }
}

simple_filter!(
    PhotoToSketch,
    "filter.neural.photo_to_sketch",
    "Photo to Sketch",
    "Neural Filters",
    [
        param("detail", "Detail", 0.0, 100.0, 50.0, ""),
        param("weight", "Line Weight", 0.0, 100.0, 50.0, ""),
        param("shading", "Shading", 0.0, 100.0, 40.0, "")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // The pencil-sketch construction every drawing tutorial teaches,
        // because it is the one that works: invert the picture, blur it,
        // and divide the original by it. Where the two agree the quotient
        // saturates to white and the paper is left blank; where an edge
        // makes them disagree, a line appears exactly as wide as the
        // blur.
        let detail = v.get("detail") / 100.0;
        let weight = v.get("weight") / 100.0;
        let shading = v.get("shading") / 100.0;
        let plane = crate::util::luma_map(px, w, h);
        let mut soft: Vec<f32> = plane.iter().map(|l| 1.0 - l).collect();
        crate::util::blur_plane(&mut soft, w, h, 1.0 + (1.0 - detail) * 12.0);
        let mut out = vec![0.0f32; w * h];
        for i in 0..w * h {
            let dodge = (plane[i] / (1.0 - soft[i]).max(1e-3)).min(1.0);
            // Line Weight decides how dark a disagreement has to be
            // before it counts as a line.
            let line = 1.0 - ((1.0 - dodge) * (0.5 + weight * 3.0)).min(1.0);
            // Shading lays the original tone back underneath, which is
            // the difference between a line drawing and a pencil
            // rendering.
            out[i] = (line - (1.0 - plane[i]) * shading * 0.6).clamp(0.0, 1.0);
        }
        crate::util::from_luma(px, &out, 0.0);
    }
);

/// Face to Caricature: exaggerate what is already there.
///
/// Adobe's redraws the face outright. This one warps it, and warps it
/// from the detector's box plus the proportions a face has when it is
/// looking at the camera -- eyes a little above the middle, mouth three
/// quarters down. So it does not *find* the features, it assumes where
/// they usually are, which is why it works on a portrait and falls apart
/// on a profile. Without the face model it has nothing to work from and
/// does nothing.
pub struct FaceToCaricature {
    faces: Memo<Vec<Face>>,
}

impl FaceToCaricature {
    pub const fn new() -> FaceToCaricature {
        FaceToCaricature { faces: Memo::new() }
    }
}

impl Default for FaceToCaricature {
    fn default() -> Self {
        Self::new()
    }
}

impl FilterPlugin for FaceToCaricature {
    fn id(&self) -> &'static str {
        "filter.neural.face_to_caricature"
    }
    fn name(&self) -> &'static str {
        "Face to Caricature"
    }
    fn category(&self) -> &'static str {
        "Neural Filters"
    }
    fn params(&self) -> Vec<FilterParam> {
        vec![
            param("eyes", "Eyes", -100.0, 100.0, 60.0, ""),
            param("mouth", "Mouth", -100.0, 100.0, 40.0, ""),
            param("head", "Head", -100.0, 100.0, 25.0, ""),
        ]
    }

    fn info(&self) -> Option<String> {
        model_note(
            "face",
            "so this does nothing \u{2014} there is nothing to caricature.",
        )
    }

    fn apply(&self, px: &mut [f32], width: usize, height: usize, values: &FilterValues) {
        if width == 0 || height == 0 {
            return;
        }
        let eyes = values.get("eyes") / 100.0;
        let mouth = values.get("mouth") / 100.0;
        let head = values.get("head") / 100.0;
        let Some(model) = schist_neural::get("face") else {
            return;
        };
        let key = fingerprint(px, width, height);
        let faces = self.faces.get(key, || {
            let rgb = rgb_of(px);
            schist_neural::faces(&model, &rgb, width, height)
                .map_err(|e| log::warn!("caricature: {e:#}"))
                .ok()
        });
        let Some(faces) = faces else { return };
        if faces.is_empty() {
            return;
        }
        // Each feature is a bubble in the coordinate map: pull the
        // sampling point towards a centre to enlarge what is there, push
        // it away to shrink it.
        let mut pulls: Vec<(f32, f32, f32, f32)> = Vec::new();
        for f in faces.iter() {
            let (cx, cy) = (f.x + f.width / 2.0, f.y + f.height / 2.0);
            let eye_y = f.y + f.height * 0.40;
            let mouth_y = f.y + f.height * 0.75;
            let span = f.width.max(1.0);
            pulls.push((cx - span * 0.22, eye_y, span * 0.30, eyes));
            pulls.push((cx + span * 0.22, eye_y, span * 0.30, eyes));
            pulls.push((cx, mouth_y, span * 0.32, mouth));
            pulls.push((cx, cy, span * 0.95, head));
        }
        warp(px, width, height, |x, y| {
            let (mut sx, mut sy) = (x, y);
            for (fx, fy, radius, amount) in pulls.iter() {
                let (dx, dy) = (x - fx, y - fy);
                let d = dx.hypot(dy) / radius.max(1.0);
                if d >= 1.0 {
                    continue;
                }
                // A smooth bubble, strongest in the middle and zero at
                // the rim, so the rest of the face is untouched and there
                // is no seam.
                let falloff = (1.0 - d * d).powi(2);
                let scale = 1.0 - amount * 0.45 * falloff;
                sx = fx + (sx - fx) * scale;
                sy = fy + (sy - fy) * scale;
            }
            (sx, sy)
        });
    }
}

pub fn register(registry: &mut schist_plugin_api::PluginRegistry) {
    registry.register_filter(Box::new(SkinSmoothing::new()));
    registry.register_filter(Box::new(JpegArtifactRemoval));
    registry.register_filter(Box::new(Colorize::new()));
    registry.register_filter(Box::new(SuperZoom));
    registry.register_filter(Box::new(StyleTransfer));
    registry.register_filter(Box::new(ColorTransfer));
    registry.register_filter(Box::new(DepthBlur::new()));
    registry.register_filter(Box::new(Harmonization));
    registry.register_filter(Box::new(LandscapeMixer::new()));
    registry.register_filter(Box::new(PhotoRestoration));
    registry.register_filter(Box::new(PhotoToSketch));
    registry.register_filter(Box::new(FaceToCaricature::new()));
}
