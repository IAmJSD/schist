# Architecture

Photoslop is a microkernel plus plugins. The kernel owns *state and
contracts*; everything a user can see or click lives in a plugin.

```
crates/app              GPUI shell: window, canvas, panels, dialogs, keymap
├── crates/plugin-api   the trait surface every feature implements
├── crates/core         kernel: document, COW tiles, layers, history, selection
├── crates/color        pixel/colour primitives, depth conversion
├── crates/pixel-ops    CPU reference blend modes (the semantic contract)
├── crates/compositor   tile compositor, adjustment rendering, damage cache
├── crates/adjustments  adjustment parameters, PSD payloads, LUT compilation
├── crates/vector       path building, Bézier flattening, AA rasterization
├── crates/text-engine  font discovery, layout, glyph rasterization
├── crates/colormgmt    ICC profiles, display transforms, dithering
├── crates/codec-psd    PSD/PSB reader and writer
├── crates/plugin-host-wasm  sandboxed third-party plugins
└── crates/plugin-sdk   what plugin authors compile against

plugins/                first-party features, each optional at compile time
├── tools-basic         move, eyedropper, hand, zoom
├── tools-paint         brush, pencil, eraser, clone, gradient, bucket, dodge…
├── tools-select        marquees, lasso, magic wand
├── tools-transform     free transform, crop, image/canvas resize
├── tools-vector        shapes, pen
├── tools-type          text layers
├── filters-core        blur, sharpen, noise
├── codecs-common       PNG/JPEG/WebP/TIFF
└── commands-core       menu commands and their keybindings
```

## Why the kernel is small

`crates/core` contains no features. Delete every `plugins/` entry and the
app still builds and boots — to an empty workspace that can do nothing.
That is the test of "everything is a plugin", and it keeps the contracts
honest: a tool cannot reach around the API because there is nothing to
reach for.

## Pixels

All raster data — layer pixels, masks, selections — lives in 256×256
**copy-on-write tiles**. One decision buys a lot:

* **Undo is cheap.** An edit stores `Arc`s of the tiles it replaced, so a
  history entry costs memory proportional to *changed* pixels, not document
  size.
* **Duplication is free** until something is written.
* **Damage tracking falls out.** Edits mark rectangles; the compositor and
  canvas recomposite only the tiles that intersect them.

Colour is straight-alpha `f32` RGBA in the compositing pipeline; documents
store 8/16/32-bit and convert at tile granularity.

## Rendering

`crates/pixel-ops` is the semantic reference: all 27 PSD blend modes,
per-pixel, tested against the spec's formulas. `crates/compositor` walks the
layer tree bottom-up per tile — groups isolate unless pass-through, masks
multiply source alpha, clipped layers are confined to their base's alpha,
adjustment layers re-colour the backdrop beneath them.

Painting is deliberately *not* per tile. GPUI's sprite atlas has no padding
between entries, so drawing one quad per tile let the sampler bleed past
each tile's slot at fractional zoom and drew a dark line at every tile
boundary. The canvas instead assembles the visible tiles into a single
image — resampled (nearest when zoomed in, so pixels stay crisp; bilinear
when zoomed out, to damp aliasing) and checkered — and paints that. Colour
management stays cached per tile, since converting is the expensive part.

Interactivity comes from doing less, not from a GPU: only damaged, visible
tiles recomposite. On a 100 MP document with three full-canvas blend layers
plus a curves adjustment, a 1920×1080 viewport recomposites in ~16 ms and a
single dirty tile — what a brush stroke actually costs — in ~3 ms
(`cargo run --release -p photoslop-compositor --example bench`). A `Compositor`
trait marks where a GPU backend would slot in; see PLAN.md §2 for why it
isn't there yet.

Tools declare a `group()`, so related tools share one toolbar slot with a
flyout and a shared shortcut letter — third-party tools can join an existing
group or form their own, and unknown groups sort after the built-ins.

## The GPUI boundary

The kernel and plugins never import GPUI. Tools receive `PointerInput` in
document space and return `Overlay` primitives; `crates/app` translates
between those and GPUI events/paint calls. Two consequences: tools are unit
testable with no window, and a GPUI upgrade touches one crate.

## PSD fidelity

Every block `codec-psd` doesn't interpret — layer effects, text engine data,
smart objects, unknown image resources — is preserved verbatim on the layer
or document and re-emitted on save. Unimplemented features therefore mean
"untouched", never "corrupted", which is why files survive a round trip long
before every feature exists.

## Third-party plugins

Plugins are WebAssembly modules loaded by `wasmtime` with exactly one host
import (`photoslop::log`) and a fuel budget. No filesystem, no network, no
clock, no randomness: isolation comes from what the sandbox lacks. See
[plugin-guide.md](plugin-guide.md).
