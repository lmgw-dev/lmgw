#!/usr/bin/env python3
"""Test of ci/rpm_guard.py on real RPMs, and of ci/rpm-allowlist.toml against the bundle config.

Usage: python3 ci/rpm_guard_test.py   (ci/check.sh runs it)

Builds small sample packages with rpmbuild under target/ (a private %_topdir and HOME, so
neither ~/rpmbuild nor ~/.rpmmacros is touched; no root, no network): a clean one with
exactly the allowlisted files, and one per kind of problem the guard refuses. Then checks
that the destinations tauri-bundler derives from src-tauri/tauri.conf.json are exactly the
allowlist's files, so a new bundle path fails here before it fails a pipeline, and that the
allowlist's `pinned` entries name what tauri.conf.json packs (the policy's sha256 is its
source's, the helper's `built` file is its source). Without rpmbuild the sample packages are
skipped, visibly, and the config checks still run.

The samples stand in for the root helper with a few bytes that start like an ELF file, and
check them against a copy of the allowlist whose `built` names that stand-in; the policy in
them is the real one, so its pinned sha256 is the real pin.
"""

import hashlib
import json
import os
import shutil
import struct
import subprocess
import sys
import tempfile
import tomllib

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
GUARD = os.path.join(ROOT, "ci", "rpm_guard.py")
ALLOWLIST = os.path.join(ROOT, "ci", "rpm-allowlist.toml")
TAURI_CONF = os.path.join(ROOT, "src-tauri", "tauri.conf.json")

HELPER = "/usr/libexec/lmgw/lmgw-machine-helper"
POLICY = "/usr/share/polkit-1/actions/io.github.lmgw-dev.lmgw.machine-setup.policy"
HELPER_BUILT = 'built = "target/release/lmgw-machine-helper"'
# What the samples ship as the helper: an ELF header's first bytes, then something of its own.
FAKE_HELPER = b"\x7fELF\x02\x01\x01\x00 rpm_guard_test stand-in for the root helper\n"

failures = []

# The allowlist the samples are checked against: the real one with the helper's `built`
# pointing at the stand-in (samples() writes it).
SAMPLE_ALLOWLIST = None


def check(cond, what, detail=""):
    print(("ok   " if cond else "FAIL ") + what)
    if not cond:
        failures.append(what)
        if detail:
            print("     " + detail.replace("\n", "\n     ")[:4000])


def allowlisted_files():
    with open(ALLOWLIST, "rb") as f:
        return tomllib.load(f)["files"]


def allowlisted_requires():
    with open(ALLOWLIST, "rb") as f:
        return tomllib.load(f)["dependencies"]["requires"]


def tauri_sources():
    """bundle.linux.rpm.files of src-tauri/tauri.conf.json: destination -> source path,
    absolute."""
    with open(TAURI_CONF) as f:
        files = json.load(f)["bundle"]["linux"]["rpm"].get("files", {})
    conf_dir = os.path.dirname(TAURI_CONF)
    return {dest: os.path.normpath(os.path.join(conf_dir, src)) for dest, src in files.items()}


def policy_bytes():
    with open(tauri_sources()[POLICY], "rb") as f:
        return f.read()


class Builder:
    def __init__(self, tmp):
        self.tmp = tmp
        self.top = os.path.join(tmp, "topdir")
        self.home = os.path.join(tmp, "home")
        os.makedirs(self.home)
        self.n = 0
        # What a file holds unless a test says otherwise; every other file is "x\n".
        self.contents = {HELPER: FAKE_HELPER, POLICY: policy_bytes()}

    def build(self, files, sections="", extra=(), header=(), requires=None, contents=None):
        """files: [(attr, path)], regular files with attr like "0644,root,root" or
        "%caps(...) 0755,root,root"; extra: [(install command, %files line)] for anything else
        (a symlink, a directory); sections: more spec text (scriptlets); header: more tag lines
        (Provides:, Obsoletes:, ...); requires: the Requires names, default the allowlist's;
        contents: {path: bytes} over the defaults (the stand-in helper, the real policy).
        Returns the .rpm."""
        if requires is None:
            requires = allowlisted_requires()
        contents = {**self.contents, **(contents or {})}
        self.n += 1
        release = str(self.n)
        install = ["mkdir -p %{buildroot}"]
        listed = ["%defattr(-,root,root,-)"]
        for i, (attr, path) in enumerate(files):
            install.append(f"mkdir -p %{{buildroot}}{os.path.dirname(path)}")
            if path in contents:
                src = os.path.join(self.tmp, f"content-{release}-{i}")
                with open(src, "wb") as f:
                    f.write(contents[path])
                install.append(f"cp '{src}' '%{{buildroot}}{path}'")
            else:
                install.append(f"printf 'x\\n' > %{{buildroot}}{path}")
            caps = ""
            if attr.startswith("%caps"):
                caps, attr = attr.split(" ", 1)
                caps += " "
            listed.append(f"{caps}%attr({attr}) {path}")
        for command, line in extra:
            install.append(command)
            listed.append(line)
        spec = "\n".join([
            "Name: lmgw",
            "Version: 0.0.1",
            f"Release: {release}",
            "Summary: rpm_guard test package",
            "License: MIT",
            "BuildArch: noarch",
            *[f"Requires: {r}" for r in requires],
            *header,
            "",
            "%description",
            "A sample package for ci/rpm_guard_test.py.",
            "",
            "%install",
            *install,
            "",
            "%files",
            *listed,
            "",
            sections,
            "",
        ])
        spec_path = os.path.join(self.tmp, f"sample-{release}.spec")
        with open(spec_path, "w") as f:
            f.write(spec)
        p = subprocess.run(
            ["rpmbuild", "-bb", "--quiet",
             "--define", f"_topdir {self.top}",
             "--define", f"_tmppath {self.tmp}/tmp",
             "--define", "__os_install_post %{nil}",
             "--define", "_build_id_links none",
             "--define", "debug_package %{nil}",
             # Uncompressed: the samples are tiny and the default zstd level costs seconds.
             "--define", "_binary_payload w.ufdio",
             spec_path],
            env={"PATH": os.environ["PATH"], "HOME": self.home, "LANG": "C.UTF-8", "LC_ALL": "C"},
            capture_output=True, text=True, stdin=subprocess.DEVNULL,
        )
        if p.returncode != 0:
            raise RuntimeError(f"rpmbuild failed for {spec_path}:\n{p.stdout}{p.stderr}")
        rpm = os.path.join(self.top, "RPMS", "noarch", f"lmgw-0.0.1-{release}.noarch.rpm")
        if not os.path.isfile(rpm):
            raise RuntimeError(f"rpmbuild made no {rpm}")
        return rpm


def guard(*args):
    """The guard on `args`, against the samples' allowlist unless `--allowlist` is given."""
    if "--allowlist" not in args and SAMPLE_ALLOWLIST:
        args = ("--allowlist", SAMPLE_ALLOWLIST, *args)
    # LC_ALL=C like the guard's own rpm calls: its checks read rpm's English messages.
    p = subprocess.run([sys.executable, "-I", GUARD, *args], capture_output=True, text=True,
                       stdin=subprocess.DEVNULL,
                       env={"PATH": os.environ["PATH"], "LC_ALL": "C"})
    return p.returncode, p.stdout + p.stderr


def allowlist_text():
    """The samples' allowlist, for a test to change and write elsewhere."""
    with open(SAMPLE_ALLOWLIST) as f:
        return f.read()


def write_allowlist(tmp, name, text):
    path = os.path.join(tmp, name)
    with open(path, "w") as f:
        f.write(text)
    return path


def clean_files():
    return [("0755,root,root" if p in ("/usr/bin/lmgw", HELPER) else "0644,root,root", p)
            for p in allowlisted_files()]


def with_attr(files, path, attr):
    return [(attr if p == path else a, p) for a, p in files]


def samples(tmp):
    global SAMPLE_ALLOWLIST
    stand_in = os.path.join(tmp, "built-helper")
    with open(stand_in, "wb") as f:
        f.write(FAKE_HELPER)
    with open(ALLOWLIST) as f:
        text = f.read()
    check(text.count(HELPER_BUILT) == 1, "the allowlist pins the helper to the file ci/build.sh "
          "builds", HELPER_BUILT)
    SAMPLE_ALLOWLIST = write_allowlist(tmp, "allowlist.toml",
                                       text.replace(HELPER_BUILT, f'built = "{stand_in}"'))
    b = Builder(tmp)
    lic = "/usr/share/licenses/lmgw/silero-vad/LICENSE"

    rc, out = guard(b.build(clean_files()))
    check(rc == 0 and "no scriptlets" in out and "2 pinned file(s) as pinned" in out,
          "a clean package passes", out)

    pinned(b, tmp, stand_in)

    rc, out = guard(b.build(with_attr(clean_files(), lic, "0666,root,root")))
    check(rc == 1 and f"{lic}: group and world-writable (mode 0666)" in out,
          "a 0666 file is refused as group- and world-writable", out)

    rc, out = guard(b.build(with_attr(clean_files(), lic, "0602,root,root")))
    check(rc == 1 and f"{lic}: world-writable (mode 0602)" in out,
          "a world-writable-only file is refused", out)

    rc, out = guard(b.build(with_attr(clean_files(), lic, "0664,root,root")))
    check(rc == 1 and f"{lic}: group-writable (mode 0664)" in out,
          "a group-writable file is refused", out)

    rc, out = guard(b.build(with_attr(clean_files(), "/usr/bin/lmgw", "4755,root,root")))
    check(rc == 1 and "/usr/bin/lmgw: has the setuid bit (mode 4755)" in out,
          "a setuid binary is refused", out)

    rc, out = guard(b.build(with_attr(clean_files(), "/usr/bin/lmgw", "2755,root,root")))
    check(rc == 1 and "/usr/bin/lmgw: has the setgid bit (mode 2755)" in out,
          "a setgid binary is refused", out)

    rc, out = guard(b.build(with_attr(clean_files(), "/usr/bin/lmgw",
                                      "%caps(cap_net_raw=ep) 0755,root,root")))
    check(rc == 1 and "/usr/bin/lmgw: has the file capability cap_net_raw=ep" in out,
          "a file capability is refused", out)

    rc, out = guard(b.build(with_attr(clean_files(), lic, "0644,nobody,nobody")))
    check(rc == 1 and f"{lic}: is owned by nobody:nobody, not root:root" in out,
          "a file not owned by root is refused", out)

    rc, out = guard(b.build(clean_files() + [("0644,root,root", "/etc/lmgw/helper.conf")]))
    check(rc == 1 and "/etc/lmgw/helper.conf: a regular file outside the allowlist" in out,
          "a file outside the allowlist is refused", out)

    rc, out = guard(b.build(clean_files() + [("0755,root,root", "/usr/libexec/lmgw-helper")]))
    check(rc == 1 and "/usr/libexec/lmgw-helper: a regular file outside the allowlist" in out,
          "a new root helper path needs an allowlist edit", out)

    # rpmbuild refuses a tab or a newline in a name, so a space is the oddest it lets through.
    odd = "/usr/share/licenses/lmgw/two words"
    rc, out = guard(b.build(clean_files(), extra=[
        (f"printf 'x\\n' > '%{{buildroot}}{odd}'", f'%attr(0666,root,root) "{odd}"')]))
    entries = len(allowlisted_files()) + 1
    check(rc == 1 and f"{odd}: a regular file outside the allowlist" in out
          and f"{odd}: group and world-writable (mode 0666)" in out
          and f"REFUSED, 2 problem(s) in {entries} entries" in out,
          "a path with a space is read whole", out)

    rc, out = guard("--installed", "lmgw-rpm-guard-test-no-such-package")
    check(rc == 2 and "is not installed" in out, "--installed on a missing package is an error",
          out)

    files = [(a, p) for a, p in clean_files() if p != "/usr/bin/lmgw"]
    symlink = ("mkdir -p %{buildroot}/usr/bin && ln -s ../lib/lmgw/lmgw %{buildroot}/usr/bin/lmgw",
               "/usr/bin/lmgw")
    rc, out = guard(b.build(files, extra=[symlink]))
    check(rc == 1 and "/usr/bin/lmgw: is a symlink to ../lib/lmgw/lmgw, the allowlist expects "
          "a regular file" in out, "a symlink where a regular file is expected is refused", out)

    rc, out = guard(b.build(files))
    check(rc == 1 and "/usr/bin/lmgw: is in the allowlist as a regular file, but not in the "
          "package" in out, "an allowlisted path the package lacks is refused", out)

    post = b.build(clean_files(), sections="%post\necho installed\n")
    rc, out = guard(post)
    check(rc == 1 and "(scriptlets): the package has scriptlet: it runs code" in out
          and "echo installed" in out, "a %post scriptlet is refused, shown and named a "
          "scriptlet", out)

    rc, out = guard(b.build(clean_files(), sections="%posttrans -p /usr/bin/true\n"))
    check(rc == 1 and "(scriptlets)" in out and "/usr/bin/true" in out,
          "a program-only %posttrans is refused", out)

    rc, out = guard(b.build(clean_files(), sections="%triggerin -- bash\necho triggered\n"))
    check(rc == 1 and "the package has trigger:" in out and "echo triggered" in out,
          "a trigger is refused and seen as a trigger, not a scriptlet", out)

    rc, out = guard(b.build(clean_files(), sections="%filetriggerin -- /usr/lib\necho ft\n"))
    check(rc == 1 and "the package has file trigger:" in out and "echo ft" in out,
          "a file trigger is refused and seen as a file trigger alone", out)

    rc, out = guard(b.build(clean_files(), sections="%post\necho a\n%triggerin -- bash\n"
                            "echo b\n%filetriggerin -- /usr/lib\necho c\n"))
    check(rc == 1 and "the package has scriptlet, trigger, file trigger:" in out,
          "a package with one of each kind is seen with all three", out)

    # An explicit allow: the sha256 the refusal named, in a copy of the allowlist.
    rc, out = guard(post)
    digest = out.split('adding "', 1)[1].split('"', 1)[0] if 'adding "' in out else ""
    allow = os.path.join(tmp, "allow-post.toml")
    with open(allow, "w") as f:
        f.write(allowlist_text().replace("scriptlets = []", f'scriptlets = ["{digest}"]')
                .replace('  "libgtk-3.so.0()(64bit)",', '  "libgtk-3.so.0()(64bit)",\n'
                         '  "/bin/sh (post,interp)",'))
    rc, out = guard("--allowlist", allow, post)
    check(rc == 0 and "allowed scriptlets" in out,
          "a scriptlet whose sha256 is allowlisted passes", out)
    rc, out = guard("--allowlist", allow, b.build(clean_files(),
                                                  sections="%post\necho something else\n"))
    check(rc == 1 and "(scriptlets)" in out,
          "a changed scriptlet under that allowlist is refused again", out)

    # Dependency metadata. rpm 6 creates users and groups from user()/group()/groupmember()
    # Provides at install; Obsoletes and Conflicts make dnf remove packages.
    rc, out = guard(b.build(clean_files(), header=["Provides: user(evil)"]))
    check(rc == 1 and "(provides user(evil)): creates a system user or group" in out,
          "a user() Provides is refused (sysusers)", out)
    rc, out = guard(b.build(clean_files(), header=["Provides: group(evil)",
                                                   "Provides: groupmember(evil)"]))
    check(rc == 1 and "(provides group(evil))" in out and "(provides groupmember(evil))" in out,
          "group() and groupmember() Provides are refused", out)
    allow = os.path.join(tmp, "allow-user.toml")
    with open(allow, "w") as f:
        f.write(allowlist_text().replace("provides = []", 'provides = ["user(evil)"]'))
    rc, out = guard("--allowlist", allow, b.build(clean_files(), header=["Provides: user(evil)"]))
    check(rc == 1 and "creates a system user or group" in out,
          "a user() Provides stays refused even when the allowlist lists it", out)
    rc, out = guard(b.build(clean_files(), header=["Obsoletes: polkit"]))
    check(rc == 1 and "(obsoletes polkit): is not in [dependencies] obsoletes" in out
          and "dnf removes" in out, "an Obsoletes is refused", out)
    rc, out = guard(b.build(clean_files(), header=["Conflicts: sudo"]))
    check(rc == 1 and "(conflicts sudo): is not in [dependencies] conflicts" in out,
          "a Conflicts is refused", out)
    rc, out = guard(b.build(clean_files(), requires=allowlisted_requires() + ["curl"]))
    check(rc == 1 and "(requires curl): is not in [dependencies] requires" in out,
          "an extra Requires is refused", out)
    rc, out = guard(b.build(clean_files(), requires=allowlisted_requires()[1:]))
    check(rc == 1 and f"(requires {allowlisted_requires()[0]}): is in the allowlist's" in out,
          "a missing Requires is refused", out)
    rc, out = guard(b.build(clean_files(), requires=allowlisted_requires()[1:],
                            header=["Requires(pre): " + allowlisted_requires()[0]]))
    check(rc == 1 and f"(requires {allowlisted_requires()[0]} (pre))" in out,
          "a Requires with another kind (pre) is not the pinned one", out)
    rc, out = guard(b.build(clean_files(), header=["Provides: libfoo.so.1()(64bit)"]))
    check(rc == 1 and "(provides libfoo.so.1()(64bit)): is not the package's own name" in out,
          "an extra Provides is refused", out)
    rc, out = guard(b.build(clean_files(), header=["Provides: lmgw(noarch)"]))
    check(rc == 0, "the package's own NAME(ARCH) Provides is fine", out)
    for tag in ("Recommends", "Suggests", "Supplements", "Enhances"):
        rc, out = guard(b.build(clean_files(), header=[f"{tag}: bash"]))
        check(rc == 1 and f"({tag.lower()} bash)" in out, f"an extra {tag} is refused", out)
    allow = os.path.join(tmp, "allow-obs.toml")
    with open(allow, "w") as f:
        f.write(allowlist_text().replace("obsoletes = []", 'obsoletes = ["old-lmgw"]'))
    rc, out = guard("--allowlist", allow, b.build(clean_files(), header=["Obsoletes: old-lmgw"]))
    check(rc == 0, "an Obsoletes the allowlist lists passes", out)

    # Directories: only where the allowlist names one, and never writable or sticky.
    allow = os.path.join(tmp, "allow-dir.toml")
    with open(allow, "w") as f:
        f.write(allowlist_text().replace("directories = []",
                                               'directories = ["/usr/lib/lmgw"]'))
    rc, out = guard("--allowlist", allow, dir_sample(b, "0755"))
    check(rc == 0, "an allowlisted 0755 directory passes", out)
    rc, out = guard("--allowlist", allow, dir_sample(b, "1777"))
    check(rc == 1 and "/usr/lib/lmgw: group and world-writable (mode 1777)" in out
          and "/usr/lib/lmgw: has the sticky bit (mode 1777)" in out,
          "a 1777 directory is refused as writable and sticky", out)
    rc, out = guard(dir_sample(b, "0755"))
    check(rc == 1 and "/usr/lib/lmgw: a directory outside the allowlist" in out,
          "a directory the allowlist does not name is refused", out)


def dir_sample(b, mode):
    """The allowlisted files plus an owned directory /usr/lib/lmgw with `mode`."""
    return b.build(clean_files(), extra=[("mkdir -p %{buildroot}/usr/lib/lmgw",
                                          f"%dir %attr({mode},root,root) /usr/lib/lmgw")])


def pinned(b, tmp, stand_in):
    """The `pinned` files: the polkit policy by its sha256, the root helper by the file this job
    built (the stand-in here), each with its exact mode."""
    loose = policy_bytes().replace(b"<allow_any>no</allow_any>", b"<allow_any>yes</allow_any>")
    check(loose != policy_bytes(), "the policy says <allow_any>no</allow_any>")
    rc, out = guard(b.build(clean_files(), contents={POLICY: loose}))
    check(rc == 1 and f"{POLICY}: its content is not the pinned one" in out,
          "a policy whose content says allow_any yes is refused", out)

    rc, out = guard(b.build(with_attr(clean_files(), POLICY, "0600,root,root")))
    check(rc == 1 and f"{POLICY}: has mode 0600, the allowlist pins 0644" in out,
          "a policy of another mode is refused", out)

    rc, out = guard(b.build(with_attr(clean_files(), HELPER, "0700,root,root")))
    check(rc == 1 and f"{HELPER}: has mode 0700, the allowlist pins 0755" in out,
          "a helper of another mode is refused", out)

    rc, out = guard(b.build(clean_files(), contents={HELPER: FAKE_HELPER + b"changed"}))
    check(rc == 1 and f"{HELPER}: is not the file this job built: its sha256 is" in out,
          "a helper that is not the file this job built is refused", out)

    clean = b.build(clean_files())
    gone = write_allowlist(tmp, "allow-gone.toml", allowlist_text().replace(
        stand_in, os.path.join(tmp, "never-built")))
    rc, out = guard("--allowlist", gone, clean)
    check(rc == 1 and f"{HELPER}: should be the file this job built" in out
          and "which is not there" in out, "a helper whose build is missing is refused", out)

    link = os.path.join(tmp, "built-link")
    os.symlink(stand_in, link)
    linked = write_allowlist(tmp, "allow-link.toml", allowlist_text().replace(stand_in, link))
    rc, out = guard("--allowlist", linked, clean)
    check(rc == 1 and "which is not a regular file" in out,
          "a built file that is a symlink is refused", out)

    script = b"#!/bin/sh\necho not an ELF file\n"
    not_elf = os.path.join(tmp, "built-script")
    with open(not_elf, "wb") as f:
        f.write(script)
    allow = write_allowlist(tmp, "allow-script.toml", allowlist_text().replace(stand_in, not_elf))
    rc, out = guard("--allowlist", allow, b.build(clean_files(), contents={HELPER: script}))
    check(rc == 1 and f"{HELPER}: should be an executable" in out
          and "is not an ELF file" in out, "a helper that is no ELF file is refused", out)

    # The allowlist itself: a pin it cannot honour is an error (exit 2), not a pass.
    for name, old, new, said in [
        ("unlisted", f'[pinned."{POLICY}"]', '[pinned."/usr/share/polkit-1/actions/x.policy"]',
         "is not a regular file listed under `files`"),
        ("no-content", f'sha256 = "{hashlib.sha256(policy_bytes()).hexdigest()}"', "",
         "pins no content"),
        ("mode", 'mode = "0644"', 'mode = "644"', "is not four octal digits"),
        ("sha", f'sha256 = "{hashlib.sha256(policy_bytes()).hexdigest()}"', 'sha256 = "abc"',
         "sha256 is not 64 lowercase hex digits"),
        ("unknown", 'mode = "0644"', 'mode = "0644"\nowner = "root"', "unknown keys ['owner']"),
        ("unpinned", f'[pinned."{POLICY}"]', '[pinned."/usr/bin/lmgw"]',
         f"{POLICY} is under /usr/libexec/, /usr/share/polkit-1/ but not pinned"),
    ]:
        text = allowlist_text()
        check(old in text, f"the allowlist has {old!r} to change ({name})")
        allow = write_allowlist(tmp, f"allow-{name}.toml", text.replace(old, new, 1))
        rc, out = guard("--allowlist", allow, clean)
        check(rc == 2 and said in out, f"an allowlist with a bad pin is an error ({name})", out)


def pinned_unit_check():
    """check_pinned on stated entries: a pinned file is compared in sha256 alone, so digests in
    MD5 or SHA-1 (which collide on purpose; MD5 is rpm's for old packages) fail every pinned
    file, the build's included, as do a stronger algorithm and one the guard does not know;
    sha256 digests compare with the build and the pin."""
    g = guard_module()
    os.makedirs(os.path.join(ROOT, "target"), exist_ok=True)
    tmp = tempfile.mkdtemp(prefix="rpm-guard-unit-", dir=os.path.join(ROOT, "target"))
    try:
        built = os.path.join(tmp, "helper")
        with open(built, "wb") as f:
            f.write(FAKE_HELPER)
        pins = {HELPER: {"mode": 0o755, "built": built},
                POLICY: {"mode": 0o644, "sha256": hashlib.sha256(b"x").hexdigest()}}
        for algo, name in ((1, "md5"), (2, "sha1"), (10, "sha512")):
            entries = [
                {"path": HELPER, "mode": 0o100755,
                 "digest": hashlib.new(name, FAKE_HELPER).hexdigest()},
                {"path": POLICY, "mode": 0o100644, "digest": hashlib.new(name, b"x").hexdigest()},
            ]
            got = g.check_pinned(entries, pins, algo)
            check(sorted(p for p, _ in got) == sorted([HELPER, POLICY])
                  and all(f"{name}, not sha256" in m for _, m in got),
                  f"{name} digests fail every pinned file, the build's matching one included",
                  repr(got))
            got = g.check_pinned(entries, pins, algo, compare_built=False)
            check(len(got) == 2, f"--installed refuses {name} digests too", repr(got))
        entries = [
            {"path": HELPER, "mode": 0o100755, "digest": hashlib.md5(FAKE_HELPER).hexdigest()},
            {"path": POLICY, "mode": 0o100644, "digest": hashlib.md5(b"x").hexdigest()},
        ]
        got = g.check_pinned(entries, pins, 99)
        check(len(got) == 2 and all("does not know" in m for _, m in got),
              "an unknown digest algorithm compares nothing and says so", repr(got))
        entries = [
            {"path": HELPER, "mode": 0o100755, "digest": hashlib.sha256(FAKE_HELPER).hexdigest()},
            {"path": POLICY, "mode": 0o100644, "digest": hashlib.sha256(b"x").hexdigest()},
        ]
        got = g.check_pinned(entries, pins, 8)
        check(got == [], "sha256 digests that match the build and the pin pass", repr(got))
        entries[0]["digest"] = "0" * 64
        got = g.check_pinned(entries, pins, 8)
        check([p for p, _ in got] == [HELPER] and "not the file this job built" in got[0][1],
              "a sha256 that is not the build's fails", repr(got))
        got = g.check_pinned(entries, pins, 8, compare_built=False)
        check(got == [], "--installed compares no built file", repr(got))
    finally:
        shutil.rmtree(tmp)


def pinned_config_check():
    """The allowlist's pins name what src-tauri/tauri.conf.json packs: the policy's sha256 is
    its source's, the helper's `built` is its source."""
    with open(ALLOWLIST, "rb") as f:
        pins = tomllib.load(f).get("pinned", {})
    sources = tauri_sources()
    for dest, pin in pins.items():
        src = sources.get(dest)
        if src is None:
            check(False, f"tauri.conf.json packs the pinned {dest}")
            continue
        if "sha256" in pin:
            with open(src, "rb") as f:
                have = hashlib.sha256(f.read()).hexdigest()
            check(have == pin["sha256"], f"the pinned sha256 of {dest} is its source's",
                  f"{os.path.relpath(src, ROOT)}: {have}\nallowlist: {pin['sha256']}\n"
                  "a changed file is a reviewed change to its pin")
        if "built" in pin:
            check(os.path.normpath(os.path.join(ROOT, pin["built"])) == src,
                  f"the pinned built file of {dest} is what tauri.conf.json packs",
                  f"built: {pin['built']}, packed: {os.path.relpath(src, ROOT)}")
    check(set(pins) == {HELPER, POLICY}, "the helper and its policy are pinned",
          f"pinned: {sorted(pins)}")


def png_size(path):
    with open(path, "rb") as f:
        head = f.read(24)
    if head[:8] != b"\x89PNG\r\n\x1a\n" or head[12:16] != b"IHDR":
        raise ValueError(f"{path} is not a PNG")
    return struct.unpack(">II", head[16:24])


def bundle_destinations():
    """What tauri-bundler (tauri-cli 2.12.0, bundle/linux/rpm.rs and freedesktop.rs) installs
    for src-tauri/tauri.conf.json: the main binary, the .desktop file, one hicolor icon per PNG
    in bundle.icon and the destinations of bundle.linux.rpm.files (a directory source adds each
    file under it)."""
    with open(TAURI_CONF) as f:
        conf = json.load(f)
    name = conf.get("mainBinaryName") or conf["productName"]
    bundle = conf["bundle"]
    dests = {f"/usr/bin/{name}", f"/usr/share/applications/{name}.desktop"}
    conf_dir = os.path.dirname(TAURI_CONF)
    for icon in bundle.get("icon", []):
        if not icon.endswith(".png"):
            continue
        w, h = png_size(os.path.join(conf_dir, icon))
        hd = "@2" if os.path.splitext(os.path.basename(icon))[0].endswith("@2x") else ""
        dests.add(f"/usr/share/icons/hicolor/{w}x{h}{hd}/apps/{name}.png")
    for dest, src in bundle.get("linux", {}).get("rpm", {}).get("files", {}).items():
        src = os.path.join(conf_dir, src)
        if os.path.isdir(src):
            for d, _, names in os.walk(src):
                for n in names:
                    dests.add(os.path.join(dest, os.path.relpath(os.path.join(d, n), src)))
        else:
            dests.add(dest)
    for key in ("preInstallScript", "postInstallScript", "preRemoveScript", "postRemoveScript"):
        if bundle.get("linux", {}).get("rpm", {}).get(key):
            dests.add(f"(scriptlet: bundle.linux.rpm.{key})")
    if bundle.get("resources") or bundle.get("externalBin"):
        dests.add("(bundle.resources / bundle.externalBin: more paths under /usr)")
    return dests


def depends_check():
    with open(TAURI_CONF) as f:
        depends = json.load(f)["bundle"]["linux"]["rpm"].get("depends", [])
    rpm_conf = json.load(open(TAURI_CONF))["bundle"]["linux"]["rpm"]
    other = [k for k in ("provides", "obsoletes", "conflicts", "recommends", "suggests",
                         "supplements", "enhances") if rpm_conf.get(k)]
    # The sonames are what tauri-bundler adds itself; everything else is `depends`.
    requires = allowlisted_requires()
    sonames = {r for r in requires if ".so." in r}
    check(set(requires) - sonames == set(depends) and set(depends) <= set(requires),
          "the allowlist's requires are tauri.conf.json's depends plus the bundler's sonames",
          f"depends: {sorted(depends)}\nrequires: {sorted(requires)}")
    check(not other, "tauri.conf.json asks for no provides/obsoletes/conflicts/... "
          "(the allowlist pins them empty)", f"set in the config: {other}")


def guard_module():
    """ci/rpm_guard.py as a module, for checks on its functions with a faked rpm."""
    import importlib.util
    spec = importlib.util.spec_from_file_location("rpm_guard", GUARD)
    g = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(g)
    return g


def non_utf8_check():
    """A file name that is not UTF-8 stops the guard with a QueryError (exit 2), not a
    traceback. rpmbuild cannot make such a name, so rpm's answer is faked."""
    g = guard_module()
    check(g.text(b"/usr/bin/\xff") == "/usr/bin/\udcff", "rpm output decodes with surrogateescape")

    def fake(args):
        if "--qf" not in args:
            return ""
        qf = args[args.index("--qf") + 1]
        if qf == "%{NVRA}":
            return "x-1.noarch"
        if "arraysize" in qf:
            return "1"
        if qf == g.QUERY:
            return "'100644' 'root' 'root' '(none)' '(none)' '' '/usr/bin/\udcff'\n"
        return ""
    g.rpm = fake
    try:
        g.read_package(["-q", "x"])
        check(False, "a non-UTF-8 file name is a QueryError")
    except g.QueryError as e:
        check("not valid UTF-8" in str(e), "a non-UTF-8 file name is a QueryError (exit 2)",
              str(e))


def config_check():
    want = bundle_destinations()
    have = set(allowlisted_files())
    check(want == have, "ci/rpm-allowlist.toml lists exactly what tauri.conf.json bundles",
          f"in the config, not the allowlist: {sorted(want - have)}\n"
          f"in the allowlist, not the config: {sorted(have - want)}")


def main():
    config_check()
    pinned_config_check()
    depends_check()
    non_utf8_check()
    pinned_unit_check()
    if shutil.which("rpmbuild") is None:
        print("SKIP sample RPMs: rpmbuild is not installed (dnf install rpm-build)")
    else:
        os.makedirs(os.path.join(ROOT, "target"), exist_ok=True)
        tmp = tempfile.mkdtemp(prefix="rpm-guard-test-", dir=os.path.join(ROOT, "target"))
        try:
            samples(tmp)
        finally:
            shutil.rmtree(tmp)
    if failures:
        print(f"\n{len(failures)} check(s) failed")
        sys.exit(1)
    print("\nall rpm guard checks passed")


if __name__ == "__main__":
    main()
