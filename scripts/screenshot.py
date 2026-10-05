#!/usr/bin/env python3
# Screenshot a page in headless Chrome, waiting for JS/WASM to settle first.
# Exists because `--screenshot --virtual-time-budget` never fires on pages
# holding an open SSE/EventSource connection (the Leptos UI always does).
# Usage: scripts/screenshot.py <url> <out.png> [wait_seconds=3] [WxH=1280x900] [js]
#   js: JavaScript evaluated 1.5s after navigation (e.g. to click a button);
#       the wait_seconds delay runs after it, then the screenshot is taken.
import asyncio
import base64
import json
import os
import socket
import subprocess
import sys
import tempfile
import urllib.request

import websockets

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import chrome_profile  # noqa: E402


async def main() -> None:
    url = sys.argv[1]
    out = sys.argv[2]
    wait = float(sys.argv[3]) if len(sys.argv) > 3 else 3.0
    size = sys.argv[4] if len(sys.argv) > 4 else "1280x900"
    js = sys.argv[5] if len(sys.argv) > 5 else None
    width, height = (int(v) for v in size.split("x"))

    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        port = s.getsockname()[1]

    # /tmp is a tmpfs on Fedora, so a profile left behind costs RAM, not disk.
    profile = tempfile.mkdtemp(prefix="lmgw-shot-")
    tmp_before = chrome_profile.snapshot()
    chrome = subprocess.Popen(
        [
            "google-chrome",
            "--headless",
            "--disable-gpu",
            f"--remote-debugging-port={port}",
            f"--window-size={width},{height}",
            f"--user-data-dir={profile}",
            "about:blank",
        ],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    try:
        pages = []
        for _ in range(50):
            try:
                with urllib.request.urlopen(
                    f"http://127.0.0.1:{port}/json/list"
                ) as resp:
                    tabs = json.load(resp)
                pages = [t for t in tabs if t.get("type") == "page"]
                if pages:
                    break
            except OSError:
                pass
            await asyncio.sleep(0.2)
        ws_url = pages[0]["webSocketDebuggerUrl"]

        async with websockets.connect(ws_url, max_size=64 * 1024 * 1024) as ws:
            msg_id = 0

            async def call(method: str, **params):
                nonlocal msg_id
                msg_id += 1
                await ws.send(
                    json.dumps({"id": msg_id, "method": method, "params": params})
                )
                while True:
                    reply = json.loads(await ws.recv())
                    if reply.get("id") == msg_id:
                        return reply.get("result", {})

            await call("Emulation.setDeviceMetricsOverride",
                       width=width, height=height, deviceScaleFactor=1, mobile=False)
            await call("Page.enable")
            await call("Page.navigate", url=url)
            if js:
                await asyncio.sleep(1.5)
                await call("Runtime.evaluate", expression=js)
            await asyncio.sleep(wait)
            shot = await call("Page.captureScreenshot", format="png")
            with open(out, "wb") as f:
                f.write(base64.b64decode(shot["data"]))
            print(f"wrote {out}")
    finally:
        chrome.terminate()
        chrome.wait()
        chrome_profile.remove(profile, tmp_before)


asyncio.run(asyncio.wait_for(main(), timeout=60))
