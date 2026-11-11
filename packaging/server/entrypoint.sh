#!/usr/bin/env bash
# The server image's entry under catatonit: the runtime dir and the cgroup leaf, then lmgw-server.
# Usage: entrypoint.sh [serve | --version | --help]   (any other command runs instead, e.g. bash)
set -euo pipefail
# -m for the runtime dir itself; its parents (/run/user) keep the usual 0755.
# shellcheck disable=SC2174
mkdir -p -m 700 "$XDG_RUNTIME_DIR"

# cgroup v2 "no internal processes": with a writable /sys/fs/cgroup (podman run --systemd=always),
# every process in this cgroup namespace's root moves into a leaf, so the inner Podman can enable
# the controllers at the root for its containers' memory and CPU limits. A container restarted in
# place keeps its cgroup and may still have the leaf from its last run, while its new processes
# start in the root again, so this runs at every start, not only when the leaf is new.
#
# A process can enter the root between reading cgroup.procs and enabling the controllers (a
# `podman exec` joins the root; the unit's health probe is one), and the write then fails. Each
# try reads the root's processes again, moves them and writes again. The bound is a real one,
# stated in every line it logs: CGROUP_TRIES tries 0.1 s apart (2 s). Expected, not yet measured:
# a racing exec is gone within one or two tries, so a write that still fails after all of them
# has another cause. The entrypoint then exits non-zero before lmgw starts, and the unit's
# Restart= and the journal show the line.
CGROUP_TRIES=20
ROOT=/sys/fs/cgroup
LEAF="$ROOT/init"
if [ -w "$ROOT/cgroup.subtree_control" ]; then
    mkdir -p "$LEAF"
    controllers=()
    read -r -a controllers < "$ROOT/cgroup.controllers" || true
    if [ "${#controllers[@]}" -gt 0 ]; then
        enable="$(printf '+%s ' "${controllers[@]}")"
        enable="${enable% }"
        try=1
        while :; do
            mapfile -t pids < "$ROOT/cgroup.procs"
            for pid in "${pids[@]}"; do
                # A pid that is gone by now is fine.
                echo "$pid" > "$LEAF/cgroup.procs" 2>/dev/null || true
            done
            if err="$( { printf '%s' "$enable" > "$ROOT/cgroup.subtree_control"; } 2>&1 )"; then
                echo "entrypoint: cgroup leaf ${LEAF}; controllers enabled at the root:" \
                     "${controllers[*]} (try ${try} of ${CGROUP_TRIES})" >&2
                break
            fi
            if [ "$try" -ge "$CGROUP_TRIES" ]; then
                echo "entrypoint: could not enable the cgroup controllers (${controllers[*]}) in" \
                     "${CGROUP_TRIES} tries 0.1 s apart, the bound for a racing podman exec to leave" \
                     "the root: ${err}. The inner Podman's memory and CPU limits need them; exiting" \
                     "before lmgw starts, so the unit restarts and the journal keeps this line." >&2
                exit 1
            fi
            echo "entrypoint: enabling the cgroup controllers failed (try ${try} of ${CGROUP_TRIES}):" \
                 "${err}; moving the root's processes into the leaf again" >&2
            try=$((try + 1))
            sleep 0.1
        done
    fi
fi

case "${1:-}" in
    "" | serve | -*) exec /usr/local/bin/lmgw-server "$@" ;;
    *) exec "$@" ;;
esac
