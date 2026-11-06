#!/usr/bin/env python3
# Usage: python3 ci/rpm_guard.py [--allowlist FILE] (RPM_FILE | --installed NAME)
"""Refuse an lmgw RPM that would give anyone but root a way into what root installs.

rpm installs every file as root with the mode and owner the package header says, so a
group- or world-writable icon or licence file is a file any local user can replace under
/usr, and a setuid bit, a file capability or a scriptlet is code that runs with privileges.
ci/build.sh runs this right after tauri bundles the RPM; a non-zero exit fails the build.

Checked, from the header (`rpm -qp --qf`, what rpm itself installs from):

  - every entry is listed in ci/rpm-allowlist.toml (or --allowlist FILE), with the kind
    listed there: a regular file, or a directory; a symlink, device, fifo or socket never is
  - every listed path is in the package (a stale entry would later allow something unseen)
  - no entry is group- or world-writable
  - no setuid, setgid or sticky bit
  - no file capability (setcap)
  - owner and group are root
  - no scriptlet, trigger or file trigger (`rpm -qp --scripts`, `--triggers` and
    `--filetriggers`, one call each) unless the sha256 of that labelled output is under
    `scriptlets` in the allowlist
  - the dependency metadata is exactly the pinned set (`[dependencies]` in the allowlist):
    Requires (with their kind, e.g. `git (pre)`, and version), Recommends, Suggests,
    Supplements, Enhances, Conflicts, Obsoletes and Order. Provides may only be the package's
    own name and NAME(ARCH), plus what the allowlist lists. Never allowed, listed or not: a
    `user(`, `group(` or `groupmember(` Provides (rpm 6 turns those into systemd-sysusers
    users and groups at install), and Obsoletes or Conflicts that were not listed (dnf
    removes the packages they name). `rpmlib(...)` Requires are rpm's own feature checks and
    are not compared.
  - a `pinned` file (the root helper, its polkit policy) has exactly the pinned mode, and its
    content is pinned: by `sha256` for a file that is the same in every build (a policy whose
    content says <allow_any>yes</allow_any> passes every check above), or by `built`, a file
    this job built, whose digest the package's copy must have and which must be an ELF
    executable. The content comes from the header's file digests (FILEDIGESTS), which rpm
    checks every file it installs against. Every allowlisted file under /usr/libexec/ or
    /usr/share/polkit-1/ must be pinned.

--installed NAME runs the same checks on an installed package's header (`rpm -q`, read
only), e.g. to see what a package already on this machine ships. It compares no `built`
file (an installed package is not this tree's build) and says so.

Prints one line per problem, `rpm_guard: <path>: <what is wrong>`, and exits 1 when there
is any; exits 0 with a one-line summary otherwise, 2 on a usage or query error.
"""

import argparse
import re
import hashlib
import os
import shlex
import stat
import subprocess
import sys
import tomllib

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
ALLOWLIST = os.path.join(ROOT, "ci", "rpm-allowlist.toml")

# One record per file: mode, user, group, capability, link target, content digest (empty for
# anything but a regular file), path, every string shell-quoted so that any byte in a name (a
# space, a tab, a newline) survives the split. A tag the package lacks altogether (FILECAPS
# when no file has one) prints as `(none)`.
FIELDS = ["FILEMODES:octal", "FILEUSERNAME:shescape", "FILEGROUPNAME:shescape",
          "FILECAPS:shescape", "FILELINKTOS:shescape", "FILEDIGESTS:shescape",
          "FILENAMES:shescape"]
QUERY = "[" + " ".join("%{" + f + "}" for f in FIELDS) + "\n]"
NONE = "(none)"

# FILEDIGESTALGO's values (rpm's PGPHASHALGO_*), as hashlib names them. A package without the
# tag has MD5 digests (rpm's default before it). tauri-bundler's RPMs and rpmbuild's on Fedora
# say 8.
DIGEST_ALGOS = {1: "md5", 2: "sha1", 8: "sha256", 9: "sha384", 10: "sha512", 11: "sha224"}
# The one algorithm a pinned file's digest is compared in: sha256. MD5 and SHA-1 collide on
# purpose (a second file with the build's digest can be made), and a pin is a sha256.
PIN_ALGO = 8

# Allowlisted files under these must be `pinned`: root runs what is in /usr/libexec, and
# polkit reads its actions from /usr/share/polkit-1.
PIN_REQUIRED = ("/usr/libexec/", "/usr/share/polkit-1/")
ELF_MAGIC = b"\x7fELF"

KINDS = [
    (stat.S_ISREG, "a regular file"),
    (stat.S_ISDIR, "a directory"),
    (stat.S_ISLNK, "a symlink"),
    (stat.S_ISCHR, "a character device"),
    (stat.S_ISBLK, "a block device"),
    (stat.S_ISFIFO, "a fifo"),
    (stat.S_ISSOCK, "a socket"),
]


# The dependency tags: allowlist key -> (rpm tag prefix, heading used in messages). `provides`
# is checked differently (see check_dependencies), the rest are an exact set.
DEP_KEYS = {"requires": "REQUIRE", "provides": "PROVIDE", "recommends": "RECOMMEND",
            "suggests": "SUGGEST", "supplements": "SUPPLEMENT", "enhances": "ENHANCE",
            "conflicts": "CONFLICT", "obsoletes": "OBSOLETE", "orders": "ORDER"}
SYSUSERS = ("user(", "group(", "groupmember(")


class QueryError(Exception):
    pass


def kind_of(mode):
    for test, name in KINDS:
        if test(mode):
            return name
    return f"an unknown file type ({stat.S_IFMT(mode):o})"


def text(raw):
    """Decode rpm output without ever raising: a byte that is not UTF-8 stays a lone
    surrogate (surrogateescape), and read_package then refuses it as a QueryError (exit 2)."""
    return raw.decode("utf-8", errors="surrogateescape")


def rpm(args):
    # LC_ALL=C: rpm translates its messages ("package X is not installed"), and the checks
    # below read its words.
    env = {**os.environ, "LC_ALL": "C"}
    try:
        p = subprocess.run(["rpm", *args], capture_output=True, stdin=subprocess.DEVNULL,
                           env=env)
    except FileNotFoundError:
        raise QueryError("rpm is not installed")
    out, err = text(p.stdout), text(p.stderr)
    if p.returncode != 0:
        # `rpm -q` says "package X is not installed" on stdout.
        why = (err.strip() or out.strip() or f"exit {p.returncode}")
        raise QueryError(f"rpm {' '.join(args)} failed: {why}")
    return out


def load_allowlist(path):
    with open(path, "rb") as f:
        data = tomllib.load(f)
    unknown = set(data) - {"files", "directories", "scriptlets", "pinned", "dependencies"}
    if unknown:
        raise QueryError(f"{path}: unknown keys {sorted(unknown)}")
    allowed = {}
    for key, kind in (("files", "a regular file"), ("directories", "a directory")):
        for p in data.get(key, []):
            if not isinstance(p, str) or not p.startswith("/"):
                raise QueryError(f"{path}: {key} entry {p!r} is not an absolute path")
            if p in allowed:
                raise QueryError(f"{path}: {p} is listed twice")
            allowed[p] = kind
    scriptlets = set(data.get("scriptlets", []))
    deps = data.get("dependencies", {})
    unknown = set(deps) - set(DEP_KEYS)
    if unknown:
        raise QueryError(f"{path}: unknown [dependencies] keys {sorted(unknown)}")
    pinned = {}
    for key in DEP_KEYS:
        items = deps.get(key, [])
        if not isinstance(items, list) or not all(isinstance(i, str) for i in items):
            raise QueryError(f"{path}: dependencies.{key} is not a list of strings")
        pinned[key] = set(items)
    return allowed, scriptlets, pinned, load_pinned(path, data.get("pinned", {}), allowed)


def load_pinned(path, table, allowed):
    """{file: {"mode": int, "sha256"?: hex, "built"?: str}} from the allowlist's `pinned`."""
    if not isinstance(table, dict):
        raise QueryError(f"{path}: pinned is not a table of paths")
    out = {}
    for p, pin in table.items():
        where = f"{path}: pinned.\"{p}\""
        if allowed.get(p) != "a regular file":
            raise QueryError(f"{where} is not a regular file listed under `files`")
        if not isinstance(pin, dict):
            raise QueryError(f"{where} is not a table")
        unknown = set(pin) - {"mode", "sha256", "built"}
        if unknown:
            raise QueryError(f"{where}: unknown keys {sorted(unknown)}")
        mode = pin.get("mode")
        if not isinstance(mode, str) or not re.fullmatch(r"0[0-7]{3}", mode):
            raise QueryError(f"{where}: mode {mode!r} is not four octal digits like \"0644\"")
        entry = {"mode": int(mode, 8)}
        if "sha256" in pin:
            if not isinstance(pin["sha256"], str) or not re.fullmatch(r"[0-9a-f]{64}",
                                                                     pin["sha256"]):
                raise QueryError(f"{where}: sha256 is not 64 lowercase hex digits")
            entry["sha256"] = pin["sha256"]
        if "built" in pin:
            if not isinstance(pin["built"], str) or not pin["built"]:
                raise QueryError(f"{where}: built is not a path")
            entry["built"] = pin["built"]
        if "sha256" not in entry and "built" not in entry:
            raise QueryError(f"{where} pins no content: give it sha256 or built")
        out[p] = entry
    for p, kind in allowed.items():
        if kind == "a regular file" and p.startswith(PIN_REQUIRED) and p not in out:
            raise QueryError(f"{path}: {p} is under {', '.join(PIN_REQUIRED)} but not pinned: "
                             "give it a [pinned] entry with its mode and content")
    return out


def dependencies(query, label):
    """{allowlist key: set of strings}. A dependency renders as `name`, then ` (kind)` when
    it is not a plain manual one (Requires(pre) is `name (pre)`), then ` <flags> <version>`."""
    out = {}
    for key, tag in DEP_KEYS.items():
        # Flags and kind are rpm's own words (`>=`, `pre`), printed inside quotes by hand: the
        # shescape modifier cannot follow another one, and an empty value must stay a token.
        fields = [f"{tag}NAME:shescape", f"'%{{{tag}FLAGS:depflags}}'",
                  f"{tag}VERSION:shescape"]
        if key == "requires":
            fields.insert(1, f"'%{{{tag}FLAGS:deptype}}'")
        fmt = "[" + " ".join(f if f.startswith("'") else "%{" + f + "}" for f in fields) + "\n]"
        tokens = shlex.split(rpm([*query, "--qf", fmt]))
        n = len(fields)
        if tokens == [NONE] or tokens == [NONE] * n:
            tokens = []
        if len(tokens) % n:
            raise QueryError(f"{label}: unreadable {key} records")
        deps = set()
        for i in range(0, len(tokens), n):
            rec = tokens[i:i + n]
            name, rest = rec[0], rec[1:]
            kind = rest.pop(0) if key == "requires" else "manual"
            flags, version = rest
            if name.startswith("rpmlib(") and kind == "rpmlib":
                continue
            s = name
            if kind not in ("manual", NONE):
                s += f" ({kind})"
            if version and version != NONE:
                s += f" {flags} {version}"
            deps.add(s)
        out[key] = deps
    return out


def read_package(query):
    """query: ["-qp", file] or ["-q", name]. Returns (label, entries, scriptlet text, deps,
    own names, scriptlet kinds present, file digest algorithm as rpm numbers it)."""
    label = rpm([*query, "--qf", "%{NVRA}"]).strip()
    count = rpm([*query, "--qf", "%{FILENAMES:arraysize}"]).strip()
    count = 0 if count == NONE else int(count)
    tokens = shlex.split(rpm([*query, "--qf", QUERY]))
    n = len(FIELDS)
    if len(tokens) != count * n:
        raise QueryError(f"{label}: read {len(tokens)} fields for {count} files, "
                         f"expected {count * n}")
    entries = []
    for i in range(0, len(tokens), n):
        mode, user, group, caps, link, digest, path = tokens[i:i + n]
        try:
            mode = int(mode, 8)
        except ValueError:
            raise QueryError(f"{label}: unreadable mode {mode!r} for {path}")
        for field in (user, group, caps, link, digest, path):
            try:
                field.encode("utf-8")
            except UnicodeEncodeError:
                raise QueryError(f"{label}: a file name or owner is not valid UTF-8: "
                                 f"{path.encode('utf-8', 'backslashreplace').decode()}")
        entries.append({
            "path": path, "mode": mode, "user": user, "group": group,
            "caps": "" if caps == NONE else caps, "link": "" if link == NONE else link,
            "digest": "" if digest == NONE else digest.lower(),
        })
    algo = rpm([*query, "--qf", "%{FILEDIGESTALGO}"]).strip()
    try:
        algo = 1 if algo in (NONE, "") else int(algo)
    except ValueError:
        raise QueryError(f"{label}: unreadable file digest algorithm {algo!r}")
    # One call per kind: rpm prints each kind under its own option, and a package that has
    # one kind but not another must not hide it (a single call with all three options
    # concatenates, and the format of that output is rpm's to change).
    sections = []
    kinds = []
    for option, name in (("--scripts", "scriptlet"), ("--triggers", "trigger"),
                         ("--filetriggers", "file trigger")):
        got = rpm([*query, option])
        if got.strip():
            kinds.append(name)
            sections.append(f"## rpm {option}\n{got}")
    own = rpm([*query, "--qf", "%{NAME} %{ARCH}"]).split()
    return label, entries, "".join(sections), dependencies(query, label), own, kinds, algo


def file_digest(path, algo):
    h = hashlib.new(DIGEST_ALGOS[algo])
    with open(path, "rb") as f:
        for block in iter(lambda: f.read(1 << 20), b""):
            h.update(block)
    return h.hexdigest()


def check_pinned(entries, pinned, algo, root=ROOT, compare_built=True):
    """The problems of the `pinned` files, as (path, message) pairs. A pinned path the package
    lacks, or holds as another kind, is already a problem of check()'s."""
    problems = []
    by_path = {e["path"]: e for e in entries}
    algo_name = DIGEST_ALGOS.get(algo, f"rpm's algorithm {algo}, which the guard does not know")
    for path, pin in pinned.items():
        e = by_path.get(path)
        if e is None or not stat.S_ISREG(e["mode"]):
            continue
        perms = stat.S_IMODE(e["mode"])
        if perms != pin["mode"]:
            problems.append((path, f"has mode {perms:04o}, the allowlist pins {pin['mode']:04o}"))
        if algo != PIN_ALGO:
            problems.append((path, f"the package's file digests are {algo_name}, not sha256 "
                                   f"(FILEDIGESTALGO {algo}, not {PIN_ALGO}), so its content "
                                   "cannot be compared with its pin: a pinned file is compared "
                                   "in sha256 only"))
            continue
        if "sha256" in pin:
            if e["digest"] != pin["sha256"]:
                problems.append((path, f"its content is not the pinned one: sha256 "
                                       f"{e['digest'] or '(none)'}, the allowlist pins "
                                       f"{pin['sha256']} (a changed file is a reviewed change "
                                       "to its `pinned` entry in ci/rpm-allowlist.toml)"))
        if "built" in pin and compare_built:
            problems += check_built(path, e["digest"], pin["built"], algo, root)
    return problems


def check_built(path, digest, built, algo, root):
    """The package's `path` (its `digest`) is the file this job built at `built`."""
    src = built if os.path.isabs(built) else os.path.join(root, built)
    if not os.path.lexists(src):
        return [(path, f"should be the file this job built, {built}, which is not there "
                       "(ci/build.sh builds it before the bundle)")]
    if os.path.islink(src) or not os.path.isfile(src):
        return [(path, f"should be the file this job built, {built}, which is not a regular "
                       "file")]
    with open(src, "rb") as f:
        magic = f.read(len(ELF_MAGIC))
    problems = []
    if magic != ELF_MAGIC:
        problems.append((path, f"should be an executable, and {built} is not an ELF file "
                               f"(it starts with {magic!r})"))
    want = file_digest(src, algo)
    if digest != want:
        problems.append((path, f"is not the file this job built: its {DIGEST_ALGOS[algo]} is "
                               f"{digest or '(none)'}, {built}'s is {want}"))
    return problems


def check_dependencies(deps, pinned, own):
    problems = []
    name, arch = own
    own_names = {name, f"{name}({arch})"}
    for key in DEP_KEYS:
        got = deps[key]
        if key == "provides":
            for d in sorted(got):
                pname = d.split(" ", 1)[0]
                if pname.startswith(SYSUSERS):
                    problems.append((f"(provides {d})", "creates a system user or group at "
                                     "install (rpm 6 reads user(), group() and groupmember() "
                                     "Provides): never allowed"))
                elif pname not in own_names and d not in pinned[key]:
                    problems.append((f"(provides {d})", "is not the package's own name; "
                                     "allow it only by listing it in [dependencies] provides"))
            continue
        for d in sorted(got - pinned[key]):
            extra = ""
            if key in ("conflicts", "obsoletes"):
                extra = " (dnf removes the packages it names)"
            problems.append((f"({key} {d})", f"is not in [dependencies] {key} in "
                                             f"ci/rpm-allowlist.toml{extra}"))
        for d in sorted(pinned[key] - got):
            problems.append((f"({key} {d})", f"is in the allowlist's [dependencies] {key}, "
                                             "but not in the package"))
    return problems


def check(entries, scripts, allowed, allowed_scriptlets, kinds=("scriptlet",)):
    """The problems, as (path, message) pairs, in package order."""
    problems = []
    seen = set()
    for e in entries:
        path, mode = e["path"], e["mode"]
        seen.add(path)
        kind = kind_of(mode)
        shown = kind + (f" to {e['link']}" if stat.S_ISLNK(mode) and e["link"] else "")
        want = allowed.get(path)
        if want is None:
            problems.append((path, f"{shown} outside the allowlist (ci/rpm-allowlist.toml)"))
        elif kind != want:
            problems.append((path, f"is {shown}, the allowlist expects {want}"))
        perms = stat.S_IMODE(mode)
        writable = [who for bit, who in ((stat.S_IWGRP, "group"), (stat.S_IWOTH, "world"))
                    if perms & bit]
        if writable and not stat.S_ISLNK(mode):
            problems.append((path, f"{' and '.join(writable)}-writable (mode {perms:04o})"))
        special = [name for bit, name in ((stat.S_ISUID, "setuid"), (stat.S_ISGID, "setgid"),
                                          (stat.S_ISVTX, "sticky")) if perms & bit]
        if special:
            problems.append((path, f"has the {', '.join(special)} bit (mode {perms:04o})"))
        if e["caps"]:
            problems.append((path, f"has the file capability {e['caps']}"))
        if e["user"] != "root" or e["group"] != "root":
            problems.append((path, f"is owned by {e['user']}:{e['group']}, not root:root"))
    for path, want in allowed.items():
        if path not in seen:
            problems.append((path, f"is in the allowlist as {want}, but not in the package "
                                   "(remove it from ci/rpm-allowlist.toml, or find out why "
                                   "the bundle lost it)"))
    if scripts.strip():
        digest = hashlib.sha256(scripts.encode()).hexdigest()
        if digest not in allowed_scriptlets:
            shown = scripts.rstrip("\n").replace("\n", "\n    ")
            problems.append(("(scriptlets)",
                             f"the package has {', '.join(kinds)}: it runs code at install, "
                             "upgrade or removal; allow it "
                             f"only by adding \"{digest}\" to `scriptlets` in "
                             f"ci/rpm-allowlist.toml:\n    {shown}"))
    return problems


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("rpm", nargs="?", help="the RPM file to check")
    ap.add_argument("--installed", metavar="NAME", help="check an installed package instead")
    ap.add_argument("--allowlist", default=ALLOWLIST, help="default: ci/rpm-allowlist.toml")
    args = ap.parse_args()
    if bool(args.rpm) == bool(args.installed):
        ap.error("give either an RPM file or --installed NAME")
    if args.rpm:
        if not os.path.isfile(args.rpm):
            ap.error(f"no such file: {args.rpm}")
        query = ["-qp", os.path.abspath(args.rpm)]
    else:
        query = ["-q", args.installed]
    compare_built = not args.installed
    try:
        allowed, allowed_scriptlets, pinned_deps, pinned = load_allowlist(args.allowlist)
        label, entries, scripts, deps, own, kinds, algo = read_package(query)
        problems = check(entries, scripts, allowed, allowed_scriptlets, kinds)
        problems += check_pinned(entries, pinned, algo, compare_built=compare_built)
    except (QueryError, OSError, tomllib.TOMLDecodeError) as e:
        print("rpm_guard: " + str(e).encode("utf-8", "backslashreplace").decode(),
              file=sys.stderr)
        return 2
    problems += check_dependencies(deps, pinned_deps, own)
    for path, msg in problems:
        print(f"rpm_guard: {path}: {msg}", file=sys.stderr)
    if problems:
        print(f"rpm_guard: {label}: REFUSED, {len(problems)} problem(s) in {len(entries)} "
              "entries", file=sys.stderr)
        return 1
    built = [p for p, pin in pinned.items() if "built" in pin]
    if built and not compare_built:
        print(f"rpm_guard: note: --installed compares no built file, so {', '.join(built)} "
              "was checked for its mode only")
    print(f"rpm_guard: {label}: {len(entries)} entries, all in the allowlist, none group- or "
          "world-writable, no setuid/setgid/sticky bit or capability, root:root, "
          f"{len(pinned)} pinned file(s) as pinned, dependencies as pinned, "
          + ("allowed scriptlets" if scripts.strip() else "no scriptlets"))
    return 0


if __name__ == "__main__":
    sys.exit(main())
