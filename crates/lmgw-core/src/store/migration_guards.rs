//! Pre-migration guard (migration 0023) and pre-migration repair (migration 0018)

use sqlx::{Row, SqlitePool};

/// Version of `0023_managed_upstreams_go_synthetic.sql`, the migration this
/// guard stands in front of.
const SYNTHETIC_UPSTREAM_MIGRATION: i64 = 23;

/// The two managed `upstreams.name` values 0023 deletes.
const MANAGED_UPSTREAM_NAMES: [&str; 2] = ["llama-aux", "audiocpp"];

/// Refuse the upgrade — loudly, by name — if an alias still points at one of
/// the managed rows migration 0023 removes.
///
/// `models.upstream_id` is `REFERENCES upstreams(id) ON DELETE CASCADE`, so
/// 0023's `DELETE` would take those alias rows with it and say nothing. Per
/// the per-model-containers design §5 this is surfaced, never silent: the
/// owner is told which aliases are in the way and gets to decide, and the
/// gateway does not start until they have. The live deployment was verified to
/// have zero such rows before the design was accepted — this guard exists for
/// honesty about what the migration *can* destroy, not because anything is
/// expected to trip it.
///
/// Deliberately *not* a repair (unlike 0018's): there is no correct automatic
/// answer. An alias onto a local model cannot be repointed anywhere after §5 —
/// local models are not aliasable in any class — so silently dropping it and
/// silently keeping it are equally wrong, and only the owner can pick.
pub(super) async fn refuse_if_aliases_pin_the_managed_upstreams(
    pool: &SqlitePool,
) -> anyhow::Result<()> {
    // A database this process is about to create has neither table.
    if !table_exists(pool, "_sqlx_migrations").await? {
        return Ok(());
    }
    if migration_applied(pool, SYNTHETIC_UPSTREAM_MIGRATION).await? {
        return Ok(());
    }
    if !table_exists(pool, "upstreams").await? || !table_exists(pool, "models").await? {
        return Ok(());
    }
    let rows = sqlx::query(
        "SELECT m.alias AS alias, u.name AS upstream
         FROM models m JOIN upstreams u ON u.id = m.upstream_id
         WHERE u.name IN (?1, ?2)
         ORDER BY m.alias",
    )
    .bind(MANAGED_UPSTREAM_NAMES[0])
    .bind(MANAGED_UPSTREAM_NAMES[1])
    .fetch_all(pool)
    .await?;
    if rows.is_empty() {
        return removal_notice(pool).await;
    }
    let offenders: Vec<String> = rows
        .iter()
        .map(|r| {
            format!(
                "'{}' → {}",
                r.get::<String, _>("alias"),
                r.get::<String, _>("upstream")
            )
        })
        .collect();
    anyhow::bail!(
        "this upgrade removes the managed upstream rows 'llama-aux' and 'audiocpp' \
         (per-model containers §5), but {} alias(es) still point at them: {}. Deleting the \
         rows would delete those aliases with them (ON DELETE CASCADE), so nothing has been \
         changed. Delete or repoint them on the Models page and start lmgw again — local \
         models are reached by their public name ('embed/<model id>', 'audio/<model id>'), \
         not through an alias.",
        offenders.len(),
        offenders.join(", "),
    )
}

/// Say what 0023 is about to delete, before it deletes it.
///
/// The match is on the two managed names alone, so a row an owner created
/// themselves under one of those names goes too. That is a deliberately small
/// blast radius — anything pointing at it has already stopped the upgrade
/// above — but it is still a row disappearing, and a row must never disappear
/// without a line saying so.
async fn removal_notice(pool: &SqlitePool) -> anyhow::Result<()> {
    let rows = sqlx::query("SELECT id, name, base_url FROM upstreams WHERE name IN (?1, ?2)")
        .bind(MANAGED_UPSTREAM_NAMES[0])
        .bind(MANAGED_UPSTREAM_NAMES[1])
        .fetch_all(pool)
        .await?;
    for r in &rows {
        tracing::warn!(
            "removing the managed upstream row '{}' (id {}, {}) — per-model containers serve \
             these classes from their own tables now, and the name survives as a synthetic \
             upstream. Nothing referenced it.",
            r.get::<String, _>("name"),
            r.get::<i64, _>("id"),
            r.get::<String, _>("base_url"),
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Pre-migration repair (migration 0018)
// ---------------------------------------------------------------------------

/// Version of `0018_aux_models.sql`, the migration this repair exists for.
const AUX_MIGRATION: i64 = 18;

/// Fix what migration 0018 cannot fix about itself, before the migrator runs.
///
/// 0018 renames the managed upstream row `llama-embed` → `llama-aux` with a
/// plain `UPDATE` on a column that has been `UNIQUE` since 0001. An install
/// that already has an upstream of its own called `llama-aux` — nothing has
/// ever stopped an owner from naming one that — hits the constraint. 0018 is a
/// `-- no-transaction` migration, so there is nothing to roll back: it is left
/// half applied, sqlx never records it, and the next start replays it from the
/// top and dies on `ALTER TABLE embed_models RENAME TO aux_models` instead —
/// a different error every time and no way forward. That is an install that
/// cannot be started again.
///
/// The file itself is untouchable: sqlx validates the SHA-384 of every
/// migration it has already applied, so editing one byte of 0018 would lock out
/// every install that *did* upgrade cleanly. So the repair lives here, ahead of
/// the migrator, and covers both sides of the failure:
///
/// * **Not upgraded yet** — free the `llama-aux` name so 0018's `UPDATE`
///   cannot collide. The pre-existing row is renamed, not deleted, and the
///   rename is logged at `warn` so the owner can find it on the Upstreams page.
/// * **Already half applied** — finish 0018's remaining steps here, each
///   guarded by its own post-condition, and record it as applied with the
///   migrator's own checksum so the file is skipped rather than replayed.
pub(super) async fn repair_before_migrations(pool: &SqlitePool) -> anyhow::Result<()> {
    // A database this process has just created has no tables at all: nothing to
    // read, nothing to repair, and the migrator is about to build it.
    if !table_exists(pool, "_sqlx_migrations").await? {
        return Ok(());
    }
    if migration_applied(pool, AUX_MIGRATION).await? {
        return Ok(());
    }

    // 0018's very first statement renames `embed_models`. If that table is gone
    // and `aux_models` is here, the file ran and then stopped part way.
    if table_exists(pool, "aux_models").await? && !table_exists(pool, "embed_models").await? {
        tracing::warn!(
            "migration 0018 (aux router) was left half applied by an earlier start — \
             finishing it here"
        );
        finish_aux_migration(pool).await?;
        record_migration_applied(pool, AUX_MIGRATION).await?;
        return Ok(());
    }

    free_the_aux_upstream_name(pool).await
}

/// Move an owner's own `llama-aux` upstream out of the way, so the rename 0018
/// performs has a name to rename *into*.
///
/// Only acts when there is actually a `llama-embed` row to rename: without one
/// 0018's `UPDATE` matches nothing and a row called `llama-aux` is either the
/// migration's own work or an unrelated upstream that must be left alone.
async fn free_the_aux_upstream_name(pool: &SqlitePool) -> anyhow::Result<()> {
    if !table_exists(pool, "upstreams").await? {
        return Ok(());
    }
    let clash: Option<i64> =
        sqlx::query_scalar("SELECT id FROM upstreams WHERE name = 'llama-aux'")
            .fetch_optional(pool)
            .await?;
    let Some(id) = clash else { return Ok(()) };
    let renaming: Option<i64> =
        sqlx::query_scalar("SELECT id FROM upstreams WHERE name = 'llama-embed'")
            .fetch_optional(pool)
            .await?;
    if renaming.is_none() {
        return Ok(());
    }

    let free = free_upstream_name(pool, "llama-aux").await?;
    sqlx::query("UPDATE upstreams SET name = ?2 WHERE id = ?1")
        .bind(id)
        .bind(&free)
        .execute(pool)
        .await?;
    tracing::warn!(
        "an upstream named 'llama-aux' already existed, and the aux-router migration needs that \
         name for the managed row. It has been renamed to '{free}' — nothing else about it \
         changed, and it can be renamed again on the Upstreams page."
    );
    Ok(())
}

/// A name near `base` that no upstream holds.
async fn free_upstream_name(pool: &SqlitePool, base: &str) -> anyhow::Result<String> {
    for n in 1..u32::MAX {
        let candidate = if n == 1 {
            format!("{base}-conflict")
        } else {
            format!("{base}-conflict-{n}")
        };
        let taken: Option<i64> = sqlx::query_scalar("SELECT id FROM upstreams WHERE name = ?1")
            .bind(&candidate)
            .fetch_optional(pool)
            .await?;
        if taken.is_none() {
            return Ok(candidate);
        }
    }
    anyhow::bail!("no free upstream name near '{base}'")
}

/// Run whatever of 0018 is still missing. Every step checks its own
/// post-condition first, so this is safe to run against a database that stopped
/// anywhere inside the file.
async fn finish_aux_migration(pool: &SqlitePool) -> anyhow::Result<()> {
    // 1. The class discriminant.
    if !aux_models_has_kind(pool).await? {
        sqlx::query(
            "ALTER TABLE aux_models
                 ADD COLUMN kind TEXT NOT NULL DEFAULT 'embed' CHECK (kind IN ('embed','rerank'))",
        )
        .execute(pool)
        .await?;
    }

    // 2. `hf_models`, rebuilt for the `aux` download target. The rebuild is a
    //    drop-and-rename, so a run that stopped inside it can have left the new
    //    table under its working name.
    if table_exists(pool, "hf_models_new").await? && !table_exists(pool, "hf_models").await? {
        sqlx::query("ALTER TABLE hf_models_new RENAME TO hf_models")
            .execute(pool)
            .await?;
    }
    if !hf_models_targets_aux(pool).await? {
        sqlx::query("DROP TABLE IF EXISTS hf_models_new")
            .execute(pool)
            .await?;
        sqlx::query(
            "CREATE TABLE hf_models_new (
                 id            INTEGER PRIMARY KEY AUTOINCREMENT,
                 repo          TEXT NOT NULL,
                 file          TEXT NOT NULL,
                 dest_path     TEXT NOT NULL,
                 target        TEXT NOT NULL DEFAULT 'chat'
                               CHECK (target IN ('chat','aux','audio')),
                 etag          TEXT,
                 size_bytes    INTEGER,
                 status        TEXT NOT NULL DEFAULT 'queued'
                               CHECK (status IN ('queued','downloading','done','failed',
                                                 'update_available')),
                 error         TEXT,
                 downloaded_at TEXT,
                 created_at    TEXT NOT NULL DEFAULT (datetime('now')),
                 UNIQUE (repo, file, target)
             )",
        )
        .execute(pool)
        .await?;
        sqlx::query(
            "INSERT INTO hf_models_new
                 (id, repo, file, dest_path, target, etag, size_bytes, status, error,
                  downloaded_at, created_at)
             SELECT id, repo, file, dest_path,
                    CASE target WHEN 'embed' THEN 'aux' ELSE target END,
                    etag, size_bytes, status, error, downloaded_at, created_at
             FROM hf_models",
        )
        .execute(pool)
        .await?;
        sqlx::query("DROP TABLE hf_models").execute(pool).await?;
        sqlx::query("ALTER TABLE hf_models_new RENAME TO hf_models")
            .execute(pool)
            .await?;
    }

    // 3. The rename that started all this — this time with the name freed first.
    free_the_aux_upstream_name(pool).await?;
    sqlx::query("UPDATE upstreams SET name = 'llama-aux' WHERE name = 'llama-embed'")
        .execute(pool)
        .await?;

    // 4. The settings blob's key.
    sqlx::query(
        "UPDATE settings
         SET value = json_remove(
                 json_set(value, '$.aux_router', json_extract(value, '$.embed_router')),
                 '$.embed_router')
         WHERE key = 'settings' AND json_extract(value, '$.embed_router') IS NOT NULL",
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Record a migration this module applied by hand, using the migrator's own
/// checksum so sqlx's SHA-384 validation passes on the next start.
async fn record_migration_applied(pool: &SqlitePool, version: i64) -> anyhow::Result<()> {
    let migrator = sqlx::migrate!("./migrations");
    let m = migrator
        .migrations
        .iter()
        .find(|m| m.version == version)
        .ok_or_else(|| anyhow::anyhow!("this build has no migration {version}"))?;
    sqlx::query(
        "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time)
         VALUES (?1, ?2, 1, ?3, 0)",
    )
    .bind(m.version)
    .bind(m.description.as_ref())
    .bind(m.checksum.to_vec())
    .execute(pool)
    .await?;
    Ok(())
}

async fn migration_applied(pool: &SqlitePool, version: i64) -> anyhow::Result<bool> {
    let found: Option<i64> =
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations WHERE version = ?1")
            .bind(version)
            .fetch_optional(pool)
            .await?;
    Ok(found.is_some())
}

async fn table_exists(pool: &SqlitePool, name: &str) -> anyhow::Result<bool> {
    let found: Option<String> =
        sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type = 'table' AND name = ?1")
            .bind(name)
            .fetch_optional(pool)
            .await?;
    Ok(found.is_some())
}

/// Whether 0018's second statement — the `kind` discriminant — already landed.
async fn aux_models_has_kind(pool: &SqlitePool) -> anyhow::Result<bool> {
    let rows = sqlx::query("PRAGMA table_info(aux_models)")
        .fetch_all(pool)
        .await?;
    Ok(rows.iter().any(|r| r.get::<String, _>("name") == "kind"))
}

/// Whether `hf_models` is already the post-0018 table — the one whose `target`
/// check knows `aux`.
async fn hf_models_targets_aux(pool: &SqlitePool) -> anyhow::Result<bool> {
    let ddl: Option<String> =
        sqlx::query_scalar("SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1")
            .bind("hf_models")
            .fetch_optional(pool)
            .await?;
    Ok(ddl.is_some_and(|sql| sql.contains("'aux'")))
}
