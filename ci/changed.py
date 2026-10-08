#!/usr/bin/env python3
"""Work out what `ci/check.sh --changed [base]` runs for the diff against base.

Usage: python3 ci/changed.py [base | A..B]   (base defaults to the merge-base with main)

Prints the selection and the reason for every part of it on stderr, and the
plan as shell assignments on stdout, which ci/check.sh evals:

  CHANGED_CRATES   packages the diff touches (cargo package names)
  CHANGED_ALL      1 when a workspace-wide file changed (Cargo.lock, ...)
  RUN_TRUNK        1 when the dashboard bundle has to be rebuilt
  RUN_CLIENT_WASM  1 when lmgw-client (or the types it builds on) has to be
                   checked for wasm32 on its own
  RUN_WORKLET      1 when the audio worklets changed
  RUN_PUBLISH      1 when scripts/publish-github.sh or its helpers changed
  NEXTEST_FILTER   the nextest filterset for the selected tests

The it-module half reads ci/it-map.toml; its header says how the naming
convention maps a source path to modules and what the file adds to it.
"""

import fnmatch
import json
import os
import re
import shlex
import subprocess
import sys
import tomllib

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
IT_DIR = "crates/lmgw-core/tests/it"
CORE = "lmgw-core"
CORE_SRC = "crates/lmgw-core/src/"
UI_DIR = "crates/lmgw-ui/"
CLIENT_DIR = "crates/lmgw-client/"

# Files that change how every crate builds: the selection is then everything.
WORKSPACE_WIDE = ["Cargo.toml", "Cargo.lock", "clippy.toml", "rust-toolchain", "rust-toolchain.toml",
                  ".cargo/*"]


def say(msg=""):
    print(msg, file=sys.stderr)


def git(*args):
    r = subprocess.run(["git", *args], cwd=ROOT, capture_output=True, text=True)
    if r.returncode != 0:
        sys.exit(f"ci/changed.py: git {' '.join(args)} failed: {r.stderr.strip()}")
    return r.stdout


def changed_files(base):
    """The diff from base to the working tree, untracked files included; or,
    for an `A..B` range, the files those commits touch (the selection is
    taken from the range, the tests still build the working tree)."""
    if ".." in base:
        return sorted(f for f in git("diff", "--name-only", base).split("\n") if f)
    tracked = git("diff", "--name-only", base).split("\n")
    untracked = git("ls-files", "--others", "--exclude-standard").split("\n")
    return sorted({f for f in tracked + untracked if f})


def workspace_packages():
    """{package name: repo-relative dir} and {package: [workspace dependents]}."""
    meta = json.loads(subprocess.run(
        ["cargo", "metadata", "--no-deps", "--format-version", "1"], cwd=ROOT, check=True,
        capture_output=True, text=True).stdout)
    dirs, deps = {}, {}
    for p in meta["packages"]:
        d = os.path.relpath(os.path.dirname(p["manifest_path"]), ROOT)
        dirs[p["name"]] = d + "/"
        deps[p["name"]] = {x["name"] for x in p["dependencies"] if x.get("path")}
    dependents = {n: sorted(m for m in deps if n in deps[m]) for n in dirs}
    return dirs, dependents


def it_modules():
    """The `mod` lines of tests/it/main.rs, minus the shared helpers."""
    text = open(os.path.join(ROOT, IT_DIR, "main.rs")).read()
    mods = re.findall(r"^mod (\w+);", text, re.M)
    return [m for m in mods if m not in ("common", "support")]


def expand(entry, modules):
    """A map/smoke entry → module names (or module::test paths, kept as is)."""
    if entry == "*":
        return list(modules)
    if "::" in entry:
        return [entry]
    if entry.endswith("_"):
        return [m for m in modules if m.startswith(entry)]
    return [entry] if entry in modules else []


def convention(path, modules, skip):
    """src/<a>/<b>.rs → the modules named after its first non-skipped part."""
    rel = path[len(CORE_SRC):]
    parts = [p for p in re.sub(r"\.rs$", "", rel).split("/") if p not in ("mod", "lib")]
    parts = [p for p in parts if p not in skip] or parts
    if not parts:
        return [], None
    name = parts[0]
    while name:
        hits = [m for m in modules if m == name or m.startswith(name + "_")]
        if hits:
            return hits, name
        name = name.rpartition("_")[0]
    return [], None


def users_of(helper, modules):
    """The it modules whose source names `support::<helper>` / `common::<helper>`."""
    hits = []
    pat = re.compile(r"\b(support|common)::" + re.escape(helper) + r"\b")
    for m in modules:
        for f in module_files(m):
            if pat.search(open(f).read()):
                hits.append(m)
                break
    return hits


def module_files(m):
    base = os.path.join(ROOT, IT_DIR, m)
    files = [base + ".rs"] if os.path.exists(base + ".rs") else []
    if os.path.isdir(base):
        for d, _, fs in os.walk(base):
            files += [os.path.join(d, f) for f in fs if f.endswith(".rs")]
    return files


def mentioning(word, modules):
    return [m for m in modules if any(word in open(f).read() for f in module_files(m))]


def dist_stale():
    """Is crates/lmgw-ui/dist older than the sources trunk builds it from?"""
    index = os.path.join(ROOT, UI_DIR, "dist/index.html")
    if not os.path.exists(index):
        return "crates/lmgw-ui/dist/index.html does not exist"
    built = os.path.getmtime(index)
    sources = git("ls-files", "crates/lmgw-ui", "crates/lmgw-api-types", "crates/lmgw-client").split("\n")
    newer = [f for f in sources if f and os.path.exists(os.path.join(ROOT, f))
             and os.path.getmtime(os.path.join(ROOT, f)) > built]
    if newer:
        return f"dist/ is older than {newer[0]}" + (f" and {len(newer) - 1} more" if len(newer) > 1
                                                     else "")
    return None


def main():
    base = sys.argv[1] if len(sys.argv) > 1 else git("merge-base", "HEAD", "main").strip()
    short = "..".join(git("rev-parse", "--short", r or "HEAD").strip() for r in base.split(".."))
    files = changed_files(base)
    cfg = tomllib.load(open(os.path.join(ROOT, "ci/it-map.toml"), "rb"))
    skip = set(cfg.get("skip", []))
    dirs, dependents = workspace_packages()
    modules = it_modules()

    what = "in the range" if ".." in base else "differ from"
    extra = "" if ".." in base else ", untracked included"
    say(f"--changed: {len(files)} file(s) {what} {base} ({short}){extra}")
    for f in files:
        say(f"    {f}")

    touched = {}  # crate → first file that touched it
    beyond_tests = set()  # crates touched outside their tests/ (dependents rebuild)
    wide = [f for f in files if any(fnmatch.fnmatch(f, p) for p in WORKSPACE_WIDE)]
    for f in files:
        for name, d in sorted(dirs.items(), key=lambda kv: -len(kv[1])):
            if f.startswith(d):
                touched.setdefault(name, f)
                if not f.startswith(d + "tests/"):
                    beyond_tests.add(name)
                break
    if wide:
        touched = {n: wide[0] for n in dirs}

    say("\ncrates (clippy and the build run --workspace; cargo itself re-checks only what the "
        "diff dirtied):")
    if wide:
        say(f"    every crate: {wide[0]} is workspace-wide")
    elif not touched:
        say("    none touched")
    for name in sorted(touched):
        if not wide:
            say(f"    {name} (first touched by {touched[name]})")
        dep = [d for d in dependents[name] if d not in touched]
        if dep and (wide or name in beyond_tests):
            say(f"      its dependents {', '.join(dep)} are re-checked and rebuilt by cargo "
                f"(not tested unless touched)")

    # --- it modules -----------------------------------------------------------------
    picked = {}  # module (or module::test) → reason

    def pick(entries, why):
        for e in entries:
            picked.setdefault(e, why)

    unmatched = []
    if wide:
        pick(modules, "workspace-wide change")
    for f in files:
        if wide:
            break
        hit = False
        for rule in cfg.get("map", []):
            if any(fnmatch.fnmatch(f, p) for p in rule["paths"]):
                got = [x for e in rule["modules"] for x in expand(e, modules)]
                pick(got, f"it-map.toml: {f} ({rule.get('why', 'mapped')})")
                hit = True
        if f.startswith(CORE_SRC):
            got, name = convention(f, modules, skip)
            if got:
                pick(got, f"convention: {f} -> {name}*")
                hit = True
        elif f.startswith(IT_DIR + "/"):
            rel = f[len(IT_DIR) + 1:]
            top = rel.split("/")[0].removesuffix(".rs")
            if top in ("support", "common"):
                helper = os.path.basename(rel).removesuffix(".rs")
                got = modules if helper == "mod" else users_of(helper, modules)
                pick(got, f"{f} is shared test code its users name")
                hit = bool(got)
            elif top in modules:
                pick([top], f"{f} is the module's own file")
                hit = True
            elif top == "main":
                hit = True  # a new `mod` line: the new module's file is in the diff too
        elif f.startswith("crates/lmgw-core/tests/fixtures/"):
            name = os.path.basename(f)
            got = mentioning(name, modules) or mentioning(os.path.basename(os.path.dirname(f)),
                                                          modules)
            pick(got, f"{f} is a fixture these modules name")
            hit = bool(got)
        if not hit and (f.startswith(CORE_SRC) or f.startswith(IT_DIR)):
            unmatched.append(f)

    smoke = [x for e in cfg.get("smoke", []) for x in expand(e, modules)]

    # A map entry that matches no file or no module has rotted: say so.
    tracked = git("ls-files").split("\n")
    for rule in cfg.get("map", []):
        for p in rule["paths"]:
            if not any(fnmatch.fnmatch(f, p) for f in tracked):
                say(f"NOTE: ci/it-map.toml path {p!r} matches no tracked file")
        for e in rule["modules"]:
            if not expand(e, modules):
                say(f"NOTE: ci/it-map.toml module {e!r} names no tests/it module")
    for e in cfg.get("smoke", []):
        if not expand(e, modules):
            say(f"NOTE: ci/it-map.toml smoke entry {e!r} names no tests/it module")
    pick(smoke, "smoke set (it-map.toml)")

    say("\ntests/it modules:")
    by_reason = {}
    for m, why in picked.items():
        by_reason.setdefault(why, []).append(m)
    for why, ms in by_reason.items():
        say(f"    {why}:")
        say(f"        {' '.join(sorted(ms))}")
    whole = sorted(m for m in picked if "::" not in m)
    exact = sorted(m for m in picked if "::" in m and m.split("::")[0] not in whole)
    say(f"    = {len(whole)} of {len(modules)} modules"
        + (f", plus {len(exact)} single test(s)" if exact else ""))
    for f in unmatched:
        say(f"    NOTE: no it module matched {f}: only the smoke set covers it "
            f"(map it in ci/it-map.toml)")

    # --- filters ----------------------------------------------------------------------
    clauses = []
    for name in sorted(touched):
        if name == CORE:
            clauses.append(f"(package({CORE}) & (kind(lib) | binary(trace_span)))")
        else:
            clauses.append(f"package({name})")
    if wide or set(whole) >= set(modules):
        clauses.append(f"(package({CORE}) & binary(it))")
    else:
        alts = [f"test(/^({'|'.join(whole)})::/)"] if whole else []
        alts += [f"test(={m})" for m in exact]
        if alts:
            clauses.append(f"(package({CORE}) & binary(it) & ({' | '.join(alts)}))")

    # --- trunk and worklets ---------------------------------------------------------
    ui = [f for f in files if f.startswith((UI_DIR, CLIENT_DIR, "crates/lmgw-api-types/"))]
    stale = dist_stale()
    if wide:
        trunk, why = 1, f"{wide[0]} is workspace-wide"
    elif ui:
        trunk, why = 1, f"{ui[0]} changed"
    elif stale:
        trunk, why = 1, stale
    else:
        trunk, why = 0, "no lmgw-ui / lmgw-client / lmgw-api-types change and dist/ is fresh"
    say(f"\ntrunk build: {'yes' if trunk else 'skipped'} ({why})")
    client = [f for f in files if f.startswith((CLIENT_DIR, "crates/lmgw-api-types/"))]
    client_wasm = bool(wide or client)
    say(f"lmgw-client wasm32 check: {'yes (' + (wide or client)[0] + ' changed)' if client_wasm else 'skipped (no lmgw-client / lmgw-api-types change)'}")
    voice = [f for f in files if f.startswith(UI_DIR + "assets/voice/")
             or f == "scripts/worklet-check.mjs"]
    say(f"worklet check: {'yes (' + voice[0] + ' changed)' if voice else 'skipped (no change under crates/lmgw-ui/assets/voice/)'}")
    publish = [f for f in files if f.startswith(("scripts/publish", "scripts/pre-push-github"))]
    say(f"publish script check: {'yes (' + publish[0] + ' changed)' if publish else 'skipped (no change to scripts/publish*)'}")

    out = {
        "CHANGED_CRATES": " ".join(sorted(touched)),
        "CHANGED_ALL": "1" if wide else "0",
        "RUN_TRUNK": str(trunk),
        "RUN_CLIENT_WASM": "1" if client_wasm else "0",
        "RUN_WORKLET": "1" if voice else "0",
        "RUN_PUBLISH": "1" if publish else "0",
        "NEXTEST_FILTER": " | ".join(clauses),
    }
    for k, v in out.items():
        print(f"{k}={shlex.quote(v)}")


if __name__ == "__main__":
    main()
