#!/usr/bin/env python3
# Usage: scripts/shell-check.py [--no-build] [--keep] [--port 8899] [--json OUT] [--skip-route]
#                               [--only quit]
"""Live check of the Tauri shell's voice plumbing (chat-voice WP5), in the real
shell binary, with nothing reaching the desktop, the real audio graph or the
installed app.

It builds the debug shell and the headless runner, seeds a fresh data dir under
target/shell-check-<pid>/ (a work dir per run, so a second run never removes the
first one's tree or podman stub) with the headless runner (a container prefix of its own,
lmgw-shellcheck-<pid>, bind 127.0.0.1:<port>, update checks off), and starts
the shell on it with:

- no real podman: a stub first on PATH records every call and runs none (so it
  refuses every removal), and a check reads the record back — every call the
  seed and the shell made must name this run's prefix and nothing else, whatever
  its verb: a listing filters on the run's label, a read names only the run's
  containers, and anything that changes something names them in every operand;
- its own runtime, cache, config and data dirs under target/shell-check-<pid>
  (XDG_RUNTIME_DIR, XDG_CACHE_HOME, XDG_CONFIG_HOME — with the Downloads folder
  inside it — and XDG_DATA_HOME), so agent run dirs, the build lock, the
  broadway sockets, GStreamer's registry and any download stay in it;
- a private D-Bus session bus, so the single-instance plugin cannot reach the
  installed app (a debug build also uses its own bus name, src-tauri dev_guard);
- a private GTK3 broadwayd display, so no window appears;
- WebKit's mock capture devices (LMGW_MOCK_CAPTURE=1), so no microphone opens;
- a private PipeWire graph (pipewire + wireplumber's policy-only profile +
  pipewire-pulse, under target/shell-check-<pid>/run, their own config and state
  dirs) whose only sinks are two null sinks. Nothing can play audibly or touch
  a real device, a real stream or the real WirePlumber memory. Without
  pipewire/wireplumber the shell gets no audio server at all and the routing
  checks are skipped;
- WebKit's remote inspector (WEBKIT_INSPECTOR_HTTP_SERVER) on loopback, which
  is how the checks run script in the page and its frames. It is
  unauthenticated: while the check runs (about six minutes: 353 s for 148
  checks in the WP11 UI fix run), any local process could script the dev window,
  whose data is the scratch dir's; broadwayd's HTTP port is open on loopback as
  long.

Checks (§15 WP5):
- media: audio getUserMedia is granted without a prompt; video, audio+video and
  display capture are denied by the handler; an agent-origin frame and the
  chat preview's sandboxed frame are denied (by WebKit's Permissions Policy,
  before the handler); a frame given allow="microphone" is measured against
  the dashboard's Permissions-Policy header; enumerateDevices shows labels
  after the grant.
- bind (WP11 review m3): with the run's port held by another process, the
  debug shell refuses to start, says why, builds no window and asks podman
  nothing.
- document (review M1): the window navigated top-level to an agent app's
  origin is denied the microphone and the shell's commands; from there, a
  navigation back to the gateway with a microphone request in flight is never
  granted (a sweep of delays, both orders), and the shell's commands are
  flooded during such a navigation. Whether anything landed in the
  provisional window — the handler's or the command gate's "navigation in
  progress" — is reported; on WebKitGTK 2.54 nothing did.
- route (private graph): audio_outputs lists the null sinks with the default;
  audio_output_set fails visibly with no stream; a silent playback context's
  streams carry application.name/id lmgw-dev (a debug build's); routing them to
  the second sink sets
  target.object and relinks them; a second WebKitGTK app started afterwards is
  keyed apart by WirePlumber and stays on the default; a context opened later
  follows the remembered choice; null clears the key and WirePlumber's
  remembered target; an unknown sink is refused.
- popover (chat-voice WP6, private graph): the Chat composer's devices popover
  lists the shell's outputs with the default marked; with an output chosen
  while no playback context exists (WirePlumber remembers nothing then), the
  first test tone is recorded off both null sinks' monitors (pw-record): the
  three-note chime arrives whole on the chosen one from its first note and
  nothing on the other (review M1); the popover says where it plays; every
  lmgw stream — the test microphone's capture context's too — is on it; one
  route application per context made. After the 10 s idle suspend the next
  tone resumes it with one route application (review M2) and arrives whole
  again; "System default" sends every stream back and clears the memory.
- dictation (chat-voice WP7): a dictation round through the Chat composer's
  microphone in the app — mock capture at 16 kHz, the WAV upload, the text at
  the caret with its mark, Esc, Right Ctrl held and Right Ctrl + another key,
  the hold and a failed fallback said — against a mock ASR the page answers
  itself (scripts/media-probe.js), every track ended after each.
- realtime (chat-voice WP9): voice mode in the app against a bound session
  the page answers itself (scripts/realtime-mock.js): the state walk, the
  appends at 24 kHz, captions and bubbles, a barge-in's truncate, stop
  talking, push-to-talk, M, a takeover with Re-enter, Esc releasing every
  track.

- quit (the final review's F-1): SIGTERM quits the real shell as its
  tray's Quit does (both ask Tauri to exit, and every exit runs the same
  sequence, src-tauri gateway::quit): with the Chat change feed open as a
  client holds it, the server stops first — the feed ends whole, its last
  chunk sent — and is waited for, and only then the model containers stop
  (the log says each step, in that order); the process exits 0 within the
  quit's bound. It is the last check of a full run, which ends the shell
  this way, and `--only quit` runs it alone (seed, shell, quit: no audio
  graph, no inspector checks).

The debug shell keeps its webview store (cookies, local storage, cache) in the
data dir, so the installed app's is not touched either. Needs broadwayd (gtk3),
dbus-daemon, python3-websocket-client, and for the routing checks pipewire,
wireplumber, pipewire-pulse and pw-dump. Everything it starts is killed at the
end — also on SIGTERM/SIGHUP, and every child dies with the script
(PR_SET_PDEATHSIG) — and target/shell-check-<pid> is removed unless --keep. Exit
status 0 when every check passed.

The WP5 checks and the run live here; the WP6, WP7 and WP9 checks in
shell_check_voice.py, what both share in shell_check_lib.py, and the process
handling (shared with webkit-check.py) in private_session.py.
"""
import argparse
import json
import os
import re
import shutil
import signal
import socket
import sqlite3
import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import private_session  # noqa: E402
from private_session import port_free, stop, stop_all, wait_for  # noqa: E402
from shell_check_lib import (  # noqa: E402
    PREFIX, REPO, STREAM_ID, WORK, Inspector, Results, audio_graph, base_env, broadway, free_port,
    gum, handler_lines, http_ok, invoke, linked_to, location, log, navigate, playback,
    private_bus, private_dirs, private_env, props, pw_dump, sinks, spawn, target_of, wp_state, ENUM)
from shell_check_voice import dictation_checks, popover_checks, realtime_checks  # noqa: E402


def preview_sandbox():
    """The chat's HTML preview frame's sandbox attribute, read from the page source."""
    src = (REPO / "crates/lmgw-ui/src/pages/chat.rs").read_text()
    m = re.search(r'class="preview-frame"\s*sandbox="([^"]*)"', src)
    if not m:
        raise RuntimeError("the preview frame's sandbox attribute was not found in pages/chat.rs")
    return m.group(1)


# --- build and seed -------------------------------------------------------------------------


def build():
    jobs = os.environ.get("CARGO_BUILD_JOBS", "8")
    for args in (["cargo", "build", "-p", "lmgw", "-j", jobs],
                 ["cargo", "build", "-p", "lmgw-core", "--example", "headless", "-j", jobs]):
        log(" ".join(args))
        r = subprocess.run(args, cwd=REPO, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        if r.returncode != 0:
            sys.stdout.write(r.stdout.decode(errors="replace")[-4000:])
            raise RuntimeError("build failed")


def seed(data, addr, run):
    """A fresh data dir, booted once by the headless runner: it moves the container
    prefix off the production default (to this run's own) and pins bind_addr before
    anything lists or removes a container."""
    data.mkdir(parents=True)
    env = private_env(run)
    env.update(LMGW_DATA_DIR=str(data), LMGW_DEV="1", RUST_LOG="warn", SHELL_CHECK_WHO="seed")
    p = spawn([str(REPO / "target/debug/examples/headless"), addr], env, WORK / "seed.log")
    wait_for("the headless runner", lambda: http_ok(f"http://{addr}/v1/models") or p.poll() is not None, 120)
    if p.poll() is not None:
        raise RuntimeError(f"headless runner exited; see {WORK / 'seed.log'}")
    # Its boot passes ask the podman stub, which refuses; stopping the runner
    # before they have would kill the stub (one process group) unrecorded.
    try:
        wait_for("the seed's boot passes", lambda: all(
            w in (WORK / "seed.log").read_text(errors="replace")
            for w in ("boot reconciliation", "agent container reconciliation")), 20)
    except RuntimeError:
        log("the seed's boot passes said nothing within 20 s; stopping it anyway")
    stop(p, signal.SIGINT, 60)
    db = sqlite3.connect(data / "lmgw.sqlite")
    s = json.loads(db.execute("SELECT value FROM settings WHERE key = 'settings'").fetchone()[0])
    if s.get("container_prefix") != PREFIX or s.get("bind_addr") != addr:
        raise RuntimeError(f"seed did not steer the dir: {s.get('container_prefix')} {s.get('bind_addr')}")
    s["update_check_enabled"] = False
    db.execute("UPDATE settings SET value = ? WHERE key = 'settings'", (json.dumps(s),))
    db.commit()
    db.close()
    log(f"seeded {data} (prefix {s['container_prefix']}, bind {addr})")


# The dashboard's HTML carries `Permissions-Policy: microphone=(self), ...`
# (lmgw-core web/ui.rs). Whether WebKitGTK applies it to a cross-origin frame
# that `allow="microphone"` delegates to is measured here.
ALLOWED_FRAME_GRANTED = True  # measured on WebKitGTK 2.54: the header does not stop it
ALLOWED_FRAME_CHECK = ('a frame given allow="microphone" IS granted despite the Permissions-Policy '
                       'header (WebKitGTK 2.54; the source scan is the guard)')


def media_checks(insp, port, shell_log, res):
    insp.eval("(async () => { for (let i = 0; i < 100 && document.readyState !== 'complete'; i++) "
              "await new Promise(r => setTimeout(r, 100)); return JSON.stringify(location.pathname); })()")
    before = insp.eval(ENUM)
    res.check("enumerateDevices before a grant", all(not d["label"] for d in before),
              f"{len(before)} entries, labels hidden until a capture is granted (WebKit's rule)")
    n0 = len(handler_lines(shell_log))
    # Order matters: WebKit remembers a frame's denials and answers a later
    # request that includes a denied kind from that memory, without asking the
    # handler — audio first, then audio+video, which therefore still reaches it.
    r = insp.eval(gum({"audio": True}))
    res.check("audio granted without a prompt", r["ok"] and any(t.startswith("audio:Mock audio device") for t in r["tracks"]), r)
    after = insp.eval(ENUM)
    labelled = [d["label"] for d in after if d["kind"] == "audioinput" and d["label"]]
    res.check("enumerateDevices shows labels after the grant", bool(labelled) and all(d["id"] for d in after if d["label"]),
              labelled)
    r = insp.eval(gum({"audio": True, "video": True}))
    res.check("audio+video denied", not r["ok"] and "NotAllowedError" in r["err"], r)
    r = insp.eval(gum({"video": True}))
    res.check("video denied", not r["ok"] and "NotAllowedError" in r["err"], r)
    r = insp.eval(gum({"video": True}, display=True), gesture=True)
    res.check("display capture denied", not r["ok"], r)
    time.sleep(0.5)
    lines = handler_lines(shell_log)[n0:]
    want = ["granted for http://127.0.0.1:%d" % port, "denied for http://127.0.0.1:%d: video" % port,
            "denied for http://127.0.0.1:%d: display capture" % port]
    seen = [any(w in line for line in lines) for w in want]
    res.check("each top-level request reached the shell's handler", all(seen),
              [line.split("media permission: ", 1)[1] for line in lines])

    # Frames: an agent app's origin, the same with allow="microphone", and the
    # chat preview's sandboxed srcdoc frame.
    known = set(insp.contexts)
    insp.eval(f"""(async () => {{
      const mk = (id, attrs) => {{ const f = document.createElement('iframe'); f.id = id;
        for (const [k, v] of Object.entries(attrs)) f.setAttribute(k, v);
        f.style.cssText = 'position:fixed;left:0;top:0;width:40px;height:40px;opacity:0';
        document.body.appendChild(f); }};
      mk('wp5-agent', {{src: 'http://wp5probe.localhost:{port}/'}});
      mk('wp5-agent-allowed', {{src: 'http://wp5allowed.localhost:{port}/', allow: 'microphone'}});
      mk('wp5-preview', {{sandbox: {json.dumps(preview_sandbox())}, srcdoc: '<!doctype html><p>preview</p>'}});
      return JSON.stringify('ok'); }})()""")
    frames = {}

    def found():
        insp.pump(0.2)
        tree = insp.call("Page.getResourceTree").get("frameTree", {})
        for child in tree.get("childFrames", []):
            f = child["frame"]
            for key, needle in (("agent", "wp5probe.localhost"), ("allowed", "wp5allowed.localhost"), ("preview", "about:srcdoc")):
                if needle in f.get("url", ""):
                    # The frame's page world: the newest normal context it has.
                    ids = [cid for cid, c in insp.contexts.items() if cid not in known
                           and c.get("frameId") == f["id"] and c.get("type", "normal") == "normal"]
                    if ids:
                        frames[key] = max(ids)
        return len(frames) == 3

    try:
        wait_for("the three probe frames", found, 30, 0.1)
    except RuntimeError:
        pass
    n1 = len(handler_lines(shell_log))
    allowed_granted = None
    for key, name, expect_ok in (("agent", "agent-origin frame denied", False),
                                 ("preview", "HTML preview frame denied", False),
                                 ("allowed", ALLOWED_FRAME_CHECK, ALLOWED_FRAME_GRANTED)):
        if key not in frames:
            res.check(name, False, f"the frame never got a script context ({sorted(frames)})")
            continue
        r = insp.eval(gum({"audio": True}), context=frames[key])
        if key == "allowed":
            allowed_granted = r["ok"]
        res.check(name, r["ok"] == expect_ok, r)
    time.sleep(0.5)
    frame_lines = handler_lines(shell_log)[n1:]
    res.check("the denied frames never reached the handler (Permissions Policy)",
              len([line for line in frame_lines if "granted" in line]) == (1 if allowed_granted else 0)
              and not [line for line in frame_lines if "denied" in line],
              [line.split("media permission: ", 1)[1] for line in frame_lines])


SILENT = """(async () => {
  const ctx = new AudioContext({sampleRate: 24000});
  const osc = ctx.createOscillator(); const g = ctx.createGain(); g.gain.value = 0;
  osc.connect(g).connect(ctx.destination); osc.start(); await ctx.resume();
  window.__wp5ctx = ctx; return JSON.stringify(ctx.state); })()"""

SECOND_APP = r"""
import sys, gi
gi.require_version("Gtk", "3.0"); gi.require_version("WebKit2", "4.1")
from gi.repository import Gtk, GLib, WebKit2
w = Gtk.Window()
v = WebKit2.WebView(website_policies=WebKit2.WebsitePolicies(autoplay=WebKit2.AutoplayPolicy.ALLOW))
w.add(v)
v.get_settings().set_media_playback_requires_user_gesture(False)
v.load_html("<script>const c = new AudioContext(); const o = c.createOscillator(); const g = c.createGain();"
            "g.gain.value = 0; o.connect(g).connect(c.destination); o.start(); c.resume(); window.c = c;</script>",
            "http://127.0.0.1/")
def state():
    v.evaluate_javascript("c.resume(); c.state", -1, None, None, None,
                          lambda wv, r: print("context", wv.evaluate_javascript_finish(r).to_string(), flush=True))
    return True
w.show_all(); w.present(); GLib.timeout_add(1000, state)
GLib.timeout_add(int(sys.argv[1]) * 1000, Gtk.main_quit); Gtk.main()
"""


def route_checks(insp, client, shell_pid, broadway_n, res):
    dump = pw_dump(client)
    sk = sinks(dump)
    serial = {n: props(o).get("object.serial") for n, o in sk.items()}
    r = insp.eval(invoke("audio_outputs"))
    outs = (r.get("v") or {}).get("outputs") or []
    res.check("audio_outputs lists the sinks and the default",
              r["ok"] and {o["name"] for o in outs} == set(sk) and [o["name"] for o in outs if o["default"]] == ["check_speakers"]
              and all(o["serial"] == serial[o["name"]] for o in outs), r)
    t = time.time()
    r = insp.eval(invoke("audio_output_set", {"sink": "check_headphones"}), timeout=15)
    took = time.time() - t
    waited = re.search(r"waited ([0-9.]+) s", r.get("err") or "")
    res.check("audio_output_set without a stream fails visibly after its wait, within its 2 s deadline",
              not r["ok"] and "no playback stream found" in r["err"] and waited
              and 1.5 <= float(waited.group(1)) <= 2.0 and took < 3.0, {"answer": r, "took": round(took, 2)})

    state = insp.eval(SILENT, gesture=True)
    own = wait_for("lmgw's playback stream", lambda: playback(pw_dump(client), shell_pid), 10)
    res.check("a silent playback context makes an lmgw stream", state == "running" and own,
              f"context {state}, streams {[o['id'] for o in own]}")
    p = props(own[0])
    res.check("lmgw's stream carries its own identity (a debug build's)",
              p.get("application.name") == STREAM_ID and p.get("application.id") == STREAM_ID,
              {k: p.get(k) for k in ("application.name", "application.id", "application.process.binary", "client.api")})

    r = insp.eval(invoke("audio_output_set", {"sink": "check_headphones"}), timeout=15)
    ids = (r.get("v") or {}).get("streams") or []
    hp = sk["check_headphones"]["id"]
    sp = sk["check_speakers"]["id"]

    def moved():
        d = pw_dump(client)
        mine = playback(d, shell_pid)
        ok = mine and all(target_of(d, o["id"]) == str(serial["check_headphones"]) and linked_to(d, o["id"]) == {hp} for o in mine)
        return ok and d
    try:
        wait_for("lmgw's streams on the headphones", moved, 5)
        moved_ok = True
    except RuntimeError:
        moved_ok = False
    res.check("routing moves every lmgw stream to the chosen sink",
              r["ok"] and moved_ok and sorted(ids) == sorted(o["id"] for o in own), r)
    key = f"Output/Audio:application.id:{STREAM_ID}"
    try:
        wait_for("WirePlumber to remember the target", lambda: "check_headphones" in wp_state().get(key, ""), 5)
    except RuntimeError:
        pass
    res.check("WirePlumber remembers it under lmgw's own key", "check_headphones" in wp_state().get(key, ""),
              {k: v for k, v in wp_state().items() if "Output/Audio" in k})

    # A context the page opens after the choice: restore-stream links it to the
    # remembered sink before the page gets to call audio_output_set again.
    insp.eval(SILENT.replace("__wp5ctx", "__wp5ctx2"), gesture=True)
    first = {o["id"] for o in own}
    try:
        later = wait_for("the second lmgw stream", lambda: [o for o in playback(pw_dump(client), shell_pid)
                                                             if o["id"] not in first], 10)
        later_ok = wait_for("it to land on the headphones", lambda: all(
            linked_to(pw_dump(client), o["id"]) == {hp} for o in later), 5)
    except RuntimeError:
        later, later_ok = [], False
    res.check("a later lmgw stream follows the remembered choice", later and later_ok,
              {"stream": [o["id"] for o in later], "links": [sorted(linked_to(pw_dump(client), o["id"])) for o in later]})

    # Another WebKitGTK app, started after lmgw's choice was remembered. The
    # run's own dirs, so WebKit's default store for it is not under the real
    # ~/.local/share (review n4).
    env = base_env()
    env.update(client, GDK_BACKEND="broadway", BROADWAY_DISPLAY=f":{broadway_n}",
               XDG_RUNTIME_DIR=str(WORK / "run"), XDG_CACHE_HOME=str(WORK / "cache"),
               XDG_DATA_HOME=str(WORK / "xdg-data"), XDG_CONFIG_HOME=str(WORK / "config"))
    other = spawn([sys.executable, "-c", SECOND_APP, "20"], env, WORK / "second-app.log")
    try:
        theirs = wait_for("the second app's stream", lambda: playback(pw_dump(client), other.pid), 15)
    except RuntimeError:
        theirs = []
    if theirs:
        time.sleep(1.0)
        d = pw_dump(client)
        theirs = playback(d, other.pid)
        tp = props(theirs[0])
        res.check("another WebKitGTK app is keyed apart and stays on the default",
                  tp.get("application.id") not in ("lmgw", STREAM_ID)
                  and tp.get("application.name") not in ("lmgw", STREAM_ID)
                  and all(linked_to(d, o["id"]) == {sp} and target_of(d, o["id"]) is None for o in theirs),
                  {"name": tp.get("application.name"), "id": tp.get("application.id"),
                   "links": [sorted(linked_to(d, o["id"])) for o in theirs]})
    else:
        res.check("another WebKitGTK app is keyed apart and stays on the default", False,
                  f"its stream never appeared; see {WORK / 'second-app.log'}")
    stop(other)

    r = insp.eval(invoke("audio_output_set", {"sink": None}), timeout=15)

    def cleared():
        d = pw_dump(client)
        mine = playback(d, shell_pid)
        return mine and all(target_of(d, o["id"]) is None and linked_to(d, o["id"]) == {sp} for o in mine)
    try:
        wait_for("lmgw's streams back on the default", cleared, 5)
        cleared_ok = True
    except RuntimeError:
        cleared_ok = False
    time.sleep(1.5)
    res.check("null clears the key and every lmgw stream follows the default again",
              r["ok"] and cleared_ok and len((r.get("v") or {}).get("streams") or []) == 2
              and "check_headphones" not in wp_state().get(key, ""),
              {"answer": r, "remembered": wp_state().get(key)})
    r = insp.eval(invoke("audio_output_set", {"sink": "no_such_sink"}), timeout=15)
    res.check("an unknown sink is refused", not r["ok"] and "is not present" in r["err"], r)
    insp.eval("(async () => { await window.__wp5ctx.close(); await window.__wp5ctx2.close(); "
              "return JSON.stringify('closed'); })()")


# Delays (ms) between an agent document's microphone request and its
# navigation to the gateway, both ways round: a sweep, since the provisional
# window (from the load's start to its commit) is a few milliseconds on
# loopback. Measured on WebKitGTK 2.54: a request made once the navigation is
# pending is rejected by WebKit itself (TypeError) without asking the handler,
# and one made before is decided before the provisional load starts — the
# window was never hit. The sweep stays as the regression check.
PROVISIONAL_DELAYS = [0, 2, 5, 10, 20, 40]


def document_checks(insp, port, shell_log, res):
    """Review M1: an agent app's top-level document, and its navigation back."""
    agent = f"http://wp5probe.localhost:{port}/"
    gateway = f"http://127.0.0.1:{port}/"
    navigate(insp, agent, agent)
    n0 = len(handler_lines(shell_log))
    r = insp.eval(gum({"audio": True}))
    time.sleep(0.5)
    lines = handler_lines(shell_log)[n0:]
    res.check("a top-level agent-app document is denied the microphone by the handler",
              not r["ok"] and any(f"denied for http://wp5probe.localhost:{port}: not the gateway's origin" in line
                                  for line in lines),
              {"answer": r, "handler": [line.split("media permission: ", 1)[1] for line in lines]})
    r = insp.eval(invoke("audio_outputs"))
    res.check("a top-level agent-app document cannot call the shell's commands", not r["ok"], r)

    # WebKit remembers a denial per origin pair for the page, so each attempt
    # asks from an agent origin of its own.
    gum_js = "navigator.mediaDevices.getUserMedia({audio: true}).then(s => s.getTracks().forEach(t => t.stop()), () => {})"
    granted, reasons, attempts = [], {}, 0
    for order in ("ask, then navigate", "navigate, then ask"):
        for k in PROVISIONAL_DELAYS:
            origin = f"http://wp5nav{attempts}.localhost:{port}/"
            navigate(insp, origin, origin)
            n = len(handler_lines(shell_log))
            go = f"location.href = {json.dumps(f'{gateway}?wp5nav={attempts}')}"
            first, then = (gum_js, go) if order.startswith("ask") else (go, gum_js)
            try:
                insp.eval(f"(() => {{ {first}; setTimeout(() => {{ {then}; }}, {k}); "
                          "return JSON.stringify('asked'); })()", timeout=5)
            except Exception:
                pass
            attempts += 1
            try:
                wait_for("the gateway document", lambda: (location(insp) or "").startswith(gateway), 20, 0.1)
            except RuntimeError:
                pass
            time.sleep(0.3)
            for line in handler_lines(shell_log)[n:]:
                what = line.split("media permission: ", 1)[1]
                if "granted" in what:
                    granted.append((order, k, what))
                else:
                    reason = what.rsplit(": ", 1)[-1]
                    reasons.setdefault(f"{order}: {reason}", []).append(k)
    # The shell's commands, flooded from a navigating agent document: one that
    # reached the shell's gate during the provisional window is logged as
    # refused there (Tauri's own check, which reads the active URI, let it by).
    def gate_lines():
        return [line.split("lmgw:", 1)[-1].strip() for line in shell_log.read_text(errors="replace").splitlines()
                if "command " in line and " refused: " in line]
    c0 = len(gate_lines())
    for i in range(2):
        origin = f"http://wp5ipc{i}.localhost:{port}/"
        navigate(insp, origin, origin)
        insp.eval(f"""(() => {{
          location.href = {json.dumps(f'{gateway}?wp5ipc={i}')};
          (async () => {{ for (let j = 0; j < 2000; j++) {{
              window.__TAURI_INTERNALS__.invoke('audio_outputs').catch(() => {{}});
              await new Promise(r => setTimeout(r, 1)); }} }})();
          return JSON.stringify('flooding'); }})()""", timeout=5)
        try:
            wait_for("the gateway document", lambda: (location(insp) or "").startswith(gateway), 20, 0.1)
        except RuntimeError:
            pass
        time.sleep(0.3)
    refused = gate_lines()[c0:]
    res.check("an agent document navigating to the gateway is never granted the microphone",
              not granted and attempts,
              {"attempts": attempts, "granted": granted, "handler said (delays in ms)": reasons,
               "provisional window hit": any("navigation in progress" in r for r in reasons),
               "commands refused at the shell's gate": refused[:3], "of": len(refused)})


# The quit's bound (src-tauri gateway::QUIT_WITHIN: the server's 10 s and a
# 2 s margin), and what the check allows on top for the process to go.
QUIT_WITHIN_S = 12
QUIT_SLACK_S = 5


def quit_check(shell, shell_log, port, res):
    """F-1: SIGTERM quits the shell as the tray's Quit does — the server stops
    and is waited for, the open feed ends whole, then the model containers'
    stop, and the process exits 0 within the bound."""
    db = sqlite3.connect(WORK / "data" / "lmgw.sqlite")
    key = db.execute("SELECT key_plain FROM api_keys WHERE name = 'owner:dashboard'").fetchone()[0]
    db.close()
    feed = socket.create_connection(("127.0.0.1", port), timeout=20)
    feed.sendall((f"GET /chat/api/feed HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n"
                  f"Authorization: Bearer {key}\r\nAccept: text/event-stream\r\n\r\n").encode())
    got = b""
    while b"event: hello" not in got:
        chunk = feed.recv(65536)
        if not chunk:
            break
        got += chunk
    opened = b"event: hello" in got
    t0 = time.monotonic()
    os.kill(shell.pid, signal.SIGTERM)
    ended_whole = False
    try:
        while True:
            chunk = feed.recv(65536)
            if not chunk:
                break
            got += chunk
        ended_whole = got.endswith(b"0\r\n\r\n")
    except OSError as e:
        got += f"<{e}>".encode()
    feed_after = round(time.monotonic() - t0, 2)
    feed.close()
    try:
        code = shell.wait(timeout=QUIT_WITHIN_S + QUIT_SLACK_S)
    except subprocess.TimeoutExpired:
        code = "still running"
    took = round(time.monotonic() - t0, 2)
    text = shell_log.read_text(errors="replace")
    steps = ["SIGTERM: quitting", "the server stops", "quit: the gateway has stopped"]
    at = [text.find(s) for s in steps]
    in_order = all(i >= 0 for i in at) and at == sorted(at)
    res.check("SIGTERM quits the shell as Quit does: the server stops first and is waited for, "
              "the open feed ends whole, then the model containers stop (F-1)",
              opened and ended_whole and in_order and code == 0
              and took < QUIT_WITHIN_S + QUIT_SLACK_S,
              {"feed opened": opened, "feed ended whole": ended_whole, "feed ended after s": feed_after,
               "exit": code, "exited after s": took, "log steps in order": in_order,
               "log positions": dict(zip(steps, at))})


def taken_port_check(env, port, res):
    """Review m3: a debug shell whose port another process holds refuses to start, so it
    never builds a window on (or grants the microphone and its commands to) whoever holds
    it. Run before the real shell, on the same data dir, without the inspector."""
    holder = socket.socket()
    holder.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    holder.bind(("127.0.0.1", port))
    holder.listen()
    env = dict(env, SHELL_CHECK_WHO="shell-taken-port")
    env.pop("WEBKIT_INSPECTOR_HTTP_SERVER", None)
    log_file = WORK / "shell-taken-port.log"
    try:
        p = spawn([str(REPO / "target/debug/lmgw")], env, log_file)
        try:
            code = p.wait(timeout=60)
        except subprocess.TimeoutExpired:
            stop(p)
            code = "still running after 60 s"
    finally:
        holder.close()
    text = log_file.read_text(errors="replace")
    said = [line.split("could not serve on", 1)[1][:160] for line in text.splitlines()
            if "could not serve on" in line]
    calls = WORK / "podman-calls.jsonl"
    asked = [r for r in (json.loads(line) for line in calls.read_text().splitlines())
             if r["who"] == "shell-taken-port"] if calls.exists() else []
    res.check("a debug shell whose port is taken refuses to start, before any window (review m3)",
              code not in (0, "still running after 60 s") and said and "media permission" not in text
              and not asked,
              {"exit": code, "said": said[:1], "podman calls": asked})


# podman verbs that only read; anything else changes something.
READ_ONLY = {("ps",), ("inspect",), ("images",), ("version",), ("info",), ("logs",), ("port",),
             ("container", "ps"), ("container", "ls"), ("container", "list"),
             ("container", "inspect"), ("container", "exists"), ("container", "logs"),
             ("image", "ls"), ("image", "list"), ("image", "inspect"), ("image", "exists"),
             ("system", "info"), ("system", "df")}
LISTINGS = {("ps",), ("container", "ps"), ("container", "ls"), ("container", "list")}
# Flags whose value is the next word, among those lmgw's podman calls use.
VALUE_FLAGS = {"-t", "--time", "--timeout", "-s", "--signal", "--format", "--filter", "--label",
               "-l", "--name", "--condition"}


def podman_call_flag(argv, prefix):
    """Why a recorded podman call is not scoped to this run's prefix, or None.

    A listing must filter on the run's lmgw.instance label; another read-only
    verb may name only the run's containers (or nothing, as version/info do);
    every other verb — rm, rmi, image rm, volume rm, system prune, run, … —
    must name the run's prefix in every operand (run/create: its --name)."""
    own = f"label=lmgw.instance={prefix}"
    if any(a.startswith("label=lmgw.instance=") and a != own for a in argv):
        return "names another instance's label"
    words = [a for i, a in enumerate(argv)
             if not a.startswith("-") and not (i and argv[i - 1] in VALUE_FLAGS)]
    verb = tuple(words[:2]) if tuple(words[:2]) in READ_ONLY else tuple(words[:1])
    operands = [a for a in words[len(verb):] if a != own]
    if verb in LISTINGS:
        return None if own in argv else "a listing without the run's label filter"
    if verb in READ_ONLY:
        return None if all(a.startswith(prefix) for a in operands) else "reads another name"
    if verb and verb[0] in ("run", "create"):
        names = [argv[i + 1] for i, a in enumerate(argv[:-1]) if a == "--name"]
        names += [a.split("=", 1)[1] for a in argv if a.startswith("--name=")]
        return None if names and all(n.startswith(prefix) for n in names) else "starts another name"
    if operands and all(a.startswith(prefix) for a in operands):
        return None
    return f"{' '.join(verb) or 'a call'} not scoped to the run's prefix"


def podman_check(res):
    """B1: every podman call of the seed and the shell named this run's prefix,
    and both made their boot listings (so the record is not empty by accident).
    Every verb is checked, not only rm/stop/kill (review n5)."""
    f = WORK / "podman-calls.jsonl"
    rows = [json.loads(line) for line in f.read_text().splitlines()] if f.exists() else []
    own = f"label=lmgw.instance={PREFIX}"
    scoped = {}
    for r in rows:
        if own in r["argv"]:
            scoped[r["who"]] = scoped.get(r["who"], 0) + 1
    flagged = [{"argv": r["argv"], "who": r["who"], "why": why} for r in rows
               if (why := podman_call_flag(r["argv"], PREFIX))]
    res.check("podman (stubbed) was only ever asked about this run's prefix, by the seed and the shell",
              scoped.get("seed") and scoped.get("shell") and not flagged,
              {"calls": len(rows), "scoped listings": scoped, "not scoped": flagged,
               "verbs": sorted({" ".join(a for a in r["argv"][:2] if not a.startswith("-")) for r in rows})})


# --- main --------------------------------------------------------------------------------------


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--port", type=int, default=8899)
    ap.add_argument("--no-build", action="store_true")
    ap.add_argument("--keep", action="store_true",
                    help="keep target/shell-check-<pid> (logs, data dir)")
    ap.add_argument("--skip-route", action="store_true", help="media checks only, no audio server at all")
    ap.add_argument("--json", help="write the results here")
    ap.add_argument("--only", choices=["quit"],
                    help="run one check alone (quit: seed, shell, the quit check)")
    args = ap.parse_args()
    only_quit = args.only == "quit"

    # SIGTERM/SIGHUP (a harness timeout, a closed terminal) unwind through the
    # `finally` below like Ctrl-C does.
    private_session.unwind_on_signals()

    if not port_free(args.port):
        sys.exit(f"127.0.0.1:{args.port} is in use")
    if WORK.exists():
        shutil.rmtree(WORK)
    WORK.mkdir(parents=True)
    run = WORK / "run"
    run.mkdir(mode=0o700)
    private_dirs()
    addr = f"127.0.0.1:{args.port}"
    res = Results()
    socks = []
    try:
        if not args.no_build:
            build()
        seed(WORK / "data", addr, run)
        bus = private_bus(run)
        n, socks = broadway(run)
        # GTK3's broadwayd aborts when a second client leaves, so the other
        # WebKitGTK app of the routing checks gets a display of its own.
        no_audio = args.skip_route or only_quit
        n2, socks2 = (None, []) if no_audio else broadway(run)
        socks += socks2
        client = None if no_audio else audio_graph(run, bus)
        if client is None and not no_audio:
            log("pipewire/wireplumber/pipewire-pulse/pw-dump not all found: routing checks skipped")
        iaddr = f"127.0.0.1:{free_port()}"
        env = private_env(run)
        if shutil.which("gst-inspect-1.0"):
            # GStreamer's registry, built once in the private cache dir before
            # WebKit needs it, rather than inside a timed check.
            subprocess.run(["gst-inspect-1.0"], env=env, stdout=subprocess.DEVNULL,
                           stderr=subprocess.DEVNULL, timeout=120)
        env.update(GDK_BACKEND="broadway", BROADWAY_DISPLAY=f":{n}", DBUS_SESSION_BUS_ADDRESS=bus,
                   LMGW_DATA_DIR=str(WORK / "data"), LMGW_DEV="1", LMGW_MOCK_CAPTURE="1",
                   SHELL_CHECK_WHO="shell",
                   WEBKIT_INSPECTOR_HTTP_SERVER=iaddr,
                   RUST_LOG="lmgw=info,lmgw_core=warn,lmgw_core::server=info",
                   # No GStreamer path to a sound card that bypasses the audio server.
                   GST_PLUGIN_FEATURE_RANK="alsasink:0,alsasrc:0,alsadeviceprovider:0,oss4sink:0,osssink:0")
        if client:
            env.update(client)
        else:
            env.update(PIPEWIRE_REMOTE="lmgw-shell-check-none", PULSE_SERVER="unix:/nonexistent/lmgw-shell-check")
        if not only_quit:
            taken_port_check(env, args.port, res)
        shell_log = WORK / "shell.log"
        shell = spawn([str(REPO / "target/debug/lmgw")], env, shell_log)
        log(f"shell pid {shell.pid} on broadway :{n}, inspector {iaddr}")
        if not only_quit:
            insp = Inspector(iaddr, f"http://{addr}/")
            media_checks(insp, args.port, shell_log, res)
            if client:
                route_checks(insp, client, shell.pid, n2, res)
                popover_checks(insp, client, shell.pid, args.port, shell_log, res)
            dictation_checks(insp, args.port, res)
            realtime_checks(insp, args.port, res)
            document_checks(insp, args.port, shell_log, res)
        wait_for("the shell's gateway", lambda: http_ok(f"http://{addr}/api/version"), 120)
        # The last check ends the shell, the way a logout or `kill` would.
        quit_check(shell, shell_log, args.port, res)
        stop(shell, signal.SIGTERM, 30)
    except Exception as e:  # report what ran, and why the rest did not
        res.check("the check ran to the end", False, f"{type(e).__name__}: {e}")
    finally:
        stop_all(log)
        # Whatever ran, and however far: what the seed and the shell asked podman.
        try:
            podman_check(res)
        except Exception as e:
            res.check("podman (stubbed) was only ever asked about this run's prefix", False,
                      f"{type(e).__name__}: {e}")
        for s in socks:
            if s.exists():
                s.unlink()
        if args.json:
            Path(args.json).write_text(json.dumps(res.rows, indent=1) + "\n")
        if not args.keep:
            shutil.rmtree(WORK, ignore_errors=True)
    failed = [r for r in res.rows if not r["ok"]]
    log(f"{len(res.rows) - len(failed)}/{len(res.rows)} passed")
    return 1 if failed or not res.rows else 0


if __name__ == "__main__":
    sys.exit(main())
