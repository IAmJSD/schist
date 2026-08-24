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

pub mod archive;
pub mod container;
pub mod emit;
pub mod error;
pub mod export;
pub mod graph;
pub mod import;
pub mod preserve;

pub use archive::{is_affinity, Archive};
pub use error::AffinityError;
pub use export::write_affinity;
pub use import::read_affinity;
