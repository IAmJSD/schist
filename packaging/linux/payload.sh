# Sourced, not run: the file layout every Linux package installs.
#
# The AppImage and the three native packages (packages.sh) ship the same
# files under the same prefix; only the metadata wrapped around them
# differs. Keeping the layout here means a new file lands in all four at
# once rather than in whichever script was edited.

payload_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
payload_root="$(cd "$payload_dir/../.." && pwd)"

# The one version the whole workspace shares -- the same line the Makefile
# reads to name the Windows installer.
payload_version="$(sed -n '0,/^version = /s/^version = "\(.*\)"/\1/p' \
    "$payload_root/Cargo.toml")"

# stage_payload DIR -- fill DIR with the /usr tree a package installs.
stage_payload() {
    local dest="$1"

    install -Dm755 "$payload_root/target/release/schist" "$dest/usr/bin/schist"
    install -Dm644 "$payload_dir/schist.desktop" \
        "$dest/usr/share/applications/schist.desktop"
    # The desktop entry looks its icon up by the Icon= key, so the file has
    # to carry the app ID as its name rather than the source's.
    install -Dm644 "$payload_dir/schist.png" \
        "$dest/usr/share/icons/hicolor/256x256/apps/com.infrawrench.schist.png"
    # The Affinity MIME types the desktop entry names; without them the
    # entry associates with nothing. update-mime-database compiles them at
    # install time.
    install -Dm644 "$payload_dir/com.infrawrench.schist.mime.xml" \
        "$dest/usr/share/mime/packages/com.infrawrench.schist.xml"
    install -Dm644 "$payload_root/LICENSE" \
        "$dest/usr/share/licenses/schist/LICENSE"
}
