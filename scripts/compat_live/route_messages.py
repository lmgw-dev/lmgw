"""The /v1/messages route: request bodies, parsing, replay as an Anthropic client does it."""

from . import rules_errors, rules_messages
from .common import Call, Turn

NAME = "messages"
PATH = "/v1/messages"
DIALECT = "anthropic"
FINISH = {"end_turn": "stop", "stop_sequence": "stop", "max_tokens": "length", "tool_use": "tool",
          "refusal": "filter", "compaction": "compaction"}
THINKING_BUDGET = 1024


def new(system=None):
    return {"system": system, "messages": []}


def user(st, text, image_png_b64=None):
    content = [{"type": "text", "text": text}]
    if image_png_b64:
        content.append({"type": "image", "source": {"type": "base64", "media_type": "image/png",
                                                    "data": image_png_b64}})
    _append_user(st, content)


def _append_user(st, blocks):
    msgs = st["messages"]
    if msgs and msgs[-1]["role"] == "user":
        msgs[-1]["content"].extend(blocks)
    else:
        msgs.append({"role": "user", "content": list(blocks)})


def body(st, model, stream, o):
    b = {"model": model, "max_tokens": o["max_tokens"], "messages": [dict(m) for m in st["messages"]]}
    if st.get("system"):
        b["system"] = st["system"]
    if o.get("temperature") is not None and not o.get("reasoning"):
        b["temperature"] = o["temperature"]
    if o.get("tools"):
        b["tools"] = [{"name": t["name"], "description": t["description"], "input_schema": t["parameters"]}
                      for t in o["tools"]]
    tc = o.get("tool_choice")
    if isinstance(tc, dict):
        b["tool_choice"] = {"type": "tool", "name": tc["name"]}
    elif tc:
        b["tool_choice"] = {"type": {"auto": "auto", "none": "none", "required": "any"}[tc]}
    if o.get("stop"):
        b["stop_sequences"] = o["stop"]
    if o.get("schema"):
        b["output_config"] = {"format": {"type": "json_schema", "schema": o["schema"]["schema"]}}
    if o.get("reasoning") == "levels":  # the current models: adaptive thinking and an effort
        b["thinking"] = {"type": "adaptive"}
        b["output_config"] = dict(b.get("output_config") or {}, effort="high")
    elif o.get("reasoning"):  # a toggle model: the older budgeted thinking
        b["thinking"] = {"type": "enabled", "budget_tokens": THINKING_BUDGET}
        b["max_tokens"] = max(b["max_tokens"], THINKING_BUDGET * 2)
    if o.get("compaction"):
        b["compaction"] = o["compaction"]
    if o.get("betas"):  # sent as anthropic-beta, never in the body (transport.sdk_send)
        b["_betas"] = list(o["betas"])
    if stream:
        b["stream"] = True
    return b


def _turn(data):
    stop = data.get("stop_reason")
    t = Turn(finish_raw=stop or "", finish=FINISH.get(stop, f"other:{stop}"),
             stop_sequence=data.get("stop_sequence"), response_id=data.get("id") or "")
    blocks = data.get("content") or []
    t.text = "".join(b.get("text", "") for b in blocks if b.get("type") == "text")
    for b in blocks:
        if b.get("type") == "tool_use":
            inp = b.get("input")
            t.calls.append(Call(b.get("id", ""), b.get("name", ""), inp, inp if isinstance(inp, dict) else None))
    t.reasoning = any(b.get("type") in ("thinking", "redacted_thinking") for b in blocks)
    u = data.get("usage") or {}
    t.usage = {"in": u.get("input_tokens") or 0, "out": u.get("output_tokens") or 0}
    # Replay the content as received, as `messages.append({"role": "assistant", "content": msg.content})`.
    t.replay = [{"role": "assistant", "content": [{k: v for k, v in b.items() if k != "citations" or v}
                                                  for b in blocks]}]
    return t


def parse(data):
    return _turn(data)


def parse_stream(events):
    return _turn(rules_messages.assemble(events))


def replay(st, turn):
    st["messages"].extend(turn.replay)


def results(st, items):
    _append_user(st, [{"type": "tool_result", "tool_use_id": call.id, "content": text, "is_error": bool(is_error)}
                      for call, text, is_error in items])


check = rules_messages.check
check_stream = rules_messages.check_stream


def check_error(body, content_type=""):
    return rules_errors.anthropic(body, content_type)
