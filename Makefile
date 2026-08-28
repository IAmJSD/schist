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

# `--release` names the directory `release`; the default profile builds
# into `debug` and takes no flag at all.
ifeq ($(PROFILE),release)
  PROFILE_FLAG := --release
else
  PROFILE_FLAG :=
endif

DESTDIR ?= target/$(PROFILE)

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
name-x86_64-pc-windows-msvc := schist-8bf-helper-x86_64.exe
name-i686-pc-windows-msvc   := schist-8bf-helper-x86.exe
name-x86_64-apple-darwin    := schist-8bf-helper-x86_64
name-aarch64-apple-darwin   := schist-8bf-helper-arm64

# Cargo's own output name differs from the installed one only by the
# extension, which Windows targets carry and Unix ones do not.
exe = $(if $(findstring windows,$(1)),.exe,)

HELPERS := $(foreach t,$(HELPER_TARGETS),$(DESTDIR)/$(name-$(t)))

.DEFAULT_GOAL := help
.PHONY: help all app helpers install-helpers preflight clean-helpers FORCE

help:
	@echo 'make app              build the Schist binary ($(PROFILE))'
	@echo 'make helpers          build the .8bf plug-in helpers for this platform'
	@echo 'make all              both'
	@echo 'make install-helpers DESTDIR=DIR   put the helpers somewhere else'
	@echo
	@echo 'this platform hosts plug-ins built for:'
	@$(foreach t,$(HELPER_TARGETS),echo '  $(t)  ->  $(name-$(t))';)

all: app helpers

app:
	$(CARGO) build $(PROFILE_FLAG) -p schist-app

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
	$$(CARGO) build $$(PROFILE_FLAG) -p $$(HELPER_CRATE) --bin $$(HELPER_BIN) --target $(1)
	@cp target/$(1)/$$(PROFILE)/$$(HELPER_BIN)$$(call exe,$(1)) $$@
	@echo '  helper   $$@'
endef
$(foreach t,$(HELPER_TARGETS),$(eval $(call helper_rule,$(t))))

clean-helpers:
	rm -f $(HELPERS)

FORCE:
