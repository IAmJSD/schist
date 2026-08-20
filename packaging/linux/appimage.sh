#!/usr/bin/env bash
# Build an AppImage. Requires `appimagetool` on PATH (or set APPIMAGETOOL).
set -euo pipefail

root="$(cd "$(dirname "$0")/../.." && pwd)"
appdir="$root/dist/Photoslop.AppDir"
tool="${APPIMAGETOOL:-appimagetool}"

cargo build --release -p photoslop-app

rm -rf "$appdir"
mkdir -p "$appdir/usr/bin" "$appdir/usr/share/applications" \
         "$appdir/usr/share/icons/hicolor/256x256/apps"
cp "$root/target/release/photoslop" "$appdir/usr/bin/"
cp "$root/packaging/linux/photoslop.desktop" "$appdir/usr/share/applications/"
cp "$root/packaging/linux/photoslop.desktop" "$appdir/photoslop.desktop"
if [ -f "$root/packaging/linux/photoslop.png" ]; then
    cp "$root/packaging/linux/photoslop.png" \
       "$appdir/usr/share/icons/hicolor/256x256/apps/"
    cp "$root/packaging/linux/photoslop.png" "$appdir/photoslop.png"
fi

cat > "$appdir/AppRun" <<'RUN'
#!/bin/sh
HERE="$(dirname "$(readlink -f "$0")")"
exec "$HERE/usr/bin/photoslop" "$@"
RUN
chmod +x "$appdir/AppRun"

if command -v "$tool" >/dev/null 2>&1; then
    "$tool" "$appdir" "$root/dist/Photoslop-x86_64.AppImage"
    echo "built $root/dist/Photoslop-x86_64.AppImage"
else
    echo "appimagetool not found; the AppDir is ready at $appdir" >&2
fi
