#!/usr/bin/env bash
# Build Schist.app, and sign + notarize it when credentials are present.
#
# Signing needs (all optional; the bundle still builds without them):
#   MACOS_CERT_NAME      "Developer ID Application: … (TEAMID)"
#   MACOS_NOTARY_PROFILE  a `xcrun notarytool store-credentials` profile
set -euo pipefail

root="$(cd "$(dirname "$0")/../.." && pwd)"
target="${1:-release}"
app="$root/dist/Schist.app"

cargo build --"$target" -p schist-app

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$root/packaging/macos/Info.plist" "$app/Contents/Info.plist"
cp "$root/target/$target/schist" "$app/Contents/MacOS/schist"
if [ -f "$root/packaging/macos/schist.icns" ]; then
    cp "$root/packaging/macos/schist.icns" "$app/Contents/Resources/"
fi

if [ -n "${MACOS_CERT_NAME:-}" ]; then
    echo "signing with $MACOS_CERT_NAME"
    codesign --force --deep --options runtime --timestamp \
        --sign "$MACOS_CERT_NAME" "$app"
    codesign --verify --strict --verbose=2 "$app"
else
    echo "MACOS_CERT_NAME unset: leaving the bundle unsigned"
fi

if [ -n "${MACOS_NOTARY_PROFILE:-}" ]; then
    echo "notarizing"
    ditto -c -k --keepParent "$app" "$root/dist/Schist.zip"
    xcrun notarytool submit "$root/dist/Schist.zip" \
        --keychain-profile "$MACOS_NOTARY_PROFILE" --wait
    xcrun stapler staple "$app"
else
    echo "MACOS_NOTARY_PROFILE unset: skipping notarization"
fi

echo "built $app"
