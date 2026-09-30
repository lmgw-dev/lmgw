#!/usr/bin/env bash
# Render the tray PNGs from their SVG sources. Usage: bash gen_tray.sh
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")"

rsvg-convert -w 64 -h 64 tray-source.svg -o tray.png
rsvg-convert -w 64 -h 64 tray-hold-source.svg -o tray-hold.png

echo "wrote tray.png tray-hold.png"
