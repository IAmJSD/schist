# The Gallery

Half Picasa, half Lightroom: Schist can watch folders of photos and show
them as a browsable gallery, with the editor one double-click away. It is
a desktop feature — a browser tab has no folders to watch and no cameras
to mount, so the whole subsystem is compiled out of the web build and its
menu entries with it.

## Boot

A launch with nothing to open lands in the gallery, empty or not — as
Picasa boots into its library. An empty one offers the ways in (add a
folder, import from a camera), and the top strip always carries Open…
and New File…. Opening a document from the shell (`schist file.psd`),
from Finder, or recovering unsaved work from a crash all win over the
gallery: the editor is where a document lives.

File ▸ New (⌘N, anywhere) opens the preset picker — one card per common
size, a click creates the document on the spot, Custom… opens the full
dialog. Recently opened files live under File ▸ Open Recent, in both
the editor's and the gallery's menus.

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

Folders are watched in place, never copied. Scanning is recursive
(skipping dot-directories), capped at 5000 files, and re-runs every time
the gallery opens. Thumbnails render lazily — a cell coming on screen is
what queues its decode — through `schist-preview`, so a layered PSD costs
its embedded composite rather than a full recomposite, and finished
thumbnails are cached as PNGs under the state directory keyed by path,
mtime and size, so the second launch is instant. HEIC thumbnails need
the same libheif the editor uses; when they start failing for want of
it, the gallery raises the managed-download offer once, and retries the
failed thumbnails after it installs.

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
progress. Vectors are cached beside the thumbnails (`.embed`) and held
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

## What is persisted

`~/.config/schist/library.json`: the watched folders, the recent-files
list the start screen shows, and the thumbnail size. Thumbnail caches
live under the state directory (`~/.local/state/schist/thumbs`) and can
be deleted freely.
