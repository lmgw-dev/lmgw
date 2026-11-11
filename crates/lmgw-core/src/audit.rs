//! The audit log (headless server design §5.8, ruling 18): who did what,
//! written wherever accounts are on (the image always, the desktop while
//! its switch is on). Attribution among people who are all admins, not
//! tamper evidence, and between accounts it says who signed in, not proof
//! of who acted (D68).
//!
//! Its writers: the gate for every non-`GET` `Admin` request that writes
//! ([`at_gate`]), the self-admin dispatcher for every writing `lmgw__*`
//! call ([`ToolCall`]), the sign-in routes for every sign-in event, the
//! desktop's tray and window ([`desktop`]), and the server's offline command
//! line. Each builds an [`Entry`] from an [`Actor`] and hands it to
//! [`record`] or [`record_detached`]; what a body carries reaches an entry
//! only through [`redact`].
//!
//! A failed write fails nothing (D83): the request it records already did
//! what it did, so its answer stands; the failure is an error line in the
//! log and a count on the Accounts card.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Mutex;

use serde_json::{Map, Value};

use crate::config::Snapshot;
use crate::principal::Principal;
use crate::proxy::RequestCtx;
use crate::state::SharedState;

pub mod at_gate;
pub mod labels;
pub mod list;
pub mod redact;
pub mod tool_call;

pub use tool_call::ToolCall;

/// How a request reached the gateway, as the log names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Via {
    /// A browser: a session cookie or an owner key's cookie.
    Dashboard,
    /// A bearer key.
    Api,
    /// A `lmgw__*` call on `/mcp/admin`.
    Mcp,
    /// A `lmgw__*` call a model made in the dashboard's Chat.
    Chat,
    /// A `lmgw__*` call an agent's run made.
    Agent,
    /// `lmgw-server`'s command line through the running gateway: its
    /// `x-lmgw-client: cli` header, a claim recorded as one (§5.7).
    Cli,
    /// `lmgw-server`'s command line on the database, the gateway stopped.
    CliOffline,
    /// The desktop's tray menu.
    Tray,
    /// The desktop app's own commands (setting up the computer).
    App,
}

impl Via {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dashboard => "dashboard",
            Self::Api => "api",
            Self::Mcp => "mcp",
            Self::Chat => "chat",
            Self::Agent => "agent",
            Self::Cli => "cli",
            Self::CliOffline => "cli-offline",
            Self::Tray => "tray",
            Self::App => "app",
        }
    }
}

/// Who acted, how and from where: the part of an [`Entry`] every writer
/// fills the same way.
#[derive(Debug, Clone)]
pub struct Actor {
    pub account_id: Option<i64>,
    pub principal: String,
    pub via: Via,
    pub peer: String,
    /// No credential stands behind it (a failed or throttled sign-in): its
    /// repeats collapse into one entry a minute (D91).
    pub collapses: bool,
}

impl Actor {
    /// The caller of a request: its principal, and `via` from how it
    /// authenticated (a cookie is the dashboard, a bearer the API, the
    /// command line's header the command line).
    pub fn of(snap: &Snapshot, ctx: &RequestCtx, headers: &axum::http::HeaderMap) -> Self {
        let cli = headers
            .get(crate::accounts::CLIENT_HEADER)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.trim() == crate::accounts::CLIENT_CLI);
        let via = if cli {
            Via::Cli
        } else if ctx.principal.session().is_some() || ctx.owner_key_cookie {
            Via::Dashboard
        } else {
            Via::Api
        };
        Self::with_via(snap, ctx, via)
    }

    /// The caller of `ctx`, reaching the gateway by `via`.
    pub fn with_via(snap: &Snapshot, ctx: &RequestCtx, via: Via) -> Self {
        let (account_id, principal) = who(snap, &ctx.principal);
        Self {
            account_id,
            principal,
            via,
            peer: ctx.peer.map_or_else(|| "unknown".into(), |p| p.to_string()),
            collapses: false,
        }
    }

    /// An entry of this actor's.
    pub fn entry(
        &self,
        action: impl Into<String>,
        target: Map<String, Value>,
        outcome: u16,
    ) -> Entry {
        Entry {
            account_id: self.account_id,
            principal: self.principal.clone(),
            via: self.via,
            peer: self.peer.clone(),
            action: action.into(),
            target: Value::Object(target),
            outcome,
            collapses: self.collapses,
        }
    }
}

/// The account `p` acts as and its label: the account's login with the
/// session or key it came in with.
pub fn who(snap: &Snapshot, p: &Principal) -> (Option<i64>, String) {
    match p {
        Principal::Anonymous => (None, "anonymous".into()),
        Principal::Key {
            name,
            account,
            session,
            ..
        } => {
            let how = match session {
                Some(s) => format!("session #{s}"),
                None => format!("key {name}"),
            };
            let login = account
                .and_then(|a| snap.account(a))
                .map(|a| a.login.clone());
            (
                *account,
                login.map_or_else(|| how.clone(), |l| format!("{l}, {how}")),
            )
        }
    }
}

/// One entry, as a writer hands it to [`record`].
#[derive(Debug, Clone)]
pub struct Entry {
    pub account_id: Option<i64>,
    pub principal: String,
    pub via: Via,
    pub peer: String,
    pub action: String,
    /// Built through [`redact`]: no secret's value is ever here.
    pub target: Value,
    pub outcome: u16,
    /// Its repeats add to one row a minute (D91): [`Actor::collapses`].
    pub collapses: bool,
}

/// Write `entry`, `times` occurrences of it, then prune by the retention
/// now in force. A failure is an error line and a count, never the
/// request's failure (D83).
async fn write(state: &SharedState, entry: &Entry, times: u64) {
    if let Err(e) = crate::store::insert_audit(&state.db, entry, times).await {
        state.accounts.audit_failed.fetch_add(1, Ordering::Relaxed);
        tracing::error!(
            "the audit log could not record '{}' by {} ({}): {e} — the request itself was \
             answered",
            entry.action,
            entry.principal,
            entry.via.as_str()
        );
        return;
    }
    let keep = state.snapshot().settings.accounts.audit_days;
    if let Err(e) = crate::store::prune_audit(&state.db, keep).await {
        tracing::error!("audit log: pruning to {keep} days failed: {e}");
    }
}

/// Write `entry` before the request's answer leaves: an admin's write,
/// whose entry is there by the time the caller reads the log.
pub async fn record(state: &SharedState, entry: Entry) {
    write(state, &entry, 1).await;
}

/// Collapsing entries waiting for their write (D91): one per peer, action,
/// account, target and outcome, with how many times it happened since the
/// write was asked for. A flood from one peer then costs a write each time
/// the last one finished, not one per request.
#[derive(Default)]
pub struct Pending(Mutex<HashMap<String, (Entry, u64)>>);

impl Pending {
    fn key(e: &Entry) -> String {
        format!(
            "{}\u{1f}{}\u{1f}{:?}\u{1f}{}\u{1f}{}",
            e.peer, e.action, e.account_id, e.target, e.outcome
        )
    }

    /// Count `e`; `true` when no write for it is pending yet, so the caller
    /// starts one.
    fn add(&self, e: Entry) -> bool {
        let mut map = self.0.lock().unwrap_or_else(|p| p.into_inner());
        match map.entry(Self::key(&e)) {
            std::collections::hash_map::Entry::Occupied(mut o) => {
                o.get_mut().1 += 1;
                false
            }
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert((e, 1));
                true
            }
        }
    }

    fn take(&self, key: &str) -> Option<(Entry, u64)> {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).remove(key)
    }
}

/// Write `entry` off the request's path, after its answer: every sign-in
/// route's entry (D83), so no sign-in waits for the log, and a failure's
/// answer takes what an unknown name's does (D13). A collapsing entry joins
/// the write already pending for it ([`Pending`]).
pub fn record_detached(state: &SharedState, entry: Entry) {
    let state = state.clone();
    if !entry.collapses {
        tokio::spawn(async move { write(&state, &entry, 1).await });
        return;
    }
    let key = Pending::key(&entry);
    if !state.accounts.audit_pending.add(entry) {
        return;
    }
    tokio::spawn(async move {
        if let Some((entry, times)) = state.accounts.audit_pending.take(&key) {
            write(&state, &entry, times).await;
        }
    });
}

/// Whether the log is written now: wherever accounts are on.
pub fn on(state: &SharedState) -> bool {
    state.snapshot().accounts_on
}

/// A target of one field.
pub fn target(key: &str, value: impl Into<Value>) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(key.into(), value.into());
    m
}

/// The account's login as a target (`{ "account": id, "login": ... }`).
pub fn account_target(snap: &Snapshot, id: i64) -> Map<String, Value> {
    let mut m = target("account", id);
    if let Some(a) = snap.account(id) {
        m.insert("login".into(), a.login.clone().into());
    }
    m
}

/// The actor of a sign-in event no credential stands behind yet: a failed
/// or throttled attempt, from the dashboard's sign-in card or a link. Its
/// repeats collapse (D91).
pub fn anonymous(peer: impl Into<String>) -> Actor {
    Actor {
        account_id: None,
        principal: "anonymous".into(),
        via: Via::Dashboard,
        peer: peer.into(),
        collapses: true,
    }
}

fn login_of(snap: &Snapshot, account: i64) -> String {
    snap.account(account)
        .map_or_else(|| format!("#{account}"), |a| a.login.clone())
}

/// The actor of a sign-in that started `session` of `account`: the login
/// with the session, as every later entry of that session names it (D85).
pub fn signed_in(snap: &Snapshot, account: i64, session: i64, peer: impl Into<String>) -> Actor {
    Actor {
        account_id: Some(account),
        principal: format!("{}, session #{session}", login_of(snap, account)),
        via: Via::Dashboard,
        peer: peer.into(),
        collapses: false,
    }
}

/// The actor of a set-password link's submit, which starts its session
/// after the entry: the account it set, by its login.
pub fn by_link(snap: &Snapshot, account: i64, peer: impl Into<String>) -> Actor {
    Actor {
        account_id: Some(account),
        principal: format!("{}, set-password link", login_of(snap, account)),
        via: Via::Dashboard,
        peer: peer.into(),
        collapses: false,
    }
}

/// A sign-in event's target: the account it concerns (when the presented
/// name is one's) and what it was attempted with. An unknown name is never
/// written: it may be a password typed into the name field.
pub fn sign_in_target(snap: &Snapshot, account: Option<i64>, with: &str) -> Map<String, Value> {
    let mut t = match account {
        Some(id) => account_target(snap, id),
        None => Map::new(),
    };
    t.insert("with".into(), with.into());
    t
}

/// An entry of the desktop itself, `via` the tray or the app's window: the
/// person at the machine, whom the shell's nonce signs in as account 1
/// (D11), so account 1's, `peer` "local". Written only while accounts are
/// on; `target` goes through [`redact`] like any other.
pub async fn desktop(
    state: &SharedState,
    via: Via,
    action: &str,
    target: Map<String, Value>,
    outcome: u16,
) {
    let snap = state.snapshot();
    if !snap.accounts_on {
        return;
    }
    let one = crate::accounts::ACCOUNT_ONE;
    let actor = Actor {
        account_id: Some(one),
        principal: format!("{}, {}", login_of(&snap, one), via.as_str()),
        via,
        peer: "local".into(),
        collapses: false,
    };
    let mut target = Value::Object(target);
    redact::redact(&mut target);
    let Value::Object(target) = target else {
        return;
    };
    record(state, actor.entry(action, target, outcome)).await;
}

/// The actor of `lmgw-server`'s command line on the database, the gateway
/// stopped (§5.7): root in the container, no account.
pub fn offline_cli() -> Actor {
    Actor {
        account_id: None,
        principal: "lmgw-server (gateway stopped)".into(),
        via: Via::CliOffline,
        peer: "local".into(),
        collapses: false,
    }
}

/// Prune by the retention at the start: the days that passed while nothing
/// was written (every write prunes too).
pub async fn prune_at_start(state: &SharedState) {
    let keep = state.snapshot().settings.accounts.audit_days;
    match crate::store::prune_audit(&state.db, keep).await {
        Ok(0) => {}
        Ok(n) => tracing::info!("audit log: {n} entries older than {keep} days removed"),
        Err(e) => tracing::error!("audit log: pruning to {keep} days failed: {e}"),
    }
}
