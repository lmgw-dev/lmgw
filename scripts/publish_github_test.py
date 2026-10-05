#!/usr/bin/env python3
"""Self-contained test of scripts/publish-github.sh and its pre-push hook: temp repos under
target/, a local bare repo as the fake GitHub, no network.

Usage: python3 scripts/publish_github_test.py   (ci/check.sh runs it)

The private repo's history starts at its own "pre-public root" (the hook template's root sha is
replaced by it), the public repo is a separate lineage, and the private repo's `origin` names a
host that is never contacted (GIT_SSH_COMMAND=false makes sure of it). Git's global and system
config are shut out, so the owner's settings change nothing here.
"""

import os
import shutil
import subprocess
import sys
import tempfile

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SCRIPTS = ["publish-github.sh", "publish_scan.py", "pre-push-github.sh"]
REAL_ROOT = "27871c7e621eec8795e34f811d16e7812d68a019"
PRIVATE_HOST = "git.private-host.test"
# Built at run time, so this file carries no address-shaped string of its own.
LEAK_MAIL = "someone" + "@" + "corp-mail.io"

failures = []


def check(cond, what, detail=""):
    print(("ok   " if cond else "FAIL ") + what)
    if not cond:
        failures.append(what)
        if detail:
            print("     " + detail.replace("\n", "\n     ")[:4000])


class World:
    def __init__(self, tmp):
        self.tmp = tmp
        self.bare = os.path.join(tmp, "github.com", "lmgw.git")
        self.src = os.path.join(tmp, "private")
        cfg = os.path.join(tmp, "config")
        os.makedirs(os.path.join(cfg, "lmgw"))
        with open(os.path.join(cfg, "lmgw", "publish-denylist"), "w") as f:
            f.write("# test words\nsecretword\n\\bdevicename\\b\n")
        open(os.path.join(tmp, "gitconfig"), "w").close()
        self.env = {
            "PATH": os.environ["PATH"],
            "HOME": os.path.join(tmp, "home"),
            "XDG_CONFIG_HOME": cfg,
            "GIT_CONFIG_GLOBAL": os.path.join(tmp, "gitconfig"),
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_SSH_COMMAND": "false",
            "GIT_TERMINAL_PROMPT": "0",
            "LMGW_PUBLISH_URL": self.bare,
            "LANG": "C.UTF-8",
        }
        os.makedirs(self.env["HOME"])

    def git(self, *args, cwd=None, env=None, check_ok=True):
        p = subprocess.run(
            ["git", *args],
            cwd=cwd or self.src,
            env={**self.env, **(env or {})},
            capture_output=True,
            text=True,
            stdin=subprocess.DEVNULL,
        )
        if check_ok and p.returncode != 0:
            raise RuntimeError(f"git {' '.join(args)}: {p.stderr}")
        return p

    def rev(self, what, cwd=None):
        return self.git("rev-parse", what, cwd=cwd).stdout.strip()

    def commit(self, path, text, message):
        full = os.path.join(self.src, path)
        os.makedirs(os.path.dirname(full), exist_ok=True)
        with open(full, "w") as f:
            f.write(text)
        self.git("add", path)
        self.git("commit", "-q", "-m", message)
        return self.rev("HEAD")

    def publish(self, *args, answer=None):
        """Run the script; with `answer`, on a pseudo-terminal that types it."""
        cmd = ["bash", "scripts/publish-github.sh", *args]
        if answer is None:
            p = subprocess.run(
                cmd, cwd=self.src, env=self.env, capture_output=True, text=True,
                stdin=subprocess.DEVNULL,
            )
            return p.returncode, p.stdout + p.stderr
        master, slave = os.openpty()
        try:
            p = subprocess.Popen(
                cmd, cwd=self.src, env=self.env, stdin=slave,
                stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
            )
            os.close(slave)
            os.write(master, (answer + "\n").encode())
            out, _ = p.communicate(timeout=120)
        finally:
            os.close(master)
        return p.returncode, out.decode()

    def message(self, name, text):
        path = os.path.join(self.tmp, name)
        with open(path, "w") as f:
            f.write(text)
        return path

    def public_main(self):
        return self.git("rev-parse", "main", cwd=self.bare).stdout.strip()


def setup(tmp):
    w = World(tmp)
    # The public repo: one commit of its own lineage.
    seed = os.path.join(tmp, "seed")
    os.makedirs(seed)
    w.git("init", "-q", "-b", "main", cwd=seed)
    for k, v in [("user.name", "Tester"), ("user.email", "1+tester@users.noreply.github.com")]:
        w.git("config", k, v, cwd=seed)
    with open(os.path.join(seed, "README.md"), "w") as f:
        f.write("public v1\n")
    w.git("add", "README.md", cwd=seed)
    w.git("commit", "-q", "-m", "public v1", cwd=seed)
    w.git("init", "-q", "--bare", "-b", "main", w.bare, cwd=tmp)
    w.git("push", "-q", w.bare, "main", cwd=seed)
    w.p0 = w.public_main()

    # The private repo: starts at its pre-public root, carries the scripts under test.
    os.makedirs(w.src)
    w.git("init", "-q", "-b", "main")
    for k, v in [("user.name", "Tester"), ("user.email", "1+tester@users.noreply.github.com")]:
        w.git("config", k, v)
    w.old_root = w.commit("old.txt", "pre-public\n", "pre-public root")
    os.makedirs(os.path.join(w.src, "scripts"))
    for name in SCRIPTS:
        shutil.copy2(os.path.join(ROOT, "scripts", name), os.path.join(w.src, "scripts", name))
    hook = os.path.join(w.src, "scripts", "pre-push-github.sh")
    text = open(hook).read()
    assert REAL_ROOT in text, "the hook template names its root sha"
    with open(hook, "w") as f:
        f.write(text.replace(REAL_ROOT, w.old_root))
    with open(os.path.join(w.src, ".gitignore"), "w") as f:
        f.write("/target\n")
    w.git("add", "scripts", ".gitignore")
    w.git("commit", "-q", "-m", "scripts")
    w.commit("README.md", "public v2\n", "readme")
    w.git("remote", "add", "github", w.bare)
    w.git("remote", "add", "origin", f"ssh://git@{PRIVATE_HOST}:2222/team/proj.git")
    return w


def approve(w, tree, *extra):
    return w.publish("--approve", tree, "--note", "test audit", *extra)


def main():
    os.makedirs(os.path.join(ROOT, "target"), exist_ok=True)
    tmp = tempfile.mkdtemp(prefix="publish-test-", dir=os.path.join(ROOT, "target"))
    try:
        run(setup(tmp))
    finally:
        shutil.rmtree(tmp)
    if failures:
        print(f"\n{len(failures)} check(s) failed")
        sys.exit(1)
    print("\nall publish checks passed")


def run(w):
    # --- the guard ------------------------------------------------------------------------------
    rc, out = w.publish("--install-guard")
    check(rc == 0, "--install-guard installs the hook", out)
    check(w.git("config", "remote.github.pushurl").stdout.strip().startswith("DISABLED"),
          "--install-guard disables the github push URL")
    check(w.git("config", "remote.github.tagOpt").stdout.strip() == "--no-tags",
          "--install-guard turns off github tag auto-follow")
    rc, out = w.publish("--install-guard")
    check(rc == 0, "--install-guard is idempotent", out)

    p = w.git("push", w.bare, "main:refs/heads/side", check_ok=False)
    check(p.returncode != 0 and "refusing to push to GitHub" in p.stderr,
          "the hook refuses a push to GitHub without LMGW_PUBLISH", p.stderr)
    p = w.git("push", w.bare, "main:refs/heads/side", env={"LMGW_PUBLISH": "1"}, check_ok=False)
    check(p.returncode != 0 and "pre-public history" in p.stderr,
          "the hook refuses the pre-public history even with LMGW_PUBLISH=1", p.stderr)
    p = w.git("push", "github", "main", check_ok=False)
    check(p.returncode != 0, "git push github fails on the disabled push URL", p.stderr)
    check(w.public_main() == w.p0, "nothing reached the fake GitHub so far")

    # --- gate: no approval, a scan hit, a mismatched tree --------------------------------------
    msg = w.message("msg.txt", "Publish the readme\n")
    rc, out = w.publish("-F", msg)
    check(rc == 3 and "no approval for tree" in out, "the gate refuses without an approval", out)
    tree = w.rev("main^{tree}")
    t12 = tree[:12]
    audit = os.path.join(w.src, "target", "publish")
    check(os.path.isfile(os.path.join(audit, t12 + ".diff")), "the diff is written for the audit")
    rc, out = w.publish("--dry-run", "-F", msg)
    check(rc == 3 and "would refuse" in out and "push target:" in out,
          "a dry run shows the commit and what the gate would refuse", out)

    w.commit("docs/leak.md", f"contact {LEAK_MAIL}\nthe secretword is here\n", "leak")
    tree = w.rev("main^{tree}")
    t12 = tree[:12]
    rc, out = w.publish("--dry-run", "-F", msg)
    scan = open(os.path.join(audit, t12 + ".scan")).read()
    check("e-mail address" in scan and "denylist line 2" in scan,
          "the scan finds the address and the private word", scan)
    rc, out = approve(w, tree)
    check(rc != 0 and "--ack-scan" in out, "an approval with scan hits needs --ack-scan", out)
    rc, out = approve(w, tree[:12])
    check(rc != 0 and "full tree sha" in out, "an approval takes the full sha only", out)
    rc, out = approve(w, tree, "--ack-scan")
    check(rc == 0, "an approval with --ack-scan is recorded", out)

    # The denylist grows: the approved scan is no longer the scan.
    deny = os.path.join(w.env["XDG_CONFIG_HOME"], "lmgw", "publish-denylist")
    with open(deny, "a") as f:
        f.write("contact\n")
    rc, out = w.publish("--dry-run", "-F", msg)
    check(rc == 3 and "scan is not the one approved" in out,
          "a changed scan invalidates the approval", out)
    with open(deny, "w") as f:
        f.write("# test words\nsecretword\n\\bdevicename\\b\n")

    old_tree = tree
    w.commit("docs/later.md", "a later change\n", "later")
    tree = w.rev("main^{tree}")
    shutil.copy(os.path.join(audit, old_tree + ".approved"), os.path.join(audit, tree + ".approved"))
    rc, out = w.publish("--dry-run", "-F", msg)
    check(rc == 3 and "names another tree" in out,
          "an approval for another tree does not open the gate", out)
    os.remove(os.path.join(audit, tree + ".approved"))
    rc, out = w.publish("--dry-run", "-F", msg)
    check(rc == 3 and "no approval for tree " + tree in out,
          "the old tree's approval does not cover the new tree", out)

    # --- the messages and the identity ----------------------------------------------------------
    rc, _ = approve(w, tree, "--ack-scan")
    bad = w.message("bad.txt", f"Synced from {PRIVATE_HOST}\n")
    rc, out = w.publish("--dry-run", "-F", bad)
    check(rc == 3 and "message scan has" in out, "a private remote host in the message is refused", out)
    bad = w.message("bad2.txt", "our devicename build\n")
    rc, out = w.publish("--dry-run", "-F", bad)
    check(rc == 3 and "message scan has" in out, "a denylisted word in the message is refused", out)
    w.git("config", "user.email", LEAK_MAIL)
    rc, out = w.publish("--dry-run", "-F", msg)
    check(rc == 3 and "no-reply address" in out, "a personal e-mail identity is refused", out)
    w.git("config", "user.email", "1+tester@users.noreply.github.com")

    # --- the publish ----------------------------------------------------------------------------
    rc, out = w.publish("--dry-run", "-F", msg)
    check(rc == 0 and "gate: open" in out, "the approved tree passes a dry run", out)
    check(w.public_main() == w.p0, "a dry run pushes nothing")
    rc, out = w.publish("-F", msg, answer="no")
    check(rc != 0 and "not published" in out and w.public_main() == w.p0,
          "anything but 'publish' at the prompt publishes nothing", out)
    rc, out = w.publish("-F", msg, answer="publish")
    pub = w.public_main()
    check(rc == 0 and pub != w.p0, "the publish pushes one commit", out)
    pub_tree = w.git("rev-parse", pub + "^{tree}", cwd=w.bare).stdout.strip()
    check(pub_tree == tree, "the public commit's tree is exactly the private tree")
    parents = w.git("rev-list", "--parents", "-n", "1", pub, cwd=w.bare).stdout.split()[1:]
    check(parents == [w.p0], "its one parent is the public tip", str(parents))
    has_old = w.git("cat-file", "-e", w.old_root, cwd=w.bare, check_ok=False).returncode == 0
    check(not has_old, "the private history never reached the fake GitHub")
    mapping = open(os.path.join(w.src, ".git", "info", "publish-map")).read().split()
    check(mapping[:2] == [w.rev("main"), pub], "the publish is recorded in publish-map", str(mapping))
    rc, out = w.publish("-F", msg)
    check(rc == 0 and "nothing to publish" in out, "a second run has nothing to publish", out)

    # --- tags land on the public commit ---------------------------------------------------------
    w.git("tag", "-a", "v0.1.0", "-m", "Release notes 0.1.0")
    rc, out = w.publish("--tag", "v0.1.0", answer="publish")
    check(rc == 0, "a tag on an already public tree is published", out)
    peeled = w.git("rev-parse", "v0.1.0^{commit}", cwd=w.bare).stdout.strip()
    check(peeled == pub, "the public tag is on the public commit, not the private one")
    body = w.git("cat-file", "tag", "v0.1.0", cwd=w.bare).stdout
    check("Release notes 0.1.0" in body, "the public tag carries the release notes", body)

    w.commit("CHANGES.md", "0.2.0\n", "release 0.2.0")
    w.git("tag", "-a", "v0.2.0", "-m", "Release notes 0.2.0")
    w.commit("after.md", "after the tag\n", "after")
    rc, out = w.publish("-F", msg, "--tag", "v0.2.0", "--dry-run")
    check(rc != 0 and "--ref v0.2.0" in out, "a tag on another tree than the ref's is refused", out)
    tree = w.rev("v0.2.0^{tree}")
    w.publish("--dry-run", "--ref", "v0.2.0", "-F", msg, "--tag", "v0.2.0")
    approve(w, tree, "--ack-scan")
    rc, out = w.publish("--ref", "v0.2.0", "-F", msg, "--tag", "v0.2.0", answer="publish")
    pub2 = w.public_main()
    check(rc == 0 and pub2 != pub, "a new commit and its tag go out in one push", out)
    check(w.git("rev-parse", "v0.2.0^{commit}", cwd=w.bare).stdout.strip() == pub2,
          "the new tag is on the new public commit")
    check(w.git("rev-parse", pub2 + "^{tree}", cwd=w.bare).stdout.strip() == tree,
          "the new public commit has the tag's tree")

    # A late tag on a tree published earlier lands on that earlier public commit, and an older
    # state is never copied onto the tip.
    first_src = mapping[0]
    w.git("tag", "-a", "v0.1.9", "-m", "Release notes 0.1.9", first_src)
    rc, out = w.publish("--ref", "v0.1.9", "--tag", "v0.1.9", answer="publish")
    check(rc == 0 and w.public_main() == pub2, "a late tag pushes no commit", out)
    check(w.git("rev-parse", "v0.1.9^{commit}", cwd=w.bare).stdout.strip() == pub,
          "a late tag lands on the public commit that carries its tree")
    rc, out = w.publish("--ref", first_src, "-F", msg, "--dry-run")
    check(rc != 0 and "would revert" in out, "an older state than a published one is refused", out)


if __name__ == "__main__":
    main()
