# Versioning and branching

## Versions

Schist uses semantic versioning (`MAJOR.MINOR.PATCH`) across the whole
workspace — every crate shares one version, set in the root `Cargo.toml`.

Two surfaces carry compatibility promises:

* **The plugin ABI** (`schist-plugin-host-wasm::abi::ABI_VERSION`) is
  versioned independently and changes rarely. Additive changes (new optional
  exports) keep the number; anything that would break an existing plugin
  bumps it, and the host then refuses older plugins with a clear message
  rather than mis-executing them.
* **PSD round-tripping.** Files written by any version must open in any
  later version. Blocks we don't interpret are preserved verbatim, so this
  holds even as new features land.

Before 1.0, minor versions may break internal Rust APIs between crates;
they will not break the plugin ABI or saved files.

## Branches

* `main` is always releasable: CI (fmt, clippy with `-D warnings`, the full
  test suite on Linux/macOS/Windows) must be green.
* Feature work happens on branches and merges via pull request.
* Releases are cut by tagging `vX.Y.Z` on `main`, which triggers
  `.github/workflows/release.yml` to build and attach installers.

## Updating

Schist checks GitHub for the latest release from Check for Updates —
under File, or the application menu on macOS — and once a day at launch while the "Check for new releases at
launch" preference is on (it is on by default; turning it off stops every
unattended request). The check sends nothing but the request, and a
download only starts when the user presses Update.

What happens next depends on the platform:

* **macOS** downloads `Schist.zip`, unpacks it beside the running bundle
  with `ditto`, and swaps it in with a rename. The new bundle is refused
  unless it is signed at least as well as the one it replaces: a signed
  copy only takes an update signed by the same team, and a signature that
  fails `codesign --verify --strict` is never installed. A relauncher
  waits for the editor to exit and opens the new bundle.
* **Windows** downloads `Schist-<version>-setup.exe` and hands it to a
  detached process that waits for the editor to exit — a running
  `schist.exe` cannot be overwritten — then runs it silently (`/S`,
  elevated, which is one UAC prompt) and starts the result. The installer
  is unsigned for now, so the download is only as good as its HTTPS
  connection and the SHA-256 GitHub records for the asset, which is
  checked when present.
* **Linux** installs nothing itself. A copy from `pacman`, `apt`, `dnf` or
  an AppImage belongs to whatever put it there, so the dialog names the
  new version and links to the release.

Self-updating only offers itself where the copy is one Schist may
replace: inside a writable `.app` bundle on macOS, and next to the
`uninstall.exe` the installer writes on Windows. A loose `schist.exe` or a
`cargo run` build is left alone.

The updater picks its download out of the release by asset name —
`Schist.zip` and `Schist-<version>-setup.exe`, matched in
`crates/app/src/update.rs`. Renaming either in `release.yml` without
changing it there ends self-updating silently, so keep the two together.

## Releasing

1. Update the version in the root `Cargo.toml`, `packaging/macos/Info.plist`
   and both `packaging/macos/quicklook/*-Info.plist` (an app extension
   carries its own version, and macOS re-registers one whose version
   moved).
2. `cargo test --workspace` and `cargo clippy --workspace --all-targets -- -D warnings`.
3. Tag: `git tag -a vX.Y.Z -m "vX.Y.Z" && git push origin vX.Y.Z`.
4. The release workflow builds, for x86_64 and aarch64, a Linux AppImage
   plus the native packages `packaging/linux/packages.sh` emits (`.deb`,
   `.rpm` and a binary `.pkg.tar.zst` — the last one is a convenience
   build, not the AUR package from step 5), a macOS `Schist.dmg` and
   `Schist.zip` (both signed and notarized when the secrets below exist
   — the disk image is the one to point people at, the zip is for
   anything that unpacks a download itself), and a Windows installer,
   then drafts the release. Each
   platform also ships the `schist-mcp` server from the same build: a
   loose arch-suffixed binary on Linux (`schist-mcp-linux-x86_64`,
   `schist-mcp-linux-aarch64`), a loose binary on Windows, and on macOS
   `schist-mcp-macos.zip`, signed and notarized on its own. See
   [mcp.md](mcp.md).
5. Update the AUR package from `packaging/linux/aur/PKGBUILD`: bump
   `pkgver` to the new version, reset `pkgrel=1`, then in an Arch
   environment run `updpkgsums` (re-pins the tag tarball's sha256), test
   with `makepkg -s` + `namcap`, and regenerate `.SRCINFO` with
   `makepkg --printsrcinfo > .SRCINFO` — the AUR rejects pushes without
   a current one. Commit `PKGBUILD` + `.SRCINFO` to the AUR remote
   (`ssh://aur@aur.archlinux.org/schist.git`) and mirror the `PKGBUILD`
   change back here. The PKGBUILD keeps `options=(!lto)` (makepkg's
   `-flto=auto` breaks `ring`'s C objects under the clang link) and
   `clang`/`mold` in `makedepends` for the linker settings in
   `.cargo/config.toml` — don't drop either when touching it.

Unsigned builds are still produced when signing credentials are absent, so
forks and local builds work without secrets.

## Signing secrets

macOS signing and notarization are driven entirely by repository secrets.
Set all five and tagged builds come out notarized and stapled; leave them
unset and the same workflow produces an unsigned bundle.

| Secret | What it is |
| --- | --- |
| `MACOS_CERT_P12_BASE64` | The *Developer ID Application* certificate and its private key, exported from Keychain Access as a `.p12` and then `base64 -i cert.p12 \| pbcopy`. |
| `MACOS_CERT_P12_PASSWORD` | The password set on that `.p12` during export. |
| `APPLE_ID` | The Apple ID of an account on the developer team. |
| `APPLE_APP_SPECIFIC_PASSWORD` | An app-specific password for that Apple ID, from appleid.apple.com — not the account password. |
| `APPLE_TEAM_ID` | The ten-character team ID, the part in brackets in the certificate's name. |

The workflow imports the certificate into a keychain it creates in
`$RUNNER_TEMP` and throws away with the runner, so it never touches the
login keychain. `KEYCHAIN_PASSWORD` may be set to pin that keychain's
password; otherwise the job generates a random one, which is all it needs
since nothing outside the job ever unlocks it.

The identity name is read back out of the imported certificate rather than
configured, so renewing the certificate means replacing two secrets and
nothing else.

Signing runs under the hardened runtime, which notarization requires. That
in turn requires the JIT entitlement in
[`packaging/macos/entitlements.plist`](../packaging/macos/entitlements.plist):
the plugin host compiles every plugin with Cranelift at load time, and
without that entitlement a notarized build dies as soon as it loads one.

Windows builds are not signed — there is no certificate for them yet, so
the installer triggers SmartScreen on first download.
