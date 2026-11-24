"""The prompt_cache scenario's pure parts: the marked prefix and the judgement of two answers.

The scenario sends one request with a marked system prefix twice in a row on /v1/messages:
the second answer has to read the cache the first wrote (or, when another cell wrote it a
moment before, read too). No network here, so compat_live_test.py checks it offline."""

# Anthropic caches a prefix only from a model-specific minimum length on (1,024 to 4,096
# tokens); 5,000 clears every current model's.
MIN_PREFIX_TOKENS = 5000
# A token of this English text is about four characters; five per token leaves a margin, so
# the prefix stays above the minimum on any tokenizer.
CHARS_PER_TOKEN = 5

IGNORED_HEADER = "x-lmgw-params-ignored"


def prefix(tokens=MIN_PREFIX_TOKENS, label="fixture"):
    """A deterministic system text of at least `tokens` tokens: the same bytes on every run,
    so a second request hits what the first wrote. A different `label` is a different text,
    so a scenario has a cache entry of its own."""
    lines, size, n = [], 0, 0
    while size < tokens * CHARS_PER_TOKEN:
        n += 1
        line = (f"Rule {n:04d} of the {label} manual: the clerk files form {n} in drawer {n % 17} "
                f"and stamps it with the number {n * 7 % 1000}.")
        lines.append(line)
        size += len(line) + 1
    return "You are a test fixture. The manual below is reference text only.\n" + "\n".join(lines)


def ignores_cache_control(header):
    """Whether an x-lmgw-params-ignored value names cache_control: the route is not
    Anthropic-protocol, so nothing can be cached and the cell is skipped."""
    return "cache_control" in [x.strip() for x in (header or "").split(",")]


def _n(usage, key):
    v = (usage or {}).get(key)
    return v if isinstance(v, int) and not isinstance(v, bool) else 0


def judge(first, second, starts=None, one_hour=False):
    """Violations [(rule, detail)] of two answers' usage objects. `starts` are the two
    streamed answers' message_start.message.usage objects (None for a whole answer): each has
    to show the request's input already, as Anthropic's own message_start does. With
    `one_hour` the first answer has to show the 1-hour write (usage.cache_creation) or a read
    (another cell wrote it a moment before)."""
    out = []
    read1, wrote1 = _n(first, "cache_read_input_tokens"), _n(first, "cache_creation_input_tokens")
    if not (read1 > 0 or wrote1 > 0):
        out.append(("cache.first", f"the first answer neither wrote nor read the cache: usage {first!r}"))
    if one_hour and read1 <= 0 and _n((first or {}).get("cache_creation"), "ephemeral_1h_input_tokens") <= 0:
        out.append(("cache.ttl", "the first answer shows no 1-hour write in usage.cache_creation "
                                 f"and read nothing: usage {first!r}"))
    if _n(second, "cache_read_input_tokens") <= 0:
        out.append(("cache.read", f"the second answer read nothing from the cache: usage {second!r}"))
    for i, start in enumerate(starts or [], 1):
        shown = sum(_n(start, k) for k in
                    ("input_tokens", "cache_read_input_tokens", "cache_creation_input_tokens"))
        if shown <= 0:
            out.append(("cache.message_start", f"turn {i}: message_start.message.usage shows no input "
                                               f"or cache tokens: {start!r}"))
    return out
