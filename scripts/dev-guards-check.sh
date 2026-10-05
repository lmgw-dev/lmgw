#!/usr/bin/env bash
# Negative checks for the dev guards that keep a dev run off production's data dir and downloads.
#
# Usage: scripts/dev-guards-check.sh [--no-build]
#
# Everything runs under a fake HOME and XDG_DATA_HOME in target/dev-guards-check, so the dirs
# standing in for production's are the guards' own comparisons, never the real path:
#
# - scripts/dev-copy.sh refuses production's dir as ~/.local/share/lmgw, as $XDG_DATA_HOME/lmgw
#   and as LMGW_PROD_DIR, for copy, start and stop;
# - scripts/dev-copy.sh copy marks production's queued/in-flight Hugging Face downloads failed
#   in the copy, with its note, and leaves production's database byte for byte as it was;
# - the headless runner refuses ~/.local/share/lmgw while XDG_DATA_HOME points elsewhere, and an
#   empty LMGW_DATA_DIR, before it opens or creates anything (a podman stub that runs nothing is
#   first on PATH, should a guard ever fail).
#
# Exit status 0 when every check passed. Leaves nothing behind.
set -euo pipefail
cd "$(dirname "$0")/.."

T="$PWD/target/dev-guards-check"
rm -rf "$T"
mkdir -p "$T/home/.local/share/lmgw" "$T/xdg/lmgw" "$T/prod" "$T/bin"
trap 'rm -rf "$T"' EXIT

fails=0
pass() { echo "PASS  $1"; }
fail() { echo "FAIL  $1"; fails=$((fails + 1)); }

# --- dev-copy.sh: production's dir under each of its names --------------------------------------
refused() {  # refused <label> <env...> -- <args...>
    local label="$1"; shift
    local envs=()
    while [ "$1" != "--" ]; do envs+=("$1"); shift; done
    shift
    local out
    if out="$(env -u XDG_DATA_HOME -u LMGW_PROD_DIR "${envs[@]}" bash scripts/dev-copy.sh "$@" \
            2>&1)"; then
        fail "$label (exit 0: $out)"
    elif grep -q "is production's data dir" <<< "$out"; then
        pass "$label"
    else
        fail "$label (not the guard: $out)"
    fi
}
for cmd in copy start stop; do
    refused "dev-copy $cmd refuses ~/.local/share/lmgw while XDG_DATA_HOME points elsewhere" \
        HOME="$T/home" XDG_DATA_HOME="$T/xdg" -- "$cmd" "$T/home/.local/share/lmgw"
    refused "dev-copy $cmd refuses \$XDG_DATA_HOME/lmgw" \
        HOME="$T/home" XDG_DATA_HOME="$T/xdg" -- "$cmd" "$T/xdg/lmgw"
    refused "dev-copy $cmd refuses ~/.local/share/lmgw while LMGW_PROD_DIR points elsewhere" \
        HOME="$T/home" LMGW_PROD_DIR="$T/prod" -- "$cmd" "$T/home/.local/share/lmgw/."
    refused "dev-copy $cmd refuses LMGW_PROD_DIR" \
        HOME="$T/home" LMGW_PROD_DIR="$T/prod" -- "$cmd" "$T/prod"
done
ln -s "$T/home/.local/share/lmgw" "$T/link"
refused "dev-copy copy refuses a symlink to it" HOME="$T/home" -- copy "$T/link"

# --- dev-copy.sh copy: production's downloads stay production's ----------------------------------
python3 - "$T/prod/lmgw.sqlite" <<'EOF'
import json, sqlite3, sys
c = sqlite3.connect(sys.argv[1])
c.execute("CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL)")
c.execute("INSERT INTO settings VALUES ('settings', ?)",
          (json.dumps({"container_prefix": "lmgw", "bind_addr": "127.0.0.1:8001",
                       "legacy_container_names": ["old"]}),))
c.execute("CREATE TABLE agents (id TEXT PRIMARY KEY, enabled INTEGER NOT NULL)")
c.execute("INSERT INTO agents VALUES ('a', 1)")
c.execute("CREATE TABLE hf_models (id INTEGER PRIMARY KEY, repo TEXT, file TEXT, status TEXT, "
          "error TEXT)")
c.executemany("INSERT INTO hf_models (repo, file, status) VALUES (?, ?, ?)",
              [("r", "queued.gguf", "queued"), ("r", "running.gguf", "downloading"),
               ("r", "done.gguf", "done"), ("r", "update.gguf", "update_available")])
c.commit()
EOF
before="$(sha256sum < "$T/prod/lmgw.sqlite")"
if out="$(env -u XDG_DATA_HOME HOME="$T/home" LMGW_PROD_DIR="$T/prod" \
        bash scripts/dev-copy.sh copy "$T/copy" 127.0.0.1:8898 2>&1)"; then
    echo "      $out"
    rows="$(python3 - "$T/copy/lmgw.sqlite" <<'EOF'
import sqlite3, sys
c = sqlite3.connect(sys.argv[1])
print(";".join(f"{f}={s}:{'note' if e and 'dev copy' in e else e}"
               for f, s, e in c.execute("SELECT file, status, error FROM hf_models ORDER BY id")))
EOF
)"
    want="queued.gguf=failed:note;running.gguf=failed:note;done.gguf=done:None;update.gguf=update_available:None"
    if [ "$rows" = "$want" ] && grep -q "hf downloads marked failed 2 (left to resume 0)" <<< "$out"; then
        pass "dev-copy copy marks production's queued and in-flight downloads failed in the copy"
    else
        fail "dev-copy copy's downloads: $rows"
    fi
else
    fail "dev-copy copy of a fake production failed: $out"
fi
if [ "$(sha256sum < "$T/prod/lmgw.sqlite")" = "$before" ]; then
    pass "production's database is untouched by the copy"
else
    fail "production's database changed"
fi

# --- the headless runner ------------------------------------------------------------------------
if [ "${1:-}" != "--no-build" ]; then
    cargo build -p lmgw-core --example headless -j "${CARGO_BUILD_JOBS:-8}" > "$T/build.log" 2>&1 \
        || { tail -20 "$T/build.log"; exit 1; }
fi
cat > "$T/bin/podman" <<'EOF'
#!/bin/sh
echo "dev-guards-check: podman is stubbed out" >&2
exit 125
EOF
chmod +x "$T/bin/podman"
headless_refuses() {  # headless_refuses <label> <expected text> <LMGW_DATA_DIR> <dir that must stay empty>
    local out code=0
    out="$(cd "$T" && HOME="$T/home" XDG_DATA_HOME="$T/xdg" LMGW_DATA_DIR="$3" \
        LMGW_CONTAINER_PREFIX="lmgw-guardcheck-$$" PATH="$T/bin:$PATH" RUST_LOG=warn \
        timeout 20 "$OLDPWD/target/debug/examples/headless" 127.0.0.1:8898 2>&1)" || code=$?
    if [ "$code" = 0 ] || [ "$code" = 124 ]; then
        fail "$1 (exit $code: it ran)"
    elif ! grep -q "$2" <<< "$out"; then
        fail "$1 (not the guard: $out)"
    elif [ -e "$4/lmgw.sqlite" ]; then
        fail "$1 (it created $4/lmgw.sqlite)"
    else
        pass "$1"
    fi
}
headless_refuses "headless refuses ~/.local/share/lmgw while XDG_DATA_HOME points elsewhere" \
    "is the installed app's data dir" "$T/home/.local/share/lmgw" "$T/home/.local/share/lmgw"
headless_refuses "headless refuses \$XDG_DATA_HOME/lmgw" \
    "is the installed app's data dir" "$T/xdg/lmgw" "$T/xdg/lmgw"
headless_refuses "headless refuses an empty LMGW_DATA_DIR" "not set (or empty)" "" "$T"

echo "$([ "$fails" = 0 ] && echo "all passed" || echo "$fails failed")"
[ "$fails" = 0 ]
