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

## Releasing

1. Update the version in the root `Cargo.toml` and `packaging/macos/Info.plist`.
2. `cargo test --workspace` and `cargo clippy --workspace --all-targets -- -D warnings`.
3. Tag: `git tag -a vX.Y.Z -m "vX.Y.Z" && git push origin vX.Y.Z`.
4. The release workflow builds a Linux AppImage, a macOS `.app`
   (signed/notarized when the `MACOS_CERT_NAME` and `MACOS_NOTARY_PROFILE`
   secrets exist), and a Windows installer, then drafts the release.

Unsigned builds are still produced when signing credentials are absent, so
forks and local builds work without secrets.
