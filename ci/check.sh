#!/usr/bin/env bash
# Local verification for lmgw: lint + test. Run before committing.
#
# Usage: bash ci/check.sh [-q] [--changed [base | A..B]]
#
#   (no flag)   the full check, the gate before a merge
#   -q          skips clippy, for a fast inner loop (either mode)
#   --changed   the fast tier: only what the diff touches. The diff is against
#               base (default: the merge-base with main) and includes
#               uncommitted and untracked files; `A..B` takes the files those
#               commits touch instead. ci/changed.py prints what it selected
#               and why before anything runs.
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
# clippy is gated with `-D warnings` over the whole workspace and all targets
# (the tree is clean). A new toolchain brings new lints and will fail this gate;
# fix those in their own small commit rather than allowing them. Thresholds live
# in clippy.toml (large-error-threshold, with the reason). `-q` skips it.
#
# The suite runs under cargo-nextest when it is installed (settings, retries
# and the reasons in .config/nextest.toml), with `cargo test --doc` for the
# doctests nextest does not run; without it, under `cargo test --workspace`,
# and this script says so. Install: cargo install cargo-nextest --locked.
#
# --changed still builds and lints with `--workspace`: cargo re-checks only the
# crates the diff dirtied (and their dependents), and a `-p` selection would
# resolve features differently and build a second copy of the dependency tree
# (measured 2026-10-03: 113 s the first time, and every lmgw-core change
# checked twice when the two modes alternate). What it narrows is the tests:
# the touched crates' own tests and the `tests/it` modules ci/it-map.toml maps
# the diff to, plus a smoke set. Trunk runs when lmgw-ui or lmgw-api-types
# changed or dist/ is older than their sources, the worklet check when the
# worklets changed.
#
# Some tests are gated on things a dev box has and CI does not — GGUF files
# under the models dir, a running `lmgw-llama-server` container. Those skip
# themselves cleanly when absent; nothing here requires them.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

quick=false
changed=false
base=""
while (($#)); do
  case "$1" in
    -q) quick=true ;;
    --changed)
      changed=true
      if [[ $# -gt 1 && "$2" != -* ]]; then
        base=$2
        shift
      fi
      ;;
    -h | --help)
      sed -n '2,/^set -euo pipefail$/p' "${BASH_SOURCE[0]}" | sed '$d' | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *)
      echo "check.sh: unknown argument: $1 (see --help)" >&2
      exit 2
      ;;
  esac
  shift
done

# --- Stage times, printed at the end (also when a stage fails). -------------------
stage_names=()
stage_us=()
stage_name=""
stage_t0=0
now_us() { echo "${EPOCHREALTIME//[.,]/}"; }
stage_close() {
  [[ -n "$stage_name" ]] || return 0
  stage_names+=("$stage_name")
  stage_us+=("$(($(now_us) - stage_t0))")
  stage_name=""
}
stage() {
  stage_close
  stage_name=$1
  stage_t0=$(now_us)
  echo "==> $1"
}
secs() { printf '%d.%d s' $(($1 / 1000000)) $((($1 / 100000) % 10)); }
check_t0=$(now_us)
report() {
  local rc=$? failed="$stage_name" i
  stage_close
  echo
  echo "Stage times:"
  for i in "${!stage_names[@]}"; do
    printf '  %10s  %s\n' "$(secs "${stage_us[$i]}")" "${stage_names[$i]}"
  done
  printf '  %10s  total\n' "$(secs $(($(now_us) - check_t0)))"
  if ((rc == 0)); then
    echo "OK"
  else
    echo "FAILED${failed:+ in: $failed}"
  fi
}
trap report EXIT

have_nextest() { cargo nextest --version >/dev/null 2>&1; }

# Does any doc comment hold a code block rustdoc would compile? Not one does
# today, and `cargo test --doc` still costs 10 s on an unchanged tree (25 s
# after a lmgw-core change: it resolves features of its own and compiles the
# crate again), so it runs only when there is something to run. A block
# counts when its fence has no info string or a Rust one; ```text, ```json,
# ```sh and the like are not compiled. A block doc comment (/** or /*!) or a
# doc attribute that includes a file is not read here, so it always counts.
has_doctests() {
  local hits
  hits=$(git ls-files -z --cached --others --exclude-standard -- '*.rs' | xargs -0 awk '
    FNR == 1 { open = 0 }
    /^[[:space:]]*\/\*[*!]/ || /#!?\[doc[[:space:]]*=[[:space:]]*include_str!/ {
      print FILENAME ":" FNR
    }
    /^[[:space:]]*(\/\/\/|\/\/!)/ {
      line = $0
      sub(/^[[:space:]]*(\/\/\/|\/\/!)[[:space:]]?/, "", line)
      if (line !~ /^[[:space:]]*```/) next
      if (open) { open = 0; next }
      open = 1
      info = line
      sub(/^[[:space:]]*`+/, "", info)
      gsub(/[[:space:]]/, "", info)
      if (info == "" || info ~ /(^|,)(rust|no_run|should_panic|compile_fail|edition[0-9]+)(,|$)/) {
        print FILENAME ":" FNR
      }
    }
  ')
  [[ -n "$hits" ]]
}

if $changed; then
  stage "select (--changed ${base:-<merge-base with main>})"
  if ! command -v python3 >/dev/null; then
    echo "ERROR: --changed needs python3 (ci/changed.py reads ci/it-map.toml)." >&2
    exit 1
  fi
  plan=$(python3 ci/changed.py ${base:+"$base"})
  eval "$plan"
fi

stage "cargo fmt --check (workspace)"
cargo fmt --check --all

# Advisory, like clippy: the giant files grew because every feature appended to
# them, and nobody noticed until they were 9k lines. This makes the size visible
# on every run instead. New logic belongs in a new module next to the big file.
size_limit=2000
stage "Rust files over ${size_limit} lines (advisory)"
git ls-files '*.rs' | xargs wc -l | awk -v lim="$size_limit" '$2 != "total" && $1 > lim' | sort -rn || true

if ! $quick; then
  stage "cargo clippy -D warnings (workspace)"
  cargo clippy --workspace --all-targets -- -D warnings
fi

# The dashboard is a Trunk-built WASM bundle that lmgw-core serves at `/` (from
# disk in debug), and tests/it/web_pages.rs asserts it is really served — so the
# bundle has to exist before the suite runs. Cheap after the first build.
if $changed && [[ "$RUN_TRUNK" == 0 ]]; then
  echo "==> trunk build skipped (see the selection above)"
else
  stage "trunk build (lmgw-ui)"
  if command -v trunk >/dev/null; then
    (cd crates/lmgw-ui && trunk build)
  elif [[ ! -f crates/lmgw-ui/dist/index.html ]]; then
    echo "ERROR: trunk is not installed and crates/lmgw-ui/dist/ is empty." >&2
    echo "       Install it with ci/install-build-deps.sh." >&2
    exit 1
  else
    echo "trunk not installed — reusing the existing crates/lmgw-ui/dist/"
  fi
fi

# The page's audio worklets are JavaScript that no Rust test reaches: the
# resampler's quality and the player's barge-in accounting are checked offline
# in Node (scripts/worklet-check.mjs, chat-voice §11). Skipped, visibly, where
# node is not installed.
if $changed && [[ "$RUN_WORKLET" == 0 ]]; then
  echo "==> worklet check skipped (see the selection above)"
else
  stage "worklet check (node)"
  if command -v node >/dev/null; then
    node scripts/worklet-check.mjs
  else
    echo "node not installed: scripts/worklet-check.mjs skipped"
  fi
fi

# scripts/publish-github.sh decides what reaches the public repo. Its test builds temp repos
# under target/ with a local bare repo standing in for GitHub; nothing leaves the machine.
if $changed && [[ "$RUN_PUBLISH" == 0 ]]; then
  echo "==> publish script check skipped (see the selection above)"
else
  stage "publish script check (temp repos)"
  python3 scripts/publish_github_test.py
fi

if $changed && have_nextest; then
  stage "cargo nextest run --workspace (selected tests)"
  cargo nextest run --workspace -E "$NEXTEST_FILTER"
  # Doctests belong to the full check: they would cost the separate
  # `cargo test --doc` build that has_doctests explains.
  echo "==> doctests: full check only"
elif have_nextest; then
  stage "cargo nextest run --workspace"
  cargo nextest run --workspace
  stage "doctests (cargo test --doc)"
  if has_doctests; then
    cargo test --workspace --doc
  else
    echo "no doctests: no doc comment in the workspace holds a Rust code block, so"
    echo "cargo test --doc has nothing to run (ci/check.sh's has_doctests)"
  fi
else
  stage "cargo test --workspace (cargo-nextest not installed)"
  echo "NOTE: cargo-nextest is not installed, so the suite runs under cargo test: one"
  echo "      process per test binary, several times slower here (lmgw-core's tests/it"
  echo "      took 359 s against 36 s, 2026-10-03), and known flakes are not retried."
  if $changed; then
    echo "      --changed narrows the tests through nextest filters, so this is the WHOLE"
    echo "      suite, not the selection printed above."
  fi
  echo "      Install it with: cargo install cargo-nextest --locked"
  cargo test --workspace
fi

stage_close
