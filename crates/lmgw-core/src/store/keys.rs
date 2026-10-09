//! API keys

use sqlx::{Row, SqliteConnection, SqlitePool};

use crate::config::KeyPolicy;

use super::*;

pub async fn insert_api_key(pool: &SqlitePool, name: &str, key_hash: &str) -> DbResult<i64> {
    let res = sqlx::query("INSERT INTO api_keys (name, key_hash) VALUES (?1, ?2)")
        .bind(name)
        .bind(key_hash)
        .execute(pool)
        .await?;
    Ok(res.last_insert_rowid())
}

/// Write one key's **owner-set** policy: scope, budget, the three limits,
/// expiry, the enabled flag and the note, in one statement (usage-analytics
/// §4.1).
///
/// The counterpart to [`set_agent_key_scope`] and [`set_agent_key_enabled`],
/// which are lmgw writing an agent token's *derived* half to itself. One
/// statement rather than a column at a time, because a save that failed in the
/// middle would leave a row carrying half of one policy and half of another —
/// and the gate reads the row, not the intent.
///
/// `hosts_label` rides along (a device's hosting grant, client-apps design
/// §1.5), and so does `self_admin` (a device's admin-tools level, L3/L5):
/// `None` and `off` on every other kind, which the table's CHECKs hold them
/// to. A level that moved (`self_admin.1`: the one before it) is recorded
/// in the Chat change feed in the same transaction
/// (`feed::record_device_reach`), so the device's stream hears it in commit
/// order.
///
/// Returns the number of rows written, so a caller can tell "id does not
/// exist" from "wrote nothing because nothing changed".
pub async fn update_key_policy(
    pool: &SqlitePool,
    id: i64,
    policy: &KeyPolicy,
    enabled: bool,
    note: &str,
    hosts_label: Option<&str>,
    (self_admin, was): (
        crate::config::DeviceAdmin,
        Option<crate::config::DeviceAdmin>,
    ),
) -> DbResult<u64> {
    let mut tx = super::begin_write(pool).await?;
    // The grant as stored: a write that changes it must get its row.
    let stored: Option<String> =
        sqlx::query_scalar("SELECT hosts_label FROM api_keys WHERE id = ?1")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?
            .flatten();
    let res = sqlx::query(
        "UPDATE api_keys
            SET enabled = ?2, scope_mode = ?3, scope_patterns = ?4, budget_micro = ?5,
                budget_period = ?6, rpm_limit = ?7, tpm_limit = ?8, concurrency_limit = ?9,
                expires_at = ?10, note = ?11, tool_scope_mode = ?12, tool_scope_patterns = ?13,
                hosts_label = ?14, self_admin = ?15
          WHERE id = ?1",
    )
    .bind(id)
    .bind(i64::from(enabled))
    .bind(policy.scope_mode.as_str())
    .bind(&policy.scope_patterns)
    .bind(policy.budget_micro)
    .bind(policy.budget_period.as_str())
    .bind(policy.rpm_limit)
    .bind(policy.tpm_limit)
    .bind(policy.concurrency_limit)
    .bind(policy.expires_at.as_deref().filter(|e| !e.is_empty()))
    .bind(note)
    .bind(policy.tool_scope_mode.as_str())
    .bind(&policy.tool_scope_patterns)
    .bind(hosts_label)
    .bind(self_admin.column())
    .execute(&mut *tx)
    .await?;
    if let Some(was) = was.filter(|_| res.rows_affected() > 0) {
        super::feed::record_device_reach(&mut tx, id, self_admin, was).await?;
    }
    // A device's hosted-tools row follows its grant, in this transaction
    // (client-apps design §5.2). Only a device row carries a label.
    if res.rows_affected() > 0 {
        let device: Option<String> =
            sqlx::query_scalar("SELECT name FROM api_keys WHERE id = ?1 AND kind = 'device'")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?;
        if let Some(name) = device {
            let granting = stored.as_deref() != hosts_label;
            super::sync_device_server(&mut tx, id, &name, (hosts_label, granting)).await?;
        }
    }
    tx.commit().await?;
    Ok(res.rows_affected())
}

pub async fn delete_api_key(pool: &SqlitePool, id: i64) -> DbResult<()> {
    let mut tx = super::begin_write(pool).await?;
    // A device's hosted-tools row goes with it (client-apps design §5.2).
    super::sync_device_server(&mut tx, id, "", (None, false)).await?;
    // A device is gone for good: the feed says so to the clients it showed
    // the device to, and to the hosts of the tasks it started (MCP Tasks
    // design §4.1). Only the owner deletes keys.
    let device: Option<String> =
        sqlx::query_scalar("SELECT name FROM api_keys WHERE id = ?1 AND kind = 'device'")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    if let Some(name) = device {
        super::feed::record_device_revoked(&mut tx, id, &name, Some(super::feed::BY_OWNER)).await?;
    }
    sqlx::query("DELETE FROM api_keys WHERE id = ?1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Owner keys (principals §3.1)
// ---------------------------------------------------------------------------

/// Insert one owner key, or leave the row that already has that name exactly
/// as it is. `Ok(None)` means "it was already there".
///
/// One statement rather than a read followed by a write, because this is what
/// makes the startup seed (`agents::token::seed_owner_keys`) idempotent: the
/// name's `UNIQUE` index decides, so two gateways racing on one data directory
/// cannot both decide the row is absent and mint two doors.
pub async fn insert_owner_key(
    pool: &SqlitePool,
    name: &str,
    key_hash: &str,
    key_plain: &str,
    enabled: bool,
) -> DbResult<Option<i64>> {
    let row = sqlx::query(
        "INSERT INTO api_keys (name, key_hash, key_plain, enabled, kind, note)
              VALUES (?1, ?2, ?3, ?4, 'owner', ?5)
         ON CONFLICT (name) DO NOTHING
         RETURNING id",
    )
    .bind(name)
    .bind(key_hash)
    .bind(key_plain)
    .bind(i64::from(enabled))
    .bind(format!("Owner credential '{name}'"))
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| r.get("id")))
}

/// Create the owner key, or rewrite **both** credential columns of the one
/// that has that name — which is what rotation is (§3.12): hash and plaintext
/// in one write, so the two can never disagree.
pub async fn upsert_owner_key(
    pool: &SqlitePool,
    name: &str,
    key_hash: &str,
    key_plain: &str,
    enabled: bool,
) -> DbResult<i64> {
    let row = sqlx::query(
        "INSERT INTO api_keys (name, key_hash, key_plain, enabled, kind, note)
              VALUES (?1, ?2, ?3, ?4, 'owner', ?5)
         ON CONFLICT (name)
              DO UPDATE SET key_hash = excluded.key_hash,
                            key_plain = excluded.key_plain,
                            enabled = excluded.enabled
         RETURNING id",
    )
    .bind(name)
    .bind(key_hash)
    .bind(key_plain)
    .bind(i64::from(enabled))
    .bind(format!("Owner credential '{name}'"))
    .fetch_one(pool)
    .await?;
    Ok(row.get("id"))
}

// ---------------------------------------------------------------------------
// Agent tokens (container-runtime §3.1)
// ---------------------------------------------------------------------------

/// One agent's key row: `(id, key_plain)`. `None` means the agent has never
/// been run and has therefore never minted a credential (§3.1).
pub async fn agent_key(pool: &SqlitePool, agent_id: &str) -> DbResult<Option<(i64, String)>> {
    let row =
        sqlx::query("SELECT id, key_plain FROM api_keys WHERE agent_id = ?1 AND kind='agent'")
            .bind(agent_id)
            .fetch_optional(pool)
            .await?;
    Ok(row.map(|r| (r.get("id"), r.get::<String, _>("key_plain"))))
}

/// Create the agent's key, or rewrite **both** columns of the one it has —
/// which is what rotation is: one write, hash and plaintext together, so the
/// two can never disagree (§3.1).
pub async fn upsert_agent_key(
    pool: &SqlitePool,
    agent_id: &str,
    name: &str,
    key_hash: &str,
    key_plain: &str,
    enabled: bool,
) -> DbResult<i64> {
    // `ON CONFLICT (agent_id)` targets the partial unique index from 0032, so
    // the "already has one" branch is the index's job rather than a read
    // followed by a write that races it.
    let row = sqlx::query(
        "INSERT INTO api_keys (name, key_hash, key_plain, enabled, kind, agent_id, note)
              VALUES (?1, ?2, ?3, ?6, 'agent', ?4, ?5)
         ON CONFLICT (agent_id) WHERE agent_id IS NOT NULL
              DO UPDATE SET name = excluded.name,
                            key_hash = excluded.key_hash,
                            key_plain = excluded.key_plain,
                            enabled = excluded.enabled
         RETURNING id",
    )
    .bind(name)
    .bind(key_hash)
    .bind(key_plain)
    .bind(agent_id)
    .bind(format!("Agent '{agent_id}': container and ledger calls"))
    .bind(i64::from(enabled))
    .fetch_one(pool)
    .await?;
    Ok(row.get("id"))
}

/// Rewrite the derived scope (§3.1). Separate from the token itself: scope is
/// recomputed whenever the config changes, and a recompute must never touch
/// the credential a running container is holding.
pub async fn set_agent_key_scope(
    pool: &SqlitePool,
    agent_id: &str,
    scope_mode: &str,
    scope_patterns: &str,
) -> DbResult<()> {
    sqlx::query(
        "UPDATE api_keys SET scope_mode = ?2, scope_patterns = ?3
          WHERE agent_id = ?1 AND kind = 'agent'",
    )
    .bind(agent_id)
    .bind(scope_mode)
    .bind(scope_patterns)
    .execute(pool)
    .await?;
    Ok(())
}

/// Mirror the agent's `enabled` onto its key row, so **Disable is the kill
/// switch** (§3.1): `Snapshot::verify_api_key` already drops a row with
/// `enabled = 0`, which is what makes the token stop working everywhere at
/// once rather than at each call site that remembers to check.
pub async fn set_agent_key_enabled(
    pool: &SqlitePool,
    agent_id: &str,
    enabled: bool,
) -> DbResult<()> {
    sqlx::query("UPDATE api_keys SET enabled = ?2 WHERE agent_id = ?1 AND kind = 'agent'")
        .bind(agent_id)
        .bind(i64::from(enabled))
        .execute(pool)
        .await?;
    Ok(())
}

/// `ON DELETE` is manual (§3.1): deleting the agent deletes its credential.
pub async fn delete_agent_key(pool: &SqlitePool, agent_id: &str) -> DbResult<()> {
    sqlx::query("DELETE FROM api_keys WHERE agent_id = ?1 AND kind = 'agent'")
        .bind(agent_id)
        .execute(pool)
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Device keys (client-apps design §1.1)
// ---------------------------------------------------------------------------

/// Insert one device key with its whole owner-set policy and its hosting
/// label, in one statement: a device is never live, even for a moment, with a
/// scope wider than the one the pairing form confirmed (§11 Q1).
///
/// Hash only (L1): `key_plain` stays NULL, as the table's CHECK requires.
pub async fn insert_device_key(
    pool: &SqlitePool,
    name: &str,
    key_hash: &str,
    policy: &KeyPolicy,
    (hosts_label, self_admin): (Option<&str>, crate::config::DeviceAdmin),
    note: &str,
) -> DbResult<i64> {
    let mut tx = super::begin_write(pool).await?;
    let row = sqlx::query(
        "INSERT INTO api_keys
              (name, key_hash, enabled, kind, scope_mode, scope_patterns, budget_micro,
               budget_period, rpm_limit, tpm_limit, concurrency_limit, expires_at, note,
               tool_scope_mode, tool_scope_patterns, hosts_label, self_admin)
         VALUES (?1, ?2, 1, 'device', ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
         RETURNING id",
    )
    .bind(name)
    .bind(key_hash)
    .bind(policy.scope_mode.as_str())
    .bind(&policy.scope_patterns)
    .bind(policy.budget_micro)
    .bind(policy.budget_period.as_str())
    .bind(policy.rpm_limit)
    .bind(policy.tpm_limit)
    .bind(policy.concurrency_limit)
    .bind(policy.expires_at.as_deref().filter(|e| !e.is_empty()))
    .bind(note)
    .bind(policy.tool_scope_mode.as_str())
    .bind(&policy.tool_scope_patterns)
    .bind(hosts_label)
    .bind(self_admin.column())
    .fetch_one(&mut *tx)
    .await?;
    let id: i64 = row.get("id");
    // Born with its hosted-tools row when it is born with a grant (§5.2).
    if hosts_label.is_some() {
        super::sync_device_server(&mut tx, id, name, (hosts_label, true)).await?;
    }
    tx.commit().await?;
    Ok(id)
}

/// Replace a device key's hash — Rotate on a hash-only row (L1), which is a
/// re-pairing: the old value matches nothing from this write on. The row, its
/// id, its policy and its history stay.
///
/// Returns the number of rows written, so a caller can tell a row that went
/// away from a rotation that happened.
pub async fn set_device_key_hash(pool: &SqlitePool, id: i64, key_hash: &str) -> DbResult<u64> {
    let res = sqlx::query("UPDATE api_keys SET key_hash = ?2 WHERE id = ?1 AND kind = 'device'")
        .bind(id)
        .bind(key_hash)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

/// Stamp `last_seen_at` with now (L15): a device's connection opened or
/// closed. Not part of the snapshot — it moves without a reload, and only the
/// Keys page reads it ([`key_last_seen`]).
pub async fn touch_key_last_seen(pool: &SqlitePool, id: i64) -> DbResult<()> {
    sqlx::query(
        "UPDATE api_keys SET last_seen_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now') WHERE id = ?1",
    )
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

/// When one key's connection last opened or closed, as RFC 3339 UTC.
pub async fn key_last_seen(pool: &SqlitePool, id: i64) -> DbResult<Option<String>> {
    let row = sqlx::query("SELECT last_seen_at FROM api_keys WHERE id = ?1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.and_then(|r| r.get::<Option<String>, _>("last_seen_at")))
}

/// Key `id`'s level of lmgw's admin tools now, as its row says — `off` for a
/// key that is gone, not a device, disabled or expired — whatever the
/// published snapshot still holds (review P-8): what a device's `lmgw__*`
/// call is checked against as well, so a key write that committed is in
/// force before its snapshot is reloaded.
pub async fn device_admin_now(pool: &SqlitePool, id: i64) -> DbResult<crate::config::DeviceAdmin> {
    let mut conn = pool.acquire().await?;
    device_admin_in(&mut conn, id).await
}

/// [`device_admin_now`] on `conn`, a transaction's.
pub async fn device_admin_in(
    conn: &mut SqliteConnection,
    id: i64,
) -> DbResult<crate::config::DeviceAdmin> {
    let row = sqlx::query(
        "SELECT self_admin, enabled, expires_at FROM api_keys WHERE id = ?1 AND kind = 'device'",
    )
    .bind(id)
    .fetch_optional(conn)
    .await?;
    Ok(row
        .filter(|r| {
            let mut key = crate::config::ApiKey::default();
            key.policy.expires_at = r.get("expires_at");
            r.get::<i64, _>("enabled") != 0
                && crate::policy::check_expiry(&key, chrono::Utc::now()).is_ok()
        })
        .map_or(crate::config::DeviceAdmin::Off, |r| {
            crate::config::DeviceAdmin::from_column(r.get("self_admin"))
        }))
}

/// The gateway's self-admin level as the stored settings say now, whatever
/// the published snapshot still holds (client-apps design L3's note,
/// 2026-10-07): checked with [`device_admin_now`] on a device's `lmgw__*`
/// call, so a lowered level is in force from its commit, as a device's own
/// is. A blob that does not parse reads as the default, as `load_settings`
/// reads it.
pub async fn gateway_self_admin_now(pool: &SqlitePool) -> DbResult<crate::config::SelfAdmin> {
    let mut conn = pool.acquire().await?;
    gateway_self_admin_in(&mut conn).await
}

/// [`gateway_self_admin_now`] on `conn`, a transaction's.
///
/// The whole blob, through the parse the snapshot loader uses
/// (`settings_from_row`), and the same defaults while no settings are
/// stored. Read as one field with `json_extract`, a blob that held a valid
/// level but did not parse as settings said that level here and the
/// default in the published snapshot, for good: every device's feed then
/// waited a full keep-alive for a publish that would never say it. A blob
/// that was not JSON at all failed the read.
pub async fn gateway_self_admin_in(
    conn: &mut SqliteConnection,
) -> DbResult<crate::config::SelfAdmin> {
    let raw: Option<String> =
        sqlx::query_scalar("SELECT value FROM settings WHERE key = 'settings'")
            .fetch_optional(conn)
            .await?;
    Ok(super::settings_from_row(raw.as_deref()).self_admin)
}

/// Key `id`'s level of lmgw's admin tools, as its row says now
/// (`ApiKey::self_admin`; `off` for a key that is no device): what a Chat
/// feed opens with, read in the transaction that reads the feed's head
/// (`feed::bounds_and_switch`), so it neither trails nor runs ahead of the
/// records the stream reads.
pub async fn device_self_admin(
    conn: &mut SqliteConnection,
    id: i64,
) -> DbResult<crate::config::DeviceAdmin> {
    let level: Option<i64> =
        sqlx::query_scalar("SELECT self_admin FROM api_keys WHERE id = ?1 AND kind = 'device'")
            .bind(id)
            .fetch_optional(conn)
            .await?;
    Ok(crate::config::DeviceAdmin::from_column(level.unwrap_or(0)))
}
