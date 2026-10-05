//! Pre-migration notice (migration 0055): name the rows whose empty run-args
//! override 0055 turns into "inherit the class's run args".
//!
//! The migration is plain SQL and cannot say which rows it changed. On this
//! kind of host `[]` was always a mistake (no `label=disable`, no card), but
//! on a host without SELinux it was the way to run a model without the
//! class's flags — the GPU device among them — and that row starts with the
//! class's flags from now on. So the first start that runs 0055 says, per
//! row, what changes and how to opt out again; afterwards nothing is left to
//! read it from.

use sqlx::SqlitePool;

use super::{migration_applied, table_exists};

/// Version of `0055_empty_run_args_inherit.sql`.
const EMPTY_RUN_ARGS_MIGRATION: i64 = 55;

/// One `warn` line per row 0055 is about to move onto its class's run args,
/// before it moves them. Nothing on a new database, one that ran 0055
/// already, or a table from before the column existed.
pub(in crate::store) async fn empty_run_args_notice(pool: &SqlitePool) -> anyhow::Result<()> {
    if !table_exists(pool, "_sqlx_migrations").await?
        || migration_applied(pool, EMPTY_RUN_ARGS_MIGRATION).await?
    {
        return Ok(());
    }
    for (table, class, sql) in [
        (
            "local_models",
            "chat",
            "SELECT model_id FROM local_models WHERE extra_run_args = '[]' ORDER BY model_id",
        ),
        (
            "aux_models",
            "aux",
            "SELECT model_id FROM aux_models WHERE extra_run_args = '[]' ORDER BY model_id",
        ),
        (
            "audio_models",
            "audio",
            "SELECT model_id FROM audio_models WHERE extra_run_args = '[]' ORDER BY model_id",
        ),
        (
            "image_models",
            "image",
            "SELECT model_id FROM image_models WHERE extra_run_args = '[]' ORDER BY model_id",
        ),
    ] {
        if !has_column(pool, table, "extra_run_args").await? {
            continue;
        }
        let ids: Vec<String> = sqlx::query_scalar(sql).fetch_all(pool).await?;
        for id in ids {
            tracing::warn!(
                "migration 0055: {class} model '{id}' had an empty run-args override, which ran \
                 it without the {class} class's run args (its GPU device and SELinux \
                 label=disable among them); it inherits the class's run args from now on — to \
                 run it without some of them, give it an override that names at least one flag \
                 of its own"
            );
        }
    }
    Ok(())
}

/// `table` exists and has `column`.
async fn has_column(pool: &SqlitePool, table: &str, column: &str) -> anyhow::Result<bool> {
    if !table_exists(pool, table).await? {
        return Ok(false);
    }
    let names: Vec<String> = sqlx::query_scalar("SELECT name FROM pragma_table_info(?1)")
        .bind(table)
        .fetch_all(pool)
        .await?;
    Ok(names.iter().any(|n| n == column))
}
