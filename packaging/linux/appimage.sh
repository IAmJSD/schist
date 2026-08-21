#!/usr/bin/env bash
# Build an AppImage. Requires `appimagetool` on PATH (or set APPIMAGETOOL).
set -euo pipefail

root="$(cd "$(dirname "$0")/../.." && pwd)"
appdir="$root/dist/Schist.AppDir"
tool="${APPIMAGETOOL:-appimagetool}"

cargo build --release -p schist-app

rm -rf "$appdir"
mkdir -p "$appdir/usr/bin" "$appdir/usr/share/applications" \
         "$appdir/usr/share/icons/hicolor/256x256/apps"
cp "$root/target/release/schist" "$appdir/usr/bin/"
cp "$root/packaging/linux/schist.desktop" "$appdir/usr/share/applications/"
cp "$root/packaging/linux/schist.desktop" "$appdir/schist.desktop"
if [ -f "$root/packaging/linux/schist.png" ]; then
    cp "$root/packaging/linux/schist.png" \
       "$appdir/usr/share/icons/hicolor/256x256/apps/"
    cp "$root/packaging/linux/schist.png" "$appdir/schist.png"
fi

cat > "$appdir/AppRun" <<'RUN'
#!/bin/sh
HERE="$(dirname "$(readlink -f "$0")")"
exec "$HERE/usr/bin/schist" "$@"
RUN
chmod +x "$appdir/AppRun"

if command -v "$tool" >/dev/null 2>&1; then
    "$tool" "$appdir" "$root/dist/Schist-x86_64.AppImage"
    echo "built $root/dist/Schist-x86_64.AppImage"
else
    echo "appimagetool not found; the AppDir is ready at $appdir" >&2
fi
