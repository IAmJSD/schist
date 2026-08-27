#!/usr/bin/env bash
# Verify the .8bf host against real, third-party Photoshop plug-ins.
#
# Nothing here is vendored: the two plug-ins are downloaded fresh, used
# as black boxes, and thrown away. Only their shipped binaries are
# touched — neither project's source is read, which is what keeps
# crates/plugin-host-8bf clean room. See docs/8bf-abi-provenance.md.
#
# Needs: wine, mingw-w64, the x86_64-pc-windows-gnu Rust target, and —
# for the dialog test — Xvfb and xdotool.
set -uo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
work=${SCHIST_8BF_WORK:-/tmp/schist-8bf-verify}
export WINEPREFIX=${WINEPREFIX:-$HOME/.wine-8bf}
export WINEDEBUG=-all
fail=0

need() { command -v "$1" >/dev/null || { echo "skip: $1 not installed"; exit 0; }; }
need wine
need x86_64-w64-mingw32-gcc

mkdir -p "$work" && cd "$work"

echo "== fetching plug-ins =="
ff_url=https://github.com/danielmarschall/filter_foundry/releases/download/1.7.0.25/FilterFoundry.1.7.0.25.SVN.Rev.624.zip
gm_url=https://github.com/0xC0000054/gmic-8bf/releases/download/v4.0.4/GmicPlugin_x64.zip
[ -f ff.zip ] || curl -sSL --max-time 300 -o ff.zip "$ff_url" || { echo "skip: no network"; exit 0; }
[ -f gm.zip ] || curl -sSL --max-time 600 -o gm.zip "$gm_url" || { echo "skip: no network"; exit 0; }
python3 - <<'PY'
import zipfile
zf = zipfile.ZipFile('ff.zip')
for n in zf.namelist():
    if n.endswith('FilterFoundry64.8bf'):
        open('FilterFoundry64.8bf', 'wb').write(zf.read(n))
# G'MIC needs the companion tree beside it or it stops at Start with its
# own "resources missing" code, so extract the lot.
zipfile.ZipFile('gm.zip').extractall('.')
PY

echo "== building the host for Windows =="
( cd "$root" && cargo build -q -p schist-plugin-host-8bf --example 8bf \
    --target x86_64-pc-windows-gnu ) || exit 1
exe=$root/target/x86_64-pc-windows-gnu/debug/examples/8bf.exe
winpath() { printf 'Z:%s' "${1//\//\\}"; }

# A gradient, so a wrong stride shows up as garbage rather than as a
# plausible flat colour.
python3 -c "
w,h=64,48
d=bytearray()
for y in range(h):
    for x in range(w):
        d += bytes([x*4%256, y*5%256, (x+y)*3%256])
open('in.ppm','wb').write(b'P6\n%d %d\n255\n'%(w,h)+bytes(d))
"

echo
echo "== discovery (no code runs) =="
for p in FilterFoundry64.8bf GmicPlugin.8bf; do
  "$root"/target/debug/examples/8bf inspect "$work/$p" || fail=1
done

echo
echo "== G'MIC: selector sequence, handle suite, advanceState =="
# What matters is that it gets through Prepare, drives the handle suite
# and advanceState, and then stops cleanly. It cannot finish headless —
# it wants its Qt UI — so any *reported* error is fine and a page fault
# is not.
SCHIST_8BF_TRACE=1 timeout 60 wine "$exe" apply "$(winpath "$work/GmicPlugin.8bf")" \
  "$(winpath "$work/in.ppm")" "$(winpath "$work/gmic-out.ppm")" --no-dialog \
  >gmic.log 2>&1
if grep -q 'page fault' gmic.log; then
  echo "FAIL: G'MIC faulted"; grep -o 'page fault.*' gmic.log | head -1; fail=1
elif ! grep -q '<- selector 2 = 0' gmic.log; then
  echo "FAIL: did not get through Prepare"
  grep -v '^0[0-9a-f]\{3\}:' gmic.log | tail -3; fail=1
elif ! grep -q 'handle.new' gmic.log; then
  echo "FAIL: never used the handle suite"; fail=1
elif ! grep -q 'advanceState' gmic.log; then
  echo "FAIL: never called advanceState"; fail=1
else
  echo "ok: through Prepare, used the handle suite and advanceState, then stopped cleanly"
fi

if ! command -v Xvfb >/dev/null || ! command -v xdotool >/dev/null; then
  echo; echo "skip: Xvfb/xdotool missing, not running the dialog test"
  exit $fail
fi

echo
echo "== Filter Foundry: dialog, formula, pixels =="
Xvfb :99 -screen 0 1280x1024x24 >/dev/null 2>&1 &
xvfb=$!
trap 'kill $xvfb 2>/dev/null' EXIT
sleep 3
export DISPLAY=:99
rm -f ff-out.ppm
( timeout 150 wine "$exe" apply "$(winpath "$work/FilterFoundry64.8bf")" \
    "$(winpath "$work/in.ppm")" "$(winpath "$work/ff-out.ppm")" >ff.log 2>&1 & )
sleep 22
if ! xdotool search --name "Filter Foundry" >/dev/null 2>&1; then
  echo "FAIL: the plug-in never opened its dialog"; kill $xvfb; exit 1
fi
echo "ok: dialog is up; typing 255-r into R, G and B"
for y in 537 583 628; do
  xdotool mousemove 650 "$y" click 1; sleep 0.3
  xdotool key --clearmodifiers ctrl+a; sleep 0.2
  xdotool type --delay 35 "255-r"; sleep 0.3
done
xdotool mousemove 826 722 click 1   # OK
sleep 18

python3 - <<'PY' || fail=1
import sys
try:
    a = open('in.ppm', 'rb').read(); b = open('ff-out.ppm', 'rb').read()
except FileNotFoundError:
    print('FAIL: no output written'); sys.exit(1)
pa = a[a.index(b'255\n') + 4:]; pb = b[b.index(b'255\n') + 4:]
ok = len(pa) == len(pb) and all(255 - pa[i // 3 * 3] == pb[i] for i in range(len(pa)))
print('ok: every channel is 255 - input red' if ok else 'FAIL: pixels are wrong')
sys.exit(0 if ok else 1)
PY

exit $fail
