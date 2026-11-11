//! Sessions (headless server design §5.2, ruling 19): a `sessions` row per
//! sign-in wherever accounts are on, and the in-memory desk resolution
//! reads them from.
//!
//! A session is a 256-bit token the server mints after a successful
//! sign-in (so nothing a request carried before is ever promoted: no
//! fixation), stored by its sha256, with its account, its expiry
//! (`accounts.session_days` from its start) and where it signed in from.
//! The cookie carries [`SESSION_PREFIX`] + the token. Resolution answers
//! the principal of the account's door key, with the account and the
//! session set ([`crate::principal`]).
//!
//! **A session's end closes what it opened.** Every long-lived connection
//! a principal can open watches its credential through
//! [`crate::devices::watch`], and that watch reads [`SessionDesk::alive`]
//! for a session's principal: ending a session takes it off the desk and
//! moves the revocation generation, so each watch re-reads and ends. The
//! connections that carry a cookie principal (WP6's list): the dashboard's
//! `/api/events` stream, the Chat's turn streams and its change feed, the
//! `/mcp` notification stream, `/v1/realtime` sockets and WebRTC calls,
//! the Chat's media actions, and streams proxied on an agent's MCP face.
//!
//! **A start never outlives an end that raced it.** A start's row is
//! inserted only while its account is enabled and still holds the password
//! it was verified against ([`Proof`]), in one statement; every end moves
//! its accounts' generation before it deletes anything, and a start goes on
//! the desk only while the generation it read before its insert still
//! stands (else it deletes its row and is refused). The generation is
//! checked and the desk written under one lock, the one the bump takes.

use std::collections::HashMap;
use std::sync::{Mutex, RwLock};

use crate::principal::Refusal;
use crate::state::SharedState;

/// The prefix a session cookie's value carries, so it is told from an
/// owner key at a glance (`lmgw-owner-…`).
pub const SESSION_PREFIX: &str = "lmgw-session-";

/// `last_seen_at` is written at most this often per session (§5.2): the
/// card says "last seen" to the hour.
pub const LAST_SEEN_EVERY_SECS: i64 = 3600;

#[derive(Debug, Clone, Copy)]
struct Live {
    id: i64,
    account_id: i64,
    expires: i64,
    seen_written: i64,
}

/// The live sessions by their token's hash. Loaded from the table at start
/// and written through by every start and end of a session.
#[derive(Debug, Default)]
pub struct SessionDesk {
    live: RwLock<HashMap<String, Live>>,
    /// The ends' generations. Taken before `live` wherever both are.
    ends: Mutex<Ends>,
}

/// How many ends each account has seen, and how many ended every session.
#[derive(Debug, Default)]
struct Ends {
    of: HashMap<i64, u64>,
    all: u64,
}

impl Ends {
    fn of(&self, account_id: i64) -> (u64, u64) {
        (self.all, self.of.get(&account_id).copied().unwrap_or(0))
    }
}

/// An account's end generation ([`SessionDesk::generation`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Generation(u64, u64);

/// What a new session's start has verified, which its insert checks again
/// in the same statement (the account enabled, always).
#[derive(Debug, Clone, Copy)]
pub enum Proof<'a> {
    /// A password: the PHC string it was verified against (or the one it
    /// was just set to), which the account must still hold.
    Password(&'a str),
    /// A credential that is not the password: an owner key, a login nonce,
    /// or the switch's caller.
    Credential,
}

/// One session a cookie resolved to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Found {
    pub id: i64,
    pub account_id: i64,
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

impl SessionDesk {
    /// The session a cookie value names: one with [`SESSION_PREFIX`] whose
    /// token's hash is live and not expired. `None` for anything else.
    pub fn lookup(&self, cookie: &str) -> Option<Found> {
        let token = cookie.strip_prefix(SESSION_PREFIX)?;
        let hash = crate::config::hash_api_key(token);
        let live = self.live.read().unwrap_or_else(|e| e.into_inner());
        let l = live.get(&hash)?;
        (l.expires > now()).then_some(Found {
            id: l.id,
            account_id: l.account_id,
        })
    }

    /// Whether session `id` is live and not expired: what a connection's
    /// watch reads.
    pub fn alive(&self, id: i64) -> bool {
        let t = now();
        self.live
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .any(|l| l.id == id && l.expires > t)
    }

    /// Session `id`'s expiry as a Unix time, while it is live.
    pub fn expires(&self, id: i64) -> Option<i64> {
        self.live
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .find(|l| l.id == id)
            .map(|l| l.expires)
    }

    /// Account `account_id`'s end generation now: what a start reads before
    /// its insert.
    pub fn generation(&self, account_id: i64) -> Generation {
        let (all, of) = self
            .ends
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .of(account_id);
        Generation(all, of)
    }

    /// Put a started session on the desk, if no end of its account moved
    /// the generation since `seen` and `still` holds (accounts still on);
    /// expired sessions go on the way. Answers whether it went on.
    fn admit(&self, hash: String, live: Live, seen: Generation, still: impl Fn() -> bool) -> bool {
        let ends = self.ends.lock().unwrap_or_else(|e| e.into_inner());
        let (all, of) = ends.of(live.account_id);
        if Generation(all, of) != seen || !still() {
            return false;
        }
        let t = now();
        let mut desk = self.live.write().unwrap_or_else(|e| e.into_inner());
        desk.retain(|_, l| l.expires > t);
        desk.insert(hash, live);
        true
    }

    /// Move the generation of the accounts `which` ends sessions of
    /// (`account` is a single session's, when it is known).
    fn bump(&self, which: Which, account: Option<i64>) {
        let mut ends = self.ends.lock().unwrap_or_else(|e| e.into_inner());
        match (which, account) {
            (Which::Account { account_id, .. }, _) | (Which::One(_), Some(account_id)) => {
                *ends.of.entry(account_id).or_default() += 1;
            }
            (Which::All, _) | (Which::One(_), None) => ends.all += 1,
        }
    }

    /// The account session `id` belongs to, while it is on the desk.
    fn account_of(&self, id: i64) -> Option<i64> {
        self.live
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .find(|l| l.id == id)
            .map(|l| l.account_id)
    }

    fn insert(&self, hash: String, live: Live) {
        self.live
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(hash, live);
    }

    /// Take sessions off the desk: those `pick` answers `true` for. Answers
    /// how many went.
    fn remove_where(&self, pick: impl Fn(&Live) -> bool) -> usize {
        let mut live = self.live.write().unwrap_or_else(|e| e.into_inner());
        let before = live.len();
        live.retain(|_, l| !pick(l));
        before - live.len()
    }

    /// The session's id when its `last_seen_at` is due a write (once an
    /// hour), marking it written.
    pub fn due_touch(&self, id: i64) -> bool {
        let t = now();
        let mut live = self.live.write().unwrap_or_else(|e| e.into_inner());
        match live.values_mut().find(|l| l.id == id) {
            Some(l) if t - l.seen_written >= LAST_SEEN_EVERY_SECS => {
                l.seen_written = t;
                true
            }
            _ => false,
        }
    }

    /// How many sessions are live.
    pub fn len(&self) -> usize {
        self.live.read().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// Whether none is.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// `409 sign_in_superseded`: the account changed while the sign-in ran.
pub fn superseded() -> Refusal {
    Refusal {
        status: axum::http::StatusCode::CONFLICT,
        code: "sign_in_superseded",
        message: "the account changed while this sign-in was checked (its password was changed, \
                  it was disabled, or its sessions were ended); sign in again"
            .into(),
    }
}

/// A session [`start`] began: its id (the audit log's "session #N") and the
/// cookie value to hand the browser.
pub struct Started {
    pub id: i64,
    pub cookie: String,
}

/// The id only: the cookie is the session's credential.
impl std::fmt::Debug for Started {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Started")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

/// Start a session for `account_id`: the row, the desk, and the cookie
/// value to hand the browser. `peer` and `user_agent` are what the card
/// shows; `proof` is what the insert checks again. Expired rows are
/// deleted on the way, so the card never lists one.
pub async fn start(
    state: &SharedState,
    account_id: i64,
    peer: &str,
    user_agent: &str,
    proof: Proof<'_>,
) -> Result<Started, Refusal> {
    let days = state.snapshot().settings.accounts.session_days;
    let (token, hash) = crate::accounts::mint_secret();
    let created = chrono::Utc::now();
    let expires = created + chrono::Duration::days(i64::from(days));
    let desk = &state.accounts.sessions;
    let seen = desk.generation(account_id);
    let id = crate::store::insert_session(
        &state.db,
        &crate::store::NewSession {
            token_hash: &hash,
            account_id,
            expires_at: &expires.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            peer,
            user_agent,
        },
        proof,
    )
    .await
    .map_err(|e| Refusal::from(format!("starting the session failed: {e}")))?
    .ok_or_else(superseded)?;
    let live = Live {
        id,
        account_id,
        expires: expires.timestamp(),
        seen_written: created.timestamp(),
    };
    if !desk.admit(hash, live, seen, || state.snapshot().accounts_on) {
        if let Err(e) = crate::store::delete_sessions(&state.db, Which::One(id)).await {
            tracing::warn!("session {id}: deleting a superseded start failed: {e}");
        }
        return Err(superseded());
    }
    Ok(Started {
        id,
        cookie: format!("{SESSION_PREFIX}{token}"),
    })
}

/// What a session's end is (for the log and the audit's word).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Which {
    /// One session by id.
    One(i64),
    /// Every session of an account, but `except` (this browser's).
    Account {
        account_id: i64,
        except: Option<i64>,
    },
    /// Every session there is.
    All,
}

/// End sessions: their rows, the desk, and every connection they opened
/// (the revocation generation moves, so each watch re-reads the desk).
/// Answers how many ended.
///
/// The generation moves first, so a start that raced this end and has not
/// reached the desk yet never does.
pub async fn end(state: &SharedState, which: Which) -> Result<usize, String> {
    let desk = &state.accounts.sessions;
    let account = match which {
        Which::One(id) => match desk.account_of(id) {
            Some(a) => Some(a),
            None => crate::store::session_account(&state.db, id)
                .await
                .map_err(|e| format!("ending the session failed: {e}"))?,
        },
        _ => None,
    };
    desk.bump(which, account);
    let rows = crate::store::delete_sessions(&state.db, which)
        .await
        .map_err(|e| format!("ending the sessions failed: {e}"))?;
    let gone = state.accounts.sessions.remove_where(|l| match which {
        Which::One(id) => l.id == id,
        Which::Account { account_id, except } => l.account_id == account_id && Some(l.id) != except,
        Which::All => true,
    });
    state.devices.revocations.rearm();
    if gone > 0 || rows > 0 {
        tracing::info!(
            "accounts: {} session(s) ended ({which:?})",
            gone.max(rows as usize)
        );
    }
    Ok(gone.max(rows as usize))
}

/// Load the live sessions from the table, at start; expired rows are
/// deleted on the way.
pub async fn load(state: &SharedState) -> Result<(), String> {
    let rows = crate::store::list_sessions(&state.db)
        .await
        .map_err(|e| format!("loading the sessions failed: {e}"))?;
    let t = now();
    let mut expired = Vec::new();
    for r in rows {
        let expires = chrono::DateTime::parse_from_rfc3339(&r.expires_at)
            .map(|d| d.timestamp())
            .unwrap_or(0);
        if expires <= t {
            expired.push(r.id);
            continue;
        }
        let seen = chrono::DateTime::parse_from_rfc3339(&r.last_seen_at)
            .map(|d| d.timestamp())
            .unwrap_or(t);
        state.accounts.sessions.insert(
            r.token_hash.clone(),
            Live {
                id: r.id,
                account_id: r.account_id,
                expires,
                seen_written: seen,
            },
        );
    }
    for id in expired {
        let _ = crate::store::delete_sessions(&state.db, Which::One(id)).await;
    }
    Ok(())
}

/// Write session `id`'s `last_seen_at`, when it is due (once an hour), off
/// the request's path.
pub fn touch(state: &SharedState, id: i64) {
    if !state.accounts.sessions.due_touch(id) {
        return;
    }
    let db = state.db.clone();
    tokio::spawn(async move {
        if let Err(e) = crate::store::touch_session(&db, id).await {
            tracing::debug!("session {id}: writing last_seen_at failed: {e}");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cookie_resolves_by_its_hash_and_only_with_the_prefix() {
        let desk = SessionDesk::default();
        let (token, hash) = crate::accounts::mint_secret();
        desk.insert(
            hash,
            Live {
                id: 4,
                account_id: 2,
                expires: now() + 60,
                seen_written: now(),
            },
        );
        assert_eq!(
            desk.lookup(&format!("{SESSION_PREFIX}{token}")),
            Some(Found {
                id: 4,
                account_id: 2
            })
        );
        assert_eq!(desk.lookup(&token), None, "no prefix");
        assert!(desk.alive(4));
        assert!(!desk.due_touch(4), "written at its start");
        assert_eq!(desk.remove_where(|l| l.id == 4), 1);
        assert!(!desk.alive(4));
        assert_eq!(desk.lookup(&format!("{SESSION_PREFIX}{token}")), None);
    }

    #[test]
    fn an_expired_session_is_nobody() {
        let desk = SessionDesk::default();
        let (token, hash) = crate::accounts::mint_secret();
        desk.insert(
            hash,
            Live {
                id: 1,
                account_id: 1,
                expires: now() - 1,
                seen_written: 0,
            },
        );
        assert_eq!(desk.lookup(&format!("{SESSION_PREFIX}{token}")), None);
        assert!(!desk.alive(1));
    }

    #[test]
    fn an_end_between_a_start_s_read_and_its_admission_keeps_it_off_the_desk() {
        let desk = SessionDesk::default();
        let live = |id, account_id| Live {
            id,
            account_id,
            expires: now() + 60,
            seen_written: now(),
        };
        let one = |account_id| Which::Account {
            account_id,
            except: None,
        };
        let seen = desk.generation(2);
        desk.bump(one(2), None);
        assert!(!desk.admit("a".into(), live(1, 2), seen, || true));
        // Another account's ends do not move this one's.
        let other = desk.generation(3);
        desk.bump(one(2), None);
        desk.bump(Which::One(9), Some(2));
        assert!(desk.admit("b".into(), live(2, 3), other, || true));
        // An end of every session moves every account's.
        let seen = desk.generation(3);
        desk.bump(Which::All, None);
        assert!(!desk.admit("c".into(), live(3, 3), seen, || true));
        // Accounts switched off meanwhile.
        assert!(!desk.admit("d".into(), live(4, 3), desk.generation(3), || false));
        assert_eq!(desk.len(), 1);
        // An admission drops what has expired.
        desk.insert(
            "old".into(),
            Live {
                id: 5,
                account_id: 3,
                expires: now() - 1,
                seen_written: 0,
            },
        );
        assert!(desk.admit("e".into(), live(6, 3), desk.generation(3), || true));
        assert_eq!(desk.len(), 2);
    }
}
