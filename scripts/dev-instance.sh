#!/usr/bin/env bash
# Run the headless dev gateway against a scratch data dir, on a dev port.
#
# Usage: scripts/dev-instance.sh [bind_addr]   (default: 127.0.0.1:8899)
#
# If LMGW_DATA_DIR is already set, it is used as-is. Otherwise a fresh
# `mktemp` directory under /tmp is created for this run and printed, so a dev
# instance never has to be started against — or accidentally lands on — the
# real ~/.local/share/lmgw data dir. The headless example itself steers a
# fresh dir's container_prefix off the production default before anything
# reconciles a container (examples/headless.rs, chat-archive-pin-attachments
# review finding 12); this script only guarantees the data dir is scratch.
#
# It also exports LMGW_DEV=1, the explicit dev-instance flag (container-builds
# design §10) — the headless runner is a dev instance regardless: a dev
# instance shares production's podman image store, so it refuses image
# deletion and keep_runs pruning, tags builds into localhost/lmgw-dev-<engine>:…
# (never production's names), and — its data dir being on the /tmp tmpfs —
# defaults the builds dir to ~/.cache/lmgw-dev/builds instead of
# <data_dir>/builds. Each scratch data dir gets its own build instance id, so
# dev instances sharing that builds dir never mix up their runs' files.
set -euo pipefail

ADDR="${1:-127.0.0.1:8899}"

if [ -z "${LMGW_DATA_DIR:-}" ]; then
    LMGW_DATA_DIR="$(mktemp -d -p /tmp lmgw-dev-XXXX)"
    export LMGW_DATA_DIR
fi
echo "LMGW_DATA_DIR=$LMGW_DATA_DIR"
export LMGW_DEV=1

exec cargo run -p lmgw-core --example headless -- "$ADDR"
