//! The agent catalog (agent-catalog design §3)

use sqlx::{Row, SqlitePool};

use super::*;

/// Shipped embedded and seeded once; survives an edit so "Reset to shipped" has
/// something to go back to.
pub const AGENT_SOURCE_BUILTIN: &str = "builtin";
/// Arrived through `POST /api/agents/import` or `lmgw__agent_set`.
pub const AGENT_SOURCE_IMPORTED: &str = "imported";
/// Written in the Definition editor.
pub const AGENT_SOURCE_AUTHORED: &str = "authored";

/// One catalog row, verbatim. The manifest stays text here: parsing it is
/// [`crate::agents::Agent::from_row`]'s job, and a row written by a newer build
/// has to be *readable* by an older one even when it is not *runnable* (§4.1).
#[derive(Debug, Clone, PartialEq)]
pub struct AgentRow {
    pub id: String,
    pub manifest: String,
    /// JSON object of stored config values, secrets included (§2.6).
    pub config: String,
    pub enabled: bool,
    /// [`AGENT_SOURCE_BUILTIN`], [`AGENT_SOURCE_IMPORTED`] or
    /// [`AGENT_SOURCE_AUTHORED`].
    pub source: String,
    /// Where the package came from, as JSON — `{}` for a row nobody installed
    /// from an image (container-runtime §3.4, §5). Parsed by
    /// [`crate::agents::package::Provenance`]; text here for the same reason
    /// `manifest` is text.
    pub provenance: String,
    /// The dev-server override (§3.4): while it is set, the agent's origin
    /// goes there and **no container is started** for the app. Never exported.
    pub dev_url: Option<String>,
    /// The paired device whose `lmgw__agent_set` or `lmgw__agent_install`
    /// created the row (migration 0069, client-apps design L5's note,
    /// 2026-10-07); `None` for the owner's, a built-in and every row from
    /// before. A device may replace or delete only an agent it created, and
    /// the owner's write to the row adopts it (`None` again, [`adopt_agent`]).
    pub created_by_key: Option<i64>,
    pub created_at: String,
    pub updated_at: String,
}

fn agent_from_row(row: &sqlx::sqlite::SqliteRow) -> AgentRow {
    AgentRow {
        id: row.get("id"),
        manifest: row.get("manifest"),
        config: row.get("config"),
        enabled: row.get::<i64, _>("enabled") != 0,
        source: row.get("source"),
        provenance: row.get("provenance"),
        dev_url: row.get("dev_url"),
        created_by_key: row.get("created_by_key"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

/// The whole catalog, by id. There is no limit and no paging: the catalog is
/// the list of agents an owner installed, and hiding the tail of it behind an
/// invisible cap would be a worse answer than a long page.
pub async fn list_agents(pool: &SqlitePool) -> DbResult<Vec<AgentRow>> {
    let rows = sqlx::query("SELECT * FROM agents ORDER BY id")
        .fetch_all(pool)
        .await?;
    Ok(rows.iter().map(agent_from_row).collect())
}

pub async fn get_agent(pool: &SqlitePool, id: &str) -> DbResult<Option<AgentRow>> {
    let row = sqlx::query("SELECT * FROM agents WHERE id = ?1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(agent_from_row))
}

/// Create a row. Fails on a duplicate id rather than overwriting — "replace an
/// existing agent" is a decision the import path makes explicitly (`replace=1`,
/// §5), never a side effect of a save.
pub async fn insert_agent(
    pool: &SqlitePool,
    id: &str,
    manifest: &str,
    source: &str,
) -> DbResult<()> {
    insert_agent_by(pool, id, manifest, source, None).await
}

/// [`insert_agent`], recording the paired device that created it
/// (`AgentRow::created_by_key`).
pub async fn insert_agent_by(
    pool: &SqlitePool,
    id: &str,
    manifest: &str,
    source: &str,
    created_by_key: Option<i64>,
) -> DbResult<()> {
    sqlx::query(
        "INSERT INTO agents (id, manifest, source, created_by_key) VALUES (?1, ?2, ?3, ?4)",
    )
    .bind(id)
    .bind(manifest)
    .bind(source)
    .bind(created_by_key)
    .execute(pool)
    .await?;
    Ok(())
}

/// Replace the manifest of an existing agent, **keeping its config**: a
/// manifest update is not a reason to lose the taxonomy someone tuned (§5).
/// `source` is left as it was, so an edited built-in stays a built-in.
pub async fn update_agent_manifest(pool: &SqlitePool, id: &str, manifest: &str) -> DbResult<u64> {
    let res =
        sqlx::query("UPDATE agents SET manifest = ?2, updated_at = datetime('now') WHERE id = ?1")
            .bind(id)
            .bind(manifest)
            .execute(pool)
            .await?;
    Ok(res.rows_affected())
}

/// [`update_agent_manifest`], plus the pruning [`put_builtin_manifest`] already
/// does for a shipped upgrade (container-runtime §5.1, WP5 review).
///
/// "Keeping the config" cannot mean keeping a value whose **field the new
/// manifest no longer declares**: `validate_values` refuses an undeclared key,
/// so such a row fails on every run, every apply and every `agent_config_set`
/// that touches it — a catalog entry that cannot be used and cannot be fixed
/// from the form that is supposed to fix it. The stale keys go with the
/// manifest that declared them, in the same transaction, and the caller names
/// them in its report.
///
/// One transaction, not two calls, for `put_builtin_manifest`'s reason: a row
/// carrying the new manifest against the old config is exactly the broken state
/// this exists to prevent, and a crash between two statements would leave it.
///
/// `by_device` is who writes it: the device that created the row keeps it
/// its own, and anyone else's write — the owner's — adopts it
/// (`created_by_key` set to `by_device`, so `NULL` for the owner; the
/// branch review's verification, V-5). The import path refuses a device's
/// write over a row it did not create before this runs.
pub async fn update_agent_manifest_pruned(
    pool: &SqlitePool,
    id: &str,
    manifest: &str,
    keep_config_fields: &[AgentConfigField],
    by_device: Option<i64>,
) -> DbResult<Vec<String>> {
    let mut tx = super::begin_write(pool).await?;
    let dropped = prune_config_tx(&mut tx, id, keep_config_fields).await?;
    sqlx::query(
        "UPDATE agents SET manifest = ?2, created_by_key = ?3, updated_at = datetime('now') \
         WHERE id = ?1",
    )
    .bind(id)
    .bind(manifest)
    .bind(by_device)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(dropped)
}

/// One config field as pruning reads it: its name, and whether the schema
/// declaring it marked it `format: "secret"`.
///
/// A pair rather than the whole [`manifest::Field`](crate::agents::manifest::Field)
/// because those are the only two properties a prune decision turns on, and the
/// caller already has the parsed manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentConfigField {
    pub name: String,
    pub secret: bool,
}

impl AgentConfigField {
    /// The list the replace paths hand over, from a parsed manifest.
    pub fn of(m: &crate::agents::manifest::Manifest) -> Vec<Self> {
        m.fields()
            .unwrap_or_default()
            .into_iter()
            .map(|f| Self {
                secret: f.is_secret(),
                name: f.name,
            })
            .collect()
    }
}

/// Drop every stored config key the new field list does not declare **as the
/// same kind of field**, inside a transaction. Returns what it removed, in the
/// order the column held it.
///
/// Two reasons a value goes, not one:
///
///  - the new manifest does not declare the key at all — `validate_values`
///    refuses an undeclared key, so leaving it behind makes the row unusable;
///  - the old manifest declared it `format: "secret"` and the new one does
///    **not** (final review, §5.1). The stored string is a credential that was
///    written into a masked field, and every reader downstream masks by asking
///    the *current* schema: `AgentDetail.config`, `lmgw__agent_get`, an export
///    with config, the Config form and the container's `input.json` would all
///    print it in the clear the moment the field stopped saying `secret`. A
///    manifest replace is not a reveal, so the value goes with the schema that
///    promised to hide it and the caller names it like any other dropped key.
async fn prune_config_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    id: &str,
    fields: &[AgentConfigField],
) -> DbResult<Vec<String>> {
    let Some(row) = sqlx::query("SELECT config, manifest FROM agents WHERE id = ?1")
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?
    else {
        return Ok(Vec::new());
    };
    let stored: String = row.get("config");
    let Ok(serde_json::Value::Object(mut m)) = serde_json::from_str(&stored) else {
        return Ok(Vec::new());
    };
    // What the *outgoing* schema said about each key. A manifest this build
    // cannot parse answers nothing, and then only the undeclared-key rule
    // applies — the conservative direction, since an unparseable manifest is
    // being replaced by one that does parse.
    let old_manifest: String = row.get("manifest");
    let was: Vec<AgentConfigField> = crate::agents::manifest::load(&old_manifest)
        .ok()
        .map(|m| AgentConfigField::of(&m))
        .unwrap_or_default();
    let dropped: Vec<String> = m
        .keys()
        .filter(|k| match fields.iter().find(|f| &f.name == *k) {
            None => true,
            Some(now) => !now.secret && was.iter().any(|f| &f.name == *k && f.secret),
        })
        .cloned()
        .collect();
    if dropped.is_empty() {
        return Ok(Vec::new());
    }
    for k in &dropped {
        m.remove(k);
    }
    sqlx::query("UPDATE agents SET config = ?2 WHERE id = ?1")
        .bind(id)
        .bind(serde_json::Value::Object(m).to_string())
        .execute(&mut **tx)
        .await?;
    Ok(dropped)
}

/// Write a built-in's manifest **and** the seeded-hash map in one transaction
/// (container-runtime §5.1), returning the config keys it had to drop.
///
/// Not two calls: a row that carried the *new* manifest against the *old*
/// recorded hash would read as "edited" on the next start and stay pinned out
/// of the upgrade path for good — the exact trap §5.1 exists to close. The row
/// is inserted when it is not there, which is the reset-after-delete case.
///
/// `keep_config_fields` is the new manifest's config field names; `None` leaves
/// the config exactly as it is. Passing them prunes values the new schema no
/// longer declares, in the same transaction, so a dropped field cannot turn a
/// silent upgrade into a Start that fails on the config it was told to keep.
pub async fn put_builtin_manifest(
    pool: &SqlitePool,
    id: &str,
    manifest: &str,
    kv_key: &str,
    kv_value: &str,
    keep_config_fields: Option<&[AgentConfigField]>,
) -> DbResult<Vec<String>> {
    let mut tx = super::begin_write(pool).await?;
    // Config is kept across the write — that is the whole point — but a field
    // the new manifest no longer declares is not "kept", it is a value the
    // schema will refuse on the next Start ("'x' is not a config field"). An
    // upgrade nobody pressed must not leave the agent in that state, so the
    // stale keys go with the manifest that declared them and the caller names
    // them in the log. The same pruning every other manifest-replace path does
    // ([`update_agent_manifest_pruned`]), one implementation.
    let mut dropped: Vec<String> = Vec::new();
    if let Some(fields) = keep_config_fields {
        dropped = prune_config_tx(&mut tx, id, fields).await?;
    }
    let res =
        sqlx::query("UPDATE agents SET manifest = ?2, updated_at = datetime('now') WHERE id = ?1")
            .bind(id)
            .bind(manifest)
            .execute(&mut *tx)
            .await?;
    if res.rows_affected() == 0 {
        sqlx::query("INSERT INTO agents (id, manifest, source) VALUES (?1, ?2, ?3)")
            .bind(id)
            .bind(manifest)
            .bind(AGENT_SOURCE_BUILTIN)
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query(
        "INSERT INTO settings (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = ?2",
    )
    .bind(kv_key)
    .bind(kv_value)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(dropped)
}

pub async fn set_agent_config(pool: &SqlitePool, id: &str, config: &str) -> DbResult<u64> {
    let res =
        sqlx::query("UPDATE agents SET config = ?2, updated_at = datetime('now') WHERE id = ?1")
            .bind(id)
            .bind(config)
            .execute(pool)
            .await?;
    Ok(res.rows_affected())
}

/// Record where this row's package came from (container-runtime §3.4).
///
/// Its own column and its own write, never folded into the manifest write: an
/// install writes the manifest through the ordinary import path — the one that
/// validates, warns and keeps the config — and provenance is what that path
/// knows nothing about. `updated_at` moves with it, because "this agent was
/// re-pulled" is a change to the row an owner should see on the card.
pub async fn set_agent_provenance(pool: &SqlitePool, id: &str, provenance: &str) -> DbResult<u64> {
    let res = sqlx::query(
        "UPDATE agents SET provenance = ?2, updated_at = datetime('now') WHERE id = ?1",
    )
    .bind(id)
    .bind(provenance)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Point this agent's app at a dev server, or (`None`) back at its image.
///
/// `NULL` and not the empty string: "there is no override" is one state, and a
/// column that could hold `''` as well would make every reader test for two.
pub async fn set_agent_dev_url(pool: &SqlitePool, id: &str, url: Option<&str>) -> DbResult<u64> {
    let res =
        sqlx::query("UPDATE agents SET dev_url = ?2, updated_at = datetime('now') WHERE id = ?1")
            .bind(id)
            .bind(url)
            .execute(pool)
            .await?;
    Ok(res.rows_affected())
}

/// Every agent id that currently carries a `dev_url`, mapped to it.
///
/// The one question two hot readers ask about a column that is not in the
/// [`Snapshot`](crate::config::Snapshot) — the MCP aggregate's "may I list this agent's tools?"
/// (container-runtime §3.4) and the MCP page's status detail. A single scan of
/// a table with a handful of rows, rather than a second cache that could
/// disagree with the column.
pub async fn agent_dev_urls(
    pool: &SqlitePool,
) -> DbResult<std::collections::HashMap<String, String>> {
    let rows = sqlx::query("SELECT id, dev_url FROM agents WHERE dev_url IS NOT NULL")
        .fetch_all(pool)
        .await?;
    Ok(rows
        .iter()
        .filter_map(|r| {
            let url: String = r.get("dev_url");
            let url = url.trim().to_string();
            (!url.is_empty()).then(|| (r.get::<String, _>("id"), url))
        })
        .collect())
}

pub async fn set_agent_enabled(pool: &SqlitePool, id: &str, enabled: bool) -> DbResult<u64> {
    let res =
        sqlx::query("UPDATE agents SET enabled = ?2, updated_at = datetime('now') WHERE id = ?1")
            .bind(id)
            .bind(i64::from(enabled))
            .execute(pool)
            .await?;
    Ok(res.rows_affected())
}

pub async fn delete_agent(pool: &SqlitePool, id: &str) -> DbResult<bool> {
    let res = sqlx::query("DELETE FROM agents WHERE id = ?1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}

/// [`delete_agent`] for paired device `device`: only while the row is still
/// the device's own (`AgentRow::created_by_key`), checked in the statement
/// itself, so an owner's write that adopts it ([`adopt_agent`]) between a
/// read and this delete keeps the row (the branch review's verification,
/// V-5). Whether a row went.
pub async fn delete_agent_created_by(pool: &SqlitePool, id: &str, device: i64) -> DbResult<bool> {
    let res = sqlx::query("DELETE FROM agents WHERE id = ?1 AND created_by_key = ?2")
        .bind(id)
        .bind(device)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}

/// The owner wrote agent `id` (the dashboard, or an owner's own tool call):
/// it is the owner's from now on, `created_by_key` back to `NULL`, so the
/// device that created it may no longer replace or delete what the owner
/// has since reviewed (the branch review's verification, V-5). A row that
/// is not there, or is the owner's already, is left alone.
pub async fn adopt_agent(pool: &SqlitePool, id: &str) -> DbResult<()> {
    sqlx::query(
        "UPDATE agents SET created_by_key = NULL WHERE id = ?1 AND created_by_key IS NOT NULL",
    )
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Jobs of one kind carrying one key, newest first — "runs of this agent"
/// (§3). Finished rows keep the key, which is what makes this the run history
/// rather than just the live one. `limit <= 0` means every row retention has
/// kept, the same convention [`list_jobs`] uses.
pub async fn list_jobs_by_key(
    pool: &SqlitePool,
    kind: &str,
    key: &str,
    limit: i64,
) -> DbResult<Vec<JobRow>> {
    let rows = sqlx::query(
        "SELECT * FROM jobs WHERE kind = ?1 AND key = ?2
         ORDER BY id DESC
         LIMIT (CASE WHEN ?3 > 0 THEN ?3 ELSE -1 END)",
    )
    .bind(kind)
    .bind(key)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(job_from_row).collect())
}

/// Unlink every Chat thread an agent opened, without touching the threads.
///
/// What `agent_delete` does about `chat_threads.agent_id`, which has no foreign
/// key: a conversation is the owner's data and an agent is a preset, so
/// deleting the preset must not delete what was said through it. The threads
/// stay, listed on the Chat page as any other, and stop claiming an agent that
/// is no longer in the catalog. Returns how many were unlinked, so the op can
/// say it.
///
/// Each thread unlinked is recorded in the change feed (`thread.updated`),
/// as the owner's: only the owner deletes an agent.
pub async fn clear_chat_thread_agent(pool: &SqlitePool, agent_id: &str) -> DbResult<u64> {
    let mut tx = super::begin_write(pool).await?;
    let ids: Vec<i64> = sqlx::query_scalar(
        "UPDATE chat_threads SET agent_id = NULL WHERE agent_id = ?1 RETURNING id",
    )
    .bind(agent_id)
    .fetch_all(&mut *tx)
    .await?;
    super::feed::record_threads_updated(&mut tx, &ids, Some(super::feed::BY_OWNER)).await?;
    tx.commit().await?;
    Ok(ids.len() as u64)
}

/// How many Chat threads a `chat` agent has opened (§2.5) — the card's "3
/// threads" line.
pub async fn count_chat_threads_by_agent(pool: &SqlitePool, agent_id: &str) -> DbResult<i64> {
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM chat_threads WHERE agent_id = ?1")
        .bind(agent_id)
        .fetch_one(pool)
        .await?;
    Ok(n)
}
