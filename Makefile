# Schist build orchestration.
#
# `cargo build` builds the app and is still the thing to reach for. This
# file exists for the one job cargo cannot do on its own: the Photoshop
# plug-in helpers.
#
# A `.8bf` plug-in is a binary for a particular OS and architecture, and
# it is very often not the one Schist was compiled for — a 32-bit Windows
# filter on 64-bit Linux, an Intel filter on an Apple Silicon Mac. Schist
# runs each one in a helper process built for the *plug-in's* target, so
# every install needs several helpers alongside a single app binary.
# Cargo builds one target per invocation, so something has to drive it
# once per architecture. That is all this is.
#
#   make helpers                  the helpers this platform can use
#   make helpers PROFILE=debug    ... beside a debug build
#   make install-helpers DESTDIR=path/to/somewhere
#   make all                      app and helpers
#
# Deliberately *not* a build.rs: a build script that shells out to cargo
# re-enters a cargo that already holds the lock on target/, and blocks
# until it times out.

CARGO   ?= cargo
RUSTUP  ?= rustup
PROFILE ?= release

HELPER_CRATE := schist-plugin-host-8bf
HELPER_BIN   := schist-8bf-helper

# Helpers always build with the `helper` profile, whatever the app is
# built with: they are a shipping artifact either way, and the profile
# strips them. Debug info is 86% of a helper's size and nothing reads it
# -- see the profile's comment in Cargo.toml.
HELPER_PROFILE := helper

# `--release` names the directory `release`; the default profile builds
# into `debug` and takes no flag at all.
ifeq ($(PROFILE),release)
  PROFILE_FLAG := --release
else
  PROFILE_FLAG :=
endif

DESTDIR ?= target/$(PROFILE)

# Where `make build` stages the helpers for the app build to embed. An
# absolute path: build.rs turns these into `include_bytes!` arguments,
# and cargo runs it from the crate directory rather than this one.
HELPER_STAGE := $(CURDIR)/target/helper-bundle/$(PROFILE)

# Only used to name the Windows installer, and only read on Windows.
VERSION := $(shell sed -n '0,/^version = /s/^version = "\(.*\)"/\1/p' Cargo.toml)

# Windows sets OS in the environment and may have no `uname` at all, so
# it is checked first; MSYS and Cygwin set it too and are Windows for
# this purpose. Anything unrecognised is an error rather than a guess —
# defaulting would silently build the wrong architectures.
ifeq ($(OS),Windows_NT)
  HOST := windows
else
  UNAME_S := $(shell uname -s)
  ifeq ($(UNAME_S),Linux)
    HOST := linux
  else ifeq ($(UNAME_S),Darwin)
    HOST := macos
  else
    HOST := unknown
  endif
endif

# Which plug-ins this platform can host, from the table in
# `crates/plugin-host-8bf/src/launch.rs`. Linux and Windows both host
# Windows plug-ins — Linux by way of Wine, which runs the same PE binary
# — so both build a pair of `.exe` helpers, differing only in whether
# they link against mingw or MSVC.
ifeq ($(HOST),linux)
  HELPER_TARGETS := x86_64-pc-windows-gnu i686-pc-windows-gnu
else ifeq ($(HOST),macos)
  HELPER_TARGETS := aarch64-apple-darwin x86_64-apple-darwin
else ifeq ($(HOST),windows)
  HELPER_TARGETS := x86_64-pc-windows-msvc i686-pc-windows-msvc
else
  HELPER_TARGETS :=
endif

# What each helper is called once installed. These names are not
# decoration: `Helper::file_name` looks a helper up by this exact string,
# so changing one here is a runtime failure rather than a build one.
# `tests/launch.rs` pins the same names from the Rust side.
name-x86_64-pc-windows-gnu  := schist-8bf-helper-x86_64.exe
name-i686-pc-windows-gnu    := schist-8bf-helper-x86.exe
# These two are also spelled out in .github/workflows/release.yml, whose
# Windows job stages helpers without make: MSYS make would hand build.rs
# an /d/a/... path that a native Windows build cannot read. Both sides are
# pinned from Rust by `Helper::file_name` in tests/launch.rs.
name-x86_64-pc-windows-msvc := schist-8bf-helper-x86_64.exe
name-i686-pc-windows-msvc   := schist-8bf-helper-x86.exe
name-x86_64-apple-darwin    := schist-8bf-helper-x86_64
name-aarch64-apple-darwin   := schist-8bf-helper-arm64

# Cargo's own output name differs from the installed one only by the
# extension, which Windows targets carry and Unix ones do not.
exe = $(if $(findstring windows,$(1)),.exe,)

HELPERS := $(foreach t,$(HELPER_TARGETS),$(DESTDIR)/$(name-$(t)))

.DEFAULT_GOAL := help
.PHONY: help all app build web helpers install-helpers preflight release check-bundle clean-helpers FORCE

help:
	@echo 'make build            the app, carrying the plug-in helpers ($(PROFILE))'
	@echo 'make release          build and package into dist/'
	@echo
	@echo 'make app              just the Schist binary, no helpers'
	@echo 'make web              the browser build, into dist/web/'
	@echo 'make helpers          just the .8bf plug-in helpers, beside the binary'
	@echo 'make install-helpers DESTDIR=DIR   put the helpers somewhere else'
	@echo
	@echo 'this platform hosts plug-ins built for:'
	@$(foreach t,$(HELPER_TARGETS),echo '  $(t)  ->  $(name-$(t))';)

all: build

# The app, carrying the helpers inside it.
#
# They are staged somewhere of their own rather than reused from beside
# the binary, so that what gets embedded is exactly what this build
# produced and not whatever an earlier `make helpers` left lying there.
build: stage-helpers
	SCHIST_BUNDLED_HELPERS='$(HELPER_STAGE)' $(CARGO) build $(PROFILE_FLAG) -p schist-app

.PHONY: stage-helpers
stage-helpers:
	@$(MAKE) --no-print-directory install-helpers \
	  DESTDIR='$(HELPER_STAGE)' PROFILE='$(PROFILE)'

# Just the binary. Without helpers it still runs; it just has none to
# unpack, and says so if asked to run a plug-in.
app:
	$(CARGO) build $(PROFILE_FLAG) -p schist-app

# The browser deployment, assembled into dist/web/. A script rather than
# rules here: it is one linear pipeline (bindgen, opt, chunk, manifest)
# with nothing make's dependency graph would add. See docs/web.md.
web:
	./tools/web-build.sh

helpers: preflight $(HELPERS)

install-helpers: helpers

# Linking a Windows binary from Linux needs mingw's linker, and rustc's
# failure when it is absent names only `cc`, which is present and is not
# the problem. Say so plainly instead.
preflight:
ifeq ($(HOST),unknown)
	@echo 'error: unrecognised host "$(UNAME_S)"; no idea which helpers to build.' >&2; exit 1
endif
ifeq ($(HOST),linux)
	@command -v x86_64-w64-mingw32-gcc >/dev/null 2>&1 || { \
	  echo 'error: the Windows plug-in helpers need mingw-w64 to link.'; \
	  echo '       Debian/Ubuntu: sudo apt install gcc-mingw-w64'; \
	  echo '       Fedora:        sudo dnf install mingw64-gcc mingw32-gcc'; \
	  echo '       Arch:          sudo pacman -S mingw-w64-gcc'; \
	  exit 1; }
endif

$(DESTDIR):
	@mkdir -p $@

# One rule per architecture, generated rather than written out, so the
# list above stays the only place a target is named.
#
# The recipe depends on FORCE and not on the crate's sources: cargo is
# the incremental build system here, and it already knows what changed.
# Restating its dependency graph in make would only be a second, worse
# copy of it — and a stale one the first time a file is added.
define helper_rule
$$(DESTDIR)/$$(name-$(1)): FORCE | $$(DESTDIR)
	@$$(RUSTUP) target list --installed 2>/dev/null | grep -qx '$(1)' \
	  || $$(RUSTUP) target add $(1)
	SCHIST_BUNDLED_HELPERS= $$(CARGO) build --profile $$(HELPER_PROFILE) \
	  -p $$(HELPER_CRATE) --bin $$(HELPER_BIN) --target $(1)
	@cp target/$(1)/$$(HELPER_PROFILE)/$$(HELPER_BIN)$$(call exe,$(1)) $$@
	@echo '  helper   $$@' $$$$(du -h '$$@' 2>/dev/null | cut -f1)
endef
$(foreach t,$(HELPER_TARGETS),$(eval $(call helper_rule,$(t))))

# Packaging. Each script runs its own cargo build, so the staged helpers
# are exported rather than passed: the build inside picks them up and the
# packaged binary carries them, with no change to the scripts themselves.
release: stage-helpers
	@mkdir -p dist
ifeq ($(HOST),linux)
	SCHIST_BUNDLED_HELPERS='$(HELPER_STAGE)' ./packaging/linux/appimage.sh
	SCHIST_BUNDLED_HELPERS='$(HELPER_STAGE)' ./packaging/linux/packages.sh
else ifeq ($(HOST),macos)
	SCHIST_BUNDLED_HELPERS='$(HELPER_STAGE)' ./packaging/macos/bundle.sh $(PROFILE)
else ifeq ($(HOST),windows)
	SCHIST_BUNDLED_HELPERS='$(HELPER_STAGE)' $(CARGO) build $(PROFILE_FLAG) -p schist-app -p schist-mcp
	cp target/$(PROFILE)/schist.exe target/$(PROFILE)/schist-mcp.exe dist/
	makensis -DVERSION=$(VERSION) packaging/windows/installer.nsi
else
	@echo 'error: nothing to package on this host' >&2; exit 1
endif
	@echo 'packaged into dist/'

# Build the helpers, embed them, and check they unpack. The unpacking
# tests only have something to unpack when a bundle is present, so this
# is the run that exercises them at all.
check-bundle: stage-helpers
	SCHIST_BUNDLED_HELPERS='$(HELPER_STAGE)' \
	  $(CARGO) test -p $(HELPER_CRATE) --lib bundled

clean-helpers:
	rm -f $(HELPERS)
	rm -rf $(HELPER_STAGE)

FORCE:
