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
///   own permissions is ungated; change it in the dashboard.
/// - `bind_addr` — takes effect only on restart, and a wrong value strands the
///   gateway on an address nothing is talking to.
/// - `hf_token` / `update_token` / `forge_tokens` — secrets. They are redacted
///   on read, and writing them through a tool call would put them in the
///   request log.
///
/// Container settings (images, ports, model directories) likewise stay on the
/// dashboard; `lmgw__container` drives their lifecycle, not their definition.
/// So does `builds_dir`, for the reason a models dir does: it is where lmgw
/// writes gigabytes and removes directories, and pointing it somewhere is the
/// owner's call, not a tool's.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
// The advertised inputSchema is closed (`additionalProperties: false`), so an
// argument we don't know is an error, not something to drop on the floor: a
// caller reaching for a field that doesn't exist (`self_admin` on a settings
// patch, say) must be told, never silently reported success.
#[serde(default, deny_unknown_fields)]
pub struct SettingsPatch {
    pub auth_enabled: Option<bool>,
    pub retention_days: Option<i64>,
    pub retention_max_rows: Option<i64>,
    pub max_body_mb: Option<u32>,
    /// Archive an idle Chat thread this many days after its last activity.
    /// `0` disables auto-archive (chat-archive-pin-attachments design §1).
    pub chat_archive_days: Option<i64>,
    /// Delete an archived, unpinned Chat thread this many days after it was
    /// archived. `0` keeps archived threads forever (design §1).
    pub chat_purge_days: Option<i64>,
    /// The system prompt new Chat threads start with (each thread keeps its
    /// own copy). The built-in text returns to the built-in default; `""`
    /// starts new threads with none.
    pub chat_system_prompt: Option<String>,
    /// How a text PDF attached in Chat starts out: `text` | `images` | `ask`.
    pub chat_pdf_mode: Option<String>,
    /// Speech-to-text alias for Chat audio attachments; `""` = none. Must
    /// resolve to a model whose capability task is `asr`.
    pub chat_stt_alias: Option<String>,
    /// Tokens of knowledge-base excerpts one Chat turn may carry. Must be
    /// above zero.
    pub chat_kb_budget_tokens: Option<i64>,
    pub sampling_alias: Option<String>,
    pub update_check_enabled: Option<bool>,
    /// Global GPU-hold fallback for chat-class local models (gpu-hold design
    /// §3.1). `""` clears it back to "refuse"; otherwise validated (must
    /// resolve, must not be local) before it is stored. Deliberately not
    /// `hold_active` — see [`crate::ops::hold_set`] (package 2).
    pub hold_fallback_alias: Option<String>,
    /// `vram.fallback_on_external` (candidate-aliases design §4.7, §12.25):
    /// answer a local model's fallback at once when VRAM outside lmgw's
    /// control is short, instead of queueing. On by default; turn off on
    /// shared-memory systems (APUs).
    pub fallback_on_external: Option<bool>,
    /// How often the build update check runs, in hours; `0` turns it off
    /// (container-builds §8). At most
    /// [`MAX_CHECK_HOURS`](crate::backends::updates::MAX_CHECK_HOURS).
    pub build_update_check_hours: Option<u32>,
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
    let _guard = state.settings_write.lock().await;
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

    if changed.is_empty() {
        return Err("no settings supplied — nothing to change".into());
    }
    store::save_settings(&state.db, &s)
        .await
        .map_err(|e| e.to_string())?;
    state.reload_snapshot().await.map_err(|e| e.to_string())?;
    Ok(json!({
        "ok": true,
        "changed": changed,
        "message": format!("updated {}", changed.join(", ")),
    }))
}
