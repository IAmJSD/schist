# Shared document library

`schist-document` is a headless Rust library shared by the desktop cloud client and Schist Cloud's Node server. It depends on core, codecs, the CPU compositor and Yrs. It has no GPUI, GPU, network, authentication or payment dependency. The image model also supports WASM with a browser-compatible clock.

```rust
let codecs = schist_document::registry();
let doc = schist_document::import(&codecs, &file_bytes, "image.psd")?;
let state = schist_document::SharedDocument::new(&doc)?.full_state();
// Persist/exchange this Yjs v1 update. Later, after merging client updates:
let edited = schist_document::materialize(&state)?;
let export = schist_document::export(&codecs, &edited, "psd")?;
```

`crates/cloud::document::SharedDocument` remains a compatibility re-export. The PSD plugin wrapper now lives in `schist-codecs-common`, so the UI and headless registry use the same wrapper and registration order. `registry()` returns the normal extensible `PluginRegistry`; a host can register additional trusted codecs.

The library includes every built-in desktop codec. HEIC requires libheif and a compatible HEVC decoder at runtime; no library is downloaded automatically by the worker. RAW and HEIC can be imported and collaboratively edited even though their codecs cannot export. Export to layered PSD/PSB to keep an editable document, or explicitly choose another available encoder. The desktop codecs' existing support/fidelity limits still apply. Pixel merging remains tile-level Yjs conflict resolution, not per-pixel blending.

## Build and verify

```sh
make document-worker PROFILE=debug
make check-document
make check-cloud
# Deployment binary:
make document-worker PROFILE=release
```

The standalone `target/<profile>/schist-document-worker` links this library. It reads a single binary input from stdin and writes one MessagePack map to stdout. Errors go to stderr with a nonzero exit code. It never opens a GUI or makes network requests.

| Command | stdin | stdout |
| --- | --- | --- |
| `formats` | empty | array of codec capabilities |
| `import <filename>` | original bytes | model, width, height, state (Yjs bin) |
| `validate` | Yjs v1 state | model, width, height, or an error |
| `export <extension>` | Yjs v1 state | model, width, height, bytes (bin), extension, mime_type |

The worker accepts up to 256 MiB input. Hosts should impose their own memory/CPU and concurrency limits; the Node host limits runtime to 25 seconds and two concurrent workers, and accepts up to 128 MiB merged collaboration state. Build the worker and Node host in the same base distribution for compatible system libraries; Schist Cloud's Dockerfile does this with an additional `schist` build context. Match installed fonts if identical text rendering matters across machines.

The `schist.image.v1` format is documented in [cloud.md](cloud.md) and [the client specification](ts-draft-client.ts). Its deterministic seed reserves Yjs client ID 1. Never seed an existing native room from a newly rendered export, since that could give different data the same CRDT item identities.
