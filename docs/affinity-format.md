# The Affinity file format, reverse engineered

What Schist knows about Serif's `.afphoto` / `.afdesign` / `.afpub`
format, as implemented by `crates/codec-affinity`. Serif publishes no
spec. This knowledge comes from prior art — [afread] by Vladimir Mamonov
(MIT) and [AFDesignLoad] by Nick Beeuwsaert (MIT) — plus our own
inspection of real files: `fixtures/affinity/` (Affinity Designer 1.x),
`fixtures/affinity-probe/` (single-feature documents drawn in the
unified Affinity 3.1 expressly to probe field layouts — see
[Probe fixtures](#probe-fixtures)), plus private corpora of Affinity
Photo 2.6 documents and Canva-era Affinity `.af` documents (not
vendored — point `SCHIST_AFFINITY_CORPUS` at colon-separated
directories of real files to sweep them in the codec tests; a document
open in Affinity leaves a `name.afphoto~lock~` sibling behind, which
the sweeps skip). Every generation uses the
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
content bounding rect (Photo; `i32::MIN+1` sentinels when unset) ·
`FiEf` effects · `AdCh` mask/adjustment children. Transforms are full
affine: import composes them exactly for vector layers and resamples
rasters and masks through rotation/shear.

**`BitR` does not place anything.** Rasters position exactly like
vectors: the bitmap's `[0,0,w,h]` through the layer transform chain.
`BitR` is only the content's bounding rect (usually in bitmap space —
sometimes the full canvas or a transform-scaled rect; conventions
vary). An earlier reading treated it as the destination rect, which
coincides with the truth exactly when the layer transform is the
identity — most Photo pixel layers — but squashes anything scaled, and
mangles rotated placed images, whose *pixel caches are stored
pre-rotated* so that the rotated `Xfrm` uprights them (their masks
carry the counter-rotation explicitly). Corpus files' own thumbnails
adjudicated this: transform-only placement wins or ties on every file.

**Clipped children.** A *non-group* layer's `Chld` list nests clipped
children — pixels, shapes, adjustments, even whole groups — each
confined to the parent's alpha and living in the parent's coordinate
space (A/B-scored against corpus thumbnails: composing the parent
transform beats treating them as siblings). Import emits them as
`clipping` layers directly above their base; a child that carries
clips of its own becomes a clipping group. Beware that nested nodes
are often written as bare tagged classes (`[CrRA]` with no
`AdjR<EncR<…` base chain), so type-chain tests miss them — dispatch on
the class tag.

**Text** (`TxtA` artistic / `TxtF` frame): no pixels, but the full
model. `StSt` (story) → `Blok` blocks → `Glyp` (`GStr`) holds the
UTF-8 string — line breaks are the Unicode paragraph/line separators
(U+2029/U+2028) or a vertical tab, *not* `\n`. Each block's `GAtt` →
`Runs` → `Item` carries character attributes — `Doub[0]` is the font
size, `RFnt`/`DFnt` the resolved and document fonts (`Post`
PostScript name, `Famy` family, `Wegt` weight, `Ital`; an *unresolved*
`RFnt` is present with empty names; bold lives in the PostScript name
more reliably than in `Wegt`), and `Objs` holds fill descriptors
(`FDsc.FDeF` → `Colr` → an `RGBA`/`HSLA`/… `_col` struct). **`RFnt`
is the writing machine's resolution and goes stale**: real corpus
files carry `RFnt = Helvetica-Bold` for text whose `DFnt` names
"Geist (Beta)", and Affinity opening them here re-resolves and draws
Geist. Import does the same — prefer whichever of `DFnt`, `RFnt`
names an *installed* family (also trying the name stripped of a
trailing parenthetical, since "Geist (Beta)" installs as "Geist"),
falling back to the stored resolution only when neither is installed.
Paragraph attributes mirror the run shape: block `PAtt` → `Runs` →
`Item` → `Ints[0]` is the alignment (0 left · 1 centre · 2 right).
`TxtH` is the frame (`ArFr` for artistic text): `FrmB` `f64[4]` is
the layout box in pre-transform coordinates — its transformed bottom
edge is the last baseline, its height running down from the first
line's cap (`ArtV`). **The box records the pen (advance) width, side
bearings and all, not the ink** — in Affinity's own exports the ink
starts one left side bearing inside the frame edge. Import re-sets
the text through the text engine (which reads GPOS `kern`-feature
pair adjustments itself; fontdue only knows the legacy `kern` table,
and modern faces kern via GPOS) — frame text reflows to the frame
width; artistic text keeps its natural layout when the real family is
installed, and only rescales to fill the recorded pen box when
substituting — anchors the pen box to the frame box per the
alignment and the last baseline to the frame bottom, and stores the
type tool's `PsTx` block so the layer stays editable.

**Layer transforms nest**: a group's `Xfrm` defines the coordinate
space its children's transforms live in; composing down the tree is
what places deeply-nested tiles correctly.

**Shapes** (`ShpN`): the layer's `ShpB` `f64[4]` local bounds plus a
`Shpe` class giving the kind and parameters; geometry is built in the
local box and pushed through the composed transform. Shape classes
repeat a tag across their base-class sections (a base default followed
by the derived value — `ShSt` carries `IRad` 0.5 then 0.382); the
*last* occurrence is the one that renders. Kinds with rebuilt geometry,
all verified against the files' own thumbnails:

- "ShNR"/"ShRR" rectangles: `ShCR` per-corner radii, but a radius only
  applies where the `CTyp` corner-type array says so — one enum per
  corner: 0 rounded · 1 straight chamfer · 2 concave (the arc bends
  inward, centred on the corner) · 3 cutout (a square bite), each
  probed with its own fixture. `Lock` ("single radius") locks every
  corner to the first one's radius and treatment. Radii are fractions
  of the shorter side when the writer also writes `CTyp` (the unified
  app: 25% chamfers 33px off a 132px rect), of *half* the shorter side
  in Designer 1.x files without it, and absolute under `AbSz`.
  Designer's plain rectangle keeps default radii in `ShCR` with no
  `CTyp` and renders sharp.
- "ShpE" ellipse.
- "ShSt" star: `Pnts` points alternating between the inscribed ellipse
  and `IRad` of it, first point up; `CrvL`/`CrvR` bow each spike's
  left (notch→tip) and right (tip→notch) edges sideways — positive
  bows outward; the sagitta is a fitted ~0.22 of the edge length per
  unit of curve.
- "ShSS" square star: `Side` rectangular arms, the first pointing
  down. Arms are the middle `COut` of each edge of the regular
  `Side`-gon inscribed in the ellipse — flat tips at the polygon's
  apothem `cos(π/N)`, half-width `COut·sin(π/N)`, adjacent arms
  meeting in a V notch at `COut` of the radius.
- "ShCl" cloud: `Bubl` circular arcs bulging out around the inscribed
  ellipse, meeting at `IRad` of the radius.
- "ShHt" heart: two lobes over a point; `Sprd` deepens the notch
  (proportions traced from the thumbnail at the default 0.2).
- "ShpT" triangle: apex `Pos` of the way along the top edge.
- "ShpD" diamond: widest at `Pos` of the height.
- "ShTz" trapezoid: top edge from `PosL` to `PosR`.
- "ShPy" polygon: `Side` vertices on the inscribed ellipse, first up
  (`Curv` bends edges — unmapped, straight only).
- "ShDS" double star: `Pnts` major tips with minor tips (`PRad`)
  between them and notches (`IRad`) between every tip.
- "ShPi" pie *and* donut: a ring sector from `AngS` to `AngE` (visual
  angles anticlockwise from +x; equal = the full ring) with inner
  radius `IRad` — zero for a wedge, a second even-odd subpath for a
  ring.
- "ShSg" segment: the inscribed ellipse above a chord `Pos0` of the
  way up (`Pos1` cuts a second chord; only the first is rebuilt).
- "ShCr" crescent: tips at top and bottom centre; both boundary arcs
  are circular (in unit space) bowing sideways by `ArcL`/`ArcR` of
  the half-width, negative left.
- "ShDA" arrow: a `Thck`-of-the-height shaft, head at an end when its
  `LSty`/`RSty` enum is nonzero, head length `LPr1`/`RPr1` of the
  height.
- "ShCg" cog: `Teth` teeth from `IRad` to the rim, tooth top `TtSz`
  and root gap `NtSz` of the period, plus a `Hole` bore (even-odd
  subpath). `Curv` bends flanks — straight only.
- "ShCR" callout (rounded rectangle): the balloon over the top
  1 − `TlHg`, radii in its own `ShCR` field (full-shorter-side
  scale), tail `TlWd` wide rooted at `TlRP`, tip at `TlEP` on the
  bottom edge.
- "ShCE" callout (ellipse) — *not* an ellipse variant: balloon over
  the top 1 − `TlHg`, tail rooted where the centre-to-tip direction
  meets the ellipse, `TlAn` of parametric angle wide, tip at `TlEP`.
- "ShTr" tear: apex over a bulb; the default (Ball 0.25, Curv 0.3,
  Tail 0.5) is reproduced from a numeric fit of Affinity's render —
  convex sides, widest at 51.5% height, elliptical bulb — and `Tail`
  scales the cone; other parameters warn.

Spirals (`ShSp` — stroke-only, not normalised into `ShpB`) and QR
codes are reported, not guessed. Every imported shape also keeps its
native `Shpe` subtree in a layer extras block (see
[Round-tripping](#round-tripping)).

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

**HSL adjustments** (`HsRA`): `AdjP` → "HSSP": `HueA` master hue shift
as a fraction of the full turn — **sign-flipped**: the UI's +90°
stores −0.25 — `SatA`/`LumA` as fractions of full range, an `HSV`
mode flag, and six per-hue-range tweak arrays (`HueC`/`SatC`/`LumC`
over `RngC` boundary angles in degrees, 315–345 = reds and so on).
Slider semantics, decoded exactly against isolated fixtures: the
saturation slider boosts reciprocally (s/(1−A) for positive A), and
the lightness slider both lifts l toward white (l + (1−l)·L) *and*
scales saturation by 1−L — Photoshop's does neither, so these are
opt-in flags on our hue/saturation adjustment that Affinity imports
set. All three master sliders now reproduce Affinity's render to
under 0.3 RMS. Per-range tweaks still warn.

**Parametric adjustments**, probed with one fixture file each (values
below are fractions of the UI's percents unless noted). The class
behind `AdjP` — `NAjP` for the gradient map — names the type; the
layer node's own tag is the type + "RA" (`LeRA` levels, `ExRA`
exposure…). All of these import as real adjustment layers; the
per-type accuracy against Affinity's own renders is pinned by
`probed_adjustments_match_affinitys_render`:

- `LevP` levels: `Blac`/`Whit` input levels, `Gamm`, `OutB`/`OutW`
  outputs; `BlkC`/`WhtC`/`GamC`/`OBlC`/`OWhC` are 5-wide per-channel
  arrays (unmodelled; warn when non-identity). *(exact)*
- `ExpP` exposure: `Expo` in stops — applied in a power-law space of
  exponent `Gamm` (2.2), so `Expo/Gamm` stops of encoded-value
  multiply reproduces it exactly. *(exact)*
- `B&CP` brightness/contrast: `Brig` fraction; `Ctrs` stores
  1 + contrast/100. Affinity's sliders drive smooth endpoint-
  preserving curves; the importer carries Affinity's *actual*
  transfer curves, tabulated from isolated brightness-only and
  contrast-only fixtures, and imports the layer as a sampled curves
  adjustment (other amounts scale/blend against the tables).
- `B&WP` black & white: `RedC Yell Gree Cyan Blue Mage`. *(exact)*
- `WhBP` white balance: `WhBV` version, `WhBa` warmth as an i32
  percent, `WBTi` tint fraction. Affinity performs a **Bradford
  chromatic adaptation in linear light** — across seven saturated
  patches Bradford beats CAT02 (err 25 vs 61) and diagonal RGB gains
  (339) decisively — whose grey-axis gains follow calibrated
  exponentials (warmth log-gains quadratic, fitted at 30 and 50;
  tint linear, fitted at 60). Imported as our own
  `Params::WhiteBalance`, which implements exactly that.
- `CoBP` colour balance: `Sh/Mi/Hi` × `CR/MG/YB` + `PeLu`. Affinity
  moves ~0.11× our step per percent (fitted).
- `VibP` vibrance: `Vibr` i32 percent, `Satu` fraction. Formula
  differs on saturated colour.
- `InRA` invert: no parameters at all — no `AdjP`. *(exact)*
- `PosP` posterise: `Post` i32 levels. Both apps quantize
  floor(v·n)/(n−1) — equal input bands, outputs over the full range
  (this fixture corrected our own rounding convention). *(exact)*
- `ThrP` threshold: `Thre` fraction (`Fals`/`True` output levels).
- `CnMP` channel mixer: `Weig`, five rows of six — `[offset, R, G, B,
  A, x]` for the R, G, B, A and composite outputs. The alpha weight is
  a flat term on opaque pixels and folds into our constant. *(exact)*
- `SCoP` selective colour: `Weig`, nine ranges of `[C, M, Y, K]` — the
  six Photoshop-model ranges, then whites/neutrals/blacks (warn) —
  plus `Rela`. *(near exact)*
- `GraP` gradient map (behind `NAjP`): a `Grad` of `Posn` (position,
  midpoint) pairs and `Cols` colours; imported as a full multi-stop
  ramp over Rec.601 luma. *(exact)*
- `LeFP` lens filter: the colour as three u16 Lab components (their
  field tags carry unprintable bytes; they are the node's first three
  u16 fields in L, a, b order), `Dens` density, `Pres` preserve
  luminosity. Density imported ×0.2 (fitted — Affinity tints far more
  gently than our multiply-toward-the-colour).
- `RecP` recolour: `RecH` hue as a fraction of the turn, `RecS`
  saturation, `RecL` lightness — our colorize, whose positive
  lightness offset l + (1−l)·L matches Affinity exactly. *(exact)*
- `STPa` split toning (`HlHu`/`HlSa`/`ShHu`/`ShSa`/`Bala`): parsed but
  reported — no equivalent adjustment yet. Soft Proof, LUT, OCIO,
  Normals and Tone Compression/Stretch are likewise reported when
  seen.

**Layer effects** (`FiEf`, an array of `FilE`-derived classes): every
entry shares `Enab`, `BlnM` (the layer blend table), `Opac` (0..1),
`SclO` (scale measures with the object) and usually `Radi`
(blur/width in px) and a `Colr`. `Shad`/`InSh` shadows add `Offs`
(distance) and `Angl` — the *offset direction* in radians, y-down, so
the 45° default points down-right — plus `Knck` knockout; `OutG`/
`InnG` glows add `Comp` (contour range, unmapped); `ColO` is a colour
overlay; `Strk` an outline stroke (`Radi` width, `Alig` position,
`Ftyp` solid/gradient with `GrFl` holding the gradient); `BevE` a
bevel (`Azim`/`Elev` light direction in radians, `Dept`, `Sftn`,
`Beve` subtype — only seen disabled, so its mapping is a guess);
`Gaus` a gaussian blur (no layer-style equivalent — reported).
Enabled effects import onto our layer style — on any layer kind,
groups included: the corpus hangs sticker outlines and drop shadows on
whole groups, so the compositor flattens a styled group's children and
runs the same fx pipeline over the result
(`schist_compositor::render_styled`).

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
A **fully evicted** bitmap (no `Sta` arrays at all) that still has its
`Bckg` *is* its source image wholesale — real Canva-era files park
whole background photos this way.

Live shapes, text and adjustments store **parameters only** — Affinity
re-renders them — so no pixel recovery is possible for them without
reimplementing Affinity's renderers. That's why the codec falls back to
the file's embedded flattened preview whenever a document contains any.

One field observation worth keeping: embedded flattened previews *and
thumbnails* in real Photo 2 / Canva-era files can be **stale** — the
container keeps savepoints, and the preview may show an older state
(different video frame, since-hidden layers) than the head revision.
The codec therefore prefers a partial layered import over the preview
whenever any pixels were recovered, and adds the preview as a hidden
reference layer. When using thumbnails as ground truth, treat
disagreements about *visibility* with suspicion; geometry and colour
remain reliable.

The corpus sweep tooling: `--example afdiff` composites an import
(through the real compositor) next to the file's thumbnail;
`--example afscore` reduces that to an RMS number, which is how the
`BitR` and clipped-children questions were settled; `--example aftree`
prints the imported layer tree with bounds.

## Round-tripping

Nothing an import understands — or doesn't — is thrown away, so a
future `.af` *exporter* can write documents back without loss:

- Every adjustment layer keeps its native parameter class (`AdjP` /
  `NAjP`) in `AdjustmentData::raw`, as typed JSON behind an `AFJ1`
  prefix (`crates/codec-affinity/src/preserve.rs`): every field keeps
  its tag and wire type, class hierarchies their (tag, version)
  chains, `Class` references inlined.
- Adjustments with no equivalent on our side (split toning, Soft
  Proof, LUT, OCIO, Normals, tone compression/stretch…) import as
  **no-op adjustment layers** that carry the same preserved block,
  instead of being dropped — the user keeps the layer in the stack
  and an export keeps its meaning.
- Every live shape keeps its native `Shpe` subtree in a layer extras
  block keyed `AfSh`.
- Text layers already keep the type tool's `PsTx` block.

## Probe fixtures

`fixtures/affinity-probe/` holds one tiny document per probed feature
— every parametric adjustment (plus isolated single-slider variants
for brightness, contrast, white balance and HSL), one document per
shape tool, per rectangle corner type, a curved-edge star, and a
rotated text layer — drawn in the unified Affinity 3.1 on a synthetic
test card (hue ramp, grey ramp, saturated patches), saved as `.af`. They serve two purposes: the field layouts
above were decoded by reading the typed values back out of them with
afdump, and their embedded thumbnails — Affinity's own renders — pin
each importer's accuracy in
`probed_adjustments_match_affinitys_render`. The same technique
(create → read back → compare to the file's own thumbnail) is how to
attack anything left in the list below; the transfer-curve forensics
work best when the document contains a grey ramp, which separates
per-channel behaviour from channel-mixing behaviour at a glance.

## What's still unknown / to do

- Per-range HSL tweaks (`HueC`/`SatC`/`LumC`) are unmapped, and the
  negative directions of the HSL/white-balance sliders are assumed to
  mirror the measured positive ones (all fixtures so far are
  positive; the minus key was swallowed by the panel fields when
  probing).
- Vibrance on *saturated* colour and the lens filter's density curve
  differ from ours (both grey-exact); more single-slider fixtures
  would pin them.
- Non-identity `FlRN` filter warps (every corpus sighting is inert).
- Spirals (`ShSp` — stroke-only, not normalised into `ShpB`) and QR
  codes are reported, not rebuilt; the curved-star bow and the tear
  profile are single-fixture fits.
- Text: single style per layer (first run wins), no per-run styling.
- Layer effect gaps: `Gaus` blur has no layer-style home; glow
  contour range (`Comp`) and shadow spread are unmapped; the `BevE`
  subtype enum and the `Strk` `Alig` values are only part-verified
  (every corpus bevel is disabled).
- ICC profiles: `ICCP` nodes carry the profile name and blob; every
  corpus sighting is sRGB, so no conversion has been needed yet.
- One corpus file (a thought-bubble collage) still renders its
  rotated/mirrored clipped children a few percent off where live
  Affinity places them, while matching the file's own thumbnail —
  the residual convention there is unresolved.

`cargo run -p schist-codec-affinity --example afdump -- file.afphoto`
prints any file's container listing and full object graph;
`--example afrev` lists an entry's savepoint revisions.

[afread]: https://github.com/VMDevCpp/afread
[AFDesignLoad]: https://github.com/NickBeeuwsaert/AFDesignLoad
