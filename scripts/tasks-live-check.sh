#!/usr/bin/env bash
# Usage: scripts/tasks-live-check.sh [--ui] [--addr HOST:PORT] [--out FILE]
# The live checks 1-4 of MCP Tasks (MCP Tasks design §7) with no model and no GPU: a dev
# instance (scripts/dev-instance.sh, scratch data dir, 127.0.0.1:8917 by default),
# scripts/mock-openai.py as the chat model (TOOL_SCRIPT: one job-starting turn per check's send)
# and as the voice (`/audio/speech`), and scripts/mock-mcp.py --tasks --device as the paired
# device `builder-pc` hosting the `builder` tools over /mcp/host (a slow `required` build, an
# index to cancel; SIGUSR1 drops its link). scripts/tasks-live-check.py then runs the checks
# through the dashboard's and a voice client's routes and writes the facts, timings included, to
# --out (default target/tasks-live-check.json).
# --ui then runs scripts/chat-tasks-drive.sh --device: the dashboard itself (result card, jobs
# strip, Answer, a confirmed Cancel) against the same kind of device, on its own instance.
# Everything started is stopped on exit and the scratch dir removed.
set -euo pipefail
cd "$(dirname "$0")/.."

ADDR="127.0.0.1:8917" OUT="target/tasks-live-check.json" UI=false
while [ $# -gt 0 ]; do
    case "$1" in
    --ui) UI=true ;;
    --addr) ADDR="$2"; shift ;;
    --out) OUT="$2"; shift ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
    shift
done
BASE="http://$ADDR"
MOCK_PORT="$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1])')"
DROP_SECONDS=10

WORK="$(mktemp -d -p /tmp lmgw-tasks-live-XXXX)"
PIDS=()
start() { local log="$1"; shift; setsid "$@" >"$WORK/$log.log" 2>&1 & PIDS+=("$!"); LAST_PID=$!; }
cleanup() {
    for p in "${PIDS[@]:-}"; do [ -n "$p" ] && kill -TERM -- "-$p" 2>/dev/null || true; done
    sleep 1
    for p in "${PIDS[@]:-}"; do [ -n "$p" ] && kill -KILL -- "-$p" 2>/dev/null || true; done
    rm -rf "$WORK"
}
trap cleanup EXIT
trap 'exit 130' INT TERM
wait_http() { for _ in $(seq 1 1200); do curl -s -o /dev/null "$1" && return 0; sleep 0.5; done; echo "timed out: $1" >&2; return 1; }

# One turn per job-starting send, in the checks' order: check 1's two builds (6 s, 4 s),
# check 2's (5 s), check 3's (6 s, ending while the link is down), check 4's index.
export TOOL_SCRIPT='[
 [{"name":"builder__build","arguments":{"seconds":6}}],
 [{"name":"builder__build","arguments":{"seconds":4}}],
 [{"name":"builder__build","arguments":{"seconds":5}}],
 [{"name":"builder__build","arguments":{"seconds":6}}],
 [{"name":"builder__index","arguments":{}}]
]'
ECHO=1 STREAM_DELAY=0.01 start mock python3 scripts/mock-openai.py "$MOCK_PORT"
wait_http "http://127.0.0.1:$MOCK_PORT/v1/models"

CARGO_BUILD_JOBS=8 LMGW_DATA_DIR="$WORK/data" start gateway scripts/dev-instance.sh "$ADDR"
wait_http "$BASE/api/session"
TOKEN="$(sed 's/\x1b\[[0-9;]*m//g' "$WORK/gateway.log" | grep -o 'lmgw-owner-[0-9a-f]*' | head -1)"
[ -n "$TOKEN" ] || { echo "no owner token in the gateway log:"; tail -20 "$WORK/gateway.log"; exit 1; }

op() { curl -fsS -X POST "$BASE/api/op/$1" -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' -d "$2"; }
scripts/chat-actions-setup.sh "$TOKEN" "$BASE" "$MOCK_PORT" >/dev/null
# The voice: an OpenAI-shaped TTS on the mock (silence, which is still audio to send).
op upstream_set "{\"action\":\"create\",\"name\":\"fake-voice\",\"protocol\":\"openai\",\"kind\":\"generic\",\"base_url\":\"http://127.0.0.1:$MOCK_PORT/v1\"}" >/dev/null
op model_set '{"action":"create","alias":"fake-tts","upstream":"fake-voice","upstream_model":"tts-1","capabilities_override":{"capabilities":{"task":"tts","endpoints":["/v1/audio/speech"],"source":"owner"}}}' >/dev/null
op settings_set '{"chat_tts_alias":"fake-tts","chat_voice":"alloy"}' >/dev/null
# The device: a device key hosting the `builder` label, and the mock on its link.
DEVICE_KEY="$(op key_create '{"kind":"device","name":"builder-pc","hosts_label":"builder"}' | python3 -c 'import json,sys;print(json.load(sys.stdin)["key"])')"
LMGW_DEVICE_KEY="$DEVICE_KEY" DROP_SECONDS="$DROP_SECONDS" \
    start device python3 scripts/mock-mcp.py --tasks --device "ws://$ADDR/mcp/host"
DEVICE_PID=$LAST_PID
for _ in $(seq 1 100); do grep -q 'mock-mcp: linked' "$WORK/device.log" && break; sleep 0.2; done
grep -q 'mock-mcp: linked' "$WORK/device.log" || { echo "the device did not link:"; cat "$WORK/device.log"; exit 1; }

mkdir -p "$(dirname "$OUT")"
STATUS=0
python3 scripts/tasks-live-check.py "$BASE" "$TOKEN" "$DEVICE_PID" "$DROP_SECONDS" "$OUT" || STATUS=$?
if [ "$STATUS" != 0 ]; then
    mkdir -p target/drive-logs
    cp "$WORK/gateway.log" target/drive-logs/tasks-live-gateway.log
    cp "$WORK/device.log" target/drive-logs/tasks-live-device.log
    echo "logs: target/drive-logs/tasks-live-{gateway,device}.log"
fi
if $UI; then
    scripts/chat-tasks-drive.sh --device --shots target/review-shots/tasks-live-ui || STATUS=$?
fi
exit "$STATUS"
