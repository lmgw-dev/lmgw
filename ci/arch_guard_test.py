#!/usr/bin/env python3
"""Test of ci/arch_guard.py on sample package tars, and of its path mapping against the
RPM allowlist.

Usage: python3 ci/arch_guard_test.py   (ci/check.sh and the arch job run it)

Builds in-memory tars laid out like makepkg's output (no makepkg, no root, no network): a clean
one with exactly the allowlisted files, and one per kind of problem the guard refuses. The
helper and the document reader are small stand-ins that start like an ELF file, and "target/
release" is a temporary directory holding the same bytes.
"""

import hashlib
import io
import os
import sys
import tarfile
import tempfile
import tomllib

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import arch_guard  # noqa: E402
import arch_guard_lock  # noqa: E402
import rpm_guard  # noqa: E402

POLICY_SRC = os.path.join(os.path.dirname(HERE), "crates", "lmgw-machine-helper", "polkit",
                          "io.github.lmgw-dev.lmgw.machine-setup.policy")
HELPER = b"\x7fELF\x02\x01\x01\x00 arch_guard_test stand-in for the root helper\n"
READER = b"\x7fELF\x02\x01\x01\x00 arch_guard_test stand-in for the doc reader\n"
PKGINFO = b"pkgname = lmgw\npkgver = 0.7.0-1\ndepend = podman\n"

failures = []


def check(cond, what, detail=""):
    print(("ok   " if cond else "FAIL ") + what)
    if not cond:
        failures.append(what)
        if detail:
            print("     " + detail.replace("\n", "\n     "))


def sample(contents, mode_of=None, extra=(), meta=(".PKGINFO", ".MTREE", ".BUILDINFO"), owner="root"):
    """A makepkg-like tar: metadata first, then directories and files."""
    mode_of = mode_of or {}
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w", format=tarfile.PAX_FORMAT) as tf:
        def add(name, data=b"", mode=0o644, typ=tarfile.REGTYPE, uname=owner, **kw):
            t = tarfile.TarInfo(name)
            t.type, t.mode, t.size = typ, mode, len(data) if typ == tarfile.REGTYPE else 0
            t.uname = t.gname = uname
            for k, v in kw.items():
                setattr(t, k, v)
            tf.addfile(t, io.BytesIO(data) if typ == tarfile.REGTYPE else None)
        for m in meta:
            add(m, PKGINFO if m == ".PKGINFO" else b"")
        dirs = set()
        for path in contents:
            parts = path.strip("/").split("/")[:-1]
            for i in range(1, len(parts) + 1):
                dirs.add("/".join(parts[:i]))
        for d in sorted(dirs):
            add(d + "/", mode=0o755, typ=tarfile.DIRTYPE)
        for path, data in contents.items():
            add(path.strip("/"), data, mode_of.get(path, 0o644))
        for name, kw in extra:
            add(name, **kw)
    buf.seek(0)
    return buf


def read_args(buf):
    return arch_guard.read_tar(buf)


def run(buf, allowed, pinned, root):
    entries, meta, pkginfo = arch_guard.read_tar(buf)
    return arch_guard.problems_of(entries, meta, pkginfo, allowed, pinned, root)


LOCK = """version = 4

[[package]]
name = "lmgw"
version = "{v}"
dependencies = ["serde"]

[[package]]
name = "serde"
version = "{s}"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "aa"
"""


def lock_checks():
    def diff(**kw):
        old = tomllib.loads(LOCK.format(v="0.7.0", s="1.0.0"))
        new = tomllib.loads(LOCK.format(**{"v": "0.7.0+9", "s": "1.0.0", **kw}))
        return arch_guard_lock.differences(old, new)
    check(not diff(), "a lock whose only change is a member's version passes")
    got = diff(s="1.0.1")
    check(any("serde 1.0.0" in g for g in got), "a registry package's version change is refused",
          "\n".join(got))
    new = tomllib.loads(LOCK.format(v="0.7.0+9", s="1.0.0"))
    new["package"].pop()
    got = arch_guard_lock.differences(tomllib.loads(LOCK.format(v="0.7.0", s="1.0.0")), new)
    check(got, "a dropped package is refused")
    new = tomllib.loads(LOCK.format(v="0.7.0+9", s="1.0.0"))
    new["package"][0]["dependencies"] = ["serde", "evil"]
    got = arch_guard_lock.differences(tomllib.loads(LOCK.format(v="0.7.0", s="1.0.0")), new)
    check(any("dependencies" in g for g in got), "a member's changed dependencies are refused",
          "\n".join(got))


def main():
    allowed, _, _, pinned = rpm_guard.load_allowlist(rpm_guard.ALLOWLIST)
    mapped = arch_guard.arch_allowlist(allowed)
    check("/usr/share/icons/hicolor/256x256/apps/lmgw.png" in mapped
          and not any("@2" in p for p in mapped) and arch_guard.LICENSE in mapped,
          "the allowlist maps to Arch's paths")
    policy_path = "/usr/share/polkit-1/actions/io.github.lmgw-dev.lmgw.machine-setup.policy"
    with open(POLICY_SRC, "rb") as f:
        policy = f.read()
    with tempfile.TemporaryDirectory() as root:
        rel = os.path.join(root, "target", "release")
        os.makedirs(rel)
        for name, data in (("lmgw-machine-helper", HELPER), ("lmgw-doc-reader", READER)):
            with open(os.path.join(rel, name), "wb") as f:
                f.write(data)
        contents = {p: b"x" for p in mapped}
        contents[policy_path] = policy
        contents["/usr/libexec/lmgw/lmgw-machine-helper"] = HELPER
        contents["/usr/libexec/lmgw/lmgw-doc-reader"] = READER
        modes = {"/usr/bin/lmgw": 0o755, "/usr/libexec/lmgw/lmgw-machine-helper": 0o755,
                 "/usr/libexec/lmgw/lmgw-doc-reader": 0o755}

        def probs(c=contents, m=modes, **kw):
            return [f"{p}: {msg}" for p, msg in run(sample(c, m, **kw), allowed, pinned, root)]

        got = probs()
        check(not got, "a clean package passes", "\n".join(got))

        def refused(what, needle, **kw):
            got = probs(**kw)
            check(any(needle in g for g in got), what, "\n".join(got) or "(no problem found)")

        refused("a file outside the allowlist", "/usr/bin/evil: ",
                c={**contents, "/usr/bin/evil": b"x"})
        refused("a missing allowlisted file", "not in the package",
                c={p: d for p, d in contents.items() if p != "/usr/bin/lmgw"})
        refused("a world-writable file", "world-writable",
                m={**modes, "/usr/share/applications/lmgw.desktop": 0o666})
        refused("a setuid file", "setuid", m={**modes, "/usr/bin/lmgw": 0o4755})
        refused("a file owned by a user", "not root:root", owner="builder")
        refused("an install script", ".INSTALL", meta=(".PKGINFO", ".MTREE", ".BUILDINFO", ".INSTALL"))
        refused("a changed polkit policy", "not the pinned one",
                c={**contents, policy_path: policy.replace(b"auth_admin", b"yes")})
        refused("a policy with another mode", "pins 0644",
                m={**modes, policy_path: 0o664})
        refused("another helper", "not the file this job built",
                c={**contents, "/usr/libexec/lmgw/lmgw-machine-helper": HELPER + b"x"})
        refused("a helper that is not executable by mode", "pins 0755",
                m={**modes, "/usr/libexec/lmgw/lmgw-machine-helper": 0o644})
        refused("a symlink", "a symlink",
                extra=[("usr/bin/lmgw-link", {"typ": tarfile.SYMTYPE, "linkname": "/etc/shadow"})])
        refused("a capability", "capability",
                extra=[("usr/bin/capped", {"pax_headers": {arch_guard.CAPS: "x"}})])
        helper = "usr/libexec/lmgw/lmgw-machine-helper"
        no_helper = {p: d for p, d in contents.items() if p != "/" + helper}
        refused("a directory where the helper belongs", "allowlist expects a regular file",
                c=no_helper, extra=[(helper + "/", {"typ": tarfile.DIRTYPE, "mode": 0o755})])
        policy_rel = policy_path.strip("/")
        refused("a directory where the polkit policy belongs", "allowlist expects a regular file",
                c={p: d for p, d in contents.items() if p != policy_path},
                extra=[(policy_rel + "/", {"typ": tarfile.DIRTYPE, "mode": 0o755})])
        refused("a directory that is no parent of a listed file", "/etc/sudoers.d: ",
                extra=[("etc/", {"typ": tarfile.DIRTYPE, "mode": 0o755}),
                       ("etc/sudoers.d/", {"typ": tarfile.DIRTYPE, "mode": 0o755})])
        refused("a directory with another owner than root", "not root:root",
                extra=[("usr/share/", {"typ": tarfile.DIRTYPE, "mode": 0o755, "uname": "builder"})])
        evil = PKGINFO + b"provides = sh-evil\nconflict = nonexistent-foo\n"
        refused("an evil .PKGINFO before a clean one", ".PKGINFO: is in the package 2 times",
                meta=(".MTREE", ".BUILDINFO"),
                extra=[(".PKGINFO", {"data": evil}), (".PKGINFO", {"data": PKGINFO})])
        refused("an evil .PKGINFO after a clean one", "provides",
                meta=(".PKGINFO", ".MTREE", ".BUILDINFO"),
                extra=[(".PKGINFO", {"data": evil})])
        refused("a path twice", "is in the package 2 times",
                extra=[("usr/bin/lmgw", {"data": b"y", "mode": 0o755})])
        refused("a metadata entry twice", "is in the package 2 times",
                extra=[(".MTREE", {"data": b"y"})])
        refused("an ACL", "ACL",
                extra=[("usr/bin/acl", {"pax_headers": {"SCHILY.acl.access": "user:nobody:rw-:65534"}})])
        refused("another package name", "pkgname",
                meta=(".MTREE", ".BUILDINFO"),
                extra=[(".PKGINFO", {"data": PKGINFO.replace(b"pkgname = lmgw", b"pkgname = glibc")})])
        got = [f"{p}: {m}" for p, m in arch_guard.problems_of(
            *read_args(sample(contents, modes)), allowed, pinned, root, pkgver="0.8.0-1")]
        check(any("pkgver" in g for g in got), "another package version", "\n".join(got))
    lock_checks()
    if failures:
        print(f"\n{len(failures)} failure(s)")
        return 1
    print("\nall checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
