#!/usr/bin/env bash
# Usage: [LMGW_WIN_HOST=user@host] scripts/land-check.sh [--no-windows] [log-dir]   the pre-landing check: the full gate and the Windows compile check, run at the same time
#
# Runs `scripts/build-slot.sh bash ci/check.sh` (log: <log-dir>/ci.log) and the Windows clippy check
# (`flock -o /tmp/lmgw-win-check.lock scripts/windows-remote-check.sh`, log: <log-dir>/win-check.log;
# the lock serializes the one checkout on the Windows host) concurrently, waits for both and ends with
# exactly three lines: a gate verdict, a windows verdict and `land-check: exit status N` (0 only
# when the gate passed and Windows passed). <log-dir> defaults to target (or $LAND_CHECK_LOG_DIR).
# The Windows host comes only from LMGW_WIN_HOST. Without it the Windows check is SKIPPED and the
# overall status is 1; --no-windows is the explicit opt-out (verdict SKIPPED, overall may be 0).
# A SIGINT/SIGTERM kills both children's whole process groups. LAND_CHECK_GATE_CMD replaces the
# gate command (run with bash -c; for testing this script).
# Example: scripts/long-run.sh target/land.log scripts/land-check.sh
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

no_windows=0
logdir=${LAND_CHECK_LOG_DIR:-target}
while (($#)); do
  case $1 in
    --no-windows) no_windows=1 ;;
    -h|--help) sed -n '2p' "$0"; exit 0 ;;
    -*) echo "land-check: unknown flag $1" >&2; sed -n '2p' "$0" >&2; exit 2 ;;
    *) logdir=$1 ;;
  esac
  shift
done
mkdir -p "$logdir"
gate_log=$logdir/ci.log
win_log=$logdir/win-check.log

gate_cmd=(scripts/build-slot.sh bash ci/check.sh)
if [[ -n "${LAND_CHECK_GATE_CMD:-}" ]]; then gate_cmd=(bash -c "$LAND_CHECK_GATE_CMD"); fi

# Job control gives each background child its own process group, so one kill reaches its tree.
set -m
gate_pid=
win_pid=
cleanup() {
  local p
  for p in $gate_pid $win_pid; do kill -TERM -- "-$p" 2>/dev/null || true; done
  wait 2>/dev/null || true
  for p in $gate_pid $win_pid; do kill -KILL -- "-$p" 2>/dev/null || true; done
  echo "land-check: interrupted, children killed"
  exit 143
}
trap cleanup TERM INT

"${gate_cmd[@]}" >"$gate_log" 2>&1 &
gate_pid=$!

win_state=run
if ((no_windows)); then
  win_state="skipped (--no-windows)"
elif [[ -z "${LMGW_WIN_HOST:-}" ]]; then
  win_state="skipped (LMGW_WIN_HOST not set)"
else
  flock -o /tmp/lmgw-win-check.lock scripts/windows-remote-check.sh \
    cargo clippy --workspace --all-targets -- -D warnings >"$win_log" 2>&1 &
  win_pid=$!
fi

gate_rc=0
win_rc=0
wait "$gate_pid" || gate_rc=$?
if [[ -n "$win_pid" ]]; then wait "$win_pid" || win_rc=$?; fi

overall=0
if ((gate_rc == 0)); then
  echo "land-check: gate OK"
else
  echo "land-check: gate FAILED (exit $gate_rc, see $gate_log)"
  overall=1
fi
if [[ "$win_state" == run ]]; then
  echo "land-check: windows exit status $win_rc$( ((win_rc)) && echo " (see $win_log)")"
  ((win_rc == 0)) || overall=1
else
  echo "land-check: windows SKIPPED (${win_state#skipped (}"
  [[ "$win_state" == *--no-windows* ]] || overall=1
fi
echo "land-check: exit status $overall"
exit "$overall"
