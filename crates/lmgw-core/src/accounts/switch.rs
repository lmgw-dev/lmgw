//! The desktop's accounts switch (headless server design §5.11, D29):
//! `accounts_switch { enabled }`, the one writer of `accounts.enabled`.
//! Refused in container mode, where accounts are always on.
//!
//! **On**: the caller's browser gets a session of its account in the same
//! answer, so it stays signed in; every other browser's owner-key cookie
//! reads as nobody from then on (and what it opened ends). Nothing else
//! moves: account 1 already holds every owner key (§5.1).
//!
//! **Off**: every session ends, as a Sign out everywhere for every account;
//! other accounts' owner keys are paused (resolution refuses them naming
//! their account) until accounts are on again; the throttle's map goes.
//! The accounts, their password hashes, known browsers, set-password links
//! and "made by" are kept. The caller gets today's door-key cookie when it
//! is account 1, and is signed out otherwise.

use lmgw_api_types as dto;

use super::ACCOUNT_ONE;
use crate::principal::{Principal, Refusal};
use crate::state::SharedState;

/// What the answer's cookie becomes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cookie {
    /// A session cookie with this value (switched on).
    Session(String),
    /// Today's cookie: the door key's plaintext (switched off, account 1).
    DoorKey(String),
    /// Cleared: signed out (switched off, another account).
    Clear,
    /// Left as it is (no change).
    Keep,
}

/// What the switch did.
pub struct Switched {
    pub done: dto::AccountDone,
    pub cookie: Cookie,
}

/// The counts the switch's confirmations and the kept-line name (§5.11).
pub async fn status(state: &SharedState) -> dto::AccountsStatus {
    let snap = state.snapshot();
    let mut errors = Vec::new();
    let (accounts, known, links) = crate::store::account_counts(&state.db)
        .await
        .unwrap_or_else(|e| {
            errors.push(format!("the accounts' counts: {e}"));
            Default::default()
        });
    let audit_kept = crate::store::audit_any(&state.db)
        .await
        .unwrap_or_else(|e| {
            errors.push(format!("the audit log: {e}"));
            false
        });
    let others: Vec<i64> = snap
        .accounts
        .iter()
        .filter(|a| a.id != ACCOUNT_ONE)
        .map(|a| a.id)
        .collect();
    let other_keys = snap
        .api_keys
        .iter()
        .filter(|k| {
            k.kind == crate::config::ApiKeyKind::Owner
                && k.account_id.is_some_and(|a| others.contains(&a))
        })
        .count();
    dto::AccountsStatus {
        on: snap.accounts_on,
        container: state.deployment().is_container(),
        enabled: snap.settings.accounts.enabled,
        accounts,
        other_accounts: others.len() as i64,
        other_keys: other_keys as i64,
        known_browsers: known,
        links,
        sessions: state.accounts.sessions.len() as i64,
        door_without_password: snap.account(ACCOUNT_ONE).is_some_and(|a| !a.has_password),
        secure_transport: snap.serving_tls.is_some() || !snap.trusted_nets.is_empty(),
        audit_kept,
        counts_error: (!errors.is_empty())
            .then(|| format!("could not be read from the database: {}", errors.join("; "))),
    }
}

/// `accounts_switch { enabled }`.
pub async fn switch(
    state: &SharedState,
    caller: &Principal,
    enabled: bool,
    peer: &str,
    user_agent: &str,
) -> Result<Switched, Refusal> {
    if state.deployment().is_container() {
        return Err(Refusal {
            status: axum::http::StatusCode::CONFLICT,
            code: "accounts_always_on",
            message: "accounts are always on in the server image; there is no switch here".into(),
        });
    }
    let was = state.snapshot().settings.accounts.enabled;
    if was == enabled {
        return Ok(Switched {
            done: dto::AccountDone {
                message: format!(
                    "accounts are {} already",
                    if enabled { "on" } else { "off" }
                ),
                sessions_ended: 0,
            },
            cookie: Cookie::Keep,
        });
    }
    // The caller's browser gets a session of its account (on), or the
    // cookie its account's state calls for (off): it needs one.
    let me = super::ops::caller_account(caller).ok_or_else(super::ops::no_account)?;
    if enabled {
        save(state, true).await?;
        state.accounts.verifier.warm();
        // Owner-key cookies read as nobody now: what they opened ends.
        state.devices.revocations.rearm();
        let proof = super::sessions::Proof::Credential;
        let cookie = super::sessions::start(state, me, peer, user_agent, proof)
            .await?
            .cookie;
        tracing::info!("accounts: switched on (by account {me})");
        return Ok(Switched {
            done: dto::AccountDone {
                message: "accounts are on: everyone who signs in to this dashboard needs an \
                          account; this browser stays signed in"
                    .into(),
                sessions_ended: 0,
            },
            cookie: Cookie::Session(cookie),
        });
    }
    let st = status(state).await;
    // Off first, then the ends: a sign-in that raced the switch finds
    // accounts off when it would go on the desk, and is refused
    // (`sessions::start`).
    save(state, false).await?;
    let ended = super::sessions::end(state, super::sessions::Which::All).await?;
    state.accounts.throttle.clear();
    // Other accounts' owner keys are paused now: their connections end.
    state.devices.revocations.rearm();
    tracing::info!(
        "accounts: switched off (by account {me}): {ended} session(s) ended, {} key(s) of {} \
         other account(s) paused",
        st.other_keys,
        st.other_accounts
    );
    let cookie = if me == ACCOUNT_ONE {
        match state
            .snapshot()
            .owner_key(crate::agents::token::OWNER_DASHBOARD)
        {
            Some(key) => Cookie::DoorKey(key.to_string()),
            None => Cookie::Clear,
        }
    } else {
        Cookie::Clear
    };
    Ok(Switched {
        done: dto::AccountDone {
            message: format!(
                "accounts are off: {ended} session(s) ended and {} key(s) of {} other account(s) \
                 are paused; the {} account(s) and their passwords are kept",
                st.other_keys, st.other_accounts, st.accounts
            ),
            sessions_ended: ended as u64,
        },
        cookie,
    })
}

/// Write `accounts.enabled` through the shared settings write: under the
/// settings lock, saved, then published.
async fn save(state: &SharedState, enabled: bool) -> Result<(), Refusal> {
    let guard = state.settings_write.lock().await;
    let mut s = state.snapshot().settings.clone();
    s.accounts.enabled = enabled;
    crate::store::save_settings(&state.db, &s)
        .await
        .map_err(|e| Refusal::from(e.to_string()))?;
    let published = state.settings_saved(&s).await;
    drop(guard);
    if let Some(e) = published.reload_failed {
        tracing::warn!("accounts switch saved, but the reload failed: {e}");
    }
    Ok(())
}
