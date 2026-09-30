#!/usr/bin/env bash
# Prepare a dev copy for the README screenshots: scrub it, start it, add demo chats and live /v1 traffic.
#
# Usage: scripts/readme-demo.sh [dir=target/dev-copy] [addr=127.0.0.1:8899]
#
# Run on a fresh `scripts/dev-copy.sh copy`, then `scripts/readme-shots.sh` while the models are
# still resident. It stops the copy to scrub it (chat threads and folders are deleted, model rows
# matching README_HIDE_MODELS too), starts it again, seeds four chat threads through the UI and
# sends a mix of OpenAI, Anthropic and embedding requests. It uses the GPU: check that the hold is
# off and the card is free first. The copy is left running.
#
# Models, overridable: README_CHAT_MODEL (gemma4-26b-a4b), README_TRAFFIC_MODEL (qwen3.5-9b),
# README_EMBED_MODEL (embed/Qwen3-Embedding-0.6B-Q8_0), README_HIDE_MODELS (a SQL LIKE pattern,
# %uncensored%).
set -euo pipefail
cd "$(dirname "$0")/.."

dir="$(realpath -m "${1:-target/dev-copy}")"
addr="${2:-127.0.0.1:8899}"
chat_model="${README_CHAT_MODEL:-gemma4-26b-a4b}"
traffic_model="${README_TRAFFIC_MODEL:-qwen3.5-9b}"
embed_model="${README_EMBED_MODEL:-embed/Qwen3-Embedding-0.6B-Q8_0}"
hide="${README_HIDE_MODELS:-%uncensored%}"

[ -e "$dir/lmgw.sqlite" ] || { echo "no copy at $dir (scripts/dev-copy.sh copy first)" >&2; exit 1; }

scripts/dev-copy.sh stop "$dir"
sqlite3 "$dir/lmgw.sqlite" "PRAGMA foreign_keys=ON;
    DELETE FROM chat_threads; DELETE FROM chat_folders;
    DELETE FROM local_models WHERE model_id LIKE '$hide';"
scripts/dev-copy.sh start "$dir" "$addr"

# Chat threads go through the UI, so they look like any other thread.
sed "s/gemma4-26b-a4b/$chat_model/g" scripts/drive/readme-chat.json \
    | python3 scripts/ui-drive.py - --base "http://$addr" --token-file "$dir.token" \
        --size 1536x864 --dsf 1.25 --podman-guard ''

base="http://$addr/v1"
chat() {  # chat <dialect> <prompt> <stream>
    if [ "$1" = openai ]; then
        curl -s -m 400 -o /dev/null -w "openai    %{http_code} %{time_total}s\n" "$base/chat/completions" \
            -H 'content-type: application/json' \
            -d "{\"model\":\"$traffic_model\",\"stream\":$3,\"messages\":[{\"role\":\"user\",\"content\":\"$2\"}]}"
    else
        curl -s -m 400 -o /dev/null -w "anthropic %{http_code} %{time_total}s\n" "$base/messages" \
            -H 'content-type: application/json' -H 'anthropic-version: 2023-06-01' \
            -d "{\"model\":\"$traffic_model\",\"max_tokens\":1024,\"messages\":[{\"role\":\"user\",\"content\":\"$2\"}]}"
    fi
}
embed() {
    curl -s -m 400 -o /dev/null -w "embedding %{http_code} %{time_total}s\n" "$base/embeddings" \
        -H 'content-type: application/json' -d "{\"model\":\"$embed_model\",\"input\":[\"$1\"]}"
}
chat openai "Write a regex that matches ISO 8601 dates." false
chat anthropic "Give me three name ideas for a home server." false
embed "a lighthouse at dusk"
chat openai "Explain what a KV cache is in two sentences." true
chat anthropic "Rewrite this commit message to be concise: fixed the thing where the stuff broke" false
embed "vector search basics"
chat openai "List five uses for a Raspberry Pi." true
chat anthropic "What is the capital of Australia? One word." false

echo "ready: scripts/readme-shots.sh $dir $addr (while the models are still resident)"
