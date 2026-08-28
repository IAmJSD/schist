#!/usr/bin/env bash
# Build an AppImage. Requires `appimagetool` on PATH (or set APPIMAGETOOL).
# The native packages -- .deb, .rpm, .pkg.tar.zst -- are packages.sh's job;
# both ship the same tree, staged by payload.sh.
set -euo pipefail

# shellcheck source=packaging/linux/payload.sh
source "$(dirname "$0")/payload.sh"
root="$payload_root"
# Scratch, not a deliverable: dist/ is uploaded wholesale by CI, and an
# AppDir in there collides with the real artifacts on the release.
appdir="$root/target/Schist.AppDir"
tool="${APPIMAGETOOL:-appimagetool}"

cargo build --release -p schist-app

rm -rf "$appdir"
stage_payload "$appdir"

# appimagetool reads the desktop entry from the AppDir root and looks the
# icon up there by its Icon= key, so both are duplicated out of usr/.
cp "$appdir/usr/share/applications/schist.desktop" "$appdir/schist.desktop"
cp "$appdir/usr/share/icons/hicolor/256x256/apps/com.infrawrench.schist.png" \
   "$appdir/com.infrawrench.schist.png"

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
