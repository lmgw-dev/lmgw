//! Get it's rows (find-a-model design §6.2, migration 0098): one
//! `model_gets` row per Get it, and the fit note a row keeps (D26).

use sqlx::{Row, SqlitePool};

use super::*;

/// One Get it, as `model_gets` holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelGetRow {
    pub id: i64,
    pub repo: String,
    pub file: String,
    /// `chat | aux`.
    pub target: String,
    /// `whole_card | beside`.
    pub measure: String,
    pub model_id: String,
    /// An aux get's kind (`embed | rerank | decision`), from the header;
    /// `None` for chat.
    pub kind: Option<String>,
    /// The `hf_models` ids of every file of the get; empty until the op
    /// queued them.
    pub download_ids: Vec<i64>,
    /// The ids of `download_ids` the get started itself; the others it
    /// joined, and its cancel leaves them alone.
    pub queued_ids: Vec<i64>,
    /// The note (JSON of `FitNote`) the row it creates takes, recorded
    /// before the create.
    pub note: Option<String>,
    /// The job running it now.
    pub job_id: Option<i64>,
    /// `running | done | failed | canceled`.
    pub status: String,
    pub error: Option<String>,
    /// The row it set up (or found), in its class's table.
    pub row_id: Option<i64>,
    pub created_at: String,
    pub finished_at: Option<String>,
}

fn model_get_from_row(row: &sqlx::sqlite::SqliteRow) -> ModelGetRow {
    let ids: String = row.get("download_ids");
    let queued: String = row.get("queued_ids");
    ModelGetRow {
        id: row.get("id"),
        repo: row.get("repo"),
        file: row.get("file"),
        target: row.get("target"),
        measure: row.get("measure"),
        model_id: row.get("model_id"),
        kind: row.get("kind"),
        download_ids: serde_json::from_str(&ids).unwrap_or_default(),
        queued_ids: serde_json::from_str(&queued).unwrap_or_default(),
        note: row.get("note"),
        job_id: row.get("job_id"),
        status: row.get("status"),
        error: row.get("error"),
        row_id: row.get("row_id"),
        created_at: row.get("created_at"),
        finished_at: row.get("finished_at"),
    }
}

/// What a new get records.
pub struct NewModelGet<'a> {
    pub repo: &'a str,
    pub file: &'a str,
    pub target: &'a str,
    pub measure: &'a str,
    pub model_id: &'a str,
    pub kind: Option<&'a str>,
}

/// Insert a `running` get with no downloads yet, unless one of the same
/// file is running already (the partial unique index): `Ok(None)` then, and
/// the caller joins that one ([`running_model_get`]).
pub async fn insert_model_get(pool: &SqlitePool, g: &NewModelGet<'_>) -> DbResult<Option<i64>> {
    let row = sqlx::query(
        "INSERT INTO model_gets (repo, file, target, measure, model_id, kind)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT DO NOTHING RETURNING id",
    )
    .bind(g.repo)
    .bind(g.file)
    .bind(g.target)
    .bind(g.measure)
    .bind(g.model_id)
    .bind(g.kind)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| r.get("id")))
}

/// The downloads of the get `id`, once queued: every file's row, and those
/// of them it started itself.
pub async fn set_model_get_downloads(
    pool: &SqlitePool,
    id: i64,
    download_ids: &[i64],
    queued_ids: &[i64],
) -> DbResult<()> {
    let ids = serde_json::to_string(download_ids).unwrap_or_else(|_| "[]".into());
    let queued = serde_json::to_string(queued_ids).unwrap_or_else(|_| "[]".into());
    sqlx::query("UPDATE model_gets SET download_ids = ?2, queued_ids = ?3 WHERE id = ?1")
        .bind(id)
        .bind(ids)
        .bind(queued)
        .execute(pool)
        .await?;
    Ok(())
}

/// The job running the get `id` now.
pub async fn set_model_get_job(pool: &SqlitePool, id: i64, job_id: i64) -> DbResult<()> {
    sqlx::query("UPDATE model_gets SET job_id = ?2 WHERE id = ?1")
        .bind(id)
        .bind(job_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// The note the row the get `id` creates takes, recorded before the
/// create (`None`: it keeps none).
pub async fn set_model_get_note(pool: &SqlitePool, id: i64, note: Option<&str>) -> DbResult<()> {
    sqlx::query("UPDATE model_gets SET note = ?2 WHERE id = ?1")
        .bind(id)
        .bind(note)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn get_model_get(pool: &SqlitePool, id: i64) -> DbResult<Option<ModelGetRow>> {
    let row = sqlx::query("SELECT * FROM model_gets WHERE id = ?1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(model_get_from_row))
}

/// The running get of `repo`'s `file`, if any.
pub async fn running_model_get(
    pool: &SqlitePool,
    repo: &str,
    file: &str,
) -> DbResult<Option<ModelGetRow>> {
    let row = sqlx::query(
        "SELECT * FROM model_gets WHERE repo = ?1 AND file = ?2 AND status = 'running' LIMIT 1",
    )
    .bind(repo)
    .bind(file)
    .fetch_optional(pool)
    .await?;
    Ok(row.as_ref().map(model_get_from_row))
}

/// Every running get, oldest first: what the boot re-attaches.
pub async fn running_model_gets(pool: &SqlitePool) -> DbResult<Vec<ModelGetRow>> {
    let rows = sqlx::query("SELECT * FROM model_gets WHERE status = 'running' ORDER BY id")
        .fetch_all(pool)
        .await?;
    Ok(rows.iter().map(model_get_from_row).collect())
}

/// The model id a get will set up, once it is settled (a suffix taken at
/// create time).
pub async fn set_model_get_model_id(pool: &SqlitePool, id: i64, model_id: &str) -> DbResult<()> {
    sqlx::query("UPDATE model_gets SET model_id = ?2 WHERE id = ?1")
        .bind(id)
        .bind(model_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// The row a get set up or found, recorded before it ends: a re-attached
/// get that finds it answers it.
pub async fn set_model_get_row(pool: &SqlitePool, id: i64, row_id: i64) -> DbResult<()> {
    sqlx::query("UPDATE model_gets SET row_id = ?2 WHERE id = ?1")
        .bind(id)
        .bind(row_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// End a get: `done`, `failed` (with its sentence) or `canceled`.
pub async fn finish_model_get(
    pool: &SqlitePool,
    id: i64,
    status: &str,
    error: Option<&str>,
) -> DbResult<()> {
    sqlx::query(
        "UPDATE model_gets SET status = ?2, error = ?3, finished_at = datetime('now')
         WHERE id = ?1",
    )
    .bind(id)
    .bind(status)
    .bind(error)
    .execute(pool)
    .await?;
    Ok(())
}

/// The model ids the running gets will set up in `target`'s class: taken
/// for a new get's id (D28), as a row's would be.
pub async fn running_model_get_ids(pool: &SqlitePool, target: &str) -> DbResult<Vec<String>> {
    let rows =
        sqlx::query("SELECT model_id FROM model_gets WHERE status = 'running' AND target = ?1")
            .bind(target)
            .fetch_all(pool)
            .await?;
    Ok(rows.iter().map(|r| r.get("model_id")).collect())
}

/// The fit note of the `class` row `row_id` (D26), as stored: the JSON of
/// `FitNote`.
pub async fn fit_note(pool: &SqlitePool, class: &str, row_id: i64) -> DbResult<Option<String>> {
    let row = sqlx::query("SELECT note FROM model_fit_notes WHERE class = ?1 AND row_id = ?2")
        .bind(class)
        .bind(row_id)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|r| r.get("note")))
}

/// Write the `class` row `row_id`'s fit note, or remove it (`None`).
pub async fn set_fit_note(
    pool: &SqlitePool,
    class: &str,
    row_id: i64,
    note: Option<&str>,
) -> DbResult<()> {
    match note {
        Some(note) => {
            sqlx::query(
                "INSERT INTO model_fit_notes (class, row_id, note) VALUES (?1, ?2, ?3)
                 ON CONFLICT (class, row_id) DO UPDATE SET note = excluded.note",
            )
            .bind(class)
            .bind(row_id)
            .bind(note)
            .execute(pool)
            .await?;
        }
        None => {
            sqlx::query("DELETE FROM model_fit_notes WHERE class = ?1 AND row_id = ?2")
                .bind(class)
                .bind(row_id)
                .execute(pool)
                .await?;
        }
    }
    Ok(())
}
