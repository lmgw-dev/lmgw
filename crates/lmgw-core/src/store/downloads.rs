//! Hugging Face downloads

use sqlx::{Row, SqlitePool};

use super::*;

/// One tracked HF download (a GGUF file in the models dir). `status` is the
/// durable state machine: queued → downloading → done | failed, plus
/// update_available when the remote ETag no longer matches.
#[derive(Debug, Clone, serde::Serialize)]
pub struct HfModelRow {
    pub id: i64,
    pub repo: String,
    pub file: String,
    pub dest_path: String,
    /// Which models dir this download targets: `chat` (default), `aux` or
    /// `audio`.
    pub target: String,
    pub etag: Option<String>,
    pub size_bytes: Option<i64>,
    pub status: String,
    pub error: Option<String>,
    pub downloaded_at: Option<String>,
}

fn hf_model_from_row(row: &sqlx::sqlite::SqliteRow) -> HfModelRow {
    HfModelRow {
        id: row.get("id"),
        repo: row.get("repo"),
        file: row.get("file"),
        dest_path: row.get("dest_path"),
        target: row.get("target"),
        etag: row.get("etag"),
        size_bytes: row.get("size_bytes"),
        status: row.get("status"),
        error: row.get("error"),
        downloaded_at: row.get("downloaded_at"),
    }
}

/// All tracked downloads (both targets) — used by the startup resume task.
pub async fn list_hf_models(pool: &SqlitePool) -> DbResult<Vec<HfModelRow>> {
    let rows = sqlx::query("SELECT * FROM hf_models ORDER BY repo, file")
        .fetch_all(pool)
        .await?;
    Ok(rows.iter().map(hf_model_from_row).collect())
}

/// Tracked downloads for one target (`chat` / `aux` / `audio`).
pub async fn list_hf_models_by_target(
    pool: &SqlitePool,
    target: &str,
) -> DbResult<Vec<HfModelRow>> {
    let rows = sqlx::query("SELECT * FROM hf_models WHERE target = ?1 ORDER BY repo, file")
        .bind(target)
        .fetch_all(pool)
        .await?;
    Ok(rows.iter().map(hf_model_from_row).collect())
}

pub async fn get_hf_model(pool: &SqlitePool, id: i64) -> DbResult<Option<HfModelRow>> {
    let row = sqlx::query("SELECT * FROM hf_models WHERE id = ?1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(hf_model_from_row))
}

/// Track (repo, file) for download; re-queues the existing row if it is
/// already tracked. Returns the row id.
pub async fn upsert_hf_model(
    pool: &SqlitePool,
    repo: &str,
    file: &str,
    dest_path: &str,
    target: &str,
) -> DbResult<i64> {
    let row = sqlx::query(
        "INSERT INTO hf_models (repo, file, dest_path, target) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(repo, file, target) DO UPDATE SET status='queued', error=NULL, dest_path=?3
         RETURNING id",
    )
    .bind(repo)
    .bind(file)
    .bind(dest_path)
    .bind(target)
    .fetch_one(pool)
    .await?;
    Ok(row.get("id"))
}

pub async fn set_hf_status(
    pool: &SqlitePool,
    id: i64,
    status: &str,
    error: Option<&str>,
) -> DbResult<()> {
    sqlx::query("UPDATE hf_models SET status=?2, error=?3 WHERE id=?1")
        .bind(id)
        .bind(status)
        .bind(error)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn mark_hf_done(
    pool: &SqlitePool,
    id: i64,
    etag: Option<&str>,
    size_bytes: i64,
) -> DbResult<()> {
    sqlx::query(
        "UPDATE hf_models SET status='done', error=NULL, etag=?2, size_bytes=?3,
         downloaded_at=datetime('now') WHERE id=?1",
    )
    .bind(id)
    .bind(etag)
    .bind(size_bytes)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn delete_hf_model(pool: &SqlitePool, id: i64) -> DbResult<()> {
    sqlx::query("DELETE FROM hf_models WHERE id = ?1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}
