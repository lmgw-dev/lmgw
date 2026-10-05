#!/usr/bin/env bash
# A headless dev gateway on a copy of production's database, detached — the "dev copy" recipe.
#
# Usage: scripts/dev-copy.sh copy  [dir] [addr]   fresh copy of production's lmgw.sqlite into dir
#        scripts/dev-copy.sh start [dir] [addr]   build the headless runner and start it on dir, detached
#        scripts/dev-copy.sh stop  [dir]          graceful stop (SIGINT, as Ctrl-C), waits for the exit
#
# dir defaults to target/dev-copy, addr to 127.0.0.1:8899. Never /tmp: it is RAM.
#
# copy reads production's database with `sqlite3 -readonly … .backup` and nothing else of it, then
# sets the copy apart before anything boots on it: container_prefix lmgw-dev, bind_addr addr, every
# agent disabled, and legacy_container_names emptied — a boot sweeps those names, and they are
# production's (examples/headless.rs only steers the prefix of a dir still on the default one).
# Hugging Face downloads production had queued or in flight are marked failed in the copy (with a
# note saying why): a boot resumes those, and the copy's models dirs are production's, so it would
# write the same <models_dir>/<file>.part production resumes into. The gateway itself refuses every
# other write a dev instance would make into a models dir outside its data dir (downloads, deletes,
# voice-library clips); copy prints the two ways that stay open. It refuses to overwrite a copy,
# and refuses production's own dir under every name it has.
#
# Production's dir is LMGW_PROD_DIR, else $XDG_DATA_HOME/lmgw, else ~/.local/share/lmgw (the app's
# own rule); dir may be none of those three, so a moved XDG_DATA_HOME or LMGW_PROD_DIR cannot make
# the real dir look like a copy.
#
# start writes <dir>.pid, <dir>.log and <dir>.token (0600: the owner bearer for /api/op/*, from
# the log's login line) next to dir, and waits up to 120 s for /v1/models. The copy's
# owner:self-admin key is production's; /mcp/admin takes it as the bearer.
set -euo pipefail
cd "$(dirname "$0")/.."

cmd="${1:-}"
dir="$(realpath -m "${2:-target/dev-copy}")"
addr="${3:-127.0.0.1:8899}"
home_prod="$(realpath -m "$HOME/.local/share/lmgw")"
xdg_prod=""
[ -n "${XDG_DATA_HOME:-}" ] && xdg_prod="$(realpath -m "$XDG_DATA_HOME/lmgw")"
prod="$(realpath -m "${LMGW_PROD_DIR:-${xdg_prod:-$home_prod}}")"

for p in "$prod" "$home_prod" "$xdg_prod"; do
    if [ -n "$p" ] && [ "$dir" = "$p" ]; then
        echo "refusing: $dir is production's data dir" >&2
        exit 1
    fi
done

alive() { [ -f "$dir.pid" ] && kill -0 "$(cat "$dir.pid")" 2>/dev/null; }

case "$cmd" in
copy)
    if [ -e "$dir/lmgw.sqlite" ]; then
        echo "refusing: $dir/lmgw.sqlite exists (remove it for a fresh copy)" >&2
        exit 1
    fi
    mkdir -p "$dir"
    sqlite3 -readonly "$prod/lmgw.sqlite" ".backup '$dir/lmgw.sqlite'"
    python3 - "$dir/lmgw.sqlite" "$addr" <<'EOF'
import json, sqlite3, sys
db, addr = sys.argv[1], sys.argv[2]
NOTE = ("not resumed in this dev copy (scripts/dev-copy.sh copy): production had it queued or "
        "in flight, and the copy's models dirs are production's; retry it in production")
c = sqlite3.connect(db)
s = json.loads(c.execute("SELECT value FROM settings WHERE key = 'settings'").fetchone()[0])
s.update(container_prefix="lmgw-dev", bind_addr=addr, legacy_container_names=[])
c.execute("UPDATE settings SET value = ? WHERE key = 'settings'", (json.dumps(s),))
c.execute("UPDATE agents SET enabled = 0")
stopped = c.execute("UPDATE hf_models SET status = 'failed', error = ? "
                    "WHERE status IN ('queued', 'downloading')", (NOTE,)).rowcount
c.commit()
s = json.loads(c.execute("SELECT value FROM settings WHERE key = 'settings'").fetchone()[0])
on = c.execute("SELECT count(*) FROM agents WHERE enabled = 1").fetchone()[0]
live = c.execute("SELECT count(*) FROM hf_models "
                 "WHERE status IN ('queued', 'downloading')").fetchone()[0]
print(f"copied: prefix {s['container_prefix']}, bind {s['bind_addr']}, "
      f"legacy names {s['legacy_container_names']}, agents enabled {on}, "
      f"hf downloads marked failed {stopped} (left to resume {live})")
EOF
    echo "still reaching production's paths: a row's own extra run args, and any agent you re-enable here (its read-write mounts are production's)"
    ;;
start)
    [ -e "$dir/lmgw.sqlite" ] || { echo "no copy at $dir (scripts/dev-copy.sh copy first)" >&2; exit 1; }
    if alive; then
        echo "already running: pid $(cat "$dir.pid")" >&2
        exit 1
    fi
    cargo build -p lmgw-core -j 8 --example headless > "$dir.build.log" 2>&1 \
        || { tail -20 "$dir.build.log" >&2; exit 1; }
    LMGW_DATA_DIR="$dir" LMGW_DEV=1 setsid nohup target/debug/examples/headless "$addr" \
        > "$dir.log" 2>&1 < /dev/null &
    echo $! > "$dir.pid"
    for _ in $(seq 1 120); do
        if curl -sf -o /dev/null "http://$addr/v1/models"; then
            (umask 077; grep -m1 -o 'token=[A-Za-z0-9_-]*' "$dir.log" | cut -d= -f2 > "$dir.token")
            echo "up: http://$addr (pid $(cat "$dir.pid"), log $dir.log, owner bearer in $dir.token)"
            exit 0
        fi
        alive || { echo "the gateway exited:" >&2; tail -20 "$dir.log" >&2; exit 1; }
        sleep 1
    done
    echo "no answer from http://$addr after 120 s; see $dir.log" >&2
    exit 1
    ;;
stop)
    if ! alive; then
        echo "not running"
        rm -f "$dir.pid"
        exit 0
    fi
    pid="$(cat "$dir.pid")"
    kill -INT "$pid"
    for _ in $(seq 1 60); do
        kill -0 "$pid" 2>/dev/null || { rm -f "$dir.pid"; echo "stopped (pid $pid)"; exit 0; }
        sleep 1
    done
    echo "pid $pid still running after 60 s" >&2
    exit 1
    ;;
*)
    sed -n '2,8p' "$0" >&2
    exit 2
    ;;
esac
