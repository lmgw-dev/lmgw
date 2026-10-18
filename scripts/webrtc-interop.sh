#!/usr/bin/env bash
# Usage: scripts/webrtc-interop.sh [speech.wav]
#
# The libwebrtc interop rig of the realtime calls design (§13.2): headless
# Chromium in the Playwright image, in a rootless Podman container, calls a
# scratch dev instance (scripts/dev-instance.sh, its own data dir, never the
# real one) through pasta's forwarding of the HTTP port (TCP) and the
# same-numbered UDP port from the container's loopback to the host's. The
# cascade is scripts/mock-openai.py (chat, speech to text, and text to speech
# answering with a committed LJSpeech fixture), so no model and no GPU is
# involved. Three pages (scripts/webrtc-interop-pages.js): OpenAI's documented
# browser example, the Agents SDK's WebRTC transport (@openai/agents-realtime,
# cached under target/webrtc-interop/npm), and the bound contract as the
# Android client speaks it against an ongoing folder (takeover=ask, playout
# reports from getStats, lmgw.response.stop with estimatedPlayoutTimestamp
# when Chromium gives one, parts both ways, a push-to-talk turn, the End). scripts/webrtc_interop.py
# configures the dev instance and judges the dev log's DEBUG lines against
# the pages: the playout reports against the formula and jitterBufferDelay,
# the capture offset against realtime.webrtc_commit_window_ms, the RTP before
# oai-events opens, a stop's heard against the tap.
#
# speech.wav is Chromium's fake microphone (default: the committed fixture
# en_two_sentences_pause.wav); it is copied into the work dir, never
# committed. The work dir target/webrtc-interop/ is mounted alone (:Z) and
# keeps the pages' result.json, the checker's report.json and the remote
# track's recordings (rec-<page>.webm, for listening); the keys' copies are
# removed at the end. The dev instance's data dir, its log and the owner
# bearer live beside it in target/webrtc-interop-run/, never mounted; the
# data dir is removed at the end, the logs stay. Chromium's profile stays
# inside the container.
#
# Env: WEBRTC_INTEROP_PORT (8897: the dev instance's HTTP port and its UDP
# port), WEBRTC_INTEROP_MOCK_PORT (8896), WEBRTC_INTEROP_IMAGE (the Playwright
# image, pinned by digest; npm runs in it too, with install scripts off),
# WEBRTC_INTEROP_AGENTS_VERSION (0.20.0), INTEROP_CONSOLE=1 (the
# page's console). Exit 0 when every check passed, 2 when one failed, 1 when
# the rig itself could not run. A known finding (tagged with its question of
# the calls design, Q5: estimatedPlayoutTimestamp absent on an audio-only
# call) passes while it holds and fails the day it changes.
set -euo pipefail
cd "$(dirname "$0")/.."

port="${WEBRTC_INTEROP_PORT:-8897}"
mock_port="${WEBRTC_INTEROP_MOCK_PORT:-8896}"
# Pinned by digest: the tag names the release, the digest what ran.
image="${WEBRTC_INTEROP_IMAGE:-mcr.microsoft.com/playwright:v1.61.1-noble@sha256:5b8f294aff9041b7191c34a4bab3ac270157a28774d4b0660e9743297b697e48}"
agents_version="${WEBRTC_INTEROP_AGENTS_VERSION:-0.20.0}"
speech="${1:-crates/lmgw-core/tests/fixtures/realtime/audio/en_two_sentences_pause.wav}"
base="http://127.0.0.1:$port"
work="target/webrtc-interop"
run="target/webrtc-interop-run"

command -v ss > /dev/null || { echo "ss (iproute) is missing: the rig cannot check its ports" >&2; exit 1; }
for p in "$port" "$mock_port"; do
    taken="$(ss -Hltun "( sport = :$p )")" || { echo "ss failed on port $p" >&2; exit 1; }
    if [ -n "$taken" ]; then
        echo "port $p is taken (TCP or UDP): set WEBRTC_INTEROP_PORT or WEBRTC_INTEROP_MOCK_PORT" >&2
        exit 1
    fi
done
[ -f "$speech" ] || { echo "no speech WAV at $speech" >&2; exit 1; }

mkdir -p "$work/npm" "$run"
data="$(mktemp -d "$run/data-XXXX")"
cp "$speech" "$work/speech.wav"
cp crates/lmgw-core/tests/fixtures/realtime/audio/en_complete_short.wav "$work/answer.wav"
rm -f "$work/result.json" "$work/report.json" "$work"/rec-*.webm "$work/pages.js" "$work/run.mjs"

# How long the dev instance's stop takes at most: server::STOP_WITHIN (10 s)
# for its sessions and turns, then lifecycle::SHUTDOWN_TIMEOUT (15 s) for its
# containers. A second signal exits it at once.
stop_within=25
dev_pid=""
mock_pid=""
cleanup() {
    if [ -n "$dev_pid" ] && kill -INT "$dev_pid" 2>/dev/null; then
        for _ in $(seq 1 "$stop_within"); do
            kill -0 "$dev_pid" 2>/dev/null || break
            sleep 1
        done
        if kill -0 "$dev_pid" 2>/dev/null; then
            echo "the dev instance did not stop within $stop_within s: TERM" >&2
            kill -TERM "$dev_pid" 2>/dev/null || true
            for _ in $(seq 1 10); do
                kill -0 "$dev_pid" 2>/dev/null || break
                sleep 0.5
            done
            kill -KILL "$dev_pid" 2>/dev/null || true
        fi
        wait "$dev_pid" 2>/dev/null || true
    fi
    if [ -n "$mock_pid" ]; then
        kill "$mock_pid" 2>/dev/null || true
        wait "$mock_pid" 2>/dev/null || true
    fi
    rm -f "$work/key" "$work/holder.key" "$run/owner.token"
    # Its data dir goes once it is gone, never under it.
    if [ -n "$dev_pid" ] && kill -0 "$dev_pid" 2>/dev/null; then
        echo "the dev instance $dev_pid is still running: its data dir $data stays" >&2
    else
        rm -rf "$data"
    fi
}
trap cleanup EXIT

echo "==> the dev instance (scripts/dev-instance.sh, data dir $data)"
cargo build -p lmgw-core -j 8 --example headless > "$run/build.log" 2>&1 \
    || { tail -20 "$run/build.log" >&2; exit 1; }
SPEECH_WAV="$work/answer.wav" TRANSCRIPT="What is the weather like today?" STREAM_DELAY=0.02 \
    STREAM_TOKENS=60 STREAM_CODE=0 python3 scripts/mock-openai.py "$mock_port" > "$run/mock.log" 2>&1 &
mock_pid=$!
# A dev instance never asks the desktop's Secret Service (dev-instance.sh).
LMGW_DATA_DIR="$(realpath "$data")" \
    scripts/dev-instance.sh "127.0.0.1:$port" > "$run/dev.log" 2>&1 &
dev_pid=$!
for _ in $(seq 1 120); do
    curl -sf -o /dev/null "$base/v1/models" && break
    kill -0 "$dev_pid" 2>/dev/null || { echo "the dev instance exited:" >&2; tail -20 "$run/dev.log" >&2; exit 1; }
    sleep 1
done
curl -sf -o /dev/null "$base/v1/models" || { echo "no answer from $base after 120 s" >&2; exit 1; }
(umask 077; grep -m1 -o 'token=[A-Za-z0-9_-]*' "$run/dev.log" | cut -d= -f2 > "$run/owner.token")
python3 scripts/webrtc_interop.py setup "$base" "$work" "http://127.0.0.1:$mock_port" "$run/owner.token"

sdk="$work/npm/node_modules/@openai/agents-realtime"
has_sdk() { grep -qs "\"version\": \"$agents_version\"" "$sdk/package.json"; }
if ! has_sdk; then
    echo "==> @openai/agents-realtime@$agents_version into $work/npm"
    # The npm dir alone is mounted (the keys beside it are not), and no
    # package's install script runs.
    podman run --rm -v "$(realpath "$work/npm"):/npm:Z" -w /npm "$image" sh -c \
        "npm init -y >/dev/null && npm install --ignore-scripts --no-audit --no-fund \
         --cache /npm/.cache @openai/agents-realtime@$agents_version >/dev/null" \
        || { echo "npm install of @openai/agents-realtime@$agents_version failed" >&2; exit 1; }
    has_sdk || { echo "no @openai/agents-realtime@$agents_version in $sdk after npm install" >&2; exit 1; }
fi
# The code that runs in the container, copied once nothing else writes there.
cp scripts/webrtc-interop-pages.js "$work/pages.js"
cp scripts/webrtc-interop.mjs "$work/run.mjs"

echo "==> Chromium calls $base (pasta: TCP and UDP port $port)"
status=0
podman run --rm --network="pasta:-T,$port,-U,$port" --shm-size=512m \
    -v "$(realpath "$work"):/probe:Z" -e INTEROP_CONSOLE="${INTEROP_CONSOLE:-}" \
    "$image" node /probe/run.mjs "$base" /probe || status=$?
if [ ! -f "$work/result.json" ]; then
    echo "the pages wrote no result (container exit $status)" >&2
    exit 1
fi
sleep 1
echo "==> checks"
status=0
python3 scripts/webrtc_interop.py check "$work" "$run/dev.log" || status=$?
echo "report: $work/report.json; recordings: $(ls "$work"/rec-*.webm 2>/dev/null | tr '\n' ' '); dev log: $run/dev.log"
exit "$status"
