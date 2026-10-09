#!/usr/bin/env bash
# Upload the built RPM (under its version) and the updater manifest (under the
# mutable `latest`) to this project's GitLab generic package registry, using the
# pipeline's CI_JOB_TOKEN. Consumes dist/ produced by ci/build.sh. The manifest
# is skipped (with a log line) when the published one is already newer.
#
# Usage: bash ci/publish.sh
set -euo pipefail

# shellcheck disable=SC1091
source dist/build.env   # VERSION, RPM_NAME

base="${CI_API_V4_URL}/projects/${CI_PROJECT_ID}/packages/generic/lmgw"

# The token goes in through a process substitution, never in curl's argv
# (which other processes on the host can read).
upload() { # <local file> <registry url>
  echo "Uploading -> $2"
  curl --fail-with-body --silent --show-error \
    -H @<(printf 'JOB-TOKEN: %s\n' "$CI_JOB_TOKEN") \
    --upload-file "$1" "$2"
  echo
}

# Split X.Y.Z[+N] into "X Y Z N"; a version without +N has N = -1, so it
# sorts before the same X.Y.Z with any build counter.
version_key() {
  local v="$1" core build=-1
  core="${v%%+*}"
  [[ "$v" == *+* ]] && build="${v#*+}"
  local IFS=.
  # shellcheck disable=SC2086
  set -- $core
  echo "${1:-0} ${2:-0} ${3:-0} ${build}"
}

# True (0) when version $1 sorts strictly after version $2.
version_gt() {
  local a b i
  read -ra a <<<"$(version_key "$1")"
  read -ra b <<<"$(version_key "$2")"
  for i in 0 1 2 3; do
    if ((a[i] > b[i])); then return 0; fi
    if ((a[i] < b[i])); then return 1; fi
  done
  return 1
}

# The version of the currently published latest.json: prints it, or nothing on
# a first publish (404). Any other failure aborts the publish.
published_version() {
  local url="$1" out status
  out="$(mktemp)"
  status="$(curl --silent --show-error --output "$out" --write-out '%{http_code}' \
    -H @<(printf 'JOB-TOKEN: %s\n' "$CI_JOB_TOKEN") "$url")" || {
    echo "Could not fetch the published manifest from ${url}" >&2
    rm -f "$out"
    return 1
  }
  case "$status" in
    404) rm -f "$out"; return 0 ;;
    200) jq -er '.version' "$out" || {
           echo "The published manifest has no readable version" >&2
           rm -f "$out"
           return 1
         }
         rm -f "$out" ;;
    *) echo "Fetching the published manifest returned HTTP ${status}" >&2
       rm -f "$out"
       return 1 ;;
  esac
}

# Versioned RPM: always uploaded.
upload "dist/${RPM_NAME}" "${base}/${VERSION}/${RPM_NAME}"

# `latest/latest.json` is overwritten, the newest upload wins on GET. It never
# goes backwards: an older pipeline finishing late must not roll the feed back.
# Requires the project to allow duplicate generic packages (the default). If
# duplicates are rejected the upload fails loudly here rather than silently.
latest_url="${base}/latest/latest.json"
current="$(published_version "$latest_url")"
if [[ -n "$current" ]] && version_gt "$current" "$VERSION"; then
  echo "Skipping latest.json: the published ${current} is newer than ${VERSION}"
else
  upload "dist/latest.json" "$latest_url"
fi

echo "Published lmgw ${VERSION}"
