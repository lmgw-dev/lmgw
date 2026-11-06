"""The /v1/responses route: request bodies, parsing, replay of output items as input items."""

import json

from . import rules_errors, rules_responses
from .common import Call, Turn, json_object

NAME = "responses"
PATH = "/v1/responses"
DIALECT = "openai"
MIN_OUTPUT = 16  # OpenAI refuses max_output_tokens below 16


def new(system=None):
    return {"instructions": system, "input": [], "new": []}


def _add(st, item):
    st["input"].append(item)
    st["new"].append(item)


def user(st, text, image_png_b64=None):
    content = [{"type": "input_text", "text": text}]
    if image_png_b64:
        content.append({"type": "input_image", "image_url": f"data:image/png;base64,{image_png_b64}"})
    _add(st, {"type": "message", "role": "user", "content": content})


def body(st, model, stream, o):
    prev = o.get("previous_response_id")
    b = {"model": model, "input": list(st["new"] if prev else st["input"]),
         "max_output_tokens": max(o["max_tokens"], MIN_OUTPUT)}
    if prev:
        b["previous_response_id"] = prev
    if st.get("instructions"):
        b["instructions"] = st["instructions"]
    if o.get("temperature") is not None:
        b["temperature"] = o["temperature"]
    if o.get("tools"):
        b["tools"] = [{"type": "function", "name": t["name"], "description": t["description"],
                       "parameters": t["parameters"], "strict": False} for t in o["tools"]]
    tc = o.get("tool_choice")
    if isinstance(tc, dict):
        b["tool_choice"] = {"type": "function", "name": tc["name"]}
    elif tc:
        b["tool_choice"] = tc
    if o.get("schema"):
        s = o["schema"]
        b["text"] = {"format": {"type": "json_schema", "name": s["name"], "schema": s["schema"], "strict": True}}
    if o.get("reasoning"):
        b["reasoning"] = {"effort": "high", "summary": "auto"}
        b["include"] = ["reasoning.encrypted_content"]
    if stream:
        b["stream"] = True
    st["new"] = []
    return b


def parse(data):
    st, reason = data.get("status"), (data.get("incomplete_details") or {}).get("reason")
    out = data.get("output") or []
    t = Turn(response_id=data.get("id") or "")
    for it in out:
        if it.get("type") == "message":
            t.text += "".join(p.get("text", "") for p in it.get("content") or [] if p.get("type") == "output_text")
        elif it.get("type") == "function_call":
            args, _ = json_object(it.get("arguments"))
            t.calls.append(Call(it.get("call_id", ""), it.get("name", ""), it.get("arguments"), args))
        elif it.get("type") == "reasoning":
            t.reasoning = True
    if st == "incomplete":
        t.finish_raw = f"incomplete:{reason}"
        t.finish = "length" if reason == "max_output_tokens" else ("filter" if reason == "content_filter" else f"other:{reason}")
    elif st == "completed":
        t.finish_raw = "completed"
        t.finish = "tool" if t.calls else "stop"
    else:
        t.finish_raw = t.finish = f"other:{st}"
    u = data.get("usage") or {}
    t.usage = {"in": u.get("input_tokens") or 0, "out": u.get("output_tokens") or 0}
    t.replay = [{k: v for k, v in it.items() if v is not None} for it in out]
    return t


def parse_stream(events):
    for _, data in reversed(events):
        try:
            ev = json.loads(data)
        except ValueError:
            continue
        if ev.get("type") in rules_responses.TERMINAL:
            return parse(ev.get("response") or {})
    return Turn(finish="other:no terminal event")


def replay(st, turn):
    for item in turn.replay:
        st["input"].append(item)


def results(st, items):
    """Responses has no error flag on function_call_output: the text says it."""
    for call, text, is_error in items:
        _add(st, {"type": "function_call_output", "call_id": call.id,
                  "output": f"ERROR: {text}" if is_error else text})


check = rules_responses.check
check_stream = rules_responses.check_stream
check_error = rules_errors.openai
