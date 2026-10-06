#!/usr/bin/env bash
# Screenshot one window by its process id on KDE Wayland: a KWin script makes it the active window,
# then spectacle captures the active window.
#
# Usage: scripts/window-shot.sh <pid> <out.png>
#
# Prints the image's width and height. Check them before looking at the image: if KWin's focus-stealing
# prevention kept another window active, the capture is that window, and that can be anything on the
# desktop. A server-side (KWin) frame adds its shadow, 84 px to each dimension; GTK's own decorations add 90.
set -euo pipefail

pid="$1"
out="$2"
name="window-shot-$pid-$$"
js="$(mktemp --suffix=.js)"
trap 'qdbus-qt6 org.kde.KWin /Scripting org.kde.kwin.Scripting.unloadScript "$name" >/dev/null 2>&1 || true; rm -f "$js"' EXIT

cat >"$js" <<EOF
for (const w of workspace.windowList()) { if (w.pid === $pid) workspace.activeWindow = w; }
EOF
id="$(qdbus-qt6 org.kde.KWin /Scripting org.kde.kwin.Scripting.loadScript "$js" "$name")"
qdbus-qt6 org.kde.KWin "/Scripting/Script$id" org.kde.kwin.Script.run >/dev/null
sleep 0.6

rm -f "$out"
spectacle -b -n -a -o "$out"
# spectacle -b returns before the file is written.
for _ in $(seq 100); do [ -s "$out" ] && break; sleep 0.1; done
python3 -c 'import sys; from PIL import Image; print(*Image.open(sys.argv[1]).size)' "$out"
