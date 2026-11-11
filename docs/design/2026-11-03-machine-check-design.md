# The machine check: getting the graphics card ready without a terminal

**Draft, 2026-11-03, revised the same day** after an adversarial review and the owner's ruling on
the CDI spec (Decisions, ruling 4). This is part I of the newcomer proposal: on first launch lmgw checks whether this
computer can run local models, says what is missing in plain words, and fixes what it can behind
one password prompt. §7 marks the work packages that are built (WP0 to WP13, all of them; the
live round on a real machine is still to come). "Decisions" separates the owner's rulings from
this draft's own calls. "Not yet" lists what
this version leaves out, and nothing there is ruled out.

Companion records, cited by short name:
- "per-model containers §n": [per-model containers](2026-08-30-per-model-containers-design.md)
  (one container per model, the VRAM ledger);
- "container builds §n": [container builds](2026-09-26-container-builds-design.md) (the Engines
  page, image pulls, the registry check);
- "chat media §n": [chat media tools](2026-10-21-chat-media-tools-design.md) (wait phases a caller
  follows);
- "candidate aliases §n": [candidate aliases](2026-09-27-candidate-aliases-unified-kv-design.md)
  (fallback reasons, resolve-time swaps).

| § | Content |
|---|---|
| 1 | What is checked, in order, and what "ready" means |
| 2 | Where it shows, and what a failed start answers |
| 3 | The fix: the root helper, its plan, polkit, who may start it, lmgw's own spec, other systems, packaging |
| 4 | API and UI |
| 5 | The download before a model's first start |
| 6 | Tests |
| 7 | Work packages |

**Today** (lines at `main` e263e4ac):
- The README sends people to a terminal: install the driver and NVIDIA's container toolkit, then
  `sudo nvidia-ctk cdi generate --output=/etc/cdi/nvidia.yaml` (`README.md:151-154`).
- The GPU probe is picked once per process (`vram/scheduler.rs:26`, `detect_probe`). NVML is
  initialised once, in-process (`vram/nvml.rs:157`). If it fails, the ledger reports "NVML
  unavailable: …" as a degraded state. That is correct for the ledger, but a newcomer is never told
  what it means or what to do.
- Model containers get the card from the class's run arguments (`--device nvidia.com/gpu=all`,
  `config/settings_classes.rs:221`), which Podman resolves against the system's CDI dirs
  (`/etc/cdi`, `/var/run/cdi`). They are always created with `podman run -d --replace`
  (`runtime/argv.rs:843`), never restarted with `podman start`, and have no restart policy. The
  argv starts at the subcommand, so lmgw passes no Podman global option today.
- A model's first start pulls its engine image implicitly. `podman_run_argv` sets no `--pull`
  (`runtime/argv.rs:842`), so `podman run -d` in `start_container` (`runtime/registry/acquire.rs:281`)
  blocks for the whole download, and the waiter shows `WaitPhase::Starting` ("starting the
  container", `vram/stage_watch.rs:73`). By then the request holds the admission gate and its VRAM
  reservation (`vram/queue.rs:401` onwards). The Engines page's explicit pull (`backends/pull.rs:300`)
  is a job that reports podman's last output line, not bytes.
- lmgw already escalates once: the updater runs `pkexec dnf install -y /tmp/lmgw-update.rpm` under
  polkit's generic `org.freedesktop.policykit.exec` action (`lmgw-update/src/install.rs:73`,
  `src-tauri/src/updater.rs:155`), and `can_self_install` hides that path on hosts without `pkexec`
  and `dnf`. It finds `pkexec` through `PATH` (`Command::new("pkexec")`), and a user's `PATH`
  commonly puts `~/.local/bin` first (Fedora's default `~/.bashrc` does), so a program planted there
  could show a fake password window. The RPM carries
  no signature (rpm shows none on the 0.5.0 package), dnf5 checks no signature on a local file
  (`localpkg_gpgcheck = 0`, checked), and the file sits at a fixed name in `/tmp`. That is a wider
  root path than the helper below. WP0 fixes the `pkexec` path. The rest is outside this design.
- The shell already has commands that only the app's own window can call: they are listed in
  `src-tauri/build.rs`, granted in `capabilities/remote-ui.json`, and checked against the window's
  running document by `media::command_allowed` (`src-tauri/src/media.rs:214`). That check proves
  the document is the gateway's. It does not prove the gateway's own pages sent the call.
- **Finding F1.** An RPM built by CI (0.5.0) ships every file from the
  Tauri `files` map and every icon world-writable: `-rw-rw-rw- root root` for
  `/usr/share/licenses/lmgw/**` and `/usr/share/icons/hicolor/*/apps/lmgw.png`. The sources are
  0644 in git. tauri-cli 2.12.0's bundler (`rpm.rs`) sets no mode for the `files` map, the icons or
  the binary, so each file keeps the mode it has in the CI checkout. The CI's container executor clones with umask 0000, so checked-out files are 0666. `/usr/bin/lmgw` is 0755 because it is built in the
  job's own shell, not checked out. A root helper or polkit policy shipped the same way would be
  writable by every local user. §3.8 and WP0 fix this first.

## Decisions

*The owner, 2026-11-03:*
1. **lmgw may change the system to get the graphics card working for its model containers.** It
   asks for the password once (polkit), after a plain confirmation that says what it will do, with
   the exact commands visible behind a fold.
2. **Who it is for:** people who should never meet parameters, compiling or what a container is
   (gamers, Kai users, owners of an OpenAI-only app, a non-technical relative).
3. **One UI** (from the proposal's "Keeping it one UI", accepted with the proposal): depth
   changes what is open, never what exists. One label per field, the same at every depth.
4. **lmgw writes its own CDI spec.** lmgw generates the spec with rootless `nvidia-ctk cdi
   generate` into its own runtime dir, regenerates it when the driver version changes, and passes
   `--cdi-spec-dir` on its own podman calls. Root is needed only to install the toolkit (§3.6). The
   alternative, relying on NVIDIA's refresh units, was considered and not chosen (§3.6.6).

*Standing rules that bind this design:* no hidden or guessed limits (every bound is a real
quantity, shown); confirm, then do, never refuse (this is a single-user desktop app); a configured
fallback always applies, for every reason, with one fallback per model or alias; no config keys as
labels or in messages; a download never starts a model (2026-10-13).

*Draft calls (this record's own):*

- **D1 — One fixed root helper, one polkit action.** `/usr/libexec/lmgw/lmgw-machine-helper`,
  action `io.github.lmgw-dev.lmgw.machine-setup`. Defaults: `allow_active` = `auth_admin`,
  `allow_inactive` = `no`, `allow_any` = `no`. lmgw starts it as `/usr/bin/pkexec …`, by absolute
  path, never through `PATH`. Why: polkit's generic exec action (what the updater uses) shows "run
  /usr/bin/dnf as the super user", which says nothing to a newcomer, and it would authorise any
  program. A dedicated action shows lmgw's own message and authorises one binary. Why `auth_admin`
  and not `auth_admin_keep`: the whole fix is one helper run, so keeping the authorisation buys
  nothing, and it would leave a five-minute window in which another run needs no password. GameMode
  ships its helpers under `/usr/libexec` in the same way (`com.feralinteractive.GameMode.policy` on a stock Fedora install).
- **D2 — The helper takes no input but a verb and a plan hash, and hashes only what root can
  recompute.** It reads the machine itself, builds the plan with the same library code the check
  uses (§3.2), and runs only steps from a closed list. The hash covers a dedicated struct (step
  kind, exact argv, the full contents of the files it writes) and nothing that differs between the
  user and root or changes with wording or an in-app update. If root's plan differs, the helper
  prints its own plan and changes nothing. Why: pkexec does no validation of arguments (its man page
  says so), and "what you saw is what runs" is the only promise worth making behind a password.
- **D3 — The fix is started only by the app's own window.** It is a Tauri shell command (§4.3).
  It is not an `/api` route and not an MCP tool. Why: the lmgw process is always local. A route
  would let a browser on another machine, or an agent, pop a password prompt on this desktop.
  A remote browser session sees the check and the commands, and "open lmgw on this computer to set
  it up".
- **D4 — Debug builds, dev instances and tests never run the fix.** They can run the check, which
  only reads, and a dev instance writes its own spec into its own dir (§3.6), which needs no root.
  `state.dev()` covers only `LMGW_DEV` and the headless runner, so the shell also requires that the
  running program is `/usr/bin/lmgw` and that the data dir is the installed default. §3.4 lists the
  gates.
- **D5 — The toolkit comes from NVIDIA's own repository, unless a distribution repository already
  ships it.** Only `nvidia-container-toolkit-base` is installed. lmgw writes its own lmgw-named
  repository file, limited to that one package (`includepkgs`), with package checks on against
  NVIDIA's key pinned by fingerprint and embedded in the helper (§3.2). Why: no distribution
  repository on Fedora 44 ships the toolkit (checked 2026-11-03: neither `nvidia-container-toolkit`
  nor `golang-github-nvidia-container-toolkit` is in `fedora`, `updates` or RPM Fusion's repos, and negativo17's `nvidia` repository for Fedora 44 lists no container toolkit
  package). Fedora retired its Go package (bug 2434398, its Fedora 44 build failure, closed
  WONTFIX 2026-03-22: "This package has been retired and obsoleted by nvidia-container-toolkit"),
  and Fedora's own `nvidia-container-toolkit` is still in package review (bug 2450007, ASSIGNED
  with nobody working on it, 1.20.1-1.fc44 submitted, last activity 2026-09-19; it is to obsolete
  the Go package below 1.18). When it lands, a reviewed lmgw change adds it to the provider table and S4 installs
  from there (§3.2). The base package is enough for Podman with CDI: it owns `nvidia-ctk`,
  `nvidia-cdi-hook`, the refresh units and the udev rule, and requires only glibc (checked on
  1.20.1). NVIDIA's own repository file says `gpgcheck=0`, so a toolkit installed from that file is not signature-checked unless NVIDIA's key was imported.
  Its packages are signed anyway (header signature, key ID `ddcae044f796ecb0`, checked on 1.20.1),
  so lmgw turns package checks on.
- **D6 — superseded by ruling 4.** It said lmgw never writes a CDI spec and repairs by starting
  NVIDIA's refresh service. lmgw now writes its own (§3.6).
- **D7 — A hand-made spec in `/etc/cdi` is left alone.** The check names it in the detail. Why: it
  is the person's file, and moving files they made is not needed to get the card working. Under
  ruling 4, lmgw's NVIDIA containers do not read the system dirs at all (D21), so such a spec can
  neither help nor break them.
- **D8 — partly superseded by ruling 4.** Its second half, installing RPM Fusion's CUDA part
  (`xorg-x11-drv-nvidia-cuda`, which carries `nvidia-smi`), is gone: it existed only because
  NVIDIA's refresh service skips itself without `nvidia-smi`, and lmgw no longer uses that service.
  The first half stands: **lmgw does not install the NVIDIA driver itself, not yet.** The check says
  how. Why: it needs a third-party repository the distribution offers in its own way, a kernel module
  build, a restart, and on Secure Boot machines a key enrolment at the firmware screen.
- **D9 — superseded by ruling 4.** It said the spec going stale after an update is upstream's fault
  and lmgw only repairs it. lmgw now regenerates its own spec before a GPU start whenever the
  machine changed (D20). The upstream report is still an option for everyone's own containers
  (§3.6.6).
- **D10 — Packaging goes through Tauri's `files` map, after F1 is fixed and guarded.** The CI job stops cloning with umask 0000, and the build fails on any file in the RPM that is group- or
  world-writable, setuid, setgid or sticky, not root-owned, outside an allowlist of destinations, a
  non-regular file where the helper or policy belong, or a scriptlet (§3.8). The helper does not
  ship before that guard exists.
- **D11 — Elsewhere, instructions instead of a button.** The AppImage, distributions without
  `dnf`, and image-based systems get the same check with the commands to run, said plainly (§3.7).
  Under ruling 4 those commands only install the toolkit. The spec is lmgw's own on every system.
- **D12 — Where it shows.** The Overview's welcome checklist; the Chat, when local models are
  involved; a permanent card in Settings → GPU; the tray, when something that worked stops
  working; model-start errors (§2). No desktop notification yet.
- **D13 — When it runs.** At boot, on demand, after a GPU model start fails, and after a fix. No
  timer yet. Separately, before every GPU container start lmgw judges its own spec, which costs
  file stats only (§3.6.2).
- **D14 — Words.** Each item is a plain sentence. Every technical term goes into its detail fold.
  The download line says "Download for your graphics card · 1.2 of 3.4 GB". It does not say
  "one-time": the engine images get updates. The proposal's example said "Getting the engine for
  your graphics card ready", but the proposal also lists "engine" among the words that stay out of
  the essentials.
- **D15 — The first-start download shows bytes, and happens before admission** (WP10, WP11). A
  request whose model's image is missing pulls it after the route is resolved and before the VRAM
  gate: nothing is evicted and nothing reserved while it downloads, and the queue's time budget
  starts after it (§5). Byte counts come from Podman's own API (the libpod pull with `compatMode`),
  and the total comes from the registry manifest.
- **D16 — Getting a model also gets its engine, by default.** When a model is downloaded and its
  class's image is not on the machine yet, the download fetches the image too, shown as its own
  line, and nothing starts. Once the machine check turns `ready`, lmgw also pre-fetches the missing
  image of every class that has a GPU model. Why it is the default: an OpenAI-only app's client
  gives up on its own timeout, which lmgw cannot see, so a multi-gigabyte download on the request
  path fails for exactly the people this design is for. Pulling an image is not loading a model, so
  the 2026-10-13 ruling holds. The alternative: it widens what a model download does.
  Built (WP11): the model download's half (D37), and the pre-fetch (D72, D73, D99): the image of
  each class that has an enabled GPU row, as this call says, and nothing on a metered connection.
- **D17 — superseded by ruling 4.** It made every spec refresh ask for the password. There is no
  root refresh any more: lmgw rewrites its own spec without a password.
- **D18 — AMD gets a check and a settings fix, not a root action.** On an AMD card with the NVIDIA
  defaults in the class settings, lmgw offers to switch those classes to the Vulkan images and
  device arguments `docs/amd-arch.md` documents, after a confirmation that lists the fields. This
  is an ordinary settings write and needs no password (§1.4, WP12).
- **D19 — `polkit` joins the RPM's `depends`.** pkexec is in it, and both the updater and the helper
  need it.
- **D20 — lmgw regenerates its spec on any package change, not only a driver version change.**
  Ruling 4 says "when the driver version changes". An `egl-wayland` update that changed no driver version broke the spec, so lmgw also regenerates when the rpm database changed
  since the last generation, or when a path the spec names is gone (§3.6.2). Generating takes 0.38 s
  (measured) and needs no password, so a needless run costs little.
- **D21 — lmgw's NVIDIA containers read lmgw's spec dir alone.** `--cdi-spec-dir` replaces Podman's
  default dirs (verified, §3.6.3), so a stale `/etc/cdi` copy, NVIDIA's own `/var/run/cdi` spec and
  any same-dir conflict between system specs cannot reach them. A row whose run arguments name a CDI
  device of another kind gets the system dirs first and lmgw's last.
- **D22 — The root steps run as a fixed system task, under a shutdown inhibitor.** The helper hands
  its steps to a transient system unit, `lmgw-machine-setup.service`, so quitting lmgw or logging
  out does not kill `dnf` mid-transaction, and the desktop's shutdown says that a setup is running
  first (§3.1). The premise, that a killed `systemd-run --wait --pipe` leaves its unit running,
  held on the user manager (2026-11-03, see "Unverified"); the system manager is the live round's.
- **D23 — A configured fallback answers a `gpu_setup` failure.** The model or alias's one fallback
  applies here as for every other reason, with `x-lmgw-fallback-reason: gpu_setup`. While the last
  check names a driver or toolkit cause, a GPU hop that has a fallback is swapped at resolve time,
  like under the hold, so no start that is bound to fail is attempted (§2.5; which causes, D95;
  never for a pinned caller, D97).
- **D24 — The toolkit is judged by who owns `nvidia-ctk` and whether it works.** Every toolkit step
  keys on `rpm -qf /usr/bin/nvidia-ctk`. Fedora's retired Go package gets a confirmed swap to
  NVIDIA's package. Any other owner, or none, is used as long as it works, and gets instructions
  when it does not. There is no version floor: lmgw needs `nvidia-ctk cdi generate` and
  `nvidia-cdi-hook`, and a toolkit that writes a spec whose paths all exist is good enough (§1.2).
- **D25 — The confirmation is the shell's own dialog.** The page shows the plan and the commands
  fold. "Set it up" opens a native dialog drawn by the shell from the plan it computed itself, and
  only a click there starts the helper. A script running at the gateway's origin cannot skip it
  (§4.5). Nor can it choose when the dialog appears: it shows only over the app's window in front
  (D133), its "Set it up" waits a second with the focus (D134), and a document that said "Not
  now" is not asked again until it is replaced (D140).
- **D26 — Where lmgw cannot write its own spec, Podman's default dirs, as before; where it cannot
  read the driver's version, it writes anyway.** Without `/usr/bin/nvidia-ctk`, or with a Podman
  whose `podman --help` lists no `--cdi-spec-dir` (asked once per instance), lmgw writes no spec
  and passes no option: its GPU containers read `/etc/cdi` and `/var/run/cdi`, which is how every
  GPU start worked before ruling 4. The log says why once per change, and `OwnSpec::default_dirs`
  holds the reason for the check (WP4), whose 1.2 reports it. When the userspace version cannot be
  read (`libnvidia-ml.so.1` a regular file or an unversioned name, or in none of the dirs lmgw
  looks in), lmgw generates anyway and lets nvidia-ctk's own NVML start report a real mismatch;
  only versions that differ and a kernel module that is not loaded still stop it. Why: ruling 4
  must not turn a machine whose system spec worked into one where every GPU start fails, and a
  version lmgw cannot read is no evidence of a mismatch. Alternative not taken yet: on those machines ruling 4's
  guarantee does not hold, and a stale system spec can break GPU starts again.
- **D27 — A GPU container's restart policy is left out, with a note.** When a row's or a class's
  run arguments give the container an NVIDIA GPU and carry `--restart` (anything but `no` or
  `never`) and lmgw points Podman at its own spec dir, lmgw renders the container without it (with
  Podman's default dirs (D26) or no managed spec the policy stays: the container reads the
  system's spec on every start anyway), and the start's warnings (and a chat row's checks) name the
  flag and say why: Podman restarts a container from conmon without `--cdi-spec-dir`, so a
  restarted container would read the system's spec (§3.6.3). Left out rather than refused, because
  a row saved with it keeps starting, and lmgw starts a model whose container ended again on its
  next request, which is what the policy was for. Alternative not taken yet: refusing such a row when it is
  saved instead.
*Draft calls made while building WP10 (2026-11-03):*

- **D28 — The download wait is a list of its own.** A start waiting for its engine image is not an
  admission-queue entry: the vram view lists it under `downloading` (position 0, needs 0, phase
  `downloading` with its bytes), so it holds no place in the queue and nobody behind it waits for
  it. Why: nothing is decided about the card until the image is here.
- **D29 — `--pull=never` on every start that downloaded first.** The registry's model starts and a
  benchmark's container launch run `podman run --pull=never`, so an image that vanished in between
  fails plainly instead of downloading inside the gate or the lease. The rendered command line (the
  benchmark's plan and its stored line) keeps podman's default, so a copy of it pulls what it needs.
- **D30 — Every start path downloads first.** A request's admission (on each pass, so a start over
  after a failed climb too), the starts nobody waits on (boot and realtime warm starts,
  `lmgw__container start`, group apply), and a benchmark run, which downloads before it takes its
  GPU lease, so the download neither holds the card nor is timed as the load. A missing image a
  registry has no longer blocks `bench_plan` or refuses `bench_start`; a missing `localhost/` build
  still does. While a wait runs, the GPU hold or a benchmark's lease ends it (`gpu_hold`, or
  `Fit::Held` for a start nobody waits on), and for a request with a fallback the outside-VRAM
  verdict is taken again at the queue's poll interval, so the fallback answers when the room turns
  into VRAM outside lmgw's control. The download goes on either way. A wait that ran to its end
  reads the snapshot and the row again before it starts anything (`Fit::Downloaded` for the starts
  nobody waits on), since the owner may have edited or deleted the row meanwhile.
- **D31 — Background traffic never waits for a download.** A guest's start (`x-lmgw-background:
  on`, a background candidate alias) begins the download or joins it and is blocked at once: "its
  engine image <image> is still downloading (x of y)". The usual background answer follows, the
  loaded alternates, the fallback or `503 gpu_deferred`, and a later guest starts the model once the
  image is here. Why: the standing rule that background requests never wait in ways that disturb others.
- **D32 — A failed download is its own verdict.** A start nobody waits on gets `Fit::Failed` with
  the sentence a request gets as its 502: the model, the image, Podman's reason, and the setting the
  image comes from (Settings → Runtimes → <class> → Image, or the model's own).
- **D33 — Which references a start pulls.** A start pulls digest-pinned references too (an
  Engines-page pull still refuses them: there is no update to pull). A `localhost/` image is a
  build on this machine and is never pulled ("run its build again on the Engines page"). A
  `localhost:<port>/` image is on a registry and is pulled like any other.
- **D34 — The API pull asks the registry every time** (`policy=always`), as `podman pull` does.
- **D35 — The private service's two bounds are visible.** It exits after 60 s idle
  (`--time=60`) and is started again on demand. A service that does not answer within 30 s,
  starting or already running, makes the pull use the `podman pull` command, and its job's size note
  and the log say why. A running service's socket is never removed: only a socket nothing listens
  on is replaced.
- **D36 — superseded by D69 (WP5).** It said a request whose engine download fails gets the 502
  even with a fallback configured, and left whether a fallback answers it to WP5.

*Draft calls made while building WP11 (2026-11-03):*

- **D37 — A model download fetches its class's image, on every model download, and says so in a
  field.** `hf_add`, `image_recipe_add` and the audio catalog's download ask `podman image exists`
  for the Image setting of the class they download into (`web/hf/engine.rs`) and, when it is
  missing, begin its `image_pull` job (`pull::begin`, so a first start or an Engines-page pull
  joins it); nothing waits and nothing starts. Also when no file was queued (every component
  already on disk, a package already installed): getting the model is what was asked. The
  class's image, not a row's own: a download creates no row, and the row it becomes runs in the
  class's image unless its owner names another. The answer carries `engine` (`EngineDownload`:
  the image, `state` — `downloading` with its job, `refused` with why it could not begin (a
  `localhost/` build is never pulled), or `not_known` with why podman could not say whether the
  image is here); the op's `message` stays the files' own, since the dashboard toasts it and the
  image belongs in no plain sentence. An image podman cannot say anything about is left alone, as
  a start leaves it, and the answer says so rather than reading as "the image is here" (D42).
- **D38 — One rendering of the download, everywhere a waiting start or a model download shows**
  (`gpu_download.rs`): the sentence of D14 with its bytes, a bar, and a tooltip with the image
  reference and, when the size is not known, why (the pull job's `size_note`, now also on
  `WaitPhase::Downloading`, the media `progress` frame and the chat turn's `state` frame). The
  bytes are said in the total's unit, binary, through the one shared formatter
  (`lmgw_api_types::units`, owner ruling 2026-11-04 and find-a-model D160: one decimal below 100
  of a unit, whole from 100, `B` below 1 KiB; "1.2 of 3.4 GiB", "120 of 308 MiB"), the unit picked
  by the rounded value (1023.97 MiB is "1.0 GiB"), the total rounded and the bytes done rounded
  down, so the line never says the total while bytes are missing ("3.3 of 3.4 GiB" with 1% left);
  a tiny total is said in KiB or bytes. Without a total, "1.2 GiB so far" and an indeterminate
  bar, never a made-up fraction; before any byte, the sentence alone, "size not known yet" in the
  tooltip. Once every byte is in while the job still runs, Podman unpacks the layers, which it
  does not count: the line says "unpacking 3.4 GiB" and the bar is indeterminate again rather than
  standing full. A state chip says the same in a shorter form beside "ready" and "stopped":
  "graphics card download · 1.2 of 3.4 GB" (D39).
- **D39 — Where a model's state shows, a start waiting for its download is its own state.** The
  Overview lists these starts under a heading of their own, "Downloading before they start" with
  its count, shown only while there is one, each with the download's line, bytes and bar and no
  Logs, Start or Stop, since there is no container yet; they count neither as running nor as
  stopped (an earlier pass listed them under Running models, which the GPU popover's "Running local
  models: None" contradicted). The Models list, the model editors' container row and the Audio and
  Image labs show the chip "graphics card download · 1.2 of 3.4 GB", D14's words in a chip's short
  form (an earlier pass said "downloading · 1.2 of 3.4 GB", which read as the model's own file
  downloading). The GPU popover and Traffic's GPU panel list the waiting starts apart from the
  admission queue ("Downloading before they start"; "1 start waiting for its download"), and
  Traffic's GPU line counts them.
- **D40 — A chat turn says its download.** The turn follows its own admission
  (`stage_watch::follow`, as a media call does) and says each step of the download as a `state`
  frame `downloading` (`ModelState` gains `image`, `done_bytes`, `total_bytes`, `size_note`),
  shown on the composer's status line with its bar; the download's end sends the follower the
  phase `starting` (`vram/download.rs`), also when admission says no phase of its own after it
  (switched off, nothing to measure, a CPU start, a model another request brought up), so the
  turn says `loading` again there, the media tool card "Loading <model>", and the `ready` time
  counts from there. The client never holds up an admission: while it runs, the frames it causes
  wait for room on the turn's channel in one slot beside it (the newest replaces one still
  waiting, and one still waiting when admission returns is dropped), so a client that stopped
  reading (an app sent to the background with its socket open) cannot leave the admission parked
  on a full channel with the gate held; what admission settled on (`ready`, `fallback`, `held`,
  `failed`) is sent after it returned, as before. A candidate alias says nothing up front, since
  its pick may be a model that is up, but a download its pick waits for is said like any turn's,
  then `loading` and `ready`. The composer drops a turn's `downloading` or `loading` note when its
  stream ends, done, failed or stopped: a Stop mid-download sends no `state` frame after it.
- **D41 — The Downloads page lists every download for the graphics card that runs now**, D16's,
  a start's and an Engines-page pull's alike, above its files, each saying who it is for (D43), and
  the sidebar's Downloads count and the page header's "downloading" count include them; the image
  recipe and audio catalog browsers show the same lines. The download wizard follows its own
  model's, reading the job's row every 2 s while the jobs feed does not list it (so a pull that
  ended before the feed ever listed it is said too): while it runs, then "done"; "stopped — it
  tries again the first time this model is used" with the reason in the tooltip; "can't be
  downloaded, so this model can't start yet" for a pull refused before it began; "could not check
  whether it is needed — the first use of this model checks again" when podman could not say
  (D42); and "could not tell how it ended — the first use of this model checks again" when the
  row could not be read. The Engines page's pull chip and its pull panel show the same bytes for
  an explicit pull.

*Draft calls made in WP11's review round (2026-11-03):*

- **D42 — A model download's question whether the image is here is bounded, and a "not known" is
  said.** `podman image exists` gets the 30 s a pull gives Podman's private service to answer
  (`SERVICE_START`, D35), for the same reason: the question takes no time, and a podman waiting
  that long (on its storage lock) is wedged. Past the bound, or when podman fails to say, nothing
  is fetched, as a start leaves such an image alone, and `engine` says `not_known` with why and
  the bound, logged as a warning; the model's first start looks again. Why: a hung podman would
  otherwise leave the download's files queued and its answer never arriving.
- **D43 — A download for the graphics card says who it is for.** On the Downloads page and the
  catalog browsers each line names the starts waiting for it ("for qwen-small", the alias each was
  asked for), else the classes whose Image setting names it, in plain words: "for chat models",
  "for search models" (the aux class: embeddings, rerank, decisions), "for voice", "for pictures",
  joined ("for chat models and search models" when both run in one image). An Engines-page pull of
  an image no class or waiting start names says no one. The image reference stays in the
  tooltip.

*Draft calls made while building WP2 and WP4 (2026-11-03), and in their review round the same day:*

- **D44 — Root accepts the plan for either answer to the one fact it cannot read.** S4′'s condition,
  "lmgw's spec generation failed", is lmgw's own state, not something root can recompute (D2). It
  is one flag, and S4′ only updates the toolkit's own packages, scoped to one section, so root
  computes the plan for both of its values and accepts either hash
  (`lmgw_machine::plan::accepted_hashes`). Built in WP2; the helper (WP6) uses it.
- **D45 — S2 is the key file, its import, or both.** lmgw's `<scope>` points `gpgkey` at lmgw's key
  file, and lmgw's own section checks its metadata against it, so the file has to be there for every
  dnf step, even where rpm already trusts NVIDIA's key (as on a typical Fedora install). S2 writes the file when
  it is not there with this lmgw's key, and imports it only while `rpmkeys --list` lacks the
  fingerprint. A `rpmkeys --list` that fails blocks the plan rather than guessing.
- **D46 — lmgw's key file carries no marker.** An armored key has no comment line lmgw could add
  without changing NVIDIA's bytes, so a file at the key's path is lmgw's when it holds one of the key
  texts lmgw has ever embedded (`plan::embedded::OWN_KEYS`, one today), and is replaced when it is
  not the current one. Anything else there is never overwritten (the plan is blocked and says so).
- **D47 — Which NVIDIA section the plan uses.** Only a section of NVIDIA's stable channel
  (`…/libnvidia-container/stable/`) that can serve the package (its `includepkgs`, if any, lets it
  through) counts; an experimental-only machine gets lmgw's own section. Order: a provider-table
  repository, the person's enabled section, lmgw's own enabled one, the person's switched-off one
  (S1′), lmgw's own switched-off one (S1′), else S1. The person's comes before lmgw's own because it
  also serves `nvidia-container-toolkit`, which S4′ updates with `-base`. A provider-table section's
  `<scope>` keeps the distribution's own keys (no `gpgcheck`/`gpgkey` options).
- **D48 — The button needs dnf5, and the steps run dnf5 itself.** The steps are dnf5's
  (`config-manager setopt`, `--repo`), so a system with an older dnf gets instructions pointing at
  NVIDIA's guide, as one without dnf does. The check offers the button only where `/usr/bin/dnf5`
  is there, and every step runs `/usr/bin/dnf5` by that path (the typed lines say `sudo dnf5`), so
  what is checked is what runs; `/usr/bin/dnf` is read only to tell an older dnf from none. On an
  image-based system the plan uses enabled sections only (rpm-ostree reads no dnf5 override and S1′
  is a dnf5 command), writes the key file without importing it, and installs with `rpm-ostree
  install`, followed by `systemctl reboot` in the typed lines.
- **D49 — The NVML probe starts `/proc/self/exe --machine-probe nvml`.** That is the running
  program's own image, which can still be started after an update replaced `/usr/bin/lmgw` on
  disk; it is bounded by the load timeout (Settings → GPU → Admission → Load timeout) like the spec
  write, and a run that outlives it says so in the person's terms ("the graphics card check (lmgw
  --machine-probe nvml) did not finish within 600 s, the load timeout (Settings → GPU → Admission →
  Load timeout)"), as do the check's `dnf repoquery` and `nvidia-ctk --version`. The tray app and
  the headless runner answer the flag first thing in `main`, before the single-instance plugin
  could hand it to the running window and before the headless runner's async runtime starts. The
  grammar is exact: `--machine-probe nvml`, and the flag with anything else is a usage error
  (exit 2), never an ordinary start.
- **D50 — Verdicts the tables leave open.** Secure Boot keeping the module from loading is
  `needs_steps` (a restart does not fix it). NVML failing while the versions agree is `needs_steps`,
  except NVML's own version-mismatch error, which is `needs_restart`. A problem only instructions
  fix outweighs one the button fixes (`needs_steps` over `needs_fix`), and a restart comes before
  both. The headline of a `needs_*` verdict other than `needs_driver` is the sentence of the item
  that decided it: the card's for a card verdict, else the first problem whose fix is a restart or
  instructions (1.2's, then 1.3's, then 1.4's), so a `needs_steps` headline never offers the button.
  A Podman whose `--help` lists no `--cdi-spec-dir` is a 1.3 problem as §1.3 says, although D26
  keeps such a machine working.
- **D51 — Settings that do not fit are `needs_steps` until WP12.** §1.4's sentence for NVIDIA
  settings on an AMD card says "This card needs the settings for AMD cards." instead of "It can
  switch…", and the detail points at `docs/amd-arch.md`, because the switch (WP12) is not built
  yet. For AMD settings on an NVIDIA card the detail gives the values lmgw ships for NVIDIA cards,
  per class and by its Settings labels. How a settings fix appears in the plan's types is not
  decided yet (an earlier pass's unused `how: settings` and `settings_fit` step kind were taken out
  again). Only `/dev/kfd` (ROCm) counts as AMD's settings: `/dev/dri` alone is what a Vulkan engine
  passes on any vendor's card (AMD's Vulkan setting in `docs/amd-arch.md`, Intel's), so it fits an
  AMD card and is a detail line on an NVIDIA card, where it lacks NVIDIA's libraries. WP12 built
  the switch: its verdict and sentences are D110's now, and the render nodes' reading stays (on an
  NVIDIA card with an AMD card beside it, the render nodes fit that card, D137).
- **D52 — The advisory `repoquery` is one cache-only run over the sections the user's cache has.**
  dnf5 refuses a cache-only query outright when any enabled section has no cache (`Cache-only
  enabled but no cache for repository "fedora"`, exit 1, checked 2026-11-03), and `--whatprovides
  /usr/bin/nvidia-ctk` answered nothing even for NVIDIA's section from the user's cache. So the
  check asks once, `/usr/bin/dnf5 repoquery -C` with `--repo=` for each enabled section whose cache
  dir is in the user's `~/.cache/libdnf5`, for the toolkit's package names (`-base`, the full
  toolkit, the retired one), within the load timeout; a failure's whole stderr goes into the
  detail. The planned package's row from the planned section gives the download size (8.5 MB for `-base` 1.20.1 from NVIDIA's section, in a measurement).
- **D53 — A plan step also carries `terminal`**, the same step as lines to type with `sudo`: a
  file as a quoted `sudo tee <path> > /dev/null <<'EOF'` here-document, so `$basearch` stays
  literal. The instructions view of §3.7 and the commands fold need nothing else to render it.
- **D54 — The frozen-section reading compares when dnf last loaded a section, by dnf5's own
  clock.** That clock is the mtime of each section's primary metadata file (the `<data
  type="primary">` location in its `repomd.xml`): libdnf5's `Repo::get_age` (`repo/repo.cpp`) is
  now minus that time, and when metadata past its `metadata_expire` is still in sync with the
  server, `RepoSack::Impl::update_and_load_repos` (`repo/repo_sack.cpp`, its
  `load_downloaded_repo`) sets the file's times to now with `utimes(primary, nullptr)` (read on
  dnf5's `main`, 2026-11-03). So it moves only when dnf loaded the section, fresh or confirmed.
  `repomd.xml`'s time is when a source last published (`repomd.xml`'s time can be the release day while the primary file is recent), and a cache dir's own time moves for
  any entry made or removed in it (many cache dirs share one mtime): the
  first build of this reading used the dir's time and was corrected in review. A cache dir
  without a primary file its `repomd.xml` names is one dnf does not load either, and is left out.
- **D55 — S4′ moves NVIDIA's installed toolkit packages together.** `nvidia-container-toolkit` pins
  `-base`'s exact version, and it and `libnvidia-container-tools` move with
  `libnvidia-container1`. So S4′'s package list is the owner, then each of those four that `rpm -q`
  finds installed (`plan::embedded::TOOLKIT_FAMILY`, an input root reads the same way). lmgw's own
  section lets only `-base` through in its file; S4′ through it appends the other installed ones for
  that one command (`--setopt=<id>.includepkgs=<package>`, which dnf5 appends to the file's list:
  `includepkgs` is an `OptionStringAppendList` in `libdnf5/repo/config_repo.cpp`), and the file
  itself stays as it is. A person's section whose own `includepkgs` keeps one of them out blocks the
  plan with that reason (`Blocker::NotOffered`) instead of a command dnf would refuse. Alternative not taken yet:
  writing the four into lmgw's repository file instead, so ordinary system updates move them too.
- **D56 — The toolkit from another repository is one scoped install, shown as instructions.** When
  the user's cached metadata says an enabled repository outside the provider table and NVIDIA's
  sections offers the toolkit (`-base` preferred, else the full toolkit), the plan is that install
  alone (`lmgw_machine::plan::plan_from_other`): `dnf5 install -y --repo=<id>
  --setopt=<id>.skip_if_unavailable=False --setopt=install_weak_deps=False <package>`, with the
  system's own signature checks, and no S1 or S2; on an image-based system `rpm-ostree install
  <package>`. The cause is `toolkit_other_provider`. It is never the button: root does not read the
  user's metadata, so its plan never has this step, and the helper would refuse the hash.
- **D57 — A failed spec write is blamed on what failed.** Only the toolkit's own failures (`cdi
  generate` failing, a spec naming paths that are not there) offer S4′. A write that ran out of the
  load timeout is the card's (NVML hung, a card off the bus): `needs_restart`, cause
  `spec_write_timed_out`, the load timeout's setting in the detail, and no plan. A folder of lmgw's
  that is not private (`spec_dir_not_private`) and lmgw's own files failing (`spec_files_failed`)
  are their own causes, plain sentences and no plan. A write that waited for another one holding the
  lock (a warm start at boot), a machine that changed mid-write and a blocked write are nobody's to
  fix: no problem, and the next start or check decides.
- **D58 — Secure Boot is blamed only for a module built before this boot.** With the driver
  installed and its module not loaded, Secure Boot on is the cause only when an `nvidia.ko*` is
  built for the running kernel (in `/lib/modules/<release>/`: `extra/nvidia`, `extra`,
  `updates/dkms`, `updates`, `kernel/drivers/video`) and this boot (`/proc/stat`'s `btime`)
  started after the module was put there. "Put there" is the
  file's change time: rpm sets its modification time to the package's (rpm sets the module's mtime to the package build time, not the install time). With no module built yet the sentence says it may
  still be getting ready (akmods builds after a kernel or driver update), and one built after this
  boot needs the restart. Secure Boot's state is a detail line either way.
- **D59 — An unreadable library version is decided by NVML.** With the module running and the
  libraries' version not readable from the file `libnvidia-ml.so.1` points at (D26's machines), the
  check asks NVML anyway, its `version_mismatch` decides between ready and `needs_restart`, the
  unreadable version is a detail line, and the boot run writes lmgw's spec as on any working card.
- **D60 — Root reads without its inherited environment.** `SystemFacts::for_root` starts every
  program from an empty environment with only `HOME=/root`, `PATH=/usr/sbin:/usr/bin` and
  `LC_ALL=C.UTF-8`, and its `env` answers from that list: rpm reads `$HOME/.rpmmacros`, whose
  `%_dbpath` could point root's queries at another database. The plan module's docs say the helper
  (WP6) must read through it, and since WP6 the helper's `main` does; since WP6's review round the
  check reads the plan's inputs through the same environment too (D87), and root's reads run in
  `/` (D85).
- **D61 — The boot run publishes once, after the spec write.** It holds the one-run lock for the
  check, lmgw's spec write and the check again, and publishes only the last result, so the status
  never says "ready" for a spec still being written. A fresh request in the meantime waits (the
  write is bounded by the load timeout) and takes that result. Generally a fresh request takes a
  published result only from a run that began reading after it asked.
- **D62 — The download's size is counted before the download.** For each engine image the
  settings name that `podman image exists` does not find (WP10's own question, with the image after
  `--`), the check reads the registry manifest for this machine's platform anonymously
  (`RegistryClient::platform_blobs`, WP10's read, bounded by that client's own request timeout) and
  counts its compressed blobs less those the image store has already: the `compressed-diff-digest`
  of each layer in containers/storage's `<graphRoot>/<driver>-layers/layers.json` (and
  `volatile-layers.json`), each blob once over all missing images. That is the figure WP10's pull
  totals against ("Already exists" leaves it). An image whose download runs counts its job's own
  bytes still to come (`pull::job_bytes`). A manifest that cannot be read, or a running download
  whose total is not known yet, leaves the sentence without a size and says why in that image's
  detail; a layer list that cannot be read counts every blob and says so. 1.5's low-space line
  compares that figure with the free space of the image store's file system.

*Draft calls made while building WP5 and WP11's pre-fetch (2026-11-03):*

- **D63 — Every start path asks the check after a failed start on the card.** The registry's
  "the container never came up" (`RuntimeError::Start`, which also carries a spec write that
  failed) of a row placed on the GPU is blamed by the check (`vram::start_error`,
  `machine::after_failed_start`, D96): a request's admission (a dead container's restart
  included), a join of a start in flight, a guest's start, the operator's start, and boot's and
  realtime's warm starts, so the status (and WP9's tray) learns of it whoever started the model. A
  row on the processor, a stop, a climb or a benchmark's lease is not checked. A test state's
  blank machine reads "no graphics card", so a start that fails there is never blamed on the
  machine (`MachineChecks::stated`). Revised in WP5's review round: each failure first ran a whole
  fresh check before the start answered (D96 says why not).
- **D64 — Which items name a start's failure, for which rows** (`machine/blame.rs`). By what the
  row's run arguments give its container, read as §1.4 reads them: a row given the NVIDIA card
  (`nvidia.com/gpu=…`, `--gpus`) is blamed on §1.1's NVIDIA causes and on §1.2; a row given AMD's
  compute node (`/dev/kfd`, ROCm) on §1.1's AMD cause (`amd_no_compute_node`); a row given the
  render nodes alone (`/dev/dri`, a Vulkan engine on any vendor's card) only when there are no
  render nodes; no card at all (`no_card`) is every such row's; a row of another CDI kind, or with
  no device, is not blamed. So a Vulkan row on an Intel card's render nodes, or beside an NVIDIA
  driver that waits for a restart, keeps its own error and is never swapped. An item that is only
  information (a retired toolkit that works, a source that stopped loading) is none. Revised in
  WP5's review round: §1.1 first named every GPU row, so an Intel or AMD Vulkan row was swapped
  from boot and never tried.
- **D65 — The words of `gpu_setup`.** "'<model>' could not start on the graphics card of the
  computer lmgw runs on." then the item's sentence, and "Open lmgw there to see what to do." only
  when the item has a plan or steps to show there: the driver's install or its missing part, the
  Secure Boot key enrolment's steps, the toolkit's plan, the folder or the files the details name
  (not a restart, a wait, an AMD card's compute node, NVML or a driver lmgw could not read). No
  card at all has its own words: "'<model>' could not start: no graphics card was found on the
  computer lmgw runs on. Cloud models still work." Then the start's own error in parentheses
  ("its start said: …": the registry's message and the container's last log lines as per-model
  containers §3.6 reads them, Podman's text included; `gpu_setup` cuts nothing of it) and any
  clause saying why a configured fallback did not answer. `api_error` in
  both dialects (never `overloaded_error`). Revised in WP5's review round: the first words said
  "Open lmgw on this computer to repair it." after every item, which promised a repair lmgw
  cannot make (a restart, Secure Boot, a folder that is not private) and named the wrong computer
  for a Kai or phone client.
- **D66 — The resolve-time swap trusts a check only while the machine is as it read it.** Each
  run reads lmgw-machine's `Current` (the driver's two halves, the NVML library file,
  `nvidia-ctk`, the rpm database's last change, the `/dev/nvidia*` nodes, the boot ID; file
  reads, no program) before anything else; at resolve the swap reads it again and swaps only
  when they are equal, and only for a cause such a change would clear (D95). A machine that
  changed (a driver or the toolkit installed in a terminal, a restart) gets its start tried, and a
  failure is blamed again. A model that is up is never swapped (its container works), nor a row on
  the processor.
- **D67 — A swap whose fallback cannot answer tries the hop.** Unlike the hold's stop, the
  machine check's lets its hop be tried when the fallback is none, unusable, met already in
  the chain, or not for this endpoint (`Stop::tries_hop`), because the verdict may be out of
  date; the start, if it fails, answers `gpu_setup` with the clause naming the fallback.
- **D68 — A GPU start that brings a model up while the last check said it could not starts a
  fresh check** in the background (`machine::after_start`, one waiting at a time as in D96), so
  the status, the tray and the next resolve read the machine as it is. Only a start that brought
  the model up: the operator's start of a model already up (revised in WP5's review round) says
  nothing new and checks nothing.
- **D69 — A failed engine download hands on to a configured fallback, reason
  `engine_download`, and its 502 has its own code, `engine_download`** (WP10 answered `upstream`;
  the status stays WP10's). Its own reason, not `gpu_setup`: the card can be fine while a
  registry or the network fails, and a client and the log tell the two apart. Its sentence
  (reworded in WP5's review round, from "…: <image>, the image it runs in, is not on this
  machine, and …"): "The engine <class> model '<model>' runs in could not be downloaded, so the
  model could not start: <why> (engine <reference>). The engine is chosen in <the class's Image
  setting>." (or "It is the model's own engine, chosen on the model."), why in the pull job's or
  Podman's own words. A start nobody waits on keeps `Fit::Failed` with the sentence. Not swapped
  at resolve: the next start downloads again.
- **D70 — A candidate alias's primary hands on to the alias's own fallback** for `gpu_setup` and
  `engine_download`, in the owner's walk (its own start and a start it joined) and in the
  guest walk (where the reason is the primary's, not `background`).
- **D71 — A plain model's guest hands on too.** Background traffic for a model whose start fails
  for the machine or its download gets the row's fallback with that reason, as the owner's
  request does; before WP5 a guest's failed start was the answer as it was.
- **D72 — The pre-fetch fetches the engine of each class that has a model on the graphics
  card**: the class's Image setting, for each class with an enabled row placed on the GPU, each
  once, as D16 says. A class whose rows are all on the processor, a disabled row and a class
  without rows add nothing, and a row's own image (an override named on the row) is not fetched
  yet: their first start downloads. A `localhost/` build is never pulled (the log says why), and
  an image podman cannot say anything about (bounded as D42) is left alone and logged. Revised in
  WP5's review round: the first build fetched every enabled row's image, rows on the processor
  included, which widened D16 beyond its ruling.
- **D73 — The pre-fetch begins when the published verdict turns `ready`**: the first result
  (the boot run's) or one after another verdict; `ready` again begins nothing. It runs in the
  background (nothing waits for it, nothing is started) and logs each pull it began with its
  job.
- **D74 — Starts nobody waits on say the check's sentence in their own words.** The operator's
  start answers "starting '<model>' (chat) failed: '<model>' could not start on the graphics card
  of the computer lmgw runs on. …", and boot's warm start logs it; none of them falls back (they
  have no client to answer). A realtime or read-aloud warm says the fallback that answers (D98).

*Draft calls made while building WP6 (2026-11-03):*

- **D75 — The helper's lines and its exit 6.** It prints one JSON event per line
  (`lmgw_machine::helper::Event`, §3.1), the same type the shell parses. `plan_changed` carries the
  helper's lmgw version beside the plan schema, and "lmgw was updated" is either differing: the
  schema alone misses an update that changed the plan's code or its embedded text but not the
  hashed layout, and the shell would then show a "new" plan it recomputes the old way, and loop.
  A system task that could not be started, or that ended without the helper's own code (killed,
  the inhibitor missing, or an exit code `run`'s own last line does not say, D102), is exit 6, with
  systemd-run's own words as `note` lines and the `task_failed` line naming what `run` said; the
  first draft had no code for it.
- **D76 — `apply` reads the unit's output itself.** `--pipe` hands the unit `apply`'s pipes, and
  `apply` passes `run`'s lines on as they come, with anything that is not an event (systemd-run's
  and the inhibitor's own words) as a `note`. So lmgw's pipe carries events only, and `apply` knows
  whether `run` began, which tells exit 5 (another start won the unit's name) from exit 6, and what
  `run` said last, which its exit code must agree with (D102). It reads until systemd-run has
  ended, not until the pipes close (D101).
- **D77 — Root in both IDs.** `apply` and `run` refuse unless the real and the effective UID are 0
  (the first draft said effective), so a setuid copy with a forged `PKEXEC_UID` cannot pass, and
  `PKEXEC_UID` must be a user ID (decimal digits within 32 bits), not just set. `apply`'s `begun`
  line and its journal entry name that user (D82).
- **D78 — The helper runs dnf5 plans only.** A plan for an image-based system (`rpm-ostree
  install`), an older dnf or no dnf is refused with exit 4 even when its hash matches: those
  systems get commands to type (§3.7), and the helper runs what the button offers, nothing else.
- **D79 — A plan without steps starts no unit.** `apply` answers `nothing_to_do` with exit 0.
- **D80 — Files go through a temporary file in their directory.** The file is written, given its
  mode and synced under a `.lmgw-*.tmp` name, then linked to its path (a new file: `link(2)`
  fails on anything there) or renamed over lmgw's own (§3.2). The first draft created the file at
  its path with `O_CREAT|O_EXCL`, which a crash mid-write leaves half-written; a half-written key
  file is no key lmgw knows, so every later plan would be blocked as "not written by lmgw" until
  the person removed a root file by hand.
- **D81 — `apply` asks systemd before and after.** `systemctl show --property=ActiveState --value
  lmgw-machine-setup.service`: `active`, `activating`, `deactivating`, `reloading` and
  `refreshing` count as running (exit 5, `lmgw_machine::helper::unit_running`, which WP7 can use
  to follow a unit). A systemctl that cannot answer is a `note` and `apply` goes on, since systemd
  refuses a second unit of the name anyway; when systemd-run then fails before `run` began,
  `apply` asks again, and a unit that runs now is exit 5.
- **D82 — The journal gets every event of `apply` and `run` once the caller check passed.** Through
  journald's native socket, with `SYSLOG_IDENTIFIER=lmgw-machine-setup`, a readable message and the
  JSON line as `LMGW_EVENT`; `run`'s entries are the unit's (`journalctl -u lmgw-machine-setup`),
  `apply`'s are in the session that started it (`journalctl -t lmgw-machine-setup`), its `begun`
  with the `PKEXEC_UID` that asked. A journal that refuses a line is said once as a `note` on
  stdout. `plan`, a refused grammar and a refused caller journal nothing: anyone can start the
  program, and the review found the first draft journaling `begun` before the check. Anyone can
  also write an entry with that identifier and any `LMGW_EVENT` through the same socket, so a
  reader (WP7's "how did it end") takes only entries whose trusted fields, the ones journald sets
  from the sender's credentials and cgroup, say root inside the unit:
  `_SYSTEMD_UNIT=lmgw-machine-setup.service` and `_UID=0`, never `SYSLOG_IDENTIFIER` or
  `LMGW_EVENT` alone (`apply`'s, outside the unit, are `_UID=0` with
  `_EXE=/usr/libexec/lmgw/lmgw-machine-helper`, for a person reading them).
- **D83 — The RPM guard pins the helper by the file this job built, the policy by its sha256**
  (§3.8). The helper's bytes change with every build, so the pin is the build itself
  (`target/release/lmgw-machine-helper`, cleaned and rebuilt by `ci/build.sh`, ELF), compared
  with the package's header digest; anything under `/usr/libexec/` or `/usr/share/polkit-1/` in
  the allowlist must be pinned. Every pinned file is compared in sha256 alone: a package whose
  `FILEDIGESTALGO` is not 8 fails each pinned file, since the first draft compared a `built` pin
  in MD5 or SHA-1 too, where a second file with the build's digest can be made (review round).
  Alternative not taken yet: a reproducible build whose hash a release could publish.
- **D84 — The helper's crate depends on `lmgw-machine`, `serde_json` and `libc` only.** It depends
  on all of `lmgw-machine`, whose CDI readers bring a YAML and a TOML parser the helper never
  calls; the release build's LTO leaves them out of the binary (833 KB stripped, no
  `serde_saphyr` or `toml::` string in it, measured 2026-11-03), so a feature that keeps them out
  of the dependency set too is not built yet.
- **D85 — Nothing the helper starts inherits a descriptor.** Every program, its own reads through
  `SystemFacts::for_root` included, gets only stdin, stdout and stderr (`close_range` with
  `CLOSE_RANGE_CLOEXEC`, as `lmgw_core::child` does for lmgw's children), since pkexec hands the
  helper whatever lmgw had open. The helper changes to `/` before anything else (pkexec 127 runs
  it in root's home, or with `--keep-cwd` in the caller's directory), every program it starts, its
  reads included, runs in `/` (the review found `for_root`'s reads without it), and it sets `umask
  022`.
- **D86 — `run` needs cgroup v2.** It proves it runs inside the unit by `/proc/self/cgroup`'s
  `0::` line; a system with cgroup v1 alone has none and is refused with exit 4 (Fedora has been
  cgroup v2 since 31).

*Draft calls made in WP6's review round (2026-11-03):*

- **D87 — The user's side reads the plan's inputs through root's environment.**
  `lmgw_machine::plan::Inputs::read` reads through `Facts::root_env`: for the check's
  `SystemFacts` that is `SystemFacts::for_root` with the same command hygiene, still running as
  the user (every program from `ROOT_ENV` alone, in `/`). Before, the check read with the user's
  environment and root without it, and anything running as the user could write `~/.rpmmacros`
  (`%_dbpath`, `%_keyring`, `%_keyringpath`) or set `RPM_CONFIGDIR`, so the user's hash never
  matched root's: a password, exit 2, the same list, and again. Now only what root may read and the
  user may not still differs (`/root/.rpmmacros`, which the user's rpm cannot open and skips; a
  file under a directory only root may list); D88 is the shell's answer to that. A stated machine
  has a root view too (`FakeFacts::program_in_users_env` states a program that answers otherwise
  in the user's environment).
- **D88 — An exit 2 that is not a change stops the button.** On exit 2 with the app's own schema
  and version, and lmgw not replaced by an update since it started (D143), the shell (WP7) checks
  whether the hash it recomputes from a fresh check is among
  `plan_changed.plans[*].sha256`. If it is, the computer changed and the new plan is offered as
  §3.5 says. If not, lmgw and the system read this computer differently: the shell stops offering
  the button for that plan, says so in plain words ("lmgw and your system read this computer
  differently, so lmgw can't set it up by itself."), shows both plans (its own and root's) in the
  detail, and never offers the same plan again, so the person is never asked for a password for a
  plan root will refuse.
- **D89 — The password window names the change, and the action is `apply` alone.** The message is
  one sentence true of every plan the button offers (a fresh install, a switched-on section, the
  toolkit update, the swap of Fedora's retired package), and it no longer says "your password",
  since an account that is not an administrator is asked for an administrator's. The action gains
  `org.freedesktop.policykit.exec.argv1=apply` (pkexec 127 reads it), so pkexec matches it only
  for `helper apply …`; `pkexec helper plan` or `run` falls back to polkit's generic "run a program
  as the super user" action and its wording, never this message.
- **D100 — The helper's stdout is a queue no reader can stall.** Every line goes into an unbounded
  queue, and a thread of its own writes it through a non-blocking descriptor (`/proc/self/fd/1`
  opened again with `O_NONBLOCK`, so the description shared with systemd-run and the inhibitor
  keeps its flags; where that open is refused, the shared description gets `O_NONBLOCK` until the
  end; a socket gets `MSG_DONTWAIT`; a regular file a copy of its descriptor), waiting for room
  with `poll`. Each line and its newline are one write. A reader that stops reading (the person
  can freeze the app's scope, which `apply` runs in; a hung app) used to fill the 64 KiB pipe and
  block `run`'s loop, so the plan stopped halfway, the unit stayed active (every later `apply`:
  exit 5) and held the inhibitor. At the end the process waits until the queue is in the pipe or
  the pipe is full, then exits without the thread; what a stopped reader never took is in the
  journal, the whole record. A reader that is behind by a full pipe at that moment loses the last
  lines too, and `apply` then answers exit 6 (D102) though the steps ran; the shell's check
  afterwards shows what is in place (§3.5).
- **D101 — A program's run ends with the program, not with its pipes.** The real host reads both
  pipes in one `poll` loop until the program is reaped (a thread waits for it and closes a pipe of
  the helper's own), then takes the bytes in each pipe at that moment (`FIONREAD`), the last line
  without a newline included, and drops the pipes. The first draft read until both pipes closed,
  so a scriptlet's background process holding dnf's stdout hung `run` and the unit with it. What a
  process left behind writes afterwards is not the step's; systemd stops it when the unit ends.
- **D102 — `apply` passes `run`'s exit on only with `run`'s own line.** 0 needs `finished` with
  exit 0 (or `nothing_to_do`), 2 needs `plan_changed`, 3 `finished` with exit 3, 4 `refused`, each
  as the last of those lines `apply` relayed; anything else is exit 6. The first draft took the
  exit code alone, which systemd-run, the inhibitor or a program that is not the helper can give.
- **D103 — The unit has a `/tmp` of its own.** `unit_argv` sets `--property=PrivateTmp=yes`: dnf
  and the packages' scriptlets run as root, and any user can create files in the shared `/tmp` and
  `/var/tmp` before they do. `ProtectHome=read-only` is not set yet: it needs the live round to show
  that no step writes under `/home` or `/root`.

*Draft calls made in WP13's review round (2026-11-03):*
- **D90 — Every answer of the gateway's origin is sandboxed, except the dashboard's own.**
  `nosniff` and `Content-Security-Policy: sandbox` come from one layer on the whole router, outside
  the gate, instead of from a list of stored-content routes: a typed address or a link on the
  dashboard carries the owner's cookie (`SameSite=Strict` keeps only a foreign site's navigation
  out), and with Require API key off nobody needs one for `/v1`, so every route that relays or
  stores is a way to run script at the dashboard's origin, and a list only covers the routes
  someone recognised. Exempt is exactly `web::ui::routes`, which marks its answers; the agent
  origins are answered above the layer. Cost: an answer opened in a tab is a document without
  script at an opaque origin; whether a browser's own viewer (PDF, JSON) still renders there is not
  checked yet.
- **D91 — The agent MCP face drops `Set-Cookie`.** It is a path on the dashboard's origin, so a
  container's cookie set there would sit beside the dashboard's session; its one legitimate caller,
  lmgw's own MCP client, keeps no cookies. The agent origin still passes them (origins §2).
- **D92 — The sandbox is added, never swapped in.** A container's or an upstream's own policy stays
  and the sandbox goes on as a second one, since a browser enforces every policy an answer carries.
  An answer that already has a bare `sandbox` gets no second copy; `sandbox allow-scripts
  allow-same-origin` does not count as one, because it opens both doors the sandbox is for.
  `nosniff` replaces whatever value was there.
- **D93 — The exemption follows the router, not the path.** Whatever `web::ui::routes` answers
  keeps the dashboard's policy and gets no `nosniff`, its empty 404 for a miss under an API prefix
  and its 405 included; the gate's refusal on a dashboard path is the gate's JSON and is sandboxed.
- **D94 — The stored-content routes keep their own call.** `web::ranged` and the routes with their
  own answer still call `web::stored::sandboxed`, so a stored file stays covered where it is served
  if the router's layer ever moves; the layer finds the bare `sandbox` already there and adds
  nothing.

*Draft calls made in WP5's review round (2026-11-03):*

- **D95 — The swap at resolve takes only causes `Current` reads, and a fresh check asked for
  writes lmgw's spec again first.** The resolve-time swap (D66) takes the last check's cause only
  when a change to `Current` would clear it: the driver's causes (`no_nvidia_driver`,
  `driver_mismatch`, `driver_not_running`, `driver_libraries_missing`, `driver_unreadable`,
  `nvml_failed`), NVIDIA's toolkit missing or offered by another repository, and a toolkit whose
  write failed (`spec_not_written`, while `nvidia-ctk` is the file the check read; its fingerprint
  is in `Current`, so an updated or reinstalled toolkit reads as changed). Not no card at all, an
  AMD card's compute node, a folder of lmgw's that is not private, lmgw's own files or a write
  that ran out of time (`no_card`, `amd_no_compute_node`, `spec_dir_not_private`,
  `spec_files_failed`, `spec_write_timed_out`): nothing `Current` reads changes when those are
  fixed, and only a start writes lmgw's spec again, so the swap kept the one start from happening
  that would have cleared the verdict — every GPU row with a fallback stayed swapped until lmgw
  restarted. Those starts are tried each time; a failure is blamed from the last check while
  nothing changed (D96) and handed on to the fallback. And a fresh `machine_check` (`fresh: true`:
  "Check again", the Settings card, the fix's check after its steps) writes lmgw's spec again
  first when its last write failed for a cause the check blames (D57), within the load timeout,
  so a fix made outside lmgw clears the verdict; it does not join a run that began after it
  without that write. `lmgw__machine_check` with `fresh` stays a read: lmgw's own spec in its
  runtime dir is no setting, and every GPU start writes it the same way. The background checks
  after a start (D68, D96) write nothing: a start on
  the NVIDIA card wrote the spec itself just before, and a card that stopped answering would hold
  the one-run lock for another load timeout.
- **D96 — A failed start's blame reads only §1.1 and §1.2, and the full check follows in the
  background.** While the machine is as the last check read it (`Current`, and how the last write
  of lmgw's spec came out — equal when it failed the same way, any success alike), that check's
  verdict blames the start and nothing is read or run. Otherwise §1.1 and §1.2 are read again
  alone (`machine/blame.rs`, `gather::for_blame`): the card's facts, NVML in its separate process,
  amdgpu's counters, and §1.2's facts without `nvidia-ctk --version` and the advisory
  `repoquery`, which only add detail and download sizes (so a toolkit another repository offers
  reads as missing until the full check). NVML is left out when the start's own write of lmgw's
  spec ran out of time on the card (`nvidia-ctk` waits on NVML, so the probe would wait as long
  again): §1.2's `spec_write_timed_out` then blames it, and the log says NVML was not asked. One
  read at a time, in a task of its own: a start that fails meanwhile joins the read under way, and
  a caller that goes away leaves it to the others. When it ends the full check runs in the
  background (one waiting at a time: one asked while it waits to run is that one), so the status
  and the tray say the new verdict; the start's answer never waits for Podman, the package
  metadata or a registry, each of which the full check bounds only by the load timeout. A read
  whose blocking thread ends without an answer blames nothing, and a full check whose card reads
  end so says "lmgw couldn't finish checking this computer. The details say why." (`needs_steps`,
  blaming no start) instead of panicking on the request path.
- **D97 — A pinned caller is never swapped for the machine check's verdict, and refusals say why
  a fallback answered.** `gate::resolve_pinned` (quickdoc's embedder and reranker through
  `open_pinned`, ingest's token count, the upscale) does not swap the model named for
  `gpu_setup`: its start is tried, and a failure answers `503 gpu_setup` at admission, where a
  pinned caller never hands on. Before, quickdoc's document chunks went to the embed row's
  fallback (possibly a cloud one) before the vectors were refused, and the refusal said
  `gpu_hold`, "release the hold", with no hold on. The hold's and a benchmark's swap still happen,
  and the caller refuses those itself. quickdoc's refusal of a fallback's answer, and the
  upscale's, word the reason the fallback answered for the model named (the walk's first hop,
  `FallbackReason::because`): the hold or a benchmark's lease as before while it is on, anything
  else as what it is (code `fallback_refused` for the embedder's probe, with what to do for that
  reason), never "release the hold" with no hold on.
- **D98 — A Background warm of a stage this computer cannot start says its fallback answers.** A
  session's or a read-aloud's Background warm (`realtime/warm/background.rs`) says `fallback`
  with `answered_by` when the check's verdict would swap the stage at resolve (D95) and its row
  has a usable fallback — nothing is started — and when its start fails with `gpu_setup` and the
  row's fallback is usable; without one, `failed` with the sentence. Before, the stage was started,
  failed and said `failed` while every turn went to the fallback. The Admit warms go through the
  gate and say it already.
- **D99 — The pre-fetch skips a metered connection.** Before anything is asked of podman,
  NetworkManager's word for the device the default IPv4 route leaves through
  (`/proc/net/route`'s lowest-metric `0.0.0.0/0` route that is up, then `nmcli -t -f
  GENERAL.METERED device show <device>` within the load timeout, as the check's programs run):
  "yes", guessed or not, skips the whole pre-fetch with the log line "the engine pre-fetch is
  skipped: metered connection (<device>: <what it said>)", and each model's first start
  downloads its engine; no default route, no `nmcli`, an error or anything but yes or no counts
  as not metered, and the log says why. Why nmcli and not D-Bus: it reads through the check's
  `Facts` seam, so a test states it and nothing reaches the system bus; on an unmetered wired connection it says "no (guessed)". A switch to turn the pre-fetch off is not built
  yet.
- **D105 — The tray's item shows only after a `ready` seen since lmgw started.** §2.4 says "from
  `ready` to anything else", and that is built as written: a computer whose first verdict in this
  run is not `ready` (a card that never worked, a driver update that wants a restart before lmgw
  was started) shows nothing in the tray, because the window opens at every launch with the
  check in front of it (§2.1) and a cloud-only person is not nagged (§2.2). Taking the item to
  every non-`ready` start, including a break that was there when lmgw started, is a one-line
  change in `machine_tray::Watch` (drop `was_ready`); whether a newcomer should see it is open.
- **D106 — Any verdict but `ready` shows the item once armed, `cloud_only` included.** A card
  that was `ready` and is not found any more has stopped working too, and the sentence says so.
- **D107 — The status line and the tooltip end with the summary's sentence.** The status line
  reads "lmgw · <address> · <n> req/min · <n> active · <sentence>" and the tooltip "lmgw —
  <sentence>" (after the hold's words, when both apply); the sentence is the summary's
  `headline` as the check wrote it. The runtime line and the icon are unchanged.
- **D108 — The tray polls the in-process summary on its existing 5 s tick.** `MachineChecks` has
  no subscription (it is a last-result cell), and the tick already reads the hold, the runtime
  list and the request stats from the same state; a change shows within 5 s. A watch channel on
  `MachineChecks` is not built yet.
- **D109 — The item raises the window and sends it to `/settings/gpu#machine`; a window that has
  to be built opens at `/`.** An existing window (the usual case: closing only hides it) is
  navigated to the route on the gateway's own origin and keeps its session. A window built by
  the item itself goes through the login, which lands on `/` only (a route after login is not
  built yet). Today `/settings/gpu` is the GPU category of Settings (admission control and the
  hold); the `#machine` anchor matches nothing until WP8a's card exists, so the page opens at
  its top.
- **D110 — The settings switch is offered whenever it changes something, and a mismatch it fixes
  is `needs_fix`** (WP12; replaces D51's "`needs_steps` until WP12"; revised in WP12's review
  round). §1.4's item carries the switch as its `action` (`MachineAction::SettingsFit`: the card,
  the button's words, every field it changes, what it leaves and the running models it stops).
  With settings that keep models from running (judged against the cards of the test set, D137), the item is
  a problem. When the settings as the switch leaves them fit (§1.4 judges the snapshot the switch
  would write, `Switch::apply`), its sentence is "lmgw is set up for NVIDIA cards. It can switch
  to the settings for AMD cards." (or the reverse) and the verdict `needs_fix` with that sentence
  as the headline. When something would still not fit after it (an image of the owner's built for
  the other card, D139; arguments with a group of the owner's, D138), the sentence is "Some model
  settings are for NVIDIA cards, and lmgw can't switch all of them to the settings for AMD cards.
  The details say which to change by hand.", the verdict `needs_steps`, the action still there
  for the part it does, and the detail names what remains. The helper's fix comes first when both
  are there (§1.2 before §1.4, in the items' order), and a problem only instructions fix still
  outweighs both. When the switch changes something but nothing it changes keeps models from
  running, the item is information with the same action ("Some model settings are for AMD cards.
  lmgw can switch them to the settings for NVIDIA cards.") and the verdict is untouched: lmgw's
  own AMD Vulkan values on an NVIDIA card (the render nodes alone stay no vendor's, D51, since an
  Intel card beside it may be what they are for), a disabled row, or a row for the AMD card beside
  an NVIDIA one (D137). Information sentences add up: the processor's ("Speech and audio models
  are set to run on the processor.") and another CDI kind's follow the switch's, where the first
  pass kept only the switch's. Why a button and not instructions: it is an ordinary settings
  write the app makes itself (D18).
- **D111 — What the switch writes** (`machine/fit/shipped.rs`, the one place). For AMD cards,
  Vulkan only: `ghcr.io/ggml-org/llama.cpp:server-vulkan` for chat and aux,
  `ghcr.io/0xshug0/audio.cpp:full-vulkan` with the audio Backend `vulkan`,
  `ghcr.io/leejet/stable-diffusion.cpp:master-vulkan` for images (image-generation design §2.5),
  and for every class the run arguments `--device /dev/dri --group-add keep-groups
  --security-opt label=disable`. For NVIDIA cards, lmgw's defaults (`Settings::default()`).
  ROCm is never written: it works only for the gfx targets the image carries
  (`docs/amd-arch.md` §3), and Vulkan is that page's "start with". Why `label=disable` although
  `docs/amd-arch.md` left it out: that page was written for Arch, without SELinux, and lmgw
  mounts the models dir without a relabel (`<dir>:/models:ro`, `runtime/argv.rs`), so on Fedora
  with SELinux enforcing a container without it cannot read its models; lmgw's NVIDIA defaults
  carry it for every class for the same reason (`config/settings_classes.rs`).
  `docs/amd-arch.md` §2 and §4 now say so and list these values. A relabel that would let it go
  is in "Not yet".
- **D112 — Which class is switched** (revised in WP12's review round). Only a class set for the
  other card: its run arguments pass the other card's device (`nvidia.com/gpu`/`--gpus`
  switching to AMD's, `/dev/kfd` switching to NVIDIA's, and switching to AMD's `/dev/kfd` too on
  an AMD card without it, D137), or they pass the render nodes (or, switching to AMD's, AMD's
  compute node) with an image built for the other card (D139). Such a class gets this card's
  image (one already built for this card's backend, a `server-vulkan-b7123` say, stays) and audio
  backend, and its run arguments swapped as a row's are (D138): the other card's flags go, this
  card's are put in front, and the owner's own arguments (`--shm-size 16g`, `-v /data:/data`)
  stay, where the first pass wrote the shipped values whole and dropped them. A class on the
  processor (no device), one naming a CDI device of another kind, one whose arguments give
  models a group of the owner's (D138), and, switching to NVIDIA's, one passing only the render
  nodes with an image that is not lmgw's for AMD cards stay as they are, each named in `left`
  with why. An audio Backend of `cpu` is kept.
- **D113 — Rows with their own settings for the other card are changed too, in the same switch**
  (confirm, then do; revised in WP12's review round). Every row counts, a disabled one included.
  A row's own run arguments set for the other card are swapped (D138), without this card's flags
  for a row on the processor. A row's own image that is one of the engines' published images
  built for the other card (by its tag in any spelling, D139, a digest pin included) becomes this
  card's image of its class. Either is cleared when it then equals the class's, so a row whose
  override was a copy of its class follows it again. An image of the owner's that lmgw can tell
  is built for the other card (its build label or its libraries, D139) stays and is named in
  `left` ("it is built for NVIDIA cards (CUDA) and isn't one of lmgw's images, so lmgw doesn't
  replace it: pick one for AMD cards on the model's page"), and so does one lmgw cannot tell on a
  row set for the other card ("lmgw can't tell which card this image is built for: …"). A row's
  own render nodes with an image that is not lmgw's for AMD cards, switching to NVIDIA's, and a
  row naming another CDI kind are named in `left` as a class is (the first pass named them only
  for a class). Why change them rather than leave a note: the confirmation lists every row field
  with its old and new value before anything is written, and a switch that left the rows for the
  other card would leave exactly the models the person runs unable to start.
- **D114 — `confirm` is the confirmation's own step, and what was offered is what runs** (revised
  by D135). The op without `confirm` writes nothing and answers the list the item's action
  carries; with it, it works the switch out again under the settings lock (`fit::switch::plan`,
  the one function both use) and writes it only when that is the confirmed list (D135). There is
  no hash, as the root plan has (D2): the list itself is what is confirmed. Asked when nothing is
  left to switch, it answers "The model settings already are the ones for AMD cards." and writes
  nothing.
- **D115 — Classes and rows in one save, the Settings page's.** The classes' part goes through
  `settings_set_full`'s own save (its notes, the publish, the MCP reconcile), which takes the
  rows' writes in its transaction (`store::set_run_overrides_in`, each only while the row still
  holds what the switch was worked out from), so all of it lands or none of it; since the review
  round it is `api_settings::settings_set_full_planned`, which works the save out under the
  settings lock (D135). The model editors' own save (`*_model_set`) is not used for the rows: it
  validates the whole row and reads the image's `--help` through a throwaway container, which may
  download the image, so it could refuse or hold up a two-field change for reasons that are not
  the switch's. The check runs again after the write and is in the answer. The running models it
  concerns are stopped after the write (D136), where the first pass left them running.
- **D116 — Refused only when this computer has no card of that maker.** The PCI bus is read as
  §1.1 reads it: no AMD display device for `amd_vulkan` ("This computer has no AMD graphics card,
  so lmgw doesn't switch to the settings for AMD cards."), no NVIDIA one for `nvidia_cuda`, and
  "No graphics card lmgw can use was found, so there are no settings to switch to. Cloud models
  work without one." with neither (an Intel card, none). Asking for one maker's settings while
  the check judged the other's card (an AMD card beside a working NVIDIA one) is allowed, both
  ways, and a test holds it.
- **D117 — No self-admin tool for the switch yet.** Class container definitions (images, run
  arguments) are not on the tool plane (`ops::settings_patch`: container settings stay on the
  dashboard), so a `lmgw__machine_settings_fit` would be the first tool to write them; that is
  the owner's call. `lmgw__machine_check`'s description says the item carries the action and
  names the op (`machine_settings_fit`, with `confirm` set to the listed changes) as not a tool
  there; the dashboard's button comes with WP8a, and the words for it with that.
- **D118 — A CUDA image on an AMD card is NVIDIA's settings, whatever device it is given.** §1.4
  compared only device flags; an image built for CUDA (`server-cuda`, `full-cuda12`,
  `full-cuda13`, `master-cuda`, a digest pin included, and since D139 an owner's build whose label
  or libraries say CUDA) given the render nodes or `/dev/kfd` now makes the class or row a
  problem on an AMD card, since that image runs on none. The reverse is not so: a Vulkan image
  given NVIDIA's device runs on an NVIDIA card.
- **D119 — The switch's labels are the page's, its values typed.** A class field is its Settings
  path ("Settings → Runtimes → Chat → Image"); the audio Backend joined
  `lmgw_api_types::settings_labels` (`AUDIO_BACKEND_AT`), and the Settings row reads its label
  from there. A row's field is named as the model's page names it ("Image override", "Extra
  podman run args override"), beside the model's id. Values are typed (`SettingsFitValue`, since
  the review round): an image or backend is a string, run arguments the list of arguments as the
  field holds them, where the first pass joined them with spaces and `-e "A=b c"` read like three
  arguments. The detail lines show arguments on one line, an argument with a space in quotes.
- **D135 — The confirmation is the list that was shown, compared under the settings lock.**
  `confirm` is the `changes` the person saw (the item's action, or an answer without `confirm`),
  every field with its value as it was and as it would become. The op works the switch out again
  inside the Settings save, under `settings_write`, from the snapshot then
  (`api_settings::settings_set_full_planned`), and writes it only when it is that same list (in
  any order); a class saved between the offer and the confirmation (an owner's `--shm-size 16g`
  added to the chat class) makes it another list, so nothing is written and the answer carries
  `outdated` (the confirmed changes the switch no longer makes as listed, and the ones it makes
  now) with the switch as it is now to confirm again: say what changed, offer the new list, as
  D88 does for the root plan. A row edited in the store meanwhile is caught by that row's write in
  the transaction (it no longer holds what the switch was worked out from) and refuses all of it,
  the classes included; the answer names the model the same way. The running models the switch
  stops (D136) are not part of the compare: a model that starts or stops between the two only
  changes who is stopped, never what is written, and no stop is forced. And the offer is never
  stale: every published snapshot judges §1.4 and the verdict again from the last run's reading
  (`MachineChecks::refit`, from `AppState::publish_over`, whichever path saved), without reading
  the machine; a turn to `ready` that way begins the pre-fetch as any turn does. What the
  re-judging cannot know until the next run reads the machine: an image named for the first time
  since (it counts as one lmgw cannot tell) and the containers that started or stopped.
- **D136 — The switch stops the running models it concerns, and one that keeps running is said.**
  The offer lists the running models whose container the switch would start with other settings
  (their image, run arguments or audio backend in effect differ between the settings now and as
  the switch leaves them); the preview's sentence says it stops them "unless one is answering a
  request then". After the write, each is stopped through the Settings apply's own path
  (`lifecycle::stop_for_apply_outcome`, never forced), and the answer says what happened to each
  (`stopped`, `not_running`, `busy` with its requests, `failed` with the error) and, for a busy or
  failed one, what to do ("'busy' is answering requests, so it keeps the old settings until it
  stops: stop it on the Overview once it is done."). Those containers are recorded with their
  generation (`MachineChecks::keep`), and while the same container runs, §1.4 is a problem with
  "Some models still run with the settings from before the switch (busy). They use the new
  settings once they stop." (cause `running_old_settings`), the verdict `needs_steps`: the check
  never says ready while a container runs with the flags from before. Why not wait for it or
  force it: a forced stop cuts a request short, and waiting would hold the answer for as long as
  a generation takes; a retry once it is idle is not built yet.
- **D137 — Settings are judged against the cards on this computer, not only §1.1's.** §1.1
  judges the NVIDIA card first when both are there; §1.4 judges each class and row against every
  card it can use (`fit::present`): NVIDIA's device fits an NVIDIA card whose driver works,
  AMD's compute node an AMD card that has `/dev/kfd`, the render nodes any AMD card on amdgpu
  with render nodes, each with an image that is not CUDA's. So a row deliberately on the AMD card
  of a computer whose NVIDIA card comes first is no problem and no tray item; the switch to
  NVIDIA's settings that would move it is information. And an AMD card on amdgpu with render nodes
  but no `/dev/kfd` is ready (§1.1 used to say "lmgw can't compute on this AMD card" without
  `/dev/kfd`, though lmgw's settings for AMD cards are Vulkan, which needs only `/dev/dri`); there
  ROCm's settings (`/dev/kfd`, a ROCm image) do not fit, the switch replaces them with the Vulkan
  ones, and §1.2's detail says ROCm engines can't use the card.
- **D138 — The switch swaps the other card's flags as a set, for a class and a row alike.**
  `fit::switch::swapped`: the other card's device flags go (`strip_gpu_devices`), and switching to
  NVIDIA's, the flags lmgw's AMD settings carry that mean nothing there go with them
  (`--group-add keep-groups`, ROCm's `--security-opt seccomp=unconfined` and `-e
  HSA_OVERRIDE_GFX_VERSION=…`, `shipped::amd_only`); this card's flags it does not hold yet are put
  in front, and every other argument stays. So a round trip ends where it began, a class's or a
  row's (the first pass left `keep-groups` on a row after AMD to NVIDIA, which kept it an override
  for good). Where AMD's `--group-add keep-groups` would join a group of the owner's
  (`--group-add video`), Podman refuses the two together, so those arguments are not switched:
  they stay, named in `left` ("it gives models a group of their own (video), and the settings for
  AMD cards give them your own groups instead; the two can't be combined, so take that group out
  and switch again"), and §1.4 counts them as what the switch does not fix (D110).
- **D139 — An image lmgw doesn't know by its name is judged by what it is.** One of the three
  engines' published images is told by its tag (`shipped::image_built`: a `-`-separated part
  `cuda…`, `vulkan`, `rocm` or `hip`, after `backends::pull::canonical_reference`, so
  `docker://`, a digest pin, `server-cuda-b7123`, `full-cuda` and `light-cuda` are read alike,
  where the first pass matched lmgw's references exactly). Any other image the settings name (an
  Engines-page build, a tag of the owner's) is read with `backends::images::image_gpu`, the aux
  hoist's reading: its `dev.lmgw.backend` label, or else its `libggml-*` libraries listed by one
  throwaway container (`--network=none`, no device, `--pull=never`). The check reads it (only for
  images it can't tell by name, and only while Podman answers), and so does the op, before the
  settings lock. One backend lmgw has settings for decides; several, none, another (`sycl`), an
  image not on this machine, or one whose files do not say, stay unknown. So a row with the
  render nodes and `localhost/sd-custom:cuda` built for CUDA is a problem on an AMD card that the
  switch names and does not fix, where the first pass called it unknown and the warning went once
  its arguments were switched.

*Draft calls made while building WP7 (2026-11-03):*

- **D120 — pkexec's own textual agent is off.** The shell runs `/usr/bin/pkexec
  --disable-internal-agent /usr/libexec/lmgw/lmgw-machine-helper apply <hash>`. Without the
  option, pkexec on a desktop with no password window tries its textual agent: with lmgw started
  from a terminal it asks for the password on that terminal, where nobody looks, and the setup
  waits; started from the app menu there is no terminal, and pkexec says "Error creating textual
  authentication agent", never the "No authentication agent found" §3.5's row reads (pkexec 127's
  order; both strings are in `/usr/bin/pkexec`). pkexec's options come before the
  program, so the helper's own arguments, and with them `exec.argv1 = apply` (D89), are
  unchanged. Both messages count as "no agent". Alternative not taken yet: the call without the option.
- **D121 — The confirmation is a GTK message dialog of the shell's own, with the commands behind
  a fold.** gtk 0.18 (the version tao builds the window with, already a dependency), modal over
  the app's window: §4.5's bullets and password line, and an expander "Show the commands" with
  every command by its absolute path, every file the steps write with its mode (a key with its
  fingerprint) and in full, and what each package step's own scripts change (D144). "Not now" is
  the default response and has the focus, so Enter and Escape change nothing; a dialog closed any
  other way is "Not now". "Set it up" is insensitive until the dialog has had the focus for a
  second (D134), and the dialog shows only over the app's window in front (D133). `tauri-plugin-dialog` (rfd's GTK dialog) sets no default response and
  has no fold. The commands are in the native dialog too, because the page's fold is drawn at the
  gateway's origin and the native dialog is the one a script there cannot alter (D25).
- **D122 — A setup already running is followed, before anything is asked.** The command asks
  systemd first (`unit_state_args`); a running unit is followed with no dialog, no password and
  no pkexec, as an exit 5 is. Following asks systemd once a second (a sampling rate; it ends when
  the unit does and has no bound of its own), reads the journal by
  `_SYSTEMD_UNIT=lmgw-machine-setup.service`, `_UID=0` and the running invocation's
  `_SYSTEMD_INVOCATION_ID` (journalctl's matches, and every entry it prints checked again for all
  three), passes `run`'s lines on as events, and takes how it ended from the journal's
  `finished`. Where the journal is not readable (an account that is not an administrator) or
  systemd names no invocation, the answer is `ended`, and the check after it says what is in
  place.
- **D123 — The page's plan must be the fresh check's, before the dialog.** The command runs a
  fresh check (core's `machine::check` with `fresh`) and goes on only when that plan's hash is the
  argument and its `how` is `helper`. Otherwise it answers without a dialog: the new check
  (`plan_changed`), the check's own reason (`unavailable`; `how` is `helper` only on dnf5 with
  steps, the helper installed, no AppImage, no other repository and no plan read differently, so
  this is the dnf5 gate), or `nothing_to_do` when the check offers no plan. The dialog is drawn
  from that check's plan.
- **D124 — A plan read differently is remembered by core, for this process.** On D88's branch
  the shell calls `MachineChecks::read_differently(hash)`, and every later check of this process
  plans it as instructions with "lmgw and your system read this computer differently, so lmgw
  can't set it up by itself. Here are the commands to run in a terminal instead."
  (`UserSide::read_differently` in `lmgw_machine::plan::how`), so the page stops offering the
  button by itself, after a reload too. A restart forgets it, so such a plan can cost one
  password per run of lmgw; keeping it across restarts is not built yet.
- **D125 — One setup at a time from the app.** A second `machine_apply` while one runs is
  answered at once ("lmgw is already setting this up. Its progress shows where it was started."),
  without a dialog.
- **D126 — pkexec's 126 and 127 are pkexec's only before the helper's first line.** After a
  `begun` the helper runs, and it never exits with either; such an end is "stopped unexpectedly",
  with the check afterwards. Before it, a 127 with pkexec's own words about the program is no
  refused password (D142).
- **D127 — What runs afterwards, and after which ends.** Core's `machine::after_fix`: lmgw's own
  spec written (`ensure_fresh` within the load timeout), a fresh check, both under the one-run
  lock and published once, then the ledger's probe detected again only when it reads nothing and
  the check found the NVIDIA card and its driver working; the new one is installed when it reads
  the card. It runs after every end in which the helper ran as root: done, a failed step, a failed
  task, a crash after the first line, a followed setup. "Not now", pkexec's 126 and 127, a
  refusal (4) and "lmgw was updated" changed nothing and answer with the check made before the
  dialog; exit 2's branches run a plain fresh check. §3.5's probe re-install meant more than it
  can: the steps install NVIDIA's container toolkit, never the driver whose `libnvidia-ml.so.1`
  the probe loads, so it only matters when the driver came up after lmgw started (an akmod build
  that finished after the boot).
- **D128 — A failed step's plain reason comes from patterns; the fallback quotes the step.**
  §3.5's reasons are matched case-insensitively in the failed step's lines, in this order:
  another transaction holding the lock, a download that is not what NVIDIA signed, a key lmgw
  lacks (the two told apart by D141), a download failure, and for S4″ a dependency problem. None matching gives "\"<the step's bullet>\" didn't work: it ended
  with exit code N." (or the signal, or "lmgw couldn't carry it out" for the helper's own error).
  The words are dnf5's, librepo's and rpm's as their libraries' strings hold them (libdnf5 5.4.6,
  librepo 1.21.2, rpm 6.0.2), not watched in a failing run ("Unverified").
- **D129 — A helper refusal (exit 4) keeps its reason in the detail.** The sentence is "lmgw
  couldn't start the setup. The details say why; nothing was changed.": the helper's reasons are
  written for a technical reader, and exit 4 is never expected from the app.
- **D130 — A replaced `/usr/bin/lmgw` reads as an update.** Once an RPM update replaced the
  program while it ran, `/proc/self/exe` names `/usr/bin/lmgw (deleted)`; that gate says "lmgw was
  updated while it was running. Restart lmgw, then try again." instead of "This copy of lmgw isn't
  the installed one". It is asked again where an update while the dialog was open would explain the
  end: pkexec that could not run the helper (D142) and an exit 2 (D143).
- **D131 — The events and the answer (§4.1).** `MachineApplyEvent`, tagged by `event`:
  `password`, `authorized` (the first `begun` of `apply`), `following`, `started`, `step_started`
  (`step` of `of` from 1, `kind`), `line` (`step`, `kind`, `stream`, `line`), `step_ended`
  (`exit`, `signal`, `error`), `note`, `finished`, `checking`. `MachineApplyResult` has `outcome`
  (§3.5's rows plus `declined`, `unavailable`, `nothing_to_do` and `ended`), the plain
  `sentence`, `step`, `output` (every line and note, as text), `detail` and `check` (`None` only
  when a gate refused before the check). A refusal is an answer, not an error: the command errs
  only while lmgw is still starting, and the invoke handler's origin refusal stays as it is. The
  window not in front is `unavailable` ("Open lmgw's window and press Set it up there.", D133), and
  a document that said "Not now" before is `declined` again without a dialog (D140).
- **D132 — pkexec gets nothing of lmgw's process.** stdin null, an empty environment (pkexec
  builds the helper's own, which the helper ignores too), `/` as its working directory, no
  descriptor beyond the standard three (`lmgw_core::child::system_tool`), and it is never killed
  when the command goes away: after pkexec's exec the helper runs as root, and the steps in a unit
  of their own. `systemctl` and `journalctl` run the same way, with `LC_ALL=C.UTF-8`. The hash it
  is given is the fresh check's plan's (equal to the page's by then), lmgw's own value.

*Draft calls from WP7's security review round (2026-11-03; no blocker, no high):*

- **D133 — The confirmation shows only over the app's window in front.** On the GTK main thread,
  in the closure that builds the dialog, the main window must be shown (`is_visible`: not hidden
  to the tray), the active one (`is_active`: the window the person works in) and running a
  document (`media::document`). Otherwise there is no dialog: the answer is `unavailable` with
  "Open lmgw's window and press Set it up there." and why in the detail. A hidden window's
  webview keeps running, so a script at the gateway's origin could otherwise pop the dialog over
  another program, under the person's cursor. The look and the dialog are one closure, so nothing
  of the window changes between them (`confirm::Asked::may_show`).
- **D134 — "Set it up" waits a second with the focus.** The button is insensitive until the
  dialog has been mapped and active for `ARM_DELAY` (1 s), and goes insensitive again, with a new
  wait, whenever it is unmapped or loses the focus: Firefox's dialog-delay model, with its
  `security.dialog_enable_delay` default of 1000 ms. A rapid-click target the page drew under the
  spot, or a dialog timed to appear under a click, lands on an insensitive button; the person sees
  the wait as the button becoming available. `confirm::Arming` decides (unit-tested); the GTK glue
  feeds it the dialog's `map`, `unmap` and `is-active` changes and starts a
  `glib::timeout_add_local_once` for each wait it hands back, and an older timer arms nothing.
  The delay's length is open.
- **D140 — A "Not now" holds for the document that said it.** The answer is kept with the number
  of the window's running document (`media::document`: a commit, the first document seen and a new
  window each take the next number; a same-document navigation, as the dashboard's router makes,
  keeps it). A later call from that document gets `declined` without a dialog: "Nothing was
  changed: you chose "Not now" already. Reload the page to be asked again." Another document (a
  reload too) is asked again, "Set it up" clears it, and a restart forgets it. The document is the
  one running when the dialog would show, not yet the one whose call it is (the command cannot
  tell which document's invoke it runs for).
- **D141 — A download that does not verify is not a key lmgw lacks.** "PGP check", "signature
  verification" and "public key" matched both. Now a download that is not what NVIDIA signed
  (rpm's `: BAD` after a signature or digest, libdnf5's "The package is not signed." and "Problem
  occurred when opening the package.", librepo's "Bad GPG signature", "GPG signature is not
  available" and a checksum that doesn't match) says "The download didn't match NVIDIA's
  signature, so nothing was installed. Try again later, and if it keeps happening, tell lmgw's
  developer.", and is matched before a key lmgw lacks (libdnf5's "Public key is not installed.",
  "Import of the key didn't help, wrong key?" and "… the key is not trusted.", rpm's `: NOKEY`
  and `: NOTTRUSTED`), which keeps "… An lmgw update will fix this." Both before a download
  failure, since librepo's lines name librepo. dnf5's "OpenPGP check for package … has failed: "
  and "Signature verification failed." come with either and decide nothing: alone they give the
  step's own words.
- **D142 — pkexec that could not run the helper did not refuse the password.** A 127 before the
  helper's first line whose stderr has pkexec's own words about the program ("Error accessing
  <program>: …" and "Cannot run program <program>: …" before the password, "Error executing
  <program>: …" after it, polkit 127's strings), and never "as another user", is `failed` with
  "lmgw couldn't start the setup. The details say why; nothing was changed.", or `updated` when
  `/usr/bin/lmgw` reads replaced by then (an RPM update between the gates and the spawn took the
  helper with it). "Error executing command as another user: …" and a 127 with neither stay
  `not_authorized`. Nothing ran as root, so the answer carries the check from before the dialog.
- **D143 — An exit 2 asks whether lmgw itself was updated first.** Before D88's branch the shell
  reads `/proc/self/exe` again: replaced (`/usr/bin/lmgw (deleted)`) is `updated`, whatever the
  line says. The versions compared stay `CARGO_PKG_VERSION` on both sides: `ci/build.sh` stamps the
  workspace version as `<X.Y.Z>+<pipeline>` for a test-feed RPM before either binary is built, so
  two such builds already differ in the line (e.g. 0.5.0+1 against 0.5.0+2); two local builds
  of one version do not, and the replaced program tells those apart.
- **D144 — The fold says what else changes: modes, a key's fingerprint, the package's own
  scripts.** Each file line gives the mode root writes it with (`(mode 0644)`), and the key file
  its OpenPGP v4 fingerprint in groups of four (`C95B 321B 61E8 8C18 09C4 F759 DDCA E044 F796
  ECB0`), computed by core from the text the step writes (`lmgw-core/src/machine/key.rs`, SHA-1
  through aws-lc-rs, so the root helper's dependency set is unchanged), never copied from
  `NVIDIA_KEY_FINGERPRINT`. After each step that installs NVIDIA's package through dnf, a note
  says what its install scripts change (`lmgw_machine::plan::scripts`, read from `nvidia-container-toolkit-base` 1.20.1's `rpm -q --scripts`): for S4 and S4″ "NVIDIA's
  package then runs its own install scripts: they write /etc/nvidia-container-runtime/config.toml
  when it is missing, switch on and start NVIDIA's refresh service (nvidia-cdi-refresh), which
  writes NVIDIA's own description of the card to /var/run/cdi/nvidia.yaml, and reload the system's
  device rules.", for S4′ the same without the switch-on (a first install's `systemctl preset`).
  The plan DTO carries all three (`PlanFile.mode`, `PlanFile.key_fingerprint`,
  `PlanStep.package_scripts`) for the page's fold too, display only: `HashedPlan` and
  `PLAN_SCHEMA` are unchanged, and the pinned fresh-Fedora hash holds. The dialog's text drops "lmgw
  changes only these things." for a plan with a package step and says instead "Besides these,
  NVIDIA's package runs its own install scripts; "Show the commands" says what they change."; a
  plan of S1, S1′ and S2 alone keeps it.

*Draft calls from WP8a, the dashboard's component and the fix (2026-11-03, revised after its
review round the same day: no blocker; H1, M1 to M5 and L1 to L14 fixed):*

- **D145 — One component, the surface draws the card.** `widgets::MachineCheck`
  (`crates/lmgw-ui/src/widgets/machine_check.rs`, with `bridge`, `items`, `plan`, `progress` and
  `fit` beside it, the look in `assets/machine.css`) is the headline, the items and their fixes,
  and nothing around them: Settings → GPU puts it in its card, WP8b's surfaces in theirs. It takes
  `fresh` (ask for a new run when shown, §2.6), `compact` (the items that are no problem folded,
  for a surface with little room, D186), and an optional `settings_written` callback for a
  surface that shows the settings the switch writes (D164). "Not now" is the surface's, in its
  card's head or at the banner's end (D175, D190), never the component's: §4.5's "[Not now]" is
  not in the page's plan block. What it shows is the app's state, not its own (D160). (Revised
  in WP8b's review round: an optional `not_now` put a "Not now" beside "Set it up" first.)
- **D146 — "In the app's window" is the dashboard's one shell test.** `__TAURI_INTERNALS__` is
  there (`ui_scale::tauri_invoke`, what the zoom, the file picker and the audio commands already
  use, and what `desktop.js` marks as `html.tauri`). Outside it a helper plan shows §2.7's
  sentence where the button would be, before the commands fold, and no control that cannot work;
  its words are D163's.
- **D147 — A fix shows under the item it is for.** The plan block, the progress view and the
  setup's answer sit under §1.2's item (using the card for models: the plan is the toolkit's), the
  settings switch's confirm under §1.4's; a check without §1.2's item shows its plan under the
  list. The person reads the problem and its fix together, and "Set it up" is on screen sooner
  than at the end of six rows. The plan block is hidden while a setup runs (D125's one at a time,
  seen).
- **D148 — The plan block says what the dialog says.** Its title, "lmgw will make these changes
  to this computer:", the step bullets, the package-scripts line or "lmgw changes only these
  things." and the password line are the shell's `confirm::of` text, so the page and the dialog
  read alike; "After that, lmgw keeps the description of your graphics card up to date by itself,
  without asking again." follows the bullets of every plan with steps (an instructions plan too).
  An instructions plan reads "How to set it up", its `why`, the bullets, and the steps' terminal
  lines open with "Copy the commands"; one without steps is its `why` alone. In a browser the
  intro and the password line say where lmgw runs (D163).
- **D149 — The commands fold has the terminal lines too.** After what runs as root (each file with
  its mode and a key with its fingerprint, the package's own scripts as a line in words, not as a
  command) and every file in full, the fold gives the plan's `terminal` lines with "Copy the
  commands", in the app and in a browser alike: a person on another machine, or one who would
  rather type, has the lines without a second view. `no_agent` shows the same lines open (§4.3).
  The copy button is in words, not the kit's icon, right beside the words it acts on (not pushed
  across the card), and a copy the browser refuses is said there (D169).
- **D150 — The Settings card mounts when GPU is first shown.** Every category stays mounted, so
  the card's component mounts the first time the GPU category is shown and asks for a fresh run
  then (§2.6's "opening the Settings card"); opening Settings at Network & access probes nothing.
  Coming back to GPU in the same visit shows the result it has; "Check again" asks anew. Opening
  Settings again after leaving it mounts the card again and asks again, except while a setup runs:
  then the run is shown and no read goes out (D160).
- **D151 — The search rows are links, the card is never a search hit.** Five `Ctl::MachineLink`
  rows with §2.3's labels carry the words a search finds the card by and show only in a search,
  as one link (D166); the card's own row (`Ctl::Machine`) shows only outside one (`in_search`,
  `in_category`). `#machine` scrolls the card's top into view (not its middle, it is taller than
  the pane) once it has its content, and rings it once (D167). The card spans the category's
  columns (`Group::wide`).
- **D152 — Folds grow; the page scrolls.** The live output, the answer's output, the details and
  the commands fold have no height of their own: they grow, long lines wrap (`pre-wrap`,
  `overflow-wrap: anywhere`), and the pane scrolls, since the dashboard keeps one scroller per page
  (ui-matrix fails a nested vertical scroller). Nothing is cut. While the person reads the live
  output's end, the page follows it, and the phase line stays in view (D161).
- **D153 — What the progress view says when.** After "Set it up" and before any event: "lmgw is
  asking you to confirm, in a window of its own." (the shell's dialog); `password`: "Your computer
  is asking for your password." with the administrator line; running: "Setting it up: step n of
  m.", the list naming the step; `following`: §4.3's sentence, and while no step of it was seen,
  "lmgw sees that it is still running. Its steps show here only when this account may read the
  system's log."; `checking`: "Checking this computer again…". "This keeps running if you close
  lmgw. Only turning the computer off can still interrupt it." shows from `authorized` or
  `following` on, while the steps run as the system task, and not once lmgw checks again after
  them; before that nothing runs as root. A pulse that never ends marks the phase (no fading, the
  WebKitGTK blink). Only the phase line is a live region: the steps and the output are not read
  out on every line.
- **D154 — Steps are the shown plan's, by position.** Step `n` of the events is the shown plan's
  `n`th bullet (the shell runs only that plan's hash); a step of another kind there (a setup
  followed from before) is named by its kind. A followed setup lists its steps only once one of
  them was seen (its steps come from the journal, which this account may not read), never as a
  list that waits forever. A step still running when the answer comes is done for `applied` and
  "didn't finish" otherwise, said beside the step's words; events after the answer change nothing
  and are not handed on (its `output` has every line). A run that began no step shows its answer's
  sentence alone.
- **D155 — An answer's tone.** `applied` and `nothing_to_do` green; the ends that changed nothing
  (`declined`, `unavailable`, `plan_changed`, `updated`, `dismissed`, `no_agent`,
  `not_authorized`, `ended`) amber; `read_differently`, `step_failed`, `refused`, `task_failed`
  and `failed` red. The sentence first, `detail` in "Details", `output` in "What the setup
  printed (n lines)"; the command's own `Err` reads "The setup didn't start: …". The answer stays
  until the next run or "Check again", on every page that shows the check (D160); "Check again"
  also closes an open settings switch and the `no_agent` terminal view, and waits while the
  switch's write is out, as the switch's Cancel does.
- **D156 — The channel is `@tauri-apps/api`'s, made by hand.** `transformCallback`, the string
  `"__CHANNEL__:" + id`, frames handed on in `index` order and the end counted after the last
  (`bridge::Ordered`, unit-tested); the callback is unregistered at the end, before the next run
  and when the component is cleaned up, never from inside its own call, and an event the page does
  not know is skipped. A message above 8 KB needs nothing of the page: Tauri's own injected code
  fetches it and calls the callback with `{ message, index }` like any other.
- **D157 — The settings switch's confirm: the sentence first, the fields folded.** The item's
  button (the action's `label`) opens it under the item. It reads like the rest of the check
  (revised after the review's M4): core's own sentence for the switch first ("Switching to the
  settings for AMD cards changes 5 settings. Once it has switched, it stops 'qwen-demo-27b' …
  Nothing is changed until it is confirmed.", which names the running models it stops; D165 for
  where it comes from), then Cancel and the label again to confirm with `confirm` = the changes
  as shown, then "What changes (n)", a fold with each change as its label (a model's own field as
  "Model 'id' (Chat) · Image override") over its old and new value (each run argument on its own
  line, as the field holds them; a model's `None` as "uses its class's setting" or "cleared, so it
  uses its class's setting") and "Left as they are:" with each one's why. `outdated`: an amber
  note with the answer's sentence and "What is different now (k)", a fold with "No longer part of
  the switch:" (struck through), "New in the switch:" and the models changed meanwhile, then the
  same button and the answer's list in "What changes (n)". Written: a green note with the sentence
  and the save's notes, and "What changed (n)" with every field and each running model's stop; the
  check it carries replaces the shown one, and the surface is told the settings were written
  (D164). Nothing to switch: a plain note, not a red one (D165). A refusal is a red note. Cancel
  and "Check again" wait while the write is out, so its answer is never dropped.
- **D158 — A "Not checked yet" item says what it is.** Core's sentence ("Not checked yet: restart
  first.") does not name the thing; the row puts the item's name in front ("Using the card for
  models · Not checked yet: restart first."), from a fixed name per item id.
- **D159 — The marks.** ✓ ok (green, drawn as an SVG: the dashboard's font has no "✓" and a
  browser's fallback drew a "V"), ! a problem (amber), i worth knowing (blue), – not checked
  (dashed). The headline's: `ready` ✓, `cloud_only` i, every `needs_*` !, and ! with "lmgw
  couldn't check this computer." when no check could be read at all (the reason in the red note
  under it). Under `cloud_only` the item that decided it is i too (D168). "Checked" names the day
  as well as the time ("Checked Thu 1 Jan at 12:00:00"). The shots are taken with
  `scripts/ui-drive.py --init-script scripts/drive/machine-fake.js` over
  `scripts/drive/machine-check.json`: the fake answers the two ops and, for a scenario in the app's
  window, stands in for the shell's bridge with scripted runs (one message delivered out of
  order); nothing of it is in the app.

*Draft calls from WP8a's review round (2026-11-03):*

- **D160 — The check and the run are the app's.** `widgets::machine_check::state::MachineState`,
  provided once in `App` (`provide_machine_state`), holds the shown check, its reads and the
  setup's run: `machine_apply`'s call, its channel callback and its answer outlive the page that
  pressed "Set it up". Every `MachineCheck` shows the same check and the same run (Settings →
  GPU's card, WP8b's surfaces); a page left while the setup runs shows it where it is when it comes
  back, and asks for no fresh check, whose plan would stand beside it; the answer of a run that
  ended meanwhile is there until the next run or "Check again". "Set it up" waits while a read is
  out, so the plan pressed is never one a read is about to replace. A host without the state (a
  test) gets one per component. A full reload still loses it: a page that loads anew learns of a
  running setup only by calling the command (D122), which the tray no longer causes (D162).
- **D161 — The live output follows its end while it is read.** When new lines come and the line
  before them was in view in the page's own scroller, the scroller moves down by what the new
  lines took; when the person scrolled away from the end (up, or past the run), it stays where
  they are, and following resumes once the end is in view again. It follows the output's end, not
  the page's: in Settings the run sits under the second item, and the page's end is the Hold card.
  The run's head (its title and the phase line with its pulse) sticks to the top of the page's
  scroller (`position: sticky`, inside the run's box) while the output grows past it, so "Setting
  it up: step 3 of 3." stays in view; no second scroller. A bounded box for the live output alone,
  following itself, would read like a terminal but break the one-scroller rule (ui-matrix's
  `nested_v`), so it is left to the owner's ruling.
- **D162 — The tray moves the page, it does not reload it.** `show_main_window_at` raises the
  window and evaluates a script (`move_js`) that fires a cancelable `lmgw-navigate` event with
  `{to}` on the page's `window`, at the gateway's origin only; the dashboard's `ShellNavigate`
  (`shell/moves.rs`, inside the router) takes a route of its own (a path, never `//host`),
  cancels the event and moves the router there, so the run, a draft and the session stay. When no
  page took it (the login page, a page still loading, an agent's app at another origin) the script
  loads the route as before, and when the script cannot be evaluated, `navigate` does. A draft the
  move would lose is asked about first, as a link is (`DirtyGuard::ask_before`). A click while the
  card is open already scrolls it in again (`ShellMoves`, which the Settings page's anchor effect
  tracks).
- **D163 — A browser is told where lmgw runs.** Outside the app's window the component's own
  sentences say "the computer lmgw runs on" where the app says "this computer" (the plan's intro,
  the headline while it checks, a read's failure); the password line is "Setting it up asks for a
  password on the computer lmgw runs on, once. If you're not an administrator there, someone who
  is has to type theirs."; the lines to type are "To type them in a terminal yourself on the
  computer lmgw runs on:" (and "Type these in a terminal on the computer lmgw runs on:" open). On
  a loopback address (`localhost`, `127.*`, `::1`), where the browser is on that computer, §2.7's
  sentence is "Open lmgw's own window (from your app menu) to set this up."; elsewhere it stays
  "Open lmgw on the computer it runs on to set this up.". Core's own sentences (the headline, the
  items) are unchanged.
- **D164 — A surface that shows the settings reads them again after the switch.** A written
  switch calls the component's `settings_written`; Settings passes one that reads
  `/api/settings-full` again and takes it with `FormState::refetched` (the kit): a field the
  person did not edit takes what the switch wrote, an edited one keeps the edit, and an edited one
  whose stored value the switch changed says under it, in amber, "The graphics card switch
  changed this setting after you edited it. Saving writes your edit over the switch's value.",
  until it is saved, discarded or no longer differs. A read that fails says in a toast to reload
  before saving.
- **D165 — The confirm reads the switch as it is now.** Opening it asks `machine_settings_fit`
  with `confirm` `None` (which writes nothing) for core's sentence and the switch as it is now, and
  shows that list (the action's until it comes); a read that fails leaves the action's list, with
  "lmgw changes n settings. Nothing is changed until you confirm." and the running models in the
  confirm's own words. An answer that wrote nothing and has nothing to switch, on opening or on
  confirming ("The model settings already are the ones for AMD cards."), closes the confirm into a
  plain note under the item and asks for a fresh check, which no longer offers the switch.
- **D166 — A search finds the card by one link.** The matching `Ctl::MachineLink` rows show as
  one, the first in page order, named by the rows whose own label has the searched words ("nvidia"
  → "NVIDIA is in This computer", "graphics card" → "Graphics card and Graphics card description
  (CDI) are in This computer"), else by the first match ("gpu" → "Graphics card is in This
  computer"). It no longer says "checked, and set up": the card may say neither. The search line's
  count counts it once.
- **D167 — `#machine` lands once the card has its content.** The anchor asks the card's row to
  scroll; the row's effect waits until the GPU category shows, the component is mounted and the
  check (or why there is none) is in, then, a frame later, scrolls the card's top, its heading
  included, to the top of the pane and rings it once (`mc-flash`, an accent box-shadow that fades
  to none: no opacity, the WebKitGTK blink). A timer before the read answered landed on a card of
  one line.
- **D168 — With no card, nothing reads as a problem.** Under `cloud_only` the item that decided it
  ("No graphics card found. …") gets the headline's i, not an amber !, since there is nothing to
  fix. Core keeps it a `problem`: its blame reads that state to name a local start's failure.
- **D169 — A copy the page may not make is said, and the lines selected.** "Copy the commands"
  checks for `navigator.clipboard` (absent over plain http from another computer) and waits for
  the write's answer; "Copied" only then. Refused or absent: "Not copied: <why>. The commands
  below are selected: press Ctrl+C to copy them.", and the lines are selected, as the kit's
  `copy_secret` says a refusal.

*Draft calls from WP8b, the other surfaces (2026-11-03):*

- **D170 — One rule for the surfaces that reach out.** §2.1 shows the check "while the verdict is
  not `ready` or `cloud_only`", §2.2 shows the banner "only when local models are involved". The
  Overview's card, the card above Chat's empty state and the Chat's banner take both: the verdict
  is a `needs_*`, a check is known, local models are involved, and no "Not now" holds for the
  check as it reads (D171; `widgets/machine_check/nudge.rs`). Whether local models are involved
  is the dashboard's to say from the admin plane's rows, not the Chat's from `/v1/models` (D184,
  revised in the review round: that list names an alias onto a local model as lmgw's own, leaves
  out a local row that is not public, and is empty when a provider's catalog could not be read).
  A setup of cloud models alone sees none of the three; Settings → GPU and the tray still say
  it. Picking a local model is the first case, since only a configured one can be picked.
- **D171 — "Not now" is one answer, stored, to the check as it reads.** Said on any of the three
  surfaces, it hides all three. It is kept in the browser's storage (`lmgw.ui.machine.not_now`,
  the app window's own WebKit storage in the app), so it holds across restarts, as the check's
  state it was said to: the verdict and each item that is a problem with its cause's kind
  (`needs_fix|card_for_models:toolkit_missing`), then `@` and the failed start the check carried
  then (D182: `2026-01-01 12:00:00 #1`, or nothing). "The check's state changes" is that key
  changing: another verdict, another problem, a problem gone. The check's time, its sentences,
  its detail and its plan's hash do not count, so a re-run that reads the same, a reworded
  sentence and NVIDIA's new key do not nag again. The stored answer goes the moment the shown
  check reads otherwise, so a state that changes and comes back is asked about again. A local
  start that fails because of the computer ends it, whoever asked for the start (a Chat turn,
  Kai, an OpenAI-only app, a lab, the model's Test): core stamps it on the check, and a check
  that carries another failed start than the one "Not now" was said beside reads otherwise
  (D182, revised in the review round: only the Chat's own `gpu_setup` card ended it before, so a
  "Not now" said once could stick for good). A "Not now" per document would ask again every
  morning, the kind of nag §2.2 keeps away from a cloud-only person.
- **D172 — The Overview's card is the first block of its state column.** Under the stat tiles and
  above Running models, inside the column that already gives way and scrolls, so a check taller
  than the window never pushes the live feed (its 200 px floor) off the screen; a strip above the
  tiles would. "This computer" heads it. Its component asks for a fresh run when the card shows
  (§2.6's "the Overview checklist mounting"), not when the Overview opens with nothing to show. A
  card that showed a setup's run stays for the rest of the visit, so its answer is read; a fresh
  run that reads `ready` before any setup hides it.
- **D173 — The card above Chat's empty state.** The same card stands above "Pick a conversation
  or start a new one." while no conversation is open, inside an element of the Chat's own
  (`.chat-empty-machine`, D191); the empty state becomes a column that scrolls under it (the one
  scroller there). The empty state takes the Chat's airy density, which it lacked: its "New chat"
  button had lost its height and padding outside `.chat-main`.
- **D174 — The banner links to the fix.** The banner stands under the Chat's head while a
  conversation is open: the problem mark, its sentence, its link and "Not now" at its end. The
  link goes to `/settings/gpu#machine`, which lands on the card (D167) and keeps a run going
  there when the person comes back (D160): the check is taller than the transcript can give up
  in place. Its words fit what it lands on (D185, revised in the review round: "Set it up" for
  every `needs_*` landed a restart on a card without a button): "Set it up" only where the plan
  has the helper's button in the app's window, "See why" after "Your computer needs a restart
  before local models can use the graphics card", "See what to do" otherwise. It steps aside
  while a `gpu_setup` card below holds the fix (D187).
- **D175 — Where "Not now" sits.** In the card's head, beside "This computer", whatever the plan
  is, and not while a setup runs: there it would hide the run and its answer (D190, revised in
  the review round: it stood beside "Set it up" when the plan had that button, below the fold of
  the Overview's state column, and stayed in the head while a setup ran). The banner has its own
  at its end; the line under a failure it explains has none (D188).
- **D176 — The `gpu_setup` card.** A turn whose error frame carries `gpu_setup` is a card of its
  own in the reply, not a "⚠" line in its text: the problem mark and the 503's sentence, "Details"
  with what follows it in brackets (the start's words, "(its start said: …)", Podman's included,
  and any fallback's clause, cut at the first " (" after a sentence's full stop, so a refused hop
  whose clause has no start's words folds too), and under it the compact component (D186). The
  card that holds the fix says only what failed, "'<model>' could not start on the graphics
  card." (D187); the others keep core's sentence without "Open lmgw there to see what to do.".
  Only the newest such card in the window carries the fix, so a conversation tried three times
  shows one check. The component asks for a fresh run (core checked again after the failed
  start, D61 joins it). In a browser it is the same component with §2.7's sentence where the
  button would be. A continued reply that fails so is a toast, as every failed continuation is.
  (Revised in the review round: the sentence was core's whole, and the cut looked for "(its
  start said:" alone.)
- **D177 — The Chat's machine slot.** The Chat does not read the check: its host hands it a
  `pages::ChatMachine` in context, as it hands it `ChatLive`. The slot holds the host's banner
  (given whether a `gpu_setup` card below holds the fix), its empty-state card, the fix under a
  `gpu_setup` card, and whether the page is on the computer lmgw runs on (the card's words,
  D187); the host decides whether local models are involved (D184), the Chat how the card reads
  the message, which card is the newest, and puts the host's card in an element of its own
  (`.chat-empty-machine`, D191). The type and the Chat's side are in `pages/chat_machine.rs`,
  which goes with the Chat in the crate move (chat-plane design WP3b), with its rules in the
  Chat's part of `app.css`; the dashboard fills it in `App` with
  `widgets::machine_check::chat_slot()`. The Chat's own files changed by a field on `Msg`, the
  card in `MsgView`, the banner under the head, the card in the empty state and the error
  frame's branch. (Revised in the review round: the Chat decided involvement from `/v1/models`,
  and the slot carried a failed start back to the host.)
  Since chat-plane WP3b the Chat side lives in `crates/lmgw-chat-ui` (`src/chat_machine.rs`, its
  rules in the crate's `chat.css`), and the slot is the host's field `ChatHost::machine`, which the
  dashboard's `/chat` route (`chat_host.rs`) fills with `chat_slot()` (chat-plane D31, D44).
- **D178 — A host without the slot.** No banner, no card above the empty state, and the
  `gpu_setup` card is the sentence alone with its "Details": the 503's own "Open lmgw there to see
  what to do." says where to look. The debug build's test host (chat-plane design WP3a,
  `test_host.rs`, `localStorage["lmgw-test-host"] = "relay"`) hands no slot, beside its relay
  transport and without `ChatLive`, so the drive sees such a host; a release build always hands
  it.
- **D179 — The other places a model start's failure is shown.** The model editor's Test card,
  under a test that failed with `gpu_setup`'s code (D189), and an Image or Audio lab run that
  failed with it, show the banner's line inside the card (`MachineInCard`) while the check says
  the card is not ready, whatever "Not now" holds, with no "Not now" of its own (D188). Any other
  failed test, a row on the processor and the op's own refusal (a 401, a timeout) show none.
  Such a failure ends "Not now" through core's stamp (D182). (Revised in the review round: the
  line stood under any failed test while the verdict was `needs_*`, and it was the banner, so
  "Not now" hid it and its own "Not now" silenced the Overview and the Chat.)
- **D180 — The fallback reasons in words.** Where the dashboard says why a fallback answered,
  `gpu_setup` is "graphics card not ready" and `engine_download` "engine download failed", as the
  Traffic badge already says them: the Decision lab's chip and the voice library's provenance take
  Traffic's words (`traffic::fallback_words`), the Voices lab's line says "the graphics card isn't
  ready for local models" and "the model's engine could not be downloaded". The Chat's "(fallback
  for …)" names no reason: its `done` frame carries none.
- **D181 — The run's sticky head per surface.** `--mc-stick-gap` is inherited from the scroller
  the component sits in: Settings' 14 px by default, the Overview's state column 0, the Chat's
  transcript and its empty state 18 px (set in the Chat's own rules since the review round,
  D191), so the head of a setup's run sticks flush with each.

*Draft calls from WP8b's review round (2026-11-03):*

- **D182 — A failed start is stamped on the check, whoever asked for it.** When a start on the
  graphics card fails and the machine check names this computer as why (WP5's blame path,
  `machine::after_failed_start`, which every start path goes through: a client's request, a
  turn, a lab, the model's Test, a warm), core stamps it on the check:
  `MachineCheck.failed_start` and `MachineSummary.failed_start`, `{ model, class, at, count }`
  (`at` UTC like `checked_at`, `count` the failed starts so stamped since lmgw started, so two in
  one second are two). The last result carries it at once and every result published after it
  too (the stamp is held across a publish, so none is lost between them); a restart of lmgw
  forgets it. "Not now" remembers the failed start the shown check carried when it was said, on
  lmgw's clock, never the browser's, which may be another computer's; a check that carries
  another one ends it (D171). The Chat's own "start failed" call to the host is gone: the fix
  under the card asks for a fresh run, whose answer carries the stamp, so it added nothing. A
  resolve-time refusal of a hop with no route (`gate/chain.rs`) starts nothing and stamps
  nothing yet.
- **D183 — The live feed says when the check changed.** `/api/events` carries a `machine` frame
  with the summary: on connect once a check finished, whenever the last result changes (a run's
  publish, the settings judged again, D135, a stamp, D182), and after the stream lagged. The
  app's machine state (`MachineState::follow`) reads the last result again (`fresh: false`, no
  run) when the frame is not the summary of the check it shows, once per summary, never while a
  read or a setup is out (it looks again when that ends), and not before any surface showed a
  check. A dashboard left open on the Overview so shows a driver update that wants a restart, or
  a failed start from Kai, without a Chat turn failing first. Chosen over a cached read on every
  mount of the surfaces, which the review offered as the alternative: a read per mount does not
  reach a page that stays open, which is the case the review named.
- **D184 — Whether local models are involved, from the admin plane's rows.** The dashboard reads
  `/api/models/full` and `/api/upstreams` (`widgets/machine_check/involved.rs`, one read at a
  time, again each time a surface that reaches out mounts) and hands the answer to every
  surface, the Chat's through its slot. Involved: a local model that runs on the graphics card
  is configured and on (a chat or image row, an aux row without `backend: cpu`, an audio row
  whose `runs_on` is the card), whether clients reach it by its own name or only through an
  alias, a candidate alias or a fallback; or nothing is configured at all, no local row, no
  alias and no upstream (a first launch). Rows on the processor alone, rows that are off, and
  cloud upstreams alone, a provider whose catalog could not be read included, are not involved.
  `/v1/models` is no longer read for it: it names an alias onto a local model as lmgw's own,
  leaves out a local row that is not public, and is empty when a catalog failed.
- **D185 — The banner's words fit what its link lands on.** "Set it up" only where the plan has
  the helper's button with steps in the app's window and no setup is out; a restart reads "Your
  computer needs a restart before local models can use the graphics card · See why"; everything
  else "Your graphics card isn't ready for local models yet · See what to do". A browser not on
  a loopback address names the computer: "The graphics card of the computer lmgw runs on isn't
  ready for local models yet", "The computer lmgw runs on needs a restart before local models
  can use its graphics card" (D163). The line inside a card has the same words (D188).
- **D186 — A compact surface folds what is no problem.** The Overview's card, the card above
  Chat's empty state and the fix under a `gpu_setup` card pass `compact`: the items that are a
  problem show, the fix under §1.2's, and the others (ok, worth knowing and not checked yet)
  fold into "What else was checked (n)" after them, so the problem and "Set it up" come first in
  the Overview's small state column and Running models moves up. The plan block there puts
  "Set it up" (or §2.7's sentence) right after the bullets and the password line, and "After
  that, …" and the package-scripts line after it: folding the items alone left the button under
  the column's fold at a 1536x864 viewport at 125 % scale, and the shell's own dialog says every line again before
  anything runs. Settings → GPU shows every item and the block in D148's order. The items not
  checked yet fold too, beyond the review's "ok and info": "Not checked yet: restart first." is a
  consequence of the problem shown, not a second thing to do.
- **D187 — One problem said once where the fix is.** While the newest `gpu_setup` card holds the
  fix on the page, the banner above steps aside (the slot tells it), and that card's sentence is
  core's first sentence alone, "'<model>' could not start on the graphics card." ("of the
  computer lmgw runs on" kept in a browser on another computer; "'<model>' could not start: no
  graphics card was found." for no card at all); the check under it says why and what to do. An
  older card keeps core's sentence without "Open lmgw there to see what to do."; a host without
  the slot keeps it whole. Its shot had the problem four times with two calls to act: the
  banner, the card's sentence, the component's headline and §1.2's item; the headline and the
  item stay, the component's own.
- **D188 — The line under a failure is that failure's explanation.** `MachineInCard` (the Test
  card, the Image and Audio labs) shows the banner's words while the check is a `needs_*`,
  whatever "Not now" holds, and has no "Not now": waving away the reason for the person's own
  failed test is not waving the check away on the Overview and in the Chat. It reads the last
  result when it mounts and follows the live feed (D183), so a check made stale by the failure
  is read again.
- **D189 — A failed test carries its code.** `ModelTestFailed.code` is the gateway's error code
  when lmgw's own error said why (`GatewayError::kind`): a start that failed at admission is now
  a failed test with its code, the container's log and the hint (it was the op's own error, in
  words), the probe's own `GatewayError` keeps its code (a dead container's restart the check
  blamed), and the image test takes the `/v1` envelope's code. The Test card shows the line for
  `gpu_setup` alone. A ladder's rungs carry no code yet.
- **D190 — "Not now" in the card's head, not while a setup runs.** The cards have it in their
  head always (D175), hidden while `machine_apply` is out; the component's plan block has none,
  so `MachineCheck` takes no `not_now`. The review found it below the fold beside "Set it up" in
  the Overview's state column, and hiding the run and its answer when pressed mid-setup.
- **D191 — The Chat's seam, ready for its crate.** The rules the Chat owns (its empty state's
  column, the box the host's card fills, the transcript's `--mc-stick-gap`, the `gpu_setup`
  card) are in the Chat's part of `app.css`, keyed on `.chat-empty-machine`, an element the
  Chat's `EmptySlot` draws around the host's card, never on a host class; `machine.css` keeps
  the host's card, banner and line. The involvement helper is the host's
  (`widgets/machine_check/involved.rs`), so neither the Overview nor the Chat imports it from the
  other, and the Chat no longer reads the kit catalog for it (`ModelCatalog::fetched` is gone
  again).
  Since chat-plane WP3b those rules are in `crates/lmgw-chat-ui/assets/chat.css`, and the banner
  takes its sides from the Chat's `--chat-side` gutter, narrower in the Chat's narrow layout
  (chat-plane D43).
- **D192 — A `gpu_setup` turn on a thread that is not on screen.** Its card never mounts, and
  the failed reply is not stored (a fresh reply that failed with nothing is no row, as for every
  failed turn), so reopening the thread shows the question without an answer and no card. What
  ends "Not now" is core's stamp, not the card (D182), so the banner says it when the thread is
  opened again, while the check still needs something. Whether such a reply should be stored
  with its error is a question for the Chat's design, not this one.

## 1. What is checked

The check runs in `lmgw-core` with the user's rights. It only reads. The items run in this order,
and an item whose prerequisite failed reports "not checked yet" with that reason, rather than
inventing its own failure. Each item has a plain sentence (what a newcomer sees), a detail fold
(what was read, where from, with the technical names), and a typed cause the surfaces and the plan
key on. Figures in the example sentences are illustrative, except where they are marked as
measured or checked. In the server image (container mode) the reader is the server's administrator,
and the sentences name the lmgw container, the host and its service where that is what to act on
(headless server design §7, D30).

### 1.1 Graphics card and driver

Read from: the PCI devices in sysfs (`/sys/bus/pci/devices/*/{class,vendor,device,driver}`,
display classes `0x03xxxx`; names from hwdata's `pci.ids`); the running kernel module's version in
`/proc/driver/nvidia/version`; the userspace driver's version on disk (`readlink -f
/usr/lib64/libnvidia-ml.so.1`, which names `libnvidia-ml.so.<version>`, and the owning
package's version through `rpm -qf` where rpm has it); NVML in a **separate process**; the Secure
Boot variable in efivarfs (world-readable on Fedora); and amdgpu's sysfs counters through the
existing `AmdSysfsProbe`.

**Why the versions come first, and NVML runs in a subprocess.** A process that has loaded
`libnvidia-ml` keeps the old library mapped after a driver update. That old library matches the
old kernel module, which keeps running until the next boot, so an in-process NVML call answers as
if nothing changed for as long as lmgw runs. So the check never asks lmgw's own NVML handle (the
ledger's is left alone). It compares the kernel module's version with the libraries on disk. While
they differ, the answer is "restart the computer", and lmgw neither probes NVML nor writes a spec
(§3.6.2). Only when they agree does it run lmgw's own executable in a probe mode (`lmgw
--machine-probe nvml`, which loads NVML, prints JSON and exits), so the probe maps what is on disk
now.

| Cause | Plain sentence | Detail (examples) |
|---|---|---|
| ok, NVIDIA | Graphics card · NVIDIA GeForce RTX 4080 · 16 GB | kernel module 580.xx (open) · libraries 580.xx · driver from RPM Fusion (`xorg-x11-drv-nvidia-580.xx`) · NVML, separate process |
| ok, AMD | Graphics card · AMD Radeon 780M · shares 16 GB with the computer | amdgpu sysfs, GTT/VRAM figures as the probe reads them · `/dev/kfd` there or not (Vulkan engines compute without it, ROCm ones need it, D137) |
| none | No graphics card found. Cloud models work; local models need a graphics card. | PCI display devices: Intel UHD 770 (i915) |
| NVIDIA, no NVIDIA driver | Your NVIDIA card is using the basic built-in driver. Local models need NVIDIA's own driver. | PCI 0000:01:00.0 10de:2704 bound to `nouveau` · how Fedora installs it (§3.7) |
| driver updated, not restarted | Restart the computer to finish the graphics driver update. | kernel module 580.10 (`/proc/driver/nvidia/version`) · libraries on disk 580.20 (`libnvidia-ml.so.1` → `libnvidia-ml.so.580.20`) |
| driver installed, not running | The graphics driver is installed but not running yet. It may still be getting ready for this computer: wait a few minutes, then restart the computer. (A module built after this boot: "The graphics driver is ready but not running yet. Restart the computer to start it.") | `akmod-nvidia` installed, no `nvidia.ko` for kernel 6.x yet, or one put there after this boot started · Secure Boot on or off |
| … with Secure Boot on, and a module built before this boot (D58) | The graphics driver is installed, but Secure Boot stops it from loading. It needs a one-time key enrolment. | SecureBoot efivar = 1 · `nvidia.ko.xz` for the running kernel, put there before `btime` · RPM Fusion's Secure Boot steps (link) |
| libraries' version not readable (D26's machines) | as NVML says: ready, or the restart on its `version_mismatch` (D59) | kernel module 580.xx · `libnvidia-ml.so.1` → a name without a version · NVML, separate process |
| driver libraries missing | A part of the graphics driver that local models need is missing. Here's how to add it. | no `libnvidia-ml.so.1` · `xorg-x11-drv-nvidia-cuda-libs` not installed (instructions, not yet a helper step) |

The last row is rare. `xorg-x11-drv-nvidia` Recommends `xorg-x11-drv-nvidia-cuda-libs` (which has
`libnvidia-ml` and `libcuda`) and Requires `nvidia-modprobe` (checked on a 580 driver), so a default
driver install has everything ruling 4 needs. Only an install with weak dependencies switched off
lacks the libraries. RPM Fusion's `xorg-x11-drv-nvidia-cuda` (`nvidia-smi`, and with it
`nvidia-persistenced`) is not needed and not checked.

Several cards: every one is listed, and the one lmgw measures (NVML before amdgpu,
`detect_probe`'s order) is marked.

### 1.2 Using the card for models (NVIDIA)

This is the item the root helper exists for. Read from: `rpm -qf /usr/bin/nvidia-ctk` (the owner),
that package's version and header signature key ID, the repository facts of §3.2, lmgw's own spec
and its stamp (§3.6), and, for information, the system's CDI dirs.

**The toolkit, by owner** (D24). Every toolkit decision keys on who owns `/usr/bin/nvidia-ctk`,
never on file presence alone or on `rpm -q` of a package name:

| Owner of `/usr/bin/nvidia-ctk` | Judged |
|---|---|
| none, and the file is absent | toolkit missing: the plan installs it (S1 or S1′, S2, S4) |
| NVIDIA's `nvidia-container-toolkit-base`, header signed by key `ddcae044f796ecb0` | NVIDIA's. Ok while lmgw's spec generates. If generation fails, the plan offers S4′ |
| a package the provider table names for this system (none yet, §3.2) | the distribution's. Ok while it generates; updates come with the system's |
| Fedora's retired `golang-github-nvidia-container-toolkit` (for example 1.17.4, left installed by an upgrade to Fedora 44) | works: ok, with an info line and S4″ offered. Does not work: a problem, with S4″ |
| any other package, or no package (a file put there by hand) | used as it is while it works. Otherwise instructions that name the owner. lmgw never overwrites it |

There is no version floor (D24): the 1.18.0 floor of the first draft came from NVIDIA's refresh
units, which ruling 4 no longer uses. 1.20.1 is the only version checked.

**How lmgw's spec is judged.** lmgw parses its own file, and every `hostPath`, device `path`, hook
`path` and mount source must exist. Its stamp must match the current kernel module version,
userspace version and library file, `nvidia-ctk` file, rpm database change time, `/dev/nvidia*`
nodes and boot (§3.6.1, §3.6.2). Normally lmgw keeps this true by itself, so a stale spec is never
a problem a person sees. They see one only when generating failed. Without the toolkit, or with a
Podman that has no `--cdi-spec-dir`, there is no spec of lmgw's to judge: its containers use the
system's (D26), and the toolkit cause says so.

**The system's specs, for information and for rows that need them.** Podman's default spec dirs
are `/etc/cdi` then `/var/run/cdi` (containers.conf's `cdi_spec_dirs`; an override in
`containers.conf` or a `containers.conf.d` drop-in is read when one is set). A device name defined
in a later dir wins over an earlier one. Two files in the *same* dir defining the same name are a
conflict, and the CDI library then drops the name entirely, even when a later dir defines it too
(`container-device-interface` `pkg/cdi/cache.go`, `refresh`: the conflict set is purged after every
dir is read; read on `main`, 2026-11-03). The check resolves them this way only for the detail line
("your own containers use `/var/run/cdi/nvidia.yaml`, which lmgw's models don't read") and for a row
that names a CDI device of another kind (D21, §3.6.3). This is also why the README's `/etc/cdi`
command goes: that copy is never refreshed, goes stale with the next driver update, and is what
Podman uses on any boot where NVIDIA's refresh failed.

| Cause | Plain sentence | Detail |
|---|---|---|
| ok | Models can use the graphics card. | toolkit 1.20.1 (`nvidia-container-toolkit-base`, NVIDIA's repository) · lmgw's own description, written <time> for driver <version> · <n> paths present · your own containers use `/var/run/cdi/nvidia.yaml`, which lmgw's models don't read |
| toolkit missing | One piece is missing so models can use the graphics card. lmgw can set it up. | NVIDIA Container Toolkit not installed (`/usr/bin/nvidia-ctk` absent); it comes from NVIDIA's repository (§3.2) |
| toolkit from Fedora's retired package (info while it works) | One piece comes from a package Fedora no longer updates. lmgw can switch it to NVIDIA's. | owner `golang-github-nvidia-container-toolkit-1.17.4` · retired, Fedora bug 2434398 |
| description can't be written, the toolkit's fault (D57) | lmgw couldn't write the description of your graphics card that models use. lmgw can update the piece that writes it. (Or, for an owner lmgw doesn't manage: "Updating the piece that writes it from where it came from usually fixes this.") | `nvidia-ctk cdi generate` exit 1 · its last lines, or the paths its spec names that are not there · owner and version |
| writing it ran out of the load timeout (D57) | Your graphics card stopped answering while lmgw prepared it for models. Restart the computer. | the program and how long it ran · a longer Settings → GPU → Admission → Load timeout gives a slow card more time |
| a folder of lmgw's is not private, or lmgw's files fail (D57) | lmgw keeps a file about your graphics card in a folder only you may open, and that folder is open to others. … / lmgw couldn't save a file about your graphics card. The details say why. | the folder and its mode, or the file error; no plan |
| another repository offers the toolkit (D56) | One piece is missing so models can use the graphics card. Here's how to add it. | the repository and package from the user's cached metadata; the plan installs from it alone |
| NVIDIA's repository not loading (info) | lmgw's graphics card piece hasn't been able to get updates since October 14, 2026. | §3.2's frozen-section reading: `updates` loaded 2 Nov, NVIDIA's section last 14 Oct, its `metadata_expire` 48 h |

When 1.1 is not ok, this item says "not checked yet: the driver comes first". While the driver
versions differ, it says "not checked yet: restart first", and lmgw keeps its last spec untouched.

### 1.3 Running models (Podman)

Read from: `podman --version`, `podman --help` (for the `--cdi-spec-dir` option), then `podman
info --format json` (rootless, `graphRoot`, cgroup version, OCI runtime), and the user's
`/etc/subuid` and `/etc/subgid` ranges when `podman info` fails.

- ok: "lmgw can run models on this computer." Detail: Podman 5.8.9, rootless, crun, cgroup v2,
  storage `~/.local/share/containers/storage`.
- missing (possible with the AppImage only, since the RPM depends on it): "lmgw needs one more
  program to run models." Detail and instructions: install `podman`.
- broken: "lmgw can't start models on this computer yet." Detail: Podman's own error, whole.
  Missing subordinate ID ranges are named, since accounts created by a directory service often
  lack them (instructions only, not yet a helper step). The account's name for that comes from the
  user database (`getpwuid`), so an SSSD or LDAP account, which is not in `/etc/passwd`, counts.
- too old for lmgw's own spec: "lmgw needs a newer version of one program to run models." Detail:
  the version, and that `podman --help` lists no `--cdi-spec-dir`. It is judged from the help
  text, not from a version number lmgw would have to guess.

### 1.4 Model settings fit the card

Read from: each class's run arguments and image in the settings (chat, aux, audio, image), plus
each model row's own override; the cards on the bus (`fit::present`: an AMD card on amdgpu with
render nodes, and its `/dev/kfd`); what an image is built for (its tag for the engines' published
images, else its build label or its libraries, D139); and the running models. It compares the
device flag those settings pass (`nvidia.com/gpu=…` or `--gpus` for NVIDIA, `/dev/kfd` for AMD's
ROCm, `/dev/dri` for a Vulkan engine) with the cards of the test set (D137), and the image: one built for
CUDA runs on no AMD card, whatever device it is given (D118). `/dev/dri` alone is the render
nodes a Vulkan engine uses on any vendor's card: it fits an AMD card (the Vulkan setting of
`docs/amd-arch.md`), and on an NVIDIA card without an AMD card beside it it is a detail line
saying it lacks NVIDIA's libraries (D51). The item is judged again on every published snapshot,
from the last run's reading (D135).

- ok: "Model settings fit this card." (no detail beyond the classes checked)
- NVIDIA settings on an AMD card: "lmgw is set up for NVIDIA cards. It can switch to the settings
  for AMD cards." A problem with a button, `needs_fix` (D110), and no password (D18), when the
  settings as the switch leaves them fit. The item's `action` is the switch (§4.2): the button's
  words ("Switch to the settings for AMD cards"), every field it changes as it is and as it would
  become, by the dashboard's labels (D119), what it leaves and why, and the running models it
  stops (D136). The detail says the same in lines ("Settings → Runtimes → Chat → Image:
  ghcr.io/ggml-org/llama.cpp:server-cuda → ghcr.io/ggml-org/llama.cpp:server-vulkan"). The values
  are D111's; which classes and rows change is D112's and D113's, and how run arguments are
  swapped D138's.
- … when the switch leaves something that still keeps models from running (an image of the
  owner's built for CUDA, arguments with a group of the owner's): "Some model settings are for
  NVIDIA cards, and lmgw can't switch all of them to the settings for AMD cards. The details say
  which to change by hand.", `needs_steps`, with the action for the part it does (D110).
- AMD settings on an NVIDIA card (`/dev/kfd`, the ROCm values of `docs/amd-arch.md`) with no AMD
  card beside it: "lmgw is set up for AMD cards. It can switch to the settings for NVIDIA cards.",
  the same way, to lmgw's defaults.
- lmgw's own AMD values on an NVIDIA card (the render nodes with an image built for AMD cards),
  only a disabled row set for the other card, or settings for an AMD card beside the NVIDIA one:
  information, "Some model settings are for AMD cards. lmgw can switch them to the settings for
  NVIDIA cards.", with the same action; the verdict stays as it is (D110, D137).
- models a confirmed switch could not stop (answering a request): "Some models still run with the
  settings from before the switch (busy). They use the new settings once they stop.", a problem,
  `needs_steps`, while each is the same container (D136).
- an Intel card, or no card: not checked (1.1 found no card lmgw has settings for), and no switch
  is offered.
- no device flag at all on a class or row: an info line, not a problem ("Speech models are set to
  run on the processor"), since CPU rows are a choice (`backend: cpu`); beside the switch's
  information it follows that sentence.
- a row naming a CDI device that is not `nvidia.com/gpu=…`: an info line naming the system spec
  that defines it, since that row reads the system dirs (D21).

### 1.5 Room for models

Read from: `statvfs` of the models directory (configured, or the default the proposal's part A
introduces), and of Podman's `graphRoot`. One line when both are on one filesystem, two otherwise.

- "Room for models · 412 GB free." Detail: both paths and filesystems.
- Low: shown only when the free space is less than a figure lmgw actually knows: what the download
  for the card still fetches (1.6, D62), against the image store's file system. A model about to be
  fetched is not compared yet. "Space is running low: 9 GB free. The download for your graphics
  card needs 4.4 GB." Low space never makes the machine "not ready", and lmgw invents no threshold.
- When `statvfs` failed for every folder, the line says "lmgw couldn't read how much room is left.
  The details say why." rather than that no folder is set.

### 1.6 The download for your card (info)

Read from: `podman image exists -- <image>` (WP10's `pull::presence`) for each image the class
settings and the enabled rows name; for a missing one, its registry manifest and the image store's
layer list (D62), or the bytes of its download when one runs.

- "The download for your graphics card is done." or "Before the first model runs, lmgw downloads
  what your graphics card needs to run it (1.6 GB)." The size is the compressed blobs of the
  missing images' manifests that the store does not have yet, each once (D62). While a download
  runs: "lmgw is downloading what your graphics card needs to run models · 2.2 GB to go." When a
  manifest can't be read, the line gives no size rather than a guess, and that image's detail line
  says "size not known — <why>". The detail lists the image references. With D16, the line will
  also say when that download happens (with the next model download, or now, once the card is
  ready); that is not built yet.

### 1.7 What "ready" means

One headline verdict, typed:

| Verdict | When | Headline |
|---|---|---|
| `ready` | 1.1 ok, 1.2 ok (NVIDIA) or render node usable (AMD, `/dev/kfd` not needed, D137), 1.3 ok, 1.4 ok | This computer is ready for local models. |
| `needs_fix` | something §3 can fix with a button here | One thing is missing so models can use the graphics card. lmgw can set it up. |
| `needs_steps` | the same, but only by instructions here (§3.7) | One thing is missing so models can use the graphics card. Here's how to add it. |
| `needs_restart` | 1.1 driver updated or not running | Restart the computer to finish the graphics driver update. (or the not-running sentence) |
| `needs_driver` | 1.1 NVIDIA card without NVIDIA's driver | Your NVIDIA card needs NVIDIA's driver before it can run models. |
| `cloud_only` | 1.1 none | No graphics card found. Cloud models work; local models need a graphics card. |

Room (1.5) and the download (1.6) are information. They never change the verdict. A plan can exist
next to `ready`: the retired-package switch (S4″) is offered while that package still works. The
headline of a `needs_*` verdict is the sentence of the item that decided it (D50).

An optional proof, "Try the graphics card", is offered only when the chat class's image is already
on the machine: one throwaway `podman --cdi-spec-dir <lmgw's dir> run --rm --pull=never
--network=none --device nvidia.com/gpu=all --security-opt label=disable --entrypoint
/app/llama-server <image> --list-devices`, which lists the CUDA devices the engine can use. The
first draft used the host's `nvidia-smi`, which the spec mounts when it exists, but a default
driver install has no `nvidia-smi` (it is in `-cuda`, which lmgw no longer installs). This runs
through the existing `Registry::run_throwaway` (`runtime/registry/podman_ops.rs:360`), which carries
the CDI option (§3.6.4). It never pulls an image just to test.

## 2. Where it shows

One component (§4.4) shows in several places. It is the same at every depth, and only its detail
folds open or close.

### 2.1 First launch

The window opens at every launch (`src-tauri/src/main.rs:756`), and the check has run by then
(boot, §2.6). While the verdict is not `ready` or `cloud_only`:
- the Overview's welcome checklist (proposal part B) has the check as its first block. Until part
  B is built, the Overview shows the check as a card at its top;
- a person who goes straight to Chat meets it in Chat's empty state (proposal part J), above
  "Which model would you like to chat with?". Choosing a local model there while the verdict is
  `needs_*` shows the same block before the download starts.

Built in WP8b (`widgets/machine_check/{surfaces,nudge,involved}.rs`, `pages/chat_machine.rs`).
§2.1's "while the verdict is not `ready` or `cloud_only`" and §2.2's "only when local models are
involved" are one rule for the three surfaces that reach out (D170): a `needs_*` verdict, local
models involved (from the admin plane's rows: a local model on the graphics card configured and
on, by its own name or only through an alias, or nothing configured at all yet, which a first
launch is, D184), and no "Not now" for the check as it reads (D171). Until part B is built, the
Overview's card is the first block of its state column, under the stat tiles, "This computer" at
its head; its component asks for a fresh run when the card shows (D172). Until part J is built,
the card above Chat's empty state stands over "Pick a conversation or start a new one.", and the
empty state scrolls under it (D173). Both are compact, the problem and its fix first and the
rest folded (D186), with "Not now" in their head (D175, D190). A check that changes while the
window stays open (a driver update that wants a restart, a failed start from Kai) shows through
the live feed's `machine` frame without a reload (D183). Choosing a local model before its
download has no block of its own yet: only a configured model can be picked today.

### 2.2 Chat

A one-line banner, "Your graphics card isn't ready for local models yet · Set it up", appears in
the Chat only when local models are involved: a local model is configured, or the person is picking
or downloading one. "Not now" hides it until the check's state changes or a local model start
fails. A cloud-only person with an NVIDIA card and no toolkit is not nagged.

Built in WP8b (`MachineBanner`, D174, D171): the banner stands under the Chat's head while a
conversation is open, and links to `/settings/gpu#machine` in words that fit where it lands
("Set it up" only beside the helper's button in the app's window, "See why" after a restart's
sentence, "See what to do" otherwise; a browser on another computer names the computer lmgw
runs on, D185). It steps aside while a `gpu_setup` card below holds the fix (D187). "Not now"
is stored in the browser as the check's state it was said to (the verdict and each problem with
its cause) and the failed start the check carried then, so it holds across restarts until the
check reads otherwise or a local model's start fails because of the computer, whoever asked for
it (core stamps that start on the check, D182). "A local model is configured" is the admin
plane's rows, not `/v1/models` (D184); "picking or downloading one" is today "a local model is
configured, or nothing is": only a configured model can be picked, and the download pages have
no banner yet. The Chat reads none of it itself: its host hands it the banner through a slot
(D177), and a host without one (a paired device's sidebar) shows none (D178).

### 2.3 Settings → GPU, the permanent place

A new first card in Settings → GPU, "This computer", has the full check, "Check again" and the
fix. The deep link `/settings/gpu#machine` is what every other surface links to. Settings search
matches the page's field rows (`pages/settings.rs`, `Page::matches` over `fields`), and the card is
not a field, so it gets search rows of its own that point at `#machine`, with human labels:
"Graphics card", "Graphics driver", "NVIDIA", "Container toolkit", and "Graphics card description
(CDI)".

Built in WP8a (`pages/settings/{table,rows,render,draft}.rs`): the card is the GPU category's
first, across its columns (`Group::wide`), its row a `Ctl::Machine` that a search never shows; the
five search rows are `Ctl::MachineLink`s, shown only in a search and as one link, "NVIDIA is in
This computer" (D151, D166). `#machine` scrolls the card's top into view once it has its content
and rings it (D167), which is where the tray's item lands. The component mounts the first time the
GPU category is shown and asks for a fresh run then (D150), since every category stays mounted;
after the settings switch writes, the page reads the settings again (D164).

### 2.4 The tray, when the card stops working later

When the verdict goes from `ready` to anything else (typically a model start fails after a driver
update that wants a restart, §2.6), the tray menu gains "Graphics card needs attention…", which
opens the window at `/settings/gpu#machine`. Its status line and tooltip name the plain sentence.
The icon stays as it is: the hold already owns the icon's second state. A desktop notification
does not exist yet (lmgw sends none today).

Built in WP9 (`src-tauri/src/machine_tray.rs` for the decision, the glue in `main.rs`): the tick
that already refreshes the status lines reads `state.machine.summary()` (D108) and the item is
added to and removed from the menu with it. "From `ready`" means a `ready` seen since lmgw
started (D105); any other verdict then shows it (D106); the sentence ends the status line and the
tooltip (D107); a click raises the window and navigates it (D109), inside the page since WP8a's
review round: the dashboard takes the move without a reload, so a running setup's progress stays
(D162).

### 2.5 Model starts that fail because of it

When a GPU model's container start fails, the start path asks the check (D63): its last verdict
while the machine is as it read it, else §1.1 and §1.2 read again alone, with the full check after
it in the background (§2.6, D96). When §1.1 or §1.2 is a problem for what that row's container is
given (D64: the NVIDIA card, AMD's compute node, the render nodes alone), that is the reason (built
in WP5: `vram/start_failed.rs`, `machine/setup.rs`, `machine/blame.rs`, `gate/machine_setup.rs`):
- With a fallback configured for the model or alias, the fallback answers (D23), as for every other
  reason. The response carries `x-lmgw-fallback` and `x-lmgw-fallback-reason: gpu_setup`
  (`FallbackReason::GpuSetup`), handed on at admission (`gate::machine_setup::hand_on`, from
  `admit_route`, a plain model's guest and a candidate alias's walks, D70, D71): nothing has
  reached the client yet, as with `unreachable`. The header's enum and description have it, and
  so does the request log's `fallback_reason`.
- While the last check names such a cause, a GPU hop that has a fallback and is not up is
  swapped at resolve time in `gate/chain.rs` `stop_at`, as under the hold, so no start that is
  bound to fail is tried first — only for a cause a change to what the check read before anything
  else would clear (the driver, the toolkit, D95), and only while the machine is as that check read
  it (D66). A hop without a fallback, or whose fallback cannot answer (D67), still tries the start,
  because the verdict can be out of date (someone fixed it in a terminal a minute ago), and so does
  a hop whose cause the swap cannot tell is still so (a folder that is not private, no card at
  all): the failed start is blamed again. Rows on the processor are never swapped, a start that
  brings a model up while the check said it could not checks again (D68), and "Check again" writes
  lmgw's spec again first when its last write failed (D95).
- A caller pinned to the model it named (quickdoc's embedder and reranker, ingest's token count,
  the upscale) is never swapped for the verdict: its start is tried, and it gets the 503 below
  (D97). quickdoc's refusal of a fallback's answer says why that fallback answered, never a hold
  that is not on.
- Without a fallback, `/v1` answers 503 with `code: "gpu_setup"` (`GatewayError::GpuSetup`):
  "'<model>' could not start on the graphics card of the computer lmgw runs on.", the item's
  plain sentence, and "Open lmgw there to see what to do." when there is something to see there
  (D65). This is an lmgw code, like `gpu_hold`, and it is in the error-code list of the API
  reference (`openapi/v1/errors.rs`, the OpenAI and llama.cpp shapes) and in `/v1/models`' notes.
  It is **not** a deliberate refusal, so it is not in `telemetry::REFUSAL_KINDS`; the usage page
  counts it as a failure.
- A failed engine download (§5) hands on the same way, reason `engine_download`, and answers its
  own 502 code `engine_download` without a fallback (D69).
- A Background warm (a realtime session's, a read-aloud's) of such a stage says `fallback` with
  the alias that answers, and starts nothing when the verdict would swap it (D98).
- The Chat's error card for that code shows the sentence and, in the app window, "Set it up"
  (§4.5). Built in WP8b (`pages/chat_machine.rs`, D176 to D178): the turn's `error` frame with
  `gpu_setup` is a card of its own in the reply, its sentence, "Details" with what the start said,
  and under the newest such card the compact component (D186), "Set it up" under §1.2's item in
  the app's window and §2.7's sentence in a browser; that card says only what failed and the
  banner steps aside while it is on the page (D187), and it asks for a fresh run. Every failed
  start that the check blames is stamped on the check by core (`failed_start`, D182), which ends
  "Not now" on every surface whoever made the request, and the live feed's `machine` frame
  tells an open dashboard (D183). A host without the machine slot shows the sentence alone. The
  model editor's Test card, under a failed test whose code is `gpu_setup` (D189), and the Image
  and Audio labs, under a run that failed with it, show the banner's line inside the card while
  the check says the card is not ready, whatever "Not now" holds, with none of its own (D179,
  D188); the Decision lab's chip, the voice library and the Voices lab say `gpu_setup` and
  `engine_download` in words, as Traffic does (D180).
- The registry's start error stays in the message ("its start said: …"), Podman's own text
  included, so nothing is lost.

### 2.6 When the check runs

At boot (in the background, after the server binds), when a surface asks for a fresh one ("Check
again", opening the Settings card, the Overview checklist mounting, debounced to one run at a time;
such a check writes lmgw's spec again first when its last write failed, D95), after a GPU container
start fails while the machine is not as the last check read it (built in WP5, every start path,
D63: the start's own answer comes from §1.1 and §1.2 read alone, and the full check follows in the
background, one waiting at a time, D96) or one brings a model up while the last check said it
could not (D68), and after a fix. When a run's verdict turns `ready`, the pre-fetch of §5 begins
(D73), unless the connection is metered (D99). Each run costs a few file stats, one NVML probe
in a subprocess, `podman --version`, `--help` and `info`, one `podman image exists` per image the
settings name, one cache-only `dnf repoquery`, and for each image not downloaded yet one anonymous
registry manifest read (D62). The first draft measured 0.12 s for a run with NVML in-process; WP4's whole run, with all of that, took 1.1 s in a measurement (2026-11-03 on a dev instance:
verdict `ready`, a 24 GiB NVIDIA card as NVML reports it, the same driver version on both halves, NVIDIA's
toolkit 1.20.1, Podman 5.8.9, all three engine images present, no frozen section; §7's WP4 entry has
the re-run after the review round). There is no timer yet, because the break that matters shows
itself at the next start. Keeping lmgw's own spec current is separate from the check and runs before
every GPU start (§3.6.2).

### 2.7 A browser on another machine

The browser sees the same check and the same plan. In place of the button it says "Open lmgw on
the computer it runs on to set this up", followed by the commands fold. No route can start the fix
(D3). Built in WP8a: "the app's window" is `__TAURI_INTERNALS__` being there, the dashboard's
one shell test (D146), and the commands fold has the steps' terminal lines with a copy button in a
browser as in the app (D149). The component's own sentences there say "the computer lmgw runs on",
and on a loopback address the sentence points at lmgw's own window instead (D163).

## 3. The fix

Under ruling 4 the fix as root is small: install the toolkit, and the repository and key it comes
from. Everything after that (writing the spec, keeping it current) is lmgw's own work with the
user's rights (§3.6).

### 3.1 The helper

`lmgw-machine-helper` is a small Rust binary in its own workspace crate
(`crates/lmgw-machine-helper`, built in WP6), which depends on `lmgw-machine`, `serde_json` and
`libc` only (D84). The lines it prints, its exit codes and the unit's command are
`lmgw_machine::helper`, which the shell (WP7) reads with the same types. Its whole interface:

```
lmgw-machine-helper plan             # anyone: print root's plans for this computer as JSON (debugging, live tests)
lmgw-machine-helper apply <sha256>   # as root via /usr/bin/pkexec: recompute the plans, compare, start the unit
lmgw-machine-helper run <sha256>     # as root inside lmgw-machine-setup.service only: recompute, compare, run the steps
```

Rules it enforces itself, in this order:
- It accepts exactly one verb and, for `apply` and `run`, one argument matching `^[0-9a-f]{64}$`.
  Anything else (an argument that is not UTF-8 included) exits 4 with the usage on stderr and a
  `refused` line, before anything is read or run.
- `apply` refuses unless the real and the effective UID are 0 and `PKEXEC_UID` is a user ID. `run`
  refuses unless both UIDs are 0 and its own cgroup (`/proc/self/cgroup`'s `0::` line) is
  `/system.slice/lmgw-machine-setup.service`; a system with no cgroup v2 line is refused (D77,
  D86). Neither runs as root by any other route. A refused caller gets one `refused` line and
  nothing in the journal; `begun` and every journal entry come after the check (D82).
- It ignores the environment and the working directory it was started in: its first act is to
  change to `/`. It reads the machine through `SystemFacts::for_root` (D60), and every program it
  starts (its own reads included) gets an empty environment with only `HOME=/root`,
  `PATH=/usr/sbin:/usr/bin` and `LC_ALL=C.UTF-8`, `/` as its working directory and no descriptor
  beyond the standard three (D85). It calls every program by absolute path, reads nothing in any
  user's home, and knows nothing of lmgw's settings or data dir. It never parses YAML and never
  loads NVML: no CDI fact is a plan input (§3.2). The check reads the plan's inputs through the same
  environment with the user's rights (`Facts::root_env`, D87), so the two sides' hashes differ only
  where root may read what the user may not.
- It builds root's plans from the machine (§3.2), one for each value of the flag only the user's
  side knows (D44, `lmgw_machine::helper::variants`). If the argument is neither plan's hash, it
  exits 2 and changes nothing. Before exiting it prints one `plan_changed` line with both of its
  hashed plans, its plan-schema version and its lmgw version, so the shell can tell "the computer
  changed" from "lmgw was updated" (§3.5, D75).
- A matched plan without steps starts nothing (`nothing_to_do`, exit 0, D79). A matched plan for
  another system than dnf5 (an image-based system's `rpm-ostree install`, an older dnf, no dnf) is
  refused with exit 4: those plans are commands to type (§3.7, D78).
- `apply` asks systemd whether the unit runs (`systemctl show --property=ActiveState --value`);
  running is exit 5 (D81). Then it starts the steps as a fixed transient system unit and waits for
  it:
  `/usr/bin/systemd-run --wait --pipe --collect --quiet --property=PrivateTmp=yes
  --unit=lmgw-machine-setup --description="lmgw: setting up the graphics card"
  /usr/bin/systemd-inhibit --what=shutdown:sleep --who=lmgw --why="Setting up the graphics card
  for local models" --mode=block /usr/libexec/lmgw/lmgw-machine-helper run <hash>`. Why a unit
  (D22): pkexec execs the helper inside lmgw's own cgroup, the app's scope. Quitting lmgw tears
  down the pipe, and logging out or shutting down tears down the scope, so `dnf` would be killed
  mid-transaction. A system unit runs in `system.slice`, outside the session, with a `/tmp` of its
  own (D103). Its fixed name is also the lock: systemd refuses a second unit of that name while
  one is active, and when systemd-run fails before `run` began, `apply` asks again and reports a
  unit that runs now as exit 5. The inhibitor makes the
  desktop's shutdown say a setup is running before it goes ahead. `apply` reads the unit's pipes
  itself (`--pipe` hands the unit `apply`'s own) and passes `run`'s lines on as they come;
  anything else, systemd-run's and the inhibitor's own words, goes on as `note` lines, so lmgw's
  pipe carries events only (D76). systemd-run passes on the unit's exit code and the inhibitor its
  program's, so `run`'s 0, 2, 3 and 4 are `apply`'s when `run`'s own last line says the same
  (`finished` 0 or `nothing_to_do`, `plan_changed`, `finished` 3, `refused`, D102); any other end
  is exit 6.
- `run` recomputes and compares again (the machine could change between `apply` and the unit
  starting), then runs the steps in order and stops at the first failure. Every step's program gets
  its own pipes. The helper reads them and writes each line, plus each step's start, end and exit
  code, as JSON to its stdout (the pipe back to lmgw) and to the journal (`SYSLOG_IDENTIFIER=
  lmgw-machine-setup`, the line itself as `LMGW_EVENT`, D82). It ignores SIGPIPE and every write
  error on stdout, so a dead lmgw never stops a step, and `dnf` never writes into lmgw's pipe
  directly. Its stdout is a queue written by a thread of its own, so a reader that stops reading
  (a frozen app) never stops a step either; at the end it waits only for what the pipe takes, and
  the journal has the rest (D100). A step ends when its program does: what the program left in
  its pipes is read, and a process it left behind holding them is not waited for (D101).
- It does not re-read anything afterwards. The shell's own check after the run (§3.5) decides
  whether the card works.

Its exit codes stay clear of pkexec's 126 and 127: 0 done, 2 plan changed, 3 step failed, 4
refused, 5 already running, 6 the system task could not be started or ended without the helper's
own code, or with a code `run`'s own last line does not say (D75, D102).

Its lines, one JSON object each with an `event` field (`lmgw_machine::helper::Event`): `begun`
(`apply`'s and `run`'s first line once the caller check passed, with the verb, the lmgw version,
the plan schema and, for `apply`, `pkexec_uid`), `plan`
(`plan`'s answer), `plan_changed`, `refused`, `already_running`, `nothing_to_do`, `started` (the
plan's hash and its number of steps), `step_started` (`step` of `of`, counted from 1, and its
`kind`, `s1_add_repo` … `s4_swap`), `line` (`step`, `kind`, `stream` `stdout`, `stderr` or
`helper` for the helper's own "running …" and "creating …", and the line whole), `step_ended`
(`exit`, `signal` or `error`), `note`, `finished` (`run`'s last, exit 0 or 3) and `task_failed`.

What it never does: remove a package, except the confirmed swap of Fedora's retired package (S4″);
remove or edit a file it did not write (S1′ switches a section on through dnf's own command, which
writes dnf's override file and leaves the person's `.repo` file as it is); touch `/etc/cdi` or
`/var/run/cdi`; change SELinux, kernel arguments or the bootloader; enrol keys; restart the
machine; or run anything the plan's closed list does not name.

### 3.2 The plan: a closed list of steps

The plan is computed by a library crate, `lmgw-machine`, which the check in `lmgw-core` and the
helper both use, and both read its inputs through root's environment (D87). So the plan block, the
commands fold, the AppImage instructions and what runs as root are one value. A step is one of
these kinds, with constant program paths and arguments. Only package names, a section's id and
versions come from the machine's own rpm database and repository files.

| Step | When | Runs (as root) |
|---|---|---|
| **S1** add NVIDIA's repository | toolkit needed, no provider-table repo enabled, no section whose `baseurl` is NVIDIA's | create `/etc/yum.repos.d/lmgw-nvidia-container-toolkit.repo` (0644, content below) |
| **S1′** switch NVIDIA's repository on | a section whose `baseurl` is NVIDIA's exists and is disabled (after dnf's overrides) | `/usr/bin/dnf5 config-manager setopt <id>.enabled=1` |
| **S2** add NVIDIA's key | NVIDIA's fingerprint is not in `rpmkeys --list` | create `/etc/pki/rpm-gpg/RPM-GPG-KEY-lmgw-nvidia-container-toolkit` (0644, embedded); `/usr/bin/rpmkeys --import` it |
| **S4** install the toolkit | `/usr/bin/nvidia-ctk` absent and unowned | `/usr/bin/dnf5 install -y <scope> nvidia-container-toolkit-base` |
| **S4′** update the toolkit | owner is NVIDIA's `-base` and lmgw's spec generation failed in the toolkit (D57) | `/usr/bin/dnf5 upgrade -y <scope> <packages>`: `nvidia-container-toolkit-base`, then each of `nvidia-container-toolkit`, `libnvidia-container-tools` and `libnvidia-container1` that is installed (they pin each other's versions, D55) |
| **S4″** switch from Fedora's retired package | owner is `golang-github-nvidia-container-toolkit` | `/usr/bin/dnf5 swap -y <scope> golang-github-nvidia-container-toolkit nvidia-container-toolkit-base` |

`<scope>` is `--repo=<id> --setopt=<id>.skip_if_unavailable=False --setopt=<id>.gpgcheck=1
--setopt=<id>.gpgkey=file:///etc/pki/rpm-gpg/RPM-GPG-KEY-lmgw-nvidia-container-toolkit
--setopt=install_weak_deps=False`, where `<id>` is the section the toolkit comes from (lmgw's own,
the person's NVIDIA section, or a provider-table repo). `--repo` restricts the transaction to that
one section, so nothing from any other repository rides along. `skip_if_unavailable=False` makes an
unreachable server or a bad signature fail the step instead of a puzzling "no match". The
`gpgcheck` and `gpgkey` options turn package checks on against the pinned key even in a section the
person wrote with `gpgcheck=0`, without editing their file. `-base` requires only glibc, so the
scoped transaction resolves (checked on 1.20.1). For S4′ through lmgw's own section, `<scope>` also
appends `--setopt=<id>.includepkgs=<package>` for each installed toolkit package the file's
`includepkgs` does not let through (D55). A `.` in `<id>` is safe in these options: dnf5 splits a
setopt key at its last `.` (D48's sources, `dnf5/main.cpp` and `dnf5/context.cpp`), and section ids
lmgw passes never hold glob characters, since dnf5 matches the id part as a glob.

They run in the order S1 or S1′, S2, then one of S4, S4′ and S4″, each only when its condition
holds. NVIDIA's package then runs its own scriptlets: they write
`/etc/nvidia-container-runtime/config.toml` and preset and start NVIDIA's refresh units (checked in
1.20.1's `%post` and `%posttrans`). The commands fold says so. lmgw leaves those units as NVIDIA
sets them.

**The package list is never empty.** S4′'s list is a non-empty type (`NonEmpty<PackageName>`: a
first element plus the rest), and the owner package from `rpm -qf` is always its first element, so
an S4′ without a package cannot be built. A bare `dnf upgrade -y` would upgrade the whole system as
root. A test asserts that every dnf step of every generated plan ends with at least one package
name (§6).

**Steps S3, S5 and S6 are gone with ruling 4.** They installed RPM Fusion's CUDA part, switched on
NVIDIA's refresh units and started the refresh. The numbers are kept so the review's references
still match.

**Where the toolkit comes from.** Before S1, the plan consults a provider table in `lmgw-machine`:
which enabled repository ships a package that owns `/usr/bin/nvidia-ctk` on this system
(`/etc/os-release` `ID` and `VERSION_ID`, repository id). The table is empty today (D5's checks).
When Fedora's review lands, a reviewed lmgw change adds a row, and S4 installs from that repository
with S1 and S2 left out. The check, with the user's rights, also runs `dnf repoquery -C
--whatprovides /usr/bin/nvidia-ctk`. That is information only, never a plan input, because the
user's metadata cache can differ from root's (as the user it can have no cache for `fedora`).
When an enabled repository outside the table offers the toolkit, the check names it, and the plan
becomes instructions that install it from that repository alone, rather than adding NVIDIA's
repository next to it (D56).

**What S1 writes, and how.** NVIDIA's stable repository section as NVIDIA publishes it at
`https://nvidia.github.io/libnvidia-container/stable/rpm/nvidia-container-toolkit.repo` (fetched
2026-11-03, byte-identical to the file of a Fedora 44 test system), in an lmgw-named file with an lmgw-named
section, with these changes: package checks are on, against the local key; the section can serve
only the one package lmgw installs, so nothing else from it ever arrives through system updates;
and the experimental section is left out.

```ini
# Written by lmgw. lmgw replaces this file when its text or NVIDIA's key changes.
[lmgw-nvidia-container-toolkit]
name=NVIDIA Container Toolkit (added by lmgw)
baseurl=https://nvidia.github.io/libnvidia-container/stable/rpm/$basearch
includepkgs=nvidia-container-toolkit-base
repo_gpgcheck=1
gpgcheck=1
enabled=1
gpgkey=file:///etc/pki/rpm-gpg/RPM-GPG-KEY-lmgw-nvidia-container-toolkit
```

The section does not set `skip_if_unavailable=False`. That would make every system update fail
whenever nvidia.github.io is down, which is worse for a newcomer than a toolkit that misses an
update for a day. lmgw's own calls set it (`<scope>`), and the check watches for a section that
stopped loading (below).

Both files are written to a temporary file created with `O_CREAT|O_EXCL|O_NOFOLLOW` in their
target directory, under a name dnf does not read (`.lmgw-<name>-<pid>-<ns>.tmp`), given its mode
exactly, synced, and then linked to the path (`link(2)`, which fails when anything is there), so an
existing file is never overwritten, no half-written file is ever at the path, and each new file
gets its directory's SELinux label (`system_conf_t` in `/etc/yum.repos.d`, `cert_t` in `/etc/pki/rpm-gpg`, as on a Fedora test system). Nothing is written elsewhere and moved into place. lmgw's
own file (its name and its first-line marker) is replaced when its content differs from the
embedded one: the temporary file is renamed over it. Either way the temporary file is gone again
and the directory is synced (D80; the first draft created the file at its path directly, which a
crash mid-write would leave half-written).

**Which sections count.** The plan reads the `.repo` files in `/etc/yum.repos.d` that are
world-readable (root skips the others in the same way, so both sides agree, and the detail names
them), then dnf5's override files, under the same world-readable rule
(`/usr/share/dnf5/repos.override.d`, then `/etc/dnf/repos.override.d`, later ones winning).
Overrides matter: a section can be switched on through `99-config_manager.repo` while its `.repo` file says `enabled=0`. Only sections whose `baseurl`
starts with `https://nvidia.github.io/libnvidia-container/`, or whose id is in the provider table,
are read, and only their id, `enabled`, `baseurl` and `includepkgs`.

**An existing NVIDIA section** (a person who followed NVIDIA's docs) is
used as it is and never rewritten. lmgw's own dnf calls turn package checks on for it through
`<scope>`. When it is disabled, the plan offers S1′, and the plan block says so: "NVIDIA's software
source is on this computer but switched off. lmgw will switch it on." dnf5's `config-manager
setopt` writes that to `/etc/dnf/repos.override.d/99-config_manager.repo` and does not change the
original file (its man page says so).

**How the key is verified.** The key S2 writes is NVIDIA's "NVIDIA CORPORATION (Open Source
Projects) <cudatools@nvidia.com>" key, fingerprint `C95B 321B 61E8 8C18 09C4 F759 DDCA E044 F796
ECB0`. It is the key served at `https://nvidia.github.io/libnvidia-container/gpgkey` on 2026-11-03,
and the key (ID `ddcae044f796ecb0`) that signed `nvidia-container-toolkit-base-1.20.1-1` on this
machine. It is embedded in the helper. A test pins its fingerprint (§6), so a changed key is a
reviewed code change, never a download. With `repo_gpgcheck=1` and `gpgcheck=1`, dnf refuses
metadata or packages the key did not sign, and a refusal shows as the step's failure.

**When NVIDIA changes its key.** The repository's metadata then fails to verify. On system updates
dnf skips the section without a word, because Fedora's defaults set `skip_if_unavailable=True`
(`/usr/share/dnf5/libdnf.conf.d/20-fedora-defaults.conf`, checked). lmgw's own calls fail loudly,
and the check's frozen-section reading shows it. The fix is an lmgw release with the new key. After
it, S2's condition (the new fingerprint is not imported) and lmgw's own-file rule rewrite the key
file and import the new key. The old key stays imported, since removing a key is not a step yet.

**A section that stopped loading.** The same `skip_if_unavailable=True` means a section whose server
is down, or whose signature fails, freezes the toolkit at its version without a word. The check
reads the system's metadata cache (`/var/cache/libdnf5/<id>-*/`, world-readable) and compares
when dnf last loaded each section, by dnf5's own clock: the primary metadata file's time (D54).
When `fedora` or `updates` were loaded after NVIDIA's section had expired by its own
`metadata_expire` (172800 s in a measurement, against 21600 s for `updates`), that section
is not loading, and 1.2 gets an info line with both dates. This is information, not a plan input.

**The hash.** SHA-256 over a dedicated struct, `HashedPlan { schema: PLAN_SCHEMA, steps:
[HashedStep { kind, argv, file: Option<{ path, bytes }> }] }`, in a canonical encoding. It is not
the `PlanStep` DTO: that DTO's sentence and `download_bytes` change with wording and with the
user's metadata cache. Root recomputes the hash from facts it reads exactly as the user does: the
rpm database (`rpm -q`, `rpm -qf`, header signature key IDs, `rpmkeys --list`), the world-readable
repository files and dnf's overrides, `/etc/os-release`, and the embedded repository text and key.
Nothing else is an input: not lmgw's or the system's CDI specs, the user's `~/.config/containers`,
NVML, dnf's metadata caches, files the user cannot read, or the lmgw version. The embedded file
contents are inputs on purpose, because they are what root writes. If an in-app update changed
them while lmgw kept running, exit 2 and the schema line say so (§3.5). Ruling 4 removed three
inputs that the first draft needed and that root could not always read the same way: unit states
(S5, S6), RPM Fusion's repository facts and the driver version pin (S3), and CDI staleness (S6's
condition, which a user who cannot stat a path reads differently from root).

### 3.3 The polkit action

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE policyconfig PUBLIC "-//freedesktop//DTD polkit Policy Configuration 1.0//EN"
 "http://www.freedesktop.org/software/polkit/policyconfig-1.dtd">
<policyconfig>
  <vendor>lmgw</vendor>
  <vendor_url>https://github.com/lmgw-dev/lmgw</vendor_url>
  <action id="io.github.lmgw-dev.lmgw.machine-setup">
    <description>Set up the graphics card for lmgw's models</description>
    <message>lmgw wants to set up the graphics card for local models: install or update NVIDIA's container toolkit and its software source.</message>
    <icon_name>lmgw</icon_name>
    <defaults>
      <allow_any>no</allow_any>
      <allow_inactive>no</allow_inactive>
      <allow_active>auth_admin</allow_active>
    </defaults>
    <annotate key="org.freedesktop.policykit.exec.path">/usr/libexec/lmgw/lmgw-machine-helper</annotate>
    <annotate key="org.freedesktop.policykit.exec.argv1">apply</annotate>
  </action>
</policyconfig>
```

It is installed as `/usr/share/polkit-1/actions/io.github.lmgw-dev.lmgw.machine-setup.policy`,
from `crates/lmgw-machine-helper/polkit/` (built in WP6, the text above byte for byte; it
validates against polkit's own `/usr/share/polkit-1/policyconfig-1.dtd` with `xmllint --nonet
--dtdvalid`, exit 0, and the helper's tests pin its id, defaults, message, exec path and argv1).
The RPM guard pins its sha256 (§3.8). polkitd watches that directory, so no scriptlet is needed.
`allow_inactive=no` means an SSH or switched-away session cannot even authenticate. The message is
one sentence true of every plan the button offers, a fresh install, a switched-on section, the
toolkit update and the swap of Fedora's retired package; the confirmation before it (§4.5) names
each step. `exec.argv1` makes the action `apply`'s alone: for `pkexec helper plan` or `run`,
pkexec uses polkit's generic action and its wording (D89). For an account that is not an
administrator, polkit asks for an administrator's password: on Fedora that means a member of
`wheel`, and the agent offers the choice. The confirmation says so beforehand (§4.5).

### 3.4 Who may start it

The Tauri command `machine_apply` (§4.3, built in WP7: `src-tauri/src/machine.rs` and
`machine/gates.rs`) refuses unless all of these hold, asked in this order:
- `media::command_allowed()`: the caller is the gateway's own document in the app's window (the
  invoke handler in `main` asks it for every shell command, before the command runs);
- Linux, and a release build (`!cfg!(debug_assertions)`, the switch `dev_guard` already uses for
  the updater);
- not a dev instance: `state.dev()` is false (it covers `LMGW_DEV` and the headless runner only),
  the running program is `/usr/bin/lmgw` (`std::env::current_exe`), so a release build run from a
  checkout never gets here (a program an RPM update replaced while it ran reads
  `/usr/bin/lmgw (deleted)` and is told to restart, D130), and the data dir is the installed
  default (`lmgw_core::state::is_installed_data_dir`), so the RPM's binary pointed at a scratch
  dir never does either;
- the helper is installed and sane (`lmgw_machine::plan::instructions::helper_installed`, asked
  again at the click): `/usr/libexec/lmgw/lmgw-machine-helper` is a regular file (not a symlink),
  owned by root, mode 0755 or stricter, with no setuid, setgid or sticky bit, in root-owned
  directories that only root can write; the policy file is a regular file, owned by root and not
  group- or world-writable; `/usr/bin/pkexec` exists, checked at that absolute path and never
  looked up in `PATH`;
- no setup runs already: systemd is asked (`unit_state_args`), and a running unit is followed
  instead, with no dialog and no password (§3.5, D122);
- a fresh check's plan has the argument as its hash and `helper` as its `how` (§4.1), which only a
  dnf5 system with steps, the helper, no AppImage, no other repository offering the toolkit and no
  plan the helper read differently gets (D123, D124);
- the app's window is shown and the active one, checked on the GTK main thread as the dialog is
  built (otherwise "Open lmgw's window and press Set it up there.", D133), and the document it
  runs has not said "Not now" before (otherwise `declined` again without a dialog, D140);
- the person pressed "Set it up" in the native confirmation drawn from that plan (§4.5), which is
  pressable only once the dialog has had the focus for a second (D134);
- no other setup is being started from this app (D125).

Each refusal is an answer with the outcome `unavailable` (or `plan_changed`, `nothing_to_do`,
`declined`) and a plain sentence that says what to do instead; nothing is started and no password
is asked for. Tests reach the command's logic through a `World` trait (the gates' facts, systemd,
the journal, the check, the dialog, the helper). Only the command builds the real one, and its
spawn refuses in a debug build as well. If a test ever reached the binary anyway, `apply` would
refuse without `PKEXEC_UID` and UID 0. The check, which only reads, runs everywhere, dev instances
included.

### 3.5 Running it and checking again

The shell spawns `/usr/bin/pkexec --disable-internal-agent /usr/libexec/lmgw/lmgw-machine-helper
apply <hash>` (the fresh check's hash, lmgw's own value) with stdin null, an empty environment, `/` as its working directory and no
descriptor beyond the standard three (D120, D132; `src-tauri/src/machine/spawn.rs`). It turns
each JSON line into an event on the command's `Channel` (§4.3, `machine/progress.rs`), and the
window shows the step list with its states. From the `password` event until `authorized` (the
helper's first `begun` line) the window says "Your computer is asking for your password".
Outcomes (`machine/outcome.rs`; 126 and 127 are pkexec's only before that first line, D126):

| Exit | Outcome | What the person reads |
|---|---|---|
| 0 | applied | Done. Models can use the graphics card now. (after the check below agrees) |
| 126 (pkexec) | dismissed | You closed the password window. Nothing was changed. |
| 127 (pkexec), stderr "No authentication agent found" (or "Error creating textual authentication agent", should pkexec ever try its own, D120) | no agent | Your desktop has no password window running, so lmgw can't ask for the password. Here are the commands to run in a terminal instead. (the instructions view) |
| 127 (pkexec), stderr pkexec's own words about the program: "Error accessing <program>", "Cannot run program <program>" (before the password) or "Error executing <program>: …" (the exec after it), never "as another user" (D142) | failed, or lmgw updated | lmgw couldn't start the setup. The details say why; nothing was changed. (Or, when `/usr/bin/lmgw` reads replaced by then: lmgw was updated while it was running. Restart lmgw, then try again.) |
| 127 (pkexec), otherwise ("Error executing command as another user: Not authorized", …) | not authorised | The password wasn't accepted, or this account may not change the system. Someone who can has to type their password. |
| 2, same schema and version, lmgw not replaced, the shell's fresh hash among `plan_changed.plans[*].sha256` | plan changed | Something on this computer changed since you confirmed. Here is the new list. (The plan block shows the new plan; nothing runs until the next confirmation.) |
| 2, same schema and version, lmgw not replaced, the shell's fresh hash not among them | read differently | lmgw and your system read this computer differently, so lmgw can't set it up by itself. (Both plans, the shell's and root's, are in the detail; the button is not offered for that plan again, D88.) |
| 2, other schema or version, or `/usr/bin/lmgw` replaced since lmgw started (asked again first, D143) | lmgw updated | lmgw was updated while it was running. Restart lmgw, then try again. (The `plan_changed` line's `version` is the helper's lmgw build, with the `+<pipeline>` suffix of a test-feed RPM; schema alone misses an update that changed the plan's code or embedded text but not its layout, D75.) |
| 3 | step failed | Setting it up didn't finish. <plain reason> The step's output is in the details, and the steps before it stay done. (D128) |
| 4 | refused | lmgw couldn't start the setup. The details say why; nothing was changed. (The helper's own refusal, its reason in the detail; never expected from the app, D129.) |
| 5 | already running | The setup is still running from before; lmgw follows it and says how it ends. (Expected after lmgw was restarted during a setup. The shell follows `systemctl show --property=ActiveState --value lmgw-machine-setup.service` until the unit is gone, passing the unit's journal lines on, then checks, D122. A unit found running before the dialog is followed the same way, without a password.) Its end, from the journal's `finished`: done or step failed as above; without a readable journal: "The setup that was running has ended. The check shows what is in place now." |
| 6 | task failed | as 101 below: the system task could not be started (systemd-run's own words are `note` lines), ended without the helper's own code, or ended with a code `run`'s own last line does not say (the `task_failed` line says which, and what `run` said). |
| 101, or killed by a signal | failed | Setting it up stopped unexpectedly. The output so far is in the detail, and the check below shows what is in place now. |

The plain reasons for a failed step are read from the failed step's lines and its exit code
(`machine/reason.rs`, D128): "NVIDIA's download server didn't answer. Try again later." "Another
program is installing updates right now. Try again when it's done." "The download didn't match
NVIDIA's signature, so nothing was installed. Try again later, and if it keeps happening, tell
lmgw's developer." "NVIDIA's packages are signed with a key lmgw doesn't know yet. An lmgw update
will fix this." (a signature that does not verify is told from a key lmgw lacks, D141) "Something
on this computer still needs the old piece, so lmgw couldn't replace it." Anything else quotes the
step's bullet and its exit code. Upstream failures are reported, not worked around. After an lmgw restart the step lines
are in the journal (`journalctl -u lmgw-machine-setup`), which the detail names; a
non-administrator cannot read the system journal, and the check's result does not depend on it.
Whatever reads those entries takes only the ones whose trusted fields say root inside the unit
(`_SYSTEMD_UNIT=lmgw-machine-setup.service`, `_UID=0`), since anyone can write an entry with the
identifier and an `LMGW_EVENT` of their choosing (D82); the shell's follow adds the running
invocation (`_SYSTEMD_INVOCATION_ID`) and checks all three again on every entry journalctl prints
(`machine/follow.rs`). A `line` event's text is the step's program's output and is shown as text,
never as HTML: the shell passes it on byte for byte as a JSON string, and the page sets it as a
text node.

Afterwards the shell calls core's `lmgw_core::machine::after_fix` (`machine/fixed.rs`): with the
toolkit now present it generates lmgw's own spec (§3.6), with the user's rights and without
another prompt, then runs the check fresh, both under the check's one-run lock and published once.
A step that exited 0 without its effect is caught here, because the check reads the result, not
the exit code. If the ledger's probe reads nothing and that check found the NVIDIA card and its
driver working, the probe is detected again and installed when it reads the card
(`state.vram.set_probe`), so VRAM admission works without a restart. The steps never change what
the probe reads (they install the toolkit, never the driver whose `libnvidia-ml.so.1` it loads),
so this only matters when the driver came up after lmgw started (D127). It runs after every end in
which the helper ran as root; the ends that changed nothing answer with the check made before the
dialog.

While a step runs, lmgw has no stop button yet. Closing lmgw or logging out does not stop it,
because it runs as the system task `lmgw-machine-setup`, and the next check shows how it ended.
Turning the computer off is what can still interrupt it, so the run holds a shutdown inhibitor and
the desktop's shutdown says "lmgw is setting up the graphics card" first. The progress view says
the same in one line.

### 3.6 lmgw's own spec (ruling 4)

#### 3.6.1 Where and how

lmgw writes `$XDG_RUNTIME_DIR/lmgw-machine/<instance>/cdi/nvidia.json`, where `<instance>` is the
slug of the `container_prefix`, as the agents' runtime dirs use it. The dir is per instance, so a
dev instance never writes the file production reads. It is outside the agents' runs root
(`$XDG_RUNTIME_DIR/lmgw/<instance>/`), because that root's sweep removes whatever is not a live run
(`agents/container/reconcile.rs:141`). Without an absolute `XDG_RUNTIME_DIR` (rootless Podman needs
it anyway) the fallback is `<data_dir>/machine/cdi/`. The private Podman socket of §5 lives beside
it.

Every dir lmgw makes there (`lmgw-machine`, `<instance>`, `cdi` and the staging dir; `machine` and
`cdi` under the data dir) is created `0700` and checked before each use, as `lmgw_update`'s private
download dir is: not a symbolic link, a directory, owned by this user, no group or other access. A
dir that fails is refused with a plain sentence that names it and why, and lmgw changes nothing in
it.

It is generated by `/usr/bin/nvidia-ctk cdi generate --format=json
--output=<root>/.generating-<pid>/nvidia.json --nvidia-cdi-hook-path=/usr/bin/nvidia-cdi-hook`,
into a staging dir of the process's own beside `cdi/`, and renamed into place, so Podman never sees
a half-written file. None of the caller's `NVIDIA_CTK_*` variables reach it (nvidia-ctk reads every
option from one, `nvidia-ctk --help`), and the hook path is pinned, so the spec names the toolkit's
own hook and not the first `nvidia-cdi-hook` on the caller's `PATH`. JSON, so the judgement before
every start reads it with `serde_json` alone; the YAML reader is only for the system's specs (§1.2).
A staging dir whose process is gone (lmgw killed mid-write) is removed by the next write, under the
one-generation lock. (Built in WP3. The first draft named `--output=<dir>/.nvidia.yaml.tmp` inside
the spec dir, which does not work: nvidia-ctk 1.20.1 appends `.yaml` to an output name without a
`.yaml` or `.json` extension, measured as `.nvidia.yaml.tmp.yaml`, and the CDI library reads every
`*.yaml` and `*.json` in a spec dir, dotfiles included.)
Measured on 2026-11-03 (toolkit 1.20.1): rootless, exit 0, 0.38 s, and the same
759-line spec as the root-written `/var/run/cdi/nvidia.yaml`, line for line apart from comments.
The JSON form with the pinned hook path, rootless the same day: 0.33 s, devices `0`, the GPU's
UUID and `all`, every hook `/usr/bin/nvidia-cdi-hook`, and Podman 5.8.9 given only its dir gave a
container `/dev/nvidia0`, `/dev/nvidiactl`, `/dev/nvidia-uvm` and `libnvidia-ml.so.1`.
Generation reads the card through NVML and is expected to create no CUDA context, so it can run
under the GPU hold (not measured yet).

Before generating:
- the driver's two halves do not differ and a kernel module runs (§1.1); while they differ lmgw
  keeps its last spec and generates nothing, because a spec for libraries the running kernel module
  does not match is no use. A userspace version lmgw cannot read (an unversioned or regular-file
  `libnvidia-ml.so.1`, or none in the dirs it looks in, NixOS's `/run/opengl-driver/lib`
  included) does not stop it: nvidia-ctk's own NVML start reports a real mismatch (D26);
- `/dev/nvidia-uvm` exists, or lmgw runs `/usr/bin/nvidia-modprobe -u -c=0` first. NVIDIA's own
  refresh creates the control device nodes before it generates (its service runs `nvidia-ctk system
  create-device-nodes --control-devices --load-kernel-modules` first), and `nvidia-modprobe` is
  setuid root and a Requires of `xorg-x11-drv-nvidia` (checked), so lmgw can do the same without a
  password.

Beside the spec dir (`<root>/nvidia.stamp.json`, not in `cdi/`: Podman reads every `*.json` in a
spec dir as a spec), a stamp records what it was generated for: the kernel module version, the
userspace version, the inode, size and mtime of the file `libnvidia-ml.so.1` resolves to (an
update replaces it even where its name says no version), the `nvidia-ctk` version, the rpm
database's change time (the newer mtime of `rpmdb.sqlite` and `rpmdb.sqlite-wal` in
`/usr/lib/sysimage/rpm`; not `-shm`, which changes with reads too, such as root's `dnf makecache`),
the names of the `/dev/nvidia*` nodes (one listing of `/dev`: nvidia-ctk names only the nodes that
are there, so `nvidia-uvm` made after a best-effort `nvidia-modprobe`, or a card plugged in later,
makes the spec stale), this boot's ID (`/proc/sys/kernel/random/boot_id`, because the data-dir
fallback survives a reboot), and the time. The judgement compares `nvidia-ctk` by its file's inode,
size and mtime, so it stays a `stat`; the `--version` text is recorded for the detail. Any other
file in `cdi/` makes the spec stale too and goes when it is written again: Podman would read it
beside lmgw's (the YAML spec of WP3's first build is one).

After generating, lmgw reads the machine again. When the driver, the toolkit, the rpm database, the
nodes or the boot changed while nvidia-ctk ran (a `dnf` transaction mid-write), the new spec is not
used, the start says what changed, and the next start writes it again.

#### 3.6.2 When

- At boot, in the background after the check.
- Before every GPU container start and every GPU throwaway probe: a cheap judgement (stat every
  path the spec names, compare the stamp with the machine as it is now). It regenerates when the
  stamp differs or a path is gone. One generation runs at a time per instance, and a start waits
  for it. The wait and the generation together take at most the start's load timeout (Settings →
  GPU → Admission → Load timeout; a probe reads the same setting): an NVML that does not answer (a
  card that fell off the bus) hangs nvidia-ctk, and it is then killed with its process group, the
  start fails with a sentence that names the program, how long it ran and the setting, and the log
  says every 10 s that it is still running.
- After a fix (§3.5).

Ruling 4 names the driver version change. D20 adds the rpm database change and the gone path,
because an egl-wayland update (1.1.23 to 1.1.24) that deleted the old library changed no driver version. A failed generation becomes 1.2's "description can't be written" cause,
and the start fails with `gpu_setup` (§2.5). While the versions differ, the start fails with the
restart sentence.

#### 3.6.3 How Podman takes it

Read in Podman's source on `main`, 2026-11-03, and checked against `podman --help` on 5.8.9:
- `--cdi-spec-dir` is a global option, so it goes before the subcommand (`podman --cdi-spec-dir
  <dir> run …`). Given once, it **replaces** the default list (`/etc/cdi`, `/var/run/cdi`): the
  flag is a pflag `StringArray`, which drops its default on the first value, and `cmd/podman/root.go`
  (line 288) sets the engine's `CdiSpecDirs` to it. Given several times, the dirs are read in that
  order, and a later one wins by device name. A remote Podman ignores it (lmgw's is local).
- Devices are resolved when a container is initialised (libpod `generateSpec`,
  `container_internal_common.go`, line 777), so the option has to be on the podman call that
  initialises the container. lmgw's model containers are created and started by one `podman run`.
  An isolated MCP server's container is created with `podman create` and started with `podman
  start --attach`, so both carry it (§3.6.4). The cleanup process conmon runs gets `--hooks-dir`
  but not `--cdi-spec-dir` (`pkg/specgenutil/util.go`), so a restart policy would have Podman
  restart a container with the system's spec: lmgw leaves `--restart` out of a GPU container and
  says so (D27). A person who restarts an lmgw container by hand with `podman restart` gets the
  system's spec for that run.

What that means for lmgw's calls (D21):
- When every CDI device the argv names is `nvidia.com/gpu=…` (lmgw's spec defines `all`, each
  index and each UUID), lmgw passes its own dir alone. A hand-made `/etc/cdi` spec, NVIDIA's
  `/var/run/cdi` spec and any conflict between system specs are invisible to lmgw's containers and
  stay exactly as they are for the person's own (D7).
- When a row's run arguments name a CDI device of another kind (one only a hand-made spec defines),
  lmgw passes the configured system dirs (containers.conf's `cdi_spec_dirs`, read the way Podman
  reads it) followed by its own dir, so its NVIDIA definitions still win by name and the other
  device resolves. That row inherits any same-dir conflict in the system dirs, and the check judges
  that resolution for it (§1.4).

#### 3.6.4 Where the code changes

- One function, `runtime::cdi::OwnSpec::globals` (through `podman_globals(state, run_args)` and,
  with the spec made fresh first, `ready_globals`), is the only source of `--cdi-spec-dir`. Every
  builder that renders a podman command carrying a class's or a row's run arguments puts its result
  before the subcommand:
  - `runtime::argv::podman_run_argv`, and `command_line`, so the copyable command line shows it
    (it shows the option as a start would pass it, assuming this Podman takes it until the first
    GPU start or probe has asked `podman --help`);
  - `Registry::start_container` (`runtime/registry/acquire.rs`), which first calls
    `OwnSpec::ready_globals` (§3.6.2);
  - `Registry::run_throwaway` (`runtime/registry/podman_ops.rs`; the `--help` probes and "Try the
    graphics card");
  - `class_run_args` (`backends/run/verify.rs`) and its callers (the builds' GPU verify);
  - the benchmark: `PodmanLauncher::podman` and `run_argv` (`bench/launcher.rs`), and `bench::plan::render`
    for the command line a run stores;
  - an isolated MCP server's container: `mcp/container.rs` `podman_globals` reads the server's
    whole create argv (its `extra_run_args` may carry a GPU device), and `config/mcp.rs`
    `stdio_argv`, `container_create_argv` and `container_start_argv` put the result before `run`,
    `create` and `start`. The connect makes the spec fresh before `podman create`, outside the
    handshake's `timeout_ms`, and starts the container with the same options. The start names no
    device (the create resolved it), so the check on `podman start` can never fire; the options
    are passed there for symmetry.
- A guard test feeds each builder a class whose arguments carry `--device nvidia.com/gpu=all` and
  expects the option before the subcommand, and feeds it CPU arguments and expects none. It covers
  the builders listed.
- A podman call written by hand elsewhere is caught twice. At run time, `OwnSpec::refuse_unpointed`
  refuses a `run` or `create` argv whose options (those before the image; what follows is the
  container's own command) name `nvidia.com/gpu` or `--gpus` without a leading `--cdi-spec-dir`
  while lmgw passes its own spec, where podman runs: `Registry::podman_argv` (every registry
  verb), `Podman::run` (the builds' and Images view's verbs), `PodmanLauncher::podman` (the
  benchmark), the agent spawner `AppState::agent_spawner` hands out (builds and pulls go through
  it too), and an isolated MCP server's create (its start is passed through the check as well, but
  names no device). The refusal
  says it is a bug in lmgw. In the tests, a source scan lists every `Command::new("podman")`,
  `.run("podman"`, `.spawn("podman"` and `.spawn_env("podman"` in the workspace's source with the
  reason it is safe, and a new one fails until it is listed.
- The check's 1.2 reads lmgw's spec (§1.2). The README drops the `/etc/cdi` command and says lmgw
  writes its own spec and which containers read it. That lmgw checks the card and installs the
  toolkit after a password is not built yet (WP4, WP6), and the README says so.

#### 3.6.5 What ruling 4 costs

- lmgw owns spec generation: a rootless `nvidia-ctk` run (0.38 s) at boot and after any package
  transaction, before the next GPU start, and one small tmpfs file per instance.
- Every GPU podman call has to take the option from one place. A call that bypasses it is refused
  at run time (§3.6.4) instead of using the system's spec.
- Without the toolkit, or with a Podman that has no `--cdi-spec-dir`, lmgw's containers use the
  system's spec as before ruling 4 (D26), and it can go stale again there.
- The option replaces Podman's defaults, so lmgw's containers stop seeing other vendors' devices
  from system specs unless a row names them (§3.6.3).
- A command copied from lmgw's rendered command line names a runtime-dir path that holds a spec
  only once a GPU start or probe wrote it, and only until the login session ends.
- The person's own containers (`podman run --device nvidia.com/gpu=all` outside lmgw) still use
  NVIDIA's `/var/run/cdi` spec, which still goes stale after an `egl-wayland` update. lmgw fixes
  only its own, and the 1.2 detail says which spec the person's own containers use.
- It works around NVIDIA's refresh triggers instead of waiting for an upstream fix, which ruling 4
  accepts.
- It depends on `nvidia-ctk cdi generate` staying usable without root across toolkit updates (only
  1.20.1 is checked). A failure surfaces as 1.2's "description can't be written", with the toolkit
  update offered.

#### 3.6.6 Considered and not chosen: NVIDIA's refresh units (Option A)

The first draft relied on NVIDIA's `nvidia-cdi-refresh` units (toolkit 1.18 and later) to write
`/var/run/cdi/nvidia.yaml`, repaired a stale spec by starting the refresh as root, and installed
RPM Fusion's `xorg-x11-drv-nvidia-cuda` when `nvidia-smi` was missing, because the service skips
itself without it (`ConditionPathExists=|/usr/bin/nvidia-smi`) and its first `ExecStart` runs
`nvidia-smi -L`. Ruling 4 chose lmgw's own spec instead, because:
- every repair after an update would have asked for the password again (the first draft's D17), or
  needed an `allow_active=yes` exception to "ask once";
- the `-cuda` install went beyond the container pieces: it also pulls `nvidia-persistenced` (an
  exact-version Requires) and, where 32-bit Mesa is installed (Steam), 78 MB of 32-bit CUDA
  libraries through a rich Requires (checked on a 580 driver);
- NVIDIA's path unit watches only `modules.dep` and `nvidia-ctk`, so the spec went stale after
  `egl-wayland` updates (an update from 1.1.23 to 1.1.24 deleted the old library), and lmgw could only repair it, not prevent it.

An upstream report is still worth sending for everyone's own containers: `nvidia-cdi-refresh.path`
should also fire on the driver's userspace libraries and on `egl-wayland`. Filing it is the owner's decision.

### 3.7 Other systems

The check (§1) works everywhere, since it reads only what any distribution has. The plan knows the
system it is on (`/etc/os-release` `ID` and `ID_LIKE`, `/run/ostree-booted`, the `APPIMAGE`
variable, whether the helper is installed). When a step can't run here, the plan shows it as
instructions: the same steps, written as commands to type (with `sudo`), with a copy button and
one plain line about why there is no button. Under ruling 4 the instructions only ever cover the
toolkit. lmgw writes its own spec on every system once the toolkit is there. A plan with no steps
(blocked by a file in the way or an unreadable package database, an older dnf, an update or the
retired package's switch on an image-based system) never says "here are the commands": its line
says what is in the way and what the person can do instead (remove the file and check again, wait
for the other install, install the system's updates, NVIDIA's guide). Without the helper the line
is "This version of lmgw can't do this by itself yet, so here are the commands to run in a
terminal.", and why the helper does not count (not there, its mode) is a detail line of 1.2.

| System | Fix |
|---|---|
| Fedora, the RPM installed | the button (§3.1–§3.5) |
| Fedora-like with dnf5 (`ID_LIKE` contains `fedora` or `rhel`) and the RPM | the button. NVIDIA's repository covers RHEL and Fedora. Not tested on these (§6). With an older dnf: instructions pointing at NVIDIA's guide (D48) |
| the AppImage, anywhere | instructions: "lmgw from an AppImage can't change the system, so here are the commands to run in a terminal." On Fedora, also: "or install lmgw's RPM, which can do this for you." |
| a distribution without dnf (Debian, Ubuntu, Arch, openSUSE) | instructions: a link to NVIDIA's install guide for the package manager. Distribution-specific package commands are not in the plan yet |
| image-based (`/run/ostree-booted`: Kinoite, Silverblue, Bazzite) | instructions: `rpm-ostree install nvidia-container-toolkit-base` after S1 and S2, then a restart. Nothing after that, since the spec is lmgw's own. Universal Blue's NVIDIA images reportedly ship the toolkit already, which the check would show as ok (not verified) |
| SteamOS | instructions only. It is read-only Arch and its devices are AMD, so the NVIDIA steps do not apply. Packaging for these systems is the proposal's "later" |
| NVIDIA card without NVIDIA's driver | instructions only (D8): on Fedora, "turn on third-party software sources in Discover (or GNOME Software) and install the NVIDIA driver, then restart", plus RPM Fusion's commands and its Secure Boot notes in the fold |

### 3.8 Packaging

- The helper and the policy go into the RPM through `bundle.linux.rpm.files` in
  `src-tauri/tauri.conf.json`: `/usr/libexec/lmgw/lmgw-machine-helper` from
  `target/release/lmgw-machine-helper` and the policy file from
  `crates/lmgw-machine-helper/polkit/` (built in WP6). `ci/build.sh` cleans the helper with the
  other version-bearing crates (`cargo clean -p lmgw -p lmgw-core -p lmgw-machine-helper`: it
  names its lmgw version in `plan_changed`), builds it with `cargo build --release -p
  lmgw-machine-helper` right before the bundle step, and fails when that left no regular file;
  the GitHub release workflow runs the same `ci/build.sh`. The bundler gives each file the mode it
  has on disk (F1), so the helper is 0755 under the script's `umask 022` and the policy 0644 after
  its `chmod go-w`. They do not go into the AppImage's `files` map, because a root helper on a
  user-owned FUSE mount is exactly what must not exist.
- **Before either ships, F1 is fixed and guarded (WP0).**
  - The cause: the CI job sets `FF_DISABLE_UMASK_FOR_DOCKER_EXECUTOR: "true"`, so the container executor no longer clones with umask 0000.
  - The barrier, whatever the cause: `ci/build.sh` reads the built RPM (`rpm -qlvp`, `rpm -qp
    --qf` with file modes, owners and flags, and `rpm -qp --scripts`) and fails the build on: any
    group- or world-writable file; any setuid, setgid or sticky bit; any file not owned by
    root:root; a non-regular file (symlink, directory) at the helper's or the policy's path; a
    helper that is not 0755 or a policy that is not 0644; any path outside an allowlist
    (`/usr/bin/lmgw`, `/usr/libexec/lmgw/lmgw-machine-helper`,
    `/usr/share/polkit-1/actions/io.github.lmgw-dev.lmgw.machine-setup.policy`,
    `/usr/share/applications/lmgw.desktop`, `/usr/share/icons/hicolor/*/apps/lmgw.png`,
    `/usr/share/licenses/lmgw/**`, and their directories); and any scriptlet (the RPM has none
    today, so one appearing is a reviewed change to the guard). This also repairs today's 0666
    icons and licence files. The guard runs on the GitHub release build too.
  - Its own test: CI builds two sample RPMs with `rpmbuild` (rpm-build in the CI image), a clean one
    and one with a 0666 file, a setuid file, a file outside the allowlist and a `%post`. The guard
    must pass the first and name each defect in the second.
  - **The content of the two root-relevant files (WP6, D83).** Mode, owner and path checks pass a
    policy whose content says `<allow_any>yes</allow_any>`, or another program at the helper's
    path. `ci/rpm-allowlist.toml` gains `[pinned."<path>"]` entries with the exact `mode` and the
    content: the policy by its `sha256`, the helper by `built = "target/release/lmgw-machine-helper"`
    (its bytes change with every build, so no hash can be pinned): the package's copy must have
    the digest of the file this job built, which must be a regular ELF file. `ci/rpm_guard.py`
    reads the contents from the header's file digests (`FILEDIGESTS`, sha256 in tauri-bundler's
    RPMs, `FILEDIGESTALGO` 8 on an installed 0.5.0 test build), which rpm checks every file it installs
    against, and compares a pinned file in sha256 alone: another `FILEDIGESTALGO` (MD5 and SHA-1
    collide on purpose) fails each pinned file (D83). Every allowlisted file under
    `/usr/libexec/` or `/usr/share/polkit-1/` must have a pin, or the allowlist is refused (exit
    2). `--installed` compares no built file and says so.
    `ci/rpm_guard_test.py` checks that the policy's pin is its source's sha256 and the helper's
    `built` is what `tauri.conf.json` packs, and builds samples with a changed policy, other
    modes, a helper that is not the build, a missing, symlinked or non-ELF build, and allowlists
    with a pin the guard cannot honour; the samples ship the real policy and a stand-in helper
    against an allowlist copy whose `built` names the stand-in. When a scriptlet is ever allowed,
    its `*FLAGS` tags join the scriptlet hash (not yet).
- `depends` gains `polkit` (D19), and the allowlist's pinned `requires` with it (built in WP6). No
  scriptlets are needed: Tauri supports them (`postInstallScript` and friends), but polkitd picks
  the action up by itself.
- The in-app updater replaces the helper with the RPM, like every other file. A running lmgw then
  has older plan code than the helper on disk, which exit 2's version and schema catch (§3.5, D75).

## 4. API and UI

### 4.1 Types (`lmgw-api-types/src/machine.rs`)

```rust
pub struct MachineCheck {
    pub checked_at: String,            // UTC, `YYYY-MM-DD HH:MM:SS`, as the other DTOs
    pub verdict: MachineVerdict,       // ready | needs_fix | needs_steps | needs_restart | needs_driver | cloud_only
    pub headline: String,              // §1.7, plain
    pub items: Vec<MachineItem>,       // §1.1 … §1.6, in order
    pub plan: Option<MachinePlan>,     // None when nothing is to be done or offered
}
pub struct MachineItem {
    pub id: MachineItemId,             // graphics_card | card_for_models | running_models | settings_fit | room | download
    pub state: ItemState,              // ok | problem | info | not_checked
    pub sentence: String,              // plain
    pub detail: Vec<String>,           // technical lines for the fold
    pub cause: Option<MachineCause>,   // typed, §1 tables (driver_mismatch { kernel, userspace }, toolkit_owner { … }, running_old_settings { models }, …)
    pub action: Option<MachineAction>, // what can be done from here without a password (§1.4's switch, WP12)
}
pub enum MachineAction {               // tag `kind`
    SettingsFit { fit: SettingsFit, label: String, changes: Vec<SettingsFitChange>,
                  left: Vec<SettingsFitLeft>, running: Vec<SettingsFitRunning> },
}
pub enum SettingsFit { AmdVulkan, NvidiaCuda }
pub enum SettingsFitValue {            // untagged: a string, or the list of run arguments (D119)
    Text(String), Args(Vec<String>),
}
pub struct SettingsFitChange {
    pub class: String,                 // chat | aux | audio | image
    pub model: Option<String>,         // the row's id; None for the class's setting
    pub field: String,                 // "Settings → Runtimes → Chat → Image", "Image override" (D119)
    pub from: Option<SettingsFitValue>, // None: the row uses its class's
    pub to: Option<SettingsFitValue>,   // None: the row's own setting is cleared
}
pub struct SettingsFitLeft { pub class: String, pub model: Option<String>, pub field: String, pub value: SettingsFitValue, pub why: String }
pub struct SettingsFitRunning { pub class: String, pub model: String, pub stop: Option<SettingsFitStop> } // stop: None in an offer
pub enum SettingsFitStop { Stopped, NotRunning, Busy { in_flight: u32 }, Failed { error: String } } // tag `kind`
pub struct SettingsFitOutdated {       // a confirmed list that is not the switch now (D135)
    pub withdrawn: Vec<SettingsFitChange>, pub added: Vec<SettingsFitChange>, pub models: Vec<String>,
}
pub struct MachineSettingsFitArgs { pub fit: SettingsFit, pub confirm: Option<Vec<SettingsFitChange>> }
pub struct MachineSettingsFit {        // machine_settings_fit's answer
    pub fit: SettingsFit,
    pub applied: bool,                 // written, or only what it would write
    pub sentence: String,
    pub changes: Vec<SettingsFitChange>, // outdated: the switch as it is now, to confirm
    pub left: Vec<SettingsFitLeft>,
    pub running: Vec<SettingsFitRunning>, // after a write, what happened to each (D136)
    pub outdated: Option<SettingsFitOutdated>,
    pub notes: Vec<String>,            // the Settings save's own message
    pub check: Option<MachineCheck>,   // the check after the write
}
pub struct MachinePlan {
    pub hash: String,                  // §3.2, over lmgw-machine's HashedPlan, not over this DTO
    pub how: PlanHow,                  // helper | instructions { why: String }
    pub steps: Vec<PlanStep>,
}
pub struct PlanStep {
    pub kind: PlanStepKind,            // s1_add_repo | s1_enable_repo | s2_add_key | s4_install | s4_update | s4_swap
    pub sentence: String,              // the confirmation's bullet
    pub commands: Vec<Vec<String>>,    // exact argv, run as root (or typed with sudo)
    pub writes_file: Option<PlanFile>, // path, mode, full content and a key's fingerprint, for S1/S2 (D144)
    pub download_bytes: Option<u64>,   // from `dnf repoquery -C`, when the user's cache can tell
    pub terminal: Vec<String>,         // the same step as lines to type, with sudo (D53)
    pub package_scripts: Option<String>, // what the package's own install scripts change (D144); not hashed
}
pub struct PlanFile {
    pub path: String,
    pub content: String,
    pub mode: u32,                     // as root writes it, 0o644; in the hash
    pub replaces: bool,
    pub key_fingerprint: Option<String>, // an OpenPGP key's, from `content`, "C95B 321B …"; display only
}
pub struct MachineSummary { pub verdict: MachineVerdict, pub headline: String, pub checked_at: String }
// The fix's command (built, WP7, D131); not part of the HTTP API, so no JSON schema:
pub enum MachineApplyEvent {           // over the command's Channel (§4.3), tagged by `event`
    Password,                          // confirmed: pkexec is asking for the password
    Authorized,                        // the helper's first `begun`: the password phase is over
    Following { state: String },       // a setup from before runs: followed, nothing asked
    Started { steps: usize },
    StepStarted { step: usize, of: usize, kind: PlanStepKind },   // step counted from 1
    Line { step: usize, kind: PlanStepKind, stream: MachineLineStream, line: String }, // text
    StepEnded { step: usize, kind: PlanStepKind, exit: Option<i32>, signal: Option<i32>, error: Option<String> },
    Note { line: String },             // systemd-run's words, a journal note: text
    Finished { exit: i32 },            // 0 every step ran, 3 one failed
    Checking,                          // the check afterwards runs
}
pub enum MachineLineStream { Stdout, Stderr, Helper }
pub struct MachineApplyResult {
    pub outcome: MachineApplyOutcome,  // applied | nothing_to_do | declined | unavailable | plan_changed
                                       // | read_differently | updated | dismissed | no_agent
                                       // | not_authorized | step_failed | refused | task_failed
                                       // | failed | ended
    pub sentence: String,              // plain: what happened and what to do next
    pub step: Option<PlanStepKind>,    // the failed step, or the last that began
    pub output: Vec<String>,           // every line and note, in order, as text
    pub detail: Vec<String>,           // pkexec's, the helper's, systemd's words; D88's two plans
    pub check: Option<MachineCheck>,   // the check afterwards (or before the dialog); None: a gate
}
```

`GatewayStatus` gains `machine: Option<MachineSummary>` (`None` until the boot check finished), so
the shell, the tray and every page that polls status see the verdict without another request.
`/api/events` carries the same summary as its `machine` frame, on connect once a check finished
and whenever the last result changes (D183).

Since WP8b's review round, `MachineCheck` and `MachineSummary` also carry the last start of a
local model on the graphics card that failed because of this computer (D182):

```rust
    pub failed_start: Option<MachineFailedStart>, // on MachineCheck and MachineSummary; None before one
pub struct MachineFailedStart {
    pub model: String,                 // the model's id
    pub class: String,                 // chat | aux | audio | image
    pub at: String,                    // UTC, `YYYY-MM-DD HH:MM:SS`
    pub count: u64,                    // such starts since lmgw started, this one included
}
```

and the Test op's `ModelTestFailed` (`lmgw-api-types/src/model_test.rs`) carries `code:
Option<String>`, the gateway's error code when lmgw's own error said why (D189).

### 4.2 Op, MCP tool, API docs

- **Op `machine_check`** (read, owner): `{ "fresh": bool }`, which answers `MachineCheck`. Without
  `fresh` it gives the last result. With it, it gives a new run, joined only when one began reading
  after the request came in (D61).
  Listed in `web/op_names.rs`, dispatched in `web/api.rs`, documented by an `OpDoc` in
  `openapi/ops/table/runtime.rs` (tag `ops-runtime`, beside `hold_set`).
- **MCP `lmgw__machine_check`** (`writes: false`, in `mcp/selfadmin/catalog/reads.rs`). Its
  description says the fix runs only from the app's window on this computer and that the plan's
  commands are in the answer. An agent can explain the plan to the person, but it cannot run it.
- **Op `machine_settings_fit`** (write, owner, built in WP12; `machine/settings_fit.rs`, the plan
  in `machine/fit/switch.rs`, what it reads beyond the settings in `machine/fit/present.rs`):
  `{ "fit": "amd_vulkan" | "nvidia_cuda", "confirm": [the changes shown] }`, answering
  `MachineSettingsFit`. Without `confirm` it writes nothing and answers what the switch would
  change, field by field, and the running models it would stop: the list §1.4's action carries
  (D114). With it, it works the switch out again under the settings lock and, only when that is
  the confirmed list (D135), switches each class set for the other card to the image and audio
  backend lmgw ships for that card with its run arguments swapped (D111, D112, D138) through the
  Settings page's own save, with the rows' own settings that follow in that save's transaction
  (D113, D115); then it stops the running models it concerns, none forced (D136), and answers the
  fields it changed by their labels, what it left, what happened to each running model, the
  save's notes and the check run again. A confirmed list that is not the switch now writes
  nothing and is answered with `outdated` and the list as it is now. Refused with a plain reason
  only when this computer has no card of that maker (D116). No password (D18). Listed in
  `web/op_names.rs`, dispatched in `web/api.rs`, documented by an `OpDoc` in
  `openapi/ops/table/runtime.rs` (with a confirm note), so the API reference page shows it. A
  self-admin tool for it is not there yet (D117).
- **Docs entries for `gpu_setup` (built, WP5):** the error-code list (`openapi/v1/errors.rs`,
  with `engine_download`), the `x-lmgw-fallback-reason` enum and its description
  (`openapi/headers.rs`, with `engine_download`; a unit test holds the enum to
  `FallbackReason::ALL` and each value to its description), `/v1/models`' notes
  (`server.rs`), and the API reference page that renders them (§2.5). The Traffic page labels
  the two reasons ("fallback · graphics card not ready", "fallback · engine download failed").

### 4.3 The shell command

Built (WP7): `async fn machine_apply(app: AppHandle, hash: String, on_event:
tauri::ipc::Channel<MachineApplyEvent>) -> Result<MachineApplyResult, String>`
(`src-tauri/src/machine.rs`, with `machine/{gates,confirm,spawn,progress,outcome,reason,follow}.rs`;
the types are §4.1's, in `lmgw-api-types`, so the dashboard reads the same ones). Progress goes
over the `Channel` passed as the command's own argument, not over window events:
`capabilities/remote-ui.json` grants the page `core:event:allow-emit` but not
`core:event:allow-listen`, so the page could not listen to them, and a `Channel` needs no event
permission. The command is in `src-tauri/build.rs`'s command list, granted in
`capabilities/remote-ui.json` (`allow-machine-apply`), and checked with `media::command_allowed()`
like the audio commands. The gates are in §3.4. `Err` only while lmgw is still starting (and the
invoke handler's own "refused: …" for another document); every refusal is an answer with its
outcome (D131).

Before it spawns anything, the shell computes the plan again in-process (a fresh check) and shows
the native confirmation (§4.5) from that plan. It goes on only if that plan's hash equals the
argument and the person pressed "Set it up" there.

**The contract for WP8a** (the `MachineCheck` component, the progress view):
- Call it as the audio commands are called, through `window.__TAURI_INTERNALS__.invoke`:
  `invoke("machine_apply", { hash, onEvent })` (Tauri renames the argument to camelCase). `hash`
  is the shown plan's `MachinePlan::hash`; offer the button only when its `how` is `helper`.
  `onEvent` is a channel as `@tauri-apps/api`'s `Channel` makes it: the string
  `"__CHANNEL__:" + id` for `id = __TAURI_INTERNALS__.transformCallback(cb)`. Tauri 2.11.2 (the
  version `Cargo.lock` pins) calls
  `cb` with `{ message, index }` (a `MachineApplyEvent`, `index` counting from 0) and once with
  `{ end: true, index }`; a message above 8 KB is fetched through
  `plugin:__TAURI_CHANNEL__|fetch`, which Tauri lets through for any origin, by the code Tauri
  itself evaluates in the page, which then calls `cb` with `{ message, index }` like any other
  (`ipc/channel.rs` in 2.11.2): the page's callback does nothing of its own for it. Deliver in
  `index` order, as `Channel` does (WP8a: `bridge::Ordered`, D156).
- Show `password` → "Your computer is asking for your password" until `authorized`; `following`
  → "The setup is still running from before"; the step list from `started` and `step_started` /
  `step_ended`; `line` and `note` as text nodes (never `inner_html`): their text is a program's
  output, byte for byte, markup and terminal escapes included.
- The answer's `sentence` is what the person reads; `detail` and `output` go into a fold; `check`,
  when present, replaces the shown check (it is published in core too, so the status and the tray
  agree). For `read_differently` the new check's plan is instructions, so the button goes by
  itself; for `no_agent`, show the plan's terminal lines.
- The command never runs twice at once (D125), and closing the page does not stop a setup; a page
  that opens while one runs learns of it only when it calls the command (it then follows, D122).
  Within one document the run is the app's (WP8a, D160): a page left and opened again shows it,
  and the tray's item moves the page without loading a new document (D162).
- **The channel's events go to whatever document the window runs** (the review's L6): Tauri
  2.11.2's `Channel::send` evaluates `__TAURI_INTERNALS__.runCallback(<id>, …)` in the webview's
  current document, not the one that called. After a reload or a navigation the new document has
  no callback of that id (Tauri's internals find none and say so on the console): the page must
  not take that for an error of its own, a component that was unmounted must ignore events that
  still reach it, and a new page learns of a running setup by calling again (D122). An agent app's document loaded
  in the window may receive them too: they carry the plan's step kinds and the setup's output,
  nothing that is not shown to the person anyway, and the page must never put anything into the
  command's arguments that it would not show there.
- The answer's sentences the page shows as they are: `unavailable` with "Open lmgw's window and
  press Set it up there." when the window was hidden or another window had the focus (D133),
  `declined` with "Nothing was changed: you chose "Not now" already. Reload the page to be asked
  again." for a document that said "Not now" before (D140). The dialog's "Set it up" becomes
  pressable a second after it got the focus (D134); the page has nothing to show for that.

### 4.4 UI pieces

- **`widgets/machine_check.rs`**, one component. It shows the headline, the items (a mark, the
  sentence, and a "Details" fold per item), the plan block (the sentences, the commands fold and
  the button, or the instructions with a copy button, or the browser wording of §2.7), "Check
  again", and, where §1.7 allows it, "Try the graphics card". It is used in Settings → GPU ("This
  computer", `#machine`), the Overview card or checklist, Chat's empty state, and the Chat error
  card for `gpu_setup`.
- **`MachineBanner`**, the one-line Chat banner of §2.2.
- **The native confirmation** (§4.5) and the **progress view** (step list, the password note, the
  "keeps running if you close lmgw" line, the outcome of §3.5).
- **The settings-fit confirm** (WP8a's, from the data WP12 built: §1.4's `action`). It lists each
  changed field by its human label, old to new, what is left, and the running models it stops,
  then posts `machine_settings_fit` with `confirm` set to the action's `changes`; an answer with
  `outdated` shows what differs and the new list to confirm (D135).

Built in WP8a (`crates/lmgw-ui/src/widgets/machine_check.rs` and its `bridge`, `items`, `plan`,
`progress` and `fit`; `assets/machine.css`): `widgets::MachineCheck { fresh, not_now }` is the
headline, "Check again" and the items, and the surface draws the card around it (D145). Each fix
shows under the item it is for (D147): the plan block, the progress view and the setup's answer
under §1.2's item, the settings-fit confirm under §1.4's (D157, D165). The check and the run are
the app's, in `state` (D160). The progress view is `progress::Progress`, changed only by an event
or the answer (D153, D154), each part of its view on a memo of its own, so a line adds a row and
nothing else; the answer's note has its tone by outcome (D155). Folds grow and the pane scrolls,
the live output following its end while it is read and the run's head sticking to the top
(D152, D161). "Try the graphics card" is not built yet: core has no probe for it yet.

Built in WP8b (`widgets/machine_check/surfaces.rs`, `nudge.rs`, `involved.rs`;
`pages/chat_machine.rs`): `MachineBanner { fix_on_page }` is §2.2's line (D174, D185), stepping
aside while a `gpu_setup` card below holds the fix (D187); `MachineInCard` is the same line
inside a card under a failed model test or a lab run that failed with `gpu_setup`, with no "Not
now" (D179, D188); `MachineCard { class }` is the Overview's card and the one above Chat's empty
state, "This computer" and "Not now" at its head, the component inside with `fresh` and
`compact` (D172, D173, D175, D186). Both show by `nudge`'s one rule and its stored "Not now"
(`Nudge`, provided with the `MachineState`, D170, D171, D182), and by whether local models are
involved (`Involvement`, from the admin plane's rows, D184). The app's `MachineState` follows the
live feed's `machine` frame (D183). The Chat reaches them only through its host's `ChatMachine`
slot, with the component under a `gpu_setup` card (`GpuSetupCard`, D176, D177).

### 4.5 The plan block and the confirmation

The plan block in the page explains, with the exact commands behind a fold. Its "Set it up" opens
the confirmation, a native dialog the shell draws from its own plan (a GTK message dialog, modal
over the app's window, `src-tauri/src/machine/confirm.rs`; `tauri-plugin-dialog` can neither make
"Not now" the default nor hold a fold, D121), with "Not now" (the default, focused) and "Set it
up". Then comes the password. That is one
confirmation and one password, as ruling 1 asks. The native dialog is the confirmation because
`command_allowed` only proves the caller is a document at the gateway's origin: a script in stored
content served there could call the command, but it cannot press a button in a dialog the shell
draws. Since WP13 every answer of the gateway's origin but the dashboard's own sends a sandboxing
CSP (D90); the native dialog stays because the page is the same origin as the gateway's own API, not
because stored content might run.

For a fresh Fedora with an RPM Fusion driver and no toolkit, the plan block reads:

> **Set up your graphics card for local models**
>
> lmgw will make these changes to this computer:
> - Add NVIDIA's software source (nvidia.github.io), limited to the one piece below, so that piece
>   gets updates with the rest of your system
> - Install NVIDIA's tool that lets models use the graphics card · 8.5 MB download
>
> After that, lmgw keeps the description of your graphics card up to date by itself, without
> asking again.
>
> Besides these, NVIDIA's package runs its own install scripts; "Show the commands" says what they
> change.
>
> Your computer will ask for your password once. If you're not an administrator, someone who is
> has to type theirs.
>
> ▸ Show the commands
>
> [Not now] [Set it up]

The native confirmation repeats the two bullets, the package-scripts line and the password line,
and has a "Show the commands" fold of its own: every command by its absolute path, in order, each
file with its mode and a key with its fingerprint, after each package step what its own scripts
change, then every file the steps write, in full (D121, D144). "lmgw changes only these things."
stays for a plan that installs no package (S1, S1′ and S2 alone), where it is true. The dialog
shows only over the app's window in front (D133), and its "Set it up" is greyed out until it has
had the focus for a second (D134). The size
comes from `dnf repoquery -C` (8.5 MB for `nvidia-container-toolkit-base` 1.20.1, in a measurement). When the
user's metadata cache can't tell, the bullet carries no size. "Show the commands" opens:

> These run as the administrator (root), in this order, as the system task `lmgw-machine-setup`:
> ```
> create /etc/yum.repos.d/lmgw-nvidia-container-toolkit.repo (mode 0644)   (the file below)
> create /etc/pki/rpm-gpg/RPM-GPG-KEY-lmgw-nvidia-container-toolkit (mode 0644)   (the file below: an OpenPGP key, fingerprint C95B 321B 61E8 8C18 09C4 F759 DDCA E044 F796 ECB0)
> /usr/bin/rpmkeys --import /etc/pki/rpm-gpg/RPM-GPG-KEY-lmgw-nvidia-container-toolkit
> /usr/bin/dnf5 install -y --repo=lmgw-nvidia-container-toolkit --setopt=lmgw-nvidia-container-toolkit.skip_if_unavailable=False --setopt=lmgw-nvidia-container-toolkit.gpgcheck=1 --setopt=lmgw-nvidia-container-toolkit.gpgkey=file:///etc/pki/rpm-gpg/RPM-GPG-KEY-lmgw-nvidia-container-toolkit --setopt=install_weak_deps=False nvidia-container-toolkit-base
> NVIDIA's package then runs its own install scripts: they write /etc/nvidia-container-runtime/config.toml when it is missing, switch on and start NVIDIA's refresh service (nvidia-cdi-refresh), which writes NVIDIA's own description of the card to /var/run/cdi/nvidia.yaml, and reload the system's device rules.
> ```
> /etc/yum.repos.d/lmgw-nvidia-container-toolkit.repo, in full (mode 0644): (§3.2's text), then the
> key file, in full (mode 0644).

The page's block, as WP8a built it (D147 to D149): it sits under §1.2's item, says the dialog's
own text (the title, the intro, the bullets, the package-scripts line or "lmgw changes only these
things.", the password line) with "After that, …" after the bullets, and its "Show the commands"
has what the dialog's fold has, the package's own scripts as a line in words, and then the steps'
terminal lines with "Copy the commands". "[Not now]" is not in the page's block: the surfaces
that can be waved away have it in their card's head (D175, D190), and "Set it up" stands alone,
left under the text. Outside the app's window §2.7's sentence stands before the fold in the button's place,
and the intro and the password line say where lmgw runs (D163).

Other bullets, as the plan needs them: "Switch on NVIDIA's software source, which is on this
computer but switched off"; "Replace the piece Fedora stopped updating
(golang-github-nvidia-container-toolkit 1.17.4) with NVIDIA's current one · 8.5 MB download";
"Update NVIDIA's tool that lets models use the graphics card". The polkit prompt's own message
(§3.3) fits every plan. There is no separate "refresh" dialog any more: lmgw rewrites its own spec
without asking (§3.6).

## 5. The download before a model's first start

The engine images are big. On a Fedora 44 test system `llama.cpp:server-cuda` is 4.37 GB unpacked,
`audio.cpp:full-cuda12` 4.76 GB and `stable-diffusion.cpp:master-cuda` 3.93 GB, so a first message
today can sit in "starting the container" for minutes, holding the admission gate and a VRAM
reservation all the while. It is in this package as WP10 and WP11, and it does not depend on the
helper.

- **The download happens before admission.** A start whose model is not up asks
  `podman image exists -- <image>`; when the image is missing it downloads it first
  (`VramScheduler::engine_download`, `vram/download.rs`): in a request's admission (`admit_local`,
  `vram/queue.rs`, on each pass, so a start over after a failed climb too) and in a start nobody
  waits on (`check_background_start`). The pull is `pull::pull_and_wait`
  (`backends/pull/ensure.rs`): `pull::begin` starts the existing `image_pull` job for the reference
  (`JobKind::ImagePull`, keyed `image:<reference>`) or joins it, and the wait follows the job's
  bytes. So two models of one class, or a start and an Engines-page pull, share one download.
  Nothing is evicted and nothing is reserved for a model whose engine is still downloading: the
  gate (`take_gate`), the evictions and the reservation all come after. The queue's time budget
  (`queue_timeout_seconds`, measured from `started` in `arbitrate`) starts after the download, so it
  measures only the wait for the card. The start itself then runs with `--pull=never`
  (`runtime::argv::never_pull`), so the bytes are never fetched twice. While the wait runs, the GPU
  hold or a benchmark's lease ends it, and so does the outside-VRAM verdict for a request with a
  fallback; a wait that ran to its end reads the row again; a guest begins the download and does
  not wait; a benchmark downloads before its lease (D28 to D32).
- **The waiter says so.** `WaitPhase` (`lmgw-api-types/src/status.rs:579`) gains `Downloading {
  image, done_bytes, total_bytes: Option<u64> }` as its own first phase. The waiter entry for it
  holds no place in the admission queue. Callers that follow their phases (chat media §5.3) hear
  it. The vram view shows it.
- **Bytes from Podman's own API.** `podman pull`'s output carries no byte counts when it is not on
  a terminal. Podman's API does: the libpod pull with `compatMode=true` goes through
  `utils.CompatPull`, which reports each layer as "Pulling fs layer", "Downloading" (with
  `current`/`total` bytes), "Already exists" or "Download complete" (read in Podman's source on
  `main`, 2026-11-03, `pkg/api/handlers/utils/images.go`, and measured on 5.8.9, see "Unverified"). lmgw starts a private
  `podman system service --time=<idle> unix://$XDG_RUNTIME_DIR/lmgw-machine/<instance>/podman.sock`
  for the pull. The socket is per instance, so a dev instance's service never answers production's
  pulls. It needs no root, uses the same storage, and the service exits when idle. lmgw does not
  rely on the user's `podman.socket`, which is not enabled by default. If the service won't start,
  the job falls back to today's CLI pull and says "size not known" rather than showing a made-up
  bar.
- **The total is known up front.** The anonymous registry client (`backends/oci.rs`) already reads
  manifests for the update check. It gains a read of the platform manifest's layer sizes, so the
  total is the compressed size of the layers this machine doesn't have. Layers Podman reports as
  "Already exists" leave the total.
- **Where it shows** (WP11, D38 to D41). One line — "Download for your graphics card · 1.2 of 3.4
  GB" (D14), a bar, the image reference and any size note in its tooltip — in the Chat's tool card
  and on the composer's status line for a turn whose model waits for it, in the GPU popover and
  Traffic's GPU panel beside the queue, under the Overview's own "Downloading before they start",
  as a chip ("graphics card download · 1.2 of 3.4 GB") in the Models list, the model editors and
  the Audio and Image labs, in the download wizard, the Downloads page and the recipe and audio
  catalog browsers, each line there saying who it is for; and the Engines page's pull chip and
  panel show the same bytes for an explicit pull. Once every byte is in, the line says "unpacking"
  until the start moves on.
- **No timeout is added.** The download takes as long as it takes, and an API client that gives up
  and retries joins the same job. But a client's own timeout, which lmgw cannot see, can end the
  request long before a multi-gigabyte download does. That is why D16 is the default: the download
  rarely happens on the request path at all.
- **D16, the default.** A model download (Hugging Face, an image recipe, the audio catalog) also
  queues the pull of its class image when that image is missing, shown as its own line on the
  download card in D14's words ("Download for your graphics card · 1.2 of 3.4 GB"; an earlier
  draft of this paragraph had "Also downloading what your graphics card needs to run models",
  which D14 does not use). Nothing starts, and the first message then only loads the model (built
  in WP11, D37). When the machine check's verdict turns `ready` (at boot, or after a fix, D73),
  lmgw pre-fetches the missing engine of each class that has a model on the graphics card as an
  ordinary pull job (built, `machine/prefetch.rs`, D72), unless NetworkManager says the connection
  is metered (D99): the job a first start or an Engines-page pull joins, never a `localhost/`
  build, nothing waited for and nothing started, each pull begun logged with its job. Pulling is not loading, so the 2026-10-13 ruling holds.
- **A failed download and a fallback** (WP5, D69): a request whose engine download fails is
  answered by its configured fallback, `x-lmgw-fallback-reason: engine_download`; without one,
  WP10's 502 with its plain sentence, code `engine_download`.

## 6. Tests

**Unit and integration, with fake command outputs and fake files.** Everything reads through a
`Facts` trait (files, commands, the NVML subprocess), so a test states the machine:

- *lmgw's own spec* (`lmgw-machine`, core's `runtime::cdi`): a current spec and stamp; one
  `hostPath` gone (regenerated once, the gone path in the log); the stamp's rpm change time, kernel
  version, userspace version, library file, `nvidia-ctk` file, `/dev/nvidia*` nodes or boot ID
  differing, or another file in the spec dir (regenerated once each, the file removed); the driver
  versions differing (nothing generated, the last spec kept); an unversioned library (generated,
  D26); two starts at once sharing one generation; `/dev/nvidia-uvm` missing (`nvidia-modprobe -u
  -c=0` run first, the node in the stamp); a failing generate (1.2's cause, `gpu_setup` on the
  start); a hung generate and a long wait for another one (stopped at the load timeout, the
  sentence naming the program and the setting); a package transaction during the write (not
  used); a spec dir others may enter (refused, untouched); no toolkit and a Podman without the
  option (Podman's default dirs, the reason kept, `podman --help` asked once).
- *The podman option:* the guard test of §3.6.4 over every builder, an isolated MCP server's create
  and start included; NVIDIA-only arguments give lmgw's dir alone; a row naming another CDI kind
  gives the system dirs then lmgw's; the run-time refusal at the registry, the benchmark's launcher
  and the agent spawner; the source scan of every podman spawn site.
- *System CDI resolution*, on fixtures modelled on a real nvidia-ctk spec, with the UUID and paths replaced into a
  temp dir: `/etc/cdi` plus `/var/run/cdi` (the later wins); two files in one dir (conflict); a
  same-dir conflict in `/etc/cdi` dropping the name even with another dir later in the list
  (pinning the behaviour read in `cache.go`); a JSON spec; a spec without `all`; a `cdi_spec_dirs`
  override.
- *Driver facts:* `/proc/driver/nvidia/version` lines (open and proprietary modules), the
  `libnvidia-ml.so.1` link, NVML subprocess errors mapped to causes (its `version_mismatch` a
  restart), a missing library leading to the libraries sentence, an unversioned library asking
  NVML and writing the spec (D59), not loaded with SecureBoot=1 leading to the Secure Boot
  sentence only for a module built before this boot (none built, one built after the boot: the
  restart sentence, D58), PCI listings with `nouveau`, `nvidia` and none, and an AMD card. **lmgw
  running across a driver update:** the facts start with module and libraries at 580.10 (ok); then the libraries on disk become 580.20 while the module stays 580.10, and the in-process
  ledger's fake NVML handle keeps answering. The check says restart, offers no plan, runs no NVML
  probe and writes no spec. After the "restart" (module 580.20) it says ok and regenerates.
- *The toolkit by owner:* each row of §1.2's owner table gives its plan; an owner signed by another
  key is not taken for NVIDIA's; a file nobody owns is never in an overwrite or install step.
- *The plan:* every fact combination gives its step list; S4′ takes every installed toolkit
  package and, through lmgw's own section, appends them to its `includepkgs` for that command (a
  person's section that filters one out blocks, D55); a plan with no steps never promises commands
  in its `why`; the toolkit from another repository is one scoped install whose hash root never
  accepts (D56); an override file not readable by every user is skipped and named; root's facts
  inherit no environment (D60), and the user's side reads the plan through root's environment, so
  a program that answers otherwise in the user's leaves the hash as root's (D87); S4′'s package
  list cannot be empty
  (`NonEmpty`), and a test over every generated plan asserts that each dnf argv ends with at least
  one package name and is never a bare `dnf upgrade -y`; every dnf step is scoped with `--repo=`;
  the provider table, when it names an enabled repository, removes S1 and S2 and scopes S4 to it;
  the advisory `repoquery` finding another provider turns the plan into instructions; an existing
  NVIDIA section is never rewritten and gets `<scope>`'s package checks; a disabled one gives S1′; a
  section disabled in its file and enabled by an override counts as enabled, and the reverse; an
  unreadable `.repo` file is skipped and named; an existing file of S1's or S2's name that lmgw did
  not write is never overwritten; lmgw's own file with other content is replaced through a
  temporary file in the same directory; ostree, AppImage and non-dnf give instructions with the
  same commands.
- *The hash:* stable across runs; changes with any argv or file content; unchanged when a step's
  sentence or `download_bytes` changes; on a mismatch the helper's line carries its plan and
  schema.
- *The embedded key:* its fingerprint equals `C95B321B61E88C1809C4F759DDCAE044F796ECB0` (parsed
  in-process, or with `gpg --show-keys --with-colons` where gpg is present); S2's condition read
  from `rpmkeys --list` output with and without it; the repository text equals §3.2's.
- *The helper:* the argv grammar (extra arguments, a bad hash, an unknown verb: exit 4); `apply`
  refusing without `PKEXEC_UID` or with a non-zero effective UID, `run` refusing outside the unit's
  cgroup (all injected), each with one `refused` line and no journal entry; a plan mismatch exits
  2, runs nothing and prints plan and schema; a failing step stops the rest and reports its output;
  an active unit of the same name gives exit 5; `run`'s 0, 2, 3 and 4 passed on only with `run`'s
  matching line, and exit 6 for each code without it (D102); a closed stdout (the reader gone) and
  a stdout nobody reads (a real pipe, never read, with a step printing far more than it holds) do
  not stop a running step (D100); a program that leaves a process behind holding its pipes ends
  its run when it ends, with everything it wrote (D101); the JSON lines parse. All steps run
  through a fake executor, and no test runs dnf, systemctl or systemd-run.
- *The shell command:* each gate of §3.4 refuses with its reason (debug, dev, a program other than
  `/usr/bin/lmgw`, another data dir, a helper writable by others, owned by a user, setuid or a
  symlink, the policy missing, no `/usr/bin/pkexec` while another `pkexec` is on `PATH`, `how` not
  `helper`); the native confirmation declined spawns nothing; a confirmation whose plan hash differs
  from the argument spawns nothing; exit codes map as in §3.5, including 127 with and without "No
  authentication agent found", 5, 101 and a signal; events arrive on the `Channel`; a success
  re-checks, generates the spec and re-installs the probe. The spawner and the dialog are fakes.
  Built in WP7 (`src-tauri/src/machine/tests.rs`, against a stated `World`): every gate above
  (and Linux, a replaced `/usr/bin/lmgw`, an unreadable program path) refusing with its sentence
  and reason before anything is checked, asked or started; a plan offered as instructions (not
  dnf5) refused with the check's own reason; a hash the fresh check does not compute and a check
  with no plan answered without a dialog; a second call while one runs; "Not now" starting
  nothing; the dialog's bullets, its package-scripts line or "only these things" for a plan
  without a package step, absolute command lines, each file's mode, the key's fingerprint, the
  package's note after its step and the files in full (`tests/confirm.rs`); the window hidden,
  not active or without a document answered `unavailable` with no dialog (D133); a "Not now"
  answered again without a dialog in the same document and asked again in a new one (D140);
  `confirm::Arming` armed only by the newest wait with the dialog mapped and active, disarmed by a
  lost focus or an unmap (D134); pkexec's exact arguments and the real spawn refusing in a debug
  build; exit 0 with its events in order
  (`password`, `authorized`, `started`, `step_started`, `line`, `step_ended`, …, `finished`,
  `checking`) and `after_fix` run, and its sentence when the check afterwards disagrees;
  `nothing_to_do`; 126, 127 with and without "No authentication agent found", and 127 after the
  helper's first line; 127 with pkexec's own words about the program ("Error executing",
  "Error accessing", "Cannot run program") `failed`, or `updated` with the program replaced
  (D142); 101, a signal and a child that dies mid-run; exit 3 with the download, the lock and the
  fallback reason, and dnf5-, rpm- and librepo-worded samples of a key lmgw lacks, a download that
  does not verify, both at once and dnf5's closing words alone (`tests/reasons.rs`, D141); 4 and
  6; exit 2's branches (another version or schema, a `+<pipeline>` suffix included; the program
  replaced while the dialog was open, D143; a new plan root also computes; a plan root does not
  compute, recorded with core, both plans in the detail); exit 5 followed through systemd and the journal (cursor by cursor) to `applied`, a unit
  found running before the dialog followed without a password, a journal that cannot be read, no
  invocation, systemd failing mid-follow, and a followed unit whose journal says a step failed;
  the journal's arguments (trusted matches only, no `SYSLOG_IDENTIFIER`) and its entries filtered
  again (another unit's, another user's, an earlier invocation's, none at all, a byte-array
  value); a `line` with markup, quotes and terminal escapes passed on byte for byte; the events'
  wire names; `media`'s document numbers (a commit, a reload and the first document seen take a
  new one, a finish of the running document does not). Core: `machine_check::fixed` (after the
  steps: the spec written and the check published, the ledger's probe replaced only when it read
  nothing and the card works, a plan read differently offered as instructions from then on), the
  plan DTO's modes, the key's fingerprint and the install step's note (`machines.rs`), and
  `machine::key` (the embedded key's fingerprint in groups of four, agreeing with
  `NVIDIA_KEY_FINGERPRINT`; none for the repository text or a broken armor); lmgw-machine: `how`
  for a plan read differently, `command_line`'s quoting, and `plan::scripts` (a note for S4, S4′
  and S4″ through dnf, none for S1, S1′, S2 and rpm-ostree, none of it in the hash).
- *Core:* the `machine_check` op with and without `fresh`, a fresh request never answered by a run
  that began before it asked (D61), the boot run publishing only after the spec write, the MCP tool
  listed as a read, the status summary, each failed spec write blamed on what failed (D57), the
  headline taken from the item that decided the verdict (D50), the download's size with a shared
  and an already stored blob, an unreadable manifest and the low-space line (D62), the probe's
  argument grammar, a failed GPU start triggering a re-check and `gpu_setup` on `/v1`; with a
  fallback, the fallback answering with `x-lmgw-fallback-reason: gpu_setup`, and the resolve-time
  swap while the verdict names a driver or toolkit cause; no swap for a CPU row; `gpu_setup` counted
  as a failure, not a refusal; the op table, header and error-code docs entries (the existing docs
  guards catch a missing `OpDoc` or header value). Built for WP5 in
  `vram_admission::gpu_setup`: a failed start after a driver update (`503 gpu_setup`, the
  sentence without "Open lmgw" for a restart, Podman's words, one read and one full check after
  it, the status and the log row's code; the next start blamed from that check with nothing read
  again), the same on `/v1/messages` in Anthropic's shape, a failed start on a ready machine as it
  was read (its own 502, nothing read or run), a fallback answering and the next request swapped
  at resolve with nothing started or checked while a hop without a fallback still starts, a
  machine changed since the check (a restart: the start tried, and the check run again by
  itself), a fallback that does not resolve (the start tried each time, the clause naming it), a
  missing toolkit blamed for an NVIDIA row with "Open lmgw there" and not for a Vulkan row, no
  swap for a row on the processor or one that is up, a candidate alias's and a guest's fallback
  answering, a blank machine never blamed, and a failed engine download's code and fallback
  (D69); in `vram_admission::gpu_setup_rows` (WP5's review round): an Intel card's render nodes
  leaving Vulkan rows to their own starts and errors while the NVIDIA row gets the no-card words,
  an NVIDIA driver waiting for a restart beside an AMD card swapping only the NVIDIA row (D64), a
  pinned caller (`open_pinned`, quickdoc's embedder) trying the start and told `gpu_setup` with no
  hold in its words while a request that may fall back is swapped (D97), a Background warm saying
  `fallback` after a blamed start and then without one, `failed` without a fallback (D98), a folder
  that is not private never swapped for and started by the first request after its fix, a failed
  toolkit write swapped for until "Check again" writes the spec again (D95), and a slow read of
  the card keeping another model's admission from waiting; in `machine_check::blame` (D96): two
  starts failing at once sharing one read and one full check, and a write that ran out of time
  answered without NVML before the full check publishes; unit tests of the rows each item
  concerns, the words and the causes the swap takes (`machine/blame.rs`), the error's status,
  codes, words and outcome (`error.rs`), and the header enum against `FallbackReason::ALL`
  (`openapi/headers.rs`).
- *The settings switch* (WP12, `machine_check::settings_fit`): an AMD card with lmgw's NVIDIA
  defaults offering the switch with its nine fields by their labels (`needs_fix`, no plan); the
  op writing nothing without `confirm`, then every class through the Settings save, the answer
  listing what was offered and carrying a ready check, and a second call with nothing left to
  switch; rows of their own (a copy of the defaults cleared, other arguments kept with the device
  flags swapped, a pinned CUDA image cleared, a row on the processor given no card, a disabled
  row switched, a row already set for AMD cards left, and an image lmgw cannot tell left and
  named); an NVIDIA card with the ROCm values (a problem) and with lmgw's own Vulkan values
  (information), both offered the way back and ready after it; a class on the processor left and
  named; an Intel card and none offering nothing and refused, the other maker's settings on an
  NVIDIA card refused by name. The confirmation (`settings_fit::confirm`, D135, D136): a class
  edited after the offer refused with what differs and the new list (the owner's arguments kept),
  nothing written, the cached check already offering the new list without a new run, and that
  list written when confirmed; a row edited in the store under the snapshot refusing all of it,
  the classes included, by the model's name; running models stopped, a busy one named with what to
  do, the check after it `needs_steps` until that container stops, then ready. The cards and
  images (`settings_fit::cards`, D137 to D139): a computer with both cards switching there and back
  (D116), the chat class and a row ending as they began with the owner's arguments, a copy of the
  class cleared, ROCm's values ending on NVIDIA's without `keep-groups`, a row with a group of its
  own left and named; an owner's image judged by its label (CUDA: steps, nothing to switch;
  Vulkan: fits) and by its libraries, and fitting when nothing tells; an AMD card without
  `/dev/kfd` ready, its ROCm settings replaced by Vulkan's and ready after; a ROCm row on the AMD
  card beside an NVIDIA one no problem and the machine ready; the render nodes with an image
  lmgw can't tell left alike on a class and a row, and the processor's sentence beside the
  switch's information. Unit tests: which images are built for what in any spelling
  (`fit/shipped.rs`), an image's own reading (`fit/present.rs`), the flags swapped, a round trip of
  every class's defaults with the owner's arguments, the group clash, which settings are the other
  card's, and values with a space (`fit/switch.rs`). `verdicts` and `machines` follow (a CUDA
  image given the render nodes on an AMD card does not fit, D118).
- *The download:* a recorded compat-mode JSON stream (every status kind, an error mid-way) turned
  into job progress; a manifest fixture's layer sizes; two starts joining one pull; a cancel; the
  CLI fallback saying "size not known"; while a start downloads, a fake ledger records no eviction
  and no reservation, and the queue's clock starts after the download; a model download paired
  with its image pull, nothing started, and a first request joining that job; an image that is
  here or a `localhost/` build beginning none, two model downloads at once beginning one pull, an
  image recipe with every file on disk still fetching the image, and podman unable to say whether
  the image is here giving `not_known` with why (D16, D37, D42,
  `vram_admission::engine_with_model_download`; the bound in `web/hf/engine.rs`'s unit test; the
  image recipe and audio catalog answers' `engine` in `image_recipes` and `audio_catalog`); a chat
  turn's `downloading` state frames before `loading` and `ready`, then `held`, `fallback` and
  `failed` after `downloading`, `loading` after the download with admission switched off, and a
  candidate alias's turn (D40, `vram_admission::engine_download_turns`); a client that stopped
  reading holding up no admission and the newest frame going out once there is room
  (`web/chat_voice/turn_state.rs`); the words, figures, chip, unpacking, who a line is for and the
  wizard line's states of `gpu_download.rs` (D38, D41, D43); the pre-fetch when the verdict turns
  `ready` (`vram_admission::engine_prefetch`: ready begins the missing image's pull once and
  starts nothing, ready again begins none, the first request joins that job and pulls once; not
  ready begins none until it turns ready; images that are here begin none; a `localhost/` build
  is said and never pulled, and a disabled row's image is not fetched; the boot run's ready
  begins it; ready, another verdict and ready again begins it twice; a class whose rows are all
  on the processor is not fetched; a metered connection fetches nothing and says so, D72, D99),
  and what NetworkManager says of the default route's device (`machine/prefetch.rs`' unit test).
- *Packaging:* the `ci/build.sh` guard against the two sample RPMs of §3.8.
- *The dashboard (WP8a):* unit tests of the channel's `index` order and its end after the last
  message, Tauri's frames read as sent (`bridge.rs`); the progress from the password through the
  steps to the answer, a line kept as text, a followed setup's steps named by kind, a step settled
  by the answer, events after it ignored, every outcome's tone (`progress.rs`); the commands
  fold's lines as the dialog's, the package's note told from a command, shell quoting, the copied
  terminal text (`machine_check/tests.rs`); the switch's values, places and stops (`fit.rs`); the
  search rows and the card's place (`pages/settings/tests.rs`). From the review round: a followed
  setup lists no steps it cannot see and the keeps-running line ends with the steps
  (`progress.rs`); nothing to switch told from an offer, one running model said as one
  (`fit.rs`); the browser's words and the cloud-only item's mark (`machine_check/tests.rs`); one
  link per search and its name (`pages/settings/tests.rs`); a refetch keeping the edits and naming
  the ones written under (`FormState::refetched`, the kit's `form.rs`); only the dashboard's own
  routes taken from the shell (`shell/moves.rs`); the tray's script (`main.rs`). The shots' drive
  (`scripts/drive/machine-check.json`) carries expectations of its own: the button only in the
  app window, the plan block hidden while a setup is out, a message out of order, a line with
  markup shown as text, the returned check replacing the shown one.
- *The dashboard (WP8b):* unit tests of the reach-out rule and "Not now" (`machine_check/nudge.rs`:
  the check's state as the verdict and its problems with their causes, the stored answer with
  its failed start, a re-run that reads the same being the same state, each verdict, a cloud-only
  setup, "Not now" holding for its state and ending when the check reads otherwise; from the
  review round: a failed start after "Not now" ending it whoever made the request, two in one
  second being two, a restart of lmgw ending nothing, and the effect's body over the app's two
  signals forgetting the answer the moment the check reads otherwise and asking again when the
  state comes back); whether local models are involved (`machine_check/involved.rs`: nothing
  configured, a local row, an alias-only setup, an empty catalog with an upstream configured,
  rows off or on the processor); the banner's words (`machine_check/surfaces.rs`: "Set it up"
  only with the button, per verdict, a browser elsewhere told whose card); the `gpu_setup` card's
  parts (`pages/chat_machine.rs`: the sentence and the start's words folded, "Open lmgw there…"
  dropped where the fix is below, the first sentence alone on the card that holds it, a fallback
  clause after the sentence without the start's words, a message without the start's words);
  the fallback reasons in words (`voice_lab/rounds.rs`, `decision_lab/run_view.rs`).
- *Core (WP8b's review round):* `vram_admission::gpu_setup_stamp`: a failed start the machine is
  blamed for stamped on the last result, on the status's summary and on the live feed, kept by
  the full check that follows, counting on; none on a ready machine; the model's Test answering
  `gpu_setup` as a failed test with its code and stamping the check, and another start failure
  without that code. `openapi_live::events_initial_frames_validate` validates the `machine`
  frame against its documented schema.
- *The drive (WP8b):* `machine-check.json`, scenes 31 to 51 on `machine-fake.js`'s `models`
  (`local`, `alias`, `cloud`, `cloud_down`, `none`), `turn`, `test` and `lab`, its `machine`
  frames (`__fakeSetCheck`, `__fakeFailStart`) and its answer counters, which every "nothing
  shows" check waits on, so it cannot pass on a page that never read the check and the rows. It
  expects: the Overview's card first in its state column with "Not now" in its head, compact
  with the problem and "Set it up" within the column; none for cloud models alone, a provider
  whose catalog failed, or `ready`; a check that changes under the open Overview showing without
  a reload; the restart's card compact; a browser's card; "Not now" stored with its failed start
  and kept by a reload; a failed start from outside the Chat ending it on the open Overview; the
  one "Not now" hiding the card above Chat's empty state and the banner without the storage being
  cleared in between; the card above the empty state in the Chat's own box; the banner's words
  and link, "Not now" kept by a reload; the `gpu_setup` card saying only what failed with the
  compact fix under it, the stored answer gone and the banner stepped aside, the older card's
  sentence after a second try; the browser's card and, after a reload, the banner's "See what to
  do"; a restart's words with "See why" and its landing; no banner for cloud models alone or a
  failed catalog; the banner for an alias-only setup; a host without the slot (no banner, no
  check read, no block, the sentence alone); the Test card's line under a `gpu_setup` failure
  while "Not now" holds, with none of its own, and none under another failure; the Image lab's
  line under a run that failed so.

**Only a live check on a real machine can show:**
- the polkit prompt on Plasma (polkit-kde-agent): lmgw's message and icon, dismiss (126), a wrong
  password (127), an account outside `wheel` getting the administrator choice, and pkexec with no
  agent running;
- the system task: quitting lmgw and logging out during a run leave `lmgw-machine-setup` running to
  its end, the shutdown inhibitor shows in Plasma's shutdown, and a restarted lmgw reports "already
  running";
- the full install path: a fresh Fedora 44 KDE install with an NVIDIA card and the driver from
  Discover, then check, set up, check again, and a first model start with the download progress.
  The test bed is a fresh Fedora on a spare machine with an NVIDIA card, or a VM with the card
  passed through. A second user account on a development machine does **not** test this, because the
  system is shared and the toolkit is already installed;
- what a second user account on a development machine *does* test: first launch on a fresh data dir, the
  surfaces of §2, lmgw's own spec in that account's runtime dir, a rootless Podman with an empty
  image store (so §5's progress on a real network), and `ready` on an already set-up system;
- the regeneration on a development desktop at the next `egl-wayland` or driver update, or staged by editing
  a path in lmgw's own spec (a file of lmgw's, on tmpfs, so no root and nothing of the system's is
  touched), then starting a GPU model;
- a boot on which no CUDA program has run yet, to see that the spec lmgw generates then has the
  `nvidia-uvm` nodes;
- Secure Boot with an unenrolled module, the AppImage on a non-Fedora distribution, and an
  ostree system: real machines only;
- the component in the app's window (WP8a): "Set it up" from Settings → GPU through the shell's
  dialog, the password and the steps' live lines in WebKitGTK at 125 %, the run's sticky head in
  WebKitGTK's column layout, and the tray's item moving the page to `#machine` without a reload;
- WP8b's surfaces in the app's window: a real start that fails with `gpu_setup` in the Chat (the
  toolkit removed on the test machine, or a driver update before its restart), its card and the
  banner coming back in WebKitGTK at 125 %, "Set it up" from the card through the shell's dialog,
  and "Not now" kept across a restart of the app; since the review round also a request from
  Kai or an OpenAI-only app whose start fails so while the window stays open on the Overview,
  the card coming back there through the live feed without a reload, and the Test card's line
  after a real failed test.

## 7. Work packages

In build order. Each is a self-contained package, and each ends with the project's checks.
Ruling 4 dropped S3, S5 and S6 with their tests, the refresh-unit facts and the helper's spec
re-read. WP3 exists only because of it, and it shrank WP2 and WP6.

- **WP0 — Today's root paths (independent; first; built).** F1:
  `FF_DISABLE_UMASK_FOR_DOCKER_EXECUTOR` in `.gitlab-ci.yml`; the full guard of §3.8 in
  `ci/build.sh`, also on the GitHub release workflow; the rpm-build sample test. The updater:
  `/usr/bin/pkexec` by absolute path in `lmgw-update/src/install.rs:73`, and `can_self_install`
  checking that path instead of `PATH`. Depends on nothing. Nothing else may ship a root-run file
  before it lands. The updater's wider root path (an unsigned RPM in `/tmp` installed as root) is a separate item.
- **WP1 — `lmgw-machine`: facts and CDI (built).** The `Facts` trait with real and fake
  implementations: PCI, `/proc`, the `libnvidia-ml.so.1` link, efivars, rpm (`-q`, `-qf`, header
  signature key IDs), `rpmkeys --list`, the repository files with dnf5's overrides,
  `/etc/os-release`, the dnf cache times. CDI spec parsing, the judgement of lmgw's own spec, and
  the system dirs' resolution (§1.2). It adds a maintained YAML parser (`serde_yaml` is
  unmaintained; the builder picks a maintained fork for review). Tests of §6's spec, system CDI
  and driver groups. Depends on nothing.
- **WP2 — The plan (built).** Steps S1, S1′, S2, S4, S4′ and S4″, the `NonEmpty` package list, `<scope>`,
  the provider table, the embedded repository file and key, `HashedPlan` with its schema, and the
  instructions for every system of §3.7. Tests of the toolkit, plan, hash and key groups. Needs WP1.
- **WP3 — lmgw's own spec (ruling 4; built).** `runtime::cdi`: generate (rootless `nvidia-ctk`,
  `nvidia-modprobe` for the uvm nodes, within the load timeout), the stamp, `ensure_fresh` before
  GPU starts, the per-instance private dir, Podman's default dirs without the toolkit or the option
  (D26, which includes the `podman --help` check); `podman_globals` and its use in `argv.rs`,
  `acquire.rs`, `podman_ops.rs`, `verify.rs`, `bench/launcher.rs` and the isolated MCP servers'
  `mcp/container.rs` and `mcp/connect.rs`; the restart policy left out (D27); the guard test, the
  run-time refusal and the source scan; the README change. Needs WP1. It fixes the `egl-wayland`
  break on every machine that already has the toolkit (a development machine included), so it can
  land before anything with a password.
- **WP4 — The check in core and its API (built; the "Try the graphics card" probe of §1.7 is not
  built yet, and the Settings search rows of §2.3 came with WP8a's card).** The `lmgw-core`
  `machine` module (the version
  comparison, the NVML subprocess probe and lmgw's probe mode, amdgpu, `podman info`, the reason
  `OwnSpec::default_dirs` keeps (WP3 asks `podman --help` for `--cdi-spec-dir`), statvfs,
  settings fit, the engine images with the download's size (D62), the frozen-section reading, the
  advisory `repoquery`), the `lmgw-api-types` DTOs, the `machine_check` op with `OpDoc` and op
  name, `lmgw__machine_check`, the status summary and the boot run. Integration tests. Needs WP2
  and WP3. Its review round (2026-11-03) built D54 to D62 and the root constructor of D60; WP6 is
  the first caller of that constructor. Live after the round (a dev instance, 2026-11-03): the boot run checked, wrote the instance's own spec and checked again, publishing
  once (`ready`); a fresh check took 0.85 s, `ready`, the download line "done" with all three engine
  images here. With the image class pointed at `ghcr.io/ggml-org/llama.cpp:server-vulkan`, which
  is not here, a fresh check took 2.3 s and said "Before the first model runs, lmgw downloads what
  your graphics card needs to run it (308.4 MB)": 7 blobs, none in the store, the same
  308,364,906 bytes a separate read of the registry's amd64 manifest against `layers.json` gave.
- **WP5 — `gpu_setup`, its fallback and the re-check (built 2026-11-03).** The re-check after a
  failed GPU start on every start path (`vram/start_failed.rs`, `machine/setup.rs`, D63, D64,
  D74), `GatewayError::GpuSetup` (503, outside `REFUSAL_KINDS`, D65), `FallbackReason::GpuSetup`,
  the hand-on at admission (`gate/machine_setup.rs`: `admit_route`, a plain model's guest, a
  candidate alias's walks, D70, D71) and the resolve-time swap in `stop_at` (only while the
  machine is as the check read it, trying the hop when its fallback cannot answer, D66, D67), a
  fresh check after a start that worked against the verdict (D68), the request log's reason,
  the `x-lmgw-fallback-reason` enum and description, the error-code list and `/v1/models`'
  notes, and the Traffic page's labels. D36's question: a failed engine download hands on too,
  with its own reason and code, `engine_download` (D69). Integration tests in
  `vram_admission::gpu_setup` (§6). Needs WP4. Its review round the same day: §1.1 scoped per row
  (D64), the words (D65, D69), the swap only for causes `Current` reads and "Check again" writing
  the spec again (D95), the blame from the last check or from §1.1 and §1.2 alone with the full
  check in the background (`machine/blame.rs`, D96), pinned callers never swapped and quickdoc's
  refusal worded from the fallback's reason (`gate::resolve_pinned`, D97), the Background warm's
  `fallback` (D98), the operator's start of a model already up checking nothing (D68), and the
  failed start's read boxed where admission and a dead container's restart await it; tests in
  `vram_admission::gpu_setup_rows` and `machine_check::blame` (§6).
- **WP6 — The helper and its packaging (built).** The `lmgw-machine-helper` binary (§3.1:
  `plan`, `apply`, `run`, the transient unit, the inhibitor, the journal lines, write errors ignored), the polkit
  policy, the `files` entries, `polkit` in `depends`, and building the helper in `ci/build.sh`.
  Helper tests. Needs WP0 and WP2. The helper's and the policy's paths get a content sha256 in
  `ci/rpm-allowlist.toml` when they are added (the guard has no content hash yet), because its mode,
  owner and path checks pass a policy file whose content says `<allow_any>yes</allow_any>`; `polkit`
  also joins the allowlist's pinned `requires`. When a scriptlet is ever allowed, its `*FLAGS` tags
  join the hash. Built 2026-11-03 (D75 to D86): the crate `crates/lmgw-machine-helper` (the
  grammar, who may run each verb, root's two plans through `SystemFacts::for_root`, its first
  caller, `apply` with the unit around the inhibitor around `run`, `run`'s steps with their file
  writes, the journal, the real host behind a `Host` seam), the wire types in
  `lmgw_machine::helper` (events, exit codes, the unit's command) and `StepKind`'s serde names; the
  policy in `crates/lmgw-machine-helper/polkit/`; `tauri.conf.json`'s two `files` entries and
  `polkit` in `depends`; `ci/build.sh` cleaning and building the helper before the bundle; the
  guard's `[pinned]` files (the policy by sha256, the helper by the file this job built, D83) with
  `polkit` in `requires`, and `ci/changed.py` running the guard check for a policy change. Tests:
  `crates/lmgw-machine-helper/tests/it` (grammar, apply, run, policy, and the real host's
  environment, descriptors, output and file writes with harmless programs in a temporary
  directory), `crates/lmgw-machine/tests/it/helper.rs` (the wire) and `ci/rpm_guard_test.py`
  (the pins). Bundled locally with tauri-cli 2.11.3 around a stand-in app binary (`cargo tauri
  bundle --bundles rpm`, no release build of lmgw): the RPM had the helper `-rwxr-xr-x
  root:root` with the sha256 of `target/release/lmgw-machine-helper`, the policy `-rw-r--r--`
  with its pinned sha256, `polkit` in its Requires, digest algorithm 8, and the guard passed it (13
  entries, 2 pinned files). Nothing was installed or run as root: the helper, pkexec, systemd-run
  and dnf ran only as fakes. Its security review round (2026-11-03, no blocker, no high; D87 to
  D89, D100 to D103, and D75, D76, D77, D82, D83 and D85 revised): the check reads the plan's
  inputs through root's environment (`Facts::root_env`, `FakeFacts::program_in_users_env`), the
  helper's stdout is a queue no reader can stall, the real host ends a run with its program, the
  helper changes to `/` first and root's reads run there, `apply` passes on `run`'s exit only with
  `run`'s matching line, a refused caller journals nothing and `apply`'s `begun` names
  `PKEXEC_UID`, the policy's message names the change and its action is `apply`'s alone
  (`exec.argv1`), the unit has `PrivateTmp=yes`, and the guard compares pinned files in sha256
  only. The user manager showed a killed `systemd-run --wait --pipe` leaving its unit running
  (D22, "Unverified").
- **WP7 — The shell command (built 2026-11-03).** `src-tauri/src/machine.rs`: the gates, the
  native confirmation, the `/usr/bin/pkexec` spawn, the `Channel` events, the exit codes, following
  an already running unit, and afterwards the re-check, the spec generation and the probe
  re-install; the capability and `build.rs`. Tests with a fake spawner and a fake dialog. Needs WP4
  and WP6. What WP6's review hands it: a `line` event's text is a program's output and is rendered
  as text, never as HTML; the journal is read only through entries whose trusted fields say root
  inside the unit (`_SYSTEMD_UNIT=lmgw-machine-setup.service`, `_UID=0`), never by
  `SYSLOG_IDENTIFIER` or `LMGW_EVENT` alone (D82); and on exit 2 with the app's own schema and
  version, the shell checks whether the hash it recomputes is among `plan_changed.plans[*].sha256`;
  if it is not, it stops offering the button, says lmgw and the system read this computer
  differently, shows both plans in the detail and never offers the same plan again (D88, §3.5).
  Built (D120 to D132): `machine_apply` in `src-tauri/src/machine.rs` over a `World` seam, with
  `machine/gates.rs` (§3.4), `confirm.rs` (the GTK dialog with its fold, D121), `spawn.rs`
  (`pkexec --disable-internal-agent`, nothing of lmgw's process, D120, D132), `progress.rs` (the
  helper's lines as events), `outcome.rs` (§3.5's table, D126, and D88's two plans in full),
  `reason.rs` (a failed step's plain reason, D128) and `follow.rs` (systemd and the journal by
  trusted fields and the invocation, D122); the command in `build.rs`, `allow-machine-apply` in
  `capabilities/remote-ui.json`, `lmgw-machine` and `lmgw-api-types` in src-tauri's dependencies.
  The types (`MachineApplyEvent`, `MachineLineStream`, `MachineApplyOutcome`,
  `MachineApplyResult`, D131) in `lmgw-api-types`. Core: `machine::after_fix` (`machine/fixed.rs`,
  D127) and `MachineChecks::read_differently` with `lmgw_machine`'s `UserSide::read_differently`
  and `READ_DIFFERENTLY` (D124), and `plan::instructions::command_line` for the dialog's and the
  detail's command lines. Tests as §6 lists them. Nothing ran as root and no dialog was shown: the
  real spawn refuses in a debug build, and no release build ran. **The contract for WP8a** is in
  §4.3 (how to call it, the channel, what each event and outcome shows). Security review round
  (2026-11-03, no blocker, no high; D133, D134, D140 to D144): the dialog shows only over the
  window in front and its "Set it up" waits a second with the focus, a "Not now" holds for its
  document (`media::document`), the fold shows modes, the key's fingerprint and what NVIDIA's
  package's own scripts change (new DTO fields, the hash unchanged), pkexec's own 127 words about
  the program are no refused password, an exit 2 asks whether lmgw was replaced first, a download
  that does not verify is told from a key lmgw lacks, the spawn takes lmgw's own hash, and the
  contract says the channel's events reach whatever document the window runs.
- **WP8a — UI: the component and the fix (built 2026-11-03, D145 to D159).** The `MachineCheck`
  component, Settings → GPU "This computer" with its own search rows, the plan block with its fold,
  the progress view, the browser wording, and ui-matrix shots of each verdict on a dev copy with a
  fake check. The read-only parts need WP4; the button needs WP7. Built: `widgets::MachineCheck`
  in `crates/lmgw-ui/src/widgets/machine_check.rs` with `bridge` (the in-app test, the channel
  made by hand and `bridge::Ordered`, `machine_apply`'s call), `items` (the rows, the marks, the
  names of items not checked yet), `plan` (the block, the commands fold mirroring
  `confirm::of`, "Copy the commands"), `progress` (`Progress` and its view, the outcomes' tones)
  and `fit` (the settings switch's confirm, its `outdated` and written answers), the look in
  `assets/machine.css`; Settings → GPU's card and search rows (`Ctl::Machine`,
  `Ctl::MachineLink`, `Group::wide`, `in_search`, `in_category`, the `#machine` anchor). Unit
  tests of the channel's order and frames, the progress, the tones, the fold's lines, the copied
  text, the switch's values and the search rows. Shots (a 1536x864 viewport at 125 % scale, not committed) of ready, needs_fix with its plan and its commands, needs_fix in a browser,
  needs_steps, needs_restart, cloud_only, the switch's offer, its `outdated` answer and its write,
  and a run held at the password, held mid-install with output, applied and step_failed, plus a
  search and its landing, through `scripts/ui-drive.py --init-script scripts/drive/machine-fake.js
  scripts/drive/machine-check.json` on a fresh dev instance (prefix `lmgw-probe`): 0 FAIL;
  `ui-matrix.py` over `/settings/gpu#machine` with the same fake in the app window's layout at its
  seven sizes: 0 FAIL. The shell's real channel and dialog did not run: the shots fake the bridge
  in headless Chrome, and no release build ran. *Review round (2026-11-03, no blocker; D160 to
  D169):* the check and the run moved into the app's `MachineState`, the tray's item moves the
  page through `lmgw-navigate` instead of reloading it, the Settings page reads the settings again
  after the switch (`FormState::refetched`), the progress view renders from memos with append-only
  output that follows its end under a sticky head, the browser's wording, the switch's confirm
  with core's sentence first and its fields folded, nothing to switch as a plain note, one search
  link, the landing once the card has its content, the cloud-only item's mark, the copy refusal,
  and the small fixes (L2 to L4, L6 to L9, L13, L14). The drive grew to 30 shots (17 to 30:
  needs_driver, `no_agent` with the commands open, `plan_changed`, `declined`, `unavailable`, a
  followed setup without its journal and its end, the command's own `Err`, nothing to switch,
  the output following its end, a run kept after leaving the page and brought back by the tray's
  script, the tray landing from a pane scrolled down, the switch writing under a Settings edit,
  a copy refused), 0 FAIL; `ui-matrix.py` at the seven sizes: 0 FAIL.
- **WP8b — UI: the other surfaces (built 2026-11-03, D170 to D181).** The Overview card (or the
  checklist's first block if part B exists), Chat's empty state and banner, and the `gpu_setup`
  error card. Needs WP8a and WP5. Built: `widgets/machine_check/surfaces.rs` (`MachineBanner`,
  `MachineCard`, the Chat's slot `chat_slot`; `MachineInCard` since the review round) and
  `nudge.rs` (the one rule, the check's state, the stored "Not now", `Nudge` provided with the
  `MachineState`); the Overview's card in `pages/overview.rs`; the Chat's seam and its side in
  `pages/chat_machine.rs` (`ChatMachine`, `BannerSlot`, `EmptySlot`, `GpuSetupCard`, `Parts`)
  with five small edits in the Chat's own files (`Msg.gpu_setup`, the card in `MsgView`, the
  banner under the head, the card in the empty state with its airy density, the error frame's
  branch in `chat_turn.rs`); the banner's line under the Test card and the Image and Audio labs'
  runs (D179); the fallback reasons in words (`traffic::fallback_words`, D180); the look in
  `assets/machine.css`. Unit tests as §6 lists. (The first pass named a `chat_host_hands_slot`
  here, which never existed: the slot is `chat_slot`, handed in `App`. It also had the Chat
  decide involvement through `local_models_involved`, `involved_now` and the kit catalog's
  `fetched()`, which the review round replaced, D184.) Shots (a 1536x864 viewport at 125 % scale, not committed) through
  `scripts/ui-drive.py --init-script scripts/drive/machine-fake.js scripts/drive/machine-check.json`
  on a fresh dev instance (prefix `lmgw-probe`), the drive grown to 45 shots (31 to 46 new: the
  Overview's card with its plan, a cloud-only setup without it, needs_restart with "Not now" in
  the head, the card in a browser and after its "Not now", the card above Chat's empty state on a
  first launch and scrolled to its end, the banner and after its "Not now", a `gpu_setup` turn in
  the app window and in a browser, the banner back after the state changed, its link's landing,
  a cloud-only Chat, a host without the slot): 0 FAIL in 161 checks; `ui-matrix.py --tauri --dsf
  1.25` with the same fake over `/` (the card), `/chat` (the empty state's card), `/chat?t=<id>`
  (the banner) and `/settings/gpu#machine` at the seven sizes: 0 FAIL in 28 measurements.
  After the rebase onto `main` with chat-plane WP3a: `ci/check.sh` OK (9127 tests), the drive 0
  FAIL, `scripts/chat-drives.sh` 14 of 17 (the three that fail are WP3a's own: chat-folders' Move
  to…, chat-ongoing's folder 1, chat-live's device key); one run of the drive under the load of
  other builds missed WP8a's output-following check once, which passed three runs of its scene
  alone and the next whole run. The real Tauri window, a real `gpu_setup` from a real start and
  the Test card's and labs' line on screen did not run (Unverified).
  Review round (2026-11-03, D182 to D192, an independent review: one high, five medium, eight low):
  core stamps a failed start the check blames (`failed_start`, D182) and sends the summary on
  the live feed (D183); the Test op's failure carries its code (D189, `modelinfo/test_failed.rs`);
  the dashboard decides involvement from the admin plane's rows (`involved.rs`, D184), the
  banner's words fit its link and the computer (D185), compact surfaces fold what is no problem
  and put "Set it up" first (D186), the card that holds the fix says only what failed while the
  banner steps aside (D187), the line under a failed test or lab run is `MachineInCard` (D188),
  "Not now" is in the cards' head and hidden while a setup runs (D190), and the Chat's rules and
  box moved to its side of the seam (D191). Unit tests and core's `gpu_setup_stamp` as §6 lists.
  Shots retaken (31, 32, 34 to 46) and new (47 a check changing under the open Overview, 48 a
  failed start from outside the Chat ending "Not now", 49 an alias-only setup's banner, 50 the
  Test card's line, 51 the Image lab's line; 43 is the restart's wording, "See why"): the whole
  drive 0 FAIL in 204 checks on a fresh dev instance (prefix `lmgw-probe`); `ui-matrix.py
  --tauri --dsf 1.25` over the same four routes, 0 FAIL in 28 measurements;
  `scripts/chat-drives.sh` 14 of 17, the same three failing on their own as before;
  `ci/check.sh` OK (9140 tests).
- **WP9 — The tray.** It watches the summary: the menu item and status line when the verdict leaves
  `ready`, and opening the window at `#machine`. Needs WP4. Built 2026-11-03 (D105 to D109):
  `machine_tray::Watch` (pure, unit-tested: absent, ready, ready to each `needs_*`, back to
  ready, never ready, a new sentence while not ready), the item inserted and removed on the
  status tick, the sentence in the status line and in `dev_guard::tray_tooltip`, and
  `show_main_window_at`. The tray itself was not run (the tray belongs to the running installed instance); the decision and the tooltip are tested.
- **WP10 — The download, core (built).** The pull before admission (`vram/download.rs`, called from
  `vram/queue.rs` and a guest's start in `vram/background.rs`), `WaitPhase::Downloading`,
  `pull::presence`/`begin`/`pull_and_wait` (`backends/pull/ensure.rs`), the per-instance private
  service and the compat-mode stream parser, the CLI fallback, the manifest layer sizes in `oci.rs`,
  job progress in bytes, and the benchmark's download before its lease. Tests of §6's download
  group. Depends on nothing above; it can run beside WP1.
- **WP11 — The download, UI, D16 and the pre-fetch.** The Chat waiting row, the GPU popover, the
  Overview and the Engines job row; the model download fetching its engine, as its own line; the
  pre-fetch once the card is ready. Needs WP10, and WP4 for the pre-fetch's verdict. Built
  2026-11-03 (D37 to D41: `gpu_download.rs` in the dashboard, `web/hf/engine.rs` in core, the
  chat turn's `downloading` frames in `web/chat_voice/turn_state.rs`), with its review round the
  same day (D42, D43); the pre-fetch was built after WP4 the same day (`machine/prefetch.rs`,
  begun from the check's publish when the verdict turns `ready`, D72, D73; tests in
  `vram_admission::engine_prefetch`), and in WP5's review round brought back to D16's scope (D72)
  and kept off a metered connection (D99).
- **WP12 — AMD settings fit (built 2026-11-03, review round the same day).** The
  `machine_settings_fit` op (`machine/settings_fit.rs`) with its `confirm` step (D114, D135), the
  plan it and §1.4 share (`machine/fit/switch.rs`: classes D112, rows D113, the flags D138) from
  the values in `machine/fit/shipped.rs` (D111) and what `machine/fit/present.rs` reads of this
  computer (the cards D137, the images D139, the running models), the rows written in the
  Settings save's transaction under the settings lock (`store/run_overrides.rs`,
  `api_settings::settings_set_full_planned`, D115, D135), the stops after it (D136), the refusal
  (D116); §1.4's sentences and detail with the switch as the item's typed `action`, `needs_fix`
  only when the switch makes the settings fit (D110), a CUDA image on an AMD card (D118), and the
  item judged again on every published snapshot (`MachineChecks::refit`); §1.1 ready on an AMD
  card without `/dev/kfd` (D137); the DTOs in `lmgw-api-types/src/machine.rs` with typed values
  (D119), the Backend label, the op name, the dispatch and the `OpDoc`; `lmgw__machine_check`'s
  description; `docs/amd-arch.md` §2 and §4 with the values lmgw writes (`label=disable` kept, the
  image class added); the registry's `ready_for_tests` for the stop tests. Tests in
  `machine_check::settings_fit` and its `confirm` and `cards` (§6). The dashboard's confirm is
  WP8a's; no self-admin tool yet (D117). Needs WP4.
- **WP13 — Stored content can't run script at the gateway's origin (built; independent).** Every
  answer of the gateway's own origin sends `X-Content-Type-Options: nosniff` and
  `Content-Security-Policy: sandbox` (the way the audio-lab refs did since f1d9d72b), except the
  dashboard's own. Why every answer and not a list of stored-content routes: a navigation from a
  typed address or a link on the dashboard carries the owner's session cookie (`SameSite=Strict`
  keeps only a foreign site's out), which resolves to the owner, who holds `Inference`, `Chat` and
  `Admin`; with **Require API key** off, Anonymous holds `Inference` too. So whatever any route
  relays or serves can be opened in a window at `http://127.0.0.1:8001/…` with the owner's rights,
  and a list misses the routes nobody thought of as stored content (the review found two: the agent
  MCP face and the native-response forward below).
  - **The router-wide default** (D90): `server::build_router_for` puts a `map_response` layer
    (`web::stored::by_default`) on the routes right outside `principal_mw`, so the gate's refusals
    carry both headers too, and inside CORS, the faces and the agent origins' `Host` dispatch. It
    inserts `nosniff` and appends `sandbox` as a policy of its own, so a policy the answer already
    carries (an agent container's, an upstream's) stays and both are enforced; a bare `sandbox`
    already there is not repeated, a `sandbox` with flags does not count (D92). Bodies, statuses and
    every other header are untouched; API clients (fetch, XHR, EventSource, WebSocket, the SDKs)
    ignore both headers, so the OpenAI and Anthropic wire is unchanged.
  - **The exemption** is exactly what `web::ui::routes` answers: `GET /`, `GET /{*path}` (the
    Trunk-built assets and the SPA's client-side routes), `GET /ui`, `/ui/` and `/ui/{*path}` (the
    old mount's redirect). That router marks every answer it gives `DashboardOwned`, its own 404,
    503 and 405 included, and those keep the dashboard's own policy (`DASHBOARD_CSP`) and no
    `nosniff` (D93). An agent's origin (`<id>.localhost`) is answered by `host_dispatch` above the
    layer and stays the container's. Checked before landing: a `302` that carries
    `sandbox` and `Set-Cookie` (the login link the Tauri window opens) still sets the cookie and
    lands on a page that runs script at its real origin, in WebKitGTK 2.54 and in Chrome; a page
    with a container's policy plus `sandbox` runs none in either.
  - **The stored-content routes keep their own call** (`web::stored::sandboxed`, and `web::ranged`
    on every answer it builds: 200, 206, 416), so a stored file is covered where it is served and
    not only by the router's position (D94). The audit, route by route (before, then after):
    - `GET /chat/api/attachments/{id}` (an upload or a media tool's file; the row's type, text as
      `text/plain`): headers on 200/206/416 by hand; now through `ranged`.
    - `GET /chat/api/attachments/{id}/text` (extracted text, `text/plain`): by hand; now
      `sandboxed`.
    - `GET /labs/api/artifacts/{id}` (image, audio and decision lab files; the row's type): by hand;
      now through `ranged`.
    - `GET /audio-lab/api/refs/{name}` (the voice library's clips; type from the extension): its own
      helper on the 200 and 416; now through `ranged`, that helper is gone.
    - `GET /voice-lab/api/rounds/{id}/candidates/{n}` (candidate audio; the stored type): **fixed**,
      it sent neither header.
    - `GET /api/knowledge/files/{id}/original` (the uploaded bytes under the upload's own type, so
      an HTML upload was `text/html`): **fixed**; `attachment` stays.
    - `GET /transfers/api/transfers/{id}/content` (a peer's file, `application/octet-stream`):
      **fixed** on 200, 206 and the 416 refusal; `attachment` stays.
    - `GET /chat/api/threads/{id}/export`, `GET /chat/api/folders/{id}/export`,
      `GET /chat/api/export` (Markdown, JSON, zip of threads): **fixed**.
    - `GET /labs/api/{lab}/export` (the lab's runs and files, the exporter's type): **fixed**.
    - `GET /api/docs/export` (the documentation corpora's text, a SQLite file): **fixed**.
  - **Relays, covered by the default only:**
    - `ANY /agents/{id}/mcp` and `/agents/{id}/mcp/{*rest}`, the agent MCP face: it passes the
      container's `Content-Type`, policy and headers through at the dashboard's origin, so a
      container answering `text/html` ran script there for anyone opening the path with the owner's
      cookie. It now gets `nosniff` and the second policy, and its `Set-Cookie` is dropped (D91);
      the agent origin keeps passing it.
    - `GET /v1/responses/{id}` and `GET /v1/responses/{id}/input_items` for a response an upstream
      made natively (`responses/retrieve.rs`): the upstream's `Content-Type` and body as sent.
    - The `/v1` audio and image answers (`proxy/audio.rs`, the upstream's `Content-Type` and
      `Content-Disposition`) and `/v1/systemone` and `/tokenize` (`proxy/relay.rs`).
  - Tests: `tests/it/sandbox_walk.rs` walks every `CAPABILITY_TABLE` row but the five dashboard
    rows with the owner's bearer and asserts `nosniff` and exactly one `sandbox` whatever the
    status; it asserts `/` and two client-side routes keep the dashboard's policy and no
    `nosniff`, a Trunk asset and the `/ui` redirect carry no sandbox, and an agent origin's answer
    none. Beside it: both headers on 200, 206 and 416 per
    stored route class, `HEAD` on a Chat attachment, the folder and all-threads zips, a native
    response relayed as `text/html`, and the MCP face's `text/html` answer with the container's
    policy and cookie (both policies, no `Set-Cookie`; on the agent origin, the container's alone).
- **Live round.** §6's live list: a development desktop first (the second account, the
  staged regeneration), then a fresh Fedora with an NVIDIA card.

## Not yet

- The tray: a desktop notification when the verdict leaves `ready`; a watch channel on
  `MachineChecks` in place of the 5 s tick (D108); the item for a computer that was not `ready`
  when lmgw started (D105); a window the item has to build opening at the route after login
  (D109).
- §1.3 and §1.4 as a failed start's reason (a Podman that is broken, settings for another
  vendor's card): only §1.1 and §1.2 name one yet (D64); such a start answers its own error.
- A resolve-time swap for a candidate alias whose primary the last check says cannot start: the
  alias picks a loaded candidate first, and its primary's failed start hands on at admission
  (D70).
- A resolve-time swap for causes `Current` does not read (no card at all, an AMD card's compute
  node, a folder of lmgw's that is not private, lmgw's own files, a write that ran out of time):
  such starts are tried each time, blamed from the last check and handed on (D95).
- lmgw's spec written again by the background checks after a start: only a fresh check asked
  for writes it first (D95).
- A failed start's read with the download sizes and another repository's toolkit (D96): the full
  check after it reads them.
- A Background warm saying `fallback` for an engine download that failed (D98 covers
  `gpu_setup`): it says `failed`, and its turns hand on at admission.
- A switch that keeps the pre-fetch from fetching, beside D16's model-download switch.
- A row's own engine image (an override named on the row) in the pre-fetch (D72): its first
  start downloads it.
- The metered check on an IPv6-only default route (D99): only `/proc/net/route` is read, so
  without an IPv4 default route the connection counts as not metered, and the log says so.
- §1.6's line saying when the download happens (now, once the card is ready).
- A feature of `lmgw-machine` that leaves its CDI readers (and their YAML and TOML parsers) out of
  the root helper (D84).
- Clearing a unit of the setup's name that someone else left loaded in a failed state
  (`systemctl reset-failed`): systemd-run then refuses the name, and `apply` answers exit 6 with
  systemd's words.
- `ProtectHome=read-only` on the unit (D103): it waits for the live round to show that no step
  writes under `/home` or `/root`.
- Keeping a plan the helper read differently across lmgw restarts (D124): a restart offers it
  once more.
- A way for the page to learn that a setup runs before it calls `machine_apply` (a page opened
  during a setup shows the button, and the click follows the setup, D122): the check and the
  status say nothing of the unit yet.
- "Try the graphics card" (§1.7) in the component: core has no probe for it yet, so WP8a's
  component offers none.
- A bounded, self-following box for the live output (D161): it would be a second scroller on the
  page, which needs the owner's ruling.
- Part B's welcome checklist and part J's "Which model would you like to chat with?" (§2.1): the
  Overview has a card at the top of its state column and Chat's empty state a card over its own
  line (D172, D173); choosing a local model to download there has no block of its own yet.
- The banner on the download pages (§2.2's "downloading one"): a person with no local model yet
  who downloads one meets it in the Chat (none configured counts, D170), not on the catalog or
  Downloads pages.
- The voice surfaces' own fix: voice mode's panel, dictation and read-aloud say core's
  `gpu_setup` sentence as their note, with no component under it (their failed start ends "Not
  now" through core's stamp since WP8b's review round, D182).
- `gpu_setup`'s code on a ladder's rungs (`RungTest` has no code yet, D189): the Test card shows
  no line under a ladder rung that failed so.
- The tray's item after a failed start (D105): it still waits for a `ready` seen since lmgw
  started, so a computer never `ready` and a start that failed because of it put nothing in the
  tray; `MachineSummary.failed_start` (D182) is there for it.
- A resolve-time `gpu_setup` refusal that starts nothing (a hop with no route, `gate/chain.rs`)
  is not stamped on the check yet (D182).
- The fix in the Decision lab's cells, which say a failed run's status, code and message on one
  line; and the reason a fallback answered in the Chat's "(fallback for …)", whose `done` frame
  carries none (D180).
- A `gpu_setup` card after a reload: the failed reply is not stored, so the card goes with the
  page, and the banner (whose "Not now" core's stamp ended, D182) is what stays (D192).
- The confirmation judged by the document whose call it is (D140): the running document when the
  dialog would show is the one asked and the one whose "Not now" is kept; the invoke handler does
  not hand the command the calling document's number yet.
- A channel bound to the document that called (§4.3, the review's L6): Tauri delivers the events
  to whatever document the window runs, and the page ignores those it did not ask for.
- A package-scripts note for S4 through rpm-ostree (D144): how rpm-ostree runs the package's
  scripts when it puts the new image together was not read.
- A content pin for a scriptlet's `*FLAGS` tags, should the RPM ever be allowed one (§3.8).
- Comparing the free space with a model about to be fetched (§1.5): only the engine download is
  compared yet.
- A layer list read from Podman's additional image stores (`additionalimagestores`) for the
  download's size (D62): only the main store is read yet.
- A voice warm (the Chat's voice warm-up, a bound realtime session) saying `downloading`: it says
  `loading` through the download (D40 covers a chat turn's own admission).
- A switch that keeps a model download from fetching its class's image (D16 makes it the
  default; there is no setting yet).
- A model row's own image (an override named on the row) fetched when the row is created or
  saved: a model download fetches the class's image (D37), and a row's own is fetched at its
  first start.
- A failed or refused D16 pull said anywhere but the download wizard once it has left the jobs
  feed (D41): the first start tries again and answers WP10's 502.
- Installing the NVIDIA driver itself, akmod builds, and Secure Boot key enrolment (D8).
- A helper step for missing driver libraries (`-cuda-libs`, §1.1).
- A stop button for a running setup (§3.5).
- Removing an old NVIDIA key after a key change (§3.2).
- A desktop notification and a tray icon state for "needs attention" (§2.4).
- A timer that re-checks on its own (§2.6).
- Distribution-specific package commands for apt, pacman and zypper in the plan (§3.7).
- A helper step for missing subordinate ID ranges (§1.3).
- A self-admin tool for the settings switch (D117): it is the `machine_settings_fit` op alone.
- A switch to AMD's ROCm settings (D111): only Vulkan is written; ROCm stays a hand setting.
- Replacing an image of the owner's that is built for the other card (D113, D139): the switch
  names it, and picking another is the model's page's; and telling an image lmgw does not know by
  its name and that is not on this machine yet (it stays unknown until it is downloaded).
- Combining a group of the owner's with AMD's `--group-add keep-groups` (D138): such arguments
  are left and named.
- Stopping a model that was answering a request once it is idle (D136): it is named, and the
  check says so until it stops.
- A relabel in place of `--security-opt label=disable` for AMD's Vulkan settings (D111): a
  shared `:z` relabel of lmgw's own models dir (never `:Z`, never `$HOME`, a checkout or an NFS
  mount) could let the AMD Vulkan settings drop `label=disable` through container-selinux's
  `container_use_dri_devices`; NVIDIA's would still need `container_use_devices`. It needs an
  AMD box and `ausearch -m avc` to see what is denied, and it is the owner's call.
- Kai's own surface for the check through `lmgw-client`, so a Kai user is told "open lmgw to set
  up your graphics card" without opening the dashboard.
- Packaging for image-based gamer systems (Bazzite, SteamOS): the proposal's "later".
- Intel GPUs: the check names them, and lmgw has no Intel path to offer yet.
- The upstream report about NVIDIA's refresh triggers (§3.6.6): filing it is the owner's decision.

## Unverified (2026-11-03)

- WP12's values on an AMD card: nothing of the switch ran on an AMD Fedora box. Not run: that a
  container needs `--security-opt label=disable` there to read `<dir>:/models:ro` with SELinux
  enforcing (D111 rests on the NVIDIA defaults' reason, not on an AVC seen on AMD); that
  `--group-add keep-groups` gives a rootless container the render node (`/dev/dri/renderD128`,
  group `render`) under Fedora's Podman, as `docs/amd-arch.md` found on Arch, and that Podman
  refuses it beside another `--group-add` (read in podman-run(1), D138); that the three Vulkan
  images (`llama.cpp:server-vulkan`, `audio.cpp:full-vulkan`, `stable-diffusion.cpp:master-vulkan`)
  find the card with `--device /dev/dri` alone, without `/dev/kfd` (D137); and that audio.cpp's
  Backend `vulkan` is the one its Vulkan image is built with. The tests ran against stated
  machines and a fake Podman.

- Measured, no longer open: `compatMode` byte progress on Podman 5.8.9 (WP10, 2026-11-03, the
  recording in `crates/lmgw-core/tests/fixtures/pull/compat-stream.jsonl`). About one
  "Downloading" line per second for each layer in flight, none for a layer that lands within a
  second, and no closing line: the answer just ends. A dropped connection stops the pull, and the
  layers that finished stay in the store.
- Whether `podman pull` prints per-layer "done" lines when not on a terminal. That would only
  matter for the fallback.
- The CDI conflict purge was read in source on `main` (container-device-interface), not run
  against 5.8.9's binary. `--cdi-spec-dir`'s replace behaviour was run in WP3's live check
  (2026-11-03, Podman 5.8.9): an empty spec dir makes `nvidia.com/gpu=all` unresolvable although
  `/var/run/cdi/nvidia.yaml` defines it (exit 126, "setting up CDI devices: unresolvable CDI
  devices nvidia.com/gpu=all"), lmgw's rootless spec alone gives the container its device nodes,
  and `/etc/cdi`, `/var/run/cdi` and lmgw's dir given together resolve without a conflict.
- Whether `nvidia-ctk cdi generate` reads a config from `$XDG_CONFIG_HOME` that changes its
  output. lmgw clears only the `NVIDIA_CTK_*` variables, not `XDG_CONFIG_HOME`.
- Whether the toolkits before 1.17 (no `--nvidia-cdi-hook-path`, no `nvidia-cdi-hook`) generate a
  spec lmgw can use when asked without the flag, and what one with the flag but without the
  binary does: lmgw asks `cdi generate --help` once and omits the flag in both cases; only the
  stated machine of the tests was run.
- Whether `nvidia-ctk cdi generate` leaves out device nodes that don't exist yet. lmgw runs
  `nvidia-modprobe -u -c=0` (and `-c=<minor>` for each card in `/proc/driver/nvidia/gpus`) first
  anyway, writes once more when the machine changed during a write, records the `/dev/nvidia*` names in the stamp so a node
  that appears later writes the spec again, and the live round looks at a fresh boot.
- How a real hung NVML looks to `nvidia-ctk` (a card off the bus). The kill at the load timeout was
  run against a stated machine and against `sh -c "sleep 30 & sleep 30"` (the whole process group
  goes), not against a hung driver.
- That killing `systemd-run --wait --pipe` (as when lmgw's scope is torn down) leaves the unit
  running to its end, on the system manager (D22). Checked on the user manager only (2026-11-03,
  systemd 259.10, `systemd-run --user`, harmless programs): with `--wait --pipe --collect
  --unit=lmgw-probe-killtest sleep 30`, SIGKILL to the client left the unit `active` (`running`)
  until `systemctl --user stop`. With a unit writing a line a second to its stdout (the client's
  pipe), a pipe reader that outlived the client kept getting every line, and the unit ended
  `success`; with the reader closed together with the client, each later write in the unit failed
  (`echo` exit 1, EPIPE) and the shell was not killed by SIGPIPE, since systemd starts a unit's
  processes with SIGPIPE ignored (`IgnoreSIGPIPE=yes`, the default), and the unit still ran to its
  end with `Result=success`. The helper ignores SIGPIPE and its write errors anyway (D100). The
  system manager, as root through pkexec, is still the live round's to confirm.
- The unit's `PrivateTmp=yes` (D103) with dnf5 and the toolkit's scriptlets: read in
  `systemd.exec(5)`, not run; the live round runs a setup with it.
- The action's `exec.argv1` (D89): `pkexec(1)` of polkit 127 says the action "will only be picked
  if the first argument to the program matches the value of the annotation", and its binary names
  the key; that `pkexec helper plan` then gets polkit's generic action was read, not run.
- The helper as root (WP6): nothing of it ran for real, by design. Read, not run: systemd-run
  passes the unit's exit code on ("the return value will be propagated from the service",
  `systemd-run(1)`, systemd 259) and systemd-inhibit its program's ("Returns the exit status of
  the executed program"), which `apply`'s mapping of 0, 2, 3 and 4 rests on; that a unit's
  processes see `0::/system.slice/lmgw-machine-setup.service` in `/proc/self/cgroup`; that
  polkitd loads the action and pkexec shows its message (the file validates against polkit's DTD); that journald takes the native-protocol entries under that identifier, with
  `_SYSTEMD_UNIT=lmgw-machine-setup.service` and `_UID=0` on `run`'s (D82's reader rests on them).
  The live round
  runs `pkexec /usr/libexec/lmgw/lmgw-machine-helper apply <hash>` on the test machine.
- WP7's shell command on a real desktop: the GTK confirmation (its look on Plasma, "Not now" as
  the default with the focus, the fold's size at 125 %), pkexec 127 with
  `--disable-internal-agent` and no agent, the events reaching a page through a real `Channel`,
  `/proc/self/exe` reading `/usr/bin/lmgw (deleted)` after an RPM update, and an account outside
  `wheel`/`adm`/`systemd-journal` getting no journal entries from `journalctl` (D122): read in
  pkexec's strings, procfs and journalctl's man pages, not run; nothing of it ran in a release
  build. The words the plain reasons of a failed step match (D128, D141: "transaction lock",
  "Curl error", "Public key is not installed.", "Problem occurred when opening the package.",
  `: BAD`, `: NOKEY`, "checksum doesn't match", …) are read in the strings of libdnf5 5.4.6,
  librepo 1.21.2 and rpm 6.0.2, not seen in a failing dnf5 run; how dnf5 joins them on its
  output was not watched, and a failure none of them names falls back to the step's own words.
  The review round's dialog (D133, D134) is unrun too: `is_visible` and `is_active` of the main
  window and the dialog's `is-active` changes on Plasma's Wayland session, the second's wait as the
  person sees it, and `media::document` moving on a reload; pkexec's "Error accessing",
  "Cannot run program" and "Error executing <program>" (D142) are polkit 127's strings, not seen
  printed. What NVIDIA's package's scripts change (D144) is read from 1.20.1's `rpm -q --scripts`; that `nvidia-ctk config --in-place` leaves an existing config.toml alone is its
  scriptlet's comment, not read in nvidia-ctk, and what Fedora's retired package's removal scripts
  do in an S4″ swap was not read.
- dnf5's `--setopt=<id>.gpgcheck=1` on a section that says `gpgcheck=0` (dnf5 reports the option as
  `pkg_gpgcheck` and keeps `gpgcheck` as its alias), and `dnf swap` together with `--repo=`. WP2
  checks both on a test machine.
- That `llama-server --list-devices` in the CUDA image lists the card as intended for "Try the
  graphics card": checked in WP3's live check with lmgw's own spec
  (`ghcr.io/ggml-org/llama.cpp:server-cuda`, "CUDA0: <card name>").
- That `nvidia-ctk cdi generate` creates no CUDA context (and so holds no VRAM), which is what lets
  it run under the GPU hold.
- That the `-shm` file is the only one in `/usr/lib/sysimage/rpm` that changes on reads, so the
  stamp's rpm change time moves with package transactions only. WP3 checked the user's side: `rpm
  -q`, `rpm -qf` and `rpmkeys --list` as the user change no mtime there (the user cannot write
  `-shm`); `-shm` was newer than the last transaction, from root's reads. A root `dnf makecache`
  was not run.
- Measured in source, no longer open: which time dnf5 moves when it loads a section (D54's reading
  rests on it). Read in libdnf5 on `main` (2026-11-03), not watched with a `dnf makecache`: the
  primary metadata file's mtime, which `Repo::get_age` reads and the in-sync reuse sets with
  `utimes`. A fresh download leaves the file's mtime at the download's time, not the server's:
  librepo's `LRO_PRESERVETIME` is set from `DownloadData::preserve_remote_time`, false by default
  (`repo/download_data.hpp`), and `updates`' primary file has the download's minute.
- That containers/storage's `<graphRoot>/<driver>-layers/layers.json` keeps a pulled layer's
  registry digest as `compressed-diff-digest` (D62's size rests on it): read on a Fedora 44 test system (1350 of 1382 layers carry one, Podman 5.8.9), not in containers/storage's source; a layer pulled
  as zstd:chunked may record another digest, and then the size counts it again.
- WP5 on a real machine: a GPU start that fails because of the computer (a driver update before
  the restart, the toolkit removed) was only run against stated machines and a fake podman; what
  the real registry error says inside "its start said: …" (lmgw's blocked spec write, Podman's
  "unresolvable CDI devices") and how long the re-check takes after it are not measured. The swap's `Current` read at resolve (file reads, done only while the last check
  names a problem) and a failed start's read of §1.1 and §1.2 (D96) were not timed either.
- The metered check (D99) against a connection NetworkManager marks metered (a phone's hotspot):
  run only against stated `nmcli` answers; on an unmetered wired connection `nmcli -t -f GENERAL.METERED device show` says "no (guessed)".
- WP11's pre-fetch on a real machine: run only against the scripted pull; a boot that turns
  `ready` with a missing image was not watched on a dev instance (its pull would write into the
  shared image store).
- WP8a in the app's window: the component was shot in headless Chrome with the shell's bridge
  faked (`scripts/drive/machine-fake.js`) and `html.tauri` set by `desktop.js`; it did not run in
  WebKitGTK with Tauri 2.11.2's real `Channel` and `invoke`, the shell's own dialog, or a real
  password. That Tauri's channel glue hands a large message to the page's hand-made callback as
  `{ message, index }` is read from `ipc/channel.rs`, not run. The settings switch's confirm ran
  only against the fake's answers, not against `machine_settings_fit` on an AMD card. From the
  review round: the tray's script ran only as the drive's copy of it, not through Tauri's `eval`;
  the run's head sticking inside the Settings columns was seen in Chrome, not WebKitGTK; the copy
  refusal was forced by removing `navigator.clipboard`, not seen over plain http from another
  computer; the Settings re-read after the switch read the fake's changed settings, not a real
  write.
- WP8b's surfaces in the app's window: shot in headless Chrome with the fake bridge and
  `html.tauri`, not in WebKitGTK; "Not now" kept in the app window's own WebKit storage across a
  restart of the app was not watched (Chrome's storage across reloads was). The `gpu_setup` card
  came from the fake's error frame, in core's words (`machine/blame.rs`, `vram/start_failed.rs`),
  not from a real start that failed; nothing of WP8b started a container. The
  host without the slot was WP3a's debug test host in Chrome, not a paired device's sidebar.
  From the review round: the live feed's `machine` frames on screen were the fake's (dispatched
  on the page's own EventSource, the dev instance's dropped), not core's after a real stamp; the
  stamp, the frame and its schema are core's tests. The Test card's line ran against the fake's
  Test answer and its row (`/api/local-model?id=901`), and the Image lab's line against a fake
  history; the Audio lab's line was not shot. Involvement ran against the fake's rows, not a
  dev instance with an alias onto a local row. A browser on another computer (not a loopback
  address), whose banner and card name the computer lmgw runs on (D185, D187), is unit-tested
  only: the dev instance listens on loopback.
- What Bazzite's and SteamOS's images ship (toolkit, Podman).
- That `golang-github-nvidia-container-toolkit` 1.17.4 can be left installed by an upgrade to
  Fedora 44. The retirement is on record (bug 2434398); the leftover case is the review's report.
- `dnf repoquery -C` as a user, partly: it can report sizes for `rpmfusion-nonfree-updates` and
  NVIDIA's repository, and has no cache for `fedora`. The plan block shows no size when it can't
  tell.
