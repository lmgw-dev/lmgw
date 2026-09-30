#!/usr/bin/env python3
# Usage: scripts/ui-matrix.py [--routes all|/a,/b] [--sizes all|1440x900,...] [--shots DIR] [--json OUT]
"""Layout matrix for the dashboard: every route at every window size, measured.

The dashboard is used in a window that is resized all day, so "does this page
still fit" is a question per route *and* per size. This drives headless Chrome
over the dev instance (logged in with the owner token), resizes it through the
seven window sizes the layout is designed against, and measures what a
screenshot only hints at:

  metric             FAIL when
  content_scroll     main.content scrolls (or clips) on either axis
  nested_v           a vertical scroller sits inside another one
                     (textarea, dialog and [popover] content excluded)
  page_hscroll       .page-body / .fill-pane scroll sideways (.content when
                     the route has neither)
  gutter             the .page is more than 2px narrower than its pane
  wrapped_btns       a .btn whose label wraps or overflows its box
  primary_offscreen  the page's primary action is not fully on screen

Reported but never failing: screens (the main scroller's height in
viewports), clipped_inputs (text inputs whose value is wider than the box),
rail (sidebar collapsed) and nav_scroll (the sidebar itself scrolls).

A route loads once at the first size and is then *resized* through the rest,
the way a window is dragged; --reload navigates afresh per size instead.
Browsing must never start a container: when podman is present, the names
matching --podman-guard are compared before and after the run.

Exit status: 0 clean, 1 on any FAIL, 3 when a container appeared or vanished.
"""
import argparse
import asyncio
import base64
import json
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import urllib.request
from pathlib import Path

import websockets

SIZES = ["1024x700", "1280x800", "1440x900", "1920x1080", "2560x1400", "1440x600", "900x1200"]
# `{agent}`, `{agent_tab}`, `{local}` and `{kb}` are filled from the instance's own data.
ROUTES = [
    "/", "/chat", "/audio-lab", "/image-lab", "/agents", "/agents/{agent}", "/agents/{agent_tab}",
    "/models", "/models/catalog", "/models/local/{local}", "/downloads", "/backends",
    "/backends/images", "/benchmarks", "/docs", "/docs/requests",
    "/docs/playground", "/docs/eval", "/knowledge", "/knowledge/{kb}",
    "/knowledge/{kb}/search", "/knowledge/{kb}/settings", "/upstreams", "/mcp-servers", "/traffic",
    "/traffic/conversations", "/usage", "/usage/keys", "/usage/prices", "/wiring", "/settings",
    "/settings/runtimes", "/api-reference", "/api-reference?op=post_v1_chat_completions",
    "/no-such-page",
]
FAILING = ["content_scroll", "nested_v", "page_hscroll", "gutter", "wrapped_btns",
           "primary_offscreen"]

MEASURE = r"""(() => {
  const vw = innerWidth, vh = innerHeight;
  const name = el => { let s = el.tagName.toLowerCase();
    if (el.id) s += '#' + el.id;
    if (typeof el.className === 'string' && el.className.trim())
      s += '.' + el.className.trim().split(/\s+/).slice(0, 3).join('.');
    return s; };
  // Laid out is not shown: a closed <details> keeps its body's boxes.
  const shown = el => { const r = el.getBoundingClientRect();
    return r.width > 0 && r.height > 0 && (!el.checkVisibility || el.checkVisibility()); };
  const vScroller = el => { const s = getComputedStyle(el);
    return (s.overflowY === 'auto' || s.overflowY === 'scroll') && el.scrollHeight > el.clientHeight + 2; };
  const out = { offenders: {} };
  const content = document.querySelector('main.content');
  if (!content) return { error: 'no main.content' };

  out.content_scroll = Math.max(0, content.scrollHeight - content.clientHeight,
                                content.scrollWidth - content.clientWidth);
  if (out.content_scroll > 1) out.offenders.content_scroll = [name(content)];

  const nested = [];
  for (const el of document.querySelectorAll('body *')) {
    if (el.tagName === 'TEXTAREA' || el.closest('dialog, [popover]') || !vScroller(el) || !shown(el))
      continue;
    for (let p = el.parentElement; p; p = p.parentElement)
      if (vScroller(p)) { nested.push(name(el) + ' in ' + name(p)); break; }
  }
  out.nested_v = nested.length; out.offenders.nested_v = nested.slice(0, 4);

  let panes = [...content.querySelectorAll('.page-body, .fill-pane')].filter(shown);
  if (!panes.length) panes = [content];
  out.page_hscroll = Math.max(0, ...panes.map(p => p.scrollWidth - p.clientWidth));
  out.offenders.page_hscroll = panes.filter(p => p.scrollWidth > p.clientWidth + 1)
    .map(p => name(p) + ' +' + (p.scrollWidth - p.clientWidth));

  const page = content.querySelector('.page');
  if (page) {
    const cs = getComputedStyle(content);
    const inner = content.clientWidth - parseFloat(cs.paddingLeft) - parseFloat(cs.paddingRight);
    out.gutter = Math.round(inner - page.getBoundingClientRect().width);
  } else out.gutter = null;

  // A label that wraps shows up as text boxes on more than one line; a label
  // that is clipped by the button's own height shows up as overflow.
  const lines = el => { const r = document.createRange(); r.selectNodeContents(el);
    const tops = [];
    for (const x of r.getClientRects()) if (x.width > 0 && x.height > 0 &&
        !tops.some(t => Math.abs(t - x.top) < 6)) tops.push(x.top);
    return tops.length; };
  const wrapped = [...content.querySelectorAll('.btn')].filter(shown)
    .filter(b => lines(b) > 1 || b.scrollHeight > b.clientHeight + 2 || b.scrollWidth > b.clientWidth + 1);
  out.wrapped_btns = wrapped.length;
  out.offenders.wrapped_btns = wrapped.slice(0, 6).map(b => b.textContent.trim().slice(0, 30));

  const prim = [...document.querySelectorAll(
      '.page-foot .btn.primary, .page-head .btn.primary, .action-bar .btn.primary, .composer .btn')]
    .filter(shown).filter(b => { const r = b.getBoundingClientRect();
      return r.left < -0.5 || r.top < -0.5 || r.right > vw + 0.5 || r.bottom > vh + 0.5; });
  out.primary_offscreen = prim.length;
  out.offenders.primary_offscreen = prim.map(b => b.textContent.trim().slice(0, 30));

  let main = null;
  for (const sel of ['.page-body', '.fill-pane', '.chat-scroll', '.lab-scroll', 'main.content']) {
    const el = [...content.parentElement.querySelectorAll(sel)].find(e => shown(e) &&
      ['auto', 'scroll'].includes(getComputedStyle(e).overflowY));
    if (el) { main = el; break; }
  }
  out.scroller = main ? name(main) : null;
  out.body_px = main ? main.scrollHeight : null;
  out.screens = main ? +(main.scrollHeight / main.clientHeight).toFixed(2) : null;

  const inputs = [...content.querySelectorAll('input.input')].filter(shown);
  const clipped = inputs.filter(i => i.scrollWidth > i.clientWidth + 1);
  out.clipped_inputs = clipped.length + '/' + inputs.length;

  const app = document.querySelector('.app'), nav = document.querySelector('.sidebar');
  out.rail = !!(app && app.classList.contains('rail'));
  out.sidebar_w = nav ? Math.round(nav.getBoundingClientRect().width) : null;
  out.nav_scroll = nav ? Math.max(0, nav.scrollHeight - nav.clientHeight) : null;
  return out;
})()"""


def fails(m: dict) -> list:
    bad = []
    if m.get("content_scroll", 0) > 1:
        bad.append("content_scroll")
    if m.get("nested_v", 0) > 0:
        bad.append("nested_v")
    if m.get("page_hscroll", 0) > 1:
        bad.append("page_hscroll")
    if m.get("gutter") is not None and m["gutter"] > 2:
        bad.append("gutter")
    if m.get("wrapped_btns", 0) > 0:
        bad.append("wrapped_btns")
    if m.get("primary_offscreen", 0) > 0:
        bad.append("primary_offscreen")
    return bad


def podman_names(prefix: str):
    if not prefix or not shutil.which("podman"):
        return None
    out = subprocess.run(["podman", "ps", "--format", "{{.Names}}"],
                         capture_output=True, text=True, check=False).stdout
    return sorted(n for n in out.split() if prefix in n)


def api_get(base: str, token: str, path: str):
    req = urllib.request.Request(base + path, headers={"Authorization": f"Bearer {token}"})
    with urllib.request.urlopen(req, timeout=10) as r:
        return json.load(r)


def expand_routes(spec: str, base: str, token: str) -> list:
    """`{agent}` becomes every installed agent (chat, app and script agents
    lay out differently), `{agent_tab}` each agent's other tabs (`<id>/runs`,
    `<id>/definition`, and `<id>/ui` for one that serves an app); `{local}`
    the first local chat model, `{kb}` the first knowledge base."""
    routes = ROUTES if spec == "all" else [r.strip() for r in spec.split(",") if r.strip()]
    fill = {}
    if any("{agent}" in r or "{agent_tab}" in r for r in routes):
        agents = api_get(base, token, "/api/agents")
        fill["agent"] = [a["id"] for a in agents]
        fill["agent_tab"] = [f"{a['id']}/{t}" for a in agents
                             for t in ("runs", "definition") + (("ui",) if a.get("app") else ())]
    if any("{local}" in r for r in routes):
        local = api_get(base, token, "/api/models/full").get("local", [])
        fill["local"] = [str(local[0]["model"]["id"])] if local else []
    if any("{kb}" in r for r in routes):
        bases = api_get(base, token, "/api/knowledge/bases").get("bases", [])
        fill["kb"] = [str(bases[0]["id"])] if bases else []
    out = []
    for r in routes:
        key = next((k for k in fill if "{" + k + "}" in r), None)
        if key is None:
            out.append(r)
        elif not fill[key]:
            print(f"skip {r}: the instance has no {key}", file=sys.stderr)
        else:
            out.extend(r.replace("{" + key + "}", v) for v in fill[key])
    return out


def slug(route: str) -> str:
    s = route.strip("/").replace("/", "_")
    return s or "overview"


async def run(args, routes, sizes, token) -> list:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        port = s.getsockname()[1]
    # /tmp is a tmpfs on Fedora: a Chrome profile left behind is ~200 MB of RAM,
    # and a few dozen runs of them got the desktop OOM-killed on 2026-09-24.
    profile = tempfile.mkdtemp(prefix="lmgw-matrix-")
    chrome = subprocess.Popen(
        ["google-chrome", "--headless", "--disable-gpu", f"--remote-debugging-port={port}",
         "--window-size=1440,900", f"--user-data-dir={profile}",
         "about:blank"],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    results = []
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
            # Requests still in flight. A section that renders nothing until
            # its fetch lands (an upstream's live catalog) looks settled to
            # the DOM, so "loaded" also means the network went quiet. The SSE
            # bus never finishes and is not counted.
            pending = set()

            def on_event(msg):
                m, p = msg.get("method"), msg.get("params", {})
                if m == "Network.requestWillBeSent" and p.get("type") != "EventSource":
                    pending.add(p["requestId"])
                elif m in ("Network.loadingFinished", "Network.loadingFailed"):
                    pending.discard(p["requestId"])

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

            # Loaded means mounted and quiet: the app shell is there, the DOM
            # stops changing and no "Loading…" is left — bounded by --wait,
            # since a live page may never go fully quiet.
            async def settle(least):
                await asyncio.sleep(least)
                last, calm, waited = None, 0, least
                while waited < args.wait and calm < 3:
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
                           deviceScaleFactor=1, mobile=False)

            await call("Page.enable")
            await call("Network.enable")
            await call("Page.navigate", url=f"{args.base}/api/session/login?token={token}")
            await settle(1.5)
            for route in routes:
                for i, size in enumerate(sizes):
                    await resize(size)
                    if i == 0 or args.reload:
                        pending.clear()
                        await call("Page.navigate", url=f"{args.base}{route}")
                        await settle(1.0)
                        if args.js:
                            await ev(args.js)
                            await asyncio.sleep(args.settle)
                    else:
                        await asyncio.sleep(args.settle)
                    m = await ev(MEASURE) or {"error": "no result"}
                    m.update(route=route, size=size, fails=fails(m))
                    results.append(m)
                    report(m, args.verbose)
                    if args.shots:
                        shot = await call("Page.captureScreenshot", format="png")
                        path = Path(args.shots) / f"{slug(route)}-{size}.png"
                        path.write_bytes(base64.b64decode(shot["data"]))
    finally:
        chrome.terminate()
        chrome.wait()
        # Chrome's network service child flushes into the profile after the
        # browser process exits, so one rmtree can race it.
        for _ in range(5):
            shutil.rmtree(profile, ignore_errors=True)
            if not os.path.exists(profile):
                break
            time.sleep(0.2)
    return results


def report(m: dict, verbose: bool) -> None:
    if "error" in m:
        print(f"{m['route']:<24} {m['size']:<9} ERROR {m['error']}")
        return
    status = "FAIL " + ",".join(m["fails"]) if m["fails"] else "ok"
    print(f"{m['route']:<24} {m['size']:<9} cs={m['content_scroll']:<5} nv={m['nested_v']} "
          f"hs={m['page_hscroll']:<4} gut={m['gutter']!s:<5} wb={m['wrapped_btns']:<2} "
          f"po={m['primary_offscreen']} | screens={m['screens']} ({m['body_px']}px "
          f"{m['scroller']}) inputs={m['clipped_inputs']} rail={int(m['rail'])} "
          f"nav={m['nav_scroll']}  {status}")
    if verbose or m["fails"]:
        for k in m["fails"]:
            if m["offenders"].get(k):
                print(f"    {k}: {m['offenders'][k]}")


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0],
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--base", default="http://127.0.0.1:8899", help="dashboard origin")
    ap.add_argument("--token", help="owner token (default: read --token-file)")
    ap.add_argument("--token-file", default="/tmp/lmgw-ux-token")
    ap.add_argument("--routes", default="all", help="'all' or comma-separated paths")
    ap.add_argument("--sizes", default="all", help="'all' or comma-separated WxH")
    ap.add_argument("--shots", metavar="DIR", help="write a viewport PNG per route and size")
    ap.add_argument("--json", metavar="OUT", help="write every measurement as JSON")
    ap.add_argument("--js", help="JavaScript evaluated after each navigation")
    ap.add_argument("--wait", type=float, default=15.0,
                    help="most seconds to wait for a route to go quiet after navigating")
    ap.add_argument("--settle", type=float, default=0.8, help="seconds after a resize")
    ap.add_argument("--reload", action="store_true", help="navigate afresh for every size")
    ap.add_argument("--podman-guard", default="lmgw-dev", metavar="PREFIX",
                    help="container names to compare before/after ('' disables)")
    ap.add_argument("-v", "--verbose", action="store_true", help="list offenders for every row")
    args = ap.parse_args()

    token = args.token or Path(args.token_file).read_text().strip()
    sizes = SIZES if args.sizes == "all" else [s.strip() for s in args.sizes.split(",")]
    routes = expand_routes(args.routes, args.base, token)
    if args.shots:
        Path(args.shots).mkdir(parents=True, exist_ok=True)

    before = podman_names(args.podman_guard)
    results = asyncio.run(asyncio.wait_for(run(args, routes, sizes, token), timeout=3600))
    after = podman_names(args.podman_guard)

    if args.json:
        Path(args.json).write_text(json.dumps(results, indent=1))
    failed = [r for r in results if r.get("fails") or "error" in r]
    print(f"\n{len(results)} measurements, {len(failed)} FAIL"
          + (f" (shots in {args.shots})" if args.shots else ""))
    if before != after:
        print(f"CONTAINERS CHANGED while browsing: before {before}, after {after}",
              file=sys.stderr)
        return 3
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
