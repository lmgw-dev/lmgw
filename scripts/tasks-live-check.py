#!/usr/bin/env python3
# Usage: scripts/tasks-live-check.py BASE TOKEN DEVICE_PID DROP_SECONDS OUT_JSON
"""The live checks 1-4 of MCP Tasks (MCP Tasks design §7), run by
scripts/tasks-live-check.sh, which starts what they need: a dev instance at
BASE (never the installed app's 8001), scripts/mock-openai.py scripting the
model's tool calls (one scripted turn per job-starting send, in the order
below) and speaking through `/audio/speech`, and scripts/mock-mcp.py --tasks
--device as the paired device `builder-pc` hosting `builder__build` (a slow
`required` tool) and `builder__index` (one to cancel) over `/mcp/host`, its
pid DEVICE_PID (SIGUSR1 drops its link for DROP_SECONDS).

1. A dashboard send starts a job; the reply comes at once; the result
   enters the thread when the job ends; Answer answers it, and a later job's
   result is answered by the next send.
2. A bound voice session: `lmgw.task.done`, then a bare `response.create`
   speaks the answer.
3. The device's link dropped while the job runs: the result still arrives
   after the reconnect.
4. The owner's cancel (the strip's route): `cancelled by …` enters the thread.

Every check goes through the routes the dashboard and a voice client use. It
prints one line per check and writes the facts (timings in seconds) to
OUT_JSON; exit 1 when a check failed.
"""
import json
import os
import re
import signal
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

from websockets.sync.client import connect

BASE, TOKEN, DEVICE_PID, DROP, OUT = sys.argv[1:6] if len(sys.argv) == 6 else [None] * 5
JOB = re.compile(r"started, job (t[0-9]+)")  # mock-mcp.py's ids


def call(method, path, body=None, raw=False):
    """`(status, answer)`: JSON, or the text for a frame stream or `raw`."""
    req = urllib.request.Request(
        BASE + path, method=method,
        data=None if body is None else json.dumps(body).encode(),
        headers={"Authorization": f"Bearer {TOKEN}", "Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=120) as r:
            status, text = r.status, r.read().decode()
    except urllib.error.HTTPError as e:
        status, text = e.code, e.read().decode()
    if raw:
        return status, text
    try:
        return status, json.loads(text)
    except ValueError:
        return status, text


def thread():
    _, t = call("POST", "/chat/api/threads", {"model_alias": "fake-echo", "kind": "chat"})
    s, v = call("POST", f"/chat/api/threads/{t['id']}/settings", {"mcp_tools": [
        {"server_label": "builder", "allowed_tools": None, "require_approval": "never"}]})
    assert s == 200, v
    return t["id"]


def messages(tid):
    return call("GET", f"/chat/api/threads/{tid}")[1]["messages"]


def tasks(tid):
    return call("GET", f"/chat/api/threads/{tid}/tasks")[1]


def results(tid):
    return [m for m in messages(tid) if m["role"] == "tool"]


def send(tid, text):
    """The send's frame stream, its seconds, and the job it started (or None)."""
    t0 = time.monotonic()
    s, body = call("POST", f"/chat/api/threads/{tid}/send", {"content": text}, raw=True)
    took = time.monotonic() - t0
    assert s == 200, (s, body)
    m = JOB.search(body)
    return body, took, m.group(1) if m else None


def until(what, f, timeout=60, step=0.1):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        v = f()
        if v:
            return v
        time.sleep(step)
    raise AssertionError(f"timed out after {timeout} s: {what}")


def answer(tid):
    """The answer route: its status and the reply's text (or the refusal's body)."""
    s, body = call("POST", f"/chat/api/threads/{tid}/answer", {}, raw=True)
    return s, reply_text(body) if s == 200 else body


def reply_text(stream):
    """The reply a send's or an answer's frame stream carries: its `delta` texts."""
    out = []
    for frame in stream.split("\n\n"):
        lines = frame.split("\n")
        if lines and lines[0] == "event: delta":
            data = "".join(line[len("data: "):] for line in lines[1:] if line.startswith("data: "))
            out.append(json.loads(data).get("text", ""))
    return "".join(out)


def device_row():
    rows = call("GET", "/api/mcp-servers")[1]["mcp_servers"]
    return next(r for r in rows if r.get("device_key_id"))


def check1():
    f = {}
    tid = thread()
    t0 = time.monotonic()
    body, f["reply_s"], job = send(tid, "build the project")
    assert job, body
    f["job"] = job
    running = tasks(tid)
    assert running and running[0]["status"] in ("working", "input_required"), running
    res = until("the result row", lambda: results(tid), timeout=60)
    f["result_after_send_s"] = round(time.monotonic() - t0, 2)  # the job runs 6 s
    task = res[0]["task"]
    assert task["status"] == "completed" and task["task_id"] == job, task
    assert "built 42 files" in res[0]["content"], res[0]
    t1 = time.monotonic()
    s, body = answer(tid)
    f["answer_s"] = round(time.monotonic() - t1, 2)
    assert s == 200 and f"job {job} (builder__build) completed" in body, (s, body)
    roles = [m["role"] for m in messages(tid)]
    assert roles == ["user", "assistant", "tool", "assistant"], roles
    s, again = call("POST", f"/chat/api/threads/{tid}/answer", {})
    assert s == 409 and again.get("code") == "nothing_to_answer", (s, again)
    # A second job's result, answered by the next send.
    t0 = time.monotonic()
    body, f["second_reply_s"], job2 = send(tid, "build it again")
    assert job2 and job2 != job, body
    until("the second result row", lambda: len(results(tid)) == 2, timeout=60)
    f["second_result_after_send_s"] = round(time.monotonic() - t0, 2)  # the job runs 4 s
    # The next send answers it, wherever the request places the result: before the new message
    # (chronological) or after it (the first build's T11 placement). The send goes without the
    # thread's tools, so it consumes no scripted turn either way.
    tools_off = call("POST", f"/chat/api/threads/{tid}/settings", {"mcp_tools": []})
    assert tools_off[0] == 200, tools_off
    body, f["next_send_s"], _ = send(tid, "what came of it?")
    text = reply_text(body)
    f["next_send_reply"] = text[:80]
    f["placement"] = ("result last (after the new message)" if f"job {job2}" in text
                      else "chronological (the new message last)")
    msgs = messages(tid)
    assert [m["role"] for m in msgs][-3:] == ["tool", "user", "assistant"], msgs
    s, again = call("POST", f"/chat/api/threads/{tid}/answer", {})
    assert s == 409 and again.get("code") == "nothing_to_answer", (s, again)
    f["reply_s"] = round(f["reply_s"], 2)
    f["second_reply_s"] = round(f["second_reply_s"], 2)
    f["next_send_s"] = round(f["next_send_s"], 2)
    return f


def rt_events(ws, stop, timeout=60):
    out = []
    end = time.monotonic() + timeout
    while True:
        left = end - time.monotonic()
        if left <= 0:
            raise AssertionError(f"timed out; last events {[e['type'] for e in out[-8:]]}")
        ev = json.loads(ws.recv(timeout=left))
        ev["_at"] = time.monotonic()  # when it arrived
        out.append(ev)
        if stop(ev):
            return out


def check2():
    f = {}
    tid = thread()
    t_send = time.monotonic()
    body, f["reply_s"], job = send(tid, "build the project")
    assert job, body
    f["job"] = job
    url = BASE.replace("http://", "ws://") + f"/v1/realtime?chat_thread={tid}"
    with connect(url, additional_headers={"Authorization": f"Bearer {TOKEN}"},
                 max_size=None) as ws:
        created = rt_events(ws, lambda e: e["type"] in ("session.created", "error"))
        assert created[-1]["type"] == "session.created", created[-1]
        ws.send(json.dumps({"type": "session.update", "session": {
            "type": "realtime", "output_modalities": ["audio"],
            "audio": {"input": {"turn_detection": None}}}}))
        rt_events(ws, lambda e: e["type"] == "session.updated")
        said = rt_events(ws, lambda e: e["type"] == "lmgw.task.done")[-1]
        f["task_done_after_send_s"] = round(said["_at"] - t_send, 2)  # the job runs 5 s
        assert said["task_id"] == job and said["status"] == "completed", said
        assert said["thread_id"] == tid and said["tool"] == "builder__build", said
        f["task_done"] = {k: said[k] for k in ("id", "task_id", "server_label", "tool", "status")}
        t1 = time.monotonic()
        ws.send(json.dumps({"type": "response.create"}))
        evs = rt_events(ws, lambda e: e["type"] in ("response.done", "error"))
        f["response_s"] = round(time.monotonic() - t1, 2)
        done = evs[-1]
        assert done["type"] == "response.done" and done["response"]["status"] == "completed", done
        audio = [e for e in evs if e["type"] == "response.output_audio.delta"]
        assert audio, [e["type"] for e in evs]
        f["audio_deltas"] = len(audio)
        f["first_audio_s"] = round(audio[0]["_at"] - t1, 2)
        spoken = "".join(e.get("delta", "") for e in evs
                         if e["type"] == "response.output_audio_transcript.delta")
        f["spoken"] = spoken[:120]
        # Speech shaping reads the brackets as pauses: "job t2, builder__build, completed."
        assert f"job {job}" in spoken and "completed" in spoken and "built 42 files" in spoken, spoken
        assert not [e for e in evs if e["type"] == "lmgw.chat.user"], "a continuation writes no user turn"
    msgs = messages(tid)
    assert [m["role"] for m in msgs] == ["user", "assistant", "tool", "assistant"], msgs
    assert (msgs[-1].get("voice") or {}).get("via") == "realtime", msgs[-1]
    f["reply_s"] = round(f["reply_s"], 2)
    return f


def check3():
    f = {}
    tid = thread()
    body, f["reply_s"], job = send(tid, "build the project")
    assert job, body
    f["job"] = job
    t_drop = time.monotonic()
    os.kill(int(DEVICE_PID), signal.SIGUSR1)
    off = until("the device offline", lambda: device_row()["status"] != "ready" and device_row())
    f["status_while_dropped"] = off["status"]
    waiting = until("the task waiting for the device",
                    lambda: [t for t in tasks(tid) if t.get("waiting_for")])
    f["waiting_for"] = waiting[0]["waiting_for"]
    # The job ends on the device (6 s) while its link is down (DROP_SECONDS).
    time.sleep(7)
    assert not results(tid), "a result entered while the device was offline"
    f["open_while_dropped"] = tasks(tid)[0]["status"]
    until("the device back", lambda: device_row()["status"] == "ready", timeout=60, step=0.05)
    t_back = time.monotonic()
    f["relinked_after_drop_s"] = round(t_back - t_drop, 2)
    res = until("the result after the reconnect", lambda: results(tid), timeout=30, step=0.05)
    f["result_after_relink_s"] = round(time.monotonic() - t_back, 2)
    task = res[0]["task"]
    assert task["status"] == "completed" and task["task_id"] == job, task
    assert "built 42 files" in res[0]["content"], res[0]
    f["reply_s"] = round(f["reply_s"], 2)
    return f


def check4():
    f = {}
    tid = thread()
    body, f["reply_s"], job = send(tid, "index the archive")
    assert job, body
    f["job"] = job
    time.sleep(1.5)
    row = tasks(tid)[0]
    assert row["status"] == "working", row
    t0 = time.monotonic()
    s, v = call("POST", f"/chat/api/threads/{tid}/tasks/{row['id']}/cancel", {})
    f["cancel_s"] = round(time.monotonic() - t0, 2)
    assert s == 200, (s, v)
    f["cancel_answer"] = {k: v.get(k) for k in ("delivered", "note")}
    res = until("the cancelled result", lambda: results(tid), timeout=15, step=0.05)
    f["result_after_cancel_s"] = round(time.monotonic() - t0, 2)
    task = res[0]["task"]
    assert task["status"] == "cancelled" and task["task_id"] == job, task
    assert "cancelled by" in res[0]["content"], res[0]
    f["result_text"] = res[0]["content"][:120]
    assert not tasks(tid), tasks(tid)
    s, body = answer(tid)
    assert s == 200 and f"job {job} (builder__index) cancelled" in body, (s, body)
    f["reply_s"] = round(f["reply_s"], 2)
    return f


def main():
    if OUT is None:
        print(__doc__, file=sys.stderr)
        return 2
    host = urllib.parse.urlsplit(BASE)
    if host.hostname != "127.0.0.1" or host.port == 8001:
        print(f"refusing: {BASE} is not a dev instance", file=sys.stderr)
        return 2
    report, fails = {}, 0
    for n, check in enumerate((check1, check2, check3, check4), 1):
        try:
            report[n] = {"result": "PASS", **check()}
        except Exception as e:  # noqa: BLE001 - a failed check is reported, the next one runs
            report[n] = {"result": "FAIL", "error": repr(e)[:2000]}
            fails += 1
        print(f"check {n}: {json.dumps(report[n])}", flush=True)
    with open(OUT, "w") as fh:
        json.dump(report, fh, indent=1)
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
