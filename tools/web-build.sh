#!/usr/bin/env bash
# Build the browser deployment of Schist into dist/web/.
#
#   tools/web-build.sh                # release build
#   tools/web-build.sh --debug        # much faster, much larger
#
# What it does, and why it isn't just cargo:
#   1. cargo build for wasm32-unknown-unknown.
#   2. wasm-bindgen writes the JS glue and the processed module. The CLI
#      version must equal the crate's in Cargo.lock, so that's checked.
#   3. wasm-opt -Oz (when installed) shaves 10-20% off the module.
#   4. The module is SPLIT into fixed-size chunks. One ~20 MB .wasm
#      downloads as a single opaque stall; chunks download over parallel
#      connections and give the loading page a byte-accurate bar. The
#      loader reassembles them before instantiation.
#   5. Everything the native build embeds (icons, fonts) or carries in
#      the store (neural models) is copied out as plain files: the wasm
#      carries no assets at all. Icons and fonts are fetched by the
#      loading page; models are fetched by the app on demand.
#   6. manifest.json tells the loader what to fetch and how big it is.
#
# Serve dist/web/ from any static server over localhost or https (WebGPU
# needs a secure context):  python3 -m http.server -d dist/web 8000

set -euo pipefail
cd "$(dirname "$0")/.."

# The `web` profile is release minus debuginfo: DWARF was a third of
# the wasm bytes and nothing in a browser reads it.
PROFILE=web
PROFILE_FLAG='--profile web'
if [ "${1:-}" = "--debug" ]; then
  PROFILE=debug
  PROFILE_FLAG=
fi

TARGET=wasm32-unknown-unknown
OUT=dist/web
# 4 MiB chunks: enough parallelism to matter, few enough files to not.
CHUNK_MIB=4

command -v wasm-bindgen >/dev/null || {
  echo 'error: wasm-bindgen-cli is not installed.' >&2
  echo '       cargo install wasm-bindgen-cli --version <the Cargo.lock version>' >&2
  exit 1
}
rustup target list --installed 2>/dev/null | grep -qx "$TARGET" \
  || rustup target add "$TARGET"

# A glue/CLI version skew fails at runtime with an inscrutable import
# error, so refuse it here where the message can say what to do.
LOCKED=$(python3 -c '
import re
lock = open("Cargo.lock").read()
m = re.search(r"name = \"wasm-bindgen\"\nversion = \"([^\"]+)\"", lock)
print(m.group(1))')
INSTALLED=$(wasm-bindgen --version | awk '{print $2}')
if [ "$LOCKED" != "$INSTALLED" ]; then
  echo "error: wasm-bindgen-cli $INSTALLED != Cargo.lock's $LOCKED." >&2
  echo "       cargo install wasm-bindgen-cli --version $LOCKED --force" >&2
  exit 1
fi

echo '-- building (this is the whole editor plus tract; it takes a while)'
cargo build --target "$TARGET" $PROFILE_FLAG -p schist-app

rm -rf "$OUT"
mkdir -p "$OUT/pkg" "$OUT/assets/icons" "$OUT/assets/fonts" \
  "$OUT/assets/models" "$OUT/assets/logo"

echo '-- wasm-bindgen'
wasm-bindgen --target web --out-name schist --out-dir "$OUT/pkg" \
  "target/$TARGET/$PROFILE/schist.wasm"

# wasm-opt takes ~40% off the module (45 -> 28 MB), but only a modern
# binaryen may touch it: rustc emits reference types by default and
# binaryen 108 (Ubuntu's packaged version) SILENTLY mangles the externref
# table's limits — the module then dies at init with "WebAssembly.
# Table.grow(): failed to grow table". Verified against version 123.
# A too-old binaryen is therefore skipped, never trusted.
WASM_OPT_MIN=116
WASM_OPT_VER=$(wasm-opt --version 2>/dev/null | sed -n 's/.*version \([0-9]*\).*/\1/p')
if [ -n "$WASM_OPT_VER" ] && [ "$WASM_OPT_VER" -ge "$WASM_OPT_MIN" ] \
  && [ "$PROFILE" = web ]; then
  echo "-- wasm-opt -Oz (binaryen $WASM_OPT_VER)"
  if wasm-opt -Oz \
    --enable-reference-types --enable-bulk-memory --enable-sign-ext \
    --enable-mutable-globals --enable-multivalue \
    --enable-nontrapping-float-to-int \
    -o "$OUT/pkg/schist_bg.wasm.opt" "$OUT/pkg/schist_bg.wasm"; then
    mv "$OUT/pkg/schist_bg.wasm.opt" "$OUT/pkg/schist_bg.wasm"
  else
    echo '   wasm-opt failed; shipping the unoptimized module' >&2
    rm -f "$OUT/pkg/schist_bg.wasm.opt"
  fi
elif [ "$PROFILE" = web ]; then
  echo "-- wasm-opt missing or older than $WASM_OPT_MIN; skipping (get binaryen" >&2
  echo '   from https://github.com/WebAssembly/binaryen/releases)' >&2
fi

echo "-- chunking ($CHUNK_MIB MiB)"
split -b "${CHUNK_MIB}m" -d -a 3 "$OUT/pkg/schist_bg.wasm" "$OUT/pkg/schist_bg.wasm."
rm "$OUT/pkg/schist_bg.wasm"
# TypeScript declarations aren't served.
rm -f "$OUT"/pkg/*.d.ts

echo '-- assets'
cp web/index.html web/loader.js "$OUT/"
cp crates/app/assets/icons/*.svg "$OUT/assets/icons/"
cp web/fonts/*.ttf web/fonts/LICENSE-* "$OUT/assets/fonts/"
cp crates/neural/models/*.onnx "$OUT/assets/models/"
cp assets/logo/schist.svg "$OUT/assets/logo/"

echo '-- manifest'
python3 - "$OUT" <<'EOF'
import json, os, sys
out = sys.argv[1]
def size(p): return os.path.getsize(os.path.join(out, p))
wasm = sorted(f for f in os.listdir(f"{out}/pkg") if ".wasm." in f)
manifest = {
    "js": "pkg/schist.js",
    "wasm": [{"file": f"pkg/{f}", "bytes": size(f"pkg/{f}")} for f in wasm],
    "fonts": [
        {"file": f"assets/fonts/{f}", "bytes": size(f"assets/fonts/{f}")}
        for f in sorted(os.listdir(f"{out}/assets/fonts"))
        if f.endswith((".ttf", ".otf"))
    ],
    # `path` is the name the app's AssetSource asks for; `file` is where
    # it is served from.
    "assets": [
        {
            "path": f"icons/{f}",
            "file": f"assets/icons/{f}",
            "bytes": size(f"assets/icons/{f}"),
        }
        for f in sorted(os.listdir(f"{out}/assets/icons"))
    ],
}
with open(f"{out}/manifest.json", "w") as fh:
    json.dump(manifest, fh, indent=1)
total = sum(c["bytes"] for c in manifest["wasm"])
print(f"   {len(manifest['wasm'])} wasm chunks, {total/2**20:.1f} MiB total")
EOF

echo "-- done: $(du -sh "$OUT" | cut -f1) in $OUT/"
echo "   serve with: python3 -m http.server -d $OUT 8000"
