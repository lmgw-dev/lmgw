"""Fixed wire rules for Anthropic /v1/messages answers (non-streamed and SSE)."""

import json

from .common import Rules

STOP = {"end_turn", "max_tokens", "stop_sequence", "tool_use", "pause_turn", "refusal", "compaction"}
BLOCKS = {"text", "tool_use", "thinking", "redacted_thinking", "compaction"}
DELTA_FOR = {"text": {"text_delta"}, "tool_use": {"input_json_delta"},
             "thinking": {"thinking_delta", "signature_delta"}, "redacted_thinking": set(),
             "compaction": {"compaction_delta"}}


def _block(r, b, where):
    t = b.get("type") if isinstance(b, dict) else None
    if not r.need(t in BLOCKS, "block.type", f"{where}type {t!r} is not one of {sorted(BLOCKS)}"):
        return
    if t == "text":
        r.typed(b, "text", str, "text.text", where)
    elif t == "tool_use":
        r.typed(b, "id", str, "tool_use.id", where)
        r.typed(b, "name", str, "tool_use.name", where)
        r.typed(b, "input", dict, "tool_use.input", where)
    elif t == "thinking":
        r.typed(b, "thinking", str, "thinking.thinking", where)
        r.typed(b, "signature", str, "thinking.signature", where)
    elif t == "compaction":  # beta compact-2026-09-04 / compact-2026-01-12: the summary, or null
        r.need(b.get("content") is None or isinstance(b.get("content"), str), "compaction.content",
               f"{where}content is {type(b.get('content')).__name__}, not a string or null")
    else:
        r.typed(b, "data", str, "redacted_thinking.data", where)


def _stop(r, reason, seq, has_tool):
    r.need(reason in STOP, "stop_reason", f"stop_reason {reason!r} is not one of {sorted(STOP)}")
    if reason == "stop_sequence":
        r.need(isinstance(seq, str), "stop_sequence", "stop_reason stop_sequence without the stop_sequence string")
    else:
        r.need(seq is None, "stop_sequence", f"stop_sequence is {seq!r} with stop_reason {reason!r}")
    if reason == "tool_use":
        r.need(has_tool, "stop_reason.tool_use", "stop_reason tool_use without a tool_use block")
    if has_tool:
        r.need(reason in ("tool_use", "max_tokens"), "stop_reason.tool_use",
               f"tool_use blocks with stop_reason {reason!r}")


def check(body):
    r = Rules("messages")
    if not r.need(isinstance(body, dict), "body", "the body is not a JSON object"):
        return r.out
    r.need(body.get("type") == "message", "type", f"type is {body.get('type')!r}")
    r.need(body.get("role") == "assistant", "role", f"role is {body.get('role')!r}")
    r.typed(body, "id", str)
    r.typed(body, "model", str)
    content = r.typed(body, "content", list)
    ids = []
    for i, b in enumerate(content or []):
        _block(r, b, f"content[{i}].")
        if isinstance(b, dict) and b.get("type") == "tool_use":
            ids.append(b.get("id"))
    r.need(len(set(ids)) == len(ids), "tool_use.id_unique", f"tool_use ids repeat: {ids}")
    _stop(r, body.get("stop_reason"), body.get("stop_sequence"), bool(ids))
    usage = r.typed(body, "usage", dict)
    if usage is not None:
        r.typed(usage, "input_tokens", int, "usage.input_tokens", "usage.")
        r.typed(usage, "output_tokens", int, "usage.output_tokens", "usage.")
    return r.out


def check_stream(events):
    r = Rules("messages.sse")
    if not r.need(events, "events", "the stream carried no events"):
        return r.out
    seen, open_block, next_index, blocks, deltas = [], None, 0, {}, {}
    stop, usage_out, ids = None, None, []
    for n, (name, data) in enumerate(events):
        try:
            ev = json.loads(data)
        except ValueError:
            r.fail("event.json", f"event {n} is not JSON: {data[:80]!r}")
            continue
        t = ev.get("type")
        r.need(name == t, "event.name", f"event {n}: SSE event name {name!r} but data type {t!r}")
        if t == "ping":
            continue
        seen.append(t)
        if t == "error":
            r.fail("error_event", f"event {n}: {data[:300]}")
        elif t == "message_start":
            r.need(n == 0 or all(e == "ping" for e, _ in events[:n]), "order.start", "message_start is not first")
            msg = ev.get("message") or {}
            r.need(msg.get("role") == "assistant", "start.role", f"message_start role {msg.get('role')!r}")
            r.need(isinstance((msg.get("usage") or {}).get("input_tokens"), int), "usage.input_tokens",
                   "message_start.message.usage.input_tokens is not an integer")
        elif t == "content_block_start":
            idx, b = ev.get("index"), ev.get("content_block") or {}
            r.need(open_block is None, "order.block", f"event {n}: block {idx} starts while {open_block} is open")
            r.need(idx == next_index, "order.index", f"event {n}: block index {idx}, expected {next_index}")
            open_block, next_index = idx, next_index + 1
            blocks[idx], deltas[idx] = b, ""
            _block(r, b, f"content_block_start {idx}: ")
            if b.get("type") == "tool_use":
                ids.append(b.get("id"))
                r.need(b.get("input") == {}, "tool_use.start_input",
                       f"block {idx}: content_block_start input should be {{}}, got {b.get('input')!r}")
        elif t == "content_block_delta":
            idx, d = ev.get("index"), ev.get("delta") or {}
            r.need(idx == open_block, "order.delta", f"event {n}: delta for {idx} while {open_block} is open")
            bt = (blocks.get(idx) or {}).get("type")
            r.need(d.get("type") in DELTA_FOR.get(bt, set()), "delta.type",
                   f"event {n}: {d.get('type')!r} delta in a {bt!r} block")
            if d.get("type") == "input_json_delta":
                if r.need(isinstance(d.get("partial_json"), str), "delta.partial_json", f"event {n}: partial_json not a string"):
                    deltas[idx] += d["partial_json"]
        elif t == "content_block_stop":
            r.need(ev.get("index") == open_block, "order.stop", f"event {n}: stop for {ev.get('index')} while {open_block} is open")
            if (blocks.get(open_block) or {}).get("type") == "tool_use":
                buf = deltas.get(open_block, "")
                try:
                    val = json.loads(buf) if buf.strip() else {}
                except ValueError:
                    val = None
                r.need(isinstance(val, dict), "tool_use.input",
                       f"block {open_block}: the input_json_delta parts must make a JSON object, got {buf[:120]!r}")
            open_block = None
        elif t == "message_delta":
            d = ev.get("delta") or {}
            r.need(stop is None, "message_delta.once", "more than one message_delta")
            stop = (d.get("stop_reason"), d.get("stop_sequence"))
            usage_out = (ev.get("usage") or {}).get("output_tokens")
        elif t == "message_stop":
            r.need(n == len(events) - 1, "order.stop_last", "events after message_stop")
        else:
            r.fail("event.type", f"event {n}: unknown event type {t!r}")
    r.need(seen[:1] == ["message_start"], "order.start", f"the stream starts with {seen[:1]}")
    r.need(seen[-1:] == ["message_stop"], "order.stop_last", f"the stream ends with {seen[-1:]}")
    r.need(open_block is None, "order.unclosed", f"block {open_block} never stopped")
    r.need(len(set(ids)) == len(ids), "tool_use.id_unique", f"tool_use ids repeat: {ids}")
    if r.need(stop is not None, "message_delta", "no message_delta with the stop_reason"):
        _stop(r, stop[0], stop[1], bool(ids))
    r.need(isinstance(usage_out, int), "usage.output_tokens", "message_delta.usage.output_tokens is not an integer")
    return r.out


def assemble(events):
    """The final message's content blocks from a stream, as the SDK's accumulator builds them."""
    out, stop, seq, usage, mid = {}, None, None, {}, ""
    for _, data in events:
        try:
            ev = json.loads(data)
        except ValueError:
            continue
        t = ev.get("type")
        if t == "message_start":
            msg = ev.get("message") or {}
            mid = msg.get("id", "")
            usage = dict(msg.get("usage") or {})
        elif t == "content_block_start":
            b = dict(ev.get("content_block") or {})
            b["_json"] = ""
            out[ev.get("index")] = b
        elif t == "content_block_delta":
            b, d = out.get(ev.get("index")), ev.get("delta") or {}
            if b is None:
                continue
            dt = d.get("type")
            if dt == "text_delta":
                b["text"] = b.get("text", "") + d.get("text", "")
            elif dt == "input_json_delta":
                b["_json"] += d.get("partial_json", "")
            elif dt == "thinking_delta":
                b["thinking"] = b.get("thinking", "") + d.get("thinking", "")
            elif dt == "signature_delta":
                b["signature"] = b.get("signature", "") + d.get("signature", "")
            elif dt == "compaction_delta":  # the whole summary, and any opaque field beside it
                for k, v in d.items():
                    if k == "content" and isinstance(v, str):
                        b["content"] = (b.get("content") or "") + v
                    elif k not in ("type", "content"):
                        b[k] = v
        elif t == "message_delta":
            d = ev.get("delta") or {}
            stop, seq = d.get("stop_reason"), d.get("stop_sequence")
            usage.update(ev.get("usage") or {})
    blocks = []
    for idx in sorted(k for k in out if isinstance(k, int)):
        b = out[idx]
        buf = b.pop("_json")
        if b.get("type") == "tool_use":
            try:
                b["input"] = json.loads(buf) if buf.strip() else b.get("input", {})
            except ValueError:
                b["input"] = buf
        blocks.append(b)
    return {"id": mid, "content": blocks, "stop_reason": stop, "stop_sequence": seq, "usage": usage}
