#!/usr/bin/env bash
# Usage: scripts/convert-perf/memrow.sh BINARY TAG [MEMMAX [RUNS]]   one build's rows of the PDF charge block (design §14, 2026-11-20): gw.sh starts BINARY (MemoryMax=MEMMAX when given, e.g. 8G; port $LMGW_PERF_PORT), sec.sh TAG RUNS (default 3), then convert-measure.py over the corpus' pdf folder (summary in target/convert-perf/measure-TAG.json), and the instance is stopped
set -euo pipefail
cd "$(dirname "$0")/../.."
bin="$1"; tag="$2"; mem="${3:-}"; runs="${4:-3}"
C="${CORPUS:-$HOME/.local/share/lmgw-corpora/docling-tests/5e600e8d37a11f70b62f795ea1e3ce3a4052e551/tests/data}"
trap 'scripts/convert-perf/gw.sh stop' EXIT
scripts/convert-perf/gw.sh start "$bin" $mem
. target/convert-perf/env.sh
scripts/convert-perf/sec.sh "$tag" "$runs"
start=$(date +%s.%N)
LMGW_KEY="$TOKEN" python3 -I scripts/convert-measure.py --corpus "$C" --lmgw "$GW" --formats pdf \
  --out "target/convert-perf/measure-$tag.json" > "target/convert-perf/measure-$tag.log" 2>&1
echo "$tag corpus pdf folder (convert-measure, one file at a time): $(echo "$(date +%s.%N) - $start" | bc) s"
