//! Per-backend-class settings: VRAM/hold/docs-search knobs, and the
//! router/audio/image per-model-container class settings.

use serde::{Deserialize, Serialize};

/// Admission control over the GPU the three managed containers share
/// (quickdoc §9b).
///
/// lmgw is the sole ingress for the chat router, the aux router and audio.cpp,
/// so it is the only place that can refuse to start a fourth model on a GPU that
/// holds three. Every number here is a *policy* choice rather than a
/// measurement, which is why each is a field the owner can see and change —
/// nothing in this file is compiled in.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct VramSettings {
    /// Master switch. When off, requests are forwarded exactly as they were
    /// before this existed and the ledger is reporting only.
    pub enabled: bool,
    /// Memory kept free above the GGUF-derived estimate, in MiB.
    ///
    /// The estimate is weights + KV cache, both read from the file and the
    /// section's own flags. What it does **not** describe — llama.cpp's compute
    /// and graph buffers, the CUDA context, allocator fragmentation — has no
    /// metadata to derive it from, so it lives here as one visible number
    /// instead of being smuggled into the estimate as a fudge factor. Raise it
    /// if loads still OOM; the status surface shows both figures side by side.
    pub headroom_mb: u64,
    /// GPU memory lmgw may plan against, in MiB. `0` = the device's real
    /// total, read from the driver (NVML on NVIDIA, amdgpu's sysfs counters on
    /// AMD — `vram::detect_probe`).
    ///
    /// This is also the **only** capacity figure available on a host where
    /// neither probe answers (no driver, an Intel GPU, a CI runner, a container
    /// without the device mapped). `0` there means admission control has nothing
    /// to measure against and stays inactive rather than guessing — the status
    /// surface says so. It is also the override for an APU, where the AMD probe
    /// counts the GTT pool as GPU memory (see `vram::amdgpu`) and the owner may
    /// want a tighter number than "carve-out + everything the kernel would map".
    pub budget_mb: u64,
    /// How long a request may wait for room before it is refused, in seconds.
    /// `0` waits indefinitely. A refusal names the wait and what held the GPU;
    /// it is never a silent drop.
    pub queue_timeout_seconds: u64,
    /// How long to wait for an admitted model to report `loaded`, in seconds.
    /// Multi-GB GGUFs off a cold page cache legitimately take minutes.
    pub load_timeout_seconds: u64,
    /// How long to wait for an evicted model to report `unloaded`, in seconds.
    /// `POST /models/unload` returns before the memory is actually free
    /// (~150 ms typical, spike-measured), so this bounds the poll that follows.
    pub unload_timeout_seconds: u64,
    /// Fall back when VRAM outside lmgw's control is short
    /// (candidate-aliases design §4.7): a local model that is not resident,
    /// whose row (or alias) has a fallback, is answered by that fallback at
    /// once when the memory missing is held by something lmgw cannot evict —
    /// a game, another app — rather than queueing for it. Needs per-process
    /// attribution of lmgw's own containers; where that is unavailable, only
    /// the hold triggers a fallback, exactly as with this off.
    ///
    /// On by default — a stored settings blob without the key reads as on.
    /// Turn it off on shared-memory systems (APUs), where the GPU's memory is
    /// host RAM that grows and shrinks with everything else running.
    pub fallback_on_external: bool,
}

/// The manual GPU hold (gpu-hold design §3.1). `#[serde(default)]`d on
/// [`Settings::hold`](crate::config::Settings::hold) so a blob written before this field existed loads as
/// `active: false, fallback_alias: None` — hold off, nothing inherited.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HoldSettings {
    /// The switch. Persisted, so a restart mid-game does not lift it.
    pub active: bool,
    /// Global fallback for chat-class local models. `None` = refuse a held
    /// chat request that has no per-row override.
    pub fallback_alias: Option<String>,
}

impl Default for VramSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            headroom_mb: 1024,
            budget_mb: 0,
            queue_timeout_seconds: 300,
            load_timeout_seconds: 600,
            unload_timeout_seconds: 60,
            fallback_on_external: true,
        }
    }
}

/// Defaults for the per-request retrieval parameters (§6).
///
/// Every one of these is overridable per request, so nothing here is a cap —
/// it is where a request that says nothing starts. The values match
/// `quickdoc_core::retrieve::SearchParams::default()`, which documents why each
/// one is what it is.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DocsSearchSettings {
    /// `K_f` — BM25 candidates.
    pub k_fts: u32,
    /// `K_v` — exact-KNN candidates.
    pub k_vec: u32,
    /// RRF rank-damping constant.
    pub rrf_k: f32,
    /// BM25 column weights. Stored rather than fixed because the playground
    /// exposes them: a knob somebody can tune and then "save as defaults" has
    /// to survive the save, or the page is lying about what it did.
    pub fts_weights: quickdoc_core::retrieve::FtsWeights,
    /// `K_r` — how deep into the fused list the cross-encoder looks. `0`
    /// disables the rerank stage.
    pub k_rerank: u32,
    /// Ranked chunks returned, before the token budget.
    pub limit: u32,
    /// `budget_tokens` for a `docs__query` that does not pass one. **`0` means
    /// no budget**: every one of `limit` chunks is returned and the response
    /// says how many tokens that was. It is not a hidden ceiling — a caller who
    /// wants one passes it.
    pub budget_tokens: u32,
    /// Depth an eval run measures hit@k at when the request names no `k`.
    pub eval_k: u32,
}

impl Default for DocsSearchSettings {
    fn default() -> Self {
        let d = quickdoc_core::retrieve::SearchParams::default();
        Self {
            k_fts: d.k_fts as u32,
            k_vec: d.k_vec as u32,
            rrf_k: d.rrf_k,
            fts_weights: d.fts_weights,
            k_rerank: d.k_rerank as u32,
            limit: d.limit as u32,
            budget_tokens: 0,
            eval_k: d.limit as u32,
        }
    }
}

impl DocsSearchSettings {
    /// These settings as one request's starting parameters.
    pub fn params(&self) -> quickdoc_core::retrieve::SearchParams {
        quickdoc_core::retrieve::SearchParams {
            k_fts: self.k_fts as usize,
            k_vec: self.k_vec as usize,
            rrf_k: self.rrf_k,
            fts_weights: self.fts_weights,
            rerank: self.k_rerank > 0,
            k_rerank: self.k_rerank as usize,
            limit: self.limit as usize,
            budget_tokens: (self.budget_tokens > 0).then_some(self.budget_tokens as usize),
        }
    }
}

/// `localhost`: every browser resolves `*.localhost` to loopback internally,
/// and systemd-resolved does the same for every other resolver client on this
/// box — so an agent origin works on a fresh install with no hosts entry, no
/// DNS zone and nothing to configure (origins §4.1).
pub(super) fn default_agent_origin_suffix() -> String {
    "localhost".into()
}

/// The stock Node image a `script` step runs in (container-runtime §4.2).
/// Alpine, so the pull is tens of megabytes rather than hundreds, and a major
/// tag rather than a digest so a security refresh arrives without an lmgw
/// release; an owner who wants a digest pins one in Settings.
pub(super) fn default_agent_script_image() -> String {
    "docker.io/library/node:24-alpine".into()
}

/// Class settings for the chat/aux llama-server containers (per-model-
/// containers design §6).
///
/// One *class* of per-model containers, not one container: `image` and
/// `extra_run_args` are the defaults each model of the class inherits unless
/// its own row overrides them (§3.1), `models_dir` is the host directory
/// mounted at `/models`, and `public_prefix` names the class's models to
/// clients. `container_name`/`listen_port`/`models_max`/`auto_start` are gone
/// with router mode (§6/§7): the name is derived (§3.3), the port is dynamic
/// (§3.5), the router's own count-based cap has nothing left to cap, and
/// auto-start is a per-model `warm_start` flag.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RouterSettings {
    /// Container image for llama-server (entrypoint `/app/llama-server`).
    /// The class default; a model's `image` column overrides it (§3.1).
    pub image: String,
    /// Host directory with GGUF files; mounted read-only at /models.
    pub models_dir: String,
    /// Extra `podman run` args (GPU/CDI flags etc.). The class default; a
    /// model's `extra_run_args` column overrides it (§3.1).
    pub extra_run_args: Vec<String>,
    /// Namespace for auto-exposed public local models (`local` → clients
    /// request `local/<model id>`); empty = bare model ids.
    pub public_prefix: String,
    /// Per-request ceiling for this class, in seconds — the `timeout_ms` its
    /// synthetic upstream carries ([`Snapshot::synthetic_upstream`](crate::config::snapshot::Snapshot::synthetic_upstream)). **0 =
    /// the maximum possible**: no deadline of lmgw's own, the request ends
    /// when the container ends it.
    ///
    /// Per class rather than one shared constant because the four have nothing
    /// in common here: an embedding batch that takes a minute is broken, a
    /// music generation that takes twenty is working. This bounds the request
    /// only — the *container start* in front of it has its own visible bound
    /// in `vram.load_timeout_seconds`.
    pub request_timeout_seconds: u64,
}

impl Default for RouterSettings {
    fn default() -> Self {
        Self {
            image: "ghcr.io/ggml-org/llama.cpp:server-cuda".into(),
            models_dir: String::new(),
            // Proven on this host (Fedora + NVIDIA CDI). An AMD or Intel host
            // changes both this and `image` in Settings — the values are in
            // docs/amd-arch.md.
            extra_run_args: vec![
                "--device".into(),
                "nvidia.com/gpu=all".into(),
                "--security-opt".into(),
                "label=disable".into(),
            ],
            public_prefix: String::new(),
            // A cold generation on a long prompt, with room to spare. The
            // chat class is the one this number was originally chosen for.
            request_timeout_seconds: 600,
        }
    }
}

impl RouterSettings {
    /// Defaults for the aux class: the same image and run args as chat, its
    /// own `public_prefix` (`embed` → clients request `embed/<model id>`).
    ///
    /// The client-facing prefix keeps its pre-rename spelling on purpose: it
    /// is a live external contract (the model names every client already
    /// sends), and "aux" is lmgw's word for the *slot*, not a reason to break
    /// it. Editable in Settings.
    pub fn default_aux() -> Self {
        Self {
            public_prefix: "embed".into(),
            // An encoder has no generation loop: one forward pass over a batch
            // is milliseconds to seconds. A minute here already means the
            // container is wedged, and saying so in a minute beats saying it
            // in ten.
            request_timeout_seconds: 60,
            ..Self::default()
        }
    }
}

/// audio.cpp `audiocpp_server` class settings (§6). Its own type rather than
/// another [`RouterSettings`] because the engine takes no CLI flags: every
/// per-model knob goes into a mounted `server.json`, and the four engine
/// fields below are what flows into it (§3.6).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioSettings {
    /// Container image; the entrypoint's first arg selects the tool
    /// (`server`). Backends are baked into the tag: `full-cuda12`,
    /// `full-cuda13`, `full-cpu`.
    pub image: String,
    /// Host directory with the audio models; mounted read-only at /models.
    pub models_dir: String,
    /// Inference backend written into server.json (`cuda` | `cpu` | `vulkan`
    /// | `metal` | `hip`) — must match the backend the image was built with.
    pub backend: String,
    /// `--device`: backend device index.
    pub device: i64,
    /// `--threads`: backend/OpenMP worker threads.
    pub threads: i64,
    /// Register configured model ids at startup, load each on first use.
    pub lazy_load: bool,
    /// `busy_timeout_ms`: how long a request waits for a model that is already
    /// running before it fails fast with `503 server_busy`.
    ///
    /// audiocpp_server serializes a model's requests on one lock, and a GPU
    /// call that wedges cannot be cancelled from userspace — without a bound,
    /// every later request parks a worker thread forever. Rendered
    /// **explicitly** rather than left to the engine's own default, so the
    /// effective value is readable in the config file and editable here.
    /// **0 = wait forever**, the engine's way of disabling the guard; a model
    /// row may lower it ([`AudioModel::busy_timeout_ms`](crate::config::AudioModel::busy_timeout_ms)) for work that is
    /// legitimately slower or faster than the class.
    #[serde(default = "default_audio_busy_timeout_ms")]
    pub busy_timeout_ms: i64,
    /// `idle_unload_ms`: unload the resident model after this long with no
    /// load or run, freeing its VRAM while the container stays up; the next
    /// request reloads it. **0 = never** (the engine default). This is the
    /// class's only "give the card back" mechanism short of stopping the
    /// container, which is what the image class's `idle_seconds` does.
    #[serde(default)]
    pub idle_unload_ms: i64,
    /// `min_free_memory_mb`: refuse a model load when host or GPU memory would
    /// not keep this much free after the load's estimated footprint —
    /// answered as `503 insufficient_memory` instead of an OOM kill halfway
    /// through. **0 = no guard** (the engine default).
    #[serde(default)]
    pub min_free_memory_mb: i64,
    /// `max_request_body_bytes`, in MiB: the largest HTTP body audiocpp_server
    /// buffers before routing it.
    ///
    /// **0 = leave the key out**, so the engine's own 2 GiB default applies —
    /// stated here rather than hidden, because an upload larger than that is
    /// refused by the container and this is the only place to raise it.
    #[serde(default)]
    pub max_request_body_mb: i64,
    /// `voice_dir`: a directory of `<name>.wav` clips plus a `prompt_text`
    /// mapping file, which every TTS model in the container can clone from by
    /// name — a request's `voice: "<name>"` resolves to `<voice_dir>/<name>.wav`
    /// when it matches no configured preset.
    ///
    /// Defaults to `/models/voices`: the Audio lab's own voice library, as the
    /// container sees it. Empty leaves the key out.
    #[serde(default = "default_audio_voice_dir")]
    pub voice_dir: String,
    /// Extra `podman run` args (GPU/CDI flags etc.).
    pub extra_run_args: Vec<String>,
    /// Namespace of the audio class's models (`audio` → clients request
    /// `audio/<model id>`); empty = bare passthrough.
    pub public_prefix: String,
    /// Per-request ceiling for this class, in seconds — the `timeout_ms` its
    /// synthetic upstream carries ([`Snapshot::synthetic_upstream`](crate::config::snapshot::Snapshot::synthetic_upstream)). **0 =
    /// the maximum possible**: no deadline of lmgw's own, the request ends
    /// when the container ends it. Bounds the request only; the container
    /// start in front of it has `vram.load_timeout_seconds`.
    pub request_timeout_seconds: u64,
}

impl Default for AudioSettings {
    fn default() -> Self {
        Self {
            // AMD/Intel: `full-vulkan` with `backend: "vulkan"` — audio.cpp
            // publishes no ROCm image (docs/amd-arch.md).
            image: "ghcr.io/0xshug0/audio.cpp:full-cuda12".into(),
            models_dir: String::new(),
            backend: "cuda".into(),
            device: 0,
            threads: 1,
            lazy_load: true,
            busy_timeout_ms: default_audio_busy_timeout_ms(),
            idle_unload_ms: 0,
            min_free_memory_mb: 0,
            max_request_body_mb: 0,
            voice_dir: default_audio_voice_dir(),
            // Proven on this host (Fedora + NVIDIA CDI).
            extra_run_args: vec![
                "--device".into(),
                "nvidia.com/gpu=all".into(),
                "--security-opt".into(),
                "label=disable".into(),
            ],
            public_prefix: "audio".into(),
            // TTS of a long script, and every /v1/tasks/* job behind it —
            // separation, diarization, music generation — is minutes of work
            // on one connection.
            request_timeout_seconds: 1800,
        }
    }
}

/// audio.cpp's own default for `busy_timeout_ms` (5 minutes). Pinned here so
/// the rendered `server.json` states the value it is running under instead of
/// depending on which build is in the image.
fn default_audio_busy_timeout_ms() -> i64 {
    300_000
}

/// The Audio lab's voice library as the container sees it — the audio models
/// dir is mounted at `/models`, and the lab writes reference clips to
/// `<models dir>/voices`.
fn default_audio_voice_dir() -> String {
    "/models/voices".into()
}

/// stable-diffusion.cpp `sd-server` class settings (image-generation design
/// §4): the same four fields every class has, and no engine-specific ones.
///
/// sd-server has no config file — everything per-process is a CLI flag, and
/// anything a *model* needs lives in that row's `args`. So unlike
/// [`AudioSettings`] there is nothing here but image, dir, run args and
/// prefix.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ImageSettings {
    /// Container image. The entrypoint is `/sd-cli`, so the rendered
    /// `podman run` overrides it with `--entrypoint /sd-server` (§2.5).
    /// Unpinned by default, the same posture the llama and audio class
    /// defaults take; pin a digest per model when a build matters.
    pub image: String,
    /// Host directory with the image weights; mounted read-only at /models.
    ///
    /// Empty by default — exactly like the three older classes, which also
    /// ship no path and refuse to start a model until the owner names one.
    /// The layout this class expects is `<data_dir>/sdcpp`
    /// (`~/.local/share/lmgw/sdcpp`, where the spike's weights already live),
    /// with `<owner>/<repo>/<file>` underneath it.
    pub models_dir: String,
    /// Extra `podman run` args (GPU/CDI flags etc.). Load-bearing beyond the
    /// usual: the sd-server binary links `libcuda.so.1` directly, so even
    /// `--help` fails without the device attached (§12.7), which is why the
    /// help probe runs with these too.
    pub extra_run_args: Vec<String>,
    /// Namespace of the image class's models (`image` → clients request
    /// `image/<model id>`); empty = bare passthrough.
    pub public_prefix: String,
    /// Per-request ceiling for this class, in seconds — the `timeout_ms` its
    /// synthetic upstream carries ([`Snapshot::synthetic_upstream`](crate::config::snapshot::Snapshot::synthetic_upstream)). **0 =
    /// the maximum possible**: no deadline of lmgw's own, the request ends
    /// when the container ends it. Bounds the request only; the container
    /// start in front of it has `vram.load_timeout_seconds`.
    pub request_timeout_seconds: u64,
}

impl Default for ImageSettings {
    fn default() -> Self {
        Self {
            // AMD/Intel: `master-vulkan` — there is no ROCm image, and no CPU
            // tag at all (§2.5).
            image: "ghcr.io/leejet/stable-diffusion.cpp:master-cuda".into(),
            models_dir: String::new(),
            // Proven on this host (Fedora + NVIDIA CDI), as the other classes.
            extra_run_args: vec![
                "--device".into(),
                "nvidia.com/gpu=all".into(),
                "--security-opt".into(),
                "label=disable".into(),
            ],
            public_prefix: "image".into(),
            // A high-step render at a large size, or a batch of them, is the
            // same shape of wait as the audio class's.
            request_timeout_seconds: 1800,
        }
    }
}
