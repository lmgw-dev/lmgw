#!/usr/bin/env bash
# Build the folder-chat agent image; `--install` also installs it into a running lmgw, `--start` then starts the app.
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: build.sh [--install | --start]

Builds the folder-chat binary on the host (cargo build --release -p
folder-chat), bakes it into a localhost/folder-chat:<version> image with the
Containerfile in this directory, and, with --install, installs that image
into a running lmgw via POST /api/op/agent_install. A running app is stopped
first (agent_service_stop), so no container keeps serving the old image; it
starts again on the next Start or App tab. --start installs the same way and
then starts the app (agent_service_start), which runs its start-up sync.

The binary is built on the host and runs against the image's glibc, so the
host's Fedora release must equal the Containerfile's base image tag
(fedora-minimal:<release>); the build refuses otherwise.

Env:
  FOLDER_CHAT_ALLOW_RELEASE_MISMATCH=1
                  Build even though the host's Fedora release and the base
                  image tag differ (the binary may then fail to start in the
                  image with a glibc version error).
  LMGW_URL        (--install, --start) Base URL of the gateway.
                  Default: http://127.0.0.1:8787
  LMGW_OWNER_KEY  (--install, --start) Owner bearer token. Required. Copy it from
                  the dashboard's Usage -> Keys page (the owner:dashboard
                  row's Copy button), or read the "dashboard login:" line
                  lmgw prints to its own log at startup. It is passed to
                  curl on stdin, never on its command line.
EOF
}

install=0
start=0
for arg in "$@"; do
  case "$arg" in
    --install) install=1 ;;
    --start)
      install=1
      start=1
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      echo "build.sh: unknown argument: $arg" >&2
      usage >&2
      exit 1
      ;;
  esac
done

command -v jq >/dev/null 2>&1 || { echo "build.sh: jq is required (dnf install jq)" >&2; exit 1; }
command -v podman >/dev/null 2>&1 || { echo "build.sh: podman is required" >&2; exit 1; }

# Resolve every path from the script's own location so this runs the same
# from anywhere, not only from this directory or the repo root.
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$here/../../.." && pwd)"
manifest="$here/agent.json"

version="$(jq -r '.version' "$manifest")"
image="localhost/folder-chat:${version}"
declared_image="$(jq -r '.run.image' "$manifest")"
if [[ "$declared_image" != "$image" ]]; then
  echo "build.sh: agent.json's version ('$version') and run.image ('$declared_image') disagree;" >&2
  echo "          the built tag must equal run.image exactly. Fix one or the other." >&2
  exit 1
fi

# The binary is built here and runs in the image: it must link against the
# glibc the image ships, so the host's Fedora release has to be the base
# image's. A binary built on a newer release can refuse to start in an older
# image ("GLIBC_2.xx not found"), and nothing before the container start would
# say so.
containerfile="$here/Containerfile"
base_release="$(sed -nE 's|^FROM[[:space:]]+[^[:space:]]*/fedora-minimal:([^[:space:]]+).*|\1|p' "$containerfile" | head -n1)"
host_id="$( (. /etc/os-release && printf '%s' "${ID:-}") 2>/dev/null || true)"
host_release="$( (. /etc/os-release && printf '%s' "${VERSION_ID:-}") 2>/dev/null || true)"
if [[ -z "$base_release" ]]; then
  echo "build.sh: cannot read the fedora-minimal release from $containerfile's FROM line" >&2
  exit 1
fi
if [[ "$host_id" != "fedora" || "$host_release" != "$base_release" ]]; then
  if [[ "${FOLDER_CHAT_ALLOW_RELEASE_MISMATCH:-}" == "1" ]]; then
    echo "build.sh: warning: host is ${host_id:-unknown} ${host_release:-unknown}, the image is" >&2
    echo "          fedora-minimal:$base_release; building anyway (FOLDER_CHAT_ALLOW_RELEASE_MISMATCH=1)" >&2
  else
    echo "build.sh: this host is ${host_id:-unknown} ${host_release:-unknown}, but the Containerfile's" >&2
    echo "          base image is fedora-minimal:$base_release. The binary is built here and" >&2
    echo "          must link against the image's glibc, so the two releases must match." >&2
    echo "          Build on Fedora $base_release, change the FROM line to this host's release," >&2
    echo "          or set FOLDER_CHAT_ALLOW_RELEASE_MISMATCH=1 to build anyway." >&2
    exit 1
  fi
fi

echo "build.sh: cargo build --release -p folder-chat"
(cd "$repo_root" && cargo build --release -p folder-chat)

binary="$repo_root/target/release/folder-chat"
if [[ ! -x "$binary" ]]; then
  echo "build.sh: $binary was not produced by the build" >&2
  exit 1
fi

# A throwaway build context, never the repo root — the repo root would send
# target/ (gigabytes, and the binary itself, twice over) to the podman build.
stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
cp "$binary" "$stage/folder-chat"
cp "$manifest" "$stage/agent.json"
cp "$containerfile" "$stage/Containerfile"

echo "build.sh: podman build -t $image"
podman build -t "$image" "$stage"
echo "build.sh: built $image"

if [[ "$install" -eq 1 ]]; then
  url="${LMGW_URL:-http://127.0.0.1:8787}"
  if [[ -z "${LMGW_OWNER_KEY:-}" ]]; then
    echo "build.sh: --install needs LMGW_OWNER_KEY set to an owner bearer token." >&2
    echo "          Copy it from Usage -> Keys (the owner:dashboard row's Copy button)," >&2
    echo "          or read the 'dashboard login:' line lmgw printed to its log at startup." >&2
    exit 1
  fi
  agent_id="$(jq -r '.id' "$manifest")"
  # One POST to an /api/op route. The bearer goes in as a header file on stdin
  # (`-H @-`; printf is a bash builtin), so it never shows in `ps`.
  # --fail-with-body: a 401 or 409 prints lmgw's answer and fails the script
  # instead of passing as a success.
  op() {
    local name=$1 body=$2
    echo "build.sh: POST $url/api/op/$name"
    if ! printf 'Authorization: Bearer %s\n' "$LMGW_OWNER_KEY" |
      curl -sS --fail-with-body -X POST "$url/api/op/$name" \
        -H @- \
        -H 'content-type: application/json' \
        -d "$body"; then
      echo >&2
      echo "build.sh: $name failed; lmgw's answer is above" >&2
      exit 1
    fi
    echo
  }
  id_body="$(jq -n --arg id "$agent_id" '{id: $id}')"
  # A running app would go on serving the image it was started from.
  op agent_service_stop "$id_body"
  op agent_install "$(jq -n --arg image "$image" '{image: $image, replace: true}')"
  if [[ "$start" -eq 1 ]]; then
    op agent_service_start "$id_body"
  fi
fi
