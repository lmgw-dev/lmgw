#!/usr/bin/env bash
# Usage: scripts/convert-perf/hang.sh TAG   performance round review B1 against the instance gw.sh started (run it with 16G): pics100.pdf with pictures=embedded and images_scale=12, whose output budget runs out mid-file (a page's crops cut again in their turn), and 5 s into it another caller's one-page PDF; both must answer. Rows in target/convert-perf/hang-TAG.txt. -e is left out on purpose: a failed row still prints.
set -uo pipefail
cd "$(dirname "$0")/../.."; . target/convert-perf/env.sh; export TOKEN GWPID CG GW
B=scripts/convert-perf/bench.py; tag="$1"; IN="${PERF_IN:-$HOME/.local/share/lmgw-corpora/convert-perf-2026-11-19}"
one=target/convert-perf/one-page.pdf
[ -f "$one" ] || pdfseparate -f 1 -l 1 "$IN/text5.pdf" "$one"
{
python3 -I $B "$tag pics100 embedded x12 16G" --field pictures=embedded --field images_scale=12 $IN/pics100.pdf &
big=$!
sleep 5
python3 -I $B "$tag one page beside it" "$one"
curl -s -X POST -H "Authorization: Bearer $TOKEN" -H 'content-type: application/json' -d '{}' $GW/api/op/doc_tools | python3 -c "import json,sys; p=json.load(sys.stdin)['pool']; print('$tag pool meanwhile', {k: p[k] for k in ('slots','running','waiting','charged_bytes')})"
wait $big
} 2>&1 | sed -E 's/ lat_max [0-9.]+//; s/ sum_time_ms [0-9]+//; s/ warn \[[^]]*\]//; s/ err \[\]//' | tee target/convert-perf/hang-"$tag".txt
