"""Talking to lmgw: the official SDKs over an httpx transport that records the raw wire.

Every request of a cell goes through one Recorder, so the SDK parses the answer as a real
client would (its exceptions are findings) while the rules read the exact bytes.
"""

import json
import threading
import time
import zlib
from dataclasses import dataclass, field

import anthropic
import httpx2 as httpx  # the SDKs (openai 3, anthropic 2) are built on httpx2
import openai

DROP_HEADERS = {"authorization", "x-api-key", "cookie", "set-cookie"}


class Counter:
    def __init__(self):
        self.n = 0
        self.lock = threading.Lock()

    def bump(self):
        with self.lock:
            self.n += 1


@dataclass
class Exchange:
    method: str
    url: str
    req_headers: dict
    req_body: str
    status: int = 0
    resp_headers: dict = field(default_factory=dict)
    resp_body: str = ""
    seconds: float = 0.0
    _buf: bytearray = field(default_factory=bytearray, repr=False)

    def content_type(self):
        return self.resp_headers.get("content-type", "")

    def lmgw_headers(self):
        return {k: v for k, v in self.resp_headers.items() if k.startswith("x-lmgw-")}


class _Tee(httpx.SyncByteStream):
    def __init__(self, inner, ex, t0):
        self.inner, self.ex, self.t0 = inner, ex, t0

    def __iter__(self):
        for chunk in self.inner:
            self.ex._buf.extend(chunk)
            yield chunk

    def close(self):
        try:
            self.inner.close()
        finally:
            raw = bytes(self.ex._buf)
            if self.ex.resp_headers.get("content-encoding") in ("gzip", "deflate"):
                try:
                    raw = zlib.decompress(raw, 47)
                except zlib.error:
                    pass
            self.ex.resp_body = raw.decode("utf-8", "replace")
            self.ex.seconds = round(time.monotonic() - self.t0, 3)


class Recorder(httpx.BaseTransport):
    def __init__(self, counter):
        self.inner = httpx.HTTPTransport()
        self.counter = counter
        self.log = []

    def handle_request(self, request):
        body = request.read().decode("utf-8", "replace")
        ex = Exchange(request.method, str(request.url),
                      {k: v for k, v in request.headers.items() if k.lower() not in DROP_HEADERS}, body)
        self.log.append(ex)
        self.counter.bump()
        t0 = time.monotonic()
        resp = self.inner.handle_request(request)
        ex.status = resp.status_code
        ex.resp_headers = {k.lower(): v for k, v in resp.headers.items() if k.lower() not in DROP_HEADERS}
        resp.stream = _Tee(resp.stream, ex, t0)
        return resp

    def close(self):
        self.inner.close()


class Kit:
    """One cell's clients: both SDKs and raw httpx, all on the same Recorder."""

    def __init__(self, base, key, counter, timeout):
        self.rec = Recorder(counter)
        hdr = {"accept-encoding": "identity"}
        self.http = httpx.Client(transport=self.rec, timeout=timeout, headers=hdr)
        self.openai = openai.OpenAI(base_url=base + "/v1", api_key=key, http_client=self.http,
                                    max_retries=0, timeout=timeout)
        self.anthropic = anthropic.Anthropic(base_url=base, api_key=key, http_client=self.http,
                                             max_retries=0, timeout=timeout)
        self.base, self.key = base, key

    def close(self):
        self.http.close()

    def raw(self, path, payload, dialect):
        """POST payload (a dict, or bytes sent as they are) without an SDK."""
        h = {"content-type": "application/json"}
        if dialect == "anthropic":
            h.update({"x-api-key": self.key, "anthropic-version": "2023-06-01"})
        else:
            h["authorization"] = f"Bearer {self.key}"
        data = payload if isinstance(payload, bytes) else json.dumps(payload).encode()
        self.http.post(self.base + path, content=data, headers=h).read()
        return self.rec.log[-1]


def _split(body, keep):
    kw = {k: body[k] for k in keep if k in body}
    extra = {k: v for k, v in body.items() if k not in keep and k != "stream"}
    return kw, extra


def sdk_send(kit, route, body):
    """Send body through the route's SDK call. Returns (Exchange, sdk_error or None)."""
    body = dict(body)
    betas = body.pop("_betas", None)
    # A beta request goes through the SDK's beta client, its flags as `betas=` (anthropic-beta).
    msgs = kit.anthropic.beta.messages if betas else kit.anthropic.messages
    beta_kw = {"betas": betas} if betas else {}
    stream = bool(body.get("stream"))
    before = len(kit.rec.log)
    err = None
    try:
        if route == "chat":
            kw, extra = _split(body, ("model", "messages"))
            if stream:
                with kit.openai.chat.completions.stream(**kw, extra_body=extra) as s:
                    for _ in s:
                        pass
                    s.get_final_completion()
            else:
                kit.openai.chat.completions.create(**kw, extra_body=extra)
        elif route == "messages":
            kw, extra = _split(body, ("model", "messages", "max_tokens"))
            if stream:
                with msgs.stream(**kw, **beta_kw, extra_body=extra) as s:
                    for _ in s:
                        pass
                    s.get_final_message()
            else:
                msgs.create(**kw, **beta_kw, extra_body=extra)
        elif route == "messages_count":
            kw, extra = _split(body, ("model", "messages"))
            msgs.count_tokens(**kw, **beta_kw, extra_body=extra)
        elif route == "responses":
            kw, extra = _split(body, ("model", "input"))
            if stream:
                with kit.openai.responses.stream(**kw, extra_body=extra) as s:
                    last = None
                    for event in s:
                        last = getattr(event, "type", None)
                    # The SDK sets its final response on response.completed
                    # only and raises on any other end; an incomplete or failed
                    # terminal event (what OpenAI sends when max_output_tokens
                    # cuts the answer) is the rules' to judge (TERMINAL in
                    # rules_responses.py), not an SDK error.
                    if last == "response.completed":
                        s.get_final_response()
            else:
                kit.openai.responses.create(**kw, extra_body=extra)
        else:
            raise ValueError(route)
    except (openai.APIStatusError, anthropic.APIStatusError) as e:
        err = f"status:{type(e).__name__}"
    except (openai.LengthFinishReasonError, openai.ContentFilterFinishReasonError):
        pass  # the chat stream helper's way of saying finish_reason length / content_filter
    except Exception as e:  # noqa: BLE001 - every SDK exception is a finding
        err = f"{type(e).__module__}.{type(e).__name__}: {e}"
    if len(kit.rec.log) == before:
        raise RuntimeError(f"no request reached the wire ({err})")
    return kit.rec.log[-1], err
