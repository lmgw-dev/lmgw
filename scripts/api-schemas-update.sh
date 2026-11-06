#!/usr/bin/env bash
# Refreshes the providers' pinned API schemas the compatibility check reads
# (crates/lmgw-core/tests/fixtures/api_schemas/, tests/it/common/api_compat).
#
# Usage: bash scripts/api-schemas-update.sh [--latest]
#
#   (no flag)  re-fetches the sources pinned in SOURCES.json (same OpenAI
#              commit, same Anthropic SDK version) and re-extracts: a rerun
#              that changes nothing proves the extraction is reproducible.
#              Gemini's discovery document has no version to pin; it is the
#              live v1beta one, its revision and sha256 recorded.
#   --latest   moves the pins to the newest OpenAI spec commit and Anthropic
#              SDK release first.
#
# Manual tool, never run by the gate (it needs the network and Podman). It
# prints what changed per schema; review the fixture diff before committing.
# Downloads land in target/api-schemas-src/ (untracked, kept for a look).
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

fixtures=crates/lmgw-core/tests/fixtures/api_schemas
src=target/api-schemas-src
latest=false
[[ "${1:-}" == "--latest" ]] && latest=true

mkdir -p "$src" "$fixtures"
pin() { python3 -I -c 'import json,sys; print(json.load(open(sys.argv[1]))[sys.argv[2]][sys.argv[3]])' "$fixtures/SOURCES.json" "$1" "$2"; }

if $latest || [[ ! -f "$fixtures/SOURCES.json" ]]; then
  openai_commit=$(curl -fsS -m 30 "https://api.github.com/repos/openai/openai-openapi/commits?per_page=1" |
    python3 -I -c 'import json,sys; print(json.load(sys.stdin)[0]["sha"])')
  anthropic_version=$(curl -fsS -m 30 https://pypi.org/pypi/anthropic/json |
    python3 -I -c 'import json,sys; print(json.load(sys.stdin)["info"]["version"])')
  pydantic_version=$(curl -fsS -m 30 https://pypi.org/pypi/pydantic/json |
    python3 -I -c 'import json,sys; print(json.load(sys.stdin)["info"]["version"])')
else
  openai_commit=$(pin openai commit)
  anthropic_version=$(pin anthropic sdk_version)
  pydantic_version=$(pin anthropic pydantic_version)
fi

openai_url="https://raw.githubusercontent.com/openai/openai-openapi/$openai_commit/openapi.json"
gemini_url='https://generativelanguage.googleapis.com/$discovery/rest?version=v1beta'

echo "==> OpenAI spec at $openai_commit"
curl -fsS -m 300 -o "$src/openai-openapi.json" "$openai_url"
echo "==> Gemini v1beta discovery document"
curl -fsS -m 120 -o "$src/gemini-v1beta-discovery.json" "$gemini_url"

echo "==> Anthropic SDK $anthropic_version (pydantic $pydantic_version) in a throwaway container"
# The image is pinned by its index digest (python:3.12-slim on 2026-11-04;
# move it deliberately, by editing this line).
python_image=docker.io/library/python@sha256:b18b261e1b4974b4f321451ad5251ded85abb22c2b8b262da527dda1d031ab30
# The SDK's wheel comes down on the host and is checked against the digest
# PyPI publishes for it; the container installs that very file, so what the
# dump ran on is what was verified.
rm -rf "$src/in" "$src/anthropic-out"
mkdir -p "$src/in/wheels" "$src/anthropic-out"
wheel_json=$(curl -fsS -m 30 "https://pypi.org/pypi/anthropic/$anthropic_version/json")
read -r wheel_url anthropic_wheel < <(python3 -I -c '
import json, sys
u = next(u for u in json.load(sys.stdin)["urls"] if u["packagetype"] == "bdist_wheel")
print(u["url"], u["digests"]["sha256"])' <<<"$wheel_json")
wheel_file="${wheel_url##*/}"
curl -fsS -m 300 -o "$src/in/wheels/$wheel_file" "$wheel_url"
echo "$anthropic_wheel  $src/in/wheels/$wheel_file" | sha256sum -c --quiet - ||
  { echo "the downloaded wheel does not match PyPI's digest" >&2; exit 1; }
# The helpers and the wheel are copied into a directory of their own and that
# copy is relabelled (:Z), never the checkout's scripts/.
cp scripts/api_schemas_anthropic_dump.py scripts/api_schemas_in_container.sh "$src/in/"
podman run --rm \
  -v "$PWD/$src/in:/in:ro,Z" \
  -v "$PWD/$src/anthropic-out:/out:Z" \
  "$python_image" \
  bash /in/api_schemas_in_container.sh "/in/wheels/$wheel_file" "$pydantic_version"
anthropic_source="https://pypi.org/project/anthropic/$anthropic_version/"

echo "==> extract"
python3 -I scripts/api_schemas_extract.py \
  --openai "$src/openai-openapi.json" --openai-url "$openai_url" --openai-commit "$openai_commit" \
  --anthropic "$src/anthropic-out/anthropic-sdk-schemas.json" --anthropic-url "$anthropic_source" \
  --anthropic-version "$anthropic_version" --pydantic-version "$pydantic_version" \
  --anthropic-wheel-sha256 "$anthropic_wheel" \
  --anthropic-image "$python_image" --pip-freeze "$src/anthropic-out/pip-freeze.txt" \
  --gemini "$src/gemini-v1beta-discovery.json" --gemini-url "$gemini_url" \
  --out "$fixtures"

echo
git status --short -- "$fixtures"
