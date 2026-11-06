#!/usr/bin/env bash
# Records Hugging Face's answers for find-a-model's regression set (design
# §4.4, D38, WP4): the 28 queries of tests/it/hf_find/regression.rs and its
# later ones (`LATER`), each on a 24 GiB and an 8 GiB card, through lmgw's
# own finder, so the recording holds exactly the reads the rules make. It
# sends no token and writes only crates/lmgw-core/tests/fixtures/hf_find/
# (index.json and answers.zip).
# Re-run it when a rule asks the hub something the last recording lacks
# (the regression test names the read), when the finder reads a new field
# of a listing (`HubModel`; loading the recording names the missing field),
# or to refresh the hub's answers; then check the table's cards again.
# With --add it keeps the last recording's answers and reads only what they
# lack (a new query, a new read), so the cards recorded before stay as they
# were; a new field of `HubModel` needs a whole recording instead.
#
# Usage: scripts/hf-find-record.sh [--add]
# About 160 hub requests for a whole recording, under the anonymous 500 per
# 5 minutes; a rate-limited run saves nothing and says so.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

# No token reaches the recording, whatever this shell has set.
unset HF_TOKEN HUGGING_FACE_HUB_TOKEN HF_ENDPOINT LMGW_HF_FIND_ADD

case "${1:-}" in
  --add) export LMGW_HF_FIND_ADD=1 ;;
  "") ;;
  *) echo "usage: $0 [--add]" >&2; exit 2 ;;
esac

export LMGW_HF_FIND_RECORD
LMGW_HF_FIND_RECORD="$(date -u +%F)"

run=(cargo test --workspace --test it -- --ignored --exact
  hf_find::regression::record_the_queries --nocapture)
# Share the machine's build lock when there is one (several builders).
if command -v flock > /dev/null; then
  flock -o /tmp/lmgw-cargo-build.lock "${run[@]}"
else
  "${run[@]}"
fi
ls -l crates/lmgw-core/tests/fixtures/hf_find/
