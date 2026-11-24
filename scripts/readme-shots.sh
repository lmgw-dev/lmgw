#!/usr/bin/env bash
# Regenerate the README screenshots: the app's window as a desktop screenshot of it shows it, docs/screenshots/*.png and docs/screenshots/thumbs/*.png.
#
# Usage: [LMGW_DEV_COPY_PREFIX=lmgw-dev] [README_CHAT_MODEL=qwen3.5-4b] scripts/readme-shots.sh [dir=target/dev-copy] [addr=127.0.0.1:8899] [which=all|pages|voice]
#
# Shoots a running dev copy, never production:
#   scripts/dev-copy.sh copy [dir]      a fresh copy of production's database
#   scripts/readme-demo.sh [dir]        scrubs it, starts it, adds demo chats and live traffic
#   scripts/readme-shots.sh [dir]       this; right away, while the demo's models are resident
# LMGW_DEV_COPY_PREFIX is the copy's container prefix (the one dev-copy.sh copy was given): the
# drives fail when a container with it starts or stops while they browse.
#
# Each image is the whole window the way Spectacle's "active window" capture of the app on KDE
# shows it: the window with its rounded corners and the window buttons in its header, the shell's
# frame shadow around it, on a transparent canvas (RGBA). The page is shot in headless Chrome at
# 1536x864 CSS px and device scale 1.25 (a 1920x1080 window at the default 125 % interface scale),
# in the framed window's state, focused, not maximized: scripts/drive/window-frame.js gives the
# shell's own page script (src-tauri/src/window_chrome/page.js) the answer the shell would, so the
# page rounds its corners and draws minimize, maximize and close, and the page is drawn on a
# transparent background (ui-drive.py --transparent). scripts/window-frame.py then adds the
# hairline and the shadow GTK draws around the window (src-tauri/src/window_chrome/linux.rs):
# 2090x1250 px, the window at 85,67. The dark theme and the bundled palette, as off KDE. Thumbs are
# 800 px wide, same aspect, alpha kept. Look at every image before committing it.
#
# README_TRAFFIC_MODEL (scripts/readme-demo.sh's, default the chat model) sets how many models the
# drives wait for (the pill's GPU · N): the chat model, the traffic model if another, the embedder.
#
# which picks the drives: pages (scripts/drive/readme-pages.json, the gallery), voice
# (scripts/drive/readme-voice.json, the Chat voice section) or both. The voice shots run in the
# newest demo thread, which needs speech models (Settings → Chat → Voice, or Settings → Realtime).
# Voice mode talks to an in-page mock session (scripts/realtime-mock.js) with a fake microphone, so
# no model runs and no real microphone opens, and Chrome is muted (--mute-audio): nothing is heard.
# Chrome runs with TZ=UTC, so the pages show times in UTC, not the machine's zone. The raw shots
# live in target/readme-shots while it runs and are removed when it ends, also when a drive fails.
set -euo pipefail
cd "$(dirname "$0")/.."

dir="$(realpath -m "${1:-target/dev-copy}")"
addr="${2:-127.0.0.1:8899}"
which="${3:-all}"
prefix="${LMGW_DEV_COPY_PREFIX:-lmgw-dev}"
chat_model="${README_CHAT_MODEL:-qwen3.5-4b}"
traffic_model="${README_TRAFFIC_MODEL:-$chat_model}"
# The GPU pill counts every running local model, the CPU embedder too: a shot waits until it
# counts the chat model (and the traffic model when it is another one) and the embedder, so a
# pill that shows only the embedder is not enough.
models_up=2
[ "$traffic_model" = "$chat_model" ] || models_up=3
work="target/readme-shots"
raw="$work/raw"

case "$which" in all|pages|voice) ;; *) echo "which must be all, pages or voice, not '$which'" >&2; exit 2 ;; esac
[ -f "$dir.token" ] || { echo "no $dir.token: start the dev copy first (scripts/dev-copy.sh start)" >&2; exit 1; }
# Only this run's shots are converted: an older run's raw files would overwrite newer images.
rm -rf "$work"
trap 'rm -rf "$work"' EXIT
mkdir -p "$raw" "$work/framed" docs/screenshots/thumbs

drive() {  # drive <steps.json> [ui-drive flags…]
    TZ=UTC python3 scripts/ui-drive.py "$1" --base "http://$addr" --token-file "$dir.token" \
        --size 1536x864 --dsf 1.25 --shots "$raw" --podman-guard "$prefix" --transparent \
        --init-script scripts/drive/window-frame.js --init-script src-tauri/src/window_chrome/page.js \
        --init-script scripts/drive/window-frame.js "${@:2}"
}
# The Models and API reference shots open the demo's chat model (scripts/readme-demo.sh's
# README_CHAT_MODEL): @CHAT_MODEL@ in the drive.
if [ "$which" != voice ]; then drive scripts/drive/readme-pages.json --var "CHAT_MODEL=$chat_model" --var "MODELS_UP=$models_up"; fi
if [ "$which" != pages ]; then drive scripts/drive/readme-voice.json --mute-audio --var "MODELS_UP=$models_up"; fi

for f in "$raw"/*.png; do
    n="$(basename "$f")"
    python3 scripts/window-frame.py frame "$f" "$work/framed/$n"
    magick "$work/framed/$n" -strip -define png:compression-level=9 "docs/screenshots/$n"
    magick "$work/framed/$n" -strip -filter Lanczos -resize 800x -define png:compression-level=9 "docs/screenshots/thumbs/$n"
done
echo "wrote $(ls "$raw" | wc -l) screenshots to docs/screenshots (review them before committing)"
