//! Accounts (headless server design §5): several people who sign in with a
//! password and all administer the same gateway (rulings 5, 6).
//!
//! An account is not a new kind of principal: its credentials are `owner`
//! keys belonging to it (D10), the first of which, its **door key**, is
//! what a session of the account resolves to. Account 1 is the door (D11):
//! its door key is `owner:dashboard`; every other account's is
//! `owner:account-<id>`.
//!
//! *Accounts on* (D25) is container mode, or the desktop's
//! `accounts.enabled` (ruling 17): published on the snapshot
//! ([`crate::config::Snapshot::accounts_on`]) and asked by every account
//! surface. With it off the desktop is what it was before accounts.

use serde::Serialize;

use crate::config::Snapshot;

pub mod links;
pub mod ops;
pub mod password;
pub mod sessions;
pub mod switch;
pub mod throttle;

/// Account 1, the door (D11): it cannot be disabled or deleted.
pub const ACCOUNT_ONE: i64 = 1;

/// Account 1's sign-in name until its first password sets another (§5.3).
pub const ACCOUNT_ONE_LOGIN: &str = "owner";

/// The name prefix of every door key but account 1's (§5.1).
pub const DOOR_KEY_PREFIX: &str = "owner:account-";

/// The code every account op but `accounts_switch` answers while accounts
/// are off on the desktop (§5.6).
pub const ACCOUNTS_OFF_CODE: &str = "accounts_off";

/// Accounts' runtime on `AppState`: the session desk (shared with every
/// published snapshot), the sign-in throttle and the verification permits.
#[derive(Default)]
pub struct Runtime {
    pub sessions: std::sync::Arc<sessions::SessionDesk>,
    pub throttle: throttle::Throttle,
    pub verifier: password::Verifier,
    /// Audit writes that failed since the start (§5.8): each is also an
    /// error line, and the card shows the count.
    pub audit_failed: std::sync::atomic::AtomicU64,
    /// Collapsing audit entries waiting for their write (D91).
    pub audit_pending: crate::audit::Pending,
}

/// One `accounts` row as the snapshot carries it. The password hash is not
/// here: a sign-in reads it from the store, so no clone of a snapshot ever
/// holds one. `last_login_at` and `failed_logins` are as of the last
/// reload: a sign-in writes them without one (D13), and `account_list`
/// reads them from the store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Account {
    pub id: i64,
    pub login: String,
    pub display_name: String,
    pub has_password: bool,
    pub enabled: bool,
    pub door_key_id: i64,
    pub created_at: String,
    pub created_by: Option<i64>,
    pub password_changed_at: Option<String>,
    pub last_login_at: Option<String>,
    pub failed_logins: i64,
}

/// The door key's name of account `id` (§5.1): named by id, never by
/// login, so no login can collide with an owner key's name.
pub fn door_key_name(id: i64) -> String {
    if id == ACCOUNT_ONE {
        crate::agents::token::OWNER_DASHBOARD.to_string()
    } else {
        format!("{DOOR_KEY_PREFIX}{id}")
    }
}

/// A sign-in name checked and spelled as stored (§5.2): ASCII letters,
/// digits, `.`, `_` and `-`, in lowercase, 1 to 64 characters.
///
/// 64 is the length a sign-in field shows whole; the refusal names it.
pub fn normalize_login(login: &str) -> Result<String, String> {
    let l = login.trim().to_ascii_lowercase();
    if l.is_empty() {
        return Err("a sign-in name is needed".into());
    }
    if l.len() > LOGIN_MAX_CHARS {
        return Err(format!(
            "a sign-in name has at most {LOGIN_MAX_CHARS} characters (this one has {})",
            l.len()
        ));
    }
    if let Some(c) = l
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')))
    {
        return Err(format!(
            "a sign-in name takes letters a–z, digits and . _ - only; '{c}' is none of them"
        ));
    }
    Ok(l)
}

/// The longest sign-in name [`normalize_login`] takes.
pub const LOGIN_MAX_CHARS: usize = 64;

impl Snapshot {
    /// Account `id`'s row.
    pub fn account(&self, id: i64) -> Option<&Account> {
        self.accounts.iter().find(|a| a.id == id)
    }

    /// The account whose sign-in name is `login` (already lowercase).
    pub fn account_by_login(&self, login: &str) -> Option<&Account> {
        self.accounts.iter().find(|a| a.login == login)
    }

    /// Why an owner row of account `account_id` authenticates nothing now,
    /// or `None` when it does: its account is disabled (§5.1), or, on a
    /// desktop with accounts off, it belongs to an account other than
    /// account 1 and is paused (§5.11). A row without an account (none
    /// after the seed) and account 1's rows with accounts off are today's
    /// owner keys.
    pub fn owner_row_blocked(&self, account_id: Option<i64>) -> Option<OwnerBlock> {
        let id = account_id?;
        if !self.accounts_on {
            return (id != ACCOUNT_ONE).then(|| OwnerBlock::Paused(self.login_of(id)));
        }
        match self.account(id) {
            Some(a) if a.enabled => None,
            Some(a) => Some(OwnerBlock::Disabled(a.login.clone())),
            // A row whose account is gone: deleted with it (§5.6), so only a
            // database edited by hand gets here.
            None => Some(OwnerBlock::Disabled(format!("#{id}"))),
        }
    }

    fn login_of(&self, id: i64) -> String {
        self.account(id)
            .map_or_else(|| format!("#{id}"), |a| a.login.clone())
    }
}

/// Why an owner row does not authenticate ([`Snapshot::owner_row_blocked`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnerBlock {
    /// Its account is disabled (§5.1, D18): the login.
    Disabled(String),
    /// Accounts are off on this desktop and the row belongs to another
    /// account than account 1 (§5.11): the login.
    Paused(String),
}

impl OwnerBlock {
    /// The refusal resolution answers.
    pub fn refusal(&self) -> crate::principal::Refusal {
        use axum::http::StatusCode;
        match self {
            Self::Disabled(login) => crate::principal::Refusal {
                status: StatusCode::UNAUTHORIZED,
                code: "account_disabled",
                message: format!(
                    "account '{login}' is disabled, so its keys and sessions authenticate \
                     nothing — another account can enable it under Settings → Network & \
                     access → Accounts"
                ),
            },
            Self::Paused(login) => crate::principal::Refusal {
                status: StatusCode::UNAUTHORIZED,
                code: "key_paused",
                message: format!(
                    "this key belongs to account '{login}'; accounts are off on this desktop, \
                     so it is paused until they are on again"
                ),
            },
        }
    }
}

/// `409 accounts_off` (§5.6).
pub fn accounts_off() -> crate::principal::Refusal {
    crate::principal::Refusal {
        status: axum::http::StatusCode::CONFLICT,
        code: ACCOUNTS_OFF_CODE,
        message: format!(
            "accounts are off on this desktop; turn them on under {}",
            lmgw_api_types::settings_labels::SIGN_IN_ACCOUNTS_AT
        ),
    }
}

/// The header `lmgw-server`'s command line sends on its calls through the
/// running gateway (§5.7), with [`CLIENT_CLI`]: a claim, recorded as one.
pub const CLIENT_HEADER: &str = "x-lmgw-client";
/// Its value from the command line.
pub const CLIENT_CLI: &str = "cli";

/// The command a container's log names for a one-time sign-in link
/// (ruling 15, §5.3).
pub const LOGIN_LINK_COMMAND: &str = "podman exec -it lmgw lmgw-server login-link";

/// The container start's sign-in lines (ruling 15, §5.3), on the origin
/// `base`: while account 1 has no password a fresh single-use set-password
/// link (replacing the previous start's), afterwards only where to sign in
/// and the command for a one-time link. Never a durable key.
pub async fn start_lines(state: &crate::state::SharedState, base: &str) {
    let snap = state.snapshot();
    match snap.account(ACCOUNT_ONE) {
        Some(one) if !one.has_password => match links::mint_on(state, ACCOUNT_ONE, base).await {
            Ok(link) => tracing::info!(
                "set a password for account '{}': {} ({}, single use)",
                one.login,
                link.url,
                link.valid_until
            ),
            Err(e) => tracing::error!(
                "no set-password link for account '{}': {e} — run {LOGIN_LINK_COMMAND}",
                one.login
            ),
        },
        _ => tracing::info!(
            "sign in at {base}/ — for a one-time sign-in link run: {LOGIN_LINK_COMMAND}"
        ),
    }
}

/// The current UTC time as the store writes it (RFC 3339, seconds).
pub(crate) fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// A fresh 256-bit secret in hex, and its sha256 in hex as the store keeps
/// it: a session, a set-password link, a known browser (§5.2, §5.4, §5.5).
pub fn mint_secret() -> (String, String) {
    let token = hex::encode(rand::random::<[u8; 32]>());
    let hash = crate::config::hash_api_key(&token);
    (token, hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logins_are_ascii_lowercase_of_a_small_alphabet() {
        assert_eq!(normalize_login(" Sam.K-1_ ").unwrap(), "sam.k-1_");
        assert!(normalize_login("").is_err());
        assert!(normalize_login("sam k").is_err());
        assert!(normalize_login("zoë").unwrap_err().contains("'ë'"));
        assert!(normalize_login("a@b").is_err());
        assert!(normalize_login(&"a".repeat(65)).is_err());
        assert!(normalize_login(&"a".repeat(64)).is_ok());
    }

    #[test]
    fn door_keys_are_named_by_id() {
        assert_eq!(door_key_name(1), "owner:dashboard");
        assert_eq!(door_key_name(7), "owner:account-7");
    }
}
