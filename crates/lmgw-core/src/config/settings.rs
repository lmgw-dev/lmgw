//! Gateway `Settings`: the persisted JSON blob, its top-level defaults, and
//! the self-admin gate.

use serde::{Deserialize, Serialize};

use super::settings_classes::{default_agent_origin_suffix, default_agent_script_image};
use super::*;

mod dev_models_dir;
pub use dev_models_dir::*;

/// Gate for the built-in **self-admin tools** (`lmgw__*`) that the northbound
/// `/mcp` endpoint exposes alongside the aggregated southbound catalog (§20).
///
/// An agent that can rewrite upstreams, aliases and container settings is a
/// large lever, and `/mcp` is reachable by anything that can reach the gateway
/// — so mutations take a deliberate opt-in here. This is a *visible* switch on
/// the Settings page, not a hidden refusal: at `ReadOnly` the mutation tools
/// are absent from `tools/list` **and** rejected on call, with an error that
/// names this setting.
///
/// Deliberately **not** settable through the self-admin tools themselves — an
/// agent that can widen its own gate has no gate (see
/// [`crate::ops::settings_set`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SelfAdmin {
    /// No `lmgw__*` tool is listed or callable.
    Off,
    /// Read-only tools only (status, models, upstreams, logs, settings).
    #[default]
    ReadOnly,
    /// Read **and** mutation tools (CRUD + container control).
    Full,
}

impl SelfAdmin {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::ReadOnly => "read_only",
            Self::Full => "full",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "off" => Some(Self::Off),
            "read_only" => Some(Self::ReadOnly),
            "full" => Some(Self::Full),
            _ => None,
        }
    }

    /// Whether the read-only tool set is exposed.
    pub fn allows_read(&self) -> bool {
        !matches!(self, Self::Off)
    }

    /// Whether the mutating tool set is exposed.
    pub fn allows_write(&self) -> bool {
        matches!(self, Self::Full)
    }
}

fn default_bind_addr() -> String {
    "127.0.0.1:8787".into()
}
/// The label every amount is shown under. One currency, no conversion
/// (usage-analytics §9): catalog prices arrive in USD, so that is the default,
/// and an owner who types EUR into a price sheet is asserting those numbers are
/// euros — nothing converts them.
fn default_currency() -> String {
    "USD".into()
}
fn default_retention_days() -> i64 {
    30
}
fn default_retention_max_rows() -> i64 {
    200_000
}
fn default_true() -> bool {
    true
}
/// Two weeks of job history: long enough to see what ran overnight and why it
/// failed, short enough that the table stays small on a desktop gateway.
fn default_jobs_retention_days() -> i64 {
    14
}
fn default_jobs_max_rows() -> i64 {
    500
}

/// Gateway settings, persisted as one JSON blob in the `settings` table.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub bind_addr: String,
    pub auth_enabled: bool,
    pub retention_days: i64,
    pub retention_max_rows: i64,
    /// Keep finished background-job rows (§9c) this many days. `0` keeps them
    /// forever. Same shape as the request-log rules above, and for the same
    /// reason: a growing table needs a bound the owner can see and change.
    #[serde(default = "default_jobs_retention_days")]
    pub jobs_retention_days: i64,
    /// Keep at most this many finished job rows, newest first. `0` is
    /// unlimited. Running jobs are never trimmed by either rule.
    #[serde(default = "default_jobs_max_rows")]
    pub jobs_max_rows: i64,
    /// Keep hourly usage rollups this many months. **`0` = forever**, and that
    /// is the default: a busy desktop gateway writes a few hundred rollup rows
    /// a day, so ten years of them costs less than one day of raw request logs
    /// — and unlike the raw rows they are the only copy of that history
    /// (usage-analytics §3.3).
    #[serde(default)]
    pub usage_retention_months: i64,
    #[serde(default = "default_currency")]
    pub currency: String,
    /// Alias whose price answers "what would the locally-served tokens have
    /// cost?" (§2.5). Empty = the panel says no reference is configured rather
    /// than quietly picking one.
    #[serde(default)]
    pub local_reference_alias: String,
    /// Spend ceiling across every key, in currency micro-units. `0` = none.
    /// The owner's real question is "what is this month costing me", which is
    /// not a per-key question.
    #[serde(default)]
    pub global_budget_micro: i64,
    #[serde(default)]
    pub global_budget_period: BudgetPeriod,
    pub router: RouterSettings,
    /// The aux class: the small stateless model kinds — embeddings and
    /// rerankers (§8). A separate class rather than a separate container:
    /// every model runs in its own container now, and the class only carries
    /// the image/models_dir/prefix its members inherit.
    ///
    /// Was `embed_router` before rerankers moved in beside the embedders; the
    /// alias reads blobs written under the old key. `migrations/0018` rewrites
    /// the stored key once, so the alias only has to catch a settings JSON
    /// that arrived some other way (hand-edited, restored from a backup).
    #[serde(alias = "embed_router")]
    pub aux_router: RouterSettings,
    /// The audio class: audio.cpp `audiocpp_server` models (TTS/ASR/music/…).
    pub audio: AudioSettings,
    /// The image class: stable-diffusion.cpp `sd-server` models (image
    /// generation, and video through the same server later).
    #[serde(default)]
    pub image: ImageSettings,
    /// `GET /v1/realtime`, spoken conversations over a WebSocket (realtime
    /// design §12).
    #[serde(default)]
    pub realtime: RealtimeSettings,
    /// Hugging Face access token for gated/private repos (empty = anonymous).
    pub hf_token: String,
    /// Poll the release feed for new app versions in the background (§12).
    /// The Tauri shell prompts to install when a newer RPM is published.
    #[serde(default = "default_true")]
    pub update_check_enabled: bool,
    /// Optional token for a private update feed (sent as `PRIVATE-TOKEN`, as a
    /// GitLab package registry expects). Empty = the build's own deploy token
    /// if it has one, else anonymous, which is all the public GitHub feed
    /// needs. `LMGW_REGISTRY_TOKEN` overrides it for headless runs.
    pub update_token: String,
    /// Default model alias used to answer MCP `sampling/createMessage` when a
    /// server has no per-server `sampling_alias` (§8). Empty = none configured.
    pub sampling_alias: String,
    /// Exposure of the built-in `lmgw__*` self-admin tools (§20).
    /// Defaults to [`SelfAdmin::ReadOnly`] — inspection without mutation.
    ///
    /// This is the *capability* gate and it applies everywhere, including the
    /// in-process Admin Chat. Reachability is now the `owner:self-admin` key's
    /// `enabled` flag (principals §3.7), not a second field here.
    pub self_admin: SelfAdmin,
    /// **Migration residue, read once and blanked** (principals §6).
    ///
    /// This used to be the bearer `/mcp/admin` compared against, with "empty"
    /// meaning "the route is closed". It is now an `api_keys` row:
    /// `agents::token::seed_owner_keys` carries a non-blank value over as the
    /// plaintext of `owner:self-admin` on the first start that sees it, blanks
    /// this field, and thereafter the row's `enabled` flag carries what
    /// "closed" meant. No surface writes it any more — not Settings, not
    /// `settings_set` — and the field itself goes after one release, once
    /// every install has taken that start.
    #[serde(default)]
    pub self_admin_token: String,
    /// Ceiling on server-side tool calls in one `/v1/responses` run (§21).
    ///
    /// A tool loop that cannot terminate is a runaway GPU, so a bound has to
    /// exist — this is that bound, made **visible and editable** rather than
    /// compiled in. A request's own `max_tool_calls` may lower it but not raise
    /// it, and hitting either ends the response as `status: "incomplete"` with
    /// `incomplete_details.reason = "max_tool_calls"`, never as a silently
    /// truncated answer.
    #[serde(default = "default_responses_max_tool_calls")]
    pub responses_max_tool_calls: u32,
    /// Wall-clock ceiling for one `/v1/responses` run, in seconds (§21).
    /// Bounds the whole loop, where the per-upstream `timeout_ms` bounds only a
    /// single turn. Generous by default: a local model doing real tool work is
    /// legitimately slow, and this exists to stop runaways, not to hurry work.
    #[serde(default = "default_responses_timeout_seconds")]
    pub responses_timeout_seconds: u64,
    /// Persist responses so `previous_response_id` works (§21 stage 2).
    ///
    /// On by default, matching the API's own `"store": true` default. Turning it
    /// off makes the gateway report `"store": false` on every response and
    /// refuse `previous_response_id` with that reason — the stage-1 behavior,
    /// available deliberately rather than by accident.
    #[serde(default = "default_true")]
    pub responses_store: bool,
    /// Evict a stored conversation this long after its **last** activity, in
    /// hours. `0` keeps everything until deleted by hand.
    ///
    /// Chain-aware: the unit of eviction is the whole `previous_response_id`
    /// chain, timed from its newest response. Ageing out individual responses
    /// would delete each chain's root first — the oldest row — and orphan a
    /// conversation still being extended.
    #[serde(default = "default_responses_retention_hours")]
    pub responses_retention_hours: i64,
    /// Keep at most this many stored conversations, evicting the least recently
    /// active first. `0` is unlimited.
    #[serde(default = "default_responses_max_chains")]
    pub responses_max_chains: i64,
    /// Largest request body accepted on the JSON `/v1` routes, in MiB. `0` is
    /// unlimited.
    ///
    /// Exists because axum applies a 2 MiB `DefaultBodyLimit` whether or not
    /// anybody asked for one, and a body over it was rejected with nothing
    /// naming the cap — a base64 image a shade over 2 MiB just failed. A limit
    /// that cannot be seen or changed is the exact thing this gateway does not
    /// do, so it is a setting, its 413 names it, and it can be turned off.
    ///
    /// The `/v1/audio/*` and `/v1/tasks/*` routes are not bounded by it: their
    /// payloads are whole audio files and they keep the explicit
    /// `DefaultBodyLimit::disable()` they have always had.
    #[serde(default = "default_max_body_mb")]
    pub max_body_mb: u32,
    /// Archive a Chat thread this many days after its last activity
    /// (`updated_at` — a new message or a settings change; pinning does not
    /// touch it) (chat-archive-pin-attachments design §1). `0` disables
    /// auto-archive. Reversible: archiving only sets `archived_at`, and a
    /// pin, a restore, or sending into the thread clears it again.
    #[serde(default = "default_chat_archive_days")]
    pub chat_archive_days: i64,
    /// Delete an archived, unpinned Chat thread this many days after it was
    /// archived (design §1). `0` keeps archived threads forever. The clock is
    /// `archived_at`, not `updated_at`, so a thread archived by hand gets the
    /// full period too.
    #[serde(default = "default_chat_purge_days")]
    pub chat_purge_days: i64,
    /// How a text PDF attached in Chat starts out (chat-complete design §8,
    /// §11): `text` (the extracted text), `images` (every page as an image,
    /// needs vision) or `ask` (the chip starts unset and Send waits for a
    /// choice). One of [`CHAT_PDF_MODES`].
    #[serde(default = "default_chat_pdf_mode")]
    pub chat_pdf_mode: String,
    /// The speech-to-text alias that turns a Chat audio attachment into a
    /// transcript when the thread's model takes no audio itself. Empty = none.
    /// When set it must name an alias whose capability `task` is `asr`.
    #[serde(default)]
    pub chat_stt_alias: String,
    /// The text-to-speech alias the Chat speaks with — read-aloud and
    /// realtime mode (chat-voice design §2.1). Empty = `realtime.tts_alias`.
    /// When set it must name an alias whose capability `task` is `tts` or
    /// `vdes`. A thread can override it.
    #[serde(default)]
    pub chat_tts_alias: String,
    /// The Chat's voice, a voice of its TTS model (`chat_tts_alias`, else
    /// `realtime.tts_alias`). Empty = none named: realtime's own chain
    /// decides (`realtime.default_voice`, then the row's default preset —
    /// realtime design §5.3). A thread can override it; one that speaks with
    /// another model does not take it (chat-voice design §2.3).
    #[serde(default)]
    pub chat_voice: String,
    /// The speech instructions the Chat's TTS gets, as a realtime session's
    /// `session.lmgw.speech_instructions`. Empty =
    /// `realtime.speech_instructions`. A thread can override it, `""`
    /// included (none for that thread).
    #[serde(default)]
    pub chat_speech_style: String,
    /// The language the user speaks, an ISO 639-1 code: what the
    /// speech-to-text model is told where it takes a language, and what the
    /// prompt states the user speaks (chat-voice design §2.1, split
    /// 2026-10-05). Empty = none: the ASR detects. A thread can override it.
    #[serde(default)]
    pub chat_voice_language: String,
    /// The language replies are in, an ISO 639-1 code: what the model is
    /// asked to answer in and the text-to-speech model speaks where it takes
    /// a language (chat-voice design §2.1, added 2026-10-05). Empty =
    /// [`Self::chat_voice_language`], resolved; with neither, the reply
    /// follows the user. A thread can override it.
    #[serde(default)]
    pub chat_voice_reply_language: String,
    /// Read every Chat reply aloud as it streams. A thread can override it.
    #[serde(default)]
    pub chat_read_aloud: bool,
    /// How realtime mode in the Chat detects a turn: `semantic_vad` (Smart
    /// Turn), `server_vad` (silence only) or `push_to_talk`. A thread can
    /// override it. One of [`crate::store::TurnDetection::NAMES`].
    #[serde(default = "default_chat_turn_detection")]
    pub chat_turn_detection: String,
    /// Whether a voice turn goes to the chat model as audio
    /// (voice-audio-input design §2.1): `off` (the model reads the
    /// transcript, as before) or `local` — a local model lmgw runs whose
    /// input modalities include audio hears the turn, the ASR still
    /// transcribes it. Experimental; a cloud chat model never gets audio (a
    /// cloud speech-to-text model still transcribes each turn's audio, as
    /// with `off`). A thread can override it. One of
    /// [`crate::store::AudioInputMode::NAMES`].
    #[serde(default = "default_chat_voice_audio_input")]
    pub chat_voice_audio_input: String,
    /// Tokens of knowledge-base excerpts one Chat turn may carry (design §9.3,
    /// §11); a thread can override it. Always above zero: `0` would attach a
    /// knowledge base and then never let it say anything.
    #[serde(default = "default_chat_kb_budget_tokens")]
    pub chat_kb_budget_tokens: u32,
    /// The system prompt a new Chat thread starts with, when the owner wrote
    /// their own; `None` = the built-in one
    /// ([`BUILTIN_CHAT_SYSTEM_PROMPT`](super::BUILTIN_CHAT_SYSTEM_PROMPT)),
    /// never stored so that it can improve. `Some("")` = no prompt at all.
    /// Read through [`Settings::default_chat_prompt`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_system_prompt: Option<String>,
    /// Room reserved for one extraction reply, in tokens (quickdoc §8).
    ///
    /// Ingestion sizes each extraction window as `ingest model's real context −
    /// prompt − this`, so it is the one number in that sum that is a *choice*
    /// rather than a measurement — which is exactly why it is a setting. Raise
    /// it for documents whose sections are many and small; the windows shrink
    /// to match, visibly.
    #[serde(default = "default_docs_ingest_reply_tokens")]
    pub docs_ingest_reply_tokens: u32,
    /// Texts per embedding call during ingest and re-embed. Bigger batches are
    /// faster; how big a batch the aux router accepts depends on its
    /// `ubatch-size`, so this is the knob that matches the two.
    #[serde(default = "default_docs_embed_batch")]
    pub docs_embed_batch: u32,
    /// Pause between two fetches to the same host while ingesting, in
    /// milliseconds. `0` disables the pause. Documentation hosts are somebody
    /// else's servers; this is the politeness dial and it is visible.
    #[serde(default = "default_docs_fetch_delay_ms")]
    pub docs_fetch_delay_ms: u64,
    /// The §6 retrieval stage defaults every `docs__query` and debug search
    /// starts from. A caller may override any of them per request; only the
    /// owner changes what "default" means.
    #[serde(default)]
    pub docs_search: DocsSearchSettings,
    /// Model alias the rerank stage calls (quickdoc §6, §9a). Empty means
    /// "whichever rerank model the aux router has enabled"; when there is none,
    /// the stage is skipped and the search trace says why.
    #[serde(default)]
    pub docs_rerank_model: String,
    /// VRAM admission control (quickdoc §9b).
    #[serde(default)]
    pub vram: VramSettings,
    /// The manual GPU hold (gpu-hold design §1/§3.1) — orthogonal to
    /// `vram.enabled`: hold is a switch on the resolve and start paths, not a
    /// capacity policy, so `enabled: false` (no NVML, admission inactive) and
    /// `hold.active: true` are not a contradiction and both work
    /// independently. `active` is written only by `ops::hold_set` — never by
    /// this settings blob's own generic patch paths — because engaging it
    /// runs the hold sweep (§5), a side effect a plain settings save must not
    /// grow.
    #[serde(default)]
    pub hold: HoldSettings,
    /// Podman container name prefix for the per-model container runtime
    /// (per-model-containers design §3.3): `<container_prefix>-<class>-
    /// <slug>-<hash6>`. Global rather than per-class — a dev instance sets
    /// its own prefix, which is what removes the dev/prod container-name
    /// collision class entirely, not a per-class knob.
    #[serde(default = "default_container_prefix")]
    pub container_prefix: String,
    /// The DNS suffix every service agent's UI is served under (principals and
    /// origins design §4.1): `http://<id>.<agent_origin_suffix>:<bind port>/`,
    /// the id itself as the label. Default `localhost`, which browsers and
    /// systemd-resolved answer for on this machine without a hosts entry.
    ///
    /// The one reason to change it is a gateway bound to a real address and
    /// opened from another box (§4.10): point a zone with a wildcard record
    /// (`*.lmgw.lan`) at this host and set it here. Refused on write when it
    /// would shadow a name the gateway itself answers on.
    #[serde(default = "default_agent_origin_suffix")]
    pub agent_origin_suffix: String,
    /// The stock image a manifest's `script` step runs in (container-runtime
    /// §4.2). Visible and editable rather than compiled in: a script is sugar
    /// over the container runtime, and which Node the sugar dissolves in is the
    /// owner's choice — an air-gapped box mirrors it, a pinned digest is legal
    /// here too.
    ///
    /// lmgw runs it as `node /lmgw/shim.mjs` with `--pull=missing`, so the
    /// first script run on a box downloads it once. Everything else — the
    /// limits, cancel, logging, the ledger — is the container runtime's,
    /// unchanged.
    #[serde(default = "default_agent_script_image")]
    pub agent_script_image: String,
    /// Where container builds keep their git mirrors, per-run worktrees and
    /// logs (container-builds §5). `None` = `<data_dir>/builds`, or on a dev
    /// instance `~/.cache/lmgw-dev/builds` — resolved by
    /// [`crate::backends::paths::builds_dir`], never read raw. A build refuses
    /// to run when the resolved directory is on tmpfs (§10): a worktree per
    /// run is gigabytes, and `/tmp` is RAM.
    #[serde(default)]
    pub builds_dir: Option<String>,
    /// Forge API tokens by exact host — `github.com`, `git.example.com`
    /// (container-builds §7). A token goes only to its own host: in API calls,
    /// and as a git `http.<url>.extraHeader` passed through the environment,
    /// never argv or a log. Redacted like `hf_token` on every read (one
    /// `<set>` per host), and like it not writable through the self-admin
    /// tools.
    #[serde(default)]
    pub forge_tokens: std::collections::BTreeMap<String, String>,
    /// How often the build update check runs, in hours (container-builds §8).
    /// **`0` = off**; **Check now** still works.
    #[serde(default = "default_build_update_check_hours")]
    pub build_update_check_hours: u32,
    /// The shared router-mode container names a pre-upgrade install had
    /// configured, captured by the settings-shape migration (§6) so the boot
    /// sweep (§3.4) can still find and remove them after the fields that held
    /// them are gone.
    ///
    /// Written exactly once, by [`crate::store::load_settings`], on the first
    /// load of a blob that still carries `router.container_name` /
    /// `aux_router.container_name` / `audio.container_name`; the old keys are
    /// dropped by the next save, which is also the save that persists this
    /// list. [`crate::runtime::lifecycle::boot`] consumes it and clears it
    /// once the sweep has actually looked at every name, so an install that
    /// booted without podman keeps its list for the next try instead of
    /// forgetting names it never swept.
    ///
    /// Empty on every install that never ran router mode — including a fresh
    /// one, which has no settings row for the migration to read at all.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub legacy_container_names: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            bind_addr: default_bind_addr(),
            auth_enabled: false,
            retention_days: default_retention_days(),
            retention_max_rows: default_retention_max_rows(),
            jobs_retention_days: default_jobs_retention_days(),
            usage_retention_months: 0,
            currency: default_currency(),
            local_reference_alias: String::new(),
            global_budget_micro: 0,
            global_budget_period: BudgetPeriod::Month,
            jobs_max_rows: default_jobs_max_rows(),
            router: RouterSettings::default(),
            aux_router: RouterSettings::default_aux(),
            audio: AudioSettings::default(),
            image: ImageSettings::default(),
            realtime: RealtimeSettings::default(),
            hf_token: String::new(),
            update_check_enabled: true,
            update_token: String::new(),
            sampling_alias: String::new(),
            self_admin: SelfAdmin::ReadOnly,
            self_admin_token: String::new(),
            responses_max_tool_calls: default_responses_max_tool_calls(),
            responses_timeout_seconds: default_responses_timeout_seconds(),
            responses_store: true,
            responses_retention_hours: default_responses_retention_hours(),
            responses_max_chains: default_responses_max_chains(),
            max_body_mb: default_max_body_mb(),
            chat_archive_days: default_chat_archive_days(),
            chat_purge_days: default_chat_purge_days(),
            chat_system_prompt: None,
            chat_pdf_mode: default_chat_pdf_mode(),
            chat_stt_alias: String::new(),
            chat_tts_alias: String::new(),
            chat_voice: String::new(),
            chat_speech_style: String::new(),
            chat_voice_language: String::new(),
            chat_voice_reply_language: String::new(),
            chat_read_aloud: false,
            chat_turn_detection: default_chat_turn_detection(),
            chat_voice_audio_input: default_chat_voice_audio_input(),
            chat_kb_budget_tokens: default_chat_kb_budget_tokens(),
            docs_ingest_reply_tokens: default_docs_ingest_reply_tokens(),
            docs_embed_batch: default_docs_embed_batch(),
            docs_fetch_delay_ms: default_docs_fetch_delay_ms(),
            docs_search: DocsSearchSettings::default(),
            docs_rerank_model: String::new(),
            vram: VramSettings::default(),
            hold: HoldSettings::default(),
            container_prefix: default_container_prefix(),
            agent_origin_suffix: default_agent_origin_suffix(),
            agent_script_image: default_agent_script_image(),
            builds_dir: None,
            forge_tokens: Default::default(),
            build_update_check_hours: default_build_update_check_hours(),
            legacy_container_names: Vec::new(),
        }
    }
}

/// Four checks a day: a moved branch or a pushed PR shows up the same
/// working day, and a forge without a token (60 requests/h on GitHub) is
/// nowhere near its limit.
fn default_build_update_check_hours() -> u32 {
    6
}

/// Enough for a few dozen sections of spans and metadata on a normal page.
fn default_docs_ingest_reply_tokens() -> u32 {
    4096
}
/// llama-server's default `ubatch-size` (512) handles this comfortably for
/// documentation-sized chunks.
fn default_docs_embed_batch() -> u32 {
    32
}
/// Four fetches a second against one host: brisk for a local mirror, unremarkable
/// for a docs site.
fn default_docs_fetch_delay_ms() -> u64 {
    250
}

fn default_responses_max_tool_calls() -> u32 {
    64
}
fn default_responses_timeout_seconds() -> u64 {
    600
}
/// One week. Long enough that a conversation resumed the next day still works,
/// short enough that a desktop gateway's DB doesn't grow without bound.
fn default_responses_retention_hours() -> i64 {
    168
}
fn default_responses_max_chains() -> i64 {
    500
}
/// Roomy enough that base64-inlined images and multi-page documents — the
/// things that were silently hitting axum's 2 MiB default — simply go through.
fn default_max_body_mb() -> u32 {
    64
}
/// Two weeks: long enough that a conversation picked up again the next day
/// (or the next few) is never surprised into the archive.
fn default_chat_archive_days() -> i64 {
    14
}
/// The values `chat_pdf_mode` takes.
pub const CHAT_PDF_MODES: [&str; 3] = ["text", "images", "ask"];
/// Text works with every model and costs the fewest tokens; a page image
/// needs vision, so it is opt-in.
fn default_chat_pdf_mode() -> String {
    "text".to_string()
}
/// Smart Turn: the owner's default for voice (chat-voice design §16); realtime's
/// own A/B between it and silence windows is still open.
fn default_chat_turn_detection() -> String {
    "semantic_vad".to_string()
}
/// Off: llama.cpp marks audio input experimental, and real voices are
/// untested (voice-audio-input design, decision 4).
fn default_chat_voice_audio_input() -> String {
    "off".to_string()
}
/// Enough for a few well-matched excerpts on any context window a chat model
/// is run with; a thread that needs more overrides it.
fn default_chat_kb_budget_tokens() -> u32 {
    4000
}
/// A month past archiving, so "I archived this by hand" still leaves time to
/// change your mind before the purge.
fn default_chat_purge_days() -> i64 {
    30
}

/// The one prefix every existing container-name field already spells out by
/// hand (`lmgw-llama-server`, `lmgw-llama-embed`, `lmgw-audiocpp`) — chosen so
/// an install that has never touched this setting derives the same names the
/// per-model runtime's naming scheme (§3.3) would produce for it anyway.
///
/// `pub`, not just used through the `serde(default = …)` attribute below: the
/// dev-instance safety check in `examples/headless.rs` (chat-archive-pin-
/// attachments review finding 12) compares a freshly loaded settings row
/// against exactly this value to tell "a data dir nobody has customized yet"
/// from one an owner (or an earlier run of this same check) already moved off
/// it.
pub fn default_container_prefix() -> String {
    "lmgw".into()
}

/// The pure decision behind the dev-instance safety check in
/// `examples/headless.rs` (chat-archive-pin-attachments review finding 12).
///
/// A fresh `LMGW_DATA_DIR` loads settings whose `container_prefix` is still
/// the production default — the exact same label the real tray app's model
/// containers carry. Boot reconciliation
/// (`runtime::registry::Registry::reconcile`, driven by
/// `runtime::lifecycle::boot`) trusts that prefix to mean "every container
/// under it belongs to *this* gateway" and force-removes whatever it does not
/// recognize, so a dev instance that boots under the production prefix can
/// stop the real app's model containers the moment it reconciles.
///
/// Returns `Some((new_container_prefix, new_bind_addr))` when the row still
/// carries the production default — the caller persists both before anything
/// reconciles a container. Returns `None` when it has already moved off that
/// default (an owner-configured data dir, or one an earlier run of this same
/// check already fixed): that case is left alone entirely, prefix *and*
/// `bind_addr` both, rather than re-stamping a value someone may have chosen
/// on purpose.
///
/// `env_container_prefix` is `LMGW_CONTAINER_PREFIX` when set and non-blank
/// (trimmed), overriding the `"lmgw-dev"` fallback. `cli_bind_addr` is the
/// address this run was actually launched against; `None` (no argument on the
/// command line) leaves `bind_addr` untouched in the returned tuple.
pub fn dev_instance_override(
    container_prefix: &str,
    env_container_prefix: Option<&str>,
    cli_bind_addr: Option<&str>,
) -> Option<(String, Option<String>)> {
    if container_prefix != default_container_prefix() {
        return None;
    }
    let dev_prefix = env_container_prefix
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("lmgw-dev")
        .to_string();
    Some((dev_prefix, cli_bind_addr.map(str::to_string)))
}

/// Why a dev instance must not serve on `container_prefix`, when it must not:
/// the production default is the installed app's, and `server::run` is what
/// starts every pass that lists and removes containers by prefix (model,
/// benchmark and agent reconciliation, the run-dir sweep) and every start
/// that names one. The backstop behind each entry point's own step (the
/// headless runner moves a fresh dir off the default, a debug shell refuses
/// one): a dev instance that reaches `server::run` on the default prefix
/// refuses to serve rather than treat the installed app's containers as its
/// own (chat-voice WP5 review B1). The settings write asks it too, so a dev
/// window cannot move its own instance onto the production prefix either
/// (WP11 review m1).
///
/// The hint names only the two seeding scripts: both also move `bind_addr`
/// off production's, where a hand-made copy keeps it (WP11 review m3).
pub fn dev_prefix_refusal(dev: bool, container_prefix: &str) -> Option<String> {
    (dev && container_prefix.trim() == default_container_prefix()).then(|| {
        format!(
            "this is a dev instance, and its container_prefix is the production default \
             '{}': its boot would list and remove the installed app's containers as its \
             own. Seed the data dir with scripts/dev-instance.sh or scripts/dev-copy.sh copy \
             (prefix lmgw-dev, a dev bind address).",
            container_prefix.trim()
        )
    })
}

#[cfg(test)]
mod dev_instance_override_tests {
    use super::dev_instance_override;

    #[test]
    fn leaves_an_already_customized_prefix_alone() {
        assert_eq!(
            dev_instance_override("lmgw-dev", None, Some("127.0.0.1:9")),
            None
        );
        assert_eq!(
            dev_instance_override("my-box", None, Some("127.0.0.1:9")),
            None
        );
    }

    #[test]
    fn defaults_to_lmgw_dev_when_no_env_override_is_set() {
        assert_eq!(
            dev_instance_override("lmgw", None, Some("127.0.0.1:8899")),
            Some(("lmgw-dev".to_string(), Some("127.0.0.1:8899".to_string())))
        );
    }

    #[test]
    fn env_override_wins_and_is_trimmed() {
        assert_eq!(
            dev_instance_override("lmgw", Some("  my-dev-box  "), None),
            Some(("my-dev-box".to_string(), None))
        );
    }

    #[test]
    fn a_blank_env_override_falls_back_to_lmgw_dev() {
        assert_eq!(
            dev_instance_override("lmgw", Some("   "), None),
            Some(("lmgw-dev".to_string(), None))
        );
    }

    #[test]
    fn no_cli_address_leaves_bind_addr_untouched() {
        assert_eq!(
            dev_instance_override("lmgw", None, None),
            Some(("lmgw-dev".to_string(), None))
        );
    }

    #[test]
    fn a_dev_instance_never_serves_on_the_production_prefix() {
        use super::dev_prefix_refusal;
        assert!(dev_prefix_refusal(true, "lmgw").is_some());
        assert!(dev_prefix_refusal(true, " lmgw ").is_some());
        assert!(dev_prefix_refusal(true, "lmgw-dev").is_none());
        assert!(dev_prefix_refusal(true, "my-box").is_none());
        // Production on its own prefix is the normal case.
        assert!(dev_prefix_refusal(false, "lmgw").is_none());
        // The hint never invites a hand-made copy, which keeps production's
        // bind_addr: only the scripts that move it are named.
        let why = dev_prefix_refusal(true, " lmgw ").unwrap();
        assert!(why.contains("'lmgw'"), "{why}");
        assert!(!why.contains("set another container_prefix"), "{why}");
    }
}
