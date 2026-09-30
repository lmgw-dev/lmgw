#!/usr/bin/env python3
"""List a Hugging Face repo's weight files with their exact sizes and gating.

`crates/lmgw-core/src/image_recipes.rs` promises that every
`(repo, file, size_bytes, gated)` in the shipped list was read from the hub's
public metadata API, never from memory — a size that is off by a byte makes
the recipe's own `size_bytes` assertion fail against the file on disk, and a
`gated` that is wrong queues a download that dies on its first byte. This is
that read, so adding a family does not mean re-deriving the curl invocation.

    scripts/hf-recipe-check.py leejet/Qwen-Image-2.1-GGUF Comfy-Org/Qwen-Image-2.1

    scripts/hf-recipe-check.py --rust QuantStack/Qwen-Image-GGUF  # literals

`--rust` prints `ImageRecipeAlternative` literals with the underscore-grouped
sizes the file is written in, so the numbers are transcribed by the machine
rather than by hand.

Read-only and unauthenticated: it sees exactly what a box with no token sees,
which is the question a recipe has to answer. A gated repo therefore lists no
files — that is the answer, not a failure.
"""

import argparse
import json
import sys
import urllib.error
import urllib.request

ENDPOINT = "https://huggingface.co"
WEIGHTS = (".gguf", ".safetensors", ".sft", ".ckpt", ".pt", ".pth")


def get(url: str):
    req = urllib.request.Request(url, headers={"User-Agent": "lmgw-recipe-check"})
    with urllib.request.urlopen(req, timeout=60) as r:
        return json.load(r)


def grouped(n: int) -> str:
    """`4197494816` -> `4_197_494_816`, the spelling the recipe file uses."""
    s = str(n)
    out = []
    while len(s) > 3:
        out.insert(0, s[-3:])
        s = s[:-3]
    out.insert(0, s)
    return "_".join(out)


def report(repo: str, as_rust: bool) -> int:
    try:
        info = get(f"{ENDPOINT}/api/models/{repo}")
    except urllib.error.HTTPError as e:
        print(f"### {repo}: {e.code} {e.reason}", file=sys.stderr)
        return 1
    gated = info.get("gated", False)
    print(f"### {repo}  gated={gated}")
    try:
        tree = get(f"{ENDPOINT}/api/models/{repo}/tree/main?recursive=1")
    except urllib.error.HTTPError as e:
        # 401/403 here is the licence gate answering, which is the useful fact.
        print(f"    tree: {e.code} {e.reason} — a token that accepted the licence is needed")
        return 0
    files = [
        e for e in tree if e.get("type") == "file" and e["path"].endswith(WEIGHTS)
    ]
    for e in sorted(files, key=lambda x: x["path"]):
        if as_rust:
            print(
                "ImageRecipeAlternative {\n"
                f'    file: "{e["path"]}",\n'
                f"    size_bytes: {grouped(e['size'])},\n"
                f'    label: "{e["path"].rsplit("/", 1)[-1]}",\n'
                "},"
            )
        else:
            print(f"    {grouped(e['size']):>17}  {e['path']}")
    if not files:
        print("    (no weight files)")
    return 0


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("repos", nargs="+", metavar="owner/repo")
    ap.add_argument(
        "--rust",
        action="store_true",
        help="print ImageRecipeAlternative literals instead of a table",
    )
    args = ap.parse_args()
    rc = 0
    for repo in args.repos:
        rc |= report(repo, args.rust)
        print()
    return rc


if __name__ == "__main__":
    sys.exit(main())
