#!/usr/bin/env python3
# Usage: scripts/leak-listener.py [PORT=9299]
"""Records every request that reaches it; GET /hits lists the paths as JSON.

The target of the "leak" URLs in scripts/drive/chat-security.json: a dashboard
that fetches a remote resource on its own (a Mermaid label image, CSS url())
shows up in /hits. CORS is open so the driven page can read /hits itself.
"""
import json
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

HITS = []


class H(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == "/hits":
            body = json.dumps(HITS).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
        else:
            HITS.append(self.path)
            body = b""
            self.send_response(404)
        self.send_header("Access-Control-Allow-Origin", "*")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *_):
        pass


ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1]) if len(sys.argv) > 1 else 9299), H).serve_forever()
