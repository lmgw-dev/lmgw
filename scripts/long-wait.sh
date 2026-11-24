#!/usr/bin/env bash
# Usage: scripts/long-wait.sh <log>   block until the scripts/long-run.sh job on <log> ends or the window closes; exit 0 = ended, 3 = still running
#
# One call blocks for up to WINDOW_S seconds and then says what happened since the previous call
# on the same log (kept in <log>.wait): time since start, log growth, CPU seconds of the command's
# whole process tree, the last log lines, and whether a build-slot holder (scripts/build-slot.sh)
# is making progress while the command queues. The last line is a one-line verdict.
set -uo pipefail
if [ "$#" -ne 1 ]; then sed -n '2p' "$0" >&2; exit 2; fi
log=$1
# The longest a caller's single blocking call may take: a 10-minute (600 s) tool limit; 570 s
# leaves room for the final report. LONG_WAIT_S overrides it for tests.
WINDOW_S=${LONG_WAIT_S:-570}
STEP_S=5
tick=$(getconf CLK_TCK)

[ -e "$log.pid" ] || [ -e "$log.exit" ] || { echo "long-wait: no $log.pid; was the job started with scripts/long-run.sh?" >&2; exit 2; }
pid=$(cat "$log.pid" 2>/dev/null || true)
start=$(stat -c %Y "$log.pid" 2>/dev/null || date +%s)

# "<cpu ticks> <pid>..." of a process and all its descendants. The stat files go through cat:
# gawk stops with a fatal error on an input file that is gone by the time it opens it (a
# short-lived child), which read as "the command is gone" while it ran on.
tree() {
    cat /proc/[0-9]*/stat 2>/dev/null | awk -v root="$1" '
    { match($0, /\)[^)]*$/); split(substr($0, RSTART + 2), f, " ")
      pid = $1 + 0; ppid[pid] = f[2]; t[pid] = f[12] + f[13] }
    END { seen[root] = 1
          do { more = 0
               for (p in ppid) if (!(p in seen) && (ppid[p] in seen)) { seen[p] = 1; more = 1 } } while (more)
          s = 0; out = ""
          for (p in seen) if (p in t) { s += t[p]; out = out " " p }
          print s out }'
}
size() { stat -c %s "$log" 2>/dev/null || echo 0; }
fmt() { local s=$1; if [ "$s" -ge 3600 ]; then printf '%dh%02dm%02ds' $((s/3600)) $((s%3600/60)) $((s%60)); else printf '%dm%02ds' $((s/60)) $((s%60)); fi; }
cmdline() { tr '\0' ' ' <"/proc/$1/cmdline" 2>/dev/null; }

# Pids holding a build slot (flock locks of scripts/build-slot.sh).
slot_holders() {
    local lock ino
    for lock in /tmp/lmgw-cargo-build*.lock; do
        [ -e "$lock" ] || continue
        ino=$(stat -c %i "$lock")
        awk -v ino="$ino" '$2 == "FLOCK" { split($6, a, ":"); if (a[3] == ino) print $5 }' /proc/locks
    done
}

# Is the tree waiting in scripts/build-slot.sh: a build-slot.sh process none of whose children works?
queued() {
    local p c kids busy
    for p in $1; do
        case $(cmdline "$p") in *build-slot.sh*) ;; *) continue ;; esac
        busy=0
        kids=$(ps -o pid= --ppid "$p" 2>/dev/null)
        for c in $kids; do
            case $(cat "/proc/$c/comm" 2>/dev/null) in
                sleep) ;;
                flock) [ -n "$(ps -o pid= --ppid "$c" 2>/dev/null)" ] && busy=1 ;;
                *) busy=1 ;;
            esac
        done
        [ "$busy" -eq 0 ] && return 0
    done
    return 1
}

# Previous call's figures: epoch size cpu_ticks (a first call measures from the start).
prev_t=$start prev_size=0 prev_cpu=0
[ -r "$log.wait" ] && read -r prev_t prev_size prev_cpu <"$log.wait"

declare -A h0
for h in $(slot_holders); do set -- $(tree "$h"); h0[$h]=${1:-0}; done

deadline=$(( $(date +%s) + WINDOW_S ))
cpu_moved=0 last_cpu=$prev_cpu gone=0 ended=0
while :; do
    if [ -e "$log.exit" ]; then ended=1; break; fi
    set -- $(tree "${pid:-0}")
    cur=${1:-0}; [ "$#" -gt 0 ] && shift
    if [ "$#" -eq 0 ]; then gone=$((gone + 1)); else gone=0; fi
    [ "$cur" -gt "$last_cpu" ] && cpu_moved=$((cpu_moved + cur - last_cpu))
    last_cpu=$cur
    # The command is gone but no status was written (long-run.sh itself was killed); give it two steps.
    if [ "$gone" -ge 2 ] && [ ! -e "$log.exit" ]; then ended=2; break; fi
    left=$(( deadline - $(date +%s) ))
    [ "$left" -le 0 ] && break
    sleep $(( left < STEP_S ? left : STEP_S ))
done

now=$(date +%s)
cur_size=$(size)
grew=$(( cur_size - prev_size ))
echo "elapsed since start: $(fmt $((now - start))); this call covered $(fmt $((now - prev_t)))"
if [ "$ended" -eq 1 ]; then
    rc=$(cat "$log.exit")
    echo "finished: exit status $rc"
    echo "log: $cur_size bytes (grew $grew); last 15 lines:"
    tail -n 15 "$log"
    rm -f "$log.wait"
    exit 0
elif [ "$ended" -eq 2 ]; then
    echo "finished without an exit status: pid $pid is gone and $log.exit was never written (long-run.sh was probably killed)"
    echo "log: $cur_size bytes (grew $grew); last 15 lines:"
    tail -n 15 "$log"
    rm -f "$log.wait"
    exit 0
fi

echo "log grew $grew bytes (now $cur_size); command tree used $(awk -v t="$cpu_moved" -v c="$tick" 'BEGIN{printf "%.1f", t/c}') CPU seconds"
echo "last 3 log lines:"
tail -n 3 "$log" | sed 's/^/  | /'

set -- $(tree "$pid"); [ "$#" -gt 0 ] && shift
holder_moved=0 verdict_slot=0
if queued "$*"; then
    verdict_slot=1
    echo "waiting for a build slot"
    holders=$(slot_holders | sort -un)
    [ -n "$holders" ] || echo "  (no slot holder found)"
    for h in $holders; do
        set -- $(tree "$h"); c1=${1:-0}
        kid=$(ps -o args= --ppid "$h" 2>/dev/null | head -n 1)
        [ -n "$kid" ] || kid=$(ps -o args= -p "$h" 2>/dev/null)
        if [ -z "${h0[$h]+x}" ] || [ "${h0[$h]}" != "$c1" ]; then mv_s="CPU moved during this call"; holder_moved=1; else mv_s="CPU did NOT move during this call"; fi
        echo "  slot holder pid $h: $kid ($mv_s)"
    done
fi

echo "$now $cur_size $last_cpu" >"$log.wait"
if [ "$verdict_slot" -eq 1 ]; then
    if [ "$holder_moved" -eq 1 ] || [ "$grew" -gt 0 ]; then echo "running: waiting for a build slot, holder making progress"; exit 3; fi
elif [ "$grew" -gt 0 ] || [ "$cpu_moved" -gt 0 ]; then
    echo "running: making progress"; exit 3
fi
echo "running: NO progress during this call (no log growth, no CPU, no slot holder moving) — look at the process tree"
exit 3
