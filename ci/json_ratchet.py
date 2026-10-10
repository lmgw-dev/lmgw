#!/usr/bin/env python3
# Usage: python3 ci/json_ratchet.py [--verbose]  (counts json!(/[/{ in non-test web/ and ops/ code vs ci/json-ratchet.toml)
"""A ratchet on inline JSON in handlers and ops.

Answers are lmgw-api-types types the handler builds, so a `json!` in
non-test code under crates/lmgw-core/src/{web,ops} is either an answer that
has no type yet or a request to another program. The count per directory may
only fall: above the baseline fails (build the answer from an api-types type),
below it fails too (lower the baseline in the same commit), so it stays tight.

Test code is not counted: files under tests/ or *_tests/ dirs, tests.rs, *_tests.rs,
tests_*.rs, modules declared `#[cfg(test)] mod x;` (also with a `#[path]`), files starting with `#![cfg(test)]`, and everything from a
`#[cfg(test)]` item on (its braces are matched).
"""
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BASELINE = ROOT / "ci" / "json-ratchet.toml"
SRC = ROOT / "crates" / "lmgw-core" / "src"
DIRS = {"web": [SRC / "web"], "ops": [SRC / "ops", SRC / "ops.rs"]}

CFG_TEST = re.compile(r"#\[cfg\(test\)\]")
INNER_CFG_TEST = re.compile(r"\A\s*(?:#!\[[^\]]*\]\s*)*#!\[cfg\(test\)\]")
OUTLINE_MOD = re.compile(
    r"#\[cfg\(test\)\]\s*(?:#\[[^\]]*\]\s*)*(?:pub(?:\([^)]*\))?\s+)?mod\s+(\w+)\s*;"
)
PATH_ATTR = re.compile(r'#\[path\s*=\s*"([^"]*)"\]')
JSON_MAC = re.compile(r"(?<!\w)(?:(?:::)?serde_json::)?json!\s*[(\[{]")


def strip_noise(text: str) -> str:
    """Blank comments, strings and char literals (keeping length and newlines)
    so braces and macro names inside them are not read as code."""
    out = []
    i, n = 0, len(text)
    while i < n:
        c = text[i]
        two = text[i : i + 2]
        if two == "//":
            j = text.find("\n", i)
            j = n if j < 0 else j
            out.append(" " * (j - i))
            i = j
        elif two == "/*":
            depth, j = 1, i + 2
            while j < n and depth:
                if text[j : j + 2] == "/*":
                    depth, j = depth + 1, j + 2
                elif text[j : j + 2] == "*/":
                    depth, j = depth - 1, j + 2
                else:
                    j += 1
            out.append(re.sub(r"[^\n]", " ", text[i:j]))
            i = j
        elif (c == "r" or two == "br") and re.match(r'b?r#*"', text[i:]) and (i == 0 or not text[i - 1].isalnum() and text[i - 1] != "_"):
            hashes = len(re.match(r"b?r(#*)", text[i:]).group(1))
            end = '"' + "#" * hashes
            j = text.find(end, i + 2 + hashes + (c == "b"))
            j = n if j < 0 else j + len(end)
            out.append(re.sub(r"[^\n]", " ", text[i:j]))
            i = j
        elif c == '"':
            j = i + 1
            while j < n and text[j] != '"':
                j += 2 if text[j] == "\\" else 1
            j = min(j + 1, n)
            out.append(re.sub(r"[^\n]", " ", text[i:j]))
            i = j
        elif c == "'":
            m = re.match(r"'(?:\\.[^']*|[^\\'])'", text[i:])
            if m:
                out.append(" " * len(m.group(0)))
                i += len(m.group(0))
            else:  # a lifetime
                out.append(c)
                i += 1
        else:
            out.append(c)
            i += 1
    return "".join(out)


def count_file(path: Path) -> int:
    raw = path.read_text()
    code = strip_noise(raw)
    if INNER_CFG_TEST.match(code):  # `#![cfg(test)]`: the whole file is test code
        return 0
    # Drop each #[cfg(test)] item: up to its matching close brace, or its `;`.
    keep, pos = [], 0
    for m in CFG_TEST.finditer(code):
        if m.start() < pos:
            continue
        keep.append(code[pos : m.start()])
        i, depth, nest = m.end(), 0, 0  # braces; (/[ nesting, so `[u8; 4]` does not end it
        while i < len(code):
            if code[i] == "{":
                depth += 1
            elif code[i] in "([":
                nest += 1
            elif code[i] in ")]":
                nest -= 1
            elif code[i] == "}":
                depth -= 1
                if depth == 0:
                    i += 1
                    break
            elif code[i] == ";" and depth == 0 and nest <= 0:
                i += 1
                break
            i += 1
        pos = i
    keep.append(code[pos:])
    return len(JSON_MAC.findall("".join(keep)))


def is_test_file(path: Path, skipped_mods: set) -> bool:
    name = path.name
    if any(d == "tests" or d.endswith("_tests") for d in path.relative_to(SRC).parts[:-1]):
        return True
    if name == "tests.rs" or name.endswith("_tests.rs") or name.startswith("tests_"):
        return True
    return path in skipped_mods


def rust_files(entries):
    for e in entries:
        if e.is_file():
            yield e
        elif e.is_dir():
            yield from sorted(e.rglob("*.rs"))


def outline_test_mods(files):
    out = set()
    for f in files:
        raw = f.read_text()
        text = strip_noise(raw)
        for m in OUTLINE_MOD.finditer(text):
            name = m.group(1)
            base = f.parent if f.name in ("mod.rs", "lib.rs", "main.rs") else f.parent / f.stem
            out.update({base / f"{name}.rs", base / name / "mod.rs"})
            # `#[path = "x.rs"]` (strings are blanked in `text`, so read the raw span)
            for pm in PATH_ATTR.finditer(raw[m.start() : m.end()]):
                out.add((f.parent / pm.group(1)).resolve())
    return out


def read_baseline():
    base = {}
    for line in BASELINE.read_text().splitlines():
        line = line.split("#", 1)[0].strip()
        if line:
            k, v = (p.strip() for p in line.split("=", 1))
            base[k] = int(v)
    return base


def main():
    verbose = "--verbose" in sys.argv[1:]
    base = read_baseline()
    bad = False
    for name, entries in DIRS.items():
        files = list(rust_files(entries))
        skipped = outline_test_mods(files)
        total = 0
        for f in files:
            if is_test_file(f, skipped):
                continue
            n = count_file(f)
            total += n
            if verbose and n:
                print(f"  {n:4d}  {f.relative_to(ROOT)}")
        want = base.get(name)
        print(f"json! in non-test {name}/: {total} (baseline {want})")
        if want is None:
            print(f"json_ratchet: no baseline for {name} in ci/json-ratchet.toml", file=sys.stderr)
            bad = True
        elif total > want:
            print(
                f"json_ratchet: {name}/ went from {want} to {total}. New inline JSON: build the "
                "answer from an lmgw-api-types type instead (see 'Typed answers' in README.md). "
                "Run with --verbose for the per-file counts.",
                file=sys.stderr,
            )
            bad = True
        elif total < want:
            print(
                f"json_ratchet: {name}/ is down to {total}: lower the baseline in "
                f"ci/json-ratchet.toml to {total} in this commit.",
                file=sys.stderr,
            )
            bad = True
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
