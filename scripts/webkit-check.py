#!/usr/bin/env python3
# Usage: scripts/webkit-check.py [--base URL] [--routes /a,/b,...] [--sizes WxH,...]
#                                 [--shots DIR] [--token-file PATH] [--json OUT]
#                                 [--backend auto|broadway|wayland]
"""WebKitGTK compatibility check for lmgw's dashboard.

Every other UI script here (ui-matrix.py, ui-drive.py, screenshot.py) drives
headless Chrome over the Chrome DevTools Protocol. That is a different
rendering engine than the one the Tauri window actually uses (WebKitGTK
6.0/2.52 on this machine). This script loads the same routes in a real
WebKitWebView and runs the same kind of DOM probes, to catch engine-specific
breakage in the things the UX overhaul leans on: top-layer `popover`,
`<dialog>.showModal()`, `:has()`, container queries (`@container pane`),
`position: sticky` and ResizeObserver.

It never pops a window onto the desktop if it can help it: it tries the GTK
Broadway backend first (a virtual display served by `gtk4-broadwayd`, so the
WebView is realized and rendered without a Wayland surface ever appearing),
and only falls back to a real — brief — Wayland window if WebKit cannot
render there. The render path is picked the way the app picks it
(src-tauri/src/main.rs, render_workaround): on NVIDIA `__NV_DISABLE_EXPLICIT_SYNC=1`,
unless one of the WebKit render variables is already set. Each backend attempt
runs in a fresh subprocess: GDK picks its backend once per process and cannot
be steered mid-run.

Per (route, size) it evaluates one JS probe that checks CSS.supports() and
HTMLElement/HTMLDialogElement/ResizeObserver feature detection, main.content's
computed overflow and whether it actually scrolls, whether a known
`@container pane` rule is applying (the /usage grid's column span, and the
sidebar's `.app.rail` breakpoint), and whether a `table.data` header is really
sticky (scrolls its ancestor and checks the header's rect barely moved). Then
it saves a viewport PNG (`webkit_web_view_get_snapshot`).

On /chat it opens an existing thread (never "New chat" — that POSTs a real
thread) to reveal the model picker, clicks it, and checks the resulting
`[popover]` is `:popover-open` and inside the viewport. On /mcp-servers it
clicks a server's name (the row's only "edit" control) to open its `<dialog>`,
checks it is open and its Save button's rect is inside the viewport — also at
1440x600, a short window the standard size list does not cover — then closes
it with the dialog's own Close (✕) button. Neither interaction saves, deletes,
tests, starts, stops or toggles anything.

Exit status: 0 the run completed (see the printed report for probe PASS/FAIL),
1 if no backend could render WebKit content at all.
"""
import argparse
import json
import os
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

DEFAULT_BASE = "http://127.0.0.1:8899"
DEFAULT_ROUTES = ["/chat", "/mcp-servers", "/settings", "/models", "/usage", "/api-reference"]
DEFAULT_SIZES = ["1440x900", "1024x700"]

# ---------------------------------------------------------------------------
# Orchestrator: picks a backend, launches the worker subprocess in it, cleans
# up. Nothing here imports gi — the backend must be chosen (via env vars)
# before GDK's default display is ever opened, and that only happens once per
# process, so each attempt is its own subprocess.
# ---------------------------------------------------------------------------


def slug(route: str) -> str:
    return route.strip("/").replace("/", "_") or "overview"


def broadway_sockets(n: int) -> list:
    # gtk4-broadwayd :N listens on a unix socket named for N+1 (observed:
    # `:5` -> broadway6.socket). Treat either name as the display's, to be
    # safe across versions.
    uid = os.getuid()
    return [Path(f"/run/user/{uid}/broadway{n + 1}.socket"),
            Path(f"/run/user/{uid}/broadway{n}.socket")]


def socket_in_use(path: Path) -> bool:
    """A socket file nobody answers on is what a stopped broadwayd leaves
    behind; only a live listener makes the display taken."""
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    try:
        s.connect(str(path))
        return True
    except (ConnectionRefusedError, FileNotFoundError):
        return False
    except OSError:
        return True
    finally:
        s.close()


def free_broadway_display(start=5, tries=20) -> int | None:
    for n in range(start, start + tries):
        paths = [p for p in broadway_sockets(n) if p.exists()]
        if any(socket_in_use(p) for p in paths):
            continue
        # Stale leftovers of an earlier run: without this every run used up
        # a display for good, and after twenty the check fell back to a
        # visible window on the desktop.
        for p in paths:
            p.unlink(missing_ok=True)
        return n
    return None


def stop_broadwayd(proc, display):
    stop_process(proc)
    if display is not None:
        for p in broadway_sockets(display):
            if p.exists() and not socket_in_use(p):
                p.unlink(missing_ok=True)


def start_broadwayd(display: int, log_path: Path):
    log = open(log_path, "wb")
    proc = subprocess.Popen(
        ["gtk4-broadwayd", f":{display}"],
        stdout=log,
        stderr=subprocess.STDOUT,
        start_new_session=True,
    )
    # It either binds its socket in well under a second or it is not going to.
    for _ in range(20):
        if proc.poll() is not None:
            log.close()
            return None
        if "Listening on" in log_path.read_text(errors="ignore"):
            return proc
        time.sleep(0.1)
    return proc  # no confirmation line seen, but still alive: let it try


def stop_process(proc):
    if proc is None or proc.poll() is not None:
        return
    try:
        os.killpg(proc.pid, signal.SIGTERM)
    except (ProcessLookupError, PermissionError):
        proc.terminate()
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        try:
            os.killpg(proc.pid, signal.SIGKILL)
        except (ProcessLookupError, PermissionError):
            proc.kill()
        proc.wait(timeout=5)


# Same list and rule as src-tauri/src/main.rs (render_workaround): an explicit
# value of any of these wins; otherwise NVIDIA gets __NV_DISABLE_EXPLICIT_SYNC=1,
# which avoids the Wayland "Error 71" crash and keeps the zero-copy hardware
# renderer (WEBKIT_DISABLE_DMABUF_RENDERER=1 means software compositing).
RENDER_VARS = ("WEBKIT_DISABLE_DMABUF_RENDERER", "WEBKIT_DMABUF_RENDERER_FORCE_SHM",
               "__NV_DISABLE_EXPLICIT_SYNC")


def with_render_workaround(env: dict) -> dict:
    nvidia = Path("/dev/nvidiactl").exists() or Path("/sys/module/nvidia").exists()
    if nvidia and not any(v in env for v in RENDER_VARS):
        env["__NV_DISABLE_EXPLICIT_SYNC"] = "1"
    return env


def run_worker(env: dict, spec_path: Path, result_path: Path, timeout: float):
    if result_path.exists():
        result_path.unlink()
    proc = subprocess.Popen(
        [sys.executable, __file__, "--_worker", "--spec", str(spec_path),
         "--result", str(result_path)],
        env=env,
    )
    try:
        proc.wait(timeout=timeout)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait()
        return None, "worker timed out"
    if not result_path.exists():
        return None, f"worker exited {proc.returncode} with no result"
    try:
        return json.loads(result_path.read_text()), None
    except json.JSONDecodeError as e:
        return None, f"worker wrote unparseable result: {e}"


def orchestrate(args) -> int:
    workdir = Path(tempfile.mkdtemp(prefix="lmgw-webkit-check-"))
    shots = Path(args.shots)
    shots.mkdir(parents=True, exist_ok=True)
    token = args.token or Path(args.token_file).read_text().strip()
    sizes = [tuple(int(v) for v in s.split("x")) for s in args.sizes]
    routes = args.routes
    spec = {
        "base": args.base,
        "token": token,
        "routes": routes,
        "sizes": sizes,
        "shots": str(shots),
        "settle_max_ms": args.wait * 1000,
    }
    spec_path = workdir / "spec.json"
    result_path = workdir / "result.json"
    spec_path.write_text(json.dumps(spec))

    broadwayd = None
    display = None
    backend_used = None
    result = None
    try:
        attempts = []
        if args.backend in ("auto", "broadway") and shutil.which("gtk4-broadwayd"):
            display = free_broadway_display()
            if display is None:
                print("no free Broadway display found in :5-:24, skipping it",
                      file=sys.stderr)
            else:
                broadwayd = start_broadwayd(display, workdir / "broadwayd.log")
                if broadwayd is None or broadwayd.poll() is not None:
                    print("gtk4-broadwayd would not start, skipping Broadway",
                          file=sys.stderr)
                    broadwayd = None
                else:
                    base_env = with_render_workaround(dict(os.environ))
                    base_env["GDK_BACKEND"] = "broadway"
                    base_env["BROADWAY_DISPLAY"] = f":{display}"
                    attempts.append(("broadway", base_env))
                    soft_env = dict(base_env)
                    soft_env["WEBKIT_DISABLE_COMPOSITING_MODE"] = "1"
                    attempts.append(("broadway (software compositing)", soft_env))

        for name, env in attempts:
            r, err = run_worker(env, spec_path, result_path, timeout=args.timeout)
            if err:
                print(f"[{name}] {err}", file=sys.stderr)
                continue
            if r.get("backend_ok"):
                backend_used, result = name, r
                break
            print(f"[{name}] worker ran but the page never rendered "
                  f"(first probe: {r.get('first_probe_note')})", file=sys.stderr)
        if backend_used:
            stop_broadwayd(broadwayd, display)
            broadwayd = None

        if backend_used is None and args.backend in ("auto", "wayland"):
            print("falling back to a real (visible, brief) Wayland window",
                  file=sys.stderr)
            env = with_render_workaround(dict(os.environ))
            env.pop("GDK_BACKEND", None)
            env.pop("BROADWAY_DISPLAY", None)
            r, err = run_worker(env, spec_path, result_path, timeout=args.timeout)
            if err:
                print(f"[wayland] {err}", file=sys.stderr)
            elif r.get("backend_ok"):
                backend_used, result = "wayland (visible window)", r
            else:
                result = r

        if backend_used is None:
            print("\nNo backend could render WebKit content.", file=sys.stderr)
            if result:
                print(json.dumps(result, indent=1)[:2000], file=sys.stderr)
            return 1

        report(backend_used, result, args)
        if args.json:
            Path(args.json).write_text(json.dumps(result, indent=1))
        return 0
    finally:
        stop_broadwayd(broadwayd, display)
        shutil.rmtree(workdir, ignore_errors=True)


def report(backend: str, result: dict, args) -> None:
    print(f"backend: {backend}\n")
    for m in result.get("measurements", []):
        if "error" in m:
            print(f"{m['route']:<14} {m['size']:<10} ERROR {m['error']}")
            continue
        feat = m.get("features", {})
        feat_line = (
            f"has():{feat.get('has_has')} popover:{feat.get('has_popover')} "
            f"showModal:{feat.get('has_show_modal')} "
            f"cq:{feat.get('has_container_queries')} "
            f"RO:{feat.get('has_resize_observer')}"
        )
        content = m.get("content", {})
        content_line = (
            f"overflow:{content.get('overflow')} "
            f"scrolls:{content.get('scrolls')} "
            f"(dh={content.get('scroll_h')} dw={content.get('scroll_w')})"
        )
        cq = m.get("container_query", {})
        cq_line = f"rail:{cq.get('app_rail')} page_pad:{cq.get('page_body_padding')}"
        if "usage_budget_grid_column" in cq:
            cq_line += f" usage.budget:{cq.get('usage_budget_grid_column')!r}"
        sticky = m.get("sticky")
        sticky_line = f" sticky:{sticky}" if sticky else ""
        print(f"{m['route']:<14} {m['size']:<10} {feat_line}")
        print(f"{'':<14} {'':<10} {content_line}  {cq_line}{sticky_line}")
    for i in result.get("interactions", []):
        d = i.get("detail") or {}
        # "ok" only means the script ran; a returned {"...": false} or an
        # {"error": ...} payload is itself the FAIL signal for a check step.
        semantic_fail = (
            not i.get("ok")
            or (isinstance(d, dict) and (
                "error" in d
                or d.get("popover_open") is False
                or d.get("dialog_open") is False
                or d.get("inside_viewport") is False
                or d.get("save_inside_viewport") is False
            ))
        )
        status = "FAIL" if semantic_fail else "PASS"
        print(f"\n[{status}] {i['name']}: {json.dumps(d)[:300]}")
    print(f"\nsnapshots: {args.shots}")


# ---------------------------------------------------------------------------
# Worker: everything below runs in the child process, with GDK_BACKEND/
# BROADWAY_DISPLAY/WEBKIT_* already set in its environment by the orchestrator
# above. It drives one WebKitWebView through the whole spec and writes one
# JSON result file, then exits — no interactive/long-lived state to clean up
# beyond the process itself (which the orchestrator waits on).
# ---------------------------------------------------------------------------

MEASURE_JS = r"""(() => {
  const out = { features: {}, content: {}, container_query: {} };
  try { out.features.has_has = CSS.supports('selector(:has(a))'); }
  catch (e) { out.features.has_has = 'error: ' + e; }
  out.features.has_popover = 'popover' in HTMLElement.prototype;
  out.features.has_show_modal =
    typeof (window.HTMLDialogElement && HTMLDialogElement.prototype.showModal);
  out.features.has_container_queries = CSS.supports('container-type: inline-size');
  out.features.has_resize_observer = typeof ResizeObserver !== 'undefined';

  const content = document.querySelector('main.content');
  out.has_content = !!content;
  if (content) {
    const cs = getComputedStyle(content);
    out.content.overflow = cs.overflow;
    out.content.scroll_h = content.scrollHeight - content.clientHeight;
    out.content.scroll_w = content.scrollWidth - content.clientWidth;
    out.content.scrolls = out.content.scroll_h > 1 || out.content.scroll_w > 1;
  }

  const appEl = document.querySelector('.app');
  out.container_query.app_rail = appEl ? appEl.classList.contains('rail') : null;
  const pageBody = document.querySelector('.page-body');
  if (pageBody) {
    const cs = getComputedStyle(pageBody);
    out.container_query.page_body_padding = [cs.paddingLeft, cs.paddingRight];
  }
  const budget = document.querySelector('.usage .u-budget');
  if (budget) out.container_query.usage_budget_grid_column = getComputedStyle(budget).gridColumn;
  const tiles = document.querySelector('.usage .tiles');
  if (tiles) out.container_query.usage_tiles_columns = getComputedStyle(tiles).gridTemplateColumns;

  const table = document.querySelector('table.data');
  const th = table ? table.querySelector('thead th') : null;
  if (th) {
    let scroller = table.parentElement;
    while (scroller && !(scroller.scrollHeight > scroller.clientHeight + 2)) {
      scroller = scroller.parentElement;
    }
    if (scroller) {
      const before = th.getBoundingClientRect().top;
      scroller.scrollTop += 150;
      const after = th.getBoundingClientRect().top;
      scroller.scrollTop -= 150;
      out.sticky = { position: getComputedStyle(th).position,
        before: before, after: after, delta: after - before,
        stuck: Math.abs(after - before) < 2 };
    } else {
      out.sticky = 'no-scrolling-ancestor-found';
    }
  }

  out.viewport = [innerWidth, innerHeight];
  out.body_text_len = document.body.innerText.length;
  return JSON.stringify(out);
})()"""

SETTLE_JS = (
    "JSON.stringify([document.querySelectorAll('*').length,"
    " document.body.innerText.length,"
    " !document.querySelector('main.content')"
    " || document.body.innerText.includes('Loading')])"
)

CHAT_OPEN_THREAD_JS = r"""(() => {
  const row = document.querySelector('.thread-list .thread-row');
  if (!row) return JSON.stringify({ error: 'no existing thread to open (none in sidebar)' });
  row.click();
  return JSON.stringify({ clicked: true });
})()"""

CHAT_PICKER_CLICK_JS = r"""(() => {
  const btn = document.querySelector('.model-picker button.select-btn')
    || document.querySelector('.model-picker .mp-btn');
  if (!btn) return JSON.stringify({ error: 'no model picker button found',
    has_model_picker: !!document.querySelector('.model-picker') });
  btn.click();
  return JSON.stringify({ clicked: true });
})()"""

POPOVER_CHECK_JS = r"""(() => {
  const pop = document.querySelector('[popover]:popover-open');
  if (!pop) return JSON.stringify({ popover_open: false });
  const r = pop.getBoundingClientRect();
  const inside = r.left >= -1 && r.top >= -1 && r.right <= innerWidth + 1 && r.bottom <= innerHeight + 1;
  return JSON.stringify({
    popover_open: pop.matches(':popover-open'),
    rect: { left: r.left, top: r.top, right: r.right, bottom: r.bottom, width: r.width, height: r.height },
    inside_viewport: inside, viewport: [innerWidth, innerHeight],
  });
})()"""

MCP_EDIT_CLICK_JS = r"""(() => {
  const btn = document.querySelector('table.data tbody tr td button.link-btn');
  if (!btn) return JSON.stringify({ error: 'no server row edit control found' });
  btn.click();
  return JSON.stringify({ clicked: true, server: btn.textContent.trim() });
})()"""

MCP_DIALOG_CHECK_JS = r"""(() => {
  const dlg = document.querySelector('dialog.modal[open]') || document.querySelector('dialog[open]');
  if (!dlg) return JSON.stringify({ dialog_open: false });
  const saveBtn = [...dlg.querySelectorAll('.modal-foot button')].find(b => /save/i.test(b.textContent));
  const r = saveBtn ? saveBtn.getBoundingClientRect() : null;
  const inside = r ? (r.left >= -1 && r.top >= -1 && r.right <= innerWidth + 1 && r.bottom <= innerHeight + 1) : null;
  return JSON.stringify({
    dialog_open: dlg.open, save_label: saveBtn ? saveBtn.textContent.trim() : null,
    save_rect: r ? { left: r.left, top: r.top, right: r.right, bottom: r.bottom } : null,
    save_inside_viewport: inside, viewport: [innerWidth, innerHeight],
  });
})()"""

MCP_DIALOG_CLOSE_JS = r"""(() => {
  const btn = document.querySelector('dialog[open] .modal-head button.btn.ghost');
  if (!btn) return JSON.stringify({ error: 'no close (✕) button found' });
  btn.click();
  return JSON.stringify({ closed_clicked: true });
})()"""


def worker_main(spec_path: str, result_path: str) -> int:
    import gi

    gi.require_version("Gtk", "4.0")
    gi.require_version("WebKit", "6.0")
    from gi.repository import Gtk, WebKit, GLib  # noqa: E402

    spec = json.loads(Path(spec_path).read_text())
    base, token = spec["base"], spec["token"]
    routes = spec["routes"]
    sizes = [tuple(s) for s in spec["sizes"]]
    shots = Path(spec["shots"])
    settle_max = spec["settle_max_ms"]

    measurements = []
    interactions = []

    max_w = max([w for w, h in sizes] + [1440]) + 40
    max_h = max([h for w, h in sizes] + [900]) + 40

    app = Gtk.Application(application_id="dev.lmgw.webkitcheck")

    def on_activate(app):
        win = Gtk.Window(application=app)
        win.set_decorated(False)
        win.set_default_size(max_w, max_h)
        session = WebKit.NetworkSession.new_ephemeral()
        webview = WebKit.WebView(network_session=session)
        # Sized before first present, so the login page (before any resize
        # step runs) is not rendered into a 0x0 view.
        webview.set_size_request(*sizes[0])
        fixed = Gtk.Fixed()
        fixed.put(webview, 0, 0)
        win.set_child(fixed)
        win.present()

        def ev(script, cb):
            def inner(wv, gres, data):
                try:
                    val = wv.evaluate_javascript_finish(gres)
                    cb(val.to_string() if val is not None else "null", None)
                except Exception as e:  # noqa: BLE001 - reported, not raised
                    cb(None, str(e))

            webview.evaluate_javascript(script, -1, None, None, None, inner, None)

        def wait_ms(ms, cb):
            GLib.timeout_add(ms, lambda: (cb(), False)[1])

        def wait_for_load(cb, timeout_ms=30000):
            state = {"done": False, "handler": None, "timeout_id": None}

            def finish():
                if state["done"]:
                    return
                state["done"] = True
                if state["handler"] is not None:
                    webview.disconnect(state["handler"])
                if state["timeout_id"] is not None:
                    GLib.source_remove(state["timeout_id"])
                cb()

            def on_load(wv, event, *a):
                if event == WebKit.LoadEvent.FINISHED:
                    finish()

            state["handler"] = webview.connect("load-changed", on_load)

            def on_timeout():
                state["timeout_id"] = None
                finish()
                return False

            state["timeout_id"] = GLib.timeout_add(timeout_ms, on_timeout)

        def settle(done_cb, min_wait_ms=800):
            st = {"last": None, "calm": 0, "waited": 0}

            def poll():
                def cb(v, e):
                    try:
                        sig = json.loads(v) if v else None
                    except (TypeError, ValueError):
                        sig = None
                    busy = not isinstance(sig, list) or sig[2]
                    st["calm"] = st["calm"] + 1 if sig == st["last"] and not busy else 0
                    st["last"] = sig
                    st["waited"] += 300
                    if st["calm"] >= 3 or st["waited"] >= settle_max:
                        done_cb()
                    else:
                        GLib.timeout_add(300, poll)

                ev(SETTLE_JS, cb)
                return False

            GLib.timeout_add(min_wait_ms, poll)

        def snapshot(name, done_cb):
            def cb(wv, gres, data):
                try:
                    tex = wv.get_snapshot_finish(gres)
                    tex.save_to_png(str(shots / f"{name}.png"))
                except Exception as e:  # noqa: BLE001
                    interactions.append({"name": f"snapshot:{name}", "ok": False, "detail": str(e)})
                done_cb()

            webview.get_snapshot(WebKit.SnapshotRegion.VISIBLE, WebKit.SnapshotOptions(0), None, cb, None)

        # --- step queue -----------------------------------------------
        queue = []

        def add(fn):
            queue.append(fn)

        def run_next():
            if not queue:
                finish_run()
                return
            queue.pop(0)(run_next)

        def step_login(nxt):
            wait_for_load(lambda: wait_ms(500, nxt))
            webview.load_uri(f"{base}/api/session/login?token={token}")

        def make_nav(route):
            def step(nxt):
                wait_for_load(lambda: settle(nxt))
                webview.load_uri(f"{base}{route}")

            return step

        def make_resize(w, h):
            # A fixed delay after set_size_request was not reliable: chained
            # through many resizes in one run, the widget's own reallocation
            # (and, downstream, the app's `resize` listener redrawing the
            # rail) sometimes lands well after 500ms and sometimes before —
            # so poll innerWidth/innerHeight for the resize to actually take,
            # then give the app's listener a moment to react to it.
            def step(nxt):
                webview.set_size_request(w, h)

                def poll(n):
                    def cb(v, e):
                        try:
                            dims = json.loads(v) if v else None
                        except (TypeError, ValueError):
                            dims = None
                        if dims == [w, h] or n <= 0:
                            wait_ms(500, nxt)
                        else:
                            GLib.timeout_add(150, lambda: (poll(n - 1), False)[1])

                    ev("JSON.stringify([innerWidth, innerHeight])", cb)

                GLib.timeout_add(100, lambda: (poll(20), False)[1])

            return step

        def make_measure(route, w, h):
            def step(nxt):
                def cb(v, e):
                    if e:
                        measurements.append({"route": route, "size": f"{w}x{h}", "error": e})
                    else:
                        try:
                            m = json.loads(v)
                        except (TypeError, ValueError) as ex:
                            m = {"error": f"bad JSON from probe: {ex}: {v!r}"}
                        m["route"], m["size"] = route, f"{w}x{h}"
                        measurements.append(m)
                    nxt()

                ev(MEASURE_JS, cb)

            return step

        def make_snapshot(name):
            return lambda nxt: snapshot(name, nxt)

        def make_click(script, name):
            def step(nxt):
                def cb(v, e):
                    detail = json.loads(v) if v and not e else {"error": e}
                    interactions.append({"name": name, "ok": e is None and "error" not in (detail or {}),
                                          "detail": detail})
                    nxt()

                ev(script, cb)

            return step

        def make_wait(ms):
            return lambda nxt: wait_ms(ms, nxt)

        def make_check(script, name):
            def step(nxt):
                def cb(v, e):
                    detail = json.loads(v) if v and not e else {"error": e}
                    interactions.append({"name": name, "ok": e is None, "detail": detail})
                    nxt()

                ev(script, cb)

            return step

        add(step_login)
        for route in routes:
            add(make_nav(route))
            for w, h in sizes:
                add(make_resize(w, h))
                add(make_measure(route, w, h))
                add(make_snapshot(f"{slug(route)}_{w}x{h}"))

            if route == "/chat":
                for w, h in sizes:
                    add(make_resize(w, h))
                    add(make_click(CHAT_OPEN_THREAD_JS, f"chat_open_thread_{w}x{h}"))
                    add(make_wait(600))
                    add(make_click(CHAT_PICKER_CLICK_JS, f"chat_picker_click_{w}x{h}"))
                    add(make_wait(500))
                    add(make_check(POPOVER_CHECK_JS, f"chat_popover_{w}x{h}"))
                    add(make_snapshot(f"chat_popover_{w}x{h}"))

            if route == "/mcp-servers":
                for w, h in sizes + [(1440, 600)]:
                    add(make_resize(w, h))
                    add(make_click(MCP_EDIT_CLICK_JS, f"mcp_edit_click_{w}x{h}"))
                    add(make_wait(600))
                    add(make_check(MCP_DIALOG_CHECK_JS, f"mcp_dialog_{w}x{h}"))
                    add(make_snapshot(f"mcp_dialog_{w}x{h}"))
                    add(make_click(MCP_DIALOG_CLOSE_JS, f"mcp_dialog_close_{w}x{h}"))
                    add(make_wait(400))

        def finish_run():
            first_ok = bool(measurements) and measurements[0].get("has_content")
            note = None
            if measurements:
                note = {"has_content": measurements[0].get("has_content"),
                        "body_text_len": measurements[0].get("body_text_len")}
            out = {
                "backend_ok": first_ok,
                "first_probe_note": note,
                "measurements": measurements,
                "interactions": interactions,
            }
            Path(result_path).write_text(json.dumps(out, indent=1))
            app.quit()

        # Global watchdog: however far the queue got, always write a result.
        GLib.timeout_add(int(os.environ.get("LMGW_WK_WATCHDOG_MS", "240000")),
                          lambda: (finish_run(), False)[1] if queue or not Path(result_path).exists() else False)

        run_next()

    app.connect("activate", on_activate)
    app.run(None)
    return 0


def main() -> int:
    ap = argparse.ArgumentParser(
        description=__doc__.splitlines()[0],
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    ap.add_argument("--_worker", action="store_true", help=argparse.SUPPRESS)
    ap.add_argument("--spec", help=argparse.SUPPRESS)
    ap.add_argument("--result", help=argparse.SUPPRESS)
    ap.add_argument("--base", default=DEFAULT_BASE, help="dashboard origin")
    ap.add_argument("--token", help="owner token (default: read --token-file)")
    ap.add_argument("--token-file", default="/tmp/lmgw-ux-token")
    ap.add_argument("--routes", default=",".join(DEFAULT_ROUTES),
                     help="comma-separated routes")
    ap.add_argument("--sizes", default=",".join(DEFAULT_SIZES),
                     help="comma-separated WxH window sizes")
    ap.add_argument("--shots", default="/tmp/lmgw-ux-shots/webkit",
                     help="directory for snapshot PNGs")
    ap.add_argument("--json", metavar="OUT", help="write the full result as JSON")
    ap.add_argument("--backend", choices=["auto", "broadway", "wayland"], default="auto")
    ap.add_argument("--timeout", type=float, default=280.0,
                     help="seconds allowed per backend attempt")
    ap.add_argument("--wait", type=float, default=20.0,
                     help="most seconds to wait for a route to settle")
    args = ap.parse_args()

    if args._worker:
        return worker_main(args.spec, args.result)

    args.routes = [r.strip() for r in args.routes.split(",") if r.strip()]
    args.sizes = [s.strip() for s in args.sizes.split(",") if s.strip()]
    return orchestrate(args)


if __name__ == "__main__":
    sys.exit(main())
