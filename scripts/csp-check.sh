#!/usr/bin/env bash
# Usage: scripts/csp-check.sh [--shots DIR]
# The Chat under an extension page's script policy (chat-plane design, "WP3a: the transport seam"):
# a fresh dev instance (scratch data dir, the echo mock upstream as chat model and TTS) behind
# scripts/csp-check/proxy.py, which records each violation the browser reports. Two headless-Chrome
# drives run through it: scripts/drive/chat-csp.json (the dashboard host, under the gateway's own policy
# plus `script-src 'self' 'wasm-unsafe-eval'`: the page loads with no inline script, and highlight.js,
# KaTeX and Mermaid render a reply; links, thumbnails and exports are the routes themselves, as before)
# and scripts/drive/chat-csp-relay.json (the debug build's test host, crates/lmgw-ui/src/test_host.rs,
# under the sidebar's extension page policy alone, proxy.py's EXTENSION: the send stream, read-aloud,
# the upload and the feed through the transport, which the proxy proves by the header the transport puts
# on every request; bytes as object URLs, revoked when their thread is left; a blob export; the feed cut
# mid-stream and resumed with its cursor; the preview page fed by postMessage). Passes when both drives
# pass, the only violation reported is the canary's, and no Chat request went past the test host's
# transport.
# Needs the bundle built (trunk build in crates/lmgw-ui, a debug build for the test host) and
# google-chrome. Everything it starts is stopped by pid on exit and its scratch dir removed.
set -euo pipefail
cd "$(dirname "$0")/.."

SHOTS=""
while [ $# -gt 0 ]; do
    case "$1" in
    --shots) SHOTS="$(realpath -m "$2")"; shift ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
    shift
done

[ -f crates/lmgw-ui/dist/index.html ] || { echo "no crates/lmgw-ui/dist: run trunk build first" >&2; exit 1; }

free_port() { python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1])'; }
GW_PORT="$(free_port)" MOCK_PORT="$(free_port)" PROXY_PORT="$(free_port)"
GW="http://127.0.0.1:$GW_PORT"
BASE="http://127.0.0.1:$PROXY_PORT"

WORK="$(mktemp -d -p /tmp lmgw-csp-check-XXXX)"
export TMPDIR="$WORK" # ui-drive's Chrome profiles land here and go with it
REPORTS="$WORK/reports.jsonl"
UNTAGGED="$WORK/untagged.jsonl"
: >"$REPORTS"
: >"$UNTAGGED"
PIDS=()
start() { # start LOGNAME CMD… : own session, so the whole group stops by its pid
    local log="$1"; shift
    setsid "$@" >"$WORK/$log.log" 2>&1 &
    PIDS+=("$!")
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

LMGW_DATA_DIR="$WORK/data" start gateway scripts/dev-instance.sh "127.0.0.1:$GW_PORT"
wait_http "$GW/api/session"
TOKEN="$(sed 's/\x1b\[[0-9;]*m//g' "$WORK/gateway.log" | grep -o 'lmgw-owner-[0-9a-f]*' | head -1)"
[ -n "$TOKEN" ] || { echo "no owner token in the gateway log:"; tail -20 "$WORK/gateway.log"; exit 1; }

ECHO=1 STREAM_DELAY=0.02 start mock python3 scripts/mock-openai.py "$MOCK_PORT"
wait_http "http://127.0.0.1:$MOCK_PORT/v1/models"
scripts/chat-actions-setup.sh "$TOKEN" "$GW" "$MOCK_PORT" >/dev/null
UP_ID="$(curl -sS "$GW/api/upstreams" -H "Authorization: Bearer $TOKEN" | python3 -c '
import json,sys
d=json.load(sys.stdin); d=d.get("upstreams",d) if isinstance(d,dict) else d
print(next(u["id"] for u in d if u["name"]=="fake"))')"
curl -fsS -X POST "$GW/api/op/alias_set" -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
    -d "{\"action\":\"create\",\"alias\":\"fake-vision\",\"upstream_id\":$UP_ID,\"upstream_model\":\"triage\",\"capabilities_override\":{\"capabilities\":{\"task\":\"chat\",\"vision\":true,\"input_modalities\":[\"text\",\"image\"]}}}" >/dev/null
# Read-aloud's voice: an OpenAI-shaped TTS on the mock (a silent WAV, still audio to stream).
op() { curl -fsS -X POST "$GW/api/op/$1" -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' -d "$2" >/dev/null; }
op upstream_set "{\"action\":\"create\",\"name\":\"fake-voice\",\"protocol\":\"openai_compatible\",\"kind\":\"generic\",\"base_url\":\"http://127.0.0.1:$MOCK_PORT/v1\"}"
op model_set '{"action":"create","alias":"fake-tts","upstream":"fake-voice","upstream_model":"tts-1","capabilities_override":{"capabilities":{"task":"tts","endpoints":["/v1/audio/speech"],"source":"owner"}}}'
op settings_set '{"chat_tts_alias":"fake-tts","chat_voice":"alloy"}'

start proxy python3 scripts/csp-check/proxy.py "$PROXY_PORT" "$GW" "$REPORTS" "$UNTAGGED"
wait_http "$BASE/__csp-check/canary.html"

FAILS=0
for d in chat-csp chat-csp-relay; do
    EXTRA=()
    if [ -n "$SHOTS" ]; then mkdir -p "$SHOTS/$d"; EXTRA+=(--shots "$SHOTS/$d"); fi
    if python3 scripts/ui-drive.py "scripts/drive/$d.json" --token "$TOKEN" --base "$BASE" "${EXTRA[@]}" \
        >"$WORK/drive-$d.log" 2>&1; then
        echo "$d: PASS"
    else
        echo "$d: FAIL"
        FAILS=$((FAILS + 1))
        mkdir -p target/drive-logs
        cp "$WORK/drive-$d.log" "target/drive-logs/$d.log"
        grep -m 12 -E 'FAIL|panicked' "$WORK/drive-$d.log" | sed 's/^/    /'
    fi
done

# The reports: the canary's inline script, and nothing else.
python3 - "$REPORTS" <<'EOF' || FAILS=$((FAILS + 1))
import json, sys
rows = [json.loads(l)["report"] for l in open(sys.argv[1]) if l.strip()]
reports = []
for r in rows:
    try:
        reports.append(json.loads(r).get("csp-report", {}))
    except ValueError:
        reports.append({"raw": r})
canary = [r for r in reports if r.get("document-uri", "").endswith("/__csp-check/canary.html")]
other = [r for r in reports if r not in canary]
print(f"violations reported: {len(reports)} (canary {len(canary)})")
for r in other:
    print("  ", r.get("document-uri"), r.get("violated-directive"), r.get("blocked-uri"), r.get("source-file"), r.get("line-number"))
if not canary:
    print("FAIL: the canary's inline script was not reported, so the check cannot see violations")
    sys.exit(1)
if other:
    print("FAIL: the Chat's pages broke the policy")
    sys.exit(1)
print("PASS: nothing but the canary broke the policy")
EOF

# The relay check: a Chat request in the test host that did not carry the transport's tag.
if [ -s "$UNTAGGED" ]; then
    echo "FAIL: Chat requests went past the test host's transport:"
    sed 's/^/    /' "$UNTAGGED"
    FAILS=$((FAILS + 1))
else
    echo "PASS: every Chat request of the relay drive went through the transport"
fi

echo "$FAILS FAIL"
[ "$FAILS" -eq 0 ]
