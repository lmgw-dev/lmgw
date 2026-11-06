"""Strict parsing of the captured wire with the SDKs' own pydantic types.

The SDKs build their response objects without validation (`construct`), so a wrong type
passes them silently; validating the same JSON against the same types catches it.
"""

import json
import typing

import pydantic
from anthropic import types as at
from anthropic.types import beta as atb
from openai.types import chat as oc
from openai.types import responses as orsp


def _by_type(union):
    """{type literal: model} for a discriminated union of event models."""
    out = {}
    stack = [union]
    while stack:
        t = stack.pop()
        args = typing.get_args(t)
        if isinstance(t, type) and issubclass(t, pydantic.BaseModel):
            ann = t.model_fields.get("type")
            for lit in typing.get_args(ann.annotation) if ann else ():
                out[lit] = t
        else:
            stack.extend(a for a in args if not isinstance(a, (str, pydantic.fields.FieldInfo)))
    return out


_ADAPTERS = {
    "chat": oc.ChatCompletion,
    "chat.chunk": oc.ChatCompletionChunk,
    "messages": at.Message,
    "messages.event": _by_type(at.RawMessageStreamEvent),
    "messages_count": at.MessageTokensCount,
    # A beta request's answer (compaction): the SDK's beta types know its blocks.
    "messages.beta": atb.BetaMessage,
    "messages.beta.event": _by_type(atb.BetaRawMessageStreamEvent),
    "responses": orsp.Response,
    "responses.event": _by_type(orsp.ResponseStreamEvent),
}


def _validate(kind, data):
    a = _ADAPTERS[kind]
    if isinstance(a, dict):
        a = a.get(data.get("type"))
        if a is None:
            return [f"sdk.schema.{kind}: the SDK knows no event type {data.get('type')!r}"]
    try:
        a.model_validate(data)
    except pydantic.ValidationError as e:
        errs = e.errors()[:3]
        where = "; ".join(f"{'.'.join(map(str, x['loc']))}: {x['msg']}" for x in errs)
        more = f" (+{len(e.errors()) - 3} more)" if len(e.errors()) > 3 else ""
        return [f"sdk.schema.{kind}: {where}{more}"]
    return []


def body(route, data):
    return _validate(route, data)


def stream(route, events):
    kind = {"chat": "chat.chunk", "messages": "messages.event", "messages.beta": "messages.beta.event",
            "responses": "responses.event"}[route]
    out = []
    for n, (_, data) in enumerate(events):
        if data == "[DONE]":
            continue
        try:
            ev = json.loads(data)
        except ValueError:
            continue
        if kind.startswith("messages.") and ev.get("type") in ("ping", "error"):
            continue
        for v in _validate(kind, ev):
            out.append(f"{v} (event {n}, {ev.get('type') or ev.get('object')})")
    return out
