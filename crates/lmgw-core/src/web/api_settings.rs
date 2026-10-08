//! Settings surface of the `/api` plane. Deliberately broader than
//! `ops::settings_set` (the tool plane must not widen its own grant, the
//! human dashboard owns everything): bind address, self-admin mode + token,
//! secrets with explicit clear flags, and all three class definitions.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::agents::token;
use crate::config::{
    ApiKey, ApiKeyKind, AudioSettings, BudgetPeriod, ImageSettings, RouterSettings, SelfAdmin,
    VramSettings,
};
use crate::ops::KeyPatch;
use crate::principal::Refusal;
use crate::state::SharedState;
use crate::store;

/// The device half of the credential ops (client-apps design §1.4).
mod device_keys;

/// The chat/aux *class* settings (per-model-containers §6). Four fields left
/// with router mode — the container name is derived (§3.3), the port is
/// dynamic (§3.5), `models_max` has no router left to cap, and auto-start is
/// the per-model `warm_start` flag — and this is a contract change to any
/// third-party caller of `/api/settings-full`, not a compat shim.
#[derive(Serialize)]
pub struct RouterSettingsDto {
    pub image: String,
    pub models_dir: String,
    pub extra_run_args: Vec<String>,
    pub public_prefix: String,
    /// Per-request ceiling in seconds; 0 = the maximum possible (no deadline
    /// of lmgw's own). Per class since the four stopped sharing one constant.
    pub request_timeout_seconds: u64,
}

impl From<&RouterSettings> for RouterSettingsDto {
    fn from(r: &RouterSettings) -> Self {
        RouterSettingsDto {
            image: r.image.clone(),
            models_dir: r.models_dir.clone(),
            extra_run_args: r.extra_run_args.clone(),
            public_prefix: r.public_prefix.clone(),
            request_timeout_seconds: r.request_timeout_seconds,
        }
    }
}

#[derive(Serialize)]
pub struct AudioSettingsDto {
    pub image: String,
    pub models_dir: String,
    pub backend: String,
    pub device: i64,
    pub threads: i64,
    pub lazy_load: bool,
    /// Wait-for-a-busy-model bound, in ms (0 = wait forever).
    pub busy_timeout_ms: i64,
    /// Unload a resident model after this long idle, in ms (0 = never).
    pub idle_unload_ms: i64,
    /// Free memory a load must leave behind, in MiB (0 = no guard).
    pub min_free_memory_mb: i64,
    /// Largest buffered request body, in MiB (0 = the engine's 2 GiB).
    pub max_request_body_mb: i64,
    /// Shared voice library as the container sees it (empty = none).
    pub voice_dir: String,
    pub extra_run_args: Vec<String>,
    pub public_prefix: String,
    /// See [`RouterSettingsDto::request_timeout_seconds`].
    pub request_timeout_seconds: u64,
    /// Local speech-to-text model for voice-library clip transcripts (empty =
    /// none, nothing transcribed automatically).
    pub voice_transcribe_alias: String,
    /// `pinned` | `latest`: what an audio catalog download takes.
    pub catalog_revision: &'static str,
}

impl From<&AudioSettings> for AudioSettingsDto {
    fn from(a: &AudioSettings) -> Self {
        AudioSettingsDto {
            image: a.image.clone(),
            models_dir: a.models_dir.clone(),
            backend: a.backend.clone(),
            device: a.device,
            threads: a.threads,
            lazy_load: a.lazy_load,
            busy_timeout_ms: a.busy_timeout_ms,
            idle_unload_ms: a.idle_unload_ms,
            min_free_memory_mb: a.min_free_memory_mb,
            max_request_body_mb: a.max_request_body_mb,
            voice_dir: a.voice_dir.clone(),
            extra_run_args: a.extra_run_args.clone(),
            public_prefix: a.public_prefix.clone(),
            request_timeout_seconds: a.request_timeout_seconds,
            voice_transcribe_alias: a.voice_transcribe_alias.clone(),
            catalog_revision: a.catalog_revision.as_str(),
        }
    }
}

/// Mirror of `config::ImageSettings` (image-generation design §4). The four
/// fields every class has and no engine-specific ones: sd-server has no config
/// file, so anything per-process is a flag in some row's `args`.
#[derive(Serialize)]
pub struct ImageSettingsDto {
    pub image: String,
    pub models_dir: String,
    pub extra_run_args: Vec<String>,
    pub public_prefix: String,
    /// See [`RouterSettingsDto::request_timeout_seconds`].
    pub request_timeout_seconds: u64,
}

impl From<&ImageSettings> for ImageSettingsDto {
    fn from(i: &ImageSettings) -> Self {
        ImageSettingsDto {
            image: i.image.clone(),
            models_dir: i.models_dir.clone(),
            extra_run_args: i.extra_run_args.clone(),
            public_prefix: i.public_prefix.clone(),
            request_timeout_seconds: i.request_timeout_seconds,
        }
    }
}

/// `GET /api/settings-full`.
pub async fn settings_full(State(st): State<SharedState>) -> Response {
    let snap = st.snapshot();
    let s = &snap.settings;
    let keys: Vec<Value> = snap
        .api_keys
        .iter()
        .map(|k| json!({ "id": k.id, "name": k.name, "enabled": k.enabled }))
        .collect();
    // `chat_archive_days`/`chat_purge_days` are set outside the macro below,
    // not woven into it: this `json!` invocation was already near rustc's
    // default macro recursion limit (128), and two more fields tipped it
    // over. Assigning after construction avoids bumping a crate-wide limit
    // just to route around one call site.
    let mut body = json!({
        "bind_addr": s.bind_addr,
        "auth_enabled": s.auth_enabled,
        "retention_days": s.retention_days,
        "retention_max_rows": s.retention_max_rows,
        "jobs_retention_days": s.jobs_retention_days,
        "jobs_max_rows": s.jobs_max_rows,
        "usage_retention_months": s.usage_retention_months,
        "currency": s.currency,
        "local_reference_alias": s.local_reference_alias,
        "global_budget_micro": s.global_budget_micro,
        "global_budget_period": s.global_budget_period.as_str(),
        "max_body_mb": s.max_body_mb,
        "self_admin": lmgw_api_types::AdminLevel::from(s.self_admin),
        "sampling_alias": s.sampling_alias,
        "responses_max_tool_calls": s.responses_max_tool_calls,
        "responses_timeout_seconds": s.responses_timeout_seconds,
        "responses_store": s.responses_store,
        "responses_retention_hours": s.responses_retention_hours,
        "responses_max_chains": s.responses_max_chains,
        "docs_ingest_reply_tokens": s.docs_ingest_reply_tokens,
        "docs_embed_batch": s.docs_embed_batch,
        "docs_fetch_delay_ms": s.docs_fetch_delay_ms,
        "docs_search": s.docs_search,
        "docs_rerank_model": s.docs_rerank_model,
        "vram": s.vram,
        "hold": s.hold,
        "container_prefix": s.container_prefix,
        "agent_origin_suffix": s.agent_origin_suffix,
        // Asked per read rather than remembered from boot (origins §4.1): the
        // rule is about `bind_addr`, this machine's names and the suffix
        // together, and any of the three can move under a suffix nobody
        // retyped. `null` when it shadows nothing, which is every gateway that
        // has not been moved.
        "agent_origin_suffix_warning": crate::agents::service::origin_suffix_warning(s),
        "agent_script_image": s.agent_script_image,
        "update_check_enabled": s.update_check_enabled,
        "has_update_token": !s.update_token.is_empty(),
        "has_hf_token": !s.hf_token.is_empty(),
        "router": RouterSettingsDto::from(&s.router),
        "aux_router": RouterSettingsDto::from(&s.aux_router),
        "audio": AudioSettingsDto::from(&s.audio),
        "image": ImageSettingsDto::from(&s.image),
        "api_keys": keys,
        "data_dir": st.data_dir.display().to_string(),
        "version": env!("CARGO_PKG_VERSION"),
    });
    body["chat_archive_days"] = json!(s.chat_archive_days);
    body["chat_purge_days"] = json!(s.chat_purge_days);
    // The Chat change feed's limits (client-apps design §2.3).
    body["chat_feed_retention_days"] = json!(s.chat_feed_retention_days);
    body["chat_feed_keepalive_s"] = json!(s.chat_feed_keepalive_s);
    body["chat_feed_page_size"] = json!(s.chat_feed_page_size);
    body["chat_feed_live_buffer"] = json!(s.chat_feed_live_buffer);
    // The default new Chat threads start with, as in force now, and the
    // built-in one beside it: the page's Reset puts that back in the box.
    body["chat_pdf_mode"] = json!(s.chat_pdf_mode);
    body["chat_stt_alias"] = json!(s.chat_stt_alias);
    // The Chat's Voice group (chat-voice design §2.1).
    body["chat_tts_alias"] = json!(s.chat_tts_alias);
    body["chat_voice"] = json!(s.chat_voice);
    body["chat_speech_style"] = json!(s.chat_speech_style);
    body["chat_voice_language"] = json!(s.chat_voice_language);
    body["chat_voice_reply_language"] = json!(s.chat_voice_reply_language);
    // Where the saved languages do not reach a speech model as set — the
    // spoken one the ASR, the reply's the TTS — said beside the fields
    // (chat-voice design §2.1).
    body["chat_voice_language_notes"] = json!(super::chat_voice::language_notes(&st).await);
    body["chat_read_aloud"] = json!(s.chat_read_aloud);
    body["chat_turn_detection"] = json!(s.chat_turn_detection);
    body["chat_voice_audio_input"] = json!(s.chat_voice_audio_input);
    body["chat_kb_budget_tokens"] = json!(s.chat_kb_budget_tokens);
    body["chat_system_prompt"] = json!(s.default_chat_prompt());
    body["chat_system_prompt_builtin"] = json!(crate::config::BUILTIN_CHAT_SYSTEM_PROMPT);
    // Container builds (container-builds §5, §7, §8), outside the macro for
    // the recursion-limit reason above. The effective dir is spelled out
    // because "unset" means a default that differs between prod and a dev
    // instance, and the warning is asked per read: what the dir sits on can
    // change under a setting nobody retyped.
    let builds_dir = st.builds_dir();
    body["builds_dir"] = json!(s.builds_dir);
    body["builds_dir_effective"] = json!(builds_dir.display().to_string());
    body["builds_dir_warning"] = json!(crate::backends::paths::tmpfs_refusal(&builds_dir));
    body["forge_tokens"] = crate::ops::redact_map(&s.forge_tokens);
    body["build_update_check_hours"] = json!(s.build_update_check_hours);
    body["realtime"] = json!(crate::ops::realtime_view(&s.realtime));
    // A derived fact, like the built-in prompt above: what an audio row on
    // the CPU runs with when it names no thread count.
    let cpu = crate::host::cpu();
    body["host_cpu"] = json!({
        "physical_cores": cpu.physical_cores,
        "logical_cpus": cpu.logical_cpus,
        "source": cpu.source,
    });
    Json(body).into_response()
}

#[derive(Deserialize, Default, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RouterSettingsPatch {
    image: Option<String>,
    models_dir: Option<String>,
    extra_run_args: Option<Vec<String>>,
    public_prefix: Option<String>,
    request_timeout_seconds: Option<u64>,
}

impl RouterSettingsPatch {
    fn apply(self, r: &mut RouterSettings) {
        if let Some(v) = self.image {
            r.image = v;
        }
        if let Some(v) = self.models_dir {
            r.models_dir = v;
        }
        if let Some(v) = self.extra_run_args {
            r.extra_run_args = v;
        }
        if let Some(v) = self.public_prefix {
            r.public_prefix = v.trim_matches('/').to_string();
        }
        // No clamp on the way in: 0 is a meaningful value here (no ceiling),
        // and any positive number an owner types is a number they meant.
        if let Some(v) = self.request_timeout_seconds {
            r.request_timeout_seconds = v;
        }
    }
}

#[derive(Deserialize, Default, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct AudioSettingsPatch {
    image: Option<String>,
    models_dir: Option<String>,
    backend: Option<String>,
    device: Option<i64>,
    threads: Option<i64>,
    lazy_load: Option<bool>,
    busy_timeout_ms: Option<i64>,
    idle_unload_ms: Option<i64>,
    min_free_memory_mb: Option<i64>,
    max_request_body_mb: Option<i64>,
    voice_dir: Option<String>,
    extra_run_args: Option<Vec<String>>,
    public_prefix: Option<String>,
    request_timeout_seconds: Option<u64>,
    /// Checked by the save (a local `asr` row, or empty), not by `apply`.
    voice_transcribe_alias: Option<String>,
    /// `pinned` | `latest`. Set by the save, not by `apply`: it is not part
    /// of the class's container definition.
    catalog_revision: Option<String>,
}

impl AudioSettingsPatch {
    fn apply(self, a: &mut AudioSettings) {
        if let Some(v) = self.image {
            a.image = v;
        }
        if let Some(v) = self.models_dir {
            a.models_dir = v;
        }
        // Trimmed: audio.cpp matches the word as written, so a stray space
        // would stop every inheriting container from starting, and lmgw's
        // CPU predicate would not read it as `cpu` either.
        if let Some(v) = self.backend {
            a.backend = v.trim().to_string();
        }
        if let Some(v) = self.device {
            a.device = v;
        }
        if let Some(v) = self.threads {
            a.threads = v;
        }
        if let Some(v) = self.lazy_load {
            a.lazy_load = v;
        }
        // audiocpp_server refuses a negative value for the three bounds at
        // startup, so a typo here would cost a container that never comes up;
        // they are floored instead, and 0 keeps its meaning (no bound).
        if let Some(v) = self.busy_timeout_ms {
            a.busy_timeout_ms = v.max(0);
        }
        if let Some(v) = self.idle_unload_ms {
            a.idle_unload_ms = v.max(0);
        }
        if let Some(v) = self.min_free_memory_mb {
            a.min_free_memory_mb = v.max(0);
        }
        if let Some(v) = self.max_request_body_mb {
            a.max_request_body_mb = v.max(0);
        }
        if let Some(v) = self.voice_dir {
            a.voice_dir = v.trim().to_string();
        }
        if let Some(v) = self.extra_run_args {
            a.extra_run_args = v;
        }
        if let Some(v) = self.public_prefix {
            a.public_prefix = v.trim_matches('/').to_string();
        }
        if let Some(v) = self.request_timeout_seconds {
            a.request_timeout_seconds = v;
        }
    }
}

/// The image class's patch. Shaped like [`RouterSettingsPatch`] — the four
/// fields are the same four — but kept separate rather than reusing it,
/// because the two types are not the same settings struct and a shared patch
/// would make `image.backend` a compile error away from existing.
#[derive(Deserialize, Default, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ImageSettingsPatch {
    image: Option<String>,
    models_dir: Option<String>,
    extra_run_args: Option<Vec<String>>,
    public_prefix: Option<String>,
    request_timeout_seconds: Option<u64>,
}

impl ImageSettingsPatch {
    fn apply(self, i: &mut ImageSettings) {
        if let Some(v) = self.image {
            i.image = v;
        }
        if let Some(v) = self.models_dir {
            i.models_dir = v;
        }
        if let Some(v) = self.extra_run_args {
            i.extra_run_args = v;
        }
        if let Some(v) = self.public_prefix {
            i.public_prefix = v.trim_matches('/').to_string();
        }
        if let Some(v) = self.request_timeout_seconds {
            i.request_timeout_seconds = v;
        }
    }
}

/// Sparse patch over the whole Settings struct. Secrets: a non-empty value
/// replaces, empty keeps, the matching `clear_*` flag erases.
#[derive(Deserialize, Default, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SettingsFullPatch {
    bind_addr: Option<String>,
    auth_enabled: Option<bool>,
    retention_days: Option<i64>,
    retention_max_rows: Option<i64>,
    jobs_retention_days: Option<i64>,
    jobs_max_rows: Option<i64>,
    /// Keep hourly usage rollups this many months. `0` = forever.
    usage_retention_months: Option<i64>,
    /// The label every amount on the Usage page is shown under. One currency,
    /// no conversion.
    currency: Option<String>,
    /// Alias whose price answers the local/cloud counterfactual.
    /// `""` clears it back to "no reference configured".
    local_reference_alias: Option<String>,
    /// Spend ceiling across every key, in currency micro-units. `0` = none.
    global_budget_micro: Option<i64>,
    global_budget_period: Option<String>,
    max_body_mb: Option<u32>,
    /// Archive an idle Chat thread this many days after its last activity.
    /// `0` disables auto-archive. A folder's own `archive_days` overrides it
    /// for the threads in that folder, and an ongoing conversation's current
    /// thread is never archived.
    chat_archive_days: Option<i64>,
    /// Delete an archived, unpinned Chat thread this many days after it was
    /// archived. `0` keeps archived threads forever. A folder's own
    /// `purge_days` overrides it for the threads in that folder.
    chat_purge_days: Option<i64>,
    /// Keep the Chat change feed's records this many days; `0` keeps every
    /// record. A client away longer resumes with a `resync`.
    chat_feed_retention_days: Option<i64>,
    /// Seconds between the Chat feed's keep-alive comments, at least 1.
    chat_feed_keepalive_s: Option<i64>,
    /// Records the Chat feed reads per query while a client catches up,
    /// from 1 to 10000.
    chat_feed_page_size: Option<i64>,
    /// Live events the Chat feed holds for a slow client before it sends it
    /// a fresh `state` instead, from 1 to 65536.
    chat_feed_live_buffer: Option<i64>,
    /// The system prompt new Chat threads start with. The built-in text
    /// returns to the built-in default (and follows it from then on); `""`
    /// starts new threads with no system prompt.
    chat_system_prompt: Option<String>,
    /// How a text PDF attached in Chat starts out: `text` | `images` | `ask`.
    chat_pdf_mode: Option<String>,
    /// The Chat's speech-to-text alias; `""` = `realtime.asr_alias`.
    chat_stt_alias: Option<String>,
    // The Chat's Voice group (chat-voice design §2.1), checked by
    // `ops::apply_chat_voice` as the self-admin path checks it.
    /// The Chat's text-to-speech alias (task `tts` or `vdes`); `""` =
    /// `realtime.tts_alias`.
    chat_tts_alias: Option<String>,
    /// The Chat's voice, a voice of the Chat's text-to-speech model; `""` =
    /// none named, so realtime's chain decides (`realtime.default_voice`,
    /// then the model's default).
    chat_voice: Option<String>,
    /// What the Chat's voice is told (a speaking style, or a voice-design
    /// description); `""` = `realtime.speech_instructions`.
    chat_speech_style: Option<String>,
    /// The language the user speaks, an ISO 639-1 code the speech-to-text
    /// model is told; `""` = none.
    chat_voice_language: Option<String>,
    /// The language replies are in, an ISO 639-1 code: the model answers in
    /// it and the text-to-speech model speaks it; `""` = the spoken one.
    chat_voice_reply_language: Option<String>,
    /// Read every Chat reply aloud as it streams; a thread can override it.
    chat_read_aloud: Option<bool>,
    /// How voice mode detects the end of a turn: `semantic_vad` |
    /// `server_vad` | `push_to_talk`.
    chat_turn_detection: Option<String>,
    /// `off` | `on`: whether a voice turn goes to the chat model as audio
    /// when the model that answers it takes audio input, wherever it runs
    /// (experimental); a thread can override it.
    chat_voice_audio_input: Option<String>,
    /// Tokens of knowledge-base excerpts one Chat turn may carry; above 0.
    chat_kb_budget_tokens: Option<i64>,
    self_admin: Option<String>,
    sampling_alias: Option<String>,
    responses_max_tool_calls: Option<u32>,
    responses_timeout_seconds: Option<u64>,
    responses_store: Option<bool>,
    responses_retention_hours: Option<i64>,
    responses_max_chains: Option<i64>,
    docs_ingest_reply_tokens: Option<u32>,
    docs_embed_batch: Option<u32>,
    docs_fetch_delay_ms: Option<u64>,
    docs_search: Option<DocsSearchPatch>,
    docs_rerank_model: Option<String>,
    vram: Option<VramSettingsPatch>,
    /// The GPU hold's global fallback. `active` is deliberately not reachable
    /// through this patch: the hold is switched only through the `hold_set`
    /// operation.
    hold: Option<HoldSettingsPatch>,
    /// Podman container name prefix for the per-model container runtime.
    container_prefix: Option<String>,
    /// The DNS suffix a service agent's UI is served under:
    /// `http://<id>.<agent_origin_suffix>:<bind port>/`. Refused when it would
    /// shadow a name the gateway itself answers on, and a **change** stops
    /// every running app container.
    agent_origin_suffix: Option<String>,
    /// The stock image a manifest's `script` step runs in. Empty restores the shipped default rather than leaving a script
    /// step with no image to run in.
    agent_script_image: Option<String>,
    update_check_enabled: Option<bool>,
    update_token: Option<String>,
    clear_update_token: Option<bool>,
    hf_token: Option<String>,
    clear_hf_token: Option<bool>,
    /// Where container builds keep their files. `""`
    /// clears it back to the default; otherwise an absolute path.
    builds_dir: Option<String>,
    /// Forge tokens to set, by host. Secrets like
    /// `hf_token`: a non-empty value replaces that host's token, an empty one
    /// keeps it, and a host not named is untouched.
    #[schemars(transform = lmgw_api_types::openapi_ext::secret)]
    forge_tokens: Option<std::collections::BTreeMap<String, String>>,
    /// Hosts whose forge token to erase.
    clear_forge_tokens: Option<Vec<String>>,
    /// Build update check interval in hours; `0` = off, at most 8760.
    build_update_check_hours: Option<u32>,
    router: Option<RouterSettingsPatch>,
    aux_router: Option<RouterSettingsPatch>,
    audio: Option<AudioSettingsPatch>,
    image: Option<ImageSettingsPatch>,
    /// `GET /v1/realtime`'s settings — the same patch
    /// `settings_set` takes, with the same checks.
    realtime: Option<crate::ops::RealtimeSettingsPatch>,
}

/// VRAM admission control. Every field is optional so one knob moves
/// without restating the rest.
#[derive(Deserialize, Default, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct VramSettingsPatch {
    enabled: Option<bool>,
    headroom_mb: Option<u64>,
    budget_mb: Option<u64>,
    queue_timeout_seconds: Option<u64>,
    load_timeout_seconds: Option<u64>,
    unload_timeout_seconds: Option<u64>,
    /// Fall back when VRAM outside lmgw's control is short.
    fallback_on_external: Option<bool>,
}

impl VramSettingsPatch {
    fn apply(self, v: &mut VramSettings) -> Result<(), String> {
        if let Some(x) = self.enabled {
            v.enabled = x;
        }
        if let Some(x) = self.headroom_mb {
            v.headroom_mb = x;
        }
        if let Some(x) = self.budget_mb {
            v.budget_mb = x;
        }
        if let Some(x) = self.queue_timeout_seconds {
            v.queue_timeout_seconds = x;
        }
        if let Some(x) = self.load_timeout_seconds {
            // 0 here is not "unlimited" but "give up before the load starts",
            // which would refuse every cold model.
            if x == 0 {
                return Err("a load timeout of 0 would refuse every model that is not \
                            already resident"
                    .into());
            }
            v.load_timeout_seconds = x;
        }
        if let Some(x) = self.unload_timeout_seconds {
            if x == 0 {
                return Err(
                    "an unload timeout of 0 would stop waiting before any memory is freed".into(),
                );
            }
            v.unload_timeout_seconds = x;
        }
        if let Some(x) = self.fallback_on_external {
            v.fallback_on_external = x;
        }
        Ok(())
    }
}

/// The GPU hold's global fallback. Carries no `active` field: engaging the
/// hold runs the GPU sweep, a side effect a generic settings patch must not
/// have, so the switch itself is flipped only through the `hold_set`
/// operation. A patch that names `active` is rejected, like any other field
/// this route does not carry.
// Design: gpu-hold §3.1, sweep §5. The rejection is `deny_unknown_fields`.
#[derive(Deserialize, Default, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct HoldSettingsPatch {
    /// `""` clears it back to "refuse"; otherwise validated (must resolve,
    /// must not be local) before it is stored.
    fallback_alias: Option<String>,
}

/// The Docs search stage defaults. Every field is optional so the Docs tab can
/// move one knob without restating the rest.
#[derive(Deserialize, Default, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct DocsSearchPatch {
    k_fts: Option<u32>,
    k_vec: Option<u32>,
    rrf_k: Option<f32>,
    fts_weights: Option<FtsWeightsPatch>,
    k_rerank: Option<u32>,
    limit: Option<u32>,
    budget_tokens: Option<u32>,
    eval_k: Option<u32>,
}

/// The four BM25 column weights, each optional like every other stage default.
#[derive(Deserialize, Default, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct FtsWeightsPatch {
    payload: Option<f32>,
    heading_path: Option<f32>,
    derived_title: Option<f32>,
    derived_summary: Option<f32>,
}

impl DocsSearchPatch {
    fn apply(self, s: &mut crate::config::DocsSearchSettings) -> Result<(), String> {
        // A stage asked for zero candidates is a stage switched off, which is a
        // legitimate thing to measure — except for the two that decide whether
        // an answer exists at all.
        if let Some(v) = self.k_fts {
            s.k_fts = v;
        }
        if let Some(v) = self.k_vec {
            s.k_vec = v;
        }
        if let Some(v) = self.rrf_k {
            if v <= 0.0 {
                return Err("the RRF constant must be positive".into());
            }
            s.rrf_k = v;
        }
        if let Some(w) = self.fts_weights {
            // A negative weight would invert what a column means; zero is a
            // column deliberately switched off, which is a real thing to try.
            for (label, v) in [
                ("payload", w.payload),
                ("heading_path", w.heading_path),
                ("derived_title", w.derived_title),
                ("derived_summary", w.derived_summary),
            ] {
                if v.is_some_and(|v| v < 0.0) {
                    return Err(format!(
                        "the BM25 weight for {label} cannot be negative — 0 switches the column \
                         off, below that inverts it"
                    ));
                }
            }
            let f = &mut s.fts_weights;
            f.payload = w.payload.unwrap_or(f.payload);
            f.heading_path = w.heading_path.unwrap_or(f.heading_path);
            f.derived_title = w.derived_title.unwrap_or(f.derived_title);
            f.derived_summary = w.derived_summary.unwrap_or(f.derived_summary);
        }
        if let Some(v) = self.k_rerank {
            s.k_rerank = v;
        }
        if let Some(v) = self.limit {
            if v == 0 {
                return Err("a result limit of 0 would answer every query with nothing".into());
            }
            s.limit = v;
        }
        if let Some(v) = self.budget_tokens {
            s.budget_tokens = v;
        }
        if let Some(v) = self.eval_k {
            if v == 0 {
                return Err("hit@0 is not a measurement".into());
            }
            s.eval_k = v;
        }
        if s.k_fts == 0 && s.k_vec == 0 {
            return Err("both retrieval stages cannot be off — set k_fts or k_vec above 0".into());
        }
        Ok(())
    }
}

/// The `forge_tokens` half of a save: [`secret`]'s rules per host. Hosts to
/// clear go first, so one save can move a token (clear the old host, set the
/// new). Every host and every supplied token is checked — a token goes into
/// an HTTP header, and whitespace in one would end that header early.
fn forge_tokens(
    current: &mut std::collections::BTreeMap<String, String>,
    supplied: Option<std::collections::BTreeMap<String, String>>,
    clear: Option<Vec<String>>,
) -> Result<(), String> {
    use crate::backends::validate::{validate_forge_host, validate_forge_token};
    for host in clear.unwrap_or_default() {
        current.remove(&validate_forge_host(&host)?);
    }
    for (host, token) in supplied.unwrap_or_default() {
        let host = validate_forge_host(&host)?;
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        validate_forge_token(&host, token)?;
        current.insert(host, token.to_string());
    }
    Ok(())
}

fn secret(current: &mut String, supplied: Option<String>, clear: Option<bool>) {
    if clear.unwrap_or(false) {
        current.clear();
    } else if let Some(v) = supplied.filter(|v| !v.is_empty()) {
        *current = v;
    }
}

/// The dashboard's own settings save — every field, `bind_addr` and the class
/// definitions included.
///
/// Answers `Result<Value, Refusal>` rather than the plane's usual
/// `Result<Value, String>` for the reason the credential ops do (§3.12): two of
/// its refusals carry a code of their own (`origin_suffix_shadows_gateway`,
/// origins §4.1; `dev_production_prefix`, a dev instance asked to take the
/// production container prefix, chat-voice WP11 review m1). Everything else
/// here is still the flat `400 op_failed` every
/// op-level input error is — which is exactly what [`Refusal`]'s `From<String>`
/// renders — so nothing else about this surface moved.
pub async fn settings_set_full(st: &SharedState, p: SettingsFullPatch) -> Result<Value, Refusal> {
    // Held across the whole read-modify-write: see `AppState::settings_write`.
    let guard = st.settings_write.lock().await;
    let mut s = st.snapshot().settings.clone();
    let mut notes: Vec<String> = Vec::new();

    let mut bind_addr_moved = false;
    if let Some(addr) = p.bind_addr {
        let addr = addr.trim().to_string();
        addr.parse::<std::net::SocketAddr>()
            .map_err(|_| format!("'{addr}' is not a valid bind address (host:port)"))?;
        if addr != s.bind_addr {
            notes.push("bind address changes take effect after an app restart".into());
            bind_addr_moved = true;
        }
        s.bind_addr = addr;
    }
    if let Some(v) = p.auth_enabled {
        s.auth_enabled = v;
    }
    if let Some(v) = p.retention_days {
        s.retention_days = v;
    }
    if let Some(v) = p.retention_max_rows {
        s.retention_max_rows = v;
    }
    if let Some(v) = p.jobs_retention_days {
        s.jobs_retention_days = v;
    }
    if let Some(v) = p.jobs_max_rows {
        s.jobs_max_rows = v;
    }
    if let Some(v) = p.usage_retention_months {
        s.usage_retention_months = v;
    }
    if let Some(v) = p.currency {
        let v = v.trim().to_string();
        if v.is_empty() {
            return Err(bad_request(
                "currency cannot be empty — it is the label every amount is shown under",
            ));
        }
        s.currency = v;
    }
    if let Some(v) = p.local_reference_alias {
        let v = v.trim().to_string();
        // Empty is meaningful ("no reference configured" — the counterfactual
        // panel says so rather than quietly picking one, §2.5), so only a
        // non-empty name is checked, and checked now rather than at the first
        // read of the Usage page.
        if !v.is_empty() {
            st.snapshot()
                .resolve(&v)
                .map_err(|e| format!("local_reference_alias '{v}': {e}"))?;
        }
        s.local_reference_alias = v;
    }
    if let Some(v) = p.global_budget_micro {
        s.global_budget_micro = v;
    }
    if let Some(v) = p.global_budget_period {
        s.global_budget_period = BudgetPeriod::parse(&v);
    }
    if let Some(v) = p.max_body_mb {
        s.max_body_mb = v;
    }
    if let Some(v) = p.chat_archive_days {
        s.chat_archive_days = v.max(0);
    }
    if let Some(v) = p.chat_purge_days {
        s.chat_purge_days = v.max(0);
    }
    crate::ops::apply_chat_feed(
        &mut s,
        crate::ops::ChatFeedPatch {
            chat_feed_retention_days: p.chat_feed_retention_days,
            chat_feed_keepalive_s: p.chat_feed_keepalive_s,
            chat_feed_page_size: p.chat_feed_page_size,
            chat_feed_live_buffer: p.chat_feed_live_buffer,
        },
    )?;
    if let Some(v) = p.chat_system_prompt {
        s.set_default_chat_prompt(&v);
    }
    if let Some(v) = p.chat_pdf_mode {
        s.chat_pdf_mode = crate::ops::validate_chat_pdf_mode(&v)?;
    }
    if let Some(v) = p.chat_stt_alias {
        let v = v.trim().to_string();
        if !v.is_empty() {
            crate::ops::validate_stt_alias(st, &v).await?;
        }
        s.chat_stt_alias = v;
    }
    if let Some(v) = p.chat_kb_budget_tokens {
        s.chat_kb_budget_tokens = crate::ops::validate_chat_kb_budget(v)?;
    }
    crate::ops::apply_chat_voice(
        st,
        &mut s,
        crate::ops::ChatVoicePatch {
            chat_tts_alias: p.chat_tts_alias,
            chat_voice: p.chat_voice,
            chat_speech_style: p.chat_speech_style,
            chat_voice_language: p.chat_voice_language,
            chat_voice_reply_language: p.chat_voice_reply_language,
            chat_read_aloud: p.chat_read_aloud,
            chat_turn_detection: p.chat_turn_detection,
            chat_voice_audio_input: p.chat_voice_audio_input,
        },
    )
    .await?;
    if let Some(v) = p.self_admin {
        s.self_admin =
            SelfAdmin::parse(&v).ok_or_else(|| format!("unknown self-admin mode '{v}'"))?;
    }
    if let Some(v) = p.sampling_alias {
        s.sampling_alias = v.trim().to_string();
    }
    if let Some(v) = p.responses_max_tool_calls {
        s.responses_max_tool_calls = v;
    }
    if let Some(v) = p.responses_timeout_seconds {
        if v == 0 {
            return Err(bad_request(
                "responses time limit of 0 would expire before the first turn",
            ));
        }
        s.responses_timeout_seconds = v;
    }
    if let Some(v) = p.responses_store {
        s.responses_store = v;
    }
    if let Some(v) = p.responses_retention_hours {
        s.responses_retention_hours = v;
    }
    if let Some(v) = p.responses_max_chains {
        s.responses_max_chains = v;
    }
    if let Some(v) = p.docs_ingest_reply_tokens {
        if v == 0 {
            return Err(bad_request(
                "an extraction reply budget of 0 leaves no room for an answer",
            ));
        }
        s.docs_ingest_reply_tokens = v;
    }
    if let Some(v) = p.docs_embed_batch {
        if v == 0 {
            return Err(bad_request("an embed batch of 0 would never send anything"));
        }
        s.docs_embed_batch = v;
    }
    if let Some(v) = p.docs_fetch_delay_ms {
        s.docs_fetch_delay_ms = v;
    }
    if let Some(d) = p.docs_search {
        d.apply(&mut s.docs_search)?;
    }
    if let Some(v) = p.docs_rerank_model {
        let v = v.trim().to_string();
        // Empty is meaningful ("use whatever rerank model the aux router has"),
        // so only a non-empty name is checked — and it is checked now rather
        // than at the first query, where it would surface as a skipped stage.
        if !v.is_empty() {
            st.snapshot()
                .resolve(&v)
                .map_err(|e| format!("docs rerank model '{v}': {e}"))?;
        }
        s.docs_rerank_model = v;
    }
    if let Some(v) = p.vram {
        v.apply(&mut s.vram)?;
    }
    if let Some(r) = p.realtime {
        crate::ops::apply_realtime(st, &mut s.realtime, r).await?;
    }
    if let Some(h) = p.hold {
        if let Some(v) = h.fallback_alias {
            let v = v.trim().to_string();
            if !v.is_empty() {
                crate::ops::validate_fallback_alias(&st.snapshot(), &v)
                    .map_err(|e| format!("hold.fallback_alias: {e}"))?;
            }
            s.hold.fallback_alias = (!v.is_empty()).then_some(v);
        }
    }
    if let Some(v) = p.container_prefix {
        let v = validate_container_prefix(&v)?;
        // The boot refusal's rule, on the write as well (chat-voice WP11
        // review m1): a dev instance holds production's model ids, so its
        // next start under the production prefix would `--replace` the
        // installed app's live container of the same name.
        if let Some(why) = crate::config::dev_prefix_refusal(st.dev(), &v) {
            return Err(Refusal {
                status: StatusCode::BAD_REQUEST,
                code: "dev_production_prefix",
                message: why,
            });
        }
        if v != s.container_prefix {
            // Every managed container's *name* and its `lmgw.instance` label
            // are rendered from this prefix (§3.3), and both are fixed at
            // `podman run` time. So a change does not rename anything: the
            // containers that are up keep the old prefix, and boot
            // reconciliation — which filters on `lmgw.instance=<prefix>` —
            // will not see them at all under the new one. Silence here would
            // turn a settings edit into orphaned containers holding VRAM that
            // nothing in lmgw can account for, which is precisely the failure
            // the sibling class-definition notes exist to prevent.
            let running = st.runtime().list().len();
            if running > 0 {
                notes.push(format!(
                    "container name prefix changed while {running} model container(s) are \
                     running — they keep the old prefix until they are restarted, and \
                     reconciliation under the new prefix will not recognise them; stop the \
                     running models (Overview → stop, or lmgw__container action=stop) before \
                     or right after this change"
                ));
            } else {
                notes.push(
                    "container name prefix changed — it applies to the next container each \
                     model starts"
                        .into(),
                );
            }
            s.container_prefix = v;
        }
    }
    let mut origin_suffix_moved = false;
    if let Some(v) = p.agent_origin_suffix {
        let v = validate_origin_suffix(&v)?;
        // The other direction of the manifest-write check (origins §4.1): a
        // suffix that is part of an address this gateway answers on would let
        // an agent named like that address's first label be served at the
        // dashboard's own door. A code of its own, because it is a rule of the
        // design rather than a typo — the shape complaints above are the
        // typos.
        if let Some(why) = crate::agents::service::origin_suffix_refusal(&v, &s.bind_addr) {
            return Err(Refusal {
                status: StatusCode::BAD_REQUEST,
                code: "origin_suffix_shadows_gateway",
                message: why,
            });
        }
        if v != s.agent_origin_suffix {
            origin_suffix_moved = true;
            notes.push(format!(
                "agent UIs are served at http://<id>.{v}:<port>/ from now on"
            ));
            s.agent_origin_suffix = v;
        }
    }
    if let Some(v) = p.agent_script_image {
        let v = v.trim().to_string();
        // Blanking it is "give me the default back", not "run scripts in
        // nothing": a script step with no image could only fail at Start, and
        // the field would show an empty box with no hint of what belongs there.
        let v = if v.is_empty() {
            let d = crate::config::Settings::default().agent_script_image;
            notes.push(format!(
                "agent script image cleared — back to the shipped default {d}"
            ));
            d
        } else {
            // This string goes onto a `podman run` argv as the image operand.
            // A leading `-` would be read as a flag and whitespace would split
            // it into several words, either of which turns a settings typo into
            // an argv nobody wrote. Refused here, where the owner is looking,
            // rather than at the first Start of a script step.
            if v.starts_with('-') {
                return Err(bad_request(format!(
                    "agent_script_image is '{v}'; an image reference cannot start with '-' — \
                     podman would read it as a flag"
                )));
            }
            if v.split_whitespace().count() > 1 {
                return Err(bad_request(format!(
                    "agent_script_image is '{v}'; an image reference is one word — it is passed \
                     to podman as a single argument, not a command line"
                )));
            }
            v
        };
        if v != s.agent_script_image {
            notes.push(format!(
                "agent script image is now {v} — it applies to the next script step that runs"
            ));
            s.agent_script_image = v;
        }
    }
    if let Some(v) = p.update_check_enabled {
        s.update_check_enabled = v;
    }
    secret(&mut s.update_token, p.update_token, p.clear_update_token);
    secret(&mut s.hf_token, p.hf_token, p.clear_hf_token);
    forge_tokens(&mut s.forge_tokens, p.forge_tokens, p.clear_forge_tokens)?;
    if let Some(v) = p.build_update_check_hours {
        s.build_update_check_hours = crate::backends::updates::validate_check_hours(v)?;
    }
    if let Some(v) = p.builds_dir {
        let v = crate::backends::paths::validate_builds_dir(&v)?;
        if v != s.builds_dir {
            s.builds_dir = v;
            let effective = crate::backends::paths::builds_dir(&s, &st.data_dir, st.dev());
            // Saved either way — the owner may be about to mount something
            // there — but said now rather than at the first run's refusal.
            match crate::backends::paths::tmpfs_refusal(&effective) {
                Some(why) => notes.push(why),
                None => notes.push(format!(
                    "builds now use {} — mirrors and logs already under the old directory \
                     stay there",
                    effective.display()
                )),
            }
        }
    }
    // A class definition — image, mounts, run args — is fixed in each
    // container's `podman run` argv, which is rendered fresh at every start.
    // A model already running keeps the argv it was started with until it is
    // restarted, and the note says so rather than implying a hot reload that
    // does not exist.
    if let Some(r) = p.router {
        r.apply(&mut s.router);
        notes.push(
            "chat class definition changed — running chat models keep the previous one until \
             they restart"
                .into(),
        );
    }
    if let Some(r) = p.aux_router {
        r.apply(&mut s.aux_router);
        notes.push(
            "aux class definition changed — running aux models keep the previous one until they \
             restart"
                .into(),
        );
    }
    if let Some(mut a) = p.audio {
        // Not part of the class's container definition: checked here (a
        // speech-to-text model, wherever it runs — changed 2026-10-06, the
        // owner's ruling: its configured fallback is used too), and no
        // restart note when it is all that changed.
        if let Some(v) = a.voice_transcribe_alias.take() {
            let v = v.trim().to_string();
            if !v.is_empty() {
                crate::ops::validate_stt_alias(st, &v)
                    .await
                    .map_err(|e| format!("audio.voice_transcribe_alias: {e}"))?;
            }
            s.audio.voice_transcribe_alias = v;
        }
        // What a catalog download takes: no container reads it, so no
        // restart note either.
        if let Some(v) = a.catalog_revision.take() {
            s.audio.catalog_revision = crate::config::CatalogRevision::parse(&v)?;
        }
        let before = serde_json::to_value(&s.audio).ok();
        a.apply(&mut s.audio);
        if serde_json::to_value(&s.audio).ok() != before {
            notes.push(
                "audio class definition changed — running audio models keep the previous one \
                 until they restart"
                    .into(),
            );
        }
    }
    if let Some(i) = p.image {
        i.apply(&mut s.image);
        notes.push(
            "image class definition changed — running image models keep the previous one until \
             they restart"
                .into(),
        );
        // §3: the LoRA and upscaler directories exist "when the class is first
        // configured". sd-server's capabilities route *throws* when the two
        // flags point at nothing (§12.2), and lmgw renders them
        // unconditionally — so naming the models dir is the moment to make
        // them, rather than leaving the first start to discover the gap. A
        // start creates them too (`render_spec`), which is what covers a
        // directory deleted between two starts; this covers the operator who
        // looks at the tree before ever starting anything.
        let root = s.image.models_dir.clone();
        if !root.trim().is_empty() {
            // A dev instance on a models dir outside its data dir saves the
            // setting, creates nothing there, and says so.
            if let Err(why) = st.refuse_shared_models_dir(std::path::Path::new(&root)) {
                notes.push(format!("the loras/upscalers dirs were not created: {why}"));
            } else {
                for (_, sub) in crate::runtime::image::DEFAULT_DIRS {
                    let dir = std::path::Path::new(&root).join(sub);
                    if let Err(e) = std::fs::create_dir_all(dir) {
                        // Reported, never fatal: an unwritable path is worth
                        // saving the setting for (the owner may be about to
                        // mount it) and worth naming, because the first start
                        // will fail on it otherwise with sd-server's own
                        // filesystem exception instead of this sentence.
                        notes.push(format!("could not create {root}/{sub}: {e}"));
                    }
                }
            }
        }
    }

    store::save_settings(&st.db, &s)
        .await
        .map_err(|e| e.to_string())?;
    // Saved is saved (review G-6): a reload that fails is said beside it,
    // and these settings apply all the same.
    let published = st.settings_saved(&s).await;
    // Published under the lock; the MCP reconcile and the agents' resync
    // and stops below run after it (`settings_saved`). None of them writes
    // the settings blob. The reconcile reads the snapshot of then
    // (`reconcile_mcp`).
    drop(guard);
    st.reconcile_mcp().await;
    if let Some(e) = published.reload_failed {
        notes.push(format!(
            "the rest of the configuration could not be reloaded ({e}); these settings apply \
             now, and the rest at the next reload"
        ));
    }
    if bind_addr_moved {
        // An `agent:<id>` MCP row's url is built from `bind_addr` when the
        // *manifest* is written (container-runtime §3.3), so a bind address
        // that moves afterwards would leave every one of them pointing at a
        // port nothing is listening on — across restarts, because nothing else
        // re-derives it.
        crate::agents::service::resync_all(st).await;
        notes.push("each agent's own MCP registration was re-pointed at the new address".into());
        // A stored `dev_url` was checked against the *old* bind address, and
        // one of the things that check refuses is lmgw's own port — pointing
        // an agent's origin at the gateway makes it proxy the dashboard through
        // itself until the file descriptors run out. Moving the bind
        // address onto a stored dev port arms that loop from the other side, so
        // the question is asked again here (container-runtime §3.4).
        let cleared = crate::agents::service::revalidate_dev_urls(st, &s.bind_addr).await;
        if !cleared.is_empty() {
            notes.push(format!(
                "the dev server override was cleared on {} — {}",
                if cleared.len() == 1 {
                    "one agent".to_string()
                } else {
                    format!("{} agents", cleared.len())
                },
                cleared
                    .iter()
                    .map(|(id, url, why)| format!("'{id}' ({url}): {why}"))
                    .collect::<Vec<_>>()
                    .join("; ")
            ));
        }
    }
    if origin_suffix_moved {
        // Nothing to resync — an origin is computed per request — but every
        // running app container is holding the old one in `LMGW_APP_ORIGIN`
        // (§4.10). The ninth reason a service container stops, and the only
        // one that stops all of them at once.
        let stopped =
            crate::agents::service::stop_all(st, "it is holding an origin the owner has renamed")
                .await;
        if !stopped.is_empty() {
            notes.push(format!(
                "{} app container(s) were stopped because they were started under the old \
                 origin ({}); the next request to an agent's origin starts it again",
                stopped.len(),
                stopped.join(", ")
            ));
        }
    }
    let message = if notes.is_empty() {
        "settings saved".to_string()
    } else {
        format!("settings saved — {}", notes.join("; "))
    };
    Ok(json!({ "ok": true, "message": message }))
}

/// The `agent_origin_suffix` setting, checked as a DNS name (origins §4.1).
///
/// **Shape only** — one or more labels joined by `.`, nothing else. A label is
/// `[a-z0-9-]`, at most 63 characters, and starts and ends with an
/// alphanumeric; a port, a scheme, a path or a `_` is a value that would
/// render an origin no browser could dial. The *shadowing* rule is
/// `service::origin_suffix_refusal`, which answers with its own code.
///
/// Upper case is accepted and lower-cased on the way in, because a host name
/// is case-insensitive and a field that refuses `Lmgw.lan` would be refusing
/// the same value it is about to store.
fn validate_origin_suffix(raw: &str) -> Result<String, String> {
    let v = raw.trim().trim_end_matches('.').to_ascii_lowercase();
    if v.is_empty() {
        return Err(
            "the agent origin suffix cannot be empty — it is the DNS suffix every agent's UI is \
             served under (default 'localhost')"
                .into(),
        );
    }
    if v.contains("://") || v.contains('/') {
        return Err(format!(
            "agent origin suffix '{v}' is a DNS name, not a URL — no scheme and no path, just \
             the suffix (for example 'localhost' or 'lmgw.lan')"
        ));
    }
    if v.contains(':') {
        return Err(format!(
            "agent origin suffix '{v}' carries a port — the port is lmgw's own bind port and is \
             added for you; give the name alone"
        ));
    }
    for label in v.split('.') {
        if label.is_empty() {
            return Err(format!(
                "agent origin suffix '{v}' has an empty label — labels are joined by a single '.'"
            ));
        }
        if label.len() > 63 {
            return Err(format!(
                "agent origin suffix '{v}': the label '{label}' is {} characters and a DNS label \
                 is at most 63",
                label.len()
            ));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(format!(
                "agent origin suffix '{v}': the label '{label}' cannot start or end with '-'"
            ));
        }
        if let Some(bad) = label
            .chars()
            .find(|c| !c.is_ascii_lowercase() && !c.is_ascii_digit() && *c != '-')
        {
            return Err(format!(
                "agent origin suffix '{v}': the label '{label}' contains '{bad}' — a DNS label is \
                 letters, digits and '-'"
            ));
        }
    }
    Ok(v)
}

/// The `container_prefix` setting, checked against what podman accepts in a
/// container name (design §3.3: the prefix is the first component of
/// `<prefix>-<class>-<slug>-<hash6>`, and the `lmgw.instance` label value
/// reconciliation filters on).
///
/// Empty is rejected rather than defaulted: it would render `-chat-…`, which
/// podman refuses outright ("names must match [a-zA-Z0-9][a-zA-Z0-9_.-]*"),
/// and every start on the box would fail with a podman error that says nothing
/// about the settings field that caused it. Same for a stray `/`, a space or
/// an upper-case letter — all of them fail at `podman run`, hours later, on a
/// path where the message is a container-name complaint rather than a settings
/// one. Checked here, once, where the value is entered.
fn validate_container_prefix(raw: &str) -> Result<String, String> {
    let v = raw.trim();
    if v.is_empty() {
        return Err(
            "the container name prefix cannot be empty — it is the first component of \
                    every container's name (default 'lmgw')"
                .into(),
        );
    }
    if v.starts_with('-') || v.ends_with('-') {
        return Err(format!(
            "container name prefix '{v}' cannot start or end with '-'"
        ));
    }
    if let Some(bad) = v
        .chars()
        .find(|c| !c.is_ascii_lowercase() && !c.is_ascii_digit() && *c != '-')
    {
        return Err(format!(
            "container name prefix '{v}' contains '{bad}' — use lower-case letters, digits and \
             '-' only"
        ));
    }
    Ok(v.to_string())
}

// ---------------------------------------------------------------------------
// Keys (principals design §3.12)
// ---------------------------------------------------------------------------

/// An op-level input error in the shape every other op's has: `400 op_failed`,
/// which is exactly what `api::ops_result` produces from a bare `Err(String)`.
/// Here so that moving the credential ops onto [`Refusal`] — which they need,
/// for the codes below — did not change what a bad argument looks like.
fn bad_request(message: impl Into<String>) -> Refusal {
    Refusal {
        status: StatusCode::BAD_REQUEST,
        code: "op_failed",
        message: message.into(),
    }
}

/// `400 refuse_owner` (design §7): the arguments were understood, and the
/// answer is no because the row they name is an owner key. A code of its own
/// rather than `op_failed` because it is a rule of the design, not a typo —
/// and the Keys dialog greys out what it can only after the server has said so
/// at least once.
fn refuse_owner(message: impl Into<String>) -> Refusal {
    Refusal {
        status: StatusCode::BAD_REQUEST,
        code: "refuse_owner",
        message: message.into(),
    }
}

/// The row an id names, refused in `key_set`'s existing words when there is
/// none. Read from the snapshot rather than the database: it already holds
/// every `api_keys` row, and every writer here reloads it.
fn key_by_id(st: &SharedState, id: i64) -> Result<ApiKey, Refusal> {
    st.snapshot()
        .api_keys
        .iter()
        .find(|k| k.id == id)
        .cloned()
        .ok_or_else(|| {
            bad_request(format!(
                "no key with id {id} — pass the id the Keys table (or lmgw__usage) shows"
            ))
        })
}

/// The five credential ops of the `/api` plane, dispatched off `api::op` the
/// way the agent catalog's ops are (§3.12).
///
/// They answer `Result<Value, Refusal>` rather than the plane's usual
/// `Result<Value, String>` because an owner refusal carries its own code.
///
/// None of them is on the `lmgw__*` self-admin plane. That plane has never
/// exposed a credential — there is no `lmgw__agent_token_get` either, only the
/// `/api` op — and `key_reveal` / `key_rotate` hand out the key that holds
/// every capability. A model-driven client with the self-admin tools is
/// exactly who must not be able to ask for it.
pub(super) async fn key_op(
    st: &SharedState,
    name: &str,
    args: Map<String, Value>,
) -> Result<Value, Refusal> {
    let id = |args: &Map<String, Value>| {
        args.get("id")
            .and_then(Value::as_i64)
            .ok_or_else(|| bad_request("pass id"))
    };
    match name {
        "key_create" => {
            let name = args
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let kind = args
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("client")
                .to_string();
            // A device is created with its whole policy and its pairing link
            // (client-apps design §1.4), not with the scope alone.
            if kind.trim() == "device" {
                return device_keys::create(st, name.trim(), &args).await;
            }
            // The admin-tools level is a paired device's (client-apps
            // design L3/L5): asked of any other kind, said, not dropped.
            if args
                .get("self_admin")
                .is_some_and(|v| !v.is_null() && v.as_str().map(str::trim) != Some("off"))
            {
                return Err(bad_request(
                    "self_admin is a paired device's level (kind 'device'); an owner key \
                     uses lmgw's admin tools as the owner, and a client key never",
                ));
            }
            let scope = KeyPatch {
                scope_mode: str_arg(&args, "scope_mode"),
                scope_patterns: str_arg(&args, "scope_patterns"),
                tool_scope_mode: str_arg(&args, "tool_scope_mode"),
                tool_scope_patterns: str_arg(&args, "tool_scope_patterns"),
                ..Default::default()
            };
            key_create(st, &name, &kind, scope).await
        }
        "key_set" => {
            let p: KeyPatch = crate::ops::patch_from_args(Some(args)).map_err(bad_request)?;
            key_set(st, p).await
        }
        "key_delete" => key_delete(st, id(&args)?).await,
        "key_reveal" => key_reveal(st, id(&args)?).await,
        "key_rotate" => {
            let id = id(&args)?;
            // A device rotates by re-pairing: a new path, because its row
            // keeps no plaintext to replace (L1).
            match key_by_id(st, id)? {
                key if key.kind == ApiKeyKind::Device => device_keys::rotate(st, &key, &args).await,
                _ => key_rotate(st, id).await,
            }
        }
        other => Err(bad_request(format!("unknown op '{other}'"))),
    }
}

fn str_arg(args: &Map<String, Value>, key: &str) -> Option<String> {
    args.get(key).and_then(Value::as_str).map(str::to_string)
}

/// `key_create { name, kind, scope_mode?, scope_patterns?, tool_scope_mode?,
/// tool_scope_patterns? }` — a client key or an owner key (§3.12).
///
/// The four scope fields are `key_set`'s, with `key_set`'s validation and
/// normalisation; `scope` carries only those (its `id` is unused). An owner
/// key refuses a scope that narrows anything, as it does on `key_set`.
///
/// `kind` absent means `client`, which is what every caller predating the
/// owner principal sends. The two spellings of that kind are both taken: the
/// dashboard's word is `client`, the `api_keys.kind` column's is `key`, and
/// refusing one of them would only ever be a trap.
pub async fn key_create(
    st: &SharedState,
    name: &str,
    kind: &str,
    scope: KeyPatch,
) -> Result<Value, Refusal> {
    let name = name.trim();
    if name.is_empty() {
        return Err(bad_request("key name is required"));
    }
    let owner = match kind.trim() {
        "" | "client" | "key" => false,
        "owner" => true,
        other => {
            return Err(bad_request(format!(
                "unknown key kind '{other}' — 'client', 'owner' or 'device'"
            )))
        }
    };
    let scoped = crate::ops::validate_create_scope(&scope).map_err(bad_request)?;
    if owner && scoped {
        return Err(refuse_owner(crate::ops::REFUSE_OWNER_POLICY));
    }
    // An owner key names itself: the prefix is the server's to write, and it
    // is what `kind = owner` is read back from in every list. A name that
    // already carries it is accepted rather than doubled.
    let name = if owner {
        format!(
            "owner:{}",
            name.strip_prefix("owner:").unwrap_or(name).trim()
        )
    } else {
        name.to_string()
    };
    if name == "owner:" {
        return Err(bad_request("key name is required"));
    }
    // `api_keys.name` is UNIQUE, so this is also enforced by the database —
    // but the owner path writes through an UPSERT (rotation's primitive), and
    // an UPSERT on a taken name would silently rewrite that row's credential
    // instead of refusing. Checked here, once, for both kinds.
    if st.snapshot().api_keys.iter().any(|k| k.name == name) {
        return Err(bad_request(format!(
            "a key named '{name}' already exists — pick another name"
        )));
    }

    if owner {
        let plaintext = token::mint_owner();
        let id = token::set_owner_key(st, &name, &plaintext, true)
            .await
            .map_err(bad_request)?;
        return Ok(json!({
            "ok": true,
            "id": id,
            "name": name,
            // Stored, unlike a client key's: an owner key is handed back by
            // `key_reveal` for as long as the row exists (§3.1).
            "plaintext": plaintext,
            "message": format!(
                "owner key '{name}' created — it holds every capability, and stays copyable on \
                 Usage → Keys"
            ),
        }));
    }

    let plaintext = format!("lmgw-{}", super::rand_hex32());
    let id = store::insert_api_key(&st.db, &name, &crate::config::hash_api_key(&plaintext))
        .await
        .map_err(|e| bad_request(e.to_string()))?;
    st.reload_snapshot()
        .await
        .map_err(|e| bad_request(e.to_string()))?;
    if scoped {
        // Validated above, so this can only fail on the database; the key
        // then exists unscoped, and the error says so rather than hiding it.
        let patch = KeyPatch { id, ..scope };
        if let Err(e) = crate::ops::key_set(st, patch).await {
            return Err(bad_request(format!(
                "key '{name}' (id {id}) was created but its scope was not saved: {e} — set it \
                 with key_set before using the key"
            )));
        }
    }
    Ok(json!({
        "ok": true,
        "id": id,
        "name": name,
        // Shown exactly once; only the hash is stored.
        "plaintext": plaintext,
        "message": format!("key '{name}' created — copy it now, it is not shown again"),
    }))
}

/// `key_set` with the owner rules in front of it (§3.12).
///
/// The rules themselves live in `ops::owner_key_refusal`, which `ops::key_set`
/// also calls; this is where the refusal gets its status and its code.
pub async fn key_set(st: &SharedState, p: KeyPatch) -> Result<Value, Refusal> {
    if let Some(key) = st.snapshot().api_keys.iter().find(|k| k.id == p.id) {
        if let Some(message) = crate::ops::owner_key_refusal(key, &p) {
            return Err(refuse_owner(message));
        }
    }
    crate::ops::key_set(st, p).await.map_err(bad_request)
}

/// `key_delete { id }` — every row but the door (§3.12).
///
/// Deleting `owner:self-admin` is allowed and means what §3.7 means by
/// "closed": the next start seeds it again, **disabled**, so `/mcp/admin`
/// answers `401` naming the row until the owner enables it. The credential
/// that was handed out is gone in the meantime, which is the point of
/// reaching for delete rather than for the toggle.
pub async fn key_delete(st: &SharedState, id: i64) -> Result<Value, Refusal> {
    let key = key_by_id(st, id)?;
    if key.kind == ApiKeyKind::Owner && key.name == token::OWNER_DASHBOARD {
        return Err(refuse_owner(crate::ops::REFUSE_DOOR));
    }
    store::delete_api_key(&st.db, id)
        .await
        .map_err(|e| bad_request(e.to_string()))?;
    // The row is gone: what it opened ends now, whether or not the reload
    // succeeds (review W2-6).
    let reloaded = st.reload_snapshot().await;
    if reloaded.is_err() {
        st.key_written(key.id, crate::state::KeyWritten::Deleted);
    }
    device_keys::deleted(st, &key);
    reloaded.map_err(|e| bad_request(e.to_string()))?;
    Ok(json!({ "ok": true, "message": "key revoked" }))
}

/// The stored plaintext of one owner row, or why there is none to hand back.
///
/// A client key is hashed and shown once, so there is nothing to reveal; an
/// agent token has one of its own on the agent's page. Neither is a rule about
/// who is asking — the caller already holds `Admin` — only about what exists.
fn owner_plaintext(key: &ApiKey) -> Result<String, Refusal> {
    match key.kind {
        ApiKeyKind::Owner => key
            .key_plain
            .as_ref()
            .map(|s| s.expose().to_string())
            .ok_or_else(|| {
                bad_request(format!(
                    "owner key '{}' has no stored plaintext — rotate it to mint one",
                    key.name
                ))
            }),
        ApiKeyKind::Agent => Err(bad_request(format!(
            "'{}' is an agent token: copy or rotate it on the agent's own page",
            key.name
        ))),
        ApiKeyKind::Internal => Err(bad_request(format!(
            "'{}' is an internal identity, not a credential — there is nothing to reveal",
            key.name
        ))),
        ApiKeyKind::Key => Err(bad_request(format!(
            "'{}' is a client key: only its hash is stored, so it cannot be shown again — \
             create a new one",
            key.name
        ))),
        ApiKeyKind::Device => Err(device_keys::reveal_refusal(key)),
    }
}

/// `key_reveal { id }` → `{ key }` (§3.12).
///
/// No origin list of its own: a cookie-authenticated caller is same-origin by
/// the gate (§3.6) and a bearer caller already holds the key it is asking for.
pub async fn key_reveal(st: &SharedState, id: i64) -> Result<Value, Refusal> {
    let key = key_by_id(st, id)?;
    let plaintext = owner_plaintext(&key)?;
    Ok(json!({
        "ok": true,
        "id": key.id,
        "name": key.name,
        "key": plaintext,
        "message": format!("'{}' copied", key.name),
    }))
}

/// `key_rotate { id }` → `{ key }` (§3.12).
///
/// One write for the hash and the plaintext (`token::set_owner_key`), so the
/// old value is dead the moment the new one is live and the two can never
/// disagree. The row's enabled flag is carried across: rotating a disabled
/// `owner:self-admin` re-mints it and leaves the plane closed.
pub async fn key_rotate(st: &SharedState, id: i64) -> Result<Value, Refusal> {
    let key = key_by_id(st, id)?;
    // Same three refusals as `key_reveal`, for the same reason — an agent
    // token rotates with `agent_token_rotate`, and a client key has no
    // plaintext to replace, only a row to delete and re-create.
    owner_plaintext(&key)?;
    let plaintext = token::mint_owner();
    token::write_owner_key(st, &key.name, &plaintext, key.enabled)
        .await
        .map_err(bad_request)?;
    // What the old value opened ends now, as a device's does (§1.6) —
    // whether or not the reload succeeds (review W2-6, W3-7): the new value
    // is written, and the old one must not keep working.
    let reloaded = st.reload_snapshot().await;
    if reloaded.is_err() {
        st.key_written(
            key.id,
            crate::state::KeyWritten::Rehashed {
                hash: crate::config::hash_api_key(&plaintext),
                plain: Some(plaintext.clone()),
            },
        );
    }
    crate::devices::revoke(st, key.id, crate::devices::RevokeReason::Rotated);
    let mut message = if key.name == token::OWNER_DASHBOARD {
        "the dashboard key is rotated — every other open tab lands on the login view".to_string()
    } else {
        format!("'{}' is rotated — the old value no longer works", key.name)
    };
    // The new value goes back either way: it is written, and a failed
    // reload is said beside it.
    if let Err(e) = reloaded {
        message.push_str(&format!(
            " (the key is written, but the configuration could not be reloaded: {e} — it takes \
             effect at the next reload)"
        ));
    }
    Ok(json!({
        "ok": true,
        "id": key.id,
        "name": key.name,
        "key": plaintext,
        "message": message,
    }))
}

pub async fn update_check(st: &SharedState) -> Result<Value, String> {
    let current = crate::update::current_version();
    match crate::update::check(st, current).await {
        Ok(Some(info)) => Ok(json!({
            "ok": true,
            "message": format!(
                "update available: {} (you have {}) — install it from the tray menu",
                info.manifest.version, info.current
            ),
        })),
        Ok(None) => Ok(json!({ "ok": true, "message": format!("up to date ({current})") })),
        Err(e) => Err(format!("update check failed: {e}")),
    }
}
