# The Affinity file format, reverse engineered

What Schist knows about Serif's `.afphoto` / `.afdesign` / `.afpub`
format, as implemented by `crates/codec-affinity`. Serif publishes no
spec. This knowledge comes from prior art — [afread] by Vladimir Mamonov
(MIT) and [AFDesignLoad] by Nick Beeuwsaert (MIT) — plus our own
inspection of real files: `fixtures/affinity/` (Affinity Designer 1.x),
`fixtures/affinity-probe/` (single-feature documents drawn in the
unified Affinity 3.1/3.2 expressly to probe field layouts — see
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
            #FT2/#FT3/#FT4 add a u32 (always 32); #FT4 adds a
            CRC-32 of the *compressed* payload — Affinity verifies
            it and calls the file corrupted on a mismatch
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
mode flag, and six per-hue-range tweak arrays (`HueC`/`SatC`/`LumC`)
over the `RngC` boundary angles. Slider semantics, decoded exactly
against isolated fixtures: the saturation slider boosts reciprocally
(s/(1−A) for positive A), and the lightness slider both lifts l toward
white (l + (1−l)·L) *and* scales saturation by 1−L — Photoshop's does
neither, so these are opt-in flags on our hue/saturation adjustment
that Affinity imports set. Negative amounts are *not* mirrored
guesses: −45° hue, −40 saturation and −40 lightness were each probed,
and s·(1+A), l·(1+L) and the same 1−|L| desaturation reproduce them to
0.0 RMS.

**Per-hue-range tweaks.** `RngC` is 24 f32 — six ranges of four
boundary angles in degrees: weight 0 at the first, ramping to 1 by the
second, flat to the third, back to 0 by the fourth. Reds are
`315, 345, 15, 45`, and the six wrap round in 60° steps
(yellows `15, 45, 75, 105`…), so neighbouring ramps overlap and the
six weights sum to 1 at every hue. `HueC`/`SatC`/`LumC` hold that
range's shifts, in the same units and with the same sign convention as
the master `HueA`/`SatA`/`LumA`; the weight comes from the pixel's
**source** hue, before the master shift (`hsl_range_mix.af` probes
master hue +120° against a reds-range tweak and settles it). Hue and
saturation simply add to the master sliders before their transfer
curves run — but *luminosity does not*: a range's lightness is a
separate, hue-preserving pull of every channel toward the brightest
one (positive) or the darkest one (negative), by |w·L| of the way, so
it changes the HSV saturation or value and leaves the other exactly
alone. Pure green at range luminosity ±50% goes to (128,255,128) and
(0,128,0) on the nose, and the master lightness lift would give
neither. All the per-range fixtures now import to under 0.15 RMS.

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
  percent, `WBTi` tint fraction — and **`WBTi` is the negation of the
  Tint field the UI shows** (the panel reads −60 % where the file says
  0.6; reopening a saved file in Affinity confirms it, and `WhBa` is
  *not* negated). Affinity performs a **Bradford chromatic adaptation
  in linear light** — across seven saturated patches Bradford beats
  CAT02 (err 25 vs 61) and diagonal RGB gains (339) decisively. Its
  grey-axis gains are now **measured, not fitted**: one probe document
  per slider position, ten percent apart across both sliders' whole
  ranges, each solved for the three linear-light gains that reproduce
  Affinity's own render of the test card. Every solve lands within
  0.3/255 RMS, so the adaptation *is* the operation; the tables live in
  `WARMTH_LOG_GAINS`/`TINT_LOG_GAINS` in `schist-adjustments` and are
  read by linear interpolation. Neither curve is linear or symmetric —
  warmth +10 moves nearly three times as far as −10, warming saturates
  towards +100 while cooling runs away (log-gain −1.07 on red at −100
  against −0.44 at −70) — which is why the earlier mirrored-quadratic
  fit was 2.5–3.5 RMS out on the cooling half. Single-slider documents
  now import at 0.1–0.2 RMS. The two sliders are **not** independent:
  Affinity moves one white point in two dimensions rather than adapting
  twice, so the product of the two tables is right only on the axes.
  Fitting a 3×3 in linear light to a probe's whole cube comes back with
  0.0012 residual and, taken into Bradford cone space, is diagonal to
  0.001 — so the model really is a chromatic adaptation, and all a
  combined setting changes is the cone gains. A 5×5 grid of probes
  (`wbgrid`, warmth × tint at −100, −50, 0, 50, 100, each read off a
  full grey and primary ramp with the clipped samples dropped)
  measures those; the residual over the two axis tables is
  `WB_INTERACTION`, zero along both axes and small in most of the
  interior, but very large in the cool-magenta corner — at (−100,
  +100) Affinity's S-cone gain is 11.97 against the 3.79 the two
  sliders separately predict, and a mid grey comes back (9, 59, 215)
  where the product model says blue ≈ 129. With that correction
  interpolated bilinearly, every combined probe lands at 0.1–0.2 RMS,
  and `whitebalance.af` (warmth 30, tint 40) at 0.6.
- `CoBP` colour balance: `Sh/Mi/Hi` × `CR/MG/YB` + `PeLu`. Affinity
  moves ~0.11× our step per percent (fitted).
- `VibP` vibrance: `Satu` is a plain fraction, but **`Vibr` is an i32
  on a 0..127 scale, not a percentage** — the panel's 50 % writes 64
  and 100 % writes 127. The Saturation slider is a straight scale of
  **CIELAB chroma** by 1 + `Satu`: across the probe card every pixel
  that stays in gamut comes back at 1.500× its chroma for the +50 %
  fixture (sd 0.01) and 0.499× for −50 %, against 15 RMS for an HSL
  saturation scale, 16 for HSV and 11 for a luma push. Clipping back
  into sRGB leaves ~1.3 RMS, so Affinity's gamut mapping is a little
  gentler than a hard clip. Vibrance is a chroma scale too, in the same
  space (the cube probes hold L\* to 0.11 and Lab hue to 0.3° on
  average), with two separable weightings on the gain — measured from
  `cube_vib100.af`, `cube_vib50.af` and `cube_vibneg100.af`:
  - **Turned down it is just a weaker Saturation.** At −100 every
    pixel comes back at exactly 0.500× its chroma, hue irrelevant, so
    the gain is a flat 1 + t/2.
  - **By hue, a skin-tone protection window.** The gain is exactly 1
    between Lab hue 30° and 45° and ramps linearly to full over the
    next 45° either side, so reds and oranges hold still while
    everything from about 90° round to 345° gets the whole boost.
  - **By chroma, a rise-and-ease curve**: nothing at grey, +47 % near
    chroma 58, easing back to +37 % by chroma 92 (and the distribution
    inside each chroma bin is tight, so that easing is real and not
    gamut clipping).
  - **The slider is not a scale on that.** Half strength also reads
    the curve at a *lower* chroma: 1 + t·A(t^0.7·C) tracks the 50 %
    probe to 1.4 % of the gain, against 2.4 % for a plain 1 + t·A(C).
  Together those take the three vibrance fixtures from 5–10 RMS to
  1.4–2.0.
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
  u16 fields in L, a, b order, L over 0..100 and a/b over −128..127),
  `Dens` density, `Pres` preserve luminosity. The cube probes pin all
  three parts *(exact)*:
  - it is a **per-channel multiply in encoded sRGB**, not in linear
    light — with Preserve Luminosity off the whole cube collapses to
    three 1-D ramps with zero residual;
  - the multiplier is `1 − k + k·colour` where **k = 0.9 · density²**,
    which lands the same k on all three channels to four decimals at
    density 0.35, 0.5 and 1.0 — and only if the Lab is decoded
    against **D50** and Bradford-adapted to sRGB, which is what makes
    the blue channel come out at 0.100 rather than 0.012;
  - Preserve Luminosity is Photoshop's `SetLum`/`ClipColor`: add the
    Rec.601 luma difference to all three channels, then, if that
    leaves the range, pull the triple back towards its own luma
    instead of clipping per channel. A straight rescale gets filtered
    white badly wrong (it stays white in Affinity); this does not.
- `RecP` recolour: `RecH` hue as a fraction of the turn, `RecS`
  saturation, `RecL` lightness — our colorize, whose positive
  lightness offset l + (1−l)·L matches Affinity exactly. *(exact)*
- `STPa` split toning (`HlHu`/`HlSa`/`ShHu`/`ShSa`/`Bala`, hues as a
  fraction of the turn): parsed but reported — no equivalent
  adjustment yet. The cube probes say most of what an implementation
  would need: the tonal key is **Rec.601 luma in encoded sRGB** (it
  cuts the changed pixels off at exactly the balance point, where
  Rec.709 and HSL lightness both smear), that luma is preserved
  exactly, the two halves act only on their own side of `Bala`, and
  the strength is a bump — zero at black, at the balance point and at
  white, peaking half way through each half. On the grey ramp
  `SetLum(src + w·(tint − luma(tint)), luma)` with
  w = sat · 0.23 · (4p(1−p))^1.2 lands inside 1/255; over the whole
  cube it only halves the error, so how the tint combines with a
  pixel's *own* colour is still open. Soft Proof, LUT, OCIO,
  Normals and Tone Compression/Stretch are likewise reported when
  seen.

**Layer effects** (`FiEf`, an array of `FilE`-derived classes): every
entry shares `Enab`, `BlnM` (the layer blend table), `Opac` (0..1),
`SclO` (scale measures with the object) and usually `Radi`
(blur/width in px) and a `Colr`. The panel's ten rows — probed one
document per row and per enum setting, in `fixtures/affinity-probe/fx_*.af`
— are:

| tag | effect | own fields |
|---|---|---|
| `Shad` | Outer Shadow | `Offs` · `Angl` · `Comp` · `Knck` · `Colr` |
| `InnS` | Inner Shadow | `Offs` · `Angl` · `Comp` · `Colr` |
| `OutG` | Outer Glow | `Comp` · `Colr` |
| `InnG` | Inner Glow | `Comp` · `Colr` |
| `ColO` | Colour Overlay | `Colr` |
| `GrdO` | Gradient Overlay | `GrFl` |
| `Strk` | Outline | `Alig` · `Ftyp` · `Colr` · `GrFl` |
| `BevE` | Bevel / Emboss | `Beve` · `Azim` · `Elev` · `Dept` · `Disr` · `Sftn` · `Prof` · `Invt` · `ShBM` · `ShOp` · `ShCl` · `HiCl` |
| `PhgB` | 3D | `Ambi` · `Diff` · `Spec` · `Expo` · `AmbC` · `SpeC` · `Lits` |
| `Gaus` | Gaussian Blur | `Radi` · `PrAl` |

The inner shadow's tag is **`InnS`**, not the `InSh` an earlier reading
assumed — nothing in any corpus or probe file spells it `InSh`, so
inner shadows were being skipped wholesale.

`Offs` is the shadow's distance and `Angl` the *offset direction* in
radians, y-down, so the stored 45° default is the panel's 315° and
points down-right. `Knck` is "fill knocks out shadow".

**`Comp` is the Intensity slider, stored inverted**: 0 % writes 1.0 and
100 % writes 0.0, on shadows and glows alike (probed at 40, 60, 70 and
80 %). It is our `spread`, taken as 1 − `Comp` — but it is **a gain
applied after the blur, not a choke before it**. An inner glow on a
hard-edged square at radius 40 (`ig_r40_i*.af`) comes back as the plain
blurred step multiplied by exactly `1 / (1 − intensity)` and clipped:
50 % doubles it to within 1/255 and 80 % quintuples it. Photoshop's
Spread/Choke is the other thing — a dilation before the blur — and
running it that way made it a no-op on any hard-edged layer, which is
most of them; that alone was most of the inner glow's old 21 RMS.

**Every `Radi` that is a blur means what it means on `Gaus`.** Fitting
an error function to the same square's glow gives sigma 8.02, 13.16 and
27.49 px for `Radi` 20, 40 and 80 — the same ~0.34 × `Radi` the
Gaussian Blur effect uses, so shadows, glows and the bevel's height map
all convert through it. (A stroke's `Radi` is a width, not a blur, and
stays as it is.) The hard-square probes put the conversion at
0.57–0.60 of our own radius and the test-card ones a little higher,
which says a residual remains in the falloff *shape*; 0.58 is where the
two sets agree best.

**The bevel's `Radi` sets the ramp's *width*, not its steepness.**
Probed on a mid-grey square at `Radi` 10 and 30 with `Dept` 5
(`bv_pillow_*.af`), the shading is the same curve in both, stretched:
reading the highlight back out of its Screen-75 % blend gives 0.50,
0.52, 0.52, 0.37, 0.16, 0.01 at 0, 0.2, 0.4, 0.6, 0.8 and 1.0 of the
way in, and it reaches flat at exactly `Radi` pixels from the edge. So
the peak is 0.535 whichever radius is used — the surface slope does
*not* fall off as the bevel widens, which means the height field is a
ramp over `Radi` px built from a distance field, not a blurred alpha,
and `Dept` acts on the slope rather than as a height in pixels.

Ours does the opposite: it takes the gradient of an alpha blurred by
`Radi`, so the slope goes as 1/`Radi` and a wide bevel comes out nearly
flat (at `Radi` 30 our profile never leaves 128/255 where Affinity's
runs 175 down to 53). That is the whole of the bevel residual, and it
is a re-implementation rather than a constant: a distance-field ramp,
the profile above, and one more probe at a second `Dept` to pin what
`Dept` scales.

Export divides by the same factor (`BLUR_RADI`, shared so the two
directions cannot drift), or a shadow we write would come back nearly
twice the size we meant — which is what the round-trip test caught. The
exporter still writes no `Gaus` or `BevE` node at all, so a Gaussian
blur or bevel is reported and dropped on the way out.

Two of our own bugs fell out of these probes and are worth recording,
because both flatter the numbers above. The **inner** glow and inner
shadow blur the *outside* of the shape inwards, so the buffer has to
hold that exterior out to the blur's reach — the styled raster was
grown only for the effects that draw beyond the layer, leaving a
one-pixel border that the first blur pass smeared into nothing and a
glow at two thirds strength along its own edge. And the blur itself
used one integer box width for all three passes, whose achievable
sigmas step by a whole pixel around r = 8; mixing two adjacent widths
across the passes hits the target sigma instead, which is what stopped
two probes of the same effect disagreeing about the scale factor.

`Strk`'s `Alig` is **0 outside · 1 centre · 2 inside**, and `BevE`'s
`Beve` subtype is **0 inner · 1 outer · 2 emboss · 3 pillow** (pillow
is the app's default) — a fixture apiece; both had been guessed, and
both guesses were wrong. A bevel's `Dept` is a depth in *pixels*
alongside `Radi`, not a factor, `Disr` links the two in the panel,
`Invt` flips the bevel, and the highlight and shadow each carry their
own blend, opacity and colour (`BlnM`/`Opac`/`HiCl` against
`ShBM`/`ShOp`/`ShCl`). `Prof` is an optional contour profile.

`GrdO`'s gradient hangs off `GrFl`, an `FDsc` fill descriptor whose
`FDeF` is the usual `FilG`; unlike a shape's, it carries **no `FDeX`**
— the panel's scale, offset and angle controls are absent at their
defaults and the ramp simply runs left to right across the layer's
bounds.

Enabled effects import onto our layer style — on any layer kind,
groups included: the corpus hangs sticker outlines and drop shadows on
whole groups, so the compositor flattens a styled group's children and
runs the same fx pipeline over the result
(`schist_compositor::render_styled`). `PhgB` has no layer-style home
and is reported. The field mapping is exact for everything else; what
the probe fixtures' bounds still record is our own effect renderers'
falloff against Affinity's. With the intensity and radius conventions
below in place the shadows and glows are close (0.5-3.3 RMS); what is
left is the bevel (1.8-15, worst on pillow emboss) and the stroke
(5-10), neither of which has been probed at two radii yet.

**`Gaus`'s `Radi` is not a pixel radius.** Blurring a hard-edged
square (`blur_r10.af`, `blur_r30.af`, `blur_r60.af`) and fitting an
error function to the resulting alpha gives σ = 11.29, 31.88 and 67.11
px for `Radi` 30, 90 and 180 — a standard deviation of **0.373 ×
`Radi`**, with the middle probe 6 % under that line and the other two
on it. (The panel's own "px" figure is a third of `Radi` again; the
stored value is in some finer internal unit.) The residual against a
true Gaussian is under 1/255, so it really is one. Import maps it onto
our blur radius — a parameter where σ = radius/√3 — through 0.60,
which is where the three probes agree once our three-box
approximation's own few-percent width and integer box quantisation are
in play. `PrAl` is "preserve alpha": blur the colour inside an
unchanged silhouette. Because the effect softens the layer itself, it
sits at the bottom of the panel's list and everything else works from
the blurred shape; its reach is √3 times the radius, which the styled
raster has to be grown by or the soft edge comes back square.

**Live filter nodes** (`FlRN`): a `Filt` pipeline warping the content
below between source and destination `Quad`s. The node hangs off a
layer's `AdCh` list, beside its masks, and the `Filt` class names the
filter — `Pers<RDPF<RasC<SCBa<DcCm` is Live Perspective. A `Quad` is
eight f64 (`X0`–`X3` then `Y0`–`Y3`) listing its corners top-left,
bottom-left, bottom-right, top-right in the layer's own pixel space,
and `Src`→`Dst` is the projective map: dragging one corner handle in
of a 512² layer writes `Src` (0,0) (0,512) (512,512) (512,0) against
`Dst` (122,78) (0,512) (512,512) (512,0), so `Dst` is where the
content lands. `DSrA`/`DDsA` and `DSrB`/`DDsB` are the second pair of
quads the "Two planes" mode uses (`DMod` says whether it's on; at
defaults they split the layer down the middle and map onto
themselves), `AClp` is autoclip and `Live` marks it live. Every
*corpus* sighting maps each quad onto itself — configured but inert —
and imports as nothing. A genuine warp (`flrn_perspective.af`) is
resampled through: import solves the eight-equation homography taking
`Src` to `Dst`, warps the layer's bitmap in its own space, and slides
the layer transform by wherever the destination box landed (0.01 RMS
against Affinity's own render). Two-plane mode is still reported
rather than applied.

The other live filters hang their own class off `Filt`, all deriving
from `RDPF<RasC<SCBa<DcCm`, one per menu entry under Pixel ▸ New Live
Filter Layer. Only Live Perspective warps between quads; the rest
carry plain parameter fields, and every one of them shares `OlCm`,
`NwCm`, `Live`, `AbrP`, `IRec` and (mostly) `PMin`/`PMax` after the
fields below. None are applied yet, but the layouts are decoded:

| Menu | Class | Fields |
| --- | --- | --- |
| Distort ▸ Perspective | `Pers` | quads, above |
| Distort ▸ Twirl | `RTwC` | `Angl`, `Radi`, `Orin` (centre) |
| Distort ▸ Pinch / Punch | `RPPC` | `Inte`, `Radi`, `Orig` |
| Distort ▸ Spherical | `RSpC` | `Inte`, `Radi`, `Orig` |
| Distort ▸ Ripple | `RRiC` | `Inte`, `Orig` |
| Distort ▸ Lens Distortion | `RLdC` | `Inte`, `Orig`, `RadX`, `RadY` |
| Distort ▸ Pixelate | `RPxC` | `Quan` |
| Distort ▸ Glitch | `RGlC` | `GlSt`, `GMtd`, `GChn`, `GCns`, `Orig`, `RadX`, `RadY`, `Angl`, `DspH`, `DspV`, `GlAS`, `GlSP`, `GlIM`, `GlID` |
| Blur ▸ Gaussian | `RGBC` | `Radi` |
| Blur ▸ Box | `RBBC` | `Radi` |
| Blur ▸ Motion | `RMoB` | `Radi`, `Angl` |
| Blur ▸ Radial | `RRaB` | `Cent`, `Angl` |
| Blur ▸ Lens | `RLbC` | `Radi`, `Blad`, `FSto`, `BlmT`, `BlmF`, `BlmC` |
| Blur ▸ Maximum | `RMBC` | `Radi`, `Circ` |
| Blur ▸ Median | `RMeB` | `Radi` |
| Sharpen ▸ Unsharp Mask | `RUSC` | `Radi`, `Fact`, `Thrs` |
| Sharpen ▸ Clarity | `Clrt` | `Strn` |
| Sharpen ▸ High Pass | `RHPC` | `Radi`, `Mono` |
| Noise ▸ Add Noise | `RANC` | `Inte`, `Mono`, `Gaus` |
| Noise ▸ Denoise | `RNRC` | `LumS`, `LumD`, `LumB`, `ColS`, `ColB` |
| Noise ▸ Dust & Scratches | `D&SC` | `Radi`, `Tole`, `Chan` |
| Lighting ▸ Lighting | `RLig` | `lpar` → `LigP` (`Ambi`, `Diff`, `Spec`, `Expo`, `AmbC`, `SpeC`, `Dept`, `BMap`, `ScaX`, `ScaY`, `BMOp`, `Lits` → `LigS` lights with `Type`, `Colo`, `Spin`, `Tilt`, `Cent`, `Dist`, `OCon`, `ICon`) |
| Lighting ▸ Shadows / Highlights | `RNSH` | `SStr`, `SRng`, `HStr`, `HRng` |
| Colours ▸ Vignette | `RVgC` | `Expo`, `Hard`, `Scal`, `Shap` |
| Colours ▸ Halftone | `RHtC` | `Size`, `Cont`, `ScrT`, `DotT`, `Angl`, `Gcre`, `Ucre` |
| Colours ▸ Voronoi | `RVoC` | `Size`, `Widt` |

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

## Writing

Schist writes `.af` documents (`crates/codec-affinity/src/{emit,
container,export}.rs`). The write path is built on two rules:

**Byte-exact re-serialization.** The graph parser records every
wire-level detail semantics alone wouldn't need — each field's type
byte, class framing (0x30/0x31/0x32), 0x31 section boundaries and
chain terminators, and the array headers empty arrays can't rederive
(a hoisted 0x32 `(tag, id)` header, an enum array's version, a curve
array's record size, even the non-canonical `0xFF` some writers store
for `true`). `emit::serialize` inverts the parse exactly:
`serialize(parse(x)) == x` for all 212 graphs across every fixture and
corpus document, all generations (pinned by `tests/emit_roundtrip.rs`).

**Never guess at boilerplate.** The exporter parses a vendored probe
fixture (a real Affinity 3.1 document) as a template, patches the
canvas fields (`DfSz`, spread `MiID`, base-raster dimensions), replaces
the spread's `Chld` with freshly built layer nodes, and re-emits.
Affinity sizes the opened canvas from the spread's page geometry, not
`DfSz`: the slice persona's `SlcP.SRct` and the spread's
`SpMd.PagR[].rctp` must be patched too, or the document opens at the
template's 512×512 (the `DocR`-level `spmd` page rect may stay
`[0,0,0,0]` — the template, itself Affinity-written, has it so). The
root's `CSel` selection is reset to an empty `Itms` (the real-file
idiom for "nothing selected") so it doesn't drag an orphaned copy of
the template's layer into the file.
Layer node layouts (`Rstr`, `Grup`, `MRst`, adjustment nodes, `FilE`
effects, `DyBm`/`Blck` bitmaps) are transcribed field-for-field from
real documents with `--example afschema`, which prints each class's
chain versions, framing, and per-field wire types. Notably, real
writers put **all fields in the trailing stream** of a 0x31 definition
— the type-chain sections are field-less.

**Stream-order invariants.** Affinity's reader is stateful across the
graph stream and enforces two conventions our own (stateless-enough)
reader never needed; violating either makes the app reject the file as
"corrupted" (issue #40):

- *Declare-once type chains.* The first node of a class carries the
  full versioned declaration: one field-less type section per
  not-yet-declared class in its ancestry (most-derived first), ending
  closed (flag 2) at the ancestry root or with a lone tag (flag 1)
  naming the first ancestor an earlier declaration covered. Every
  later node of the class is the single lone-tag shorthand. Verified
  against Affinity 3.2: cloning an existing node is accepted, writing
  a second full declaration of its class is not.
- *Sequential object ids.* 0x31 object ids count 0, 1, 2… in exactly
  definition (stream) order — the id is the stream position, in every
  real file across the corpus.

Patching a template graph breaks both (replaced subtrees orphan ids
and declarations; fresh nodes append out-of-order ids and re-declare
known classes), so the exporter runs `emit::normalize_declarations`
and `emit::renumber_ids` — passes proven to be no-ops on real
documents (`normalization_is_identity_on_real_documents`) — before
serializing; `exported_graphs_keep_affinitys_stream_invariants` pins
both properties on exporter output.

Container conventions (v12), transcribed from 3.1-written files: one
`#FT4` savepoint; every block after the first prefixed `FF FF FF FF`;
`#Inf.length` = Σ compressed sizes; `num` = next free entry id; the
FAT mirrors (thumb offset, length); per-entry `#FT2+` extra = 32,
`#FT4` extra = CRC-32 of the compressed payload (verified by
Affinity); `Prot` = 12; entries zstd-compressed (`ruzstd`,
falling back to stored), no prediction, CRC-32 over plain bytes; a
`Thmb` block holds the composited PNG preview. `--example afwrite`
exports any importable file (or `--demo`) for testing against real
Affinity.

Exported today: rasters as native planar tiles (status 1/2/4, partial
`Rect`s, mip chain), groups (pass-through vs isolated via `PasT`),
masks as `AdCh` `MRst` nodes, clipping layers as Affinity clipped
children, opacity / fill opacity / blend enums / visibility, shadows,
glows, colour overlay and outline effects, and adjustment layers
re-emitted from their preserved parameter blocks (below). Text and
vector layers export their rasterized pixels — re-emitting native
`TxtA`/`ShpN`/`PCrv` (the `AfSh` extras block already carries a
shape's native `Shpe` subtree) is the natural next step.
`tests/export_roundtrip.rs` proves every fixture and corpus document
survives import → export → import with structure and pixels intact.

## Round-tripping

Nothing an import understands — or doesn't — is thrown away, so the
`.af` exporter can write documents back without loss:

- Every adjustment layer keeps its native parameter class (`AdjP` /
  `NAjP`) in `AdjustmentData::raw`, as typed JSON behind an `AFJ1`
  prefix (`crates/codec-affinity/src/preserve.rs`): every field keeps
  its tag, wire type byte and framing, class hierarchies their
  (tag, version) chains — the owning layer's chain too — `Class`
  references inlined. `preserve::decode` turns a block back into graph
  nodes for the exporter, inferring canonical wire types for blocks
  saved before they were recorded.
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
for brightness, contrast, white balance and HSL, both signs, and one
per HSL hue range), one document per shape tool, per rectangle corner
type, a curved-edge star, a rotated text layer, one per layer-effect
row and per effect enum setting (`fx_*.af`, drawn on the same card
shrunk to 300² so outer effects have room), and a live perspective
warp — drawn in the unified Affinity 3.1/3.2 on a synthetic test card
(hue ramp, grey ramp, saturated patches), saved as `.af`; plus the
`cube_*.af` and `blur_r*.af` probes on the cards below. They serve
two purposes: the field layouts above were decoded by reading the
typed values back out of them with afdump, and their embedded
thumbnails — Affinity's own renders — pin each importer's accuracy in
`probed_adjustments_match_affinitys_render`. The same technique
(create → read back → compare to the file's own thumbnail) is how to
attack anything left in the list below.

The single most useful thing to know about that comparison: **for a
512×512 document the embedded thumbnail is a byte-exact render**. It
is a lossless PNG at 1:1, and Affinity's Invert over the test card
comes back bit-identical to `255 - card`. So a document whose canvas
*is* a colour cube hands back the adjustment's entire transfer
function in one file: 512×512 = 262144 pixels is exactly 64³, so
laying the cube out as 64 tiles of 64×64 (tile index = blue, x = red,
y = green) samples every 4th value of every channel, and reading the
thumbnail back gives the complete 3-D LUT with nothing to fit. That
is how vibrance, the lens filter and the white-balance grid below were
solved after per-channel ramps had stalled on them; `cube_*.af` are
those probes. Two cautions: the document really must be 512×512 (a
256² one is thumbnailed at 512² through an interpolating upscale, and
the exactness is gone), and the comparison must not resample — the
`image` crate's `resize` is a convolution even at matching sizes, and
quietly blurs a cube card into a mush the importer then gets blamed
for.

A plain grey ramp is still the right card for anything that is
per-channel or where 8-bit precision along one axis matters more than
coverage (a full 0–255 ramp in grey and in each primary, doubled to
512 wide), and an opaque square on transparency is the card for
fitting a blur's radius convention off its alpha edge.
`python3 tools/probecards.py <dir>` writes all three.

Driving the app from a terminal, since much of the above was found
that way: `System Events` menu clicks work, but a *nested* submenu
click can hang the AppleScript, and killing it mid-tracking leaves a
phantom menu window that swallows clicks over that corner of the
screen until the process is killed — so guard every `osascript` with a
timeout, and check `CGWindowListCopyWindowInfo` when clicks stop
landing. Panel fields are custom-drawn (no accessibility children):
click their pixel coordinates, ⌘A, then type. Typing a leading minus
works in some fields and is silently swallowed in others (the White
Balance panel's Tint is one) — paste from the clipboard instead. A
popup menu anchors its current selection under the cursor, so item
positions move after every pick; rescreenshot between them. Save As
over an existing file raises a replace sheet the scripted flow will
not answer, so delete the target first. Two more, both learned the
hard way while probing a grid: never *close and reopen* an adjustment
panel between saves — double-clicking a layer row is the one step that
misfires, and once it does, the keystrokes meant for a panel field
land on the canvas (a stray text layer, a Move/Duplicate sheet, a
Layer Effects dialog) and every probe after it silently records the
previous values. Leave the panel open, Save As straight over it, and
**read the parameters back out of each saved file** before trusting
the batch.

## What's still unknown / to do

Resolved since the last pass, all with the RGB-cube probes described
above: the warmth/tint interaction, vibrance's weighting, the lens
filter's density curve and colour decode, `Gaus`'s radius convention,
and applying single-plane `FlRN` perspective warps. What is left:

- Live filters other than Live Perspective: every class and its fields
  are decoded (table above) but none are applied — Twirl, Pinch/Punch,
  Spherical, Ripple, Lens Distortion, Pixelate, Glitch, the blurs, the
  sharpeners, the noise filters, Lighting, Shadows/Highlights,
  Vignette, Halftone and Voronoi all import as nothing but a line in
  the report. Perspective's own "Two planes" mode (`DMod`, the
  `DSrA`/`DDsA` + `DSrB`/`DDsB` quad pairs) is likewise decoded and
  not applied. A live filter standing as a *layer* — rather than
  hanging off one layer's `AdCh` — would also have to warp everything
  its scope covers below it, which our compositor has no notion of.
- Split toning: the key, the balance split and the grey-ramp strength
  are measured (see `STPa` above), but how the tint combines with a
  pixel's own colour is not, so the layer still imports as a no-op
  that keeps its native parameters. Soft Proof, LUT, OCIO, Normals and
  Tone Compression/Stretch have not been probed at all.
- Vibrance is a fit, not a derivation: the hue window and the chroma
  curve are tables read off one probe each, and the 0.7 exponent on
  the slider is fitted. 2–3 RMS over the whole cube is what that
  costs; a closed form would presumably be exact.
- Spirals (`ShSp` — stroke-only, not normalised into `ShpB`) and QR
  codes are reported, not rebuilt; the curved-star bow and the tear
  profile are single-fixture fits.
- Text: single style per layer (first run wins), no per-run styling.
- Layer effects: the field mapping is settled, `Gaus` has a home, and
  the Intensity ramp, the blur radius, the stroke's edge and the
  bevel's ramp have all been probed at two radii. Shadows, glows and
  strokes now land at 0.1-3.3 RMS. What is left:
  - the **bevel**, which needs re-implementing rather than
    recalibrating (see the ramp measurements above): a distance-field
    ramp `Radi` wide, the measured profile, and one more probe at a
    second `Dept` to pin what `Dept` scales. Its `Prof` contour
    profile is also always null so far, so that encoding is unknown.
  - `PhgB`, the 3D effect (`Ambi`/`Diff`/`Spec`/`Expo` over a `Lits`
    array of `PLig` lights), which has no home in our layer style.
- ICC profiles: `ICCP` nodes carry the profile name and blob; every
  corpus sighting is sRGB, so no conversion has been needed yet.
- One corpus file (a thought-bubble collage) still renders its
  rotated/mirrored clipped children a few percent off where live
  Affinity places them, while matching the file's own thumbnail —
  the residual convention there is unresolved.
- Export: text, shapes and paths write rasterized pixels; emitting
  native `TxtA`/`ShpN`/`PCrv` nodes (the `AfSh` block already keeps a
  shape's `Shpe` subtree) would keep them live in Affinity. Schist-
  native adjustments without a preserved block are skipped (only
  parameter-free Invert exports) — building `AdjP` classes from our
  params by inverting the import tables is the fix. (The `#FT4`
  per-entry trailing u32 looked random until live Affinity rejected a
  file omitting it as corrupted — it is a CRC-32 of the compressed
  payload, now written and documented above.)
- Exports open in real Affinity 3.2 (the whole 25-file corpus,
  re-exported, opens with structure and rendering intact — the
  stream-order invariants above were the blocker); edit-and-resave
  validation, and carrying the source document's spread background
  colour instead of the template's, are still open.

`cargo run -p schist-codec-affinity --example afdump -- file.afphoto`
prints any file's container listing and full object graph;
`--example afschema` prints wire-level class layouts (the exporter's
transcription source); `--example afrev` lists an entry's savepoint
revisions; `--example afwrite` re-exports any importable file as .af
(`--demo` writes a synthetic exercise document).

[afread]: https://github.com/VMDevCpp/afread
[AFDesignLoad]: https://github.com/NickBeeuwsaert/AFDesignLoad
