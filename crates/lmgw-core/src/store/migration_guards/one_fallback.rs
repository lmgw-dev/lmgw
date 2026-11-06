//! The pre-migration guard of 0095 (openai egress design §14.10): one
//! fallback per model and alias, the global hold fallback and `inherit`
//! removed.
//!
//! The migration is plain SQL. It copies every row's old fallback into the
//! new `fallback` column — the global for a chat row or a candidate alias
//! that inherited it — and afterwards nothing is left to say where a value
//! came from. So the first start that runs it:
//!
//! - **refuses** when the SQL would read the global differently from the
//!   loader: a blob that is JSON but that the loader did not read as
//!   `Settings` (so no row inherited anything) while the SQL would copy a
//!   name out of it. That is the one state that cannot be carried over
//!   faithfully;
//! - **evens out** what the loader read as blank and the SQL's `TRIM` would
//!   not (a name of tabs or other whitespace than spaces): it is stored as
//!   `''` first, so the SQL drops it as the loader did;
//! - **says**, per row, what it changes: a fallback that came from the
//!   global, a stale name under mode `none` or `inherit` that was never
//!   read, a fallback that names a local model (refused until now, a working
//!   hop from now on), and a fallback of another model type as far as the
//!   tables tell it (decision 29: kept, but refused at the row's next save).
//!   A name of a cloud model is typed by the first start after 0095, which
//!   has the catalogs (`ops::fallback::first_start_type_check`).
//!
//! It imitates the old loader with this build's [`Settings`], not the old
//! build's: the two read the same blob the same way as long as no field
//! changed type in between, since every field added since is
//! `#[serde(default)]`. That holds for this build; a later change of a
//! field's type would make this guard read a blob the old loader rejected
//! as valid, or the reverse, and needs a look here then.

use std::collections::HashMap;

use sqlx::{Row, SqlitePool};

use super::{has_column, migration_applied, table_exists};
use crate::config::Settings;

/// Version of `0095_one_fallback.sql`.
const ONE_FALLBACK_MIGRATION: i64 = 95;

/// One row the migration copies a fallback for: where it is, what the
/// owner is, its mode and the name stored beside it.
struct OldRow {
    table: &'static str,
    label: String,
    /// The owner's type, as a noun phrase.
    ty: &'static str,
    mode: String,
    name: Option<String>,
    /// A chat row or a candidate alias: `inherit` read the global.
    inherits_global: bool,
}

pub(in crate::store) async fn one_fallback_notice(pool: &SqlitePool) -> anyhow::Result<()> {
    if !table_exists(pool, "_sqlx_migrations").await?
        || migration_applied(pool, ONE_FALLBACK_MIGRATION).await?
        || !table_exists(pool, "settings").await?
    {
        return Ok(());
    }
    let raw: Option<String> =
        sqlx::query_scalar("SELECT value FROM settings WHERE key = 'settings'")
            .fetch_optional(pool)
            .await?;
    let global = global_as_loaded(pool, raw.as_deref()).await?;
    let settings = crate::store::snapshot::settings_from_row(raw.as_deref());

    even_out_blanks(pool).await?;
    let rows = old_rows(pool).await?;
    let local = local_names(pool, &settings).await?;
    let aliases: Vec<String> = if table_exists(pool, "models").await? {
        sqlx::query_scalar("SELECT alias FROM models WHERE enabled = 1")
            .fetch_all(pool)
            .await?
    } else {
        Vec::new()
    };

    let mut copied = (0usize, 0usize);
    for r in &rows {
        let named = r.name.as_deref().filter(|n| !n.trim().is_empty());
        // The loader read a mode it did not know as `inherit`
        // (`hold_fallback_mode_from_row`), and so does 0095's SQL.
        let inherits = !matches!(r.mode.as_str(), "alias" | "none") && r.inherits_global;
        let fallback: Option<&str> = match r.mode.as_str() {
            "alias" => named,
            _ if inherits => global.as_deref(),
            _ => None,
        };
        if inherits {
            if let Some(g) = global.as_deref() {
                tracing::warn!(
                    "migration 0095: {} followed the global hold fallback '{g}'; '{g}' is its \
                     own fallback now, the global setting is gone",
                    r.label
                );
                match r.table {
                    "candidate_aliases" => copied.1 += 1,
                    _ => copied.0 += 1,
                }
            }
        }
        if r.mode != "alias" {
            if let Some(stale) = named {
                tracing::warn!(
                    "migration 0095: {} had '{stale}' stored as its fallback under mode '{}', \
                     which was never read; it is dropped",
                    r.label,
                    r.mode
                );
            }
        }
        let Some(fb) = fallback else { continue };
        // A name a plain alias answers to is that alias, as a client's
        // `model` is (`Snapshot::resolve`).
        if aliases.iter().any(|a| a == fb) {
            continue;
        }
        let Some((what, target_ty)) = local.get(fb) else {
            continue;
        };
        tracing::warn!(
            "migration 0095: {} falls back to the local {what} '{fb}', which lmgw refused to use \
             until now; it is used from now on, with '{fb}''s own fallback after it",
            r.label
        );
        if *target_ty != r.ty {
            tracing::warn!(
                "migration 0095: {} falls back to '{fb}', {target_ty}; a fallback must be the \
                 same type of model ({} here), so the next save of the row refuses it until it \
                 is changed. It is kept as it is: a request that reaches it is refused as one \
                 its endpoint cannot serve",
                r.label,
                r.ty
            );
        }
    }
    if let Some(g) = global.as_deref() {
        tracing::info!(
            "migration 0095: the global hold fallback '{g}' was copied onto {} model(s) and {} \
             candidate alias(es) and removed",
            copied.0,
            copied.1
        );
    }
    Ok(())
}

/// The global hold fallback as the loader read it — `None` when unset,
/// blank, or in a blob the loader did not read as `Settings` — after
/// refusing the start where the migration's SQL would read it differently.
async fn global_as_loaded(pool: &SqlitePool, raw: Option<&str>) -> anyhow::Result<Option<String>> {
    let Some(raw) = raw else { return Ok(None) };
    // As text: a global that is a JSON number or boolean is copied by the
    // SQL as its text, and is the refusal below, not a type error here.
    let sql: Option<String> = sqlx::query_scalar(
        "SELECT CASE WHEN json_valid(value) \
         THEN CAST(json_extract(value, '$.hold.fallback_alias') AS TEXT) END \
         FROM settings WHERE key = 'settings'",
    )
    .fetch_optional(pool)
    .await?
    .flatten();
    let parsed: Option<serde_json::Value> = serde_json::from_str(raw).ok();
    let field = parsed
        .as_ref()
        .and_then(|v| v.pointer("/hold/fallback_alias"))
        .cloned();
    // The loader read `Settings` whole or nothing: the field a string or
    // null, and the rest a `Settings` (it is the one field 0095 removes).
    let loaded = matches!(
        field,
        None | Some(serde_json::Value::Null) | Some(serde_json::Value::String(_))
    ) && serde_json::from_str::<Settings>(raw).is_ok();
    let global = match (&field, loaded) {
        (Some(serde_json::Value::String(s)), true) => Some(s.clone()),
        _ => None,
    };
    let sql_copies = sql
        .as_deref()
        .is_some_and(|s| !s.trim_matches(' ').is_empty());
    match (&global, sql_copies) {
        // The loader had no global, the SQL would copy one.
        (None, true) => anyhow::bail!(
            "this upgrade gives every model and alias its own fallback and removes the global \
             hold fallback (settings hold.fallback_alias), copying the global onto every chat \
             model and candidate alias that followed it. The stored settings hold {} there, \
             but this lmgw did not read those settings as valid (so no model followed it), and \
             the upgrade would copy it all the same. Nothing has been changed. Fix or clear the \
             setting with the previous version of lmgw, then start this one again.",
            match &field {
                Some(serde_json::Value::String(s)) => format!("'{s}'"),
                Some(v) => format!("{v} (not a name)"),
                None => format!("'{}'", sql.unwrap_or_default()),
            }
        ),
        // Blank to the loader, not to SQL's TRIM: even it out first.
        (Some(g), true) if g.trim().is_empty() => {
            sqlx::query(
                "UPDATE settings SET value = json_set(value, '$.hold.fallback_alias', NULL) \
                 WHERE key = 'settings'",
            )
            .execute(pool)
            .await?;
            Ok(None)
        }
        (Some(g), _) if g.trim().is_empty() => Ok(None),
        _ => Ok(global),
    }
}

/// The name columns 0095 reads, each blank to the loader but not to SQL's
/// `TRIM` (which trims spaces only), stored as `''` so the SQL drops it.
async fn even_out_blanks(pool: &SqlitePool) -> anyhow::Result<()> {
    for (table, col) in NAME_COLUMNS {
        if !has_column(pool, table, col).await? {
            continue;
        }
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT id, {col} AS name FROM {table} WHERE {col} IS NOT NULL"
        )))
        .fetch_all(pool)
        .await?;
        for r in rows {
            let name: String = r.get("name");
            if name.trim().is_empty() && !name.trim_matches(' ').is_empty() {
                sqlx::query(sqlx::AssertSqlSafe(format!(
                    "UPDATE {table} SET {col} = '' WHERE id = ?1"
                )))
                .bind(r.get::<i64, _>("id"))
                .execute(pool)
                .await?;
            }
        }
    }
    Ok(())
}

/// The tables 0095 reads, and the column each stores its name in.
const NAME_COLUMNS: [(&str, &str); 5] = [
    ("local_models", "hold_fallback"),
    ("aux_models", "hold_fallback"),
    ("audio_models", "hold_fallback"),
    ("image_models", "hold_fallback"),
    ("candidate_aliases", "fallback"),
];

/// Every row 0095 copies a fallback for. A table or a column this install
/// does not have yet is created by an earlier migration of the same start,
/// with every row on `inherit` and no name, as 0024 and 0033 leave it.
async fn old_rows(pool: &SqlitePool) -> anyhow::Result<Vec<OldRow>> {
    let mut out = Vec::new();
    for (table, name_col) in NAME_COLUMNS {
        if !table_exists(pool, table).await? {
            continue;
        }
        let mode_col = match table {
            "candidate_aliases" => "fallback_mode",
            _ => "hold_fallback_mode",
        };
        let has_mode = has_column(pool, table, mode_col).await?;
        let has_name = has_column(pool, table, name_col).await?;
        let key_col = match table {
            "candidate_aliases" => "alias",
            _ => "model_id",
        };
        let kind_col = match table {
            "aux_models" if has_column(pool, table, "kind").await? => "kind",
            "audio_models" => "task",
            _ => "''",
        };
        let sql = format!(
            "SELECT id, {key_col} AS who, {mode} AS mode, {name} AS name, {kind_col} AS kind \
             FROM {table} ORDER BY id",
            mode = if has_mode { mode_col } else { "'inherit'" },
            name = if has_name { name_col } else { "NULL" },
        );
        for r in sqlx::query(sqlx::AssertSqlSafe(sql))
            .fetch_all(pool)
            .await?
        {
            let who: String = r.get("who");
            let kind: String = r.try_get("kind").unwrap_or_default();
            let (label, ty) = match table {
                "local_models" => (format!("chat model '{who}'"), "a chat model"),
                "aux_models" => match kind.as_str() {
                    "rerank" => (format!("reranker '{who}'"), "a reranker"),
                    "decision" => (format!("decision model '{who}'"), "a decision model"),
                    _ => (format!("embedding model '{who}'"), "an embedding model"),
                },
                "audio_models" => (format!("audio model '{who}'"), audio_type(&kind)),
                "image_models" => (format!("image model '{who}'"), "an image model"),
                _ => (format!("candidate alias '{who}'"), "a chat model"),
            };
            out.push(OldRow {
                table,
                label,
                ty,
                mode: r.get("mode"),
                name: r.get("name"),
                inherits_global: matches!(table, "local_models" | "candidate_aliases"),
            });
        }
    }
    Ok(out)
}

/// An audio task's type, as the save check names it.
fn audio_type(task: &str) -> &'static str {
    match task {
        "tts" => "a text-to-speech model",
        "asr" => "a speech-to-text model",
        "vdes" => "a voice-design model",
        "align" => "an alignment model",
        _ => "an audio model of another task",
    }
}

/// The public name of every enabled local row a client can name, with what
/// it is: the class prefixes from the stored settings, the tables' ids. A
/// chat row must be public as well (`Snapshot::resolve_public_local`).
async fn local_names(
    pool: &SqlitePool,
    s: &Settings,
) -> anyhow::Result<HashMap<String, (&'static str, &'static str)>> {
    use lmgw_api_types::public_names::{aux_prefix, prefixed};
    let mut out = HashMap::new();
    if table_exists(pool, "local_models").await? {
        for r in sqlx::query("SELECT model_id FROM local_models WHERE enabled = 1 AND public = 1")
            .fetch_all(pool)
            .await?
        {
            let id: String = r.get("model_id");
            out.insert(
                prefixed(&s.router.public_prefix, &id),
                ("chat model", "a chat model"),
            );
        }
    }
    if table_exists(pool, "aux_models").await? {
        let kind = if has_column(pool, "aux_models", "kind").await? {
            "kind"
        } else {
            "'embed'"
        };
        for r in sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT model_id, {kind} AS kind FROM aux_models WHERE enabled = 1"
        )))
        .fetch_all(pool)
        .await?
        {
            let (id, kind): (String, String) = (r.get("model_id"), r.get("kind"));
            let a = &s.aux_router;
            let what = match kind.as_str() {
                "rerank" => ("reranker", "a reranker"),
                "decision" => ("decision model", "a decision model"),
                _ => ("embedding model", "an embedding model"),
            };
            out.insert(
                prefixed(aux_prefix(&kind, &a.public_prefix, &a.decision_prefix), &id),
                what,
            );
        }
    }
    if table_exists(pool, "audio_models").await? {
        for r in sqlx::query("SELECT model_id, task FROM audio_models WHERE enabled = 1")
            .fetch_all(pool)
            .await?
        {
            let (id, task): (String, String) = (r.get("model_id"), r.get("task"));
            out.insert(
                prefixed(&s.audio.public_prefix, &id),
                ("audio model", audio_type(&task)),
            );
        }
    }
    if table_exists(pool, "image_models").await? {
        for r in sqlx::query("SELECT model_id FROM image_models WHERE enabled = 1")
            .fetch_all(pool)
            .await?
        {
            let id: String = r.get("model_id");
            out.insert(
                prefixed(&s.image.public_prefix, &id),
                ("image model", "an image model"),
            );
        }
    }
    Ok(out)
}
