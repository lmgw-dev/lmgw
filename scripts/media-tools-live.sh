#!/usr/bin/env bash
# Live check of the Chat's image tools while they run (chat-media-tools design §11, slice 2):
# a Chat turn's progress frames and previews, /mcp progress, and a cancel mid-generation with
# its timing and the GPU memory back at its baseline. Four generations: two at 512x512, one
# at 1024x1024 cancelled mid-way, and one /mcp call at 512x512.
#
# Usage: LMGW_TOKEN=<owner bearer> [STEPS="1 2 3 4"] scripts/media-tools-live.sh <gateway-url> [image-alias]
#
# STEPS picks the checks to run (each costs one generation): 1 a Chat turn at 512x512 with the
# row's preview mode, 2 an /mcp call at 512x512 with a progress token, 3 a Chat turn at 512x512
# with full-decoder previews, 4 a Chat turn at 1024x1024 cancelled mid-generation.
#
# Runs against a running instance (a dev instance: scripts/dev-instance.sh) whose image row
# is enabled; skips with exit 0 when it has none. The Chat's model is scripts/mock-openai.py
# on a free port, registered as an upstream and an alias for the run and removed afterwards;
# so is the thread. Reports go to stdout and target/media-tools-live/ (the SSE bodies, the
# running build's capabilities as the gateway read them, the preview frames).
set -euo pipefail

BASE="${1:?usage: LMGW_TOKEN=<owner bearer> $0 <gateway-url> [image-alias]}"
BASE="${BASE%/}"
TOKEN="${LMGW_TOKEN:?set LMGW_TOKEN to the instance's owner bearer (its log's dashboard login token)}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$ROOT/target/media-tools-live"
mkdir -p "$OUT"
AUTH=(-H "Authorization: Bearer $TOKEN")

api() { # api <op> <json>: POST /api/op/<op>
    curl -fsS "${AUTH[@]}" -H 'content-type: application/json' -d "$2" "$BASE/api/op/$1"
}
vram_used() { # MiB in use on the first GPU, or "n/a"
    nvidia-smi --query-gpu=memory.used --format=csv,noheader,nounits 2>/dev/null | head -1 || echo n/a
}

ALIAS="${2:-$(curl -fsS "${AUTH[@]}" "$BASE/v1/models" | python3 -c '
import json, sys
ids = [m["id"] for m in json.load(sys.stdin)["data"] if m["id"].startswith("image/")]
print(ids[0] if ids else "")')}"
if [ -z "$ALIAS" ]; then
    echo "skip: no enabled image row on $BASE"
    exit 0
fi
echo "image row: $ALIAS"

# -- The Chat's model: a mock that asks for one picture per turn ------------------------------
PORT="$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])')"
STEPS="${STEPS:-1 2 3 4}"
on() { [[ " $STEPS " == *" $1 "* ]]; }
SCRIPT="$OUT/tool-script.json"
# One scripted turn per Chat step that runs, in order.
python3 - "$ALIAS" "$STEPS" >"$SCRIPT" <<'PY'
import json, sys
alias, steps = sys.argv[1], sys.argv[2].split()
draw = lambda size: [{"name": "image__generate",
                      "arguments": {"model": alias, "prompt": "a lighthouse on a cliff at dusk",
                                    "size": size, "seed": 42}}]
sizes = {"1": "512x512", "3": "512x512", "4": "1024x1024"}
print(json.dumps([draw(sizes[s]) for s in sorted(steps) if s in sizes]))
PY
TOOL_SCRIPT="@$SCRIPT" STREAM_DELAY=0 STREAM_TOKENS=4 python3 "$ROOT/scripts/mock-openai.py" "$PORT" \
    >"$OUT/mock.log" 2>&1 &
MOCK=$!
UP_ID="" ALIAS_ID="" TID=""
cleanup() {
    [ -n "$TID" ] && curl -fsS "${AUTH[@]}" -X POST "$BASE/chat/api/threads/$TID/delete" >/dev/null || true
    [ -n "$ALIAS_ID" ] && api model_set "{\"action\":\"delete\",\"id\":$ALIAS_ID}" >/dev/null || true
    [ -n "$UP_ID" ] && api upstream_set "{\"action\":\"delete\",\"id\":$UP_ID}" >/dev/null || true
    kill "$MOCK" 2>/dev/null || true
    wait "$MOCK" 2>/dev/null || true
}
trap cleanup EXIT
for _ in $(seq 50); do curl -fsS "http://127.0.0.1:$PORT/v1/models" >/dev/null 2>&1 && break; sleep 0.1; done

UP_ID="$(api upstream_set "{\"action\":\"create\",\"name\":\"media-live-mock\",\"base_url\":\"http://127.0.0.1:$PORT/v1\",\"protocol\":\"openai_compatible\"}" | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')"
ALIAS_ID="$(api model_set '{"action":"create","alias":"media-live-chat","upstream":"media-live-mock","upstream_model":"mock"}' | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')"
TID="$(curl -fsS "${AUTH[@]}" -H 'content-type: application/json' -d '{"model_alias":"media-live-chat"}' \
    "$BASE/chat/api/threads" | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')"
curl -fsS "${AUTH[@]}" -H 'content-type: application/json' \
    -d '{"mcp_tools":[{"server_label":"image"}]}' "$BASE/chat/api/threads/$TID/settings" >/dev/null

send() { # send <out-file>: one Chat turn, its SSE body in the file
    curl -sS -N "${AUTH[@]}" -H 'content-type: application/json' -d '{"content":"draw it"}' \
        "$BASE/chat/api/threads/$TID/send" >"$1"
}
# What a turn's progress frames say: phases, steps, preview sizes (PNG header) and the result.
report() {
    python3 - "$1" <<'PY'
import base64, json, struct, sys
frames, result = [], None
for block in open(sys.argv[1]).read().split("\n\n"):
    ev = data = None
    for line in block.splitlines():
        if line.startswith("event:"): ev = line[6:].strip()
        elif line.startswith("data:"): data = line[5:].strip()
    if ev != "tool" or not data: continue
    f = json.loads(data)
    if f.get("event") == "progress": frames.append(f)
    elif f.get("event") == "result": result = f
sizes = set()
for f in frames:
    if f.get("preview"):
        png = base64.b64decode(f["preview"])
        sizes.add(struct.unpack(">II", png[16:24]) if png[:8] == b"\x89PNG\r\n\x1a\n" else ("not a PNG",))
phases = []
for f in frames:
    p = f["phase"] + (f" {f['step']}/{f['total_steps']}" if "step" in f else "")
    if not phases or phases[-1] != p: phases.append(p)
print(f"  progress frames: {len(frames)}; phases: {' -> '.join(phases)}")
print(f"  cancel said: {sorted({f.get('cancel') for f in frames if f.get('cancel')})}")
print(f"  preview sizes: {sorted(sizes) or 'none'}; frames with a preview: {sum(1 for f in frames if f.get('preview'))}")
if result:
    print(f"  result ({result['ms']} ms): {result['output'][:300]}")
    print(f"  attachments: {[(a['id'], a['name']) for a in result.get('attachments', [])]}")
PY
}

BASELINE="$(vram_used)"
echo "GPU memory before: $BASELINE MiB"

# The running build's capabilities as the gateway read them, once the row is up.
CAPS_SAVED=""
capabilities() {
    [ -n "$CAPS_SAVED" ] && return 0
    CAPS_SAVED=1
    echo "  GPU memory with the row loaded: $(vram_used) MiB"
    curl -fsS "${AUTH[@]}" "$BASE/image-lab/api/models" | python3 -c '
import json, sys
for m in json.load(sys.stdin)["models"]:
    c = m.get("image_capabilities")
    if c:
        json.dump(c, open(sys.argv[1], "w"), indent=2)
        f = c["features"]
        print("  build: preview_modes", c.get("preview_modes"), "cancel_queued",
              f.get("cancel_queued"), "cancel_generating", f.get("cancel_generating"))
' "$OUT/capabilities.json"
}

# -- 1. A Chat turn at 512x512 with the row's preview mode (quick sketch by default) ----------
if on 1; then
echo "1. Chat, 512x512, default preview mode"
send "$OUT/turn-512.sse"
report "$OUT/turn-512.sse"
capabilities
fi

# -- 2. /mcp at 512x512 with a progress token ---------------------------------------------------
if on 2; then
echo "2. /mcp, 512x512, progressToken"
SID="$(curl -fsS -D - -o /dev/null "${AUTH[@]}" -H 'accept: application/json, text/event-stream' \
    -H 'content-type: application/json' \
    -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"media-tools-live","version":"0"}}}' \
    "$BASE/mcp" | tr -d '\r' | awk -F': ' 'tolower($1)=="mcp-session-id"{print $2}')"
curl -sS -N "${AUTH[@]}" -H 'accept: application/json, text/event-stream' -H 'content-type: application/json' \
    -H "mcp-session-id: $SID" \
    -d "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"image__generate\",\"arguments\":{\"model\":\"$ALIAS\",\"prompt\":\"a red bicycle\",\"size\":\"512x512\"},\"_meta\":{\"progressToken\":\"live-1\"}}}" \
    "$BASE/mcp" >"$OUT/mcp.sse"
python3 - "$OUT/mcp.sse" <<'PY'
import json, sys
msgs = [json.loads(l[5:]) for l in open(sys.argv[1]) if l.startswith("data:")]
notes = [m["params"] for m in msgs if m.get("method") == "notifications/progress"]
rising = all(b["progress"] > a["progress"] for a, b in zip(notes, notes[1:]))
print(f"  notifications: {len(notes)}, rising: {rising}; last: {notes[-1]['message'] if notes else '-'}")
last = msgs[-1] if msgs else {}
print(f"  answer: id {last.get('id')}, isError {last.get('result', {}).get('isError')}")
PY
capabilities
fi

# -- 3. A Chat turn at 512x512, previews drawn by the full decoder ------------------------------
if on 3; then
echo "3. Chat, 512x512, full-decoder previews"
ROW="$(curl -fsS "${AUTH[@]}" "$BASE/image-lab/api/models" | python3 -c '
import json, sys
print(next((m.get("model_id") or "" for m in json.load(sys.stdin)["models"] if m.get("name") == sys.argv[1]), ""))' "$ALIAS")"
if [ -n "$ROW" ] && api image_model_set "{\"action\":\"update\",\"model_id\":\"$ROW\",\"preview_mode\":\"vae\"}" >/dev/null; then
    send "$OUT/turn-512-vae.sse"
    report "$OUT/turn-512-vae.sse"
    api image_model_set "{\"action\":\"update\",\"model_id\":\"$ROW\",\"clear\":\"preview_mode\"}" >/dev/null
else
    echo "  skipped: $ALIAS is no local row of this gateway"
fi
fi

# -- 4. A Chat turn at 1024x1024, cancelled mid-generation --------------------------------------
if on 4; then
echo "4. Chat, 1024x1024, cancelled mid-generation"
send "$OUT/turn-1024.sse" &
TURN=$!
CALL=""
for _ in $(seq 1200); do
    CALL="$(python3 - "$OUT/turn-1024.sse" <<'PY'
import json, sys
for block in open(sys.argv[1]).read().split("\n\n"):
    data = next((l[5:].strip() for l in block.splitlines() if l.startswith("data:")), None)
    if "event: tool" in block and data:
        f = json.loads(data)
        if f.get("event") == "progress" and f.get("phase") == "generating" and f.get("step", 0) >= 2:
            print(f["call_id"]); break
PY
)"
    [ -n "$CALL" ] && break
    sleep 0.05
done
if [ -z "$CALL" ]; then
    echo "  no generating step was seen; nothing cancelled"
    wait "$TURN"
else
    echo "  GPU memory while generating: $(vram_used) MiB"
    T0="$(date +%s.%N)"
    curl -fsS "${AUTH[@]}" -X POST "$BASE/chat/api/threads/$TID/media/$CALL/cancel" | sed 's/^/  cancel answered: /'
    echo
    wait "$TURN"
    T1="$(date +%s.%N)"
    echo "  the turn ended $(python3 -c "print(f'{$T1-$T0:.2f}')") s after the cancel"
    report "$OUT/turn-1024.sse"
    for i in $(seq 300); do
        NOW="$(vram_used)"
        if [ "$NOW" = n/a ] || [ "$BASELINE" = n/a ] || [ "$NOW" -le $((BASELINE + 64)) ]; then
            echo "  GPU memory back to $NOW MiB (baseline $BASELINE) $(python3 -c "print(f'{$(date +%s.%N)-$T0:.2f}')") s after the cancel"
            break
        fi
        [ "$i" = 300 ] && echo "  GPU memory still $NOW MiB 30 s after the cancel (baseline $BASELINE)"
        sleep 0.1
    done
fi
fi
echo "files: $OUT"
