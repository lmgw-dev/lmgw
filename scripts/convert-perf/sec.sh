#!/usr/bin/env bash
# Usage: scripts/convert-perf/sec.sh TAG [RUNS]   security round A's rows (design §14) against the instance gw.sh started, RUNS times each (default 3): 50 small PDFs, the same from 8 callers at once, the corpus' 20 PDFs in one request, pics100.pdf with embedded pictures, an async job of 200 HTML files; rows in target/convert-perf/sec-TAG.txt. -e is left out on purpose: the rows after a failed one still run
set -uo pipefail
cd "$(dirname "$0")/../.."; . target/convert-perf/env.sh; export TOKEN GWPID CG GW
B=scripts/convert-perf/bench.py; J=scripts/convert-perf/jobs.py; tag="$1"; runs="${2:-3}"
IN="${PERF_IN:-$HOME/.local/share/lmgw-corpora/convert-perf-2026-11-19}"
C="${CORPUS:-$HOME/.local/share/lmgw-corpora/docling-tests/5e600e8d37a11f70b62f795ea1e3ce3a4052e551/tests/data}/pdf/sources"
{
for _ in $(seq "$runs"); do
python3 -I $B "$tag small50" $IN/small50/*.pdf
python3 -I $B "$tag small50 x8 callers" --callers 8 $IN/small50/*.pdf
python3 -I $B "$tag corpus 20 pdfs one req" $C/*.pdf
python3 -I $B "$tag pics100 embedded" $IN/pics100.pdf --field pictures=embedded
python3 -I $J "$tag html200 job" $IN/html200/*
done
} 2>&1 | sed -E 's/ lat_max [0-9.]+//; s/ sum_time_ms [0-9]+//; s/ http 200//; s/ warn \[[^]]*\]//; s/ err \[\]//' | tee target/convert-perf/sec-"$tag".txt
