#!/usr/bin/env bash
# Usage: scripts/flake-hunt.sh suite [RUNS] | copies TEST [COPIES] [ROUNDS]
#
# Reproduce a timing flake under load, without taking the whole desktop:
#
#   suite   RUNS whole-workspace nextest runs (default 8), no retries, with
#           twice as many test threads as CPUs in the set — what a parallel
#           gate does to the suite. Prints each run's failures.
#   copies  COPIES processes of one lmgw-core `it` test at once (default 32),
#           ROUNDS times (default 2), from a copy of the test binary taken
#           first, so an edit and rebuild meanwhile cannot change what runs.
#           A copy still running once it has had as long as nextest gives a
#           test (.config/nextest.toml's slow-timeout) is ended, and fails.
#           Prints "N failed of M"; the failing runs' output stays in
#           target/flake-hunt/<test>/.
#
# CPUS (default: all but the first quarter of the CPUs, left for
# the desktop — e.g. 8-31 on a 32-CPU machine) is the taskset CPU list everything runs
# on; a short list (CPUS=24-27) squeezes the copies onto four cores, which
# starves them far harder. Everything runs under nice. Nothing is left
# running when it returns, Ctrl-C included.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

n=$(nproc)
cpus=${CPUS:-$((n / 4))-$((n - 1))}
out=target/flake-hunt
mkdir -p "$out"
ncpu=$(taskset -c "$cpus" nproc)

case "${1:-}" in
suite)
  runs=${2:-8}
  failed=0
  for i in $(seq "$runs"); do
    log="$out/suite-$i.log"
    if nice -n 10 taskset -c "$cpus" cargo nextest run --workspace --retries 0 \
      --test-threads $((ncpu * 2)) --no-fail-fast >"$log" 2>&1; then
      echo "run $i: passed"
    else
      failed=$((failed + 1))
      echo "run $i: failed ($log)"
      grep -E '^\s+(FAIL|TIMEOUT|SIGSEGV|SIGABRT) ' "$log" | sort -u || true
    fi
  done
  echo "$failed of $runs runs failed"
  ;;
copies)
  test=${2:?copies needs a test name, e.g. vram_admission::audio_sampling::what_a_request_takes_while_it_runs_is_learned}
  copies=${3:-32}
  rounds=${4:-2}
  # How long nextest lets one test run: slow-timeout's period ×
  # terminate-after, in either of its forms; 0, timeout's "never", when it
  # ends no test.
  budget=$(
    python3 - <<'PY'
import re, tomllib
with open(".config/nextest.toml", "rb") as f:
    setting = tomllib.load(f)["profile"]["default"]["slow-timeout"]
if isinstance(setting, str):
    setting = {"period": setting}
period = setting["period"]
if not re.fullmatch(r"(\s*\d+(ms|s|m|h))+\s*", period):
    raise SystemExit(f"slow-timeout's period {period!r}: the units read are ms, s, m and h")
unit = {"ms": 0.001, "s": 1, "m": 60, "h": 3600}
seconds = sum(int(n) * unit[u] for n, u in re.findall(r"(\d+)(ms|s|m|h)", period))
print(f"{seconds * setting['terminate-after']:g}s" if "terminate-after" in setting else 0)
PY
  )
  bin=$(cargo nextest list -p lmgw-core --test it --message-format json 2>/dev/null |
    python3 -c "import json,sys; print(json.load(sys.stdin)['rust-suites']['lmgw-core::it']['binary-path'])")
  frozen="$out/it-$$"
  # The copies run in the background, where a non-interactive shell's Ctrl-C
  # does not reach them: they end with the script, however it ends.
  pids=()
  trap 'kill "${pids[@]}" 2>/dev/null || true; rm -f "$frozen"' EXIT
  trap 'exit 130' INT
  trap 'exit 143' TERM
  cp --reflink=auto "$bin" "$frozen"
  logs="$out/${test//::/.}"
  mkdir -p "$logs"
  failed=0
  for r in $(seq "$rounds"); do
    pids=()
    for c in $(seq "$copies"); do
      (cd crates/lmgw-core && exec nice -n 10 taskset -c "$cpus" timeout "$budget" "../../$frozen" \
        --exact "$test" --test-threads 1) >"$logs/$r-$c.log" 2>&1 &
      pids+=($!)
    done
    for i in "${!pids[@]}"; do
      if wait "${pids[$i]}"; then
        rm -f "$logs/$r-$((i + 1)).log"
      else
        failed=$((failed + 1))
      fi
      # Waited for, so its pid may be another process's by now.
      unset 'pids[i]'
    done
  done
  echo "$test: $failed failed of $((copies * rounds))"
  ;;
*)
  sed -n '2,/^set -euo pipefail$/p' "${BASH_SOURCE[0]}" | sed '$d' | sed 's/^# \{0,1\}//'
  exit 2
  ;;
esac
