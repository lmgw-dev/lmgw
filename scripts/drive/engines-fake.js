// In-page fakes for the Engines and Downloads shots (modern design WP8): populated pages without a
// build, a pull or a download ever starting. It answers, in the page:
//   engines:   POST /api/op/builds (three builds: one verified and current with an update waiting,
//              one with a live run, one whose last run failed), /api/op/build_get (each build's
//              runs), /api/op/container_images (four images in two engines, one with two tags and three users, one
//              with a registry update waiting, one built by hand; and the disk totals), /api/op/build_env (two
//              repository presets);
//   downloads: GET /api/hf/downloads (a repo with weights and a projector, a single-file repo, a
//              transfer in flight, a failed one, one with an update) and /api/gguf-files (the
//              listings that say which are on disk and who uses them);
//   live feed: the `jobs` frames carry a live build run (phase build, climbing) and a live
//              Hugging Face transfer (climbing), merged over the dev instance's own.
// Every write the pages could make (build_run, build_set, job_cancel, hf_set, image ops) is
// answered with a refusal here and never reaches the instance; run the drives with
// --block-writes as well.
//
// Loaded with `scripts/ui-drive.py --init-script scripts/drive/engines-fake.js` (or ui-matrix.py),
// before the page's own scripts. `?fake-engines=1` on any URL turns it on for the tab
// (sessionStorage "lmgw.fake.engines"); without it nothing is faked. window.__enginesFake counts
// the answers by key.
//
// The data is made up and looks it: repositories are on git.example, models are *-demo.
(() => {
  "use strict";
  try {
    if (new URL(location.href).searchParams.get("fake-engines") === "1") {
      sessionStorage.setItem("lmgw.fake.engines", "1");
    }
    if (sessionStorage.getItem("lmgw.fake.engines") !== "1") return;
  } catch (_) {
    return;
  }
  const answered = (window.__enginesFake = {});
  const count = (k) => { answered[k] = (answered[k] || 0) + 1; };
  const json = (v, status = 200) => new Response(JSON.stringify(v), {
    status, headers: { "content-type": "application/json" },
  });
  const ago = (min) => new Date(Date.now() - min * 60000).toISOString();
  const GiB = 1024 ** 3, MiB = 1024 ** 2;
  // u64 fields take whole numbers only.
  const b = (x) => Math.round(x);

  // --- Engines ---------------------------------------------------------------------------------
  const spec = (o) => Object.assign({
    slug: "", name: "", engine: "llama", repo_url: "https://git.example/demo/llama.cpp.git",
    forge: "github", ref: "master", extras: [], backend: "cuda", cuda_version: null, arch: null,
    ccache: true, ccache_max_size: "20G", build_args: "", keep_layers: false, keep_runs: 3,
    notes: "",
  }, o);
  const run = (o) => Object.assign({
    id: 0, build_id: 0, job_id: null, slug: "", engine: "llama", trigger: "manual",
    status: "succeeded", inputs: { config: { extras: [] } }, base_sha: "0123456789abcdef0123",
    image_id: "sha256:" + "0".repeat(64), image_present: true, tags: [], promoted: false,
    size_bytes: b(5.4 * GiB), error: null, started_at: ago(180), finished_at: ago(170),
  }, o);
  const B1 = spec({ slug: "official-master", name: "Official master",
    notes: "Upstream as it is, rebuilt when master moves." });
  const B2 = spec({ slug: "mtp-demo", name: "MTP demo with two PRs", ref: "b9001",
    extras: [{ kind: "pr", number: 12345 }, { kind: "pr", number: 12399, pin: "abcdef0123456789" }] });
  const B3 = spec({ slug: "sd-demo", name: "", engine: "sdcpp", backend: "vulkan",
    repo_url: "https://git.example/demo/stable-diffusion.cpp.git" });
  const R1 = run({ id: 31, build_id: 1, slug: "official-master", promoted: true,
    tags: ["localhost/lmgw-dev-llama.cpp:official-master", "localhost/lmgw-dev-llama.cpp:official-master-r31"] });
  const R1b = run({ id: 27, build_id: 1, slug: "official-master", status: "up_to_date",
    started_at: ago(2900), finished_at: ago(2899), size_bytes: null, tags: [] });
  const R2 = run({ id: 33, build_id: 2, slug: "mtp-demo", status: "running", finished_at: null,
    image_id: null, size_bytes: null, started_at: ago(6) });
  const R2b = run({ id: 30, build_id: 2, slug: "mtp-demo", status: "unverified",
    tags: ["localhost/lmgw-dev-llama.cpp:mtp-demo-r30"], started_at: ago(1500), finished_at: ago(1480) });
  const R3 = run({ id: 32, build_id: 3, slug: "sd-demo", engine: "sdcpp", status: "failed",
    image_id: null, size_bytes: null, error: "cmake: CUDA toolkit not found (demo)",
    started_at: ago(400), finished_at: ago(395) });
  const view = (id, s, last, current, extra) => Object.assign({
    build: Object.assign({ id, created_at: ago(9000), updated_at: ago(500) }, s),
    last_run: last, current_run: current, live_job_id: null, used_by: [], update: null,
    moving_tag: `localhost/lmgw-dev-${s.engine === "sdcpp" ? "sd.cpp" : "llama.cpp"}:${s.slug}`,
  }, extra || {});
  const BUILDS = [
    view(1, B1, R1, R1, {
      used_by: [{ kind: "class_default", class: "chat" },
        { kind: "running_container", class: "chat", model_id: "qwen-demo-8b", container: "lmgw-wp8-chat-qwen-demo-8b" }],
      update: { checked_at: ago(45), reasons: ["master +37 commits"], ref_moved: true, extras: [], errors: [] },
    }),
    view(2, B2, R2, null, { live_job_id: 9001 }),
    view(3, B3, R3, null),
  ];
  const RUNS = { 1: [R1, R1b], 2: [R2, R2b], 3: [R3] };
  const IMAGES = {
    images: [
      { id: "sha256:" + "1".repeat(64),
        tags: ["localhost/lmgw-dev-llama.cpp:official-master-0123456-89abcde", "localhost/lmgw-dev-llama.cpp:official-master"],
        engine: "llama", backend: "cuda", size: b(5.4 * GiB), created: ago(170),
        provenance: { build_id: 1, build_name: "Official master", run_id: 31, slug: "official-master",
          repo: "https://git.example/demo/llama.cpp.git", git_ref: "master", base: "0123456789abcdef", extras: [] },
        used_by: [{ kind: "class_default", class: "chat" }, { kind: "class_default", class: "aux" },
          { kind: "model_override", class: "embed", model_id: "demo-embed-4b-q4-k-m-cpu" }],
        run_status: "succeeded" },
      { id: "sha256:" + "6".repeat(64), tags: ["localhost/llama-server-cuda:ik-main-demo-build"], engine: "llama",
        backend: null, size: b(4.9 * GiB), created: ago(40000), external: true, used_by: [] },
      { id: "sha256:" + "2".repeat(64), tags: ["registry.example/demo/audio.cpp:v1-demo"], engine: "audio",
        backend: "cuda", size: b(2.1 * GiB), created: ago(20000), external: true, used_by: [],
        registry_update: { checked_at: ago(60), reference: "registry.example/demo/audio.cpp:v1-demo",
          remote_digest: "sha256:" + "3".repeat(64), local_digests: ["sha256:" + "4".repeat(64)], update_available: true } },
      { id: "sha256:" + "5".repeat(64), tags: ["localhost/lmgw-dev-llama.cpp:mtp-demo-r30"], engine: "llama",
        backend: "cuda", size: b(5.6 * GiB), created: ago(1480), used_by: [], run_status: "unverified",
        provenance: { build_id: 2, build_name: "MTP demo with two PRs", run_id: 30, slug: "mtp-demo",
          repo: "https://git.example/demo/llama.cpp.git", git_ref: "b9001", base: "89abcdef01234567", extras: [] } },
    ],
    disk: { images_total: b(18.0 * GiB), reclaimable: b(10.5 * GiB), buildah_orphans: [] },
  };

  const ENV = {
    repo_presets: [
      { id: "llama-demo", name: "llama.cpp (demo upstream)", engine: "llama", repo_url: "https://git.example/demo/llama.cpp.git", forge: "github", default_ref: "master" },
      { id: "sd-demo", name: "stable-diffusion.cpp (demo upstream)", engine: "sdcpp", repo_url: "https://git.example/demo/stable-diffusion.cpp.git", forge: "github", default_ref: "master" },
    ],
    cuda_default: "13.0", arch_auto: ["89"], driver_cuda_max: "13.0", git_ok: true,
    builds_dir: "/home/demo/.cache/lmgw-dev/builds", builds_dir_free_bytes: b(120 * GiB),
  };

  // --- Downloads -------------------------------------------------------------------------------
  const DL = [
    { id: 1, repo: "demo-org/Qwen-Demo-8B-GGUF", file: "Qwen-Demo-8B-Q4_K_M.gguf", target: "chat", status: "done", size: "4.7 GiB" },
    { id: 2, repo: "demo-org/Qwen-Demo-8B-GGUF", file: "mmproj-Qwen-Demo-8B-F16.gguf", target: "chat", status: "done", size: "644.2 MiB" },
    { id: 3, repo: "demo-org/Gemma-Demo-27B-GGUF", file: "Gemma-Demo-27B-UD-Q4_K_XL.gguf", target: "chat", status: "downloading", size: null },
    { id: 4, repo: "demo-org/Gemma-Demo-27B-GGUF", file: "mmproj-Gemma-Demo-27B-F16.gguf", target: "chat", status: "done", size: "812.0 MiB" },
    { id: 5, repo: "demo-org/Embed-Demo-0.6B-GGUF", file: "Embed-Demo-0.6B-Q8_0.gguf", target: "aux", status: "update_available", size: "610.0 MiB" },
    { id: 6, repo: "demo-org/Phi-Demo-mini-GGUF", file: "Phi-Demo-mini-Q8_0.gguf", target: "chat", status: "failed",
      size: null, error: "HTTP 503 from huggingface.co (demo)", kept_bytes: b(1.2 * GiB) },
  ].map((d) => Object.assign({ dest_path: `/home/demo/models/${d.repo}/${d.file}`, error: null, kept_bytes: null }, d));
  const listing = {
    chat: [
      { path: "demo-org/Qwen-Demo-8B-GGUF/Qwen-Demo-8B-Q4_K_M.gguf", size: "4.7 GiB", size_bytes: b(4.7 * GiB), used_by: ["qwen-demo-8b"], role_guess: "weights" },
      { path: "demo-org/Qwen-Demo-8B-GGUF/mmproj-Qwen-Demo-8B-F16.gguf", size: "644.0 MiB", size_bytes: b(644 * MiB), used_by: ["qwen-demo-8b"], role_guess: "mmproj" },
      { path: "demo-org/Gemma-Demo-27B-GGUF/mmproj-Gemma-Demo-27B-F16.gguf", size: "812.0 MiB", size_bytes: b(812 * MiB), used_by: [], role_guess: "mmproj" },
    ],
    aux: [{ path: "demo-org/Embed-Demo-0.6B-GGUF/Embed-Demo-0.6B-Q8_0.gguf", size: "610.0 MiB", size_bytes: b(610 * MiB), used_by: ["embed-demo"], role_guess: "weights" }],
    audio: [], image: [],
  };

  // --- the live feed ---------------------------------------------------------------------------
  const t0 = Date.now();
  const climb = (from, perSec) => Math.min(99, Math.floor(from + ((Date.now() - t0) / 1000) * perSec));
  const fakeJobs = () => {
    const runPct = climb(42, 0.5), dlPct = climb(37, 1);
    return [
      { id: 9001, kind: "build_run", key: "build:2", label: "build mtp-demo", status: "running",
        done: runPct, total: 100, percent: runPct, stage: "build",
        detail: { run_id: 33, build_id: 2, phase: "build", step: "7/12", stage: "builder",
          percent: 61, last_line: "[ 61%] Building CUDA object ggml/src/ggml-cuda/CMakeFiles/fattn-demo.cu.o", waiting_for: null },
        error: null, created_at: ago(6), started_at: ago(6), finished_at: null },
      { id: 9002, kind: "hf_download", key: "hf:3", label: "Gemma-Demo-27B-UD-Q4_K_XL.gguf", status: "running",
        done: b(16.8 * GiB * dlPct / 100), total: b(16.8 * GiB), percent: dlPct, stage: "",
        detail: {}, error: null, created_at: ago(3), started_at: ago(3), finished_at: null },
    ];
  };
  const RealES = window.EventSource;
  let jobsCb = [];
  let lastReal = [];
  class FakeES extends RealES {
    addEventListener(type, cb, opts) {
      if (type !== "jobs") return super.addEventListener(type, cb, opts);
      jobsCb.push(cb);
      return super.addEventListener(type, (ev) => {
        if (ev.lmgwFake) { cb(ev); return; }
        try { lastReal = JSON.parse(ev.data) || []; } catch (_) { lastReal = []; }
        tell();
      }, opts);
    }
  }
  window.EventSource = FakeES;
  const tell = () => {
    const data = JSON.stringify([...lastReal, ...fakeJobs()]);
    for (const cb of jobsCb) {
      const ev = new MessageEvent("jobs", { data });
      ev.lmgwFake = true;
      cb(ev);
    }
  };
  setInterval(tell, 1500);

  // --- fetch -----------------------------------------------------------------------------------
  const WRITES = new Set(["build_run", "build_set", "build_promote", "build_verify", "job_cancel", "hf_set",
    "container_image_pull", "container_image_delete", "container_image_tag", "build_check_merge",
    "build_updates_check", "settings_set_full"]);
  const realFetch = window.fetch;
  window.fetch = async function (input, init) {
    const url = new URL(typeof input === "string" ? input : input.url, location.href);
    const p = url.pathname;
    const m = p.match(/^\/api\/op\/([a-z_]+)$/);
    if (m) {
      const op = m[1];
      let body = {};
      try { body = JSON.parse((init && init.body) || "{}"); } catch (_) { body = {}; }
      if (WRITES.has(op)) {
        count("refused:" + op);
        return json({ error: { message: `refused by engines-fake.js: ${op} would write` } }, 403);
      }
      if (op === "builds") { count("builds"); return json({ builds: BUILDS }); }
      if (op === "build_get") {
        count("build_get");
        const v = BUILDS.find((b) => b.build.id === body.id);
        if (!v) return json({ error: { message: "no such build" } }, 404);
        return json({ view: v, runs: RUNS[body.id] || [], more: false });
      }
      if (op === "container_images") { count("images"); return json(IMAGES); }
      if (op === "build_env") { count("build_env"); return json(ENV); }
    }
    if (p === "/api/hf/downloads") { count("downloads"); return json({ downloads: DL }); }
    if (p === "/api/gguf-files") {
      count("gguf_files");
      return json({ files: listing[url.searchParams.get("target")] || [] });
    }
    return realFetch.apply(this, arguments);
  };
})();
