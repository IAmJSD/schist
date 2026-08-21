//! The Schist plugin ABI (version 1).
//!
//! Plugins are plain `wasm32-unknown-unknown` modules — no WASI, no
//! component model — so the surface is small enough to keep stable and to
//! implement from any language that can emit WebAssembly.
//!
//! ## Exports a plugin must provide
//!
//! | export | signature | purpose |
//! |---|---|---|
//! | `schist_abi_version` | `() -> i32` | must return [`ABI_VERSION`] |
//! | `schist_manifest` | `() -> i64` | packed pointer/length of a JSON [`Manifest`] |
//! | `schist_alloc` | `(i32) -> i32` | allocate guest memory for the host to fill |
//! | `schist_free` | `(i32, i32)` | release memory the host allocated |
//!
//! Filters additionally export `schist_filter_apply(ptr, width, height,
//! params_ptr, params_len)`, operating in place on straight-alpha f32 RGBA.
//! Codecs export `schist_codec_probe(ptr, len) -> i32` and
//! `schist_codec_decode(ptr, len) -> i64`, the latter returning a packed
//! pointer/length to a decoded [`DecodedImage`] JSON header followed by
//! RGBA8 pixels.
//!
//! ## Imports the host provides
//!
//! Only `schist::log(ptr, len)`. There is deliberately no filesystem,
//! clock, network or random source: a plugin can compute and nothing else,
//! so "capabilities" start empty and are granted per manifest request.

use serde::{Deserialize, Serialize};

/// Bumped whenever the ABI changes incompatibly. Plugins declaring a
/// different version are refused rather than mis-executed.
pub const ABI_VERSION: i32 = 1;

pub const EXPORT_ABI_VERSION: &str = "schist_abi_version";
pub const EXPORT_MANIFEST: &str = "schist_manifest";
pub const EXPORT_ALLOC: &str = "schist_alloc";
pub const EXPORT_FREE: &str = "schist_free";
pub const EXPORT_FILTER_APPLY: &str = "schist_filter_apply";
pub const EXPORT_CODEC_PROBE: &str = "schist_codec_probe";
pub const EXPORT_CODEC_DECODE: &str = "schist_codec_decode";

/// What a plugin provides.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PluginKind {
    Filter,
    Codec,
}

/// A capability a plugin asks the host for. The set is intentionally tiny;
/// anything absent here simply doesn't exist inside the sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Capability {
    /// Write to the application log.
    Log,
}

/// One tunable exposed to the host's dialog builder.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParamSchema {
    pub key: String,
    pub label: String,
    pub min: f32,
    pub max: f32,
    pub default: f32,
    #[serde(default)]
    pub suffix: String,
}

/// The JSON blob a plugin returns from `schist_manifest`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    /// Stable identifier, e.g. "com.example.sepia".
    pub id: String,
    pub name: String,
    pub kind: PluginKind,
    /// Must equal [`ABI_VERSION`].
    pub api_version: i32,
    #[serde(default)]
    pub description: String,
    /// Filters: menu grouping.
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub params: Vec<ParamSchema>,
    /// Codecs: lowercase extensions without the dot.
    #[serde(default)]
    pub extensions: Vec<String>,
    #[serde(default)]
    pub capabilities: Vec<Capability>,
}

/// Header a codec plugin returns ahead of its RGBA8 pixel data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    /// Byte length of the RGBA8 data that follows the header.
    pub data_len: u32,
}

/// Pack a pointer and length into the i64 the ABI returns.
pub fn pack(ptr: i32, len: i32) -> i64 {
    ((ptr as i64) << 32) | (len as i64 & 0xFFFF_FFFF)
}

/// Split a packed pointer/length.
pub fn unpack(value: i64) -> (i32, i32) {
    (
        ((value >> 32) & 0xFFFF_FFFF) as i32,
        (value & 0xFFFF_FFFF) as i32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_round_trips() {
        for (ptr, len) in [(0, 0), (1024, 16), (0x7FFF_FFF0, 12), (65536, 1_000_000)] {
            assert_eq!(unpack(pack(ptr, len)), (ptr, len));
        }
    }

    #[test]
    fn manifest_json_round_trips() {
        let manifest = Manifest {
            id: "com.example.sepia".into(),
            name: "Sepia".into(),
            kind: PluginKind::Filter,
            api_version: ABI_VERSION,
            description: "Warms an image".into(),
            category: "Color".into(),
            params: vec![ParamSchema {
                key: "amount".into(),
                label: "Amount".into(),
                min: 0.0,
                max: 100.0,
                default: 50.0,
                suffix: "%".into(),
            }],
            extensions: vec![],
            capabilities: vec![Capability::Log],
        };
        let json = serde_json::to_string(&manifest).unwrap();
        assert_eq!(serde_json::from_str::<Manifest>(&json).unwrap(), manifest);
    }

    #[test]
    fn manifest_defaults_keep_minimal_declarations_valid() {
        let json = r#"{"id":"a","name":"A","kind":"codec","api_version":1}"#;
        let manifest: Manifest = serde_json::from_str(json).unwrap();
        assert_eq!(manifest.kind, PluginKind::Codec);
        assert!(manifest.params.is_empty());
        assert!(manifest.capabilities.is_empty());
    }
}
