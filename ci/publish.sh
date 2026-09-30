#!/usr/bin/env bash
# Upload the built RPM (under its version) and the updater manifest (under the
# mutable `latest`) to this project's GitLab generic package registry, using the
# pipeline's CI_JOB_TOKEN. Consumes dist/ produced by ci/build.sh.
#
# Usage: bash ci/publish.sh
set -euo pipefail

# shellcheck disable=SC1091
source dist/build.env   # VERSION, RPM_NAME

base="${CI_API_V4_URL}/projects/${CI_PROJECT_ID}/packages/generic/lmgw"

upload() { # <local file> <registry url>
  echo "Uploading -> $2"
  curl --fail-with-body --silent --show-error \
    --header "JOB-TOKEN: ${CI_JOB_TOKEN}" \
    --upload-file "$1" "$2"
  echo
}

upload "dist/${RPM_NAME}" "${base}/${VERSION}/${RPM_NAME}"
# `latest/latest.json` is overwritten every build; the newest upload wins on GET.
# Requires the project to allow duplicate generic packages (the default). If
# duplicates are rejected the upload fails loudly here rather than silently.
upload "dist/latest.json" "${base}/latest/latest.json"

echo "Published lmgw ${VERSION}"
