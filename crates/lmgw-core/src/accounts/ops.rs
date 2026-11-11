//! Account ops (headless server design §5.6): `/api` only, `Admin`, absent
//! from the `lmgw__*` plane (D17). Every account administers the same
//! gateway (rulings 5, 6), so each op is open to every account, with the
//! few refusals §5.6 lists: account 1 is never disabled or deleted (D11),
//! an account never disables or deletes itself, and a set-password link for
//! one's own account is refused (§5.5).
//!
//! While accounts are off on the desktop every op here answers
//! `409 accounts_off` (the dispatcher, `web::accounts`, asks first).

use lmgw_api_types as dto;

use super::{door_key_name, normalize_login, Account, ACCOUNT_ONE};
use crate::config::{ApiKey, ApiKeyKind, Snapshot};
use crate::principal::{Principal, Refusal};
use crate::state::SharedState;

/// The passwords: the transport rule, hashing, `account_password_set`.
mod password;
pub use password::{
    first_login, hash_password, password_set, require_secure, PasswordSet, SetRefusal, Transport,
};

/// The account a caller acts as: its principal's, `None` for a principal
/// that belongs to no account (an owner row the seed has not attached
/// yet). Never account 1 by default: what needs an account refuses
/// ([`no_account`]).
pub fn caller_account(caller: &Principal) -> Option<i64> {
    caller.account()
}

/// `403 no_account`: the op acts as the caller's own account, and the
/// caller belongs to none.
pub fn no_account() -> Refusal {
    Refusal {
        status: axum::http::StatusCode::FORBIDDEN,
        code: "no_account",
        message: "this credential belongs to no account, and this asks for one: call it with an \
                  account's owner key or from a signed-in browser"
            .into(),
    }
}

fn not_found(id: i64) -> Refusal {
    Refusal::not_found(format!("no account with id {id}"))
}

pub(super) fn refused(code: &'static str, message: impl Into<String>) -> Refusal {
    Refusal {
        status: axum::http::StatusCode::CONFLICT,
        code,
        message: message.into(),
    }
}

pub(super) fn account_of(snap: &Snapshot, id: i64) -> Result<Account, Refusal> {
    snap.account(id).cloned().ok_or_else(|| not_found(id))
}

/// "made by" on the Keys and Devices cards (§5.1): the account's login, or
/// "a deleted account (<login>)" once it is gone. Left out on a desktop
/// with accounts off that has only account 1, which is today's desktop
/// (§12, "accounts off: nothing changed"): every key there is its one
/// owner's.
pub fn made_by(snap: &Snapshot, key: &ApiKey) -> Option<String> {
    if !snap.accounts_on && snap.accounts.len() <= 1 {
        return None;
    }
    let id = key.account_id?;
    match snap.account(id) {
        Some(a) => Some(a.login.clone()),
        None => Some(format!(
            "a deleted account ({})",
            key.made_by_login.as_deref().unwrap_or("?")
        )),
    }
}

/// Why an owner key is paused (§5.11) or held by a disabled account, as
/// the Keys card says it.
pub fn paused(snap: &Snapshot, key: &ApiKey) -> Option<String> {
    if key.kind != ApiKeyKind::Owner {
        return None;
    }
    snap.owner_row_blocked(key.account_id)
        .map(|b| b.refusal().message)
}

/// `account_list` (§5.6).
pub async fn list(state: &SharedState, caller: &Principal) -> Result<dto::AccountList, Refusal> {
    let snap = state.snapshot();
    let me = caller_account(caller);
    let sessions = live_sessions(state).await?;
    // The counters a sign-in writes without a reload (§5.4): the store's.
    let counters = crate::store::account_counters(&state.db)
        .await
        .map_err(|e| Refusal::from(e.to_string()))?;
    let accounts = snap
        .accounts
        .iter()
        .map(|a| dto::AccountView {
            id: a.id,
            login: a.login.clone(),
            display_name: a.display_name.clone(),
            enabled: a.enabled,
            has_password: a.has_password,
            door: a.id == ACCOUNT_ONE,
            you: Some(a.id) == me,
            created_at: a.created_at.clone(),
            created_by: a
                .created_by
                .and_then(|by| snap.account(by))
                .map(|b| b.login.clone()),
            last_login_at: counters
                .get(&a.id)
                .map_or_else(|| a.last_login_at.clone(), |c| c.1.clone()),
            password_changed_at: a.password_changed_at.clone(),
            failed_logins: counters.get(&a.id).map_or(a.failed_logins, |c| c.0),
            keys: snap
                .api_keys
                .iter()
                .filter(|k| k.kind == ApiKeyKind::Owner && k.account_id == Some(a.id))
                .count() as i64,
            sessions: sessions.iter().filter(|s| s.account_id == a.id).count() as i64,
        })
        .collect();
    Ok(dto::AccountList {
        accounts,
        limits: limits(state),
    })
}

/// The session rows that have not expired: what the card lists and counts
/// (expired rows go at the next sign-in, `sessions::start`).
async fn live_sessions(state: &SharedState) -> Result<Vec<crate::store::SessionRow>, Refusal> {
    let now = super::now_rfc3339();
    Ok(crate::store::list_sessions(&state.db)
        .await
        .map_err(|e| Refusal::from(e.to_string()))?
        .into_iter()
        .filter(|r| r.expires_at > now)
        .collect())
}

/// The card's fixed numbers (§5.2, §5.4, §5.10).
pub fn limits(state: &SharedState) -> dto::AccountLimits {
    let v = &state.accounts.verifier;
    dto::AccountLimits {
        password_min_chars: super::password::PASSWORD_MIN_CHARS as u32,
        body_limit_bytes: crate::web::session::SESSION_BODY_LIMIT as u64,
        backoff_start_s: super::throttle::LOGIN_BACKOFF_START.as_secs(),
        backoff_max_s: super::throttle::LOGIN_BACKOFF_MAX.as_secs(),
        link_ttl_s: super::links::PASSWORD_LINK_TTL.as_secs(),
        login_link_ttl_s: state.login_nonces.ttl().as_secs(),
        verifications_at_once: v.count() as u32,
        kept_for_known_browsers: v.kept_for_known() as u32,
        verification_memory_bytes: v.memory_bytes(),
        throttle_entries: state.accounts.throttle.len() as u64,
        session_days: state.snapshot().settings.accounts.session_days,
    }
}

fn link_view(account: &Account, link: super::links::Link) -> dto::AccountLink {
    dto::AccountLink {
        account_id: account.id,
        login: account.login.clone(),
        url: link.url,
        expires_at: link.expires_at,
        validity: link.valid_until,
    }
}

pub(super) async fn reload(state: &SharedState) -> Result<(), Refusal> {
    state
        .reload_snapshot()
        .await
        .map(|_| ())
        .map_err(|e| Refusal::from(format!("saved, but the reload failed: {e}")))
}

/// `account_create { login, display_name }`: the account and its door
/// key, answering a set-password link to hand over (§5.3 step 4, §5.5).
pub async fn create(
    state: &SharedState,
    caller: &Principal,
    login: &str,
    display_name: &str,
) -> Result<dto::AccountLink, Refusal> {
    let login = normalize_login(login)?;
    if state.snapshot().account_by_login(&login).is_some() {
        return Err(refused(
            "login_taken",
            format!("there is an account '{login}' already"),
        ));
    }
    let door = crate::agents::token::mint_owner();
    let (id, _) = crate::store::create_account(
        &state.db,
        &login,
        display_name.trim(),
        caller_account(caller),
        &door,
    )
    .await
    .map_err(|e| Refusal::from(format!("creating the account failed: {e}")))?;
    reload(state).await?;
    let account = account_of(&state.snapshot(), id)?;
    tracing::info!(
        "accounts: account '{login}' (id {id}, door key '{}') created",
        door_key_name(id)
    );
    let link = super::links::mint(state, id).await?;
    Ok(link_view(&account, link))
}

/// `account_set { id, display_name?, enabled? }` (§5.6, D18). Disabling ends
/// the account's sessions and its owner keys' connections; keys of other
/// kinds it made keep working.
pub async fn set(
    state: &SharedState,
    caller: &Principal,
    id: i64,
    display_name: Option<&str>,
    enabled: Option<bool>,
) -> Result<dto::AccountDone, Refusal> {
    let account = account_of(&state.snapshot(), id)?;
    if enabled == Some(false) {
        if id == ACCOUNT_ONE {
            return Err(refused(
                "account_door",
                "account 1 is the door to this gateway (its sign-in key is the dashboard \
                 key) and cannot be disabled",
            ));
        }
        if Some(id) == caller_account(caller) {
            return Err(refused(
                "account_self",
                "an account cannot disable itself; another account can",
            ));
        }
    }
    crate::store::set_account(&state.db, id, display_name.map(str::trim), enabled)
        .await
        .map_err(|e| Refusal::from(e.to_string()))?;
    reload(state).await?;
    let mut ended = 0;
    if enabled == Some(false) && account.enabled {
        ended = super::sessions::end(
            state,
            super::sessions::Which::Account {
                account_id: id,
                except: None,
            },
        )
        .await?;
    }
    Ok(dto::AccountDone {
        message: match enabled {
            Some(false) if account.enabled => format!(
                "account '{}' is disabled: its {ended} session(s) ended and its owner keys \
                 authenticate nothing; the client and device keys it made keep working",
                account.login
            ),
            Some(true) if !account.enabled => format!("account '{}' is enabled", account.login),
            _ => format!("account '{}' saved", account.login),
        },
        sessions_ended: ended as u64,
    })
}

/// `account_delete { id }` (§5.6): not account 1, not oneself. Its owner
/// keys and sessions go; keys of other kinds it made keep working and read
/// "made by a deleted account (<login>)".
pub async fn delete(
    state: &SharedState,
    caller: &Principal,
    id: i64,
) -> Result<dto::AccountDone, Refusal> {
    let account = account_of(&state.snapshot(), id)?;
    if id == ACCOUNT_ONE {
        return Err(refused(
            "account_door",
            "account 1 is the door to this gateway and cannot be deleted",
        ));
    }
    if Some(id) == caller_account(caller) {
        return Err(refused(
            "account_self",
            "an account cannot delete itself; another account can",
        ));
    }
    let sessions = crate::store::list_sessions(&state.db)
        .await
        .map_err(|e| Refusal::from(e.to_string()))?
        .iter()
        .filter(|s| s.account_id == id)
        .count();
    let keys = crate::store::delete_account(&state.db, id)
        .await
        .map_err(|e| Refusal::from(e.to_string()))?;
    reload(state).await?;
    super::sessions::end(
        state,
        super::sessions::Which::Account {
            account_id: id,
            except: None,
        },
    )
    .await?;
    for key in &keys {
        state
            .devices
            .revocations
            .revoke(*key, crate::devices::RevokeReason::Deleted);
    }
    Ok(dto::AccountDone {
        message: format!(
            "account '{}' deleted with its {} owner key(s) and {sessions} session(s); the keys \
             it made keep working",
            account.login,
            keys.len()
        ),
        sessions_ended: sessions as u64,
    })
}

/// `account_password_link { id }` (§5.5): any account but one's own.
pub async fn password_link(
    state: &SharedState,
    caller: &Principal,
    id: i64,
) -> Result<dto::AccountLink, Refusal> {
    let account = account_of(&state.snapshot(), id)?;
    // A signed-in browser of the account; a bearer of one of its owner keys
    // (the command line's door key) holds more than a password already.
    if Some(id) == caller_account(caller) && caller.session().is_some() {
        return Err(refused(
            "account_self",
            "change your own password with your current one (a set-password link for your own \
             account would step around it)",
        ));
    }
    let link = super::links::mint(state, id).await?;
    Ok(link_view(&account, link))
}

/// `account_sessions { id }` (§5.2's list).
pub async fn sessions(
    state: &SharedState,
    caller: &Principal,
    id: i64,
) -> Result<dto::SessionList, Refusal> {
    account_of(&state.snapshot(), id)?;
    let rows = live_sessions(state).await?;
    Ok(dto::SessionList {
        sessions: rows
            .into_iter()
            .filter(|r| r.account_id == id)
            .map(|r| dto::SessionRowView {
                this_browser: caller.session() == Some(r.id),
                id: r.id,
                account_id: r.account_id,
                created_at: r.created_at,
                last_seen_at: r.last_seen_at,
                expires_at: r.expires_at,
                peer: r.peer,
                user_agent: r.user_agent,
            })
            .collect(),
    })
}

/// `session_end { id }` or `session_end { account }` (§5.2): one session,
/// or every session of an account ("Sign out everywhere" for one's own).
/// Ending one's own current session is a sign-out.
pub async fn session_end(
    state: &SharedState,
    id: Option<i64>,
    account: Option<i64>,
) -> Result<dto::AccountDone, Refusal> {
    let which = match (id, account) {
        (Some(id), None) => super::sessions::Which::One(id),
        (None, Some(account_id)) => {
            account_of(&state.snapshot(), account_id)?;
            super::sessions::Which::Account {
                account_id,
                except: None,
            }
        }
        _ => return Err("name one session (id) or one account (account), not both".into()),
    };
    let ended = super::sessions::end(state, which).await?;
    if ended == 0 {
        if let Some(id) = id {
            return Err(Refusal::not_found(format!("no session with id {id}")));
        }
    }
    Ok(dto::AccountDone {
        message: format!("{ended} session(s) ended"),
        sessions_ended: ended as u64,
    })
}

/// `account_login_link { id }` (§5.6, §5.7): a one-time sign-in link for
/// the account, valid `LOGIN_NONCE_TTL` and once. The command line's op:
/// refused for a session, which could otherwise turn a signed-in browser
/// into a sign-in as any account without its password; a bearer of an
/// owner key (the door key `lmgw-server login-link` reads) holds the
/// gateway already.
pub async fn login_link(
    state: &SharedState,
    caller: &Principal,
    id: i64,
) -> Result<dto::AccountLink, Refusal> {
    if caller.session().is_some() {
        return Err(Refusal {
            status: axum::http::StatusCode::FORBIDDEN,
            code: "login_link_cli_only",
            message: "a one-time sign-in link is minted on the command line (lmgw-server \
                      login-link), not from a signed-in browser; to hand someone access, give \
                      them a set-password link (Settings → Network & access → Accounts)"
                .into(),
        });
    }
    let account = account_of(&state.snapshot(), id)?;
    if !account.enabled {
        return Err(super::OwnerBlock::Disabled(account.login.clone()).refusal());
    }
    let nonce = crate::web::session::mint_login_nonce_for(state, id);
    let ttl = state.login_nonces.ttl();
    let expires = chrono::Utc::now() + chrono::Duration::from_std(ttl).unwrap_or_default();
    Ok(dto::AccountLink {
        account_id: id,
        login: account.login.clone(),
        url: format!(
            "{}/api/session/login?nonce={nonce}",
            super::links::public_base(state)
        ),
        expires_at: expires.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        validity: format!("valid {} s, once", ttl.as_secs()),
    })
}
