"""Shared types of the live check: a parsed turn, the outcomes, the rule helper, redaction."""

import json
from dataclasses import dataclass, field

# One cell's status. "model" = every protocol rule held, but the model did not do what the
# prompt asked (no tool call, a forgotten code word), so the cell says nothing about lmgw.
PASS, FAIL, UNSUPPORTED, SKIPPED, MODEL = "pass", "fail", "unsupported", "skipped", "model"
STATUSES = (PASS, FAIL, UNSUPPORTED, SKIPPED, MODEL)


@dataclass
class Call:
    id: str
    name: str
    raw_args: object  # the arguments as on the wire (a string on OpenAI, an object on Anthropic)
    args: object  # parsed: a dict when the arguments are a JSON object, else None


@dataclass
class Turn:
    text: str = ""
    calls: list = field(default_factory=list)
    finish: str = ""  # normalised: stop | length | tool | filter | other:<raw>
    finish_raw: str = ""
    stop_sequence: object = None
    usage: dict = field(default_factory=dict)  # normalised: {"in": int, "out": int}
    reasoning: bool = False  # the answer carried reasoning / thinking content
    replay: list = field(default_factory=list)  # what a client appends to its history
    response_id: str = ""


class Outcome(Exception):
    """Ends a scenario early with a status other than pass."""

    def __init__(self, status, reason):
        super().__init__(reason)
        self.status = status
        self.reason = reason


class Rules:
    """Collects rule violations as 'rule-id: detail' strings."""

    def __init__(self, prefix):
        self.prefix = prefix
        self.out = []

    def fail(self, rule, detail):
        self.out.append(f"{self.prefix}.{rule}: {detail}")

    def need(self, cond, rule, detail):
        if not cond:
            self.fail(rule, detail)
        return bool(cond)

    def typed(self, obj, key, types, rule=None, where=""):
        """The key exists in obj and its value has one of the types; returns the value."""
        name = types.__name__ if isinstance(types, type) else "/".join(t.__name__ for t in types)
        if not isinstance(obj, dict) or key not in obj:
            self.fail(rule or f"{key}.present", f"{where}{key} is missing")
            return None
        val = obj[key]
        ok = isinstance(val, types) and not (isinstance(val, bool) and bool not in _tuple(types))
        if not ok:
            self.fail(rule or f"{key}.type", f"{where}{key} should be {name}, got {_short(val)}")
            return None
        return val


def _tuple(types):
    return types if isinstance(types, tuple) else (types,)


def _short(val, n=80):
    s = json.dumps(val) if not isinstance(val, str) else repr(val)
    return s if len(s) <= n else s[: n - 3] + "..."


def json_object(text):
    """(dict, None) when text is a JSON object, else (None, why)."""
    if not isinstance(text, str):
        return None, f"not a string: {_short(text)}"
    try:
        val = json.loads(text)
    except ValueError as e:
        return None, f"not JSON ({e}): {_short(text)}"
    if not isinstance(val, dict):
        return None, f"JSON but not an object: {_short(text)}"
    return val, None


def redact(text, secrets):
    """Replace every secret in text (and its common encodings) with [redacted]."""
    if not isinstance(text, str):
        return text
    for s in secrets:
        if s:
            text = text.replace(s, "[redacted]")
    return text


def excerpt(text, limit=60000):
    if not isinstance(text, str):
        text = json.dumps(text)
    if len(text) <= limit:
        return text
    return text[: limit // 2] + f"\n... [{len(text) - limit} chars cut] ...\n" + text[-limit // 2 :]
