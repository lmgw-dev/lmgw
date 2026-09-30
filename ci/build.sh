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
#   GitHub Actions (GITHUB_ACTIONS=true) — public releases, one per pushed tag
#     v<X.Y.Z>. Version X.Y.Z, which must equal the version in Cargo.toml (so the
#     tagged source says what it is); the binary polls the public default feed
#     (lmgw_core::update::PUBLIC_MANIFEST_URL), and .github/workflows/release.yml
#     attaches dist/ to the release of that tag.
#   Neither — a local run. Version as in Cargo.toml, rpm.url is a file:// path,
#     nothing is published. LMGW_UPDATE_* already in your environment pass
#     through as-is.
#
# The in-app updater (lmgw_core::update::is_newer) and rpm order the two kinds
# the same way: 0.3.0 < 0.3.0+98 < 0.3.0+99 < 0.3.1. A private build is always
# newer than the release it follows and older than the next one, and the
# pipeline counter is monotonic without any git history. scripts/release.sh
# bumps the version and makes the tag.
#
# Usage: bash ci/build.sh
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
export PATH="${CARGO_HOME:-$HOME/.cargo}/bin:$PATH"

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
  if [[ "${GITHUB_REF_TYPE:-}" != "tag" || "${GITHUB_REF_NAME:-}" != v* ]]; then
    echo "ERROR: public builds are made from a v<X.Y.Z> tag, not ${GITHUB_REF:-an unknown ref}" >&2
    exit 1
  fi
  VERSION="${GITHUB_REF_NAME#v}"
  if [[ "$VERSION" != "$BASE" ]]; then
    echo "ERROR: tag ${GITHUB_REF_NAME} does not match version ${BASE} in Cargo.toml" >&2
    echo "       (scripts/release.sh bumps the version and tags in one go)" >&2
    exit 1
  fi
  COMMIT="${GITHUB_SHA:-unknown}"
  COMMIT="${COMMIT:0:8}"
  REF="${GITHUB_REF_NAME}"
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
    # URL-encode the project path (group/project -> group%2Fproject) for the
    # token-resolvable generic-package API URLs: the feed the private test
    # builds poll, and the RPM download URL embedded in the manifest.
    REGISTRY="${CI_API_V4_URL}/projects/${CI_PROJECT_PATH//\//%2F}/packages/generic/lmgw"
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
# forever. Clear any stale bundle and force the two version-bearing crates to
# recompile so the binary and the RPM both actually carry $VERSION. (Their deps
# stay cached, so this is cheap.) dist/ likewise starts empty, so nothing from
# an earlier run can be picked up by a publisher.
rm -rf target/release/bundle dist
cargo clean -p lmgw -p lmgw-core

# The Leptos UI must exist before lmgw-core compiles: rust-embed bakes
# crates/lmgw-ui/dist/ into the binary (the `cargo clean -p lmgw-core` above
# already guarantees the embed is re-run). Fail here, not with a UI-less RPM.
(cd crates/lmgw-ui && trunk build --release)
if [[ ! -f crates/lmgw-ui/dist/index.html ]]; then
  echo "ERROR: trunk build produced no crates/lmgw-ui/dist/index.html" >&2
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
