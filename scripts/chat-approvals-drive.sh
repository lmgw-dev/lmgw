#!/usr/bin/env bash
# Usage: scripts/chat-approvals-drive.sh [--shots DIR] [--dsf SCALE] [--drive FILE] [--addr HOST:PORT]
# Drives the Chat's approvals (client-apps design §6) end to end with no model: a dev instance on
# 127.0.0.1:8917 over a scratch data dir, scripts/mock-openai.py scripting the model's tool calls
# (TOOL_SCRIPT, one turn per send), scripts/mock-mcp.py registered as the stdio MCP server `notes`
# (prefix `notes`), then scripts/drive/chat-approvals.json through scripts/ui-drive.py.
# --shots keeps the drive's PNGs (default target/approvals-ui-shots); --dsf renders at that
# interface scale in a 1440x900 window (default 1.25, the scale the app runs at).
# Everything started is stopped on exit and the scratch dir (data, Chrome profiles) removed.
set -euo pipefail
cd "$(dirname "$0")/.."

SHOTS="target/approvals-ui-shots" DSF="1.25" DRIVE="scripts/drive/chat-approvals.json" ADDR="127.0.0.1:8917"
while [ $# -gt 0 ]; do
    case "$1" in
    --shots) SHOTS="$2"; shift ;;
    --dsf) DSF="$2"; shift ;;
    --drive) DRIVE="$2"; shift ;;
    --addr) ADDR="$2"; shift ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
    shift
done
BASE="http://$ADDR"
MOCK_PORT="$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1])')"

WORK="$(mktemp -d -p /tmp lmgw-approvals-XXXX)"
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

# One turn per send, in the order the drive sends: a lone gated call; a read beside a gated
# call; two gated calls; a gated call a new message then overtakes.
export TOOL_SCRIPT='[
 [{"name":"notes__delete_note","arguments":{"id":"7"}}],
 [{"name":"notes__list_notes","arguments":{}},{"name":"notes__delete_note","arguments":{"id":"8"}}],
 [{"name":"notes__delete_note","arguments":{"id":"1"}},{"name":"notes__delete_note","arguments":{"id":"2"}}],
 [{"name":"notes__delete_note","arguments":{"id":"9"}}]
]'
ECHO=1 STREAM_DELAY=0.02 start mock python3 scripts/mock-openai.py "$MOCK_PORT"
wait_http "http://127.0.0.1:$MOCK_PORT/v1/models"

CARGO_BUILD_JOBS=6 LMGW_DATA_DIR="$WORK/data" start gateway scripts/dev-instance.sh "$ADDR"
wait_http "$BASE/api/session"
TOKEN="$(sed 's/\x1b\[[0-9;]*m//g' "$WORK/gateway.log" | grep -o 'lmgw-owner-[0-9a-f]*' | head -1)"
[ -n "$TOKEN" ] || { echo "no owner token in the gateway log:"; tail -20 "$WORK/gateway.log"; exit 1; }

scripts/chat-actions-setup.sh "$TOKEN" "$BASE" "$MOCK_PORT" >/dev/null
curl -fsS -X POST "$BASE/api/op/mcp_server_set" -H "Authorization: Bearer $TOKEN" \
    -H 'Content-Type: application/json' \
    -d "{\"action\":\"create\",\"name\":\"notes\",\"transport\":\"stdio\",\"command\":\"python3\",\"args\":\"$PWD/scripts/mock-mcp.py\",\"tool_prefix\":\"notes\"}" >/dev/null

mkdir -p "$SHOTS"
SIZE="$(python3 -c 'import sys;s=float(sys.argv[1]);print(f"{round(1440/s)}x{round(900/s)}")' "$DSF")"
python3 scripts/ui-drive.py "$DRIVE" --token "$TOKEN" --base "$BASE" --shots "$SHOTS" --dsf "$DSF" --size "$SIZE"
