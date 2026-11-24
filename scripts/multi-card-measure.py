#!/usr/bin/env python3
"""WP0 of the device ledger (docs/design/2026-11-13-device-ledger-design.md §13): what llama.cpp
puts on each card of a machine with several NVIDIA cards, measured through a running lmgw, in the
manner of scripts/llama-fit-check.py. Dry run by default: nothing starts without --run.

What it measures, one case each (--cases picks; every case is sized and checked before it starts):
  order      `llama-server --list-devices` in the chat image with the cards passed by UUID, in
             reverse PCI order, without and with CUDA_DEVICE_ORDER=PCI_BUS_ID: the engine's
             device order (§2.2). A throwaway container through lmgw's inner Podman, not a row.
  uuid       the 4B on one card by `--device nvidia.com/gpu=GPU-<uuid>`: that the start runs
             (§7.1), and the one-card figures of the engine's cost per card (D5, §4.2).
  uuid-nvd   the same with `-e NVIDIA_VISIBLE_DEVICES=GPU-<uuid>` beside it.
  split2     the 4B on two cards, `--split-mode layer --tensor-split <layer counts>` summing to
  split3     n_layer + 1, `-e CUDA_DEVICE_ORDER=PCI_BUS_ID`, cards by UUID (§4.3, §7.2): what
             lands on each card against §4.3's formula, where the compute buffers and the output
             layer go, and the cost per card on two and three cards.
  mmproj     a vision model and its projector on two cards: the projector's card (--vl-file,
             --mmproj; skipped without them).
  draft      the 4B with a drafter on two cards, llama.cpp's default placement (--draft).
  draft-dev  the same with the drafter confined by its own device flag (`--device-draft CUDA0`,
             when the image's --help has it).
  moe-cpu    a small MoE model on two cards with `n_cpu_moe` (--moe-file).
  moe-cache  the same with `moe_cache_mib` (when the image knows --moe-cache-mib).

Where the figures come from: lmgw's `GET /api/vram` (each card's free memory, lmgw's charge and
its own per-process reading of the model), the host's `nvidia-smi --query-compute-apps`
(what the model's llama-server process holds on each card; the process is the one whose command
line has `--alias <row>`), and the model's own log through the `container` op's `logs` action
(llama.cpp's model, KV and compute buffers per CUDA device, the device each layer was assigned,
the devices it found). The layer assignment needs `-v` on the row (on by default, --no-verbose
drops it). §4.3's prediction reads the GGUF's tensor sizes (through `docker exec <container> cat`,
or --models-host-dir).

Safety (a test server may be shared with other programs; the owner's ruling: use only memory that is free):
- Dry run unless --run. A dry run reads lmgw's API, nvidia-smi and the GGUF headers, prints every
  planned start (the row it would create, its run and llama-server arguments, the podman line of
  the order case) and the free-memory check, and writes nothing.
- Before every start, each card's free memory is read now (the lower of lmgw's per-card figure,
  less what lmgw keeps free there, and nvidia-smi's) and the case is skipped, saying why, unless
  every card it uses holds its planned bytes plus --margin-mib (4096 by default) more. The plan
  per card is generous: (weights + KV) x the card's share x 1.25, the whole compute estimate, the
  output layer on the last card, --engine-mib (1024) for the CUDA context, and a projector's or a
  drafter's whole size on every card. A case whose model is estimated above --max-model-mib
  (6144) is skipped unless --allow-large.
- While a case runs, a watchdog reads nvidia-smi every second; when any card's free memory drops
  below --floor-mib (1536) the case's container is stopped at once and no further case starts.
- Rows of its own only, named `wp0-<case>`. It refuses to start when such a row exists already
  (a run that was killed): --clean-leftovers stops and deletes exactly those names. Every row it
  creates is stopped and deleted at the end, on an error and on Ctrl-C or SIGTERM (a second
  Ctrl-C during the clean-up is ignored until it is done). lmgw's settings are not changed.
  A row is public while it runs (one short chat request reads what a first request adds;
  --no-chat leaves it out), and should lmgw's admission stop an idle model of its own to make
  room, the case's report says which.
- --run needs the host's nvidia-smi (the watchdog): run it on the machine that has the cards.

On a server deployment (lmgw under Docker Compose, container `lmgw`; the plain
listener is the container's loopback, so the API is reached on the TLS listener), as a user who
may run `docker` (or with sudo), with this file copied there:
  export LMGW_TOKEN=<owner key>   # Usage -> Keys & budgets, an owner key (key_reveal shows it)
  python3 -I multi-card-measure.py --base https://lmgw.example:8443            # the plan
  python3 -I multi-card-measure.py --base https://lmgw.example:8443 --run      # base cases
  python3 -I multi-card-measure.py --base https://lmgw.example:8443 --run \\
      --cases draft,draft-dev,mmproj,moe-cpu,moe-cache --draft Qwen3-0.6B-Q8_0.gguf \\
      --vl-file Qwen3-VL-4B-Instruct-Q4_K_M.gguf --mmproj mmproj-Qwen3-VL-4B-Instruct-F16.gguf \\
      --moe-file <small MoE>.gguf
Every file is a path under the chat class's models dir (`/api/gguf-files` lists them); the base
model is Qwen3-4B-Instruct-2507-Q4_K_M.gguf (§1). Where the host does not resolve its own name:
--base https://127.0.0.1:8443 --tls-name lmgw.example (the certificate is still verified,
against that name). The order case runs
  docker exec lmgw sh -c 'nsenter -t "$(pgrep -o -x lmgw-server)" --user --mount -- podman ...'
(the inner Podman only through lmgw's namespaces, docs/server.md section 14); `--docker ''` runs
`podman` directly instead, for lmgw under the rootless Podman unit.

Output: a Markdown report (a table per case, per card MiB measured against lmgw's estimate and
§4.3's prediction, in a form for §1) on stdout and, with --run, in <out-dir>/report.md beside
raw.json (every reading) and <case>.log (the model's log). The offline test is
scripts/multi_card_measure_test.py (ci/check.sh runs it).
"""
import argparse
import bisect
import http.client
import json
import os
import re
import shlex
import signal
import ssl
import struct
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass, field
from datetime import datetime, timezone

MIB = 2**20
PREFIX = "wp0-"
BASE_FILE = "Qwen3-4B-Instruct-2507-Q4_K_M.gguf"
# vram/headroom.rs: the margin llama.cpp rows are given today, CUDA context included (§9.1).
LLAMA_MARGIN_MIB = 807
CASES = ("order", "uuid", "uuid-nvd", "split2", "split3", "mmproj", "draft", "draft-dev",
         "moe-cpu", "moe-cache")
DEFAULT_CASES = ("order", "uuid", "uuid-nvd", "split2", "split3")
SAFETY = 1.25
PROJECTOR_COMPUTE = 512 * MIB


def mib(b):
    return None if b is None else b / MIB


def fmt(b):
    """Bytes as whole MiB for a table; a dash for none."""
    return "–" if b is None else f"{b / MIB:.0f}"


# ---------------------------------------------------------------------------------------------
# Cards
# ---------------------------------------------------------------------------------------------

@dataclass
class Card:
    index: int
    id: str
    name: str
    total: int
    free: int
    pci: str = ""

    def label(self):
        return f"{self.name} (card {self.index})"


def cards_from_vram(v):
    """The cards of a `GET /api/vram` answer, in the driver's order. A card's free memory is the
    driver's less what lmgw keeps free there (starts in flight, image peaks, pending loads)."""
    out = []
    for d in v.get("devices") or []:
        free = int(d.get("free_bytes") or 0)
        kept = d.get("kept_free")
        if isinstance(kept, dict):
            free -= sum(int(kept.get(k) or 0) for k in
                        ("starting_bytes", "image_peak_bytes", "pending_load_bytes"))
        out.append(Card(index=int(d.get("index", len(out))), id=str(d.get("id") or ""),
                        name=str(d.get("name") or ""), total=int(d.get("total_bytes") or 0),
                        free=max(free, 0)))
    return sorted(out, key=lambda c: c.index)


def norm_pci(s):
    """`00000000:17:00.0` (nvidia-smi) and `0000:17:00.0` (llama.cpp) as one spelling."""
    m = re.search(r"(?:([0-9a-fA-F]{1,8}):)?([0-9a-fA-F]{2}):([0-9a-fA-F]{2})\.([0-7])", s or "")
    if not m:
        return ""
    dom = int(m.group(1) or "0", 16)
    return f"{dom:04x}:{m.group(2).lower()}:{m.group(3).lower()}.{m.group(4)}"


SMI_GPU_QUERY = "index,uuid,pci.bus_id,name,memory.total,memory.used,memory.free"


def parse_smi_gpus(text):
    """`nvidia-smi --query-gpu=<SMI_GPU_QUERY> --format=csv,noheader,nounits`."""
    out = []
    for line in text.splitlines():
        parts = [p.strip() for p in line.split(",")]
        if len(parts) < 7:
            continue
        try:
            out.append({"index": int(parts[0]), "uuid": parts[1], "pci": norm_pci(parts[2]),
                        "name": ", ".join(parts[3:-3]), "total": int(parts[-3]) * MIB,
                        "used": int(parts[-2]) * MIB, "free": int(parts[-1]) * MIB})
        except ValueError:
            continue
    return out


def parse_smi_apps(text):
    """`nvidia-smi --query-compute-apps=gpu_uuid,pid,used_memory --format=csv,noheader,nounits`:
    (uuid, pid, bytes or None where the driver gives no figure)."""
    out = []
    for line in text.splitlines():
        parts = [p.strip() for p in line.split(",")]
        if len(parts) != 3 or not parts[1].isdigit():
            continue
        used = int(parts[2]) * MIB if parts[2].isdigit() else None
        out.append((parts[0], int(parts[1]), used))
    return out


def apply_smi(cards, gpus):
    """Each card's free memory as the lower of lmgw's and nvidia-smi's, and its PCI address.
    An lmgw older than the device ledger's WP1 gives its cards no id: those are matched by the
    index (NVML's, which nvidia-smi shares), and take nvidia-smi's UUID as their id."""
    by = {g["uuid"]: g for g in gpus}
    by_index = {g["index"]: g for g in gpus}
    for c in cards:
        g = by.get(c.id) if c.id else by_index.get(c.index)
        if g and not c.id:
            c.id = g["uuid"]
        if g:
            c.free = min(c.free, g["free"])
            c.pci = g["pci"]
    return cards


# ---------------------------------------------------------------------------------------------
# Run arguments
# ---------------------------------------------------------------------------------------------

def _gpu_device(v):
    return v.startswith("nvidia.com/gpu") or v.split(":")[0].startswith(("/dev/nvidia", "/dev/dri",
                                                                         "/dev/kfd"))


def strip_gpu_args(args):
    """The class's podman run arguments without the ones that hand a container a card (the
    rule of lmgw-api-types' strip_gpu_devices): --device of a CDI GPU or a GPU node, --gpus,
    -e/--env NVIDIA_VISIBLE_DEVICES, --runtime nvidia, a CDI --annotation."""
    out, i = [], 0
    while i < len(args):
        a = args[i]
        flag, inline = (a.split("=", 1) + [None])[:2] if a.startswith("--") and "=" in a else (a, None)
        nxt = args[i + 1] if i + 1 < len(args) else ""
        val = inline if inline is not None else nxt
        step = 1 if inline is not None else 2
        drop = False
        if flag == "--device":
            drop = _gpu_device(val)
        elif flag == "--gpus":
            drop = True
        elif flag in ("-e", "--env"):
            drop = val.split("=", 1)[0] == "NVIDIA_VISIBLE_DEVICES"
        elif flag == "--runtime":
            drop = "nvidia" in val
        elif flag == "--annotation":
            drop = "nvidia.com/gpu" in val
        else:
            step = 1
        if drop:
            i += step
            continue
        out.extend(args[i:i + step])
        i += step
    return out


def run_args_for(class_args, uuids, nvd=False, pci_order=False):
    """A row's own podman run arguments: the class's without its card, the cards by UUID, and
    the environment of §7.1 asked for."""
    out = strip_gpu_args(class_args)
    for u in uuids:
        out += ["--device", f"nvidia.com/gpu={u}"]
    if nvd:
        out += ["-e", "NVIDIA_VISIBLE_DEVICES=" + ",".join(uuids)]
    if pci_order:
        out += ["-e", "CUDA_DEVICE_ORDER=PCI_BUS_ID"]
    return out


# ---------------------------------------------------------------------------------------------
# llama.cpp's layer split (§4.3), as remembered from llama-model.cpp: the entries are made
# cumulative fractions, and layer il of the n_layer + 1 (the output layer last) goes to the
# first device whose fraction is above il / (n_layer + 1). WP0 checks it against the log.
# ---------------------------------------------------------------------------------------------

def default_entries(total, k):
    """Uneven layer counts summing to `total`, so each card's share differs."""
    if k == 1:
        return [total]
    if k == 2:
        a = round(total * 0.4)
        return [a, total - a]
    a, b = round(total * 0.25), round(total * 0.35)
    return [a, b, total - a - b]


def assign_layers(entries, n_layer, n_gpu_layers=999):
    """The device of each layer 0..n_layer (index n_layer is the output layer); None = CPU."""
    s = float(sum(entries))
    cum, acc = [], 0.0
    for e in entries:
        acc += e
        cum.append(acc / s)
    total = n_layer + 1
    act = min(n_gpu_layers, total)
    start = max(total - n_gpu_layers, 0)
    out = []
    for il in range(total):
        if il < start or act <= 0:
            out.append(None)
        else:
            out.append(min(bisect.bisect_right(cum, (il - start) / act), len(entries) - 1))
    return out


# ---------------------------------------------------------------------------------------------
# GGUF tensor sizes (the header only)
# ---------------------------------------------------------------------------------------------

class _Reader:
    def __init__(self, f):
        self.f, self.pos = f, 0

    def read(self, n):
        b = b""
        while len(b) < n:
            chunk = self.f.read(n - len(b))
            if not chunk:
                raise EOFError("GGUF header ends early")
            b += chunk
        self.pos += n
        return b

    def u32(self):
        return struct.unpack("<I", self.read(4))[0]

    def u64(self):
        return struct.unpack("<Q", self.read(8))[0]

    def string(self):
        return self.read(self.u64()).decode("utf-8", "replace")


_SCALARS = {0: "<B", 1: "<b", 2: "<H", 3: "<h", 4: "<I", 5: "<i", 6: "<f", 7: "<?", 10: "<Q",
            11: "<q", 12: "<d"}


def _value(r, t):
    if t in _SCALARS:
        fmt_ = _SCALARS[t]
        return struct.unpack(fmt_, r.read(struct.calcsize(fmt_)))[0]
    if t == 8:
        return r.string()
    if t == 9:
        et, n = r.u32(), r.u64()
        if et in _SCALARS:
            r.read(struct.calcsize(_SCALARS[et]) * n)
        else:
            for _ in range(n):
                _value(r, et)
        return None
    raise ValueError(f"GGUF value type {t} unknown")


def read_gguf_layout(f, file_size):
    """{arch, n_layer, tensors: {name: bytes}} from a GGUF's header. A tensor's bytes are the
    distance to the next tensor's offset (the last: to the end of the file), so no table of
    quantization types is needed; the alignment padding counts with it."""
    r = _Reader(f)
    if r.read(4) != b"GGUF":
        raise ValueError("not a GGUF file")
    version = r.u32()
    count = r.u32 if version == 1 else r.u64
    n_tensors, n_kv = count(), count()
    kv = {}
    for _ in range(n_kv):
        key = r.string()
        val = _value(r, r.u32())
        if val is not None and (not isinstance(val, str) or key.startswith("general.")):
            kv[key] = val
    infos = []
    for _ in range(n_tensors):
        name = r.string()
        dims = [r.u64() for _ in range(r.u32())]
        r.u32()  # type
        infos.append((name, r.u64(), dims))
    align = int(kv.get("general.alignment") or 32)
    data_start = (r.pos + align - 1) // align * align
    arch = kv.get("general.architecture") or ""
    infos.sort(key=lambda t: t[1])
    tensors = {}
    for i, (name, off, _) in enumerate(infos):
        end = infos[i + 1][1] if i + 1 < len(infos) else file_size - data_start
        tensors[name] = max(end - off, 0)
    return {"arch": arch, "n_layer": int(kv.get(f"{arch}.block_count") or 0), "tensors": tensors}


_EXPS = re.compile(r"^blk\.\d+\.ffn_(up|down|gate|gate_up)_(ch|)exps")


def layer_weights(layout):
    """Per repeating layer its weights and its routed experts' part, the output layer (with a
    tied embedding the token embedding again: llama.cpp duplicates it onto the output's
    device), the input layer (kept in host memory) and the rest."""
    n = layout["n_layer"]
    per, exps = [0] * n, [0] * n
    out = {"input": 0, "output": 0, "extra": 0}
    t = layout["tensors"]
    for name, size in t.items():
        m = re.match(r"^blk\.(\d+)\.", name)
        if m:
            i = int(m.group(1))
            if i < n:
                per[i] += size
                if _EXPS.match(name):
                    exps[i] += size
            else:
                out["extra"] += size  # MTP layers past n_layer
        elif name.startswith("token_embd"):
            out["input"] += size
        elif name.startswith("output"):
            out["output"] += size
        else:
            out["extra"] += size
    if "output.weight" not in t and "token_embd.weight" in t:
        out["output"] += t["token_embd.weight"]
    return {"per_layer": per, "exps": exps, **out}


def predict_cards(lw, assignment, n_cards, kv_total, n_cpu_moe=0):
    """§4.3's formula per card: the weights of its layers (less the experts `n_cpu_moe` keeps
    in host memory), its layers' share of the KV cache (even per layer: an approximation for a
    model whose layers differ), and the output layer on the card the assignment gives it."""
    n = len(lw["per_layer"])
    weights, kv, layers = [0] * n_cards, [0] * n_cards, [0] * n_cards
    for il, dev in enumerate(assignment):
        if dev is None:
            continue
        if il < n:
            weights[dev] += lw["per_layer"][il] - (lw["exps"][il] if il < n_cpu_moe else 0)
            kv[dev] += kv_total // max(n, 1)
            layers[dev] += 1
        else:
            weights[dev] += lw["output"]
    return {"weights": weights, "kv": kv, "layers": layers,
            "output_card": assignment[n] if len(assignment) > n else None}


# ---------------------------------------------------------------------------------------------
# The engine's log
# ---------------------------------------------------------------------------------------------

_PCI = r"([0-9a-fA-F]{4,8}:[0-9a-fA-F]{2}:[0-9a-fA-F]{2}\.[0-7])"
_BUF = re.compile(r"(\w+):\s+(\S+)\s+(.+?) buffer size =\s*([\d.]+) MiB")
_USING = re.compile(r"using device (\S+) \((.*?)\)(?: \(" + _PCI + r"\))? - (\d+) MiB free")
_CUDA_DEV = re.compile(r"Device (\d+): ([^,]+),")
_LAYER = re.compile(r"layer\s+(\d+) assigned to device (\S+?),")
_LIST = re.compile(r"^\s*(\S+): (.*) \((\d+) MiB, (\d+) MiB free\)")


def parse_engine_log(text):
    """What llama-server's log says about its devices and buffers. The buffers are kept with
    the phase they were read in (main, draft, mmproj): the phase is the last marker line seen
    (a draft model's load, a projector's), inferred from the log's order, so the raw lines go
    into the report as well."""
    out = {"cuda_found": None, "cuda_devices": [], "using": [], "buffers": [], "layers": {},
           "offloaded": None, "clip_backend": [], "breakdown": [], "errors": [], "raw": []}
    phase = "main"
    for line in text.splitlines():
        low = line.lower()
        if "buffer size" not in low:
            if re.search(r"load\w*\W+draft|draft model", low) and "load" in low:
                phase = "draft"
            elif re.search(r"clip_model_loader|clip_ctx|mtmd_init|loading mmproj|mmproj model", low):
                phase = "mmproj"
        m = re.search(r"found (\d+) CUDA devices", line)
        if m:
            out["cuda_found"] = int(m.group(1))
        m = _CUDA_DEV.search(line)
        if m and "compute capability" in line:
            pci = re.search(_PCI, line)
            out["cuda_devices"].append({"dev": f"CUDA{m.group(1)}", "name": m.group(2).strip(),
                                        "pci": norm_pci(pci.group(1)) if pci else ""})
        m = _USING.search(line)
        if m:
            out["using"].append({"dev": m.group(1), "name": m.group(2),
                                 "pci": norm_pci(m.group(3)) if m.group(3) else "",
                                 "free": int(m.group(4)) * MIB})
            out["raw"].append(line.strip())
        m = _BUF.search(line)
        if m:
            out["buffers"].append({"phase": phase, "fn": m.group(1), "dev": m.group(2),
                                   "kind": m.group(3).strip(), "bytes": int(float(m.group(4)) * MIB)})
            out["raw"].append(line.strip())
        m = _LAYER.search(line)
        if m:
            out["layers"][int(m.group(1))] = m.group(2)
        m = re.search(r"offloaded (\d+/\d+) layers", line)
        if m:
            out["offloaded"] = m.group(1)
            out["raw"].append(line.strip())
        m = re.search(r"CLIP using (\S+) backend", line)
        if m:
            out["clip_backend"].append(m.group(1))
            out["raw"].append(line.strip())
        if "memory_breakdown" in line or "memory breakdown" in low:
            out["breakdown"].append(line.rstrip())
        if "out of memory" in low or re.search(r"\berror\b", low):
            out["errors"].append(line.strip())
    return out


def buffers_by_dev(log, phase=None):
    """{dev: {kind: bytes}} of the log's buffers, of one phase or of all."""
    out = {}
    for b in log["buffers"]:
        if phase and b["phase"] != phase:
            continue
        d = out.setdefault(b["dev"], {})
        d[b["kind"]] = d.get(b["kind"], 0) + b["bytes"]
    return out


def parse_list_devices(text):
    """`llama-server --list-devices`: [{dev, name, total, free}] of the non-CPU devices."""
    out = []
    for line in text.splitlines():
        m = _LIST.match(line)
        if m and not m.group(1).upper().startswith("CPU"):
            out.append({"dev": m.group(1), "name": m.group(2).strip(),
                        "total": int(m.group(3)) * MIB, "free": int(m.group(4)) * MIB})
    return out


def match_devices(devs, gpus):
    """Each engine device's card: by its PCI address where the log gives one, else by the name
    and the free memory nearest nvidia-smi's (`approx`)."""
    out = []
    for d in devs:
        g = None
        how = "pci"
        if d.get("pci"):
            g = next((x for x in gpus if x["pci"] == d["pci"]), None)
        if g is None:
            how = "approx"
            same = [x for x in gpus if d["name"] and (d["name"] in x["name"] or x["name"] in d["name"])]
            if same and d.get("free") is not None:
                g = min(same, key=lambda x: abs(x["free"] - d["free"]))
            elif len(same) == 1:
                g = same[0]
        out.append({**d, "card": None if g is None else g["index"],
                    "uuid": None if g is None else g["uuid"], "how": how if g else "none",
                    "ambiguous": how == "approx" and sum(
                        1 for x in gpus if d["name"] and d["name"] in x["name"]) > 1})
    return out


def layer_counts(log_layers, n_layer):
    """{dev: repeating layers} from the log's `layer N assigned to device` lines."""
    out = {}
    for il, dev in log_layers.items():
        if il < n_layer:
            out[dev] = out.get(dev, 0) + 1
    return out


def entries_verdict(entries, devs, counts, n_layer):
    """Whether the log's layers per device are the stated entries read as layer counts that
    sum to n_layer + 1, the last entry's extra layer being the output layer."""
    got = [counts.get(d, 0) for d in devs]
    if not counts:
        return "the log has no layer assignment (run with -v; --no-verbose was given?)"
    want = list(entries)
    if sum(entries) == n_layer + 1:
        want[-1] -= 1
        if got == want:
            return (f"yes: the entries are layer counts summing to n_layer + 1 = {n_layer + 1}; the "
                    f"repeating layers per card are {got} and the last entry's remaining layer is "
                    f"the output layer")
    if got == list(entries):
        return f"the repeating layers per card equal the entries {entries} themselves (sum {sum(entries)})"
    return f"no: entries {entries}, repeating layers per card {got} (n_layer {n_layer})"


# ---------------------------------------------------------------------------------------------
# Cases and their plan
# ---------------------------------------------------------------------------------------------

@dataclass
class Plan:
    case: str
    title: str
    status: str = "ready"            # ready | skip
    reason: str = ""
    cards: list = field(default_factory=list)
    entries: list = field(default_factory=list)
    planned: list = field(default_factory=list)   # bytes per card
    body: dict = field(default_factory=dict)      # local_model_set create
    podman: list = field(default_factory=list)    # the order case's variants: (name, argv)
    footprint: dict = field(default_factory=dict)
    notes: list = field(default_factory=list)

    @property
    def model_id(self):
        return PREFIX + self.case


def choose_cards(cards, k):
    """The k cards with the most free memory now, in PCI (driver) order."""
    best = sorted(cards, key=lambda c: c.free, reverse=True)[:k]
    return sorted(best, key=lambda c: c.index)


def plan_bytes(fp, shares, engine, output=0, extra_each=0):
    """The generous plan per card: (weights + KV) x share x SAFETY, the whole compute
    estimate, the engine's cost, the output layer on the last card, and `extra_each` (a
    projector, a drafter) on every card."""
    out = []
    for i, s in enumerate(shares):
        b = int((fp["weights"] + fp["kv"]) * s * SAFETY) + fp["compute"] + engine + extra_each
        if i == len(shares) - 1:
            b += output
        out.append(b)
    return out


def fit_check(cards, planned, margin):
    """(ok, lines): every card holds its plan plus the margin in its free memory now."""
    ok, lines = True, []
    for c, p in zip(cards, planned, strict=True):
        need = p + margin
        good = need <= c.free
        ok &= good
        lines.append(f"{c.label()}: plan {fmt(p)} + margin {fmt(margin)} = {fmt(need)} MiB "
                     f"{'<=' if good else '>'} {fmt(c.free)} MiB free now"
                     f"{'' if good else '  -> does not fit'}")
    return ok, lines


def make_plan(case, cards, ctx, a):
    """One case's plan from the cards now and what lmgw and the files say (`ctx`: footprints,
    n_layer, layouts, flags, class args, files present)."""
    titles = {
        "order": "the engine's device order, cards by UUID, without and with CUDA_DEVICE_ORDER",
        "uuid": "one card by its UUID CDI name",
        "uuid-nvd": "one card by its UUID CDI name, with NVIDIA_VISIBLE_DEVICES",
        "split2": "two cards, a stated --tensor-split",
        "split3": "three cards, a stated --tensor-split",
        "mmproj": "a vision model and its projector on two cards",
        "draft": "the 4B and a drafter on two cards, llama.cpp's default placement",
        "draft-dev": "the 4B and a drafter on two cards, the drafter on the first card",
        "moe-cpu": "a MoE model on two cards with n_cpu_moe",
        "moe-cache": "a MoE model on two cards with n_cpu_moe and moe_cache_mib",
    }
    p = Plan(case=case, title=titles[case])
    margin, engine = a.margin_mib * MIB, a.engine_mib * MIB

    if case == "order":
        need = a.list_devices_mib * MIB
        use = [c for c in cards if c.free >= need + margin]
        p.cards = use
        p.planned = [need] * len(use)
        _, p.notes = fit_check(cards, [need] * len(cards), margin)
        if len(use) < 2:
            return skip(p, "fewer than two cards hold a CUDA context plus the margin now")
        if not ctx.get("image"):
            return skip(p, "the chat class's image is not known")
        uuids = [c.id for c in reversed(use)]
        base = strip_gpu_args(ctx["class_args"])
        for name, env in (("uuid", []), ("uuid+PCI_BUS_ID", ["-e", "CUDA_DEVICE_ORDER=PCI_BUS_ID"])):
            argv = (["podman"] + ctx.get("globals", []) +
                    ["run", "--rm", "--pull=never", "--network=none", "--name", f"{PREFIX}list-devices"] +
                    base + [x for u in uuids for x in ("--device", f"nvidia.com/gpu={u}")] + env +
                    ([f"--entrypoint={a.entrypoint}"] if a.entrypoint else []) +
                    [ctx["image"], "--list-devices"])
            p.podman.append((name, argv))
        return p

    # Rows: which model and how many cards.
    k = {"uuid": 1, "uuid-nvd": 1, "split2": 2, "split3": 3}.get(case, 2)
    fields = {}
    if case in ("uuid", "uuid-nvd", "split2", "split3", "draft", "draft-dev"):
        model = a.file
    elif case == "mmproj":
        model = a.vl_file
        if not (a.vl_file and a.mmproj):
            return skip(p, "needs --vl-file and --mmproj (a vision model and its projector)")
        fields["mmproj_path"] = a.mmproj
    else:
        model = a.moe_file
        if not a.moe_file:
            return skip(p, "needs --moe-file (a small mixture-of-experts GGUF)")
        fields["n_cpu_moe"] = a.n_cpu_moe
        if case == "moe-cache":
            if "moe-cache-mib" not in ctx["flags"]:
                return skip(p, "the image's llama-server has no --moe-cache-mib")
            fields["moe_cache_mib"] = a.moe_cache_mib
    if case in ("draft", "draft-dev"):
        if not a.draft:
            return skip(p, "needs --draft (a drafter for the base model, e.g. Qwen3-0.6B)")
        fields["draft_gguf_path"] = a.draft
        # llama-server's --spec-type defaults to none: without it the drafter is loaded,
        # freed again ("no implementations specified for speculative decoding") and never
        # holds memory while the model serves (seen on build 12352, 2026-11-16).
        fields["spec_type"] = "draft-simple"
    missing = [f for f in [model, fields.get("mmproj_path"), fields.get("draft_gguf_path")]
               if f and f not in ctx["files"]]
    if missing:
        return skip(p, f"not in the chat models dir: {', '.join(missing)}")
    fp = ctx["footprints"].get(case)
    if not fp:
        return skip(p, "lmgw gave no footprint for it (/api/ladder-rung-plan)")
    p.footprint = fp
    if fp["total"] > a.max_model_mib * MIB and not a.allow_large:
        return skip(p, f"lmgw estimates {fmt(fp['total'])} MiB, above --max-model-mib "
                       f"{a.max_model_mib}: pass --allow-large to run it")
    n_layer = ctx["n_layer"].get(model)
    if not n_layer:
        return skip(p, f"no block_count in {model}'s header (/api/model-inspect)")
    if len(cards) < k:
        return skip(p, f"needs {k} cards, the machine has {len(cards)}")

    chosen = choose_cards(cards, k)
    given = {2: a.split2, 3: a.split3}.get(k)
    entries = [int(x) for x in given.split(",")] if given else default_entries(n_layer + 1, k)
    if len(entries) != k:
        return skip(p, f"--split{k} has {len(entries)} entries, not {k}")
    shares = [e / sum(entries) for e in entries]
    lw = ctx["layouts"].get(model)
    output = lw["output"] if lw else int(fp["weights"] * 0.1)
    extra = 0
    if "mmproj_path" in fields:
        extra += ctx["sizes"].get(fields["mmproj_path"], 0) + PROJECTOR_COMPUTE
    if "draft_gguf_path" in fields:
        extra += ctx["sizes"].get(fields["draft_gguf_path"], 0)
    planned = plan_bytes(fp, shares, engine, output=output if k > 1 else 0, extra_each=extra)
    p.cards, p.entries, p.planned = chosen, entries, planned
    ok, p.notes = fit_check(chosen, planned, margin)
    if not ok:
        p.status, p.reason = "skip", "a card it would use does not hold its plan plus the margin now"

    uuids = [c.id for c in chosen]
    # §7.1 renders NVIDIA_VISIBLE_DEVICES beside the CDI devices; the splits do as WP6 would.
    nvd = case == "uuid-nvd" or (k > 1 and not a.split_without_nvd)
    run_args = run_args_for(ctx["class_args"], uuids, nvd=nvd, pci_order=k > 1)
    extra_args = []
    if k > 1:
        extra_args += ["--split-mode", "layer", "--tensor-split", ",".join(map(str, entries))]
    if case == "draft-dev":
        flag = next((f for f in ("device-draft", "devd") if f in ctx["flags"]), None)
        if not flag:
            return skip(p, "the image's llama-server --help lists no --device-draft")
        extra_args += [f"--{flag}", "CUDA0"]
    if not a.no_verbose:
        extra_args.append("-v")
    p.body = {"action": "create", "model_id": p.model_id, "gguf_path": model,
              "ctx_size": a.ctx, "n_gpu_layers": 999, "fit": "off", "idle_seconds": 900,
              "enabled": True, "public": True, "warm_start": False,
              "extra_run_args": run_args, "extra_args": extra_args, **fields}
    return p


def skip(p, why):
    p.status, p.reason = "skip", why
    return p


def plan_text(p):
    """A plan as the dry run prints it."""
    lines = [f"## {p.case}: {p.title}", ""]
    if p.status == "skip":
        lines += [f"SKIPPED: {p.reason}", ""]
    if p.cards:
        lines.append("Cards: " + ", ".join(f"{c.label()} {c.id}" for c in p.cards))
    if len(p.entries) > 1:
        lines.append(f"--tensor-split {','.join(map(str, p.entries))} (layer counts; sum "
                     f"{sum(p.entries)})")
    if p.footprint:
        f = p.footprint
        lines.append(f"lmgw's estimate: weights {fmt(f['weights'])} + KV {fmt(f['kv'])} + compute "
                     f"{fmt(f['compute'])} = {fmt(f['total'])} MiB")
    lines += [f"  {n}" for n in p.notes]
    if p.body:
        lines.append("local_model_set: " + json.dumps(p.body))
        lines.append("  podman run arguments: " + shlex.join(p.body["extra_run_args"]))
        lines.append("  llama-server arguments: " + shlex.join(p.body["extra_args"]))
    for name, argv in p.podman:
        lines.append(f"  {name}: {shlex.join(argv)}")
    lines.append("")
    return "\n".join(lines)


# ---------------------------------------------------------------------------------------------
# The report
# ---------------------------------------------------------------------------------------------

def case_table(res):
    """The Markdown of one measured case: a row per card, MiB."""
    p, log = res["plan"], res.get("log") or {}
    lines = [f"### {p.case}: {p.title}", ""]
    if res.get("status") != "measured":
        return "\n".join(lines + [f"Not measured: {res.get('reason', p.reason)}", ""])
    lines.append(f"Run arguments: `{shlex.join(p.body['extra_run_args'])}`; llama-server: "
                 f"`{shlex.join(p.body['extra_args'])}`; ctx {p.body['ctx_size']}.")
    est = res.get("estimate")
    charge = res.get("charge") or []
    lines.append(f"Start {'ok' if res.get('start_ok') else 'refused'} in {res.get('start_s', 0):.1f} s; "
                 f"lmgw's estimate {fmt(est)} MiB, charged "
                 + ("; ".join(f"{fmt(c.get('bytes'))} MiB on {len(c.get('cards') or [])} card(s)"
                              for c in charge) or "–") + ".")
    if log.get("cuda_found") is not None:
        lines.append(f"CUDA devices the engine found: {log['cuda_found']}; offloaded "
                     f"{log.get('offloaded') or '?'} layers.")
    lines.append("")
    lines.append("| Card | Engine device | Free before | Measured (process) | After a request | "
                 "lmgw's reading | Weights (log) | KV (log) | Compute (log) | Other (log) | "
                 "Overhead | §4.3 weights + KV | Layers (log) |")
    lines.append("|" + "---|" * 13)
    sums = {}
    for row in res["cards"]:
        cells = [row["label"], row.get("dev") or "–", fmt(row.get("free_before")),
                 fmt(row.get("measured")), fmt(row.get("after_request")), fmt(row.get("lmgw")),
                 fmt(row.get("weights")), fmt(row.get("kv")), fmt(row.get("compute")),
                 fmt(row.get("other")), fmt(row.get("overhead")), fmt(row.get("predicted")),
                 "–" if row.get("layers") is None else str(row["layers"])]
        for k in ("measured", "after_request", "lmgw", "weights", "kv", "compute", "other",
                  "overhead", "predicted"):
            if row.get(k) is not None:
                sums[k] = sums.get(k, 0) + row[k]
        lines.append("| " + " | ".join(cells) + " |")
    lines.append("| Sum | | | " + " | ".join(fmt(sums.get(k)) for k in (
        "measured", "after_request", "lmgw", "weights", "kv", "compute", "other", "overhead",
        "predicted")) + " | |")
    lines.append("")
    for n in res.get("findings") or []:
        lines.append(f"- {n}")
    for w in res.get("warnings") or []:
        lines.append(f"- warning: {w}")
    if log.get("raw"):
        lines += ["", "<details><summary>the log's device and buffer lines</summary>", "", "```"]
        lines += log["raw"] + (log.get("breakdown") or []) + ["```", "</details>"]
    lines.append("")
    return "\n".join(lines)


def order_table(res):
    p = res["plan"]
    lines = [f"### {p.case}: {p.title}", ""]
    if res.get("status") != "measured":
        return "\n".join(lines + [f"Not measured: {res.get('reason', p.reason)}", ""])
    passed = [c.label() for c in reversed(p.cards)]
    lines.append("Passed, in this order: " + ", ".join(passed) + ".")
    lines.append("")
    lines.append("| Variant | Engine order (device: card) | How matched |")
    lines.append("|---|---|---|")
    for v in res["variants"]:
        if v.get("error"):
            lines.append(f"| {v['name']} | failed: {v['error']} | |")
            continue
        order = ", ".join(f"{d['dev']}: card {d['card']}" for d in v["devices"])
        how = "; ".join(sorted({d["how"] + (" (same name, by free memory)" if d.get("ambiguous") else "")
                                for d in v["devices"]}))
        lines.append(f"| {v['name']} | {order} | {how} |")
    lines.append("")
    for v in res["variants"]:
        lines += [f"<details><summary>{v['name']}: output</summary>", "", "```",
                  (v.get("output") or "").rstrip(), "```", "</details>", ""]
    return "\n".join(lines)


def summary_table(results):
    """The engine's cost per card across the measured row cases (D5)."""
    rows = []
    for r in results:
        if r.get("status") != "measured" or r["plan"].case == "order":
            continue
        cards = r["cards"]
        meas = sum(c.get("measured") or 0 for c in cards)
        over = [c.get("overhead") for c in cards]
        known = [o for o in over if o is not None]
        rows.append(f"| {r['plan'].case} | {len(cards)} | {fmt(meas)} | {fmt(r.get('estimate'))} | "
                    f"{fmt(meas - (r.get('estimate') or 0)) if r.get('estimate') else '–'} | "
                    f"{', '.join(fmt(o) for o in over)} | "
                    f"{fmt(sum(known) // len(known)) if known else '–'} |")
    if not rows:
        return ""
    return "\n".join([
        "### The engine's cost per card", "",
        f"Overhead: what the process holds on a card less the buffers the log names there "
        f"(LLAMA_MARGIN today: {LLAMA_MARGIN_MIB} MiB).", "",
        "| Case | Cards | Measured (sum) | lmgw's estimate | Measured − estimate | Overhead per card "
        "| Mean overhead |",
        "|---|---|---|---|---|---|---|", *rows, ""])


def analyse(res, plan, gpus_by_uuid, log, lw, n_layer, kv_total, n_cpu_moe):
    """Fill res["cards"] and res["findings"] from the readings and the log."""
    dev_of = {}
    for d in match_devices(log["using"] or log["cuda_devices"], list(gpus_by_uuid.values())):
        if d.get("uuid"):
            dev_of[d["uuid"]] = d["dev"]
    assumed = False
    if not dev_of and log["cuda_found"] == len(plan.cards) and (
            len(plan.cards) == 1 or "CUDA_DEVICE_ORDER=PCI_BUS_ID" in plan.body.get("extra_run_args", [])):
        # The log names no PCI address: CUDAi is taken as the i-th card in PCI order, which is
        # what CUDA_DEVICE_ORDER=PCI_BUS_ID promises and the order case checks.
        dev_of = {c.id: f"CUDA{i}" for i, c in enumerate(plan.cards)}
        assumed = len(plan.cards) > 1
    bufs = buffers_by_dev(log)
    assignment = assign_layers(plan.entries or [1], n_layer) if n_layer else []
    pred = predict_cards(lw, assignment, len(plan.cards), kv_total, n_cpu_moe) if lw and assignment else None
    counts = layer_counts(log["layers"], n_layer) if n_layer else {}
    rows = []
    for i, c in enumerate(plan.cards):
        dev = dev_of.get(c.id)
        b = bufs.get(dev, {}) if dev else {}
        weights = b.get("model")
        kv = b.get("KV")
        compute = b.get("compute")
        other = sum(v for k, v in b.items() if k not in ("model", "KV", "compute")) if b else None
        named = sum(b.values()) if b else None
        measured = res["measured"].get(c.id)
        rows.append({"label": c.label(), "dev": dev, "free_before": res["free_before"].get(c.id),
                     "measured": measured, "after_request": res["after_request"].get(c.id),
                     "lmgw": res["lmgw_cards"].get(c.id), "weights": weights, "kv": kv,
                     "compute": compute, "other": other or None,
                     "overhead": measured - named if measured is not None and named else None,
                     "predicted": pred["weights"][i] + pred["kv"][i] if pred else None,
                     "layers": counts.get(dev) if dev else None})
    res["cards"] = rows
    f = res.setdefault("findings", [])
    if assumed:
        f.append("The log names no PCI address per device: CUDA0, CUDA1, … were taken as the cards "
                 "in PCI order (CUDA_DEVICE_ORDER=PCI_BUS_ID; the order case checks it).")
    stray = [u for u in res["measured"] if u not in {c.id for c in plan.cards}]
    if stray:
        f.append(f"the process also holds memory on cards it was not given: {', '.join(stray)}")
    if plan.entries and len(plan.entries) > 1 and n_layer:
        devs = [dev_of.get(c.id) for c in plan.cards]
        f.append("Layer counts: " + entries_verdict(plan.entries, devs, counts, n_layer))
        if pred:
            oc = pred["output_card"]
            f.append(f"Output layer ({fmt(lw['output'])} MiB) predicted on "
                     f"{plan.cards[oc].label() if oc is not None else '?'}.")
    if pred:
        for i, c in enumerate(plan.cards):
            w = rows[i]["weights"]
            if w is not None:
                f.append(f"{c.label()}: weights in the log {fmt(w)} MiB, §4.3 predicts "
                         f"{fmt(pred['weights'][i])} MiB ({pred['layers'][i]} repeating layers"
                         f"{', the output layer' if pred['output_card'] == i else ''}).")
    for ph in ("draft", "mmproj"):
        pb = buffers_by_dev(log, ph)
        if pb:
            f.append(f"{ph} buffers by device: " + "; ".join(
                f"{d}: " + ", ".join(f"{k} {fmt(v)}" for k, v in kinds.items()) for d, kinds in pb.items()))
    if log["clip_backend"]:
        f.append("The projector runs on " + ", ".join(sorted(set(log["clip_backend"]))) + ".")
    for e in log["errors"][:5]:
        f.append(f"log: {e}")


# ---------------------------------------------------------------------------------------------
# Talking to lmgw, nvidia-smi and the inner Podman (the live part)
# ---------------------------------------------------------------------------------------------

class _NamedHandler(urllib.request.HTTPSHandler):
    """HTTPS whose certificate is verified against `name` rather than the URL's host: lmgw's
    TLS listener on 127.0.0.1 with the certificate of the name people use."""

    def __init__(self, name):
        super().__init__(context=ssl.create_default_context())
        self.name = name

    def https_open(self, req):
        name = self.name

        class Conn(http.client.HTTPSConnection):
            def connect(self):
                http.client.HTTPConnection.connect(self)
                self.sock = self._context.wrap_socket(self.sock, server_hostname=name)

        return self.do_open(Conn, req, context=self._context)


class Lmgw:
    def __init__(self, base, token, tls_name=None):
        self.base = base.rstrip("/")
        self.h = {"authorization": f"Bearer {token}"}
        self.opener = urllib.request.build_opener(*([_NamedHandler(tls_name)] if tls_name else []))

    def call(self, path, body=None, timeout=60):
        data = None if body is None else json.dumps(body).encode()
        h = {**self.h, **({"content-type": "application/json"} if body is not None else {})}
        req = urllib.request.Request(self.base + path, data, h)
        try:
            r = self.opener.open(req, timeout=timeout)
            return r.status, json.load(r)
        except urllib.error.HTTPError as e:
            b = e.read()
            try:
                return e.code, json.loads(b)
            except ValueError:
                return e.code, b.decode("utf-8", "replace")
        except (urllib.error.URLError, OSError) as e:
            # No answer (refused, a timeout): status 0, which every caller reads as a failure.
            return 0, f"no answer from {self.base}{path.split('?')[0]}: {e}"

    def get(self, route, /, **q):
        q = {k: v for k, v in q.items() if v is not None}
        return self.call(route + ("?" + urllib.parse.urlencode(q) if q else ""))

    def op(self, name, args, timeout=120):
        return self.call(f"/api/op/{name}", args, timeout=timeout)


def run(cmd, timeout=60, check=True):
    return subprocess.run(cmd, capture_output=True, text=True, timeout=timeout, check=check)


class Host:
    """nvidia-smi on this machine, and the inner Podman and the models dir through Docker."""

    def __init__(self, docker):
        self.docker = docker
        self.smi_ok = self._has_smi()

    @staticmethod
    def _has_smi():
        try:
            run(["nvidia-smi", "-L"], timeout=30)
            return True
        except (OSError, subprocess.SubprocessError):
            return False

    def gpus(self):
        out = run(["nvidia-smi", f"--query-gpu={SMI_GPU_QUERY}", "--format=csv,noheader,nounits"])
        return parse_smi_gpus(out.stdout)

    def apps(self):
        out = run(["nvidia-smi", "--query-compute-apps=gpu_uuid,pid,used_memory",
                   "--format=csv,noheader,nounits"])
        return parse_smi_apps(out.stdout)

    def in_lmgw(self, shell, timeout=60, check=True):
        """A shell line in lmgw's container (as is, outside its user namespace)."""
        if self.docker:
            return run(["docker", "exec", self.docker, "sh", "-c", shell], timeout=timeout, check=check)
        return run(["sh", "-c", shell], timeout=timeout, check=check)

    def podman(self, argv, timeout=300):
        """The inner Podman, only through lmgw's namespaces (docs/server.md section 14)."""
        line = shlex.join(argv)
        if self.docker:
            line = f'exec nsenter -t "$(pgrep -o -x lmgw-server)" --user --mount -- {line}'
        return self.in_lmgw(line, timeout=timeout, check=False)

    def spec_globals(self, prefix):
        """`--cdi-spec-dir` as lmgw passes it ($XDG_RUNTIME_DIR/lmgw-machine/<instance>/cdi)."""
        try:
            out = self.in_lmgw('ls -d "$XDG_RUNTIME_DIR"/lmgw-machine/*/cdi', timeout=30).stdout.split()
        except (OSError, subprocess.SubprocessError):
            return []
        pick = [d for d in out if d.rstrip("/").split("/")[-2] == prefix] or out[:1]
        return ["--cdi-spec-dir", pick[0]] if pick else []

    def open_model(self, path):
        """(stream, size, close) of a file in lmgw's models dir."""
        if self.docker:
            size = int(self.in_lmgw(f"stat -c %s -- {shlex.quote(path)}", timeout=30).stdout.strip())
            p = subprocess.Popen(["docker", "exec", self.docker, "cat", "--", path],
                                 stdout=subprocess.PIPE)
            return p.stdout, size, lambda: (p.kill(), p.wait())
        f = open(path, "rb")
        return f, os.fstat(f.fileno()).st_size, f.close


def alias_of(pid):
    """The `--alias` a process was started with, or None."""
    try:
        with open(f"/proc/{pid}/cmdline", "rb") as f:
            argv = f.read().split(b"\0")
    except OSError:
        return None
    for i, a in enumerate(argv[:-1]):
        if a == b"--alias":
            return argv[i + 1].decode("utf-8", "replace")
    return None


def per_card(apps, model_id, cache):
    """{uuid: bytes} the model's processes hold on each card."""
    out = {}
    for uuid, pid, used in apps:
        if pid not in cache:
            cache[pid] = alias_of(pid)
        if cache[pid] == model_id and used is not None:
            out[uuid] = out.get(uuid, 0) + used
    return out


class Watchdog(threading.Thread):
    """Reads nvidia-smi every second while a case runs; stops the case's container when a
    card's free memory drops below the floor."""

    def __init__(self, host, lm, model_id, floor):
        super().__init__(daemon=True)
        self.host, self.lm, self.model_id, self.floor = host, lm, model_id, floor
        self.halt = threading.Event()
        self.tripped = None

    def run(self):
        while not self.halt.wait(1.0):
            try:
                low = [g for g in self.host.gpus() if g["free"] < self.floor]
            except (OSError, subprocess.SubprocessError):
                continue
            if low and not self.tripped:
                self.tripped = ", ".join(f"card {g['index']} has {fmt(g['free'])} MiB free" for g in low)
                print(f"\n!! watchdog: {self.tripped} (floor {fmt(self.floor)} MiB): stopping "
                      f"{self.model_id} now", flush=True)
                if self.model_id:
                    self.lm.op("container", {"model": self.model_id, "action": "stop"})

    def stop(self):
        self.halt.set()
        self.join(5)


class Session:
    """The live run: what it created, and the clean-up."""

    def __init__(self, lm, host, a):
        self.lm, self.host, self.a = lm, host, a
        self.created = []
        self.abort = None

    def exists(self, model_id):
        st, _ = self.lm.get("/api/local-model", model_id=model_id, target="chat")
        return st == 200

    def remove(self, model_id):
        st, r = self.lm.op("container", {"model": model_id, "action": "stop"}, timeout=180)
        st2, r2 = self.lm.op("local_model_set", {"action": "delete", "model_id": model_id}, timeout=180)
        print(f"  removed {model_id}: stop HTTP {st}, delete HTTP {st2}: "
              f"{(r2 or {}).get('message') if isinstance(r2, dict) else r2}")

    def cleanup(self):
        old = signal.signal(signal.SIGINT, signal.SIG_IGN)
        try:
            if self.created:
                print("\ncleaning up (Ctrl-C is ignored until this is done):")
            for mid in list(self.created):
                try:
                    self.remove(mid)
                    self.created.remove(mid)
                except Exception as e:  # noqa: BLE001 - every row gets its try
                    print(f"  could not remove {mid}: {e}; remove it by hand (Models page)")
            if self.a.cases_run_order:
                self.host.podman(["podman", "rm", "-f", "--ignore", f"{PREFIX}list-devices"], timeout=60)
        finally:
            signal.signal(signal.SIGINT, old)

    def free_now(self):
        st, v = self.lm.get("/api/vram")
        cards = cards_from_vram(v if st == 200 else {})
        gpus = self.host.gpus() if self.host.smi_ok else []
        return apply_smi(cards, gpus), gpus

    def run_order(self, plan):
        res = {"plan": plan, "variants": []}
        cards, gpus = self.free_now()
        ok, _ = fit_check([c for c in cards if c.id in {x.id for x in plan.cards}],
                          plan.planned, self.a.margin_mib * MIB)
        if not ok:
            return {**res, "status": "skipped",
                    "reason": "a card no longer holds a CUDA context plus the margin"}
        self.a.cases_run_order = True
        for name, argv in plan.podman:
            wd = Watchdog(self.host, self.lm, None, self.a.floor_mib * MIB)
            wd.start()
            try:
                before = {g["uuid"]: g["free"] for g in self.host.gpus()}
                out = self.host.podman(argv, timeout=300)
            finally:
                wd.stop()
            text = (out.stdout or "") + (out.stderr or "")
            devs = parse_list_devices(text)
            snap = [{**g, "free": before.get(g["uuid"], g["free"])} for g in gpus]
            v = {"name": name, "argv": argv, "output": text, "devices": match_devices(devs, snap)}
            if out.returncode != 0 or not devs:
                v["error"] = f"exit {out.returncode}" + ("" if devs else ", no device listed")
            res["variants"].append(v)
            if wd.tripped:
                self.abort = wd.tripped
                break
        return {**res, "status": "measured"}

    def settle(self, model_id, cache, wd, seconds):
        """The highest per-card reading over a few seconds, once it stops rising."""
        best, steady, t0 = {}, 0, time.time()
        while time.time() - t0 < seconds and not wd.tripped:
            try:
                got = per_card(self.host.apps(), model_id, cache)
            except (OSError, subprocess.SubprocessError):
                time.sleep(1)
                continue
            grew = any(v > best.get(k, 0) for k, v in got.items())
            for k, v in got.items():
                best[k] = max(best.get(k, 0), v)
            steady = 0 if grew else steady + 1
            if steady >= 3 and time.time() - t0 >= 5:
                break
            time.sleep(1)
        return best

    def run_row(self, plan, layouts, n_layer):
        a, lm = self.a, self.lm
        res = {"plan": plan, "measured": {}, "after_request": {}, "lmgw_cards": {},
               "free_before": {}, "warnings": []}
        cards, gpus = self.free_now()
        now = {c.id: c for c in cards}
        fresh = [now[c.id] for c in plan.cards if c.id in now]
        ok, lines = fit_check(fresh, plan.planned, a.margin_mib * MIB)
        res["checked"] = lines
        if len(fresh) != len(plan.cards) or not ok:
            return {**res, "status": "skipped", "reason": "free memory now: " + "; ".join(lines)}
        res["free_before"] = {c.id: c.free for c in fresh}
        mid = plan.model_id
        _, v0 = lm.get("/api/vram")
        others = {x.get("model") for x in (v0 if isinstance(v0, dict) else {}).get("resident", [])}
        # Registered before the call: a Ctrl-C while lmgw creates it still removes it.
        self.created.append(mid)
        st, r = lm.op("local_model_set", plan.body, timeout=300)
        if st != 200 or not isinstance(r, dict) or not r.get("ok"):
            if not self.exists(mid):
                self.created.remove(mid)
            return {**res, "status": "failed", "reason": f"create refused (HTTP {st}): {r}"}
        res["warnings"] += (r.get("warnings") or []) + (r.get("advisories") or [])
        _, detail = lm.get("/api/local-model", model_id=mid, target="chat")
        res["command_line"] = detail.get("command_line") if isinstance(detail, dict) else None
        wd = Watchdog(self.host, lm, mid, a.floor_mib * MIB)
        wd.start()
        cache = {}
        try:
            t = time.time()
            st, r = lm.op("container", {"model": mid, "action": "start"}, timeout=900)
            res["start_s"] = time.time() - t
            res["start_ok"] = st == 200 and isinstance(r, dict) and r.get("ok")
            if isinstance(r, dict):
                res["warnings"] += r.get("warnings") or []
                res["start_message"] = r.get("message")
            if res["start_ok"] and not wd.tripped:
                res["measured"] = self.settle(mid, cache, wd, a.settle_s)
                _, v = lm.get("/api/vram")
                v = v if isinstance(v, dict) else {}
                gone = others - {x.get("model") for x in v.get("resident", [])}
                if gone:
                    res["warnings"].append("lmgw stopped " + ", ".join(sorted(gone)) + " while this "
                                           "case started (admission's eviction, or its idle timer)")
                rr = next((x for x in v.get("resident", []) if x.get("model") == mid), {})
                res["estimate"] = rr.get("estimated_bytes")
                res["charge"] = rr.get("charge")
                res["lmgw_cards"] = {c["id"]: c.get("bytes") for c in rr.get("cards") or []}
                if not a.no_chat and not wd.tripped:
                    prefix = a.public_prefix
                    body = {"model": f"{prefix}/{mid}" if prefix else mid, "max_tokens": 16,
                            "messages": [{"role": "user", "content": "Say hello in five words."}]}
                    cst, c = lm.call("/v1/chat/completions", body, timeout=300)
                    res["chat"] = f"HTTP {cst}"
                    if cst != 200:
                        res["warnings"].append(f"the chat request answered HTTP {cst}: {str(c)[:300]}")
                    res["after_request"] = self.settle(mid, cache, wd, 8)
            _, lg = lm.op("container", {"model": mid, "action": "logs", "tail": 20000}, timeout=120)
            res["log_text"] = lg.get("logs", "") if isinstance(lg, dict) else str(lg)
        finally:
            lm.op("container", {"model": mid, "action": "stop"}, timeout=180)
            wd.stop()
        # Wait for the process to let go of the cards, and read the log once more: llama.cpp
        # prints its memory breakdown at exit, and a stopped container keeps its log.
        for _ in range(60):
            try:
                if not per_card(self.host.apps(), mid, cache):
                    break
            except (OSError, subprocess.SubprocessError):
                pass
            time.sleep(1)
        _, lg = lm.op("container", {"model": mid, "action": "logs", "tail": 20000}, timeout=120)
        if isinstance(lg, dict) and len(lg.get("logs", "")) > len(res.get("log_text", "")):
            res["log_text"] = lg["logs"]
        if wd.tripped:
            self.abort = wd.tripped
            res["warnings"].append(f"stopped by the watchdog: {wd.tripped}")
        if not res.get("start_ok"):
            return {**res, "status": "failed", "reason": f"the start failed: {res.get('start_message')}",
                    "log": parse_engine_log(res.get("log_text", ""))}
        model = plan.body["gguf_path"]
        log = parse_engine_log(res.get("log_text", ""))
        res["log"] = log
        analyse(res, plan, {g["uuid"]: g for g in gpus}, log, layouts.get(model),
                n_layer.get(model), plan.footprint.get("kv", 0), plan.body.get("n_cpu_moe") or 0)
        if plan.case.startswith("uuid"):
            ok = res["start_ok"] and res["measured"] and set(res["measured"]) <= {plan.cards[0].id}
            res["findings"].insert(0, f"A start with `--device nvidia.com/gpu={plan.cards[0].id}`"
                                      f"{' and NVIDIA_VISIBLE_DEVICES' if plan.case == 'uuid-nvd' else ''}"
                                      f" {'runs, on that card alone' if ok else 'did not run as asked'}"
                                      f" (CUDA devices found: {log['cuda_found']}).")
        return {**res, "status": "measured"}


# ---------------------------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------------------------

def args_parser():
    P = argparse.ArgumentParser(description=__doc__.split("\n")[0],
                                formatter_class=argparse.RawDescriptionHelpFormatter, epilog=__doc__)
    P.add_argument("--base", required=True, help="lmgw's URL, e.g. https://lmgw.example:8443")
    P.add_argument("--token", default=os.environ.get("LMGW_TOKEN"),
                   help="an owner key (default: $LMGW_TOKEN, which keeps it out of ps)")
    P.add_argument("--tls-name",
                   help="verify lmgw's certificate against this name (for a --base on 127.0.0.1)")
    P.add_argument("--run", action="store_true", help="start the cases (without it: a dry run)")
    P.add_argument("--cases", default=",".join(DEFAULT_CASES),
                   help=f"comma-separated, of {','.join(CASES)} (default: {','.join(DEFAULT_CASES)})")
    P.add_argument("--file", default=BASE_FILE, help="the base model, under the chat models dir")
    P.add_argument("--draft", help="a drafter for the base model (draft, draft-dev)")
    P.add_argument("--vl-file", help="a vision model (mmproj)")
    P.add_argument("--mmproj", help="its projector (mmproj)")
    P.add_argument("--moe-file", help="a small mixture-of-experts model (moe-cpu, moe-cache)")
    P.add_argument("--n-cpu-moe", type=int, default=2)
    P.add_argument("--moe-cache-mib", type=int, default=256)
    P.add_argument("--ctx", type=int, default=16384, help="every row's context (§1: 16384)")
    P.add_argument("--split2", help="two layer counts, e.g. 15,22 (default: 40 %% / 60 %% of n_layer + 1)")
    P.add_argument("--split3", help="three layer counts (default: 25 / 35 / 40 %% of n_layer + 1)")
    P.add_argument("--margin-mib", type=int, default=4096,
                   help="kept free on every card used, beyond the plan")
    P.add_argument("--engine-mib", type=int, default=1024,
                   help="the plan's guess of the CUDA context per card")
    P.add_argument("--list-devices-mib", type=int, default=768, help="the plan of a --list-devices per card")
    P.add_argument("--floor-mib", type=int, default=1536, help="the watchdog stops a case below this free")
    P.add_argument("--max-model-mib", type=int, default=6144, help="larger models need --allow-large")
    P.add_argument("--allow-large", action="store_true")
    P.add_argument("--settle-s", type=int, default=25, help="how long a start's memory is watched")
    P.add_argument("--no-chat", action="store_true", help="no request after the start")
    P.add_argument("--no-verbose", action="store_true", help="rows without -v (no layer assignment lines)")
    P.add_argument("--split-without-nvd", action="store_true",
                   help="the split cases without NVIDIA_VISIBLE_DEVICES (if uuid-nvd fails)")
    P.add_argument("--docker", default="lmgw",
                   help="lmgw's Docker container ('' for lmgw under Podman on this host)")
    P.add_argument("--entrypoint", help="the chat image's llama-server, when it is not the entrypoint")
    P.add_argument("--models-host-dir",
                   help="the chat models dir as this host sees it (else docker exec cat)")
    P.add_argument("--clean-leftovers", action="store_true",
                   help="stop and delete rows named wp0-<case> a killed run left behind")
    P.add_argument("--out-dir", help="where --run writes report.md, raw.json and the logs")
    return P


def gather(lm, host, a, cases):
    """Everything the plans read, read-only."""
    st, s = lm.get("/api/settings-full")
    if st != 200:
        sys.exit(f"/api/settings-full answered HTTP {st}: {s}")
    router = s.get("router") or {}
    a.public_prefix = (router.get("public_prefix") or "").strip("/")
    ctx = {"class_args": router.get("extra_run_args") or [], "image": router.get("image"),
           "models_dir": router.get("models_dir") or "", "footprints": {}, "n_layer": {},
           "layouts": {}, "sizes": {}, "files": set(), "flags": set()}
    st, files = lm.get("/api/gguf-files")
    if st == 200:
        ctx["models_dir"] = files.get("models_dir") or ctx["models_dir"]
        for f in files.get("files") or []:
            ctx["files"].add(f["path"])
            ctx["sizes"][f["path"]] = int(f.get("size_bytes") or 0)
    st, flags = lm.get("/api/llama-flags")
    if st == 200:
        ctx["flags"] = set(flags.get("flags") or [])
    for need in ("tensor-split", "split-mode"):
        if ctx["flags"] and need not in ctx["flags"]:
            print(f"note: the chat image's llama-server has no --{need}")
    if "order" in cases:
        ctx["globals"] = host.spec_globals(s.get("container_prefix") or "lmgw")
    models = {a.file}
    if a.vl_file:
        models.add(a.vl_file)
    if a.moe_file:
        models.add(a.moe_file)
    for m in models:
        if m not in ctx["files"]:
            continue
        st, ins = lm.get("/api/model-inspect", path=m, target="chat")
        if st == 200 and ins.get("block_count"):
            ctx["n_layer"][m] = int(ins["block_count"])
        try:
            if a.models_host_dir:
                path = os.path.join(a.models_host_dir, m)
                f = open(path, "rb")
                stream, size, close = f, os.fstat(f.fileno()).st_size, f.close
            else:
                stream, size, close = host.open_model(f"{ctx['models_dir'].rstrip('/')}/{m}")
            try:
                ctx["layouts"][m] = layer_weights(read_gguf_layout(stream, size))
            finally:
                close()
        except (OSError, ValueError, EOFError, subprocess.SubprocessError) as e:
            print(f"note: {m}'s tensor sizes could not be read ({e}): no §4.3 prediction for it")
    for case in cases:
        if case == "order":
            continue
        q = {"ctx_size": a.ctx, "n_gpu_layers": 999}
        if case == "mmproj":
            q.update(gguf_path=a.vl_file, mmproj_path=a.mmproj)
        elif case.startswith("moe"):
            q.update(gguf_path=a.moe_file, n_cpu_moe=a.n_cpu_moe)
            if case == "moe-cache":
                q["moe_cache_mib"] = a.moe_cache_mib
        else:
            q["gguf_path"] = a.file
            if case.startswith("draft"):
                q["draft_gguf_path"] = a.draft
        if not q.get("gguf_path") or None in q.values():
            continue
        st, rp = lm.get("/api/ladder-rung-plan", **q)
        if st == 200:
            fp = rp.get("footprint") or {}
            ctx["footprints"][case] = {"weights": int(fp.get("weights_bytes") or 0),
                                       "kv": int(fp.get("kv_cache_bytes") or 0),
                                       "compute": int(fp.get("compute_bytes") or 0),
                                       "total": int(fp.get("total_bytes") or 0)}
        else:
            print(f"note: /api/ladder-rung-plan for {case} answered HTTP {st}: {rp}")
    return ctx


def main(argv=None):
    a = args_parser().parse_args(argv)
    a.cases_run_order = False
    if not a.token:
        sys.exit("an owner key is needed: --token or $LMGW_TOKEN")
    cases = [c.strip() for c in a.cases.split(",") if c.strip()]
    bad = [c for c in cases if c not in CASES]
    if bad:
        sys.exit(f"unknown case(s): {', '.join(bad)}; the cases are {', '.join(CASES)}")
    lm = Lmgw(a.base, a.token, a.tls_name)
    host = Host(a.docker)
    st, ver = lm.get("/api/version")
    print(f"lmgw {ver.get('version') if isinstance(ver, dict) else ver} at {a.base}; "
          f"{'RUN' if a.run else 'DRY RUN (nothing is started; --run starts)'}")
    if a.run and not host.smi_ok:
        sys.exit("--run needs nvidia-smi on this machine (the watchdog reads it): run it where the cards are")
    sess = Session(lm, host, a)

    leftovers = [PREFIX + c for c in CASES if sess.exists(PREFIX + c)]
    if leftovers:
        if a.clean_leftovers and a.run:
            print("removing rows a killed run left: " + ", ".join(leftovers))
            for mid in leftovers:
                sess.remove(mid)
        else:
            print("rows a killed run left: " + ", ".join(leftovers) +
                  " (--run --clean-leftovers stops and deletes exactly these)")
            if a.run:
                sys.exit(1)

    ctx = gather(lm, host, a, cases)
    cards, gpus = sess.free_now()
    if not cards:
        sys.exit("lmgw's /api/vram lists no card")
    print("\n# The cards now (free = the lower of lmgw's, less what it keeps free, and nvidia-smi's)\n")
    for c in cards:
        print(f"- {c.label()} {c.id} {c.pci}: {fmt(c.free)} of {fmt(c.total)} MiB free")
    print()
    plans = [make_plan(c, cards, ctx, a) for c in cases]
    for p in plans:
        print(plan_text(p))
    if not a.run:
        return 0

    out_dir = a.out_dir or "wp0-" + datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    os.makedirs(out_dir, exist_ok=True)

    def on_term(signum, _frame):
        raise KeyboardInterrupt(f"signal {signum}")

    for sig in (signal.SIGTERM, signal.SIGHUP):
        signal.signal(sig, on_term)
    results = []
    try:
        for p in plans:
            if sess.abort:
                results.append({"plan": p, "status": "skipped", "reason": f"aborted: {sess.abort}"})
                continue
            if p.status != "ready":
                results.append({"plan": p, "status": "skipped", "reason": p.reason})
                continue
            print(f"--- {p.case}", flush=True)
            try:
                if p.case == "order":
                    r = sess.run_order(p)
                else:
                    r = sess.run_row(p, ctx["layouts"], ctx["n_layer"])
            except (OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError) as e:
                # The case's own finally has stopped its container; its row goes in the clean-up.
                r = {"plan": p, "status": "failed", "reason": f"{type(e).__name__}: {e}"}
            if p.case != "order":
                if r.get("log_text"):
                    with open(os.path.join(out_dir, f"{p.case}.log"), "w") as f:
                        f.write(r["log_text"])
            print(f"    {r['status']}" + (f": {r.get('reason')}" if r.get("reason") else ""), flush=True)
            results.append(r)
    except KeyboardInterrupt as e:
        print(f"\ninterrupted ({e or 'Ctrl-C'})")
    finally:
        sess.cleanup()

    report = [f"## WP0 measurements, {datetime.now(timezone.utc):%Y-%m-%d %H:%M} UTC", "",
              f"lmgw {ver.get('version') if isinstance(ver, dict) else ver}; context {a.ctx}; "
              f"margin {a.margin_mib} MiB per card; MiB throughout.", ""]
    for r in results:
        report.append(order_table(r) if r["plan"].case == "order" else case_table(r))
    report.append(summary_table(results))
    text = "\n".join(report)
    print(text)
    with open(os.path.join(out_dir, "report.md"), "w") as f:
        f.write(text)
    with open(os.path.join(out_dir, "raw.json"), "w") as f:
        json.dump([{**{k: v for k, v in r.items() if k not in ("plan", "log_text")},
                    "case": r["plan"].case, "body": r["plan"].body, "planned": r["plan"].planned,
                    "cards": r.get("cards") if r.get("cards") is not None else
                    [c.__dict__ for c in r["plan"].cards]} for r in results], f, indent=1, default=str)
    print(f"\nwritten: {out_dir}/report.md, raw.json and the cases' logs")
    return 1 if sess.created else 0


if __name__ == "__main__":
    sys.exit(main())
