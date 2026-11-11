#!/usr/bin/env bash
# Start headless lmgw on 0.0.0.0:8897 (or the address in "$1"); any other command runs instead.
set -euo pipefail
mkdir -p -m 700 "$XDG_RUNTIME_DIR"
# storage.conf names the host store; without its mount (SPIKE_MODE=confined) it stays empty.
mkdir -p /var/lib/hoststore
# cgroup v2 "no internal processes": with a writable /sys/fs/cgroup (podman run
# --systemd=always), move ourselves into a leaf so the inner podman can enable
# controllers for its containers at the root of this cgroup namespace.
if [ -w /sys/fs/cgroup/cgroup.subtree_control ] && [ ! -d /sys/fs/cgroup/init ]; then
    mkdir /sys/fs/cgroup/init
    # Everything in the namespace root, catatonit (PID 1) too; a pid that is gone is fine.
    for pid in $(cat /sys/fs/cgroup/cgroup.procs); do
        echo "$pid" > /sys/fs/cgroup/init/cgroup.procs 2>/dev/null || true
    done
    echo "$(sed 's/\([a-z]\+\)/+\1/g' /sys/fs/cgroup/cgroup.controllers)" > /sys/fs/cgroup/cgroup.subtree_control \
        || echo "entrypoint: could not enable cgroup controllers; set cgroups=disabled for the inner podman" >&2
fi
case "${1:-}" in
    ""|[0-9]*|\[*) exec /usr/local/bin/lmgw-headless "${1:-0.0.0.0:8897}" ;;
    *) exec "$@" ;;
esac
