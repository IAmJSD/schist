<img src="assets/logo/schist-512.png" alt="" width="88" align="right">

# Schist

A layered image editor written in Rust on [GPUI], with first-class PSD
support and a plugin-first architecture — every tool, filter, format and
menu command is a plugin, including the built-in ones.

[GPUI]: https://gpui.rs

**Status: v1 feature-complete,
plus two Photoshop-parity passes — 55 tools, 57 filters, 16 adjustments,
all nine layer effects, and live vector shapes.** 540 tests,
clippy-clean, verified end-to-end under a real window. [What is still
missing](#not-there-yet) is a short list now.

## Build and run

```sh
# Linux needs GPUI's system dependencies:
sudo apt-get install build-essential pkg-config libfontconfig-dev \
  libwayland-dev libxkbcommon-x11-dev libxcb1-dev libxcb-render0-dev \
  libxcb-shape0-dev libxcb-xfixes0-dev libvulkan-dev clang

cargo run --release -p schist-app -- [file.psd|file.png|…]
```

Those are the packages to *build* against. Running also needs a Vulkan
**driver** installed — GPUI renders through it, and the loader above is
only the part that finds one. On Debian and Fedora that is
`mesa-vulkan-drivers`; on Arch it is `vulkan-driver` (any of
`vulkan-radeon`, `vulkan-intel`, `nvidia-utils`, …), or `vulkan-swrast`
on a virtual machine with no GPU driver of its own. With no driver
installed Schist stops at startup and says which package is missing.

Schist also runs in the browser: `make web` assembles a static
deployment (WebGPU, chunked wasm, a loading page) into `dist/web/` —
see [docs/web.md](docs/web.md) for what's included and what isn't.

`cargo test --workspace` runs everything. `make helpers` cross-compiles
the Photoshop plug-in helpers, which are separate binaries for the
architectures a `.8bf` plug-in might be built for — see
[docs/8bf-host.md](docs/8bf-host.md#building-the-helpers). Packaging scripts for macOS,
Windows and Linux live in [packaging/](packaging); tagging `vX.Y.Z` builds
them all in CI, signing and notarizing the macOS bundle when the
[signing secrets](docs/versioning.md#signing-secrets) are set. All three
register `.psd`, `.psb`, `.afphoto`, `.afdesign`, `.afpub` and `.af` as
openable, never as the default handler: an installed Schist joins the
"Open with" menu rather than taking files off Photoshop or Affinity.

On macOS the bundle also carries two Quick Look app extensions, so
Finder draws real thumbnails for those files and the space bar opens a
real preview of them — layers, masks, blend modes and effects composited
by Schist itself, or the writing app's own embedded preview when that is
already big enough for the size asked for. Nothing needs enabling: the
extensions register when the app is first opened, and
`schist-quicklook --render file.afphoto out.png` shows what Quick Look
would show. See [docs/quicklook.md](docs/quicklook.md).

The logo is generated, not drawn: [`tools/logo.py`](tools/logo.py) holds the
geometry and `python3 tools/logo.py` re-emits the SVGs and every `.icns`,
`.ico` and `.png` the packaging needs. Edit the constants at the top of that
file, never its output.

## What it does

**Documents.** PSD and PSB read *and* write — layers, nested groups, masks,
all 27 blend modes, adjustment layers, layer effects, vector shapes,
8/16/32-bit, RGB, greyscale, CMYK, Lab and Indexed, RLE and zip-compressed
channels. Every block Schist doesn't understand is preserved
byte-for-byte, so a round trip never loses work. **Smart objects** keep
their source pixels, so transforming one repeatedly costs no more quality
than transforming it once. Also PNG, JPEG, WebP and TIFF, plus HEIC/HEIF import (iPhone photos)
and camera raw import: NEF, ARW, CR2, DNG, RAF, ORF, RW2, PEF, SRW and the
rest, developed with the camera's white balance to 16-bit sRGB. Raws
decode through Schist's own clean-room `schist-codec-raw` crate (pure
Rust, written from the public specifications and verified sample for
sample against LibRaw across some 250 camera files), which covers most
bodies, Canon's CR3 and Fuji's compressed RAF, Sigma's Foveon and
GoPro's VC-5 included; what it still declines — a couple of ancient
Sigma and Fuji sensors, Nikon's licensed High Efficiency NEF, and
cameras with no colour matrix in its table yet —
falls back to the system's LibRaw, loaded at runtime (`brew install
libraw`, or the distro's package); the browser build, which cannot load
one, refuses those few files and opens everything else. HEIC decodes through libheif the
same way: the system's copy if installed, otherwise Schist offers to
download a hash-pinned, decode-only build (with its LGPL license texts)
from [libheif-prebuilt](https://github.com/IAmJSD/libheif-prebuilt) —
nothing links at build time and the build stays pure Rust. Affinity files
(`.af`/`.afphoto`/`.afdesign`/`.afpub` — Affinity 1, 2 and the unified Canva-era format) open through a
natively reverse-engineered reader
([docs/affinity-format.md](docs/affinity-format.md)): pixel layers,
placed images, groups, masks, live shapes, free paths, editable text
(set in the document's real fonts, GPOS kerning and all), layer
effects — on groups too — and sixteen adjustment types come in as
real layers, each verified against renders and exports from Affinity
itself; whatever Affinity would re-render live and we can't rebuild
yet is covered by the file's embedded flattened preview, imported as
a hidden reference layer or, when nothing else survives, as the
document itself. Export writes layered `.af` documents back (unified
version-12 container): rasters as native tiles, groups, masks,
clipping, blend modes, drop shadows / glows / outlines / colour
overlays, and adjustment layers round-tripped from their preserved
native parameters — the writer's object graphs re-serialize every
fixture and corpus document byte-for-byte, so the on-disk shapes are
exactly what Affinity itself writes. Text and vector layers export as
pixels for now.

**Selecting.** Rectangular and elliptical marquee; free, polygonal and
magnetic lassos; magic wand with tolerance and contiguity; quick selection
that grows to match what you paint over; object selection that runs a
segmentation network — U^2-Net, from Filter ▸ Neural Filters ▸ Manage
Models — over the box you drew and cuts round what it finds, settling the
boundary against the picture's own colours, and falls back to reading the
box's border as background when there is no model or nothing in the box.
Then Modify (expand, contract, border, smooth, feather), Grow, Similar,
Colour Range, and save/load. Marching ants trace the selection's real
boundary — holes and all.

**Painting and retouching.** Brush, pencil, eraser, background eraser,
magic eraser, clone stamp, history brush, gradient, paint bucket,
dodge/burn/sponge, blur, sharpen, smudge; spot healing and healing brushes,
patch, content-aware move and red eye. Healing takes texture from the
source and colour from around the edge, so patching a blemish gives you
skin rather than a blurred blemish. Content-Aware Fill is a network and
a patch search working together — the network, trained here and shipped
in the binary, says what should be behind the hole, and the search finds
that in the photograph and copies it in, so what lands there is real
texture arranged the right way rather than either one's idea of an
average.

**Vector.** Pen, freeform pen and curvature pen draw paths that are
*stored*, so Path Selection and Direct Selection can edit them and Layer ▸
Path can fill, stroke or convert them to a selection. Rectangle, ellipse,
line (with its own weight, 45° constrain and arrowheads), polygon and six
custom shapes — as **live shape layers** by default, which keep their
path, regenerate their pixels from it, and survive a PSD round trip as
vectors. Editable text layers.

**Non-destructive.** Sixteen adjustments — levels, curves, hue/saturation,
brightness/contrast, black & white, colour balance, vibrance, exposure,
photo filter, gradient map, selective colour, channel mixer, invert,
posterize, threshold, solid colour — as layers, or applied straight to the
pixels from Image ▸ Adjustments. Layer masks, clipping masks, group
isolation, per-layer blend mode and opacity.

**Layer effects.** All nine: bevel & emboss, stroke, inner shadow, inner
glow, satin, colour overlay, gradient overlay, outer glow and drop shadow,
with Photoshop's Fill-vs-Opacity semantics so "Fill 0% plus a drop shadow"
does what you expect.

**Filters.** A hundred and thirty-four, which is Photoshop's Filter menu
with nothing left out: 3D, Artistic, Blur, Blur Gallery, Brush Strokes,
Distort, Noise, Pixelate, Render, Sharpen, Sketch, Stylize, Texture,
Video, Other, Camera Raw and Neural Filters, plus Lens Correction and
Adaptive Wide Angle. All preview live on the canvas inside the selection,
with Cancel restoring exactly. The count includes the forty-six effects
Photoshop keeps *inside* the Filter Gallery — Watercolor, Sumi-e, Chrome,
Stained Glass, Craquelure and the rest — which are ordinary filters here
too, so each can be run from the menu on its own or stacked in the
**Filter Gallery**, which previews the result of the lot.

Their dialogs match as well, which is most of what makes a filter feel
like the one you know: Mezzotint's ten screens, Lens Blur's iris shapes,
Smart Sharpen's choice of which blur it is undoing, Wave's several
generators, Diffuse's anisotropic mode, Extrude's pyramids, Wind's blast
and stagger.

And they read the same things Photoshop's read. The Sketch group draws in
the **foreground and background colours**, as do Clouds, Fibers, Tiles,
Neon Glow, Colored Pencil's paper and Diffuse Glow's glow — set the
swatches to sepia and Stamp comes out as a sepia print. **Displace**
warps through a map you pick from a file, red for horizontal and green
for vertical, stretched or tiled. **Flame** burns along the active path
when the document has one. **Lens Blur** can take its depth from the
layer's transparency or from what is underneath it, so part of the
picture stays sharp. Harmonization, Colour Transfer and Landscape Mixer
match against the layer below. Everything a filter needs beyond its own
pixels is gathered by the host and handed over, which is what
`FilterPlugin::wants_map`, `wants_path` and `wants_backdrop` are for.

What is left is ergonomic rather than functional: Photoshop puts blur
pins, light gizmos and flame paths on the canvas, and here they are
position sliders and the path you already drew.

**Warping.** Liquify with all seven brushes, Puppet Warp (Moving Least
Squares, so pins hold and nothing shears), Content-Aware Scale (seam
carving, with the selection as the protect mask), and Vanishing Point,
which clones along a perspective plane so the copy foreshortens with the
surface.

**Document furniture.** Artboards and slices, each exportable to its own
file; frames that clip their contents; notes; the Count tool; and layer
comps that capture every layer's visibility and appearance under a name.

**Colour.** ICC profiles honoured on open, assign vs. convert as separate
operations, a document→display transform, soft proofing, and ordered
dithering when exporting to 8-bit.

**GPU.** Compositing runs on the GPU when an adapter exists: the layer
tree — blend modes, masks, clipping, group isolation, all sixteen
adjustment kinds — compiles to a compute-shader program (wgpu), and
zooming, rotating and panning resample on the GPU too, which is what keeps
large documents responsive. The big filter sweeps go the same way: the box
passes behind every Gaussian, the lens blur's disc, and the displacement
resample Liquify and Puppet Warp re-run on every pointer move — the
operations that cross the whole selection per keystroke of a dialog, where
a lens blur at radius 60 is eleven thousand taps a pixel. **Content-Aware
Scale** runs there too, and runs *entirely* there: find the lowest-energy
seam, cut it, start again is hundreds of full-image passes for one
command, so the whole loop stays on the device and only the finished image
comes back. The CPU is the semantic reference throughout: parity tests
hold the GPU to it, anything it can't express (layers mid-drag) or can't
fit falls back for that call, and machines with no usable adapter just run
the CPU path. Toggle it in
Preferences, or override with `SCHIST_GPU=0` / `SCHIST_GPU=1`.

**Image.** Mode (RGB, greyscale, CMYK, Lab, Indexed), Auto Tone /
Contrast / Colour, image and canvas size, the five rotations and flips,
crop and trim.

**Editor.** Rotate View, rulers with drag-out guides, grid and snapping,
screen modes,
light/dark themes, navigator, history with click-to-jump, unlimited undo,
crash recovery, and a fully remappable keymap. Right-click the layers,
history, colour or navigator panels — or the canvas — for Photoshop-style
context menus (layer properties, duplicate, clipping mask, reorder, merge…).

## Keyboard

Photoshop's defaults (⌘ on macOS, Ctrl elsewhere):

| | |
|---|---|
| Tools | `V` move · `M` marquee · `L` lasso · `W` wand · `C` crop · `B` brush · `E` eraser · `S` clone · `J` spot healing · `Y` history brush · `G` gradient · `O` dodge · `P` pen · `A` path selection · `T` type · `U` shapes · `I` eyedropper · `H`/space hand · `Z` zoom |
| Tool groups | Shift+the tool's key cycles nested tools (Shift+`M` marquee ⇄ ellipse); hold or right-click a toolbar slot for its flyout |
| Edit | ⌘Z / ⌘⇧Z undo・redo · ⌘X/C/V · ⌘⇧C copy merged · ⌘T free transform |
| Select | ⌘A all · ⌘D deselect · ⌘⇧D reselect · ⌘⇧I inverse · shift/alt-drag to add/subtract |
| Layers | ⌘⇧N new · ⌘J duplicate · ⌘⇧J via cut · ⌘G group · ⌘E/⌘⇧E merge · ⌘[ ⌘] reorder · ⌘⌥G clipping mask |
| Adjust | ⌘L levels · ⌘M curves · ⌘U hue/sat · ⌘I invert |
| Fill | ⇧F5 Fill… · ⌥⌫ / ⌃⌫ fill with fore/background |
| View | ⌘0 fit · ⌘1 100% · ⌘R rulers · ⌘' grid · ⌘; guides · ⌘H extras · Tab/F screen modes · ⌘K preferences |
| Painting | `[`/`]` brush size · digits set opacity · `D`/`X` default・swap colours |

## Mouse and touchpad

Two-finger scroll pans; **Ctrl/⌘/Alt + scroll zooms** toward the pointer.
Prefer it the other way round? **Preferences ▸ Zoom with scroll wheel**
swaps them, so plain scrolling zooms and the modifier pans.

**Pinch-to-zoom** works on macOS, Linux — Wayland, and X11 on
xorg-server 21.1+ (XI 2.4) — and Windows touchscreens, zooming about the
centre of the gesture, and **stylus pressure** drives brush size on all
four backends. Upstream GPUI surfaces neither, so both come from a fork —
[IAmJSD/gpui](https://github.com/IAmJSD/gpui), which adds `PinchEvent`,
`on_pinch` and a `pressure` field on the mouse events on top of
gpui 0.2.2 — pinned by revision in the workspace `Cargo.toml`.

The one pinch gap left is Windows precision touchpads, and it is a gap by
choice. Windows delivers their pinches as Ctrl+scroll rather than as a
gesture — which is already a zoom here anyway — and the only way to see
the real gesture is Direct Manipulation, which cannot be claimed for
pinches alone: the same claim covers two-finger pans and takes over
scrolling with them. The fork implements it, behind
`GPUI_ENABLE_DIRECT_MANIPULATION=1`, but it is untested on hardware and
off by default rather than put in the path of ordinary scrolling. On a
pre-21.1 X11 server there is no pinch at all. Ctrl+scroll, the
zoom-with-scroll preference, ⌘+/⌘-, and the navigator's zoom slider work
everywhere.

Remap anything in `~/.config/schist/keymap.json`:

```json
{ "ctrl-shift-x": "command:edit.fill_foreground", "f1": "tool:brush" }
```

## Not there yet

- **CMYK and Lab edit in RGB.** Files open, edit and save in their own
  mode, converting at the boundaries; the editing in between is RGB, so
  individual ink channels are not separately editable.
- **Text is not on a path**, and the type engine has no OpenType
  feature controls.
- **The mesh warp and the carve need the layer to fit one storage
  binding.** A displacement may read anywhere in its source, and every
  seam depends on the one before it over the whole image, so neither can
  be split into bands the way the blurs are; on adapters at the 128 MB
  baseline a large layer falls back to the CPU. Real GPUs report bindings
  in the gigabytes and never reach it. The carve also stays on the CPU
  below a couple of megapixels, where its row-by-row scan costs more in
  dispatches than it saves.

## Diagnostics

Schist does not phone home. There are three things it can send over the
network and each is off until you turn it on: **Check for Updates**, the
on-demand font and model downloads, and crash reporting.

Crash reporting is two separate ticks in **Preferences ▸ Diagnostics**.
The first writes a report next to the crash-recovery snapshot in
`~/.local/state/schist/crashes/` and sends nothing anywhere. The second
also uploads it to the project's Sentry, and only appears on the official
releases — a build from source is given no DSN, so the code that would
report has nowhere to report to and never starts. Uploads carry no
personal data, no hostname and no breadcrumbs, and the panic message has
your home directory rewritten to `~` before it leaves, because a panic
tends to quote the path it choked on and that path is your work.

Set `SCHIST_CRASH_REPORTS=1` or `SCHIST_CRASH_UPLOAD=1` to turn either on
for a single run without changing preferences.

## Plugins

Third-party plugins are sandboxed WebAssembly — no filesystem, network or
clock, and a fuel budget so a runaway plugin can't hang the editor. A filter
is one function:

```rust
schist_filter! {
    id: "com.example.sepia",
    name: "Sepia",
    category: "Plugins",
    params: [param("amount", "Amount", 0.0, 100.0, 100.0, "%")],
    apply: |pixels: &mut [f32], _w: usize, _h: usize, params: &Params| { /* … */ }
}
```

Drop the `.wasm` in `~/.config/schist/plugins/` — or use **File ▸
Plugins…**, which also shows why anything failed to load. Full instructions
and a format example: [docs/plugin-guide.md](docs/plugin-guide.md).

## MCP

`schist-mcp` is a [Model Context
Protocol](https://modelcontextprotocol.io) server that drives Schist
headless — sessions instead of windows. Every canvas tool, menu command,
filter and adjustment in the registry the app uses is published as its
own MCP tool, with its own parameters described, plus inline PNG
rendering. It ships with every release; `cargo build --release -p
schist-mcp` builds it from source. See [docs/mcp.md](docs/mcp.md).

The same tool surface also powers the in-app **AI panel** (View ▸ AI
Panel): a sidebar that drives your installed `claude` or `codex` CLI
against the document you have open, edits streaming onto the canvas as
undoable history entries. No API keys — it uses the agent CLI you are
already logged into. See [docs/ai-panel.md](docs/ai-panel.md).

## Documentation

* [docs/architecture.md](docs/architecture.md) — how the pieces fit
* [docs/gallery.md](docs/gallery.md) — the Picasa-style photo gallery
* [docs/plugin-guide.md](docs/plugin-guide.md) — writing plugins
* [docs/mcp.md](docs/mcp.md) — the MCP server
* [docs/ai-panel.md](docs/ai-panel.md) — the in-app AI sidebar
* [docs/quicklook.md](docs/quicklook.md) — the macOS Quick Look extensions
* [docs/versioning.md](docs/versioning.md) — compatibility and releases
