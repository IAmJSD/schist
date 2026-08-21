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
//! Verified against Affinity 1 files. Affinity 2 files (zstd entries)
//! parse the container but not yet the payloads; callers should fall
//! back to preview extraction on any error.

pub mod archive;
pub mod error;
pub mod graph;
pub mod import;

pub use archive::{is_affinity, Archive};
pub use error::AffinityError;
pub use import::read_affinity;
