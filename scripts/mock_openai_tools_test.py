#!/usr/bin/env python3
"""Self-check for the tool-calling mode of scripts/mock-openai.py (TOOL_SCRIPT).

Usage: scripts/mock_openai_tools_test.py
Starts the mock on a free loopback port with a two-turn script, drives it with
OpenAI-shaped requests (streamed and not: call, then result) and checks the
shapes lmgw's openai_wire decoder parses. Also checks that without `tools` the
classifier and chat shapes are unchanged. Stops the mock when done.
"""
import http.client
import json
import os
import socket
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
SCRIPT = [
    [{"name": "echo", "arguments": {"text": "hi"}}],
    [{"name": "a", "arguments": {}}, {"name": "b", "arguments": {"n": 2}}],
]
TOOLS = [{"type": "function", "function": {"name": "echo", "parameters": {"type": "object"}}}]


def post(port, body, path="/v1/chat/completions"):
    c = http.client.HTTPConnection("127.0.0.1", port, timeout=10)
    c.request("POST", path, json.dumps(body), {"Content-Type": "application/json"})
    r = c.getresponse()
    data = r.read().decode()
    c.close()
    return r.status, data


def sse(data):
    return [json.loads(l[6:]) for l in data.splitlines() if l.startswith("data: ") and l != "data: [DONE]"]


def main():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        port = s.getsockname()[1]
    env = dict(os.environ, TOOL_SCRIPT=json.dumps(SCRIPT), STREAM_DELAY="0", STREAM_TOKENS="5")
    proc = subprocess.Popen([sys.executable, "-I", os.path.join(HERE, "mock-openai.py"), str(port)],
                            env=env, stdout=subprocess.DEVNULL)
    try:
        for _ in range(50):
            try:
                socket.create_connection(("127.0.0.1", port), 0.2).close()
                break
            except OSError:
                time.sleep(0.1)
        user = {"role": "user", "content": "go"}

        # Turn 1, non-streaming: one call, then its result echoed.
        st, d = post(port, {"messages": [user], "tools": TOOLS})
        ch = json.loads(d)["choices"][0]
        call = ch["message"]["tool_calls"][0]
        assert st == 200 and ch["finish_reason"] == "tool_calls", d
        assert call["function"]["name"] == "echo" and call["id"], d
        assert json.loads(call["function"]["arguments"]) == {"text": "hi"}, d
        history = [user, ch["message"], {"role": "tool", "tool_call_id": call["id"], "content": "pong"}]
        ch = json.loads(post(port, {"messages": history, "tools": TOOLS})[1])["choices"][0]
        assert ch["finish_reason"] == "stop" and ch["message"]["content"].strip() == "Tool result: pong", ch

        # Turn 2, streaming: two calls as deltas, arguments reassemble per index.
        st, d = post(port, {"messages": [user], "tools": TOOLS, "stream": True})
        names, args, finish = {}, {}, None
        for f in sse(d):
            for c in f["choices"][0]["delta"].get("tool_calls", []):
                i = c["index"]
                if c.get("function", {}).get("name"):
                    names[i] = c["function"]["name"]
                    assert c["id"], c
                args[i] = args.get(i, "") + c["function"].get("arguments", "")
            finish = f["choices"][0]["finish_reason"] or finish
        assert names == {0: "a", 1: "b"} and finish == "tool_calls", d
        assert json.loads(args[0]) == {} and json.loads(args[1]) == {"n": 2}, args
        history = [user, {"role": "tool", "tool_call_id": "x", "content": "r1"},
                   {"role": "tool", "tool_call_id": "y", "content": "r2"}]
        st, d = post(port, {"messages": history, "tools": TOOLS, "stream": True})
        text = "".join(f["choices"][0]["delta"].get("content", "") for f in sse(d))
        assert text.strip() == "Tool result: r1\nr2", repr(text)

        # Script used up: a request with tools falls through to the plain chat shape.
        st, d = post(port, {"messages": [user], "tools": TOOLS})
        assert "tool_calls" not in json.loads(d)["choices"][0]["message"], d
        # Existing modes: classifier is untouched.
        schema = {"schema": {"properties": {"category": {"enum": ["Receipts", "Other"]}}}}
        st, d = post(port, {"messages": [{"role": "user", "content": "your receipt"}],
                            "response_format": {"json_schema": schema}})
        assert json.loads(json.loads(d)["choices"][0]["message"]["content"]) == {"category": "Receipts"}, d
        print("ok")
    finally:
        proc.terminate()
        proc.wait()


main()
