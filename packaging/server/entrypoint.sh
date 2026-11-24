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

# Docker, where the container's root is the host's root (packaging/server/docker/): lmgw and its
# inner Podman move into a user namespace of their own, with a mount namespace it owns. That is
# the layout the image is built for under a rootless outer Podman, where root is already a user
# namespace's: the inner Podman takes pasta only "for rootless mode or when inside a nested
# userns", and its storage, its runtime and pasta meet the same kernel rules in both. The ids map
# to themselves, so every file on the volumes keeps its owner. That makes the namespace a way to
# run the nested Podman and NOT a security boundary: lmgw has no capability over the host's
# namespaces, but it is still the host's uid 0 wherever the kernel checks the user id, and
# /proc/sys does (a write to the host's kernel.core_pattern from here is permitted, seen
# 2026-11-13). Mapping root to an unprivileged range, as a rootless outer Podman does, is not
# planned: Docker is not a supported way to run lmgw (docs/server.md section 14). USERNS_IDS is
# not the whole range on purpose: Podman reads a map of every id as "not in a user namespace".
#
# Everything the image starts goes through here, so the inner Podman's store only ever sees one
# kind of caller: its feature probes are cached in the runroot (metacopy among them), and a probe
# made outside the namespace is wrong inside it ("conflicting options: userxattr,metacopy=on",
# seen 2026-11-13 after a `podman info` as the container's plain root).
USERNS_IDS=65536
into_userns=()
# The first line of the map: "0 0 4294967295" is the host's own, every id mapped to itself.
read -r map_inside map_outside map_count _ < /proc/self/uid_map || true
if [ "$(id -u)" = 0 ] && [ "${map_inside:-} ${map_outside:-} ${map_count:-}" = "0 0 4294967295" ]; then
    into_userns=(unshare --user "--map-users=0:0:${USERNS_IDS}" "--map-groups=0:0:${USERNS_IDS}" --mount --)
    # A /proc and a /sys with nothing mounted over them, made while this is still the host's
    # root. Inside a user namespace the kernel gives a nested container a /proc or /sys of its
    # own only where the mount namespace already shows a whole one, and the ones Docker hands
    # this container are covered in places: its masked paths, and the file NVIDIA's CDI hook
    # mounts over /proc/driver/nvidia/params. Every nested start then fails with "crun: mount
    # `proc` to `proc`: Operation not permitted" (seen 2026-11-13, kernel 6.8, with
    # nvidia.com/gpu=all passed in). Nothing reads these two; they only have to be there.
    UNMASKED=/run/lmgw-unmasked
    mkdir -p "$UNMASKED/proc" "$UNMASKED/sys"
    [ -e "$UNMASKED/proc/self" ] || mount -t proc -o nosuid,nodev,noexec proc "$UNMASKED/proc" \
        || echo "entrypoint: could not mount a whole /proc at $UNMASKED/proc; nested containers" \
                "may not start (crun: mount proc: Operation not permitted)" >&2
    [ -e "$UNMASKED/sys/kernel" ] || mount -t sysfs -o ro,nosuid,nodev,noexec sysfs "$UNMASKED/sys" \
        || echo "entrypoint: could not mount a whole /sys at $UNMASKED/sys" >&2
    # The probe makes a namespace without a map: with the map options unshare forks a helper
    # that outlives a refused unshare and keeps this command substitution's pipe open (seen
    # 2026-11-13, util-linux 2.42.4: a one-off `docker run` without the options below hung here).
    if ! err="$(unshare --user true 2>&1)"; then
        case "${1:-}" in
            "" | serve | -*)
                echo "entrypoint: this container's root is the host's root (Docker), and a user" \
                     "namespace for lmgw could not be made: ${err}. The inner Podman needs it;" \
                     "start the container as packaging/server/docker/docker-compose.yml does" \
                     "(cap_add SYS_ADMIN; security_opt seccomp=unconfined and apparmor=unconfined)." \
                     "Exiting before lmgw starts." >&2
                exit 1
                ;;
            *)
                echo "entrypoint: no user namespace could be made (${err}); running $1 as the" \
                     "container's plain root. Do not use the inner Podman from here." >&2
                into_userns=()
                ;;
        esac
    fi
fi

case "${1:-}" in
    "" | serve | -*) exec "${into_userns[@]}" /usr/local/bin/lmgw-server "$@" ;;
    *) exec "${into_userns[@]}" "$@" ;;
esac
