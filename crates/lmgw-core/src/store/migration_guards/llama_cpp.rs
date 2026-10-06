//! Pre-migration notice (migration 0058): name the upstream rows the
//! `llama_cpp` protocol migration changes (llama.cpp egress design §5,
//! decision 12).
//!
//! The migration is plain SQL and cannot say which rows it changed, and
//! afterwards nothing is left to read it from. So the first start that runs
//! 0058 says, per row, what it stores now: the respelling of `openai` +
//! `llama_server` (nothing about such a row behaves differently), every
//! native `/v1/responses` it switches off, and every `gemini` + `llama_server`
//! row that becomes `generic` — no longer local and free.

use sqlx::{Row, SqlitePool};

use super::{has_column, migration_applied, table_exists};

/// Version of `0058_llama_cpp_protocol.sql`.
const LLAMA_CPP_MIGRATION: i64 = 58;

/// Version of `0023_managed_upstreams_go_synthetic.sql`, which deletes the
/// managed `llama-aux` and `audiocpp` rows.
const MANAGED_ROWS_DELETED: i64 = 23;

/// One line per upstream row 0058 is about to change, before it changes it.
/// Nothing on a new database, one that ran 0058 already, or one from before
/// `upstreams` had a `kind`.
pub(in crate::store) async fn llama_cpp_notice(pool: &SqlitePool) -> anyhow::Result<()> {
    if !table_exists(pool, "_sqlx_migrations").await?
        || migration_applied(pool, LLAMA_CPP_MIGRATION).await?
        || !has_column(pool, "upstreams", "kind").await?
    {
        return Ok(());
    }
    // An install from before 0015 has no `supports_responses` yet: nothing to
    // switch off there.
    // Migration 0023 deletes the managed `llama-aux` / `audiocpp` rows in the
    // same start: before it has run they are not rows 0058 will see, so they
    // stay out of the notice.
    let managed = if migration_applied(pool, MANAGED_ROWS_DELETED).await? {
        ""
    } else {
        " AND name NOT IN ('llama-aux', 'audiocpp')"
    };
    let responses = if has_column(pool, "upstreams", "supports_responses").await? {
        "supports_responses"
    } else {
        "0"
    };
    let sql = format!(
        "SELECT id, name, protocol, {responses} AS responses FROM upstreams
         WHERE kind = 'llama_server' AND protocol IN ('openai', 'gemini'){managed} ORDER BY id"
    );
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .fetch_all(pool)
        .await?;
    for r in &rows {
        let id: i64 = r.get("id");
        let name: String = r.get("name");
        let responses = r.get::<i64, _>("responses") != 0;
        match r.get::<String, _>("protocol").as_str() {
            "openai" => {
                tracing::info!(
                    "migration 0058: upstream '{name}' (id {id}) was protocol openai with kind \
                     llama_server, which is now spelled protocol llama_cpp — nothing else about \
                     it changes"
                );
                if responses {
                    tracing::warn!(
                        "migration 0058: upstream '{name}' (id {id}) had native /v1/responses \
                         on, which a llama_cpp upstream never forwards: lmgw synthesizes \
                         /v1/responses on it from /v1/chat/completions from now on, so every \
                         request goes through its llama.cpp egress"
                    );
                }
            }
            _ => tracing::warn!(
                "migration 0058: upstream '{name}' (id {id}) was protocol gemini with kind \
                 llama_server, a wire llama-server does not serve; it keeps protocol gemini and \
                 becomes kind generic, so it no longer counts as a local, free llama-server — if \
                 it is one, set its protocol to llama_cpp on the Upstreams page"
            ),
        }
    }
    Ok(())
}
