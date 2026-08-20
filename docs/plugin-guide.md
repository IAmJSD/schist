# Writing Photoslop plugins

Photoslop loads third-party plugins as sandboxed WebAssembly modules. A
plugin can add a **filter** (a function over pixels) or a **format** (an
image decoder). It runs with no filesystem, network, clock or randomness —
the host exposes exactly one import, `photoslop::log` — and is bounded by an
execution-fuel budget, so a runaway plugin is unwound instead of freezing
the editor.

## Quick start (Rust)

```toml
# Cargo.toml
[lib]
crate-type = ["cdylib"]

[dependencies]
photoslop-plugin-sdk = { git = "https://github.com/IAmJSD/photoslop" }
serde_json = "1"
```

```rust
use photoslop_plugin_sdk::*;

photoslop_filter! {
    id: "com.example.sepia",
    name: "Sepia",
    category: "Plugins",
    params: [param("amount", "Amount", 0.0, 100.0, 100.0, "%")],
    apply: |pixels: &mut [f32], _w: usize, _h: usize, params: &Params| {
        let amount = params.get("amount") / 100.0;
        for px in pixels.chunks_exact_mut(4) {
            // px is [r, g, b, a] in 0..1, straight (un-premultiplied) alpha
        }
    }
}
```

```sh
rustup target add wasm32-unknown-unknown
cargo build --release --target wasm32-unknown-unknown
cp target/wasm32-unknown-unknown/release/*.wasm ~/.config/photoslop/plugins/
```

Restart Photoslop; the filter appears in the **Filter** menu with a slider
per declared parameter. **File → Plugins…** lists what loaded, shows why
anything was refused, and can enable/disable or install plugins.

Two complete examples live in [`examples/plugins`](../examples/plugins):
`sepia-filter` (a filter) and `pgm-codec` (a Netpbm PGM decoder).

## Formats

```rust
photoslop_codec! {
    id: "com.example.pgm",
    name: "Netpbm PGM",
    extensions: ["pgm"],
    probe: |bytes: &[u8]| bytes.starts_with(b"P5"),
    decode: |bytes: &[u8]| -> Option<(u32, u32, Vec<u8>)> {
        // return (width, height, RGBA8) or None
        None
    }
}
```

## The raw ABI

The SDK is a convenience, not a requirement — any language that emits
`wasm32-unknown-unknown` modules can implement the ABI directly.

| export | signature | purpose |
|---|---|---|
| `photoslop_abi_version` | `() -> i32` | must return `1` |
| `photoslop_manifest` | `() -> i64` | packed `(ptr << 32) \| len` of manifest JSON |
| `photoslop_alloc` | `(i32) -> i32` | allocate guest memory |
| `photoslop_free` | `(i32, i32)` | release it |
| `photoslop_filter_apply` | `(ptr, w, h, params_ptr, params_len)` | filters: edit f32 RGBA in place |
| `photoslop_codec_probe` | `(ptr, len) -> i32` | formats: 1 if recognised |
| `photoslop_codec_decode` | `(ptr, len) -> i64` | formats: packed pointer to header + RGBA8 |

The module must export its `memory`. Manifest fields: `id`, `name`, `kind`
(`"filter"` or `"codec"`), `api_version`, and optionally `description`,
`category`, `params`, `extensions`, `capabilities`.

## Compatibility

`api_version` is checked on load; a mismatch refuses the plugin rather than
running it against an ABI it wasn't built for. Version 1 is frozen — future
additions will arrive as new optional exports, and anything incompatible
will bump the version.
