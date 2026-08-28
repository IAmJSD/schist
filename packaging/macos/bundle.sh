#!/usr/bin/env bash
# Build Schist.app -- with its two Quick Look app extensions inside --
# and the standalone schist-mcp server, signing and notarizing both when
# credentials are present. Always leaves two
# distributables:
#   dist/Schist.zip            the app bundle
#   dist/schist-mcp-macos.zip  the MCP server, a plain CLI binary
# Only the zips are safe to hand to CI artifact upload, which would otherwise
# strip permissions and symlinks and void the signatures.
#
# Signing is optional -- without it both still build, unsigned:
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
# The server is staged outside dist/ and only its zip is published there: a
# loose binary in dist/ would be uploaded alongside, minus its exec bit.
mcp_stage="$root/target/macos-mcp"
mcp="$mcp_stage/schist-mcp"
mcp_zip="$root/dist/schist-mcp-macos.zip"

# `debug` names a directory under target/, not a cargo flag: it is what
# `cargo build` does with no profile flag at all.
packages=(-p schist-app -p schist-mcp -p schist-quicklook)
if [ "$target" = "debug" ]; then
    cargo build "${packages[@]}"
else
    cargo build --"$target" "${packages[@]}"
fi

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$root/packaging/macos/Info.plist" "$app/Contents/Info.plist"
cp "$root/target/$target/schist" "$app/Contents/MacOS/schist"
cp "$root/packaging/macos/schist.icns" "$app/Contents/Resources/"

# Quick Look ships as two app extensions around one executable: macOS
# allows a bundle only one extension point, and thumbnails (Finder icons)
# and previews (the space-bar panel) are two. Each Info.plist names the
# principal class the binary registers for that role.
install_appex() {
    ext="$app/Contents/PlugIns/$1.appex"
    mkdir -p "$ext/Contents/MacOS"
    cp "$root/packaging/macos/quicklook/$2" "$ext/Contents/Info.plist"
    cp "$root/target/$target/schist-quicklook" "$ext/Contents/MacOS/schist-quicklook"
}
install_appex SchistQuickLookThumbnail thumbnail-Info.plist
install_appex SchistQuickLookPreview preview-Info.plist

# The MCP server ships on its own rather than inside the bundle: it is a stdio
# CLI that a client spawns by path, so it wants a short path and has to be
# reachable by anyone who never installs the app.
rm -rf "$mcp_stage"
mkdir -p "$mcp_stage"
cp "$root/target/$target/schist-mcp" "$mcp"

signed=false
if [ -n "${MACOS_CERT_NAME:-}" ]; then
    echo "signing with $MACOS_CERT_NAME"
    # Expanded as `${keychain[@]+...}` below: bash 3.2, which is what
    # /usr/bin/env finds on a stock macOS, calls an empty array unbound
    # under `set -u`.
    keychain=()
    if [ -n "${MACOS_KEYCHAIN:-}" ]; then
        keychain=(--keychain "$MACOS_KEYCHAIN")
    fi

    # Inside out, and without --deep, which is deprecated: the app's
    # signature seals the extensions inside it, so they have to be signed
    # -- with their own, sandboxed entitlements -- before the app is.
    for ext in "$app/Contents/PlugIns/"*.appex; do
        codesign --force --options runtime --timestamp \
            --entitlements "$root/packaging/macos/quicklook/entitlements.plist" \
            ${keychain[@]+"${keychain[@]}"} --sign "$MACOS_CERT_NAME" "$ext"
        codesign --verify --strict --verbose=2 "$ext"
    done

    codesign --force --options runtime --timestamp \
        --entitlements "$root/packaging/macos/entitlements.plist" \
        ${keychain[@]+"${keychain[@]}"} --sign "$MACOS_CERT_NAME" "$app"
    codesign --verify --strict --verbose=2 "$app"

    # The server gets the same entitlements as the app: it hosts the same
    # wasmtime plugins, so under the hardened runtime it needs MAP_JIT too.
    codesign --force --options runtime --timestamp \
        --entitlements "$root/packaging/macos/entitlements.plist" \
        ${keychain[@]+"${keychain[@]}"} --sign "$MACOS_CERT_NAME" "$mcp"
    codesign --verify --strict --verbose=2 "$mcp"
    signed=true
else
    echo "MACOS_CERT_NAME unset: leaving both unsigned"
fi

# Zipped before notarization, not after: notarytool only takes an archive, and
# unlike the bundle a flat binary gets no stapled ticket, so nothing is added
# to it afterwards -- the submitted zip is the one that ships.
rm -f "$mcp_zip"
ditto -c -k "$mcp" "$mcp_zip"

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
    echo "notarizing the app"
    # Notarization takes a zip, but the ticket is stapled to the bundle, so
    # this upload copy is scratch -- the shippable zip gets made afterwards.
    ditto -c -k --keepParent "$app" "$root/dist/upload.zip"
    xcrun notarytool submit "$root/dist/upload.zip" "${notary[@]}" --wait
    rm -f "$root/dist/upload.zip"

    xcrun stapler staple "$app"
    xcrun stapler validate "$app"
    # What Gatekeeper will say on a machine that has never seen the app.
    spctl --assess --type exec --verbose=2 "$app"

    echo "notarizing the MCP server"
    # A stapled ticket needs a place to live inside the file, which a flat
    # Mach-O has not got -- `stapler staple` refuses one, and spctl only
    # assesses bundles. Gatekeeper looks this ticket up online instead, so
    # "Accepted" is the whole check, and it has to be made by hand: notarytool
    # exits 0 even when the submission comes back Invalid.
    if ! log=$(xcrun notarytool submit "$mcp_zip" "${notary[@]}" --wait 2>&1); then
        printf '%s\n' "$log" >&2
        echo "notarytool failed for $mcp_zip" >&2
        exit 1
    fi
    printf '%s\n' "$log"
    if ! printf '%s' "$log" | grep -q 'status: Accepted'; then
        echo "$mcp_zip was not accepted; see the log above" >&2
        exit 1
    fi
elif [ "$signed" = true ]; then
    echo "no notarization credentials: signed but not notarized"
else
    echo "skipping notarization: nothing is signed"
fi

rm -f "$zip"
ditto -c -k --keepParent "$app" "$zip"

echo "built $app"
echo "built $zip"
echo "built $mcp_zip"
