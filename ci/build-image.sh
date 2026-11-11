#!/usr/bin/env bash
# Build the lmgw server image (headless server design §3.1, §10) from packaging/server/ and the
# release lmgw-server binary, and write it as an OCI archive, the image's only form (ruling 11):
#
#   dist/lmgw-server-<version>.oci.tar          (+ .sha256)
#
# inside it the reference localhost/lmgw-server:<tag>, where <tag> is the version with "+"
# written as "_" (an image tag cannot hold a "+"). An administrator loads it with
# `podman load -i …` and tags it localhost/lmgw-server:current (packaging/server/lmgw.container).
# Nothing is pushed anywhere: publishing an image is the owner's call, never a script's.
#
# The binary comes from `LMGW_BUILD_SERVER=only bash ci/build.sh` (dist/lmgw-server, and
# dist/build.env says its version and the Fedora release it was built on) or from the path given;
# the image's Fedora is that release (FEDORA_RELEASE overrides it), so the binary finds the glibc
# and libopus it linked. It must be stripped, as the release profile does (`strip = true`): a
# binary with a symbol table is a debug or unstripped build and is refused.
#
# buildah runs with its own store under target/image-build/ (vfs, chroot isolation), so it needs
# no privileges beyond an unprivileged build and never touches the caller's Podman store. That
# store keeps the base image between runs; this script removes the image it built once the
# archive is written. To drop the whole store: buildah unshare rm -rf target/image-build
#
# Usage: bash ci/build-image.sh [path/to/lmgw-server]
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
umask 022

die() {
  echo "ERROR: $*" >&2
  exit 1
}

command -v buildah >/dev/null || die "buildah is not installed (dnf install buildah)"
command -v jq >/dev/null || die "jq is not installed (dnf install jq)"
command -v readelf >/dev/null || die "readelf is not installed (dnf install binutils)"

BIN="${1:-dist/lmgw-server}"
[[ -f "$BIN" && -x "$BIN" ]] \
  || die "no executable lmgw-server at ${BIN} (LMGW_BUILD_SERVER=only bash ci/build.sh puts it in dist/)"
SECTIONS="$(readelf -SW "$BIN")" || die "readelf cannot read ${BIN}"
if grep -qE '[[:space:]]\.symtab[[:space:]]' <<<"$SECTIONS"; then
  die "${BIN} has a symbol table (.symtab): not a stripped release build (the release profile strips)"
fi

# The version is the binary's own; ci/build.sh's record of it must agree.
LINE="$("$BIN" --version)"
VERSION="${LINE#lmgw-server }"
[[ "$LINE" == "lmgw-server "* && "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+(\+[0-9]+)?$ ]] \
  || die "${BIN} --version printed '${LINE}', not 'lmgw-server X.Y.Z[+N]'"

env_value() { # <key>: its value in dist/build.env, empty when absent
  sed -n "s/^$1=//p" dist/build.env 2>/dev/null | tail -n1
}
RELEASE="${FEDORA_RELEASE:-}"
if [[ "$BIN" == dist/lmgw-server && -f dist/build.env ]]; then
  RECORDED="$(env_value VERSION)"
  [[ "$RECORDED" == "$VERSION" ]] \
    || die "dist/build.env says VERSION=${RECORDED}, but ${BIN} is ${VERSION}"
  RELEASE="${RELEASE:-$(env_value SERVER_FEDORA_RELEASE)}"
  [[ -n "$RELEASE" ]] && echo "Fedora ${RELEASE}: the release ci/build.sh built the binary on"
fi
if [[ -z "$RELEASE" ]]; then
  # A binary built here, so this system's release is the one it linked against.
  # shellcheck disable=SC1091
  RELEASE="$(. /etc/os-release && [[ "${ID:-}" == fedora ]] && echo "${VERSION_ID:-}")" || true
  [[ -n "$RELEASE" ]] \
    || die "the Fedora release the binary was built on is unknown: set FEDORA_RELEASE"
  echo "Fedora ${RELEASE}: this system's release, where ${BIN} was built (FEDORA_RELEASE overrides)"
fi
[[ "$RELEASE" =~ ^[0-9]+$ ]] || die "FEDORA_RELEASE '${RELEASE}' is not a Fedora release number"

TAG="${VERSION//+/_}"
REF="localhost/lmgw-server:${TAG}"
COMMIT="${CI_COMMIT_SHORT_SHA:-$(git rev-parse --short HEAD 2>/dev/null || echo unknown)}"
ARCHIVE="dist/lmgw-server-${VERSION}.oci.tar"
# The labels name lmgw's public repository (README, Cargo metadata) over the base image's Fedora
# ones. The base's org.opencontainers.image.licenses (Fedora's, "MIT") is removed rather than
# replaced: the label is one SPDX expression for the whole image, which is Fedora's packages
# under their many licences plus lmgw (MIT), and OCI has no way to scope it to one part. lmgw's
# own notices are in /usr/share/licenses/lmgw-server/.
PROJECT_URL="https://github.com/lmgw-dev/lmgw"

# The build context: packaging/server/'s image files (not the unit and volumes, which go to the
# host), NVIDIA's key from the one place lmgw keeps it, the licence notices the RPM ships beside
# the same code (tauri.conf.json's /usr/share/licenses/lmgw/ files), the workspace licence, and
# the binary. Nothing else from the checkout reaches the build. A test
# (crates/lmgw-server/tests/it/packaging.rs) keeps this list equal to the Containerfile's COPYs.
WORK="$PWD/target/image-build"
CTX="$WORK/context"
rm -rf "$CTX"
mkdir -p "$CTX/licenses" dist
cp packaging/server/{Containerfile,entrypoint.sh,containers.conf,storage.conf,nvidia-container-toolkit.repo} "$CTX/"
chmod 0755 "$CTX/entrypoint.sh"
cp crates/lmgw-machine/src/plan/nvidia-container-toolkit.asc "$CTX/RPM-GPG-KEY-lmgw-nvidia-container-toolkit"
cp LICENSE.md "$CTX/licenses/LICENSE.md"
while IFS=$'\t' read -r dest src; do
  rel="${dest#/usr/share/licenses/lmgw/}"
  mkdir -p "$CTX/licenses/$(dirname "$rel")"
  cp "src-tauri/${src}" "$CTX/licenses/${rel}"
done < <(jq -r '.bundle.linux.rpm.files | to_entries[]
                | select(.key | startswith("/usr/share/licenses/lmgw/"))
                | "\(.key)\t\(.value)"' src-tauri/tauri.conf.json)
[[ -n "$(find "$CTX/licenses" -mindepth 2 -type f -print -quit)" ]] \
  || die "no licence notices found in src-tauri/tauri.conf.json's rpm files"
install -m 0755 "$BIN" "$CTX/lmgw-server"

B=(buildah --root "$WORK/storage" --runroot "$WORK/run" --storage-driver vfs)
# buildah's own scratch (the build context's overlay, the archive's staging) beside its store: a
# CI job's /var/tmp is the job container's overlay filesystem, and an overlay's upper dir cannot
# sit on another overlay (EINVAL).
export TMPDIR="$WORK/tmp"
mkdir -p "$TMPDIR"
CTR=""
cleanup() {
  if [[ -n "$CTR" ]]; then "${B[@]}" rm "$CTR" >/dev/null 2>&1 || true; fi
}
trap cleanup EXIT

echo "Building ${REF} on fedora:${RELEASE} (${COMMIT})"
"${B[@]}" bud --isolation chroot --pull=always --layers=false --force-rm \
  --build-arg "FEDORA_RELEASE=${RELEASE}" \
  --label "org.opencontainers.image.title=lmgw-server" \
  --label "org.opencontainers.image.version=${VERSION}" \
  --label "org.opencontainers.image.revision=${COMMIT}" \
  --label "org.opencontainers.image.vendor=lmgw-dev" \
  --label "org.opencontainers.image.url=${PROJECT_URL}" \
  --label "org.opencontainers.image.source=${PROJECT_URL}" \
  --unsetlabel "org.opencontainers.image.licenses" \
  --iidfile "$WORK/iid" -t "$REF" -f "$CTX/Containerfile" "$CTX"
IID="$(cat "$WORK/iid")"

# The binary runs inside the image it was built for: its libraries resolve there. The init, the
# entrypoint and the inner Podman are there and start.
CTR="$("${B[@]}" from --pull=never "$IID")"
inside() { "${B[@]}" run --isolation chroot "$CTR" -- "$@"; }
INSIDE="$(inside /usr/local/bin/lmgw-server --version)"
[[ "$INSIDE" == "$LINE" ]] || die "inside the image lmgw-server --version printed '${INSIDE}', not '${LINE}'"
inside test -x /usr/libexec/catatonit/catatonit || die "the image has no executable /usr/libexec/catatonit/catatonit"
inside bash -n /usr/local/bin/entrypoint.sh || die "the image's entrypoint.sh does not parse"
PODMAN="$(inside podman --version)" || die "podman --version fails inside the image"
"${B[@]}" rm "$CTR" >/dev/null
CTR=""
echo "In the image: ${INSIDE}, ${PODMAN}, catatonit, the entrypoint parses"

rm -f "$ARCHIVE" "$ARCHIVE.sha256"
"${B[@]}" push "$IID" "oci-archive:${ARCHIVE}:${REF}"
(cd dist && sha256sum "$(basename "$ARCHIVE")" > "$(basename "$ARCHIVE").sha256")
# The archive is complete; a store that keeps the image is only disk, so a failed removal warns.
"${B[@]}" rmi "$IID" >/dev/null || echo "WARNING: could not remove ${IID} from ${WORK}/storage" >&2

echo "Wrote ${ARCHIVE} ($(du -m "$ARCHIVE" | cut -f1) MiB), image reference ${REF}"
cat "$ARCHIVE.sha256"
