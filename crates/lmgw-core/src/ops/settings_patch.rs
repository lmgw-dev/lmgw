//! Sparse patch for gateway settings: `SettingsPatch` and `settings_set`.

use serde::Deserialize;
use serde_json::{json, Value};

use crate::config::{Settings, Snapshot};
use crate::state::SharedState;
use crate::store::{self};

/// Sparse patch for gateway settings.
///
/// Deliberately narrow. Three groups of settings are **not** reachable here,
/// and each omission is a decision rather than an oversight:
///
/// - `self_admin` — the gate on this whole surface. An agent that can widen its
///   own permissions is ungated; change it in the dashboard. It is one of the
///   access settings `lmgw__settings_set` refuses by name for every caller
///   (`selfadmin::ACCESS_SETTINGS`, 2026-10-07), with `auth_enabled`, which
///   only this patch's dashboard op (`/api/op/settings_set`) still takes.
/// - `bind_addr` — takes effect only on restart, and a wrong value strands the
///   gateway on an address nothing is talking to.
/// - `hf_token` / `update_token` / `forge_tokens` — secrets. They are redacted
///   on read, and writing them through a tool call would put them in the
///   request log.
///
/// Container settings (images, ports, model directories) likewise stay on the
/// dashboard; `lmgw__container` drives their lifecycle, not their definition.
/// So does `builds_dir`, for the reason a models dir does: it is where lmgw
/// writes gigabytes and removes directories, and pointing it somewhere is a
/// decision for the dashboard, not for a tool.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
// The advertised inputSchema is closed (`additionalProperties: false`), so an
// argument we don't know is an error, not something to drop on the floor: a
// caller reaching for a field that doesn't exist (`self_admin` on a settings
// patch, say) must be told, never silently reported success.
#[serde(default, deny_unknown_fields)]
pub struct SettingsPatch {
    /// Require a gateway API key on /v1/* and /mcp. Taken by this op only:
    /// the lmgw__settings_set tool refuses it, as it refuses every setting
    /// that decides who may reach lmgw.
    pub auth_enabled: Option<bool>,
    pub retention_days: Option<i64>,
    pub retention_max_rows: Option<i64>,
    pub max_body_mb: Option<u32>,
    /// Archive an idle Chat thread this many days after its last activity.
    /// `0` disables auto-archive. A folder's own `archive_days` overrides it
    /// for the threads in that folder, and an ongoing conversation's current
    /// thread is never archived.
    pub chat_archive_days: Option<i64>,
    /// Delete an archived, unpinned Chat thread this many days after it was
    /// archived. `0` keeps archived threads forever. A folder's own
    /// `purge_days` overrides it for the threads in that folder.
    pub chat_purge_days: Option<i64>,
    /// Keep the Chat change feed's records this many days; `0` keeps every
    /// record. A client away longer resumes with a `resync`.
    pub chat_feed_retention_days: Option<i64>,
    /// Seconds between the Chat feed's keep-alive comments, at least 1.
    pub chat_feed_keepalive_s: Option<i64>,
    /// Records the Chat feed reads per query while a client catches up,
    /// from 1 to 10000.
    pub chat_feed_page_size: Option<i64>,
    /// Live events the Chat feed holds for a slow client before it sends
    /// it a fresh `state` instead, from 1 to 65536.
    pub chat_feed_live_buffer: Option<i64>,
    /// The system prompt new Chat threads start with (each thread keeps its
    /// own copy). The built-in text returns to the built-in default; `""`
    /// starts new threads with none.
    pub chat_system_prompt: Option<String>,
    /// How a text PDF attached in Chat starts out: `text` | `images` | `ask`.
    pub chat_pdf_mode: Option<String>,
    /// The Chat's speech-to-text alias — dictation, realtime mode and audio
    /// attachments; `""` = `realtime.asr_alias`.
    /// Must resolve to a model whose capability task is `asr`.
    pub chat_stt_alias: Option<String>,
    /// The Chat's text-to-speech alias; `""` = `realtime.tts_alias`. Must be
    /// a model whose capability task is `tts` or `vdes`.
    pub chat_tts_alias: Option<String>,
    /// The Chat's voice; `""` = `realtime.default_voice`, then realtime's
    /// chain.
    pub chat_voice: Option<String>,
    /// The Chat's speech instructions; `""` = `realtime.speech_instructions`.
    pub chat_speech_style: Option<String>,
    /// The language the user speaks, an ISO 639-1 code — what the
    /// speech-to-text model is told; `""` = none.
    pub chat_voice_language: Option<String>,
    /// The language replies are in, an ISO 639-1 code — what the model is
    /// asked to answer in and the voice speaks; `""` = `chat_voice_language`.
    pub chat_voice_reply_language: Option<String>,
    /// Read Chat replies aloud as they stream.
    pub chat_read_aloud: Option<bool>,
    /// `semantic_vad` | `server_vad` | `push_to_talk`.
    pub chat_turn_detection: Option<String>,
    /// `off` | `on`: whether a voice turn goes to the chat model as audio
    /// when the model that answers it takes audio input, wherever it runs
    /// (experimental).
    pub chat_voice_audio_input: Option<String>,
    /// Tokens of knowledge-base excerpts one Chat turn may carry. Must be
    /// above zero.
    pub chat_kb_budget_tokens: Option<i64>,
    pub sampling_alias: Option<String>,
    pub update_check_enabled: Option<bool>,
    /// Global GPU-hold fallback for chat-class local models. `""` clears it
    /// back to "refuse"; otherwise validated (must resolve, must not be local)
    /// before it is stored. Deliberately not `hold_active`: engage or release
    /// the hold with `hold_set`.
    pub hold_fallback_alias: Option<String>,
    /// `vram.fallback_on_external`: answer a local model's fallback at once when VRAM outside lmgw's
    /// control is short, instead of queueing. On by default; turn off on
    /// shared-memory systems (APUs).
    pub fallback_on_external: Option<bool>,
    /// How often the build update check runs, in hours; `0` turns it off.
    /// At most 8760.
    pub build_update_check_hours: Option<u32>,
    /// `audio.catalog_revision`: `pinned` (an audio catalog download takes
    /// the commit the spec pins) or `latest` (always `main`). Not part of
    /// the audio class's container definition, so not one of the dashboard-
    /// only container settings above.
    pub audio_catalog_revision: Option<String>,
    /// `GET /v1/realtime`'s settings: any of its fields,
    /// checked as the dashboard's save checks them. The self-admin tool takes
    /// it as a JSON-encoded object (`hoist_json_arg`).
    pub realtime: Option<super::RealtimeSettingsPatch>,
}

/// A GPU-hold fallback alias is "usable" (gpu-hold design §2) when it
/// resolves at all and lands on a route that is not itself local. Applied at
/// set time by every setter that takes one — this settings patch, the local/
/// aux/audio model patches, and (candidate-aliases design §4.1)
/// `candidate_alias_set` — because a typo caught here never reaches a held
/// request. Request-time re-checks still exist on top of this (§2, package
/// 2): rows change after they are validated, and a bare `expose_all` upstream
/// accepts any name here and can still fail at the provider later — that
/// limit cannot be detected at set time either.
///
/// A candidate alias is refused by name here too (§4.1: "never another
/// candidate alias") — [`Snapshot::usable_fallback`] is the request-time twin
/// of this same rule, and both go through it so the two do not drift.
pub fn validate_fallback_alias(snap: &Snapshot, alias: &str) -> Result<(), String> {
    match snap.usable_fallback(alias) {
        Ok(_) => Ok(()),
        Err(why) => Err(format!("fallback alias '{alias}' {why}")),
    }
}

/// `chat_pdf_mode` must be one of [`crate::config::CHAT_PDF_MODES`].
pub fn validate_chat_pdf_mode(mode: &str) -> Result<String, String> {
    let m = mode.trim().to_lowercase();
    if crate::config::CHAT_PDF_MODES.contains(&m.as_str()) {
        Ok(m)
    } else {
        Err(format!(
            "chat_pdf_mode '{mode}' is not one of {}",
            crate::config::CHAT_PDF_MODES.join(", ")
        ))
    }
}

/// `chat_kb_budget_tokens` must be above zero: a budget of 0 would attach a
/// knowledge base and then never let it contribute a word.
pub fn validate_chat_kb_budget(v: i64) -> Result<u32, String> {
    match u32::try_from(v) {
        Ok(n) if n > 0 => Ok(n),
        _ => Err(format!(
            "chat_kb_budget_tokens must be a whole number above 0 (got {v})"
        )),
    }
}

/// A speech-to-text alias must resolve and its capability `task` must be
/// `asr` — the same task the audio transcription route serves. Empty (none)
/// is the caller's business; this is only asked for a non-empty name.
pub async fn validate_stt_alias(state: &SharedState, alias: &str) -> Result<(), String> {
    state
        .snapshot()
        .resolve(alias)
        .map_err(|_| format!("STT alias '{alias}' does not resolve"))?;
    let task = crate::capabilities::exposed::exposed_entry(state, alias)
        .await
        .and_then(|e| e.capabilities)
        .map(|c| c.task);
    match task.as_deref() {
        Some("asr") => Ok(()),
        Some(other) => Err(format!(
            "STT alias '{alias}' is a '{other}' model; it must be a speech-to-text (asr) model"
        )),
        None => Err(format!(
            "STT alias '{alias}' has no readable capabilities, so it cannot be confirmed \
             as a speech-to-text (asr) model"
        )),
    }
}

pub async fn settings_set(state: &SharedState, p: SettingsPatch) -> Result<Value, String> {
    // Held across the whole read-modify-write: see `AppState::settings_write`.
    let guard = state.settings_write.lock().await;
    let snap = state.snapshot();
    let mut s: Settings = snap.settings.clone();
    let mut changed: Vec<&str> = Vec::new();

    if let Some(v) = p.auth_enabled {
        s.auth_enabled = v;
        changed.push("auth_enabled");
    }
    if let Some(v) = p.retention_days {
        s.retention_days = v.max(0);
        changed.push("retention_days");
    }
    if let Some(v) = p.retention_max_rows {
        s.retention_max_rows = v.max(0);
        changed.push("retention_max_rows");
    }
    if let Some(v) = p.max_body_mb {
        s.max_body_mb = v;
        changed.push("max_body_mb");
    }
    if let Some(v) = p.chat_archive_days {
        s.chat_archive_days = v.max(0);
        changed.push("chat_archive_days");
    }
    if let Some(v) = p.chat_purge_days {
        s.chat_purge_days = v.max(0);
        changed.push("chat_purge_days");
    }
    changed.extend(super::apply_chat_feed(
        &mut s,
        super::ChatFeedPatch {
            chat_feed_retention_days: p.chat_feed_retention_days,
            chat_feed_keepalive_s: p.chat_feed_keepalive_s,
            chat_feed_page_size: p.chat_feed_page_size,
            chat_feed_live_buffer: p.chat_feed_live_buffer,
        },
    )?);
    if let Some(v) = p.chat_system_prompt.as_deref() {
        s.set_default_chat_prompt(v);
        changed.push("chat_system_prompt");
    }
    if let Some(v) = p.chat_pdf_mode.as_deref() {
        s.chat_pdf_mode = validate_chat_pdf_mode(v)?;
        changed.push("chat_pdf_mode");
    }
    if let Some(v) = p.chat_stt_alias.as_deref() {
        let v = v.trim();
        if !v.is_empty() {
            validate_stt_alias(state, v).await?;
        }
        s.chat_stt_alias = v.to_string();
        changed.push("chat_stt_alias");
    }
    if let Some(v) = p.chat_kb_budget_tokens {
        s.chat_kb_budget_tokens = validate_chat_kb_budget(v)?;
        changed.push("chat_kb_budget_tokens");
    }
    changed.extend(
        super::apply_chat_voice(
            state,
            &mut s,
            super::ChatVoicePatch {
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
        .await?,
    );
    if let Some(v) = p.sampling_alias.as_deref() {
        s.sampling_alias = v.trim().to_string();
        changed.push("sampling_alias");
    }
    if let Some(v) = p.update_check_enabled {
        s.update_check_enabled = v;
        changed.push("update_check_enabled");
    }
    if let Some(v) = p.hold_fallback_alias.as_deref() {
        let v = v.trim();
        if v.is_empty() {
            s.hold.fallback_alias = None;
        } else {
            validate_fallback_alias(&snap, v)?;
            s.hold.fallback_alias = Some(v.to_string());
        }
        changed.push("hold_fallback_alias");
    }
    if let Some(v) = p.fallback_on_external {
        s.vram.fallback_on_external = v;
        changed.push("fallback_on_external");
    }
    if let Some(v) = p.build_update_check_hours {
        s.build_update_check_hours = crate::backends::updates::validate_check_hours(v)?;
        changed.push("build_update_check_hours");
    }

    if let Some(v) = p.audio_catalog_revision.as_deref() {
        s.audio.catalog_revision = crate::config::CatalogRevision::parse(v)?;
        changed.push("audio_catalog_revision");
    }

    if let Some(r) = p.realtime {
        changed.extend(super::apply_realtime(state, &mut s.realtime, r).await?);
    }

    if changed.is_empty() {
        return Err("no settings supplied — nothing to change".into());
    }
    store::save_settings(&state.db, &s)
        .await
        .map_err(|e| e.to_string())?;
    // Saved is saved (review G-6): a reload that fails is said beside it.
    let published = state.settings_saved(&s).await;
    // Published under the lock, reconciled after it (`settings_saved`),
    // against the snapshot of then (`reconcile_mcp`).
    drop(guard);
    state.reconcile_mcp().await;
    let message = match published.reload_failed {
        None => format!("updated {}", changed.join(", ")),
        Some(e) => format!(
            "updated {} — the rest of the configuration could not be reloaded ({e}); these \
             settings apply now, and the rest at the next reload",
            changed.join(", ")
        ),
    };
    Ok(json!({
        "ok": true,
        "changed": changed,
        "message": message,
    }))
}
