#!/usr/bin/env bash
# Runs inside the throwaway Python container of scripts/api-schemas-update.sh:
# installs the pinned Anthropic SDK from the wheel the host verified, dumps
# its schemas to /out, and records `pip freeze` beside them.
# Usage (container): bash /in/api_schemas_in_container.sh WHEEL_PATH PYDANTIC_VERSION
set -euo pipefail
pip install -q --disable-pip-version-check --root-user-action=ignore \
  "$1" "pydantic==$2" >/dev/null
pip freeze --disable-pip-version-check >/out/pip-freeze.txt
python3 -I /in/api_schemas_anthropic_dump.py /out/anthropic-sdk-schemas.json
