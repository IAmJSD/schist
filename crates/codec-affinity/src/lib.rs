//! Native reader for Affinity (.afphoto / .afdesign / .afpub) files.
//!
//! Serif publishes no spec; this implementation reproduces the format as
//! reverse engineered from real files and prior art (afread by Vladimir
//! Mamonov, MIT; AFDesignLoad by Nick Beeuwsaert, MIT). The format is
//! documented in `docs/affinity-format.md`. Three stages:
//!
//! 1. [`archive`] — the container: a tiny versioned filesystem holding
//!    named, compressed, CRC-checked entries ("doc.dat", tile blocks…).
//! 2. [`graph`] — the object graph: "doc.dat" deserialized into a tree
//!    of tagged classes and fields.
//! 3. [`import`] — interpretation: walk the graph's document → spread →
//!    layer hierarchy into a [`schist_core::Document`], loading
//!    raster layers' pixel tiles from the container.
//!
//! Verified against every generation: Affinity 1 (zlib entries),
//! Affinity 2 / Canva-era (zstd entries), and the unified ".af"
//! container version 12.
//!
//! The write direction mirrors the same stages: [`emit`] re-serializes
//! object graphs byte-exactly, [`container`] writes archives, and
//! [`export`] builds a whole document from a [`schist_core::Document`].

/// How Affinity's `Radi` on a blur-shaped layer effect converts to our
/// own blur radius, on which the standard deviation is `radius/sqrt(3)`.
///
/// Affinity's is a standard deviation of about 0.34 x `Radi`, measured
/// by fitting an error function to an inner glow on a hard-edged square
/// at radius 20, 40 and 80 (fixtures/affinity-probe/ig_r*_i0.af). Import
/// multiplies by this and export divides by it, so the two cannot drift
/// — and so a shadow we write comes back the size we meant. A stroke's
/// `Radi` is a width rather than a blur and does not use it.
pub(crate) const BLUR_RADI: f32 = 0.58;

pub mod archive;
pub mod container;
pub(crate) mod distort;
pub mod emit;
pub mod error;
pub mod export;
pub mod graph;
pub mod import;
pub(crate) mod liveblur;
pub mod preserve;
pub(crate) mod vignette;

pub use archive::{is_affinity, Archive};
pub use error::AffinityError;
pub use export::write_affinity;
pub use import::read_affinity;
