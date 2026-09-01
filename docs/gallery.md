# The Gallery

Half Picasa, half Lightroom: Schist can watch folders of photos and show
them as a browsable gallery, with the editor one double-click away. It is
a desktop feature — a browser tab has no folders to watch and no cameras
to mount, so the whole subsystem is compiled out of the web build and its
menu entries with it.

## Boot

A launch with nothing to open lands on one of two screens:

* **No gallery yet** — the start screen: new-document presets (the same
  rows as File ▸ New), an Open button, the recently opened files, and
  the offer to set a gallery up by choosing a folder or importing from a
  camera.
* **A gallery exists** — the gallery itself, as Picasa boots into its
  library.

Opening a document from the shell (`schist file.psd`), from Finder, or
recovering unsaved work from a crash all win over the gallery: the editor
is where a document lives.

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
mtime and size, so the second launch is instant.

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

* **Place filter.** "Anywhere", or a named place ("taken in New York").
  With a place chosen, only photos whose EXIF GPS position falls inside
  its bounding box import; photos without a recorded position stay on
  the camera. The dialog shows the box on a real OpenStreetMap
  rendering — standard raster tiles from tile.openstreetmap.org
  (fetched with an identifying User-Agent per the tile policy, cached
  under the state directory, attributed in the dialog) with the bounds
  drawn over them.
* **Destination.** `~/Pictures/Schist Imports/<place>` when filtered,
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

## What is persisted

`~/.config/schist/library.json`: the watched folders, the recent-files
list the start screen shows, and the thumbnail size. Thumbnail caches
live under the state directory (`~/.local/state/schist/thumbs`) and can
be deleted freely.
