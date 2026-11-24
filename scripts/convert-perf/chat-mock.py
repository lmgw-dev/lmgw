#!/usr/bin/env python3
# Usage: chat-mock.py PORT [MIB]   a loopback OpenAI-compatible upstream for the allocator rows (design §14 performance round review): /v1/models, and /v1/chat/completions answering MIB MiB (default 64) of text, streamed as SSE chunks of 16 KiB when "stream" is true, else in one JSON body
import json, sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
PORT = int(sys.argv[1]); MIB = int(sys.argv[2]) if len(sys.argv) > 2 else 64
PIECE = ("example " * 2048)[:16384]
class H(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def log_message(self, *a): pass
    def do_GET(self):
        body = json.dumps({"object": "list", "data": [{"id": "big", "object": "model"}]}).encode()
        self.send_response(200); self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body))); self.end_headers(); self.wfile.write(body)
    def do_POST(self):
        req = json.loads(self.rfile.read(int(self.headers.get("content-length", 0))) or b"{}")
        n = MIB * 1024 * 1024 // len(PIECE)
        if req.get("stream"):
            self.send_response(200); self.send_header("content-type", "text/event-stream")
            self.send_header("transfer-encoding", "chunked"); self.end_headers()
            def send(b):
                self.wfile.write(b"%x\r\n" % len(b) + b + b"\r\n")
            for i in range(n):
                c = {"id": "c1", "object": "chat.completion.chunk", "created": 0, "model": "big",
                     "choices": [{"index": 0, "delta": {"content": PIECE}, "finish_reason": None}]}
                send(b"data: " + json.dumps(c).encode() + b"\n\n")
            end = {"id": "c1", "object": "chat.completion.chunk", "created": 0, "model": "big",
                   "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                   "usage": {"prompt_tokens": 1, "completion_tokens": n * 2048, "total_tokens": n * 2048 + 1}}
            send(b"data: " + json.dumps(end).encode() + b"\n\ndata: [DONE]\n\n"); send(b"")
        else:
            body = json.dumps({"id": "c1", "object": "chat.completion", "created": 0, "model": "big",
                               "choices": [{"index": 0, "message": {"role": "assistant", "content": PIECE * n}, "finish_reason": "stop"}],
                               "usage": {"prompt_tokens": 1, "completion_tokens": n * 2048, "total_tokens": n * 2048 + 1}}).encode()
            self.send_response(200); self.send_header("content-type", "application/json")
            self.send_header("content-length", str(len(body))); self.end_headers(); self.wfile.write(body)
ThreadingHTTPServer(("127.0.0.1", PORT), H).serve_forever()
