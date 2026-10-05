# lmgw on Arch Linux with an AMD Ryzen APU

lmgw is developed on Fedora with an NVIDIA card, and a few of its shipped
defaults say so. Nothing in the gateway itself is NVIDIA-specific — it talks to
`podman` and to whatever llama.cpp container you point it at — so an AMD Ryzen
laptop runs it fine, but you have to change four things: how you install it,
which container image the model classes use, which devices those containers
get, and (optionally) how GPU memory is accounted for.

This page is the whole list. Everything here is host configuration or Settings
values; none of it needs a code change.

---

## 1. Install (there is no Arch package)

CI builds an RPM, so on Arch you build from source. Tauri v2's Linux
dependencies plus the Rust toolchain:

```sh
sudo pacman -S --needed base-devel git curl wget file openssl pkgconf \
                        webkit2gtk-4.1 gtk3 libsoup3 librsvg \
                        libappindicator-gtk3 rustup
rustup default stable
rustup target add wasm32-unknown-unknown       # the dashboard is a Leptos WASM app
cargo install tauri-cli --version '^2.0' --locked
```

`trunk` builds the dashboard bundle. Take the upstream **prebuilt binary**
rather than `cargo install trunk` — from source it pulls a vendored C
dependency that recent GCC refuses (the same trap `ci/install-build-deps.sh`
documents):

```sh
curl -fsSL https://github.com/trunk-rs/trunk/releases/download/v0.21.14/trunk-x86_64-unknown-linux-gnu.tar.gz \
  | tar -xz -C ~/.cargo/bin trunk
```

Then build and run:

```sh
(cd crates/lmgw-ui && trunk build --release)   # must come first — lmgw-core embeds dist/
cargo tauri build --bundles appimage
```

`cargo tauri dev` is a development run, not a way to run lmgw: a debug build
needs a dev data dir and keeps away from your real one (README, Development).

The binary lands in `target/release/lmgw`, the AppImage under
`target/release/bundle/appimage/`. Data lives in `~/.local/share/lmgw/`
(`LMGW_DATA_DIR` overrides it), and the gateway binds `127.0.0.1:8787` by
default — every URL below assumes that.

**Updates.** The in-app updater installs an RPM with `pkexec dnf`, which your
machine does not have. lmgw checks for this: the background poll will not
open a dialog it cannot act on, and the tray's "Check for Updates…" tells you a
version exists and that you have to rebuild. `git pull && cargo tauri build` is
the update path. Turn the check off entirely in **Settings → Tokens & updates** if you
would rather not hear about it.

**Webview.** The `__NV_DISABLE_EXPLICIT_SYNC=1` workaround lmgw sets is for an
NVIDIA-on-Wayland crash; it is applied only when the NVIDIA driver is actually
loaded, so on AMD lmgw sets nothing and your webview keeps its default hardware
rendering. If the window ever fails to open, `WEBKIT_DISABLE_DMABUF_RENDERER=1 lmgw`
switches WebKit to software rendering; any WebKit render variable you set
yourself is left alone.

---

## 2. Podman prerequisites

lmgw runs every model in its own rootless Podman container. On Arch:

```sh
sudo pacman -S --needed podman passt          # passt provides pasta, podman's default network
```

Two things Fedora sets up for you and Arch does not:

```sh
# 1. Rootless UID/GID ranges — without these, `podman run` fails outright.
grep "$USER" /etc/subuid /etc/subgid || {
  sudo usermod --add-subuids 100000-165535 --add-subgids 100000-165535 "$USER"
  podman system migrate
}

# 2. Access to the GPU render node. Check first — you may already be in it.
id -nG | tr ' ' '\n' | grep -qx render || sudo usermod -aG render,video "$USER"
# log out and back in afterwards
```

`/dev/dri/renderD128` is group `render` on Arch. Rootless Podman does **not**
carry your supplementary groups into the container by default, which is why
every run-args block below includes `--group-add keep-groups` — without it the
container opens the device as nobody and finds no GPU.

SELinux is not enabled on Arch, so the `--security-opt label=disable` in lmgw's
default run args is a harmless no-op. Leave it or drop it, it makes no
difference; the `:Z` volume suffixes lmgw renders are likewise ignored.

---

## 3. Vulkan or ROCm?

**Start with Vulkan.** It needs no userspace stack beyond the Mesa drivers
already inside the image, one device, and it supports every Ryzen iGPU back to
Vega. ROCm is worth trying afterwards if you want to compare speed — it is
usually faster for prompt processing on the newer parts and it is the only one
of the two that rocBLAS-tuned kernels reach.

llama.cpp publishes both (verified 2026-09-19 against ghcr.io):

| | image |
| --- | --- |
| Vulkan | `ghcr.io/ggml-org/llama.cpp:server-vulkan` |
| ROCm | `ghcr.io/ggml-org/llama.cpp:server-rocm` |
| CPU only | `ghcr.io/ggml-org/llama.cpp:server` |

**ROCm only works if your iGPU's gfx target was compiled into the image.** As
of today the ROCm image is built for:

```
gfx908 gfx90a gfx942 gfx1030 gfx1100 gfx1101 gfx1102 gfx1150 gfx1151 gfx1200 gfx1201
```

Where a Ryzen laptop lands:

| SoC | iGPU | gfx | ROCm image |
| --- | --- | --- | --- |
| Ryzen AI Max (Strix Halo) | 8060S | `gfx1151` | built in ✅ |
| Ryzen AI 300 (Strix Point) | 890M | `gfx1150` | built in ✅ |
| Ryzen 7040/8040 (Phoenix, Hawk Point) | 780M / 760M | `gfx1103` | ❌ — needs `HSA_OVERRIDE_GFX_VERSION=11.0.2` |
| Ryzen 6000 (Rembrandt) | 680M | `gfx1035` | ❌ — `HSA_OVERRIDE_GFX_VERSION=10.3.0`, or use Vulkan |
| Ryzen 5000 and older | Vega | `gfx90c` | ❌ — use Vulkan |

Check yours with `podman run --rm --device /dev/kfd --device /dev/dri
--group-add keep-groups docker.io/rocm/dev-ubuntu-24.04 rocminfo | grep gfx`,
or just read the table. `HSA_OVERRIDE_GFX_VERSION` makes ROCm load the nearest
built target's kernels; it usually works and is not guaranteed to — if a model
produces garbage output under an override, that is the override, not lmgw.

### Building your own images (Vulkan/ROCm)

The published ghcr.io images above are the fast path. If you'd rather build
from a specific ref, a PR, or with a gfx target the published image doesn't
carry, use the **Backends** page (`/backends`) instead of a hand-rolled
`podman build` — see [docs/backends.md](backends.md). Its Vulkan and ROCm
profiles exist for all three engines except ik_llama.cpp (no ROCm or CPU
Dockerfile upstream any more) and audio.cpp (no ROCm Dockerfile upstream at
all), but **nobody has built or run one**: only the four CUDA profiles are
verified. For official llama.cpp's ROCm profile, set the build's `arch` field
to your gfx target — it becomes `ROCM_DOCKER_ARCH` (the arch build-arg llama's
ROCm Dockerfile reads; every other backend uses a different arg name, e.g.
`CUDA_DOCKER_ARCH`). If a build fails or verifies broken on your card, that's
useful data lmgw doesn't have yet.

---

## 4. The Settings values to change

Three class blocks in **Settings** carry the NVIDIA defaults. The run-args box
is **one token per line** — flag and value on separate lines — because lmgw
passes each line through as a single `podman run` argument.

### Chat container · llama-server, and Aux container · llama-server

Both classes have their own copy of these fields; change both if you use
embeddings.

**Vulkan**

| field | value |
| --- | --- |
| Image | `ghcr.io/ggml-org/llama.cpp:server-vulkan` |
| Extra podman run args | `--device` / `/dev/dri` / `--group-add` / `keep-groups` (four lines) |

**ROCm**

| field | value |
| --- | --- |
| Image | `ghcr.io/ggml-org/llama.cpp:server-rocm` |
| Extra podman run args | `--device` / `/dev/kfd` / `--device` / `/dev/dri` / `--group-add` / `keep-groups` / `--security-opt` / `seccomp=unconfined` |

On an unsupported gfx target add two more lines: `-e` and
`HSA_OVERRIDE_GFX_VERSION=11.0.2`.

Same thing over the API, if you prefer not to click:

```sh
curl -sS http://127.0.0.1:8787/api/op/settings_set_full \
  -H 'content-type: application/json' -d '{
    "router": {
      "image": "ghcr.io/ggml-org/llama.cpp:server-vulkan",
      "extra_run_args": ["--device","/dev/dri","--group-add","keep-groups"]
    },
    "aux_router": {
      "image": "ghcr.io/ggml-org/llama.cpp:server-vulkan",
      "extra_run_args": ["--device","/dev/dri","--group-add","keep-groups"]
    }
  }'
```

Hit **Apply to running containers** (or `lmgw__container action=apply`)
afterwards — class settings reach a running model only when it restarts.

### Audio class · audio.cpp

audio.cpp publishes **no ROCm image**. Vulkan is the only GPU option:

| field | value |
| --- | --- |
| Image | `ghcr.io/0xshug0/audio.cpp:full-vulkan` |
| Backend | `vulkan` |
| Extra podman run args | `--device` / `/dev/dri` / `--group-add` / `keep-groups` |

The `hip` entry in the Backend dropdown is audio.cpp's own list, not a promise
that an image exists for it. If Vulkan misbehaves, `full-cpu` + backend `cpu`
works for TTS at laptop speeds.

---

## 5. GPU memory accounting on an APU

lmgw refuses to start a model that will not fit rather than letting it OOM, and
it needs a memory figure for that. NVML is NVIDIA-only, so on AMD lmgw reads
amdgpu's own sysfs counters instead (`/sys/class/drm/card*/device/mem_info_*`) —
no ROCm install, no group membership, no root required. You can see which
source it picked on the Traffic page's **GPU memory** row (hover it) or in
`lmgw__status`.

An APU has no dedicated VRAM, so two pools are reported and lmgw counts both:

* the **BIOS carve-out** (`mem_info_vram_total`), typically 512 MiB unless you
  raised "UMA Frame Buffer Size" in firmware, and
* **GTT**, the host pages the GPU may map — where everything past the carve-out
  actually lives.

Capacity is therefore *carve-out + GTT limit*, and free memory is the
carve-out's free bytes plus at most `MemAvailable` of free GTT, because GTT is
your RAM and the kernel's GTT ceiling says nothing about whether the machine
can spare it right now.

Two knobs if that is not the number you want:

* **Settings → GPU → Admission → Budget** (MiB) overrides the measurement entirely. On a 32 GB
  laptop where you also want to run a browser, declaring e.g. `12288` is more
  honest than letting lmgw plan against every page the kernel would hand out.
  `0` means "use the measurement".
* **`Headroom · MiB`** is what stays free above the GGUF-derived estimate
  (compute buffers, the driver's context, fragmentation — none of it derivable
  from the file). Raise it if loads still OOM.

If you want a bigger GTT ceiling than the kernel's default, that is a boot
parameter, not an lmgw setting: `amdgpu.gttsize=<MiB>` (and on newer kernels
`ttm.pages_limit=<pages>`). Strix Halo owners generally want this; a 16 GB
laptop generally does not.

---

## 6. Model settings worth changing on a laptop

`lmgw__local_model_plan` derives its suggestions from the GGUF alone — it does
not know what card you have — so its defaults are "the model's full trained
context, every layer offloaded". On a 780M with a 512 MiB carve-out that is
often not loadable. What to adjust on the model's edit page:

* **`ctx_size`** — the plan proposes the model's trained maximum. Lower it
  until the estimate fits; this is by far the biggest lever, since KV cache
  scales linearly with it.
* **`n_gpu_layers`** — the plan proposes `999` (everything). On an APU that is
  usually still right, because "GPU memory" is your RAM either way; on a
  memory-starved config, lowering it moves layers to the CPU backend.
* **`cache_type_k` / `cache_type_v`** — `q8_0` halves the KV cache and the plan
  already suggests it.
* **`flash_attn`** — the plan sets `on`. If a Vulkan build refuses it for your
  model, set `auto` and llama.cpp picks.

lmgw's model page shows the rendered `podman run …` command line and the
estimate side by side, so you can see what a change did before starting
anything.

---

## 7. Verify, in order

```sh
# 1. The kernel sees the iGPU and exposes its counters (this is what lmgw reads).
cat /sys/class/drm/card*/device/mem_info_vram_total \
    /sys/class/drm/card*/device/mem_info_gtt_total

# 2. The container can see the GPU. Should list a "Radeon" device, not just CPU.
podman run --rm --device /dev/dri --group-add keep-groups \
  ghcr.io/ggml-org/llama.cpp:server-vulkan --list-devices

# 3. ROCm only:
podman run --rm --device /dev/kfd --device /dev/dri --group-add keep-groups \
  --security-opt seccomp=unconfined \
  ghcr.io/ggml-org/llama.cpp:server-rocm --list-devices

# 4. Then in lmgw: add a model, hit Test (or lmgw__local_model_test).
#    It loads the model through the real path and prints the container log on failure.
```

---

## 8. When it goes wrong

| symptom | cause |
| --- | --- |
| `--list-devices` shows no GPU, only CPU | `/dev/dri` not passed, or `--group-add keep-groups` missing |
| `ggml_vulkan: No devices found` | same, or the render node is not group `render` — check `ls -l /dev/dri/renderD128` |
| ROCm: `No GPU agents found` / `HSA_STATUS_ERROR` | gfx target not in the image (see §3) — set `HSA_OVERRIDE_GFX_VERSION` or switch to Vulkan |
| `podman run` fails before the image is even pulled | `/etc/subuid` not configured (see §2) |
| Model test says "not enough VRAM" | lower `ctx_size`, quantize the KV cache, or raise the Budget if the measurement is the thing that is wrong |
| GPU memory row says "admission control … forwarding every request unchanged" | neither probe found a device — lmgw is running unarbitrated, which is safe but silent; declare a Budget to get admission back |
| Window never appears | `WEBKIT_DISABLE_DMABUF_RENDERER=1 lmgw` |

Anything else: the model's **Test** button and `podman logs <container>` carry
the real error. lmgw's own status (`lmgw__status`, or the Traffic page) says
which telemetry source it found and why admission is or is not active.
