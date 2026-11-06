#!/usr/bin/env bash
# Usage: scripts/chat-media-drive.sh [--chrome] [--webkit] [--kwin] [--matrix] [--live] [--shots DIR] [--dsf SCALE]
#                                    [--drive FILE] [--tools FILE] [--keep]
#
# The Chat's image and audio tools in the dashboard (chat-media-tools design §11, slices 3 and 4),
# with no container and no GPU: one fresh dev instance (scratch data dir, its own container prefix)
# whose chat model is scripts/mock-openai.py with a TOOL_SCRIPT, an image row `image/mock-turbo`
# (an empty weights file in a scratch models dir) held by the GPU hold, its fallback a cloud image
# alias `cloud-image` on the same mock (`/v1/images/generations` answers a small PNG after
# IMAGE_DELAY), and a TTS alias `tts-mock` on the mock too. Then scripts/drive/chat-media.json, with
# the helpers of scripts/drive/chat-media.js, its own thread and a second one to switch to, in each
# engine asked for:
# --chrome (scripts/ui-drive.py), --webkit (scripts/webkit-check.py --backend broadway --drive),
# both when neither is given; --kwin runs the WebKit drive on a real Wayland frame clock instead of
# Broadway: webkit-check's wayland backend inside a nested, virtual kwin_wayland (its own socket,
# nothing appears on the desktop; GPU compositing as the app window has it); --matrix adds scripts/ui-matrix.py on a drive's thread at 720, 900
# and 1280 CSS px wide (--tauri). Each drive: a picture's live pane, its card (facts, the fallback
# that answered, the full-size viewer), Save, Attach, Re-roll (left for another thread and back while
# it runs), a refused Upscale, a cancel that only stops the wait, a spoken result's player, a turn
# that draws twice (calls the mock sends without ids), a Stop mid-picture, every stream's frames
# whole at once, the card fitting at 720, 900 and 1280 CSS px, and the image tool's default model
# set in Settings → Chat, overridden by a folder and by the thread, then used by a call that names
# no model; --matrix measures /settings/chat too.
# --live puts a real image row in the mock row's place, with no hold: Z-Image-Turbo weights
# under LIVE_DIR (default ~/.local/share/lmgw/sdcpp) on SD_IMAGE (default
# localhost/lmgw-sd-server:sdcpp-master), and runs scripts/drive/chat-media-live.json instead: one
# container start and two 512x512 pictures per engine (a picture's previews and steps, then one
# cancelled mid-way behind the restart confirm); use it with one engine (--webkit). The prefix's
# containers (lmgw-media3-*, or $MEDIA_PREFIX-* when set: a parallel run needs its own) are removed
# on exit.
# --dsf is the interface scale (default 1.25, the app's); --shots keeps the PNGs (default
# target/chat-media-shots/drive); --drive runs another drive file on the same instance (a step
# `"js": "__HELPERS:<path>__"` gets that file's helpers, after chat-media.js's); --tools FILE is the
# mock's TOOL_SCRIPT instead of the built-in one (the turns of every engine run, in order: e.g.
# scripts/drive/chat-follow.json with scripts/drive/chat-follow.tools.json); --keep leaves the
# instance running (its URL and token printed, stopped with Ctrl-C).
# Everything started is stopped by pid/process group on exit and the scratch dir (data, Chrome
# profiles) removed.
set -euo pipefail
cd "$(dirname "$0")/.."

CHROME=false WEBKIT=false KWIN=false MATRIX=false KEEP=false DSF="1.25" SHOTS="target/chat-media-shots/drive"
DRIVE="" LIVE=false TOOLS=""
LIVE_DIR="${LIVE_DIR:-$HOME/.local/share/lmgw/sdcpp}" SD_IMAGE="${SD_IMAGE:-localhost/lmgw-sd-server:sdcpp-master}"
PREFIX="${MEDIA_PREFIX:-lmgw-media3}"
while [ $# -gt 0 ]; do
    case "$1" in
    --chrome) CHROME=true ;;
    --webkit) WEBKIT=true ;;
    --kwin) WEBKIT=true KWIN=true ;;
    --matrix) MATRIX=true ;;
    --keep) KEEP=true ;;
    --live) LIVE=true ;;
    --drive) DRIVE="$2"; shift ;;
    --tools) TOOLS="$2"; shift ;;
    --dsf) DSF="$2"; shift ;;
    --shots) SHOTS="$2"; shift ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
    shift
done
if ! $CHROME && ! $WEBKIT; then CHROME=true WEBKIT=true; fi
if [ -z "$DRIVE" ]; then
    if $LIVE; then DRIVE="scripts/drive/chat-media-live.json"; else DRIVE="scripts/drive/chat-media.json"; fi
fi
BIN=target/debug/examples/headless
[ -x "$BIN" ] || { echo "build it first: cargo build --workspace --example headless" >&2; exit 1; }

free_port() { python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1])'; }
PORT="$(free_port)" MOCK_PORT="$(free_port)"
BASE="http://127.0.0.1:$PORT"
WORK="$(mktemp -d -p /tmp lmgw-media-XXXX)"
export TMPDIR="$WORK" # Chrome profiles of ui-drive and ui-matrix land here and go with it
PIDS=()
start() { local log="$1"; shift; setsid "$@" >"$WORK/$log.log" 2>&1 & PIDS+=("$!"); }
cleanup() {
    for p in "${PIDS[@]:-}"; do [ -n "$p" ] && kill -TERM -- "-$p" 2>/dev/null || true; done
    for p in "${PIDS[@]:-}"; do
        [ -n "$p" ] || continue
        for _ in $(seq 1 50); do kill -0 "$p" 2>/dev/null || break; sleep 0.1; done
        kill -KILL -- "-$p" 2>/dev/null || true
    done
    # The instance stops its containers when it quits; whatever of this prefix is left goes too.
    podman ps -a --format '{{.Names}}' 2>/dev/null | grep "^$PREFIX-" | xargs -r podman rm -f >/dev/null 2>&1 || true
    rm -rf "$WORK"
}
trap cleanup EXIT
trap 'exit 130' INT TERM
wait_http() { for _ in $(seq 1 600); do curl -s -o /dev/null "$1" && return 0; sleep 0.5; done; echo "timed out: $1" >&2; return 1; }

# Per engine, one turn per send in the drive's order: a picture, a picture the drive cancels,
# a spoken sentence, two pictures one after the other (a tool loop of two rounds, each call without
# an id, which the gateway names), a picture the drive stops and a picture whose call names no model
# (the thread's default draws it); live, a picture of 20 steps and one of 40 the drive cancels.
python3 - "$LIVE" >"$WORK/tool-script.json" <<'PY'
import json, sys
live = sys.argv[1] == "true"
def draw(model, steps=None, no_id=False):
    args = {"model": model, "prompt": "a lighthouse on a cliff at dusk, waves below",
            "size": "512x512", "seed": 42}
    if model is None:  # the thread's default model draws it (design §3.8)
        del args["model"]
    if steps:
        args["steps"] = steps
    call = {"name": "image__generate", "arguments": args}
    if no_id:
        call["id"] = False
    return [call]
speak = [{"name": "audio__speak", "arguments": {"model": "tts-mock", "text": "The lighthouse is ready."}}]
if live:
    print(json.dumps([draw("image/z-image-turbo", 20), draw("image/z-image-turbo", 40)] * 2))
else:
    twice = {"rounds": [draw("image/mock-turbo", no_id=True), draw("image/mock-turbo", no_id=True)]}
    print(json.dumps([draw("image/mock-turbo"), draw("image/mock-turbo"), speak, twice,
                      draw("image/mock-turbo"), draw(None)] * 2))
PY
[ -z "$TOOLS" ] || cp "$TOOLS" "$WORK/tool-script.json"
TOOL_SCRIPT="@$WORK/tool-script.json" IMAGE_DELAY=4 STREAM_DELAY=0.01 STREAM_TOKENS=6 \
    start mock python3 scripts/mock-openai.py "$MOCK_PORT"
wait_http "http://127.0.0.1:$MOCK_PORT/v1/models"

mkdir -p "$WORK/sdcpp/leejet/Z-Image-Turbo-GGUF"
: >"$WORK/sdcpp/leejet/Z-Image-Turbo-GGUF/z_image_turbo-Q4_K.gguf"
LMGW_DATA_DIR="$WORK/data" LMGW_DEV=1 LMGW_CONTAINER_PREFIX="$PREFIX" start gateway "$BIN" "127.0.0.1:$PORT"
wait_http "$BASE/api/session"
TOKEN="$(sed 's/\x1b\[[0-9;]*m//g' "$WORK/gateway.log" | grep -o 'lmgw-owner-[0-9a-f]*' | head -1)"
[ -n "$TOKEN" ] || { echo "no owner token in the gateway log:"; tail -20 "$WORK/gateway.log"; exit 1; }
printf '%s' "$TOKEN" >"$WORK/token"

op() { # op NAME JSON: the answer on stdout; a refusal's body on stderr
    local out
    out="$(curl -sS --fail-with-body -X POST "$BASE/api/op/$1" -H "Authorization: Bearer $TOKEN" \
        -H 'Content-Type: application/json' -d "$2")" || { echo "op $1 refused: $out" >&2; return 1; }
    printf '%s\n' "$out"
}
if $LIVE; then
    op settings_set_full "{\"image\":{\"models_dir\":\"$LIVE_DIR\",\"image\":\"$SD_IMAGE\"}}" >/dev/null
else
    op settings_set_full "{\"image\":{\"models_dir\":\"$WORK/sdcpp\"}}" >/dev/null
fi
op upstream_set "{\"action\":\"create\",\"name\":\"mock\",\"base_url\":\"http://127.0.0.1:$MOCK_PORT/v1\",\"protocol\":\"openai_compatible\"}" >/dev/null
op model_set '{"action":"create","alias":"chat-mock","upstream":"mock","upstream_model":"mock"}' >/dev/null
op model_set '{"action":"create","alias":"cloud-image","upstream":"mock","upstream_model":"mock-image","capabilities_override":{"capabilities":{"task":"image_generation"}}}' >/dev/null
op model_set '{"action":"create","alias":"tts-mock","upstream":"mock","upstream_model":"mock-tts","capabilities_override":{"capabilities":{"task":"tts"}}}' >/dev/null
if $LIVE; then
    op image_model_set '{"action":"create","model_id":"z-image-turbo","files":{"diffusion_model":"leejet/Z-Image-Turbo-GGUF/z_image_turbo-Q4_K.gguf","vae":"Comfy-Org/z_image_turbo/split_files/vae/ae.safetensors","llm":"unsloth/Qwen3-4B-Instruct-2507-GGUF/Qwen3-4B-Instruct-2507-Q4_K_M.gguf"},"args":{"steps":8,"cfg_scale":1.0},"enabled":true}' >/dev/null
else
    op image_model_set '{"action":"create","model_id":"mock-turbo","files":{"diffusion_model":"leejet/Z-Image-Turbo-GGUF/z_image_turbo-Q4_K.gguf"},"args":{"steps":8,"cfg_scale":1.0},"enabled":true,"fallback":"cloud-image"}' >/dev/null
    op hold_set '{"active":true}' >/dev/null
fi

# A thread per engine on the mock, with the image and audio toolsets attached.
thread() {
    local id
    id="$(curl -fsS -X POST "$BASE/chat/api/threads" -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
        -d '{"model_alias":"chat-mock"}' | python3 -c 'import json,sys;print(json.load(sys.stdin)["id"])')"
    curl -fsS -X POST "$BASE/chat/api/threads/$id/settings" -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
        -d '{"mcp_tools":[{"server_label":"image"},{"server_label":"audio"}]}' >/dev/null
    echo "$id"
}
# The drive with its thread and the helpers filled in.
render() { # render THREAD OTHER OUT
    python3 - "$1" "$2" "$DRIVE" scripts/drive/chat-media.js "$3" <<'PY'
import json, sys
tid, other, src, helpers, out = sys.argv[1:6]
js = open(helpers).read()
def fill(v):
    if isinstance(v, str):
        if v.startswith("__HELPERS:") and v.endswith("__"):
            return open(v[len("__HELPERS:"):-2]).read()
        return js if v == "__HELPERS__" else v.replace("__THREAD__", tid).replace("__OTHER__", other)
    if isinstance(v, list):
        return [fill(x) for x in v]
    if isinstance(v, dict):
        return {k: fill(x) for k, x in v.items()}
    return v
json.dump(fill(json.load(open(src))), open(out, "w"), indent=1)
PY
}

mkdir -p "$SHOTS"
SIZE="$(python3 -c 'import sys;s=float(sys.argv[1]);print(f"{round(1440/s)}x{round(900/s)}")' "$DSF")"
FAILS=0
declare -A RESULT
LAST_THREAD=""
if $CHROME; then
    T="$(thread)" LAST_THREAD="$T"
    render "$T" "$(thread)" "$WORK/drive-chrome.json"
    mkdir -p "$SHOTS/chrome"
    if python3 scripts/ui-drive.py "$WORK/drive-chrome.json" --token "$TOKEN" --base "$BASE" \
        --shots "$SHOTS/chrome" --dsf "$DSF" --size "$SIZE" >"$WORK/chrome.log" 2>&1; then
        RESULT[chrome]=PASS
    else RESULT[chrome]=FAIL FAILS=$((FAILS + 1)); fi
    cp "$WORK/chrome.log" "$SHOTS/chrome/drive.log"
    curl -fsS "$BASE/chat/api/threads/$T" -H "Authorization: Bearer $TOKEN" >"$SHOTS/chrome/thread.json" || true
    grep -E 'PASS|FAIL' "$WORK/chrome.log" | sed 's/^/  chrome  /' | tail -40
fi
if $WEBKIT; then
    T="$(thread)" LAST_THREAD="$T"
    render "$T" "$(thread)" "$WORK/drive-webkit.json"
    mkdir -p "$SHOTS/webkit"
    # webkit-check exits 0 when the run completed; its expects are read from the report.
    WK=(python3 scripts/webkit-check.py --zoom "$DSF" --timeout 1200 --base "$BASE"
        --token "$TOKEN" --drive "$WORK/drive-webkit.json" --shots "$SHOTS/webkit"
        --json "$WORK/webkit.json")
    if $KWIN; then
        # The view in a nested kwin of its own, on the session's bus (as the app window is).
        printf '#!/usr/bin/env bash\nunset DISPLAY\nexport DBUS_SESSION_BUS_ADDRESS=%q\nexec %s --backend wayland\n' \
            "$DBUS_SESSION_BUS_ADDRESS" "$(printf '%q ' "${WK[@]}")" >"$WORK/kwin-session.sh"
        chmod +x "$WORK/kwin-session.sh"
        timeout 1300 dbus-run-session -- kwin_wayland --virtual --socket "wayland-lmgw-media-$$" \
            --width 1700 --height 1100 --no-lockscreen --no-global-shortcuts \
            --exit-with-session "$WORK/kwin-session.sh" >"$WORK/webkit.log" 2>&1 || true
    else
        "${WK[@]}" --backend broadway >"$WORK/webkit.log" 2>&1 || true
    fi
    cp "$WORK/webkit.log" "$SHOTS/webkit/drive.log"
    curl -fsS "$BASE/chat/api/threads/$T" -H "Authorization: Bearer $TOKEN" >"$SHOTS/webkit/thread.json" || true
    if python3 - "$WORK/webkit.json" <<'PY'
import json, sys
try:
    r = json.load(open(sys.argv[1]))
except Exception as e:
    print(f"  webkit  no report: {e}"); sys.exit(1)
rows = [i for run in (r if isinstance(r, list) else [r]) for i in run.get("interactions", [])]
bad = [i for i in rows if i.get("drive_pass") is False or not i.get("ok", True)]
for i in rows:
    if "drive_pass" in i:
        print(f"  webkit  {'PASS' if i['drive_pass'] else 'FAIL'}  {i['name']}"
              + ("" if i["drive_pass"] else f"  ({json.dumps(i.get('detail'))[:300]})"))
sys.exit(1 if bad or not rows else 0)
PY
    then RESULT[webkit]=PASS; else RESULT[webkit]=FAIL FAILS=$((FAILS + 1)); fi
fi
if $MATRIX && [ -n "$LAST_THREAD" ]; then
    if python3 scripts/ui-matrix.py --routes "/chat?t=$LAST_THREAD,/settings/chat" --sizes 720x900,900x900,1280x800 \
        --dsf "$DSF" --tauri --token "$TOKEN" --base "$BASE" --shots "$SHOTS/matrix" >"$WORK/matrix.log" 2>&1; then
        RESULT[matrix]=PASS
    else RESULT[matrix]=FAIL FAILS=$((FAILS + 1)); fi
    cp "$WORK/matrix.log" "$SHOTS/matrix.log"
    tail -8 "$WORK/matrix.log"
fi

echo
for k in chrome webkit matrix; do [ -n "${RESULT[$k]:-}" ] && printf '%-8s %s\n' "$k" "${RESULT[$k]}"; done
echo "$FAILS FAIL"
if $KEEP; then
    echo "instance: $BASE  token: $TOKEN  (Ctrl-C stops it)"
    wait "${PIDS[1]}" || true
fi
[ "$FAILS" -eq 0 ]
