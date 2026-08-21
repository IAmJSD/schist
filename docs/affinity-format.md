# The Affinity file format, reverse engineered

What Schist knows about Serif's `.afphoto` / `.afdesign` / `.afpub`
format, as implemented by `crates/codec-affinity`. Serif publishes no
spec. This knowledge comes from prior art — [afread] by Vladimir Mamonov
(MIT) and [AFDesignLoad] by Nick Beeuwsaert (MIT) — plus our own
inspection of real files: `fixtures/affinity/` (Affinity Designer 1.x)
plus private corpora of Affinity Photo 2.6 documents and Canva-era
Affinity `.af` documents (not vendored — point
`SCHIST_AFFINITY_CORPUS` at colon-separated directories of real
files to sweep them in the codec tests). Every generation uses the
same container and object graph: Affinity 2 swaps zlib for zstd, adds
a few fields, and stores placed-image pixels by reference; the unified
`.af` format bumps the container version to 12 with **no other
observed change**.

All integers are little-endian. Two tag conventions coexist, and mixing
them up costs an afternoon: **container** tags (`#Inf`, `Prot`, `#FAT`,
`#Fil`) are stored as literal byte sequences, while **class and field**
tags in the object graph are stored reversed (`DocR` on disk is
`RcoD`). Signed sizes below are `u32` unless noted.

## Layer 1 — the container (a tiny versioned filesystem)

```text
00 FF 4B 41            magic
u16  version           7–11 seen (Affinity 1/2); 12 = unified ".af"
u16  flags             0; low bits nonzero → layout we don't know
u32  class tag         "Prsn" (persona/document); other values exist
                       for presets, brushes, macros…
"#Inf"                 info block:
  u64 fat_offset       → first FAT block
  u64 thumb_offset     → thumbnail block when nonzero
  u64 length, u64 ?, u64 creation_date, u32 revision, u32 ?
version > 7: "Prot" + u32 protocol revision
```

**Thumbnail.** At `thumb_offset`: `FF FF FF FF` `"Thmb"` · u32 count ·
u32 block size, then a 13-byte header (`u32 ?, u32 0, u32 png_size,
u8 ?`) and a PNG — the writing app's own 512×512 render of the
document. `Archive::thumbnail()` carves it; the codec tests use it as
ground truth for shape geometry, comparing what we rebuild against what
Affinity actually drew.

**FAT chain.** At `fat_offset`, blocks tagged `#FAT`, `#FT2`, `#FT3` or
`#FT4` (successive revisions of the same structure), linked by a
next-offset. Each block is one **savepoint** — Affinity persists undo
history as whole FAT generations:

```text
u32 tag · u64 next_offset · u64 creation_date · u64 ? · u64 ? · u64 ?
u32 files_count · u32 ? · u32 ? · u16 dirs_count · u8 ?
files_count × entry:
  u32 id · u8 flag        flag: 0 = named, 1 = revision, 2 = deleted
  flag ≠ 2: u64 offset · u64 size · u64 compressed_size · u32 crc32
            u8 compression
            #FT2/#FT3/#FT4 add a u32; #FT4 adds one more
  flag = 0: u16 name_len · name   ("doc.dat", "d/1", "d/2"…)
dirs_count × (u16 name_len · u16 0 · u64 files_num · name)
```

The *head* revision of a name is the entry bearing its id in the newest
savepoint. `#FAT`/`#FT2` store a compression *index* which maps to the
full byte: 1→0x01, 2→0x41, 3→0x81, 4→0xC1.

**Entry payload.** At `offset`: literal `#Fil`, then the data.
Compression byte: bits 0–1 = algorithm (0 stored, 1 zlib, 2 zstd),
bits 6–7 = prediction type, bit 5 = a variant flag. After
decompression, prediction is undone: type 1 = byte-wise delta
(cumulative sum), type 2 = u16-wise delta; with bit 5 set on type 2,
byte-wise delta then, for exactly-64 KiB payloads, a low/high byte-plane
re-interleave. A CRC-32 (zlib polynomial) of the plain bytes closes the
loop. Every entry in every fixture round-trips with matching CRC.

## Layer 2 — the object graph ("doc.dat")

```text
00 FF 4B 53 · u16 file_ver (≤2) · u32 root class tag ("Pers") · u16 ver
file_ver = 2: extra u32
```

then a **field stream**: `u8 type` (bit 7 = array) · `u32 tag` · value,
terminated by type 0x00. Types:

| type | value |
|---|---|
| 0x01–0x0A | u8 u16 u32 u64 i8 i16 i32 i64 f32 f64 |
| 0x15–0x19 | i32 vector, 2–6 wide |
| 0x1F–0x23 / 0x24–0x28 | f32 / f64 vector, 2–6 wide |
| 0x29 | bool (arrays are bit-packed) |
| 0x2A | enum: u16 id · u16 version (arrays: count, then one version, then ids) |
| 0x2B, 0x2E | string: u32 len + UTF-8 (arrays prefix a total byte size) |
| 0x2C | curve records: u16 size ∈ {12,16,18,24,32}, raw |
| 0x2D | binary: u32 size + bytes |
| 0x2F, 0x34 | u32 |
| 0x30 | nested class, untagged fields |
| 0x31 | nested class: u8 status — 0 null · 1 definition (u32 shared id, then type sections, then fields) · 2 link to an id |
| 0x32 | nested class: u8 status — 0 null · 1 definition (u32 tag + **u16** id, then fields) |
| 0x33 | embedded data: u32 tag · u32 len · string — **names another container entry** |
| 0x35–0x74 | fixed struct of (type − 0x34) bytes (colors etc.), raw |
| 0x75 | flag set: u16 version · u8 count ≤ 8 · count bytes |

A 0x31 definition's *type sections* encode the class hierarchy: repeat
`u8 0` · u32 tag · u16 version · fields (a base class's fields, same
flat namespace), until `u8 1` · u32 tag (root of the hierarchy) or
`u8 2`. Arrays of 0x32 hoist one shared tag+id header before the
elements. Links (0x31 status 2) always reference an id defined earlier
in the stream.

## Layer 3 — the document model

```text
Pers                        persona (root)
├─ OVer/NVer: ApVs          writing app: name, version, build, platform
└─ DocR: DocN               document node
   ├─ DfSz: f64[2]          canvas size (Photo; Designer uses SprB)
   └─ Chld: [Sprd]          spreads / artboards
      ├─ SprB: f64[4]       spread bounds [x0 y0 x1 y1] — the canvas
      ├─ RasS: SRst         spread base raster (Bitm inside); Photo 2
      │                     leaves an *evicted* composite cache here —
      │                     format 6, every tile status 0, no data
      └─ Chld: [layer]      layer tree, recursively via Chld
```

Layer node type tags seen: `Scop` (Designer's layers-panel "Layer" —
a container; treat as group), `Grup` (group), `Rstr` (pixels), `ImgN`
(placed image — carries both the original file *and* rendered pixels),
`ShpN` (live shape), `PCrv` (curves/path), `TxtA`/`TxtF` (text),
`FlRN` (fill layer), plus adjustment types. Common fields: `Desc` name
· `Visi` visible · `Opac` opacity (f32) · `Blnd` blend enum · `PasT`
false = a group isolates (default passthrough) · `Xfrm` f64[6]
**row-major** 2×3 transform `[sx kx tx ky sy ty]` · `BitR` i32[4]
on-canvas pixel rect (Photo; `i32::MIN+1` sentinels when unset) ·
`FiEf` effects · `AdCh` mask/adjustment children. Transforms are full
affine: import composes them exactly for vector layers and resamples
rasters and masks through rotation/shear.

**Text** (`TxtA` artistic / `TxtF` frame): no pixels, but the full
model. `StSt` (story) → `Blok` blocks → `Glyp` (`GStr`) holds the
UTF-8 string; each block's `GAtt` → `Runs` → `Item` carries character
attributes — `Doub[0]` is the font size, `RFnt`/`DFnt` the resolved
font (`Post` PostScript name, `Famy` family), and `Objs` holds fill
descriptors (`FDsc.FDeF` → `Colr` → an `RGBA`/`HSLA`/… `_col` struct).
`TxtH` is the frame (`ArFr` for artistic text): `FrmB` `f64[4]` is the
layout box in pre-transform coordinates — its transformed bottom edge
is the first baseline, its height the visual cap height (`ArtV`).
Import re-sets the text through the text engine, anchors the rendered
ink box to the transformed frame box (fitting the size to the frame
width when a substituted font's metrics disagree), and stores the type
tool's `PsTx` block so the layer stays editable.

**Layer transforms nest**: a group's `Xfrm` defines the coordinate
space its children's transforms (and `BitR` rects) live in; composing
down the tree is what places deeply-nested tiles correctly.

**Shapes** (`ShpN`): the layer's `ShpB` `f64[4]` local bounds plus a
`Shpe` class giving the kind and parameters; geometry is built in the
local box and pushed through the composed transform. Shape classes
repeat a tag across their base-class sections (a base default followed
by the derived value — `ShSt` carries `IRad` 0.5 then 0.382); the
*last* occurrence is the one that renders. Kinds with rebuilt geometry,
all verified against the files' own thumbnails:

- "ShNR"/"ShRR" rectangles: `ShCR` per-corner radii (fractions of half
  the shorter side unless `AbSz`), but a radius only applies where the
  `CTyp` corner-type array says rounded (enum 0) — Designer's plain
  rectangle keeps default radii in `ShCR` with no `CTyp` and renders
  sharp.
- "ShpE"/"ShCE" ellipse.
- "ShSt" star: `Pnts` points alternating between the inscribed ellipse
  and `IRad` of it, first point up; `CrvL`/`CrvR` bend the edges (only
  straight-edged stars are rebuilt).
- "ShSS" square star: `Side` rectangular arms, the first pointing
  down. Arms are the middle `COut` of each edge of the regular
  `Side`-gon inscribed in the ellipse — flat tips at the polygon's
  apothem `cos(π/N)`, half-width `COut·sin(π/N)`, adjacent arms
  meeting in a V notch at `COut` of the radius.
- "ShCl" cloud: `Bubl` circular arcs bulging out around the inscribed
  ellipse, meeting at `IRad` of the radius.
- "ShHt" heart: two lobes over a point; `Sprd` deepens the notch
  (proportions traced from the thumbnail at the default 0.2).

Anything else (polygons, cogs, callouts…) is reported, not guessed.

Two paint conventions coexist. Photo 2 hangs *descriptors* off the
layer — `BFFl` fill, `LIFl` line fill, `LILn` line style, the class
behind `FDeF` — while Designer 1.x stores the classes directly: `BFil`
fill, `PFil` pen/line fill, `LSty` line style. The fill class is
`FilS` solid (a `Colr` colour), `FilN` none, or `FilG` gradient;
stroke width is `LDeL.Wght`, and `LDeL`'s 12-byte `Data` record is
`f64 miter · u8 cap · u8 join · u8 style · u8 0` where style 0 means
no line is drawn (1 solid, 2 dashed, 3 textured/brush — imported as
solid). A gradient holds stop positions (`Grad.Posn`), stop colours
(`Cols`), a linear/radial `Type` enum, and `FDeX` — a 2×3 transform
mapping the unit gradient axis into path space — which hangs off the
*descriptor* in newer files and the fill itself in older ones. Import
rebuilds shapes as live vector layers (editable, re-rasterized by the
app); gradient-filled ones keep rasterized pixels only.

**Free paths** (`PCrv`): `Crvs` → "PCvD" → `Data`, an untagged record
holding a subpath count then per subpath a closed flag and an array of
18-byte records — f64 x, f64 y, and a marker pair classifying the
record: **(1,0) and (2,0) are on-curve points** (terminal and interior
respectively), **(0,1) is the previous point's outgoing control** and
**(0,2) the next point's incoming control**. A closed path's trailing
controls belong to the segment joining back to the first point. (An
earlier reading — (1,0) control₁ · (0,1) control₂ · (0,2) endpoint —
looked plausible on dense pen strokes because every point lands near
its own controls, but it drops single-segment paths, whose stream is
just `(1,0) (0,1) (0,2) (1,0)`.) Coordinates are in a local design
space (`CvsB` bounds) mapped by the layer transform. Imported as live
vector layers, even-odd filled so traced outlines keep their holes.

**Curves adjustments** (`CrRA`): `AdjP` → "CrvP" with one `Spln` per
channel (`Mast`, `C1Sp`–`C5Sp`): `Cnt` control points, `Vals` as xs
then ys then tangents, in 0..1. Imported as a real curves adjustment
layer (master + RGB channels).

**Live filter nodes** (`FlRN`): a `Filt` pipeline warping the content
below between source and destination `Quad`s. Every corpus sighting
maps each quad onto itself — configured but inert — and imports as
nothing; a genuine warp would be reported.

**Masks**: "MRst" (mask raster) nodes in a layer's `AdCh` list — each
a full layer node with its own `Xfrm` and a single-channel bitmap
(format 6) where white reveals. A layer can carry several; the visible
ones multiply, and import multiplies them into one real, editable
layer mask. Adjustment layers (`CrRA` curves, etc.) nest their own
masks the same way.

**Blend enum**, read from `layer_mode.afdesign` (a file with one layer
per mode). The *(id, version)* pair is the key — later modes reuse ids
under version 1: 1 darken · 2 multiply · **2.1 darker colour** ·
3 colour burn · 4 lighten · 5 screen · 6 colour dodge · **6.1 lighter
colour** · 7 add · 8 overlay · 9 soft light · 10 hard light · 11 vivid
light · 12 pin light · 13 hard mix · 14 difference · 15 exclusion ·
**15.1 linear light** · 16 subtract · 17 hue · 18 saturation ·
19 luminosity · 20 colour · 21–25 average/negation/reflect/glow/erase
(no Photoshop-model equivalent).

**Raster pixels** (`Rstr`/`ImgN` → `Bitm`, class `DyBm`): `Frmt` enum
— 0 RGBA8 · 1 RGBA16 · 2 Gray8+A · 3 Gray16+A · 4 CMYK8+A · 5 LAB16+A
· 6 single 8-bit channel (masks; also Photo 2's usually-evicted
composite caches) · 9 RGBA32f — with `BmpW`/`BmpH`
(Photo 2 adds explicit tile-grid dims `TWiN`/`THiN`). Channels are
**planar**, each a grid of **256-byte × 256-row tiles** (so a 16-bit
channel is 128 px per tile column). Per channel N: `StaN`, one status
byte per tile in row-major order — 0/1 empty · 2 fill 0xFF · 3 fill
f32 1.0 · 4 stored · 5 **source-backed** (Photo 2) — and `IdxN`, a
list of `Blck { Rect: i32[4] valid region, Data }` for the status-4
tiles in order. Photo 2 omits `Rect` on full tiles and deduplicates:
identical tiles are one shared `Blck` object linked repeatedly.
`Blck.Data` (type 0x33) names a container entry whose payload is the
64 KiB tile plane — bare, or wrapped in a one-field graph document of
type `Data` (field `DatI`, blob). A fully evicted bitmap drops its
`Sta` arrays entirely. `MI*`/`MT*` fields are mip levels of the same
shape (the tag's third byte is the level number); ignored.

**Source-backed tiles** (status 5): a placed image doesn't duplicate
its pixels as tiles. The bitmap's `Bckg` entry (a graph document of
type `Blck` with `Link` bool, `Size` u64, `Data` blob) holds the
*original file bytes* (PNG, JPEG…) at exactly `BmpW`×`BmpH`; status-5
tiles decode from it. Mip levels for such bitmaps do store real tiles.

Live shapes, text and adjustments store **parameters only** — Affinity
re-renders them — so no pixel recovery is possible for them without
reimplementing Affinity's renderers. That's why the codec falls back to
the file's embedded flattened preview whenever a document contains any.

One field observation worth keeping: embedded flattened previews in
real Photo 2 files can be **stale** (one corpus file's preview showed a
half-rendered state while its layers were intact). The codec therefore
prefers a partial layered import over the preview whenever any pixels
were recovered, and adds the preview as a hidden reference layer.

## What's still unknown / to do

- Adjustments beyond curves (levels, HSL, recolour…): same `AdjP`
  pattern, not yet mapped — no corpus file to read the field layouts
  from.
- Non-identity `FlRN` filter warps (none seen in the corpora).
- Shapes beyond the kinds above (polygons, cogs, callouts, curved-edge
  stars…) are parsed but not turned into geometry; a fixture drawn
  with each tool (plus its thumbnail) is all it takes to add one.
- Text: single style per layer (first run wins), no per-run styling,
  effects on text (drop shadows) not yet applied, `TxtF` frame text
  gets artistic-text treatment.
- Adjustment parameters, ICC profiles (`Prof` on DyBm holds the raw
  ICC blob): parsed but not yet interpreted.
- Rotated/sheared *text* still imports axis-aligned (the text engine
  lays out horizontally); vectors and rasters carry full affine.
- The `CTyp` corner-type enum: 0 is rounded; the other values
  (straight/concave/cutout in the UI) haven't been observed and
  import as sharp.

`cargo run -p schist-codec-affinity --example afdump -- file.afphoto`
prints any file's container listing and full object graph.

[afread]: https://github.com/VMDevCpp/afread
[AFDesignLoad]: https://github.com/NickBeeuwsaert/AFDesignLoad
