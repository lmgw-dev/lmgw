#!/usr/bin/env python3
# Usage: python3 ci/arch_guard_lock.py CARGO_LOCK_BEFORE CARGO_LOCK_AFTER
"""Fail unless a Cargo.lock changed only in the `version` lines of the workspace's own packages.

ci/build-arch.sh stamps a build version into Cargo.toml and runs `cargo update --workspace`,
which rewrites the workspace members' versions in Cargo.lock. That step has the network (a git
dependency needs it), so it must not be able to move any other package: this compares the lock
before and after as TOML, and every difference other than a path package's `version` is a
refusal naming the package. A lock that cannot be read, or whose package list changed in size,
is refused too.

Exits 0 when only member versions changed, 1 on any other difference, 2 on a usage or read error.
"""

import sys
import tomllib


def differences(before, after):
    """The refusals, as strings, between two parsed Cargo.lock files."""
    out = []
    for key in sorted((set(before) | set(after)) - {"package"}):
        if before.get(key) != after.get(key):
            out.append(f"the top-level `{key}` changed")
    old, new = before.get("package"), after.get("package")
    if not isinstance(old, list) or not isinstance(new, list) or not old:
        return out + ["a lock without a [[package]] list"]
    if len(old) != len(new):
        return out + [f"the package list went from {len(old)} to {len(new)} entries"]
    for a, b in zip(old, new):
        label = f"{a.get('name')} {a.get('version')}"
        if a.get("name") != b.get("name") or a.get("source") != b.get("source"):
            out.append(f"{label}: replaced by {b.get('name')} {b.get('version')}")
            continue
        # Only a path package (no `source`) is a workspace member, and only its version may move.
        ignore = {"version"} if "source" not in a else set()
        for key in sorted((set(a) | set(b)) - ignore):
            if a.get(key) != b.get(key):
                out.append(f"{label}: `{key}` changed")
    return out


def load(path):
    with open(path, "rb") as f:
        return tomllib.load(f)


def main(argv):
    if len(argv) != 3:
        print(__doc__.split("\n\n")[0], file=sys.stderr)
        return 2
    try:
        found = differences(load(argv[1]), load(argv[2]))
    except (OSError, ValueError) as e:
        print(f"arch_guard_lock: {e}", file=sys.stderr)
        return 2
    for line in found:
        print(f"arch_guard_lock: {line}", file=sys.stderr)
    if found:
        print(f"arch_guard_lock: REFUSED, {len(found)} difference(s) beyond the workspace "
              "members' versions", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
