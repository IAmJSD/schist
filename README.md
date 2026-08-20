# Photoslop

A layered image editor written in Rust on [GPUI], with first-class PSD
support and a plugin-first architecture — every tool, filter, format and
menu command is a plugin, including the built-in ones.

[GPUI]: https://gpui.rs

**Status: v1 feature-complete (milestones M0–M12 of [PLAN.md](PLAN.md)),
plus a Photoshop-parity pass — 46 tools, 50 filters, 16 adjustments and
all nine layer effects.** 349 tests, clippy-clean, verified end-to-end
under a real window. [Gaps.md](#not-there-yet) lists what is still
missing.

## Build and run

```sh
# Linux needs GPUI's system dependencies:
sudo apt-get install build-essential pkg-config libfontconfig-dev \
  libwayland-dev libxkbcommon-x11-dev libxcb1-dev libxcb-render0-dev \
  libxcb-shape0-dev libxcb-xfixes0-dev libvulkan-dev clang

cargo run --release -p photoslop-app -- [file.psd|file.png|…]
```

`cargo test --workspace` runs everything. Packaging scripts for macOS,
Windows and Linux live in [packaging/](packaging); tagging `vX.Y.Z` builds
them all in CI.

## What it does

**Documents.** PSD and PSB read *and* write — layers, nested groups, masks,
all 27 blend modes, adjustment layers, 8/16/32-bit, RGB and greyscale. Every
block Photoslop doesn't understand (layer effects, text engine data, smart
objects) is preserved byte-for-byte, so a round trip never loses work. Also
PNG, JPEG, WebP and TIFF.

**Selecting.** Rectangular and elliptical marquee; free, polygonal and
magnetic lassos; magic wand with tolerance and contiguity; quick selection
that grows to match what you paint over; object selection that finds a
subject inside a drawn box. Then Modify (expand, contract, border, smooth,
feather), Grow, Similar, Colour Range, and save/load. Marching ants trace
the selection's real boundary — holes and all.

**Painting and retouching.** Brush, pencil, eraser, background eraser,
magic eraser, clone stamp, history brush, gradient, paint bucket,
dodge/burn/sponge, blur, sharpen, smudge; spot healing and healing brushes,
patch, content-aware move and red eye. Healing takes texture from the
source and colour from around the edge, so patching a blemish gives you
skin rather than a blurred blemish.

**Vector.** Pen, freeform pen and curvature pen draw paths that are
*stored*, so Path Selection and Direct Selection can edit them and Layer ▸
Path can fill, stroke or convert them to a selection. Rectangle, ellipse,
line (with its own weight, 45° constrain and arrowheads), polygon and six
custom shapes. Editable text layers.

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

**Filters.** Fifty across eight categories — Blur, Distort, Noise,
Pixelate, Render, Sharpen, Stylize and Other — all previewing live on the
canvas inside the selection, with Cancel restoring exactly.

**Colour.** ICC profiles honoured on open, assign vs. convert as separate
operations, a document→display transform, soft proofing, and ordered
dithering when exporting to 8-bit.

**Image.** Mode (RGB, greyscale), Auto Tone / Contrast / Colour, image and
canvas size, the five rotations and flips, crop and trim.

**Editor.** Rulers with drag-out guides, grid and snapping, screen modes,
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

**Pinch-to-zoom** works on macOS and Linux/Wayland, zooming about the
centre of the gesture. Upstream GPUI surfaces no pinch event at all, so
this is provided by a fork — [IAmJSD/gpui](https://github.com/IAmJSD/gpui),
which adds `PinchEvent` and `on_pinch` on top of gpui 0.2.2 — pinned by
revision in the workspace `Cargo.toml`.

There is no pinch under X11: XI2 has no pinch gesture, so touchpad
pinches are never forwarded to X11 clients, and Windows is not yet
implemented in the fork. Ctrl+scroll, the zoom-with-scroll preference,
⌘+/⌘-, and the navigator's zoom slider work everywhere.

Remap anything in `~/.config/photoslop/keymap.json`:

```json
{ "ctrl-shift-x": "command:edit.fill_foreground", "f1": "tool:brush" }
```

## Not there yet

Measured against Photoshop CC 2020, the notable gaps are:

- **Smart objects.** The PSD blocks are preserved byte-for-byte on a round
  trip, but nothing interprets or creates them.
- **Layer effects in PSD.** Effects render and edit, but `lfx2` descriptors
  are not *written*: saving a layer whose effects you have edited drops the
  stale block rather than writing back something the canvas is not showing.
- **Curves needs a graph editor.** The adjustment works and round-trips;
  only the sliders are missing, so it has no UI of its own.
- **Filter Gallery, Liquify, Camera Raw, Neural Filters, Vanishing Point.**
  Each is its own subsystem rather than a filter.
- **Transform Selection**, Puppet Warp, Content-Aware Scale.
- **Frame tool, artboards, layer comps, slices, the Notes and Count tools.**
- **3D.** Never planned. Adobe deprecated it right after CC 2020 and
  removed it in 2022.
- **CMYK and Lab documents**, and zip-compressed PSD channels.

## Plugins

Third-party plugins are sandboxed WebAssembly — no filesystem, network or
clock, and a fuel budget so a runaway plugin can't hang the editor. A filter
is one function:

```rust
photoslop_filter! {
    id: "com.example.sepia",
    name: "Sepia",
    category: "Plugins",
    params: [param("amount", "Amount", 0.0, 100.0, 100.0, "%")],
    apply: |pixels: &mut [f32], _w: usize, _h: usize, params: &Params| { /* … */ }
}
```

Drop the `.wasm` in `~/.config/photoslop/plugins/` — or use **File ▸
Plugins…**, which also shows why anything failed to load. Full instructions
and a format example: [docs/plugin-guide.md](docs/plugin-guide.md).

## Documentation

* [PLAN.md](PLAN.md) — the roadmap this was built against, with status
* [docs/architecture.md](docs/architecture.md) — how the pieces fit
* [docs/plugin-guide.md](docs/plugin-guide.md) — writing plugins
* [docs/versioning.md](docs/versioning.md) — compatibility and releases
