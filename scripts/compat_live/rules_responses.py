"""Fixed wire rules for OpenAI /v1/responses answers (non-streamed and SSE)."""

import json

from .common import Rules, json_object

TERMINAL = {"response.completed", "response.incomplete", "response.failed"}
ITEMS = {"message", "function_call", "reasoning"}


def _item(r, it, where):
    t = it.get("type") if isinstance(it, dict) else None
    if not r.need(t in ITEMS, "item.type", f"{where}type {t!r} is not one of {sorted(ITEMS)}"):
        return
    r.typed(it, "id", str, "item.id", where)
    if t == "message":
        r.need(it.get("role") == "assistant", "message.role", f"{where}role is {it.get('role')!r}")
        for j, part in enumerate(it.get("content") or []):
            w = f"{where}content[{j}]."
            pt = part.get("type")
            r.need(pt in ("output_text", "refusal"), "message.part", f"{w}type {pt!r}")
            if pt == "output_text":
                r.typed(part, "text", str, "output_text.text", w)
                r.typed(part, "annotations", list, "output_text.annotations", w)
    elif t == "function_call":
        r.typed(it, "call_id", str, "function_call.call_id", where)
        r.typed(it, "name", str, "function_call.name", where)
        _, why = json_object(it.get("arguments"))
        r.need(why is None, "function_call.arguments", f"{where}arguments must be a JSON object string: {why}")
    else:
        r.typed(it, "summary", list, "reasoning.summary", where)


def check(body, where=""):
    r = Rules("responses")
    if not r.need(isinstance(body, dict), "body", "the body is not a JSON object"):
        return r.out
    r.need(body.get("object") == "response", "object", f"{where}object is {body.get('object')!r}")
    r.typed(body, "id", str, None, where)
    r.typed(body, "created_at", (int, float), None, where)
    r.typed(body, "model", str, None, where)
    st = body.get("status")
    r.need(st in ("completed", "incomplete"), "status", f"{where}status is {st!r}")
    if st == "incomplete":
        reason = (body.get("incomplete_details") or {}).get("reason")
        r.need(reason in ("max_output_tokens", "content_filter"), "incomplete_details",
               f"{where}incomplete without a known incomplete_details.reason: {reason!r}")
    output = r.typed(body, "output", list, None, where)
    calls = []
    for i, it in enumerate(output or []):
        _item(r, it, f"{where}output[{i}].")
        if isinstance(it, dict) and it.get("type") == "function_call":
            calls.append(it.get("call_id"))
    r.need(len(set(calls)) == len(calls), "function_call.call_id_unique", f"call ids repeat: {calls}")
    usage = r.typed(body, "usage", dict, None, where)
    if usage is not None:
        for k in ("input_tokens", "output_tokens", "total_tokens"):
            r.typed(usage, k, int, f"usage.{k}", where + "usage.")
        r.typed(usage, "input_tokens_details", dict, "usage.input_tokens_details", where + "usage.")
        r.typed(usage, "output_tokens_details", dict, "usage.output_tokens_details", where + "usage.")
    return r.out


def check_stream(events):
    r = Rules("responses.sse")
    if not r.need(events, "events", "the stream carried no events"):
        return r.out
    types, seqs, added, done, args, arg_done, final = [], [], {}, {}, {}, {}, None
    for n, (name, data) in enumerate(events):
        if data == "[DONE]":
            continue
        try:
            ev = json.loads(data)
        except ValueError:
            r.fail("event.json", f"event {n} is not JSON: {data[:80]!r}")
            continue
        t = ev.get("type")
        types.append(t)
        r.need(name == t, "event.name", f"event {n}: SSE event name {name!r} but data type {t!r}")
        seqs.append(ev.get("sequence_number"))
        if t == "error":
            r.fail("error_event", f"event {n}: {data[:300]}")
        elif t == "response.output_item.added":
            oi = ev.get("output_index")
            r.need(oi not in added, "item.added_once", f"event {n}: output_index {oi} added twice")
            r.need(oi == len(added), "item.index", f"event {n}: output_index {oi}, expected {len(added)}")
            added[oi] = ev.get("item") or {}
        elif t == "response.output_item.done":
            oi = ev.get("output_index")
            r.need(oi in added, "item.done_after_added", f"event {n}: output_index {oi} done before added")
            done[oi] = ev.get("item") or {}
            _item(r, done[oi], f"output_item.done {oi}: ")
        elif t == "response.function_call_arguments.delta":
            oi = ev.get("output_index")
            r.need(isinstance(ev.get("delta"), str), "arguments.delta", f"event {n}: delta not a string")
            args[oi] = args.get(oi, "") + (ev.get("delta") or "")
        elif t == "response.function_call_arguments.done":
            arg_done[ev.get("output_index")] = ev.get("arguments")
        elif t in TERMINAL:
            final = ev.get("response")
            r.need(n == len(events) - 1 or all(d == "[DONE]" for _, d in events[n + 1:]),
                   "terminal_last", f"events after {t}")
    r.need(types[:1] == ["response.created"], "order.created", f"the stream starts with {types[:1]}")
    r.need(types[-1:] and types[-1] in TERMINAL, "order.terminal", f"the stream ends with {types[-1:]}")
    ints = [s for s in seqs if isinstance(s, int)]
    if r.need(len(ints) == len(seqs), "sequence_number", "events without an integer sequence_number"):
        r.need(ints == list(range(ints[0], ints[0] + len(ints))), "sequence_number.order",
               "sequence_number does not count up by one")
    r.need(set(added) == set(done), "item.done", f"items added {sorted(added)} but done {sorted(done)}")
    for oi, it in done.items():
        if it.get("type") != "function_call":
            continue
        if oi in arg_done:
            r.need(arg_done[oi] == it.get("arguments"), "arguments.done",
                   f"item {oi}: arguments.done differs from output_item.done")
        # A client builds the arguments from the added item's arguments plus every delta.
        built = (added.get(oi) or {}).get("arguments", "") + args.get(oi, "")
        r.need(built == it.get("arguments"), "arguments.deltas",
               f"item {oi}: output_item.added's arguments + the deltas make {built[:80]!r}, "
               f"the item ends with {str(it.get('arguments'))[:80]!r}")
    if r.need(isinstance(final, dict), "terminal.response", "no terminal event with a response"):
        r.out += check(final, "terminal response: ")
        out = final.get("output") or []
        r.need(len(out) == len(done), "terminal.output",
               f"terminal response has {len(out)} output items, the stream finished {len(done)}")
    return r.out
