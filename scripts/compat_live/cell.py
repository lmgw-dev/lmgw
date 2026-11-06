"""One matrix cell (scenario x mode x route x model): sends turns, applies the rules, keeps
the evidence."""

import json

from . import classify, rules_errors, sdk_check, sse
from .common import FAIL, MODEL, PASS, Outcome, excerpt, redact
from .transport import sdk_send

DEFAULT_MAX_TOKENS = 1024


class Cell:
    def __init__(self, route, model, stream, kit, temperature, secrets, reasoning_kind=""):
        self.route, self.model, self.stream, self.kit = route, model, stream, kit
        self.reasoning_kind = reasoning_kind  # the card's reasoning kind: levels | toggle | ""
        self.temperature = temperature
        self.secrets = secrets
        self.violations, self.model_notes, self.notes = [], [], []
        self.usage = {"in": 0, "out": 0}
        self.turns = 0

    # --- sending -------------------------------------------------------------
    def call(self, st, **o):
        o.setdefault("max_tokens", DEFAULT_MAX_TOKENS)
        o.setdefault("temperature", self.temperature)
        b = self.route.body(st, self.model, self.stream, o)
        self.turns += 1
        ex, err = sdk_send(self.kit, self.route.NAME, b)
        # A beta answer is judged by the SDK's beta types, which know its blocks.
        return self._read(ex, err, self.stream, beta=bool(o.get("betas")))

    def raw(self, path, payload, dialect=None):
        """A request without the SDK; returns the Exchange (status not judged)."""
        self.turns += 1
        return self.kit.raw(path, payload, dialect or self.route.DIALECT)

    def sdk(self, kind, body):
        """An SDK call of another kind (messages_count); returns (Exchange, sdk_error)."""
        self.turns += 1
        return sdk_send(self.kit, kind, body)

    def sdk_schema(self, kind, data):
        return sdk_check.body(kind, data)

    def _read(self, ex, err, stream, beta=False):
        step = f"turn {self.turns}"
        kind = self.route.NAME + (".beta" if beta else "")
        if ex.status != 200:
            self.error_shape(ex)
            status, reason = classify.classify(ex.status, ex.resp_body)
            if status == FAIL:
                self.violations.append(f"{step}: http: {reason}")
            raise Outcome(status, f"{step}: {reason}")
        ct = ex.content_type()
        if stream:
            self.expect("text/event-stream" in ct, "content_type", f"{step}: streamed answer has content-type {ct!r}")
            events = sse.parse(ex.resp_body)
            found = self.route.check_stream(events) + sdk_check.stream(kind, events)
            turn = self.route.parse_stream(events)
        else:
            self.expect("application/json" in ct, "content_type", f"{step}: answer has content-type {ct!r}")
            try:
                data = json.loads(ex.resp_body)
            except ValueError:
                self.violations.append(f"{step}: body.json: not JSON: {ex.resp_body[:200]!r}")
                raise Outcome(FAIL, f"{step}: the body is not JSON")
            found = self.route.check(data) + sdk_check.body(kind, data)
            turn = self.route.parse(data)
        if err:
            found.append(f"sdk.exception: {err}")
        self.violations += [f"{step}: {v}" for v in found]
        for k in ("in", "out"):
            self.usage[k] += int(turn.usage.get(k) or 0)
        for h, v in ex.lmgw_headers().items():
            self.notes.append(f"{step}: {h}: {v}")
        return turn

    def error_shape(self, ex):
        check = rules_errors.anthropic if self.route.DIALECT == "anthropic" else rules_errors.openai
        found = check(ex.resp_body, ex.content_type())
        if self.route.DIALECT == "anthropic":
            found += rules_errors.anthropic_type_for(ex.status, ex.resp_body)
        self.violations += [f"turn {self.turns}: {v}" for v in found]

    # --- judging -------------------------------------------------------------
    def expect(self, cond, rule, detail):
        if not cond:
            self.violations.append(f"{rule}: {detail}")
        return bool(cond)

    def soft(self, cond, detail):
        """A semantic check: a miss is model behaviour, not an lmgw failure."""
        if not cond:
            self.model_notes.append(detail)
        return bool(cond)

    def need_calls(self, turn, n=1, name=None):
        calls = [c for c in turn.calls if name is None or c.name == name]
        if len(calls) < n:
            raise Outcome(MODEL, f"the model made {len(calls)} matching tool call(s), the scenario needs {n}"
                          f"{' of ' + name if name else ''} (text: {turn.text[:120]!r})")
        return calls

    def finish(self, turn, *allowed):
        self.expect(turn.finish in allowed, "finish_reason",
                    f"turn {self.turns}: finish {turn.finish_raw!r} maps to {turn.finish!r}, expected {list(allowed)}")

    def status(self):
        if self.violations:
            return FAIL
        if self.model_notes:
            return MODEL
        return PASS

    def evidence(self):
        out = []
        for ex in self.kit.rec.log:
            out.append({
                "request": f"{ex.method} {ex.url}",
                "request_body": excerpt(redact(ex.req_body, self.secrets)),
                "status": ex.status,
                "seconds": ex.seconds,
                "lmgw_headers": ex.lmgw_headers(),
                "response_body": excerpt(redact(ex.resp_body, self.secrets)),
            })
        return out
