#!/usr/bin/env bash
# Verify the .8bf host against real, third-party Photoshop plug-ins.
#
# Nothing here is vendored: the two plug-ins are downloaded fresh, used
# as black boxes, and thrown away. Only their shipped binaries are
# touched — neither project's source is read, which is what keeps
# crates/plugin-host-8bf clean room. See docs/8bf-abi-provenance.md.
#
# Needs: wine, the x86_64-pc-windows-gnu Rust target, and — for the
# dialog tests — Xvfb and xdotool. The 32-bit checks additionally need
# wine32 and the i686-pc-windows-gnu target, and are skipped without
# them. Everything it skips, it says so.
set -uo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
work=${SCHIST_8BF_WORK:-/tmp/schist-8bf-verify}
export WINEPREFIX=${WINEPREFIX:-$HOME/.wine-8bf}
export WINEDEBUG=-all
export SCHIST_8BF_TRACE=1
fail=0

need() { command -v "$1" >/dev/null || { echo "skip: $1 not installed"; exit 0; }; }
need wine
mkdir -p "$work" && cd "$work"

echo "== fetching plug-ins =="
ff_url=https://github.com/danielmarschall/filter_foundry/releases/download/1.7.0.25/FilterFoundry.1.7.0.25.SVN.Rev.624.zip
gm_url=https://github.com/0xC0000054/gmic-8bf/releases/download/v4.0.4/GmicPlugin_x64.zip
[ -f ff.zip ] || curl -sSL --max-time 300 -o ff.zip "$ff_url" || { echo "skip: no network"; exit 0; }
[ -f gm.zip ] || curl -sSL --max-time 600 -o gm.zip "$gm_url" || { echo "skip: no network"; exit 0; }
python3 "$root/tools/verify-8bf-support.py" extract || exit 1

echo "== building the host for Windows =="
( cd "$root" && cargo build -q -p schist-plugin-host-8bf --example 8bf \
    --target x86_64-pc-windows-gnu ) || exit 1
exe=$root/target/x86_64-pc-windows-gnu/debug/examples/8bf.exe
exe32=$root/target/i686-pc-windows-gnu/debug/examples/8bf.exe
if ( cd "$root" && cargo build -q -p schist-plugin-host-8bf --example 8bf \
       --target i686-pc-windows-gnu 2>/dev/null ); then
  have32=1
else
  echo "note: no i686-pc-windows-gnu target, skipping the 32-bit checks"
  have32=0
fi

winpath() { printf 'Z:%s' "${1//\//\\}"; }
python3 "$root/tools/verify-8bf-support.py" gradient

echo
echo "== discovery (no code runs) =="
for p in FilterFoundry64.8bf GmicPlugin.8bf; do
  "$root"/target/debug/examples/8bf inspect "$work/$p" || fail=1
done

echo
echo "== G'MIC: selector sequence, handle suite, advanceState =="
# What matters is that it gets through Prepare, drives the handle suite
# and advanceState, and then stops cleanly. It cannot finish headless —
# it wants its Qt UI — so any *reported* error is fine and a fault is not.
timeout 60 wine "$exe" apply "$(winpath "$work/GmicPlugin.8bf")" \
  "$(winpath "$work/in.ppm")" "$(winpath "$work/gmic-out.ppm")" --no-dialog \
  >gmic.log 2>&1
if grep -q 'page fault' gmic.log; then
  echo "FAIL: G'MIC faulted"; grep -o 'page fault.*' gmic.log | head -1; fail=1
elif ! grep -q '<- selector 2 = 0' gmic.log; then
  echo "FAIL: did not get through Prepare"; tail -3 gmic.log; fail=1
elif ! grep -q 'handle.new' gmic.log; then
  echo "FAIL: never used the handle suite"; fail=1
elif ! grep -q 'advanceState' gmic.log; then
  echo "FAIL: never called advanceState"; fail=1
else
  echo "ok: through Prepare, used the handle suite and advanceState, then stopped cleanly"
fi

echo
echo "== about box =="
( timeout 30 wine "$exe" about "$(winpath "$work/FilterFoundry64.8bf")" >about.log 2>&1 & )
sleep 14
pkill -f '8bf.exe' 2>/dev/null
if grep -q 'page fault' about.log; then
  echo "FAIL: the about selector faulted"; fail=1
else
  echo "ok: AboutRecord accepted, no fault"
fi

if ! command -v Xvfb >/dev/null || ! command -v xdotool >/dev/null; then
  echo; echo "skip: Xvfb/xdotool missing, not running the dialog tests"
  exit $fail
fi

Xvfb :99 -screen 0 1400x1050x24 >/dev/null 2>&1 &
xvfb=$!
trap 'kill $xvfb 2>/dev/null' EXIT
sleep 3
export DISPLAY=:99

# Click at a position relative to a window's top-left corner, so the
# script does not depend on where the window manager put it or on the
# screen size.
click_in() {  # $1 = window id, $2 = dx, $3 = dy
  local geom x y
  geom=$(xdotool getwindowgeometry --shell "$1")
  x=$(echo "$geom" | sed -n 's/^X=//p')
  y=$(echo "$geom" | sed -n 's/^Y=//p')
  xdotool mousemove $((x + $2)) $((y + $3)) click 1
}

# Type 255-r into the R, G and B formula fields, then click OK.
drive_filter_foundry() {
  local wid
  wid=$(xdotool search --name "Filter Foundry" 2>/dev/null | head -1)
  if [ -z "$wid" ]; then
    echo "FAIL: the plug-in never opened its dialog"; return 1
  fi
  echo "ok: dialog is up; typing 255-r into R, G and B"
  local dy
  for dy in 237 283 328; do
    click_in "$wid" 244 "$dy"; sleep 0.3
    xdotool key --clearmodifiers ctrl+a; sleep 0.2
    xdotool type --delay 35 "255-r"; sleep 0.3
  done
  click_in "$wid" 420 422    # OK
}

# Search for a filter, select it, and apply. G'MIC is the only plug-in
# here that reaches for the buffer suite, which is the one thing a purely
# headless run never covers.
drive_gmic() {
  local wid
  wid=$(xdotool search --name "G.MIC-Qt for" 2>/dev/null | head -1)
  if [ -z "$wid" ]; then
    echo "FAIL: G'MIC never opened its UI"; return 1
  fi
  echo "ok: G'MIC UI is up; selecting a filter"
  click_in "$wid" 451 29; sleep 0.5      # search box
  xdotool key --clearmodifiers ctrl+a; sleep 0.3
  xdotool type --delay 60 "invert"; sleep 5
  click_in "$wid" 477 117; sleep 8       # the single result
  click_in "$wid" 874 672                # OK
}

echo
echo "== Filter Foundry, 64-bit: dialog, formula, pixels =="
rm -f ff-out.ppm
( timeout 150 wine "$exe" apply "$(winpath "$work/FilterFoundry64.8bf")" \
    "$(winpath "$work/in.ppm")" "$(winpath "$work/ff-out.ppm")" >ff.log 2>&1 & )
sleep 22
drive_filter_foundry || fail=1
sleep 18
python3 "$root/tools/verify-8bf-support.py" check ff-out.ppm 64-bit || fail=1

# Serving the PICA handle suite is what makes the plug-in call
# ReleaseSuite, which is the only evidence for that slot of SPBasicSuite.
if grep -q 'served the handle suite' ff.log && grep -q 'pica.release_suite' ff.log; then
  echo "ok: PICA handle suite acquired, used and released"
else
  echo "FAIL: the PICA handle suite was not exercised"; fail=1
fi

if [ "$have32" = 1 ]; then
  echo
  echo "== Filter Foundry, 32-bit host and 32-bit plug-in =="
  rm -f ff32-out.ppm
  ( timeout 150 wine "$exe32" apply "$(winpath "$work/FilterFoundry.8bf")" \
      "$(winpath "$work/in.ppm")" "$(winpath "$work/ff32-out.ppm")" >ff32.log 2>&1 & )
  sleep 24
  drive_filter_foundry || fail=1
  sleep 18
  python3 "$root/tools/verify-8bf-support.py" check ff32-out.ppm 32-bit || fail=1
fi

echo
echo "== G'MIC: full round trip through the buffer suite =="
python3 "$root/tools/verify-8bf-support.py" halves
rm -f gmic-halves-out.ppm
( timeout 240 wine "$exe" apply "$(winpath "$work/GmicPlugin.8bf")" \
    "$(winpath "$work/halves.ppm")" "$(winpath "$work/gmic-halves-out.ppm")" \
    >gmic-ui.log 2>&1 & )
sleep 50
drive_gmic || fail=1
sleep 30
python3 "$root/tools/verify-8bf-support.py" check-halves gmic-halves-out.ppm gmic || fail=1
for want in buffer.allocate buffer.lock buffer.unlock buffer.free; do
  grep -q "$want" gmic-ui.log || { echo "FAIL: G'MIC never called $want"; fail=1; }
done
grep -q 'buffer.allocate(0)' gmic-ui.log && {
  echo "FAIL: allocate was called with size 0 — the suite order is wrong again"; fail=1; }

exit $fail
