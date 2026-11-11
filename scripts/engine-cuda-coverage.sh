#!/usr/bin/env bash
# Read the CUDA architecture coverage of every pinned Windows CUDA engine build (Windows design
# §4.2): download each CUDA asset of the pin table into a new empty dir, check its SHA-256
# against the pin, unpack its DLLs and EXEs into another new empty dir, and list the SASS
# (`cuobjdump --list-elf`) and PTX (`cuobjdump --list-ptx`, plus each PTX's `.version`) every
# file carries, inside a CUDA devel container. Prints one JSON line per asset; with --check it
# compares them with the `cuda.files` the pin table records and exits 1 on a difference.
# Usage: scripts/engine-cuda-coverage.sh [--check] <work-dir> [cuda-devel-image]
#   <work-dir> must not exist yet (it is created; every download gets its own dir inside it).
# Needs: curl, sha256sum, unzip, python3, podman.
set -euo pipefail

check=0
if [[ ${1:-} == --check ]]; then check=1; shift; fi
work=${1:?usage: scripts/engine-cuda-coverage.sh [--check] <work-dir> [cuda-devel-image]}
image=${2:-docker.io/nvidia/cuda:13.0.0-devel-ubuntu24.04}
here=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
pins="$here/crates/lmgw-core/src/runtime/engine_assets/pins.json"

if [[ -e $work ]]; then
  echo "engine-cuda-coverage: $work exists; give a new path" >&2
  exit 2
fi
mkdir -p "$work"
work=$(cd "$work" && pwd)

# engine variant repo tag asset sha256, one CUDA build per line.
python3 -I -c '
import json, sys
for platform, engines in json.load(open(sys.argv[1])).items():
    for engine, e in engines.items():
        for b in e.get("builds", []):
            if "cuda" in b:
                print(engine, b["variant"], e["repo"], e["tag"], b["asset"]["name"], b["asset"]["sha256"])
' "$pins" >"$work/builds.txt"

fail=0
while read -r engine variant repo tag asset sha; do
  dl="$work/$engine-$variant-download"
  un="$work/$engine-$variant-unpacked"
  mkdir "$dl" "$un"
  curl -sSL --fail -o "$dl/$asset" "https://github.com/$repo/releases/download/$tag/$asset"
  got=$(sha256sum "$dl/$asset" | cut -d' ' -f1)
  if [[ $got != "$sha" ]]; then
    echo "engine-cuda-coverage: $asset has SHA-256 $got, the pin says $sha" >&2
    exit 1
  fi
  unzip -q "$dl/$asset" '*.dll' '*.exe' -d "$un"
  # cuobjdump on Linux reads no PE file ("does not contain device code"), so the device code
  # is taken out of each DLL's and EXE's `.nv_fatb` section first, into a dir of its own, as
  # `<path with / as __>.fatbin`.
  fb="$work/$engine-$variant-fatbin"
  mkdir "$fb"
  python3 -I -c '
import os, struct, sys
src, dst = sys.argv[1:3]
for root, _, names in os.walk(src):
    for n in names:
        if not n.lower().endswith((".dll", ".exe")):
            continue
        p = os.path.join(root, n)
        d = open(p, "rb").read()
        if d[:2] != b"MZ":
            continue
        pe = struct.unpack_from("<I", d, 0x3C)[0]
        if d[pe:pe + 4] != b"PE\0\0":
            continue
        nsec, optsz = struct.unpack_from("<H", d, pe + 6)[0], struct.unpack_from("<H", d, pe + 20)[0]
        for i in range(nsec):
            name, vsize, _, rawsize, rawptr = struct.unpack_from("<8sIIII", d, pe + 24 + optsz + 40 * i)
            if name.rstrip(b"\0") == b".nv_fatb":
                rel = os.path.relpath(p, src).replace(os.sep, "__")
                open(os.path.join(dst, rel + ".fatbin"), "wb").write(d[rawptr:rawptr + min(vsize, rawsize)])
' "$un" "$fb"
  # The container reads the fatbins only; its script is given as an argument.
  podman run --rm --network=none --entrypoint bash -v "$fb:/w:ro,Z" "$image" -c '
    shopt -s nullglob
    for f in /w/*.fatbin; do
      out=$(cuobjdump --list-elf --list-ptx "$f" 2>/dev/null) || continue
      [[ -n $out ]] || continue
      ptxv=$(cuobjdump -ptx "$f" 2>/dev/null | grep -m1 -oE "^\.version [0-9.]+" | cut -d" " -f2 || true)
      n=${f#/w/}; n=${n%.fatbin}
      echo "FILE ${n//__//} ${ptxv:-none}"
      echo "$out"
    done' >"$dl/cuobjdump.txt"
  line=$(python3 -I -c '
import json, re, sys
engine, variant, asset, path = sys.argv[1:5]
files, cur = [], None
for l in open(path):
    if l.startswith("FILE "):
        _, name, ptxv = l.split()
        cur = {"file": name, "sass": set(), "ptx": set(), "ptx_isa": None if ptxv == "none" else ptxv}
        files.append(cur)
        continue
    m = re.search(r"(ELF|PTX) file\s+\d+: .*\.sm_(\d+[a-z]?)\.(cubin|ptx)", l)
    if m and cur is not None:
        cur["sass" if m.group(1) == "ELF" else "ptx"].add(m.group(2))
key = lambda s: (int(re.match(r"\d+", s).group()), s)
for f in files:
    f["sass"] = sorted(f["sass"], key=key)
    f["ptx"] = sorted(f["ptx"], key=key)
print(json.dumps({"engine": engine, "variant": variant, "asset": asset, "files": files}))
' "$engine" "$variant" "$asset" "$dl/cuobjdump.txt")
  echo "$line"
  if (( check )); then
    if ! python3 -I -c '
import json, sys
seen = json.loads(sys.argv[2])
for platform, engines in json.load(open(sys.argv[1])).items():
    for b in engines.get(seen["engine"], {}).get("builds", []):
        if b["variant"] == seen["variant"] and b["asset"]["name"] == seen["asset"]:
            sys.exit(0 if b["cuda"]["files"] == seen["files"] else 1)
sys.exit(1)
' "$pins" "$line"; then
      echo "engine-cuda-coverage: $asset differs from the pin table's cuda.files" >&2
      fail=1
    fi
  fi
done <"$work/builds.txt"
exit "$fail"
