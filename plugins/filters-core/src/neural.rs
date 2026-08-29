//! Photoshop's Neural Filters.
//!
//! Five of the seven run a real network. Super Zoom, JPEG Artifact
//! Removal and Colorize use models trained for this application and
//! shipped inside the binary (`tools/train/`); Style Transfer uses the
//! fast neural-style networks from the ONNX Model Zoo, Depth Blur uses
//! MiDaS and Skin Smoothing uses UltraFace, all three downloaded on
//! demand. Everything runs through `schist-neural`, which is `tract` --
//! pure Rust, so there is no runtime to install.
//!
//! The two that are not networks are not networks for a reason. Colour
//! Transfer is a statistic -- moving one image's colour distribution onto
//! another's is arithmetic, and a network would be a slower way to get
//! the same numbers. Skin Smoothing's *smoothing* is frequency
//! separation, which is what a retoucher does by hand; the network's job
//! there is to say where the faces are, which is the part that needs to
//! know what a face is.
//!
//! Every model-backed filter also works without its model, falling back
//! to the classical path and saying so in its dialog. Nothing here is a
//! stub that stops working when a download fails.

use std::sync::{Arc, Mutex};

use crate::util::{at, gaussian_rgba, luma, put, sample};
use crate::{choice, param, simple_filter};
use schist_neural::Face;
use schist_plugin_api::{FilterParam, FilterPlugin, FilterValues};

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

simple_filter!(
    ColorTransfer,
    "filter.neural.color_transfer",
    "Color Transfer",
    "Neural Filters",
    [
        param("hue", "Target Hue", 0.0, 360.0, 30.0, "\u{b0}"),
        param("strength", "Strength", 0.0, 100.0, 60.0, ""),
        param("contrast", "Match Contrast", 0.0, 100.0, 50.0, "")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // Reinhard-style colour transfer, aimed at a hue rather than a
        // reference image: shift the image's mean chroma towards the
        // target and optionally normalise its spread. There is no model
        // here and there is no missing one -- moving one distribution
        // onto another is arithmetic, and Photoshop's own network is
        // doing the *segmentation* around it, which Object Selection is
        // the filter for.
        let _ = (w, h);
        let strength = v.get("strength") / 100.0;
        let match_contrast = v.get("contrast") / 100.0;
        let hue = v.get("hue").to_radians();
        if strength <= 0.0 {
            return;
        }
        // Mean and spread of luminance, and mean chroma.
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
        // The target chroma direction.
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
);

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

pub fn register(registry: &mut schist_plugin_api::PluginRegistry) {
    registry.register_filter(Box::new(SkinSmoothing::new()));
    registry.register_filter(Box::new(JpegArtifactRemoval));
    registry.register_filter(Box::new(Colorize::new()));
    registry.register_filter(Box::new(SuperZoom));
    registry.register_filter(Box::new(StyleTransfer));
    registry.register_filter(Box::new(ColorTransfer));
    registry.register_filter(Box::new(DepthBlur::new()));
}
