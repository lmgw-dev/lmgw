#!/usr/bin/env bash
# Usage: spike/headless-image/run.sh build|start|stop|shell [cmd...]  (from the worktree root)
#
# build: copy this dir's files and a stripped copy of the headless example into a
#        small build context under target/spike/ctx and build the image.
# start: run headless lmgw in the outer container (spike-hl-outer) on 127.0.0.1:8897.
# shell: a throwaway outer container with the same flags, running "cmd..." instead.
# stop:  stop and remove spike-hl-outer.
set -euo pipefail

ROOT="$(git rev-parse --show-toplevel)"
HERE="$ROOT/spike/headless-image"
SPIKE="$ROOT/target/spike"
IMAGE=localhost/spike-lmgw-headless:dev
NAME=spike-hl-outer
PORT=8897
# The host user's rootless store, read-only, as the inner podman's additional image store.
HOST_STORE="$(podman info --format '{{.Store.GraphRoot}}')"

# SPIKE_MODE=shared (default): the host's image store read-only, which needs label=disable.
# SPIKE_MODE=confined: SELinux-confined (container_engine_t), images only in the inner
# store; load one there first (REPORT.md, question 3). Each mode has its own store volume:
# files a label=disable run copies up from the host store keep container_ro_file_t, and a
# confined podman then fails on them (REPORT.md, question 4).
SPIKE_MODE="${SPIKE_MODE:-shared}"
STORE=spike-hl-store
[ "$SPIKE_MODE" = confined ] && STORE=spike-hl-store-confined

flags() {
    # Minimal set found by the spike (REPORT.md, question 3).
    printf '%s\n' \
        --device nvidia.com/gpu=all \
        --device /dev/net/tun \
        --cap-add SYS_ADMIN,NET_ADMIN \
        --systemd=always \
        --stop-signal SIGTERM \
        -v "$STORE:/var/lib/containers" \
        -v "$SPIKE/data:/data:Z"
    if [ "$SPIKE_MODE" = confined ]; then
        printf '%s\n' --security-opt label=type:container_engine_t
    else
        printf '%s\n' \
            --security-opt label=disable \
            -v "$HOST_STORE/overlay:/var/lib/hoststore/overlay:ro" \
            -v "$HOST_STORE/overlay-images:/var/lib/hoststore/overlay-images:ro" \
            -v "$HOST_STORE/overlay-layers:/var/lib/hoststore/overlay-layers:ro"
    fi
}

case "${1:-}" in
build)
    mkdir -p "$SPIKE/ctx"
    cp "$HERE"/{Containerfile,containers.conf,storage.conf,entrypoint.sh,nvidia-container-toolkit.repo} "$SPIKE/ctx/"
    objcopy --strip-debug "$ROOT/target/debug/examples/headless" "$SPIKE/ctx/headless"
    podman build -t "$IMAGE" "$SPIKE/ctx"
    ;;
start)
    mkdir -p "$SPIKE/data"
    mapfile -t F < <(flags)
    podman run -d --name "$NAME" "${F[@]}" -p "127.0.0.1:$PORT:$PORT" "$IMAGE" "0.0.0.0:$PORT"
    ;;
shell)
    shift
    mkdir -p "$SPIKE/data"
    mapfile -t F < <(flags)
    podman run --rm --name spike-hl-shell "${F[@]}" "$IMAGE" "${@:-bash}"
    ;;
stop)
    podman stop -t 20 "$NAME" || true
    podman rm "$NAME" || true
    ;;
*)
    sed -n 2,8p "$0"
    exit 2
    ;;
esac
