"""Fixed wire rules for OpenAI /v1/chat/completions answers (non-streamed and SSE)."""

import json

from .common import Rules, json_object

FINISH = {"stop", "length", "tool_calls", "content_filter", "function_call"}


def _usage(r, usage, where):
    if not r.need(isinstance(usage, dict), "usage.present", f"{where}usage is missing"):
        return
    for k in ("prompt_tokens", "completion_tokens", "total_tokens"):
        r.typed(usage, k, int, f"usage.{k}", where)
    p, c, t = (usage.get(k) for k in ("prompt_tokens", "completion_tokens", "total_tokens"))
    if all(isinstance(v, int) for v in (p, c, t)):
        r.need(t == p + c, "usage.total", f"{where}total_tokens {t} != prompt {p} + completion {c}")


def _tool_calls(r, calls, where, streamed_args=None):
    ids = []
    for i, tc in enumerate(calls):
        w = f"{where}tool_calls[{i}]."
        tid = r.typed(tc, "id", str, "tool_call.id", w)
        r.need(tid != "", "tool_call.id", f"{w}id is empty")
        ids.append(tid)
        r.need(tc.get("type") == "function", "tool_call.type", f"{w}type is {tc.get('type')!r}")
        fn = r.typed(tc, "function", dict, "tool_call.function", w)
        if fn is None:
            continue
        r.typed(fn, "name", str, "tool_call.name", w + "function.")
        args = fn.get("arguments")
        _, why = json_object(args)
        r.need(why is None, "tool_call.arguments", f"{w}function.arguments must be a JSON object string: {why}")
    r.need(len(set(ids)) == len(ids), "tool_call.id_unique", f"{where}tool call ids repeat: {ids}")


def check(body):
    r = Rules("chat")
    if not r.need(isinstance(body, dict), "body", "the body is not a JSON object"):
        return r.out
    r.need(body.get("object") == "chat.completion", "object", f"object is {body.get('object')!r}")
    r.typed(body, "id", str)
    r.typed(body, "created", int)
    r.typed(body, "model", str)
    choices = r.typed(body, "choices", list)
    if choices is not None and r.need(len(choices) >= 1, "choices", "choices is empty"):
        ch = choices[0]
        r.typed(ch, "index", int, "choice.index", "choices[0].")
        fr = ch.get("finish_reason")
        r.need(fr in FINISH, "finish_reason", f"finish_reason {fr!r} is not one of {sorted(FINISH)}")
        msg = r.typed(ch, "message", dict, "message", "choices[0].")
        if msg is not None:
            r.need(msg.get("role") == "assistant", "message.role", f"role is {msg.get('role')!r}")
            content = msg.get("content")
            r.need(content is None or isinstance(content, str), "message.content",
                   f"content should be a string or null, got {type(content).__name__}")
            calls = msg.get("tool_calls") or []
            if r.need(isinstance(calls, list), "message.tool_calls", "tool_calls is not a list"):
                _tool_calls(r, calls, "message.")
                if fr == "tool_calls":
                    r.need(calls, "finish_reason.tool_calls", "finish_reason tool_calls without tool_calls")
    _usage(r, body.get("usage"), "")
    return r.out


def check_stream(events):
    """events: [(event, data)] from sse.parse; usage was requested (stream_options)."""
    r = Rules("chat.sse")
    if not r.need(events, "events", "the stream carried no events"):
        return r.out
    r.need(events[-1][1] == "[DONE]", "done_last", f"the last event is {events[-1][1][:60]!r}, not [DONE]")
    named = [e for e, _ in events if e not in (None, "message")]
    r.need(not named, "no_event_names", f"chat SSE has no event names, saw {sorted(set(named))}")
    ids, role_seen, finish, usage_chunks, after_finish = set(), False, [], [], False
    args, heads, first_choice = {}, {}, True
    for n, (_, data) in enumerate(events):
        if data == "[DONE]":
            r.need(n == len(events) - 1, "done_last", "[DONE] before the end of the stream")
            continue
        try:
            ch = json.loads(data)
        except ValueError:
            r.fail("chunk.json", f"event {n} is not JSON: {data[:80]!r}")
            continue
        r.need(ch.get("object") == "chat.completion.chunk", "chunk.object",
               f"event {n}: object is {ch.get('object')!r}")
        ids.add(ch.get("id"))
        choices = ch.get("choices")
        if not r.need(isinstance(choices, list), "chunk.choices", f"event {n}: choices is not a list"):
            continue
        if ch.get("usage") is not None:
            usage_chunks.append(ch["usage"])
            r.need(choices == [], "usage.chunk", f"event {n}: the usage chunk has choices")
        for c in choices:
            delta = c.get("delta")
            if not r.need(isinstance(delta, dict), "delta", f"event {n}: delta missing"):
                continue
            if first_choice:
                role_seen = delta.get("role") == "assistant"
                first_choice = False
            if after_finish and (delta.get("content") or delta.get("tool_calls")):
                r.fail("after_finish", f"event {n}: content after the finish_reason chunk")
            for tc in delta.get("tool_calls") or []:
                idx = tc.get("index")
                if not r.need(isinstance(idx, int), "tool_call.index", f"event {n}: tool call delta without index"):
                    continue
                fn = tc.get("function") or {}
                if idx not in heads:
                    heads[idx] = tc
                    r.need(isinstance(tc.get("id"), str) and tc["id"], "tool_call.id",
                           f"event {n}: the first delta of tool call {idx} has no id")
                    r.need(isinstance(fn.get("name"), str) and fn["name"], "tool_call.name",
                           f"event {n}: the first delta of tool call {idx} has no name")
                    r.need(tc.get("type") == "function", "tool_call.type",
                           f"event {n}: the first delta of tool call {idx} has type {tc.get('type')!r}")
                a = fn.get("arguments")
                if a is not None:
                    if r.need(isinstance(a, str), "tool_call.arguments_delta", f"event {n}: arguments delta is not a string"):
                        args[idx] = args.get(idx, "") + a
            if c.get("finish_reason") is not None:
                finish.append(c["finish_reason"])
                after_finish = True
    r.need(role_seen, "role_first", "the first delta does not carry role: assistant")
    r.need(len(ids) == 1, "chunk.id", f"chunk ids differ across the stream: {sorted(map(str, ids))[:3]}")
    if r.need(len(finish) == 1, "finish_once", f"finish_reason appeared {len(finish)} times: {finish}"):
        r.need(finish[0] in FINISH, "finish_reason", f"finish_reason {finish[0]!r} is not one of {sorted(FINISH)}")
        if finish[0] == "tool_calls":
            r.need(heads, "finish_reason.tool_calls", "finish_reason tool_calls without tool call deltas")
    for idx in heads:
        _, why = json_object(args.get(idx, ""))
        r.need(why is None, "tool_call.arguments",
               f"tool call {idx}: the concatenated arguments must be a JSON object: {why}")
    hid = [h.get("id") for h in heads.values()]
    r.need(len(set(hid)) == len(hid), "tool_call.id_unique", f"tool call ids repeat: {hid}")
    if r.need(len(usage_chunks) == 1, "usage.chunk",
              f"stream_options.include_usage asks for one usage chunk, saw {len(usage_chunks)}"):
        _usage(r, usage_chunks[0], "usage chunk: ")
    return r.out
