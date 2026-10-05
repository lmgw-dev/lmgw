#!/usr/bin/env bash
# Regenerate the README screenshots: docs/screenshots/*.png (1920x1080, a 1080p screen at 125%) and docs/screenshots/thumbs/*.png (800x450).
#
# Usage: scripts/readme-shots.sh [dir=target/dev-copy] [addr=127.0.0.1:8899] [which=all|pages|voice]
#
# Shoots a running dev copy, never production:
#   scripts/dev-copy.sh copy [dir]      a fresh copy of production's database
#   scripts/readme-demo.sh [dir]        scrubs it, starts it, adds demo chats and live traffic
#   scripts/readme-shots.sh [dir]       this; right away, while the demo's models are resident
# Every page gets the window chrome the app shows in its own window (html.tauri, the window
# controls), which a plain browser leaves hidden. Look at every image before committing it.
#
# which picks the drives: pages (scripts/drive/readme-pages.json, the gallery), voice
# (scripts/drive/readme-voice.json, the Chat voice section) or both. The voice shots run in the
# newest demo thread, which needs speech models (Settings → Chat → Voice, or Settings → Realtime).
# Voice mode talks to an in-page mock session (scripts/realtime-mock.js) with a fake microphone, so
# no model runs and no real microphone opens, and Chrome is muted (--mute-audio): nothing is heard.
set -euo pipefail
cd "$(dirname "$0")/.."

dir="$(realpath -m "${1:-target/dev-copy}")"
addr="${2:-127.0.0.1:8899}"
which="${3:-all}"
raw="target/readme-shots/raw"

case "$which" in all|pages|voice) ;; *) echo "which must be all, pages or voice, not '$which'" >&2; exit 2 ;; esac
[ -f "$dir.token" ] || { echo "no $dir.token: start the dev copy first (scripts/dev-copy.sh start)" >&2; exit 1; }
# Only this run's shots are converted: an older run's raw files would overwrite newer images.
rm -rf "$raw"
mkdir -p "$raw" docs/screenshots/thumbs

drive() {  # drive <steps.json> [ui-drive flags…]
    python3 scripts/ui-drive.py "$1" --base "http://$addr" --token-file "$dir.token" \
        --size 1536x864 --dsf 1.25 --shots "$raw" --podman-guard '' "${@:2}"
}
if [ "$which" != voice ]; then drive scripts/drive/readme-pages.json; fi
if [ "$which" != pages ]; then drive scripts/drive/readme-voice.json --mute-audio; fi

for f in "$raw"/*.png; do
    n="$(basename "$f")"
    magick "$f" -strip -define png:compression-level=9 "docs/screenshots/$n"
    magick "$f" -strip -filter Lanczos -resize 800x450 -define png:compression-level=9 "docs/screenshots/thumbs/$n"
done
echo "wrote $(ls "$raw" | wc -l) screenshots to docs/screenshots (review them before committing)"
