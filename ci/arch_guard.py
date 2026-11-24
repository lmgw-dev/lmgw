#!/usr/bin/env python3
# Usage: python3 ci/arch_guard.py [--allowlist FILE] [--root DIR] PACKAGE.pkg.tar.zst
"""Refuse an lmgw Arch package that would give anyone but root a way into what root installs.

The counterpart of ci/rpm_guard.py for the package packaging/arch/PKGBUILD builds, with the
same policy: it reads ci/rpm-allowlist.toml and runs rpm_guard's own checks on the package's
entries, so there is one allowlist. ci/build-arch.sh runs it right after makepkg; a non-zero
exit fails the build and nothing is published.

The package is read as a tar stream (`bsdtar --format=pax -cf - @PACKAGE`, which also unpacks
zstd) and nothing is extracted. Checked:

  - every entry is listed in the allowlist, with the kind listed there; the allowlist's paths
    are the RPM's, with the 256x256 icon where hicolor wants it (the RPM's `256x256@2`) and
    the licence Arch requires at /usr/share/licenses/lmgw/LICENSE. Directories are not
    listed: the package may carry exactly the parent directories of the listed files, with the
    same mode and owner checks as a file, and a directory at a listed file's path is refused
  - every listed path is in the package, and no path is in it twice
  - no entry carries an ACL
  - no entry is group- or world-writable, none has a setuid, setgid or sticky bit or a file
    capability, all are root:root
  - the polkit policy and the two /usr/libexec programs are as pinned: the policy has the
    pinned mode and sha256, the programs the pinned mode and the digest of the file this job
    built (target/release/..., under --root), which must be an ELF executable. The PKGBUILD
    sets !strip so that the packaged copies are those files
  - the package metadata is only .PKGINFO, .MTREE and .BUILDINFO: no .INSTALL (an install
    script runs as root), no .CHANGELOG; and .PKGINFO declares no provides, conflict or
    replaces (pacman removes the packages those name); each of them is there once, and
    `pkgname` is lmgw (with --pkgver, `pkgver` is that too), because pacman installs under the
    name .PKGINFO gives

Not checked yet: the package's `depend`, `optdepend` and `makedepend` lines in .PKGINFO (the RPM's
are pinned in the allowlist's [dependencies]), and the content of the desktop entry and the icons
beyond their paths, modes and owners.

Prints one line per problem, `arch_guard: <path>: <what is wrong>`, and exits 1 when there is
any; exits 0 with a one-line summary otherwise, 2 on a usage or read error.
"""

import argparse
import collections
import hashlib
import os
import stat
import subprocess
import sys
import tarfile

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import rpm_guard  # noqa: E402

ROOT = os.path.dirname(HERE)
ALLOWLIST = rpm_guard.ALLOWLIST

METADATA = {".PKGINFO", ".MTREE", ".BUILDINFO"}
# .PKGINFO keys that act on other packages at install.
FORBIDDEN_PKGINFO = ("provides", "conflict", "replaces")
LICENSE = "/usr/share/licenses/lmgw/LICENSE"
CAPS = "SCHILY.xattr.security.capability"

TYPES = [
    (lambda t: t.isreg(), stat.S_IFREG),
    (lambda t: t.isdir(), stat.S_IFDIR),
    (lambda t: t.issym(), stat.S_IFLNK),
    (lambda t: t.ischr(), stat.S_IFCHR),
    (lambda t: t.isblk(), stat.S_IFBLK),
    (lambda t: t.isfifo(), stat.S_IFIFO),
]


def arch_allowlist(allowed):
    """The RPM allowlist's files as this package lays them out."""
    out = {}
    for path, kind in allowed.items():
        out[path.replace("/256x256@2/", "/256x256/")] = kind
    out[LICENSE] = "a regular file"
    return out


def ancestors(path):
    """The directories above a path, as "/usr", "/usr/bin", ... (not "/")."""
    parts = path.strip("/").split("/")[:-1]
    return ["/" + "/".join(parts[:i]) for i in range(1, len(parts) + 1)]


def pkginfo_value(pkginfo, key):
    """The values `key = value` has in .PKGINFO text, in order."""
    out = []
    for line in pkginfo.splitlines():
        k, sep, v = line.partition("=")
        if sep and k.strip() == key:
            out.append(v.strip())
    return out


def read_tar(fileobj):
    """(entries as rpm_guard.check reads them, metadata names, the text of every .PKGINFO) of a
    tar stream."""
    entries, meta, pkginfo = [], [], ""
    with tarfile.open(fileobj=fileobj, mode="r|") as tf:
        for t in tf:
            name = t.name
            if name.startswith("./"):
                name = name[2:]
            name = name.rstrip("/")
            if name.startswith("."):
                meta.append(name)
                if not t.isreg():
                    raise rpm_guard.QueryError(f"{name}: package metadata that is not a "
                                               "regular file")
                if name == ".PKGINFO":
                    pkginfo += tf.extractfile(t).read().decode("utf-8", errors="replace") + "\n"
                continue
            kind = next((bit for test, bit in TYPES if test(t)), 0)
            digest = ""
            if t.isreg():
                h = hashlib.sha256()
                src = tf.extractfile(t)
                for block in iter(lambda: src.read(1 << 20), b""):
                    h.update(block)
                digest = h.hexdigest()
            entries.append({
                "path": "/" + name, "mode": kind | t.mode, "user": t.uname or str(t.uid),
                "group": t.gname or str(t.gid),
                "caps": ascii(t.pax_headers[CAPS]) if t.pax_headers.get(CAPS) else "",
                "acl": sorted(k for k in t.pax_headers if k.startswith("SCHILY.acl.")),
                "link": t.linkname if t.issym() or t.islnk() else "", "digest": digest,
            })
    return entries, meta, pkginfo


def problems_of(entries, meta, pkginfo, allowed, pinned, root=ROOT, pkgver=None):
    """The problems, as (path, message) pairs."""
    problems = []
    allowed = arch_allowlist(allowed)
    # The only directories allowed are the parents of the listed files; they never take the
    # place of a listed path, and they are still mode- and owner-checked.
    for d in {d for f in list(allowed) for d in ancestors(f)}:
        allowed.setdefault(d, "a directory")
    problems += rpm_guard.check(entries, "", allowed, set())
    problems += rpm_guard.check_pinned(entries, pinned, rpm_guard.PIN_ALGO, root=root)
    for path, n in collections.Counter(e["path"] for e in entries).items():
        if n > 1:
            problems.append((path, f"is in the package {n} times"))
    for e in entries:
        if e.get("acl"):
            problems.append((e["path"], f"has an ACL ({', '.join(e['acl'])})"))
    for name, n in collections.Counter(meta).items():
        if name not in METADATA:
            problems.append((name, "package metadata that is not .PKGINFO, .MTREE or "
                                   ".BUILDINFO (an .INSTALL script runs as root at install)"))
        if n > 1:
            problems.append((name, f"is in the package {n} times"))
    if ".PKGINFO" not in meta:
        problems.append((".PKGINFO", "the package has none"))
    for line in pkginfo.splitlines():
        key = line.split("=", 1)[0].strip()
        if key in FORBIDDEN_PKGINFO:
            problems.append((".PKGINFO", f"declares `{line.strip()}`: pacman acts on other "
                                         "packages for it, and the allowlist lists none"))
    if pkginfo_value(pkginfo, "pkgname") != ["lmgw"]:
        found = pkginfo_value(pkginfo, "pkgname")
        problems.append((".PKGINFO", "pkgname is not exactly one `pkgname = lmgw` (pacman "
                                     f"installs under the name given there; found {found})"))
    if pkgver is not None and pkginfo_value(pkginfo, "pkgver") != [pkgver]:
        found = pkginfo_value(pkginfo, "pkgver")
        problems.append((".PKGINFO", f"pkgver is not exactly one `pkgver = {pkgver}` "
                                     f"(found {found})"))
    return problems


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("package", help="the .pkg.tar.zst to check")
    ap.add_argument("--allowlist", default=ALLOWLIST, help="default: ci/rpm-allowlist.toml")
    ap.add_argument("--root", default=ROOT, help="the checkout whose target/release was built")
    ap.add_argument("--pkgver", help="the pkgver-pkgrel .PKGINFO must carry (default: not checked)")
    args = ap.parse_args()
    if not os.path.isfile(args.package):
        ap.error(f"no such file: {args.package}")
    try:
        allowed, _, _, pinned = rpm_guard.load_allowlist(args.allowlist)
        p = subprocess.Popen(["bsdtar", "--format=pax", "-cf", "-", "@" + os.path.abspath(args.package)],
                             stdout=subprocess.PIPE, stdin=subprocess.DEVNULL)
        try:
            entries, meta, pkginfo = read_tar(p.stdout)
        finally:
            p.stdout.read()
            p.stdout.close()
        if p.wait() != 0:
            raise rpm_guard.QueryError(f"bsdtar could not read {args.package}")
        problems = problems_of(entries, meta, pkginfo, allowed, pinned, args.root, args.pkgver)
    except (rpm_guard.QueryError, OSError, ValueError, AttributeError, tarfile.TarError) as e:
        print(f"arch_guard: {args.package}: {type(e).__name__}: {e}", file=sys.stderr)
        return 2
    for path, msg in problems:
        print(f"arch_guard: {path}: {msg}", file=sys.stderr)
    name = os.path.basename(args.package)
    if problems:
        print(f"arch_guard: {name}: REFUSED, {len(problems)} problem(s) in {len(entries)} "
              "entries", file=sys.stderr)
        return 1
    print(f"arch_guard: {name}: {len(entries)} entries, all in the allowlist, none group- or "
          "world-writable, no setuid/setgid/sticky bit or capability, root:root, "
          f"{len(pinned)} pinned file(s) as pinned, no install script")
    return 0


if __name__ == "__main__":
    sys.exit(main())
