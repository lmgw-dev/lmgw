//! Settings

use super::*;
use serde::{Deserialize, Serialize};

/// `GET /api/settings-full`. Secrets never round-trip — only `has_*` flags.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SettingsFull {
    pub bind_addr: String,
    pub auth_enabled: bool,
    pub retention_days: i64,
    pub retention_max_rows: i64,
    pub jobs_retention_days: i64,
    pub jobs_max_rows: i64,
    /// Keep hourly usage rollups this many months; `0` = forever.
    pub usage_retention_months: i64,
    /// The label every amount on the Usage page is shown under. One currency,
    /// no conversion — changing it relabels, it never converts.
    pub currency: String,
    /// Alias whose price answers the local-vs-cloud counterfactual; empty =
    /// the panel says no reference is configured.
    pub local_reference_alias: String,
    /// Spend ceiling across every key, in currency micro-units. `0` = none.
    pub global_budget_micro: i64,
    /// `day | month | total`.
    pub global_budget_period: String,
    pub max_body_mb: u32,
    /// The system prompt new Chat threads start with, as in force: the
    /// configured prompt, or the built-in one when none was written.
    pub chat_system_prompt: String,
    /// The built-in default, which saving `chat_system_prompt` as this text
    /// returns to.
    pub chat_system_prompt_builtin: String,
    /// How a text PDF attached in Chat starts out: `text | images | ask`.
    pub chat_pdf_mode: String,
    /// The Chat's speech-to-text alias — dictation, realtime mode and audio
    /// attachments; empty = `realtime.asr_alias`.
    pub chat_stt_alias: String,
    /// The Chat's text-to-speech alias; empty = `realtime.tts_alias`.
    pub chat_tts_alias: String,
    /// The Chat's voice, chosen for the Chat's text-to-speech model; empty =
    /// none named, so realtime's chain decides (`realtime.default_voice`,
    /// then the model's default). A thread that speaks with another model
    /// does not take it.
    pub chat_voice: String,
    /// The Chat's speech instructions; empty = `realtime.speech_instructions`.
    pub chat_speech_style: String,
    /// The language the user speaks (ISO 639-1), what the speech-to-text
    /// model is told; empty = none.
    pub chat_voice_language: String,
    /// The language replies are in (ISO 639-1): the model answers in it
    /// and the voice speaks it; empty = `chat_voice_language`.
    pub chat_voice_reply_language: String,
    /// Where the Chat's own speech models (as saved) do not take their
    /// language as set — an ASR that detects `chat_voice_language` itself, a
    /// voice that cannot speak the reply language; empty without a language.
    pub chat_voice_language_notes: Vec<crate::chat_voice::LanguageNote>,
    /// Read Chat replies aloud as they stream.
    pub chat_read_aloud: bool,
    /// `semantic_vad | server_vad | push_to_talk`.
    pub chat_turn_detection: String,
    /// `off | on`: whether a voice turn goes to the chat model as audio
    /// when the model that answers it takes audio input, wherever it runs
    /// (experimental).
    pub chat_voice_audio_input: String,
    /// Tokens of knowledge-base excerpts one Chat turn may carry (> 0).
    pub chat_kb_budget_tokens: u32,
    /// Days the Chat change feed keeps its records; `0` = all.
    pub chat_feed_retention_days: i64,
    /// Seconds between the Chat feed's keep-alive comments (≥ 1).
    pub chat_feed_keepalive_s: u32,
    /// Records the Chat feed reads per query while a client catches up
    /// (≥ 1).
    pub chat_feed_page_size: u32,
    /// Live Chat feed events held for a slow client before it gets a fresh
    /// `state` instead (≥ 1).
    pub chat_feed_live_buffer: u32,
    /// The gateway's self-admin level: what the `lmgw__*` tools may do, for
    /// every caller, a paired device's own level capped by it.
    pub self_admin: crate::AdminLevel,
    pub sampling_alias: String,
    pub responses_max_tool_calls: u32,
    pub responses_timeout_seconds: u64,
    pub responses_store: bool,
    pub responses_retention_hours: i64,
    pub responses_max_chains: i64,
    /// Room reserved for one extraction reply.
    pub docs_ingest_reply_tokens: u32,
    /// Texts per embedding call during ingest and re-embed.
    pub docs_embed_batch: u32,
    /// Politeness pause between two fetches to the same host, ms.
    pub docs_fetch_delay_ms: u64,
    /// Alias the rerank stage calls; empty = the aux router's single enabled
    /// rerank model, and the search trace names the reason when there is none.
    pub docs_rerank_model: String,
    /// The stage defaults a request that overrides nothing starts from.
    pub docs_search: DocsSearchDefaults,
    pub update_check_enabled: bool,
    pub has_update_token: bool,
    pub has_hf_token: bool,
    /// Where container builds keep their git mirrors, per-run worktrees and
    /// logs, as configured. `None` = the default, which
    /// `builds_dir_effective` spells out.
    pub builds_dir: Option<String>,
    /// The directory builds actually use: `builds_dir`, else
    /// `<data_dir>/builds` (a dev instance: `~/.cache/lmgw-dev/builds`).
    pub builds_dir_effective: String,
    /// Why builds would refuse to run in `builds_dir_effective` (it is on
    /// tmpfs, i.e. RAM); `None` when they would not.
    pub builds_dir_warning: Option<String>,
    /// Forge API tokens by host. Values are always the
    /// `<set>` placeholder — a token never round-trips.
    pub forge_tokens: std::collections::BTreeMap<String, String>,
    /// How often the build update check runs, in hours. `0` = off.
    pub build_update_check_hours: u32,
    /// GPU admission control.
    pub vram: VramSettingsDto,
    /// The manual GPU hold — orthogonal to `vram`
    /// above: a switch on the resolve/start paths, not a capacity policy.
    pub hold: HoldSettingsDto,
    /// Podman container name prefix for the per-model container runtime:
    /// `<container_prefix>-<class>-
    /// <slug>-<hash6>`.
    pub container_prefix: String,
    /// The DNS suffix a service agent's UI is served under,
    /// default `localhost`: `http://<id>.<agent_origin_suffix>:<bind port>/`.
    pub agent_origin_suffix: String,
    /// Why the **stored** suffix shadows the gateway, when it does. It is validated on write, but
    /// `bind_addr`, this machine's host
    /// name and its search domains are the other side of that rule and move
    /// without it: this is the same sentence the boot log carries, so a
    /// gateway that started shadowing says so where the setting is read rather
    /// than only where it was typed. `None` on a gateway that shadows nothing.
    pub agent_origin_suffix_warning: Option<String>,
    /// The stock image a manifest's `script` step runs in, default
    /// `docker.io/library/node:24-alpine`.
    pub agent_script_image: String,
    pub router: RouterSettings,
    pub aux_router: RouterSettings,
    pub audio: AudioSettings,
    /// The stable-diffusion.cpp class.
    pub image: ImageSettings,
    /// `GET /v1/realtime`, spoken conversations.
    pub realtime: crate::realtime::RealtimeSettings,
    pub api_keys: Vec<ApiKeyRow>,
    pub data_dir: String,
    pub version: String,
    /// This machine's CPUs, read once (a derived fact, never settable): the
    /// thread count an audio row switched to the CPU runs with when it names
    /// none.
    pub host_cpu: HostCpu,
}

/// This machine's CPUs (reported as `host_cpu`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct HostCpu {
    /// Physical cores, or the logical count when the topology is unreadable
    /// (`source`).
    pub physical_cores: u32,
    /// Online logical CPUs, SMT siblings included.
    pub logical_cpus: u32,
    /// `topology` (counted from the kernel's CPU topology) | `logical` (the
    /// topology could not be read, so the logical count stands in).
    pub source: String,
}

/// The GPU hold settings. `active` is
/// reported here but never settable through `SettingsFullPatch` — it is
/// toggled only through the `hold_set` op, because engaging it has the side
/// effect of stopping containers.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct HoldSettingsDto {
    pub active: bool,
    pub fallback_alias: Option<String>,
}

/// The GPU admission (VRAM) settings.
// Mirror of `config::VramSettings`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct VramSettingsDto {
    pub enabled: bool,
    pub headroom_mb: u64,
    /// `0` = the device's real total, as the driver reports it (NVML on
    /// NVIDIA, amdgpu sysfs on AMD).
    pub budget_mb: u64,
    /// `0` = wait indefinitely.
    pub queue_timeout_seconds: u64,
    pub load_timeout_seconds: u64,
    pub unload_timeout_seconds: u64,
    /// Fall back when VRAM outside lmgw's control is short. Absent reads as on, like the server's
    /// own default.
    #[serde(default = "default_fallback_on_external")]
    pub fallback_on_external: bool,
}

fn default_fallback_on_external() -> bool {
    true
}

/// The chat/aux *class* settings.
/// `container_name`/`listen_port`/`models_max`/
/// `auto_start` left with router mode: names are derived, ports are dynamic,
/// and auto-start is the per-model `warm_start` flag.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RouterSettings {
    /// Class default; a model's own `image` overrides it.
    pub image: String,
    pub models_dir: String,
    /// Class default; a model's own `extra_run_args` overrides it.
    pub extra_run_args: Vec<String>,
    pub public_prefix: String,
    /// Per-request ceiling for the class, in seconds. **0 = the maximum
    /// possible** — no deadline of lmgw's own. One per class since the four
    /// stopped sharing a single 600 s constant.
    pub request_timeout_seconds: u64,
}

/// The audio class settings: the same fields as the chat/aux class settings,
/// plus the engine fields that flow into every audio model's own
/// `server.json`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AudioSettings {
    pub image: String,
    pub models_dir: String,
    pub backend: String,
    pub device: i64,
    pub threads: i64,
    pub lazy_load: bool,
    /// How long a request waits for a model that is already running before it
    /// fails with `503 server_busy`, in ms. **0 = wait forever.**
    pub busy_timeout_ms: i64,
    /// Unload the resident model after this long without a load or run,
    /// freeing its VRAM while the container stays up. **0 = never.**
    pub idle_unload_ms: i64,
    /// Free memory (host and GPU) a model load must leave behind, in MiB.
    /// **0 = no guard.**
    pub min_free_memory_mb: i64,
    /// Largest request body audiocpp_server buffers, in MiB. **0 = leave the
    /// key out**, so the engine's own 2 GiB default applies.
    pub max_request_body_mb: i64,
    /// Shared voice library as the container sees it (`/models/voices` by
    /// default — the Audio lab's own clips). Empty = no library.
    pub voice_dir: String,
    pub extra_run_args: Vec<String>,
    pub public_prefix: String,
    /// Per-request ceiling for the class, in seconds. 0 = the maximum possible — no deadline of
    /// lmgw's own.
    pub request_timeout_seconds: u64,
    /// The speech-to-text model that writes voice-library clip transcripts
    /// (its configured fallback answers as for any request). Empty = none:
    /// nothing is transcribed automatically.
    pub voice_transcribe_alias: String,
    /// What an audio catalog download takes: `pinned` (the commit the spec
    /// pins, `main` where it pins none — the default) or `latest` (`main`).
    pub catalog_revision: String,
}

/// The image class settings: the same fields as the chat/aux class settings
/// and no engine ones. sd-server has no config file, so everything
/// per-process is a flag in some row's `args`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ImageSettings {
    pub image: String,
    pub models_dir: String,
    pub extra_run_args: Vec<String>,
    pub public_prefix: String,
    /// Per-request ceiling for the class, in seconds. 0 = the maximum possible — no deadline of
    /// lmgw's own.
    pub request_timeout_seconds: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ApiKeyRow {
    pub id: i64,
    pub name: String,
    pub enabled: bool,
}
