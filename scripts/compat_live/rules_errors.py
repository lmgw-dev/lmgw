"""Fixed rules for error bodies: OpenAI's {error:{message,type,param,code}} and Anthropic's
{type:"error", error:{type,message}}."""

import json

from .common import Rules

ANTHROPIC_TYPES = {
    "invalid_request_error", "authentication_error", "billing_error", "permission_error",
    "not_found_error", "request_too_large", "rate_limit_error", "api_error",
    "overloaded_error", "timeout_error",
}


def _load(r, body):
    try:
        data = json.loads(body)
    except (ValueError, TypeError):
        r.fail("json", f"the error body is not JSON: {str(body)[:120]!r}")
        return None
    if not r.need(isinstance(data, dict), "body", "the error body is not an object"):
        return None
    return data


def openai(body, content_type=""):
    r = Rules("openai.error")
    r.need("application/json" in content_type, "content_type", f"content-type is {content_type!r}")
    data = _load(r, body)
    if data is None:
        return r.out
    err = r.typed(data, "error", dict)
    if err is not None:
        r.typed(err, "message", str, "message", "error.")
        r.typed(err, "type", str, "type", "error.")
        for k in ("param", "code"):
            r.need(k in err, k, f"error.{k} is missing (null when it does not apply)")
            r.need(err.get(k) is None or isinstance(err.get(k), str), k, f"error.{k} should be a string or null")
    return r.out


def anthropic(body, content_type=""):
    r = Rules("anthropic.error")
    r.need("application/json" in content_type, "content_type", f"content-type is {content_type!r}")
    data = _load(r, body)
    if data is None:
        return r.out
    r.need(data.get("type") == "error", "type", f"type is {data.get('type')!r}, not 'error'")
    err = r.typed(data, "error", dict)
    if err is not None:
        r.typed(err, "message", str, "message", "error.")
        t = err.get("type")
        r.need(t in ANTHROPIC_TYPES, "error.type", f"error.type {t!r} is not one of Anthropic's")
    return r.out


EXPECTED_ANTHROPIC_TYPE = {400: "invalid_request_error", 401: "authentication_error",
                           403: "permission_error", 404: "not_found_error",
                           413: "request_too_large", 429: "rate_limit_error"}


def anthropic_type_for(status, body):
    """A violation when the error.type does not match Anthropic's own type for the status."""
    want = EXPECTED_ANTHROPIC_TYPE.get(status)
    try:
        got = json.loads(body)["error"]["type"]
    except (ValueError, KeyError, TypeError):
        return []
    if want and got != want:
        return [f"anthropic.error.status_type: HTTP {status} should carry {want}, got {got!r}"]
    return []
