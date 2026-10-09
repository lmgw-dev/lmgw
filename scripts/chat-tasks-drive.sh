#!/usr/bin/env bash
# Usage: scripts/chat-tasks-drive.sh [--shots DIR] [--dsf SCALE] [--drive FILE] [--addr HOST:PORT]
#                                   [--webkit] [--device]
# Drives the Chat's MCP Tasks (MCP Tasks design §6) end to end with no model: a dev instance on
# 127.0.0.1:8918 over a scratch data dir, scripts/mock-openai.py scripting the model's tool calls
# (TOOL_SCRIPT, one turn per send), scripts/mock-mcp.py --tasks registered as the stdio MCP server
# `builder` (prefix `builder`: `build` completes, `deploy` fails, `index` runs until cancelled),
# then scripts/drive/chat-tasks.json through scripts/ui-drive.py: the jobs strip, a result card,
# Answer, a confirmed Cancel, a failed and a cancelled result, a refused Answer, Settings → MCP's
# poll interval and the MCP page's open-task count, then a light-theme pass at a 2 s poll: an armed Cancel survives a
# poll that changes the job's message. Last, the send race on a thread gating `deploy`: a build ends while a
# gated reply holds its result off, the next send lets the result in before its message, and the page puts
# the result in front of its own message in place (no reload; MCP Tasks design §3.1).
# --shots keeps the drive's PNGs (default target/review-shots/tasks-ui); --dsf renders at that
# interface scale in a 1440x900 window (default 1.25, the scale the app runs at).
# --webkit then loads /chat (the drive's thread: its cards and the job it left running) and
# /mcp-servers in WebKitGTK too (scripts/webkit-check.py on Broadway, no desktop window), at
# 1152x720 CSS px (that window at 125%), its PNGs in <shots>/webkit.
# --device hosts the same three tools on a paired device instead (scripts/mock-mcp.py --tasks
# --device on /mcp/host, a device key `builder-pc` hosting the label `builder`): the same drive,
# the jobs then followed over the host link (MCP Tasks WP5).
# Everything started is stopped on exit and the scratch dir (data, Chrome profiles) removed.
set -euo pipefail
cd "$(dirname "$0")/.."

SHOTS="target/review-shots/tasks-ui" DSF="1.25" DRIVE="scripts/drive/chat-tasks.json" ADDR="127.0.0.1:8918"
WEBKIT=false DEVICE=false
while [ $# -gt 0 ]; do
    case "$1" in
    --shots) SHOTS="$2"; shift ;;
    --dsf) DSF="$2"; shift ;;
    --drive) DRIVE="$2"; shift ;;
    --addr) ADDR="$2"; shift ;;
    --webkit) WEBKIT=true ;;
    --device) DEVICE=true ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
    shift
done
BASE="http://$ADDR"
MOCK_PORT="$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1])')"

WORK="$(mktemp -d -p /tmp lmgw-tasks-XXXX)"
export TMPDIR="$WORK" # Chrome profiles of ui-drive land here and go with it
PIDS=()
start() { local log="$1"; shift; setsid "$@" >"$WORK/$log.log" 2>&1 & PIDS+=("$!"); }
cleanup() {
    for p in "${PIDS[@]:-}"; do
        [ -n "$p" ] || continue
        kill -TERM -- "-$p" 2>/dev/null || true
    done
    sleep 1
    for p in "${PIDS[@]:-}"; do [ -n "$p" ] && kill -KILL -- "-$p" 2>/dev/null || true; done
    rm -rf "$WORK"
}
trap cleanup EXIT
trap 'exit 130' INT TERM
wait_http() { for _ in $(seq 1 1200); do curl -s -o /dev/null "$1" && return 0; sleep 0.5; done; echo "timed out: $1" >&2; return 1; }

# One turn per send, in the order the drive sends: a build that completes, a deploy that fails,
# an index the drive cancels, one it leaves running for the MCP page's count, then the send race's
# build and its gated deploy.
export TOOL_SCRIPT='[
 [{"name":"builder__build","arguments":{"seconds":8}}],
 [{"name":"builder__deploy","arguments":{"seconds":3}}],
 [{"name":"builder__index","arguments":{}}],
 [{"name":"builder__index","arguments":{}}],
 [{"name":"builder__build","arguments":{"seconds":8}}],
 [{"name":"builder__deploy","arguments":{"seconds":1}}]
]'
ECHO=1 STREAM_DELAY=0.02 start mock python3 scripts/mock-openai.py "$MOCK_PORT"
wait_http "http://127.0.0.1:$MOCK_PORT/v1/models"

CARGO_BUILD_JOBS=6 LMGW_DATA_DIR="$WORK/data" start gateway scripts/dev-instance.sh "$ADDR"
wait_http "$BASE/api/session"
TOKEN="$(sed 's/\x1b\[[0-9;]*m//g' "$WORK/gateway.log" | grep -o 'lmgw-owner-[0-9a-f]*' | head -1)"
[ -n "$TOKEN" ] || { echo "no owner token in the gateway log:"; tail -20 "$WORK/gateway.log"; exit 1; }

scripts/chat-actions-setup.sh "$TOKEN" "$BASE" "$MOCK_PORT" >/dev/null
if $DEVICE; then
    DEVICE_KEY="$(curl -fsS -X POST "$BASE/api/op/key_create" -H "Authorization: Bearer $TOKEN" \
        -H 'Content-Type: application/json' \
        -d '{"kind":"device","name":"builder-pc","hosts_label":"builder"}' |
        python3 -c 'import json,sys;print(json.load(sys.stdin)["key"])')"
    LMGW_DEVICE_KEY="$DEVICE_KEY" start device python3 scripts/mock-mcp.py --tasks --device "ws://$ADDR/mcp/host"
    for _ in $(seq 1 100); do grep -q 'mock-mcp: linked' "$WORK/device.log" && break; sleep 0.2; done
    grep -q 'mock-mcp: linked' "$WORK/device.log" || { echo "the device did not link:"; cat "$WORK/device.log"; exit 1; }
else
    curl -fsS -X POST "$BASE/api/op/mcp_server_set" -H "Authorization: Bearer $TOKEN" \
        -H 'Content-Type: application/json' \
        -d "{\"action\":\"create\",\"name\":\"builder\",\"transport\":\"stdio\",\"command\":\"python3\",\"args\":\"$PWD/scripts/mock-mcp.py\\n--tasks\",\"tool_prefix\":\"builder\"}" >/dev/null
fi

mkdir -p "$SHOTS"
SIZE="$(python3 -c 'import sys;s=float(sys.argv[1]);print(f"{round(1440/s)}x{round(900/s)}")' "$DSF")"
python3 scripts/ui-drive.py "$DRIVE" --token "$TOKEN" --base "$BASE" --shots "$SHOTS" --dsf "$DSF" --size "$SIZE"
if $WEBKIT; then
    python3 scripts/webkit-check.py --base "$BASE" --token "$TOKEN" --backend broadway \
        --routes /chat,/mcp-servers --sizes 1152x720 --shots "$SHOTS/webkit"
fi
