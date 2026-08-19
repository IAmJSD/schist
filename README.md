# Photoslop

A Photoshop-class, plugin-first image editor written in Rust on [GPUI]
(Zed's GPU-accelerated UI framework), with native PSD/PSB support.

**Status: M0–M5 complete** (see [PLAN.md](PLAN.md) for the full roadmap):
tiled canvas with pan/zoom, full layer model (groups, masks, clipping, all
27 PSD blend modes), painting with unlimited undo, anti-aliased selections,
and a PSD/PSB reader with byte-level round-trip preservation of everything
it doesn't yet interpret.

[GPUI]: https://gpui.rs

## Building & running

Linux needs GPUI's system deps:

```sh
sudo apt-get install build-essential pkg-config libfontconfig-dev \
  libwayland-dev libxkbcommon-x11-dev libxcb1-dev libxcb-render0-dev \
  libxcb-shape0-dev libxcb-xfixes0-dev libvulkan-dev clang mold
cargo run -p photoslop-app --release [file.psd|file.png|…]
```

`cargo test --workspace` runs the full suite (105 tests: kernel, blend-mode
reference, compositor semantics, tools, commands, PSD corpus).

## Architecture (microkernel + plugins)

```
crates/core         kernel: document, COW tile store, layers, history, selection
crates/color        pixel/color primitives, depth conversion
crates/pixel-ops    CPU reference blend modes (the semantic contract)
crates/compositor   tile compositor + damage-driven cache
crates/plugin-api   the trait surface every feature implements
crates/codec-psd    standalone PSD/PSB reader (writer lands in M6)
crates/app          GPUI shell: workspace, canvas, panels, keymap
plugins/*           every user-facing feature: tools, commands, codecs
```

The kernel contains **zero** user-facing features. Tools receive input as
plain document-space events and return overlay primitives, so every tool is
unit-testable without a GUI. Pixels live in 256×256 copy-on-write tiles;
undo snapshots cost memory proportional to *changed* pixels only.

## Keybindings (defaults, ctrl on Linux/Windows ⇄ cmd on macOS)

Tools: `V` move · `M` marquee · `L` lasso · `W` wand · `B` brush ·
`E` eraser · `I` eyedropper · `H` hand (or hold Space) · `Z` zoom.
Commands: `⌘Z/⌘⇧Z` undo/redo · `⌘A/⌘D/⌘⇧I` select all/none/inverse ·
`⌘J` duplicate layer · `⌘E/⌘⇧E` merge down/visible · `⌘G` group ·
`⌘C/X/V` clipboard · `⌥⌫` fill · `[`/`]` brush size · digits = opacity ·
`⌘0/⌘1` fit/100% · `⌘O/⌘S` open/save.

Remap via `~/.config/photoslop/keymap.json`:
`{ "ctrl-shift-x": "command:edit.fill_foreground", "f1": "tool:brush" }`

## PSD support

Reads PSD and PSB, 8/16/32-bit, RGB + Grayscale: layers, nested groups,
masks, all blend modes, opacity/clipping/visibility, adjustment-layer
identification, RLE + raw compression. Text layers and smart objects import
as raster layers carrying their original data blocks. Every unknown byte
block is preserved verbatim so files survive a future save-and-reopen in
Photoshop without loss (the M6 writer re-emits them).
