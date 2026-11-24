#!/usr/bin/env bash
# Build the Arch Linux package from this checkout, in a throwaway Arch container
# (GitLab's `arch` job, release.yml's `arch` job). Runs as root: it installs the
# PKGBUILD's dependencies with pacman and builds as an unprivileged `builder`,
# because makepkg refuses root.
#
# The version and the update feed are ci/build.sh's, so the RPM and the Arch
# package of one pipeline agree:
#   GitLab  <Cargo.toml version>+<pipeline IID>, this project's registry feed
#           and LMGW_UPDATE_DEPLOY_TOKEN baked in;
#   GitHub  the v<X.Y.Z> tag's version (it must match Cargo.toml), the public
#           default feed;
#   local   Cargo.toml's version, the public default feed.
# Like ci/build.sh it stamps that version into Cargo.toml and
# src-tauri/tauri.conf.json, so run it on a checkout you can throw away.
#
# The package must pass ci/arch_guard.py (the RPM guard's policy and allowlist) before it is
# handed on.
#
# Root and builder are kept apart like this, and no further:
#   - the checkout's top directory stays root's, so builder cannot rename ci/ or .git away or
#     create new top-level names; ci/ and .git are root's, and the rest of the tree is builder's.
#     Root runs only ci/'s scripts, the PKGBUILD's dependency lists (sourced before any builder
#     process exists) and system tools; it runs nothing builder wrote (the built binary's
#     --version runs as builder) and reads builder's files as data only
#   - the build's Cargo.lock update may change nothing but the workspace members' versions
#   - once makepkg is done, every process of builder is killed before root touches the result;
#     the guard then checks a root-owned copy of the package, and that copy is the one published
# Builder's build itself is not confined: it can write anything in the tree it owns.
#
# Output in dist-arch/: the package, its .sha256 and arch.json
# ({version, file, sha256}), which ci/publish.sh arch and release.yml's
# publish-arch check before uploading.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
umask 022

die() { echo "ERROR: $*" >&2; exit 1; }

[[ -f /etc/arch-release ]] || die "build-arch.sh runs in an Arch Linux container"
[[ "$(id -u)" == 0 ]] || die "build-arch.sh runs as root (pacman, useradd); it builds as 'builder'"

ROOT="$PWD"
PKGBUILD=packaging/arch/PKGBUILD

BASE="$(grep -m1 -oE '^version = "[0-9]+\.[0-9]+\.[0-9]+"' Cargo.toml | grep -oE '[0-9.]+' || true)"
[[ -n "$BASE" ]] || die "no X.Y.Z version in Cargo.toml"
PKGVER="$(sed -n 's/^pkgver=//p' "$PKGBUILD")"
[[ "$PKGVER" == "$BASE" ]] \
  || die "$PKGBUILD says pkgver=$PKGVER, Cargo.toml $BASE (scripts/release.sh sets both)"

if [[ "${GITLAB_CI:-}" == "true" ]]; then
  VERSION="${BASE}+${CI_PIPELINE_IID:?CI_PIPELINE_IID is unset under GitLab CI}"
  export LMGW_UPDATE_MANIFEST_URL="${CI_API_V4_URL}/projects/${CI_PROJECT_ID}/packages/generic/lmgw/latest/latest.json"
  if [[ -n "${LMGW_UPDATE_DEPLOY_TOKEN:-}" ]]; then
    export LMGW_UPDATE_DEPLOY_TOKEN
  else
    echo "WARNING: LMGW_UPDATE_DEPLOY_TOKEN is not set: this build cannot read its own update feed" >&2
  fi
elif [[ "${GITHUB_ACTIONS:-}" == "true" ]]; then
  TAG="${RELEASE_TAG:-${GITHUB_REF_NAME:-}}"
  [[ "$TAG" == v* ]] || die "public builds are made from a v<X.Y.Z> tag, not ${TAG:-an unknown ref}"
  VERSION="${TAG#v}"
  [[ "$VERSION" == "$BASE" ]] || die "tag $TAG does not match version $BASE in Cargo.toml"
  unset LMGW_UPDATE_MANIFEST_URL LMGW_UPDATE_DEPLOY_TOKEN
else
  VERSION="$BASE"
fi
echo "Building the Arch package of lmgw ${VERSION}"
echo "Update feed baked in: ${LMGW_UPDATE_MANIFEST_URL:-public default}" \
  "(deploy token: $([[ -n "${LMGW_UPDATE_DEPLOY_TOKEN:-}" ]] && echo yes || echo no))"

# As ci/build.sh: the binaries' CARGO_PKG_VERSION and tauri's must be $VERSION.
sed -i -E "s/^version = \"[0-9]+\.[0-9]+\.[0-9]+\"/version = \"${VERSION}\"/" Cargo.toml
sed -i -E "s/(\"version\"[[:space:]]*:[[:space:]]*)\"[0-9]+\.[0-9]+\.[0-9]+\"/\1\"${VERSION}\"/" \
  src-tauri/tauri.conf.json

# python runs ci/arch_guard.py on the built package (the Arch image lacks it); sudo comes with
# base-devel.
pacman -Syu --noconfirm --needed git python
export PYTHONDONTWRITEBYTECODE=1

# The guard's own test first, before any builder process exists: a guard that stopped refusing
# things must fail here, not pass every package.
python3 ci/arch_guard_test.py
# The PKGBUILD's own lists, read in a subshell (makepkg -s would want sudo for
# the builder).
# shellcheck disable=SC1090,SC2154
(source "$PKGBUILD"; pacman -S --noconfirm --needed "${depends[@]}" "${makedepends[@]}")

id builder >/dev/null 2>&1 || useradd -m builder
# The unprivileged build writes target/, the cargo home and caches, the UI's dist/, tauri's
# generated files and Cargo.lock, so builder owns everything in the checkout except the top
# directory itself, .git and ci/. The top directory stays root's (and not group- or
# world-writable), so builder can neither rename ci/ away nor plant another one; the top-level
# directories it must create are made here. The checkout goes back to its owner on exit,
# because the runner's next job reuses the directory.
scratch="$(mktemp -d)"
owner="$(stat -c '%u:%g' "$ROOT")"
reap_builder() { pkill -KILL -u builder 2>/dev/null || true; }
trap 'reap_builder; chown -R "$owner" "$ROOT"; rm -rf "$scratch"' EXIT
chown root: "$ROOT"
chmod 0755 "$ROOT"
mkdir -p "$ROOT/target" "$ROOT/.cargo" "$ROOT/.cache"
find "$ROOT" -mindepth 1 \( -path "$ROOT/.git" -o -path "$ROOT/ci" \) -prune \
  -o -exec chown -h builder: {} +
# A fixed work dir, not a mktemp one: the PKGBUILD's build remaps $srcdir (and
# $startdir under it) in RUSTFLAGS, which cargo fingerprints, so a new name per run
# would rebuild every cached unit.
work=/tmp/lmgw-arch-pkg
rm -rf "$work"
mkdir "$work"
cp packaging/arch/PKGBUILD packaging/arch/lmgw.desktop "$work/"
# pacman's version: no '-' allowed, and '+' sorts as a separator, so
# 0.7.0+41 < 0.7.0+42 < 0.8.0 as for the RPM.
sed -i -E "s/^pkgver=.*/pkgver=${VERSION}/; s/^pkgrel=.*/pkgrel=1/" "$work/PKGBUILD"
chown -R builder: "$work"

# What makepkg's build needs from this environment; sudo resets the rest.
keep=LMGW_PKG_TREE,CARGO_HOME,XDG_CACHE_HOME,ORT_CACHE_DIR,CARGO_TERM_COLOR,CARGO_INCREMENTAL
keep+=,CARGO_PROFILE_RELEASE_LTO,CARGO_PROFILE_RELEASE_CODEGEN_UNITS
keep+=,LMGW_UPDATE_MANIFEST_URL,LMGW_UPDATE_DEPLOY_TOKEN
export LMGW_PKG_TREE="$ROOT"
as_builder() { sudo -u builder --preserve-env="$keep" "$@"; }

# The stamp changed the workspace members' versions, which Cargo.lock records. Bring the
# lock up to date here, for the members only, so the PKGBUILD's --locked builds then fail on
# any other drift. Not --offline: the workspace has a git dependency whose checkout is in no
# cargo home this job starts with. So that this step can move nothing else, root compares the
# lock with the one from before and stops on any difference beyond the members' version lines.
cp Cargo.lock "$scratch/Cargo.lock.before"
as_builder cargo update --workspace --quiet
python3 ci/arch_guard_lock.py "$scratch/Cargo.lock.before" Cargo.lock

# As ci/build.sh: a cached target/ does not reliably rebuild the crates that
# carry the version (or the baked feed), so they compile afresh.
as_builder cargo clean -p lmgw -p lmgw-core -p lmgw-machine-helper -p lmgw-doc-reader

(cd "$work" && as_builder makepkg --noconfirm)

# As ci/build.sh: the binary the package carries says the version this job stamped.
# Run as builder: root never executes what the build wrote.
READER_LINE="$(as_builder target/release/lmgw-doc-reader --version)"
[[ "$READER_LINE" == "lmgw-doc-reader ${VERSION} "* ]] \
  || die "target/release/lmgw-doc-reader --version printed '${READER_LINE}', not 'lmgw-doc-reader ${VERSION} …'"

# Builder is done: nothing of it may still run when root reads or copies the result.
reap_builder

shopt -s nullglob
pkgs=("$work"/lmgw-"${VERSION}"-1-x86_64.pkg.tar.zst)
((${#pkgs[@]} == 1)) || die "makepkg produced no lmgw-${VERSION}-1-x86_64.pkg.tar.zst"
[[ -f "${pkgs[0]}" && ! -L "${pkgs[0]}" ]] || die "${pkgs[0]} is not a plain file"
FILE="$(basename "${pkgs[0]}")"
# Root's own copy, in a directory only root can write: the guard checks it and it is the copy
# that is published, so nothing can change between the two.
install -m 0644 -o root -g root "${pkgs[0]}" "$scratch/$FILE"

# Nothing root installs may give anyone else a way in: ci/arch_guard.py refuses a package
# with a path outside ci/rpm-allowlist.toml, a writable, setuid or non-root entry, an
# install script, a polkit policy whose sha256 is not the pinned one, or a helper or
# document reader that is not the file just built. A refused package is never copied to
# dist-arch/, so nothing publishes it.
python3 ci/arch_guard.py --root "$ROOT" --pkgver "${VERSION}-1" "$scratch/$FILE"

rm -rf dist-arch
mkdir dist-arch
cp "$scratch/$FILE" "dist-arch/$FILE"
SHA="$(sha256sum "dist-arch/$FILE" | cut -d' ' -f1)"
echo "$SHA  $FILE" > "dist-arch/$FILE.sha256"
cat > dist-arch/arch.json <<EOF
{
  "version": "${VERSION}",
  "file": "${FILE}",
  "sha256": "${SHA}"
}
EOF
rm -rf "$work"
echo "Built ${FILE} (sha256 ${SHA})"
