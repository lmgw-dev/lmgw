#!/usr/bin/env bash
# Usage: scripts/compat-live.sh [--base URL] [--preset default|kilo|direct | --models a,b] [--routes chat,messages,responses] [--scenarios id,..] [--modes plain,stream] [--jobs N] [--list]
# The live API compatibility check (docs/design/2026-11-04-api-compat-design.md): drives the
# scenario catalogue through the running lmgw with the official openai and anthropic SDKs and
# raw httpx, in a rootless Podman container, and writes target/compat/runs/<UTC>/results.json
# and report.html. The key comes from LMGW_API_KEY or ~/.config/lmgw/compat-key (mode 600) and
# reaches the container as an environment variable, never as an argument. The venv and pip cache
# live under target/compat/, so a rerun installs nothing. Exits 1 when a cell failed.
set -euo pipefail
cd "$(dirname "$0")/.."
root=$PWD

base=http://127.0.0.1:8001
args=("$@")
for ((i = 0; i < ${#args[@]}; i++)); do
  case "${args[i]}" in
    --base) base=${args[i + 1]:-$base} ;;
    --base=*) base=${args[i]#--base=} ;;
    -h | --help)
      sed -n '2,/^set -euo pipefail$/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'
      exit 0
      ;;
  esac
done

if [[ -z "${LMGW_API_KEY:-}" ]]; then
  keyfile=${COMPAT_KEY_FILE:-$HOME/.config/lmgw/compat-key}
  if [[ ! -f "$keyfile" ]]; then
    echo "compat-live: no key: set LMGW_API_KEY or write $keyfile (mode 600)" >&2
    exit 2
  fi
  if [[ "$(stat -c %a "$keyfile")" != 600 ]]; then
    echo "compat-live: $keyfile must be mode 600 (chmod 600 $keyfile)" >&2
    exit 2
  fi
  LMGW_API_KEY=$(<"$keyfile")
fi
export LMGW_API_KEY

# A loopback lmgw is reached through pasta's -T forward of that one port, as 127.0.0.1 inside.
hostport=${base#*://}
hostport=${hostport%%/*}
host=${hostport%:*}
port=${hostport##*:}
[[ "$port" == "$hostport" ]] && port=80
case "$host" in
  127.0.0.1 | localhost | "[::1]") network="pasta:-T,$port" ;;
  *) network=pasta ;;
esac

image=${COMPAT_IMAGE:-docker.io/library/python:3.12-slim}
mkdir -p target/compat/pip-cache target/compat/runs

# The checkout is mounted read-only and never relabelled: label=disable instead of :Z.
exec podman run --rm --init \
  --security-opt label=disable \
  --network="$network" \
  --env LMGW_API_KEY \
  --env PIP_CACHE_DIR=/compat/pip-cache \
  --env PIP_DISABLE_PIP_VERSION_CHECK=1 \
  --env PYTHONDONTWRITEBYTECODE=1 \
  --env COMPAT_IMAGE="$image" \
  --env COMPAT_HOST_RUNS="$root/target/compat/runs" \
  --volume "$root/scripts/compat_live:/src/compat_live:ro" \
  --volume "$root/target/compat:/compat" \
  --workdir /src \
  "$image" \
  sh -c '
    set -e
    tag="$(cat /src/compat_live/requirements.txt | sha256sum | cut -c1-12)-$(echo "$COMPAT_IMAGE" | sha256sum | cut -c1-8)"
    venv=/compat/venv-$tag
    if [ ! -f "$venv/.ready" ]; then
      echo "compat-live: setting up $venv" >&2
      python3 -m venv "$venv"
      "$venv/bin/pip" install -q -r /src/compat_live/requirements.txt
      touch "$venv/.ready"
    fi
    exec "$venv/bin/python" -m compat_live --out /compat/runs "$@"
  ' compat-live "$@"
