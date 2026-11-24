#!/usr/bin/env bash
# Usage: scripts/convert-perf/mem.sh TAG   large conversions against the instance gw.sh started, then its idle RSS, malloc's own count (gdb: in use / free in its arenas) and the RSS after malloc_trim(0); rows in target/convert-perf/mem-TAG.txt. -e is left out on purpose: the rows after a failed one still run
set -uo pipefail
cd "$(dirname "$0")/../.."; . target/convert-perf/env.sh; export TOKEN GWPID CG GW
B=scripts/convert-perf/bench.py; tag="$1"; out="$PWD/target/convert-perf"
IN="${PERF_IN:-$HOME/.local/share/lmgw-corpora/convert-perf-2026-11-19}"
rss() { awk '/VmRSS/{print int($2/1024)}' /proc/"$GWPID"/status; }
{
echo "$tag fresh rss $(rss) MiB"
python3 -I $B "$tag text5k" $IN/text5k.pdf
python3 -I $B "$tag pics100 embedded" --field pictures=embedded $IN/pics100.pdf
python3 -I $B "$tag scan20" $IN/scan20.pdf
python3 -I $B "$tag big200 ocr off" --field ocr=off $IN/big200.pdf
python3 -I $B "$tag big.xlsx json" --repeat 2 --field output=json $IN/big.xlsx
python3 -I $B "$tag text5 after" --repeat 2 $IN/text5.pdf
sleep 5
echo "$tag idle rss $(rss) MiB"
gdb -p "$GWPID" -batch -ex "call (int)((int(*)(int,void*))malloc_info)(0, ((void*(*)(const char*,const char*))fopen)(\"$out/mi-$tag.xml\",\"w\"))" -ex 'call (int)((int(*)(void*))fflush)(0)' > /dev/null 2>&1
python3 -I - "$out/mi-$tag.xml" <<'PY'
import re, sys
x = open(sys.argv[1]).read(); tail = x[x.rfind('</heap>'):]
free = sum(int(s) for _, _, s in re.findall(r'<total type="(\w+)" count="(\d+)" size="(\d+)"/>', tail))
cur = int(re.search(r'<system type="current" size="(\d+)"/>', tail).group(1))
print(f"malloc: arenas hold {cur >> 20} MiB, {free >> 20} MiB of it free, {(cur - free) >> 20} MiB in use")
PY
gdb -p "$GWPID" -batch -ex 'call (int)((int(*)(unsigned long))malloc_trim)(0)' > /dev/null 2>&1
echo "$tag after malloc_trim(0) rss $(rss) MiB"
} 2>&1 | sed -E 's/ lat_max [0-9.]+//; s/ sum_time_ms [0-9]+//; s/ http 200//; s/ warn \[[^]]*\]//; s/ err \[\]//' | tee "$out/mem-$tag.txt"
