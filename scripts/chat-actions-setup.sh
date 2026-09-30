#!/usr/bin/env bash
# Usage: scripts/chat-actions-setup.sh OWNER_TOKEN [BASE=http://127.0.0.1:8899] [MOCK_PORT=9291]
# Registers the fake upstream (scripts/mock-openai.py, started separately with
#   ECHO=1 STREAM_DELAY=0.02 scripts/mock-openai.py 9291 &
# ) and the alias `fake-echo` on a dev instance, so the Chat streams replies
# without a GPU. Idempotent: an existing upstream/alias is left as it is.
# The upstream is a `llama_server` one, because only that (or Anthropic) route
# takes the assistant prefill the Chat's Continue sends. Used by
# scripts/drive/chat-actions.json.
set -euo pipefail

TOKEN="${1:?owner token (the log line of the dev instance)}"
BASE="${2:-http://127.0.0.1:8899}"
PORT="${3:-9291}"

op() { # op NAME JSON -> prints the answer; "already there" is fine
    local out
    out="$(curl -sS -X POST "$BASE/api/op/$1" \
        -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' -d "$2")"
    case "$out" in
    *UNIQUE*) echo "$1: already registered" ;;
    *'"ok":true'*) echo "$1: $out" ;;
    *) echo "$1 failed: $out" >&2; exit 1 ;;
    esac
}

op upstream_set "{\"action\":\"create\",\"name\":\"fake\",\"protocol\":\"openai\",\"kind\":\"llama_server\",\"base_url\":\"http://127.0.0.1:$PORT/v1\"}"
op model_set '{"action":"create","alias":"fake-echo","upstream":"fake","upstream_model":"triage"}'
