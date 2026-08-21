#!/usr/bin/env bash
# Build Schist.app, and sign + notarize it when credentials are present.
# Always leaves a distributable dist/Schist.zip; only that zip is safe to hand
# to CI artifact upload, which would otherwise strip the bundle's permissions
# and break the signature.
#
# Signing is optional -- without it the bundle still builds, unsigned:
#   MACOS_CERT_NAME   "Developer ID Application: … (TEAMID)"
#   MACOS_KEYCHAIN    keychain holding that identity, if not the default one
#
# Notarizing needs signing to have happened, plus either
#   MACOS_NOTARY_PROFILE  a profile saved by `notarytool store-credentials`
# or the three pieces that profile would have stored:
#   APPLE_ID, APPLE_APP_SPECIFIC_PASSWORD, APPLE_TEAM_ID
set -euo pipefail

root="$(cd "$(dirname "$0")/../.." && pwd)"
target="${1:-release}"
app="$root/dist/Schist.app"
zip="$root/dist/Schist.zip"

cargo build --"$target" -p schist-app

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$root/packaging/macos/Info.plist" "$app/Contents/Info.plist"
cp "$root/target/$target/schist" "$app/Contents/MacOS/schist"
cp "$root/packaging/macos/schist.icns" "$app/Contents/Resources/"

signed=false
if [ -n "${MACOS_CERT_NAME:-}" ]; then
    echo "signing with $MACOS_CERT_NAME"
    keychain=()
    if [ -n "${MACOS_KEYCHAIN:-}" ]; then
        keychain=(--keychain "$MACOS_KEYCHAIN")
    fi

    # No --deep: it is deprecated, and there is nothing nested to reach --
    # plugins ship as wasm, not as dylibs.
    codesign --force --options runtime --timestamp \
        --entitlements "$root/packaging/macos/entitlements.plist" \
        "${keychain[@]}" --sign "$MACOS_CERT_NAME" "$app"
    codesign --verify --strict --verbose=2 "$app"
    signed=true
else
    echo "MACOS_CERT_NAME unset: leaving the bundle unsigned"
fi

notary=()
if [ -n "${MACOS_NOTARY_PROFILE:-}" ]; then
    notary=(--keychain-profile "$MACOS_NOTARY_PROFILE")
    if [ -n "${MACOS_KEYCHAIN:-}" ]; then
        notary+=(--keychain "$MACOS_KEYCHAIN")
    fi
elif [ -n "${APPLE_ID:-}" ] && [ -n "${APPLE_TEAM_ID:-}" ]; then
    notary=(--apple-id "$APPLE_ID" --team-id "$APPLE_TEAM_ID"
            --password "${APPLE_APP_SPECIFIC_PASSWORD:-}")
fi

if [ "$signed" = true ] && [ ${#notary[@]} -gt 0 ]; then
    echo "notarizing"
    # Notarization takes a zip, but the ticket is stapled to the bundle, so
    # this upload copy is scratch -- the shippable zip gets made afterwards.
    ditto -c -k --keepParent "$app" "$root/dist/upload.zip"
    xcrun notarytool submit "$root/dist/upload.zip" "${notary[@]}" --wait
    rm -f "$root/dist/upload.zip"

    xcrun stapler staple "$app"
    xcrun stapler validate "$app"
    # What Gatekeeper will say on a machine that has never seen the app.
    spctl --assess --type exec --verbose=2 "$app"
elif [ "$signed" = true ]; then
    echo "no notarization credentials: signed but not notarized"
else
    echo "skipping notarization: the bundle is unsigned"
fi

rm -f "$zip"
ditto -c -k --keepParent "$app" "$zip"

echo "built $app"
echo "built $zip"
