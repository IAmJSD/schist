#!/usr/bin/env bash
# Build an AppImage. Requires `appimagetool` on PATH (or set APPIMAGETOOL).
set -euo pipefail

root="$(cd "$(dirname "$0")/../.." && pwd)"
# Scratch, not a deliverable: dist/ is uploaded wholesale by CI, and an
# AppDir in there collides with the real artifacts on the release.
appdir="$root/target/Schist.AppDir"
tool="${APPIMAGETOOL:-appimagetool}"

cargo build --release -p schist-app

rm -rf "$appdir"
mkdir -p "$appdir/usr/bin" "$appdir/usr/share/applications" \
         "$appdir/usr/share/mime/packages" \
         "$appdir/usr/share/icons/hicolor/256x256/apps"
cp "$root/target/release/schist" "$appdir/usr/bin/"
cp "$root/packaging/linux/schist.desktop" "$appdir/usr/share/applications/"
# The Affinity MIME types the desktop entry names; a desktop integrator that
# installs the entry finds them here rather than binding it to nothing.
cp "$root/packaging/linux/place.astrid.schist.mime.xml" \
   "$appdir/usr/share/mime/packages/"
cp "$root/packaging/linux/schist.desktop" "$appdir/schist.desktop"
# appimagetool looks the icon up by the desktop entry's Icon= key, so both
# copies have to carry the app ID as their name.
icon="$appdir/usr/share/icons/hicolor/256x256/apps/place.astrid.schist.png"
cp "$root/packaging/linux/schist.png" "$icon"
cp "$icon" "$appdir/place.astrid.schist.png"

cat > "$appdir/AppRun" <<'RUN'
#!/bin/sh
HERE="$(dirname "$(readlink -f "$0")")"
exec "$HERE/usr/bin/schist" "$@"
RUN
chmod +x "$appdir/AppRun"

out="$root/dist/Schist-$(uname -m).AppImage"
if command -v "$tool" >/dev/null 2>&1; then
    "$tool" "$appdir" "$out"
    echo "built $out"
else
    echo "appimagetool not found; the AppDir is ready at $appdir" >&2
fi
