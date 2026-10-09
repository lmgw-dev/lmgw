//! Personality profiles' list, create, change and delete
//! (personality-profiles design §3.1, D12): the one implementation the
//! `/chat/api/profiles*` routes and the self-admin tools share. No `/api/op`
//! op: a device edits profiles from the desktop client's window, and devices
//! never reach `/api/op`.
//!
//! Every write is one `begin_write` transaction that also records its
//! `profile.*` feed event (§3.2), so a write that fails records nothing.
//! The snapshot, which holds every profile (D10), is published after the
//! commit, and the open feeds are woken.
//!
//! What the caller decides: who it is (`by`, the feed's author), how far it
//! reaches into the self-admin plane (`admin`, which `used_by` counts as
//! far as), and the device it is (`device`): a device's written
//! `voice.tts_alias` passes its key's alias scope, and it may not change a
//! profile that steers lmgw's admin tools unless it may use them with
//! writes.

use lmgw_api_types::chat_profiles::{
    Profile, ProfileCreate, ProfileDeleted, ProfileList, ProfilePatch, UsedBy,
};
use serde::Deserialize;

use crate::config::chat_profile::{self as cp, ChatProfile, ProfileRefusal};
use crate::config::Snapshot;
use crate::state::SharedState;
use crate::store::{self, chat_profiles as table, feed, AdminThreads};

/// Why a profile call failed: a refusal of what was asked (its status and
/// code are [`ProfileRefusal`]'s), or the store.
#[derive(Debug)]
pub enum ProfileError {
    Refused(ProfileRefusal),
    /// A device's `voice.tts_alias` outside its key's alias scope
    /// (`403 key_scope`), the gateway's own refusal.
    Scope(crate::error::GatewayError),
    Internal(String),
}

impl ProfileError {
    pub fn status(&self) -> u16 {
        match self {
            Self::Refused(r) => r.status(),
            Self::Scope(e) => e.http_status().as_u16(),
            Self::Internal(_) => 500,
        }
    }

    pub fn code(&self) -> &'static str {
        match self {
            Self::Refused(r) => r.code(),
            Self::Scope(e) => e.code(),
            Self::Internal(_) => "internal",
        }
    }
}

impl std::fmt::Display for ProfileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(r) => r.fmt(f),
            Self::Scope(e) => e.fmt(f),
            Self::Internal(e) => f.write_str(e),
        }
    }
}

impl From<ProfileRefusal> for ProfileError {
    fn from(r: ProfileRefusal) -> Self {
        Self::Refused(r)
    }
}

impl From<crate::error::GatewayError> for ProfileError {
    fn from(e: crate::error::GatewayError) -> Self {
        Self::Internal(e.to_string())
    }
}

impl From<sqlx::Error> for ProfileError {
    fn from(e: sqlx::Error) -> Self {
        Self::Internal(e.to_string())
    }
}

/// Every profile with what uses it as far as `admin` reaches, in name
/// order, and the profile new threads start with (`chat_profile`).
pub async fn profiles_list(
    state: &SharedState,
    admin: AdminThreads,
) -> Result<ProfileList, ProfileError> {
    let profiles = table::list_chat_profiles(&state.db).await?;
    let mut used = table::chat_profiles_used_by(&state.db, admin).await?;
    let default_profile_id = state
        .snapshot()
        .settings
        .chat_profile
        .filter(|id| profiles.iter().any(|p| p.id == *id));
    Ok(ProfileList {
        profiles: profiles
            .iter()
            .map(|p| p.to_wire(used.remove(&p.id).unwrap_or_default()))
            .collect(),
        default_profile_id,
    })
}

/// Profile `id` with what uses it as far as `admin` reaches.
pub async fn profile_get(
    state: &SharedState,
    id: i64,
    admin: AdminThreads,
) -> Result<Profile, ProfileError> {
    let p = table::get_chat_profile(&state.db, id)
        .await?
        .ok_or(ProfileRefusal::NotFound(id))?;
    wire(state, &p, admin).await
}

/// Create a profile: the owner's own (`name` and any content fields) or a
/// built-in again (`builtin` alone, `409` while it exists). Recorded as
/// `profile.created`.
pub async fn profile_create(
    state: &SharedState,
    body: &ProfileCreate,
    admin: AdminThreads,
    by: feed::By<'_>,
    device: Option<i64>,
) -> Result<Profile, ProfileError> {
    let kind = cp::create_kind(body)?;
    if let cp::CreateKind::Named(_, draft) = &kind {
        check_tts_alias(state, draft.voice.tts_alias.as_deref(), None, device).await?;
    }
    let mut tx = store::begin_write(&state.db).await?;
    let p = table::create_in(&mut tx, &kind).await??;
    feed::record_profile(&mut tx, feed::kind::PROFILE_CREATED, p.id, &p.name, by).await?;
    tx.commit().await?;
    written(state).await;
    wire(state, &p, admin).await
}

/// Change profile `id`: the fields `patch` names (absent unchanged, `null`
/// unset). Recorded as `profile.updated`.
pub async fn profile_update(
    state: &SharedState,
    id: i64,
    mut patch: ProfilePatch,
    admin: AdminThreads,
    by: feed::By<'_>,
    device: Option<i64>,
) -> Result<Profile, ProfileError> {
    cp::normalise_patch(&mut patch)?;
    if let Some(Some(v)) = &patch.voice {
        let stored = table::get_chat_profile(&state.db, id)
            .await?
            .ok_or(ProfileRefusal::NotFound(id))?;
        check_tts_alias(
            state,
            v.tts_alias.as_deref(),
            stored.voice.tts_alias.as_deref(),
            device,
        )
        .await?;
    }
    let mut tx = store::begin_write(&state.db).await?;
    admin_use_guard(&mut tx, id, device).await?;
    let p = table::update_in(&mut tx, id, &patch).await??;
    feed::record_profile(&mut tx, feed::kind::PROFILE_UPDATED, p.id, &p.name, by).await?;
    tx.commit().await?;
    written(state).await;
    wire(state, &p, admin).await
}

/// Reset built-in profile `id` to its built-in texts (the editor's "Reset
/// to built-in"): every content field follows the built-in again — so it
/// takes the built-in's later improvements — and the voice is unset; the
/// name stays. Refused on one of the owner's own profiles
/// (`profile_not_builtin`). Recorded as `profile.updated`.
pub async fn profile_reset(
    state: &SharedState,
    id: i64,
    admin: AdminThreads,
    by: feed::By<'_>,
    device: Option<i64>,
) -> Result<Profile, ProfileError> {
    let mut tx = store::begin_write(&state.db).await?;
    admin_use_guard(&mut tx, id, device).await?;
    let p = table::reset_in(&mut tx, id).await??;
    feed::record_profile(&mut tx, feed::kind::PROFILE_UPDATED, p.id, &p.name, by).await?;
    tx.commit().await?;
    written(state).await;
    wire(state, &p, admin).await
}

/// Delete profile `id` (D16): in one transaction every thread using it goes
/// back to none, every folder default naming it loses it, the Chat's
/// default for new threads is emptied when it names it, and
/// `profile.deleted` is recorded. Under the settings lock, so a settings
/// save beside it never writes the deleted id back. The answer counts what
/// was cleared as far as `admin` reaches.
pub async fn profile_delete(
    state: &SharedState,
    id: i64,
    admin: AdminThreads,
    by: feed::By<'_>,
    device: Option<i64>,
) -> Result<ProfileDeleted, ProfileError> {
    let _settings = state.settings_write.lock().await;
    let name = table::get_chat_profile(&state.db, id)
        .await?
        .ok_or(ProfileRefusal::NotFound(id))?
        .name;
    let mut tx = store::begin_write(&state.db).await?;
    admin_use_guard(&mut tx, id, device).await?;
    let deleted = table::delete_in(&mut tx, id, admin, by).await??;
    feed::record_profile(&mut tx, feed::kind::PROFILE_DELETED, id, &name, by).await?;
    tx.commit().await?;
    // Temporary threads live outside the database: cleared here, as the
    // delete cleared the stored ones (they are not counted in the answer,
    // which counts what the transaction cleared).
    state.chat_temp.clear_profile(id);
    written(state).await;
    Ok(deleted)
}

/// A device's change, reset or delete of profile `id` while Admin Chat or
/// the self-admin toolset uses it (review fix 1; `table::admin_use_in`):
/// the profile's text lands in a system prompt that steers lmgw's admin
/// tools, a use the device cannot even see (`used_by` counts only what it
/// reaches). So it is refused (`403 profile_in_admin_use`) unless the
/// device may use those tools with writes itself — its own level capped by
/// the gateway's, as stored now, the gate a device's `lmgw__*` write call
/// passes. Checked on the write's transaction, so a thread that takes the
/// profile beside it cannot slip between check and write. The owner
/// (`device` `None`) always passes.
async fn admin_use_guard(
    tx: &mut sqlx::SqliteConnection,
    id: i64,
    device: Option<i64>,
) -> Result<(), ProfileError> {
    let Some(device) = device else {
        return Ok(());
    };
    let used = table::admin_use_in(tx, id).await?;
    if !used.any() {
        return Ok(());
    }
    let own = store::device_admin_in(tx, device).await?;
    let gateway = store::gateway_self_admin_in(tx).await?;
    if own.capped(gateway).allows_write() {
        return Ok(());
    }
    Err(ProfileRefusal::InAdminUse {
        threads: used.threads,
        folders: used.folders,
    }
    .into())
}

/// After a committed write: the snapshot that holds every profile, and the
/// open feeds. Saved is saved: a reload that fails is logged, and the
/// change reaches the snapshot at the next reload.
async fn written(state: &SharedState) {
    if let Err(e) = state.publish_snapshot().await {
        tracing::warn!(
            "chat profiles: the change is saved, but the configuration could not be reloaded \
             ({e}); it applies at the next reload"
        );
    }
    state.chat_feed.wake();
}

async fn wire(
    state: &SharedState,
    p: &ChatProfile,
    admin: AdminThreads,
) -> Result<Profile, ProfileError> {
    let used: UsedBy = table::chat_profiles_used_by(&state.db, admin)
        .await?
        .remove(&p.id)
        .unwrap_or_default();
    Ok(p.to_wire(used))
}

/// A written TTS alias names a text-to-speech model (task `tts` or `vdes`),
/// as a thread's does, and, written by `device`, one within its key's alias
/// scope (`403 key_scope`, design §3.1) — checked first, so an alias the
/// key may not use is not looked at. One equal to `before` (the stored
/// one) is not checked again, so a model deleted since, or a scope
/// narrowed since, does not block the rest. The routes and the self-admin
/// tools both come here (review fix 4: the tools skipped the scope).
async fn check_tts_alias(
    state: &SharedState,
    alias: Option<&str>,
    before: Option<&str>,
    device: Option<i64>,
) -> Result<(), ProfileError> {
    let Some(a) = alias.filter(|a| Some(*a) != before) else {
        return Ok(());
    };
    if let Some(device) = device {
        in_key_scope(&state.snapshot(), device, a)?;
    }
    super::validate_tts_alias(state, a)
        .await
        .map_err(|e| ProfileRefusal::Invalid(format!("voice.tts_alias: {e}")).into())
}

/// `alias` within device key `device`'s alias scope, as `snap` says; a key
/// that is gone admits nothing.
fn in_key_scope(snap: &Snapshot, device: i64, alias: &str) -> Result<(), ProfileError> {
    let alias = alias.trim();
    if alias.is_empty() {
        return Ok(());
    }
    let Some(key) = snap.api_keys.iter().find(|k| k.id == device) else {
        return Err(ProfileError::Scope(crate::error::GatewayError::KeyScope {
            key: format!("device key {device}"),
            alias: alias.to_string(),
            reason: "the key is gone".into(),
        }));
    };
    crate::policy::check_scope(snap, Some(&key.name), alias).map_err(ProfileError::Scope)
}

/// Whether `profile_id` names a profile the snapshot holds; the refusal's
/// message otherwise (`400 unknown_profile` in the Chat routes, design
/// §3.1). `None` (no profile) always passes.
pub fn profile_known(snap: &Snapshot, profile_id: Option<i64>) -> Result<(), String> {
    match profile_id {
        Some(id) if snap.chat_profile(id).is_none() => Err(format!(
            "profile_id: there is no profile with id {id} (GET /chat/api/profiles lists them)"
        )),
        _ => Ok(()),
    }
}

/// `chat_profile` as a settings patch writes it (design §3.3): a profile
/// id, as a number or as a numeric text, or `""` for none.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum ProfileChoice {
    Id(i64),
    Text(String),
}

/// The `chat_profile` a patch's value stores: `None` for `""`, else the id,
/// refused when no profile has it.
pub fn validate_chat_profile(snap: &Snapshot, v: &ProfileChoice) -> Result<Option<i64>, String> {
    let id = match v {
        ProfileChoice::Id(id) => *id,
        ProfileChoice::Text(t) if t.trim().is_empty() => return Ok(None),
        ProfileChoice::Text(t) => t.trim().parse().map_err(|_| {
            format!("chat_profile must be a profile id, or empty for none (got '{t}')")
        })?,
    };
    if snap.chat_profile(id).is_none() {
        return Err(format!(
            "chat_profile: there is no profile with id {id} (GET /chat/api/profiles lists them)"
        ));
    }
    Ok(Some(id))
}
