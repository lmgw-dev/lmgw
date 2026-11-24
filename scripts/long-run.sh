#!/usr/bin/env bash
# Usage: scripts/long-run.sh <log> <command...>   run a long command with its output in <log>, its pid in <log>.pid and its exit status in <log>.exit
#
# Start it as the one background command of a long job (the gate, a release build, the full test
# suite, the Windows check); wait on it with scripts/long-wait.sh <log>. Exits with the command's
# status. Output of an earlier run on the same <log> (and its .pid, .exit, .wait) is replaced.
# Example: scripts/long-run.sh target/ci.log scripts/build-slot.sh bash ci/check.sh
set -euo pipefail
if [ "$#" -lt 2 ]; then sed -n '2p' "$0" >&2; exit 2; fi
log=$1
shift
rm -f "$log.pid" "$log.exit" "$log.wait"
: >"$log"
"$@" >"$log" 2>&1 &
pid=$!
echo "$pid" >"$log.pid"
# A signal to this script goes on to the command; the wait below then reports how it ended.
trap 'kill -TERM "$pid" 2>/dev/null || true' TERM INT
rc=0
wait "$pid" || rc=$?
while kill -0 "$pid" 2>/dev/null; do wait "$pid" || rc=$?; done
echo "$rc" >"$log.exit.tmp"
mv "$log.exit.tmp" "$log.exit"
exit "$rc"
