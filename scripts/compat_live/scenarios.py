"""The scenario catalogue. Each scenario is protocol-neutral: the route module turns its
turns into the route's bodies, and the Cell applies the route's fixed rules to every answer.
Prompts are tiny so a full matrix stays cheap."""

import json
from dataclasses import dataclass

from . import classify

from .common import UNSUPPORTED, Outcome
from .media import png_b64

ALL_ROUTES = ("chat", "messages", "responses")
BOTH = ("plain", "stream")

T_TIME = {"name": "get_time", "description": "Returns the current time. Takes no arguments.",
          "parameters": {"type": "object", "properties": {}}}
T_WEATHER = {"name": "get_weather", "description": "Returns the weather for one city.",
             "parameters": {"type": "object", "properties": {"city": {"type": "string"}},
                            "required": ["city"]}}
T_ORDER = {"name": "lookup_order", "description": "Looks up an order by its id.",
           "parameters": {"type": "object", "properties": {"order_id": {"type": "string"}},
                          "required": ["order_id"]}}
T_EVENT = {"name": "create_event", "description": "Creates a calendar event.",
           "parameters": {"type": "object", "required": ["title", "when", "attendees"], "properties": {
               "title": {"type": "string"},
               "when": {"type": "object", "required": ["date", "time"],
                        "properties": {"date": {"type": "string"}, "time": {"type": "string"}}},
               "attendees": {"type": "array", "items": {
                   "type": "object", "required": ["name", "email"],
                   "properties": {"name": {"type": "string"}, "email": {"type": "string"}}}}}}}
PERSON = {"name": "person", "schema": {
    "type": "object", "additionalProperties": False, "required": ["name", "age"],
    "properties": {"name": {"type": "string"}, "age": {"type": "integer"}}}}


@dataclass
class Scenario:
    id: str
    title: str
    fn: object
    modes: tuple = BOTH
    routes: tuple = ALL_ROUTES
    needs: str = ""  # "reasoning": only models whose /v1/models card lists a reasoning control
    model_free: bool = False  # runs once per route, not per model


def text(c):
    st = c.route.new()
    c.route.user(st, "Reply with exactly one word: pong")
    t = c.call(st)
    c.expect(t.text.strip(), "text", "the answer carries no text")
    c.finish(t, "stop")
    c.soft("pong" in t.text.lower(), f"asked for 'pong', got {t.text[:80]!r}")


def system(c):
    st = c.route.new(system="You are a test fixture. Answer every message with exactly one word: BANANA")
    c.route.user(st, "Hello")
    t = c.call(st)
    c.finish(t, "stop")
    c.soft("banana" in t.text.lower(), f"the system prompt asked for BANANA, got {t.text[:80]!r}")


def multi_turn(c):
    st = c.route.new()
    c.route.user(st, "Remember the code word KIWI. Reply only: OK")
    t1 = c.call(st)
    c.finish(t1, "stop")
    c.route.replay(st, t1)
    c.route.user(st, "What was the code word? Reply with that one word.")
    t2 = c.call(st)
    c.finish(t2, "stop")
    c.soft("kiwi" in t2.text.lower(), f"turn 2 forgot the code word: {t2.text[:80]!r}")


def _final(c, st, tools, t1, results):
    c.route.replay(st, t1)
    c.route.results(st, results)
    t2 = c.call(st, tools=tools)
    if t2.calls:  # calling again (a retry after an error, say) is legal; the replay was accepted
        c.finish(t2, "tool")
        c.soft(False, f"turn 2 called {[x.name for x in t2.calls]} again instead of answering")
        return t2
    c.expect(t2.text.strip(), "text", "the answer after the tool result carries no text")
    c.finish(t2, "stop")
    return t2


def tool_noargs(c):
    st = c.route.new()
    c.route.user(st, "What time is it? Call the get_time tool.")
    t1 = c.call(st, tools=[T_TIME])
    call = c.need_calls(t1, 1, "get_time")[0]
    c.finish(t1, "tool")
    c.expect(call.args == {}, "tool_args.empty_object",
             f"a no-argument call's arguments should be an empty JSON object, got {call.raw_args!r}")
    t2 = _final(c, st, [T_TIME], t1, [(call, "12:00 UTC", False)])
    c.soft("12" in t2.text, f"the answer does not use the tool result: {t2.text[:80]!r}")


def tool_nested(c):
    st = c.route.new()
    c.route.user(st, "Create an event titled Sync on 2026-01-05 at 10:00 with Ann (ann@example.com) and "
                     "Bob (bob@example.com). Call create_event.")
    t1 = c.call(st, tools=[T_EVENT])
    call = c.need_calls(t1, 1, "create_event")[0]
    a = call.args or {}
    for key, typ in (("when", dict), ("attendees", list)):
        v = a.get(key)
        if isinstance(v, str) and v.strip()[:1] in "{[":
            c.expect(False, "tool_args.nested", f"{key} arrived as a JSON string, not a nested value: {v[:80]!r}")
        else:
            c.soft(isinstance(v, typ), f"{key} is {type(v).__name__}, the schema says {typ.__name__}")
    if isinstance(a.get("attendees"), list):
        c.soft(all(isinstance(x, dict) for x in a["attendees"]), "attendees are not objects")
    _final(c, st, [T_EVENT], t1, [(call, '{"created": "evt_1"}', False)])


def tool_parallel(c):
    st = c.route.new()
    c.route.user(st, "Get the weather for Paris and for Tokyo: call get_weather twice, both in this one turn.")
    t1 = c.call(st, tools=[T_WEATHER])
    calls = c.need_calls(t1, 1, "get_weather")
    c.soft(len(calls) >= 2, f"the model made {len(calls)} call(s) in one turn, asked for 2")
    c.expect(len({x.id for x in calls}) == len(calls), "tool_call.id_unique", "parallel calls share an id")
    _final(c, st, [T_WEATHER], t1, [(x, f"sunny, 20 C ({(x.args or {}).get('city', '?')})", False) for x in calls])


def tool_error(c):
    st = c.route.new()
    c.route.user(st, "Look up order A-1 with lookup_order, then tell me its state.")
    t1 = c.call(st, tools=[T_ORDER])
    call = c.need_calls(t1, 1, "lookup_order")[0]
    _final(c, st, [T_ORDER], t1, [(call, "the order service is unavailable", True)])


def tool_forced(c):
    st = c.route.new()
    c.route.user(st, "Say hi.")
    t = c.call(st, tools=[T_TIME, T_WEATHER], tool_choice={"name": "get_weather"})
    c.expect([x.name for x in t.calls][:1] == ["get_weather"], "tool_choice.forced",
             f"tool_choice named get_weather, the answer called {[x.name for x in t.calls]}")
    c.finish(t, "tool", "stop")


def tool_none(c):
    st = c.route.new()
    c.route.user(st, "Call the get_time tool.")
    t = c.call(st, tools=[T_TIME], tool_choice="none")
    c.expect(not t.calls, "tool_choice.none", f"tool_choice none, the answer called {[x.name for x in t.calls]}")
    c.finish(t, "stop")


def image(c):
    st = c.route.new()
    c.route.user(st, "What colour is this image? Answer with one word.", image_png_b64=png_b64())
    t = c.call(st)
    c.finish(t, "stop")
    c.soft("red" in t.text.lower(), f"a red square, the model said {t.text[:80]!r}")


def json_schema(c):
    st = c.route.new()
    c.route.user(st, "Give me a person named Ann who is 30 years old.")
    t = c.call(st, schema=PERSON)
    c.finish(t, "stop")
    try:
        val = json.loads(t.text)
    except ValueError:
        val = None
    ok = isinstance(val, dict) and isinstance(val.get("name"), str) and type(val.get("age")) is int \
        and set(val) == {"name", "age"}
    c.expect(ok, "structured_output", f"the text does not match the json_schema: {t.text[:160]!r}")


def stop_seq(c):
    st = c.route.new()
    c.route.user(st, "Count from 1 to 10, the numbers separated by single spaces, nothing else.")
    t = c.call(st, stop=["5"])
    c.finish(t, "stop")
    c.expect("5" not in t.text, "stop_sequence", f"the text runs past the stop sequence: {t.text[:80]!r}")
    if c.route.NAME == "messages":
        c.expect(t.finish_raw == "stop_sequence" and t.stop_sequence == "5", "stop_sequence.reason",
                 f"stop_reason {t.finish_raw!r}, stop_sequence {t.stop_sequence!r}")


def max_tokens(c):
    st = c.route.new()
    c.route.user(st, "Write a 300-word story about a lighthouse.")
    t = c.call(st, max_tokens=16)
    c.finish(t, "length")


def reasoning_tool(c):
    st = c.route.new()
    c.route.user(st, "Think it over before you act: is it warmer in Paris or in Oslo right now? Call get_weather for both, then answer in one sentence.")
    t1 = c.call(st, tools=[T_WEATHER], reasoning=c.reasoning_kind)
    calls = c.need_calls(t1, 1, "get_weather")
    c.soft(t1.reasoning, "the answer with the tool call carried no reasoning / thinking content")
    c.route.replay(st, t1)
    c.route.results(st, [(x, f"{12 if 'aris' in json.dumps(x.args) else 3} C", False) for x in calls])
    t2 = c.call(st, tools=[T_WEATHER], reasoning=c.reasoning_kind)
    if t2.calls:
        c.soft(False, f"turn 2 called {[x.name for x in t2.calls]} again instead of answering")
        return
    c.expect(t2.text.strip(), "text", "the answer after the tool results carries no text")


def prev_response(c):
    st = c.route.new()
    c.route.user(st, "Remember the code word KIWI. Reply only: OK")
    t1 = c.call(st)
    c.expect(t1.response_id, "response.id", "no response id to continue from")
    c.route.user(st, "What was the code word? Reply with that one word.")
    t2 = c.call(st, previous_response_id=t1.response_id)
    c.soft("kiwi" in t2.text.lower(), f"previous_response_id lost the code word: {t2.text[:80]!r}")


def count_tokens(c):
    name = c.route.NAME
    if name == "messages":
        body = {"model": c.model, "system": "Be brief.", "tools": [
            {"name": T_WEATHER["name"], "description": T_WEATHER["description"],
             "input_schema": T_WEATHER["parameters"]}],
            "messages": [{"role": "user", "content": "Hello, world"}]}
        ex, err = c.sdk("messages_count", body)
        _count_read(c, ex, err, "input_tokens", "messages_count")
    elif name == "chat":  # lmgw's own counter, the one an OpenAI-shaped client has
        ex = c.raw("/v1/count_tokens", {"model": c.model, "input": "Hello, world"})
        _count_read(c, ex, None, "tokens", None)
    else:
        ex = c.raw("/v1/responses/input_tokens", {"model": c.model, "input": "Hello, world"})
        _count_read(c, ex, None, "input_tokens", None)
        try:
            obj = json.loads(ex.resp_body).get("object")
        except (ValueError, AttributeError):
            obj = None
        c.expect(obj == "response.input_tokens", "count.object", f"object is {obj!r}")


def _count_read(c, ex, err, key, sdk_kind):
    if ex.status != 200:
        if ex.status in (404, 405) and not ex.resp_body.strip().startswith("{"):
            c.expect(False, "route.served", f"lmgw does not serve {ex.url.split('8001')[-1]} "
                     f"(HTTP {ex.status}, no typed error): {ex.resp_body[:120]!r}")
            return
        c.error_shape(ex)
        status, reason = classify.classify(ex.status, ex.resp_body)
        if status == "fail":
            c.expect(False, "http", reason)
            return
        raise Outcome(status, reason)
    try:
        data = json.loads(ex.resp_body)
    except ValueError:
        data = {}
    c.expect(isinstance(data.get(key), int) and data[key] > 0, f"count.{key}", f"{key} is {data.get(key)!r}")
    if sdk_kind:
        c.violations += c.sdk_schema(sdk_kind, data)
    if err:
        c.expect(False, "sdk.exception", err)
    for h, v in ex.lmgw_headers().items():
        c.notes.append(f"{h}: {v}")


COMPACT_BETA = "compact-2026-09-04"


def compaction(c):
    """On-demand compaction (Anthropic beta compact-2026-09-04): the block comes back alone with
    stop_reason compaction, and the next turn, which sends it first in place of the turns it
    summarizes, is answered from the summary."""
    st = c.route.new()
    c.route.user(st, "Remember the code word KIWI. Reply only: OK")
    t1 = c.call(st, max_tokens=64)
    c.finish(t1, "stop")
    c.route.replay(st, t1)
    t2 = c.call(st, compaction={"type": "summarize"}, betas=[COMPACT_BETA], max_tokens=4096)
    c.finish(t2, "compaction")
    blocks = [b for m in t2.replay for b in m["content"] if b.get("type") == "compaction"]
    if not c.expect(len(blocks) == 1 and isinstance(blocks[0].get("content"), str), "compaction.block",
                    f"the answer carries {len(blocks)} compaction block(s), expected one with a summary"):
        return
    st["messages"] = list(t2.replay)  # the block replaces what it summarizes
    c.route.user(st, "What was the code word? Reply with that one word.")
    # Room to think, as a real client gives it: Claude Haiku 5.5 thinks adaptively when a request
    # names no `thinking`, its display omitted, and at 64 a turn that thought stopped at
    # max_tokens with no text (seen in a live run).
    t3 = c.call(st, betas=[COMPACT_BETA], max_tokens=1024)
    c.finish(t3, "stop")
    c.soft("kiwi" in t3.text.lower(), f"the summary lost the code word: {t3.text[:80]!r}")


def error_unknown_model(c):
    st = c.route.new()
    c.route.user(st, "hi")
    try:
        c.call(st, max_tokens=16)
    except Outcome as o:
        ex = c.kit.rec.log[-1]
        c.expect(ex.status == 404, "error.status", f"an unknown model answers HTTP {ex.status}, expected 404")
        c.violations[:] = [v for v in c.violations if " http: " not in v]
        if o.status == UNSUPPORTED:
            c.expect(False, "error.kind", o.reason)
        return
    c.expect(False, "error.status", "an unknown model was answered with 200")


def error_malformed(c):
    ex = c.raw(c.route.PATH, b'{"model": "x", "messages": [')
    c.expect(ex.status == 400, "error.status", f"a malformed body answers HTTP {ex.status}, expected 400")
    c.error_shape(ex)


CATALOGUE = [
    Scenario("text", "plain text answer", text),
    Scenario("system", "system prompt", system),
    Scenario("multi_turn", "two text turns, the history replayed", multi_turn),
    Scenario("tool_noargs", "a no-argument tool call, its result, the final answer", tool_noargs),
    Scenario("tool_nested", "nested object/array arguments, then replay", tool_nested),
    Scenario("tool_parallel", "parallel calls in one turn, all results", tool_parallel),
    Scenario("tool_error", "a tool result flagged as an error", tool_error),
    Scenario("tool_forced", "tool_choice naming a tool", tool_forced),
    Scenario("tool_none", "tool_choice none with tools present", tool_none),
    Scenario("image", "a PNG image input", image),
    Scenario("json_schema", "structured output with a json_schema", json_schema),
    Scenario("stop_seq", "a stop sequence", stop_seq, routes=("chat", "messages")),
    Scenario("max_tokens", "max tokens reached: the length finish reason", max_tokens),
    Scenario("reasoning_tool", "reasoning / thinking kept across a tool call", reasoning_tool, needs="reasoning"),
    Scenario("prev_response", "previous_response_id continuation", prev_response, routes=("responses",)),
    Scenario("count_tokens", "the route's token counter", count_tokens, modes=("plain",)),
    Scenario("compaction", "on-demand compaction: the block alone, then sent back first", compaction,
             routes=("messages",), needs="compaction"),
    Scenario("error_unknown_model", "error shape: unknown model", error_unknown_model, modes=("plain",),
             model_free=True),
    Scenario("error_malformed", "error shape: malformed JSON body", error_malformed, modes=("plain",),
             model_free=True),
]
BY_ID = {s.id: s for s in CATALOGUE}
UNKNOWN_MODEL = "compat-live-no-such-model"
