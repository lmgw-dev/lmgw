//! Sparse patch for gateway settings: `SettingsPatch` and `settings_set`.

use lmgw_api_types as dto;
use serde::Deserialize;

use crate::config::Settings;
use crate::principal::Refusal;
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
    /// Seconds before an upstream remembered out of quota is asked again,
    /// when its answer carried no `Retry-After` (at least 1); until then a
    /// model with a fallback on it answers with the fallback.
    pub quota_recheck_seconds: Option<u64>,
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
    /// Seconds between the Chat feed's keep-alive comments, from 1 to
    /// `chat_feed_keepalive_max_s`.
    pub chat_feed_keepalive_s: Option<i64>,
    /// The longest keep-alive a feed may ask for itself
    /// (`GET /chat/api/feed?keepalive_s=`), from 1 to 32767 seconds (the longest Linux accepts for a TCP
    /// keep-alive timer).
    pub chat_feed_keepalive_max_s: Option<i64>,
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
    /// The personality profile new Chat threads start with: a profile id
    /// (`GET /chat/api/profiles`), as a number or a numeric text, or `""`
    /// for none. An unknown id is refused.
    pub chat_profile: Option<super::ProfileChoice>,
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
    /// The image and audio tools' default models: `generate`, `edit`,
    /// `speak`, `transcribe` (a name of that tool's `model` enum) and
    /// `voice` (a voice of the default speech model). Only the fields
    /// given change; `""` clears one (no default: the model chooses). The
    /// self-admin tool takes it as a JSON-encoded object.
    pub chat_media_defaults: Option<lmgw_api_types::chat_media::MediaDefaults>,
    pub sampling_alias: Option<String>,
    pub update_check_enabled: Option<bool>,
    /// `vram.fallback_on_external`: answer a local model's fallback at once when VRAM outside lmgw's
    /// control is short, instead of queueing. On by default; turn off on
    /// shared-memory systems (APUs).
    pub fallback_on_external: Option<bool>,
    /// `vram.headroom_mode`: `auto` (from what other programs did on each
    /// card, the default) or `fixed` (`headroom_mb` as written).
    pub headroom_mode: Option<lmgw_api_types::HeadroomMode>,
    /// `vram.headroom_mb`: the headroom's floor in MiB, kept free for other
    /// programs on each graphics card — in `auto` no more than the most they
    /// ever held there, with the learned room on top; in `fixed` as written.
    pub headroom_mb: Option<u64>,
    /// `vram.keep_free`: what is kept free for other programs on a card, by
    /// the card's id (`lmgw__settings`' `vram.headroom.cards[].id`):
    /// `{"mode": "always", "mb": N}` (N MiB, never given up), `{"mode":
    /// "nothing"}` (the card is lmgw's) or `{"mode": "automatic"}` (as
    /// `headroom_mode` says; takes the card's amount out). Only the cards
    /// given change. The self-admin tool takes it as a JSON-encoded object.
    pub keep_free:
        Option<std::collections::BTreeMap<lmgw_api_types::DeviceId, lmgw_api_types::KeepFree>>,
    /// How often the build update check runs, in hours; `0` turns it off.
    /// At most 8760.
    pub build_update_check_hours: Option<u32>,
    /// `audio.catalog_revision`: `pinned` (an audio catalog download takes
    /// the commit the spec pins) or `latest` (always `main`). Not part of
    /// the audio class's container definition, so not one of the dashboard-
    /// only container settings above.
    pub audio_catalog_revision: Option<String>,
    /// `documents.tool_slots`, Settings › Documents' "Parallel tool runs":
    /// how many document-conversion steps (tool runs, native reads) run at
    /// once, gateway-wide. `0` = derived from the machine (the smaller of
    /// the logical cores and the memory over the largest per-run memory
    /// limit; `doc_tools`' `pool` shows both).
    pub documents_tool_slots: Option<u32>,
    /// `documents.job_storage_mib`, Settings › Documents' "Storage for
    /// conversion jobs": the most bytes async conversion jobs store together,
    /// in MiB. `0` = derived (half the space free on the data directory's
    /// file system when lmgw started; `doc_tools`' `job_bounds` shows it).
    pub documents_job_storage_mib: Option<u64>,
    /// `documents.jobs_per_key`, Settings › Documents' "Jobs per key": the
    /// most async conversion jobs one key has running or queued at once.
    /// `0` = derived (the tool pool's size).
    pub documents_jobs_per_key: Option<u32>,
    /// `documents.fetch_sources`, Settings › Documents' "Let callers send
    /// web addresses": whether the docling-serve adapter fetches `http`
    /// sources from the allowed sites. Taken by this op only: the
    /// lmgw__settings_set tool refuses it, as it decides where lmgw sends
    /// requests for a caller.
    pub documents_fetch_sources: Option<bool>,
    /// `documents.source_hosts`, "Allowed sites": the hosts (each with an
    /// optional `:port`) a docling web source may name; a private or
    /// loopback address only when listed itself. Stored normalised; an
    /// entry that is not a host refuses the save. Taken by this op only,
    /// like `documents_fetch_sources`.
    pub documents_source_hosts: Option<Vec<String>>,
    /// `documents.onnx_layout`, Settings › Documents' "Use the layout
    /// model": PDF pages are laid out from the regions docling's layout
    /// model finds, on the processor. `true` is refused while the model's
    /// file is not here (`doc_tools`' `models` says; `doc_model_add`
    /// downloads it).
    pub documents_onnx_layout: Option<bool>,
    /// `documents.page_reader`, Settings › Documents' "Read pages with a
    /// model": the lmgw name of the vision model that reads the pages a
    /// conversion's tools read badly (pages without text, table pages).
    /// `""` = off, the default: no conversion asks a model. Refused by name
    /// when it does not resolve, is no chat model, or says it cannot see.
    /// Taken by this op only: the lmgw__settings_set tool refuses it, as it
    /// decides which model callers' document pages are sent to (D5: off
    /// until the user picks).
    pub documents_page_reader: Option<String>,
    /// `documents.page_reader_doctags`: the page-reading model answers in
    /// DocTags, docling's page format (a granite-docling model); it is asked
    /// with docling's prompt and read by the DocTags reader. Its llama-server
    /// must be started with `--special` (the model's extra llama-server
    /// flags). Taken by this op only, like `documents_page_reader`.
    pub documents_page_reader_doctags: Option<bool>,
    /// `documents.picture_alias`, Settings › Documents' "Describe pictures
    /// with": the lmgw name of the vision model that describes a
    /// conversion's pictures when a request asks for descriptions. `""` =
    /// off, the default: no picture is sent to a model. Refused by name when
    /// it does not resolve, is no chat model, or says it cannot see. Taken
    /// by this op only, like `documents_page_reader`: it decides which model
    /// callers' document pictures are sent to.
    pub documents_picture_alias: Option<String>,
    /// `documents.asr_alias`, Settings › Documents' "Transcribe audio with":
    /// the alias audio and video files are transcribed with in a
    /// conversion, as the calling key; `""` = none (they are then not
    /// transcribed). Must resolve to a model whose capability task is
    /// `asr`. Taken by this op only: the lmgw__settings_set tool refuses
    /// it, as it decides which model callers' audio is sent to (D5).
    pub documents_asr_alias: Option<String>,
    /// `GET /v1/realtime`'s settings: any of its fields,
    /// checked as the dashboard's save checks them. The self-admin tool takes
    /// it as a JSON-encoded object (`hoist_json_arg`).
    pub realtime: Option<super::RealtimeSettingsPatch>,
    /// Settings → Network & access → Devices: the device link's bounds (a
    /// pairing token's lifetime, the certificates' validity, the
    /// connection timeouts, the failed-enrolment rate), each refused by
    /// name outside its range. Taken by this op only: like `bind_addr`, an
    /// access setting the lmgw__settings_set tool refuses.
    pub devices: Option<super::DevicesSettingsPatch>,
    /// Settings → Network & access → Accounts: `session_days`, a session's
    /// lifetime (1 to 3650 days). `enabled` is refused by every save path:
    /// the accounts_switch op changes it. Taken by this op only: like
    /// `bind_addr`, an access setting the lmgw__settings_set tool refuses.
    pub accounts: Option<crate::config::AccountsSettingsPatch>,
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

/// Save `p` over the stored settings, as a task of its own (*changed
/// 2026-10-24*, see [`crate::detached::detached`]): the save runs under the
/// settings lock from its read to its write, so the task starts at the lock,
/// and a dropped request can no longer skip the publish, the MCP reconcile,
/// the tools' nudge or the device listener's move after the write.
///
/// Answers a [`Refusal`]: a pinned field (headless server design §2.2) is
/// refused with `400 setting_pinned`, as the dashboard's full save refuses
/// it; every other refusal is the flat `400 op_failed`.
pub async fn settings_set(
    state: &SharedState,
    p: SettingsPatch,
) -> Result<dto::SettingsSaved, Refusal> {
    let st = state.clone();
    crate::detached::detached("saving the settings", async move { save(&st, p).await }).await?
}

/// [`settings_set`]'s save.
/// `quota_recheck_seconds`: at least 1 — a re-check every request would
/// send every request to an upstream known to refuse it.
pub fn validate_quota_recheck_seconds(v: u64) -> Result<u64, String> {
    if v == 0 {
        return Err(
            "quota_recheck_seconds must be at least 1: it is how long an upstream remembered out \
             of quota is left alone before it is asked again"
                .into(),
        );
    }
    Ok(v)
}

async fn save(state: &SharedState, p: SettingsPatch) -> Result<dto::SettingsSaved, Refusal> {
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
    if let Some(v) = p.quota_recheck_seconds {
        s.quota_recheck_seconds = validate_quota_recheck_seconds(v)?;
        changed.push("quota_recheck_seconds");
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
            chat_feed_keepalive_max_s: p.chat_feed_keepalive_max_s,
            chat_feed_page_size: p.chat_feed_page_size,
            chat_feed_live_buffer: p.chat_feed_live_buffer,
        },
    )?);
    if let Some(v) = p.chat_system_prompt.as_deref() {
        s.set_default_chat_prompt(v);
        changed.push("chat_system_prompt");
    }
    if let Some(v) = &p.chat_profile {
        s.chat_profile = super::validate_chat_profile(&snap, v)?;
        changed.push("chat_profile");
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
    let media_changed = match &p.chat_media_defaults {
        Some(m) => {
            let new = crate::mcp::media::defaults::settings_patch(state, &s.chat_media_defaults, m)
                .await?;
            changed.push("chat_media_defaults");
            let moved = new != s.chat_media_defaults;
            s.chat_media_defaults = new;
            moved
        }
        None => false,
    };
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
    if let Some(v) = p.fallback_on_external {
        s.vram.fallback_on_external = v;
        changed.push("fallback_on_external");
    }
    if let Some(v) = p.headroom_mode {
        s.vram.headroom_mode = v;
        changed.push("headroom_mode");
    }
    if let Some(v) = p.headroom_mb {
        s.vram.headroom_mb = v;
        changed.push("headroom_mb");
    }
    if let Some(k) = p.keep_free {
        state.vram.merge_keep_free(&mut s.vram.keep_free, k)?;
        changed.push("keep_free");
    }
    if let Some(v) = p.build_update_check_hours {
        s.build_update_check_hours = crate::backends::updates::validate_check_hours(v)?;
        changed.push("build_update_check_hours");
    }

    if let Some(v) = p.audio_catalog_revision.as_deref() {
        s.audio.catalog_revision = crate::config::CatalogRevision::parse(v)?;
        changed.push("audio_catalog_revision");
    }

    if let Some(v) = p.documents_tool_slots {
        s.documents.tool_slots = v;
        changed.push("documents_tool_slots");
    }
    if let Some(v) = p.documents_job_storage_mib {
        s.documents.job_storage_mib = v;
        changed.push("documents_job_storage_mib");
    }
    if let Some(v) = p.documents_jobs_per_key {
        s.documents.jobs_per_key = v;
        changed.push("documents_jobs_per_key");
    }
    if let Some(v) = p.documents_fetch_sources {
        s.documents.fetch_sources = v;
        changed.push("documents_fetch_sources");
    }
    if let Some(v) = p.documents_source_hosts.as_deref() {
        s.documents.source_hosts = crate::extract::convert::source_fetch::HostList::normalise(v)?;
        changed.push("documents_source_hosts");
    }
    if let Some(v) = p.documents_onnx_layout {
        if v {
            crate::extract::convert::layout_model::status::can_switch_on(&s.aux_router.models_dir)?;
        }
        s.documents.onnx_layout = v;
        changed.push("documents_onnx_layout");
    }
    if let Some(v) = p.documents_page_reader.as_deref() {
        let alias = v.trim();
        if !alias.is_empty() {
            crate::extract::convert::model_help::settings::validate(state, alias).await?;
        }
        s.documents.page_reader = alias.to_string();
        changed.push("documents_page_reader");
    }
    if let Some(v) = p.documents_page_reader_doctags {
        s.documents.page_reader_doctags = v;
        changed.push("documents_page_reader_doctags");
    }
    if let Some(v) = p.documents_picture_alias.as_deref() {
        let alias = v.trim();
        if !alias.is_empty() {
            crate::extract::convert::describe::settings::validate(state, alias).await?;
        }
        s.documents.picture_alias = alias.to_string();
        changed.push("documents_picture_alias");
    }
    if let Some(v) = p.documents_asr_alias.as_deref() {
        let v = v.trim();
        if !v.is_empty() {
            validate_stt_alias(state, v).await?;
        }
        s.documents.asr_alias = v.to_string();
        changed.push("documents_asr_alias");
    }
    if let Some(r) = p.realtime {
        changed.extend(super::apply_realtime(state, &mut s.realtime, r).await?);
    }
    let mut changed: Vec<String> = changed.into_iter().map(String::from).collect();
    // When each device-link setting applies, said in the answer (§9.1), and
    // each WebRTC listener setting (calls design §9).
    let mut applies: Vec<String> = crate::config::webrtc_applies(state.deployment(), &changed);
    let mut listener_moved = false;
    if let Some(d) = p.devices {
        listener_moved = super::touches_listener(&d);
        for c in super::apply_devices(&mut s.devices, d, &s.bind_addr)? {
            applies.push(c.note());
            changed.push(c.key);
        }
    }

    if let Some(a) = p.accounts {
        changed.extend(a.apply(&mut s.accounts)?.into_iter().map(String::from));
    }

    if changed.is_empty() {
        return Err("no settings supplied — nothing to change".into());
    }
    // The shared settings write's check (headless server design §2.2): a
    // setting the server's container pins is not changed here.
    crate::config::deployment::refuse_pinned(state.start_facts(), &snap.settings, &s)?;
    if changed
        .iter()
        .any(|k| k.starts_with("devices.") || crate::config::WEBRTC_KEYS.contains(&k.as_str()))
    {
        crate::config::webrtc_port_collision(&s)?;
    }
    if changed.iter().any(|k| k.starts_with("devices.")) {
        crate::config::tcp_port_collision(&s, state.deployment().is_container())?;
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
    // A default model changes the media tools' schemas and descriptions.
    if media_changed {
        state.mcp.notify_tools_changed_now();
    }
    // The device listener follows at once (device link design §4.1).
    if listener_moved {
        applies.extend(super::apply_devices_listener(state).await);
    }
    let mut message = match published.reload_failed {
        None => format!("updated {}", changed.join(", ")),
        Some(e) => format!(
            "updated {} — the rest of the configuration could not be reloaded ({e}); these \
             settings apply now, and the rest at the next reload",
            changed.join(", ")
        ),
    };
    if !applies.is_empty() {
        message = format!("{message} ({})", applies.join("; "));
    }
    Ok(dto::SettingsSaved {
        ok: true,
        changed,
        message,
    })
}
