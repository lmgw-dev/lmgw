#!/usr/bin/env python3
"""The llama.cpp fit check (per-model containers §4, "A llama-server row is charged what lands on
the card" and "Headroom, auto or fixed"): what VRAM admission charges a llama-server row against
what the card really holds, on a small card. Run from Linux against a running lmgw; it talks to
lmgw's API and reads the card's used memory over ssh with nvidia-smi.

Usage:
  ssh -N -L 18950:127.0.0.1:18950 <user@windows> &
  python3 -I scripts/llama-fit-check.py --base http://127.0.0.1:18950 --token <owner key> \\
      --ssh <user@windows> --models <models dir on that machine>
  # On this machine (Linux, a dev instance from scripts/dev-instance.sh): no --ssh, and the
  # llama-server processes' own figures are read too.
  python3 -I scripts/llama-fit-check.py --base http://127.0.0.1:8897 --token <owner key> \\
      --models <models dir> --file <gguf under it> --rows fit,ngl

<owner key> is the `token=` of the "dashboard login" line in lmgw's log. The models dir must hold
Qwen3-4B-Instruct-2507-Q4_K_M.gguf; the chat class is pointed at it. Three rows are created when
missing: `fit-4b` (no context, layers on auto: llama.cpp's --fit would shrink it), `ngl-4b`
(ctx 8192, --n-gpu-layers 20) and `pinned-4b` (ctx 8192, every layer: held to it as written).
The headroom is set to fixed 1024 MiB for the passes and put back to auto at the end. Each pass
starts one row alone on the card, prints admission's charge, the flags its container got, the
card's used memory less what it held before the start, and the entry's warnings, then stops it.
Prints one line per measurement; nothing is written outside that lmgw.
"""
import argparse
import json
import subprocess
import time
import urllib.error
import urllib.request

P = argparse.ArgumentParser(description=__doc__.split("\n")[0])
P.add_argument("--base", required=True)
P.add_argument("--token", required=True)
P.add_argument("--ssh", help="user@host to read nvidia-smi on; this machine when omitted")
P.add_argument("--models", required=True)
P.add_argument("--file", default="Qwen3-4B-Instruct-2507-Q4_K_M.gguf",
               help="the GGUF under --models the rows run")
P.add_argument("--rows", default="pinned,fit,ngl", help="which passes, in order")
A = P.parse_args()
H = {"authorization": f"Bearer {A.token}"}
MIB = 2**20
FILE = A.file


def call(path, body=None, timeout=900):
    data = None if body is None else json.dumps(body).encode()
    h = {**H, **({"content-type": "application/json"} if body is not None else {})}
    try:
        r = urllib.request.urlopen(urllib.request.Request(A.base + path, data, h), timeout=timeout)
        return r.status, json.load(r)
    except urllib.error.HTTPError as e:
        b = e.read()
        try:
            return e.code, json.loads(b)
        except ValueError:
            return e.code, b


def op(name, args):
    return call(f"/api/op/{name}", args)[1]


def smi(query):
    cmd = ["nvidia-smi", query, "--format=csv,noheader,nounits"]
    if A.ssh:
        cmd = ["ssh", "-o", "BatchMode=yes", A.ssh, " ".join(cmd)]
    return subprocess.run(cmd, check=True, capture_output=True, text=True).stdout


def used_mib():
    return int(smi("--query-gpu=memory.used").strip().splitlines()[0])


def server_mib():
    """What llama-server processes hold, per process (Linux only: WDDM lists none)."""
    if A.ssh:
        return None
    out = smi("--query-compute-apps=process_name,used_memory")
    return sum(int(l.rsplit(",", 1)[1]) for l in out.splitlines() if "llama-server" in l)


def settle_used(seconds=6):
    """The card's used memory once a start has settled: the highest of a few readings."""
    most = 0
    for _ in range(seconds):
        most = max(most, used_mib())
        time.sleep(1)
    return most


def settings(**vram):
    s = call("/api/settings-full")[1]["vram"]
    s.update(vram)
    op("settings_set_full", {"vram": s})


def ensure_rows():
    s = call("/api/settings-full")[1]
    router = s["router"]
    if router.get("models_dir") != A.models:
        router["models_dir"] = A.models
        op("settings_set_full", {"router": router})
    ids = {m["id"] for m in call("/v1/models")[1]["data"]}
    base = {"flash_attn": "on", "jinja": True}
    for mid, extra in (
        ("fit-4b", {}),
        ("ngl-4b", {"ctx_size": 8192, "n_gpu_layers": 20}),
        ("pinned-4b", {"ctx_size": 8192, "n_gpu_layers": 999}),
    ):
        if mid.split("-")[0] not in A.rows.split(","):
            continue
        if mid not in ids:
            r = op("local_model_set", {"action": "create", "model_id": mid, "gguf_path": FILE, **base, **extra})
            print("create", mid, r.get("message"))


def entry(model):
    st = call("/api/status")[1]
    for e in st.get("runtime", {}).get("containers", []) or st.get("containers", []) or []:
        if e.get("model_id") == model or e.get("model") == model:
            return e
    return {}


def resident(model):
    v = call("/api/vram")[1]
    return next((r for r in v["resident"] if r["model"] == model), {}), v


def run(model):
    op("container", {"model": model, "action": "stop"})
    time.sleep(3)
    before = settle_used(3)
    t = time.time()
    r = op("container", {"model": model, "action": "start"})
    dt = time.time() - t
    print(f"{model}: start {'ok' if r.get('ok') else 'refused'} in {dt:.1f} s: {r.get('message')}")
    for w in r.get("warnings") or []:
        print(f"  warning: {w}")
    if not r.get("ok"):
        return
    after = settle_used()
    res, v = resident(model)
    charged = res.get('estimated_bytes', 0) / MIB
    print(f"  admission charges {charged:.0f} MiB; the card holds "
          f"{after - before} MiB more than before the start ({before} -> {after} MiB, nvidia-smi); "
          f"lmgw's share {(v.get('lmgw_share_bytes') or 0) / MIB:.0f} MiB; llama-server holds "
          f"{server_mib()} MiB")
    note = res.get("note") or ""
    if note:
        print(f"  note: {note}")
    body = {"model": model, "messages": [{"role": "user", "content": "Say hello in five words."}],
            "max_tokens": 16}
    st, c = call("/v1/chat/completions", body)
    print(f"  chat: HTTP {st}" + (f", {c['usage']['completion_tokens']} tokens" if st == 200 else f": {c}"))
    print(f"  after the first request: the card holds {settle_used(3) - before} MiB more than "
          f"before the start; llama-server holds {server_mib()} MiB (charged {charged:.0f} MiB)")
    op("container", {"model": model, "action": "stop"})
    time.sleep(3)


print("lmgw", call("/api/version")[1].get("version"))
ensure_rows()
settings(headroom_mode="fixed", headroom_mb=1024, enabled=True)
print(f"card at rest: {used_mib()} MiB used")
for m in A.rows.split(","):
    run(f"{m}-4b")
settings(headroom_mode="auto")
h = call("/api/settings-full")[1]["headroom_now"]
learned = h.get("learned_bytes")
so_far = "-" if learned is None else f"{learned / MIB:.0f} MiB"
print(f"headroom now: {h['sentence']} (learned so far: {so_far}, "
      f"{h['hours_seen']} of {h['hours_needed']} hours)")
