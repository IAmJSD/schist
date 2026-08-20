//! Write Photoslop plugins in Rust.
//!
//! A plugin is a `wasm32-unknown-unknown` cdylib that exports a handful of
//! functions. This crate supplies the boilerplate — memory management, the
//! manifest export, pointer packing — so a filter is just a function over
//! pixels:
//!
//! ```ignore
//! use photoslop_plugin_sdk::*;
//!
//! photoslop_filter! {
//!     id: "com.example.invert",
//!     name: "Invert (plugin)",
//!     category: "Plugins",
//!     params: [param("amount", "Amount", 0.0, 100.0, 100.0, "%")],
//!     apply: |pixels: &mut [f32], _w: usize, _h: usize, params: &Params| {
//!         let amount = params.get("amount") / 100.0;
//!         for px in pixels.chunks_exact_mut(4) {
//!             for c in 0..3 {
//!                 px[c] += (1.0 - px[c] - px[c]) * amount;
//!             }
//!         }
//!     }
//! }
//! ```
//!
//! Build with:
//! `cargo build --release --target wasm32-unknown-unknown`
//! and drop the resulting `.wasm` into `~/.config/photoslop/plugins/`.

use std::collections::HashMap;

/// ABI version this SDK targets. Must match the host's.
pub const ABI_VERSION: i32 = 1;

/// Parameter values passed to a filter, keyed by the manifest's param keys.
#[derive(Debug, Default, Clone)]
pub struct Params(pub HashMap<String, f32>);

impl Params {
    pub fn from_json(bytes: &[u8]) -> Params {
        let map: HashMap<String, f64> = serde_json::from_slice(bytes).unwrap_or_default();
        Params(map.into_iter().map(|(k, v)| (k, v as f32)).collect())
    }

    pub fn get(&self, key: &str) -> f32 {
        self.0.get(key).copied().unwrap_or_default()
    }

    pub fn get_or(&self, key: &str, fallback: f32) -> f32 {
        self.0.get(key).copied().unwrap_or(fallback)
    }
}

/// Declare one tunable for a filter's manifest.
pub fn param(
    key: &str,
    label: &str,
    min: f32,
    max: f32,
    default: f32,
    suffix: &str,
) -> serde_json::Value {
    serde_json::json!({
        "key": key,
        "label": label,
        "min": min,
        "max": max,
        "default": default,
        "suffix": suffix,
    })
}

/// Pack a pointer and length the way the host expects.
pub fn pack(ptr: *const u8, len: usize) -> i64 {
    ((ptr as i64) << 32) | (len as i64 & 0xFFFF_FFFF)
}

/// Leak a byte buffer so the host can read it after the call returns, and
/// give back its packed pointer/length.
pub fn leak_packed(bytes: Vec<u8>) -> i64 {
    let boxed = bytes.into_boxed_slice();
    let len = boxed.len();
    let ptr = Box::into_raw(boxed) as *const u8;
    pack(ptr, len)
}

/// Write to the host's log.
pub fn log(message: &str) {
    unsafe {
        host::log(message.as_ptr() as i32, message.len() as i32);
    }
}

pub mod host {
    // Without this the import lands in the default "env" module and the
    // host's `photoslop::log` never matches it.
    #[link(wasm_import_module = "photoslop")]
    unsafe extern "C" {
        /// The host's only import: append a line to the application log.
        pub fn log(ptr: i32, len: i32);
    }
}

/// Memory exports every plugin needs. Invoked by the entry-point macros.
#[macro_export]
macro_rules! photoslop_memory_exports {
    () => {
        #[unsafe(no_mangle)]
        pub extern "C" fn photoslop_alloc(len: i32) -> i32 {
            let mut buf = vec![0u8; len.max(0) as usize];
            let ptr = buf.as_mut_ptr();
            std::mem::forget(buf);
            ptr as i32
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn photoslop_free(ptr: i32, len: i32) {
            if ptr == 0 || len <= 0 {
                return;
            }
            unsafe {
                drop(Vec::from_raw_parts(
                    ptr as *mut u8,
                    len as usize,
                    len as usize,
                ));
            }
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn photoslop_abi_version() -> i32 {
            $crate::ABI_VERSION
        }
    };
}

/// Define a filter plugin.
#[macro_export]
macro_rules! photoslop_filter {
    (
        id: $id:expr,
        name: $name:expr,
        category: $category:expr,
        params: [$($param:expr),* $(,)?],
        apply: $apply:expr $(,)?
    ) => {
        $crate::photoslop_memory_exports!();

        #[unsafe(no_mangle)]
        pub extern "C" fn photoslop_manifest() -> i64 {
            let manifest = serde_json::json!({
                "id": $id,
                "name": $name,
                "kind": "filter",
                "api_version": $crate::ABI_VERSION,
                "category": $category,
                "params": [$($param),*],
                "capabilities": ["log"],
            });
            $crate::leak_packed(manifest.to_string().into_bytes())
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn photoslop_filter_apply(
            ptr: i32,
            width: i32,
            height: i32,
            params_ptr: i32,
            params_len: i32,
        ) {
            let count = (width.max(0) as usize) * (height.max(0) as usize) * 4;
            if ptr == 0 || count == 0 {
                return;
            }
            let pixels: &mut [f32] =
                unsafe { std::slice::from_raw_parts_mut(ptr as *mut f32, count) };
            let params = if params_ptr == 0 || params_len <= 0 {
                $crate::Params::default()
            } else {
                let bytes = unsafe {
                    std::slice::from_raw_parts(params_ptr as *const u8, params_len as usize)
                };
                $crate::Params::from_json(bytes)
            };
            let f: fn(&mut [f32], usize, usize, &$crate::Params) = $apply;
            f(pixels, width as usize, height as usize, &params);
        }
    };
}

/// Define an image-format plugin.
#[macro_export]
macro_rules! photoslop_codec {
    (
        id: $id:expr,
        name: $name:expr,
        extensions: [$($ext:expr),* $(,)?],
        probe: $probe:expr,
        decode: $decode:expr $(,)?
    ) => {
        $crate::photoslop_memory_exports!();

        #[unsafe(no_mangle)]
        pub extern "C" fn photoslop_manifest() -> i64 {
            let manifest = serde_json::json!({
                "id": $id,
                "name": $name,
                "kind": "codec",
                "api_version": $crate::ABI_VERSION,
                "extensions": [$($ext),*],
                "capabilities": ["log"],
            });
            $crate::leak_packed(manifest.to_string().into_bytes())
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn photoslop_codec_probe(ptr: i32, len: i32) -> i32 {
            if ptr == 0 || len <= 0 {
                return 0;
            }
            let bytes = unsafe { std::slice::from_raw_parts(ptr as *const u8, len as usize) };
            let f: fn(&[u8]) -> bool = $probe;
            if f(bytes) {
                1
            } else {
                0
            }
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn photoslop_codec_decode(ptr: i32, len: i32) -> i64 {
            if ptr == 0 || len <= 0 {
                return 0;
            }
            let bytes = unsafe { std::slice::from_raw_parts(ptr as *const u8, len as usize) };
            let f: fn(&[u8]) -> Option<(u32, u32, Vec<u8>)> = $decode;
            match f(bytes) {
                Some((width, height, rgba)) => $crate::leak_packed($crate::encode_image(
                    width, height, rgba,
                )),
                None => 0,
            }
        }
    };
}

/// Build the blob a codec returns: a little-endian header length, the JSON
/// header, then RGBA8 pixels.
pub fn encode_image(width: u32, height: u32, rgba: Vec<u8>) -> Vec<u8> {
    let header = serde_json::json!({
        "width": width,
        "height": height,
        "data_len": rgba.len() as u32,
    })
    .to_string()
    .into_bytes();
    let mut out = Vec::with_capacity(4 + header.len() + rgba.len());
    out.extend_from_slice(&(header.len() as u32).to_le_bytes());
    out.extend_from_slice(&header);
    out.extend_from_slice(&rgba);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_parse_from_host_json() {
        let params = Params::from_json(br#"{"amount": 42.5, "radius": 3}"#);
        assert_eq!(params.get("amount"), 42.5);
        assert_eq!(params.get("radius"), 3.0);
        assert_eq!(params.get("missing"), 0.0);
        assert_eq!(params.get_or("missing", 7.0), 7.0);
    }

    #[test]
    fn malformed_params_fall_back_to_empty() {
        let params = Params::from_json(b"not json");
        assert_eq!(params.get("anything"), 0.0);
    }

    #[test]
    fn encode_image_layout_is_readable() {
        let blob = encode_image(2, 1, vec![1, 2, 3, 4, 5, 6, 7, 8]);
        let header_len = u32::from_le_bytes([blob[0], blob[1], blob[2], blob[3]]) as usize;
        let header: serde_json::Value = serde_json::from_slice(&blob[4..4 + header_len]).unwrap();
        assert_eq!(header["width"], 2);
        assert_eq!(header["height"], 1);
        assert_eq!(&blob[4 + header_len..], &[1, 2, 3, 4, 5, 6, 7, 8]);
    }
}
