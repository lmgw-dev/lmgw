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
    /// The revision the download asks for: `main`, or the commit an audio
    /// catalog spec pins. `None` on a row from before it was recorded — a
    /// `main` download.
    pub requested_revision: Option<String>,
    /// The commit the file on disk came from (the hub's `X-Repo-Commit`);
    /// `None` when unknown, never guessed.
    pub resolved_commit: Option<String>,
}

impl HfModelRow {
    /// The revision a transfer of this row fetches.
    pub fn revision(&self) -> &str {
        self.requested_revision.as_deref().unwrap_or("main")
    }
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
        requested_revision: row.get("requested_revision"),
        resolved_commit: row.get("resolved_commit"),
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

/// Track (repo, file) for download at `main`; re-queues the existing row if
/// it is already tracked. Returns the row id.
pub async fn upsert_hf_model(
    pool: &SqlitePool,
    repo: &str,
    file: &str,
    dest_path: &str,
    target: &str,
) -> DbResult<i64> {
    upsert_hf_model_at(pool, repo, file, dest_path, target, "main").await
}

/// [`upsert_hf_model`] at `revision` (`main`, or a commit a spec pins). A
/// re-queue asks for the revision given now; the commit of the file on
/// disk stays until a transfer replaces it.
pub async fn upsert_hf_model_at(
    pool: &SqlitePool,
    repo: &str,
    file: &str,
    dest_path: &str,
    target: &str,
    revision: &str,
) -> DbResult<i64> {
    let row = sqlx::query(
        "INSERT INTO hf_models (repo, file, dest_path, target, requested_revision)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(repo, file, target) DO UPDATE SET status='queued', error=NULL, dest_path=?3,
             requested_revision=?5
         RETURNING id",
    )
    .bind(repo)
    .bind(file)
    .bind(dest_path)
    .bind(target)
    .bind(revision)
    .fetch_one(pool)
    .await?;
    Ok(row.get("id"))
}

/// Point a tracked row at another revision (a re-download under
/// `audio.catalog_revision = latest` of a row taken at a pin).
pub async fn set_hf_revision(pool: &SqlitePool, id: i64, revision: &str) -> DbResult<()> {
    sqlx::query("UPDATE hf_models SET requested_revision=?2 WHERE id=?1")
        .bind(id)
        .bind(revision)
        .execute(pool)
        .await?;
    Ok(())
}

/// The row's file is byte-identical at `revision`: it tracks that revision
/// now, and `commit` (when `revision` is one) is where its bytes came from
/// too. `None` keeps the commit recorded — still a commit with these bytes.
pub async fn set_hf_revision_matched(
    pool: &SqlitePool,
    id: i64,
    revision: &str,
    commit: Option<&str>,
) -> DbResult<()> {
    sqlx::query(
        "UPDATE hf_models SET requested_revision=?2, resolved_commit=COALESCE(?3, resolved_commit)
         WHERE id=?1",
    )
    .bind(id)
    .bind(revision)
    .bind(commit)
    .execute(pool)
    .await?;
    Ok(())
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

/// A finished transfer: the file's ETag and size, the revision it fetched,
/// and the commit it came from (`None`: unknown). The revision is written
/// again because a re-queue may have rewritten the row's while this transfer
/// ran at the one it started with — the row says what the file on disk is.
pub async fn mark_hf_done(
    pool: &SqlitePool,
    id: i64,
    etag: Option<&str>,
    size_bytes: i64,
    revision: &str,
    commit: Option<&str>,
) -> DbResult<()> {
    sqlx::query(
        "UPDATE hf_models SET status='done', error=NULL, etag=?2, size_bytes=?3,
         downloaded_at=datetime('now'), requested_revision=?4, resolved_commit=?5 WHERE id=?1",
    )
    .bind(id)
    .bind(etag)
    .bind(size_bytes)
    .bind(revision)
    .bind(commit)
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
