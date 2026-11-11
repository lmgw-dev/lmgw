//! Accounts, sessions, set-password links and known browsers (headless
//! server design §5.1, migration 0101). Every write that touches more than
//! one statement goes through [`super::begin_write`].

use sqlx::{Row, SqlitePool};

use crate::accounts::sessions::{Proof, Which};
use crate::accounts::Account;

use super::*;

fn account_from_row(r: &sqlx::sqlite::SqliteRow) -> Account {
    Account {
        id: r.get("id"),
        login: r.get("login"),
        display_name: r.get("display_name"),
        has_password: r.get::<Option<String>, _>("password_hash").is_some(),
        enabled: r.get::<i64, _>("enabled") != 0,
        door_key_id: r.get("door_key_id"),
        created_at: r.get("created_at"),
        created_by: r.get("created_by"),
        password_changed_at: r.get("password_changed_at"),
        last_login_at: r.get("last_login_at"),
        failed_logins: r.get("failed_logins"),
    }
}

/// Every account, by id.
pub async fn list_accounts(pool: &SqlitePool) -> DbResult<Vec<Account>> {
    let rows = sqlx::query("SELECT * FROM accounts ORDER BY id")
        .fetch_all(pool)
        .await?;
    Ok(rows.iter().map(account_from_row).collect())
}

/// Account `id`'s PHC string, `None` while it has no password (or no row).
pub async fn account_password_hash(pool: &SqlitePool, id: i64) -> DbResult<Option<String>> {
    Ok(
        sqlx::query_scalar("SELECT password_hash FROM accounts WHERE id = ?1")
            .bind(id)
            .fetch_optional(pool)
            .await?
            .flatten(),
    )
}

/// The id and PHC string of the enabled account whose login is `login`
/// (already folded), while it has a password: what a sign-in verifies
/// against. One query for every presented name, existing or not (D13).
pub async fn sign_in_hash(pool: &SqlitePool, login: &str) -> DbResult<Option<(i64, String)>> {
    let row = sqlx::query(
        "SELECT id, password_hash FROM accounts
          WHERE login = ?1 AND enabled = 1 AND password_hash IS NOT NULL",
    )
    .bind(login)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| (r.get("id"), r.get("password_hash"))))
}

/// Account 1 and the owner rows' attachment, in one `BEGIN IMMEDIATE`
/// transaction (§5.1): account 1 (login `owner`, no password, door key
/// `door_key_id`) when it does not exist, then every owner row without an
/// account attached to it. Idempotent; answers whether account 1 was made.
pub async fn seed_account_one(pool: &SqlitePool, door_key_id: i64) -> DbResult<bool> {
    let mut tx = begin_write(pool).await?;
    let made = sqlx::query(
        "INSERT INTO accounts (id, login, door_key_id) VALUES (1, ?1, ?2)
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(crate::accounts::ACCOUNT_ONE_LOGIN)
    .bind(door_key_id)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        > 0;
    sqlx::query("UPDATE api_keys SET account_id = 1 WHERE kind = 'owner' AND account_id IS NULL")
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(made)
}

/// A new account and its door key `owner:account-<id>` (§5.1), in one
/// transaction. Answers the account's id and its door key's id.
pub async fn create_account(
    pool: &SqlitePool,
    login: &str,
    display_name: &str,
    created_by: Option<i64>,
    door_plain: &str,
) -> DbResult<(i64, i64)> {
    let mut tx = begin_write(pool).await?;
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO accounts (login, display_name, door_key_id, created_by)
              VALUES (?1, ?2, 0, ?3) RETURNING id",
    )
    .bind(login)
    .bind(display_name)
    .bind(created_by)
    .fetch_one(&mut *tx)
    .await?;
    let name = crate::accounts::door_key_name(id);
    let made_by: Option<String> = match created_by {
        Some(by) => {
            sqlx::query_scalar("SELECT login FROM accounts WHERE id = ?1")
                .bind(by)
                .fetch_optional(&mut *tx)
                .await?
        }
        None => None,
    };
    let key_id: i64 = sqlx::query_scalar(
        "INSERT INTO api_keys (name, key_hash, key_plain, enabled, kind, note, account_id,
                               made_by_login)
              VALUES (?1, ?2, ?3, 1, 'owner', ?4, ?5, ?6) RETURNING id",
    )
    .bind(&name)
    .bind(crate::config::hash_api_key(door_plain))
    .bind(door_plain)
    .bind(format!("Sign-in key of account '{login}'"))
    .bind(id)
    .bind(made_by)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query("UPDATE accounts SET door_key_id = ?2 WHERE id = ?1")
        .bind(id)
        .bind(key_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok((id, key_id))
}

/// Change an account's display name and enabled flag; `None` keeps each.
/// Answers the rows written.
pub async fn set_account(
    pool: &SqlitePool,
    id: i64,
    display_name: Option<&str>,
    enabled: Option<bool>,
) -> DbResult<u64> {
    Ok(sqlx::query(
        "UPDATE accounts SET display_name = COALESCE(?2, display_name),
                             enabled = COALESCE(?3, enabled)
          WHERE id = ?1",
    )
    .bind(id)
    .bind(display_name)
    .bind(enabled.map(i64::from))
    .execute(pool)
    .await?
    .rows_affected())
}

/// Delete an account with its owner keys, sessions, link and known
/// browsers (§5.6). Keys of other kinds it made keep their `account_id`
/// and `made_by_login`, so they read "made by a deleted account". Answers
/// the ids of the owner keys that went.
pub async fn delete_account(pool: &SqlitePool, id: i64) -> DbResult<Vec<i64>> {
    let mut tx = begin_write(pool).await?;
    let keys: Vec<i64> =
        sqlx::query_scalar("SELECT id FROM api_keys WHERE kind = 'owner' AND account_id = ?1")
            .bind(id)
            .fetch_all(&mut *tx)
            .await?;
    sqlx::query("DELETE FROM api_keys WHERE kind = 'owner' AND account_id = ?1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    for statement in [
        "DELETE FROM sessions WHERE account_id = ?1",
        "DELETE FROM account_tokens WHERE account_id = ?1",
        "DELETE FROM known_browsers WHERE account_id = ?1",
    ] {
        sqlx::query(statement).bind(id).execute(&mut *tx).await?;
    }
    sqlx::query("DELETE FROM accounts WHERE id = ?1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(keys)
}

/// Set an account's password (and, for account 1's first one, its login,
/// §5.3), delete its set-password link, and reset its failure count, in
/// one transaction. The caller ends the sessions.
pub async fn set_account_password(
    pool: &SqlitePool,
    id: i64,
    hash: &str,
    login: Option<&str>,
) -> DbResult<()> {
    let mut tx = begin_write(pool).await?;
    sqlx::query(
        "UPDATE accounts SET password_hash = ?2, password_changed_at = ?3,
                             login = COALESCE(?4, login), failed_logins = 0
          WHERE id = ?1",
    )
    .bind(id)
    .bind(hash)
    .bind(crate::accounts::now_rfc3339())
    .bind(login)
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM account_tokens WHERE account_id = ?1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// A good sign-in: `last_login_at` now, the failures back to 0, and, when
/// the hash `verified` was rehashed (D12), the new one, unless the
/// password changed since it was verified (the rehash would undo that
/// change).
pub async fn record_login_ok(
    pool: &SqlitePool,
    id: i64,
    verified: &str,
    rehash: Option<&str>,
) -> DbResult<()> {
    sqlx::query(
        "UPDATE accounts SET last_login_at = ?2, failed_logins = 0,
                             password_hash = CASE WHEN password_hash IS ?3
                                                  THEN COALESCE(?4, password_hash)
                                                  ELSE password_hash END
          WHERE id = ?1",
    )
    .bind(id)
    .bind(crate::accounts::now_rfc3339())
    .bind(verified)
    .bind(rehash)
    .execute(pool)
    .await?;
    Ok(())
}

/// A failed sign-in for an existing account.
pub async fn record_login_failed(pool: &SqlitePool, id: i64) -> DbResult<()> {
    sqlx::query("UPDATE accounts SET failed_logins = failed_logins + 1 WHERE id = ?1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Each account's failed sign-ins and last sign-in, as the store has them
/// now: the Accounts card reads these counters here rather than from the
/// snapshot, which no sign-in reloads.
pub async fn account_counters(
    pool: &SqlitePool,
) -> DbResult<std::collections::HashMap<i64, (i64, Option<String>)>> {
    let rows = sqlx::query("SELECT id, failed_logins, last_login_at FROM accounts")
        .fetch_all(pool)
        .await?;
    Ok(rows
        .iter()
        .map(|r| {
            (
                r.get("id"),
                (r.get("failed_logins"), r.get("last_login_at")),
            )
        })
        .collect())
}

// ---------------------------------------------------------------------------
// Sessions (§5.2)
// ---------------------------------------------------------------------------

/// A session row to insert.
pub struct NewSession<'a> {
    pub token_hash: &'a str,
    pub account_id: i64,
    pub expires_at: &'a str,
    pub peer: &'a str,
    pub user_agent: &'a str,
}

/// One `sessions` row.
#[derive(Debug, Clone)]
pub struct SessionRow {
    pub id: i64,
    pub token_hash: String,
    pub account_id: i64,
    pub created_at: String,
    pub last_seen_at: String,
    pub expires_at: String,
    pub peer: String,
    pub user_agent: String,
}

/// Insert a session row while its account is enabled and, for a password
/// sign-in, still holds the hash it was verified against (`proof`): one
/// `INSERT … SELECT`, in a write transaction that first deletes the expired
/// rows. `None` when the account no longer qualifies.
pub async fn insert_session(
    pool: &SqlitePool,
    s: &NewSession<'_>,
    proof: Proof<'_>,
) -> DbResult<Option<i64>> {
    let now = crate::accounts::now_rfc3339();
    let (check, hash) = match proof {
        Proof::Password(h) => (1, Some(h)),
        Proof::Credential => (0, None),
    };
    let mut tx = begin_write(pool).await?;
    sqlx::query("DELETE FROM sessions WHERE expires_at <= ?1")
        .bind(&now)
        .execute(&mut *tx)
        .await?;
    let id: Option<i64> = sqlx::query_scalar(
        "INSERT INTO sessions (token_hash, account_id, created_at, last_seen_at, expires_at,
                               peer, user_agent)
              SELECT ?1, id, ?3, ?3, ?4, ?5, ?6 FROM accounts
               WHERE id = ?2 AND enabled = 1 AND (?7 = 0 OR password_hash IS ?8)
           RETURNING id",
    )
    .bind(s.token_hash)
    .bind(s.account_id)
    .bind(&now)
    .bind(s.expires_at)
    .bind(s.peer)
    .bind(s.user_agent)
    .bind(check)
    .bind(hash)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(id)
}

/// The account session `id` belongs to, while its row exists.
pub async fn session_account(pool: &SqlitePool, id: i64) -> DbResult<Option<i64>> {
    Ok(
        sqlx::query_scalar("SELECT account_id FROM sessions WHERE id = ?1")
            .bind(id)
            .fetch_optional(pool)
            .await?,
    )
}

/// Every session row.
pub async fn list_sessions(pool: &SqlitePool) -> DbResult<Vec<SessionRow>> {
    let rows = sqlx::query("SELECT * FROM sessions ORDER BY id")
        .fetch_all(pool)
        .await?;
    Ok(rows
        .iter()
        .map(|r| SessionRow {
            id: r.get("id"),
            token_hash: r.get("token_hash"),
            account_id: r.get("account_id"),
            created_at: r.get("created_at"),
            last_seen_at: r.get("last_seen_at"),
            expires_at: r.get("expires_at"),
            peer: r.get("peer"),
            user_agent: r.get("user_agent"),
        })
        .collect())
}

/// Delete the sessions `which` names; answers how many rows went.
pub async fn delete_sessions(pool: &SqlitePool, which: Which) -> DbResult<u64> {
    let res = match which {
        Which::One(id) => {
            sqlx::query("DELETE FROM sessions WHERE id = ?1")
                .bind(id)
                .execute(pool)
                .await?
        }
        Which::Account { account_id, except } => {
            sqlx::query("DELETE FROM sessions WHERE account_id = ?1 AND id IS NOT ?2")
                .bind(account_id)
                .bind(except)
                .execute(pool)
                .await?
        }
        Which::All => sqlx::query("DELETE FROM sessions").execute(pool).await?,
    };
    Ok(res.rows_affected())
}

pub async fn touch_session(pool: &SqlitePool, id: i64) -> DbResult<()> {
    sqlx::query("UPDATE sessions SET last_seen_at = ?2 WHERE id = ?1")
        .bind(id)
        .bind(crate::accounts::now_rfc3339())
        .execute(pool)
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Set-password links (§5.5) and known browsers (§5.4)
// ---------------------------------------------------------------------------

/// The account's set-password link, replacing an earlier one.
pub async fn put_link_token(
    pool: &SqlitePool,
    account_id: i64,
    token_hash: &str,
    expires_at: &str,
) -> DbResult<()> {
    sqlx::query(
        "INSERT INTO account_tokens (token_hash, account_id, expires_at) VALUES (?1, ?2, ?3)
         ON CONFLICT (account_id) DO UPDATE SET token_hash = excluded.token_hash,
                                                expires_at = excluded.expires_at",
    )
    .bind(token_hash)
    .bind(account_id)
    .bind(expires_at)
    .execute(pool)
    .await?;
    Ok(())
}

/// The account and expiry a link's token hash names.
pub async fn link_token(pool: &SqlitePool, token_hash: &str) -> DbResult<Option<(i64, String)>> {
    let row =
        sqlx::query("SELECT account_id, expires_at FROM account_tokens WHERE token_hash = ?1")
            .bind(token_hash)
            .fetch_optional(pool)
            .await?;
    Ok(row.map(|r| (r.get("account_id"), r.get("expires_at"))))
}

/// What spending a set-password link found ([`set_password_by_link`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkSpent {
    /// The password is set on this account; the link is gone.
    Set(i64),
    /// No live link with this token: unknown, used or expired.
    Invalid,
    /// The link's account is disabled (its login): nothing written, the
    /// link kept.
    Disabled(String),
}

/// Spend a set-password link and set its account's password (and, for
/// account 1's first one, its login, §5.3), in one write transaction: the
/// link's row is deleted by its token while it is live, and only the
/// statement that deleted it goes on, so of two submits of one link exactly
/// one sets a password.
pub async fn set_password_by_link(
    pool: &SqlitePool,
    token_hash: &str,
    hash: &str,
    login: Option<&str>,
) -> DbResult<LinkSpent> {
    let now = crate::accounts::now_rfc3339();
    let mut tx = begin_write(pool).await?;
    let account: Option<i64> = sqlx::query_scalar(
        "DELETE FROM account_tokens WHERE token_hash = ?1 AND expires_at > ?2
         RETURNING account_id",
    )
    .bind(token_hash)
    .bind(&now)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(id) = account else {
        return Ok(LinkSpent::Invalid);
    };
    let row = sqlx::query("SELECT login, enabled FROM accounts WHERE id = ?1")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(row) = row else {
        return Ok(LinkSpent::Invalid);
    };
    if row.get::<i64, _>("enabled") == 0 {
        // Dropped without a commit: the link stays for when it is enabled.
        return Ok(LinkSpent::Disabled(row.get("login")));
    }
    sqlx::query(
        "UPDATE accounts SET password_hash = ?2, password_changed_at = ?3,
                             login = COALESCE(?4, login), failed_logins = 0
          WHERE id = ?1",
    )
    .bind(id)
    .bind(hash)
    .bind(&now)
    .bind(login)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(LinkSpent::Set(id))
}

/// A known browser of `account_id` (§5.4), for `accounts.session_days`.
pub async fn put_known_browser(
    pool: &SqlitePool,
    token_hash: &str,
    account_id: i64,
    expires_at: &str,
) -> DbResult<()> {
    let mut tx = begin_write(pool).await?;
    sqlx::query("DELETE FROM known_browsers WHERE expires_at < ?1")
        .bind(crate::accounts::now_rfc3339())
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO known_browsers (token_hash, account_id, expires_at) VALUES (?1, ?2, ?3)
         ON CONFLICT (token_hash) DO UPDATE SET account_id = excluded.account_id,
                                                expires_at = excluded.expires_at",
    )
    .bind(token_hash)
    .bind(account_id)
    .bind(expires_at)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// The account a known browser's token hash belongs to, while it lasts.
pub async fn known_browser(pool: &SqlitePool, token_hash: &str) -> DbResult<Option<i64>> {
    Ok(sqlx::query_scalar(
        "SELECT account_id FROM known_browsers WHERE token_hash = ?1 AND expires_at > ?2",
    )
    .bind(token_hash)
    .bind(crate::accounts::now_rfc3339())
    .fetch_optional(pool)
    .await?)
}

/// The counts the desktop's switch names (§5.11): accounts, known
/// browsers, set-password links.
pub async fn account_counts(pool: &SqlitePool) -> DbResult<(i64, i64, i64)> {
    let accounts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM accounts")
        .fetch_one(pool)
        .await?;
    let known: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM known_browsers")
        .fetch_one(pool)
        .await?;
    let links: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM account_tokens")
        .fetch_one(pool)
        .await?;
    Ok((accounts, known, links))
}
