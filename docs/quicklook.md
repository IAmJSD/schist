# Quick Look on macOS

Finder asks an app for two different pictures of a document: the
thumbnail it draws as the file's icon, and the preview the space bar
opens. Schist supplies both for the formats nothing else on the system
can read — `.psd`, `.psb`, `.afphoto`, `.afdesign`, `.afpub` and `.af` —
so those files stop looking like blank pages in a folder.

Nothing has to be enabled. The extensions live inside `Schist.app`, and
macOS registers them the first time the app is launched from a location
Launch Services scans (`/Applications` or `~/Applications`). Moving the
app re-registers them; deleting it takes them with it.

## What it renders

Cheapest path first:

1. **The embedded preview.** Photoshop writes a JPEG thumbnail into
   image resource `0x040C`; Affinity writes a PNG of the page into its
   container. Both are the writing app's own render, so whenever one is
   at least as large as the size Quick Look asked for, it is used
   as-is — and for a document over 64 megapixels it is used even when it
   is smaller, because nothing else can answer in the time an extension
   is given.
2. **A composite.** Otherwise the file opens through Schist's own
   codecs and is composited exactly as the editor would: layers, groups,
   masks, blend modes, adjustment layers and layer effects. The result
   is scaled down in bands of tiles, so a 400-megapixel PSB is rendered
   in about 64 MB rather than in gigabytes.

If the document cannot be opened at all, an embedded preview is still
shown when the file has one. A file with neither produces no
preview — Quick Look falls back to the document icon, which is what it
would have shown anyway.

## How it is built

`crates/preview` does the rendering and knows nothing about macOS.
`crates/quicklook` is the extension binary: it registers two
Objective-C classes with the runtime — `SchistThumbnailProvider`, a
`QLThumbnailProvider`, and `SchistPreviewProvider`, a
`QLPreviewProvider` conforming to `QLPreviewingController` — and then
calls `NSExtensionMain`. There is no Objective-C source and no Xcode
project; the classes are assembled at startup, before the host looks up
the principal class its Info.plist names.

macOS allows one extension point per bundle and thumbnails and previews
are two, so `packaging/macos/bundle.sh` wraps that one executable in two
`.appex` bundles under `Schist.app/Contents/PlugIns`, each with its own
Info.plist. Both are sandboxed (`packaging/macos/quicklook/entitlements.plist`)
and are signed before the app, whose signature seals them. `make release`
is what runs that script on macOS.

Data-based previews need macOS 12. On macOS 11 the preview extension's
class does not exist, the binary notices and skips it, and thumbnails —
the older extension point — still work.

## Looking at what it produces

The extension binary renders from the command line too, which needs no
signing, no installation and no Finder:

```sh
cargo run -p schist-quicklook -- --render file.afphoto out.png [max-edge]
```

It prints the size and which of the two paths above produced it.

To exercise the real thing, build the bundle and let Launch Services see
it. `make release` cross-builds the `.8bf` plug-in helpers on the way,
which the extensions never touch, so the script it wraps builds the same
bundle without them — `./packaging/macos/bundle.sh debug` — if the rustup
targets those helpers need are not installed:

```sh
make release PROFILE=debug
cp -R dist/Schist.app ~/Applications/
# Sign them the way bundle.sh does -- inside out, the extensions with
# their own sandboxed entitlements first -- so what is tested matches
# what ships. An ad-hoc signature is enough for a local run.
for ext in ~/Applications/Schist.app/Contents/PlugIns/*.appex; do
    codesign --force --sign - \
        --entitlements packaging/macos/quicklook/entitlements.plist "$ext"
done
codesign --force --sign - \
    --entitlements packaging/macos/entitlements.plist ~/Applications/Schist.app

# Registration happens when the app is launched, not when it is copied.
open -g -j ~/Applications/Schist.app
pluginkit -m | grep schist
```

Two identifiers listed means both extensions are live; Finder and the
space bar will use them from then on. Nothing listed means the app was
not registered — that is Launch Services, not Quick Look, and a bundle
sitting in a build directory is not somewhere it looks.

`qlmanage -p file.afphoto` opens the preview panel the extension fills.
For thumbnails, ask the framework directly rather than through Finder's
icon cache, which will happily keep showing an older answer:

```swift
// swift probe.swift file.afphoto
import QuickLookThumbnailing
let request = QLThumbnailGenerator.Request(
    fileAt: URL(fileURLWithPath: CommandLine.arguments[1]),
    size: CGSize(width: 512, height: 512), scale: 1,
    representationTypes: .thumbnail)
let done = DispatchSemaphore(value: 0)
QLThumbnailGenerator.shared.generateBestRepresentation(for: request) { rep, err in
    print(rep.map { "\($0.cgImage.width)x\($0.cgImage.height)" }
        ?? "none: \(String(describing: err))")
    done.signal()
}
_ = done.wait(timeout: .now() + 30)
```

Each extension renders into its own sandbox container, so
`~/Library/Containers/place.astrid.schist.quicklook-*/Data/tmp/schist-quicklook`
holds exactly the PNGs it handed Quick Look — the quickest way to see
what a provider actually produced.

## Which files it claims

The `QLSupportedContentTypes` in both Info.plists are exactly the type
identifiers `Schist.app` imports. Where Affinity itself is installed it
declares its own (`com.canva.affinity*`) and previews them with its own
extension; Schist does not shadow those. Photoshop's identifiers are
system-declared, so `.psd` previews resolve to whichever extension the
system prefers — and `.psb`, which nothing else declares, is Schist's
either way.
