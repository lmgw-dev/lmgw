#!/usr/bin/env bash
# Build the lmgw RPM and assemble dist/ (the RPM + the updater manifest
# `latest.json`) for whichever pipeline publishes it.
#
# One build, three environments, told apart by the variables each CI sets:
#
#   GitLab CI (GITLAB_CI=true) — the private test feed, a build of every push to
#     main. Version <Cargo.toml version>+<CI_PIPELINE_IID>; the binary is pointed
#     at this project's generic package registry (LMGW_UPDATE_MANIFEST_URL, plus
#     the masked LMGW_UPDATE_DEPLOY_TOKEN CI/CD variable), and ci/publish.sh
#     uploads dist/ there.
#   GitHub Actions (GITHUB_ACTIONS=true) — public releases, one per pushed tag (or a dispatch naming one)
#     v<X.Y.Z>. Version X.Y.Z, which must equal the version in Cargo.toml (so the
#     tagged source says what it is); the binary polls the public default feed
#     (lmgw_core::update::PUBLIC_MANIFEST_URL), and .github/workflows/release.yml
#     attaches dist/ to the release of that tag.
#   Neither — a local run. Version as in Cargo.toml, rpm.url is a file:// path,
#     nothing is published. LMGW_UPDATE_* already in your environment pass
#     through as-is.
#
# The in-app updater (lmgw_update::is_newer) and rpm order the two kinds
# the same way: 0.3.0 < 0.3.0+98 < 0.3.0+99 < 0.3.1. A private build is always
# newer than the release it follows and older than the next one, and the
# pipeline counter is monotonic without any git history. scripts/release.sh
# bumps the version and makes the tag.
#
# LMGW_BUILD_SERVER=only builds the server image's binary instead of the RPM
# (headless server design §10): the same version, update feed and UI as the RPM
# gets, then the release lmgw-server, into dist/lmgw-server + dist/build.env for
# ci/build-image.sh. No RPM, no guard, no latest.json. GitLab runs it as a job of
# its own after the RPM's (.gitlab-ci.yml `server-binary`); the RPM build never
# builds the server.
#
# Usage: bash ci/build.sh
#        LMGW_BUILD_SERVER=only bash ci/build.sh
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
export PATH="${CARGO_HOME:-$HOME/.cargo}/bin:$PATH"

# tauri's rpm bundler (tauri-cli 2.12.0) gives each packaged file the mode of the file it
# packs, and rpm installs it as root with that mode. Whatever this script creates (the
# binary, the .desktop file) gets owner-only write from this umask, whatever the caller's.
umask 022

case "${LMGW_BUILD_SERVER:-}" in
  "") SERVER_ONLY=0 ;;
  only) SERVER_ONLY=1 ;;
  *) echo "ERROR: LMGW_BUILD_SERVER is '${LMGW_BUILD_SERVER}': 'only' builds the server binary, unset builds the RPM" >&2; exit 1 ;;
esac

# The release this tree belongs to: the [workspace.package] version.
BASE="$(grep -m1 -oE '^version = "[0-9]+\.[0-9]+\.[0-9]+"' Cargo.toml | grep -oE '[0-9.]+' || true)"
[[ -n "$BASE" ]] || { echo "ERROR: no X.Y.Z version in Cargo.toml" >&2; exit 1; }

if [[ "${GITLAB_CI:-}" == "true" ]]; then
  PLATFORM=gitlab
  VERSION="${BASE}+${CI_PIPELINE_IID:?CI_PIPELINE_IID is unset under GitLab CI}"
  COMMIT="${CI_COMMIT_SHORT_SHA:-unknown}"
  REF="${CI_COMMIT_BRANCH:-${CI_COMMIT_REF_NAME:-unknown}}"
elif [[ "${GITHUB_ACTIONS:-}" == "true" ]]; then
  PLATFORM=github
  # The tag: RELEASE_TAG (release.yml sets it, also for a dispatch of an
  # existing tag), else the pushed ref.
  TAG="${RELEASE_TAG:-}"
  if [[ -z "$TAG" && "${GITHUB_REF_TYPE:-}" == "tag" ]]; then
    TAG="${GITHUB_REF_NAME:-}"
  fi
  if [[ "$TAG" != v* ]]; then
    echo "ERROR: public builds are made from a v<X.Y.Z> tag, not ${TAG:-${GITHUB_REF:-an unknown ref}}" >&2
    exit 1
  fi
  VERSION="${TAG#v}"
  if [[ "$VERSION" != "$BASE" ]]; then
    echo "ERROR: tag ${TAG} does not match version ${BASE} in Cargo.toml" >&2
    echo "       (scripts/release.sh bumps the version and tags in one go)" >&2
    exit 1
  fi
  # The checked-out commit (a dispatch's GITHUB_SHA is the default branch's).
  COMMIT="$(git rev-parse HEAD 2>/dev/null || echo "${GITHUB_SHA:-unknown}")"
  COMMIT="${COMMIT:0:8}"
  REF="$TAG"
else
  PLATFORM=local
  VERSION="$BASE"
  COMMIT="$(git rev-parse --short HEAD 2>/dev/null || echo local)"
  REF="$(git branch --show-current 2>/dev/null || true)"
  REF="${REF:-local}"
fi

echo "Building lmgw ${VERSION} (${PLATFORM})"

# Which update feed the binary polls is decided at COMPILE time
# (option_env! in crates/lmgw-core/src/update.rs), so it has to be settled here,
# before cargo runs.
case "$PLATFORM" in
  gitlab)
    # The numeric project id (not the URL-encoded path) addresses the
    # generic-package API, so moving or renaming the project never breaks the
    # feed URL an installed updater has baked in: the feed the private test
    # builds poll, and the RPM download URL embedded in the manifest.
    REGISTRY="${CI_API_V4_URL}/projects/${CI_PROJECT_ID}/packages/generic/lmgw"
    export LMGW_UPDATE_MANIFEST_URL="${REGISTRY}/latest/latest.json"
    # A masked CI/CD variable is already in the job environment; it only needs
    # to reach cargo. Without it the build still succeeds, but its updater gets
    # a 401/404 from the private registry — say so where it will be seen.
    if [[ -n "${LMGW_UPDATE_DEPLOY_TOKEN:-}" ]]; then
      export LMGW_UPDATE_DEPLOY_TOKEN
    else
      echo "WARNING: LMGW_UPDATE_DEPLOY_TOKEN is not set: this build cannot read its own" >&2
      echo "         update feed. Define it as a masked CI/CD variable (a deploy token with" >&2
      echo "         read_package_registry); a *protected* variable only reaches protected branches." >&2
    fi
    ;;
  github)
    # Public builds poll the public default feed. Unset, not merely left
    # alone, so a stray repository variable can never bake a private feed —
    # or its token — into a public artifact.
    unset LMGW_UPDATE_MANIFEST_URL LMGW_UPDATE_DEPLOY_TOKEN
    ;;
esac
echo "Update feed baked in: ${LMGW_UPDATE_MANIFEST_URL:-public default}" \
  "(deploy token: $([[ -n "${LMGW_UPDATE_DEPLOY_TOKEN:-}" ]] && echo yes || echo no))"

# Stamp the version into the workspace package + Tauri config so the binary
# (CARGO_PKG_VERSION), the RPM filename, and the published manifest all agree.
# A no-op unless this is a private build (the +BUILD suffix).
sed -i -E "s/^version = \"[0-9]+\.[0-9]+\.[0-9]+\"/version = \"${VERSION}\"/" Cargo.toml
sed -i -E "s/(\"version\"[[:space:]]*:[[:space:]]*)\"[0-9]+\.[0-9]+\.[0-9]+\"/\1\"${VERSION}\"/" \
  src-tauri/tauri.conf.json

# The CI cache restores a populated target/ between pipelines. A version-only
# change to Cargo.toml / tauri.conf.json does NOT reliably invalidate the app
# binary or the bundle, so without this the previous pipeline's RPM (and the
# binary's baked CARGO_PKG_VERSION) leak into this build: the manifest then
# advertises $VERSION while shipping an older RPM, and the in-app updater loops
# forever. Clear any stale bundle and force the version-bearing crates to
# recompile so the binaries and the RPM all actually carry $VERSION. (Their deps
# stay cached, so this is cheap.) dist/ likewise starts empty, so nothing from
# an earlier run can be picked up by a publisher. The root helper carries the
# version too (its plan-changed line names it, machine-check design §3.5), and
# its clean also means the target/release/lmgw-machine-helper the RPM packs, and
# ci/rpm_guard.py compares the package's copy with, is this job's build.
if [[ "$SERVER_ONLY" == 1 ]]; then
  # The same for the server binary: lmgw-server's --version and lmgw-core's
  # /api/version carry $VERSION.
  rm -rf dist
  cargo clean -p lmgw-server -p lmgw-core
else
  rm -rf target/release/bundle dist
  cargo clean -p lmgw -p lmgw-core -p lmgw-machine-helper
fi

# The Leptos UI must exist before lmgw-core compiles: rust-embed bakes
# crates/lmgw-ui/dist/ into the binary (the `cargo clean -p lmgw-core` above
# already guarantees the embed is re-run). Fail here, not with a UI-less RPM.
(cd crates/lmgw-ui && trunk build --release)
if [[ ! -f crates/lmgw-ui/dist/index.html ]]; then
  echo "ERROR: trunk build produced no crates/lmgw-ui/dist/index.html" >&2
  exit 1
fi

if [[ "$SERVER_ONLY" == 1 ]]; then
  # `-p lmgw-server` resolves lmgw-core's features for the server alone (no desktop features),
  # so its feature-dependent units differ from the RPM's: correct, and compiled in this job.
  # The time and the largest single process's peak memory (GNU time's "Maximum resident set
  # size": getrusage's RUSAGE_CHILDREN, the largest descendant, in practice the link) are
  # printed for §10's measurement, also when the build fails. A rustc that sccache runs sits in
  # sccache's server, not under cargo, so on a machine with sccache the figure undercounts.
  echo "Building lmgw-server ${VERSION} (no RPM: LMGW_BUILD_SERVER=only)"
  server_status=0
  if [[ -x /usr/bin/time ]]; then
    mkdir -p target
    /usr/bin/time -v -o target/lmgw-server-build.time cargo build --release -p lmgw-server \
      || server_status=$?
    grep -E 'Elapsed \(wall clock\)|Maximum resident set size|Exit status|terminated by signal' \
      target/lmgw-server-build.time || true
  else
    echo "NOTE: /usr/bin/time is not installed (dnf install time), so the build's peak memory is not measured" >&2
    server_t0=$SECONDS
    cargo build --release -p lmgw-server || server_status=$?
    echo "cargo build took $((SECONDS - server_t0)) s"
  fi
  if [[ "$server_status" != 0 ]]; then
    echo "ERROR: cargo build --release -p lmgw-server failed (exit ${server_status})" >&2
    exit "$server_status"
  fi
  if [[ ! -f target/release/lmgw-server || -L target/release/lmgw-server ]]; then
    echo "ERROR: cargo built no target/release/lmgw-server" >&2
    exit 1
  fi
  SERVER_LINE="$(target/release/lmgw-server --version)"
  if [[ "$SERVER_LINE" != "lmgw-server ${VERSION}" ]]; then
    echo "ERROR: target/release/lmgw-server --version printed '${SERVER_LINE}', not 'lmgw-server ${VERSION}'" >&2
    exit 1
  fi
  # The image's Fedora is the one this binary linked against (ci/build-image.sh).
  # shellcheck disable=SC1091
  SERVER_FEDORA_RELEASE="$(. /etc/os-release && [[ "${ID:-}" == fedora ]] && echo "${VERSION_ID:-}")" || true
  if [[ -z "$SERVER_FEDORA_RELEASE" ]]; then
    echo "ERROR: lmgw-server was built on a system that is not Fedora; the image needs its release" >&2
    exit 1
  fi
  mkdir -p dist
  install -m 0755 target/release/lmgw-server dist/lmgw-server
  # Consumed by ci/build-image.sh.
  { echo "VERSION=${VERSION}"; echo "SERVER_FEDORA_RELEASE=${SERVER_FEDORA_RELEASE}"; } > dist/build.env
  echo "Built ${SERVER_LINE} on Fedora ${SERVER_FEDORA_RELEASE}"
  exit 0
fi

# The bundle packs checked-out files too (the icons, the licence files), with their mode in
# the checkout. GitLab Runner's docker executor cloned with umask 0000 before .gitlab-ci.yml
# set FF_DISABLE_UMASK_FOR_DOCKER_EXECUTOR, so they were 0666 and an lmgw 0.5.0 CI build installed
# them world-writable under /usr. Take group and other write off every tracked regular file
# (symlinks are skipped: chmod would follow them) whichever runner cloned it; a no-op on a
# normal checkout. ci/rpm_guard.py below is the barrier, this only keeps it from tripping.
if git rev-parse --git-dir >/dev/null 2>&1; then
  # Only paths that exist: a tracked file deleted in the work tree must not abort a local build.
  git ls-files -z --stage | grep -z '^100' | cut -z -f2- \
    | while IFS= read -r -d '' f; do if [[ -e "$f" ]]; then printf '%s\0' "$f"; fi; done \
    | xargs -0 -r chmod go-w --
else
  echo "ci/build.sh: not a git checkout, so no tracked file is chmod go-w'd; ci/rpm_guard.py" \
       "still refuses a group- or world-writable entry" >&2
fi

# The root helper (machine-check design §3.1, §3.8): src-tauri/tauri.conf.json packs
# target/release/lmgw-machine-helper as /usr/libexec/lmgw/lmgw-machine-helper, so it is built
# before the bundle. Its mode in the RPM is the file's (0755 under the umask above), and
# ci/rpm_guard.py pins the package's copy to this file's digest.
cargo build --release -p lmgw-machine-helper
if [[ ! -f target/release/lmgw-machine-helper || -L target/release/lmgw-machine-helper ]]; then
  echo "ERROR: cargo built no target/release/lmgw-machine-helper" >&2
  exit 1
fi

cargo tauri build --bundles rpm

# Select the RPM by its exact built version — never blindly glob the dir, which
# may still hold artifacts from a prior build. Fail loudly if no RPM matching
# $VERSION was produced rather than publish a mislabeled manifest.
RPM_PATH="$(ls -1 target/release/bundle/rpm/lmgw-${VERSION}-*.rpm 2>/dev/null | head -n1 || true)"
if [[ -z "${RPM_PATH}" || ! -f "${RPM_PATH}" ]]; then
  echo "ERROR: no RPM matching lmgw-${VERSION}-*.rpm was produced." >&2
  echo "Bundle dir contents:" >&2
  ls -la target/release/bundle/rpm/ >&2 2>/dev/null || echo "  (missing)" >&2
  exit 1
fi

# Nothing root installs may give anyone else a way in: ci/rpm_guard.py refuses a group- or
# world-writable entry, a setuid/setgid/sticky bit, a file capability, an owner other than
# root, any path outside ci/rpm-allowlist.toml and any scriptlet, a polkit policy whose
# content is not the pinned one, and a root helper that is not the file built above, and
# fails the build.
python3 ci/rpm_guard.py "$RPM_PATH"

RPM_NAME="$(basename "$RPM_PATH")"
SHA="$(sha256sum "$RPM_PATH" | cut -d' ' -f1)"

# Where the updater downloads the RPM from — the same place the publisher of
# this platform puts it.
case "$PLATFORM" in
  gitlab) RPM_URL="${REGISTRY}/${VERSION}/${RPM_NAME}" ;;
  github) RPM_URL="https://github.com/${GITHUB_REPOSITORY}/releases/download/v${VERSION}/${RPM_NAME}" ;;
  local) RPM_URL="file://${PWD}/dist/${RPM_NAME}" ;;
esac

mkdir -p dist
cp "$RPM_PATH" "dist/${RPM_NAME}"
cat > dist/latest.json <<EOF
{
  "version": "${VERSION}",
  "notes": "Automated build of ${COMMIT} on ${REF}.",
  "pub_date": "$(date -u +%Y-%m-%dT%H:%M:%SZ)",
  "rpm": {
    "file": "${RPM_NAME}",
    "url": "${RPM_URL}",
    "sha256": "${SHA}"
  }
}
EOF

# Consumed by ci/publish.sh (GitLab).
{ echo "VERSION=${VERSION}"; echo "RPM_NAME=${RPM_NAME}"; } > dist/build.env

echo "Built ${RPM_NAME} (sha256 ${SHA})"
cat dist/latest.json
