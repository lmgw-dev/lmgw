#!/usr/bin/env bash
# Upload the built RPM (under its version) and the updater manifest (under the
# mutable `latest`) to this project's GitLab generic package registry, using the
# pipeline's CI_JOB_TOKEN. Consumes dist/ produced by ci/build.sh. The manifest
# is skipped (with a log line) when the published one is already newer.
#
# `windows` (Windows design §5, WP7): upload the Windows installer the
# windows-installer job built (dist-windows/: the installer and nsis.json, from
# ci/build-windows.ps1) under its version, beside the RPM, and add it to the
# published latest.json as its `nsis` entry when that manifest is this
# installer's version; a newer one (a later pipeline published first) is left
# alone, with a log line. Runs after `publish`, so the RPM's manifest is there.
#
# Usage: bash ci/publish.sh            the RPM and latest.json
#        bash ci/publish.sh windows    the Windows installer, then latest.json again
set -euo pipefail

mode="${1:-rpm}"
case "$mode" in
  rpm | windows) ;;
  *) echo "publish.sh: unknown mode '$mode' (none for the RPM, or 'windows')" >&2; exit 2 ;;
esac

base="${CI_API_V4_URL}/projects/${CI_PROJECT_ID}/packages/generic/lmgw"
latest_url="${base}/latest/latest.json"

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

# The currently published latest.json, written to $2: true, or false on a
# first publish (404). Any other failure aborts the publish.
fetch_published() {
  local url="$1" out="$2" status
  status="$(curl --silent --show-error --output "$out" --write-out '%{http_code}' \
    -H @<(printf 'JOB-TOKEN: %s\n' "$CI_JOB_TOKEN") "$url")" || {
    echo "Could not fetch the published manifest from ${url}" >&2
    exit 1
  }
  case "$status" in
    404) return 1 ;;
    200) return 0 ;;
    *) echo "Fetching the published manifest returned HTTP ${status}" >&2
       exit 1 ;;
  esac
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

if [[ "$mode" == windows ]]; then
  fail() { echo "publish.sh windows: $*" >&2; exit 1; }
  m=dist-windows/nsis.json
  [[ -f "$m" ]] || fail "the windows-installer job handed on no $m"
  version="$(jq -er .version "$m")" || fail "$m has no version"
  file="$(jq -er .file "$m")" || fail "$m names no installer"
  sha="$(jq -er .sha256 "$m")" || fail "$m has no sha256"
  [[ "$file" == "lmgw_${version}_x64-setup.exe" && "$file" =~ ^lmgw_[0-9.+]+_x64-setup\.exe$ ]] \
    || fail "$m names $file, not lmgw ${version}'s installer"
  [[ "$sha" =~ ^[0-9a-f]{64}$ ]] || fail "$m's sha256 is not 64 hex digits: $sha"
  [[ -f "dist-windows/$file" && ! -L "dist-windows/$file" ]] || fail "dist-windows/$file is not a plain file"
  have="$(sha256sum "dist-windows/$file" | cut -d' ' -f1)"
  [[ "$have" == "$sha" ]] || fail "$file's sha256 is $have, $m says $sha"

  url="${base}/${version}/${file}"
  upload "dist-windows/${file}" "$url"

  published="$(mktemp)"
  if ! fetch_published "$latest_url" "$published"; then
    echo "Skipping latest.json: none is published yet, so there is no ${version} to add the installer to"
    exit 0
  fi
  current="$(jq -er .version "$published")" || fail "the published manifest has no readable version"
  if [[ "$current" != "$version" ]]; then
    echo "Skipping latest.json: the published one is ${current}, this installer ${version}"
    exit 0
  fi
  jq --arg file "$file" --arg url "$url" --arg sha "$sha" \
    '.nsis = {file: $file, url: $url, sha256: $sha}' "$published" > dist-windows/latest.json
  upload dist-windows/latest.json "$latest_url"
  echo "Published the lmgw ${version} installer for Windows"
  exit 0
fi

# shellcheck disable=SC1091
source dist/build.env   # VERSION, RPM_NAME

# Versioned RPM: always uploaded.
upload "dist/${RPM_NAME}" "${base}/${VERSION}/${RPM_NAME}"

# `latest/latest.json` is overwritten, the newest upload wins on GET. It never
# goes backwards: an older pipeline finishing late must not roll the feed back.
# Requires the project to allow duplicate generic packages (the default). If
# duplicates are rejected the upload fails loudly here rather than silently.
current="$(published_version "$latest_url")"
if [[ -n "$current" ]] && version_gt "$current" "$VERSION"; then
  echo "Skipping latest.json: the published ${current} is newer than ${VERSION}"
else
  upload "dist/latest.json" "$latest_url"
fi

echo "Published lmgw ${VERSION}"
