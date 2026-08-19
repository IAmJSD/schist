# Photoslop — Development Plan

> **Status (2026-08-19): M0–M5 complete.** Workspace + kernel + plugin
> registry (M0); tiled canvas with pan/zoom and codecs (M1); full layer
> model with all PSD blend modes and layers panel (M2); brush engine +
> history (M3); selections, masks, clipboard, fill (M4); PSD/PSB reader
> with round-trip preservation (M5). Verified end-to-end under Xvfb.
> Known deferrals within M0–M5: rotate-view rendering, animated marching
> ants (static outline for now), tablet pressure (mouse = 1.0), system
> clipboard interop (internal clipboard only), magnetic lasso (planned M8).

A GPU-accelerated, Photoshop-class raster/vector image editor written in Rust on **GPUI**
(Zed's UI framework), with first-class **PSD** support, a **plugin-first architecture**
(every feature — tools, filters, codecs, panels — is a plugin), full **layer** support,
and Photoshop-compatible **tools and keybindings**.

---

## 1. Product goals & non-goals

**Goals (v1.0):**
- Open, edit, and save PSD files with high fidelity (layers, groups, masks, blend modes,
  common adjustment layers), plus PNG/JPEG/WebP/TIFF import/export.
- Layered, non-destructive editing: raster layers, groups, layer masks, adjustment layers,
  opacity/blend modes, clipping masks.
- The standard tool set (move, marquee, lasso, wand, crop, brush, eraser, clone, gradient,
  bucket, pen, text, shapes, hand, zoom, eyedropper) with Photoshop-parity keybindings.
- 60fps canvas interaction on multi-hundred-megapixel documents via tiled rendering.
- Everything is a plugin: the core is a microkernel; all user-facing features ship as
  first-party plugins; third parties extend via sandboxed WASM plugins.
- Cross-platform: macOS, Linux, Windows (GPUI supports all three: Metal on macOS, Blade
  (Vulkan/DX12) elsewhere).

**Non-goals (v1.0):** CMYK print pipeline beyond basic conversion, video/animation
timelines, 3D, camera-raw development, generative/AI features, full smart-object editing
(we *preserve* smart objects on round-trip, we don't rasterize-edit them), scripting UI
automation. All are plugin-shaped and can come later.

---

## 2. Technology decisions

| Concern | Choice | Rationale |
|---|---|---|
| Language | Rust (stable, 2024 edition) | Requirement; safety + performance |
| UI framework | `gpui` | Requirement; retained/immediate hybrid, GPU-rendered, built-in **actions + keymap** system that maps perfectly onto "all the keybinds", element system for panels |
| Pixel engine (phase 1) | CPU tiles: `rayon` + SIMD (`std::simd`/`wide`) | Correctness first; deterministic, testable, no GPU interop risk |
| Pixel engine (phase 2) | `wgpu` compute offscreen → texture handoff to GPUI | GPU compositing/filters once semantics are locked by CPU reference impl |
| Geometry/paths | `kurbo` + `lyon` (tessellation) | Pen tool, shapes, vector masks |
| Text | `parley`/`swash` (layout+shaping), rendered into raster layers; GPUI text for UI | Text layers need our own layout, independent of UI text |
| Color management | `lcms2` (ICC profiles), documents in 8/16-bit int + 32-bit float per channel | PSD compatibility demands all three depths |
| PSD codec | **Own crate** (`codec-psd`) | Existing `psd` crate is read-only and too limited; writing PSD and round-tripping unknown blocks requires full control |
| Other codecs | `image`/`zune-*` wrapped as codec plugins | Cheap wins |
| Plugin sandbox | `wasmtime` (WASM + WASI, capability-based host API) | Safe third-party plugins; first-party plugins are native Rust compiled into the workspace |
| Serialization | Native project format: PSD is *the* save format; app-private sidecar for extras | "Support PSDs" as the core promise, no lock-in format |
| Undo | Command pattern + copy-on-write tile snapshots | Bounded memory, fast, works for any plugin-issued edit |

**Key architectural risk called out up front:** GPUI renders with its own GPU backend and
does not hand out a raw device for arbitrary compute. Mitigation is the two-phase pixel
engine above: Phase 1 composites tiles on CPU and uploads dirty tiles as GPUI images
(fast enough with tiling + damage tracking); Phase 2 moves compositing to wgpu rendering
into shared textures. All compositing goes through one `Compositor` trait so the swap is
invisible to the rest of the app.

---

## 3. Architecture: microkernel + plugins

```
┌─────────────────────────────────────────────────────────────┐
│  app shell (GPUI): windows, workspace, docking, menus       │
├─────────────────────────────────────────────────────────────┤
│  KERNEL (crates/core)                                       │
│   • Document model (layers, tiles, selections, history)     │
│   • Registries: tools, commands, codecs, filters, panels    │
│   • Event bus (document/selection/tool events)              │
│   • Command dispatch + keymap binding (via GPUI actions)    │
│   • Plugin host: native (in-process) + WASM (sandboxed)     │
├────────────┬───────────────┬───────────────┬────────────────┤
│ tool       │ filter/adjust │ codec plugins │ panel plugins  │
│ plugins    │ plugins       │ (psd, png…)   │ (layers, hist…)│
└────────────┴───────────────┴───────────────┴────────────────┘
```

### 3.1 The kernel is small on purpose
The kernel owns *state and contracts only*: the document model, the tile store, the
history log, the registries, and the event bus. It contains **zero** user-facing
features. If you delete every plugin, the app boots to an empty workspace that can do
nothing — that's the test of "everything is a plugin."

### 3.2 Plugin contracts (crates/plugin-api)
One crate defines the stable trait surface. Everything user-facing implements one of:

- **`ToolPlugin`** — pointer/keyboard event handlers, cursor, tool-options UI schema,
  overlay painting (marching ants, transform handles). Activated exclusively (one active
  tool), registered with an id, icon, default keybind.
- **`FilterPlugin` / `AdjustmentPlugin`** — pure function over pixel regions
  (`fn apply(&self, region: &mut TileRegion, params: &Params)`), a params schema
  (auto-generates the dialog UI), and a flag for "can run as adjustment layer"
  (non-destructive) vs destructive-only.
- **`CodecPlugin`** — `probe(bytes) -> Confidence`, `import(reader) -> Document`,
  `export(doc, writer, options)`, declared extensions/MIME types.
- **`PanelPlugin`** — a GPUI view factory + dock placement (layers panel, history,
  color, brushes, navigator are all this).
- **`CommandPlugin`** — a bag of named commands (GPUI actions) with default keybinds;
  how menu items and chords like `Cmd+Shift+E` (merge visible) register.
- **`RendererPlugin`** — layer-type renderers (raster, text, shape, adjustment, smart
  object) so new layer kinds can be added without touching the kernel.

**Two hosting tiers, one API:**
- *First-party (native)*: workspace crates compiled in, registered at startup via an
  inventory of `PluginManifest`s. Full-speed, full-trust. This is how the entire built-in
  feature set ships.
- *Third-party (WASM)*: same conceptual API projected over a wasmtime host with
  capability-based imports (read/write pixel regions they were handed, declare params UI,
  no ambient filesystem/network). Filters and codecs first; tools later (they need the
  event stream). ABI versioned from day one (`api_version` in the manifest).

### 3.3 Document model (crates/core)
- `Document { canvas_size, color_mode, depth, icc_profile, root: GroupLayer, guides, resolution, unknown_psd_blocks }`
- `Layer` enum: `Raster`, `Group`, `Adjustment(kind, params)`, `Text(TextSpec)`,
  `Shape(VectorSpec)`, `SmartObjectRef` (opaque, preserved). Common fields: name, id,
  visible, locked flags, opacity, fill-opacity, blend mode, layer mask (raster), vector
  mask, clipping flag, layer effects (preserved; rendered subset later).
- **Tile store**: pixels live in 256×256 tiles, reference-counted and copy-on-write.
  Layers share tiles until written. This single decision powers cheap undo snapshots,
  cheap layer duplication, damage-region compositing, and memory-mapped scratch for
  huge documents.
- **Blend modes**: all PSD modes (normal, dissolve, darken family, lighten family,
  contrast family, inversion family, component/HSL family) implemented once in
  `crates/pixel-ops` as the CPU reference, mirrored later in WGSL. Golden-image tests
  against Photoshop-produced reference PSDs.
- **Selections** are 8-bit coverage masks in the same tile store (anti-aliased,
  featherable) — not just rects. Selection = the universal "where edits apply" input to
  every tool and filter.
- **History**: an ordered log of `Edit` objects; each `Edit` stores the tile references
  it replaced (COW makes this nearly free) plus structural deltas (layer add/remove/
  reorder/property). Linear undo/redo in v1; the log design permits history-panel
  snapshots later.

### 3.4 Rendering pipeline
1. Tool/filter mutates tiles → marks damage rects on the document.
2. `Compositor` recomposites only damaged tiles bottom-up through the layer tree
   (groups composite to intermediate tile buffers; clipping masks and masks applied
   per spec order: mask → fill-opacity/blend → clip group → opacity).
3. Composited tiles feed the canvas view, which draws visible tiles (zoom-aware
   mip levels for far zoom-out), plus overlay pass (selection ants, guides, transform
   handles, brush cursor) drawn by the active tool plugin via GPUI paint.
4. Checkerboard transparency, pixel grid at ≥800% zoom, rotation/flip of the *view*
   (R key) is a canvas transform, never touches pixels.

---

## 4. PSD support (crates/codec-psd)

PSD is the make-or-break feature, so it gets its own sub-plan. Format facts that shape
the design: big-endian; five sections (File Header, Color Mode Data, Image Resources,
Layer & Mask Info, merged Image Data); PackBits RLE; layer records with "additional
layer information" keys; **PSB** variant for documents over 30,000px.

**Read (milestone M5):**
- Header + merged composite (instant preview while layers parse in background).
- Layer records: bounds, channels (incl. mask + real-mask), blend key, opacity, clipping,
  flags; groups via `lsct` section dividers; Unicode names `luni`; layer colors `lclr`.
- Adjustment layers: map the common keys (levels `levl`, curves `curv`, hue/sat `hue2`,
  brightness/contrast `brit`, black&white `blwh`, solid color `SoCo`, gradient `GdFl`,
  pattern `PtFl`, invert/posterize/threshold) to our adjustment plugins.
- Text layers `TySh`: parse enough to *render approximately* (string, font, size, color,
  transform) and always keep the raw block for round-trip.
- Smart objects (`SoLd`/`PlLd` + linked/embedded data): imported as `SmartObjectRef`
  showing the embedded preview; never destroyed.
- 8/16/32-bit, RGB + Grayscale in v1; Lab/CMYK convert-on-import with a warning.

**Round-trip preservation rule (the fidelity strategy):** every byte block we don't
understand — unknown image resources, unknown layer-info keys, effects `lfx2`, patterns,
paths — is retained verbatim on the layer/document and re-emitted on save, unless the
user destructively edits that specific layer. This is how we achieve "open in Photoshop
and nothing broke" long before we implement every feature.

**Write (milestone M6):** emit header, resources (incl. preserved), full layer tree with
RLE-compressed channels, masks, groups, blend keys, and a freshly composited merged
image (so old/simple viewers still see the flattened result). PSB emitted automatically
when dimensions require it.

**Verification:** corpus of fixture PSDs (authored in Photoshop/Krita/Affinity, plus
generated) with golden tests: parse → composite → compare against Photoshop's own
flattened output within per-blend-mode tolerance; write → re-parse → structural equality;
write → open in `psd`-crate/Krita as third-party sanity checks.

---

## 5. Tools & keybindings (Photoshop parity)

Keybinds live in the GPUI keymap, user-remappable via a JSON keymap file; every binding
below is a default, not a hardcode. Single-letter tool binds are context-gated to
"canvas focused, not typing."

**Tool roster (each is a `ToolPlugin`):**

| Key | Tool(s) (Shift+key cycles) | Milestone |
|---|---|---|
| V | Move | M2 |
| M | Rect marquee / Ellipse marquee | M4 |
| L | Lasso / Polygonal / Magnetic | M4 (magnetic M8) |
| W | Quick Select / Magic Wand | M4 |
| C | Crop | M7 |
| I | Eyedropper | M3 |
| B | Brush / Pencil | M3 |
| E | Eraser | M3 |
| S | Clone Stamp | M7 |
| G | Gradient / Paint Bucket | M7 |
| O | Dodge / Burn / Sponge | M8 |
| P | Pen (Bézier) | M7 |
| T | Type (horizontal/vertical) | M7 |
| U | Shapes (rect, ellipse, line, polygon, custom) | M7 |
| H | Hand (pan) — Space = spring-loaded | M1 |
| R | Rotate view | M1 |
| Z | Zoom — Cmd+= / Cmd+- / Cmd+0 fit / Cmd+1 100% | M1 |
| D / X | Default colors / Swap colors | M3 |
| \[ / \] | Brush smaller/larger; Shift+\[/\] hardness | M3 |
| Tab / F | Hide panels / cycle screen modes | M11 |

**Command keybinds** (registered by the relevant plugin; ⌘=Ctrl on Linux/Windows):

- File: `⌘N` new, `⌘O` open, `⌘S` save, `⌘⇧S` save as, `⌘⌥⇧S`/`⌘⌥⇧W` export, `⌘W` close, `⌘P` print (later)
- Edit: `⌘Z` undo, `⌘⇧Z` redo, `⌘X/C/V` cut/copy/paste, `⌘⇧C` copy merged, `⌘⇧V` paste in place, `⌘T` free transform (Enter commit / Esc cancel), `⇧F5` fill, `⌘K` preferences
- Select: `⌘A` all, `⌘D` deselect, `⌘⇧D` reselect, `⌘⇧I` inverse, `⌘⌥R` refine edge (later); Shift-drag add, Alt-drag subtract
- Layers: `⌘J` duplicate, `⌘⇧J` cut to new layer, `⌘⇧N` new layer, `⌘E` merge down, `⌘⇧E` merge visible, `⌘G` group, `⌘⇧G` ungroup, `⌘⌥G` clipping mask, `⌘[`/`⌘]` move down/up, `⌘⇧[`/`⌘⇧]` to bottom/top, `⌥`-click visibility = solo
- Image/adjust: `⌘L` levels, `⌘M` curves, `⌘U` hue/sat, `⌘I` invert, `⌘⌥I` image size, `⌘⌥C` canvas size
- Painting: digits set tool opacity (5→50%, 05→5%), `⌥`-click = eyedropper while painting, `⇧`-click = line stroke
- View: `⌘R` rulers, `⌘'` grid, `⌘;` guides, `⌘H` show/hide extras

---

## 6. Workspace layout

```
photoslop/
├── Cargo.toml                # workspace
├── crates/
│   ├── app/                  # binary: GPUI bootstrap, workspace/docking, menus
│   ├── core/                 # kernel: document, tiles, history, registries, events
│   ├── plugin-api/           # stable traits + types all plugins compile against
│   ├── plugin-host-wasm/     # wasmtime host, capability layer, ABI
│   ├── pixel-ops/            # blend modes, brush stamping, samplers (SIMD)
│   ├── compositor/           # Compositor trait + CPU impl (+ wgpu impl later)
│   ├── color/                # ICC, depth conversion, color types
│   ├── codec-psd/            # standalone PSD/PSB reader-writer (publishable)
│   └── text-engine/          # text layer layout/shaping/raster
├── plugins/                  # first-party, all optional at compile time
│   ├── tools-basic/          # move, hand, zoom, eyedropper
│   ├── tools-paint/          # brush, pencil, eraser, clone, gradient, bucket
│   ├── tools-select/         # marquees, lassos, wand/quick-select
│   ├── tools-vector/         # pen, shapes
│   ├── tools-type/           # text tool
│   ├── tools-transform/      # crop, free transform
│   ├── filters-core/         # blur, sharpen, noise, etc.
│   ├── adjustments-core/     # levels, curves, hue/sat, B&C, invert…
│   ├── codecs-common/        # png/jpeg/webp/tiff via CodecPlugin
│   ├── panels-core/          # layers, history, color, tool options, navigator
│   └── commands-core/        # menu/keybind command bags (file, edit, select…)
├── fixtures/psd/             # golden-test corpus
└── docs/                     # architecture notes, plugin-author guide
```

CI from M0: fmt, clippy (deny warnings), tests on the three OS targets, golden-image
tests, a `cargo deny` pass, and a benchmark job (criterion) for composite/brush hot paths.

---

## 7. Milestones

Each milestone has a demoable exit criterion. Order front-loads the two highest risks:
GPUI-as-app-shell (M0–M1) and PSD fidelity (M5–M6). Rough sizing assumes 1–2 experienced
Rust devs; treat as relative weights.

**M0 — Skeleton & kernel (2–3 wks).** Workspace, CI, GPUI window with docked-panel
workspace shell, action/keymap loading (user JSON keymap file), plugin registries + event
bus, empty document opens. *Exit: app boots, panels dock, a demo command bound to a
chord fires.*

**M1 — Canvas & tiles (3 wks).** Tile store, document model, CPU compositor v0 (normal
blend), canvas view with pan/zoom/rotate-view (H, Space, Z, R, ⌘0/⌘1), checkerboard,
open PNG via codec plugin. *Exit: open a 200MP PNG, pan/zoom at 60fps.*

**M2 — Layers (3 wks).** Full layer tree (groups, visibility, lock, opacity, all blend
modes in CPU reference), layers panel plugin (reorder, rename, thumbnails), move tool,
`⌘J/⌘E/⌘⇧E/⌘G` etc. *Exit: multi-layer composition matching Photoshop reference renders
in golden tests.*

**M3 — Painting & history (3–4 wks).** History log + undo/redo, brush engine (stamped,
spacing, hardness, pressure via tablet events, smoothing), brush/pencil/eraser tools,
color panel + eyedropper, D/X/`[`/`]`/digit-opacity binds. *Exit: paint a picture; undo
100 strokes instantly; memory stays bounded.*

**M4 — Selections & masks (3 wks).** Coverage-mask selections, marquee/lasso/wand tools,
marching ants overlay, boolean modifiers (Shift/Alt), feather, `⌘A/D/⇧I`, cut/copy/paste
(system clipboard interop), layer masks + clipping masks, selection-clipped painting and
fill (`⇧F5`). *Exit: composite a cutout between layers using only keyboard+mouse like in
Photoshop.*

**M5 — PSD read (3–4 wks).** `codec-psd` reader per §4: layers, groups, masks, blend
modes, adjustment mapping, text-approx, unknown-block preservation, 8/16/32-bit, PSB.
Background loading with instant merged-image preview. *Exit: fixture corpus opens with
golden-composite match; a gnarly real-world PSD opens correctly.*

**M6 — PSD write & round-trip (2–3 wks).** Writer + preservation pipeline; save/save-as
flows, dirty tracking, autosave/crash recovery journal. *Exit: open → edit one layer →
save → reopen in Photoshop/Krita with everything intact.*

**M7 — Transform, crop, vector, text (4 wks).** Free transform (`⌘T`: scale/rotate/skew,
resampling filters), crop tool, image/canvas resize, pen + shape tools (shape layers via
lyon), text tool + text layers (editable, rasterized on demand), clone stamp, gradient +
bucket. *Exit: design a simple poster from scratch and save as a layered PSD.*

**M8 — Adjustments, filters, GPU compositor (4 wks).** Adjustment-layer runtime
(non-destructive, mask-aware): levels, curves, hue/sat, brightness/contrast, invert,
posterize, threshold, black&white; destructive filters: gaussian/box/motion blur,
sharpen/unsharp, add-noise/median; dodge/burn; wgpu compositor behind the `Compositor`
trait, validated tile-for-tile against the CPU reference. *Exit: `⌘M` curves on an
adjustment layer previews live at 60fps on a 100MP doc.*

**M9 — Third-party plugins (3–4 wks).** WASM host: manifest + capability grants, filter
and codec plugin ABI, params-schema-driven dialogs, plugin manager panel (install from
file/dir, enable/disable), versioned API docs + `photoslop-plugin` template crate +
two example plugins (a filter, a format). *Exit: an external dev builds a working filter
against the template without touching our repo.*

**M10 — Color management & export (2 wks).** ICC profile honor/assign/convert, display
profile transform in the compositor, soft-proof toggle, export dialog (format options,
quality preview), bit-depth conversions with dithering. *Exit: wide-gamut PSD displays
correctly on sRGB and P3 monitors.*

**M11 — Parity & polish (3 wks).** Full keybind audit vs §5 table, remappable-keymap UI,
preferences (`⌘K`), themes (light/dark), screen modes (Tab/F), rulers/guides/grid/snap,
navigator panel, performance hardening (profiles on 500MP docs, scratch-disk spill),
accessibility pass on panels. *Exit: a Photoshop user's muscle memory works.*

**M12 — Release engineering (2 wks).** Signed/notarized macOS bundle, Windows installer,
Linux AppImage/Flatpak, crash reporting (opt-in), update channel, docs site with the
plugin-author guide, versioning/branching policy. *Exit: v1.0 tagged, installable by a
stranger.*

**Total: roughly 8–10 months** for 1–2 devs at the stated scope; parallelizable to ~5–6
months with 3 (codec-psd and the plugin host are cleanly separable workstreams).

---

## 8. Risks & mitigations

1. **GPUI is young as a third-party app framework** (API churn tracks Zed's needs).
   → Pin the version per milestone; isolate GPUI types behind the app/panel layer so
   plugins (except panels) never see them; upgrade on a schedule, not continuously.
2. **GPUI ↔ custom GPU compute interop** may be awkward. → CPU compositor is the
   contract-holder and permanent fallback; GPU path is an optimization, never a
   correctness dependency (M8, not M1).
3. **PSD fidelity is a long tail** (effects, text engine, smart objects). → The
   preservation rule in §4 turns "unimplemented" into "untouched" instead of "corrupted";
   fidelity grows feature-by-feature without ever losing user data.
4. **Blend-mode/color math mismatches Photoshop** in subtle ways (e.g. non-linear
   compositing conventions). → Golden-image corpus from real Photoshop output, tolerance
   thresholds per mode, tested from M2 onward.
5. **Plugin ABI regret.** → Third-party ABI is WASM-only in v1 (no native dylib promises);
   `api_version` gating from day one; first-party plugins use in-repo traits that can
   still evolve freely until M9 freezes the external surface.
6. **Undo memory blow-up on huge docs.** → COW tiles make snapshots proportional to
   *changed* pixels; history size cap + scratch-disk spill in M11.
7. **Tablet/pressure input across three platforms.** → Abstract input in M3 behind one
   event type; ship mouse-simulated pressure fallback; platform quirks are contained.

---

## 9. Definition of done (v1.0)

- Opens and saves the PSD fixture corpus with round-trip fidelity (golden tests green).
- All §5 tools and keybinds implemented and remappable.
- Layers: raster/group/adjustment/text/shape, masks, clipping, all PSD blend modes.
- Kernel contains no features; `plugins/` can be compiled out individually; one
  third-party WASM plugin demonstrably works from the public template.
- 60fps pan/zoom/paint on a 100MP document on a mid-range laptop; no data loss on crash
  (recovery journal).
- Ships as signed installers for macOS, Windows, Linux.
