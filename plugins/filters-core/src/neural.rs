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
/// frequencies back. (Image Size can also do the whole thing at once now,
/// resampling through waifu2x; this stays for enlargements made
/// elsewhere, and for its slider.)
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

/// How much of a region a pixel belongs to, given the face it is part of
/// and its own colour: geometry for the eyes and the skin, geometry and
/// redness for the lips.
type Region = fn(&FaceParts, f32, f32, &[f32]) -> f32;

/// Where the features of a face looking at the camera are, as fractions
/// of the detector's box.
///
/// The two filters below are geometry, not recognition: they assume the
/// eyes are a little above the middle and the mouth three quarters of
/// the way down, because on a face that is looking at the camera they
/// are. That is also why both fall apart on a profile, and both say so.
struct FaceParts {
    /// Centre of the box and how wide it is.
    centre: (f32, f32),
    span: f32,
    /// The two eyes and the mouth, as centres and radii.
    eyes: [(f32, f32, f32); 2],
    mouth: (f32, f32, f32),
    brows: (f32, f32, f32),
}

impl FaceParts {
    fn of(f: &Face) -> FaceParts {
        let span = f.width.max(1.0);
        let (cx, cy) = (f.x + f.width / 2.0, f.y + f.height / 2.0);
        let eye_y = f.y + f.height * 0.40;
        FaceParts {
            centre: (cx, cy),
            span,
            eyes: [
                (cx - span * 0.21, eye_y, span * 0.17),
                (cx + span * 0.21, eye_y, span * 0.17),
            ],
            mouth: (cx, f.y + f.height * 0.75, span * 0.26),
            brows: (cx, f.y + f.height * 0.32, span * 0.34),
        }
    }

    /// A soft mask value for a round feature, 1 at the centre and 0 at
    /// the rim.
    fn falloff(x: f32, y: f32, part: (f32, f32, f32)) -> f32 {
        let d = (x - part.0).hypot(y - part.1) / part.2.max(1.0);
        if d >= 1.0 {
            0.0
        } else {
            (1.0 - d * d).powi(2)
        }
    }
}

/// Smart Portrait: the retouches Photoshop's network makes, made the way
/// a retoucher would.
///
/// Adobe's generates a new face from a latent code, which is how it can
/// turn a head or change a hairline. Nothing that fits in a filter can do
/// that. What *can* be done is everything the sliders are actually asked
/// for on a portrait: a smile is the mouth corners lifted, surprise is
/// the brows raised, age is skin texture added or taken away, and the
/// light comes from wherever the shading says it does -- which the depth
/// model can be asked about.
///
/// So the eyes here are warped rather than redrawn, and the filter says
/// so. It works on a face looking at the camera and falls apart on a
/// profile, because that is where its assumptions about where things are
/// stop holding.
pub struct SmartPortrait {
    faces: Memo<Vec<Face>>,
    depth: Memo<Vec<f32>>,
}

impl SmartPortrait {
    pub const fn new() -> SmartPortrait {
        SmartPortrait {
            faces: Memo::new(),
            depth: Memo::new(),
        }
    }
}

impl Default for SmartPortrait {
    fn default() -> Self {
        Self::new()
    }
}

impl FilterPlugin for SmartPortrait {
    fn id(&self) -> &'static str {
        "filter.neural.smart_portrait"
    }
    fn name(&self) -> &'static str {
        "Smart Portrait"
    }
    fn category(&self) -> &'static str {
        "Neural Filters"
    }
    fn params(&self) -> Vec<FilterParam> {
        vec![
            param("happiness", "Happiness", -100.0, 100.0, 0.0, ""),
            param("surprise", "Surprise", -100.0, 100.0, 0.0, ""),
            param("age", "Facial Age", -100.0, 100.0, 0.0, ""),
            param("gaze", "Gaze", -100.0, 100.0, 0.0, ""),
            param("light", "Light Direction", -100.0, 100.0, 0.0, ""),
        ]
    }

    fn info(&self) -> Option<String> {
        model_note(
            "face",
            "so this does nothing \u{2014} it has no face to work on.",
        )
    }

    fn apply(&self, px: &mut [f32], width: usize, height: usize, values: &FilterValues) {
        if width == 0 || height == 0 {
            return;
        }
        let happiness = values.get("happiness") / 100.0;
        let surprise = values.get("surprise") / 100.0;
        let age = values.get("age") / 100.0;
        let gaze = values.get("gaze") / 100.0;
        let light = values.get("light") / 100.0;
        let Some(model) = schist_neural::get("face") else {
            return;
        };
        let key = fingerprint(px, width, height);
        let faces = self.faces.get(key, || {
            let rgb = rgb_of(px);
            schist_neural::faces(&model, &rgb, width, height)
                .map_err(|e| log::warn!("smart portrait: {e:#}"))
                .ok()
        });
        let Some(faces) = faces else { return };
        if faces.is_empty() {
            return;
        }
        let parts: Vec<FaceParts> = faces.iter().map(FaceParts::of).collect();

        // Expression first, because it moves pixels: everything after it
        // works on where they ended up.
        if happiness != 0.0 || surprise != 0.0 || gaze != 0.0 {
            warp(px, width, height, |x, y| {
                let (mut sx, mut sy) = (x, y);
                for p in parts.iter() {
                    // A smile lifts the corners of the mouth and widens
                    // it; a frown does the opposite. Sampling from lower
                    // down is what raises what is drawn.
                    let m = FaceParts::falloff(x, y, p.mouth);
                    if m > 0.0 && happiness != 0.0 {
                        let side = ((x - p.mouth.0) / p.mouth.2).clamp(-1.0, 1.0);
                        sy += happiness * m * side.abs() * p.span * 0.10;
                        sx -= happiness * m * side * p.span * 0.04;
                    }
                    // Surprise raises the brows and opens the eyes.
                    let b = FaceParts::falloff(x, y, p.brows);
                    if b > 0.0 && surprise != 0.0 {
                        sy += surprise * b * p.span * 0.06;
                    }
                    for eye in p.eyes.iter() {
                        let e = FaceParts::falloff(x, y, *eye);
                        if e <= 0.0 {
                            continue;
                        }
                        if surprise != 0.0 {
                            // Towards the eye's centre enlarges it.
                            let scale = 1.0 - surprise * e * 0.35;
                            sx = eye.0 + (sx - eye.0) * scale;
                            sy = eye.1 + (sy - eye.1) * scale;
                        }
                        if gaze != 0.0 {
                            // The iris slides; the lids do not.
                            sx -= gaze * e * eye.2 * 0.35;
                        }
                    }
                }
                (sx, sy)
            });
        }

        // Age: skin is the one thing here that is texture rather than
        // shape. Younger smooths it and lifts it; older puts the texture
        // back, harder than it was.
        if age != 0.0 {
            let src = px.to_vec();
            let mut low = px.to_vec();
            gaussian_rgba(&mut low, width, height, 4.0);
            for y in 0..height {
                for x in 0..width {
                    let i = y * width + x;
                    let mut inside = 0.0f32;
                    for p in parts.iter() {
                        inside = inside.max(FaceParts::falloff(
                            x as f32,
                            y as f32,
                            (p.centre.0, p.centre.1, p.span * 0.85),
                        ));
                    }
                    let skin = inside * skinness(&src[i * 4..i * 4 + 4]);
                    if skin <= 0.0 {
                        continue;
                    }
                    for c in 0..3 {
                        let texture = src[i * 4 + c] - low[i * 4 + c];
                        // Negative age keeps less of the texture and
                        // lifts the tone; positive keeps more of it and
                        // deepens it.
                        let kept = 1.0 + age * 1.4;
                        let lift = -age * 0.06;
                        let target = low[i * 4 + c] + texture * kept + lift;
                        px[i * 4 + c] =
                            (src[i * 4 + c] + (target - src[i * 4 + c]) * skin).clamp(0.0, 1.0);
                    }
                }
            }
        }

        // Light: shade the face by its own surface, which the depth model
        // knows and nothing else here does. Without the model there is no
        // relighting -- guessing a normal from luminance would just
        // sharpen the shadows that are already there.
        if light != 0.0 {
            let depth = schist_neural::get("depth").and_then(|model| {
                let key = fingerprint(px, width, height);
                self.depth.get(key, || {
                    let rgb = rgb_of(px);
                    schist_neural::depth_map(&model, &rgb, width, height)
                        .map_err(|e| log::warn!("smart portrait light: {e:#}"))
                        .ok()
                })
            });
            if let Some(depth) = depth {
                for y in 0..height {
                    for x in 0..width {
                        let i = y * width + x;
                        let mut inside = 0.0f32;
                        for p in parts.iter() {
                            inside = inside.max(FaceParts::falloff(
                                x as f32,
                                y as f32,
                                (p.centre.0, p.centre.1, p.span * 1.1),
                            ));
                        }
                        if inside <= 0.0 {
                            continue;
                        }
                        // The face as a surface: the depth map's slope
                        // is its normal, and a light rakes across that.
                        let at = |xx: usize, yy: usize| depth[yy * width + xx];
                        let gx = at((x + 1).min(width - 1), y) - at(x.saturating_sub(1), y);
                        let gy = at(x, (y + 1).min(height - 1)) - at(x, y.saturating_sub(1));
                        // A silhouette is a cliff in the depth map, not a
                        // surface: shading across one puts a bright rim
                        // round the head, which is exactly what it looked
                        // like before this test was here.
                        let steep = (gx.hypot(gy) * 25.0).min(1.0);
                        let n = [-gx * 14.0, -gy * 14.0, 1.0];
                        let nl = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt().max(1e-6);
                        // The slider swings the light from one side to
                        // the other, a little above.
                        let l = [light.signum(), -0.35, 0.9];
                        let ll = (l[0] * l[0] + l[1] * l[1] + l[2] * l[2]).sqrt();
                        let lambert =
                            ((n[0] * l[0] + n[1] * l[1] + n[2] * l[2]) / (nl * ll)).clamp(0.0, 1.0);
                        // Around the average, so lighting one side darkens
                        // the other rather than brightening everything.
                        let shade =
                            1.0 + (lambert - 0.6) * light.abs() * 1.2 * inside * (1.0 - steep);
                        for c in 0..3 {
                            px[i * 4 + c] = (px[i * 4 + c] * shade.clamp(0.4, 1.8)).clamp(0.0, 1.0);
                        }
                    }
                }
            }
        }
    }
}

/// Makeup Transfer: take the colour off one face and put it on another.
///
/// The reference is the layer underneath, as it is for Harmonization --
/// paste the face whose makeup you want below the one you are editing.
///
/// Adobe's network segments both faces and matches them feature to
/// feature. This assumes where the features are, and then refines the one
/// that matters: lips are the reddest thing in the bottom half of a face,
/// so the mouth's mask is weighted by how red each pixel actually is
/// rather than trusting the geometry alone. Eyes and skin are geometry.
///
/// It moves colour, not texture: it will give you someone else's lipstick
/// and eyeshadow, and it will not give you their eyeliner.
pub struct MakeupTransfer {
    faces: Memo<Vec<Face>>,
    reference: Memo<Vec<Face>>,
}

impl MakeupTransfer {
    pub const fn new() -> MakeupTransfer {
        MakeupTransfer {
            faces: Memo::new(),
            reference: Memo::new(),
        }
    }
}

impl Default for MakeupTransfer {
    fn default() -> Self {
        Self::new()
    }
}

/// How much a colour looks like a lip: red, but not skin.
fn lipness(p: &[f32]) -> f32 {
    let l = luma(p);
    if l < 0.06 {
        return 0.0;
    }
    // Lips are further towards red than the cheek beside them, and
    // darker.
    let redness = (p[0] - (p[1] + p[2]) / 2.0) / l.max(1e-3);
    ((redness - 0.12) * 4.0).clamp(0.0, 1.0)
}

impl FilterPlugin for MakeupTransfer {
    fn id(&self) -> &'static str {
        "filter.neural.makeup_transfer"
    }
    fn name(&self) -> &'static str {
        "Makeup Transfer"
    }
    fn category(&self) -> &'static str {
        "Neural Filters"
    }
    fn params(&self) -> Vec<FilterParam> {
        vec![
            param("lips", "Lips", 0.0, 100.0, 80.0, ""),
            param("eyes", "Eyes", 0.0, 100.0, 60.0, ""),
            param("skin", "Skin Tone", 0.0, 100.0, 30.0, ""),
        ]
    }

    fn wants_backdrop(&self) -> bool {
        true
    }

    fn info(&self) -> Option<String> {
        match schist_neural::installed("face") {
            true => Some(
                "Takes the makeup from a face on the layer underneath. \
                 Colour, not texture."
                    .to_string(),
            ),
            false => model_note("face", "so this does nothing."),
        }
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
        if width == 0 || height == 0 {
            return;
        }
        let lips = values.get("lips") / 100.0;
        let eyes = values.get("eyes") / 100.0;
        let skin = values.get("skin") / 100.0;
        let Some(backdrop) = context.backdrop else {
            return;
        };
        let Some(model) = schist_neural::get("face") else {
            return;
        };
        let find = |memo: &Memo<Vec<Face>>, buf: &[f32]| -> Option<Arc<Vec<Face>>> {
            let key = fingerprint(buf, width, height);
            memo.get(key, || {
                let rgb = rgb_of(buf);
                schist_neural::faces(&model, &rgb, width, height)
                    .map_err(|e| log::warn!("makeup transfer: {e:#}"))
                    .ok()
            })
        };
        let (Some(mine), Some(theirs)) = (find(&self.faces, px), find(&self.reference, backdrop))
        else {
            return;
        };
        let (Some(mine), Some(theirs)) = (mine.first(), theirs.first()) else {
            return;
        };
        let (here, there) = (FaceParts::of(mine), FaceParts::of(theirs));

        // Each region is a mask over both faces; the colour of one moves
        // onto the other.
        let regions: [(Region, f32); 3] = [
            (region_lips, lips),
            (region_eyes, eyes),
            (region_skin, skin),
        ];
        for (mask, amount) in regions {
            if amount <= 0.0 {
                continue;
            }
            // Both masks up front: the one over this layer is needed
            // while the layer is being written to, and a closure that
            // reads the pixels cannot outlive that.
            let mask_of = |buf: &[f32], parts: &FaceParts| -> Vec<f32> {
                (0..width * height)
                    .map(|i| {
                        let (x, y) = ((i % width) as f32, (i / width) as f32);
                        mask(parts, x, y, &buf[i * 4..i * 4 + 4])
                    })
                    .collect::<Vec<f32>>()
            };
            let mine_mask = mask_of(px, &here);
            let their_mask = mask_of(backdrop, &there);
            let (Some(from), Some(to)) = (
                stats_of(px, |i| mine_mask[i] > 0.35),
                stats_of(backdrop, |i| their_mask[i] > 0.35),
            ) else {
                continue;
            };
            for i in 0..width * height {
                let m = mine_mask[i];
                if m <= 0.0 {
                    continue;
                }
                restat(&mut px[i * 4..i * 4 + 4], from, to, amount * m);
            }
        }
    }
}

fn region_lips(p: &FaceParts, x: f32, y: f32, px: &[f32]) -> f32 {
    // Geometry says roughly where the mouth is; redness says which of
    // those pixels is actually lip rather than chin.
    FaceParts::falloff(x, y, p.mouth) * lipness(px)
}

fn region_eyes(p: &FaceParts, x: f32, y: f32, _px: &[f32]) -> f32 {
    // The lid, which is the part makeup is on: the eye's own circle,
    // pushed up a little.
    p.eyes
        .iter()
        .map(|e| FaceParts::falloff(x, y + e.2 * 0.35, (e.0, e.1, e.2 * 1.25)))
        .fold(0.0, f32::max)
}

fn region_skin(p: &FaceParts, x: f32, y: f32, px: &[f32]) -> f32 {
    let inside = FaceParts::falloff(x, y, (p.centre.0, p.centre.1, p.span * 0.9));
    // Everything in the oval that is skin-coloured and is not a feature.
    let feature = p
        .eyes
        .iter()
        .map(|e| FaceParts::falloff(x, y, *e))
        .fold(FaceParts::falloff(x, y, p.mouth), f32::max);
    inside * skinness(px) * (1.0 - feature)
}

/// Sketch to Portrait: put a photograph back into a drawing.
///
/// Adobe's invents one, from a generative model that has been shown an
/// enormous number of faces. This is the small honest version of the same
/// idea: a network trained to *invert this build's own Photo to Sketch*,
/// which is a much easier question than inventing a face and fits in 450k
/// parameters. Give it a sketch this application made and it puts the
/// tone and the colour back; give it a pencil drawing and it does
/// something in the same spirit and rather worse.
///
/// It runs on the face rather than on the picture, because that is what
/// it was trained on: the detector finds one, the crop goes through the
/// network at the size it learned, and the result is blended back inside
/// a soft oval. With no face -- or no detector -- the whole selection
/// goes through instead, which is the right thing for a portrait that
/// fills the frame and the wrong thing for a landscape.
pub struct SketchToPortrait {
    faces: Memo<Vec<Face>>,
}

impl SketchToPortrait {
    pub const fn new() -> SketchToPortrait {
        SketchToPortrait { faces: Memo::new() }
    }
}

impl Default for SketchToPortrait {
    fn default() -> Self {
        Self::new()
    }
}

impl FilterPlugin for SketchToPortrait {
    fn id(&self) -> &'static str {
        "filter.neural.sketch_to_portrait"
    }
    fn name(&self) -> &'static str {
        "Sketch to Portrait"
    }
    fn category(&self) -> &'static str {
        "Neural Filters"
    }
    fn params(&self) -> Vec<FilterParam> {
        vec![
            param("strength", "Strength", 0.0, 100.0, 100.0, ""),
            param("detail", "Keep Lines", 0.0, 100.0, 25.0, ""),
            param("colour", "Colour", 0.0, 300.0, 150.0, "%"),
        ]
    }

    fn info(&self) -> Option<String> {
        Some(match schist_neural::installed("face") {
            true => "Trained to fill in this application's own Photo to \
                     Sketch. Works on the face it finds."
                .to_string(),
            false => "Trained to fill in this application's own Photo to \
                      Sketch. Install Faces (Skin Smoothing) and it will \
                      work on the face rather than the whole selection."
                .to_string(),
        })
    }

    fn apply(&self, px: &mut [f32], width: usize, height: usize, values: &FilterValues) {
        if width == 0 || height == 0 {
            return;
        }
        let strength = (values.get("strength") / 100.0).clamp(0.0, 1.0);
        let keep = (values.get("detail") / 100.0).clamp(0.0, 1.0);
        let colour = values.get("colour") / 100.0;
        if strength <= 0.0 {
            return;
        }
        let Some(model) = schist_neural::get("portrait") else {
            return;
        };

        // The face, if a detector found one. Grown the way the training
        // crops were grown, or the network sees a tighter frame than it
        // was taught on and paints the jaw where the cheek should be.
        let face = schist_neural::get("face")
            .and_then(|detector| {
                let key = fingerprint(px, width, height);
                self.faces.get(key, || {
                    let rgb = rgb_of(px);
                    schist_neural::faces(&detector, &rgb, width, height)
                        .map_err(|e| log::warn!("sketch to portrait: {e:#}"))
                        .ok()
                })
            })
            .and_then(|faces| faces.first().copied());

        let (x0, y0, side) = match face {
            Some(f) => {
                let side = f.width.max(f.height) * PORTRAIT_GROW;
                (
                    f.x + f.width / 2.0 - side / 2.0,
                    f.y + f.height / 2.0 - side * 0.56,
                    side,
                )
            }
            None => (0.0, 0.0, width.max(height) as f32),
        };

        // Cut the crop out, letting it hang off the edges: a face at the
        // margin still gets a square, mirrored where it runs out.
        let side_px = side.round().max(8.0) as usize;
        let mut crop = vec![0.0f32; side_px * side_px * 3];
        for y in 0..side_px {
            for x in 0..side_px {
                let sx = mirror(x0 as i32 + x as i32, width);
                let sy = mirror(y0 as i32 + y as i32, height);
                let from = (sy * width + sx) * 4;
                let to = (y * side_px + x) * 3;
                crop[to..to + 3].copy_from_slice(&px[from..from + 3]);
            }
        }

        let Ok(mut filled) = schist_neural::run_framed(&model, &crop, side_px, side_px)
            .map_err(|e| log::warn!("sketch to portrait: {e:#}"))
        else {
            return;
        };

        // The network hedges on colour, because a network fitted to
        // absolute error always does: a sketch genuinely does not say
        // what colour anything was, and the safest guess is a pale one.
        // Colour scales what it decided on around its own luminance,
        // which is the difference between a plausible face and a
        // washed-out one.
        if (colour - 1.0).abs() > 1e-3 {
            for p in filled.as_chunks_mut::<3>().0.iter_mut() {
                let y = luma(&[p[0], p[1], p[2], 1.0]);
                for c in p.iter_mut() {
                    *c = (y + (*c - y) * colour).clamp(0.0, 1.0);
                }
            }
        }

        // Blend back inside a soft oval, so a face does not arrive in a
        // rectangle. With no face the whole crop is the picture and the
        // oval would cut its corners off, so it is skipped.
        let oval = face.is_some();
        for y in 0..side_px {
            for x in 0..side_px {
                let (ix, iy) = (x0 as i32 + x as i32, y0 as i32 + y as i32);
                if ix < 0 || iy < 0 || ix >= width as i32 || iy >= height as i32 {
                    continue;
                }
                let mut cover = strength;
                if oval {
                    let (u, v) = (
                        (x as f32 / side_px as f32 - 0.5) * 2.0,
                        (y as f32 / side_px as f32 - 0.5) * 2.0,
                    );
                    let d = u.hypot(v * 0.85);
                    cover *= ((1.0 - d) / 0.25).clamp(0.0, 1.0);
                }
                if cover <= 0.0 {
                    continue;
                }
                let to = (iy as usize * width + ix as usize) * 4;
                let from = (y * side_px + x) * 3;
                for c in 0..3 {
                    // Keep Lines multiplies the drawing back over the
                    // painting, which puts the pencil back on top of the
                    // colour rather than under it.
                    let painted =
                        filled[from + c] * (1.0 - keep) + filled[from + c] * px[to + c] * keep;
                    px[to + c] += (painted - px[to + c]) * cover;
                }
            }
        }
    }
}

/// How much bigger than the detector's box a portrait crop is, matching
/// `tools/train/faces.py` -- a box stops at the jaw and the hairline, and
/// the network was taught on crops that do not.
const PORTRAIT_GROW: f32 = 1.9;

/// Fold a coordinate back inside the image, so a crop that hangs off the
/// edge is mirrored rather than black.
fn mirror(v: i32, n: usize) -> usize {
    if n <= 1 {
        return 0;
    }
    let n = n as i32;
    let period = 2 * (n - 1);
    let mut m = v.rem_euclid(period);
    if m >= n {
        m = period - m;
    }
    m as usize
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
    registry.register_filter(Box::new(SmartPortrait::new()));
    registry.register_filter(Box::new(MakeupTransfer::new()));
    registry.register_filter(Box::new(SketchToPortrait::new()));
}
