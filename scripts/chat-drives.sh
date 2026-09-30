#!/usr/bin/env bash
# Usage: scripts/chat-drives.sh [--matrix] [--webkit] [--only NAME[,NAME…]] [--addr HOST:PORT] [--mock-port N]
# Runs the 13 chat-branch UI drives in sequence on ONE fresh dev instance (scratch data dir,
# mock upstream, leak listener), prints a PASS/FAIL table, exits non-zero on any FAIL. Everything
# it starts is stopped by pid/process group on exit and its scratch dir (data, Chrome profiles) removed.
# --matrix runs `ui-matrix.py --routes all`, --webkit runs webkit-check.py on the chat routes, after the drives.
set -euo pipefail
cd "$(dirname "$0")/.."

MATRIX=0 WEBKIT=0 ONLY="" ADDR="" MOCK_PORT=""
while [ $# -gt 0 ]; do
    case "$1" in
    --matrix) MATRIX=1 ;;
    --webkit) WEBKIT=1 ;;
    --only) ONLY="$2"; shift ;;
    --addr) ADDR="$2"; shift ;;
    --mock-port) MOCK_PORT="$2"; shift ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
    shift
done

free_port() { python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1])'; }
[ -n "$ADDR" ] || ADDR="127.0.0.1:$(free_port)"
[ -n "$MOCK_PORT" ] || MOCK_PORT="$(free_port)"
LEAK_PORT=9299 # hard-wired into chat-drives' chat-security.json
BASE="http://$ADDR"

WORK="$(mktemp -d -p /tmp lmgw-drives-XXXX)"
export TMPDIR="$WORK" # Chrome profiles of ui-drive/ui-matrix land here and go with it
PIDS=()
start() { # start LOGNAME CMD… : own session so the whole group can be stopped by its pid
    local log="$1"; shift
    setsid "$@" >"$WORK/$log.log" 2>&1 &
    PIDS+=("$!")
    LAST_PID=$!
}
stop_pid() {
    local p="$1"
    kill -TERM -- "-$p" 2>/dev/null || kill -TERM "$p" 2>/dev/null || true
    for _ in $(seq 1 50); do kill -0 "$p" 2>/dev/null || return 0; sleep 0.1; done
    kill -KILL -- "-$p" 2>/dev/null || true
}
cleanup() {
    local p
    for p in "${PIDS[@]:-}"; do [ -n "$p" ] && stop_pid "$p"; done
    rm -rf "$WORK"
}
trap cleanup EXIT
trap 'exit 130' INT TERM

wait_http() { # URL
    for _ in $(seq 1 600); do curl -s -o /dev/null "$1" && return 0; sleep 0.5; done
    echo "timed out waiting for $1" >&2; return 1
}

start_mock() { # STREAM_DELAY
    ECHO=1 EMBED_DIMS=64 STREAM_DELAY="$1" start mock python3 scripts/mock-openai.py "$MOCK_PORT"
    MOCK_PID=$LAST_PID
    wait_http "http://127.0.0.1:$MOCK_PORT/v1/models"
}

LMGW_DATA_DIR="$WORK/data" start gateway scripts/dev-instance.sh "$ADDR"
wait_http "$BASE/api/session"
TOKEN="$(sed 's/\x1b\[[0-9;]*m//g' "$WORK/gateway.log" | grep -o 'lmgw-owner-[0-9a-f]*' | head -1)"
[ -n "$TOKEN" ] || { echo "no owner token in the gateway log:"; tail -20 "$WORK/gateway.log"; exit 1; }

start_mock 0.02
start leak python3 scripts/leak-listener.py "$LEAK_PORT"

scripts/chat-actions-setup.sh "$TOKEN" "$BASE" "$MOCK_PORT" >/dev/null
scripts/knowledge-setup.sh "$TOKEN" "$BASE" "$MOCK_PORT" >/dev/null
op() { curl -fsS -X POST "$BASE/api/op/$1" -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' -d "$2" >/dev/null; }
UP_ID="$(curl -sS "$BASE/api/upstreams" -H "Authorization: Bearer $TOKEN" | python3 -c '
import json,sys
d=json.load(sys.stdin); d=d.get("upstreams",d) if isinstance(d,dict) else d
print(next(u["id"] for u in d if u["name"]=="fake"))')"
# chat-attachments.json: a text-only and a vision alias (vision is its own key, not derived from the modalities)
op alias_set "{\"action\":\"create\",\"alias\":\"fake-novision\",\"upstream_id\":$UP_ID,\"upstream_model\":\"triage\",\"capabilities_override\":{\"capabilities\":{\"task\":\"chat\",\"vision\":false,\"input_modalities\":[\"text\"]}}}"
op alias_set "{\"action\":\"create\",\"alias\":\"fake-vision\",\"upstream_id\":$UP_ID,\"upstream_model\":\"triage\",\"capabilities_override\":{\"capabilities\":{\"task\":\"chat\",\"vision\":true,\"input_modalities\":[\"text\",\"image\"]}}}"
op settings_set '{"chat_pdf_mode":"ask"}'

DRIVES=(chat-render chat-actions chat-sampling chat-folders chat-search chat-knowledge knowledge
    chat-attachments chat-export chat-security chat-leave-midstream key-scope-editor key-scope-create-deny)
declare -A RESULT
FAILS=0
for d in "${DRIVES[@]}"; do
    if [ -n "$ONLY" ] && [[ ",$ONLY," != *",$d,"* ]]; then continue; fi
    if [ "$d" = chat-leave-midstream ]; then # needs a slow stream: restart the mock on the same port
        stop_pid "$MOCK_PID"
        start_mock 4
    fi
    if python3 scripts/ui-drive.py "scripts/drive/$d.json" --token "$TOKEN" --base "$BASE" \
        >"$WORK/drive-$d.log" 2>&1; then RESULT[$d]=PASS; else RESULT[$d]=FAIL; FAILS=$((FAILS + 1)); fi
    echo "$d: ${RESULT[$d]}"
    if [ "${RESULT[$d]}" = FAIL ]; then
        mkdir -p target/drive-logs; cp "$WORK/drive-$d.log" "target/drive-logs/$d.log"
        grep -E 'FAIL|panicked' "$WORK/drive-$d.log" | head -8 | sed 's/^/    /'
    fi
done

for x in matrix webkit; do
    [ "$x" = matrix ] && [ "$MATRIX" = 0 ] && continue
    [ "$x" = webkit ] && [ "$WEBKIT" = 0 ] && continue
    if [ "$x" = matrix ]; then cmd=(python3 scripts/ui-matrix.py --routes all --token "$TOKEN" --base "$BASE")
    else cmd=(python3 scripts/webkit-check.py --token "$TOKEN" --base "$BASE" --routes /chat,/knowledge,/usage/keys,/settings/chat); fi
    if "${cmd[@]}" >"target/drive-logs-$x.log" 2>&1; then RESULT[$x]=PASS; else RESULT[$x]=FAIL; FAILS=$((FAILS + 1)); fi
    tail -5 "target/drive-logs-$x.log"
done

echo
printf '%-26s %s\n' DRIVE RESULT
for d in "${DRIVES[@]}" matrix webkit; do
    [ -n "${RESULT[$d]:-}" ] && printf '%-26s %s\n' "$d" "${RESULT[$d]}"
done
echo "$FAILS FAIL"
[ "$FAILS" -eq 0 ]
