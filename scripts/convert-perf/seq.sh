#!/usr/bin/env bash
# Usage: scripts/convert-perf/seq.sh TAG   the performance round's fixed sequence (design §14) against the instance gw.sh started; the stress files in $PERF_IN (default ~/.local/share/lmgw-corpora/convert-perf-2026-11-19), the corpus in $CORPUS; rows in target/convert-perf/seq-TAG.txt, `table.py` puts runs side by side. -e is left out on purpose: the rows after a failed one still run
set -uo pipefail
cd "$(dirname "$0")/../.."; . target/convert-perf/env.sh; export TOKEN GWPID CG GW
B=scripts/convert-perf/bench.py; tag="$1"
IN="${PERF_IN:-$HOME/.local/share/lmgw-corpora/convert-perf-2026-11-19}"
C="${CORPUS:-$HOME/.local/share/lmgw-corpora/docling-tests/5e600e8d37a11f70b62f795ea1e3ce3a4052e551/tests/data}/pdf/sources"
{
python3 -I $B "$tag text5" --repeat 2 $IN/text5.pdf
python3 -I $B "$tag text50" $IN/text50.pdf
python3 -I $B "$tag small50" --repeat 2 $IN/small50/*.pdf
python3 -I $B "$tag small50 x8 callers" --callers 8 $IN/small50/*.pdf
python3 -I $B "$tag corpus 20 pdfs one req" $C/*.pdf
python3 -I $B "$tag text500" $IN/text500.pdf
python3 -I $B "$tag text500 text" --field output=text $IN/text500.pdf
python3 -I $B "$tag text5k" $IN/text5k.pdf
python3 -I $B "$tag dense200" $IN/dense200.pdf
python3 -I $B "$tag bigtable" $IN/bigtable.pdf
python3 -I $B "$tag pics100 md" $IN/pics100.pdf
python3 -I $B "$tag pics100 embedded" $IN/pics100.pdf --field pictures=embedded
python3 -I $B "$tag scan20" $IN/scan20.pdf
python3 -I $B "$tag big200 ocr off" --field ocr=off $IN/big200.pdf
python3 -I $B "$tag html200" $IN/html200/*
python3 -I $B "$tag big.xlsx json (inflate)" --repeat 2 --field output=json $IN/big.xlsx
python3 -I $B "$tag text5 @high rss" --repeat 2 $IN/text5.pdf
python3 -I $B "$tag small50 @high rss" --repeat 2 $IN/small50/*.pdf
python3 -I $B "$tag scan20 @high rss" $IN/scan20.pdf
} 2>&1 | sed -E 's/ lat_max [0-9.]+//; s/ sum_time_ms [0-9]+//; s/ http 200//; s/ warn \[[^]]*\]//; s/ err \[\]//' | tee target/convert-perf/seq-"$tag".txt
