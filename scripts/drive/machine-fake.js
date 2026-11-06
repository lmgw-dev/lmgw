// In-page fakes for the machine check's shots (scripts/drive/machine-check.json, machine-check
// design WP8a): the answers of POST /api/op/machine_check and /api/op/machine_settings_fit, and,
// for a scenario in the app's window, the shell's bridge (window.__TAURI_INTERNALS__) with a
// scripted machine_apply on a Tauri-shaped channel. desktop.js sees the bridge and adds
// html.tauri itself, so the page is laid out as the app window lays it out.
//
// Loaded with `scripts/ui-drive.py --init-script scripts/drive/machine-fake.js`, before the page's
// own scripts, in every document. The scenario is sessionStorage's "lmgw.fake.machine", a JSON
// object: {check: <name in CHECKS>, app: bool, apply: <name in RUNS>,
// fit: "outdated" | "applied" | "already",
// models: "local" | "alias" | "cloud" | "cloud_down" | "none", turn: "gpu_setup",
// test: "gpu_setup" | "other", lab: "gpu_setup"}. Without it nothing is faked. `models` answers
// the admin plane's rows (GET /api/models/full and /api/upstreams, which say whether local models
// are involved, D184) and GET /v1/models with that setup: a llama.cpp model beside a provider's,
// a local model reached only through an alias, a provider's alone, a provider whose catalog
// could not be read (an empty /v1/models), or nothing configured yet. `turn` answers a Chat
// turn's send with the error frame of a model that could not start on the graphics card,
// `test` the model editor's Test with a failed test (with gpu_setup's code, or another failure
// without it), and `lab` the Image lab's history with one run that failed with gpu_setup (WP8b's
// surfaces). Every other request goes to the dev instance as it is, except /api/settings-full
// after a written switch, which reads as the switch left the settings (the class images on
// Vulkan), so the Settings page's re-read is seen.
//
// The live feed's `machine` frames (D183) are the fake's too: the dev instance's own are dropped,
// and window.__fakeSetCheck(name) / window.__fakeFailStart(model) change the check the op
// answers and send the frame a real change would. window.__fakeAnswered counts what the fake
// answered (check, models_full, upstreams, v1_models, test, lab), so a drive that expects
// nothing on screen waits until the page has read what would have shown it.
//
// The data is made up and looks it: the home folder is /home/demo, models are *-demo, the GPU's
// UUID is zeros.
(() => {
  "use strict";
  let want;
  try {
    want = JSON.parse(sessionStorage.getItem("lmgw.fake.machine") || "null");
  } catch (_) {
    want = null;
  }
  if (!want) return;

  const T = "2026-01-01 12:00:00";
  const item = (id, state, sentence, detail = [], cause = null, action = null) =>
    ({ id, state, sentence, detail, cause, action });

  // --- the items -----------------------------------------------------------------------------
  const GPU_OK = item("graphics_card", "ok", "Graphics card · NVIDIA GeForce RTX 3090 · 24.0 GiB", [
    "PCI 0000:01:00.0 10de:2204 NVIDIA GeForce RTX 3090, bound to nvidia",
    "kernel module 600.20.03 (open) · libraries on disk 600.20.03 (libnvidia-ml.so.1 → libnvidia-ml.so.600.20.03)",
    "driver packages: xorg-x11-drv-nvidia-600.20.03-1.fc44.x86_64 (RPM Fusion)",
    "NVML in a separate process: driver 600.20.03, 1 card(s)",
    "card 0: NVIDIA GeForce RTX 3090 · GPU-00000000-0000-0000-0000-000000000000 · PCI 00000000:01:00.0",
    "Secure Boot: off",
  ]);
  const GPU_AMD = item("graphics_card", "ok", "Graphics card · AMD Radeon RX 7900 XTX · 24.0 GiB", [
    "PCI 0000:03:00.0 1002:744c AMD Radeon RX 7900 XTX, bound to amdgpu",
    "amdgpu sysfs: 24.0 GiB VRAM, 32.0 GiB GTT",
    "/dev/dri/renderD128 there · /dev/kfd there (Vulkan engines compute without it, ROCm ones need it)",
  ]);
  const CDI_OK = item("card_for_models", "ok", "Models can use the graphics card.", [
    "NVIDIA Container Toolkit: /usr/bin/nvidia-ctk owned by nvidia-container-toolkit-base-1.20.1-1.x86_64 · nvidia-ctk 1.20.1",
    "lmgw's own description, written 12:00 for driver 600.20.03 · 69 paths present",
    "your own containers use /var/run/cdi/nvidia.yaml, which lmgw's models don't read",
  ]);
  const CDI_AMD = item("card_for_models", "ok", "Models can use the graphics card.", [
    "the render node /dev/dri/renderD128 is usable by this account (group render)",
  ]);
  const CDI_MISSING = item("card_for_models", "problem",
    "One piece is missing so models can use the graphics card. lmgw can set it up.", [
      "NVIDIA Container Toolkit not installed (/usr/bin/nvidia-ctk is not there)",
      "it comes from NVIDIA's repository (nvidia.github.io), which is not on this computer yet",
      "no spec in the system's spec dirs (/etc/cdi, /var/run/cdi) defines nvidia.com/gpu=all",
    ], { kind: "toolkit_missing" });
  const CDI_STEPS = item("card_for_models", "problem",
    "One piece is missing so models can use the graphics card. Here's how to add it.", [
      "NVIDIA Container Toolkit not installed (/usr/bin/nvidia-ctk is not there)",
      "lmgw runs from an AppImage: /home/demo/Applications/lmgw-0.5.0-x86_64.AppImage",
    ], { kind: "toolkit_missing" });
  const PODMAN_OK = item("running_models", "ok", "lmgw can run models on this computer.", [
    "Podman 5.8.9, rootless, crun, cgroup v2",
    "storage /home/demo/.local/share/containers/storage",
  ]);
  const FIT_OK = item("settings_fit", "ok", "Model settings fit this card.", [
    "checked: Chat, Aux, Audio, Image",
  ]);
  const ROOM = item("room", "info", "Room for models · 466 GiB free.", [
    "/home/demo/.local/share/lmgw/models and /home/demo/.local/share/containers/storage: one file system (btrfs), 466 GiB free",
  ]);
  const DL_DONE = item("download", "info", "The download for your graphics card is done.", [
    "ghcr.io/ggml-org/llama.cpp:server-cuda: on this computer",
    "ghcr.io/lmgw-dev/audio.cpp:server-cuda: on this computer",
  ]);
  const DL_TODO = item("download", "info",
    "Before the first model runs, lmgw downloads what your graphics card needs to run it (1.6 GiB).", [
      "ghcr.io/ggml-org/llama.cpp:server-cuda: 1.6 GiB to download (registry manifest, 9 layers)",
    ]);
  const notChecked = (id, because) => item(id, "not_checked", `Not checked yet: ${because}.`);

  // --- the plan ------------------------------------------------------------------------------
  const REPO = "/etc/yum.repos.d/lmgw-nvidia-container-toolkit.repo";
  const KEY = "/etc/pki/rpm-gpg/RPM-GPG-KEY-lmgw-nvidia-container-toolkit";
  const REPO_TEXT = [
    "# Written by lmgw. lmgw replaces this file when its text or NVIDIA's key changes.",
    "[lmgw-nvidia-container-toolkit]",
    "name=NVIDIA Container Toolkit (added by lmgw)",
    "baseurl=https://nvidia.github.io/libnvidia-container/stable/rpm/$basearch",
    "includepkgs=nvidia-container-toolkit-base",
    "repo_gpgcheck=1",
    "gpgcheck=1",
    "enabled=1",
    `gpgkey=file://${KEY}`,
    "",
  ].join("\n");
  const KEY_TEXT = [
    "-----BEGIN PGP PUBLIC KEY BLOCK-----",
    "",
    "mQINBFzEXAMPLEEXAMPLEEXAMPLEEXAMPLEEXAMPLEEXAMPLEEXAMPLEEXAMPLE",
    "(a stand-in for the shots: NVIDIA's real key is 1.7 KiB of this)",
    "=EXMP",
    "-----END PGP PUBLIC KEY BLOCK-----",
    "",
  ].join("\n");
  const SCRIPTS = "NVIDIA's package then runs its own install scripts: they write " +
    "/etc/nvidia-container-runtime/config.toml when it is missing, switch on and start NVIDIA's " +
    "refresh service (nvidia-cdi-refresh), which writes NVIDIA's own description of the card to " +
    "/var/run/cdi/nvidia.yaml, and reload the system's device rules.";
  const tee = (path, text) =>
    [`sudo tee ${path} > /dev/null <<'EOF'`, ...text.trimEnd().split("\n"), "EOF"];
  const DNF = ["/usr/bin/dnf5", "install", "-y", "--repo=lmgw-nvidia-container-toolkit",
    "--setopt=lmgw-nvidia-container-toolkit.skip_if_unavailable=False",
    "--setopt=lmgw-nvidia-container-toolkit.gpgcheck=1",
    `--setopt=lmgw-nvidia-container-toolkit.gpgkey=file://${KEY}`,
    "--setopt=install_weak_deps=False", "nvidia-container-toolkit-base"];
  const S1 = {
    kind: "s1_add_repo",
    sentence: "Add NVIDIA's software source (nvidia.github.io), limited to the one piece below, so that piece gets updates with the rest of your system",
    commands: [],
    writes_file: { path: REPO, content: REPO_TEXT, mode: 0o644, replaces: false, key_fingerprint: null },
    download_bytes: null,
    terminal: tee(REPO, REPO_TEXT),
    package_scripts: null,
  };
  const S2 = {
    kind: "s2_add_key",
    sentence: "Add NVIDIA's signing key, so your computer checks that the piece below really comes from NVIDIA",
    commands: [["/usr/bin/rpmkeys", "--import", KEY]],
    writes_file: { path: KEY, content: KEY_TEXT, mode: 0o644, replaces: false,
      key_fingerprint: "C95B 321B 61E8 8C18 09C4 F759 DDCA E044 F796 ECB0" },
    download_bytes: null,
    terminal: [...tee(KEY, KEY_TEXT), `sudo rpmkeys --import ${KEY}`],
    package_scripts: null,
  };
  const S4 = {
    kind: "s4_install",
    sentence: "Install NVIDIA's tool that lets models use the graphics card · 8.1 MiB download",
    commands: [DNF],
    writes_file: null,
    download_bytes: 8500000,
    terminal: [["sudo", "dnf5", ...DNF.slice(1)].join(" ")],
    package_scripts: SCRIPTS,
  };
  const HASH = "0e3f".repeat(16);
  const helper = (steps, hash = HASH) => ({ hash, how: { kind: "helper" }, steps });

  // --- the checks ----------------------------------------------------------------------------
  const check = (verdict, headline, items, plan = null) =>
    ({ checked_at: T, verdict, headline, items, plan });
  const READY = check("ready", "This computer is ready for local models.",
    [GPU_OK, CDI_OK, PODMAN_OK, FIT_OK, ROOM, DL_DONE]);
  const NEEDS_FIX = check("needs_fix",
    "One thing is missing so models can use the graphics card. lmgw can set it up.",
    [GPU_OK, CDI_MISSING, PODMAN_OK, FIT_OK, ROOM, DL_TODO], helper([S1, S2, S4]));
  // After a run whose install failed: the source and the key are there now.
  const NEEDS_FIX_AFTER = check("needs_fix", NEEDS_FIX.headline,
    [GPU_OK, CDI_MISSING, PODMAN_OK, FIT_OK, ROOM, DL_TODO], helper([S4], "4a1c".repeat(16)));
  const NEEDS_STEPS = check("needs_steps",
    "One thing is missing so models can use the graphics card. Here's how to add it.",
    [GPU_OK, CDI_STEPS, PODMAN_OK, FIT_OK, ROOM, DL_TODO], {
      hash: HASH,
      how: { kind: "instructions", why: "lmgw from an AppImage can't change the system, so here are the commands to run in a terminal. Or install lmgw's RPM, which can do this for you." },
      steps: [S1, S2, S4],
    });
  const NEEDS_RESTART = check("needs_restart", "Restart the computer to finish the graphics driver update.", [
    item("graphics_card", "problem", "Restart the computer to finish the graphics driver update.", [
      "PCI 0000:01:00.0 10de:2204 NVIDIA GeForce RTX 3090, bound to nvidia",
      "kernel module 600.10.01 (/proc/driver/nvidia/version)",
      "libraries on disk 600.20.03 (libnvidia-ml.so.1 → libnvidia-ml.so.600.20.03)",
      "driver packages: xorg-x11-drv-nvidia-600.20.03-1.fc44.x86_64 (RPM Fusion)",
    ], { kind: "driver_mismatch", kernel: "600.10.01", userspace: "600.20.03" }),
    notChecked("card_for_models", "restart first"),
    PODMAN_OK,
    notChecked("settings_fit", "the graphics card comes first"),
    ROOM, DL_DONE,
  ]);
  const NEEDS_DRIVER = check("needs_driver",
    "Your NVIDIA card is using the basic built-in driver. Local models need NVIDIA's own driver.", [
      item("graphics_card", "problem",
        "Your NVIDIA card is using the basic built-in driver. Local models need NVIDIA's own driver.", [
          "PCI 0000:01:00.0 10de:2204 NVIDIA GeForce RTX 3090, bound to nouveau",
          "no NVIDIA kernel module running (/proc/driver/nvidia/version is not there)",
          "how Fedora installs it: turn on third-party software sources in Discover (or GNOME Software) and install the NVIDIA driver, then restart; or RPM Fusion's commands: sudo dnf install akmod-nvidia, then wait until modinfo -F version nvidia names a version, then restart (https://rpmfusion.org/Howto/NVIDIA)",
        ], { kind: "no_nvidia_driver", bound: "nouveau" }),
      notChecked("card_for_models", "the driver comes first"),
      PODMAN_OK,
      notChecked("settings_fit", "the graphics card comes first"),
      ROOM, DL_TODO,
    ]);
  const CLOUD_ONLY = check("cloud_only",
    "No graphics card found. Cloud models work; local models need a graphics card.", [
      item("graphics_card", "problem",
        "No graphics card found. Cloud models work; local models need a graphics card.", [
          "PCI display devices: Intel Corporation Raptor Lake-S GT1 [UHD Graphics 770] (i915)",
        ], { kind: "no_card", others: ["Intel UHD Graphics 770 (i915)"] }),
      notChecked("card_for_models", "no graphics card"),
      PODMAN_OK,
      notChecked("settings_fit", "no graphics card"),
      ROOM,
    ]);

  // The settings switch on an AMD card with NVIDIA's settings (§1.4, WP12).
  const C = (cls, model, field, from, to) => ({ class: cls, model, field, from, to });
  const CHANGES = [
    C("chat", null, "Settings → Runtimes → Chat → Image",
      "ghcr.io/ggml-org/llama.cpp:server-cuda", "ghcr.io/ggml-org/llama.cpp:server-vulkan"),
    C("chat", null, "Settings → Runtimes → Chat → Extra podman run args",
      ["--device", "nvidia.com/gpu=all", "--security-opt", "label=disable"],
      ["--device", "/dev/dri", "--group-add", "keep-groups", "--security-opt", "label=disable"]),
    C("aux", null, "Settings → Runtimes → Aux → Image",
      "ghcr.io/ggml-org/llama.cpp:server-cuda", "ghcr.io/ggml-org/llama.cpp:server-vulkan"),
    C("audio", null, "Settings → Runtimes → Audio → Backend", "cuda", "vulkan"),
    C("chat", "qwen-demo-27b", "Image override", "ghcr.io/ggml-org/llama.cpp:server-cuda-b7000", null),
  ];
  const LEFT = [{
    class: "audio", model: null, field: "Settings → Runtimes → Audio → Extra podman run args",
    value: [], why: "these models are set to run on the processor, which the switch keeps",
  }];
  const RUNNING = [{ class: "chat", model: "qwen-demo-27b", stop: null }];
  const FIT_ACTION = {
    kind: "settings_fit", fit: "amd_vulkan", label: "Switch to the settings for AMD cards",
    changes: CHANGES, left: LEFT, running: RUNNING,
  };
  const SETTINGS_AMD = check("needs_fix",
    "lmgw is set up for NVIDIA cards. It can switch to the settings for AMD cards.", [
      GPU_AMD, CDI_AMD, PODMAN_OK,
      item("settings_fit", "problem",
        "lmgw is set up for NVIDIA cards. It can switch to the settings for AMD cards.", [
          "Settings → Runtimes → Chat → Image: ghcr.io/ggml-org/llama.cpp:server-cuda → ghcr.io/ggml-org/llama.cpp:server-vulkan",
          "Settings → Runtimes → Chat → Extra podman run args: --device nvidia.com/gpu=all --security-opt label=disable → --device /dev/dri --group-add keep-groups --security-opt label=disable",
          "Settings → Runtimes → Aux → Image: ghcr.io/ggml-org/llama.cpp:server-cuda → ghcr.io/ggml-org/llama.cpp:server-vulkan",
          "Settings → Runtimes → Audio → Backend: cuda → vulkan",
          "model qwen-demo-27b (chat), Image override: ghcr.io/ggml-org/llama.cpp:server-cuda-b7000 → the class's",
        ], { kind: "settings_for_nvidia" }, FIT_ACTION),
      ROOM, DL_TODO,
    ]);
  const READY_AMD = check("ready", "This computer is ready for local models.",
    [GPU_AMD, CDI_AMD, PODMAN_OK, item("settings_fit", "ok", "Model settings fit this card.",
      ["checked: Chat, Aux, Audio, Image"]), ROOM, DL_TODO]);
  // The confirmed list is outdated: the row's own image was cleared on its page meanwhile, and
  // the image class's settings were set to NVIDIA's.
  const NOW = [...CHANGES.slice(0, 4), C("image", null, "Settings → Runtimes → Image → Image",
    "ghcr.io/leejet/stable-diffusion.cpp:server-cuda", "ghcr.io/leejet/stable-diffusion.cpp:server-vulkan")];
  const OUTDATED = {
    fit: "amd_vulkan", applied: false,
    sentence: "The model settings changed after this list was made, so nothing was changed. The list here is the switch as it is now: confirm it to switch.",
    changes: NOW, left: LEFT, running: RUNNING,
    outdated: { withdrawn: [CHANGES[4]], added: [NOW[4]], models: [] },
    notes: [], check: null,
  };
  // The switch as it is now, not confirmed: what the confirm says first (review M4).
  const OFFER = {
    fit: "amd_vulkan", applied: false,
    sentence: "Switching to the settings for AMD cards changes 5 settings. Once it has switched, it stops 'qwen-demo-27b' so it starts again with the new settings, unless it is answering a request then. Nothing is changed until it is confirmed.",
    changes: CHANGES, left: LEFT, running: RUNNING, outdated: null, notes: [], check: null,
  };
  // Switched meanwhile (another tab): nothing left to switch (review M5).
  const ALREADY = {
    fit: "amd_vulkan", applied: false,
    sentence: "The model settings already are the ones for AMD cards.",
    changes: [], left: LEFT, running: [], outdated: null, notes: [], check: null,
  };
  const APPLIED = {
    fit: "amd_vulkan", applied: true,
    sentence: "lmgw now uses the settings for AMD cards. 'qwen-demo-27b' was stopped and starts with the new settings at the next request.",
    changes: NOW, left: LEFT,
    running: [{ class: "chat", model: "qwen-demo-27b", stop: { kind: "stopped" } }],
    outdated: null, notes: ["Settings saved."], check: READY_AMD,
  };

  const CHECKS = {
    ready: READY, needs_fix: NEEDS_FIX, needs_steps: NEEDS_STEPS, needs_restart: NEEDS_RESTART,
    needs_driver: NEEDS_DRIVER, cloud_only: CLOUD_ONLY, settings_amd: SETTINGS_AMD,
  };
  let current = CHECKS[want.check] || READY;
  const answered = { check: 0, models_full: 0, upstreams: 0, v1_models: 0, test: 0, lab: 0 };
  window.__fakeAnswered = answered;

  // --- the answers ---------------------------------------------------------------------------
  const json = (body, status = 200) => Promise.resolve(new Response(JSON.stringify(body),
    { status, headers: { "content-type": "application/json" } }));
  const wait = ms => new Promise(r => setTimeout(r, ms));
  let fits = 0;
  let turns = 0;
  window.__fakeTurns = () => turns;
  // The setups (WP8b): their /v1/models catalog, their local rows and aliases, their upstreams.
  const MODEL = (id, owned_by) => ({ id, object: "model", owned_by, capabilities: { task: "chat" } });
  const ROW = (id, pub) => ({ public_name: id, model: { id: 1, model_id: id,
    gguf_path: "demo/qwen-demo-8b-Q4_K_M.gguf", idle_seconds: 300, enabled: true, public: pub } });
  const ALIAS = (alias, upstream_id, model) =>
    ({ id: 1, alias, upstream_id, upstream_model_id: model, enabled: true });
  const UP = (id, name, protocol, kind) => ({ id, name, protocol, kind,
    base_url: kind === "llama_server" ? "http://127.0.0.1:9393/v1" : "https://api.demo-cloud.example/v1",
    enabled: true });
  const CLOUD = UP(2, "demo-cloud", "openai_compatible", "generic");
  const SETUPS = {
    local: { v1: [MODEL("qwen-demo-8b", "llama-server"), MODEL("demo-cloud/gpt-demo", "demo-cloud")],
      local: [ROW("qwen-demo-8b", true)], aliases: [], upstreams: [CLOUD] },
    // A local model clients reach only by an alias: /v1/models names it lmgw's own.
    alias: { v1: [MODEL("assistant-demo", "lmgw")], local: [ROW("qwen-demo-8b", false)],
      aliases: [ALIAS("assistant-demo", 7, "qwen-demo-8b")],
      upstreams: [UP(7, "local-demo", "llama_cpp", "llama_server")] },
    cloud: { v1: [MODEL("demo-cloud/gpt-demo", "demo-cloud")], local: [], aliases: [],
      upstreams: [CLOUD] },
    // A provider whose catalog could not be read: /v1/models is empty, the upstream is there.
    cloud_down: { v1: [], local: [], aliases: [], upstreams: [CLOUD] },
    none: { v1: [], local: [], aliases: [], upstreams: [] },
  };
  const setup = SETUPS[want.models];
  // gpu_setup's message, in core's words (machine/blame.rs, vram/start_failed.rs).
  const GPU_SETUP = "'qwen-demo-8b' could not start on the graphics card of the computer lmgw " +
    "runs on. One piece is missing so models can use the graphics card. lmgw can set it up. " +
    "Open lmgw there to see what to do. (its start said: Error: setting up CDI devices: " +
    "unresolvable CDI devices nvidia.com/gpu=all)";
  // A written switch, kept across the documents of this tab: the settings read as it left them.
  const SWITCHED = "lmgw.fake.machine.switched";
  const realFetch = window.fetch;
  window.fetch = function (input, init) {
    const url = typeof input === "string" ? input : (input && input.url) || String(input);
    const path = new URL(url, location.href).pathname;
    if (path === "/api/op/machine_check") {
      return wait(250).then(() => { answered.check += 1; return json(current); });
    }
    if (path === "/api/op/machine_settings_fit") {
      // The page's fetch hands a Request (gloo-net), whose body is read from a clone.
      const body = input instanceof Request ? input.clone().text()
        : Promise.resolve((init && init.body) || "{}");
      return body.then(text => {
        let args = {};
        try { args = JSON.parse(text || "{}"); } catch (_) { args = {}; }
        if (args.confirm == null) return wait(150).then(() => json(OFFER));
        fits += 1;
        return wait(300).then(() => {
          let a = APPLIED;
          if (want.fit === "outdated" && fits === 1) a = OUTDATED;
          if (want.fit === "already") { a = ALREADY; current = READY_AMD; }
          if (a.check) current = a.check;
          if (a.applied) sessionStorage.setItem(SWITCHED, "1");
          return json(a);
        });
      });
    }
    // The setup's rows, which say whether local models are involved (D184), and its catalog.
    if (path === "/api/models/full" && setup) {
      return wait(120).then(() => {
        answered.models_full += 1;
        return json({ aliases: setup.aliases, candidate_aliases: [], local: setup.local, aux: [],
          audio: [], image: [], passthrough: [] });
      });
    }
    if (path === "/api/upstreams" && setup) {
      return wait(120).then(() => { answered.upstreams += 1; return json({ upstreams: setup.upstreams }); });
    }
    if (path === "/v1/models" && setup) {
      answered.v1_models += 1;
      return json({ object: "list", data: setup.v1 });
    }
    // The model editor's row for the Test's scenes (`test`): a local chat model that is on, as
    // GET /api/local-model?id=901 answers it, so the drive needs no row of its own.
    if (path === "/api/local-model" && want.test && new URL(url, location.href).searchParams.get("id") === "901") {
      return json({
        id: 901, model_id: "qwen-demo-8b", gguf_path: "demo/qwen-demo-8b-Q4_K_M.gguf",
        gguf_present: true, licence: null, source: "manual", mmproj_present: null,
        draft_present: null, params: { ctx_size: 32768 }, extra_args: [], idle_seconds: 300,
        enabled: true, public: true, image: null, extra_run_args: null, warm_start: false,
        fallback: null, ladder: [], rungs: null, problems: [], advisories: [],
        command_line: "podman run --rm --name lmgw-demo-chat-qwen-demo-8b … ghcr.io/ggml-org/llama.cpp:server-cuda --model /models/demo/qwen-demo-8b-Q4_K_M.gguf --ctx-size 32768",
      });
    }
    // The model editor's Test (D179, D188): a start that failed because of the computer, with
    // gpu_setup's code as the op answers it, or a failure of the model's own without it. Always
    // the fake's under a scenario, so a drive never loads a model for real.
    if (path === "/api/op/local_model_test") {
      const gpu = want.test === "gpu_setup";
      return wait(300).then(() => {
        answered.test += 1;
        return json({
          ok: false, loaded: false, model_id: "qwen-demo-8b", class: "chat", probe: "generate",
          latency_ms: gpu ? 840 : 2310,
          error: gpu ? GPU_SETUP : "HTTP 500: {\"error\":{\"code\":500,\"message\":\"failed to load model\"}}",
          ...(gpu ? { code: "gpu_setup" } : {}),
          container_log: gpu ? [] : ["llama_model_load: error loading model: tensor 'blk.0.attn_q.weight' has wrong shape"],
          hint: "see container_log below, and lmgw__local_model_get for the exact command line this model renders to",
        });
      });
    }
    // The Image lab's kept history: one ask whose run failed with gpu_setup (D179).
    if (want.lab === "gpu_setup" && /^\/labs\/api\/image\/groups$/.test(path)) {
      answered.lab += 1;
      const run = {
        id: 9001, group_id: 901, cell: 0, variant: "", endpoint: "/v1/images/generations",
        model_asked: "image/sdxl-demo", answered_by: "image/sdxl-demo", fallback_reason: null,
        upstream_model: null, http_status: 503, error_code: "gpu_setup",
        error_message: GPU_SETUP.replace(/'qwen-demo-8b'/, "'sdxl-demo'"),
        request: "{\"model\":\"image/sdxl-demo\",\"prompt\":\"a red cube on a white table\"}",
        response: JSON.stringify({ error: { code: "gpu_setup", message: "…" } }),
        response_type: "application/json", headers: {}, input_tokens: null, output_tokens: null,
        latency_ms: 910, created_at: "2026-01-01 12:09:00", artifacts: [],
      };
      return json({ groups: [{ id: 901, lab: "image", title: "a red cube on a white table",
        form: {}, plan: [{ model: "image/sdxl-demo", variant: "" }], override: null,
        created_at: "2026-01-01 12:09:00", runs: [run], artifacts: [], item: null,
        in_flight: false }], total: 1, older: 0 });
    }
    // A Chat turn whose model could not start on the graphics card (WP8b): the 503's code on
    // the turn's own error frame, as core sends it (web/chat_turn/out.rs).
    if (want.turn === "gpu_setup" && /^\/chat\/api\/threads\/\d+\/send$/.test(path)) {
      turns += 1;
      // Core stamps the failed start on the check before it answers, and tells the live feed.
      window.__fakeFailStart();
      const frames = [["error", { message: GPU_SETUP, code: "gpu_setup" }]];
      const text = frames.map(([e, d]) => `event: ${e}\ndata: ${JSON.stringify(d)}\n\n`).join("");
      return wait(300).then(() => new Response(text,
        { status: 200, headers: { "content-type": "text/event-stream" } }));
    }
    if (path === "/api/settings-full" && sessionStorage.getItem(SWITCHED)) {
      return realFetch.apply(this, arguments).then(r => r.json()).then(j => {
        const vk = "ghcr.io/ggml-org/llama.cpp:server-vulkan";
        if (j.router) j.router.image = vk;
        if (j.aux_router) j.aux_router.image = vk;
        if (j.audio) j.audio.backend = "vulkan";
        return json(j);
      });
    }
    return realFetch.apply(this, arguments);
  };

  // --- the live feed's machine frames (D183) --------------------------------------------------
  // The page's EventSource, with the dev instance's own `machine` frames dropped: the fake's
  // check is the one the page reads, and a real change would come as a frame of its own.
  const RealES = window.EventSource;
  let stream = null;
  class FakeES extends RealES {
    constructor(url, opts) { super(url, opts); stream = this; }
    addEventListener(type, cb, opts) {
      if (type !== "machine") return super.addEventListener(type, cb, opts);
      return super.addEventListener(type, ev => { if (ev.lmgwFake) cb(ev); }, opts);
    }
  }
  window.EventSource = FakeES;
  const summary = c => ({ verdict: c.verdict, headline: c.headline, checked_at: c.checked_at,
    failed_start: c.failed_start || null });
  const tell = () => {
    if (!stream) return "no stream";
    const ev = new MessageEvent("machine", { data: JSON.stringify(summary(current)) });
    ev.lmgwFake = true;
    stream.dispatchEvent(ev);
    return "told";
  };
  // The check changes under an open page (a driver update that wants a restart).
  window.__fakeSetCheck = name => {
    current = { ...CHECKS[name], checked_at: "2026-01-01 12:18:00" };
    return tell();
  };
  // A local model's start failed because of the computer, from a request outside the Chat
  // (Kai's, an app's): core stamps it on the check (D182).
  let failures = 0;
  window.__fakeFailStart = (model = "qwen-demo-8b") => {
    failures += 1;
    current = { ...current,
      failed_start: { model, class: "chat", at: `2026-01-01 12:3${failures}:00`, count: failures } };
    return tell();
  };

  if (!want.app) return;

  // --- the app window's bridge, and machine_apply ---------------------------------------------
  const callbacks = new Map();
  let nextId = 4000;
  const internals = {
    transformCallback(cb, once) {
      const id = nextId++;
      callbacks.set(id, data => { if (once) callbacks.delete(id); cb && cb(data); });
      return id;
    },
    unregisterCallback(id) { callbacks.delete(id); },
    runCallback(id, data) {
      const cb = callbacks.get(id);
      if (cb) cb(data);
      else console.warn(`[TAURI] Couldn't find callback id ${id}.`);
    },
  };
  const ev = (event, more = {}) => ({ event, ...more });
  const line = (step, kind, stream, text) => ev("line", { step, kind, stream, line: text });
  const OK = (step, kind) => ev("step_ended", { step, kind, exit: 0, signal: null, error: null });
  const RUN_123 = [
    ev("password"), ev("authorized"), ev("started", { steps: 3 }),
    ev("step_started", { step: 1, of: 3, kind: "s1_add_repo" }),
    line(1, "s1_add_repo", "helper", `create ${REPO} (mode 0644)`), OK(1, "s1_add_repo"),
    ev("step_started", { step: 2, of: 3, kind: "s2_add_key" }),
    line(2, "s2_add_key", "helper", `create ${KEY} (mode 0644)`),
    line(2, "s2_add_key", "helper", `/usr/bin/rpmkeys --import ${KEY}`), OK(2, "s2_add_key"),
    ev("step_started", { step: 3, of: 3, kind: "s4_install" }),
    line(3, "s4_install", "helper", DNF.join(" ")),
    line(3, "s4_install", "stdout", "Updating and loading repositories:"),
    line(3, "s4_install", "stdout", " NVIDIA Container Toolkit (added by lmgw) 100% |  12.4 KiB/s |   3.1 KiB |  00m00s"),
    line(3, "s4_install", "stdout", "Repositories loaded."),
  ];
  const INSTALL = [
    line(3, "s4_install", "stdout", "Package                              Arch    Version     Repository       Size"),
    line(3, "s4_install", "stdout", "Installing:"),
    line(3, "s4_install", "stdout", " nvidia-container-toolkit-base       x86_64  1.20.1-1    lmgw-nvidia-c  27.0 MiB"),
    line(3, "s4_install", "stderr", "<b>[1/1] nvidia-container-toolkit-base-0:1.20.1-1.x86_64 100% | 4.2 MiB/s | 8.5 MiB | 00m02s</b>"),
    line(3, "s4_install", "stdout", "Running transaction"),
    line(3, "s4_install", "stdout", "Complete!"),
  ];
  const FAIL = [
    line(3, "s4_install", "stderr", "Curl error (28): Timeout was reached for https://nvidia.github.io/libnvidia-container/stable/rpm/x86_64/repodata/repomd.xml [Connection timed out after 30000 milliseconds]"),
    line(3, "s4_install", "stderr", "Failed to download metadata (librepo) for repository \"lmgw-nvidia-container-toolkit\""),
    ev("step_ended", { step: 3, kind: "s4_install", exit: 1, signal: null, error: null }),
    ev("finished", { exit: 3 }), ev("checking"),
  ];
  const texts = evs => evs.filter(e => e.event === "line" || e.event === "note").map(e => e.line);
  // Each run: the events, then the answer (none: the run is held there, for a shot).
  const RUNS = {
    password: { events: [ev("password")], answer: null },
    running: { events: [...RUN_123, ...INSTALL.slice(0, 4)], answer: null },
    applied: {
      events: [...RUN_123, ...INSTALL, OK(3, "s4_install"), ev("finished", { exit: 0 }), ev("checking")],
      answer: () => ({
        outcome: "applied", sentence: "Done. Models can use the graphics card now.", step: "s4_install",
        output: texts([...RUN_123, ...INSTALL]), detail: [], check: READY,
      }),
    },
    step_failed: {
      events: [...RUN_123, ...FAIL],
      answer: () => ({
        outcome: "step_failed",
        sentence: "Setting it up didn't finish. NVIDIA's download server didn't answer. Try again later. The step's output is in the details, and the steps before it stay done.",
        step: "s4_install", output: texts([...RUN_123, ...FAIL]),
        detail: ["step 3 of 3 (s4_install) exited with code 1",
          "the setup's lines are in the system journal: journalctl -u lmgw-machine-setup"],
        check: NEEDS_FIX_AFTER,
      }),
    },
  };
  // The ends that ran nothing (§3.5, §4.3): each answers with the check from before the dialog.
  const said = (outcome, sentence, detail = [], check = NEEDS_FIX) =>
    ({ events: [], answer: () => ({ outcome, sentence, step: null, output: [], detail, check }) });
  RUNS.no_agent = said("no_agent",
    "Your desktop has no password window running, so lmgw can't ask for the password. Here are the commands to run in a terminal instead.",
    ["pkexec: Error creating textual authentication agent: No authentication agent found."]);
  RUNS.plan_changed = said("plan_changed",
    "Something on this computer changed since lmgw showed the list. Here is the new list; nothing was changed.",
    [`the page's plan is ${HASH}; lmgw's check computes ${"4a1c".repeat(16)} now`], NEEDS_FIX_AFTER);
  RUNS.declined = said("declined", "Nothing was changed.");
  RUNS.unavailable = said("unavailable", "Open lmgw's window and press Set it up there.",
    ["no confirmation was shown: the window was not in front"]);
  // Lines that keep coming, one every 120 ms, held at the end: the output following its end.
  const STREAM = Array.from({ length: 140 }, (_, i) =>
    line(3, "s4_install", "stdout", `[${String(i + 1).padStart(3, "0")}/140] demo-package-${i + 1}.example.rpm`));
  RUNS.stream = { events: [...RUN_123, ...STREAM], answer: null, pace: 120 };
  // A setup from before, followed without the journal (this account may not read it).
  RUNS.following = { events: [ev("following", { state: "active" })], answer: null };
  RUNS.following_end = {
    events: [ev("following", { state: "active" }), ev("checking")],
    answer: () => ({
      outcome: "ended", step: null, output: [],
      sentence: "The setup that was running has ended. The check shows what is in place now.",
      detail: ["lmgw-machine-setup.service was already active: lmgw followed it instead of starting another",
        "lmgw-machine-setup.service is gone now"],
      check: READY,
    }),
  };
  internals.invoke = function (cmd, args) {
    if (cmd !== "machine_apply") return Promise.resolve(null); // the zoom, the mounted event
    // The command's own Err (lmgw still starting): a rejection with its words.
    if (want.apply === "err") return wait(200).then(() => Promise.reject("lmgw is still starting"));
    const run = RUNS[want.apply] || RUNS.password;
    const id = Number(String(args.onEvent).split(":")[1]);
    const send = (i, message) => internals.runCallback(id, { message, index: i });
    return new Promise(resolve => {
      (async () => {
        await wait(400); // the shell's own dialog, answered "Set it up"
        for (let i = 0; i < run.events.length; i++) {
          // Delivered out of order once, as a large message's fetch can be: the page orders.
          if (i === 4 && i + 1 < run.events.length) {
            send(i + 1, run.events[i + 1]);
            send(i, run.events[i]);
            i += 1;
          } else {
            send(i, run.events[i]);
          }
          await wait(run.pace || 60);
        }
        if (!run.answer) return; // held: the shot is of this moment
        const a = run.answer();
        if (a.check) current = a.check;
        resolve(a);
        await wait(50);
        internals.runCallback(id, { end: true, index: run.events.length });
      })();
    });
  };
  window.__TAURI_INTERNALS__ = internals;
})();
