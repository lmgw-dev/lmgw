#!/usr/bin/env bash
# Every route of the dashboard as a screenshot, plus the layout matrix's verdict, in one command.
#
# Usage: scripts/ui-shots.sh <out-dir> [--routes all|extra|full|/a,/b] [--runs dark-1152,dark-2048,dark-800,light-1152] [--keep]
#
# Makes a dev copy of production's database (scripts/dev-copy.sh: read-only backup, GPU hold on, no model
# can load, nothing opens on the desktop: headless Chrome only), starts it through the build slot, runs
# scripts/ui-matrix.py once per run at 125 % scale with the app window's classic
# scrollbars (--tauri), stops the instance and deletes the copy on every exit path. The copy holds
# production's provider keys in plain text, so it never stays unless --keep.
#
# --routes   full (the default here) = ui-matrix's `all` plus `extra` (settings categories, one editor per
#            model class, upstream/MCP/key/traffic/benchmark/build details, chat profiles, a text chat
#            thread, the other labs ...). {name} routes get one real id from the copy through the admin API
#            (ui-matrix.py FILLERS); a kind the data has no row of is skipped with a printed note, so ids
#            are never hard-coded. Also: all | extra | a comma list of paths.
# --runs     dark-1152 (1152x768), dark-2048 (2048x1152), dark-800 (800x600), light-1152 (1152x768, the
#            stored theme set to light by an init script; the run FAILs when the page is not light).
#            CSS size is window / 1.25, as ui-matrix's --dsf 1.25 does. A 2048x1152 run takes about 12 min
#            and the whole default set about 25 min: start it through scripts/long-run.sh.
# --keep     leave the instance and the copy running; prints the dir, the address and how to stop it.
#
# Output: <out-dir>/<run>/<slug>.png (slug = route with / ? = as _, "overview" for /), <out-dir>/<run>/matrix.json
# (every measurement) and matrix.log (ui-matrix's output). One summary line per run, then its FAIL lines.
# Exit status: 0 only when every run had 0 FAIL.
#
# The copy lives in target/ui-shots-<hash of the out dir>, with container prefix lmgw-shots-<hash>, so two
# runs with different out dirs can go at once; it refuses to start when that dir exists (the same out dir
# is already running, or a --keep copy is still there: stop it first). The port is the first free one of
# 127.0.0.1:8920-8959 (picked and started under a lock, so two starts do not pick the same one).
# Never touches production's gateway or containers. Chrome profiles under /tmp (RAM) are removed by
# ui-matrix itself, also when it is interrupted here.
set -euo pipefail
cd "$(dirname "$0")/.."
root="$PWD"

usage() { sed -n '2,3p' "$0" >&2; exit 2; }

out="" routes="full" runs="dark-1152,dark-2048,dark-800,light-1152" keep=0
while [ $# -gt 0 ]; do
    case "$1" in
        --routes) routes="${2:?--routes needs a value}"; shift 2 ;;
        --runs) runs="${2:?--runs needs a value}"; shift 2 ;;
        --keep) keep=1; shift ;;
        -h|--help) usage ;;
        -*) echo "unknown option $1" >&2; usage ;;
        *) [ -z "$out" ] || usage; out="$1"; shift ;;
    esac
done
[ -n "$out" ] || usage
out="$(realpath -m "$out")"

# run name -> "size [light]"
run_spec() {
    case "$1" in
        dark-1152) echo "1152x768" ;;
        dark-2048) echo "2048x1152" ;;
        dark-800) echo "800x600" ;;
        light-1152) echo "1152x768 light" ;;
        *) echo "unknown run '$1' (dark-1152, dark-2048, dark-800, light-1152)" >&2; return 1 ;;
    esac
}
IFS=, read -r -a run_list <<< "$runs"
[ "${#run_list[@]}" -gt 0 ] || usage
for r in "${run_list[@]}"; do run_spec "$r" > /dev/null || exit 2; done

hash="$(printf '%s' "$out" | cksum | cut -d' ' -f1)"
dir="$root/target/ui-shots-$hash"
prefix="lmgw-shots-$hash"
theme_js="$dir.light.js"
if [ -e "$dir" ] || [ -e "$dir.pid" ] || [ -e "$dir.token" ]; then
    echo "refusing: $dir (or its .pid/.token) exists: this out dir is already running, or a --keep copy is still up" >&2
    echo "  stop it: scripts/dev-copy.sh stop $dir; then delete the copy ($dir and $dir.{log,token,pid,build.log,light.js}): it holds provider keys" >&2
    exit 1
fi

child=""
started=0
cleanup() {
    local rc=$?
    trap - EXIT INT TERM
    if [ -n "$child" ] && kill -0 "$child" 2>/dev/null; then
        kill -TERM "$child" 2>/dev/null || true   # ui-matrix unwinds on it and removes its Chrome profile
        wait "$child" 2>/dev/null || true
    fi
    if [ "$keep" = 1 ] && [ "$started" = 1 ] && [ "$rc" -eq 0 ]; then
        echo "kept: dir $dir, address http://$addr, token file $dir.token"
        echo "stop and delete: scripts/dev-copy.sh stop $dir && rm -rf $dir $dir.log $dir.token $dir.pid $dir.build.log $dir.light.js"
        exit "$rc"
    fi
    if [ -e "$dir.pid" ]; then
        "$root/scripts/dev-copy.sh" stop "$dir" > /dev/null 2>&1 || echo "WARNING: stopping the instance failed: kill pid $(cat "$dir.pid") yourself" >&2
    fi
    case "$dir" in
        "$root"/target/ui-shots-*) rm -rf -- "$dir" "$dir.log" "$dir.token" "$dir.pid" "$dir.build.log" "$theme_js" ;;
    esac
    if command -v podman > /dev/null; then
        left="$(podman ps -a --format '{{.Names}}' | grep -- "$prefix" || true)"
        [ -z "$left" ] || echo "WARNING: containers with prefix $prefix left: $left" >&2
    fi
    exit "$rc"
}
addr="?"
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# A command in the background, waited for: a signal then reaches the trap at once instead of after the command.
bg() { "$@" & child=$!; local rc=0; wait "$child" || rc=$?; child=""; return "$rc"; }

mkdir -p "$out" "$root/target"
exec 9> "$root/target/.ui-shots-port.lock"
flock 9
addr=""
for port in $(seq 8920 8959); do
    if ! ss -ltn | awk '{print $4}' | grep -q ":$port\$"; then addr="127.0.0.1:$port"; break; fi
done
[ -n "$addr" ] || { echo "no free port in 127.0.0.1:8920-8959" >&2; exit 1; }

bg env LMGW_DEV_COPY_PREFIX="$prefix" LMGW_DEV_COPY_HOLD=1 "$root/scripts/dev-copy.sh" copy "$dir" "$addr"
started=1
bg "$root/scripts/build-slot.sh" "$root/scripts/dev-copy.sh" start "$dir" "$addr"
flock -u 9
exec 9>&-
echo "ui-shots: instance up on http://$addr (copy $dir, containers $prefix*, GPU hold on)"

printf "%s\n" "localStorage.setItem('lmgw-theme', 'light');" > "$theme_js"

status=0
for r in "${run_list[@]}"; do
    read -r size light <<< "$(run_spec "$r")"
    rdir="$out/$r"
    mkdir -p "$rdir"
    rm -f -- "$rdir"/*.png "$rdir/matrix.json" "$rdir/matrix.log"
    args=(--base "http://$addr" --token-file "$dir.token" --routes "$routes" --sizes "$size" --dsf 1.25
          --tauri --shots "$rdir" --json "$rdir/matrix.json" --podman-guard "$prefix" --wait 25)
    if [ -n "$light" ]; then args+=(--init-script "$theme_js" --expect-theme light); else args+=(--expect-theme dark); fi
    echo "ui-shots: run $r ($size at 125%) ..."
    rc=0
    bg python3 scripts/ui-matrix.py "${args[@]}" > "$rdir/matrix.log" 2>&1 || rc=$?
    for f in "$rdir"/*-"$size".png; do
        [ -e "$f" ] || continue
        mv -f -- "$f" "${f%-"$size".png}.png"
    done
    summary="$(grep -E '^[0-9]+ measurements, [0-9]+ FAIL' "$rdir/matrix.log" | tail -1 | sed 's/ (shots in .*//' || true)"
    if [ -z "$summary" ]; then
        echo "$r: no result (ui-matrix exit $rc); last lines of $rdir/matrix.log:"
        tail -5 "$rdir/matrix.log" | sed 's/^/  /'
        status=1
        continue
    fi
    echo "$r: $summary"
    awk '/^[0-9]+ measurements/{next} / (FAIL|ERROR) /{print "  " $0; p=1; next} /^    /{if(p)print "  " $0; next} {p=0}' "$rdir/matrix.log"
    grep -E '^skip ' "$rdir/matrix.log" | sed 's/^/  /' || true
    case "$summary" in *" 0 FAIL"*) ;; *) status=1 ;; esac
    case "$rc" in 0|1) ;; *) echo "$r: ui-matrix exit status $rc"; status=1 ;; esac
done
exit "$status"
