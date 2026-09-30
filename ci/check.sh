#!/usr/bin/env bash
# Local verification for lmgw: lint + test. Run before committing.
#
# Usage: bash ci/check.sh [-q]      (-q skips clippy, for a fast inner loop)
#
# The pipeline in .gitlab-ci.yml only builds and publishes the RPM, so this is
# the gate that actually runs the test suite before a push (GitHub's
# .github/workflows/ci.yml runs fmt + tests too, but only once the code is
# there, and without clippy). Keeping it as a script (rather
# than three commands remembered differently each time) means "did you run the
# checks" has one answer.
#
# rustfmt is gated for the WHOLE workspace: `cargo fmt --all` is a no-op on a
# clean tree, so a drift fails here instead of burying a later diff. rustfmt does not touch the contents of lmgw-ui's Leptos
# `view!` blocks — it treats a macro body as an opaque token tree — so the
# markup is still hand-laid-out and stays that way.
#
# clippy's `-D warnings` is deliberately NOT gated, because the tree has never
# satisfied it and turning it on would bury a real regression under the
# pre-existing hits. clippy runs and prints, but does not fail the script.
# Check that the files YOU touched are clean:
#     cargo clippy -p lmgw-core --all-targets 2>&1 | grep your_file.rs
#
# Some tests are gated on things a dev box has and CI does not — GGUF files
# under the models dir, a running `lmgw-llama-server` container. Those skip
# themselves cleanly when absent; nothing here requires them.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

quick=false
[[ "${1:-}" == "-q" ]] && quick=true

echo "==> cargo fmt --check (workspace)"
cargo fmt --check --all

# Advisory, like clippy: the giant files grew because every feature appended to
# them, and nobody noticed until they were 9k lines. This makes the size visible
# on every run instead. New logic belongs in a new module next to the big file.
size_limit=2000
echo "==> Rust files over ${size_limit} lines (advisory)"
git ls-files '*.rs' | xargs wc -l | awk -v lim="$size_limit" '$2 != "total" && $1 > lim' | sort -rn || true

if ! $quick; then
  echo "==> cargo clippy (advisory)"
  cargo clippy --workspace --all-targets 2>&1 | grep -E "^(warning|error)" -A 4 || true
fi

# The dashboard is a Trunk-built WASM bundle that lmgw-core serves at `/` (from
# disk in debug), and tests/it/web_pages.rs asserts it is really served — so the
# bundle has to exist before the suite runs. Cheap after the first build.
echo "==> trunk build (lmgw-ui)"
if command -v trunk >/dev/null; then
  (cd crates/lmgw-ui && trunk build)
elif [[ ! -f crates/lmgw-ui/dist/index.html ]]; then
  echo "ERROR: trunk is not installed and crates/lmgw-ui/dist/ is empty." >&2
  echo "       Install it with ci/install-build-deps.sh." >&2
  exit 1
else
  echo "trunk not installed — reusing the existing crates/lmgw-ui/dist/"
fi

echo "==> cargo test"
cargo test --workspace

echo "OK"
