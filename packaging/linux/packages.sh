#!/usr/bin/env bash
# Build the native Linux packages from one already-built binary:
#
#   dist/schist_VERSION-1_ARCH.deb            dpkg-deb, or ar+tar without it
#   dist/schist-VERSION-1.ARCH.rpm            needs rpmbuild
#   dist/schist-VERSION-1-ARCH.pkg.tar.zst    tar+zstd, .MTREE needs bsdtar
#
# Each format is skipped, loudly, when the tool it needs is absent, so a
# machine with none of them still gets the ones it can build.
#
# Packages are built for the host architecture only: they carry a compiled
# binary, and cargo builds one target per invocation. The Arch package here
# is a *binary* package assembled from this checkout -- it is not what AUR
# users get, which is built from source by packaging/linux/aur/PKGBUILD and
# has to stay in step with the dependency lists below.
set -euo pipefail

# shellcheck source=packaging/linux/payload.sh
source "$(dirname "$0")/payload.sh"

version="$payload_version"
# The package revision, bumped only when the packaging changes under a
# version that has already shipped. The AUR PKGBUILD's pkgrel is its own.
release=1
summary="Layered image editor with PSD and Affinity support"
url="https://github.com/Infrawrench/schist"
# Both a .deb's Maintainer and a pacman package's packager want
# "Name <email>", so there is no useful "unknown" to fall back to.
packager="${PACKAGER:-Infrawrench LLC <astrid@infrawrench.com>}"
builddate="${SOURCE_DATE_EPOCH:-$(date +%s)}"

machine="$(uname -m)"
case "$machine" in
    x86_64)  deb_arch=amd64; rpm_arch=x86_64;  pkg_arch=x86_64 ;;
    aarch64) deb_arch=arm64; rpm_arch=aarch64; pkg_arch=aarch64 ;;
    *) echo "error: no package architecture known for $machine" >&2; exit 1 ;;
esac

dist="$payload_root/dist"
# Scratch, not a deliverable: dist/ is uploaded wholesale by CI, and a
# staging tree in there would collide with the real artifacts.
build="$payload_root/target/linux-packages"

cargo build --release -p schist-app
mkdir -p "$dist"
rm -rf "$build"

built=()

# libheif is dlopen'd at run time for HEIC import -- present or not, the
# app starts -- so it is a weak dependency in all three formats.

build_deb() {
    local work="$build/deb" out size
    mkdir -p "$work/DEBIAN"
    stage_payload "$work"
    # Debian keeps the licence in the documentation directory and nowhere
    # else; /usr/share/licenses is an rpm and pacman convention.
    install -Dm644 "$work/usr/share/licenses/schist/LICENSE" \
        "$work/usr/share/doc/schist/copyright"
    rm -rf "$work/usr/share/licenses"

    # The data archive carries an entry for ./ itself, which dpkg applies
    # to / on install -- so it has to be 755 and not whatever the umask
    # left on the staging directory.
    chmod 755 "$work"

    size="$(du -sk --exclude=DEBIAN "$work" | cut -f1)"
    # Written out by hand rather than by shlibdeps: this runs on whatever
    # machine cut the release, which is not necessarily a Debian one.
    cat > "$work/DEBIAN/control" <<EOF
Package: schist
Version: $version-$release
Section: graphics
Priority: optional
Architecture: $deb_arch
Maintainer: $packager
Installed-Size: $size
Depends: libc6, libfontconfig1, libfreetype6, libxcb1, libxkbcommon0,
 libxkbcommon-x11-0, libwayland-client0, libvulkan1, hicolor-icon-theme
Recommends: libheif1
Homepage: $url
Description: $summary
 Schist is a layered image editor that opens and writes Photoshop (PSD and
 PSB) and Affinity documents, and hosts Photoshop .8bf filter plug-ins.
EOF

    # dpkg-deb does not generate md5sums itself; the Debian helper that
    # normally would is not in play here.
    ( cd "$work" && find usr -type f -exec md5sum {} + > DEBIAN/md5sums )

    # Debian and Ubuntu fire these off dpkg triggers, so this only does
    # anything on a system without the trigger-providing packages -- and
    # then only for whichever of them is installed at all.
    cat > "$work/DEBIAN/postinst" <<'EOF'
#!/bin/sh
set -e
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database -q /usr/share/applications || true
fi
if command -v update-mime-database >/dev/null 2>&1; then
    update-mime-database /usr/share/mime || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -qtf /usr/share/icons/hicolor || true
fi
EOF
    cp "$work/DEBIAN/postinst" "$work/DEBIAN/postrm"
    chmod 755 "$work/DEBIAN/postinst" "$work/DEBIAN/postrm"

    out="$dist/schist_${version}-${release}_${deb_arch}.deb"
    rm -f "$out"
    if command -v dpkg-deb >/dev/null 2>&1; then
        dpkg-deb --root-owner-group --build "$work" "$out" >/dev/null
    else
        # No dpkg here: a .deb is an ar archive of exactly three members in
        # exactly this order, which binutils' ar and tar can write.
        local ar="$build/deb-ar"
        mkdir -p "$ar"
        echo 2.0 > "$ar/debian-binary"
        tar --owner=0 --group=0 --numeric-owner \
            -czf "$ar/control.tar.gz" -C "$work/DEBIAN" .
        tar --owner=0 --group=0 --numeric-owner --exclude=./DEBIAN \
            -czf "$ar/data.tar.gz" -C "$work" .
        ( cd "$ar" && ar rcD "$out" debian-binary control.tar.gz data.tar.gz )
    fi
    built+=("$out")
}

build_rpm() {
    if ! command -v rpmbuild >/dev/null 2>&1; then
        echo "rpmbuild not found; skipping the .rpm" >&2
        return
    fi
    local work="$build/rpm"
    mkdir -p "$work/payload"
    stage_payload "$work/payload"

    # The payload is built and staged already, so the spec only wraps it:
    # no %prep, no %build, and an %install that copies the tree in.
    #
    # Requires are Fedora's names for the libraries -- rpmbuild adds the
    # sonames it can see in the ELF on top, but fontconfig, wayland and the
    # Vulkan loader are dlopen'd and invisible to it.
    cat > "$work/schist.spec" <<EOF
%global debug_package %{nil}
%global _build_id_links none

Name:           schist
Version:        $version
Release:        $release
Summary:        $summary
License:        MIT
URL:            $url
Requires:       fontconfig
Requires:       freetype
Requires:       libxcb
Requires:       libxkbcommon
Requires:       libxkbcommon-x11
Requires:       libwayland-client
Requires:       vulkan-loader
Requires:       hicolor-icon-theme
Recommends:     libheif

%description
Schist is a layered image editor that opens and writes Photoshop (PSD and
PSB) and Affinity documents, and hosts Photoshop .8bf filter plug-ins.

%install
cp -a %{payload}/. %{buildroot}/

%files
%license %{_datadir}/licenses/schist/LICENSE
%{_bindir}/schist
%{_datadir}/applications/schist.desktop
%{_datadir}/icons/hicolor/256x256/apps/com.infrawrench.schist.png
%{_datadir}/mime/packages/com.infrawrench.schist.xml

%post
update-desktop-database -q %{_datadir}/applications &>/dev/null || :
update-mime-database %{_datadir}/mime &>/dev/null || :
gtk-update-icon-cache -qtf %{_datadir}/icons/hicolor &>/dev/null || :

%postun
update-desktop-database -q %{_datadir}/applications &>/dev/null || :
update-mime-database %{_datadir}/mime &>/dev/null || :
gtk-update-icon-cache -qtf %{_datadir}/icons/hicolor &>/dev/null || :
EOF

    # _build_name_fmt keeps the result in dist/ rather than dist/<arch>/;
    # the %% survive one round of macro expansion at --define time.
    rpmbuild -bb --quiet --target "$rpm_arch" \
        --define "_topdir $work" \
        --define "payload $work/payload" \
        --define "_rpmdir $dist" \
        --define "_build_name_fmt %%{NAME}-%%{VERSION}-%%{RELEASE}.%%{ARCH}.rpm" \
        "$work/schist.spec"
    built+=("$dist/schist-${version}-${release}.${rpm_arch}.rpm")
}

build_pkg() {
    local work="$build/pkg" out size dep
    mkdir -p "$work"
    stage_payload "$work"

    # size is the installed size in bytes, which is what pacman reports and
    # what it checks the disk against.
    size="$(du -sb "$work" | cut -f1)"
    cat > "$work/.PKGINFO" <<EOF
pkgname = schist
pkgbase = schist
pkgver = $version-$release
pkgdesc = $summary
url = $url
builddate = $builddate
packager = $packager
size = $size
arch = $pkg_arch
license = MIT
EOF
    # The same list the AUR PKGBUILD carries: fontconfig, wayland and the
    # Vulkan loader are dlopen'd rather than linked, and belong here even
    # though nothing in the ELF points at them.
    for dep in fontconfig freetype2 hicolor-icon-theme libxcb libxkbcommon \
               libxkbcommon-x11 vulkan-icd-loader wayland; do
        echo "depend = $dep" >> "$work/.PKGINFO"
    done
    echo "optdepend = libheif: HEIC import" >> "$work/.PKGINFO"
    # Recorded in the .MTREE below, so it is set before that is written.
    chmod 644 "$work/.PKGINFO"

    # pacman reads the metadata from the front of the stream, so .PKGINFO
    # goes in first and the payload last.
    local entries=(.PKGINFO)
    if command -v bsdtar >/dev/null 2>&1; then
        # --uid/--gid record the ownership the tar below writes, not this
        # user's, so the two agree and `pacman -Qkk` stays quiet.
        ( cd "$work" && LC_ALL=C bsdtar -czf .MTREE --format=mtree \
            --uid 0 --gid 0 --uname root --gname root \
            --options='!all,use-set,type,uid,gid,mode,time,size,md5,sha256,link' \
            .PKGINFO usr )
        chmod 644 "$work/.MTREE"
        entries+=(.MTREE)
    else
        echo "bsdtar not found; the .pkg.tar.zst ships without a .MTREE" \
             "(it installs, but \`pacman -Qkk\` cannot verify it)" >&2
    fi
    entries+=(usr)

    out="$dist/schist-${version}-${release}-${pkg_arch}.pkg.tar.zst"
    tar --owner=0 --group=0 --numeric-owner -C "$work" -cf - "${entries[@]}" \
        | zstd -q -T0 -19 -c > "$out"
    built+=("$out")
}

build_deb
build_rpm
build_pkg

for f in "${built[@]}"; do
    echo "built $f" "$(du -h "$f" | cut -f1)"
done
