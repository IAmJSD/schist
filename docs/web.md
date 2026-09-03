# Schist in the browser

Schist compiles to `wasm32-unknown-unknown` and runs as a static web page,
rendering through gpui's web backend (WebGPU into a full-viewport canvas;
the browser's JS event loop is the platform event loop). One build serves
every platform a browser runs on.

## Building and serving

```sh
make web            # or: tools/web-build.sh   (--debug for a fast build)
python3 -m http.server -d dist/web 8000
# open http://localhost:8000
```

Requirements: the `wasm32-unknown-unknown` rustup target, a
`wasm-bindgen-cli` whose version equals the `wasm-bindgen` entry in
`Cargo.lock` (the script checks and says so), optionally `wasm-opt` from
[binaryen](https://github.com/WebAssembly/binaryen/releases) **version
116 or newer** (it takes ~40% off the module; older binaryens — including
Ubuntu's packaged 108 — silently corrupt the externref table rustc now
emits, so the script refuses them rather than shipping a module that dies
at init), and a browser with WebGPU (Chrome/Edge 113+, Firefox 141+,
Safari 26+). WebGPU needs a secure context, so serve from `localhost` or
https. On macOS build hosts, see the archiver note in the gpui fork's
`docs/web.md` (`psm`'s prebuilt object vs Xcode's `ar`).

## Deployment (try.schist.app)

`.github/workflows/web.yml` builds `dist/web/` on every push to main and
PR (uploading it as an artifact either way), and on main deploys it to
Cloudflare Workers as an assets-only Worker — `wrangler.jsonc` at the
repository root names it `schist-try` and binds the `try.schist.app`
custom domain. The deploy needs two repository secrets and skips itself
politely without them: `CLOUDFLARE_API_TOKEN` (Workers Scripts:Edit plus
the schist.app zone, which the custom domain's DNS and certificate come
from) and `CLOUDFLARE_ACCOUNT_ID`. By hand: `make web && npx wrangler
deploy`.

## How the page loads

`dist/web/` is entirely static:

- `index.html` is the **loading page**: logo, a byte-accurate progress
  bar, and a status line. On a first visit it also shows a one-time
  notice that the desktop app is the fuller Schist (with a link to the
  releases page); OK records the acceptance in localStorage
  (`schist.desktop-notice-accepted`) and it never shows again. It fades out when the app's window is up. The
  app's panic hook turns it into an error card, so a crash during boot
  never reads as "still loading". A browser without WebGPU is told so
  before anything downloads.
- `loader.js` fetches everything in parallel with one progress count,
  then instantiates the module.
- `pkg/schist_bg.wasm.NNN` — the module, **split into 4 MiB chunks** by
  the build script. One large `.wasm` downloads as an opaque
  single-connection stall; chunks download in parallel and re-assemble
  in the loader before instantiation.
- `assets/` — everything the native binary embeds is a plain served file
  here instead, so the wasm itself carries no assets: the icon SVGs and
  the UI font are fetched by the loading page and handed to the app on
  `window.__schist_boot`; the neural models are fetched by the app on
  demand (below).

The app itself never fetches during startup: `crates/app/src/web/mod.rs`
reads the boot payload synchronously, registers the fonts with both text
systems (a browser exposes no system fonts), and opens the window.

## What is different from the desktop build

Files. A browser has no file paths, but every open/save flow in the app
is built on them, so the web build keeps the flows and invents the paths:
File ▸ Open raises a real file picker, and the picked bytes are stored in
an in-memory map under `/web/open/<n>/<name>`; decoding reads from that
map. Saving runs the codec as ever, then hands the bytes to the browser
as a download — the browser's own prompt asks for the file name, whose
extension picks the format. Export and slice/artboard export work the
same way. Preferences persist in `localStorage` instead of a config file.

Neural models. Nothing ships inside the module — 11 MB of baseline
download for filters that may never run is the wrong trade — so the
formerly-embedded models are served under `assets/models/` and fetched
(with progress, via the same Filter ▸ Neural Filters ▸ Manage Models
dialog) into an in-memory store; after the first fetch the browser's HTTP
cache makes re-fetching per session cheap. The externally-hosted models
(style transfer, depth, segmentation, faces) are unavailable: their
GitHub URLs redirect through a host that sends no CORS headers, so a
browser fetch is refused before it starts. Every neural filter falls
back to its classical implementation when its model is absent — that is
a design guarantee of `crates/neural`, not a web special case.

Compositing happens on the CPU reference backend. The GPU compositor
opens a second wgpu device behind a blocking wait, which the browser's
single thread cannot make progress under; gpui's own WebGPU renderer
still draws the UI. Likewise rayon degrades to sequential execution on
this target (no threads without cross-origin-isolated SharedArrayBuffer),
so heavy filters are slower than native.

Compiled out entirely, with the reason:

| Feature | Why |
| --- | --- |
| AI sidebar | drives locally installed `claude`/`codex` CLIs; a tab spawns no processes |
| Photo gallery | watches folders and mounts cameras; a tab has neither, and the PSD sidecars it versions edits into need a real file system |
| Photoshop `.8bf` plug-ins | helper subprocesses and dlopen |
| Third-party wasm plug-ins | wasmtime is a JIT; a wasm module cannot host one |
| HEIC import | libheif is dlopen'd |
| Auto-update | a web deployment updates by serving newer files |
| Crash reporting (Sentry) | blocking transport; panics go to the console instead |
| Font downloads | the Google Fonts catalogue trick needs a spoofed legacy user agent to be served TTFs |
| Crash-recovery autosave | no state directory; the timer no-ops |

The menu entries for those features are filtered out of the web build
rather than left to fail.

## Known gaps

Camera raws open through the pure-Rust `schist-codec-raw` decoder, the
same one the desktop uses first. The few codecs it declines — Canon CR3
pixels, Fuji's compressed RAF, a handful of odd ones — go to a runtime
LibRaw on the desktop; a browser has no library to load, so those files
are refused with a message saying so (their embedded previews still
show).

- Text layers can only use the fonts the page ships (IBM Plex Sans
  Regular today — add more in `web/fonts/` and they are picked up by the
  build script). No CJK face is shipped by default.
- Drag-and-drop of files onto the window doesn't arrive: gpui's
  file-drop events carry paths, which browser drops don't have.
- Clipboard is a write-through mirror (see the gpui fork's `docs/web.md`
  for the details and the paste-keystroke exception).
- Saving always downloads; there is no File System Access API
  integration yet, so "Save" cannot silently overwrite the original.
