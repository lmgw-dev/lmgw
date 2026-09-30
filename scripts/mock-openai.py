#!/usr/bin/env python3
"""Mock OpenAI-shaped (and llama-server-shaped) upstream: a JSON classifier,
a real SSE token stream, and the API-reference tester's compatibility routes.

Two shapes for `/chat/completions`, picked off the request body:

* **Classifier** (`response_format.json_schema` present, no `stream`) — what the
  classify stage of a `batch` agent sends: one non-streaming /chat/completions
  per row, answered as `{"category": <enum>}` by keyword so the review table
  looks like a real run. One row is deliberately left unmatched so the "needs
  attention" branch of the UI has something in it. The mail-labeler agent's
  enum output has exactly this shape, and so did the mail workflow it replaced.
* **Chat** (`stream: true`) — a genuine `text/event-stream` of one-token-ish
  chunks with a configurable per-chunk delay, which is what makes it usable for
  exercising anything that measures a live stream (the titlebar's tok/s
  readout: STREAM_DELAY=0.05 lands around 20 tok/s). Without `stream` the same
  answer comes back as one JSON body.

Four more routes, matched on the request path (before the shapes above, which
never look at the path at all and would otherwise swallow them): `/tokenize`
and `/apply-template` answer in llama-server's own shape (api-docs design
§7.5) — this is what a `kind: llama_server` upstream row needs for the
`/api-reference` tester's `post_tokenize` and the count-tokens compat routes
to have something real to call. `/embeddings` and `/audio/speech` answer
OpenAI-shaped, for `post_v1_embeddings` and `post_v1_audio_speech`.

Environment:
  MODE=slow          per-call delay so the live progress view can be caught mid-run
  MODE=down          500 everything, to exercise the failure banner
  DELAY=<s>          delay before a non-streaming answer (default 0)
  STREAM_DELAY=<s>   delay between streamed chunks (default 0.05 → ~20 tok/s)
  STREAM_TOKENS=<n>  chunks in a streamed answer (default 80)
  STREAM_CODE=0      drop the fenced code block from the answer (default: keep)
  EMBED_DIMS=<n>     `/embeddings` answers deterministic hashed bag-of-words vectors
                     of n dimensions (L2-normalised: texts sharing words are close,
                     so a knowledge-base search finds the right file) instead of the
                     fixed 3-number vector — for driving the Knowledge page
                     (scripts/knowledge-setup.sh, scripts/drive/knowledge.json)
  ECHO=1             streamed chat answers "Echo: <last user message>" instead of
                     the canned text; a trailing assistant message (a prefill,
                     as the Chat's Continue sends) is continued with "…and then
                     some more words." — for driving the Chat's message actions
                     (scripts/drive/chat-actions.json). A user message carrying a
                     `<context source="knowledge">` block (the Chat's knowledge
                     retrieval) is answered with the question plus citations
                     "[1]" and "[9]" (scripts/drive/chat-knowledge.json)

Usage: mock-openai.py <port>
"""
import io
import itertools
import json
import os
import re
import sys
import time
import wave
import zlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

MODE = os.environ.get("MODE", "normal")
DELAY = float(os.environ.get("DELAY", "0"))
STREAM_DELAY = float(os.environ.get("STREAM_DELAY", "0.05"))
STREAM_TOKENS = int(os.environ.get("STREAM_TOKENS", "80"))
STREAM_CODE = os.environ.get("STREAM_CODE", "1") != "0"
ECHO = os.environ.get("ECHO", "0") == "1"
EMBED_DIMS = int(os.environ.get("EMBED_DIMS", "0"))

# Matched against the mail's own text only — deliberately narrow keys, so the
# List-Unsubscribe header every mail carries does not turn everything into a
# newsletter.
RULES = [
    ("Receipts", ["receipt", "order #"]),
    ("Finance", ["invoice", "amount due"]),
    ("Travel", ["flight", "booking bx"]),
    ("Promotions", ["% off", "half price", "weekend sale"]),
    ("Personal", ["dinner", "board game"]),
    ("Work", ["sprint"]),
    ("Newsletter", ["digest", "stories you missed"]),
    # only reachable once the user widens the taxonomy in the re-run control
    ("Automated", ["no further detail", "ref 88121"]),
]

# Filler for the chat answer. Deliberately prose-shaped and varied so a
# streamed reply looks like a model wrote it and markdown rendering has
# something to chew on.
SENTENCES = [
    "A gateway is only as observable as its slowest path, so the readout you are"
    " watching is fed by the same relay that carries these words.",
    "Every chunk below leaves this mock upstream on a fixed delay, which makes"
    " the resulting rate predictable rather than merely plausible.",
    "Nothing here is generated; it is canned text, metered out one piece at a"
    " time so the transport behaves exactly like a real completion.",
    "If the number in the titlebar tracks the configured delay, the measurement"
    " is honest end to end.",
    "The point of a mock is to be boring in every dimension except the one under"
    " test.",
]

CODE_BLOCK = """

```rust
fn tokens_per_second(counted: u64, window: Duration) -> f64 {
    counted as f64 / window.as_secs_f64()
}
```

"""


def classify(text, allowed):
    low = text.lower()
    for cat, keys in RULES:
        if cat in allowed and any(k in low for k in keys):
            return cat
    return "Other"


def fake_tokenize(content, add_special):
    """llama-server's `/tokenize` shape (api-docs design §7.5): one id per
    whitespace word, `crc32(word) % 50000` so the same word always gets the
    same id — real enough for a UI check that counts tokens, not their
    values. `content` may be a string or, per llama.cpp, a mixed array of
    strings and already-tokenized ids; an int entry is passed through as its
    own id (nothing left to tokenize). `id` 1 (a stand-in BOS id) is
    prepended when `add_special` is set, paired with the piece `"<s>"`.
    Returns `(ids, pieces)`, same length, zipped by the caller into whichever
    of the two response shapes `with_pieces` asked for.
    """
    parts = content if isinstance(content, list) else [content]
    ids, pieces = [], []
    for part in parts:
        if isinstance(part, int):
            ids.append(part)
            pieces.append(f"[{part}]")
        else:
            for word in str(part).split():
                ids.append(zlib.crc32(word.encode()) % 50000)
                pieces.append(word)
    if add_special:
        ids.insert(0, 1)
        pieces.insert(0, "<s>")
    return ids, pieces


def fake_embedding(text, dims):
    """Hashed bag of words: each lower-cased word adds 1 to bucket
    `crc32(word) % dims`, then the vector is L2-normalised. Deterministic, and
    two texts that share words have a high cosine — enough for a retrieval UI
    check with no GPU."""
    v = [0.0] * dims
    for word in re.findall(r"\w+", str(text).lower()):
        v[zlib.crc32(word.encode()) % dims] += 1.0
    norm = sum(x * x for x in v) ** 0.5 or 1.0
    return [x / norm for x in v]


def fake_prompt(messages):
    """llama-server's `/apply-template` shape (api-docs design §7.5): one
    `"role: content"` line per message — not a real chat template, but
    enough to prove the count-tokens compat route rendered *something*
    through this call rather than skipping it."""
    lines = []
    for m in messages:
        content = m.get("content", "")
        if isinstance(content, list):
            content = "".join(p.get("text", "") for p in content if isinstance(p, dict))
        lines.append(f"{m.get('role', '')}: {content}")
    return "\n".join(lines)


def silent_wav(seconds=0.5, rate=16000):
    """0.5 s of 16 kHz mono 16-bit silence, real WAV bytes (stdlib `wave`,
    no synthesis library needed) — enough for `<audio>` to accept it and
    `post_v1_audio_speech`'s response classification (§6.7) to see `audio/*`."""
    buf = io.BytesIO()
    with wave.open(buf, "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(rate)
        w.writeframes(b"\x00\x00" * int(seconds * rate))
    return buf.getvalue()


def answer_pieces(n):
    """`n` chunk-sized pieces of a canned answer (word + trailing space).

    llama-server streams one chunk per token; splitting on words is the closest
    a mock gets to that without a tokenizer, and it keeps the arithmetic
    obvious: n pieces at STREAM_DELAY apart is n/STREAM_DELAY tokens/second.
    """
    parts = ["Streaming from the mock upstream.\n\n"]
    if STREAM_CODE:
        parts.append(CODE_BLOCK)
    parts.extend(itertools.islice(itertools.cycle(SENTENCES), n))
    text = " ".join(parts)
    return re.findall(r"\S+\s*", text)[:n]


def message_text(m):
    content = m.get("content", "")
    if isinstance(content, list):
        content = "".join(p.get("text", "") for p in content if isinstance(p, dict))
    return content


def echo_pieces(messages):
    """ECHO=1: what the last user message said, or — after a trailing assistant
    message — a continuation of it."""
    if messages and messages[-1].get("role") == "assistant":
        return re.findall(r"\S+\s*", "…and then some more words.")
    last = next((message_text(m) for m in reversed(messages) if m.get("role") == "user"), "")
    if '<context source="knowledge"' in last:
        # A Chat turn with retrieved excerpts (auto mode): answer the question
        # and cite the first excerpt, plus one number that has no excerpt, so
        # the citation badges and the "out of range stays text" rule both show.
        last = last.rsplit("</context>", 1)[-1].strip()
        return re.findall(r"\S+\s*", f"Echo: {last} (from the notes [1], and [9]; `code[1]` stays)")
    return re.findall(r"\S+\s*", f"Echo: {last}")


class Handler(BaseHTTPRequestHandler):
    # Chunked framing, so a streamed answer is delimited by the protocol rather
    # than by closing the socket — the client sees the same shape a real
    # upstream sends.
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def _json(self, code, body):
        raw = json.dumps(body).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def _binary(self, code, content_type, data):
        self.send_response(code)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def _chunk(self, data):
        self.wfile.write(b"%x\r\n" % len(data) + data + b"\r\n")
        self.wfile.flush()

    def _sse(self, obj):
        self._chunk(b"data: " + json.dumps(obj).encode() + b"\n\n")

    def do_GET(self):
        if self.path.endswith("/models"):
            self._json(200, {"object": "list", "data": [{"id": "triage", "object": "model"}]})
        else:
            self._json(404, {"error": {"message": "not found"}})

    # --- api-reference tester compat routes (api-docs design §7.5) --------
    # Matched on the path, ahead of the classifier/chat shapes below, which
    # never look at the path and would otherwise treat any of these bodies
    # as a chat completion.

    def tokenize(self, req):
        ids, pieces = fake_tokenize(req.get("content", ""), bool(req.get("add_special")))
        tokens = [{"id": i, "piece": p} for i, p in zip(ids, pieces)] if req.get("with_pieces") else ids
        self._json(200, {"tokens": tokens})

    def apply_template(self, req):
        self._json(200, {"prompt": fake_prompt(req.get("messages", []))})

    def embeddings(self, req):
        inp = req.get("input", "")
        items = inp if isinstance(inp, list) else [inp]
        data = [{"object": "embedding", "index": i,
                 "embedding": fake_embedding(t, EMBED_DIMS) if EMBED_DIMS else [0.1, 0.2, 0.3]}
                for i, t in enumerate(items)]
        self._json(200, {
            "object": "list",
            "data": data,
            "model": req.get("model", "triage"),
            "usage": {"prompt_tokens": len(items), "total_tokens": len(items)},
        })

    def audio_speech(self, req):
        self._binary(200, "audio/wav", silent_wav())

    def do_POST(self):
        n = int(self.headers.get("Content-Length", 0))
        raw = self.rfile.read(n) or b"{}"
        path = self.path.split("?", 1)[0]

        routes = {
            "/tokenize": self.tokenize,
            "/apply-template": self.apply_template,
            "/embeddings": self.embeddings,
            "/audio/speech": self.audio_speech,
        }
        route = next((fn for suffix, fn in routes.items() if path.endswith(suffix)), None)
        if route:
            route(json.loads(raw))
            return

        req = json.loads(raw)

        if MODE == "down":
            body = b"proxy error: Could not establish connection"
            self.send_response(500)
            self.send_header("Content-Type", "text/plain")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return

        if req.get("stream"):
            self.stream_chat(req)
            return

        if DELAY:
            time.sleep(DELAY)
        schema = req.get("response_format", {}).get("json_schema", {})
        if schema:
            user = "".join(
                m.get("content", "") for m in req.get("messages", []) if m.get("role") == "user"
            )
            allowed = schema.get("schema", {}).get("properties", {}).get("category", {}).get(
                "enum", []
            )
            content = json.dumps({"category": classify(user, allowed)})
            completion_tokens = 6
        else:
            pieces = answer_pieces(STREAM_TOKENS)
            content = "".join(pieces)
            completion_tokens = len(pieces)
        self._json(200, {
            "id": "chatcmpl-mock",
            "object": "chat.completion",
            "model": req.get("model", "triage"),
            "choices": [{
                "index": 0,
                "finish_reason": "stop",
                "message": {"role": "assistant", "content": content},
            }],
            "usage": {
                "prompt_tokens": 420,
                "completion_tokens": completion_tokens,
                "total_tokens": 420 + completion_tokens,
            },
        })

    def stream_chat(self, req):
        model = req.get("model", "triage")
        include_usage = bool(req.get("stream_options", {}).get("include_usage"))
        pieces = echo_pieces(req.get("messages", [])) if ECHO else answer_pieces(STREAM_TOKENS)

        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Transfer-Encoding", "chunked")
        self.end_headers()

        def frame(delta, finish=None, usage=None):
            out = {
                "id": "chatcmpl-mock",
                "object": "chat.completion.chunk",
                "model": model,
                "choices": [] if usage else [
                    {"index": 0, "delta": delta, "finish_reason": finish}
                ],
            }
            if usage:
                out["usage"] = usage
            return out

        try:
            self._sse(frame({"role": "assistant", "content": ""}))
            for piece in pieces:
                if STREAM_DELAY:
                    time.sleep(STREAM_DELAY)
                self._sse(frame({"content": piece}))
            self._sse(frame({}, finish="stop"))
            if include_usage:
                self._sse(frame({}, usage={
                    "prompt_tokens": 420,
                    "completion_tokens": len(pieces),
                    "total_tokens": 420 + len(pieces),
                }))
            self._chunk(b"data: [DONE]\n\n")
            self.wfile.write(b"0\r\n\r\n")
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass  # client hit Stop / went away mid-stream


port = int(sys.argv[1])
print(
    f"mock-openai on 127.0.0.1:{port} (mode={MODE}, delay={DELAY}, "
    f"stream_delay={STREAM_DELAY}, stream_tokens={STREAM_TOKENS})",
    flush=True,
)
ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()
