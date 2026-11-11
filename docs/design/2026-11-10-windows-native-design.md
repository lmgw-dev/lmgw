# lmgw on Windows: a native build with engines as native processes

**Draft, 2026-11-10.** A feasibility spike and the plan that follows from it. WP1 (§3.1), WP2,
WP3 (§4.1) and WP6 (§4.4) are built; the rest is not built yet. §2 and §3 are measurements from the spike; §4 onward is this design's proposal, and "Decisions" separates the owner's rulings from the design's own decisions. "Not
yet" lists what this version leaves out; nothing there is ruled out.

Companion records, cited by short name:
- "per-model containers §n": [per-model containers](2026-08-30-per-model-containers-design.md)
  (one container per model, the registry, the VRAM ledger);
- "container builds §n": [container builds](2026-09-26-container-builds-design.md) (the Engines
  page, image pulls);
- "candidate aliases §n": [candidate aliases](2026-09-27-candidate-aliases-unified-kv-design.md)
  (§4.7, the outside-VRAM fallback and its per-process attribution);
- "machine check §n": [machine check](2026-11-03-machine-check-design.md).

| § | Content |
|---|---|
| 1 | Why native processes and not Podman |
| 2 | Spike: the three engines' Windows builds on a real card |
| 3 | Spike: what the workspace needs to compile on Windows; 3.1 built (WP1) |
| 4 | Design: the launcher seam, engines as release assets, VRAM, the platform layer |
| 5 | CI |
| 6 | Work packages |
| 7 | Decisions, open questions, not yet |

## 1. Why native processes and not Podman

Podman on Windows is a client plus `podman machine`, a Linux VM on WSL2 or Hyper-V. It runs
Linux containers only. Windows containers need Docker or containerd on the Host Compute
Service, and a GPU reaches them only as DirectX, with no CUDA or Vulkan compute, so they are
no route for the engines.

Running the Linux engine containers inside WSL2 was weighed and is not proposed:

- **Model files.** GGUFs on NTFS reach the WSL VM over 9p; mmap-loading tens of GiB that way
  is slow. Fast loads need the models inside the WSL disk image, where Windows tools and the
  user do not see them and they take the space twice.
- **GPU.** CUDA works through `/dev/dxg`, but NVML inside WSL is partial, Vulkan there is
  Mesa's D3D12 layer (dozen) and slow, and AMD support in WSL is thin.
- **Memory.** The WSL VM takes its own slice of host RAM (half by default), which competes
  with expert offload (`n_cpu_moe`) rows that need host RAM.
- **Install.** WSL plus a Podman machine needs administrator rights, virtualization features
  and a reboot, and in a VM nested virtualization. For the newcomer audience that is a
  stopper.
- **macOS.** Podman machines on a Mac have no Metal either, so a native-process runtime is the
  same work for both platforms.

All three engines publish native Windows builds (§2), so lmgw runs natively on Windows and
starts each engine as a child process.

## 2. Spike: the engines' Windows builds on a real card

Measured 2026-11-10 on a Windows 11 test VM (build 26100) with a passed-through
**GeForce GTX 970** test card (Maxwell, compute capability 5.2, 4 GiB, driver 560.81), from an SSH
session (session 0). Builds: llama.cpp `b12236`, stable-diffusion.cpp `master-1079-a3c49b1`,
audio.cpp `v0.9.4`, each the upstream release zip plus its separate cudart zip for CUDA.
Models: Qwen3.5-0.8B UD-Q4_K_XL (chat), Z-Image-Turbo Q4_K with Qwen3-4B-Instruct Q4_K_M and
its VAE (image, 512×512, 4 steps, `--offload-to-cpu`), Supertonic 3 and Parakeet TDT 0.6B v3
Q8_0 (audio: speech, then that speech transcribed).

Reproduce with `scripts/windows-engine-probe.ps1` (its header has the usage); it pins the same
release assets.

| Engine | Build | Result | Ready | Request | Card memory |
|---|---|---|---|---|---|
| llama-server | CUDA 12.4 | works, 57 tok/s decode | 164 s on the first start, 3 to 21 s after | 0.5 s | 796 MiB |
| llama-server | Vulkan | works, 10 tok/s decode (cold) | 7 to 35 s | 0.5 s | 711 MiB |
| sd-server | CUDA 12 | **fails**: "no kernel image is available for execution on the device" | — | — | — |
| sd-server | Vulkan | works, correct image | 3 to 39 s | 83 to 94 s | weights offloaded |
| audiocpp_server | CUDA 12.4 | **fails**: same "no kernel image" at the first compute | — | — | — |
| audiocpp_server | Vulkan | works; the transcript matched the spoken text exactly | 2 s | 9 to 12 s cold, 1 s warm | 2106 MiB |

What this shows:

1. **Every engine runs natively on Windows**, on both CUDA and Vulkan, from a plain process
   start with the model path as an argument. No container, WSL or install step was needed.
   CUDA and Vulkan both work from session 0, so a GitLab runner service can run live tests.
2. **A CUDA build's architecture list differs per engine.** llama.cpp's CUDA 12.4 build covers
   Maxwell (through PTX it JIT-compiles); sd.cpp's and audio.cpp's CUDA 12 builds do not, and
   fail at the first kernel launch. CUDA 13 drops Maxwell, Pascal and Volta altogether, and
   this driver is too old for CUDA 13 anyway. So "NVIDIA card → CUDA build" is not enough;
   the variant choice needs the build's architecture coverage (§4.2).
3. **The first CUDA start on an older card JIT-compiles** for minutes (164 s here, 49 MiB
   written to `%APPDATA%\NVIDIA\ComputeCache`); later starts read the cache. lmgw's start
   timeout and its "starting" state must allow for that on the first start.
4. **Kokoro needs eSpeak NG.** audio.cpp answered a Kokoro request with "Could not load
   eSpeak-ng; install the shared library or provide its path". The Linux image carries the
   library; on Windows lmgw has to ship or fetch `espeak-ng.dll` for the phonemizing families.
5. **NVML gives no per-process memory on Windows.** `nvidia-smi --query-compute-apps` lists the
   engine's PID with `used_gpu_memory` `[N/A]` (the WDDM driver model), so the per-process
   attribution the outside-VRAM fallback relies on (candidate aliases §4.7,
   `vram/attribution.rs`) has no source there (§4.3). The card's total use is reported.
   Windows' own per-process GPU counters (`\GPU Process Memory(*)\Dedicated Usage`) could not
   be read in this spike: from the SSH session even `\Processor(_Total)` was not found, which is
   the network logon, not the GPU. That read is still to be made from a desktop session.

## 3. Spike: what the workspace needs to compile on Windows

`cargo check --workspace --all-targets` on the same VM (Rust stable MSVC, Visual Studio 18
build tools, NASM installed), at main `3b6f3202`, cold: about 5 minutes per pass. All
third-party dependencies compiled, including `secret-service`, `nvml-wrapper`, Tauri and the
Leptos crates. The errors are all lmgw's own Unix-only code. Each pass stops at a crate whose
dependencies failed, so the spike stubbed `lmgw-machine` and `lmgw-update` (on the VM only) to
reach `lmgw-core`; the counts below are resolve-phase errors, and type checking will add more.

| Crate | Errors | What it uses |
|---|---|---|
| `lmgw-machine` | 21 | `std::os::unix` metadata (mode, uid, inode), `getpwuid_r`, `statvfs`, process groups and `kill` |
| `lmgw-update` | 10 (+4 in tests) | a 0700 private directory checked by uid and mode, `O_NOFOLLOW`; the RPM installer as a whole |
| `lmgw-core` lib | 71 | see below |
| `lmgw-core` tests | 113 | the same plus Unix test helpers |
| `lmgw-machine-helper` | 55 | the pkexec root helper; Linux-only by nature |
| `folder-chat` | 58 | an agent that runs in a Linux container; Linux-only by nature |
| `lmgw-server` | not reached | the headless server image; Linux-only by nature |
| `src-tauri` | not reached | |

`lmgw-core`'s 71 sit in about 20 files: the CDI spec writer (`runtime/cdi/generate.rs`, 23),
agent containers (11), TCP keepalive and user-timeout options plus raw fds in `server/conn.rs`
and `server/main_tls.rs` (17), private-file checks in `devices/tls/files.rs` (10),
`pre_exec` with `close_range` and `setrlimit` in `child.rs` and `extract/pdf.rs` (13), the
pull service's Unix socket (4), Podman run helpers (6), and single uses of
`std::os::unix` in the audio file view, the machine probe, `state.rs` and the dev models dir.

Most of it is `cfg(unix)` gating plus a Windows branch of a few lines; the real work is the
launcher (§4.1), not the compile gap.

### 3.1 Built: the workspace compiles on Windows (WP1)

Since 2026-11-10 `cargo clippy --workspace --all-targets -- -D warnings` passes on
x86_64-pc-windows-msvc (Rust 1.99.1, the spike's VM), and the Linux gate is unchanged.
`scripts/windows-remote-check.sh` reproduces it from a Linux checkout: with `LMGW_WIN_HOST` set to
the Windows machine's ssh destination it mirrors the working tree there (uncommitted edits
included) and runs the check, or the command it is given; the `windows-check` CI job (§5) runs the
same command on the Windows runner.

The rules the branches follow: `cfg(unix)` for what is POSIX, `cfg(target_os = "linux")` for
what is Linux (`close_range`, `TCP_USER_TIMEOUT`, `/proc`), and a Windows branch only where std
has the answer in a few lines. Where the right Windows behaviour needs design, the Windows branch
is an error that says "… is not supported on Windows yet", never a skipped check or a made-up
value. WP6 (§4.4) replaced the rows of the platform layer (`lmgw-machine`'s file facts and free
space, `lmgw-update`'s private download dir and file, the device CA's files, lmgw's CDI spec dir,
a stream's keep-alive, the main TLS key's readers); what is left:

| Where | Not on Windows yet |
|---|---|
| `extract/pdf.rs` | poppler's address-space limit, so PDF extraction fails with that reason. poppler-utils is not shipped for Windows yet; once it is, a job memory limit (the native launcher's, §4.4) is the counterpart |
| `backends/pull/service.rs` | Podman's API socket (a pull takes the `podman` command); out of this version (§4.5) |
| `backends/run/mod.rs` | engine builds from git (`start_run` refuses); out of this version (§4.5) |
| `backends/run/podman.rs` `real_uid` | no uid: it is `None`. The build lock path (`lock_path`, so the boot sweep skips with a logged note and a run stops) and buildah's cache root (`prepare_scratch`) are errors; a build delete still removes its row and dir and skips the buildah cache dirs with a logged note (§4.5) |
| `agents/container.rs` | an agent's private run dir; SIGTERM is not delivered, a kill is `TerminateProcess` (§4.5) |

What does have a Windows branch: the user name from `USERNAME`, a limited run stopped with
`Child::kill` (the program, not what it started: no job object yet), the TLS pair's change stamp
from the modification and creation times, Ctrl-C as the quit signal, a child's handles left to
Windows' default (only inheritable handles pass, and Rust opens its own non-inheritable).
`lmgw-machine-helper`, `lmgw-server` and `folder-chat` build to an empty library and a `main`
that says it runs on Linux only. Unix-only tests are gated at module level, or per test where a
module is mostly portable. Since WP3 the native launcher's tests run on Windows, and since WP6
the platform layer's (§4.4), the killed-lmgw test among them (there it asserts the kill-on-close
job alone).

## 4. Design

### 4.1 The launcher seam

**Built (WP2, 2026-11-10).** The registry (per-model containers §3.2) reaches a model's process
only through one trait, `runtime::launcher::Launcher`; the Podman launcher behind it is the code
the registry used to run itself, moved unchanged (the same argv through the same
`CommandRunner`, in the same order, with the same sentences). A registry is built with the
Podman launcher (`Registry::new` / `with_ports`) or with any other (`Registry::with_launcher`).

A model's process is **named, not held**: every verb takes the instance name the registry
renders (`runtime::container_name`), which its entries already record. A launch under a name
that is taken replaces what runs under it. Readiness stays the registry's: it polls the
server's own HTTP route (`/health`, `/v1/models`, sd-server's capabilities), whatever started
it, and only asks the launcher whether the process is still alive.

| Method | Called by | Podman launcher | Native launcher (WP3) |
|---|---|---|---|
| Method | Called by | Podman launcher | Native launcher |
|---|---|---|---|
| `runs_containers()` | the legacy sweep, the sd-server vocabulary probe | true | false |
| `prepare(Prepare)` → `Prepared` | a start, once, before a port is taken | lmgw's CDI spec made fresh, the `--cdi-spec-dir` global options | the engine's executable found in its install, the GPU environment from the run args and placement |
| `launch(Launch)` → `Launched` / `LaunchFailure` | each start attempt (cold start, climb rung) | `podman run -d --replace …` (`never_pull`, `stamp_owner`); the D27 restart note as a warning; `port_in_use` from stderr | kill any child under the name first, spawn the executable from the spawner thread (Linux) and into the job object (Windows), stdout and stderr into the log ring |
| `exited(name)` → `Exited` | the readiness poll, every 2 s | `podman inspect` State; never `port_in_use` | the child's exit status; `port_in_use` when its log says it could not bind or the port is held by something else after the exit |
| `stop(name, unload_timeout)` | `stop`, eviction, the reaper, a climb's old rung, the pass after boot | `podman stop -t 10`, then `podman wait` | Linux: `SIGTERM` to its process group, `SIGKILL` after 10 s; Windows: `TerminateProcess`; then the wait for the exit, within 10 s plus the unload timeout |
| `stop_quietly(name)` | a start whose readiness failed | `podman stop -t 10` | the same stop, without waiting |
| `remove(name)` | delete/disable hygiene, a port-conflict husk, an aborted start or climb, reconcile's refusals | `podman rm -f` | kill, wait for the exit (10 s), drop the child and its log ring |
| `logs_tail(name, n)` / `log_excerpt(name)` | `container logs`, the local model test, a failed start's error | `podman logs --tail` | the log ring |
| `exists(name)` | the legacy sweep (Podman only since WP3) | `podman inspect` | the child table |
| `list(prefix, All/Running)` | boot reconciliation (`All`), the pass after boot (`Running`) | `podman ps [-a]` by `lmgw.instance` | empty: nothing outlives lmgw, so nothing is adopted |
| `inspect(name)` → `Found` | adoption, `container stop` of an unheld model | `podman inspect`: state, labels, `Config.Cmd`, the `8080/tcp` host port | refuses (nothing is listed, so adoption never asks) |
| `same_start(prefix, render, found, labels)` | adoption, per candidate rung | `Config.Cmd` against the engine argv, the `lmgw.run_args` digest | refuses |
| `host_pids(names)` | VRAM attribution (`vram::ledger`, candidate aliases §4.7) | one `podman inspect --format '{{.Name}} {{.State.Pid}}'` | the running children's PIDs |

`Prepare` carries the model's class (which engine), its resolved run args, its `Placement` and
the start's budget. `Launch` carries the model's `RenderSpec` at the attempt's port, the start's
`Prepared`, the owner stamp and the placement. The registry keeps the decisions: the port and
its one retry (§10.4), the image row's render check, the `/health` poll, the audio `server.json`
comparison and the readiness probe during adoption.

**The seam changes WP3 made** (Podman's behaviour unchanged):

- **The port retry covers an exit.** `exited` answers a typed `Exited { message, port_in_use }`.
  A native engine binds its port itself and, when the port was taken between the probe and the
  bind, exits; the readiness poll then ends with `port_in_use`, and the registry gives the start
  the same one retry on a fresh port it gives a `LaunchFailure::port_in_use`, after `remove`.
  Podman's exits never say `port_in_use`: Podman binds the published port at `podman run`.
- **`prepare` knows the engine.** It takes `Prepare` (class, run args, placement, budget)
  instead of the prefix and run args alone, and `Prepared` carries the native launcher's
  resolved install and environment beside Podman's global options.
- **The card a native engine sees** is read from the run args, the one place a model's card is
  chosen today, in one function (`launcher::native::gpu`): the row on the CPU, or run args that
  would give a container no GPU, hide every backend (`CUDA_VISIBLE_DEVICES=-1`,
  `HIP_VISIBLE_DEVICES=-1`, an empty `GGML_VK_VISIBLE_DEVICES`, which ggml's Vulkan backend reads
  as "no device", per `ggml-vulkan.cpp` at b12236; checked on Windows, see below); `--device nvidia.com/gpu=<n>` (or `--gpus
  device=<n>`) becomes `CUDA_VISIBLE_DEVICES=<n>` with `CUDA_DEVICE_ORDER=PCI_BUS_ID`, so CUDA
  counts cards as CDI and `nvidia-smi` do; `nvidia.com/gpu=all`, `--gpus all`, `/dev/dri` and
  `/dev/kfd` set nothing. Of `--device`, only a CDI name or a GPU node counts; another device
  (`/dev/snd`) selects nothing and is named as not applied. `-e NAME=VALUE` in the run args is the
  process's environment, after those, so a row's own value wins, except when no card is passed:
  there the hiding variables win over the row's own `CUDA_VISIBLE_DEVICES`,
  `HIP_VISIBLE_DEVICES` and `GGML_VK_VISIBLE_DEVICES` (a container without a device could not
  have reached a card; a CPU row counts as no VRAM and runs under the GPU hold), and the start's
  log names what was left out. The other run args mean something to a container only; each
  start's log line names them. A row placed on the GPU whose run args pass no card runs on the
  CPU while lmgw counts it as a GPU model: the start returns a warning saying so (it reaches the
  entry's warnings like Podman's restart-policy note).
  **Checked 2026-11-10 on the Windows VM** (GTX 970, llama.cpp build 12236 Vulkan,
  `llama-server.exe --list-devices`, the variable set through `ProcessStartInfo`, which like
  Rust's `Command::env` puts `NAME=` with an empty value in the child's block): unset lists
  `Vulkan0: NVIDIA GeForce GTX 970`; set empty lists `(none)`; set to `99` lists `(none)` and
  prints "Invalid device index". So the empty value hides the cards on Windows too.
- **The legacy sweep runs on Podman only** (`runs_containers`). On another launcher `exists`
  answers "absent" for a container it cannot see, and the sweep used to retire the router-mode
  names on that answer; now it skips with a log line and keeps them.
- **audio.cpp's `server.json` renders in a view** (`runtime::audio::render_single_model_config_in`):
  model paths, listen address and port are the view's. `EngineArgs::Audio` carries the row and
  its engine settings for that. The container's file is written as before; the native one goes
  to `native/server.json` beside it, so the stale check (`runtime::audio::stale`) still compares
  the row with the container's render. In the host view the argv is `--config <file>`, without
  the image entrypoint's `server` subcommand (`EngineView::in_container`).
- **Podman's mounts are the container view's.** `podman_run_argv` takes `/models`, `/config`
  and the container port from `EngineView::container()`, and a test ties them together. The engine side of `same_start` and the
descendant walk under each PID (`vram::attribution`'s `/proc` reader) stay shared; on the
native launcher the walk finds the engine's own children, if it has any.

**The engine argv is shared.** `runtime::argv` renders it once (`engine_argv`) against an
`EngineView`: where the engine sees the models directory and its config directory and which
address it listens on. The container view is `/models`, `/config` and `0.0.0.0:8080`; a native
launcher passes the host directories and `127.0.0.1:<port>`. The `podman run` wrapper around it
(`argv/podman.rs`: `podman_run_argv`, the labels, `--init`, `-p`, the mounts) is the Podman
launcher's alone.

**The native launcher (built, WP3, 2026-11-10)**, `runtime::launcher::native`:

- **Wiring.** On Windows the registry is built with it (`state.rs`). On Linux Podman stays the
  launcher, and the native one is built only through `Registry::with_launcher` (the tests);
  there is no setting that selects it yet.
- **Exit and port in use.** An exit says "port in use" when the log holds an engine's bind text
  (`couldn't bind`, `address already in use`, `only one usage of each socket address`,
  `WSAEADDRINUSE`, `error 10048`) or the port is held by something else right after the exit.
  The bare number 10048 is no pattern: llama.cpp prints sizes such as `buffer size = 10048.55
  MiB`.
- **Engine location, until WP4.** `launcher::native::engines` finds the executable at
  `<data>/engines/<engine>/<build>/<executable>` (a build directory may be a symlink):
  `<engine>` is the class's (`llama` for chat
  and aux, `sdcpp`, `audio`), the executables `llama-server`, `sd-server`, `audiocpp_server`
  (`.exe` on Windows). Exactly one build directory per engine is used; none, or several, is the
  start's error, naming the path it looked for or the builds it found. WP4's release assets
  replace this one module.
- **What runs.** The executable with `engine_argv(render, EngineView::host(…))` (the host's
  models directory, `127.0.0.1:<port>`), its build directory as the working directory, the GPU
  environment above, no `LD_LIBRARY_PATH` (llama.cpp's Linux build finds its libraries through
  `RUNPATH $ORIGIN`; a row's own `-e LD_LIBRARY_PATH=…` still wins), no inherited `LLAMA_ARG_*`
  (llama-server reads one for every flag its argv does not set, so a stray
  `LLAMA_ARG_N_PARALLEL` in lmgw's environment would change a start unseen; the start's log names
  the ones removed, and a row's own `-e LLAMA_ARG_…` passes), stdin closed, and on Linux no descriptor beyond stdio (`child::stdio_only`).
  Windows: `CREATE_NO_WINDOW`. A child is kept under the instance name the registry renders.
- **Lifetime: nothing outlives lmgw**, so `list` is empty and nothing is adopted; a restart of
  lmgw is a cold start of every model. Windows: every child is assigned to one Job Object with
  `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, whose (non-inheritable) handle the launcher owns; closing
  it, at the launcher's drop or lmgw's exit or crash, kills the children. Since WP6 a child is
  created suspended and runs only once it is in the job (§4.4). Linux: each child has its own
  process group and `PR_SET_PDEATHSIG(SIGKILL)`, checked against a parent that died before the
  call. The kernel
  sends that signal when the *thread* that forked the child ends, not the process, so every
  child is forked from one thread the launcher owns (`native::spawner`), which ends only with
  the launcher. Its own drop kills its children too (each child's supervisor kills its process
  when its entry goes).
- **Stop.** Linux: `SIGTERM` to the child's process group, `SIGKILL` after the registry's stop
  grace (`STOP_GRACE_SECONDS`, 10 s), then the wait for the exit within the grace plus
  `vram.unload_timeout_seconds`; after the exit the group gets a `SIGKILL` for anything the
  engine left. Windows: `TerminateProcess` at once. A console break is no graceful route there:
  `GenerateConsoleCtrlEvent` reaches only processes on the caller's console, which a GUI lmgw
  does not have, and llama-server's console handler acts on `CTRL_C_EVENT` alone, which cannot
  be sent to a process group. The engines hold nothing a stop would have to save. Each exit is
  a log line (info after a stop, warn when the engine ended on its own). The exit is announced
  after the stdout and stderr readers have reached their end, for at most `READER_DRAIN` = 2 s,
  so an exit check right after it sees the last error lines (the bind failure, the start's
  excerpt); a pipe a grandchild still holds open after that does not hold the exit up (a warning
  says the last lines may be missing).
- **Logs.** stdout and stderr go into one in-memory ring per child, in arrival order:
  `LOG_RING_LINES` = 4000 lines, a longer run without a line break split at `LOG_LINE_BYTES` =
  8 KiB (split, not cut). The first line a ring drops is said in lmgw's log. The ring is kept
  after the exit, until the next start under the name or a remove. Nothing is written to disk
  yet.
- **PIDs.** `host_pids` is each running child's PID; attribution walks its descendants as
  before.
- **The start timeout.** Readiness is bounded by `vram.load_timeout_seconds` (600 s by default,
  a setting the timeout error names) and nothing else: the native launcher adds no bound of its
  own to a start. The first CUDA start on an older card spends its JIT inside that budget (164 s
  on the GTX 970, §2 point 3), and the start's log line says which budget it has. A slower first
  start needs that setting raised; lmgw does not know a first start from a later one yet.
- **The engine vocabulary probes** stay off the seam: an image row started through the native
  launcher is spelled against the sd-server vocabulary lmgw ships with, and the entry's warning
  says why ("sd-server's flag vocabulary is read from its container image, and this lmgw starts
  sd-server as a native process, which has no image to read it from …"). `Registry::help_text`
  (the llama flag check in `ops`) still asks Podman.

**Tests and live checks (2026-11-10).** `tests/it/runtime_launcher_native.rs` drives the
registry through the real native launcher against `lmgw-fake-engine` (`crates/lmgw-core/fake-engine`,
std only, installed as `<engines>/llama/fake-b1/llama-server`): start to ready, the host view in
its argv, the environment and working directory, the log tail, the PID, the stop and the remove,
an exit while loading, the port retry against a held port, a dropped launcher killing its child,
and a killed lmgw (the test binary re-run as the parent, then `SIGKILL`ed) killing its child
through the death signal. `runtime_launcher.rs` covers the seam with a fake launcher, adoption
from a non-empty list included.

Live checks, 2026-11-10, CPU only (`-ngl 0`, `--ctx-size 4096`), Qwen3.5-0.8B UD-Q4_K_XL, one
chat request ("Name three colors.", 32 tokens) through `native_live_llama_server_on_the_cpu_answers_a_chat`:

| Machine | Engine build | Ready | Request | Stop |
|---|---|---|---|---|
| Linux (Fedora) | llama.cpp b12236 `llama-b12236-bin-ubuntu-x64.tar.gz` (SHA-256 `5b157242dead6c8a6cc556d25a4be545eaaa0f0cf8ab550dd857d990a524af8c`), unpacked into `<engines>/llama/` | 0.76 s | 0.48 s | 0.07 s |
| The Windows 11 VM (§2), from the SSH session | llama.cpp b12236 `llama-b12236-bin-win-cpu-x64.zip` (SHA-256 `5a2b6cfc686c70184e7f980a6f1d13d23f9d889d87d5300327994c19bee9c0ae`), unpacked into `<engines>\llama\b12236-cpu\` | 3.41 s | 1.14 s | 0.12 s |

Both answered with three colours, and no `llama-server` process was left afterwards. On Windows
the fake-engine tests passed as well (start to ready, exit while loading, the port retry), each
child going through the job object; the two lifetime tests are Unix-only so far (they read
process state through `kill(pid, 0)`), so the job's kill-on-close is exercised by every Windows
start but not yet asserted by a test. Reproduce: set `LMGW_LIVE_NATIVE_ENGINES` to the engines
dir and `LMGW_LIVE_NATIVE_GGUF` to the model, then `cargo nextest run --workspace -E
'test(/runtime_launcher_native/)' --no-capture` on Linux; on Windows, through
`scripts/windows-remote-check.sh`, `cargo test --workspace --features lmgw-core/opus-bundled
--test it -- runtime_launcher_native --nocapture` (the `it` binary links libopus, which the
Windows machine has no `opus.lib` for; the bundled build makes it; 13 minutes cold on the VM).
Windows' `cargo clippy --workspace --all-targets -- -D warnings` passes with WP3 (8.5 minutes).

**Not behind the launcher yet** (on Podman directly, or Linux-shaped):

- the engine vocabulary probes: `llama-server --help` and `sd-server --help` from a throwaway
  container, and `podman image inspect` for the image ID they are cached under
  (`Registry::help_text`, `sdcpp_caps`, `run_throwaway`, `image_facts`). On another launcher an
  image row's start falls back to the vocabulary lmgw ships with and says so, as it does today
  when the probe fails; WP4's engine pins are where a native vocabulary read belongs;
- the agents' containers (`agents::container`), the benchmark's container (`bench::launcher`),
  engine builds and pulls (`backends`), MCP stdio containers (`mcp::connect`), and the `ps` and
  `rm -f` the registry lends them (`Registry::ps_filtered`, `Registry::rm_force`);
- the llama flag check (`Registry::help_text`, `ops::local_model_checks`), which runs
  `llama-server --help` in a throwaway container.

### 4.2 Engines as release assets

On Linux an engine is an image (container builds). On Windows it is a pinned upstream release
asset:

- one entry per engine build: release tag, asset name, the asset's SHA-256 (the GitHub API
  reports it as `digest`), and its cudart companion for CUDA builds;
- unpacked under `%LOCALAPPDATA%\lmgw\engines\<engine>\<tag>-<variant>\`, verified before
  first use, never run from the download directory;
- the variant is chosen per machine and **per engine**: CUDA 13 on a driver that supports it
  and a card of compute capability 7.5 or newer; the CUDA 12 build where its architecture list
  covers the card; otherwise Vulkan; CPU when no GPU is usable. The coverage of each pinned
  CUDA build is a fact recorded with the pin (read once with `cuobjdump --list-elf
  --list-ptx` on its `ggml-cuda.dll`), not guessed. A start that still ends in "no kernel image
  is available" falls back to that engine's Vulkan build and says so.
- the Engines page shows these as the Windows column of an engine, with "update available"
  from the same release check it does for images.

**Built (WP4, 2026-11-10)**, `runtime::engine_assets`:

- **The pin table** is `engine_assets/pins.json`, compiled in and checked by a test (64-digit
  digests, a CPU build per engine, cudart and coverage on every CUDA build). Per platform
  (`windows-x86_64` only so far) and engine: repository, tag, publication day, and per variant
  the asset (name, SHA-256, size), the cudart asset and the coverage: every file with device
  code, its SASS and PTX architectures and the PTX's ISA version. The versions follow what the
  Linux side runs, the newest upstream build (its images track `server-cuda`, `master-cuda` and
  the latest audio.cpp release): on 2026-11-10 llama.cpp `b12244`, stable-diffusion.cpp
  `master-1079-a3c49b1` and audio.cpp `v0.9.4` (the spike's sd.cpp and audio.cpp pins were the
  newest already), and eSpeak NG `1.52.0`.

  | Engine | Variant | Toolkit | SASS | PTX |
  |---|---|---|---|---|
  | llama.cpp b12244 | cuda13 | 13.5 | 86, 89, 120a, 121a | 75, 80, 90 |
  | llama.cpp b12244 | cuda12 | 12.4 | 86, 89 | 50, 61, 70, 75, 80, 90 |
  | sd.cpp master-1079 | cuda12 | 12.8 (PTX ISA 8.7) | 61 to 120a (`ggml-cuda.dll`), 61 to 120 (`stable-diffusion.dll`) | the same |
  | audio.cpp v0.9.4 | cuda13 | 13.3 | 75, 80, 86, 89, 90, 120a | the same |
  | audio.cpp v0.9.4 | cuda12 | 12.4 | 70, 75, 80, 86, 89, 90 | 61, 70, 75, 80, 86, 89, 90 |

  sd.cpp publishes no CUDA 13 build. The coverage was read 2026-11-10 with
  `scripts/engine-cuda-coverage.sh <new dir>` (cuobjdump of CUDA 13.0 in
  `nvidia/cuda:13.0.0-devel-ubuntu24.04`; cuobjdump on Linux reads no PE file, so the script
  takes each DLL's and EXE's `.nv_fatb` section out first); `--check` reads it again and compares
  with the table. It matches the spike: llama.cpp's CUDA 12 build reaches the GTX 970 (5.2) only
  through compute_50 PTX, and the other two have nothing below 6.1. eSpeak NG's MSI has no GitHub
  digest (uploaded before GitHub kept them); its pinned SHA-256 is the download's, the same on
  the desktop and the VM.
- **The choice** (`choice`, `coverage`), per engine, from NVML's cards and driver CUDA version,
  read in process (`facts`), and the Vulkan loader (`vulkan-1.dll` in the system directory):
  CUDA 13 when the driver's CUDA major is 13 or newer, every card is 7.5 or newer and the
  coverage reaches each; CUDA 12 when the coverage reaches each card; Vulkan with a loader; CPU.
  "Reaches": SASS `sm_XY` runs on X.Z with Z ≥ Y, an arch-specific `sm_XYa` on X.Y alone; PTX
  `compute_XY` on X.Y or newer, when the driver's CUDA version is at least the build's toolkit
  (it has to know the PTX's ISA); every file with device code has to reach the card. A PTX-only
  reach is a JIT on the first start, which the start's log line says. It still ranks above Vulkan:
  the owner's ruling of 2026-11-11 that CUDA ranks above Vulkan, after WP8's live round measured
  Vulkan faster on a GTX 970 (§7). The choice's sentence names
  each card, how it is reached and why the builds before it were passed over; it is in the
  start's log line and on the Engines page.
- **Download, verify, unpack** (`install`): the assets go to
  `<engines>/.downloads/<engine>/<tag>-<variant>/<name>.part`, hashed while they arrive; size and
  SHA-256 are compared with the pin before the rename, and a mismatch removes the part and is the
  start's error ("… and the pin is … — the download was removed and nothing of it is used"). An
  asset already in that directory from an install that did not finish is hashed again and used
  when it still matches, so a failed install keeps its verified downloads. The verified zips are
  unpacked (the `zip` crate) into `.staging-<tag>-<variant>` beside the build, which must hold the
  executable at its top level and gets the install record (`.lmgw-install.json`: repository, tag,
  variant, assets with digests); then it is renamed to `<tag>-<variant>` and the download
  directory is removed. A build directory is either a whole verified install or absent. One whose
  record names the same repository, tag and variant with other assets (the release was re-pinned
  under its tag) is renamed aside to `.<tag>-<variant>.replaced-<UTC time>` and installed anew,
  nothing deleted; one that exists without such a record is refused with its path, never replaced.
  One install at a time per build, within the process (a mutex) and across every lmgw process
  sharing the engines root (production beside a dev instance): a file lock
  `<engines>/<engine>/.<tag>-<variant>.lock`. A wait for another process's install has no time
  bound and is on the starting entry's warnings ("waiting for another lmgw process to finish
  installing llama.cpp b12244 cuda12 (it holds …)"), where the dashboard shows it, and in the log.
  On Windows a virus scanner opens a new file for a moment, and a rename or removal then fails
  with "access denied" (os error 5) or a sharing violation (32): those are tried 8 times with
  pauses of 100 ms doubling to 2 s (7.1 s in all, `install::BUSY_ATTEMPTS`, `busy_pause`), and the
  error after the last names the attempts and the time. A genuine access denied (no permission)
  cannot be told apart, so it is tried as often before it is the error. A connection to the
  download server that is not made within 30 s (`install::CONNECT_TIMEOUT`) fails with that figure
  in its error; a download that delivers no bytes for 60 s (`install::STALL`) fails and says that
  figure; one that keeps moving has no bound. A model's first start installs its engine's build,
  inside `prepare`, before the port is taken; the load timeout does not cover the download. An
  install runs for one start (the launcher seam's `StartWatch`, which `Prepare` carries): when a
  stop, the GPU hold's sweep or an eviction takes that start's entry, the install's waits and
  downloads stop within 250 ms (`install::WATCH_POLL`), a download's `.part` stays for the next
  download to write over, and the start ends `Aborted`; the unpack and the renames are not cut
  short. The registry's start loop asks before every launch whether the entry is still the
  start's, and again right after the launch (a stop that took the entry before the process was
  there found nothing to stop): an entry taken away gets nothing launched, or what was launched
  removed, so nothing loads on the card outside the ledger (the hold's "no VRAM", the Podman
  launcher's starts included).
- **The engines root** has one accessor, `engine_assets::engines_root(data_dir)`:
  `<local dir>/engines`, through WP6's `state::local_data_dir` (§4.4), so the installed app's
  engines are under `%LOCALAPPDATA%\lmgw` on Windows and every other data dir keeps them in itself.
- **The Vulkan fallback** (`fallback`, the native launcher's `exit`, the registry's start loop): a
  pinned CUDA build's process that ended on its own (lmgw did not ask it to stop: not the idle
  reaper, a user's stop or a replacing start), not with code 0, with ggml's CUDA error line in its
  log ring writes `.lmgw-no-kernel-image` into that build's directory: JSON with the line, how the
  process ended (`exited with code 1`) and the time. The line is `ggml_cuda_error`'s `CUDA error:
  no kernel image is available …` at the start of a line, after only the prefixes the engines'
  loggers put there (read from their sources): none for ggml's own logger (audio.cpp, seen on the
  VM), llama.cpp's `common_log` timestamp `M.ss.mmm.uuu ` and level `E ` (with `--log-prefix` /
  `--log-timestamps`, b12244), stable-diffusion.cpp's `[E] ggml: ` (master-1079-a3c49b1), and
  terminal colour codes around them; the words after anything else (`prompt: CUDA error: …`,
  another function's name) do not count. The whole ring (4000 lines) is read, not the excerpt's
  last 60. While loading, the exit tells the registry so (`Exited::fallback`, beside
  `port_in_use`): the registry's start loop prepares the start again, where the choice passes over
  the marked release's CUDA builds and the Vulkan build is installed outside the load timeout, and
  launches it on a fresh port with a fresh load budget and its own port retry, once. The CUDA
  attempt's log stays readable (`logs_tail`, the failure's excerpt) until the Vulkan launch
  replaces it. The sentence (both builds, the line, the exit) is in lmgw's log and on the entry's
  warnings, and a start that still fails after it, at any step (a port, the render, the launch,
  the load), says "after the fallback to the engine's next build: …". A CUDA build that failed
  that way after it was ready is marked at the next start of its engine. Each child is judged once
  and its verdict kept with it: a cleared mark is not written again from the same log, and when
  another start's `prepare` judges a loading child first, that child's own start still reads the
  verdict and falls back. The choice passes over every CUDA build of a release with a mark, so a
  new pin is tried on its own merits; the owner clears a mark with `engine_fallback_clear` (**Try
  CUDA again** on the Windows tab), after which the next start chooses from the machine's facts
  again.
- **eSpeak NG** (`msi`): the MSI is unpacked by lmgw itself (the `msi` and `cab` crates read its
  Directory, Component, File and Media tables and its embedded MSZIP cabinet), because `msiexec
  /a` answers 1601 from a non-administrator's SSH logon. It is installed at an audio start under
  `<engines>/espeak/1.52.0/`, for every audio row, since lmgw has no per-family "phonemizes" fact
  yet. audio.cpp's dynamic build loads `libespeak-ng.dll` by name and eSpeak reads its data from
  `ESPEAK_DATA_PATH`, so the audio child gets that directory first on its `PATH` and as
  `ESPEAK_DATA_PATH` (a row's own `-e` wins). An install that fails is a warning on the start
  (lmgw's log and the entry) and the process starts without it: a phonemizing voice then fails at
  its first request, the other families are not affected. The failure is remembered for 10 min
  (`engine_assets::ESPEAK_RETRY_AFTER`): the audio starts in that time do not try again (with no
  network a try can cost the 30 s connect timeout) and their warning says so; the first audio
  start after it, or after lmgw restarts, tries again. The warning states the interval.
- **A card selection and the Vulkan build.** `--device nvidia.com/gpu=<n>` becomes
  `CUDA_VISIBLE_DEVICES` (§4.1); ggml's Vulkan device order is not mapped to NVIDIA's yet, so a
  row that names cards and runs a pinned Vulkan build (the default below Pascal, and the fallback)
  gets a start warning: "the card selection does not reach the Vulkan build yet: the native …
  process sees every card".
- **audio.cpp's backend.** The native `server.json` takes the backend of the build that runs:
  a GPU row on the Vulkan build says `vulkan` whatever the class says (`cuda` by default), a row
  on the CPU stays `cpu`.
- **The release check** (`releases`) runs in the registry images' check (`check_all`: the
  schedule and **Check now**) where the platform has pins, one `GET /repos/<repo>/releases` per
  project. The newest non-draft release with a Windows asset is compared (llama.cpp's builds are
  pre-releases, its `v0.9.0` releases carry no binaries); an update is another tag published on
  the pin's day or later. The observations are stored with the images' (`engines` in
  `backends:update_check`) and counted in the nav badge (`engines_with_updates`).
- **The Engines page's Windows tab** (`engine_releases`, `pages/backends/releases.rs`), shown
  when the answer says the engines run natively: per engine the pinned release, the build this
  machine runs (its sentence in the tooltip, a note when its first start compiles kernels), each
  build's state (installed, not downloaded with its size, or the no-kernel-image line), the
  coverage in the tooltip, and the upstream chip; a marked build shows the engine's line and how
  its process ended (the time in the tooltip) and a **Try CUDA again** button
  (`engine_fallback_clear`); eSpeak NG as a row of its own; the machine's cards, driver CUDA
  version and Vulkan loader above. A hand-typed `/backends/windows` where the engines run as
  containers shows a note saying so instead of the table. Verified from Linux: the op's answer
  (over HTTP), the tab's chip, state and mark wording (unit tests), the nav badge count and its
  tooltip; the page compiles into the UI bundle. Not verified: the tab as rendered, in WebView2
  or anywhere, since on Linux the answer has no engines and the tab stays hidden, and no Windows
  dashboard was opened in this pass. WP8's round saw it rendered in WebView2 and clicked Try CUDA
  again there (§7, WP8 as built).

**Tests and live checks (2026-11-10).** `tests/it/runtime_engine_assets.rs` drives the registry
through the real native launcher against a local release server whose zips hold
`lmgw-fake-engine`: a pinned CUDA 12 build with its cudart is downloaded, verified, unpacked and
started from `<tag>-<variant>`, and a second start downloads nothing; a wrong digest installs
nothing and leaves no part file; a CUDA build whose start prints ggml's "no kernel image" line is
marked (line and exit) and the same start ends ready on the Vulkan build with the fallback in its
warnings, the next start goes there directly, and after the owner clears the mark the next start
tries CUDA again; the fallback's Vulkan download takes 5 s against a 4 s load timeout and the
start still ends ready; a stop lmgw asked for, an exit with code 0 and the words outside ggml's
line mark nothing; a build that fails at its first request after ready is marked at the next
start; an engine removed while loading ends the start at once; a second start whose `prepare`
judges the first start's failed CUDA child before that start's readiness poll does leaves the
first start its fallback; a stop while the fallback's Vulkan build downloads (10 s held back by
the server) ends the start within the download, launches nothing, installs nothing and keeps the
CUDA attempt's log; a held install lock is on the starting entry's warnings; a failed eSpeak NG
install is asked for once and the next try is answered from memory; the Windows tab's answer says
the choice, each build's state and a mark's reason; both ops answer over HTTP. Unit tests cover
the line match (each engine's prefixes, and `prompt: CUDA error: …` and other lines that are not
it), the busy retry's pauses and its error, the re-pin record match, mixed cards (5.2 beside 8.9
on a CUDA 13.5 driver: llama.cpp's CUDA 12 build, reaching the 5.2 card through PTX; sd.cpp and
audio.cpp on Vulkan), a stored cabinet block whose sizes differ, and an MSZIP block whose one
match reaches into the previous block's history. They pass on Linux and on the Windows VM. The
live checks ran on the VM of §2 (GTX 970, driver 560.81, CUDA 12.6) from the SSH session, through
the real pins:

| Check | Result |
|---|---|
| The choice | llama.cpp `cuda12` ("through compute_50 PTX, compiled on the first start"; CUDA 13 passed over for 5.2 < 7.5); sd.cpp and audio.cpp `vulkan` ("no kernels for compute capability 5.2", their lowest is 6.1) |
| llama.cpp b12244 cuda12 download and verify | 276 MiB + 373 MiB in 48 s |
| Qwen3.5-0.8B UD-Q4_K_XL, every layer on the card (`--device nvidia.com/gpu=all`, `-ngl 99`) | ready 3.6 s, then 2.6 s; "Name three colors." 0.5 to 0.6 s at 77.5 tok/s decode, on `CUDA0` |
| Kokoro 82M Q8_0 (`kokoro_tts`, voice `af_heart`) on audio.cpp v0.9.4 vulkan with eSpeak NG 1.52.0 | ready 7.3 s (2.3 s once installed); one sentence, a 170 444-byte WAV, in 9.7 s cold and 0.9 s warm |
| The release check against GitHub (from the desktop, `LMGW_LIVE_RELEASE_CHECK=1`, test `release_check_live`) | every project's newest Windows release was the pin (b12244, master-1079-a3c49b1, v0.9.4, 1.52.0): no update, no error, 2.1 s for the four |
| The fallback: audio.cpp's cuda12 build forced on the card | ready after 106 s (its 1.0 GiB download included); the first request ended the engine with "CUDA error: no kernel image is available for execution on the device"; the next start marked the build and ran Kokoro on the Vulkan build, a 200 in 0.9 s |

The fallback on the card went the after-ready way (audio.cpp loads, then fails at its first
compute); the while-loading way is the fake engine's test.

After the review round (same day, same VM and pins, a new engines dir, so everything was
downloaded again): llama.cpp b12244 cuda12 649 MiB downloaded and verified in 52.8 s, ready in
40.1 s on its first start (the PTX JIT), "Name three colors." in 0.68 s at 70.9 tok/s. audio.cpp's
cuda12 build forced for Kokoro: ready in 102 s (its download included), the first request ended
the process with code -1073740791 (0xC0000409) after "CUDA error: no kernel image is available
for execution on the device"; the exit check marked the build at once with that line and exit,
and the next start installed the Vulkan build in its `prepare` and was ready in 12.5 s, a 200
(170 444-byte WAV) in 0.85 s. That is the after-ready path again: no engine on this card fails
while loading, so the registry's fallback retry ran only in the fake engine's tests (Linux and
the VM). The card selection's Vulkan warning was checked by a test (`--device nvidia.com/gpu=0`
on a Vulkan build); the VM's one card gave no live case where a mapping would differ. Reproduce on Windows, with an engines
dir that is kept between runs (a second run downloads nothing): set `LMGW_LIVE_ENGINE_ASSETS` to
it, `LMGW_LIVE_NATIVE_GGUF` to the chat GGUF, `LMGW_LIVE_KOKORO_DIR` to the directory holding
`Kokoro-82M-GGUF`, optionally `LMGW_LIVE_FORCE_VARIANT=cuda12` for the fallback and
`LMGW_LIVE_PORT_RANGE` for the engines' ports, then `cargo test --workspace --features
lmgw-core/opus-bundled --test it -- runtime_engine_assets::engine_assets_live --nocapture
--test-threads 1`. `LMGW_LIVE_ESPEAK_MSI` (the downloaded MSI) checks the unpack alone, on any
platform: 443 files in 0.45 s on the desktop.

Seen on the way, upstream: llama.cpp b12244's CUDA build exits with 0xC0000005 when it is asked
for `-ngl 99` and every CUDA device is hidden (`CUDA_VISIBLE_DEVICES=-1`); with `-ngl 0` the same
build serves on the CPU. lmgw does not start that combination knowingly (a GPU row whose run args
pass no card gets the start warning of §4.1). Not reported upstream yet.

Building an engine from a git ref on Windows (container builds' path) is not in this version;
ik_llama.cpp publishes no Windows builds, so it is not offered on Windows yet.

### 4.3 VRAM

**Built (WP5, 2026-11-10).** On Windows the ledger's probe is `vram::wddm::WddmProbe`
(`vram::detect_probe`): NVML for the devices, power and driver version, D3DKMT for what each
process holds. Linux keeps NVML, then amdgpu's sysfs counters, unchanged.

- **Totals.** NVML works on Windows for the device: total, used, free. The ledger and the
  admission (per-model containers §4) read those as on Linux. `nvml.dll` is loaded by its full
  path, from the system directory (`GetSystemDirectoryW`, where every DCH driver installs it;
  `C:\Windows\System32\nvml.dll` with driver 560.81), then from
  `%ProgramFiles%\NVIDIA Corporation\NVSMI\` (older standard drivers). A full path keeps the
  application and working directories out of the search for `nvml.dll` itself. Its static
  imports would still follow the default search order, which starts at the application
  directory, through nvml-wrapper's load (libloading 0.8.9's `LoadLibraryExW(path, NULL, 0)`);
  nvml-wrapper 0.12 takes only a path, not a library loaded beforehand. So `vram::nvml::load`
  loads it first with `LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32`, which
  resolves its static imports from its own directory and the system directory only, and
  nvml-wrapper's load by the same full path then gets that module, already bound. The flags
  cover only the static imports: a delay-loaded import, or a `LoadLibrary` call inside
  `nvmlInit` itself, still follows the default search order. Neither path found is the probe's error,
  naming both paths; there is no second source of device totals on Windows yet.
- **Per process.** NVML answers `[N/A]` per process under WDDM (§2, point 5; again on 2026-11-10
  for the engine's PID). The source is `D3DKMTQueryStatistics` (`gdi32.dll`), what Task Manager's
  "Dedicated GPU memory" reads: `D3DKMTEnumAdapters2` lists the adapters; per adapter
  `D3DKMT_QUERYSTATISTICS_ADAPTER` gives the segment count and `D3DKMT_QUERYSTATISTICS_SEGMENT`
  each segment's `Aperture` flag; a process's bytes are the `BytesCommitted` of
  `D3DKMT_QUERYSTATISTICS_PROCESS_SEGMENT` summed over the local (non-aperture) segments.
  - **Which adapters count:** those at the PCI bus, device and function of an NVML device
    (`KMTQAITYPE_ADAPTERADDRESS` against NVML's bus id; WDDM reports no PCI domain, so none is
    compared). An NVML device with no such adapter is an error naming the address, never a
    smaller share. An adapter listed twice (one LUID) counts once; two adapters at one NVML
    address are an error naming both, since which one holds the card's memory would be a guess.
    An adapter whose address or segments cannot be read is kept with the call and NTSTATUS that
    failed: when it is the one at NVML's address the error is that failure, and when its address
    is unknown, the "no adapter there" error lists it as a possible cause.
  - **The probe's availability check** (`GpuProbe::process_support`, asked on async threads and
    before every pass) is cheap: NVML loaded and the D3DKMT adapter count answering, one system
    call. Matching the adapters to NVML's devices is part of each process answer, on the blocking
    thread the pass reads on.
  - **The handle needs `PROCESS_QUERY_INFORMATION`.** With only
    `PROCESS_QUERY_LIMITED_INFORMATION` every process query answers `STATUS_INVALID_PARAMETER`.
    A process of the same user grants it without elevation, so lmgw reads its own children.
  - **"Nothing on this adapter"** is `STATUS_INVALID_PARAMETER` from
    `D3DKMT_QUERYSTATISTICS_PROCESS_ADAPTER`, once `D3DKMT_QUERYSTATISTICS_PROCESS` has answered
    for the handle (it does for any process). A PowerShell with no GPU device answers that way on
    every adapter, the engine on the two adapters it does not use.
  - **An exited process lmgw still holds a handle to** (the native supervisor keeps the engine's
    until its log readers are drained) still opens by PID, and its statistics queries fail
    (`D3DKMT_QUERYSTATISTICS_PROCESS` answers `STATUS_INVALID_PARAMETER` for an exited `cmd.exe`
    whose handle a test holds, 2026-11-10 with driver 560.81). When a query fails and
    `GetExitCodeProcess` says the process is no longer `STILL_ACTIVE`, it reads as gone, not as a
    failure of the whole pass (`vram::wddm::read_opened`, tested through stated processes on
    every platform and strictly on that real handle on Windows).
  - **The probe contract** (`GpuProbe::processes`) is amdgpu's: answered per PID asked about. A
    process with no bytes on a counted adapter, or one that is gone, is not listed; one of lmgw's
    own that cannot be opened or read is an error; a tombstone that cannot be opened or read is
    skipped.
  - **Descendants** come from a Toolhelp snapshot (`vram::wddm::toolhelp`); a process counts as
    a child only if it was created no earlier than the parent it names, because Windows keeps a
    dead parent's PID, and PIDs are reused. The walk is `vram::wddm::tree`, over `(pid, ppid,
    created)` records, tested on every platform; creation times are read only for the processes
    the parent PIDs reach from the engine. A snapshot that cannot be taken is an error with its
    Win32 error code (`ProcTree::descendants` answers a `Result`), never an empty tree. Every
    native engine has one child: the `conhost.exe` Windows starts for a console program, even
    one created with `CREATE_NO_WINDOW`. It holds nothing on the card, so it is not listed, and
    the engine's own bytes are the member's.
  - **Reused PIDs and tombstones.** Windows hands a stopped engine's PID to the next process at
    once, often the next engine. A tombstone whose PID is now a registered container's process
    is dropped (at the next PID plan for a root, at the pass's settle for every process the pass
    measured), on Linux as well, so one process is never counted as a member and a tombstone at
    once. Only a generation still registered when the settle runs protects against tombstones:
    the settle checks, under the PID cache's lock, that a measured PID is a registered
    generation's root or in its remembered tree. A generation evicted between its measurement and
    the settle (an audio residency reading, or a concurrent pass) thus keeps its tombstones while
    its process is still in podman's stop grace and on the card, and the outside-VRAM pass
    discards a measurement whose member retired before its lock ("lmgw's containers changed
    while measuring"). On Windows a tombstone also keeps the creation time its process was
    measured with (`ProcTree::started`); one whose PID now runs a process created at another time
    is dropped and not asked about, so an unrelated process of the same user that got the PID is
    never counted as lmgw's. Of two tombstones for one PID with different creation times the
    later one is kept, since a PID is handed out again only once its process is gone, so a
    generation retiring with a stale tree never replaces a live child's tombstone. On Linux tombstones have no start time yet, so only the PID rule applies
    there.
- **What lmgw's own children are** comes from the native launcher's `host_pids` (§4.1), so the
  outside-VRAM verdict (candidate aliases §4.7) and the residency sampler work on Windows as they
  do on Linux.

**Measured 2026-11-10** on the §2 VM (Windows 11 build 26100, GeForce GTX 970, driver 560.81),
over SSH as the non-administrator account (OpenSSH's network logon, no interactive session):
llama.cpp b12236 with Qwen3.5-0.8B UD-Q4_K_XL, `-ngl 99 --ctx-size 4096`, one chat request ("Name
three colors.", 32 tokens). "Delta" is NVML's used bytes minus the 25,108,480 B (23.9 MiB) used
before the start; after each stop the card was back at that figure.

| Build | Point | D3DKMT, engine PID | NVML used delta | Probe − delta | Aperture (not counted) |
|---|---|---|---|---|---|
| CUDA 12.4 (`llama-b12236-bin-win-cuda-12.4-x64.zip` + cudart) | ready | 798,081,024 B (761.1 MiB) | 797,081,600 B (760.2 MiB) | +999,424 B (0.95 MiB) | 62.8 MiB |
| CUDA 12.4 | after the chat | 834,781,184 B (796.1 MiB) | 833,781,760 B (795.2 MiB) | +999,424 B | 62.8 MiB |
| Vulkan (`llama-b12236-bin-win-vulkan-x64.zip`) | ready | 738,967,552 B (704.7 MiB) | 737,443,840 B (703.3 MiB) | +1,523,712 B (1.45 MiB) | 229.5 MiB |
| Vulkan | after the chat | 746,831,872 B (712.2 MiB) | 745,308,160 B (710.8 MiB) | +1,523,712 B | 230.5 MiB |

So the probe holds: within 0.2 % of the totals, and above them, so lmgw's share reads slightly
high and outside use slightly low. That is the direction the fallback already errs in. The
local segment group's `Usage` (`D3DKMT_QUERYSTATISTICS_PROCESS_SEGMENT_GROUP`, what DXGI's
`QueryVideoMemoryInfo` reports) matched the segment sum to the byte each time. The aperture
column is system memory mapped for the card, which NVML's used figure leaves out too. The VM lists
three adapters: the GTX 970 (local segments of 471.4 MiB and 3559.4 MiB, the card's
0.5 + 3.5 GiB split, and two apertures), the VM's display adapter (apertures only),
and the Basic Render Driver (no PCI address); only the first counts. The SSH session read
everything lmgw needs; only other users' processes (`dwm`, `LogonUI` in the console session) were
refused (`OpenProcess`: error 5), and lmgw never asks about those. The PDH counters the spike
could not read (§2 point 5) are not needed.

Reproduce: build the probe with `cargo build --example wddm-probe --features
lmgw-core/opus-bundled` (`crates/lmgw-core/examples/wddm-probe.rs`; its header has the usage),
then `powershell -ExecutionPolicy Bypass -File scripts\windows-wddm-probe.ps1 -Server
<build>\llama-server.exe -Model <gguf> -Probe target\debug\examples\wddm-probe.exe`. The live
test `wddm_live_engine_bytes_match_nvmls_used_delta` (`tests/it/vram_wddm.rs`, behind
`LMGW_LIVE_WDDM_ENGINES` and `LMGW_LIVE_WDDM_GGUF`) checks the same through the registry, the
native launcher and `WddmProbe`. It passed on 2026-11-10 on the same VM and builds, from the same
SSH session, with the same figures as the script: CUDA 798,081,024 B against a delta of
797,081,600 B, Vulkan 738,967,552 B against 737,443,840 B, the engine's `conhost.exe` unlisted,
and nothing listed for the engine after its stop. After the review round it ran again on CUDA
(same VM, driver and build) with the same 798,081,024 B against 797,081,600 B, and its new check
passed: the stopped engine, opened through a handle the test held across the stop, reads as gone
(its statistics query failed and `GetExitCodeProcess` no longer answered `STILL_ACTIVE`), also
when asked as one of lmgw's own; `an_exited_process_with_an_open_handle_reads_as_gone` checks the
same with a `cmd.exe` and needs no card. Run it through `scripts/windows-remote-check.sh`
with both variables set: `cargo test --workspace --features lmgw-core/opus-bundled --test it --
vram_wddm --nocapture`. The other `vram_wddm` tests (the decisions against a stated kernel, and on
Windows the real readers against the test's own process) need no card and run in every Windows
test pass.

### 4.4 The platform layer

| Area | Linux today | Windows |
|---|---|---|
| Secrets | Secret Service | Windows Credential Manager (built, WP6) |
| Machine check | CDI spec, SELinux, pkexec helper (`lmgw-machine-helper`) | the display adapters and their driver, NVML for NVIDIA, the processor, free space; no root helper (built, WP6) |
| Updates | `lmgw-update` installs the RPM | the per-user NSIS installer from the Tauri bundler (`%LOCALAPPDATA%\Programs\lmgw`, no administrator rights), the release feed's `nsis` entry beside the RPM, refused without a 64-hex SHA-256 or with a URL that is not https (http only to loopback), SHA-256-checked after the download and again right before the start; downloaded into WP6's private dir, started silently after the quit sequence, it starts lmgw again; one update at a time (built, WP7, §7) |
| Data dir | XDG | `%APPDATA%\lmgw` (settings, database) and `%LOCALAPPDATA%\lmgw` (models, engines) (built, WP6) |
| Private files | 0700/0600 checked by uid and mode | owner SID and DACL, checked and set by `lmgw-private` (built, WP6) |
| Child hygiene | `pre_exec` (`close_range`, `setrlimit`) | Rust's non-inheritable handles; created suspended into the job; `--memory` and `--cpus` as job limits (built, WP6) |
| Webview | WebKitGTK | WebView2 (Chromium): the WebKitGTK workarounds do not apply; ui-drive and webkit-check do not cover it yet |
| TCP keepalive | `TCP_USER_TIMEOUT` and keepalive times | keepalive times through `socket2`, read back with `TCP_KEEPIDLE`/`TCP_KEEPINTVL`; no user timeout (built, WP6) |
| Tray | KDE StatusNotifier | the Windows notification area (Tauri); links through the shell's `open` verb, `"`, `\`, whitespace and control characters percent-encoded; seen in the notification area's overflow, its menu and links working (WP8, §7) |

**Built (WP6, 2026-11-10).**

- **Private files: what "private" means** (`crates/lmgw-private`, a crate of its own because
  `lmgw-core`, `lmgw-update` and the tests of both need it, and neither `lmgw-machine`, which
  writes nothing, nor `lmgw-update`, which is the feed's, is its home; on Unix it is empty). The
  owner is the account lmgw runs as (its token's user SID), `SYSTEM` or the built-in
  Administrators: those two hold every right anyway, and a file made in an elevated session (a
  main TLS key an administrator's tool wrote) is the Administrators' by default, which is no
  finding. Every allow entry of the DACL grants
  rights to that account, `SYSTEM`, the built-in Administrators, `CREATOR OWNER` or `OWNER
  RIGHTS`, which is what a profile's own directories grant (`%LOCALAPPDATA%`, `%TEMP%`) and what
  root is on Linux, where a `0600` file is root's to read too. Any other account with a right is
  a finding; so is a NULL DACL (every account, every right) and an entry of a kind it does not
  read (object and compound allow entries, and any kind it does not know). Deny entries (types
  1, 6, 10, 12) only take rights away, and the kinds that belong in a SACL (audit, alarm, label,
  resource attribute, scoped policy, trust label, access filter) grant none; both are let be. An
  inherit-only entry (it applies to what is made inside a directory, not to the directory) is
  judged as if it applied to the object too: conservative, a finding rather than a miss. A key allows
  others nothing that reads or changes it; a directory whose files are what is private allows
  nothing that changes it (Linux's `mode & 0o022`). What lmgw makes gets a *protected* DACL of
  exactly the three entries, and its owner named, in the call that creates it
  (`CreateDirectoryW`/`CreateFileW` with a security descriptor: Linux's `mkdir`/`open` mode), so no
  other account ever had a moment of access; `make_private` replaces an existing one's (Linux's
  `chmod 700`). Every open passes `FILE_FLAG_OPEN_REPARSE_POINT`, so a symbolic link or junction
  is opened as itself and refused; a create is `CREATE_NEW`. Checked on the Windows VM: the
  profile's temp dir, as Windows made it, passes the strict check (the assumption "a per-user
  directory under the profile" rests on).
- **Where it is used.** `lmgw-machine`'s file facts: `stat`/`lstat` from the handle (kind, size,
  the NTFS file ID as the inode, write and change times; `Meta::posix` is `None` on Windows, and
  the Linux readers that check a mode or uid treat that as a file that does not pass) and free
  space (`GetDiskFreeSpaceExW`, the volume serial as the file system ID). `lmgw-update`: the
  download dir is `<app>` under the user's temp dir, created private and checked when it is there;
  the download file is created private and `CREATE_NEW`, after the unlink of what was there. The
  device CA (`devices/tls/files/windows.rs`): `tls/` made private and checked (others may read,
  not change; checked after a create too, as what stands there then may have been made by another
  in the moment between the look and the create, and so on Linux, where a recursive `mkdir`
  succeeds on a directory that is there), the key created private, read from the handle its owner and DACL were checked on,
  replaced through a private temporary file and a rename (NTFS journals it; there is no
  directory handle to sync); the certificate takes `tls/`'s access. lmgw's CDI spec dir
  (`runtime/cdi/generate.rs`): made private, closed when others may only read it, refused when
  they may change it; a staging dir of a process that is gone is swept (`OpenProcess` says gone).
  The main TLS key (`server/main_tls`): the same check at each load, a Settings warning naming
  the accounts and the `icacls` line that closes it.
- **Secrets** (`devices/tls/secret/credman.rs`). The device CA's wrapping keys are generic
  credentials of the user's, `CRED_PERSIST_LOCAL_MACHINE` (they do not roam: a key that opens one
  machine's CA file stays on that machine), target `lmgw:device-ca-wrap:<data dir>|<16 hex>`, the
  blob the key's 64 hex digits; `|` is no character of a path, so `CredEnumerateW`'s prefix
  filter finds one data dir's keys exactly. The data dir goes into the name resolved once
  (canonical, the `\\?\` prefix taken off; `\\?\UNC\…` as `\\…`), and a found name is compared
  with it without case, as Windows compares paths, so `C:\Users\…`, `c:\users\…` and
  `\\?\C:\Users\…` are one dir's keys. The comment is a fixed text, as its limit
  (`CRED_MAX_STRING_LENGTH`, 256 units) is one a long path passes; the target name's own limit,
  `CRED_MAX_GENERIC_TARGET_NAME_LENGTH` (32767 units), is checked before Credential Manager is
  asked, and a data dir whose name would be longer is refused with that limit named. Every call runs
  on a blocking thread. Add beside, prune to the one kept, as with the Secret
  Service. A production start installs it where Linux installs the Secret Service store; the
  sentences say "Windows Credential Manager".
- **Data dirs** (`state::data_dirs`, the one place that knows them). The installed app's data dir
  is `%APPDATA%\lmgw` (`APPDATA`, else the roaming known folder): the databases, the settings in
  them, the device CA. Its local dir, `%LOCALAPPDATA%\lmgw`, holds what is big or can be fetched
  again: a fresh install's models dirs (`<local>/models/<class>`) and the engines
  (`<local>/engines`, `engine_assets::engines_root`, which the launcher, the installer, the Engines
  page and the Windows room item all read). Any other data dir (a dev instance,
  `LMGW_DATA_DIR`) keeps everything in itself. Linux: the local dir is the data dir.
- **Child hygiene** (`runtime/launcher/native/windows.rs`, `limits.rs`). Every engine is created
  `CREATE_SUSPENDED`, assigned to the kill-on-close job, and only then resumed (its one thread,
  found through a Toolhelp thread snapshot: std's `main_thread_handle` and
  `spawn_with_attributes`, which would hand the thread or `PROC_THREAD_ATTRIBUTE_JOB_LIST` over, are
  unstable in Rust 1.99), so not a line of the engine runs outside the job; one that cannot be
  assigned is killed still suspended. Not closed yet: lmgw dying in the moment between the
  spawn and the assignment leaves the suspended engine outside the job, where nothing kills it,
  until `PROC_THREAD_ATTRIBUTE_JOB_LIST` (a process created in a job) can be passed. The run
  args' `--memory`/`-m` (binary units, as Podman reads
  them) and `--cpus` are a job of the child's own, nested in the kill-on-close one:
  `JOB_OBJECT_LIMIT_JOB_MEMORY` (committed memory of the engine and what it starts; an allocation
  past it fails inside the engine, where a cgroup would reclaim cache first, and a mapped model
  file is not committed memory) and a CPU rate hard cap of `cpus` of the logical processors (none
  at or above their number). The rate is a share of the whole machine's processor time, so the
  count is every processor group's (`GetActiveProcessorCount(ALL_PROCESSOR_GROUPS)`,
  `lmgw_machine::cpu::logical`; std's count sees one group, at most 64), and the log names the
  rate set and the count it was taken from. A failed limits job kills the engine before it ran,
  and says which step failed. Every other limit (`--memory-swap`, `--cpuset-cpus`, `--pids-limit`,
  …) stays in the start's "not applied" line. Handles: std opens its own non-inheritable and makes
  the child's stdio ends inheritable for the spawn alone; a handle another library made
  inheritable would still pass, as std creates every process with `bInheritHandles`.
- **The machine check** (`machine/windows_check.rs`): the same six items. §1.1 reads the display
  adapters from the display class's driver keys in the registry (`lmgw_machine::display`: name,
  driver provider and version, PCI vendor and device, the memory the driver reports; every user
  may read them, vendor-neutral; only the keys of display devices SetupAPI lists as present,
  `DIGCF_PRESENT` and each device's `SPDRP_DRIVER`, since a key outlives a card taken out) and, for an NVIDIA adapter, NVML in a separate process (`lmgw
  --machine-probe nvml`, started from `current_exe`) for the driver's and CUDA's versions and each
  card; an AMD adapter is a card used through the Vulkan builds; Intel and Microsoft's basic
  adapter alone are no card, as on Linux. §1.2 says there is nothing to set up. §1.3 names the
  processor (`lmgw_machine::cpu`, `cpuid`: the brand, the logical processors of every processor
  group, SSE4.2, AVX, AVX2,
  FMA, F16C and AVX-512 F/BW/VL, what ggml's CPU variants choose by) and says when AVX2 is
  missing. §1.5 is the room where the models dirs are and where the engines are (`<local
  dir>/engines`, named as Linux names Podman's image store: "the software that runs models"). §1.4 and §1.6 are not checked yet on
  Windows (they read images, and the engines' builds are WP4's). No CDI spec is written at boot.
- **TCP keepalive** (`server/conn.rs`): set through `socket2` (`SIO_KEEPALIVE_VALS`), read back
  with `TCP_KEEPIDLE` and `TCP_KEEPINTVL` (Windows 10 1709 and later), so a stream's override and
  its restore work as on Linux. Windows has no `TCP_USER_TIMEOUT`: a socket reads as having none,
  and a stream that asks for one gets the keep-alive alone, which the log says once per process.
- **Tray.** The Tauri tray builds on Windows and is wired as on Linux (the same
  `TrayIconBuilder` code). Its Open in Browser, and every link the window hands to the desktop
  (a `target="_blank"` link from a chat or a model's page, a pairing link), goes to the shell's
  `open` verb (`ShellExecuteW`, on a thread of its own with COM set up, `src-tauri/src/shell_open.rs`),
  not to `explorer.exe`'s command line, where a comma in a URL would be a switch; the scheme
  filter (web and mail links only) stays in front of it. The handler's registered command still
  gets the URL pasted into its `%1` as it is, and an opaque URL (a `mailto:` link's path) keeps a
  literal `"` and spaces, which would end the handler's argument and start others, and a trailing
  `\` would turn the handler's closing `"` into an escaped one: so `"`, `\`, whitespace and
  control characters are percent-encoded before the call (WP7; a web URL has none left once
  parsed but a `\` in its query, a `mailto:` handler decodes them). A failure is logged with `ShellExecuteW`'s
  own return code and its meaning (it sets no last error). A pairing link goes only to a
  registered handler, asked as the shell resolves it (`AssocQueryStringW` for the scheme's `open`
  command, with `ASSOCF_INIT_IGNOREUNKNOWN` so a scheme without a key of its own cannot resolve
  to the shell's `Unknown` "open with" class; Linux asks `xdg-mime`); lmgw's installer registers
  none, since the link is a client's (Kai's) to take. Seen 2026-11-10 on the VM of §2 over SSH (the query needs no desktop session), as a standard user: with no `lmgw-pair` key under `HKCU` or `HKLM`
  the check says no handler (`ERROR_NO_ASSOCIATION`, which this build answers without the flag
  too); with a throwaway per-user handler under `HKCU\Software\Classes\lmgw-pair` (its `open`
  command `whoami.exe %1`) it says one is registered; the key was removed again. A test does the
  same with a scheme of its own (`pairing.rs`). The link's hand-off to that handler, a click in
  the window, has not been seen yet: it needs the desktop session (§7 WP7).

**Look and feel: the Windows theme (built 2026-11-11).** The app window takes the system's dark
or light mode and the user's accent colour, through the desktop palette KDE's title-bar shade
uses (`src-tauri/src/desktop_palette/win.rs`; app.css "desktop palette"). Linux is unchanged.

- **Reading.** WinRT's `UISettings`, the documented API for both: the mode from
  `GetColorValue(Foreground)` (light text is dark mode, as Microsoft's Win32 dark-mode guide reads
  it) rather than the undocumented `AppsUseLightTheme` value, and the accent with its six shades
  (`Accent`, `AccentLight1..3`, `AccentDark1..3`). No rights needed, works from a per-user install
  (and even over SSH, session 0). `windows` 0.61, the version tao and wry build with.
- **Neutrals.** The bundled palette's OKLCH steps (`ramp.rs`) anchored on Windows 11's base
  surface as `--bg1`: WinUI's `SolidBackgroundFillColorBase`, `#202020` dark and `#F3F3F3`
  light, Mica's solid fallback (WebView2 has no Mica). Dark: sidebar `#191919`, content
  `#202020`, cards `#272727`, hover `#303030`, borders `#393939`/`#4D4D4D`; light: `#E9E9E9`,
  `#F3F3F3`, `#FBFBFB`, `#FFFFFF`, `#D7D7D7`/`#BABABA`. Against Windows' own, sampled on the VM
  of §2 (build 26100, transparency off): Explorer's panes `#191919` and dividers `#383838`,
  Settings' page `#202020` and cards `#2B2B2B`, DWM's dark caption `#202020` and light caption
  `#F3F3F3`, so the title bar runs on into the content row. `--text` stays the bundled one, as
  on KDE.
- **Sidebar and title bar as one L (built 2026-11-11).** The leftmost sidebar is the native title
  bar's colour, as on Plasma. The shell sets the colour rather than guessing DWM's:
  `DwmSetWindowAttribute(DWMWA_CAPTION_COLOR)` (Windows 11, build 22000 and up, read from
  `CurrentBuildNumber`) gets the sidebar's colour, and `DWMWA_TEXT_COLOR` the black or white
  that reads on it. The colour is the same for the active and the inactive window (on build
  26100 the unfocused window showed the same colour; whether DWM needs it set again on
  activation was not isolated, the shell sets it on focus-in and after every theme change), so `title-inactive` is
  the sidebar's colour too. Dark `#191919` and light `#E9E9E9`: the step below the content, as
  before, now also the title bar. With "Show accent colour on title bars and window borders" on
  (`HKCU\Software\Microsoft\Windows\DWM\ColorPrevalence` = 1) the colour is the accent DWM
  would paint (`DWM\AccentColor`, ABGR), the sidebar takes it, and its text, icons, hover and
  lines come from contrast (`desk-side` in app.css); the panes keep their neutrals. Before
  build 22000 the attribute is not there: the sidebar is anchored on DWM's default caption
  colour for the mode (dark `#202020`, light `#FFFFFF`, not measured on Windows 10 yet), or the
  accent, and the log says once which path was taken. A change reaches the window through
  `ColorValuesChanged` and the focus-in re-read.
- **Accent.** `--accent` is WinUI's `AccentFillColorDefault` for the mode, the shade Windows
  fills its own accent controls with (Explorer's pills, Settings' selection mark): `AccentLight2`
  in dark, `AccentDark1` in light. If the bundled text on accent (`--accent-ink`, `#10161C` dark,
  white light) reads below 4.5:1 on it, the next shade further from that text is taken (dark:
  Light3, Light1; light: Dark2, Dark3), and only if none reads the best shade and text pair by
  contrast, which flips the ink (it is shared with the ok/amber/error marks, so it stays the
  bundled one whenever a shade allows). `--accent-hi` is the bundled hover step away from the
  ink (+0.048 / −0.065 OKLCH lightness), `--accent-bg` the fill at the bundled .13/.10, selected
  text (`::selection`) the plain `Accent` under the dark or white text, whichever reads better.
  Measured shades: Blue (Windows' default) dark `#4CC2FF` on `#10161C`, light `#0067C0` on white;
  Gold light skips `AccentDark1` `#E37700` (2.7:1 with white) for `AccentDark2` `#A14600`.
- **Theme setting.** In the Windows app window Settings offers "system" beside dark and light,
  and it is the default there: the page shows Windows' mode. A forced theme stays and still gets
  Windows' neutrals and the accent; unlike KDE's one-mode title-bar shade, both sets exist for
  every system mode, so the KDE rule "only when desktop and app agree" has no case to drop here.
- **Live.** `UISettings.ColorValuesChanged` re-reads and re-applies at once (mode and accent
  picked in Settings, seen on the VM); the shared re-read on focus-in catches what the event
  misses (a registry-only edit was picked up only there).
- **Title bar.** The page tells the shell the theme it shows (`lmgw:theme` through the event
  permission it already had); the shell sets Tauri's window theme, which is tao's
  `DWMWA_USE_IMMERSIVE_DARK_MODE` with a redraw, so a forced dark lmgw on a light system has a dark
  frame. Before the page reports, tao follows the system's mode itself.
- **Not yet.** The tray menu stays light in dark mode (muda's popup menu on tray-icon's hidden
  window is not opted into dark mode; the notification area's flyout itself is the system's).
  Native dialogs were not seen: the file picker only opens from an agent's config, which needs
  Podman. High-contrast themes keep this palette under WebView2's forced colours, not checked
  yet. "Show accent colour on title bars" is DWM's to draw; the sidebar does not follow it yet. A
  Gold or orange accent sits close to the palette's amber liveness colour; nothing tells them
  apart yet.

### 4.5 Not on Windows in this version

- **Agents.** They run as containers (agent container runtime design). On Windows they show as
  needing Podman, which this version does not drive.
- **Fork and git builds of engines** (§4.2).
- **The headless server image.** It stays Linux.
- **The pull service's Unix socket**, which serves image pulls; it has nothing to pull on
  Windows.

## 5. CI

**No CI job needs a graphics card.** The Windows runner has no graphics card, so the live engine tests of §4.1 to §4.3 run by hand on a machine with a card, behind
their environment variables (`LMGW_LIVE_ENGINE_ASSETS`, `LMGW_LIVE_NATIVE_GGUF`, … in §4.2 and
§4.3), and skip themselves without them; WP8's live round is such a run.

**The runner**: tag `windows`, the shell executor with Windows PowerShell 5.1, builds under `D:\build`, one job at a time, Rust installed machine-wide. GitLab's `git clean
-ffdx` empties the build dir before every job, so `CARGO_HOME`, the target dir, ort's cache and
the release build's tools live in persistent dirs on `D:` (`.windows-runner` in `.gitlab-ci.yml`,
which its jobs extend). The runner gets no new system-wide tool: it has no Trunk and no
wasm-bindgen (checked 2026-11-10), and the jobs below need neither.

Both jobs also compile through sccache (`ci/windows-sccache.ps1`): release 0.19.0
of its `x86_64-pc-windows-msvc` zip, pinned with its SHA-256 in that script, fetched into the tools dir (`LMGW_TOOLS_DIR`) per user when missing or another version; the cache is a persistent dir on the build drive,
capped at 100 GiB (`SCCACHE_CACHE_SIZE`), with `CARGO_INCREMENTAL=0` in `.windows-runner`.
GitHub's hosted runner does not use it yet. Measured 2026-11-11 (sccache
0.19.0, each job's `--show-stats` log): `windows-check` made 64 requests, none cacheable (60 of
them clippy's, "multiple input files"); `windows-installer` made 12, with 6 Rust misses: the
version stamp rebuilds the workspace crates, and `CARGO_PKG_VERSION` is part of sccache's key. So
the kept `CARGO_TARGET_DIR` does the caching in both jobs, and sccache pays off only after a
toolchain or profile change, which empties what the target dir can reuse.

**The internal CI** (every push to `main`, a test feed):

- `windows-check` (stage `build`, WP1): `cargo clippy --workspace --all-targets -- -D warnings`,
  beside the RPM job and outside `publish`'s needs. Its run by hand on the runner's machine in a
  scratch checkout with empty dirs (2026-11-10) took 12 minutes, the crate downloads included,
  and a failing cargo call ended the script with cargo's exit code. Its test step,
  `cargo test --workspace`, is written into it as a comment: not yet, until the Windows tests pass
  (a work package of its own).
- `windows-installer` (stage `server`, needs `build` with its artifacts): `ci/build-windows.ps1`
  builds the per-user NSIS installer into `dist-windows/` (the installer and `nsis.json`: version,
  file, size, SHA-256), with the RPM's version (`dist/build.env`), Trunk's UI bundle
  (`crates/lmgw-ui/dist/`, added to `build`'s artifacts for it), the same update feed and deploy
  token baked in as the RPM's, and the test builds' profile (thin LTO, 16 codegen units). The
  script takes tauri-cli as the prebuilt release asset of the version `ci/install-build-deps.sh`
  pins, its SHA-256 pinned, into `LMGW_TOOLS_DIR`; on its first bundle the CLI
  downloads NSIS 3.11 and its Tauri plugin into the runner account's `%LOCALAPPDATA%\tauri`
  itself (SHA-1 checked by it). Windows' own libopus is the vendored one (`src-tauri`'s Windows
  dependency on `lmgw-core/opus-bundled`, built with the Build Tools' cmake).
- `publish-windows` (stage `publish`, needs `publish` and `windows-installer`, resource group
  `registry-latest`): `ci/publish.sh windows` checks `nsis.json` against the file, uploads the
  installer under its version beside the RPM, and adds the `nsis` entry to `latest/latest.json`
  when the published manifest is this pipeline's version (a later pipeline's is left alone).

The RPM never waits for Windows: `publish` needs only `build`, and a failed Windows job leaves
that pipeline's manifest without an `nsis` entry, which a Windows lmgw reads as "no Windows
installer in this release".

**GitHub** (tags, the public feed; `.github/workflows/release.yml`): the build job hands on
Trunk's bundle as the `ui` artifact; `windows` builds the installer with the same script on a
GitHub-hosted `windows-latest` runner (no card), with the release profile (fat LTO, one codegen
unit) and the public default feed; `publish-windows`, after `publish`, checks it against the tag
and `nsis.json` as `publish` checks the RPM, uploads it to the release and replaces the
release's `latest.json` with one that has the `nsis` entry. No checkout and no repository code
in either publish job.

None of the Windows jobs has run in GitLab or on GitHub yet; `ci/build-windows.ps1` and the
installer it builds were run and checked by hand on the runner's machine (§7 WP7), and
`ci/publish.sh windows` against a stub `curl`.

The installer is not signed yet (§7, open questions): a browser's download of it carries the
mark of the web, and SmartScreen warns about an unknown publisher before it runs. The updater
writes its download itself, with no such mark: the update in the desktop session ran without
any prompt (§7 WP7).

**Windows tests (WP7).** `cargo test --workspace --features lmgw-core/opus-bundled` (there is no
`opus.lib` to link on Windows; cargo-nextest is not installed on the VM, so the doctests run in the
same call) runs on the VM of §2 (2026-11-10/11, through `scripts/windows-remote-check.sh`, the VM
shared with another build): 1 h 53 min to 2 h 9 min for the whole workspace, of which the `it`
binary is 6107 s to 6407 s for its 4496 tests and lmgw-core's library 539 s to 564 s. It does not
pass twice in a row yet: besides the gates below, each of three runs had 2 to 6 tests fail, a
different few each time, each passing in the other runs: timing bounds missed under the suite's
load (`stream_silence` 2, `vram_admission::request_gate` 1, `realtime_semantic` 1,
`realtime_batches` 1, `accounts::throttle` 1, `backends_updates`' stalled remote 1), and once
SQLite's "database is locked" in `store_write_lock`, which the begin-write rule should rule out
and is not looked into on Windows yet. None of these was run alone yet; a run with fewer test
threads or nextest's retries may settle the timing ones, which is not tried yet. It needs the UI bundle Trunk builds on Linux copied into `crates/lmgw-ui/dist` (the VM has no Trunk; `scripts/windows-remote-check.sh`
keeps that dir out of its mirror). What the run found in the product, fixed: free space is read
on Windows (`GetDiskFreeSpaceExW`; device transfers refused every offer as if the disk were full),
a transfer's hash syncs through a handle open for writing (`FlushFileBuffers` refused a read-only
one), a declared oversize `/v1` or `/tokenize` body is drained before the 413 (Windows resets a
connection closed with unread bytes, and the reset lost the refusal), the main TLS key's and
certificate's paths are judged absolute (`Path::is_absolute`, so `\certs\tls.crt` and
`/etc/tls.crt`, which the working directory's drive resolves, are refused), not by a leading `/`,
the audio rows' weight candidates are written with `/`, and the physical cores come from
`GetLogicalProcessorInformationEx`. The drain is bounded as nginx's lingering close is: 30 s in
all and 5 s without a byte (nginx's `lingering_time` and `lingering_timeout` defaults), and only a
body declared at most twice `max_body_mb`; a bigger one is answered at once, and a cut drain or a
body over that budget is logged with the budget, since its client may then see the reset instead
of the 413. A client sending `Expect: 100-continue` is answered at once without reading the body
(reading it would make hyper ask for it), as on Linux, where the body is never read; an HTTP/2
body is not drained (a stream is refused without a connection reset). The review round of
2026-11-11 found and fixed three more: the device listener's sockets take `SO_EXCLUSIVEADDRUSE` on
Windows, where `SO_REUSEADDR` let another process bind the same port, and its "no IPv6" check
reads Winsock's codes; a download without a dialog goes to the Downloads known folder
(`SHGetKnownFolderPath(FOLDERID_Downloads)`, `%USERPROFILE%\Downloads` only when the shell cannot
say it, logged), where it was refused with "no HOME"; and a display device gone between the listing
and its driver key's read (`ERROR_NO_SUCH_DEVINST`) is skipped like one without a driver key
instead of failing the adapter check. The test fixtures' large
model files are marked sparse (`FSCTL_SET_SPARSE`), and `free_port`'s cross-process claim is a
named mutex. What is gated, each with its reason in the code:

- `cfg(unix)`, not on Windows in this version (§4.5): the tests that read or start agents (their
  private run dir is Unix's): `agents_package`, `agents_service`, `agents_service_wire`,
  `accounts::agent_mcp`, `main_tls::agent_proxy`, `server_forwarded::agent_proxy`, 3 tests of
  `agents_ops_wire`, 2 of `agents_self_view`, 2 of `agents_wire`, 1 of `self_admin_agents`, and 12
  of the library's `agents::package` and `agents::service` tests; engine builds from git: 5 tests
  of `backends_git`, 3 of `backends_ops`, 1 of `self_admin_builds`, and buildah's cache dirs, 2 of
  `backends_ops`; the headless server's data-dir lock naming its holder
  (`state::data_dir_lock`, 1; Windows' lock is mandatory, so the holder line cannot be read while
  it is held).
- `cfg(unix)`, PDF extraction, which does not run poppler on Windows yet (§3.1): 7 of
  `extract::pdf`'s tests run Unix tools (`sh`, `yes`, `cat`, `sleep`, `false`) under its limit,
  and `chat_attachments_wire`'s PDF upload; on Windows `a_run_is_refused_on_windows` asserts the
  refusal instead.
- `cfg(unix)`, a machine that cannot be stated on Windows: `vram_admission::gpu_setup`,
  `gpu_setup_rows`, `gpu_setup_stamp` and `engine_prefetch` state a Linux machine through the
  fake facts and need its check `ready`; Windows' check reads the computer's own display
  adapters, which no fake states yet (`machine::windows_check`'s judgements are tested in the
  crate). `engine_prefetch`'s pre-fetch is of Podman images too; a pre-fetch of the engines'
  release builds is WP4's not-yet.
- `ignore` on Windows, a product gap not closed yet: 15 tests whose refusal a Windows reset loses
  before the peer reads it. lmgw refuses while the peer is still sending and drops the
  connection without reading on to the peer's end; on Windows only a declared oversize `/v1` or
  `/tokenize` body is drained so far (above). 7 HTTP 413s: a chunked `/v1` body (`body_limit` 1),
  the Chat's upload and dictation (`chat_attachments` 2, `chat_voice_dictation` 1), a chunked
  `/tokenize` body (`count_compat` 1), an op's body (`audit::flood` 1) and a transfer's cut body
  (`transfers::stops` 1); 5 WebSocket closes over a frame or message limit (`mcp_host::close` 2,
  `mcp_host::link` 1, `realtime_session` 2); 3 TLS alerts of a device-listener handshake it
  refuses (`device_pairing` 3). The dictation test and `/tokenize`'s are split since 2026-11-11:
  dictation within `max_body_mb` and a declared oversize `/tokenize` body run on Windows, only
  their refusals named here are ignored.
- `ignore` on Windows, a product gap not closed yet whose cause is not found yet: 4 calls tests.
  `realtime_calls_close::liveness`'s never-connected call (its paused-clock bound overshot by 40 to
  470 ms), and 3 of `realtime_calls_net` (the client's DTLS handshake times out against a
  wildcard-bound call socket and after the device group moves; a new call's client reads a stale
  call's DTLS records). The device listener's `SO_EXCLUSIVEADDRUSE` changed none of them, nor
  `device_pairing`'s 3: run with `--include-ignored` on the VM (2026-11-11), all 7 still fail with
  the errors named in their reasons.
- `ignore` on Windows, missing a timing bound only under the whole suite's load there (each
  passes run alone, `-- --ignored`): `realtime_barge` 1, `realtime_barge_words` 2,
  `realtime_liveness` 3, `vram_admission::request_gate` 1. They are not made load-proof there
  yet.
- Asserted as Windows answers: a stale build check's pid is left alone on Windows (whether a
  process runs is not read there yet), the update check's engine count is the live release API's
  answer (that test does not state it, and the check asks GitHub; a stated release API for it is
  not built yet), and the RPM install's absolute `pkexec`/`dnf` paths are asserted on Unix only.

## 6. Work packages

| WP | Content | Depends on |
|---|---|---|
| WP1 | **Built 2026-11-10 (§3.1).** The workspace compiles on Windows: `cfg(unix)` gates and Windows branches for the §3 list; Linux-only crates (`lmgw-machine-helper`, `lmgw-server`, `folder-chat`) out of a Windows build; the `windows-check` clippy job; `scripts/windows-remote-check.sh` | — |
| WP2 | **Built 2026-11-10.** The launcher trait and the Podman launcher behind it, no behaviour change on Linux (§4.1) | — |
| WP3 | **Built 2026-11-10 (§4.1).** The native launcher: job object, environment GPU selection, log capture, readiness, the JIT-aware start timeout; tested on Linux with the release binaries | WP2 |
| WP4 | **Built 2026-11-10 (§4.2).** Engines as release assets: the pin table with digests and CUDA coverage, download and verify, the per-engine variant choice and the Vulkan fallback; eSpeak NG for the phonemizing audio families | WP3 |
| WP5 | **Built 2026-11-10 (§4.3).** VRAM on Windows: NVML totals, the D3DKMT per-process probe and, if it holds, attribution through it | WP3 |
| WP6 | **Built 2026-11-10 (§4.4).** Platform layer (§4.4): Credential Manager, data dirs, private files, child hygiene, the Windows machine check | WP1 |
| WP7 | **Built 2026-11-10 (§4.4, §5).** Packaging and updates: NSIS per-user installer, the update path, the Windows CI job with the installer artifact | WP1, WP6 |
| WP8 | **Built 2026-11-11 (§7 WP8 as built).** Live round on the Windows runner: chat, image and audio through lmgw end to end, on CUDA and Vulkan | WP4, WP5, WP7 |

WP1 and WP2 can run in parallel; they touch different files.

## 7. Decisions, open questions, not yet

**The owner's rulings.** The direction of this spike (native Windows build, 2026-11-10), and two
of 2026-11-11 after WP8's live round:

- CUDA always ranks first: a CUDA build that reaches a card only through PTX ranks above
  Vulkan, as §4.2's choice already has it; WP8's question whether it should rank below is
  settled, and nothing changed in the code.
- What would work is never blocked; it starts with a warning. VRAM admission charges
  an image pipeline that can run with less on the card its files where they fit, and otherwise
  lets it start with less, held to that with `--max-vram` and with a warning (per-model
  containers design §4, image-generation design §9; WP8's not-yet item below).

**The design's own decisions:**

1. Engines run as native processes on Windows; Podman, WSL and Windows containers are not used
   there (§1).
2. Children are tied to lmgw's lifetime through a kill-on-close job; no re-adoption after a
   restart (§4.1).
3. Engine builds are upstream release assets pinned by digest, the variant chosen per engine
   from recorded CUDA coverage, Vulkan the fallback (§4.2).
4. Agents and engine builds from git are not on Windows in this version (§4.5).
5. Until WP4, the native launcher runs the one build directory under
   `<data>/engines/<engine>/`; several are an error that lists them, not a pick by name (§4.1).
6. A native stop on Windows is `TerminateProcess` at once; on Linux `SIGTERM`, then `SIGKILL`
   after the registry's 10 s grace (§4.1).
7. A native engine's log is an in-memory ring of 4000 lines per child, kept until the next start
   under its name or a remove, with nothing on disk (§4.1).
8. A card named in the run args by NVIDIA index selects only CUDA (`CUDA_VISIBLE_DEVICES` in PCI
   bus order); Vulkan and HIP builds see every card until their device order is mapped (§4.1).
9. The native launcher adds no start timeout of its own: a first CUDA start's JIT has to fit in
   `vram.load_timeout_seconds` (600 s by default), as every start does (§4.1).

**Open questions.**

- ~~Per-process VRAM from `D3DKMTQueryStatistics`: does it match the totals closely enough for
  attribution (§4.3)?~~ Answered 2026-11-10: yes, within 0.2 % of NVML's used delta on CUDA and
  Vulkan, from a non-administrator SSH session; WP5 attributes through it (§4.3).
- Code signing for the installer, and its cost.
- Whether lmgw should offer Podman-backed agents on Windows when Podman Desktop is already
  installed.

**Not yet in the native launcher (WP3).** On Linux the group `SIGKILL` after the child was
reaped reaches a recycled group id only in a theoretical race that is not closed yet;
a setting that selects it on Linux; a log file on
disk beside the ring; reading a native engine's own `--help` for the sd-server and llama flag
vocabularies; `--cpuset-cpus` as CPU affinity, and on Linux `--memory` and `--cpus` (named in the start's log
as not applied); mapping an NVIDIA card index to
Vulkan's and HIP's device order; a check that the listener on a model's port is its own child
(Windows' `GetExtendedTcpTable` or Linux's `/proc/net/tcp` would say), so a foreign process that
took the port and answers `/health` in the moment before the engine's bind fails is not told
apart yet; telling a first start (JIT) from a later one to give it a longer budget.

**WP6 as built.** The platform layer of §4.4, 2026-11-10; \1ts decisions:

- WP6.1 "Private" on Windows is the owner SID and the DACL as §4.4 says: the account lmgw runs
  as, `SYSTEM` and the built-in Administrators may hold rights and own it, nobody else; a NULL
  DACL or an entry the check does not read is not private, and an inherit-only entry is judged as
  if it applied. What lmgw makes gets a protected DACL of those
  three in the call that creates it.
- WP6.2 That meaning lives in one new crate, `lmgw-private`, empty on Unix, rather than copied
  into the three crates that need it.
- WP6.3 The wrapping keys are generic credentials persisted on this machine only, so they do not
  roam; a logon with no credential store answers "unavailable", and the key stays plain (private
  to the account) with the warn line, as a Linux start with no Secret Service does.
- WP6.4 The installed app's data dir is `%APPDATA%\lmgw` and its models and engines go under
  `%LOCALAPPDATA%\lmgw`; any other data dir keeps everything in itself. The device CA stays
  with the database; a roaming profile that carries it to another machine meets a wrapped key
  whose wrapping key stayed behind, which the CA's error says (rotate the device CA).
- WP6.5 Engines are created suspended and resumed after the job assignment, the thread found
  through a Toolhelp snapshot, because std's ways to hand over the thread or a job list are
  unstable.
- WP6.6 `--memory` is the job's committed-memory limit and `--cpus` its CPU rate hard cap, in a
  job of the child's own nested in the kill-on-close one; on Linux the native launcher applies
  neither yet.
- WP6.7 The Windows machine check reads the adapters from the registry's display class keys
  (vendor-neutral, no rights needed), NVIDIA's from NVML in its own process; Intel and the basic
  adapter alone are no card, as on Linux. §1.4 and §1.6 are "not checked yet" there.
- WP6.8 A stream's `TCP_USER_TIMEOUT` is not applied on Windows; the keep-alive is, and the log
  says once per process that the user timeout was not.
- WP6.9 The main TLS key is checked with the CA key's rule (owner included) and a finding is a
  Settings warning, as Linux's mode warning is.
- WP6.10 (review round) A wrapping key's credential names the data dir canonical and without
  `\\?\`, found without case; a data dir too long for a target name is refused with
  `CRED_MAX_GENERIC_TARGET_NAME_LENGTH` named. Links go to the shell's `open` verb, not
  `explorer.exe`. The display adapters are the present devices'. The Windows room item names the
  engines' dir.

Checks of 2026-11-10 (Rust 1.99.1 on the Windows VM of §2, Windows 11 build 26100):
- **Credential Manager over SSH**: in the key-auth SSH logon (a standard user, not an administrator) every
  call answers `ERROR_NO_SUCH_LOGON_SESSION` (1312), as `cmdkey /generic:…` does there ("Credentials
  cannot be saved from this logon session"); lmgw says it as "unavailable" in those words. The
  same test run as `SYSTEM` through a one-off scheduled task (created and deleted again by the
  administrator account; that logon has a credential store) stored two keys, found them by data
  dir and not those of a dir whose name starts the same, pruned to the one kept and cleared:
  `LMGW_CREDMAN_REQUIRED=1 it.exe credential_manager --nocapture`. No desktop session was open
  on the VM, so the store has not been run from one yet.
- **Tests on Windows** (`cargo test --workspace --features lmgw-core/opus-bundled` through
  `scripts/windows-remote-check.sh`, filtered to `credential_manager private_files_windows
  runtime_launcher_native windows_check data_dirs server::conn limits cdi::generate
  windows_tests windows:: download:: display cpu:: main_tls::slot`): `lmgw-private`'s (what lmgw
  makes is private, the profile's temp dir as Windows made it is, a widened grant is refused and
  `make_private` closes it, a junction is refused), `lmgw-update`'s private dir and download,
  `lmgw-machine`'s file facts and free space, the display adapters (the VM's: the basic display
  adapter and the GTX 970) and the processor, the device CA's files, the main TLS key's readers,
  the CDI spec dir and staging sweep, the stream timers' set and restore, the data dirs, the
  Windows check's judgements, and the native launcher's, among them a dropped launcher killing
  its engine (the supervisors' kill or the job's, whichever is first; it does not tell them
  apart) and a `--memory 64m --cpus=1` engine whose
  256 MiB commit fails while an uncapped one's succeeds; every start there went through
  `CREATE_SUSPENDED`. Windows' `cargo clippy --workspace --all-targets -- -D warnings` passes
  (6.5 minutes on a warm target). Found on the way: Windows 11's redirection guard refuses to
  traverse a junction a standard user made (`ERROR_UNTRUSTED_MOUNT_POINT`, 448), which
  `lmgw-machine`'s test accepts as the answer it is; and an update download whose hash did not
  match stayed on disk, because Windows deletes no file that is still open without delete
  sharing (the file is closed before the check now).
- **Review round, same day, same VM**: as `SYSTEM` (the one-off scheduled task again, created
  and deleted by the administrator account) the Credential Manager test found one dir's keys
  under its own spelling, in upper case and as `\\?\…`, and stored, found and pruned a key for a
  data dir longer than 256 units. The adapters read with the present-device filter are the same
  two (`0000` the basic adapter, `0001` the GTX 970, 4 GiB): both are present on this VM, so no
  absent key has been seen dropped yet. `GetActiveProcessorCount(ALL_PROCESSOR_GROUPS)` says N (one group). The killed-lmgw test passes there (the kill-on-close job alone), and the limits
  engine reads its own job: the memory limit 64 MiB and a CPU rate hard cap of 10000/N for `--cpus=1` of N. Windows' clippy and the platform tests pass.
- **Not lmgw's WP6, failing on Windows**: the device-link tests that need `free_port`'s
  cross-process claim (`tests/it/support/device_link.rs`: `mcp_host::link`, `chat_feed::live`,
  `transfers::download`) and two of the shell's download-folder tests (`src-tauri`
  `downloads::tests`, which state Unix paths and XDG user dirs); the Linux machine check's tests, which state a
  Linux machine, are gated to Unix now. Seen in the review round, outside WP6's files:
  `vram_admission`'s fixture makes its GGUFs with `set_len`, which is no sparse file on NTFS, so
  its tests fail with `StorageFull` when the volume runs low (two runs of the same tree passed
  once and failed once), and `vram_admission::engine_prefetch`'s tests expect a ready machine
  verdict where Windows' check answers `NeedsSteps`.

Not yet (WP6): closing the moment between an engine's spawn and its job assignment (lmgw dying
there leaves the suspended engine outside the job, where nothing kills it, until
`PROC_THREAD_ATTRIBUTE_JOB_LIST` is usable from std); a handle list (`PROC_THREAD_ATTRIBUTE_HANDLE_LIST`) that keeps a library's
inheritable handle out of a child (std's `spawn_with_attributes` is unstable); `--cpuset-cpus` as
the job's CPU affinity and the other container limits; the machine check's own program runs
(`run_limited`) still stop only the program, not what it started; §1.4 and §1.6 on Windows; the
map from a registry adapter to NVML's and Vulkan's device order; `TCP_MAXRT`, Windows' nearest
thing to a user timeout; the device transfers' files, the webview's store, the labs' artifacts,
the voice lab's clips and audio.cpp's rendered configs still under the data dir, not the local
dir; poppler for PDF extraction (§3.1); a data dir inside a directory with NTFS's per-directory case
sensitivity on (`fsutil file setCaseSensitiveInfo`): Credential Manager's target names are found by
a prefix compared without case, so two data dirs there whose paths differ only in case would find
each other's wrapping keys, a known gap.

**Windows theme as built (2026-11-11, §4.4 "Look and feel").** The window follows Windows' dark
or light mode and accent colour live (`UISettings`), with Windows 11's neutrals and WinUI's accent
fill shades, a "system" theme setting as the default in the Windows window, and the frame's dark
mode through Tauri's window theme. Seen on the VM of §2 (build 26100, WebView2 157.0.4341.46, the
app at its default 125 %) in dark and light with the Purple, Blue and Gold accents, switched in
Settings and by `AppsUseLightTheme` with an `ImmersiveColorSet` broadcast. \1ts decisions: `UISettings` over the registry for both; the neutrals as the bundled
steps on Windows' base surface rather than a hand-picked table; WinUI's fill shades with the
bundled ink kept where a shade allows; "system" offered and default only in the Windows window.
Not yet: the tray menu's dark mode, native dialogs, high contrast.

*Sidebar as the title bar's colour (2026-11-11).* The explicit caption colour (§4.4) was checked
in the VM's desktop session (build 26100, the app at 125 %), dark, light, and each with the
accent on title bars (Purple, `#680081`), focused and unfocused: title bar and sidebar sampled
identical in all eight (`#191919`, `#E9E9E9`, `#680081`, `#680081`), switched live while the app
ran (registry write plus an `ImmersiveColorSet` broadcast). Not yet: the Windows 10 path (no
machine to measure DWM's default caption colour on), and a toggle of the accent on title bars
that raises neither `ColorValuesChanged` nor a focus change is seen only at the next focus-in.

**Not yet.** macOS (the same native launcher, with Metal builds; MLX remains the separate engine
question), AMD ROCm builds on Windows (Vulkan covers AMD in this version), ARM64 Windows
(llama.cpp ships ARM64 builds; sd.cpp and audio.cpp do not yet).

**WP5 as built.** Windows' VRAM probe is NVML's totals with D3DKMT's per-process bytes (§4.3).
\1ts decisions:

- WP5.1: D3DKMT is the per-process source on Windows, NVML stays the source of the device totals,
  power and driver version.
- WP5.2: the adapters counted are those at an NVML device's PCI bus, device and function; an NVML
  device without one makes the per-process answer an error, never a smaller share.
- WP5.3: a process with no bytes on a counted adapter is not listed, as NVML does not list a
  process without a context, so a GPU row whose engine holds nothing reads "not attributed" on
  Windows as on Linux.
- WP5.4: processes are opened with `PROCESS_QUERY_INFORMATION`, the least access the statistics
  answer for.
- WP5.5: `nvml.dll` is loaded by full path (the system directory, then NVSMI), not through the
  DLL search order.
- WP5.6: the adapters and their segments are enumerated again on every read, a few system calls,
  so an adapter that comes or goes is never stale.
- WP5.7: descendants come from a Toolhelp snapshot, a child counting only if it was created no
  earlier than its parent.
- WP5.8 (review round): a tombstone whose PID a registered container's process has is dropped on
  every platform; on Windows a tombstone is (PID, creation time), and one whose PID runs a
  process with another creation time is dropped.
- WP5.9 (review round): an exited process lmgw still holds a handle to reads as gone; an
  unreadable adapter is kept with its NTSTATUS and named when it is the one at NVML's address;
  one LUID counts once and two adapters at one NVML address are an error; a failed Toolhelp
  snapshot is an error; the availability check no longer matches adapters.
- WP5.10 (second review round): a measured PID drops a tombstone only while its generation is
  still registered, on every platform; the outside-VRAM pass refuses a measurement whose member
  retired before its settle; of two tombstones for one PID the one with the later creation time
  is kept.

Not yet in WP5: device totals without NVML (D3DKMT's segment commit limits and committed bytes
would cover AMD and Intel cards on Windows; until then such a machine has no telemetry, as an
Intel host on Linux); per-process bytes per device rather than pooled over the counted adapters;
a measurement under memory pressure, where WDDM demotes allocations to system memory and the
committed bytes may no longer be resident; measurements on sd.cpp's and audio.cpp's builds, on
more than one card, and on a card that also drives a desktop; start times for Linux tombstones
(`/proc/<pid>/stat`'s `starttime`), so the reuse rule beyond the PID one applies there too.

**WP4 as built.** §4.2 records the module. \1ts decisions:

- WP4.1 The pins follow the newest upstream Windows build on the day they are made, as the Linux
  side follows its engines' moving tags; a newer upstream release is an "update available" on the
  Engines page and in the nav badge, and runs only once an lmgw release pins it.
- WP4.2 A build is chosen when it reaches every NVIDIA card NVML lists, not only the card a row's
  run args name: one install serves every row of the engine.
- WP4.3 A row on the CPU runs its engine's machine build with the cards hidden (§4.1); no CPU
  build is downloaded beside a GPU one (llama.cpp b12244's CUDA 12 build served Qwen3.5-0.8B on
  the CPU with `CUDA_VISIBLE_DEVICES=-1` and `-ngl 0` on the VM, 2026-11-10).
- WP4.4 A model's first start installs its engine's build inside `prepare`, outside the load
  timeout, and so does the Vulkan fallback's second `prepare`; a connection not made in 30 s and
  a stall of 60 s fail a download (each figure is in its error), a download that moves has no
  bound.
- WP4.5 The Vulkan fallback marks the failed build's directory and passes over every CUDA build
  of that release, so a CUDA 13 build without kernels goes to Vulkan, not to CUDA 12. A mark
  needs an exit lmgw did not ask for, not with code 0, with ggml's CUDA error line; it stays until
  the owner clears it (`engine_fallback_clear`) or a new pin brings a new build directory.
- WP4.6 "Vulkan is usable" is read as "the Vulkan loader `vulkan-1.dll` is installed".
- WP4.7 eSpeak NG is installed with every audio start (lmgw has no per-family "phonemizes" fact
  yet), unpacked from its MSI by lmgw (the `msi` crate and an own cabinet reader), and reached
  through `PATH` and `ESPEAK_DATA_PATH`; a failed install is a start warning, not a failed start.
- WP4.8 audio.cpp's native `server.json` backend follows the build that runs (§4.2).
- WP4.9 audio.cpp's non-portable `cpu` and `vulkan` assets are pinned, not the `-portable` ones.
- WP4.10 The Windows tab shows only where lmgw starts its engines natively; on Linux the
  `engine_releases` answer has no engines, no NVML is loaded for it and the release check asks
  nothing.
- WP4.11 A platform without pins keeps WP3's hand-installed layout (one build directory per
  engine; directories starting with `.` are the installer's and are skipped), which the Linux
  tests and live checks use.
- WP4.12 A rename or removal that "access denied" or a sharing violation refuses is tried 8 times
  over 7.1 s before it is the install's error, which names both figures.
- WP4.13 A build directory re-pinned under the same tag (same repository, tag and variant, other
  assets) is moved aside to `.<dir>.replaced-<time>`, never deleted.
- WP4.14 Installs exclude each other across lmgw processes through a file lock per build beside
  the build directory, and their downloads are kept per engine (`.downloads/<engine>/<dir>`).
- WP4.15 A row that names cards and runs a pinned Vulkan build starts, with a warning that the
  card selection does not reach the Vulkan build yet.
- WP4.16 A start whose entry a stop, the hold or an eviction takes while it prepares ends
  `Aborted`: its install's waits and downloads stop within 250 ms, and the registry launches
  nothing (it asks before every launch and right after one).
- WP4.17 A failed eSpeak NG install is remembered for 10 min (`ESPEAK_RETRY_AFTER`, stated in
  the warning); the audio starts in that time start without it at once.
- WP4.18 A wait for another lmgw process's install is a note on the starting entry, with no time
  bound.
- Not yet in WP4: an install button, download progress on the page or the jobs feed, and a
  prefetch before the first start; resuming a partial download; cutting short an unpack or a
  rename when the start goes away; a note for a wait on another start of the same process's
  install of the same build (that start is on the dashboard itself); removing an older pin's build
  directories after an update (they stay, as do marks); pins for Linux or macOS; the ROCm, SYCL
  and OpenVINO assets; a choice per row's card; a setting that forces a variant (only the tests
  can); checking the unpacked files again on later starts (the install record is checked);
  noticing a "no kernel image" from an engine that keeps running after a failed request before
  it is stopped; Quantum and LZX cabinets; a native `--help` read for the flag vocabularies with
  the pinned builds; the Windows tab as the default tab on Windows; mapping an NVIDIA card
  selection (PCI bus id) to `GGML_VK_VISIBLE_DEVICES` (the VM has one card, so no mapping could
  be checked there); a typed exit status from the supervisor (the mark reads its sentence,
  "exited with code 0" being the clean one); removing `.replaced-…` directories and lock files.

**WP7 as built.** Packaging and updates, 2026-11-10: the per-user NSIS installer, the update
path through the release feed, the Windows CI jobs (§5). \1ts decisions:

- WP7.1 The installer is Tauri's NSIS bundle, per user (`installMode: currentUser`, no
  administrator rights), installed to `%LOCALAPPDATA%\Programs\lmgw`, Windows' place for a user's
  programs, with a Start menu entry. Tauri's default for a per-user install is
  `%LOCALAPPDATA%\<product>`, which is lmgw's local data dir (models, engines, §4.4), so lmgw
  keeps a copy of Tauri's template (`src-tauri/windows/installer.nsi`, tauri-bundler 2.10.0 of
  tauri-cli 2.12.0) with its changes marked `lmgw:`, made again with every tauri-cli bump
  (`ci/install-build-deps.sh` says so beside the pin).
- WP7.2 The uninstaller has no "Delete the application data" box: it keeps `%APPDATA%\lmgw`
  (settings, database, device keys), `%LOCALAPPDATA%\lmgw` (models, engines) and the window's
  browser data (`%LOCALAPPDATA%\io.github.lmgw-dev.lmgw`), says the first two on its confirm page
  and all three in its log. Tauri's box deleted only the identifier's dirs, never lmgw's.
- WP7.3 WebView2: `downloadBootstrapper`, silent, Tauri's default mode. Windows 11 ships the
  runtime, and the installer finds it in the registry and adds nothing; a machine without it gets
  Microsoft's bootstrapper downloaded and run silently, which needs the network then. Not the
  embedded bootstrapper (a larger installer for a case Windows 11 does not have) nor the offline
  runtime (an installer larger by the whole runtime). An update (`/UPDATE`) skips the check.
- WP7.4 The release feed's manifest gains an `nsis` entry beside `rpm`, the same shape (file,
  URL, SHA-256): `lmgw-update`'s `RpmArtifact` is `Artifact` and `download_rpm` is `download`,
  and `ReleaseManifest::installer` is the platform's entry. The integrity rule is the RPM's (the
  manifest's SHA-256, checked after the download into the private file, a mismatch removes it),
  and stricter for the installer: an `nsis` entry whose SHA-256 is empty or not 64 hex digits, or
  whose URL is not https, is refused, the reason in the dialog (`lmgw_update::nsis::check_entry`).
  http is allowed to a loopback host (127.0.0.1, ::1, `localhost`), which
  `scripts/windows-installer-check.ps1`'s local feed needs; the RPM path checks no scheme, as an
  `LMGW_UPDATE_ENDPOINT` of any URL is the owner's own choice. Right before the start the
  installer is hashed again, through a handle that shares reading only, so nothing can write,
  rename or delete it until the installer has started; a mismatch removes the file, starts
  nothing, and the next start's dialog says so. Neither file is signed yet. An older lmgw ignores
  the new key. A release without the entry (its Windows build failed) is "no Windows installer in
  this release", not "up to date"; that and every other reason a copy cannot install
  (`lmgw_core::update::cannot_install`) read the same in the tray's dialog and the dashboard's
  check.
- WP7.5 Only the installer's own copy updates itself: the per-user uninstall key's
  `InstallLocation` must be the running `lmgw.exe`'s directory. Any other copy (a build dir) is
  told why and pointed at the installer, as a Linux host without dnf is.
- WP7.6 The update follows Linux's steps (offered, confirmed, installed), with Windows' end: after
  the download and its check lmgw notes what it installs (`update-pending.json` in the private
  dir), runs the quit sequence (the server, then the engines), starts the installer detached with
  `/S /UPDATE /R` (silent, as an update, lmgw started again when done; out of lmgw's job where the
  job allows it) and exits. The next start reads the note: its own version at or past the noted
  one is a log line, an older one a dialog ("did not install"), and the note and the download are
  removed. An installer that does not start restarts lmgw on the old version, and the dialog says
  why. One update at a time: the poll's dialog and "Check for Updates…" can both be open and both
  download to the one fixed path, so a confirmed install holds a guard from the download through
  the install (until lmgw exits on Windows, until dnf is done on Linux), and a second confirmation
  while it is held gets a dialog that an update is already running and starts nothing. A
  confirmation while lmgw already quits keeps the note, with that reason, for the next start.
- WP7.7 lmgw's installer registers no `lmgw-pair:` handler: the link is a client's (Kai's), whose
  installer registers the scheme; the pairing check asks `AssocQueryStringW` with
  `ASSOCF_INIT_IGNOREUNKNOWN` (both answers seen on 2026-11-10, §4.4).
- WP7.8 A URL's `"`, `\`, whitespace and control characters are percent-encoded before
  `ShellExecuteW` rather than refused, so a `mailto:` link still opens.
- WP7.9 The Windows app always links the vendored libopus (`src-tauri`'s Windows dependency on
  `lmgw-core`'s `opus-bundled`), so `cargo tauri build` needs no feature flag on Windows.
- WP7.10 The publisher is `lmgw-dev` (`bundle.publisher`): Tauri took `github` from the
  identifier otherwise, for the Apps list and the installer's `Software\<publisher>` key.
- WP7.11 CI (§5): `windows-installer` after `build` (the UI bundle from its artifact, no Trunk on
  the runner), `publish-windows` after `publish`; on GitHub a hosted Windows runner; no job needs
  a graphics card.

Checks of 2026-11-10 on the VM of §2 (Windows 11 build 26100, WebView2 157.0.4341.42, Rust
1.99.1, tauri-cli 2.12.0, as a standard user, not an administrator), with installers that
`ci/build-windows.ps1` built there (thin LTO, 16 codegen units, `-j 8` on a machine another
build shared, the UI bundle from a desktop `trunk build --release`); reproduce with
`scripts/windows-installer-check.ps1` (its header has the usage):

| Check | Result |
|---|---|
| The build | the installer 51.1 MiB (`lmgw_0.6.0+1_x64-setup.exe`), `lmgw.exe` 199.4 MiB inside; 31.6 min for the script with an empty release dir (29.4 min of it cargo; NSIS 3.11 and the plugin downloaded once), 21.4 min after a version change (61.7 min while another test build ran beside it at `-j 8`); the binary's version resource is the stamped version, which the script checks |
| Silent install (`/S`) | exit 0 in 15 to 19 s over SSH (41 s in the desktop session while a build ran beside it), to `%LOCALAPPDATA%\Programs\lmgw` (`lmgw.exe`, `uninstall.exe`), a Start menu and a desktop entry, the per-user uninstall key (version, publisher `lmgw-dev`, `InstallLocation`) |
| The installed `lmgw.exe` | with a seeded data dir: `GET /v1/models` answers on 127.0.0.1:18401 0.6 s after the start over SSH, 3.1 s in the desktop session |
| The update, in the desktop session | the local feed (127.0.0.1:18402, `latest.json` with an `nsis` entry for 0.6.0+2) offered 0.6.0+2 to the running 0.6.0+1 20.3 s after the start (the updater's 20 s start delay); after "Install & Restart" 0.6.0+1 exited in 2.1 s, and 0.6.0+2, started by the installer, answered `GET /v1/models` 17.1 s after the click (48.2 s in an earlier run with a build beside it); its start read and removed the note, and its first poll tick, 19.6 s later, the downloaded installer (still running when the start tried); no prompt of any kind came up |
| Silent uninstall | 1.1 to 5.5 s: the install dir, the Start menu and desktop entries and the key are gone; a file in each of `%APPDATA%\lmgw` and `%LOCALAPPDATA%\lmgw` stayed |

**Headless over SSH, and the desktop session.** Over SSH (session 0, no desktop) the install,
the start, the API, the update's check, its dialog (driven by `TDM_CLICK_BUTTON`), the download
and its check, the quit sequence and the installer's start all work, and the window exists, not
visible. What did not: the exit after the installer's start. lmgw went on running (its window
not hung), so the installer could not replace `lmgw.exe` and, silent, still recorded the new
version in the uninstall key; the next start's check would say the update did not install. In the standard user's desktop session (console session 2, the scheduled task with an interactive logon that the user made for itself and deleted again) the same run exits 2.4 s after the click and comes back
as the new version, and the window shows the dashboard in WebView2 . So the window and the update need a desktop session; the API does not. The tray's icon
was not seen in the notification area's visible part (its window exists; Windows puts a new
icon in the overflow).

Seen on the way, outside WP7: on Windows the shell's start says "file dialogs: no file-chooser
portal for this desktop: the file dialogs are GTK's own", three boot passes warn that `podman ps`
could not be run, and the engine pre-fetch's metered check reads `/proc/net/route`; over SSH the
quit sequence's container stop took 17 s with no engine running. In the desktop session a silent
uninstall started 2 s after a running lmgw was killed ended without uninstalling and without a
word; 20 s later the same command uninstalled in 1.1 s (`scripts/windows-installer-check.ps1`
tries three times, 20 s apart, and says each try).

Not yet (WP7): signing the installer (the open question above), and Authenticode checked on the
update before it runs; why the exit does not complete in session 0; an installer that fails
after lmgw has closed does not start it again (the next start, by hand, says the update did not
install), and a silent installer that cannot replace a running `lmgw.exe` still records the new
version; why a silent uninstall right after lmgw ended in a desktop session does nothing; download progress in the update dialog; an MSI, a per-machine install and ARM64; the
GitLab and GitHub Windows jobs run in CI; the Windows tests as a CI step. (The tray in the
notification area and the Credential Manager store from a desktop session were seen in WP8.) `publish` and `publish-windows` are not one step yet: the resource group runs
one publish job at a time, but a later pipeline's `publish` can run between them, and then
`ci/publish.sh windows` skips `latest.json` on the version mismatch, so that pipeline's installer
is in the registry but not in the feed until a pipeline publishes both in order. Kai's side, not done
yet, at its next pin raise: `RpmArtifact` becomes `Artifact`, `download_rpm`
becomes `download`, and its `ReleaseManifest` literals gain `nsis: None` (`kai/src/update.rs`
lines 48, 218, 266 and 416 to 418, `kai/src/update/tests.rs` lines 28 and 360).

**WP8 as built.** The live round on the VM of §2, 2026-11-11, with a GTX 970 test card (Maxwell, compute capability 5.2, 4 GiB, driver 560.81, CUDA 12.6; Windows 11 build 26100,
WebView2 157.0.4341.46). The app was a CI installer build (`lmgw_0.6.0+1_x64-setup.exe`, SHA-256 `4403327d…53ca` as the feed's `nsis` entry said),
installed silently (`/S`, 12.7 s) as a standard user and started in that user's desktop session (console session
1, a scheduled task with an interactive logon), on its own data dirs (`%APPDATA%\lmgw`,
`%LOCALAPPDATA%\lmgw`), bound to 127.0.0.1 (its first start on the default port 8787, then a
port of the round's range through Settings and a restart) and reached through an SSH tunnel. Engine pins: llama.cpp
b12244, stable-diffusion.cpp master-1079-a3c49b1, audio.cpp v0.9.4, eSpeak NG 1.52.0. The card
also drives the desktop here: 127 to 134 MiB of it were outside use (dwm and the shell) in every
reading. Models: Qwen3.5-0.8B UD-Q4_K_XL and Qwen3-4B-Instruct-2507 Q4_K_M (ctx 8192, q8_0 KV,
flash attention, every layer on the card), Z-Image-Turbo Q4_K with its VAE and Qwen3-4B as text
encoder (512², 4 steps, `offload_to_cpu`), Kokoro 82M, Parakeet TDT 0.6B v3 and Supertonic 3
(Q8_0). Chat figures: 200 tokens of "Write a short paragraph about the history of the bicycle.";
decode is the streamed tokens after the first over their time. VRAM: lmgw's share is D3DKMT's sum
for its engines (`/api/vram`), NVML's the device's used bytes.

| # | Check | Result |
|---|---|---|
| 1 | llama.cpp b12244 `cuda12` (through compute_50 PTX; the driver's compute cache was warm from §2's runs), Qwen3.5-0.8B | pass. Install 276 + 373 MiB downloaded and verified in 6.8 + 7.2 s, unpacked, 22.7 s in all; the first start 35.2 s with it, a later one 2.6 s. 78.0 tok/s, first token 0.12 s; `/v1/messages` 2.6 s (a thinking block, the model reasons), streamed 0.10 s to the first delta. lmgw 804.7 MiB, NVML 932.1 MiB with 127.4 MiB outside, estimate 709 MiB |
| 1 | the same, Qwen3-4B | pass. Start 9.3 s; 27.0 tok/s, first token 0.09 s; `/v1/messages` `end_turn` in 7.0 s, streamed. lmgw 3248.4 MiB, NVML 3375.5 MiB, estimate 3085.6 MiB |
| 2 | llama.cpp `vulkan` (the CUDA 12 build marked by hand, below), Qwen3.5-0.8B | pass. The Vulkan build (36 MiB) installed in 1.7 s, start 5.9 s with it, 2.4 s later; 88.7 tok/s, first token 0.13 s; both APIs as on CUDA. lmgw 720.9 MiB, NVML 847.8 MiB |
| 2 | the same, Qwen3-4B | pass. Start 5.2 s; 40.9 tok/s. lmgw 3084.7 MiB, NVML 3211.3 MiB |
| 3 | sd.cpp as the machine chooses: `vulkan` (`cuda12` has no kernel below 6.1) | pass, with admission off for it (below). Install 1.5 s; sd-server ready 116 s after its launch; the first image 192 s after the request, a correct one (a red bicycle at a white wall, 477 KB PNG); the next 73.1 s. Idle on the card: nothing (offloaded); one generation's peak learned as 3.0 GiB |
| 3 | sd.cpp `cuda12` forced (`engine_assets_live_z_image_draws_after_the_fallback`, the registry and native launcher over the real pins, from SSH) | pass. Its 887 MiB download plus start 55.6 s, 7.5 s cached; the CUDA build gets to ready, and the first request ends it with code -1073740791 (0xC0000409) after `[E] ggml: CUDA error: no kernel image is available for execution on the device`; the next start marks it (that line, `exited with code -1073740791`) and runs the Vulkan build: ready 8.2 s, the image 200 in 72.9 s |
| 3 | Try CUDA again (`engine_fallback_clear`) | pass, clicked in the window (WebView2, below): the mark went, the tab showed `cuda12` again with its JIT note, and the next start ran the CUDA 12 build |
| 4 | Kokoro (`af_heart`) on audio.cpp `vulkan` with eSpeak NG | pass. eSpeak NG's MSI (12.2 MiB) downloaded in 0.3 s and unpacked in 1.0 s; the first sentence 10.3 s after the request (installs and start included), a 211 244-byte WAV; then 0.89 s |
| 4 | Parakeet on Kokoro's WAV | pass. 22.1 s cold, 0.21 s warm; "The quick brown fox jumps over the lazy dog near the riverbank." for "… river bank." |
| 4 | Supertonic | pass. 4.4 s cold, 0.68 s warm, a 379 886-byte WAV |
| 4 | Parakeet on the CPU (`backend: cpu`, 8 threads; the Vulkan build, cards hidden) | pass. 5.9 s cold, 0.58 s warm, the same text; no bytes on the card (D3DKMT), nothing charged |
| 5 | Kokoro and Parakeet resident | pass. Per engine (D3DKMT, `wddm-probe`): 999 112 704 B and 1 791 213 568 B, the figures lmgw learned as their residency (953 MiB, 1.7 GiB); together 2 790 326 272 B; NVML 2 922 762 240 B, the 132 435 968 B between them the desktop's |
| 5 | admission | pass. Qwen3-4B with the default 1 GiB headroom: refused ("needs 3.0 GiB plus 1.0 GiB headroom and the GPU holds 4.0 GiB in total"); with 512 MiB beside the two audio models, a start refused ("needs 3.5 GiB and only 1.2 GiB is free — held by audio/kokoro (953 MiB), audio/parakeet (1.7 GiB)"), a request evicted both and answered in 5.5 s. Z-Image: refused, "needs 6.2 GiB … no eviction can make it fit" (below) |
| 6 | the registry's while-loading fallback | not provoked: no engine dies of "no kernel image" while it loads on this card. llama.cpp's CUDA 12 build reaches 5.2 through PTX; sd.cpp's (above) and audio.cpp's (§4.2) load their weights without a kernel launch and fail at the first request, the after-ready path. It ran in the fake engine's tests only |
| 7 | the dashboard in WebView2 | pass. The window opens on the overview (1024 × 583 CSS px at 125 %) |
| 7 | the Engines page's Windows tab | pass after a fix: the machine line, each engine's pinned release, chosen build, builds and upstream chips (b12283 and master-1086-465adf3 newer upstream). A mark's line pushed Try CUDA again past the pane's edge; it wraps now below its build (checked in the window with the new rules injected) |
| 7 | the tray icon | pass. In the notification area's overflow (Windows 11 keeps a new icon there; `HKCU\Control Panel\NotifyIconSettings` has its entry, `IsPromoted` unset); right-click opens lmgw's menu |
| 7 | Credential Manager | pass. With the device listener on, lmgw made its device CA "key wrapped"; `cmdkey /list` in the session shows `lmgw:device-ca-wrap:C:\Users\<user>\AppData\Roaming\lmgw|<16 hex>`, local machine persistence; the next start read it back (same CA) |
| 7 | links through `ShellExecuteW` | pass. The tray's Open in Browser opened the dashboard in Edge; a `target="_blank"` link to `https://wp8-live-round.example/?a=1,b=2` clicked in the window opened in Edge with its comma intact |
| 7 | the machine check in the running app | pass. `ready`: both display adapters from the registry, "NVML in a separate process: driver 560.81, CUDA 12.6, 1 card(s)", the processor (its brand, logical count and AVX-512), the room of the models and engines dirs; §1.4 and §1.6 "not checked yet" |
| 8 | the GPU hold | pass. Engaged in 0.07 s: Supertonic stopped, Parakeet on the CPU kept; a chat request 503 `gpu_hold`; Parakeet on the CPU 200 in 0.62 s under it; released, the chat 200 in 3.5 s (its start included) |

Vulkan beat the PTX-compiled CUDA 12 build on this Maxwell card (88.7 against 78.0 tok/s for
0.8B, 40.9 against 27.0 for 4B), while the choice (§4.2) prefers a CUDA build that reaches a card
only through PTX. The owner ruled on 2026-11-11 that it stays that way. A possible cause of the gap on this card, not measured: the GTX 970's last 512 MiB
is a slower memory segment, so a run with more than 3.5 GiB on the card has part of it there. By
NVML the 4B runs held 3375.5 MiB (CUDA) and 3211.3 MiB (Vulkan) device-wide and the 0.8B runs
under 1 GiB, with the same gap, so the device totals alone do not show it; where the driver
placed each allocation was not read.

How the round was driven: the Vulkan pass wrote the mark (`.lmgw-no-kernel-image`) into the CUDA
12 build's directory by hand, since no setting forces a variant yet; the window was driven
through WebView2's DevTools port (`WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=…`,
on loopback), the tray by clicks a scheduled task made in the session; the image pass ran with
admission off, which was turned on again after it.

Reproduce: `scripts/windows-live-round.py` (its header has the usage) runs items 1 to 5 and 8
against an installed lmgw through an SSH tunnel, creating the rows it needs. Its run after the
round on the same app and VM gave the same picture within a few percent (75.5 and 89.6 tok/s for
0.8B on CUDA and Vulkan, 26.9 and 40.8 for 4B, the same VRAM figures to the 0.1 MiB, images in
109.2 s cold and 72.9 s warm, every audio answer and the hold as above). The sd.cpp fallback is
`LMGW_LIVE_ENGINE_ASSETS=<engines dir> LMGW_LIVE_IMAGE_DIR=<dir with the three files>
LMGW_LIVE_FORCE_VARIANT=cuda12 cargo test --workspace --features lmgw-core/opus-bundled --test it
-- runtime_engine_assets::engine_assets_live_z_image --nocapture` (the binary run directly with
those variables when it goes through `scripts/windows-remote-check.sh`, which passes none). Item 7
is by hand, as above.

Found and fixed in the round:

- Every model start on Windows ran podman's `--help` and judged lmgw's CDI spec against NVIDIA's
  Linux driver files, two warnings per start ("`podman --help` could not be run", "The NVIDIA
  graphics driver is not running: /proc/driver/nvidia/version is not there"). On Windows the CDI
  spec is unmanaged now ("engines run as native processes on Windows: no container reads a CDI
  spec").
- Creating a chat or image row tried to read the engine's flag vocabulary from its container
  image and reported the same Linux driver sentence. Where engines run natively the reads run
  nothing and say why: no image to read, and a read of the native build's own `--help` is not
  built yet (a test with a native launcher and no podman).
- The Windows tab's layout above.
- `scripts/windows-remote-check.sh` let cargo miss an edit: robocopy kept the Linux checkout's
  mtime on a file it wrote, so a file edited while an earlier remote build ran was older than that
  build's outputs and counted as built (the round's first Windows binaries lacked both fixes
  above). A file robocopy writes gets the time of the copy now.

The fixes were checked on the VM with the headless runner of the fixed tree (a scratch data dir
on 127.0.0.1): a chat row's create warns with the native sentence, and its starts log no CDI or
`podman --help` line. The installed app of the round predates them.

Found, not done yet:

- **Admission and an offloaded image pipeline.** Done 2026-11-11, by the owner's ruling above,
  and fixed the same day after its review. The image estimate was the pipeline's files on disk,
  so Z-Image-Turbo with `offload_to_cpu` (6.2 GiB) was refused on a 4 GiB card that runs it. A
  pipeline that can run with less on the card (`offload_to_cpu`, a `params_backend` of
  `cpu`/`disk`, sd.cpp's auto-fit) is still charged its files where they fit; where they do not,
  it is never too large for a card and gets its on-card part (for this row its largest file and
  its context until its peak is learned), else what is free beyond the headroom when that leaves
  at least the budget it has rendered with (`work_bytes`), else what is free only when nothing
  else of lmgw's could be waited for and no fallback would answer. Its context is the learned
  idle residency, else 1.0 GiB set aside for it. It is held to what it was given less the
  context with `--max-vram`, its container records the grant (`lmgw.grant`; native processes are
  not adopted across a restart, so that matters on Linux), the ledger charges it that or its
  budget and context where more, and it starts with "It will work but only fits partially on the
  GPU, expect slowdowns: …" and the figures, in the log, the start and test answers,
  `local_model_get` and on its row's warnings. On this card, beside the desktop's 130 MiB and
  with the default 1 GiB headroom: 6.2 GiB in full, 3.6 GiB largest file, 3.9 GiB free, so it is
  given 2.9 GiB, with nothing else of lmgw's on the card: held to 1.9 GiB on its first start, and
  to 2.9 GiB less 5 MiB on the starts after its context was read at rest. Still refused, with the numbers: a pipeline that pins its weights on the
  card (`auto_fit off`, a GPU-only `params_backend`) and is larger than it, a measured context
  that leaves no budget, and nothing beyond the headroom free (per-model containers design §4).
  After the third review the learned figures (peak, context, working budget) are keyed by the
  resolved engine, the native build included: the build the variant choice and the failed-CUDA
  marks pick (`Native::expected_build`, from the facts read at boot and at every start) and,
  for a running process, the build it started with, so this card's 5 MiB Vulkan context does not
  count once CUDA is chosen again (migration 0105). Less than the whole is given only where the
  engine sees exactly one card; the Vulkan build sees every card whatever the run args name.

  `--max-vram` on this card, measured 2026-11-11 with the installed engine's sd-server
  (`master-1079-a3c49b1`, Vulkan; sd.cpp has no sm_52 CUDA kernels), driver 560.81, one 512×512
  generation of Z-Image-Turbo Q4_K per case, the card's `memory.used` less its 154 MiB at rest:
  5 MiB at rest in every case; 3110 MiB during the generation with `--offload-to-cpu` and no
  budget, 1010 MiB with `--max-vram 1`, 2038 MiB with 2, 2038 MiB for auto-fit with 2 (all
  modules' weights in RAM), 146–150 s each; `--max-vram 0.5` fails its first segment (it asks
  for 942 MiB of budget). The flag binds `offload_to_cpu`, not only auto-fit, and sd.cpp parses
  it as a float. `scripts/sdcpp-max-vram-probe.ps1` reproduces a case; the table and the rule
  are in per-model containers design §4.
- WebView2's own context menu (Back, Refresh, Save as, Print, More tools) opens on a right-click
  in the window; it is not turned off yet.
- The answers of `image_model_set` and a local model's `command_line` show a `podman run` line on
  Windows, where the engine starts as a native process with another argv; the native argv is not
  shown yet.
- The device listener's start line names `TCP_USER_TIMEOUT 30 s` on Windows, where it is not
  applied (WP6.8 logs that once, separately); the line does not say so yet.
