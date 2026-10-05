#!/usr/bin/env python3
# Usage: scripts/ui-drive.py STEPS.json [--size 1440x900] [--dsf 1] [--shots DIR] [--base URL] [--block-writes]
#                            [--fake-mic WAV] [--mute-audio]
"""Drive the dashboard through a scripted interaction and check what it did.

ui-matrix.py answers "does every page fit at every size"; this answers "does
this control behave": open a Select with the keyboard, type into a filter,
press Esc inside a modal, click a backdrop. Keys and clicks go through the
DevTools input domain, so they are trusted events with their default actions
(a real Esc reaches a <dialog>, a real click light-dismisses a popover), which
synthetic dispatchEvent calls are not.

STEPS.json is a list of objects, run in order:

  {"size": "1440x600"}                resize the window
  {"nav": "/mcp-servers"}             load the URL afresh (a full reload) and wait
                                      until the page is quiet
  {"go": "/settings"}                 IN-APP navigation, the way a user moves: a
                                      trusted click on the first visible
                                      a[href="/settings"] (sidebar or page), then
                                      wait until quiet. The app is not reloaded,
                                      so the page left behind is disposed by the
                                      router — what `nav` never exercises.
  {"go": "/x", "via": "history"}      in-app too, through history.pushState +
                                      popstate (what Back/Forward do; no link, so
                                      a DirtyGuard is not asked)
  {"click": "css", "nth": 0}          mouse click at the element's centre
  {"dblclick": "css", "nth": 0}       a double-click there (two presses, the
                                      second with clickCount 2, 90 ms apart)
  {"click_at": [x, y]}                mouse click at a viewport point
  {"key": "ArrowDown", "n": 3}        press a key n times (Enter, Escape, Tab, a…)
  {"keydown": "ControlRight"}         press a key and keep it down (ControlRight,
  {"keyup": "ControlRight"}           ControlLeft, Shift…, Space), then let it go:
                                      what a hold-to-talk key needs (trusted events)
  {"mousedown": "css", "nth": 0}      press the mouse at the element's centre and
  {"mouseup": "css", "nth": 0}        keep it down, then release it there
  {"type": "gem 12"}                  type text into the focused element
  {"js": "expr"}                      evaluate and print the result
  {"script": "scripts/x.js"}          evaluate a file's code in the page (a
                                      helper the later steps call: an in-page
                                      mock, a harness), its result printed
  {"expect": "expr", "label": "…"}    evaluate (a promise is awaited, 30 s or "timeout");
                                      truthy is PASS, anything else FAIL
  {"wait": 0.5}                       sleep
  {"until": "expr", "timeout": 30}    re-evaluate (a promise is awaited) every 0.25 s until truthy (PASS) or
                                      the timeout runs out (FAIL) — for work that
                                      finishes on its own time, like an ingest
  {"shot": "name"}                    viewport PNG to --shots/name.png
  {"repeat": 10, "steps": [...]}      run the inner steps n times
  {"snap": "a"}                       garbage-collect, then record the leak
                                      counters: window/document listeners,
                                      intervals, ResizeObserver observations,
                                      EventSources, and the DOM nodes the
                                      renderer holds outside the document
                                      (detached: what a left page pins)
  {"same": ["a", "b"], "nodes": 300}  FAIL when any counter differs between two
                                      snaps, or detached nodes grew by more
                                      than "nodes" (default 0)

A wasm panic ("panicked at", "RuntimeError: unreachable", "already been
disposed") in the console is always a FAIL, listed once per source location,
so a guard/leak/panic check is a plain step list: `go` back and forth, then
`same`. Visit each page once before the first snap: the renderer keeps a
template of every view it has drawn, so a first visit always adds a few
dozen detached nodes that are a cache, not a leak.

--fake-mic WAV gives Chrome a fake microphone that plays WAV once (Chrome's
--use-fake-device-for-media-stream and --use-file-for-fake-audio-capture,
"%noloop"), grants it without a prompt, and mutes the browser (--mute-audio):
no real microphone is opened and nothing is audible. For the chat voice drives
(scripts/drive/chat-voice-*.json); use a TTS-generated recording.

--block-writes answers every non-GET fetch in the page with a local 400 and
records it in window.__blocked, so a destructive control can be pressed (a
double-click on Delete) without anything reaching the server.

Live-bus code (a tail, a tick, a reconnect) is driven without traffic:
`window.__frame("request", row)` dispatches a synthetic SSE frame of that
name on the app's open EventSource, exactly what the server's frame would
reach (a `js` step; `row` is an object or a JSON string).

scripts/drive/ holds the in-app regression checks (run with --block-writes):
inapp-leaks.json (every sidebar page, visited again after a warm-up, leaves
nothing behind), inapp-panics.json (every route left mid-load at seven
speeds: no wasm panic), inapp-guard.json (a Settings draft reached by a
link asks on the sidebar, not on its own tabs) and live-replay.json (the MCP
page does not refetch tools — which connects idle servers — when a bus
reconnect replays the same statuses, and does on a real change).

Browsing must never start a container; the lmgw-dev names are compared before
and after. Exit status: 0 all expectations passed, 1 on a FAIL, 3 when a
container appeared or vanished.
"""
import argparse
import asyncio
import base64
import json
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import urllib.request
from pathlib import Path

import websockets

sys.path.insert(0, str(Path(__file__).resolve().parent))
import chrome_profile  # noqa: E402

KEYS = {
    # key: (code, windowsVirtualKeyCode)
    "Enter": ("Enter", 13), "Escape": ("Escape", 27), "Tab": ("Tab", 9),
    "Backspace": ("Backspace", 8), "Delete": ("Delete", 46), " ": ("Space", 32),
    "ArrowDown": ("ArrowDown", 40), "ArrowUp": ("ArrowUp", 38),
    "ArrowLeft": ("ArrowLeft", 37), "ArrowRight": ("ArrowRight", 39),
    "PageDown": ("PageDown", 34), "PageUp": ("PageUp", 33),
    "Home": ("Home", 36), "End": ("End", 35), "/": ("Slash", 191),
}

# Keys held down by {"keydown": …}: code -> (key, windowsVirtualKeyCode,
# location, the modifier bit it sets while down).
HELD = {
    "ControlRight": ("Control", 17, 2, 2), "ControlLeft": ("Control", 17, 1, 2),
    "ShiftLeft": ("Shift", 16, 1, 8), "ShiftRight": ("Shift", 16, 2, 8),
    "AltLeft": ("Alt", 18, 1, 1),
    "Space": (" ", 32, 0, 0),
}


# Counts what a page registers globally, so a page that is left but never
# disposed shows up as a counter that grows with every visit.
PROBE = r"""(() => {
  const L = window.__probe = {win: new Map(), doc: new Map(), iv: new Set(), ro: 0, es: 0};
  const bucket = t => t === window ? L.win : (t === document ? L.doc : null);
  const cap = o => typeof o === 'boolean' ? o : !!(o && o.capture);
  const add = EventTarget.prototype.addEventListener;
  const rem = EventTarget.prototype.removeEventListener;
  EventTarget.prototype.addEventListener = function(type, fn, opts) {
    const b = bucket(this);
    if (b && fn) { const k = type + (cap(opts) ? '/capture' : '');
      if (!b.has(k)) b.set(k, new Set()); b.get(k).add(fn); }
    return add.call(this, type, fn, opts);
  };
  EventTarget.prototype.removeEventListener = function(type, fn, opts) {
    const b = bucket(this);
    if (b && fn) { const s = b.get(type + (cap(opts) ? '/capture' : '')); if (s) s.delete(fn); }
    return rem.call(this, type, fn, opts);
  };
  const si = window.setInterval, ci = window.clearInterval;
  window.setInterval = function(...a) { const id = si.apply(this, a); L.iv.add(id); return id; };
  window.clearInterval = function(id) { L.iv.delete(id); return ci.call(this, id); };
  const RO = window.ResizeObserver;
  window.ResizeObserver = class extends RO {
    constructor(cb) { super(cb); this.__n = 0; }
    observe(el, o) { this.__n++; L.ro++; return super.observe(el, o); }
    unobserve(el) { if (this.__n > 0) { this.__n--; L.ro--; } return super.unobserve(el); }
    disconnect() { L.ro -= this.__n; this.__n = 0; return super.disconnect(); }
  };
  const ES = window.EventSource;
  window.__eventSources = [];
  window.EventSource = class extends ES {
    constructor(...a) { super(...a); L.es++; window.__eventSources.push(this); }
    close() { L.es--; return super.close(); }
  };
  // A synthetic frame on the live bus, as if the server had sent it.
  window.__frame = (name, data) => {
    const es = window.__eventSources.filter(e => e.readyState !== 2).at(-1);
    if (!es) return false;
    es.dispatchEvent(new MessageEvent(name, {data: typeof data === 'string' ? data : JSON.stringify(data)}));
    return true;
  };
  window.__counters = () => {
    const o = {};
    for (const [n, m] of [['window', L.win], ['document', L.doc]])
      for (const [k, s] of m) if (s.size) o[n + ':' + k] = s.size;
    o.intervals = L.iv.size; o.resize_observed = L.ro; o.event_sources = L.es;
    return o;
  };
})();"""

BLOCK_WRITES = r"""(() => {
  window.__blocked = [];
  const f = window.fetch;
  window.fetch = function(input, init) {
    const method = ((init && init.method) || (input && input.method) || 'GET').toUpperCase();
    const url = typeof input === 'string' ? input : (input && input.url) || String(input);
    if (method !== 'GET' && method !== 'HEAD') {
      window.__blocked.push(method + ' ' + url);
      return Promise.resolve(new Response(JSON.stringify({error: 'blocked by ui-drive'}),
        {status: 400, headers: {'content-type': 'application/json'}}));
    }
    return f.apply(this, arguments);
  };
})();"""

PANIC_MARKS = ("panicked at", "RuntimeError: unreachable", "already been disposed")


def podman_names(prefix: str):
    if not prefix or not shutil.which("podman"):
        return None
    out = subprocess.run(["podman", "ps", "--format", "{{.Names}}"],
                         capture_output=True, text=True, check=False).stdout
    return sorted(n for n in out.split() if prefix in n)


async def run(args, steps, token) -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        port = s.getsockname()[1]
    # /tmp is a tmpfs: a Chrome profile left behind is RAM (see ui-matrix.py).
    profile = tempfile.mkdtemp(prefix="lmgw-shot-")
    tmp_before = chrome_profile.snapshot()
    media = ["--mute-audio", "--autoplay-policy=no-user-gesture-required"] if args.mute_audio else []
    if args.fake_mic:
        # A fake microphone playing the file once, granted without a prompt,
        # and the browser muted: no real device, nothing audible.
        media += ["--use-fake-ui-for-media-stream", "--use-fake-device-for-media-stream",
                  f"--use-file-for-fake-audio-capture={Path(args.fake_mic).resolve()}%noloop",
                  "--mute-audio"]
    chrome = subprocess.Popen(
        ["google-chrome", "--headless", "--disable-gpu", f"--remote-debugging-port={port}",
         "--window-size=1440,900", f"--user-data-dir={profile}", *media, "about:blank"],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    failures = 0
    try:
        pages = []
        for _ in range(50):
            try:
                with urllib.request.urlopen(f"http://127.0.0.1:{port}/json/list") as r:
                    pages = [t for t in json.load(r) if t.get("type") == "page"]
                if pages:
                    break
            except OSError:
                pass
            await asyncio.sleep(0.2)
        async with websockets.connect(pages[0]["webSocketDebuggerUrl"],
                                      max_size=256 * 1024 * 1024) as ws:
            mid = 0
            pending = set()
            panics = []
            snaps = {}

            def on_event(msg):
                m, p = msg.get("method"), msg.get("params", {})
                if m == "Network.requestWillBeSent" and p.get("type") != "EventSource":
                    pending.add(p["requestId"])
                elif m in ("Network.loadingFinished", "Network.loadingFailed"):
                    pending.discard(p["requestId"])
                elif m == "Runtime.consoleAPICalled" and p.get("type") == "error":
                    text = " ".join(str(a.get("value", a.get("description", "")))
                                    for a in p.get("args", []))
                    if any(k in text for k in PANIC_MARKS):
                        panics.append(text)
                elif m == "Runtime.exceptionThrown":
                    ed = p.get("exceptionDetails", {})
                    text = ed.get("exception", {}).get("description") or ed.get("text", "")
                    if any(k in text for k in PANIC_MARKS):
                        panics.append(text)

            async def call(method, **params):
                nonlocal mid
                mid += 1
                await ws.send(json.dumps({"id": mid, "method": method, "params": params}))
                while True:
                    reply = json.loads(await ws.recv())
                    if reply.get("id") == mid:
                        return reply.get("result", {})
                    on_event(reply)

            async def ev(expr):
                r = await call("Runtime.evaluate", expression=expr, returnByValue=True,
                               awaitPromise=True)
                if "exceptionDetails" in r:
                    return {"error": r["exceptionDetails"].get("exception", {})
                            .get("description", "")[:300]}
                return r.get("result", {}).get("value")

            async def settle(least, cap=15.0):
                await asyncio.sleep(least)
                last, calm, waited = None, 0, least
                while waited < cap and calm < 3:
                    sig = await ev("[document.querySelectorAll('*').length,"
                                   " document.body.innerText.length,"
                                   " !document.querySelector('main.content')"
                                   " || document.body.innerText.includes('Loading')]")
                    busy = not isinstance(sig, list) or sig[2] or bool(pending)
                    calm = calm + 1 if sig == last and not busy else 0
                    last = sig
                    await asyncio.sleep(0.3)
                    waited += 0.3

            async def resize(size):
                w, h = map(int, size.split("x"))
                await call("Emulation.setDeviceMetricsOverride", width=w, height=h,
                           deviceScaleFactor=args.dsf, mobile=False)

            async def mouse_click(x, y, clicks=1):
                await call("Input.dispatchMouseEvent", type="mouseMoved", x=x, y=y)
                for n in range(1, clicks + 1):
                    for t in ("mousePressed", "mouseReleased"):
                        await call("Input.dispatchMouseEvent", type=t, x=x, y=y,
                                   button="left", clickCount=n)
                    if n < clicks:
                        await asyncio.sleep(0.09)

            async def centre(sel, nth):
                return await ev(f"""(() => {{
                    const el = document.querySelectorAll({json.dumps(sel)})[{nth}];
                    if (!el) return null;
                    el.scrollIntoView({{block: 'center', inline: 'nearest'}});
                    const r = el.getBoundingClientRect();
                    return [r.left + r.width / 2, r.top + r.height / 2]; }})()""")

            async def counters():
                await call("HeapProfiler.collectGarbage")
                c = await ev("window.__counters ? window.__counters() : {}") or {}
                dom = await call("Memory.getDOMCounters")
                # What the renderer holds minus what is in the document: the
                # nodes something keeps alive after they were taken out.
                attached = await ev("""(() => { let n = 0;
                    const w = document.createTreeWalker(document, NodeFilter.SHOW_ALL);
                    while (w.nextNode()) n++; return n + 1; })()""")
                c["dom_nodes"] = dom.get("nodes")
                c["detached_nodes"] = (dom.get("nodes") or 0) - (attached or 0)
                return c

            async def key(k):
                code, vk = KEYS.get(k, (f"Key{k.upper()}" if len(k) == 1 else k,
                                        ord(k.upper()) if len(k) == 1 else 0))
                # With Ctrl or Alt held ({"keydown": "ControlRight"}) a key is a
                # shortcut (Ctrl+C copies), not text.
                mods = held["mods"]
                text = k if len(k) == 1 else ("\r" if k == "Enter" else "")
                if mods & 3:
                    text = ""
                down = {"type": "keyDown" if text else "rawKeyDown", "key": k, "code": code,
                        "windowsVirtualKeyCode": vk, "modifiers": mods}
                if text:
                    down["text"] = text
                await call("Input.dispatchKeyEvent", **down)
                await call("Input.dispatchKeyEvent", type="keyUp", key=k, code=code,
                           windowsVirtualKeyCode=vk, modifiers=mods)

            held = {"mods": 0}

            async def key_hold(code, down):
                k, vk, loc, bit = HELD.get(code, (code, 0, 0, 0))
                if down:
                    held["mods"] |= bit
                else:
                    held["mods"] &= ~bit
                await call("Input.dispatchKeyEvent", type="rawKeyDown" if down else "keyUp",
                           key=k, code=code, windowsVirtualKeyCode=vk, location=loc,
                           modifiers=held["mods"])

            await resize(args.size)
            await call("Page.enable")
            await call("Network.enable")
            await call("Runtime.enable")
            await call("Page.addScriptToEvaluateOnNewDocument", source=PROBE)
            if args.block_writes:
                await call("Page.addScriptToEvaluateOnNewDocument", source=BLOCK_WRITES)
            await call("Page.navigate", url=f"{args.base}/api/session/login?token={token}")
            await settle(1.5)

            async def run_steps(steps, prefix=""):
                nonlocal failures
                for i, st in enumerate(steps):
                    await run_step(f"[{prefix}{i:02}]", st)
                    await asyncio.sleep(st.get("after", 0.15))

            async def run_step(tag, st):
                nonlocal failures
                if "size" in st:
                    await resize(st["size"])
                    await asyncio.sleep(0.5)
                elif "nav" in st:
                    pending.clear()
                    await call("Page.navigate", url=f"{args.base}{st['nav']}")
                    await settle(1.0)
                elif "go" in st:
                    to = json.dumps(st["go"])
                    if st.get("via") == "history":
                        await ev(f"history.pushState(null, '', {to});"
                                 " dispatchEvent(new PopStateEvent('popstate'))")
                    else:
                        # The first link to that exact href that is on screen:
                        # the sidebar's, or one in the page.
                        pt = await ev(f"""(() => {{
                            const el = [...document.querySelectorAll('a[href]')].find(a =>
                                a.getAttribute('href') === {to} && a.checkVisibility());
                            if (!el) return null;
                            el.scrollIntoView({{block: 'nearest', inline: 'nearest'}});
                            const r = el.getBoundingClientRect();
                            return [r.left + r.width / 2, r.top + r.height / 2]; }})()""")
                        if not isinstance(pt, list):
                            print(f"{tag} go {st['go']}: no visible link to it")
                            failures += 1
                            return
                        await mouse_click(*pt)
                    await settle(st.get("least", 0.6))
                elif "click" in st or "dblclick" in st:
                    sel = st.get("click") or st["dblclick"]
                    pt = await centre(sel, st.get("nth", 0))
                    if not isinstance(pt, list):
                        print(f"{tag} click {sel}: no such element")
                        failures += 1
                        return
                    await mouse_click(*pt, clicks=2 if "dblclick" in st else 1)
                elif "repeat" in st:
                    for n in range(st["repeat"]):
                        await run_steps(st["steps"], f"{tag[1:-1]}.{n}.")
                elif "snap" in st:
                    snaps[st["snap"]] = await counters()
                    print(f"{tag} snap {st['snap']}: {json.dumps(snaps[st['snap']])}")
                elif "same" in st:
                    a, b = (snaps.get(k, {}) for k in st["same"])
                    nodes = ("dom_nodes", "detached_nodes")
                    grew = {k: (a.get(k, 0), b.get(k, 0)) for k in set(a) | set(b)
                            if k not in nodes and a.get(k, 0) != b.get(k, 0)}
                    dn = (b.get("detached_nodes") or 0) - (a.get("detached_nodes") or 0)
                    ok = not grew and dn <= st.get("nodes", 0)
                    failures += 0 if ok else 1
                    print(f"{tag} {'PASS' if ok else 'FAIL'} {st.get('label', 'same counters')}"
                          f" {st['same'][0]} → {st['same'][1]}: detached nodes {dn:+d}"
                          + (f", changed {json.dumps(grew)}" if grew else ""))
                elif "click_at" in st:
                    await mouse_click(*st["click_at"])
                elif "keydown" in st or "keyup" in st:
                    await key_hold(st.get("keydown") or st["keyup"], "keydown" in st)
                elif "mousedown" in st or "mouseup" in st:
                    sel = st.get("mousedown") or st["mouseup"]
                    pt = await centre(sel, st.get("nth", 0))
                    if not isinstance(pt, list):
                        print(f"{tag} {'mousedown' if 'mousedown' in st else 'mouseup'} {sel}:"
                              " no such element")
                        failures += 1
                        return
                    if "mousedown" in st:
                        await call("Input.dispatchMouseEvent", type="mouseMoved", x=pt[0], y=pt[1])
                    await call("Input.dispatchMouseEvent",
                               type="mousePressed" if "mousedown" in st else "mouseReleased",
                               x=pt[0], y=pt[1], button="left", clickCount=1)
                elif "key" in st:
                    for _ in range(st.get("n", 1)):
                        await key(st["key"])
                elif "type" in st:
                    # Letters, digits and space as key presses (type-ahead
                    # listens to keydown); punctuation has no stable key code
                    # across layouts, so it is inserted as text.
                    for ch in st["type"]:
                        if ch.isalnum() or ch in KEYS:
                            await key(ch)
                        else:
                            await call("Input.insertText", text=ch)
                elif "js" in st:
                    print(f"{tag} {json.dumps(await ev(st['js']))[:2000]}")
                elif "script" in st:
                    code = Path(st["script"]).read_text()
                    print(f"{tag} script {st['script']}: {json.dumps(await ev(code))[:300]}")
                elif "expect" in st:
                    # JS truthiness, not Python's: an element comes back as {}.
                    # A promise (an async IIFE, a fetch chain) is awaited, with a
                    # timeout: a bare Promise is truthy, so before this every
                    # async expectation passed whatever it resolved to.
                    ms = int(st.get("timeout", 30) * 1000)
                    r = await ev(f"""(async () => {{
                        const v = await Promise.race([
                          (async () => ({st['expect']}))(),
                          new Promise((_, rej) => setTimeout(
                            () => rej(new Error("expect timed out after {ms} ms")), {ms}))]);
                        return {{ok: !!v, v: v && v.nodeType ? v.outerHTML.slice(0, 200) : v}};
                        }})()""")
                    ok = isinstance(r, dict) and r.get("ok") is True
                    failures += 0 if ok else 1
                    print(f"{tag} {'PASS' if ok else 'FAIL'} {st.get('label', st['expect'])}"
                          + ("" if ok else f"  -> {json.dumps(r)[:300]}"))
                elif "until" in st:
                    deadline = time.monotonic() + st.get("timeout", 30)
                    ok = False
                    while True:
                        r = await ev(f"Promise.resolve({st['until']}).then(v => !!v)")
                        if r is True:
                            ok = True
                            break
                        if time.monotonic() >= deadline:
                            break
                        await asyncio.sleep(0.25)
                    failures += 0 if ok else 1
                    print(f"{tag} {'PASS' if ok else 'FAIL'} {st.get('label', st['until'])}")
                elif "wait" in st:
                    await asyncio.sleep(st["wait"])
                elif "shot" in st and args.shots:
                    shot = await call("Page.captureScreenshot", format="png")
                    path = Path(args.shots) / f"{st['shot']}.png"
                    path.write_bytes(base64.b64decode(shot["data"]))
                    print(f"{tag} shot {path}")

            await run_steps(steps)
            # Events that arrived after the last step still count.
            await ev("1")
            if panics:
                failures += 1
                # One line per place: the "At <file:line>" a disposed read
                # names, else the message's first line.
                places = {}
                for p in panics:
                    lines = [x.strip() for x in p.splitlines() if x.strip()]
                    at = next((x for x in lines if x.startswith("At ")), lines[0] if lines else p)
                    places[at[:220]] = places.get(at[:220], 0) + 1
                print(f"FAIL {len(panics)} wasm panic(s) in the console:")
                for at, n in places.items():
                    print(f"    {n}x {at}")
    finally:
        chrome.terminate()
        chrome.wait()
        chrome_profile.remove(profile, tmp_before)
    return failures


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0],
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("steps", help="JSON file with the step list ('-' reads stdin)")
    ap.add_argument("--base", default="http://127.0.0.1:8899", help="dashboard origin")
    ap.add_argument("--token", help="owner token (default: read --token-file)")
    ap.add_argument("--token-file", default="/tmp/lmgw-ux-token")
    ap.add_argument("--size", default="1440x900", help="starting window size")
    ap.add_argument("--dsf", type=float, default=1.0, metavar="SCALE",
                    help="device pixel ratio; shots are SIZE x SCALE pixels "
                         "(1536x864 at 1.25 is a 1080p screen at 125%%)")
    ap.add_argument("--shots", metavar="DIR", help="where {\"shot\": name} steps write")
    ap.add_argument("--block-writes", action="store_true",
                    help="answer every non-GET fetch with a local 400 (window.__blocked)")
    ap.add_argument("--fake-mic", metavar="WAV",
                    help="a fake microphone playing WAV once; the browser muted")
    ap.add_argument("--mute-audio", action="store_true",
                    help="mute the browser and let audio start without a gesture (a script's click "
                         "may then start playback); with --fake-mic too")
    ap.add_argument("--podman-guard", default="lmgw-dev", metavar="PREFIX",
                    help="container names to compare before/after ('' disables)")
    args = ap.parse_args()

    token = args.token or Path(args.token_file).read_text().strip()
    raw = sys.stdin.read() if args.steps == "-" else Path(args.steps).read_text()
    steps = json.loads(raw)
    if args.shots:
        Path(args.shots).mkdir(parents=True, exist_ok=True)

    before = podman_names(args.podman_guard)
    failures = asyncio.run(asyncio.wait_for(run(args, steps, token), timeout=1800))
    after = podman_names(args.podman_guard)
    print(f"\n{failures} FAIL")
    if before != after:
        print(f"CONTAINERS CHANGED while driving: before {before}, after {after}",
              file=sys.stderr)
        return 3
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
