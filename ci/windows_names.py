#!/usr/bin/env python3
"""Fail when a tracked path cannot be checked out by git on Windows.

Usage: python3 ci/windows_names.py

Git for Windows refuses any path with a component whose name before the first
dot is a reserved DOS device (CON, PRN, AUX, NUL, COM0-9, LPT0-9, any case),
and any component ending in a dot or a space. Directory components count too.
"""
import re
import subprocess
import sys

RESERVED = re.compile(r"(con|prn|aux|nul|com[0-9]|lpt[0-9])", re.IGNORECASE)


def bad(path):
    for part in path.split("/"):
        if RESERVED.fullmatch(part.split(".", 1)[0].rstrip(" ")):
            return f"reserved Windows device name '{part}'"
        if part.endswith((".", " ")):
            return f"component '{part}' ends in a dot or a space"
    return None


def main():
    out = subprocess.run(
        ["git", "ls-files", "-z"], check=True, capture_output=True, text=True
    ).stdout
    found = [(p, why) for p in out.split("\0") if p and (why := bad(p))]
    for path, why in found:
        print(f"ERROR: {path}: {why}", file=sys.stderr)
    if found:
        print(
            "git cannot check these paths out on Windows; rename them "
            "(e.g. aux.rs -> aux_models.rs).",
            file=sys.stderr,
        )
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
