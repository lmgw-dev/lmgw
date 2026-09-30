#!/usr/bin/env bash
# Regenerate the README screenshots: docs/screenshots/*.png (1920x1080, a 1080p screen at 125%) and docs/screenshots/thumbs/*.png (800x450).
#
# Usage: scripts/readme-shots.sh [dir=target/dev-copy] [addr=127.0.0.1:8899]
#
# Shoots a running dev copy, never production:
#   scripts/dev-copy.sh copy [dir]      a fresh copy of production's database
#   scripts/readme-demo.sh [dir]        scrubs it, starts it, adds demo chats and live traffic
#   scripts/readme-shots.sh [dir]       this; right away, while the demo's models are resident
# Every page gets the window chrome the app shows in its own window (html.tauri, the window
# controls), which a plain browser leaves hidden. Look at every image before committing it.
set -euo pipefail
cd "$(dirname "$0")/.."

dir="$(realpath -m "${1:-target/dev-copy}")"
addr="${2:-127.0.0.1:8899}"
raw="target/readme-shots/raw"

[ -f "$dir.token" ] || { echo "no $dir.token: start the dev copy first (scripts/dev-copy.sh start)" >&2; exit 1; }
mkdir -p "$raw" docs/screenshots/thumbs

python3 scripts/ui-drive.py scripts/drive/readme-pages.json --base "http://$addr" --token-file "$dir.token" \
    --size 1536x864 --dsf 1.25 --shots "$raw" --podman-guard ''

for f in "$raw"/*.png; do
    n="$(basename "$f")"
    magick "$f" -strip -define png:compression-level=9 "docs/screenshots/$n"
    magick "$f" -strip -filter Lanczos -resize 800x450 -define png:compression-level=9 "docs/screenshots/thumbs/$n"
done
echo "wrote $(ls "$raw" | wc -l) screenshots to docs/screenshots (review them before committing)"
