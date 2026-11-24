#!/usr/bin/env bash
# Usage: scripts/convert-perf/m16.sh TAG   rows for the pool derived from 16 GiB (gw.sh start BINARY 16G); target/convert-perf/m16-TAG.txt. -e is left out on purpose: the rows after a failed one still run
set -uo pipefail
cd "$(dirname "$0")/../.."; . target/convert-perf/env.sh; export TOKEN GWPID CG GW
B=scripts/convert-perf/bench.py; tag="$1"; IN="${PERF_IN:-$HOME/.local/share/lmgw-corpora/convert-perf-2026-11-19}"
{
curl -s -X POST -H "Authorization: Bearer $TOKEN" -H 'content-type: application/json' -d '{}' $GW/api/op/doc_tools | python3 -c "import json,sys; p=json.load(sys.stdin)['pool']; print('$tag pool', {k: p[k] for k in ('slots','derived')}, p['derived_from'].get('memory_bytes'), p['derived_from'].get('charges'))"
python3 -I $B "$tag scan20 16G" $IN/scan20.pdf
python3 -I $B "$tag scan20 16G" $IN/scan20.pdf
python3 -I $B "$tag pics100 md 16G" $IN/pics100.pdf
python3 -I $B "$tag pics100 embedded 16G" --field pictures=embedded $IN/pics100.pdf
python3 -I $B "$tag small50 16G" $IN/small50/*.pdf
python3 -I $B "$tag text500 16G" $IN/text500.pdf
} 2>&1 | sed -E 's/ lat_max [0-9.]+//; s/ sum_time_ms [0-9]+//; s/ http 200//; s/ warn \[[^]]*\]//; s/ err \[\]//' | tee target/convert-perf/m16-$tag.txt
