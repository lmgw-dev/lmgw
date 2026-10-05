#!/usr/bin/env python3
"""The automatic scan behind scripts/publish-github.sh: what is about to become public, checked
for secret shapes and for the owner's private words.

Usage: publish_scan.py tree --base TREE --tree TREE --diff FILE --binaries-out FILE --denylist FILE
       publish_scan.py text --label NAME --denylist FILE FILE...

`tree` reads FILE (`git diff BASE TREE`, written by the caller for the audit) and scans every
added line and every new path, then lists the added or changed binary files into
--binaries-out and scans their printable strings. `text` scans whole files (the commit message,
the tag message). Both print one hit per line, `where<TAB>rule<TAB>match`, sorted, and exit 0;
no output means a clean scan.

The rules are generic categories only. Words private to whoever publishes (names, domains,
hosts, device names) never belong in this repo: they come from the denylist file, which lives
outside it. Its format: one case-insensitive Python regular expression per line; blank lines
and lines starting with '#' are skipped. For example:

    # my own words, kept out of the public repo
    jane
    \\bmy-laptop\\b
    example-intranet\\.test

Every host of a configured git remote other than github.com is added as a rule too, with its
parent domain, so a private remote's address never travels in a diff or a message.
"""

import argparse
import re
import subprocess
import sys

# Secret shapes, by category. Deliberately generic: a hit is something to look at, and the
# caller's gate makes every hit an explicit acknowledgement.
SHAPES = [
    ("api key (sk-)", r"\bsk-(?:ant-|proj-)?[A-Za-z0-9_-]{20,}"),
    ("google api key", r"\bAIza[0-9A-Za-z_-]{35}"),
    ("hugging face token", r"\bhf_[A-Za-z0-9]{30,}"),
    ("gitlab token", r"\bglpat-[A-Za-z0-9_-]{20,}"),
    ("github token", r"\b(?:gh[pousr]_[A-Za-z0-9]{36,}|github_pat_[A-Za-z0-9_]{40,})"),
    ("private key", r"-----BEGIN (?:[A-Z0-9]+ )*PRIVATE KEY-----"),
    ("lmgw owner token", r"\blmgw-owner-[0-9a-f]{16,}"),
    ("bearer value", r"(?i)\bbearer\s+[A-Za-z0-9._~+/-]{20,}=*"),
    ("e-mail address", r"\b[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,}\b"),
]

# Addresses that are no one's: the SSH remote syntax, GitHub's no-reply addresses and the
# reserved example domains.
EMAIL_OK = re.compile(
    r"(?i)^git@|@users\.noreply\.github\.com$|@example\.(?:com|org|net)$|@[a-z0-9.-]*\.(?:test|example|invalid|localhost)$"
)

MATCH_SHOWN = 160


def denylist(path):
    rules = []
    with open(path, encoding="utf-8") as f:
        for n, line in enumerate(f, 1):
            pattern = line.strip()
            if not pattern or pattern.startswith("#"):
                continue
            try:
                rules.append((f"denylist line {n}", re.compile(pattern, re.IGNORECASE)))
            except re.error as e:
                sys.exit(f"publish_scan: denylist line {n} is not a valid regular expression: {e}")
    return rules


def remote_hosts():
    out = subprocess.run(
        ["git", "remote", "-v"], capture_output=True, text=True, stdin=subprocess.DEVNULL
    ).stdout
    hosts = set()
    for line in out.splitlines():
        parts = line.split()
        if len(parts) < 2:
            continue
        url = parts[1]
        m = re.match(r"^[a-z][a-z0-9+.-]*://(?:[^@/]+@)?([^:/]+)", url) or re.match(
            r"^(?:[^@/]+@)?([^:/]+):", url
        )
        if not m:
            continue  # a local path, or a disabled push URL
        host = m.group(1).lower()
        if host == "github.com" or host.endswith(".github.com"):
            continue
        hosts.add(host)
        labels = host.split(".")
        if len(labels) > 2:
            hosts.add(".".join(labels[-2:]))
    return [(f"remote host {h}", re.compile(re.escape(h), re.IGNORECASE)) for h in sorted(hosts)]


def rules(denylist_path):
    shapes = [(name, re.compile(rx)) for name, rx in SHAPES]
    return shapes + denylist(denylist_path) + remote_hosts()


def scan(text, where, rule_set, hits):
    for name, rx in rule_set:
        for m in rx.finditer(text):
            found = m.group(0)
            if name == "e-mail address" and EMAIL_OK.search(found):
                continue
            hits.add(f"{where}\t{name}\t{found[:MATCH_SHOWN]}")


def scan_diff(diff_path, rule_set, hits):
    """Every added line and every new path of a `git diff` (not its removals: those are public
    already, or about to stop being)."""
    path, line_no, in_hunk = "?", 0, False
    with open(diff_path, "rb") as f:
        for raw in f:
            line = raw.decode("utf-8", "replace").rstrip("\n")
            if line.startswith("diff --git "):
                in_hunk = False
                continue
            if not in_hunk:
                if line.startswith("+++ "):
                    target = line[4:]
                    path = target[2:] if target.startswith("b/") else target
                    scan(path, f"{path}:path", rule_set, hits)
                elif line.startswith(("rename to ", "copy to ")):
                    path = line.split(" to ", 1)[1]
                    scan(path, f"{path}:path", rule_set, hits)
                elif line.startswith("@@"):
                    in_hunk = True
                    line_no = int(re.match(r"@@ -\d+(?:,\d+)? \+(\d+)", line).group(1))
                continue
            if line.startswith("@@"):
                line_no = int(re.match(r"@@ -\d+(?:,\d+)? \+(\d+)", line).group(1))
            elif line.startswith("+"):
                scan(line[1:], f"{path}:{line_no}", rule_set, hits)
                line_no += 1
            elif line.startswith(" "):
                line_no += 1


def binaries(base, tree):
    """The added or changed binary files between two trees (numstat counts no lines for them)."""
    out = subprocess.run(
        ["git", "diff", "--numstat", "-z", "--no-renames", "--diff-filter=AM", base, tree],
        capture_output=True,
        check=True,
        stdin=subprocess.DEVNULL,
    ).stdout
    found = []
    for record in out.split(b"\0"):
        if record.startswith(b"-\t-\t"):
            found.append(record[4:].decode("utf-8", "replace"))
    return found


def scan_binary(tree, path, rule_set, hits):
    blob = subprocess.run(
        ["git", "cat-file", "blob", f"{tree}:{path}"],
        capture_output=True,
        check=True,
        stdin=subprocess.DEVNULL,
    ).stdout
    for m in re.finditer(rb"[\x20-\x7e]{6,}", blob):
        scan(m.group(0).decode("ascii"), f"{path}:binary@{m.start()}", rule_set, hits)


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    sub = ap.add_subparsers(dest="mode", required=True)
    t = sub.add_parser("tree")
    t.add_argument("--base", required=True)
    t.add_argument("--tree", required=True)
    t.add_argument("--diff", required=True)
    t.add_argument("--binaries-out", required=True)
    t.add_argument("--denylist", required=True)
    x = sub.add_parser("text")
    x.add_argument("--label", required=True)
    x.add_argument("--denylist", required=True)
    x.add_argument("files", nargs="+")
    args = ap.parse_args()

    rule_set = rules(args.denylist)
    hits = set()
    if args.mode == "tree":
        scan_diff(args.diff, rule_set, hits)
        found = binaries(args.base, args.tree)
        with open(args.binaries_out, "w", encoding="utf-8") as f:
            f.writelines(p + "\n" for p in found)
        for path in found:
            scan_binary(args.tree, path, rule_set, hits)
    else:
        for name in args.files:
            with open(name, encoding="utf-8", errors="replace") as f:
                for n, line in enumerate(f, 1):
                    scan(line.rstrip("\n"), f"{args.label}:{n}", rule_set, hits)
    for hit in sorted(hits):
        print(hit)


if __name__ == "__main__":
    main()
