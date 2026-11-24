#!/usr/bin/env bash
# Usage: scripts/convert-perf/gw.sh start BINARY [MEMMAX] | stop   a release headless example (lmgw-doc-reader beside its examples/ directory) on 127.0.0.1:$LMGW_PERF_PORT (default 8888) in a systemd scope of its own (MemoryMax=MEMMAX when given, e.g. 16G), its data dir on the real disk under target/convert-perf/; writes target/convert-perf/env.sh (TOKEN, GWPID, CG, GW) for the other scripts
set -euo pipefail
cd "$(dirname "$0")/../.."
out=target/convert-perf; mkdir -p "$out"
port="${LMGW_PERF_PORT:-8888}"
stop() {
  if [ -f "$out/env.sh" ]; then . "$out/env.sh"; kill "$GWPID" 2>/dev/null || true; tail --pid="$GWPID" -f /dev/null 2>/dev/null || true; rm -f "$out/env.sh"; fi
  if [ -f "$out/datadir" ]; then d=$(cat "$out/datadir"); case "$d" in */target/convert-perf/run-*) rm -rf "$d";; esac; rm -f "$out/datadir"; fi
}
case "${1:-}" in
  stop) stop; echo stopped;;
  start)
    stop
    bin="$(realpath "$2")"; mem="${3:-}"
    d="$(mktemp -d -p "$PWD/$out" run-XXXX)"; echo "$d" > "$out/datadir"
    prop=(); [ -n "$mem" ] && prop=(-p "MemoryMax=$mem")
    LMGW_DATA_DIR="$d" LMGW_DEV=1 setsid nohup systemd-run --user --scope --quiet "${prop[@]}" \
      --unit="lmgw-convert-perf-$$" "$bin" "127.0.0.1:$port" > "$out/gw.log" 2>&1 < /dev/null &
    disown
    for _ in $(seq 120); do curl -s -o /dev/null "http://127.0.0.1:$port/api/session" && break; sleep 0.25; done
    TOKEN="$(sed 's/\x1b\[[0-9;]*m//g' "$out/gw.log" | grep -o 'lmgw-owner-[0-9a-f]*' | head -1)"
    GWPID=$(ss -ltnp 2>/dev/null | grep ":$port " | grep -o 'pid=[0-9]*' | head -1 | cut -d= -f2)
    CG=/sys/fs/cgroup$(grep '^0::' /proc/"$GWPID"/cgroup | cut -d: -f3)
    printf 'export TOKEN=%s\nexport GWPID=%s\nexport CG=%s\nexport GW=http://127.0.0.1:%s\n' "$TOKEN" "$GWPID" "$CG" "$port" > "$out/env.sh"; chmod 600 "$out/env.sh"
    curl -s -H "Authorization: Bearer $TOKEN" "http://127.0.0.1:$port/api/op/settings_set" -X POST \
      -H 'content-type: application/json' -d '{"max_body_mb":512}' > /dev/null
    echo "started pid $GWPID";;
  *) sed -n '2p' "$0" >&2; exit 2;;
esac
