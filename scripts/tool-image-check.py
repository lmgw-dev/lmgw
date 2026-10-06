#!/usr/bin/env python3
# Usage: scripts/tool-image-check.py --model ALIAS [--base URL] [--runs N] [--baseline] [--svg] [--json]
#        scripts/tool-image-check.py --selftest
"""The live probe for tool-result images (llama-egress design §8.4): can a
model read an image that reaches it inside a tool result?

Each run draws its own PNG with the standard library: a random six-digit number
in a built-in bitmap font (168 px digits, black on white) and a coloured shape
beside it. It sends `POST {base}/v1/messages` in the Anthropic shape at
temperature 0: a user turn, the assistant's `tool_use` of a `snapshot` tool,
and a user turn whose `tool_result` carries "Snapshot taken." and the image,
followed by the question what six-digit number the snapshot shows.

- `--baseline` sends the same conversation with the image moved out of the
  tool result into the user turn as a plain image, so the two prompts differ
  only in where the image sits and their input tokens compare directly.
- `--svg` sends an SVG drawing of the same thing (`image/svg+xml`), for the
  placeholder check: llama.cpp cannot decode one, so it must never reach it.

Per run it prints the number drawn, the model's answer, whether the answer
contains the number and `usage.input_tokens` (plus the cache counters when the
gateway reports them); then one summary line (matches/N, mean input tokens).
`--json` prints the same as one JSON document instead.

The design's rules for the probe: run it on a dev copy (scripts/dev-copy.sh
serves on 127.0.0.1:8899, the default; 8001, the app's, is refused), on a
local vision row, never a cloud model; never switch the GPU hold. A 503 `gpu_hold` stops the probe
rather than retrying, and so does the first answer with a non-empty
`x-lmgw-fallback` header: a fallback alias answered, possibly a cloud model,
and the probe never sends it another request. A bearer for gateway auth comes
from $LMGW_KEY.

`--selftest` needs no server: it writes one PNG/SVG pair to
target/tool-image-check/, validates the PNG (signature, IHDR, every chunk's
CRC, the IDAT stream's length after zlib) and reads the digits back out of
the decoded pixels, parses the SVG, and checks the request bodies and the
answer matching.
"""
import argparse
import json
import os
import random
import re
import statistics
import struct
import sys
import urllib.error
import urllib.parse
import urllib.request
import xml.etree.ElementTree as ET
import zlib
from base64 import b64encode
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SELFTEST_DIR = ROOT / "target" / "tool-image-check"

# 5×7 digits, one string per row, "1" lit.
FONT = {
    "0": ("01110", "10001", "10011", "10101", "11001", "10001", "01110"),
    "1": ("00100", "01100", "00100", "00100", "00100", "00100", "01110"),
    "2": ("01110", "10001", "00001", "00010", "00100", "01000", "11111"),
    "3": ("11111", "00010", "00100", "00010", "00001", "10001", "01110"),
    "4": ("00010", "00110", "01010", "10010", "11111", "00010", "00010"),
    "5": ("11111", "10000", "11110", "00001", "00001", "10001", "01110"),
    "6": ("00110", "01000", "10000", "11110", "10001", "10001", "01110"),
    "7": ("11111", "00001", "00010", "00100", "01000", "01000", "01000"),
    "8": ("01110", "10001", "10001", "01110", "10001", "10001", "01110"),
    "9": ("01110", "10001", "10001", "01111", "00001", "00010", "01100"),
}
CELL = 24  # px per font cell: digits are 120 × 168 px, strokes 24 px wide
GAP = 2 * CELL  # between digits
MARGIN = 144
SHAPE = 288  # the shape's bounding box, px
WIDTH = MARGIN + 6 * 5 * CELL + 5 * GAP + MARGIN + SHAPE + MARGIN
HEIGHT = 2 * MARGIN + SHAPE
INK = (0, 0, 0)
PAPER = (255, 255, 255)
COLOURS = {
    "red": (220, 40, 40),
    "blue": (40, 90, 220),
    "green": (30, 150, 60),
    "orange": (240, 140, 0),
    "purple": (140, 60, 200),
}
SHAPES = ("circle", "square", "triangle")

QUESTION = "What six-digit number does the snapshot show? Answer with the six digits only."
TOOL = {
    "name": "snapshot",
    "description": "Takes a snapshot of the screen and returns it as an image.",
    "input_schema": {"type": "object", "properties": {}},
}


# ---------------------------------------------------------------------------
# Drawing
# ---------------------------------------------------------------------------

def digit_origin(i):
    """Top-left corner of digit i."""
    top = (HEIGHT - 7 * CELL) // 2
    return MARGIN + i * (5 * CELL + GAP), top


def shape_box():
    left = WIDTH - MARGIN - SHAPE
    return left, MARGIN


def inside_shape(shape, x, y):
    """Whether pixel (x, y) is inside the shape, in its box's coordinates."""
    if shape == "square":
        return 8 <= x < SHAPE - 8 and 8 <= y < SHAPE - 8
    if shape == "circle":
        c, r = SHAPE / 2, SHAPE / 2 - 4
        return (x + 0.5 - c) ** 2 + (y + 0.5 - c) ** 2 <= r * r
    # triangle, apex up: half width grows linearly with y
    half = (y + 0.5) / SHAPE * (SHAPE / 2 - 4)
    return 4 <= y < SHAPE - 4 and abs(x + 0.5 - SHAPE / 2) <= half


def raster(number, shape, colour):
    """Rows of RGB pixels, as a list of bytearrays."""
    rows = [bytearray(bytes(PAPER) * WIDTH) for _ in range(HEIGHT)]

    def put(x, y, rgb):
        rows[y][3 * x:3 * x + 3] = bytes(rgb)

    for i, d in enumerate(number):
        ox, oy = digit_origin(i)
        for r, line in enumerate(FONT[d]):
            for c, bit in enumerate(line):
                if bit == "1":
                    for y in range(oy + r * CELL, oy + (r + 1) * CELL):
                        for x in range(ox + c * CELL, ox + (c + 1) * CELL):
                            put(x, y, INK)
    sx, sy = shape_box()
    rgb = COLOURS[colour]
    for y in range(SHAPE):
        for x in range(SHAPE):
            if inside_shape(shape, x, y):
                put(sx + x, sy + y, rgb)
    return rows


def chunk(kind, data):
    return (struct.pack(">I", len(data)) + kind + data
            + struct.pack(">I", zlib.crc32(kind + data) & 0xFFFFFFFF))


def png(number, shape, colour):
    rows = raster(number, shape, colour)
    raw = b"".join(b"\x00" + bytes(r) for r in rows)  # filter 0 on every row
    ihdr = struct.pack(">IIBBBBB", WIDTH, HEIGHT, 8, 2, 0, 0, 0)  # 8-bit RGB
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", ihdr)
            + chunk(b"IDAT", zlib.compress(raw, 9)) + chunk(b"IEND", b""))


def svg(number, shape, colour):
    """The same picture as SVG: one rect per run of lit cells."""
    out = [f'<svg xmlns="http://www.w3.org/2000/svg" width="{WIDTH}" height="{HEIGHT}" '
           f'viewBox="0 0 {WIDTH} {HEIGHT}">',
           f'<rect width="{WIDTH}" height="{HEIGHT}" fill="#ffffff"/>']
    for i, d in enumerate(number):
        ox, oy = digit_origin(i)
        for r, line in enumerate(FONT[d]):
            for m in re.finditer("1+", line):
                out.append(f'<rect x="{ox + m.start() * CELL}" y="{oy + r * CELL}" '
                           f'width="{len(m.group()) * CELL}" height="{CELL}" fill="#000000"/>')
    sx, sy = shape_box()
    fill = "#%02x%02x%02x" % COLOURS[colour]
    if shape == "square":
        out.append(f'<rect x="{sx + 8}" y="{sy + 8}" width="{SHAPE - 16}" '
                   f'height="{SHAPE - 16}" fill="{fill}"/>')
    elif shape == "circle":
        out.append(f'<circle cx="{sx + SHAPE / 2}" cy="{sy + SHAPE / 2}" '
                   f'r="{SHAPE / 2 - 4}" fill="{fill}"/>')
    else:
        out.append(f'<polygon points="{sx + SHAPE / 2},{sy + 4} {sx + 4},{sy + SHAPE - 4} '
                   f'{sx + SHAPE - 4},{sy + SHAPE - 4}" fill="{fill}"/>')
    out.append("</svg>")
    return "\n".join(out).encode()


# ---------------------------------------------------------------------------
# Checking what was drawn
# ---------------------------------------------------------------------------

def check_png(data):
    """Validate a PNG this script writes; return (width, height, rows of RGB)."""
    if data[:8] != b"\x89PNG\r\n\x1a\n":
        raise ValueError("bad PNG signature")
    pos, kinds, idat, ihdr = 8, [], b"", None
    while pos < len(data):
        if pos + 12 > len(data):
            raise ValueError(f"truncated chunk at byte {pos}")
        (length,) = struct.unpack(">I", data[pos:pos + 4])
        kind = data[pos + 4:pos + 8]
        body = data[pos + 8:pos + 8 + length]
        if len(body) != length or pos + 12 + length > len(data):
            raise ValueError(f"truncated {kind!r} chunk")
        (crc,) = struct.unpack(">I", data[pos + 8 + length:pos + 12 + length])
        if zlib.crc32(kind + body) & 0xFFFFFFFF != crc:
            raise ValueError(f"CRC mismatch in {kind!r}")
        kinds.append(kind)
        if kind == b"IHDR":
            ihdr = body
        elif kind == b"IDAT":
            idat += body
        pos += 12 + length
    if not kinds or kinds[0] != b"IHDR" or ihdr is None or len(ihdr) != 13:
        raise ValueError("IHDR is not the first chunk, or not 13 bytes")
    if kinds[-1] != b"IEND" or b"IDAT" not in kinds:
        raise ValueError(f"chunk order {kinds}")
    w, h, depth, ctype, comp, filt, interlace = struct.unpack(">IIBBBBB", ihdr)
    if (depth, ctype, comp, filt, interlace) != (8, 2, 0, 0, 0) or not w or not h:
        raise ValueError(f"IHDR {w}x{h} depth {depth} type {ctype}: not 8-bit RGB")
    raw = zlib.decompress(idat)
    stride = 1 + 3 * w
    if len(raw) != h * stride:
        raise ValueError(f"IDAT holds {len(raw)} bytes, {h * stride} expected")
    rows = []
    for y in range(h):
        line = raw[y * stride:(y + 1) * stride]
        if line[0] != 0:
            raise ValueError(f"row {y} uses filter {line[0]}; this script writes 0 only")
        rows.append(line[1:])
    return w, h, rows


def read_digits(rows):
    """The number, read back from the pixels by sampling each font cell."""
    out = ""
    for i in range(6):
        ox, oy = digit_origin(i)
        cells = tuple(
            "".join("1" if rows[oy + r * CELL + CELL // 2][3 * (ox + c * CELL + CELL // 2)] < 128
                    else "0" for c in range(5))
            for r in range(7))
        found = [d for d, glyph in FONT.items() if glyph == cells]
        if len(found) != 1:
            raise ValueError(f"digit {i} reads as no glyph: {cells}")
        out += found[0]
    return out


def matches(number, answer):
    """Whether the answer states the number: digit groups joined first, so
    "482 913" and "482,913" count, and "4829130" does not."""
    joined = re.sub(r"(?<=\d)[\s,.'’_-](?=\d)", "", answer)
    return re.search(rf"(?<!\d){number}(?!\d)", joined) is not None


# ---------------------------------------------------------------------------
# The request
# ---------------------------------------------------------------------------

def body(model, media_type, data_b64, baseline, max_tokens, no_thinking):
    image = {"type": "image",
             "source": {"type": "base64", "media_type": media_type, "data": data_b64}}
    result = [{"type": "text", "text": "Snapshot taken."}]
    last = [{"type": "tool_result", "tool_use_id": "toolu_snapshot_1",
             "content": result if baseline else result + [image]}]
    if baseline:
        last.append(image)
    last.append({"type": "text", "text": QUESTION})
    b = {
        "model": model,
        "max_tokens": max_tokens,
        "temperature": 0,
        "stream": False,
        "tools": [TOOL],
        "messages": [
            {"role": "user", "content": "Take a snapshot, then read me the number on it."},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_snapshot_1", "name": "snapshot",
                 "input": {}}]},
            {"role": "user", "content": last},
        ],
    }
    if no_thinking:
        b["thinking"] = {"type": "disabled"}
    return b


def post(base, payload, key, timeout):
    url = base + "/v1/messages"
    headers = {"content-type": "application/json", "anthropic-version": "2023-06-01"}
    if key:
        headers["authorization"] = f"Bearer {key}"
    req = urllib.request.Request(url, data=json.dumps(payload).encode(), method="POST",
                                 headers=headers)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return json.load(r), r.headers.get("x-lmgw-fallback")
    except urllib.error.HTTPError as e:
        text = e.read().decode(errors="replace")
        hint = ""
        if e.code == 503 and '"gpu_hold"' in text:
            hint = (" (the GPU hold is on: local models are paused on purpose; "
                    "this probe never switches it)")
        raise SystemExit(f"HTTP {e.code} from {url}{hint}: {text}") from None
    except urllib.error.URLError as e:
        raise SystemExit(f"cannot reach {url}: {e.reason}") from None


def stop_on_fallback(fallback, model):
    """End the probe at the first answer a fallback alias gave: it may be a
    cloud model, and the probe never keeps talking to one."""
    if fallback:
        raise SystemExit(
            f"stopped: x-lmgw-fallback says '{fallback}' answered instead of {model}. A fallback "
            "alias may be a cloud model, and this probe only talks to a local vision row: make "
            "sure that row can load (the GPU hold may be on; this probe never switches it), then "
            "run it again. Nothing from that answer was used.")


def answer_text(resp):
    return "".join(b.get("text", "") for b in resp.get("content") or []
                   if b.get("type") == "text")


def run(a):
    base = a.base.rstrip("/")
    if base.endswith("/v1"):
        base = base[:-3]
    key = os.environ.get("LMGW_KEY") or None
    rng = random.Random(a.seed)
    fmt = "svg" if a.svg else "png"
    mode = "baseline" if a.baseline else "tool_result"
    if not a.json:
        print(f"{mode}, {fmt}: {a.runs} run(s) of POST {base}/v1/messages, model {a.model}, "
              f"temperature 0, max_tokens {a.max_tokens}"
              f"{', thinking disabled' if a.no_thinking else ''}", flush=True)
    runs = []
    for i in range(a.runs):
        number = str(rng.randint(100000, 999999))
        shape, colour = rng.choice(SHAPES), rng.choice(sorted(COLOURS))
        if a.svg:
            media, data = "image/svg+xml", svg(number, shape, colour)
        else:
            media, data = "image/png", png(number, shape, colour)
        payload = body(a.model, media, b64encode(data).decode(), a.baseline, a.max_tokens,
                       a.no_thinking)
        resp, fallback = post(base, payload, key, a.timeout)
        stop_on_fallback(fallback, a.model)
        usage = resp.get("usage") or {}
        answer = answer_text(resp)
        r = {
            "run": i + 1,
            "number": number,
            "shape": f"{colour} {shape}",
            "image_bytes": len(data),
            "answer": answer,
            "matched": matches(number, answer),
            "stop_reason": resp.get("stop_reason"),
            "input_tokens": usage.get("input_tokens"),
            "cache_read_input_tokens": usage.get("cache_read_input_tokens"),
            "cache_creation_input_tokens": usage.get("cache_creation_input_tokens"),
        }
        r["total_input_tokens"] = (None if r["input_tokens"] is None else
                                   r["input_tokens"] + (r["cache_read_input_tokens"] or 0)
                                   + (r["cache_creation_input_tokens"] or 0))
        runs.append(r)
        if not a.json:
            cache = ""
            if r["cache_read_input_tokens"] is not None or r["cache_creation_input_tokens"]:
                cache = (f" (+{r['cache_read_input_tokens'] or 0} cache read, "
                         f"+{r['cache_creation_input_tokens'] or 0} cache write "
                         f"= {r['total_input_tokens']})")
            print(f"run {i + 1}/{a.runs}: drew {number} ({r['shape']}) | answer "
                  f"{json.dumps(answer, ensure_ascii=False)} | "
                  f"{'MATCH' if r['matched'] else 'no match'} | "
                  f"input_tokens {r['input_tokens']}{cache} | stop {r['stop_reason']}",
                  flush=True)
    summary = summarise(runs)
    if a.json:
        print(json.dumps({"mode": mode, "format": fmt, "model": a.model, "base": base,
                          "max_tokens": a.max_tokens, "thinking_disabled": a.no_thinking,
                          "seed": a.seed, "runs": runs, "summary": summary},
                         indent=1, ensure_ascii=False))
    else:
        print(f"summary: {mode}, {fmt}, {a.model}: {summary['matches']}/{summary['runs']} "
              f"matched, mean input_tokens {summary['mean_input_tokens']}, "
              f"mean total input {summary['mean_total_input_tokens']}")


def summarise(runs):
    def mean(key):
        vals = [r[key] for r in runs if r[key] is not None]
        return round(statistics.mean(vals), 1) if vals else None
    return {"runs": len(runs), "matches": sum(r["matched"] for r in runs),
            "mean_input_tokens": mean("input_tokens"),
            "mean_total_input_tokens": mean("total_input_tokens")}


# ---------------------------------------------------------------------------
# Self-test
# ---------------------------------------------------------------------------

def expect(cond, detail=None):
    """An assert that `python -O` cannot strip."""
    if not cond:
        raise SystemExit(f"selftest failed{': ' + repr(detail) if detail is not None else ''}")


def selftest(seed):
    rng = random.Random(seed)
    number = str(rng.randint(100000, 999999))
    shape, colour = rng.choice(SHAPES), rng.choice(sorted(COLOURS))
    SELFTEST_DIR.mkdir(parents=True, exist_ok=True)
    png_path, svg_path = SELFTEST_DIR / "selftest.png", SELFTEST_DIR / "selftest.svg"
    data = png(number, shape, colour)
    png_path.write_bytes(data)
    svg_path.write_bytes(svg(number, shape, colour))

    w, h, rows = check_png(png_path.read_bytes())
    expect((w, h) == (WIDTH, HEIGHT), (w, h))
    expect(read_digits(rows) == number, (read_digits(rows), number))
    sx, sy = shape_box()
    centre = rows[sy + SHAPE // 2][3 * (sx + SHAPE // 2):3 * (sx + SHAPE // 2) + 3]
    expect(tuple(centre) == COLOURS[colour], (tuple(centre), colour))
    # Every glyph is distinct, and every digit reads back from a picture of it.
    every = "".join(FONT)
    for k in range(0, 10, 6):
        probe = (every[k:] + every)[:6]
        expect(read_digits(check_png(png(probe, "circle", "red"))[2]) == probe)
    # One flipped byte inside the IDAT data must fail its CRC.
    idat = 8 + 25 + 8  # signature, the IHDR chunk, the IDAT chunk's length and type
    bad = bytearray(data)
    bad[idat + 10] ^= 0xFF
    try:
        check_png(bytes(bad))
        raise SystemExit("selftest: a corrupted IDAT passed the CRC check")
    except ValueError:
        pass

    root = ET.fromstring(svg_path.read_bytes())
    expect(root.tag == "{http://www.w3.org/2000/svg}svg", root.tag)
    expect((root.get("width"), root.get("height")) == (str(WIDTH), str(HEIGHT)))
    lit = sum(line.count("1") for d in number for line in FONT[d])
    rects = [e for e in root if e.tag.endswith("rect")]
    covered = sum(int(e.get("width")) * int(e.get("height")) for e in rects[1:]
                  if e.get("fill") == "#000000")
    expect(covered == lit * CELL * CELL, (covered, lit))

    tool = body("m", "image/png", "AAAA", False, 64, False)
    base = body("m", "image/png", "AAAA", True, 64, False)
    tr = tool["messages"][2]["content"]
    expect([b["type"] for b in tr] == ["tool_result", "text"])
    expect([b["type"] for b in tr[0]["content"]] == ["text", "image"])
    br = base["messages"][2]["content"]
    expect([b["type"] for b in br] == ["tool_result", "image", "text"])
    expect([b["type"] for b in br[0]["content"]] == ["text"])
    expect(tool["messages"][:2] == base["messages"][:2] and tool["tools"] == base["tools"])
    expect(tool["temperature"] == 0 and "thinking" not in tool)
    expect(body("m", "image/png", "AAAA", False, 64, True)["thinking"] == {"type": "disabled"})

    for answer, want in [("482913", True), ("The number is 482 913.", True),
                         ("482,913", True), ("4829130", False), ("48291", False),
                         ("1482913", False), ("", False)]:
        expect(matches("482913", answer) is want, (answer, want))
    expect(answer_text({"content": [{"type": "thinking", "thinking": "482913?"},
                                    {"type": "text", "text": "123456"}]}) == "123456")
    # A fallback's answer ends the probe; no header, or an empty one, does not.
    for header in (None, ""):
        stop_on_fallback(header, "m")
    try:
        stop_on_fallback("cloud-chat", "m")
        raise SystemExit("selftest: a fallback answer did not stop the probe")
    except SystemExit as e:
        expect(str(e).startswith("stopped: x-lmgw-fallback says 'cloud-chat'"), str(e))

    print(f"selftest ok: drew {number} ({colour} {shape}), {WIDTH}x{HEIGHT}, "
          f"{len(data)} bytes, digits {7 * CELL} px tall")
    print(f"  {png_path.relative_to(ROOT)}")
    print(f"  {svg_path.relative_to(ROOT)}")


def main():
    ap = argparse.ArgumentParser(
        description="Live probe: can a model read an image inside a tool result?")
    ap.add_argument("--model", help="the alias to ask (required unless --selftest)")
    ap.add_argument("--base", default="http://127.0.0.1:8899",
                    help="a dev copy's root (default %(default)s; 8001, the app's, is refused)")
    ap.add_argument("--runs", type=int, default=5, help="default %(default)s")
    ap.add_argument("--baseline", action="store_true",
                    help="send the image as a plain user image instead of in the tool result")
    ap.add_argument("--svg", action="store_true",
                    help="send an SVG image (the placeholder check) instead of a PNG")
    ap.add_argument("--json", action="store_true", help="print one JSON document")
    ap.add_argument("--seed", type=int, help="seed the numbers and shapes (default: random)")
    ap.add_argument("--max-tokens", type=int, default=512,
                    help="the request's max_tokens (default %(default)s)")
    ap.add_argument("--no-thinking", action="store_true",
                    help='send thinking: {"type": "disabled"}')
    ap.add_argument("--timeout", type=float, default=600,
                    help="seconds per request, a cold model load included (default %(default)s)")
    ap.add_argument("--selftest", action="store_true",
                    help="no server: write and check one PNG/SVG pair under target/")
    a = ap.parse_args()
    if a.selftest:
        selftest(a.seed)
        return
    if not a.model:
        ap.error("--model is required")
    if a.runs < 1:
        ap.error("--runs must be at least 1")
    url = urllib.parse.urlsplit(a.base)
    if url.hostname != "127.0.0.1" or url.port in (None, 8001):
        raise SystemExit(f"refusing: {a.base} is not a dev copy on 127.0.0.1 (8001 is the app's)")
    run(a)


if __name__ == "__main__":
    sys.exit(main())
