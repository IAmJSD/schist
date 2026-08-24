# Architecture

Schist is a microkernel plus plugins. The kernel owns *state and
contracts*; everything a user can see or click lives in a plugin.

```
crates/app              GPUI shell: window, canvas, panels, dialogs, keymap
├── crates/plugin-api   the trait surface every feature implements
├── crates/core         kernel: document, COW tiles, layers, history, selection
├── crates/color        pixel/colour primitives, depth conversion
├── crates/pixel-ops    CPU reference blend modes (the semantic contract)
├── crates/compositor   tile compositor, viewport resampling, damage cache
├── crates/compositor-gpu  wgpu compute backend, parity-tested against the CPU
├── crates/fx           blur/warp/carve kernels, same CPU-reference seam
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

Interactivity comes first from doing less: only damaged, visible tiles
recomposite. On a 100 MP document with three full-canvas blend layers
plus a curves adjustment, a 1920×1080 viewport recomposites on the CPU in
~16 ms and a single dirty tile — what a brush stroke actually costs — in
~3 ms (`cargo run --release -p schist-compositor --example bench`).

On top of that sits a **GPU backend** (`crates/compositor-gpu`). The
`Compositor` trait is the seam; `set_backend` routes every
`composite_*` call — canvas cache, tools, exports — through whichever
backend is active. The GPU implementation compiles the layer tree once
per composite into a flat op program (a per-pixel stack machine: push
layer, blend, clip-blend, adjust, mask), uploads the referenced tiles
and masks as storage buffers, and executes the whole batch in one
compute dispatch; `composite.wgsl` mirrors `pixel-ops` formula for
formula and parity tests in `compositor-gpu/tests` hold it to ±1 RGBA8
step. Viewport resampling (`compositor/src/viewport.rs` — the
crisp/bilinear/box zoom logic the canvas uses) has the same dual
implementation. GPUI doesn't expose its render device, so this is a
second wgpu instance and results come back over the bus — which batching
amortizes, and which is why the damage-driven single-tile path stays
cheap either way. Adjustment layers arrive as a per-channel LUT where one
exists and as an explicit shader branch where none does — hue/saturation,
black & white, threshold and posterize mix channels or step, so the plan
sends their coefficients instead of a table. Layers mid-drag and nesting
deeper than the shader's fixed stack fall back to the CPU reference per
call, bit-identically.

A **second seam** (`crates/fx`) covers the pixel work that is not
compositing: the separable box passes behind every Gaussian, the lens
blur's disc, and the mesh resample Liquify and Puppet Warp re-run on every
pointer move. Same shape as the compositor's — a `FxBackend` trait,
`set_backend`, a CPU reference that is the contract — with two additions
the compositor doesn't need. Jobs declare their arithmetic intensity and
small ones stay home (`worth_offloading`), because a round trip costs the
same bytes whatever happens in between. And a plane too big for one
storage binding is blurred in horizontal bands overlapping by
`passes * radius` rows, which is exactly the distance a vertical pass
spreads information, so the rows a band keeps are the ones the whole-image
pass would have produced. The warp can't do that — an arbitrary
displacement may read anywhere in its source — so it declines instead, and
in exchange keeps its source plane resident on the device for the length
of a drag, since only the mesh changes between pointer moves.

Content-Aware Scale sits behind the same seam but is a different shape:
not one sweep the caller repeats, but a loop the *backend* repeats.
Finding the lowest-energy seam, cutting it and recomputing is hundreds of
full-image passes for one command, so coming back between them would cost
more than the work; `fx_carve.wgsl` holds every stage and one command
runs the lot, with a single readback at the end. The awkward one is the
cumulative-cost scan, which walks the rows in order: a dispatch per row
would be tens of thousands per command, so a workgroup does a band of
rows at once, loading a span wider than it owns and letting the valid
region shrink by a column a row — exactly how far the ±1 dependency
spreads. That band index rides on a dynamic uniform offset rather than a
counter in a buffer, since bumping a counter would need its own dispatch
between every band. `compositor-gpu/examples/fxbench` measures all of it.

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
import (`schist::log`) and a fuel budget. No filesystem, no network, no
clock, no randomness: isolation comes from what the sandbox lacks. See
[plugin-guide.md](plugin-guide.md).
