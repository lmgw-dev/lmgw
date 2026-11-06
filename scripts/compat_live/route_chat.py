"""The /v1/chat/completions route: request bodies, parsing, replay as an OpenAI client does it."""

import json

from . import rules_chat, rules_errors
from .common import Call, Turn, json_object

NAME = "chat"
PATH = "/v1/chat/completions"
DIALECT = "openai"
FINISH = {"stop": "stop", "length": "length", "tool_calls": "tool", "content_filter": "filter"}
REASONING_KEYS = ("reasoning_content", "reasoning", "reasoning_details")


def new(system=None):
    msgs = [{"role": "system", "content": system}] if system else []
    return {"messages": msgs}


def user(st, text, image_png_b64=None):
    if image_png_b64:
        content = [{"type": "text", "text": text},
                   {"type": "image_url", "image_url": {"url": f"data:image/png;base64,{image_png_b64}"}}]
    else:
        content = text
    st["messages"].append({"role": "user", "content": content})


def body(st, model, stream, o):
    b = {"model": model, "messages": list(st["messages"]), "max_tokens": o["max_tokens"]}
    if o.get("temperature") is not None:
        b["temperature"] = o["temperature"]
    if o.get("tools"):
        b["tools"] = [{"type": "function", "function": {"name": t["name"], "description": t["description"],
                                                         "parameters": t["parameters"]}} for t in o["tools"]]
    tc = o.get("tool_choice")
    if isinstance(tc, dict):
        b["tool_choice"] = {"type": "function", "function": {"name": tc["name"]}}
    elif tc:
        b["tool_choice"] = tc
    if o.get("stop"):
        b["stop"] = o["stop"]
    if o.get("schema"):
        s = o["schema"]
        b["response_format"] = {"type": "json_schema",
                                "json_schema": {"name": s["name"], "strict": True, "schema": s["schema"]}}
    if o.get("reasoning"):
        b["reasoning_effort"] = "high"
    if stream:
        b["stream"] = True
        b["stream_options"] = {"include_usage": True}
    return b


def _turn(msg, finish, usage, rid):
    t = Turn(finish_raw=finish or "", finish=FINISH.get(finish, f"other:{finish}"), response_id=rid or "")
    t.text = msg.get("content") or ""
    for tc in msg.get("tool_calls") or []:
        fn = tc.get("function") or {}
        args, _ = json_object(fn.get("arguments"))
        t.calls.append(Call(tc.get("id", ""), fn.get("name", ""), fn.get("arguments"), args))
    t.reasoning = any(msg.get(k) for k in REASONING_KEYS)
    if isinstance(usage, dict):
        t.usage = {"in": usage.get("prompt_tokens") or 0, "out": usage.get("completion_tokens") or 0}
    # Replay the message as received, dropping nulls, as `messages.append(choice.message)` does.
    t.replay = [{k: v for k, v in msg.items() if v is not None}]
    return t


def parse(data):
    ch = (data.get("choices") or [{}])[0]
    return _turn(ch.get("message") or {}, ch.get("finish_reason"), data.get("usage"), data.get("id"))


def parse_stream(events):
    msg, calls, finish, usage, rid = {"role": "assistant", "content": ""}, {}, None, None, None
    for _, data in events:
        if data == "[DONE]":
            continue
        try:
            ch = json.loads(data)
        except ValueError:
            continue
        rid = rid or ch.get("id")
        usage = ch.get("usage") or usage
        for c in ch.get("choices") or []:
            d = c.get("delta") or {}
            msg["content"] += d.get("content") or ""
            for k in REASONING_KEYS:
                if isinstance(d.get(k), str):
                    msg[k] = msg.get(k, "") + d[k]
            for tc in d.get("tool_calls") or []:
                cur = calls.setdefault(tc.get("index"), {"id": "", "type": "function",
                                                         "function": {"name": "", "arguments": ""}})
                cur["id"] = tc.get("id") or cur["id"]
                fn = tc.get("function") or {}
                cur["function"]["name"] += fn.get("name") or ""
                cur["function"]["arguments"] += fn.get("arguments") or ""
            finish = c.get("finish_reason") or finish
    if calls:
        msg["tool_calls"] = [calls[i] for i in sorted(calls, key=lambda x: (x is None, x))]
    if not msg["content"] and calls:
        msg["content"] = None
    return _turn(msg, finish, usage, rid)


def replay(st, turn):
    st["messages"].extend(turn.replay)


def results(st, items):
    """items: [(Call, text, is_error)]. Chat has no error flag: the text says it."""
    for call, text, is_error in items:
        st["messages"].append({"role": "tool", "tool_call_id": call.id,
                               "content": f"ERROR: {text}" if is_error else text})


check = rules_chat.check
check_stream = rules_chat.check_stream
check_error = rules_errors.openai
