//! Neural network inference for the Neural Filters.
//!
//! Runs ONNX models through [`tract`], which is pure Rust -- no ONNX
//! Runtime, no C toolchain, nothing to install. That matters here: a paint
//! program that needs a 300 MB runtime and a matching CUDA to sharpen a
//! photo is not a paint program anyone will use.
//!
//! Two kinds of model:
//!
//! * **Built in.** `detail.onnx`, `dejpeg.onnx`, `colorize.onnx` and the
//!   waifu2x upscalers ship inside the binary, a few megabytes between
//!   them -- ours were trained small on purpose (see `tools/train/`), and
//!   waifu2x's upconv_7 came small.
//! * **Downloaded.** The style-transfer, depth and face networks are
//!   megabytes to tens of megabytes each and are somebody else's work, so
//!   they are fetched on demand into the user's data directory and
//!   checked against a known hash.
//!
//! And two ways of feeding one, which is what [`Input`] distinguishes: a
//! model that *changes* an image sees it in tiles at full resolution,
//! while a model that answers a question *about* an image -- where the
//! faces are, what is near -- sees the whole thing resampled into one
//! fixed frame.
//!
//! Every filter that uses a model also works without it. The classical
//! implementation is not a stub -- it is the fallback, and the filter says
//! which one it used.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};

use anyhow::{bail, Context as _, Result};
use tract_onnx::prelude::*;

mod colour;
mod compat;
mod depth;
mod faces;
mod framed;
mod tile;
pub use colour::{chroma, recolour};
pub use depth::depth_map;
pub use faces::{faces, Face};
pub use framed::run_framed;
pub use tile::{run_scaled, run_tiled};

/// The models shipped inside the binary.
const DETAIL_ONNX: &[u8] = include_bytes!("../models/detail.onnx");
const DEJPEG_ONNX: &[u8] = include_bytes!("../models/dejpeg.onnx");
const COLORIZE_ONNX: &[u8] = include_bytes!("../models/colorize.onnx");
const PORTRAIT_ONNX: &[u8] = include_bytes!("../models/portrait.onnx");
const WAIFU2X_ART_ONNX: &[u8] = include_bytes!("../models/waifu2x-art.onnx");
const WAIFU2X_PHOTO_ONNX: &[u8] = include_bytes!("../models/waifu2x-photo.onnx");

/// How a model wants its pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Range {
    /// Channels in 0..=1, which is how everything here stores pixels.
    Unit,
    /// Channels in 0..=255, the convention the torchvision-derived
    /// style-transfer networks were trained with.
    Byte,
    /// `(v - mean) / sd` per channel, on channels in 0..=1: the
    /// ImageNet normalisation every torchvision backbone was fitted
    /// with, and which its successors kept.
    Standard { mean: [f32; 3], sd: [f32; 3] },
}

impl Range {
    /// A 0..=1 channel value as the model wants it.
    fn encode(self, v: f32, c: usize) -> f32 {
        match self {
            Range::Unit => v,
            Range::Byte => v * 255.0,
            Range::Standard { mean, sd } => (v - mean[c]) / sd[c],
        }
    }

    /// The inverse, for a model whose output is an image again.
    fn decode(self, v: f32, c: usize) -> f32 {
        match self {
            Range::Unit => v,
            Range::Byte => v / 255.0,
            Range::Standard { mean, sd } => v * sd[c] + mean[c],
        }
    }
}

/// The ImageNet statistics, spelled once.
const IMAGENET: Range = Range::Standard {
    mean: [0.485, 0.456, 0.406],
    sd: [0.229, 0.224, 0.225],
};

/// What to do when the image and the model's frame are not the same
/// shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fit {
    /// Squash it. Fine for a model whose answer is per-pixel and whose
    /// subject is the whole scene.
    Stretch,
    /// Letterbox it, so nothing is distorted. What a detector wants:
    /// squash a 16:9 panorama into a 4:3 frame and the faces in it stop
    /// looking like faces.
    Contain,
}

/// How a model wants its input framed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input {
    /// Square tiles cut from the image at full resolution, stitched back
    /// with `overlap` pixels of context trimmed off each edge. What an
    /// image-to-image model wants: it works on pixels, and there are
    /// however many of those there are. `scale` is how many output pixels
    /// the model makes of each input one -- 1 for a filter, more for an
    /// upscaler.
    Tiles {
        size: usize,
        overlap: usize,
        scale: usize,
    },
    /// One fixed frame the whole image is resampled into. What a model
    /// that answers a question *about* the picture wants -- where the
    /// faces are, how far away things are -- because that answer needs
    /// all of the picture and needs none of its resolution.
    Frame {
        width: usize,
        height: usize,
        fit: Fit,
    },
}

impl Input {
    /// The (width, height) the graph is fixed to.
    fn dims(self) -> (usize, usize) {
        match self {
            Input::Tiles { size, .. } => (size, size),
            Input::Frame { width, height, .. } => (width, height),
        }
    }

    /// Output pixels per input pixel.
    fn scale(self) -> usize {
        match self {
            Input::Tiles { scale, .. } => scale,
            Input::Frame { .. } => 1,
        }
    }
}

/// A model this build knows about.
#[derive(Debug, Clone)]
pub struct ModelSpec {
    pub id: &'static str,
    pub name: &'static str,
    /// File name inside the model directory.
    pub file: &'static str,
    /// Where to fetch it, or `None` when it ships with the binary.
    pub url: Option<&'static str>,
    /// SHA-256 of the file, so a truncated or substituted download is
    /// rejected rather than run.
    pub sha256: Option<&'static str>,
    pub bytes: usize,
    /// How the image is presented to the graph.
    pub input: Input,
    pub range: Range,
    pub license: &'static str,
    pub note: &'static str,
}

impl ModelSpec {
    pub fn built_in(&self) -> bool {
        self.url.is_none()
    }
}

/// Every model the Neural Filters can use.
pub const CATALOG: &[ModelSpec] = &[
    ModelSpec {
        id: "detail",
        name: "Detail (Super Zoom)",
        file: "detail.onnx",
        url: None,
        sha256: None,
        bytes: DETAIL_ONNX.len(),
        input: Input::Tiles {
            size: 128,
            overlap: 8,
            scale: 1,
        },
        range: Range::Unit,
        license: "Trained for Schist; same licence as the app",
        note: "Restores the high frequencies an enlargement loses. Trained \
               on the Kodak image suite; see tools/train/detail.py.",
    },
    ModelSpec {
        id: "dejpeg",
        name: "Deblock (JPEG Artifact Removal)",
        file: "dejpeg.onnx",
        url: None,
        sha256: None,
        bytes: DEJPEG_ONNX.len(),
        input: Input::Tiles {
            size: 128,
            overlap: 8,
            scale: 1,
        },
        range: Range::Unit,
        license: "Trained for Schist; same licence as the app",
        note: "Removes the blocking and ringing JPEG leaves behind. Trained \
               on the Kodak image suite compressed at every quality from 10 \
               to 60; see tools/train/dejpeg.py.",
    },
    ModelSpec {
        id: "colorize",
        name: "Colour (Colorize)",
        file: "colorize.onnx",
        url: None,
        sha256: None,
        bytes: COLORIZE_ONNX.len(),
        // Chroma is low-frequency and colour has to agree across a whole
        // subject, so this one sees the picture whole and small rather
        // than sharp and in pieces.
        input: Input::Frame {
            width: 256,
            height: 256,
            fit: Fit::Stretch,
        },
        range: Range::Unit,
        license: "Trained for Schist; same licence as the app",
        note: "Predicts colour for a greyscale photograph. Trained on \
               20,000 CC BY photographs from Open Images; see \
               tools/train/colorize.py.",
    },
    ModelSpec {
        id: "portrait",
        name: "Portrait (Sketch to Portrait)",
        file: "portrait.onnx",
        url: None,
        sha256: None,
        bytes: PORTRAIT_ONNX.len(),
        // A face at a time, whole: filling in a drawing means knowing
        // what the drawing is of, and no tile of a face knows that.
        input: Input::Frame {
            width: 128,
            height: 128,
            fit: Fit::Stretch,
        },
        range: Range::Unit,
        license: "Trained for Schist; same licence as the app",
        note: "Puts the tone and colour back into a sketch of a face. \
               Trained to invert this build's own Photo to Sketch on CC BY \
               faces from Open Images; see tools/train/portrait.py.",
    },
    ModelSpec {
        id: "waifu2x-art",
        name: "waifu2x ×2 (Art)",
        file: "waifu2x-art.onnx",
        url: None,
        sha256: None,
        bytes: WAIFU2X_ART_ONNX.len(),
        input: Input::Tiles {
            size: 128,
            overlap: 8,
            scale: 2,
        },
        range: Range::Unit,
        note: "Doubles an image's size, drawn edges staying edges. The \
               upconv_7 art model from the waifu2x project, trained on \
               illustrations; see tools/train/waifu2x.py.",
        license: "waifu2x (nagadomi), MIT",
    },
    ModelSpec {
        id: "waifu2x-photo",
        name: "waifu2x ×2 (Photo)",
        file: "waifu2x-photo.onnx",
        url: None,
        sha256: None,
        bytes: WAIFU2X_PHOTO_ONNX.len(),
        input: Input::Tiles {
            size: 128,
            overlap: 8,
            scale: 2,
        },
        range: Range::Unit,
        note: "Doubles an image's size. The upconv_7 photo model from the \
               waifu2x project, trained on photographs; see \
               tools/train/waifu2x.py.",
        license: "waifu2x (nagadomi), MIT",
    },
    ModelSpec {
        id: "style-mosaic",
        name: "Style: Mosaic",
        file: "style-mosaic.onnx",
        url: Some("https://github.com/onnx/models/raw/main/validated/vision/style_transfer/fast_neural_style/model/mosaic-9.onnx"),
        sha256: Some("fa646dedade881243f8d5a2ceb7de2b93675b21fc24f7482894ac4851a9a0a47"),
        bytes: 6_728_029,
        input: Input::Tiles { size: 384, overlap: 32, scale: 1 },
        range: Range::Byte,
        license: "ONNX Model Zoo, Apache-2.0",
        note: "Fast neural style transfer (Johnson et al.).",
    },
    ModelSpec {
        id: "style-candy",
        name: "Style: Candy",
        file: "style-candy.onnx",
        url: Some("https://github.com/onnx/models/raw/main/validated/vision/style_transfer/fast_neural_style/model/candy-9.onnx"),
        sha256: Some("9d11a3529d1e547da6ae07201d93484dbab2ec0a3614535752c8f40f0fe2968a"),
        bytes: 6_728_029,
        input: Input::Tiles { size: 384, overlap: 32, scale: 1 },
        range: Range::Byte,
        license: "ONNX Model Zoo, Apache-2.0",
        note: "Fast neural style transfer (Johnson et al.).",
    },
    ModelSpec {
        id: "style-udnie",
        name: "Style: Udnie",
        file: "style-udnie.onnx",
        url: Some("https://github.com/onnx/models/raw/main/validated/vision/style_transfer/fast_neural_style/model/udnie-9.onnx"),
        sha256: Some("8656b6ce7dec8f22ee13c2d557d6b67bd6f550dde88d0f2e7c9972aeb765cc0d"),
        bytes: 6_728_029,
        input: Input::Tiles { size: 384, overlap: 32, scale: 1 },
        range: Range::Byte,
        license: "ONNX Model Zoo, Apache-2.0",
        note: "Fast neural style transfer (Johnson et al.).",
    },
    ModelSpec {
        id: "depth",
        name: "Depth (Depth Blur)",
        file: "depth.onnx",
        url: Some("https://github.com/isl-org/MiDaS/releases/download/v2_1/model-small.onnx"),
        sha256: Some("2d8c6cb8f415229daf1eb041024208e2608c9f98e17c81cc7c6ecb449c56fd58"),
        bytes: 66_764_249,
        input: Input::Frame { width: 256, height: 256, fit: Fit::Stretch },
        range: IMAGENET,
        license: "MiDaS v2.1 small, Intel ISL, MIT",
        note: "Estimates how far away everything in a photograph is, from \
               the photograph alone (Ranftl et al.).",
    },
    ModelSpec {
        id: "face",
        name: "Faces (Skin Smoothing)",
        file: "face.onnx",
        url: Some("https://github.com/onnx/models/raw/main/validated/vision/body_analysis/ultraface/models/version-RFB-320.onnx"),
        sha256: Some("34cd7e60aeff28744c657de7a3dc64e872d506741de66987f3426f2b79f88017"),
        bytes: 1_270_727,
        input: Input::Frame { width: 320, height: 240, fit: Fit::Contain },
        range: Range::Standard { mean: [0.498_039_2; 3], sd: [0.501_960_8; 3] },
        license: "ONNX Model Zoo, MIT",
        note: "Finds faces, so Skin Smoothing can work on skin that is on \
               one rather than on anything skin-coloured.",
    },
];

pub fn spec(id: &str) -> Option<&'static ModelSpec> {
    CATALOG.iter().find(|m| m.id == id)
}

/// Where downloaded models live.
pub fn model_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("SCHIST_MODEL_DIR") {
        return PathBuf::from(dir);
    }
    let base = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".local/share")
        });
    base.join("schist/models")
}

/// Whether a model is ready to run.
pub fn installed(id: &str) -> bool {
    match spec(id) {
        Some(s) if s.built_in() => true,
        Some(s) => model_dir().join(s.file).exists(),
        None => false,
    }
}

/// A loaded model, ready to run.
pub struct Model {
    plan: Arc<TypedSimplePlan>,
    pub spec: &'static ModelSpec,
}

impl Model {
    /// Load from ONNX bytes, fixing the input to one frame so tract can
    /// optimize the graph completely rather than for an unknown size.
    pub fn from_bytes(spec: &'static ModelSpec, bytes: &[u8]) -> Result<Model> {
        let (w, h) = spec.input.dims();
        let mut cursor = std::io::Cursor::new(bytes);
        let onnx = tract_onnx::onnx();
        let mut proto = onnx
            .proto_model_for_read(&mut cursor)
            .context("not a readable ONNX model")?;
        if compat::modernise(&mut proto) {
            log::debug!("{}: rewrote a pre-opset-10 graph", spec.id);
        }
        let plan = onnx
            .model_for_proto_model(&proto)
            .context("not a model tract can parse")?
            .with_input_fact(0, f32::fact([1, 3, h, w]).into())
            .context("model does not take a 1x3xHxW float input")?
            .into_optimized()
            .context("model uses an operator tract cannot run")?
            .into_runnable()?;
        Ok(Model { plan, spec })
    }

    /// Run the graph over one frame of interleaved RGB in 0..=1, sized
    /// exactly as the spec says, and hand back its outputs untouched.
    fn run(&self, rgb: &[f32]) -> Result<TVec<TValue>> {
        let (w, h) = self.spec.input.dims();
        if rgb.len() != w * h * 3 {
            bail!("expected {} floats, got {}", w * h * 3, rgb.len());
        }
        let range = self.spec.range;
        // Interleaved RGB to the planar NCHW every ONNX vision model wants.
        let input = tract_ndarray::Array4::<f32>::from_shape_fn((1, 3, h, w), |(_, c, y, x)| {
            range.encode(rgb[(y * w + x) * 3 + c], c)
        });
        self.plan.run(tvec!(input.into_tensor().into()))
    }

    /// Run one tile of an image-to-image model. `rgb` is
    /// `size * size * 3` floats in 0..=1; the result is the same times the
    /// spec's scale factor.
    pub fn run_tile(&self, rgb: &[f32]) -> Result<Vec<f32>> {
        let (w, h) = self.spec.input.dims();
        let scale = self.spec.input.scale();
        let (w, h) = (w * scale, h * scale);
        let out = self.run(rgb)?;
        let view = out[0].to_plain_array_view::<f32>()?;
        let shape = view.shape();
        if shape.len() != 4 || shape[1] < 3 {
            bail!("unexpected output shape {shape:?}");
        }
        let (oh, ow) = (shape[2], shape[3]);
        let flat = view.as_slice().context("non-contiguous output")?;
        let range = self.spec.range;
        let mut rgb_out = vec![0.0f32; w * h * 3];
        for y in 0..h {
            for x in 0..w {
                for c in 0..3 {
                    // Some models return a different size than they were
                    // given; clamp rather than fail, so a mismatch shows
                    // as a soft edge and not a crash.
                    let sy = y.min(oh.saturating_sub(1));
                    let sx = x.min(ow.saturating_sub(1));
                    let v = range.decode(flat[((c * oh) + sy) * ow + sx], c);
                    rgb_out[(y * w + x) * 3 + c] = v.clamp(0.0, 1.0);
                }
            }
        }
        Ok(rgb_out)
    }
}

/// How an image was fitted into a model's frame, so an answer in frame
/// coordinates can be read back in image ones.
#[derive(Debug, Clone, Copy)]
struct Framing {
    /// Frame pixels per image pixel, horizontally and vertically. The two
    /// differ when the image was squashed rather than letterboxed.
    scale: (f32, f32),
    /// Where the image starts inside the frame.
    offset: (f32, f32),
}

impl Framing {
    /// The same framing against a map of a different size than the frame.
    ///
    /// A decoder that stops short of its input's resolution -- which is
    /// what the colour network does, because chroma does not need the
    /// resolution -- emits a map whose coordinates are the frame's
    /// scaled down. Folding that in here means the callers all read
    /// their output the same way.
    fn against(self, frame: (usize, usize), out: (usize, usize)) -> Framing {
        let rx = out.0 as f32 / frame.0 as f32;
        let ry = out.1 as f32 / frame.1 as f32;
        Framing {
            scale: (self.scale.0 * rx, self.scale.1 * ry),
            offset: (self.offset.0 * rx, self.offset.1 * ry),
        }
    }
}

/// Resample an image into a model's frame.
///
/// Areas outside a letterboxed image are filled with the mid grey a
/// network reads as "nothing here" rather than with black, which reads as
/// an edge.
fn frame(spec: &ModelSpec, rgb: &[f32], width: usize, height: usize) -> (Vec<f32>, Framing) {
    let (fw, fh) = spec.input.dims();
    let (sx, sy, ox, oy) = match spec.input {
        Input::Frame {
            fit: Fit::Contain, ..
        } => {
            let s = (fw as f32 / width as f32).min(fh as f32 / height as f32);
            (
                s,
                s,
                (fw as f32 - width as f32 * s) / 2.0,
                (fh as f32 - height as f32 * s) / 2.0,
            )
        }
        _ => (
            fw as f32 / width as f32,
            fh as f32 / height as f32,
            0.0,
            0.0,
        ),
    };
    let mut out = vec![0.5f32; fw * fh * 3];
    for fy in 0..fh {
        // Sample at pixel centres, so the resample is not half a pixel
        // off in both directions.
        let iy = (fy as f32 + 0.5 - oy) / sy - 0.5;
        for fx in 0..fw {
            let ix = (fx as f32 + 0.5 - ox) / sx - 0.5;
            if ix < -0.5 || iy < -0.5 || ix > width as f32 - 0.5 || iy > height as f32 - 0.5 {
                continue;
            }
            let (x0, y0) = (ix.floor().max(0.0) as usize, iy.floor().max(0.0) as usize);
            let (x1, y1) = ((x0 + 1).min(width - 1), (y0 + 1).min(height - 1));
            let (tx, ty) = (ix - x0 as f32, iy - y0 as f32);
            for c in 0..3 {
                let at = |x: usize, y: usize| rgb[(y * width + x) * 3 + c];
                let top = at(x0, y0) * (1.0 - tx) + at(x1, y0) * tx;
                let bot = at(x0, y1) * (1.0 - tx) + at(x1, y1) * tx;
                out[(fy * fw + fx) * 3 + c] = top * (1.0 - ty) + bot * ty;
            }
        }
    }
    (
        out,
        Framing {
            scale: (sx, sy),
            offset: (ox, oy),
        },
    )
}

type Cache = RwLock<HashMap<String, Option<Arc<Model>>>>;

fn cache() -> &'static Cache {
    static CACHE: OnceLock<Cache> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// Fetch a model, loading and caching it on first use.
///
/// Returns `None` when the model is not installed or will not load, which
/// is the signal for a filter to use its classical path instead. The
/// failure is cached too, so a broken file is not re-parsed on every dab.
pub fn get(id: &str) -> Option<Arc<Model>> {
    if let Some(hit) = cache().read().ok()?.get(id) {
        return hit.clone();
    }
    let spec = spec(id)?;
    let loaded = load(spec)
        .map_err(|e| log::warn!("neural model {id}: {e:#}"))
        .ok()
        .map(Arc::new);
    if let Ok(mut c) = cache().write() {
        c.insert(id.to_string(), loaded.clone());
    }
    loaded
}

fn load(spec: &'static ModelSpec) -> Result<Model> {
    if spec.built_in() {
        let bytes = match spec.id {
            "detail" => DETAIL_ONNX,
            "dejpeg" => DEJPEG_ONNX,
            "colorize" => COLORIZE_ONNX,
            "portrait" => PORTRAIT_ONNX,
            "waifu2x-art" => WAIFU2X_ART_ONNX,
            "waifu2x-photo" => WAIFU2X_PHOTO_ONNX,
            other => bail!("no built-in model named {other}"),
        };
        return Model::from_bytes(spec, bytes);
    }
    let path = model_dir().join(spec.file);
    let bytes = std::fs::read(&path).with_context(|| format!("{}", path.display()))?;
    Model::from_bytes(spec, &bytes)
}

/// Write a downloaded model into the store, rejecting it if the hash does
/// not match what this build expects.
pub fn install(spec: &ModelSpec, bytes: &[u8]) -> Result<PathBuf> {
    if let Some(want) = spec.sha256 {
        let got = sha256_hex(bytes);
        if got != want {
            bail!("checksum mismatch: expected {want}, got {got}");
        }
    }
    let dir = model_dir();
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(spec.file);
    // Write beside the target and rename, so an interrupted install
    // cannot leave a half-file that later loads as a corrupt model.
    let tmp = path.with_extension("part");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, &path)?;
    forget(spec.id);
    Ok(path)
}

/// Remove an installed model.
pub fn uninstall(spec: &ModelSpec) -> Result<()> {
    if spec.built_in() {
        bail!("{} ships with the application", spec.name);
    }
    let path = model_dir().join(spec.file);
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    forget(spec.id);
    Ok(())
}

/// Drop a model from the cache so the next use reloads it.
pub fn forget(id: &str) {
    if let Ok(mut c) = cache().write() {
        c.remove(id);
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// The size of an installed model on disk, for the manager dialog.
pub fn installed_size(spec: &ModelSpec) -> Option<u64> {
    if spec.built_in() {
        return Some(spec.bytes as u64);
    }
    std::fs::metadata(model_dir().join(spec.file))
        .ok()
        .map(|m| m.len())
}

/// Path a model would live at, for messages.
pub fn path_of(spec: &ModelSpec) -> PathBuf {
    model_dir().join(spec.file)
}

/// True when `path` is inside the model directory, so callers can refuse
/// to delete anything else.
pub fn is_in_store(path: &Path) -> bool {
    path.starts_with(model_dir())
}
