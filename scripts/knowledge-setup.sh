#!/usr/bin/env bash
# Usage: scripts/knowledge-setup.sh OWNER_TOKEN [BASE=http://127.0.0.1:8899] [MOCK_PORT=9291]
# Registers the fake upstream (scripts/mock-openai.py, started separately with
#   EMBED_DIMS=64 scripts/mock-openai.py 9291 &
# ) plus the embedding alias `fake-embed` (hashed bag-of-words vectors) on a dev
# instance, so the Knowledge page ingests and searches without a GPU. Idempotent.
# Used by scripts/drive/knowledge.json. The Chat's `fake-echo` alias comes from
# scripts/chat-actions-setup.sh (same upstream, so it is safe to run both).
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

op upstream_set "{\"action\":\"create\",\"name\":\"fake\",\"protocol\":\"llama_cpp\",\"base_url\":\"http://127.0.0.1:$PORT/v1\"}"
op model_set '{"action":"create","alias":"fake-embed","upstream":"fake","upstream_model":"embed-bow"}'
