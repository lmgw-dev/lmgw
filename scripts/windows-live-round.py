#!/usr/bin/env python3
"""The Windows live round (Windows design §7 "WP8 as built"): chat, image and audio through an
installed lmgw on a Windows machine with a card, on CUDA and on Vulkan, plus the VRAM figures,
admission and the GPU hold. Run from Linux; it only talks to lmgw's API (and, for the Vulkan
pass, to the machine over ssh).

Usage:
  ssh -N -L 18601:127.0.0.1:18601 <user@windows> &        # lmgw's bind addr, loopback only
  python3 -I scripts/windows-live-round.py --base http://127.0.0.1:18601 --token <owner key> \\
      --ssh <user@windows> [--only chat,vulkan,image,audio,vram,hold]

<owner key> is the `token=` of the "dashboard login" line in lmgw's log. The models must be in the
installed app's class dirs (hard links to one copy are fine): chat
`Qwen3.5-0.8B-UD-Q4_K_XL.gguf` and `Qwen3-4B-Instruct-2507-Q4_K_M.gguf`; image
`z_image_turbo-Q4_K.gguf`, `ae.safetensors`, `Qwen3-4B-Instruct-2507-Q4_K_M.gguf`; audio
`Kokoro-82M-GGUF/`, `Parakeet-TDT-0.6B-v3-GGUF/`, `Supertonic-3-GGUF/`. The rows are created
when missing (ctx 8192 for chat, 512² 4-step offloaded image). The script changes settings for
its passes and puts them back: the VRAM headroom to 512 MiB (a 3 GiB model fits a 4 GiB card
beside the default 1 GiB headroom only so), admission off for the image pass (its estimate is
the pipeline's on-disk size, 6.2 GiB, which a 4 GiB card refuses although --offload-to-cpu runs
it), the GPU hold. The Vulkan pass marks llama.cpp's CUDA 12 build by hand over ssh (no setting
forces a variant yet) and clears it with `engine_fallback_clear`, as Try CUDA again does.

Prints one line per measurement; nothing is written outside the machine's lmgw.
"""
import argparse
import base64
import json
import subprocess
import sys
import time
import urllib.error
import urllib.request
import uuid

P = argparse.ArgumentParser(description=__doc__.split("\n")[0])
P.add_argument("--base", required=True)
P.add_argument("--token", required=True)
P.add_argument("--ssh", help="the Windows machine's ssh destination, for the Vulkan pass")
P.add_argument("--only", default="chat,vulkan,image,audio,vram,hold")
A = P.parse_args()
ONLY = set(A.only.split(","))
H = {"authorization": f"Bearer {A.token}"}
PROMPT = "Write a short paragraph about the history of the bicycle."
TEXT = "The quick brown fox jumps over the lazy dog near the river bank."


def call(path, body=None, headers=None, raw=False, timeout=900):
    data = None if body is None else (body if isinstance(body, bytes) else json.dumps(body).encode())
    h = {**H, **({"content-type": "application/json"} if body is not None and not isinstance(body, bytes) else {}), **(headers or {})}
    try:
        r = urllib.request.urlopen(urllib.request.Request(A.base + path, data, h), timeout=timeout)
        return r.status, (r.read() if raw else json.load(r))
    except urllib.error.HTTPError as e:
        b = e.read()
        try:
            return e.code, json.loads(b)
        except ValueError:
            return e.code, b


def op(name, args):
    return call(f"/api/op/{name}", args)[1]


def vram(label):
    v = call("/api/vram")[1]
    dev = v["devices"][0]
    res = ", ".join(f"{r['model']} est {r['estimated_bytes'] / 2**20:.0f} MiB" for r in v["resident"])
    share = v.get("lmgw_share_bytes")
    print(f"  vram {label}: NVML used {dev['used_bytes'] / 2**20:.1f} MiB; lmgw (D3DKMT) "
          f"{(share or 0) / 2**20:.1f} MiB; outside {(v.get('outside_share_bytes') or 0) / 2**20:.1f} MiB; [{res}]")


def timed(f):
    t = time.time()
    r = f()
    return r, time.time() - t


def start(model):
    r, dt = timed(lambda: op("container", {"model": model, "action": "start"}))
    print(f"  start {model}: {dt:.1f} s: {r.get('message')}")
    return r.get("ok")


def stop(model):
    op("container", {"model": model, "action": "stop"})


def vram_settings(**kw):
    s = call("/api/settings-full")[1]["vram"]
    s.update(kw)
    op("settings_set_full", {"vram": s})


def ensure_rows():
    ids = {m["id"] for m in call("/v1/models")[1]["data"]}
    chat = {"ctx_size": 8192, "n_gpu_layers": 999, "flash_attn": "on", "jinja": True, "cache_type_k": "q8_0", "cache_type_v": "q8_0"}
    for mid, f in (("qwen3.5-0.8b", "Qwen3.5-0.8B-UD-Q4_K_XL.gguf"), ("qwen3-4b", "Qwen3-4B-Instruct-2507-Q4_K_M.gguf")):
        if mid not in ids:
            print("create", mid, op("local_model_set", {"action": "create", "model_id": mid, "gguf_path": f, **chat}).get("message"))
    if "image/z-image-turbo" not in ids:
        print("create z-image-turbo", op("image_model_set", {
            "action": "create", "model_id": "z-image-turbo",
            "files": {"diffusion_model": "z_image_turbo-Q4_K.gguf", "vae": "ae.safetensors", "llm": "Qwen3-4B-Instruct-2507-Q4_K_M.gguf"},
            "args": {"cfg_scale": 1.0, "steps": 4, "diffusion_fa": True, "offload_to_cpu": True, "width": 512, "height": 512}}).get("message"))
    if not any(i.startswith("audio/") for i in ids):
        op("audio_catalog", {"action": "refresh"})
    for mid, fam, path, task, extra in (
        ("kokoro", "kokoro_tts", "Kokoro-82M-GGUF", "tts", {}),
        ("parakeet", "parakeet_tdt", "Parakeet-TDT-0.6B-v3-GGUF", "asr", {}),
        ("supertonic", "supertonic", "Supertonic-3-GGUF", "tts", {}),
        ("parakeet-cpu", "parakeet_tdt", "Parakeet-TDT-0.6B-v3-GGUF", "asr", {"backend": "cpu", "threads": 8}),
    ):
        if f"audio/{mid}" not in ids:
            print("create", mid, op("audio_model_set", {"action": "create", "model_id": mid, "family": fam, "path": path, "task": task, "mode": "offline", **extra}).get("message"))


def chat(model):
    body = {"model": model, "messages": [{"role": "user", "content": PROMPT}], "max_tokens": 200}
    (st, r), dt = timed(lambda: call("/v1/chat/completions", body))
    if st != 200:
        print(f"  {model} chat: HTTP {st} {r}")
        return
    print(f"  {model} openai: {r['usage']['completion_tokens']} tok in {dt:.2f} s ({r['choices'][0]['finish_reason']})")
    t = time.time()
    first, usage = None, None
    req = urllib.request.Request(A.base + "/v1/chat/completions", json.dumps({**body, "stream": True, "stream_options": {"include_usage": True}}).encode(), {**H, "content-type": "application/json"})
    with urllib.request.urlopen(req, timeout=900) as s:
        for line in s:
            if not line.startswith(b"data:") or line.strip() == b"data: [DONE]":
                continue
            ev = json.loads(line[5:])
            usage = ev.get("usage") or usage
            if first is None and any(c.get("delta", {}).get("content") or c.get("delta", {}).get("reasoning_content") for c in ev.get("choices", [])):
                first = time.time()
    n = usage["completion_tokens"]
    print(f"  {model} openai stream: ttft {first - t:.3f} s, {n} tok, decode {(n - 1) / (time.time() - first):.1f} tok/s")
    for stream in (False, True):
        (st, r), dt = timed(lambda: call("/v1/messages", {"model": model, "max_tokens": 200, "stream": stream, "messages": [{"role": "user", "content": PROMPT}]}, {"anthropic-version": "2023-06-01"}, raw=stream))
        what = r.decode().count("event: content_block_delta") if stream else r.get("stop_reason")
        print(f"  {model} anthropic {'stream' if stream else 'non-stream'}: HTTP {st} in {dt:.2f} s ({'deltas ' if stream else ''}{what})")


def speak(model):
    (st, wav), dt = timed(lambda: call("/v1/audio/speech", {"model": model, "input": TEXT, "response_format": "wav"}, raw=True))
    print(f"  {model}: HTTP {st}, {len(wav)} bytes in {dt:.2f} s")
    return wav


def transcribe(model, wav):
    b = uuid.uuid4().hex
    body = (f"--{b}\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\n{model}\r\n--{b}\r\nContent-Disposition: form-data; "
            f"name=\"file\"; filename=\"a.wav\"\r\nContent-Type: audio/wav\r\n\r\n").encode() + wav + f"\r\n--{b}--\r\n".encode()
    (st, r), dt = timed(lambda: call("/v1/audio/transcriptions", body, {"content-type": f"multipart/form-data; boundary={b}"}))
    print(f"  {model}: HTTP {st} in {dt:.2f} s: {r.get('text') if isinstance(r, dict) else r!r}")


def image(prompt):
    (st, r), dt = timed(lambda: call("/v1/images/generations", {"model": "image/z-image-turbo", "prompt": prompt, "size": "512x512", "n": 1}))
    size = len(base64.b64decode(r["data"][0]["b64_json"])) if st == 200 else 0
    print(f"  z-image-turbo: HTTP {st} in {dt:.1f} s, {size} bytes PNG" + ("" if st == 200 else f": {r}"))


print("lmgw", call("/api/version")[1].get("version"))
ensure_rows()
vram_settings(headroom_mb=512, enabled=True)
if "chat" in ONLY:
    print("chat, llama.cpp as the machine chooses (CUDA 12 on a 5.2 card)")
    for m in ("qwen3.5-0.8b", "qwen3-4b"):
        stop(m)
        start(m)
        chat(m)
        vram(m)
        stop(m)
if "vulkan" in ONLY and A.ssh:
    print("chat, llama.cpp's Vulkan build (CUDA 12 marked by hand)")
    mark = json.dumps({"marked_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()), "exit": "written by scripts/windows-live-round.py", "line": "CUDA error: no kernel image is available for execution on the device"})
    ps = f"[IO.File]::WriteAllText(\"$env:LOCALAPPDATA\\lmgw\\engines\\llama\\b12244-cuda12\\.lmgw-no-kernel-image\", '{mark}')"
    subprocess.run(["ssh", "-o", "BatchMode=yes", A.ssh, ps], check=True)
    for m in ("qwen3.5-0.8b", "qwen3-4b"):
        start(m)
        chat(m)
        vram(m)
        stop(m)
    r = op("engine_fallback_clear", {"engine": "llama", "variant": "cuda12"})
    llama = next((e for e in r.get("engines", []) if e["engine"] == "llama"), {})
    print(f"  Try CUDA again: llama.cpp runs {llama.get('chosen')} again ({llama.get('chosen_why')})")
if "image" in ONLY:
    print("image, stable-diffusion.cpp (admission off for this pass)")
    vram_settings(enabled=False)
    image("a red bicycle leaning against a white wall, photo")
    image("a blue teapot on a wooden table")
    stop("z-image-turbo")
    vram_settings(enabled=True)
if "audio" in ONLY:
    print("audio, audio.cpp")
    wav = speak("audio/kokoro")
    speak("audio/kokoro")
    transcribe("audio/parakeet", wav)
    transcribe("audio/parakeet", wav)
    vram("kokoro + parakeet")
    speak("audio/supertonic")
    speak("audio/supertonic")
    transcribe("audio/parakeet-cpu", wav)
    transcribe("audio/parakeet-cpu", wav)
if "vram" in ONLY:
    print("admission")
    print("  start qwen3-4b beside the audio rows:", op("container", {"model": "qwen3-4b", "action": "start"}).get("message"))
    (st, r), dt = timed(lambda: call("/v1/chat/completions", {"model": "qwen3-4b", "messages": [{"role": "user", "content": "Say hi."}], "max_tokens": 20}))
    print(f"  a request for qwen3-4b instead: HTTP {st} in {dt:.1f} s")
    vram("after the request")
    stop("qwen3-4b")
    vram_settings(headroom_mb=1024)
    print("  qwen3-4b with the default 1 GiB headroom:", op("container", {"model": "qwen3-4b", "action": "start"}).get("message"))
    vram_settings(headroom_mb=512)
if "hold" in ONLY:
    print("GPU hold")
    start("supertonic")
    wav = speak("audio/kokoro")
    print("  hold on:", op("hold_set", {"active": True}).get("message"))
    st, r = call("/v1/chat/completions", {"model": "qwen3.5-0.8b", "messages": [{"role": "user", "content": "Say hi."}], "max_tokens": 20})
    print(f"  chat under the hold: HTTP {st} {r.get('error', {}).get('code')}")
    transcribe("audio/parakeet-cpu", wav)  # a CPU row keeps serving
    print("  hold off:", op("hold_set", {"active": False}).get("message"))
    (st, r), dt = timed(lambda: call("/v1/chat/completions", {"model": "qwen3.5-0.8b", "messages": [{"role": "user", "content": "Say hi."}], "max_tokens": 20}))
    print(f"  chat after the release: HTTP {st} in {dt:.1f} s")
vram_settings(headroom_mb=1024, enabled=True)
sys.exit(0)
