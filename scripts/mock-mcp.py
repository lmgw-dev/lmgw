#!/usr/bin/env python3
# Usage: register as a stdio MCP server: command `python3`, args `scripts/mock-mcp.py`
#        (add a line `--tasks` to the args for the MCP Tasks tools);
#    or: LMGW_DEVICE_KEY=<device key> scripts/mock-mcp.py --tasks --device ws://HOST:PORT/mcp/host
#        (a paired device hosting the same tools over the host link; SIGUSR1 drops the link).
"""A tiny stdio MCP server for driving the Chat's tool paths with no real server.

Three tools, one harmless and two that stand for things worth asking about:
`list_notes` (read), `delete_note` (a call a thread would gate) and `send_mail`.
Every call answers with a line saying what it did, so a driver can tell that an
approved call ran (the answer contains "ran:") and a declined one did not.
Newline-delimited JSON-RPC on stdin/stdout; nothing is stored.

With `--tasks` it speaks MCP Tasks of revision 2025-11-25 (MCP Tasks design
§1) instead: it declares `capabilities.tasks {cancel, requests.tools.call}`
and offers three tools whose `execution.taskSupport` is `required`, so lmgw
calls them as tasks:
- `build {seconds}`: works for `seconds` (default 8), its status message
  counting the steps, then completes with a line of output;
- `deploy {seconds}`: works for `seconds` (default 4), then fails with a
  tool result whose `isError` is true;
- `index {seconds}`: works for `seconds` (default 3600) — a job to cancel.
A `tools/call` with `task` answers `CreateTaskResult` at once (`ttl` one
hour, `pollInterval` one second); `tasks/get` reads the task, `tasks/result`
answers once it is terminal (held until then), `tasks/cancel` cancels a
running one; an unknown task id is `-32602`. Each status move is also sent as
`notifications/tasks/status`. Tasks live as long as the process.

With `--device URL` it is a paired device instead (client-apps design §5): it
opens the host link `GET /mcp/host` with the device key in `LMGW_DEVICE_KEY`
and serves the same MCP over that WebSocket, one JSON-RPC message per text
frame. A link that closes is opened again after a second; SIGUSR1 drops the
link at once (the socket shut, as a Wi-Fi blip would) and opens it again
after `DROP_SECONDS` (default 5). Tasks outlive the link, as a desktop
client's job table does; what was said while no link was up is lost, and a
`tasks/result` held on the dropped link is forgotten with it.
"""
import datetime
import json
import os
import signal
import socket
import sys
import threading
import time

TOOLS = [
    {
        "name": "list_notes",
        "description": "List the notes.",
        "inputSchema": {"type": "object", "properties": {}},
    },
    {
        "name": "delete_note",
        "description": "Delete a note by id.",
        "inputSchema": {
            "type": "object",
            "properties": {"id": {"type": "string"}},
            "required": ["id"],
        },
    },
    {
        "name": "send_mail",
        "description": "Send a mail.",
        "inputSchema": {
            "type": "object",
            "properties": {"to": {"type": "string"}, "body": {"type": "string"}},
            "required": ["to"],
        },
    },
]

TASKS = "--tasks" in sys.argv[1:]


def task_tool(name, description, default):
    return {
        "name": name,
        "description": description,
        "inputSchema": {
            "type": "object",
            "properties": {
                "seconds": {
                    "type": "number",
                    "description": f"how long the job runs (default {default})",
                }
            },
        },
        "execution": {"taskSupport": "required"},
    }


TASK_TOOLS = [
    task_tool("build", "Build the project. Runs as a job and completes.", 8),
    task_tool("deploy", "Deploy the build. Runs as a job and fails.", 4),
    task_tool("index", "Index the archive. Runs as a job for an hour; cancel it.", 3600),
]
DEFAULT_SECONDS = {"build": 8, "deploy": 4, "index": 3600}

OUT = threading.Lock()
TASK_LOCK = threading.Lock()
TASKS_BY_ID = {}  # id -> dict(status, message, created, updated, result, held)
NEXT = [0]


DEVICE = sys.argv[sys.argv.index("--device") + 1] if "--device" in sys.argv[1:] else None
LINK = [None]  # the device's open link (websockets' sync connection), or None


def send(msg):
    with OUT:
        if DEVICE is None:
            sys.stdout.write(json.dumps(msg) + "\n")
            sys.stdout.flush()
        elif LINK[0] is not None:
            try:
                LINK[0].send(json.dumps(msg))
            except Exception:  # the link went: what was said is lost
                pass


def reply(id_, result=None, error=None):
    msg = {"jsonrpc": "2.0", "id": id_}
    if error is not None:
        msg["error"] = error
    else:
        msg["result"] = result
    send(msg)


def call(name, args):
    if name == "list_notes":
        return "ran: list_notes -> 3 notes"
    if name == "delete_note":
        return f"ran: delete_note {args.get('id', '?')}"
    if name == "send_mail":
        return f"ran: send_mail to {args.get('to', '?')}"
    return None


def now_iso():
    return datetime.datetime.now(datetime.timezone.utc).isoformat().replace("+00:00", "Z")


def task_json(id_, t):
    j = {
        "taskId": id_,
        "status": t["status"],
        "createdAt": t["created"],
        "lastUpdatedAt": t["updated"],
        "ttl": 3600000,
        "pollInterval": 1000,
    }
    if t["message"]:
        j["statusMessage"] = t["message"]
    return j


def terminal(t):
    return t["status"] in ("completed", "failed", "cancelled")


def payload(id_, t):
    result = dict(t["result"] or {"content": []})
    result["_meta"] = {"io.modelcontextprotocol/related-task": {"taskId": id_}}
    return result


def move(id_, status, message, result=None):
    """Task `id_` moves to `status`: its held `tasks/result` answers go out once
    it is terminal, and a status notification is sent."""
    with TASK_LOCK:
        t = TASKS_BY_ID[id_]
        if terminal(t):
            return
        t["status"], t["message"], t["updated"] = status, message, now_iso()
        if result is not None:
            t["result"] = result
        held = t["held"] if terminal(t) else []
        if terminal(t):
            t["held"] = []
        j = task_json(id_, t)
        out = payload(id_, t) if terminal(t) else None
    for rid in held:
        reply(rid, out)
    send({"jsonrpc": "2.0", "method": "notifications/tasks/status", "params": j})


def run_task(id_, name, seconds):
    steps = max(1, int(seconds))
    for i in range(1, steps + 1):
        time.sleep(seconds / steps)
        with TASK_LOCK:
            if terminal(TASKS_BY_ID[id_]):
                return
        if i < steps:
            move(id_, "working", f"step {i} of {steps}")
    if name == "build":
        move(id_, "completed", "done", {
            "content": [{"type": "text", "text": f"built 42 files in {seconds:g} s"}],
            "isError": False,
        })
    elif name == "deploy":
        move(id_, "failed", "the target refused the upload", {
            "content": [{"type": "text", "text": "deploy failed: the target refused the upload (403)"}],
            "isError": True,
        })
    else:
        move(id_, "completed", "done", {
            "content": [{"type": "text", "text": f"indexed the archive in {seconds:g} s"}],
            "isError": False,
        })


def start_task(name, args):
    try:
        seconds = float(args.get("seconds", DEFAULT_SECONDS[name]))
    except (TypeError, ValueError):
        seconds = float(DEFAULT_SECONDS[name])
    with TASK_LOCK:
        NEXT[0] += 1
        id_ = f"t{NEXT[0]}"
        stamp = now_iso()
        TASKS_BY_ID[id_] = {
            "status": "working", "message": "starting", "created": stamp,
            "updated": stamp, "result": None, "held": [],
        }
        j = task_json(id_, TASKS_BY_ID[id_])
    threading.Thread(target=run_task, args=(id_, name, seconds), daemon=True).start()
    return {"task": j}


def tasks_method(method, id_, params):
    """`tasks/get`, `tasks/result`, `tasks/cancel`: answered here (or held)."""
    tid = (params or {}).get("taskId", "")
    with TASK_LOCK:
        t = TASKS_BY_ID.get(tid)
        if t is None:
            unknown = True
        else:
            unknown = False
            if method == "tasks/get":
                answer = ("now", task_json(tid, t))
            elif method == "tasks/result":
                if terminal(t):
                    answer = ("now", payload(tid, t))
                else:
                    t["held"].append(id_)
                    answer = ("held", None)
            else:
                answer = ("cancel", terminal(t))
    if unknown:
        reply(id_, error={"code": -32602, "message": f"unknown task {tid}"})
        return
    kind, value = answer
    if kind == "now":
        reply(id_, value)
    elif kind == "cancel":
        if value:
            reply(id_, error={"code": -32602, "message": f"task {tid} is already terminal"})
            return
        move(tid, "cancelled", "cancelled by the requestor")
        with TASK_LOCK:
            j = task_json(tid, TASKS_BY_ID[tid])
        reply(id_, j)


def handle(line):
    """One JSON-RPC message from lmgw: answered (or held, or ignored)."""
    line = line.strip()
    if not line:
        return
    try:
        req = json.loads(line)
    except ValueError:
        return
    method, id_ = req.get("method"), req.get("id")
    if id_ is None:
        return  # a notification or an answer
    if method == "initialize":
        caps = {"tools": {}}
        if TASKS:
            caps["tasks"] = {"cancel": {}, "requests": {"tools": {"call": {}}}}
        reply(
            id_,
            {
                "protocolVersion": (req.get("params") or {}).get("protocolVersion", "2025-03-26"),
                "capabilities": caps,
                "serverInfo": {"name": "mock-mcp", "version": "0"},
            },
        )
    elif method == "ping":
        reply(id_, {})
    elif method == "tools/list":
        reply(id_, {"tools": TASK_TOOLS if TASKS else TOOLS})
    elif TASKS and method in ("tasks/get", "tasks/result", "tasks/cancel"):
        tasks_method(method, id_, req.get("params"))
    elif TASKS and method == "tools/call":
        p = req.get("params") or {}
        name = p.get("name")
        if name not in DEFAULT_SECONDS:
            reply(id_, error={"code": -32602, "message": f"unknown tool {name}"})
        elif "task" not in p:
            reply(id_, error={"code": -32601, "message": f"{name} runs only as a task"})
        else:
            reply(id_, start_task(name, p.get("arguments") or {}))
    elif method == "tools/call":
        p = req.get("params") or {}
        text = call(p.get("name"), p.get("arguments") or {})
        if text is None:
            reply(id_, error={"code": -32602, "message": f"unknown tool {p.get('name')}"})
        else:
            reply(id_, {"content": [{"type": "text", "text": text}], "isError": False})
    else:
        reply(id_, error={"code": -32601, "message": f"no method {method}"})


def device():
    """The host link, opened again whenever it closes."""
    from websockets.sync.client import connect

    key = os.environ.get("LMGW_DEVICE_KEY", "")
    if not key:
        sys.exit("--device needs the device key in LMGW_DEVICE_KEY")
    drop = [0.0]

    def on_usr1(_sig, _frame):
        drop[0] = float(os.environ.get("DROP_SECONDS", "5"))
        link = LINK[0]
        if link is not None:
            print("mock-mcp: dropping the link", file=sys.stderr, flush=True)
            try:
                link.socket.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass

    signal.signal(signal.SIGUSR1, on_usr1)

    def run():
        while True:
            try:
                link = connect(DEVICE, additional_headers={"Authorization": f"Bearer {key}"},
                               max_size=None, open_timeout=10)
            except Exception as e:  # lmgw not up yet, or refused: try again
                print(f"mock-mcp: no link ({e})", file=sys.stderr, flush=True)
                time.sleep(1)
                continue
            with OUT:
                LINK[0] = link
            print("mock-mcp: linked", file=sys.stderr, flush=True)
            try:
                for raw in link:
                    handle(raw if isinstance(raw, str) else raw.decode())
            except Exception:
                pass
            with OUT:
                LINK[0] = None
            with TASK_LOCK:
                for t in TASKS_BY_ID.values():
                    t["held"] = []  # held on the link that went
            pause, drop[0] = drop[0] or 1.0, 0.0
            print(f"mock-mcp: link closed, again in {pause:g} s", file=sys.stderr, flush=True)
            time.sleep(pause)

    threading.Thread(target=run, daemon=True).start()
    while True:
        signal.pause()


if DEVICE is not None:
    device()
else:
    for line in sys.stdin:
        handle(line)
