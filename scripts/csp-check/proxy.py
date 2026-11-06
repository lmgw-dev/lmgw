#!/usr/bin/env python3
# Usage: scripts/csp-check/proxy.py LISTEN_PORT UPSTREAM_BASE REPORTS_FILE UNTAGGED_FILE
"""The CSP check's front (scripts/csp-check.sh): a gateway's pages and API,
each answer under the policy of the host the drive plays.

Every request is passed to UPSTREAM_BASE (a dev instance) as it came, its
answer passed back as it streams (SSE included). Which policy goes with it
is the drive's choice (`POST /__csp-check/mode`):

- `dashboard` (the default): an extension page's `script-src` is added as a
  second policy beside the gateway's own, so the page runs under both:

    Content-Security-Policy: script-src 'self' 'wasm-unsafe-eval'; report-uri /__csp-check/report

- `extension`: the gateway's own policy is dropped and the page runs under
  EXTENSION alone, the policy the Firefox sidebar's manifest carries
  (chat-plane design §8.9), plus the report-uri.

A violation is reported to /__csp-check/report and appended to REPORTS_FILE
as one JSON line.

The relay check (the debug build's test host, crates/lmgw-ui/src/test_host.rs):
while the mode says `relay`, every request to `/chat/api/…` or `/v1/…` must
carry `x-lmgw-test-relay: 1`, the header the test host's transport puts on
everything it sends. One without it went past the transport: it is appended
to UNTAGGED_FILE and listed by `GET /__csp-check/log`, and the check fails.
The drive's own requests carry `x-lmgw-test-driver: 1` and are not the
page's. The relayed ones are listed too (method, path, `Last-Event-ID`,
status), so a drive can prove which went through the transport.

`POST /__csp-check/cut-feed` cuts every open `/chat/api/feed` stream mid-way
(both sockets shut, no last chunk), as a network that drops would: the page
must reconnect with its cursor.

/__csp-check/<file> serves the files beside this script (the test host's
preview page, the canary) under the mode's policy.
"""
import http.client
import http.server
import json
import socket
import sys
import threading
from pathlib import Path
from urllib.parse import urlsplit

HERE = Path(__file__).resolve().parent
REPORT = "report-uri /__csp-check/report"
SCRIPT_ONLY = "script-src 'self' 'wasm-unsafe-eval'"
# The Firefox sidebar's extension page policy (chat-plane design §8.9), the
# only policy in `extension` mode. Every directive is one the Chat needs:
# scripts are files and the wasm module; pictures and recordings come as
# object URLs (blob:) and KaTeX/CSS as data:; inline style attributes and the
# <style> in a Mermaid diagram need 'unsafe-inline' in style-src; connect-src
# 'self' is the bundle's own wasm fetch (and, in this test host, the relay's
# fetch). default-src 'self' covers the rest (the preview page's frame).
EXTENSION = (
    "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; "
    "style-src 'self' 'unsafe-inline'; img-src 'self' blob: data:; "
    "media-src 'self' blob: data:; font-src 'self' data:; connect-src 'self'; "
    "object-src 'none'"
)
HOP = {"connection", "keep-alive", "transfer-encoding", "te", "trailer", "upgrade",
       "proxy-authorization", "proxy-authenticate"}
TYPES = {".html": "text/html; charset=utf-8", ".js": "text/javascript; charset=utf-8"}
RELAY_TAG = "x-lmgw-test-relay"
DRIVER_TAG = "x-lmgw-test-driver"


class State:
    def __init__(self, untagged: Path):
        self.lock = threading.Lock()
        self.relay = False
        self.policy = "dashboard"
        self.untagged_file = untagged
        self.untagged: list[dict] = []
        self.relayed: list[dict] = []
        self.cuts = 0
        # Open feed streams: (client socket, upstream connection).
        self.feeds: set = set()

    def header(self) -> str:
        with self.lock:
            extension = self.policy == "extension"
        return f"{EXTENSION}; {REPORT}" if extension else f"{SCRIPT_ONLY}; {REPORT}"

    def extension(self) -> bool:
        with self.lock:
            return self.policy == "extension"


def is_chat_request(path: str) -> bool:
    p = path.split("?", 1)[0]
    return p.startswith("/chat/api/") or p == "/chat/api" or p.startswith("/v1/")


def main() -> None:
    port, upstream = int(sys.argv[1]), sys.argv[2]
    reports, untagged = Path(sys.argv[3]), Path(sys.argv[4])
    up = urlsplit(upstream)
    up_host = up.netloc
    state = State(untagged)

    class Handler(http.server.BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, *_):
            pass

        def _json(self, status: int, value) -> None:
            data = json.dumps(value).encode()
            self.send_response(status)
            self.send_header("content-type", "application/json")
            self.send_header("content-length", str(len(data)))
            self.send_header("cache-control", "no-store")
            self.end_headers()
            self.wfile.write(data)

        def _body(self) -> bytes:
            n = int(self.headers.get("content-length") or 0)
            return self.rfile.read(n) if n else b""

        def _local(self) -> bool:
            path = self.path.split("?", 1)[0]
            if not path.startswith("/__csp-check/"):
                return False
            name = path[len("/__csp-check/"):]
            if name == "report":
                body = self._body().decode("utf-8", "replace")
                with reports.open("a") as f:
                    f.write(json.dumps({"report": body}) + "\n")
                self.send_response(204)
                self.send_header("content-length", "0")
                self.end_headers()
                return True
            if name == "mode" and self.command == "POST":
                want = json.loads(self._body() or b"{}")
                with state.lock:
                    state.relay = bool(want.get("relay", state.relay))
                    state.policy = want.get("policy", state.policy)
                    now = {"relay": state.relay, "policy": state.policy}
                self._json(200, now)
                return True
            if name == "log":
                with state.lock:
                    self._json(200, {"relay": state.relay, "policy": state.policy,
                                     "relayed": state.relayed, "untagged": state.untagged,
                                     "cuts": state.cuts, "open_feeds": len(state.feeds)})
                return True
            if name == "cut-feed" and self.command == "POST":
                with state.lock:
                    feeds = list(state.feeds)
                    state.cuts += len(feeds)
                for client, conn in feeds:
                    for s in (client, conn.sock):
                        try:
                            s.shutdown(socket.SHUT_RDWR)
                        except (OSError, AttributeError):
                            pass
                self._json(200, {"cut": len(feeds)})
                return True
            f = HERE / name
            if "/" in name or not f.is_file() or f.suffix not in TYPES:
                self.send_response(404)
                self.send_header("content-length", "0")
                self.end_headers()
                return True
            data = f.read_bytes()
            self.send_response(200)
            self.send_header("content-type", TYPES[f.suffix])
            self.send_header("content-length", str(len(data)))
            self.send_header("cache-control", "no-store")
            self.send_header("content-security-policy", state.header())
            self.end_headers()
            self.wfile.write(data)
            return True

        def _note(self) -> dict | None:
            """The relay check's record of this request, when it is one to
            check: None for one the check does not look at."""
            if not is_chat_request(self.path) or self.headers.get(DRIVER_TAG):
                return None
            with state.lock:
                if not state.relay:
                    return None
            row = {"method": self.command, "path": self.path,
                   "last_event_id": self.headers.get("last-event-id")}
            if self.headers.get(RELAY_TAG) == "1":
                with state.lock:
                    state.relayed.append(row)
                return row
            with state.lock:
                state.untagged.append(row)
            with state.untagged_file.open("a") as f:
                f.write(json.dumps(row) + "\n")
            return row

        def _forward(self) -> None:
            if self._local():
                return
            note = self._note()
            body = self._body() or None
            headers = {k: v for k, v in self.headers.items() if k.lower() not in HOP}
            # The gateway sees its own origin: a cookie-held session's writes
            # are checked against it.
            headers["Host"] = up_host
            for k in list(headers):
                if k.lower() in ("origin", "referer"):
                    headers[k] = headers[k].replace(f"127.0.0.1:{port}", up_host)
            conn = http.client.HTTPConnection(up.hostname, up.port, timeout=600)
            try:
                conn.request(self.command, self.path, body=body, headers=headers)
                resp = conn.getresponse()
            except OSError as e:
                self.send_error(502, str(e))
                return
            if note is not None:
                note["status"] = resp.status
            extension = state.extension()
            self.send_response(resp.status, resp.reason)
            length = resp.getheader("content-length")
            for k, v in resp.getheaders():
                if k.lower() in HOP:
                    continue
                # Under the extension policy alone, nothing of the
                # gateway's own policy.
                if extension and k.lower() == "content-security-policy":
                    continue
                self.send_header(k, v)
            self.send_header("content-security-policy", state.header())
            chunked = length is None and self.command != "HEAD" and resp.status not in (204, 304)
            if chunked:
                self.send_header("transfer-encoding", "chunked")
            self.end_headers()
            feed = self.path.split("?", 1)[0] == "/chat/api/feed" and resp.status == 200
            key = (self.connection, conn)
            if feed:
                with state.lock:
                    state.feeds.add(key)
            try:
                while True:
                    data = resp.read1(65536)
                    if not data:
                        break
                    if chunked:
                        self.wfile.write(b"%x\r\n%s\r\n" % (len(data), data))
                    else:
                        self.wfile.write(data)
                    self.wfile.flush()
                if chunked:
                    self.wfile.write(b"0\r\n\r\n")
            except (OSError, http.client.HTTPException):
                # A cut (or the browser went): no last chunk, and this
                # connection is not used again.
                self.close_connection = True
            finally:
                if feed:
                    with state.lock:
                        state.feeds.discard(key)
                conn.close()

        do_GET = do_POST = do_PUT = do_PATCH = do_DELETE = do_HEAD = _forward

    srv = http.server.ThreadingHTTPServer(("127.0.0.1", port), Handler)
    srv.daemon_threads = True
    print(f"csp-check proxy on 127.0.0.1:{port} -> {upstream}", flush=True)
    srv.serve_forever()


if __name__ == "__main__":
    main()
