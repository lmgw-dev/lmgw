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
    /// The local speech-to-text model that writes a voice-library clip's
    /// transcript (audio-class gap 5) — on upload without a typed one, and
    /// for the Audio lab's "transcribe" actions when they name none. Must be
    /// a local `asr` audio row: the clips are the owner's voice and never go
    /// to a cloud route. **Empty (the default): nothing is transcribed
    /// unless asked with a model named.**
    #[serde(default)]
    pub voice_transcribe_alias: String,
    /// Which revision an audio catalog download takes. **`pinned` (the
    /// default):** the commit an audio.cpp spec pins for a package — the one
    /// its author tested the engine against, which a third-party repo's
    /// later re-upload in another layout cannot break — and `main` where it
    /// pins none. **`latest`:** always `main`. Update checks and re-downloads
    /// of catalog files follow it too, at the pin the spec names now
    /// (`audio::pins::tracked_revision`). Never a silent switch between the
    /// two: a pin that cannot be fetched fails the download, naming this
    /// setting.
    #[serde(default)]
    pub catalog_revision: CatalogRevision,
}

/// `audio.catalog_revision`: what an audio catalog download takes when the
/// spec pins a package to a commit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CatalogRevision {
    /// The spec's pinned commit, where it pins one; `main` otherwise.
    #[default]
    Pinned,
    /// `main`, whatever the spec pins.
    Latest,
}

impl CatalogRevision {
    pub const ALL: [&'static str; 2] = ["pinned", "latest"];

    pub fn as_str(self) -> &'static str {
        match self {
            CatalogRevision::Pinned => "pinned",
            CatalogRevision::Latest => "latest",
        }
    }

    /// The setting's value from its name, refused by name otherwise.
    pub fn parse(v: &str) -> Result<Self, String> {
        match v.trim().to_ascii_lowercase().as_str() {
            "pinned" => Ok(CatalogRevision::Pinned),
            "latest" => Ok(CatalogRevision::Latest),
            other => Err(format!(
                "audio.catalog_revision is 'pinned' or 'latest', not '{other}'"
            )),
        }
    }
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
            voice_transcribe_alias: String::new(),
            catalog_revision: CatalogRevision::Pinned,
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

// The built-in voice instructions' three sentences, as literals so the
// whole is one `concat!` and stays byte-identical to what realtime has
// always sent (chat-voice design §8.5).
macro_rules! voice_persona {
    () => {
        "You are a voice assistant."
    };
}
macro_rules! voice_form {
    () => {
        "Your replies are spoken aloud, so keep them short and conversational: use plain \
         sentences, and never use markdown, lists, tables or code."
    };
}
macro_rules! voice_follows_user {
    () => {
        "Answer in the language the user speaks."
    };
}
macro_rules! voice_no_date {
    () => {
        "You do not know the current date or time unless the conversation says it."
    };
}

/// Who the model is, in the built-in voice instructions. A chat thread's
/// voice turn leaves it out: the thread's own prompt says who the model is
/// (chat-voice design §8.5).
pub const VOICE_PERSONA: &str = voice_persona!();
/// How a spoken reply is written: short, plain sentences, no Markdown,
/// lists, tables or code ([`VOICE_FORM`]), in the user's language
/// ([`VOICE_FOLLOWS_USER`]).
pub const VOICE_STYLE: &str = concat!(voice_form!(), " ", voice_follows_user!());
/// The form half of [`VOICE_STYLE`]. A chat voice turn with a conversation
/// language keeps it and says the language instead of
/// [`VOICE_FOLLOWS_USER`] (chat-voice design §8.5).
pub const VOICE_FORM: &str = voice_form!();
/// The language half of [`VOICE_STYLE`]: the reply follows the user.
pub const VOICE_FOLLOWS_USER: &str = voice_follows_user!();
/// That the model does not know the date. A chat thread's voice turn keeps
/// it only while the thread's prompt has no `{{date}}`.
pub const VOICE_NO_DATE: &str = voice_no_date!();

/// `realtime.default_instructions`' default (realtime design §12, §23 L8):
/// the three sentences above, in order.
pub const DEFAULT_VOICE_INSTRUCTIONS: &str = concat!(
    voice_persona!(),
    " ",
    voice_form!(),
    " ",
    voice_follows_user!(),
    " ",
    voice_no_date!()
);

/// What decides a barge-in once the evidence gate passed (realtime design
/// §6.4): `realtime.barge_in_check`, and `session.lmgw.barge_in_check`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BargeInCheck {
    /// The words: the evidence is transcribed, and a backchannel or nothing
    /// does not cut the answer.
    #[default]
    Words,
    /// The evidence alone, as before the word check.
    Duration,
}

/// The default `realtime.backchannel_words`: German and English words that
/// tell a speaker to go on (realtime design §6.4).
///
/// The rest is what the ASR models wrote for the owner's own backchannels
/// in the offline replays of 2026-10-01 (B3 review, E4 and E6): qwen3-asr's
/// "Uh huh.", nemotron's first check of "Ach so" ("so"), the spellings
/// either may choose, and round 2's "Okee", "Uh", "Huh", "A so", "Ah so".
/// "o.k." is the entry "O.K." normalizes to (two one-letter words). A word
/// of only h and m ("hmmmm") is a backchannel by rule, not by this list
/// (`realtime::turn::backchannel`).
pub const DEFAULT_BACKCHANNEL_WORDS: &[&str] = &[
    "mhm",
    "hm",
    "hmm",
    "mm",
    "mm-hmm",
    "ja",
    "jo",
    "jap",
    "okay",
    "ok",
    "genau",
    "richtig",
    "ach so",
    "aha",
    "stimmt",
    "gut",
    "yeah",
    "yes",
    "yep",
    "right",
    "uh-huh",
    "sure",
    "alright",
    "i see",
    "haha",
    "uh huh",
    "mhmm",
    "mmh",
    "hm hm",
    "hm-hm",
    "mm hm",
    "mmhm",
    "hmhm",
    "so",
    "jaja",
    "achso",
    "okey",
    "o.k.",
    "klar",
    "alles klar",
    "super",
    "okee",
    "uh",
    "huh",
    "uh hmm",
    "a so",
    "ah so",
    "ah",
];

/// The default `realtime.barge_in_check_scripts` (E4): words in Latin
/// script only — what German and English speakers say.
pub const DEFAULT_BARGE_IN_CHECK_SCRIPTS: &[&str] = &["Latin"];

/// `GET /v1/realtime`, the spoken-conversation route (realtime design §12).
///
/// Saved through `ops::RealtimeSettingsPatch` (both settings saves, WP8),
/// which refuses what no session could run; read back as
/// `ops::realtime_view`. A blob written another way still loads with these
/// defaults, and the session start falls back from what cannot run.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RealtimeSettings {
    /// The chat alias a session starts on when the client names no model, or
    /// names an OpenAI Realtime model (`gpt-realtime*`, `gpt-4o*-realtime*`)
    /// that `model_map` does not map (§5.1) — such a name is never taken as a
    /// chat alias, since a catch-all upstream would route it to a chat
    /// endpoint that cannot serve it. Empty = none: a model-less handshake
    /// still opens, and the session reports the missing model when it is
    /// first needed, rather than lmgw picking one.
    pub default_model: String,
    /// Client model names mapped to lmgw aliases — `session.model` to a chat
    /// alias (§5.1), `audio.input.transcription.model` to an ASR alias
    /// (§5.2) — checked **first**, before the OpenAI-name fallback and before
    /// real aliases,
    /// because an entry here is the owner's explicit intent. What lets a
    /// client that hardcodes one name reach a chosen alias without the owner
    /// renaming anything.
    pub model_map: std::collections::BTreeMap<String, String>,
    /// The system prompt a session with audio output uses while the
    /// client's `instructions` are absent or empty (§2.2); a client's own
    /// always win, a text-only session gets none, and the echoed session
    /// shows the ones in effect. **Unset (`None`) = the built-in
    /// [`DEFAULT_VOICE_INSTRUCTIONS`]**, which asks for answers made to be
    /// heard — the first live run, with none, got 19-second answers full of
    /// lists and bold text (§23, L8) — and follows it when a release
    /// improves it. **Empty = none.** An `Option` rather than a string with
    /// that default (package B review 8): the settings blob is saved whole,
    /// and a stored copy of the default text would pin an old one, a stored
    /// `""` would silently turn it off. Read through
    /// [`RealtimeSettings::voice_instructions`].
    pub default_instructions: Option<String>,
    /// `server_vad.threshold` a session starts with (§6.2): the speech
    /// probability that counts as voice. This and the two below are OpenAI's
    /// own documented defaults with OpenAI's meanings; a client's
    /// `session.update` overrides them per session.
    pub threshold: f64,
    /// `server_vad.prefix_padding_ms`: audio kept from before the detected
    /// onset, so the first word survives the detector's lag (§6.2).
    pub prefix_padding_ms: u32,
    /// `server_vad.silence_duration_ms`: the silence that ends a turn (§6.2).
    pub silence_duration_ms: u32,
    /// What serves `semantic_vad` (§6.3): `smart_turn` (the default) or
    /// `server_vad`, the escape hatch — plain silence windows by eagerness,
    /// as before Smart Turn.
    pub semantic_vad_engine: super::SemanticVadEngine,
    /// The Smart Turn rule's knobs per eagerness (§6.3): `threshold`,
    /// `floor`, `max_wait_ms`, and the plain `silence_duration_ms` a pause
    /// falls back to when it cannot be scored. A session echoes its row as
    /// `session.lmgw.resolved.semantic_vad`.
    pub semantic_vad: super::SemanticVadTable,
    /// Where a pause whose Smart Turn score is between the floor and the
    /// threshold commits, in milliseconds of silence (§6.3): the plain
    /// `server_vad` window the rule was measured with. The same for every
    /// eagerness (low has no middle band).
    pub semantic_floor_window_ms: u32,
    /// The silence that ends the turn after a barge-in, in place of
    /// `silence_duration_ms` (§6.5): a user who interrupts usually pauses to
    /// rephrase. Echoed as `session.lmgw.post_interrupt_silence_ms`, which a
    /// session may override; setting it equal to `silence_duration_ms`
    /// restores OpenAI's plain timing.
    pub post_interrupt_silence_ms: u32,
    /// Voiced time needed, while the client plays an answer, before
    /// `speech_started` fires and the answer is cut (§6.4): clients stop
    /// playback on that event, so a cough or a "mhm" must not trigger it.
    /// Speech below it is a backchannel — neither committed nor answered.
    /// Echoed as `session.lmgw.barge_in_min_ms`, which a session may
    /// override; 0 (with `barge_in_guard_ms` 0) is OpenAI's plain behaviour.
    ///
    /// **The default, 200 ms, assumes `barge_in_check` `words`** (E7): a
    /// quick "Stopp" has about 250 ms of voice, so 300 never let it through,
    /// and the word check, not this gate, tells it from "Mhm". With
    /// `duration` this gate alone decides, and backchannels cut far more
    /// often at 200 — an owner who chooses `duration` may want 300 or more.
    pub barge_in_min_ms: u32,
    /// The start of each answer's playback in which no speech counts
    /// towards a barge-in (§6.4): the echo of its first syllables, and the
    /// user's own reaction to the voice starting. Echoed as
    /// `session.lmgw.barge_in_guard_ms`, which a session may override.
    pub barge_in_guard_ms: u32,
    /// Don't listen while the client plays an answer (§6.4): no barge-in and
    /// no turn from what the microphone hears then. For clients without echo
    /// cancellation, whose microphone hears the answer itself — measured,
    /// that is ~100 false barge-ins per minute of playback, and no cheap
    /// server-side check removes them. Off by default: barge-in needs the
    /// client's echo cancellation (a browser's, PipeWire's echo-cancel
    /// module, or a headset). Echoed as `session.lmgw.half_duplex`, which a
    /// session may override.
    pub half_duplex: bool,
    /// How long after the modelled end of an answer's playback the input
    /// still counts as heard during it, on top of the measured ping round
    /// trip, in milliseconds (§6.4). The server knows when it released the
    /// audio and when the microphone's samples arrived; the client plays
    /// the audio its output buffer later, records with its input buffer's
    /// delay, and the room rings on — none of which the round trip sees.
    /// Inside that tail a "mhm" must pass the barge-in gate, and half
    /// duplex does not listen, so the echo of the answer's last words is not
    /// answered. Echoed as `session.lmgw.echo_tail_ms`, which a session may
    /// override; Bluetooth output wants more. 0 = the round trip alone.
    pub echo_tail_ms: u32,
    /// What decides a barge-in once the evidence gate passed (§6.4).
    /// `words` (the default): the evidence is transcribed with the
    /// session's ASR alias before `speech_started` goes out, and a
    /// transcript that is empty or only `backchannel_words` does not cut the
    /// answer — measured on real speech, "Mhm" and "Okay" carry more voiced
    /// time than "Stopp", so duration alone cuts on 8 of 9 backchannels.
    /// Each check is an ASR call with its own usage row. `duration`: the
    /// evidence alone. Without an ASR alias, or when a check fails or times
    /// out, the duration rule decides. Echoed as
    /// `session.lmgw.barge_in_check`, which a session may override.
    pub barge_in_check: BargeInCheck,
    /// The words that are a backchannel, not an interruption, for the word
    /// check (§6.4): a transcript made only of these — in any order,
    /// repeated, punctuation and case ignored, a hyphen joining — keeps the
    /// answer playing. An entry may be several words ("ach so").
    pub backchannel_words: Vec<String>,
    /// The writing systems whose words count for the word check (§6.4), by
    /// Unicode script name (Latin, Greek, Cyrillic, Armenian, Hebrew,
    /// Arabic, Devanagari, Bengali, Thai, Georgian, Hangul, Hiragana,
    /// Katakana, Han); empty = every script. ASR models write noise in
    /// scripts nobody spoke — a hum or a sigh as "嗯。", a cough as "咳。",
    /// "Mhm" as "Угу." — and such a "word" would cut the answer, so a word
    /// with no letter in these scripts is no word, and a transcript of only
    /// such words is a backchannel. **An owner who speaks a language written
    /// in another script must add it here** (or clear the list), or the
    /// check hears nothing they say during an answer. Echoed as
    /// `session.lmgw.barge_in_check_scripts`, which a session may override.
    pub barge_in_check_scripts: Vec<String>,
    /// How long a barge-in word check may take before the duration rule
    /// decides instead, in milliseconds (§6.4): while it runs the answer
    /// keeps playing over the user, so this bounds what a slow or cold ASR
    /// model adds to an interruption. A warm local model answers in well
    /// under 100 ms. Echoed as `session.lmgw.barge_in_check_timeout_ms`,
    /// which a session may override. 0 = no bound.
    pub barge_in_check_timeout_ms: u32,
    /// The speech-to-text alias the barge-in word check transcribes with
    /// (§6.4). Empty = the session's own ASR alias. The check hears 200 ms
    /// of voice or a little more, and not every ASR model hears that much:
    /// on the owner's recordings (live run 2) nemotron-asr's first check
    /// heard nothing in 7 of 7 barge-ins, and missed a quick "Stopp" twice,
    /// while qwen3-asr heard all of them and cut ~250 ms earlier — so a
    /// qwen3-class model here is the recommendation when the turns are
    /// transcribed with something else. Must be an alias whose capability
    /// task is `asr`; each check is a call of its own with its own usage
    /// row, and a check that fails is the duration rule. Echoed as
    /// `session.lmgw.barge_in_check_alias`, which a session may override.
    pub barge_in_check_alias: String,
    /// The speech-to-text alias a session transcribes its turns with (§5.2)
    /// when the client names no transcription model, or names an OpenAI one
    /// (`gpt-4o*-transcribe*`, `whisper-1`) that `model_map` does not map.
    /// Must be an alias whose capability task is `asr`. Empty = the Chat's
    /// `chat_stt_alias`; with neither, an audio session fails at its first
    /// commit with `asr_not_configured` (text sessions are unaffected).
    pub asr_alias: String,
    /// The text-to-speech alias a session speaks with (§5.3) unless the
    /// session names its own (`session.lmgw.tts_model`). Must be an alias
    /// whose capability task is `tts`, or `vdes` — a voice designed from the
    /// session's speech instructions. Empty = none: a response with audio
    /// output fails with `tts_not_configured` before it starts, and text
    /// output is unaffected.
    pub tts_alias: String,
    /// The voice an OpenAI built-in voice name (`alloy`, `marin`, …) — and
    /// a session that names none — speaks with (§5.3), before the TTS row's
    /// own `default_voice_preset`. Empty = the row's default preset.
    pub default_voice: String,
    /// Client voice names mapped to voices of the TTS model (§5.3), checked
    /// after the model's own voices and before the built-in names: what lets
    /// a client that hardcodes a voice reach a chosen one.
    pub voice_map: std::collections::BTreeMap<String, String>,
    /// The speech instructions a session's TTS gets while the client sends
    /// none (`session.lmgw.speech_instructions`, WP10): a speaking style
    /// ("calm, warm, unhurried") for a TTS that reads one, the description of
    /// the voice for a voice-design row. Empty = none — the TTS speaks with
    /// its own neutral delivery; there is no built-in text. On a voice-design
    /// row that has its own default description this stands back: a style is
    /// not a voice. A model that reads no instructions never gets it (the
    /// session's `resolved.speech` says `dropped`).
    pub speech_instructions: String,
    /// Whether an audio response's prompt says what square brackets do for
    /// the session's TTS, so the model can write them: the sounds it can make
    /// (`[laughter]`, `[sigh]`) for a TTS that renders inline tags (WP10), or
    /// delivery cues (`[laughing]` opening a sentence, sent as instructions
    /// until that sentence ends) for a style or passthrough TTS that renders
    /// none, such as Qwen3 CustomVoice — a cloud TTS only when its alias
    /// declares a style (WP9b). Never added to `instructions`; tags and cues apply with
    /// it off too. Echoed as `session.lmgw.tag_hint`, which a session may
    /// override.
    pub tag_hint: bool,
    /// How far output audio runs ahead of real time, in milliseconds (§8.2):
    /// the first `output_lead_ms` of a response's audio goes out at once, and
    /// the rest as fast as it plays. Echoed as `session.lmgw.output_lead_ms`,
    /// which a session may override. The listener's heard position is off by
    /// at most this much after a barge-in (§7.3).
    pub output_lead_ms: u32,
    /// How far synthesis may run ahead of the paced send, in seconds (§8.2):
    /// a speaking response stops synthesizing its next clause while more
    /// than this much of its audio is queued and not yet sent, and goes on
    /// as the writer sends it. Nothing is dropped — it is flow control (WP3
    /// review M3). The TTS hold, dropped after the last clause, then spans
    /// all but this much of a long answer's playback.
    ///
    /// **When it is lifted** (package B review 1, B2 review 3): for a
    /// response whose TTS route holds a local model, when the GPU hold comes
    /// on, a benchmark takes the GPU, or an admission waits for room. The
    /// rest of that one answer is then synthesized at full speed and held as
    /// PCM (48 KB a second) until it is sent — memory bounded by the answer
    /// itself, i.e. by the chat model's output maximum — so the model is let
    /// go seconds later. A client that stops reading does **not** lift it:
    /// the bound stays, and the model is let go while the client does not
    /// read and admitted again when it does. A cloud or fallback route never
    /// lifts it. Echoed as `session.lmgw.synthesis_ahead_s`, which a session
    /// may override. **0 = no bound**: synthesis runs as far ahead as the
    /// TTS allows, and memory grows with the answer.
    pub synthesis_ahead_s: u32,
    /// The longest silence in synthesized speech, in milliseconds (§8.2): a
    /// TTS pads every request's audio with silence of its own before the
    /// first word and after the last, so each join between two requests of
    /// one answer is a hole of 0.7–1.0 s. The silence at a request's ends is
    /// cut so a join is at most this long, and a longer silence inside one
    /// request (where the engine joined chunks of its own) is shortened to
    /// it; shorter pauses are left as the engine made them. An engine's
    /// deliberate long pause (a `[pause]` tag) is capped like any other.
    /// **Below ~150 ms it reaches the gaps inside words** (stop closures) and
    /// makes speech choppy; that is not refused, it is the owner's call.
    /// Echoed as `session.lmgw.longest_pause_ms`, which a session may
    /// override. **0 = keep the engine's silences** as they are.
    pub longest_pause_ms: u32,
    /// Start the session's models in the background, without evicting
    /// anything, when a session opens (§9.1) — they are warmed again at every
    /// `speech_started` either way. Off: the first turn pays any cold start.
    /// For API sessions only: the Chat's voice mode (a session bound to a
    /// chat thread) always loads its models when it is entered — an explicit
    /// press, which may evict idle models (chat-voice design §4.1, §8.1).
    pub warm_on_connect: bool,
    /// The largest WebSocket **message** a client may send, in MiB (§10.4).
    /// tungstenite has a built-in default of 64 MiB; it is set from here
    /// explicitly so the ceiling is a visible setting rather than a library
    /// constant. An overrun closes the socket with a close reason naming this
    /// setting. **0 = no bound.**
    pub max_message_mb: u32,
    /// The largest single WebSocket **frame**, in MiB (§10.4) — tungstenite's
    /// built-in default is 16 MiB. An overrun closes the socket like
    /// `max_message_mb`'s. **0 = no bound of its own**: a frame is then
    /// bounded by `max_message_mb`, since it can never be larger than its
    /// message — and a frame is never unbounded, because tungstenite reserves
    /// a frame's declared length before reading it. Both at 0 is refused by
    /// a save, and at load, with the defaults used instead.
    pub max_frame_mb: u32,
    /// How often a session pings its client, in seconds (§10.4). A ping with
    /// no pong for one more interval closes the session with a close reason
    /// naming this setting, and a closing session's writer gets the same
    /// interval to send what is queued before the socket is dropped — so a
    /// client that vanished, or stopped reading, cannot keep a session, its
    /// task or its socket. **0 = no liveness bound at all**: no pings and no
    /// drain bound. A peer that is still alive but has stopped reading
    /// answers TCP's own probes with a zero receive window, so TCP never
    /// gives up either — such a session, and the key's concurrency slot it
    /// holds, lasts forever.
    pub ping_interval_s: u32,
}

impl RealtimeSettings {
    /// The instructions an audio session uses while the client gives none
    /// (`default_instructions`): `None` when they are turned off.
    pub fn voice_instructions(&self) -> Option<&str> {
        match self.default_instructions.as_deref() {
            None => Some(DEFAULT_VOICE_INSTRUCTIONS),
            Some(s) if s.trim().is_empty() => None,
            Some(s) => Some(s),
        }
    }
}

impl Default for RealtimeSettings {
    fn default() -> Self {
        Self {
            default_model: String::new(),
            model_map: Default::default(),
            default_instructions: None,
            threshold: 0.5,
            prefix_padding_ms: 300,
            silence_duration_ms: 500,
            semantic_vad_engine: Default::default(),
            semantic_vad: Default::default(),
            semantic_floor_window_ms: 500,
            post_interrupt_silence_ms: 1500,
            // Measured on the owner's recordings with the word check
            // (E7): 36 of 36 interruptions caught for 7 of 46 backchannels
            // and 2 of 21 noises cut, against 32, 5 and 2 at 300.
            barge_in_min_ms: 200,
            barge_in_guard_ms: 500,
            half_duplex: false,
            // Wired or built-in audio through a browser or PipeWire: output
            // and input buffers of ~20–100 ms each, plus ~100 ms of a
            // living room's reverberation above Silero's threshold.
            echo_tail_ms: 250,
            barge_in_check: BargeInCheck::Words,
            backchannel_words: DEFAULT_BACKCHANNEL_WORDS
                .iter()
                .map(|w| w.to_string())
                .collect(),
            barge_in_check_scripts: DEFAULT_BARGE_IN_CHECK_SCRIPTS
                .iter()
                .map(|w| w.to_string())
                .collect(),
            // About five times what the slower measured local ASR
            // (qwen3-asr, ~85 ms) takes for a second of speech (§3.1), and
            // at most half a second more before an interruption cuts.
            barge_in_check_timeout_ms: 500,
            barge_in_check_alias: String::new(),
            asr_alias: String::new(),
            tts_alias: String::new(),
            default_voice: String::new(),
            voice_map: Default::default(),
            speech_instructions: String::new(),
            // It acts only on a TTS that makes sounds (WP10 D7) or takes
            // delivery cues (WP9b C6).
            tag_hint: true,
            output_lead_ms: 500,
            synthesis_ahead_s: 30,
            // Supertonic's own pauses between the sentences of one request
            // measured 0.25–0.4 s (2026-10-05), so this leaves them alone
            // and takes the 0.7–1.0 s holes at request joins down to it.
            longest_pause_ms: 400,
            warm_on_connect: true,
            // tungstenite 0.29's own `WebSocketConfig` defaults, so making
            // them explicit changes nothing a client could notice.
            max_message_mb: 64,
            max_frame_mb: 16,
            // Twice what browsers and the `websockets` library behind
            // openai-python allow a pong, so a busy client is not cut off.
            ping_interval_s: 20,
        }
    }
}
