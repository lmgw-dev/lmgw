#!/usr/bin/env python3
"""Prove that a Rust change only moved code: nothing added, nothing lost.

The file-size sprint splits giant files into modules and must not change a
single line of logic on the way. Reviewing a 9,000-line move by eye cannot
establish that; this can. It takes every `.rs` file the change touches, before
and after, and compares them as multisets of code *segments*:

* comments are dropped and string/char literals are replaced by a hash of
  their exact contents, so nothing inside a literal can confuse what follows;
* all whitespace and trailing commas are removed, so re-indentation and
  rustfmt re-wrapping a line that got longer or shorter do not count as a
  change;
* `use …;`, `mod x;` / `mod x {` and visibility (`pub`, `pub(crate)`,
  `pub(super)`, …) are dropped — a split legitimately rewires imports and
  widens visibility;
* what is left is cut after every `;`, `{` and `}`, every attribute becomes a
  segment of its own, and lone `}` are ignored (the compiler checks bracket
  structure).

A pure move leaves the two multisets identical. Anything else is printed as a
residual: `-` only before, `+` only after, with a count. A split has a few
expected ones — a repeated `implFoo{` header, a new `#[cfg(test)]`, a `super::`
path — and each has to be explainable by the move. Comment lines that exist
before but not after are listed as a warning: the code survived, the
documentation did not. Visibility is reported separately, as a count per
level before and after, since stripping it hides a widening or a narrowing.

What it cannot see, because a multiset has no order: statements, match arms or
branches swapped, a statement moved from one fn to another, an attribute moved
from one item to another. `(x,)` → `(x)` (a one-element tuple turned into a
parenthesised value) also passes, because of the trailing-comma rule. A clean
result is strong evidence, not proof; read any diff that is not a plain cut.

    scripts/move-check.py                 # HEAD vs working tree (incl. untracked)
    scripts/move-check.py HEAD~1 HEAD     # one commit
    scripts/move-check.py main HEAD       # a whole branch

Exit status: 0 when there is no code residual, 1 otherwise.
"""

import argparse
import hashlib
import re
import subprocess
import sys
from collections import Counter


def git(*args: str) -> str:
    return subprocess.run(["git", *args], check=True, capture_output=True, text=True).stdout


def exists_at(rev: str, path: str) -> bool:
    return subprocess.run(["git", "cat-file", "-e", f"{rev}:{path}"], capture_output=True).returncode == 0


def changed_paths(base: str, rev: str | None) -> list[str]:
    spec = [base] if rev is None else [base, rev]
    paths = set(git("diff", "--no-renames", "--name-only", *spec).split())
    if rev is None:
        paths |= set(git("ls-files", "--others", "--exclude-standard").split())
    return sorted(p for p in paths if p.endswith(".rs"))


def contents(rev: str | None, path: str) -> str | None:
    if rev is None:
        try:
            with open(path, encoding="utf-8") as f:
                return f.read()
        except FileNotFoundError:
            return None
    return git("show", f"{rev}:{path}") if exists_at(rev, path) else None


def lex(src: str) -> tuple[str, list[str]]:
    """Return (code with comments removed and literals hashed, comment lines)."""
    out: list[str] = []
    comments: list[str] = []
    i, n = 0, len(src)

    def literal(text: str) -> None:
        out.append('"' + hashlib.sha1(text.encode()).hexdigest()[:12] + '"')

    while i < n:
        c = src[i]
        if src.startswith("//", i):
            j = src.find("\n", i)
            j = n if j < 0 else j
            comments.append(src[i:j].strip())
            i = j
        elif src.startswith("/*", i):
            depth, j = 1, i + 2
            while j < n and depth:
                if src.startswith("/*", j):
                    depth, j = depth + 1, j + 2
                elif src.startswith("*/", j):
                    depth, j = depth - 1, j + 2
                else:
                    j += 1
            comments.extend(l.strip() for l in src[i:j].splitlines())
            out.append(" ")
            i = j
        elif (m := re.match(r'b?r(#*)"', src[i:i + 300])) and (i == 0 or not (src[i - 1].isalnum() or src[i - 1] == "_")):
            close = '"' + m.group(1)
            j = src.find(close, i + m.end())
            j = n if j < 0 else j + len(close)
            literal(src[i:j])
            i = j
        elif c == '"' or (c == "b" and src.startswith('b"', i) and (i == 0 or not (src[i - 1].isalnum() or src[i - 1] == "_"))):
            j = i + (2 if c == "b" else 1)
            while j < n and src[j] != '"':
                j += 2 if src[j] == "\\" else 1
            literal(src[i:j + 1])
            i = j + 1
        elif c == "'":
            # A char literal ('x', '\n', '\u{1F600}') or a lifetime/label ('a).
            if i + 1 < n and src[i + 1] == "\\":
                j = src.find("'", i + 2)
                literal(src[i:j + 1])
                i = j + 1
            elif i + 2 < n and src[i + 2] == "'":
                literal(src[i:i + 3])
                i += 3
            else:
                out.append(c)
                i += 1
        else:
            out.append(c)
            i += 1
    return "".join(out), [c for c in comments if c not in ("//", "///", "//!")]


VIS = re.compile(r"\bpub\b\s*(\(\s*(crate|super|self|in\s+[\w:]+)\s*\))?\s*")
USE = re.compile(r"\buse\s+[^;]*;")
# `mod x;` with its optional `#[path = "…"]`, and the header of an inline
# `mod x {` (its closing brace is a lone `}`, already ignored) — so a module
# moving between inline and its own file is not a residual.
MOD = re.compile(r'(#\[\s*path\s*=\s*"[0-9a-f]+"\s*\]\s*)?\bmod\s+\w+\s*[;{]')


def segments(code: str) -> list[str]:
    code = VIS.sub(" ", code)
    code = USE.sub(" ", code)
    code = MOD.sub(" ", code)
    code = re.sub(r"\s+", "", code)
    code = re.sub(r",([)\]}])", r"\1", code)  # rustfmt adds these when it wraps
    code = re.sub(r"([;{}])", r"\1\n", code)
    return [s for s in split_attributes(code).split("\n") if s and s != "}"]


def split_attributes(code: str) -> str:
    """Put every `#[…]` / `#![…]` on a segment of its own, so an attribute
    that ends up next to different code after a move (`#[cfg(test)]` before
    `mod tests {` vs. before `mod tests;`) is not a residual."""
    out, i = [], 0
    while (j := code.find("#", i)) >= 0:
        k = j + 2 if code.startswith("#!", j) else j + 1
        if not code.startswith("[", k):
            out.append(code[i:k])
            i = k
            continue
        depth, e = 0, k
        while e < len(code):
            depth += {"[": 1, "]": -1}.get(code[e], 0)
            e += 1
            if depth == 0:
                break
        out.append(code[i:j] + "\n" + code[j:e] + "\n")
        i = e
    out.append(code[i:])
    return "".join(out)


def visibility(code: str) -> Counter:
    """Count visibility qualifiers on items, `pub use` re-exports excluded."""
    return Counter(
        re.sub(r"\s+", "", m.group(0))
        for m in VIS.finditer(code)
        if not re.match(r"\s*use\b", code[m.end():m.end() + 8])
    )


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("base", nargs="?", default="HEAD")
    ap.add_argument("rev", nargs="?", default=None, help="default: the working tree")
    a = ap.parse_args()

    paths = changed_paths(a.base, a.rev)
    before, after = Counter(), Counter()
    before_c, after_c = Counter(), Counter()
    vis = {a.base: Counter(), a.rev: Counter()}
    for p in paths:
        for rev, segs, coms in ((a.base, before, before_c), (a.rev, after, after_c)):
            src = contents(rev, p)
            if src is None:
                continue
            code, comments = lex(src)
            segs.update(segments(code))
            coms.update(comments)
            vis[rev].update(visibility(code))

    print(f"{len(paths)} .rs file(s): {sum(before.values())} code segments before, {sum(after.values())} after")
    levels = sorted(set(vis[a.base]) | set(vis[a.rev]))
    print("visibility (before → after): " + ", ".join(f"{l} {vis[a.base][l]}→{vis[a.rev][l]}" for l in levels))
    gone, new = before - after, after - before
    for sign, diff in (("-", gone), ("+", new)):
        for seg, k in sorted(diff.items()):
            print(f"  {sign} {k:>3}× {seg[:160]}")
    lost = before_c - after_c
    if lost:
        print(f"warning: {sum(lost.values())} comment line(s) before but not after:")
        for line, k in sorted(lost.items()):
            print(f"  - {k:>3}× {line[:160]}")
    added = sum((after_c - before_c).values())
    if added:
        print(f"note: {added} comment line(s) added (module headers etc.)")
    if gone or new:
        print(f"RESIDUAL: {sum(gone.values())} segment(s) only before, {sum(new.values())} only after")
        return 1
    print("OK: pure move")
    return 0


if __name__ == "__main__":
    sys.exit(main())
