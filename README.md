# Photoslop

A layered image editor written in Rust on [GPUI], with first-class PSD
support and a plugin-first architecture — every tool, filter, format and
menu command is a plugin, including the built-in ones.

[GPUI]: https://gpui.rs

**Status: v1 feature-complete (milestones M0–M12 of [PLAN.md](PLAN.md)).**
272 tests, clippy-clean, verified end-to-end under a real window.

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

**Editing.** Brush, pencil, eraser, clone stamp, gradient, paint bucket,
dodge/burn/sponge; rectangular/elliptical marquee, lasso, magic wand with
anti-aliased selections and boolean modifiers; move, crop, free transform
with rotate/scale/skew; pen and shape tools; editable text layers. Marching
ants trace the selection's real boundary — holes and all — and dragging a
layer moves its pixels live rather than an outline.

**Non-destructive.** Adjustment layers (levels, curves, hue/saturation,
brightness/contrast, black & white, invert, posterize, threshold, solid
colour), layer masks, clipping masks, group isolation, per-layer blend mode
and opacity. Destructive filters — blurs, sharpen, unsharp mask, noise,
median — apply inside the selection.

**Colour.** ICC profiles honoured on open, assign vs. convert as separate
operations, a document→display transform, soft proofing, and ordered
dithering when exporting to 8-bit.

**Editor.** Rulers with drag-out guides, grid and snapping, screen modes,
light/dark themes, navigator, history with click-to-jump, unlimited undo,
crash recovery, and a fully remappable keymap. Right-click the layers,
history, colour or navigator panels — or the canvas — for Photoshop-style
context menus (layer properties, duplicate, clipping mask, reorder, merge…).

## Keyboard

Photoshop's defaults (⌘ on macOS, Ctrl elsewhere):

| | |
|---|---|
| Tools | `V` move · `M` marquee · `L` lasso · `W` wand · `C` crop · `B` brush · `E` eraser · `S` clone · `G` gradient · `P` pen · `T` type · `U` shapes · `I` eyedropper · `H`/space hand · `Z` zoom |
| Tool groups | Shift+the tool's key cycles nested tools (Shift+`M` marquee ⇄ ellipse); hold or right-click a toolbar slot for its flyout |
| Edit | ⌘Z / ⌘⇧Z undo・redo · ⌘X/C/V · ⌘⇧C copy merged · ⌘T free transform |
| Select | ⌘A all · ⌘D deselect · ⌘⇧D reselect · ⌘⇧I inverse · shift/alt-drag to add/subtract |
| Layers | ⌘⇧N new · ⌘J duplicate · ⌘⇧J via cut · ⌘G group · ⌘E/⌘⇧E merge · ⌘[ ⌘] reorder · ⌘⌥G clipping mask |
| Adjust | ⌘L levels · ⌘M curves · ⌘U hue/sat · ⌘I invert |
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
