//! Around the migrator: every foreign key still points at a row (SQLite's
//! twelve-step table rebuild, step 10).
//!
//! A rebuild runs with `PRAGMA foreign_keys = OFF` (0058 rebuilds `upstreams`,
//! which `models` and `hidden_passthrough_models` reference), so nothing
//! checks the references while the table is replaced. `PRAGMA
//! foreign_key_check` inside the migration file would only return rows that
//! nobody reads, so it runs here, over the whole database, and a database
//! whose references point at nothing does not start: the rows are named, and
//! nothing is deleted.
//!
//! **When.** Every pool connection has foreign keys on, so only a migration
//! can leave a dangling reference. A start that applies one sets a durable
//! marker ([`PENDING_KEY`] in the `settings` key/value table) — before the
//! migrator when the table is there, so a crash between a migration's commit
//! and the check cannot skip it — and the check runs on every start while the
//! marker is set. Only a clean check clears it: a refused start refuses again
//! on every restart until the named rows are fixed, and a database with
//! nothing pending pays nothing on an ordinary start.

use std::collections::HashSet;

use sqlx::{Row, SqlitePool};

use super::table_exists;
use crate::store::{delete_kv, get_kv, set_kv};

/// The marker: a migration was applied, and the references have not been
/// found whole since.
const PENDING_KEY: &str = "store:foreign_key_check_pending";

/// Before the migrator: set the marker when a migration is about to run.
/// Returns how many migrations are recorded, for [`check_after_migrations`].
pub(in crate::store) async fn mark_before_migrations(
    pool: &SqlitePool,
    migrator: &sqlx::migrate::Migrator,
) -> anyhow::Result<i64> {
    let applied = applied_migrations(pool).await?;
    // A fresh database has no `settings` table yet; it is marked after the
    // migrator instead, and holds no rows to dangle anyway.
    if table_exists(pool, "settings").await? && pending(pool, migrator).await? {
        set_kv(pool, PENDING_KEY, "1").await?;
    }
    Ok(applied)
}

/// After the migrator: mark when it applied anything (`applied_before`: the
/// count [`mark_before_migrations`] returned), then check while marked, and
/// clear the marker only after a clean check.
pub(in crate::store) async fn check_after_migrations(
    pool: &SqlitePool,
    applied_before: i64,
) -> anyhow::Result<()> {
    if applied_migrations(pool).await? > applied_before {
        set_kv(pool, PENDING_KEY, "1").await?;
    }
    if get_kv(pool, PENDING_KEY).await?.is_none() {
        return Ok(());
    }
    refuse_dangling_foreign_keys(pool).await?;
    delete_kv(pool, PENDING_KEY).await?;
    Ok(())
}

/// How many migrations `_sqlx_migrations` records (0 before the first).
async fn applied_migrations(pool: &SqlitePool) -> anyhow::Result<i64> {
    if !table_exists(pool, "_sqlx_migrations").await? {
        return Ok(0);
    }
    Ok(sqlx::query_scalar("SELECT COUNT(*) FROM _sqlx_migrations")
        .fetch_one(pool)
        .await?)
}

/// Whether `migrator` holds an up migration the database has not recorded.
async fn pending(pool: &SqlitePool, migrator: &sqlx::migrate::Migrator) -> anyhow::Result<bool> {
    let recorded: HashSet<i64> = if table_exists(pool, "_sqlx_migrations").await? {
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations")
            .fetch_all(pool)
            .await?
            .into_iter()
            .collect()
    } else {
        HashSet::new()
    };
    Ok(migrator
        .migrations
        .iter()
        .any(|m| m.migration_type.is_up_migration() && !recorded.contains(&m.version)))
}

/// Refuse to start, naming the rows, when a foreign key points at a row that
/// is not there.
async fn refuse_dangling_foreign_keys(pool: &SqlitePool) -> anyhow::Result<()> {
    let rows = sqlx::query("PRAGMA foreign_key_check")
        .fetch_all(pool)
        .await?;
    if rows.is_empty() {
        return Ok(());
    }
    let offenders: Vec<String> = rows
        .iter()
        .map(|r| {
            // `rowid` is NULL for a WITHOUT ROWID table.
            let rowid = r
                .try_get::<Option<i64>, _>("rowid")
                .ok()
                .flatten()
                .map_or_else(|| "a row".to_string(), |id| format!("row {id}"));
            format!(
                "{} {rowid} → {} (missing)",
                r.get::<String, _>("table"),
                r.get::<String, _>("parent"),
            )
        })
        .collect();
    anyhow::bail!(
        "{} database row(s) point at rows that do not exist: {}. lmgw does not start on a \
         database whose references are broken after a migration, and deleted nothing; it \
         refuses on every start until they are fixed. Repoint or delete those rows (sqlite3 on \
         the database file, with lmgw stopped) and start it again.",
        offenders.len(),
        offenders.join(", "),
    )
}
