//! Accounts and sessions (headless server design §5): the account ops'
//! answers, the session's account in `GET /api/session`, and the Accounts
//! card's status on the settings answer.

use serde::{Deserialize, Serialize};

/// The account a session belongs to, in `GET /api/session` (§5.2).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SessionAccount {
    pub id: i64,
    pub login: String,
    pub display_name: String,
    pub has_password: bool,
}

/// One account on the Accounts card (`account_list`, §5.6).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AccountView {
    pub id: i64,
    pub login: String,
    pub display_name: String,
    pub enabled: bool,
    pub has_password: bool,
    /// Account 1, the door (D11): never disabled or deleted.
    pub door: bool,
    /// The caller's own account.
    pub you: bool,
    pub created_at: String,
    /// The login of the account that created it, when it still exists.
    pub created_by: Option<String>,
    pub last_login_at: Option<String>,
    pub password_changed_at: Option<String>,
    /// Failed sign-ins since the last good one (§5.4).
    pub failed_logins: i64,
    /// Its owner keys, the door key included.
    pub keys: i64,
    pub sessions: i64,
}

/// `account_list`'s answer: the accounts and what the card states beside
/// them (§5.4, §5.10).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AccountList {
    pub accounts: Vec<AccountView>,
    pub limits: AccountLimits,
}

/// The fixed numbers of sign-in, shown on the card (§5.2, §5.4, §5.10).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AccountLimits {
    pub password_min_chars: u32,
    /// The largest sign-in body, in bytes (`SESSION_BODY_LIMIT`).
    pub body_limit_bytes: u64,
    pub backoff_start_s: u64,
    pub backoff_max_s: u64,
    pub link_ttl_s: u64,
    pub login_link_ttl_s: u64,
    /// Verifications at once, and the memory they can take together.
    pub verifications_at_once: u32,
    /// Of them, the ones only a known browser may take (D67): a flood of
    /// unknown browsers leaves an account's own browsers a way in.
    pub kept_for_known_browsers: u32,
    pub verification_memory_bytes: u64,
    /// Entries in the sign-in throttle now.
    pub throttle_entries: u64,
    pub session_days: u32,
}

/// A set-password or a one-time sign-in link (`account_create`,
/// `account_password_link`, `account_login_link`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AccountLink {
    pub account_id: i64,
    pub login: String,
    pub url: String,
    /// RFC 3339 UTC.
    pub expires_at: String,
    /// "valid until 2026-11-09 14:00 UTC", or "valid 60 s, once".
    pub validity: String,
}

/// One session on the card (`account_sessions`, §5.2).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SessionRowView {
    pub id: i64,
    pub account_id: i64,
    pub created_at: String,
    /// Written at most once an hour.
    pub last_seen_at: String,
    pub expires_at: String,
    pub peer: String,
    pub user_agent: String,
    /// The browser asking.
    pub this_browser: bool,
}

/// `account_sessions`'s answer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SessionList {
    pub sessions: Vec<SessionRowView>,
}

/// What an account op that changes something answers: a sentence of what
/// it did and what followed (sessions ended, keys paused).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AccountDone {
    pub message: String,
    pub sessions_ended: u64,
}

/// The Accounts place on Settings → Network & access (§5.6, §5.11): whether
/// accounts are on and why, and the counts the desktop's switch names.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AccountsStatus {
    /// Accounts on (D25).
    pub on: bool,
    /// Container mode: always on, the switch refuses.
    pub container: bool,
    /// The desktop's switch as stored.
    pub enabled: bool,
    pub accounts: i64,
    /// Accounts other than account 1, and their owner keys: what turning
    /// accounts off pauses.
    pub other_accounts: i64,
    pub other_keys: i64,
    pub known_browsers: i64,
    pub links: i64,
    pub sessions: i64,
    /// Account 1 has no password yet: the banner (§5.11).
    pub door_without_password: bool,
    /// A password can travel from another machine: a TLS listener serves,
    /// or proxy mode is on (D26). The switch's row says so while not.
    pub secure_transport: bool,
    /// The audit log (§5.8) holds an entry: the kept-line names it. Its
    /// size is `audit_list`'s, which scans the log; this one is an index
    /// step.
    pub audit_kept: bool,
    /// The counts above could not be read: why (they read 0 then). The card
    /// says so rather than showing the zeros as fact.
    pub counts_error: Option<String>,
}

/// `/api/events`' last frame, `revoked`, when the credential the stream was
/// opened with ends (headless server design D58): a sign-out, a session
/// ended on the card, a disabled account, a rotated or deleted key.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct StreamRevoked {
    pub message: String,
}

/// One audit log entry (`audit_list`, headless server design §5.8).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AuditEntry {
    pub id: i64,
    /// RFC 3339 UTC.
    pub ts: String,
    /// The account it acted as; `None` for the offline command line and a
    /// sign-in under a name no account has.
    pub account_id: Option<i64>,
    /// Who, as it was then: the account's login and the session or key
    /// ("sam, session #12", "owner, key owner:dashboard").
    pub principal: String,
    /// How: `dashboard`, `api`, `mcp`, `chat`, `agent`, `cli`,
    /// `cli-offline`, `tray` or `app` (the desktop's own window).
    pub via: String,
    /// The client address.
    pub peer: String,
    /// An op's or tool's name, a route (`POST /api/docs/corpora/{id}/delete`)
    /// or a sign-in event (`sign-in`, `sign-in failed`, `sign-out`, ...).
    pub action: String,
    /// The row it names and, for a settings save, the fields that changed
    /// with their new values; a secret's value reads `"changed"`.
    pub target: serde_json::Value,
    /// The answer's HTTP status (a tool call: 200 done, 400 an error
    /// answered, 403 refused by the self-admin mode or the device's level).
    pub outcome: u16,
    /// How many times it happened: a sign-in event no credential stands
    /// behind, repeated by the same client within a minute, adds to one
    /// entry (D91). 1 for every other entry.
    pub count: u64,
    /// The last of `count` times, RFC 3339 UTC; `None` while `count` is 1.
    pub last_ts: Option<String>,
    /// The action in words: the op's or tool's title, the route's summary,
    /// the sign-in event's sentence. Read from today's tables when the page
    /// is asked for, so it is never stored.
    pub action_label: String,
}

/// A page of the audit log, newest first (`audit_list`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AuditPage {
    pub entries: Vec<AuditEntry>,
    /// Entries older than the last one here: `before` = its id reads them.
    pub more: bool,
    /// The whole log: its entries, the space it takes on disk, its oldest
    /// entry's time.
    pub total: i64,
    pub bytes: u64,
    pub oldest: Option<String>,
    /// `accounts.audit_days`: entries are kept this many days; 0 keeps every
    /// entry.
    pub keep_days: u32,
    /// Audit writes that failed since the gateway started (each also an
    /// error line in its log); the requests they recorded were answered.
    pub failed_writes: u64,
}
