"""Dumps the Anthropic Messages API's JSON Schemas from the official Python SDK.

Run by scripts/api-schemas-update.sh inside a throwaway Python container with
`anthropic` and `pydantic` installed; not meant to run on the host. Anthropic
publishes no standalone OpenAPI document (the Stainless `openapi_spec_url` its
SDKs' .stats.yml named until 2026-09 points at a bucket that no longer
exists), so the SDK's own request TypedDicts and response models are the
closest machine-readable statement of the wire: pydantic writes each as a
JSON Schema (draft 2020-12, `$defs` + local `$ref`s).

Usage: python3 -I api_schemas_anthropic_dump.py OUT.json
"""

import json
import sys
from importlib.metadata import version

import anthropic.types as t
from anthropic.pagination import SyncPage
from anthropic.types import message_count_tokens_params, message_create_params
from pydantic import TypeAdapter

# What lmgw's Anthropic egress sends and its /v1/messages ingress answers.
ENTRIES = {
    "messages_request": message_create_params.MessageCreateParamsNonStreaming,
    "messages_request_streaming": message_create_params.MessageCreateParamsStreaming,
    "count_tokens_request": message_count_tokens_params.MessageCountTokensParams,
    "message": t.Message,
    "stream_event": t.RawMessageStreamEvent,
    "count_tokens_response": t.MessageTokensCount,
    "error_response": t.ErrorResponse,
    # GET /v1/models for a client that says `anthropic-version`: the page
    # the SDK's `models.list()` reads.
    "models_response": SyncPage[t.ModelInfo],
}


def main(out):
    entries = {}
    for name, ty in ENTRIES.items():
        # Request TypedDicts validate what lmgw sends; the response models what
        # it answers, as a client reads them (serialization mode: the JSON an
        # SDK model is, not what its constructor coerces).
        mode = "validation" if name.endswith("_request") or "_request_" in name else "serialization"
        entries[name] = TypeAdapter(ty).json_schema(mode=mode, ref_template="#/$defs/{model}")
    json.dump(
        {
            "sdk": {"anthropic": version("anthropic"), "pydantic": version("pydantic")},
            "entries": entries,
        },
        open(out, "w"),
        indent=1,
        sort_keys=True,
    )


if __name__ == "__main__":
    main(sys.argv[1])
