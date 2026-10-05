#!/usr/bin/env bash
# Usage: scripts/media-probe-chrome.sh [base-url] [token-file] [out.json]
#        (defaults: http://127.0.0.1:8899, target/dev-copy.token, target/media-probe/result.json)
#
# The page-audio probe of chat-voice WP6 (scripts/media-probe.js) in headless
# Chromium, inside the Playwright image, against a running dev gateway (a dev
# copy: scripts/dev-copy.sh start). Chromium captures a TTS-generated test
# fixture as its fake microphone and is muted; the container has no audio
# server, so nothing can play audibly and no real device is touched. The
# work dir (probe, runner, WAV, a copy of the token) is target/media-probe/,
# mounted alone; the token copy is removed at the end. A screenshot of the
# popover lands beside the result. PROBE_CONSOLE=1 prints the page's console.
# Exit 0 when every check passed, 2 when one failed.
set -euo pipefail
cd "$(dirname "$0")/.."

base="${1:-http://127.0.0.1:8899}"
token_file="${2:-target/dev-copy.token}"
out="${3:-target/media-probe/result.json}"
image="${MEDIA_PROBE_IMAGE:-mcr.microsoft.com/playwright:v1.61.1-noble}"
port="${base##*:}"
port="${port%%/*}"

work="target/media-probe"
mkdir -p "$work"
cp scripts/media-probe.js "$work/media-probe.js"
cp scripts/media-probe-chrome.mjs "$work/run.mjs"
cp crates/lmgw-core/tests/fixtures/realtime/audio/en_two_sentences_pause.wav "$work/speech.wav"
install -m 600 "$token_file" "$work/token"
trap 'rm -f "$work/token"' EXIT

status=0
podman run --rm --network="pasta:-T,$port" --shm-size=512m \
    -v "$(realpath "$work"):/probe:Z" \
    -e PROBE_CONSOLE="${PROBE_CONSOLE:-}" "$image" node /probe/run.mjs "$base" /probe || status=$?
if [ "$(realpath -m "$out")" != "$(realpath -m "$work/result.json")" ] && [ -f "$work/result.json" ]; then
    mkdir -p "$(dirname "$out")"
    cp "$work/result.json" "$out"
    [ -f "$work/chrome-popover.png" ] && cp "$work/chrome-popover.png" "$(dirname "$out")/"
fi
exit "$status"
