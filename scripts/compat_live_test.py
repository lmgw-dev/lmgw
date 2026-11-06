#!/usr/bin/env python3
"""Unit test of the live compat check's pure parts: SSE parsing, the fixed wire rules,
error classification, stream assembly and the run diff. No network, no third-party imports.

Usage: python3 scripts/compat_live_test.py   (ci/check.sh runs it)
"""

import json
import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from compat_live import classify, report, rules_chat, rules_errors, rules_messages, rules_responses, sse  # noqa: E402
from compat_live.common import redact  # noqa: E402


def sse_text(events):
    out = []
    for name, data in events:
        if name:
            out.append(f"event: {name}")
        out.append(f"data: {data if isinstance(data, str) else json.dumps(data)}")
        out.append("")
    return "\n".join(out) + "\n"


def rules(found, prefix):
    return [v for v in found if v.split(":")[0].startswith(prefix)]


CHAT_OK = {"id": "c1", "object": "chat.completion", "created": 1, "model": "m",
           "choices": [{"index": 0, "finish_reason": "tool_calls", "message": {
               "role": "assistant", "content": None, "tool_calls": [
                   {"id": "call_1", "type": "function", "function": {"name": "get_time", "arguments": "{}"}}]}}],
           "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}}


def chat_chunks(args_parts, usage=True):
    base = {"id": "c1", "object": "chat.completion.chunk", "created": 1, "model": "m"}
    ev = [dict(base, choices=[{"index": 0, "delta": {"role": "assistant", "tool_calls": [
        {"index": 0, "id": "call_1", "type": "function", "function": {"name": "get_time", "arguments": ""}}]}}])]
    for p in args_parts:
        ev.append(dict(base, choices=[{"index": 0, "delta": {"tool_calls": [{"index": 0, "function": {"arguments": p}}]}}]))
    ev.append(dict(base, choices=[{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]))
    if usage:
        ev.append(dict(base, choices=[], usage={"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}))
    return [(None, json.dumps(e)) for e in ev] + [(None, "[DONE]")]


def anthropic_stream(partials):
    ev = [("message_start", {"type": "message_start", "message": {
        "id": "msg_1", "type": "message", "role": "assistant", "content": [], "model": "m",
        "stop_reason": None, "stop_sequence": None, "usage": {"input_tokens": 5, "output_tokens": 1}}}),
        ("content_block_start", {"type": "content_block_start", "index": 0,
                                 "content_block": {"type": "tool_use", "id": "toolu_1", "name": "get_time", "input": {}}})]
    for p in partials:
        ev.append(("content_block_delta", {"type": "content_block_delta", "index": 0,
                                           "delta": {"type": "input_json_delta", "partial_json": p}}))
    ev += [("content_block_stop", {"type": "content_block_stop", "index": 0}),
           ("message_delta", {"type": "message_delta", "delta": {"stop_reason": "tool_use", "stop_sequence": None},
                              "usage": {"output_tokens": 7}}),
           ("message_stop", {"type": "message_stop"})]
    return [(n, json.dumps(d)) for n, d in ev]


class Sse(unittest.TestCase):
    def test_fields(self):
        text = ": comment\nevent: a\ndata: 1\ndata: 2\n\nretry: 5\n\ndata: [DONE]\n\n"
        self.assertEqual(sse.parse(text), [("a", "1\n2"), (None, "[DONE]")])

    def test_crlf_and_no_trailing_blank(self):
        self.assertEqual(sse.parse("data: x\r\n\r\ndata:y"), [(None, "x"), (None, "y")])


class Chat(unittest.TestCase):
    def test_ok(self):
        self.assertEqual(rules_chat.check(CHAT_OK), [])

    def test_empty_arguments_string(self):
        bad = json.loads(json.dumps(CHAT_OK))
        bad["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"] = ""
        self.assertTrue(rules(rules_chat.check(bad), "chat.tool_call.arguments"))

    def test_usage_total(self):
        bad = json.loads(json.dumps(CHAT_OK))
        bad["usage"]["total_tokens"] = 9
        self.assertTrue(rules(rules_chat.check(bad), "chat.usage.total"))

    def test_stream_ok(self):
        self.assertEqual(rules_chat.check_stream(chat_chunks(["{", "}"])), [])

    def test_stream_no_argument_deltas_is_empty_string(self):
        self.assertTrue(rules(rules_chat.check_stream(chat_chunks([])), "chat.sse.tool_call.arguments"))

    def test_stream_needs_usage_and_done(self):
        ev = chat_chunks(["{}"], usage=False)[:-1]
        found = rules_chat.check_stream(ev)
        self.assertTrue(rules(found, "chat.sse.usage.chunk"))
        self.assertTrue(rules(found, "chat.sse.done_last"))


class Messages(unittest.TestCase):
    def test_body_input_must_be_object(self):
        body = {"id": "m", "type": "message", "role": "assistant", "model": "m", "stop_reason": "tool_use",
                "stop_sequence": None, "usage": {"input_tokens": 1, "output_tokens": 1},
                "content": [{"type": "tool_use", "id": "t", "name": "n", "input": ""}]}
        self.assertTrue(rules(rules_messages.check(body), "messages.tool_use.input"))
        body["content"][0]["input"] = {}
        self.assertEqual(rules_messages.check(body), [])

    def test_stream_ok_with_no_partials(self):
        self.assertEqual(rules_messages.check_stream(anthropic_stream([])), [])
        self.assertEqual(rules_messages.assemble(anthropic_stream([]))["content"][0]["input"], {})

    def test_stream_partials_must_make_an_object(self):
        found = rules_messages.check_stream(anthropic_stream(['"x"']))
        self.assertTrue(rules(found, "messages.sse.tool_use.input"))
        self.assertEqual(rules_messages.assemble(anthropic_stream(['{"a":', "1}"]))["content"][0]["input"], {"a": 1})

    def test_stream_order(self):
        ev = anthropic_stream([])
        found = rules_messages.check_stream([ev[1], ev[0]] + ev[2:])
        self.assertTrue(rules(found, "messages.sse.order.start"))

    def test_event_name_matches_type(self):
        ev = anthropic_stream([])
        ev[1] = ("content_block_delta", ev[1][1])
        self.assertTrue(rules(rules_messages.check_stream(ev), "messages.sse.event.name"))

    def test_compaction_block_and_stop_reason(self):
        block = {"type": "compaction", "content": "Summary.", "signature": "sig"}
        body = {"id": "m", "type": "message", "role": "assistant", "model": "m", "stop_reason": "compaction",
                "stop_sequence": None, "usage": {"input_tokens": 0, "output_tokens": 0}, "content": [block]}
        self.assertEqual(rules_messages.check(body), [])
        body["content"] = [dict(block, content=7)]
        self.assertTrue(rules(rules_messages.check(body), "messages.compaction.content"))
        # A threshold block streams its whole summary in one compaction_delta.
        ev = anthropic_stream([])
        ev[1] = ("content_block_start", json.dumps({"type": "content_block_start", "index": 0,
                                                    "content_block": {"type": "compaction", "content": None}}))
        ev.insert(2, ("content_block_delta", json.dumps({"type": "content_block_delta", "index": 0, "delta": {
            "type": "compaction_delta", "content": "Summary.", "encrypted_content": "e"}})))
        ev[-2] = ("message_delta", json.dumps({"type": "message_delta", "usage": {"output_tokens": 0},
                                               "delta": {"stop_reason": "compaction", "stop_sequence": None}}))
        self.assertEqual(rules_messages.check_stream(ev), [])
        self.assertEqual(rules_messages.assemble(ev)["content"],
                         [{"type": "compaction", "content": "Summary.", "encrypted_content": "e"}])


RESP = {"id": "resp_1", "object": "response", "created_at": 1, "model": "m", "status": "completed",
        "output": [{"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "get_time",
                    "arguments": "{}", "status": "completed"}],
        "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2,
                  "input_tokens_details": {"cached_tokens": 0}, "output_tokens_details": {"reasoning_tokens": 0}}}


def resp_stream(final, args="{}"):
    item = final["output"][0]
    ev = [{"type": "response.created", "response": dict(final, status="in_progress", output=[])},
          {"type": "response.output_item.added", "output_index": 0, "item": dict(item, arguments="")},
          {"type": "response.function_call_arguments.delta", "output_index": 0, "item_id": "fc_1", "delta": args},
          {"type": "response.function_call_arguments.done", "output_index": 0, "item_id": "fc_1", "arguments": args},
          {"type": "response.output_item.done", "output_index": 0, "item": dict(item, arguments=args)},
          {"type": "response.completed", "response": final}]
    for n, e in enumerate(ev):
        e["sequence_number"] = n
    return [(e["type"], json.dumps(e)) for e in ev]


class Responses(unittest.TestCase):
    def test_ok(self):
        self.assertEqual(rules_responses.check(RESP), [])
        self.assertEqual(rules_responses.check_stream(resp_stream(RESP)), [])

    def test_empty_arguments(self):
        bad = json.loads(json.dumps(RESP))
        bad["output"][0]["arguments"] = ""
        self.assertTrue(rules(rules_responses.check(bad), "responses.function_call.arguments"))

    def test_sequence_numbers(self):
        ev = resp_stream(RESP)
        e = json.loads(ev[2][1])
        e["sequence_number"] = 9
        ev[2] = (ev[2][0], json.dumps(e))
        self.assertTrue(rules(rules_responses.check_stream(ev), "responses.sse.sequence_number.order"))


class Errors(unittest.TestCase):
    def test_openai_shape(self):
        ok = '{"error":{"message":"m","type":"invalid_request_error","param":null,"code":"x"}}'
        self.assertEqual(rules_errors.openai(ok, "application/json"), [])
        self.assertTrue(rules_errors.openai('{"error":{"message":"m","type":"t"}}', "application/json"))

    def test_anthropic_shape(self):
        ok = '{"type":"error","error":{"type":"not_found_error","message":"m"}}'
        self.assertEqual(rules_errors.anthropic(ok, "application/json"), [])
        self.assertEqual(rules_errors.anthropic_type_for(404, ok), [])
        self.assertTrue(rules_errors.anthropic_type_for(400, ok))

    def test_classify(self):
        hold = '{"error":{"code":"gpu_hold","message":"held","type":"t","param":null}}'
        self.assertEqual(classify.classify(503, hold)[0], "skipped")
        uns = '{"error":{"code":"unsupported","message":"unsupported: images","type":"t","param":null}}'
        self.assertEqual(classify.classify(400, uns)[0], "unsupported")
        anth = '{"type":"error","error":{"type":"invalid_request_error","message":"unsupported: images"}}'
        self.assertEqual(classify.classify(400, anth)[0], "unsupported")
        up = '{"type":"error","error":{"type":"invalid_request_error","message":"upstream error (400): x"}}'
        self.assertEqual(classify.classify(400, up)[0], "fail")


class Report(unittest.TestCase):
    def test_diff(self):
        prev = {"results": [{"key": "a", "status": "fail"}, {"key": "b", "status": "pass"},
                            {"key": "c", "status": "pass"}, {"key": "gone", "status": "pass"}]}
        now = [{"key": "a", "status": "pass", "reason": ""}, {"key": "b", "status": "fail", "reason": "r"},
               {"key": "c", "status": "model", "reason": ""}, {"key": "new", "status": "fail", "reason": "r"}]
        d = report.diff(prev, now)
        self.assertEqual([x["key"] for x in d["new_failures"]], ["b", "new"])
        self.assertEqual([x["key"] for x in d["fixed"]], ["a"])
        self.assertEqual([x["key"] for x in d["changed"]], ["c"])
        self.assertEqual(d["gone"], ["gone"])

    def test_previous_run_covers_this_one(self):
        full = {"models": ["m1", "m2"], "routes": ["chat", "messages"], "modes": ["plain", "stream"],
                "scenarios": ["text", "tool"]}
        one = {"models": ["m1"], "routes": ["chat"], "modes": ["stream"], "scenarios": ["text"]}
        no_modes = {k: v for k, v in full.items() if k != "modes"}
        with tempfile.TemporaryDirectory() as d:
            for name, run in (("20261101T000000Z", full), ("20261102T000000Z", one),
                              ("20261103T000000Z", no_modes), ("20261109T000000Z", full)):
                os.makedirs(os.path.join(d, name))
                with open(os.path.join(d, name, "results.json"), "w") as f:
                    json.dump({"run": run, "results": []}, f)
            # The single-cell rerun after the full run is no baseline for the next full run,
            # nor is a run that does not say its modes, nor one from after this one.
            self.assertEqual(report.previous_run(d, "20261104T000000Z", full)[::2],
                             ("20261101T000000Z", None))
            self.assertEqual(report.previous_run(d, "20261104T000000Z", one)[::2],
                             ("20261102T000000Z", None))
            self.assertEqual(report.previous_run(d, "20261101T000000Z", one), (None, None, None))

    def test_without_a_covering_run_the_largest_overlap_is_a_partial_baseline(self):
        wanted = {"models": ["m1", "m2", "m3"], "routes": ["chat", "messages"],
                  "modes": ["plain", "stream"], "scenarios": ["text", "tool"]}
        two_models = dict(wanted, models=["m1", "m2"])
        also_two = dict(wanted, models=["m2", "m3"])
        one_model = dict(wanted, models=["m1"])
        elsewhere = dict(wanted, models=["other"])
        with tempfile.TemporaryDirectory() as d:
            for name, run in (("20261101T000000Z", two_models), ("20261102T000000Z", also_two),
                              ("20261103T000000Z", one_model), ("20261104T000000Z", elsewhere)):
                os.makedirs(os.path.join(d, name))
                with open(os.path.join(d, name, "results.json"), "w") as f:
                    json.dump({"run": run, "results": []}, f)
            # The largest overlap wins over a newer, smaller one; of two equal, the newer.
            name, _, partial = report.previous_run(d, "20261105T000000Z", wanted)
            self.assertEqual((name, partial), ("20261102T000000Z", (16, 24)))
            self.assertEqual(report.baseline(name, partial),
                             "20261102T000000Z (a partial baseline: covers 16 of 24 cells)")
            # A run sharing no cell is no baseline at all.
            self.assertEqual(report.previous_run(d, "20261105T000000Z", dict(wanted, models=["x"])),
                             (None, None, None))
            self.assertEqual(report.baseline(None, None), "no earlier run sharing a cell with this one")
            self.assertEqual(report.baseline("20261101T000000Z", None), "20261101T000000Z")

    def test_report_html_names_a_partial_baseline(self):
        meta = {"started": "20261105T000000Z", "base": "http://127.0.0.1:8001", "models": ["m1"],
                "routes": ["chat"], "requests": 0, "cost_note": "", "totals": {}, "seconds": 1,
                "previous": "20261102T000000Z", "previous_partial": [16, 24]}
        d = {"new_failures": [], "fixed": [], "changed": [], "gone": []}
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "report.html")
            report.write_html(path, meta, [], d)
            with open(path) as f:
                page = f.read()
        self.assertIn("20261102T000000Z (a partial baseline: covers 16 of 24 cells)", page)
        self.assertIn("(a partial baseline)</h2>", page)

    def test_gone_stays_inside_this_run(self):
        prev = {"run": {"models": ["m1", "m2"]},
                "results": [{"key": "text|stream|chat|m1", "scenario": "text", "mode": "stream",
                             "route": "chat", "model": "m1", "status": "pass"},
                            {"key": "text|stream|chat|m2", "scenario": "text", "mode": "stream",
                             "route": "chat", "model": "m2", "status": "pass"},
                            {"key": "err|stream|chat|nope", "scenario": "err", "mode": "stream",
                             "route": "chat", "model": "nope", "status": "pass"}]}
        wanted = {"models": ["m1"], "routes": ["chat"], "modes": ["stream"], "scenarios": ["text", "err"]}
        self.assertEqual(report.diff(prev, [], wanted)["gone"], ["err|stream|chat|nope", "text|stream|chat|m1"])

    def test_by_rule(self):
        res = [{"key": "a", "violations": ["turn 1: chat.x: d", "turn 2: chat.x: d", "http: y"]},
               {"key": "b", "violations": ["turn 1: chat.x: e"]}]
        self.assertEqual(report.by_rule(res), [("chat.x", ["a", "b"]), ("http", ["a"])])

    def test_redact(self):
        self.assertEqual(redact("Bearer sk-abc and sk-abc", ["sk-abc"]), "Bearer [redacted] and [redacted]")


if __name__ == "__main__":
    unittest.main(verbosity=1)
