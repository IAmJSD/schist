//! The photo gallery's on-disk model, shared by the app and the headless
//! `schist-mcp` server.
//!
//! Everything the gallery persists is a file the user can look at:
//! `library.json` (watched folders, buckets, recents, the people and
//! their tagged faces), the index snapshot (`index.v1`: one row per
//! photo — search embedding, EXIF position and capture time, nearest
//! city, content-filter verdict, detected faces), and the caches
//! beside each thumbnail. The app builds and maintains
//! them; a headless server reads them to answer for the gallery when
//! the app is not running. Both go through here, so the formats have
//! one owner.

pub mod geo;
// The headless gallery searches with the text tower, which the neural
// crate does not build for the web — and no web build serves the
// gallery anyway.
#[cfg(not(target_arch = "wasm32"))]
pub mod headless;
pub mod index;
pub mod meta;
pub mod paths;
pub mod people;
pub mod persist;
pub mod scan;
pub mod scores;
pub mod search;

pub use geo::*;
pub use index::*;
pub use meta::*;
pub use paths::*;
pub use people::*;
pub use persist::*;
pub use scan::*;
pub use scores::*;
pub use search::*;
