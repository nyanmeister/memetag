#!/usr/bin/env bash
set -euo pipefail
if [[ ${1:-} == --version ]]; then echo 'memetag-check-gui 0.3.1'; exit 0; fi
if [[ ${1:-} == --help ]]; then echo 'Usage: tools/check-gui.sh BIN_DIRECTORY OUTPUT_DIRECTORY'; exit 0; fi
bin=${1:?provide a directory containing memetag and memetag-gui}
out=${2:?provide an output directory}
repo=$(cd -- "$(dirname -- "$0")/.." && pwd)
mkdir -p "$out"
out=$(cd "$out" && pwd)
bin=$(cd "$bin" && pwd)
fixture=$(mktemp -d /tmp/memetag-gui-check.XXXXXXXX)
export XDG_CONFIG_HOME="$fixture/config" XDG_DATA_HOME="$fixture/data" XDG_CACHE_HOME="$fixture/cache" LIBGL_ALWAYS_SOFTWARE=1
unset MEMETAG_ROOT MEMETAG_DB MEMETAG_THUMBS MEMETAG_JOURNAL WAYLAND_DISPLAY
xvfb_pid='' app_pid=''
cleanup() {
    if [[ -n "$app_pid" ]]; then kill -KILL "$app_pid" 2>/dev/null || true; wait "$app_pid" 2>/dev/null || true; fi
    if [[ -n "$xvfb_pid" ]]; then kill "$xvfb_pid" 2>/dev/null || true; wait "$xvfb_pid" 2>/dev/null || true; fi
    rm -rf -- "$fixture"
}
trap cleanup EXIT
mkdir -p "$fixture/collection/Cats" "$fixture/collection/Dogs" "$fixture/outside"
cp "$repo/tests/fixtures/animated.png" "$fixture/collection/Cats/a.png"
cp "$repo/tests/fixtures/animated.webp" "$fixture/collection/Dogs/a.webp"
cp "$repo/tests/fixtures/animated.png" "$fixture/outside/a.png"
"$bin/memetag" init "$fixture/collection" > "$out/init.log"
"$bin/memetag" reindex >> "$out/init.log" 2>&1
: > "$out/display"
Xvfb -displayfd 3 -screen 0 1400x1000x24 -nolisten tcp -noreset -ac 3> "$out/display" > "$out/xvfb.log" 2>&1 &
xvfb_pid=$!
for ((i=0; i<100; i++)); do [[ -s "$out/display" ]] && break; sleep 0.1; done
[[ -s "$out/display" ]] || { echo 'Private display failed to start' >&2; exit 1; }
DISPLAY=":$(cat "$out/display")"
export DISPLAY
wait_window() {
    local i
    for ((i=0; i<150; i++)); do
        if xdotool search --all --onlyvisible --pid "$app_pid" --name "$1" > "$out/window" 2>/dev/null; then return; fi
        kill -0 "$app_pid" || { cat "$out/app.log" >&2; return 1; }
        sleep 0.1
    done
    return 1
}
"$bin/memetag" folders menu > "$out/app.log" 2>&1 &
app_pid=$!
wait_window '^memetag folders'
window=$(head -n1 "$out/window")
sleep 0.5
import -window root "$out/menu-before.png"
xdotool windowfocus "$window"
xdotool mousemove --window "$window" 14 212 click 1
xdotool mousemove --window "$window" 60 324 click 1 type --clearmodifiers --delay 2 Downloads
xdotool mousemove --window "$window" 220 324 click 1 type --clearmodifiers --delay 2 "$fixture/outside"
xdotool mousemove --window "$window" 460 324 click 1
sleep 0.3
import -window root "$out/menu-added.png"
xdotool mousemove --window "$window" 40 307 click 1
wait "$app_pid"
app_pid=''
"$bin/memetag" folders status > "$out/folders-after.txt"
rg -q 'exclude Cats' "$out/folders-after.txt"
"$bin/memetag" sources status > "$out/sources-after.txt"
rg -q 'Downloads.*enabled.*available' "$out/sources-after.txt"
"$bin/memetag" pull --force --no-thumbs > "$out/pull.log" 2>&1
"$bin/memetag" search 'source:Downloads' > "$out/search-source.txt"
[[ $(wc -l < "$out/search-source.txt") == 1 ]]
"$bin/memetag" grab </dev/null > "$out/browser.log" 2>&1 &
app_pid=$!
wait_window '^memetag'
window=$(head -n1 "$out/window")
xdotool windowsize "$window" 980 740
xdotool windowfocus "$window"
sleep 1
import -window root "$out/browser.png"
xdotool mousemove --window "$window" 30 54 click 1
sleep 0.3
xdotool mousemove --window "$window" 380 54 click 1
sleep 0.3
import -window root "$out/selection.png"
xdotool mousemove --window "$window" 201 54 click 1
sleep 0.5
import -window root "$out/batch-before.png"
xdotool mousemove --window "$window" 140 72 click 1 type --clearmodifiers --delay 5 multi-root-test
xdotool mousemove --window "$window" 400 72 click 1
sleep 0.2
import -window root "$out/batch-ready.png"
xdotool key --clearmodifiers ctrl+s
for ((i=0; i<50; i++)); do
    "$bin/memetag" search multi-root-test > "$out/batch-search.txt" 2>/dev/null
    [[ $(wc -l < "$out/batch-search.txt") == 2 ]] && break
    sleep 0.1
done
[[ $(wc -l < "$out/batch-search.txt") == 2 ]]
"$bin/memetag" read "$fixture/outside/a.png" > "$out/outside-tags.txt"
"$bin/memetag" read "$fixture/collection/Dogs/a.webp" > "$out/main-tags.txt"
rg -q multi-root-test "$out/outside-tags.txt"
rg -q multi-root-test "$out/main-tags.txt"
import -window root "$out/batch-finished.png"
xdotool mousemove --window "$window" 210 18 click 1
sleep 0.2
xdotool mousemove --window "$window" 947 11 click 1
sleep 0.3
import -window root "$out/browser-menu.png"
echo 'PASS: private source add/save, exclusion, scoped search, batch edit across sources, embedded menu'
