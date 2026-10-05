"""What scripts/shell-check.py and scripts/shell_check_voice.py share: the run's
constants and private dirs, the private bus, broadway and PipeWire graph, the graph
readers, and WebKit's remote inspector.

Child processes go through private_session (process groups, PR_SET_PDEATHSIG,
stop_all). Every run has a work dir of its own, target/shell-check-<pid>, so a
second run (on another --port) never removes the first one's tree and podman stub.
"""
import json
import os
import re
import shutil
import subprocess
import time
import urllib.request
from pathlib import Path

import private_session
from private_session import free_port, spawn, wait_for  # noqa: F401  (re-exported)

REPO = Path(__file__).resolve().parents[1]
# Per run (review n3): a second run must not remove this one's tree, podman
# stub included, while it runs.
WORK = REPO / "target" / f"shell-check-{os.getpid()}"
# This run's container prefix: never the installed app's (lmgw) nor a live dev
# instance's (lmgw-dev).
PREFIX = f"lmgw-shellcheck-{os.getpid()}"
# A debug build's stream identity (src-tauri audio_out::STREAM_APP).
STREAM_ID = "lmgw-dev"

PODMAN_STUB = """#!/usr/bin/env python3
# shell-check's podman: records every call and runs none.
import json, os, sys
with open(os.environ["SHELL_CHECK_PODMAN_LOG"], "a") as f:
    f.write(json.dumps({"who": os.environ.get("SHELL_CHECK_WHO"), "argv": sys.argv[1:]}) + "\\n")
sys.stderr.write("shell-check: podman is stubbed out, nothing was run\\n")
sys.exit(125)
"""


T0 = time.time()


def log(msg):
    print(f"[shell-check {time.time() - T0:5.1f}s] {msg}", flush=True)


def base_env():
    env = dict(os.environ)
    # Nothing here may reach the real desktop, session bus or audio server.
    for k in list(env):
        if k in ("DISPLAY", "WAYLAND_DISPLAY", "DBUS_SESSION_BUS_ADDRESS", "PULSE_SERVER",
                 "PIPEWIRE_REMOTE", "PIPEWIRE_RUNTIME_DIR", "PIPEWIRE_PROPS",
                 "PULSE_RUNTIME_PATH", "LMGW_DATA_DIR", "LMGW_CONTAINER_PREFIX") \
                or k.startswith("PULSE_PROP"):
            del env[k]
    env["NO_AT_BRIDGE"] = "1"
    return env


def private_env(run):
    """base_env with this run's own runtime, cache, config and data dirs, the
    podman stub first on PATH, and the container prefix: what the seed and the
    shell run under."""
    env = base_env()
    env.update(XDG_RUNTIME_DIR=str(run), XDG_CACHE_HOME=str(WORK / "cache"),
               XDG_CONFIG_HOME=str(WORK / "config"), XDG_DATA_HOME=str(WORK / "xdg-data"),
               PATH=f"{WORK / 'bin'}:{os.environ.get('PATH', '/usr/bin:/bin')}",
               SHELL_CHECK_PODMAN_LOG=str(WORK / "podman-calls.jsonl"),
               LMGW_CONTAINER_PREFIX=PREFIX)
    return env


def private_dirs():
    """The podman stub, and the config dir whose user-dirs.dirs puts the
    Downloads folder inside WORK (the shell's download handler reads it)."""
    (WORK / "bin").mkdir()
    stub = WORK / "bin" / "podman"
    stub.write_text(PODMAN_STUB)
    stub.chmod(0o755)
    (WORK / "config").mkdir()
    (WORK / "config" / "user-dirs.dirs").write_text(f'XDG_DOWNLOAD_DIR="{WORK / "downloads"}"\n')
    for d in ("cache", "xdg-data", "downloads"):
        (WORK / d).mkdir()


def http_ok(url):
    try:
        with urllib.request.urlopen(url, timeout=2) as r:
            return r.status == 200
    except Exception:
        return False


# --- private session pieces ------------------------------------------------------------------


BUS_CONF = """<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:path={path}</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
"""


def private_bus(run):
    """A session bus of our own, with no service directories: nothing is
    activated on it (no portal, no a11y bus, no wallet)."""
    path = run / "bus"
    conf = WORK / "bus.conf"
    conf.write_text(BUS_CONF.format(path=path))
    env = base_env()
    p = spawn(["dbus-daemon", f"--config-file={conf}", "--nofork", "--nopidfile"], env, WORK / "dbus.log")
    wait_for("the private bus", lambda: path.exists(), 10)
    return f"unix:path={path}"


def broadway(run):
    """A GTK3 broadwayd whose socket is in this run's own runtime dir."""
    log_file = WORK / f"broadwayd-{len(private_session.started)}.log"
    env = base_env()
    env["XDG_RUNTIME_DIR"] = str(run)
    for n in range(40, 80):
        # broadwayd :N listens on broadway<N+1>.socket; either name taken means skip.
        if any((run / f"broadway{m}.socket").exists() for m in (n, n + 1)):
            continue
        socks = [run / f"broadway{n + 1}.socket"]
        port = free_port()
        p = spawn(["broadwayd", "-a", "127.0.0.1", "-p", str(port), f":{n}"], env, log_file)
        try:
            wait_for("broadwayd", lambda: b"Listening on" in log_file.read_bytes() or p.poll() is not None, 5)
        except RuntimeError:
            pass
        if p.poll() is None:
            return n, socks
    raise RuntimeError("no free broadway display")


PW_CONF = """context.properties = {
    module.x11.bell = false
    module.jackdbus-detect = false
    module.raop = false
    module.portal = false
}
context.objects = [
    { factory = adapter
      args = { factory.name = support.null-audio-sink node.name = "check_speakers"
               node.description = "Check speakers (null)" media.class = "Audio/Sink"
               audio.position = [ FL FR ] priority.session = 2000 object.linger = true } }
    { factory = adapter
      args = { factory.name = support.null-audio-sink node.name = "check_headphones"
               node.description = "Check headphones (null)" media.class = "Audio/Sink"
               audio.position = [ FL FR ] priority.session = 1000 object.linger = true } }
]
"""


def audio_graph(run, bus):
    """pipewire + wireplumber (policy profile: no ALSA, Bluetooth or camera monitor)
    + pipewire-pulse, all on their own runtime, config and state dirs. Returns the
    variables a client needs to reach it, or None when the daemons are missing."""
    if not all(shutil.which(b) for b in ("pipewire", "wireplumber", "pipewire-pulse", "pw-dump")):
        return None
    conf = WORK / "pw-config"
    (conf / "pipewire" / "pipewire.conf.d").mkdir(parents=True)
    (conf / "pipewire" / "pipewire.conf.d" / "50-shell-check.conf").write_text(PW_CONF)
    env = base_env()
    env.update(XDG_RUNTIME_DIR=str(run), XDG_CONFIG_HOME=str(conf),
               XDG_STATE_HOME=str(WORK / "state"), DBUS_SESSION_BUS_ADDRESS=bus)
    real_pulse = Path(os.environ.get("XDG_RUNTIME_DIR", f"/run/user/{os.getuid()}")) / "pulse" / "native"
    assert (run / "pulse" / "native") != real_pulse
    spawn(["pipewire"], env, WORK / "pipewire.log")
    wait_for("the private pipewire socket", lambda: (run / "pipewire-0").exists(), 10)
    spawn(["wireplumber", "-p", "policy"], env, WORK / "wireplumber.log")
    spawn(["pipewire-pulse"], env, WORK / "pipewire-pulse.log")
    wait_for("the private pulse socket", lambda: (run / "pulse" / "native").exists(), 10)
    client = {"PIPEWIRE_RUNTIME_DIR": str(run), "PULSE_SERVER": f"unix:{run}/pulse/native"}
    wait_for("the null sinks", lambda: len(sinks(pw_dump(client))) == 2, 10)
    return client


def pw_dump(client):
    env = base_env()
    env.update(client)
    r = subprocess.run(["pw-dump"], env=env, capture_output=True, timeout=10)
    return json.loads(r.stdout or b"[]")


def props(o):
    return (o.get("info") or {}).get("props") or {}


def sinks(dump):
    return {props(o)["node.name"]: o for o in dump
            if o.get("type") == "PipeWire:Interface:Node" and props(o).get("media.class") == "Audio/Sink"}


def descends(pid, ancestor):
    seen = set()
    while pid and pid not in seen:
        if pid == ancestor:
            return True
        seen.add(pid)
        try:
            stat = Path(f"/proc/{pid}/stat").read_text()
        except OSError:
            return False
        pid = int(stat[stat.rfind(")") + 1:].split()[1])
    return False


def playback(dump, owner=None):
    """Stream/Output/Audio nodes; with owner, those whose reported pid is in its tree."""
    out = []
    for o in dump:
        p = props(o)
        if o.get("type") == "PipeWire:Interface:Node" and p.get("media.class") == "Stream/Output/Audio":
            pid = int(p.get("application.process.id") or 0)
            if owner is None or descends(pid, owner):
                out.append(o)
    return out


def target_of(dump, node_id):
    """The stream's target.object in the default metadata; None also for
    "-1", WirePlumber's "no defined target" (what audio_output_set(null) writes)."""
    for o in dump:
        if o.get("type") == "PipeWire:Interface:Metadata" and (o.get("props") or {}).get("metadata.name") == "default":
            for e in o.get("metadata") or []:
                if e.get("subject") == node_id and e.get("key") == "target.object":
                    return None if str(e.get("value")) == "-1" else str(e.get("value"))
    return None


def linked_to(dump, node_id):
    """The node ids the stream's output ports are linked to."""
    return {o["info"]["input-node-id"] for o in dump
            if o.get("type") == "PipeWire:Interface:Link" and o.get("info", {}).get("output-node-id") == node_id}


def wp_state():
    f = WORK / "state" / "wireplumber" / "stream-properties"
    out = {}
    if f.exists():
        for line in f.read_text().splitlines():
            if "=" in line and not line.startswith("["):
                k, v = line.split("=", 1)
                out[k] = v
    return out


# --- the inspector -----------------------------------------------------------------------------


class Inspector:
    """WebKit's remote inspector protocol over the HTTP server's WebSocket: one
    page target, messages wrapped in Target.sendMessageToTarget."""

    def __init__(self, addr, gateway):
        import websocket  # websocket-client
        self.ws_mod = websocket

        def page_socket():
            try:
                html = urllib.request.urlopen(f"http://{addr}/", timeout=2).read().decode()
            except Exception:
                return None
            for url, sock in re.findall(r'class="targeturl">([^<]*)</div>.*?ws=[^/]*(/socket/[0-9]+/[0-9]+/WebPage)', html, re.S):
                if url.startswith(gateway) and "/api/session/login" not in url:
                    return sock
            return None

        sock = wait_for("the dashboard in the inspector", page_socket, 60, 0.5)
        self.ws = websocket.create_connection(f"ws://{addr}{sock}", timeout=10)
        self.next_id = 0
        self.target = None
        self.reenable = False
        self.contexts = {}  # id -> the context's description (frameId, type)
        self.pending = []
        msg = self._recv(10)
        while self.target is None:
            self._dispatch(msg)
            if self.target is None:
                msg = self._recv(10)
        self.call("Runtime.enable")

    def _recv(self, timeout):
        self.ws.settimeout(timeout)
        return json.loads(self.ws.recv())

    def _dispatch(self, msg):
        method = msg.get("method")
        if method == "Target.targetCreated":
            info = msg["params"]["targetInfo"]
            # A provisional target is a cross-site navigation's new process:
            # it becomes the page when (if) the navigation commits.
            if info.get("type") == "page" and (self.target is None or not info.get("isProvisional")):
                self.target = info["targetId"]
        elif method == "Target.didCommitProvisionalTarget":
            self.target = msg["params"]["newTargetId"]
            self.contexts.clear()
            self.reenable = True
        elif method == "Target.dispatchMessageFromTarget":
            inner = json.loads(msg["params"]["message"])
            if inner.get("method") == "Runtime.executionContextCreated":
                c = inner["params"]["context"]
                self.contexts[c["id"]] = c
            elif inner.get("method") == "Runtime.executionContextDestroyed":
                self.contexts.pop(inner["params"].get("executionContextId"), None)
            else:
                return inner
        return None

    def call(self, method, params=None, timeout=30):
        if self.reenable and method != "Runtime.enable":
            self.reenable = False
            self.call("Runtime.enable")
        self.next_id += 1
        mid = self.next_id
        inner = {"id": mid, "method": method, "params": params or {}}
        self.next_id += 1
        self.ws.send(json.dumps({"id": self.next_id, "method": "Target.sendMessageToTarget",
                                 "params": {"targetId": self.target, "message": json.dumps(inner)}}))
        end = time.time() + timeout
        while time.time() < end:
            try:
                reply = self._dispatch(self._recv(max(0.1, end - time.time())))
            except self.ws_mod.WebSocketTimeoutException:
                break
            if reply and reply.get("id") == mid:
                if "error" in reply:
                    raise RuntimeError(f"{method}: {reply['error']}")
                return reply.get("result", {})
        raise RuntimeError(f"{method}: no answer in {timeout} s")

    def pump(self, seconds):
        end = time.time() + seconds
        while time.time() < end:
            try:
                self._dispatch(self._recv(max(0.1, end - time.time())))
            except self.ws_mod.WebSocketTimeoutException:
                break

    def eval(self, expr, context=None, gesture=False, timeout=30):
        """Evaluate an expression that yields (a promise of) a JSON string; return it parsed."""
        params = {"expression": expr, "returnByValue": False, "emulateUserGesture": gesture}
        if context is not None:
            params["contextId"] = context
        r = self.call("Runtime.evaluate", params, timeout)
        res = r.get("result", {})
        if r.get("wasThrown"):
            raise RuntimeError(f"threw: {res.get('description')}")
        if res.get("objectId"):
            r = self.call("Runtime.awaitPromise", {"promiseObjectId": res["objectId"], "returnByValue": True}, timeout)
            res = r.get("result", {})
            if r.get("wasThrown"):
                raise RuntimeError(f"rejected: {res.get('description')}")
        v = res.get("value")
        return json.loads(v) if isinstance(v, str) else v


def gum(constraints, display=False):
    call = "getDisplayMedia" if display else "getUserMedia"
    return f"""(async () => {{
      try {{
        const s = await navigator.mediaDevices.{call}({json.dumps(constraints)});
        const tracks = s.getTracks().map(t => t.kind + ':' + t.label);
        s.getTracks().forEach(t => t.stop());
        return JSON.stringify({{ok: true, tracks}});
      }} catch (e) {{ return JSON.stringify({{ok: false, err: e.name + ': ' + e.message}}); }}
    }})()"""


ENUM = """(async () => JSON.stringify((await navigator.mediaDevices.enumerateDevices())
  .map(d => ({kind: d.kind, label: d.label, id: !!d.deviceId}))))()"""


def invoke(cmd, args=None):
    return f"""(async () => {{
      try {{ return JSON.stringify({{ok: true, v: await window.__TAURI_INTERNALS__.invoke({json.dumps(cmd)}, {json.dumps(args or {})})}}); }}
      catch (e) {{ return JSON.stringify({{ok: false, err: String(e)}}); }}
    }})()"""


class Results:
    def __init__(self):
        self.rows = []

    def check(self, name, ok, detail):
        self.rows.append({"check": name, "ok": bool(ok), "detail": detail})
        log(f"{'PASS' if ok else 'FAIL'}  {name}: {detail}")


def handler_lines(shell_log):
    return [line for line in shell_log.read_text(errors="replace").splitlines() if "media permission:" in line]


def location(insp):
    """The top document's URL, or None while there is no document to ask."""
    try:
        return insp.eval("JSON.stringify(location.href)", timeout=3)
    except Exception:
        return None


def navigate(insp, url, prefix, timeout=30):
    """Navigate the window top-level to url and wait until a document at prefix runs."""
    insp.eval(f"(async () => {{ setTimeout(() => {{ location.href = {json.dumps(url)}; }}, 20); "
              "return JSON.stringify('going'); })()")

    def arrived():
        insp.pump(0.2)
        here = location(insp)
        return here if here and here.startswith(prefix) else None
    return wait_for(f"the window at {prefix}", arrived, timeout, 0.2)
