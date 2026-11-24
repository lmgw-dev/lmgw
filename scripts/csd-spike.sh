#!/usr/bin/env bash
# Usage: scripts/csd-spike.sh [--demo]   build the shell, run it on a copy of the installed app's data; --demo shows the design sample instead of the dashboard
#
# The shell draws its own window frame by default now (LMGW_WINDOW=csd below is redundant but harmless);
# this script's value is the separate data copy.
# The copy lives in target/csd-spike/data (made once with scripts/dev-copy.sh copy, bound to
# 127.0.0.1:8871) and is set apart before its first boot: container_prefix lmgw-csd, the GPU hold on
# (no local model starts; both set by dev-copy.sh's variables), the device listener off. It holds
# the installed app's provider keys in plain text: remove target/csd-spike when done. Ctrl-C quits the shell (its teardown runs).
set -euo pipefail
cd "$(dirname "$0")/.."

demo=0
case "${1:-}" in
    "") ;;
    --demo) demo=1 ;;
    *) sed -n '2p' "$0" >&2; exit 2 ;;
esac

dir="$(realpath -m target/csd-spike/data)"
if [ ! -e "$dir/lmgw.sqlite" ]; then
    LMGW_DEV_COPY_PREFIX=lmgw-csd LMGW_DEV_COPY_HOLD=1 scripts/dev-copy.sh copy "$dir" 127.0.0.1:8871
    python3 - "$dir/lmgw.sqlite" <<'EOF'
import json, sqlite3, sys
c = sqlite3.connect(sys.argv[1])
s = json.loads(c.execute("SELECT value FROM settings WHERE key = 'settings'").fetchone()[0])
s.setdefault("devices", {})["listener"] = False
c.execute("UPDATE settings SET value = ? WHERE key = 'settings'", (json.dumps(s),))
c.commit()
print(f"set apart: prefix {s['container_prefix']}, hold {s['hold']['active']}, "
      f"device listener {s['devices']['listener']}")
EOF
fi

# The shell (cargo) and the dashboard it serves from crates/lmgw-ui/dist (trunk): a debug
# shell reads that folder from disk, so a stale dist shows an old UI. Both wait for the
# shared build lock, which a running gate can hold for minutes.
echo "csd-spike: building the shell and the dashboard (waits for the build lock) …" >&2
flock -o /tmp/lmgw-cargo-build.lock cargo build --workspace -j 8
(cd crates/lmgw-ui && flock -o /tmp/lmgw-cargo-build.lock trunk build)
echo "csd-spike: starting; the window shows once the dashboard has loaded (a debug build takes a few seconds)" >&2

export LMGW_DATA_DIR="$dir" LMGW_DEV=1 LMGW_WINDOW=csd
if [ "$demo" = 1 ]; then export LMGW_WINDOW_DEMO=1; fi
exec target/debug/lmgw
