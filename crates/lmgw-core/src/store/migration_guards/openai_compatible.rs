//! Pre-migration notice (migration 0090): name the upstream rows the
//! `openai_compatible` migration changes (openai egress design §4.2).
//!
//! The migration is plain SQL and cannot say which rows it changed, and
//! afterwards nothing is left to read it from. So the first start that runs
//! 0090 says, per row, what it stores now: every `openai` row respelled
//! `openai_compatible` (nothing about such a row behaves differently), a
//! second line for a row whose host is OpenAI's own, which the owner may want
//! to move to the platform protocol by hand, and every `anthropic` or
//! `gemini` row whose native `/v1/responses` is switched off or whose kind
//! `audio_cpp` becomes `generic`.

use sqlx::{Row, SqlitePool};

use super::{has_column, migration_applied, table_exists};
use crate::config::is_openai_platform_host;

/// Version of `0090_openai_compatible_protocol.sql`.
const OPENAI_COMPATIBLE_MIGRATION: i64 = 90;

/// Version of `0023_managed_upstreams_go_synthetic.sql`, which deletes the
/// managed `llama-aux` and `audiocpp` rows.
const MANAGED_ROWS_DELETED: i64 = 23;

/// One line (or two) per upstream row 0090 is about to change, before it
/// changes it. Nothing on a new database or one that ran 0090 already. An
/// install old enough to lack `kind` or `supports_responses` runs 0058's
/// notice first in the same start; the rows it names are named here too,
/// under their spelling before either migration.
pub(in crate::store) async fn openai_compatible_notice(pool: &SqlitePool) -> anyhow::Result<()> {
    if !table_exists(pool, "_sqlx_migrations").await?
        || migration_applied(pool, OPENAI_COMPATIBLE_MIGRATION).await?
        || !table_exists(pool, "upstreams").await?
    {
        return Ok(());
    }
    // 0058 respells `openai` + `llama_server` as `llama_cpp` in the same
    // start when it has not run yet; such a row never reaches 0090 as
    // `openai`, so it is 0058's to name.
    let kind = if has_column(pool, "upstreams", "kind").await? {
        "kind"
    } else {
        "'generic'"
    };
    let responses = if has_column(pool, "upstreams", "supports_responses").await? {
        "supports_responses"
    } else {
        "0"
    };
    // Migration 0023 deletes the managed `llama-aux` / `audiocpp` rows in the
    // same start: before it has run they are not rows 0090 will see.
    let managed = if migration_applied(pool, MANAGED_ROWS_DELETED).await? {
        ""
    } else {
        " AND name NOT IN ('llama-aux', 'audiocpp')"
    };
    let sql = format!(
        "SELECT id, name, protocol, base_url, {kind} AS kind, {responses} AS responses
         FROM upstreams
         WHERE ((protocol = 'openai' AND {kind} <> 'llama_server')
            OR (protocol IN ('anthropic', 'gemini')
                AND ({responses} <> 0 OR {kind} = 'audio_cpp'))){managed}
         ORDER BY id"
    );
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .fetch_all(pool)
        .await?;
    for r in &rows {
        let id: i64 = r.get("id");
        let name: String = r.get("name");
        let protocol: String = r.get("protocol");
        if protocol == "openai" {
            tracing::info!(
                "migration 0090: upstream '{name}' (id {id}) was protocol openai, which is now \
                 spelled openai_compatible — nothing about it changes"
            );
            if is_openai_platform_host(&r.get::<String, _>("base_url")) {
                tracing::info!(
                    "migration 0090: upstream '{name}' (id {id}) looks like the OpenAI \
                     platform: it can be switched to protocol openai on the Upstreams \
                     page"
                );
            }
        } else {
            if r.get::<i64, _>("responses") != 0 {
                tracing::warn!(
                    "migration 0090: upstream '{name}' (id {id}) is protocol {protocol} and had \
                     native /v1/responses on; neither Anthropic's nor Gemini's API serves \
                     /v1/responses, so lmgw synthesizes it from now on"
                );
            }
            if r.get::<String, _>("kind") == "audio_cpp" {
                tracing::warn!(
                    "migration 0090: upstream '{name}' (id {id}) is protocol {protocol} and \
                     kind audio_cpp; an audio.cpp server is reached over openai_compatible, so \
                     its kind is generic from now on"
                );
            }
        }
    }
    Ok(())
}
