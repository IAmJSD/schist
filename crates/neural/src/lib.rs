//! Neural network inference for the Neural Filters.
//!
//! Runs ONNX models through [`tract`], which is pure Rust -- no ONNX
//! Runtime, no C toolchain, nothing to install. That matters here: a paint
//! program that needs a 300 MB runtime and a matching CUDA to sharpen a
//! photo is not a paint program anyone will use.
//!
//! Two kinds of model:
//!
//! * **Built in.** `detail.onnx` ships inside the binary. It is 80 KB,
//!   because it was trained to be (see `tools/train/detail.py`).
//! * **Downloaded.** The style-transfer networks are megabytes each and
//!   are somebody else's work, so they are fetched on demand into the
//!   user's data directory and checked against a known hash.
//!
//! Every filter that uses a model also works without it. The classical
//! implementation is not a stub -- it is the fallback, and the filter says
//! which one it used.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};

use anyhow::{bail, Context as _, Result};
use tract_onnx::prelude::*;

mod compat;
mod tile;
pub use tile::run_tiled;

/// The model shipped inside the binary.
const DETAIL_ONNX: &[u8] = include_bytes!("../models/detail.onnx");

/// How a model wants its pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Range {
    /// Channels in 0..=1, which is how everything here stores pixels.
    Unit,
    /// Channels in 0..=255, the convention the torchvision-derived
    /// style-transfer networks were trained with.
    Byte,
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
    /// Square tile the model is run over.
    pub tile: usize,
    /// How much neighbouring context each tile needs; trimmed off after,
    /// so tile seams do not show.
    pub overlap: usize,
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
        tile: 128,
        overlap: 8,
        range: Range::Unit,
        license: "Trained for Schist; same licence as the app",
        note: "Restores the high frequencies an enlargement loses. Trained \
               on the Kodak image suite; see tools/train/detail.py.",
    },
    ModelSpec {
        id: "style-mosaic",
        name: "Style: Mosaic",
        file: "style-mosaic.onnx",
        url: Some("https://github.com/onnx/models/raw/main/validated/vision/style_transfer/fast_neural_style/model/mosaic-9.onnx"),
        sha256: Some("fa646dedade881243f8d5a2ceb7de2b93675b21fc24f7482894ac4851a9a0a47"),
        bytes: 6_728_029,
        tile: 384,
        overlap: 32,
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
        tile: 384,
        overlap: 32,
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
        tile: 384,
        overlap: 32,
        range: Range::Byte,
        license: "ONNX Model Zoo, Apache-2.0",
        note: "Fast neural style transfer (Johnson et al.).",
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

/// A loaded model, ready to run over a tile.
pub struct Model {
    plan: Arc<TypedSimplePlan>,
    pub spec: &'static ModelSpec,
}

impl Model {
    /// Load from ONNX bytes, fixing the input to one tile so tract can
    /// optimize the graph completely rather than for an unknown size.
    pub fn from_bytes(spec: &'static ModelSpec, bytes: &[u8]) -> Result<Model> {
        let t = spec.tile;
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
            .with_input_fact(0, f32::fact([1, 3, t, t]).into())
            .context("model does not take a 1x3xHxW float input")?
            .into_optimized()
            .context("model uses an operator tract cannot run")?
            .into_runnable()?;
        Ok(Model { plan, spec })
    }

    /// Run one tile. `rgb` is `tile * tile * 3` floats in 0..=1.
    pub fn run_tile(&self, rgb: &[f32]) -> Result<Vec<f32>> {
        let t = self.spec.tile;
        if rgb.len() != t * t * 3 {
            bail!("expected {} floats, got {}", t * t * 3, rgb.len());
        }
        let k = match self.spec.range {
            Range::Unit => 1.0,
            Range::Byte => 255.0,
        };
        // Interleaved RGB to the planar NCHW every ONNX vision model wants.
        let input = tract_ndarray::Array4::<f32>::from_shape_fn((1, 3, t, t), |(_, c, y, x)| {
            rgb[(y * t + x) * 3 + c] * k
        });
        let out = self.plan.run(tvec!(input.into_tensor().into()))?;
        let view = out[0].to_plain_array_view::<f32>()?;
        let shape = view.shape();
        if shape.len() != 4 || shape[1] < 3 {
            bail!("unexpected output shape {shape:?}");
        }
        let (oh, ow) = (shape[2], shape[3]);
        let flat = view.as_slice().context("non-contiguous output")?;
        let mut rgb_out = vec![0.0f32; t * t * 3];
        for y in 0..t {
            for x in 0..t {
                for c in 0..3 {
                    // Some models return a different size than they were
                    // given; clamp rather than fail, so a mismatch shows
                    // as a soft edge and not a crash.
                    let sy = y.min(oh.saturating_sub(1));
                    let sx = x.min(ow.saturating_sub(1));
                    let v = flat[((c * oh) + sy) * ow + sx] / k;
                    rgb_out[(y * t + x) * 3 + c] = v.clamp(0.0, 1.0);
                }
            }
        }
        Ok(rgb_out)
    }
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
        return Model::from_bytes(spec, DETAIL_ONNX);
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
