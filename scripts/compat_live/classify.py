"""What a non-2xx answer means for a cell: skipped, not supported, or a failure."""

import json

from .common import FAIL, SKIPPED, UNSUPPORTED

# lmgw's own refusal codes for "this model / route cannot do that" (crates/lmgw-core/src/
# error.rs kind()). On the OpenAI wire they are error.code; the Anthropic wire has no code,
# so the message prefixes lmgw writes for them are read instead.
UNSUPPORTED_CODES = {"unsupported", "response_format_unsupported"}
UNSUPPORTED_PREFIXES = ("unsupported:",)


def error_fields(body):
    """(code, message, type) from an OpenAI- or Anthropic-shaped error body, else Nones."""
    try:
        data = json.loads(body) if isinstance(body, str) else body
    except ValueError:
        return None, None, None
    err = data.get("error") if isinstance(data, dict) else None
    if not isinstance(err, dict):
        return None, None, None
    return err.get("code"), err.get("message"), err.get("type")


def classify(status, body):
    """(status, reason) for an answer that was not the 2xx the scenario wanted."""
    code, message, etype = error_fields(body)
    msg = message or (body[:300] if isinstance(body, str) else "")
    if status == 503 and code == "gpu_hold":
        return SKIPPED, f"GPU hold is on (503 gpu_hold): {msg}"
    if status == 503 and isinstance(message, str) and "GPU hold" in message:
        return SKIPPED, f"GPU hold is on (503): {msg}"
    if status == 429:
        return SKIPPED, f"rate limited (429): {msg}"
    if 400 <= status < 500 and (
        code in UNSUPPORTED_CODES
        or (isinstance(message, str) and message.lower().startswith(UNSUPPORTED_PREFIXES))
    ):
        return UNSUPPORTED, f"lmgw refused ({status} {code or etype}): {msg}"
    return FAIL, f"HTTP {status} ({code or etype or 'no error code'}): {msg}"
