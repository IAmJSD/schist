# The Gallery

Half Picasa, half Lightroom: Schist can watch folders of photos and show
them as a browsable gallery, with the editor one double-click away. It is
a desktop feature — a browser tab has no folders to watch and no cameras
to mount, so the whole subsystem is compiled out of the web build and its
menu entries with it.

## Boot

A launch with nothing to open lands in the gallery, empty or not — as
Picasa boots into its library. An empty one is the app's welcome
screen, since a fresh Schist has no other: it says hello and offers
all four ways in — Add Folder… and Import from Camera… to fill the
gallery, Open… and New File… to go straight to a document — and the
top strip carries the last two at every other moment too. Opening a document from the shell (`schist file.psd`),
from Finder, or recovering unsaved work from a crash all win over the
gallery: the editor is where a document lives. Closing the last tab
returns to the gallery exactly as it was left — search, selection,
filters and grouping are session state on the library, so leaving for
the editor and coming back loses nothing until the app closes.

File ▸ New (⌘N, anywhere) opens the preset picker — one card per common
size, a click creates the document on the spot, Custom… opens the full
dialog. Recently opened files live under File ▸ Open Recent, in both
the editor's and the gallery's menus.

A folder dropped on the window is a question rather than a file:
**Add to Gallery** (the default — the folders are watched in place,
nothing copied) or open what is inside as tabs. The dialog says how
many images a scan found, sub-folders included, and the tab route is
capped at a hundred so a dropped camera roll cannot open five thousand
tabs; loose image files dropped alongside open as they always did.

## The view

`View ▸ Browse Gallery` (⌘⇧G / Ctrl+Shift+G, or File ▸ Browse Gallery…)
opens it from anywhere; the same key closes it again. While it is up the
menu bar swaps to the gallery's own menus — on macOS the system bar, in
the window elsewhere, both built from the same description.

The layout is Picasa's: a folder list on the left, a grid of thumbnails
grouped under blue per-folder headers, and a tray along the bottom with
the green Edit button, the selected file's name, the photo count and the
thumbnail-size slider. It keeps its own quieter palette rather than the
panel chrome, but follows the theme choice: the light theme gets
Picasa's warm white lightbox, the dark theme a Lightroom-grey version of
the same room — opening the gallery from a dark editor must not be a
flashbang.

The sidebar's **Group by** chips arrange the grid three ways — **Date**
(the default: capture months, newest first, from EXIF DateTimeOriginal
with the file's own clock as fallback), **Folder** (the directories
scanning found), and **Place** (the nearest gazetteer city to each
photo's EXIF position; the largest within 60 km, so a Manhattan photo
groups under "New York City" rather than a neighbourhood). The choice
persists, the folder list keeps filtering in every mode, and the
content filter applies whatever the grouping. Capture time, position
and city are read in one EXIF pass per photo and cached beside the
thumbnail (`.meta`).

The sidebar's **World Map** view plots photos with EXIF GPS locations
on a worldwide OpenStreetMap, with a small photo preview inside each
pin. The full-world overview keeps map tiles and labels proportional;
the map repeats horizontally to fill wider panels without cropping continents. Drag to pan, scroll
or use **− / +** to zoom, and **Reset view** to restore the starting view.
Nearby photos share a pin with a count; clicking it
opens a strip of all its photos, including photos taken at exactly the
same spot. Right-click a marker to add all its photos to an existing
bucket or a new named bucket; this uses the marker’s photos regardless
of the current selection. Double-click a photo to edit it. **Photos** returns to the
grid. Both views follow the same folder, bucket, search, map and content
filters. Exactly **0,0** is treated as missing GPS, including in cached metadata.
Photos without GPS are counted above the map; locations appear
as background indexing finishes. Map tiles use the existing disk cache
and need an internet connection when first viewed. This map keeps its
own camera, independent of the import and map-filter boundary.

**Map filter** (sidebar, or Gallery ▸ Map Filter…) opens the same
navigable map as the import dialog: draw a boundary (or jump to a
preset) and Apply, and the grid shows only photos whose EXIF position
falls inside it — everywhere: every grouping, the search results, the
counts. It lasts for the session (a fresh launch starts unfiltered)
and announces itself while on: a blue "Map filter: <place> ✕" banner
in the top strip (click to edit, ✕ to turn off) and a lit sidebar
chip. Applying with nothing drawn turns it off; the boundary is shared
with the import dialog, so what you import by is one Apply from what
you browse by.

⌘-wheel over the grid (Ctrl-wheel on Linux and Windows) resizes the
thumbnails, up for bigger, the same gesture that zooms a canvas; the
tray's slider shows where it landed. The grid has a real scrollbar on
its right edge — the thumb sized and
placed from the scroll extents, draggable, with track clicks jumping
straight there — because a five-thousand-photo grid without one gives
no sense of where you are.

Selection is plural: click selects one, ⌘-click toggles, Shift-click
(and Shift+arrows) span a range, and the tray counts the take. A drag
carries the selection — as the picked-up photo's own thumbnail square
riding the pointer, with a count badge when it carries several — to a
sidebar **folder**, which moves the files (each photo's `.schist`
sidecar and versions travel with it), or to a **bucket**. Buckets are
Picasa's tray with names: persisted baskets photos are dragged into,
viewed by clicking their sidebar row, and acted on as a group from
their right-click menu — **Select all**, **Save all as ZIP…**,
**Process all…** (the batch dialog below) and **Move all to
folder…**.

A drag that leaves for another window leaves Schist altogether: the
moment the pointer is over someone else's window, the internal drag is
handed to the platform's own drag-and-drop — a pasteboard session on
macOS, OLE on Windows, XDND on X11, a data source on Wayland — so
dropping on Finder, Explorer
or a Linux file manager **copies** the photos there. Copy, always and
only: the gallery watches folders in place, and a drag that quietly
moved the originals out of a library would be a poor surprise. The
trigger is "over a foreign window" rather than "outside our
rectangle", since a file-manager window sitting on top of the gallery
is both — except on Wayland, where no client may ask whose window is
under the pointer; there the trigger is the pointer leaving our own
rectangle, which the held button keeps reporting past the edges, and
a file-manager window laid *over* the gallery is the one case it
cannot see. Wayland is also the one platform where the toolkit has to
start the drag itself (only the client that saw the button press
holds the serial a compositor demands), so that lives in our gpui
fork: a `wl_data_source` offering `text/uri-list`, copy only, and the
compositor delivers it wherever the pointer lands.

An archive holds what the gallery shows: an edited photo goes in as
its edit, never the untouched original, and each entry keeps the
photo's own format when that format is lossy — a JPEG stays a JPEG,
byte-for-byte when it is unedited (re-encoding would only cost a
generation) and at quality 92 when it is not. Everything else becomes
a PNG, as does anything lossy nothing here can write back: an edited
HEIC lands as a PNG. A camera raw is the other way round — the capture
is worth more than any rendering of it, and nothing writes one — so an
unedited raw goes in byte-for-byte under its own extension, and only an
edited one becomes a PNG. The writer is hand-rolled: it deflates each
entry and keeps the smaller of deflated and stored (a JPEG or PNG is
already compressed and usually stays as it is), and it streams one
photo at a time, so a whole bucket never has to fit in memory.

"+ New bucket" asks for a name (an empty one falls back to "Bucket N")
and the bucket is born holding the current selection. Photos have
their own right-click menu too: Edit (several selected open as tabs,
capped at a hundred), Reveal in file manager, Add to bucket, Process…,
Move to folder…, and Revert to original for edited photos (the edit
sidecar goes into `versions/` rather than being lost) — acting on the
whole selection when the click lands in it — and one of two ways out: several selected photos offer **Save
N as ZIP…**, a single one offers **Save image as…**, a dialog with the
format (every codec that exports), quality where the format takes
one, and a scale slider from 10% to 100% that says what it comes to
in pixels. It saves the photo's edit when it has one, flattened, and
never touches the original.

**Process…** is the batch dialog, for a selection or a whole bucket:
one recipe run over every photo. It has a turn (rotate by a quarter or
half turn, flip either way), a size (the two built-in waifu2x ×2
upscalers, no download needed), and a colour section where any number
of adjustments stack up — Brightness/Contrast, Levels, Exposure,
Vibrance, Hue/Saturation, Color Balance, Black & White, Photo Filter,
Channel Mixer, Invert, Posterize, Threshold — each with the same
sliders its adjustment layer has in the editor. The recipe applies in
that order: turn, enlarge, then the adjustments go on top as
adjustment layers. Where it lands is the last choice. **Gallery
edits** writes each photo's `.schist` sidecar exactly as ⌘S would —
the adjustments stay live layers you can retune in the editor, an
existing edit is what the recipe applies to, and the previous sidecar
is kept under `versions/` — so the originals are never touched and
the grid shows the results. **Copies beside the originals** flattens
each result to `<name>-edit.<ext>` in the photo's folder (never
overwriting: a second run gets `-2`), and **Copies in a folder…** asks
where; both take a format and, where it applies, a quality. Photos are
processed one at a time on the background executor with the tray
counting, since an upscale is seconds per megapixel.

## The AI panel

The editor's AI panel (View ▸ AI Panel, ⌘⇧A) is the gallery's too, on
its own switch — a chat column beside a grid of photos is different
furniture from one beside a canvas, so each room remembers whether it
is up — while everything else is shared: the harness, the model, and
the conversation itself. What changes is the prompt. A conversation
started in the gallery runs under a prompt that describes the gallery
and its tools; step into the editor and send again, and it is resumed
under the editor's, so the transcript and the agent's memory carry
over and the agent learns where it now is.

The in-app MCP server publishes the gallery beside the document tools,
as `gallery_*`: `gallery_state` (folders, counts, grouping and groups,
selection, buckets and their rules, the current search, the map
filter, the index's progress), `gallery_list` (the grid, one group by
title, or one bucket by name, paged), `gallery_search` (the search
box's own ranking, answered in the reply and shown in the box),
`gallery_thumbnail` (the cached thumbnail as an image, so the agent
can look), `gallery_select`, `gallery_bucket_create` (optionally with
a smart query), `gallery_bucket_add`, `gallery_group_by`,
`gallery_content_filter` (read or switch the content filter, with
counts of flagged, clean and not-yet-scored photos; switching it on
needs the model), `gallery_flagged` (photos by verdict, paged),
`gallery_people` (the People album, or one person's photos by name —
which also shows them in the grid), and `gallery_open`, which takes a
photo into the editor where the document tools then apply.

The headless server has the gallery too. `schist-mcp`, the stdio
server other agents drive, publishes the same `gallery_*` family over
the gallery's files on disk — `library.json`, the scan of its folders,
the index snapshot, the thumbnail and score caches — through the
`schist-gallery` crate that now owns those formats for both. It reads
what the app wrote, so it sees whatever the app has indexed and no
more; `gallery_search` ranks the snapshot's embeddings, `gallery_open`
opens the photo (its edit sidecar when there is one) as a new editing
session, and the bucket tools write to the library file, which the app
reads at launch. Selection and grouping belong to a window and are the
one thing it cannot see. Every tool does what a click would, in front of the
user; photos are named by path throughout.

A bucket can also fill itself. The New/Edit Bucket dialog (right-click
▸ Edit bucket…) takes an optional **rule**: a search query ("dog on a
beach" — same engine and same 0.15 floor as the search box, place
names understood the same fuzzy way), an **area** drawn on the same
navigable map the import filter uses, or both — both means both must
hold. A smart bucket (marked ✦ in the sidebar) re-scores whenever the
index moves, so it keeps itself current as photos are imported, edited
and indexed; its header says what the rule is, and its contents are
the hand-added photos in drop order followed by the matches, best
first. Hand-adds still work on top of a rule; removing a matched photo
doesn't (the rule would just put it back), which is why the menu only
offers it for hand-added ones, and why "Clear" on a smart bucket is
"Clear added photos". A query rule waits politely when the Search
models aren't installed — it matches nothing rather than everything —
while an area rule works standalone from EXIF alone. Rules persist;
the matches are recomputed each session.

The grid drives from the keyboard too: arrow keys move the selection —
left/right by one photo, up/down by a visual row, worked out from the
grid's real width — Enter opens it in the editor, and the grid scrolls
to keep the selection on screen. Escape still belongs to the search.

Folders are watched in place, never copied. Scanning is recursive
(skipping dot-directories), capped at 5000 files, and re-runs every time
the gallery opens. Thumbnails render lazily — a cell coming on screen is
what queues its decode — through `schist-preview`, so a layered PSD costs
its embedded composite rather than a full recomposite, and finished
thumbnails are cached as PNGs under the state directory keyed by path,
mtime and size, so the second launch is instant. HEIC thumbnails need
the same libheif the editor uses; when they start failing for want of
it, the gallery raises the managed-download offer once, and retries the
failed thumbnails after it installs. Camera raw thumbnails come from
the JPEG the camera embedded, turned upright, rather than from
developing the sensor data, which takes a second or more a file.

The grid is virtualised: rows of cells only really exist within a
viewport's height of the screen, and everything further collapses to
measured spacers — a five-thousand-photo library lays out a screenful
of real cells per frame, not five thousand. Index-only loader batches
also repaint on a throttle rather than per batch, so background
indexing cannot saturate the UI with rebuilds.

Memory stays bounded. Decoded thumbnails live under a ~256 MB budget —
past it, the least recently shown are dropped back to the disk cache
they reload from — and the gallery's neural models (the content scorer
and the two search towers, several hundred resident megabytes between
them) load only when their feature is exercised: the towers wait for
the search box to be focused rather than loading on open. When the
gallery leaves the screen for the editor, the models are released and
the thumbnail set parks at a fraction of its budget. Scores, positions
and embeddings persist in caches beside the thumbnails, so a fully
indexed library never reloads the models at all — not for reopening,
not for smart buckets, only for a photo or a query it has never seen.

## The Info tab

When the open file carries EXIF, the editor's side panel grows a tab
row above the colour panel — **Info** first and selected by default,
**Color** beside it — and Info shows what the camera wrote: make and
model, the lens, the exposure on one line (shutter, aperture, ISO,
focal length with its 35 mm equivalent), bias, flash and white balance
and metering, when it was taken, the pixel size in megapixels, the
software, and, when the photo has a position, where — the nearest
gazetteer city with the coordinates and altitude, and the same
navigable map the gallery uses opened on the spot at street scale
with a red-and-white blip on it. The rows are their own scrolling
region — bounded at about three, with a thin thumb beside them when
there is more — and the map sits fixed beneath at 16:9 across the
panel. For a gallery edit the EXIF is read from the original photo
(the sidecar is a PSD and has none). A file
with no EXIF gets the colour panel alone, exactly as before. The tab
choice lasts until the document changes.

## The content filter

Preferences ▸ Gallery ▸ "Hide photos the content filter flags as
explicit" (off by default) keeps flagged photos out of the grid, with
the tray saying how many are hidden. The judgement comes from the
**Content (NSFW Filter)** model — the NSFWJS MobileNet (Infinite Red /
GantMan, MIT), 17 MB, fetched like any other model under Filter ▸
Neural Filters ▸ Manage Models and verified against a pinned hash.
Each photo is scored once, on its thumbnail as it loads, and the scores
are cached beside the thumbnail. A photo is flagged when porn+hentai
reach 0.5, or "sexy" alone is nearly certain (0.9) — that class fires
on bare shoulders and swimwear, so summing it in flagged most of a real
camera roll. Without the model nothing is flagged — the preference says
so, and its switch stays disabled until the model is installed.

The filter holds at the door too: while it is on, flagged photos never
leave in a ZIP — not from a selection, not from a bucket's "Save all",
not when an agent asks — and the status line says how many stayed
behind. The menu rows count only what would actually go.

## Cameras

Import… looks for mounted volumes with a `DCIM` directory — at the root
or one storage-folder down, the way MTP phones nest it. The roots
scanned are `/Volumes`, `/media`, `/run/media/$USER`, `/mnt`, and the
GVFS mount directory (`$XDG_RUNTIME_DIR/gvfs`), which is how an
unlocked iPhone (`afc:`), a PTP camera (`gphoto2:`) or an Android phone
(`mtp:`) appears as files on a Linux desktop.

On macOS an iPhone never mounts as a filesystem, so the gallery asks
**ImageCaptureCore** — the framework behind Image Capture and Photos —
what is plugged in (`crates/app/src/workspace/library_icc.rs`; the
delegate class is assembled at runtime, like the Quick Look providers).
Connected iPhones and PTP cameras appear beside the mounted volumes in
the picker; downloading runs through `requestDownloadFile` with
progress in the tray, the phone must be unlocked with Trust answered
(a locked phone is reported, not hung on), and the place filter is
applied to each file as it lands — a declined photo is downloaded,
inspected and removed, since a device gives no way to read EXIF without
downloading.

Picking a source opens the import options:

* **The map.** A navigable OpenStreetMap view, driven like any web map:
  drag to pan, scroll to zoom about the pointer, ± buttons for steps.
  Shift-drag (or the Draw area button) draws a rectangle on it — the
  boundary — and preset chips ("New York City", "Tokyo", …) jump there
  and set their box, which can then be panned away from or redrawn.
  Tiles are standard rasters from tile.openstreetmap.org, fetched on
  demand with an identifying User-Agent per the tile policy, cached
  under the state directory, attributed in the dialog.
* **The filter.** With a boundary set, only photos whose EXIF GPS
  position falls inside it import; photos without a recorded position
  stay on the camera. No boundary imports everything. The boundary
  survives closing the dialog, so a re-run imports the same place.
* **Destination.** `~/Pictures/Schist Imports/<boundary name>` when
  filtered (the preset's name, or "Selected Area" for a drawn box),
  `…/<volume>` otherwise. Already-imported files (same name and size)
  are skipped, so re-running an interrupted import is safe.

The destination folder joins the watched list automatically.

## Editing and versions

Double-clicking a photo (or Edit in the tray) opens it in the editor.
The original file is never written again. Instead the document's save
path is a hidden sidecar beside the photo:

```
photos/trip/sunset.jpg
photos/trip/.schist/sunset.jpg.psd          the layered edit, ⌘S writes here
photos/trip/.schist/versions/<t>-sunset.jpg.psd   one per save before it
```

Every save first copies the previous sidecar into `versions/` stamped
with the time, so the edit history is a row of ordinary PSD files —
version control that needs no client. Re-opening the photo from the
gallery opens the sidecar, layers intact; the gallery thumbnail renders
from the sidecar too, so the grid shows the edit (badged "edited"), while
the original stays byte-identical for everything else that reads it.

Deleting a photo's `.schist` entry — or the whole directory — reverts it
to the original everywhere; Schist treats an absent sidecar as "never
edited".

## Search

Until the models are installed the box is not a box at all but a
single button, **Enable photo search…**, since one model without the
other searches nothing. It opens a dialog naming both, their sizes and
their licences, with the plain fact that they run locally and no photo
leaves the machine; agreeing starts the download, which reports itself
as a progress bar in the top strip, and the search box takes its place
the moment both have landed.

The box in the gallery's top strip searches photos by what is *in*
them: type "dog on a beach" and the grid becomes one strip ranked by
similarity. It works on embeddings — every photo mapped into the same
512 dimensions as the words, by the two **Search** models in Manage
Models (MobileCLIP-S0's towers, ~46 MB for images and ~170 MB for text,
revision-pinned and hash-verified; the pair was chosen empirically —
its convolutional image tower runs in ~200 ms under tract where a same-
size ViT took eight seconds).

Photos are embedded in the background as their thumbnails process, and
when nothing on screen wants a thumbnail the loader spends its idle
time indexing the rest of the library; the box shows the index's
progress. The finished index persists as one snapshot file in the
state directory, written whenever the loader catches up and restored
in one read at the next launch — so a library is indexed once, not
once per session, and only new or edited photos (matched by mtime)
ever go through the loader again. Vectors are cached beside the thumbnails (`.embed`) and held
in memory — ranking is a dot product over the lot, which at gallery
scale needs no database fancier than a loop. The text side runs a
CLIP byte-level BPE tokenizer implemented in `schist-neural` and pinned
against the reference tokenizer's output, then the text tower, in
milliseconds per query; results update per keystroke, Escape clears,
and results below a 0.15 cosine are dropped rather than padded with
shrugs.

A query that names somewhere also searches *where* photos were taken:
each photo's EXIF position is probed and cached during indexing, and
every one- to three-word window of the query is matched against an
embedded gazetteer (GeoNames cities of 100k+ people plus aliases,
CC-BY 4.0) — exactly, by prefix ("san fran"), or within a typo or two
("new yrok"). A resolved place boosts photos by distance to it, fading
out by three city-radii, and the results header says which place was
understood ("Search results · near New York City"). Location search
works even without the embedding models; with them, "dog in new york"
blends both readings.

A search made while viewing a bucket stays inside it: the bucket
filters first — its hand-added photos and, for a smart bucket, its
rule's current matches — and the query ranks what is left, under a
"Bucket · <name> · Search results" header. The scoping happens before
the two-hundred cut, so a bucket's matches are never crowded out by
the rest of the library, and the search follows the bucket: click
another bucket (or none) under a live query and it re-ranks there,
a smart bucket refilling re-ranks too, and Escape clears the query to
show the whole bucket again. Both `gallery_search` tools take an
optional `bucket` — the in-app one views the bucket as it searches,
so the user sees what the agent saw; the headless one sees only the
hand-added photos, as its `gallery_list` does.

## People

The People album, the way Picasa did it. Two models under Manage
Models do the work, both downloaded once and run locally: **Faces
(Detection)** — UltraFace, the same 1.2 MB detector Skin Smoothing
uses — finds the faces in each photo as its thumbnail goes through the
loader, and **Faces (Recognition)** — SFace from the OpenCV Zoo,
38.7 MB, Apache-2.0 — turns each one into 128 numbers whose cosine says
whether two faces are the same person. Detection runs on the thumbnail
(the detector's own frame is 320×240, so a thumbnail is already more
than it looks at); the recogniser crops big faces from the thumbnail
and small ones — a group photo's — from a 1024 px decode: the one kept
from rendering the thumbnail when the models were already installed,
otherwise one more decode of the original, once. A face the recogniser
cannot embed is recorded as tried, so no photo is asked again on every
pass; a recogniser file that is installed but will not load switches
recognition off for the session rather than leaving every face owed. Until the
models are installed the sidebar's PEOPLE section offers "+ Find
faces…", which opens one licence dialog for both.

Boxes are stored as fractions of the photo, not pixels, so a tag is the
same box in the thumbnail, the viewer and the original. Detections are
cached beside the thumbnail (`.faces`, with the vectors) and ride in
the index snapshot; the names are the user's and live in
`library.json`.

Naming happens in the **viewer**: Space on a selected photo, the tray's
**View** button, or "View & name people" in the right-click menu shows
the photo big with every face boxed, and a PEOPLE IN THIS PHOTO panel
beside it with a crop per face. Click a face (in either place), type a
name, Enter. Names already known complete as you type; a face the
detector missed is added by dragging a box round it on the photo; a
detection that is not a face — or not anyone worth naming — is waved
away with "Not a face". Naming a face teaches the recogniser: every
unnamed face in the library that lands above a 0.5 cosine of the
person's mean vector is put with them at once, as an *automatic* tag
(the tray says how many followed), and photos indexed later get the
same treatment as their faces arrive. Between 0.45 and 0.5 a face is
only offered — "Is this Ann?" with a Yes button — rather than taken
(SFace's authors use 0.363 on aligned crops; ours are plain squares
round the detector's box, which spreads the scores, so the bars sit
above where different people landed in testing). Automatic tags are
marked "auto" in the panel and on their box, with a "Not them" button;
saying no remembers that this face is not that person, so the
recogniser does not put it back — and only hand-named faces feed a
person's mean vector, so one wrong guess cannot steer the next. A
face drawn by hand gets its vector from the viewer's decode on the
spot, so it counts too. Arrows
walk to the next photo, Escape leaves the name field, then the viewer;
Enter with nothing focused opens the photo in the editor, as it does in
the grid. Double-click on a thumbnail still goes straight to the
editor.

The sidebar lists each person with an avatar, their name and photo
count; clicking one shows their photos under a "People · Ann" header.
With a bucket (or folder) on show the People rows work inside it: the
counts are the person's photos in that bucket, the grid shows only
those, and the header says so ("People · Ann · in Trip"). Clicking the
person again lifts the person filter and the bucket is back as it was;
right-click offers Rename… (renaming to a name somebody else already
has merges the two) and Forget person (the faces go back to unnamed;
nothing is deleted). An "Unnamed faces" row gathers every photo with a
detected face nobody has named or dismissed — Picasa's Unnamed album,
where the naming gets done. A person's name in the search box pulls
their photos in ahead of anything the towers rank ("ann at the beach"
finds Ann's beach photos first), and the results header says "· with
Ann"; a name alone needs no search models at all. The `gallery_people`
tool lists the album for the AI panel and the headless server, or one
person's photos by name.

## What is persisted

`~/.config/schist/library.json`: the watched folders, the recent-files
list the start screen shows, the thumbnail size, the buckets, and the
people — every named face (hand-made or automatic), every detected
face waved away, and every "not them". Thumbnail caches
live under the state directory (`~/.local/state/schist/thumbs`) and can
be deleted freely.
