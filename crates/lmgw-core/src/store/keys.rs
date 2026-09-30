//! API keys

use sqlx::{Row, SqlitePool};

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
/// Returns the number of rows written, so a caller can tell "id does not
/// exist" from "wrote nothing because nothing changed".
pub async fn update_key_policy(
    pool: &SqlitePool,
    id: i64,
    policy: &KeyPolicy,
    enabled: bool,
    note: &str,
) -> DbResult<u64> {
    let res = sqlx::query(
        "UPDATE api_keys
            SET enabled = ?2, scope_mode = ?3, scope_patterns = ?4, budget_micro = ?5,
                budget_period = ?6, rpm_limit = ?7, tpm_limit = ?8, concurrency_limit = ?9,
                expires_at = ?10, note = ?11, tool_scope_mode = ?12, tool_scope_patterns = ?13
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
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

pub async fn delete_api_key(pool: &SqlitePool, id: i64) -> DbResult<()> {
    sqlx::query("DELETE FROM api_keys WHERE id = ?1")
        .bind(id)
        .execute(pool)
        .await?;
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
