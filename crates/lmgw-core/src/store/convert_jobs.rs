//! The `convert_jobs` and `convert_job_files` tables (document-conversion
//! design §4.4, §7.3; migration 0114): async conversion jobs, their files'
//! states and counts. The files' converted text is on disk
//! (`extract::convert::jobs::results`), never here. Every write is one
//! `begin_write` transaction; a job's ending is conditional on it still
//! running, so a cancel and the last file's outcome cannot both end it.
//!
//! Times are RFC 3339 UTC to the second (`stamp`), so they compare as text.

use chrono::{DateTime, Utc};
use sqlx::{Row, SqlitePool};

use super::{stamp, DbResult};

/// A job to write.
#[derive(Debug, Clone)]
pub struct NewJob<'a> {
    pub id: &'a str,
    pub key_id: Option<i64>,
    /// `native` or `docling`.
    pub origin: &'a str,
    pub created_at: DateTime<Utc>,
    pub input_bytes: i64,
    /// The request's own notes, JSON.
    pub warnings: &'a str,
    /// A docling request's answer settings, JSON.
    pub adapter: Option<&'a str>,
}

/// A file to write with its job: pending, or with the outcome it already
/// has (a file the request's `from_formats` left out).
#[derive(Debug, Clone)]
pub struct NewJobFile {
    pub idx: i64,
    pub name: String,
    pub bytes: i64,
    pub state: &'static str,
}

/// A job, as stored.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JobRow {
    pub id: String,
    pub key_id: Option<i64>,
    pub origin: String,
    pub state: String,
    pub created_at: String,
    pub finished_at: Option<String>,
    pub fetched_at: Option<String>,
    pub files: i64,
    pub input_bytes: i64,
    pub parts: i64,
    pub parts_done: i64,
    pub error: Option<String>,
    pub warnings: String,
    pub tools: String,
    pub adapter: Option<String>,
}

/// A job's file, as stored.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JobFileRow {
    pub idx: i64,
    pub name: String,
    pub bytes: i64,
    pub state: String,
    pub parts: i64,
    pub parts_done: i64,
    pub error: Option<String>,
}

fn job(r: &sqlx::sqlite::SqliteRow) -> JobRow {
    JobRow {
        id: r.get("id"),
        key_id: r.get("key_id"),
        origin: r.get("origin"),
        state: r.get("state"),
        created_at: r.get("created_at"),
        finished_at: r.get("finished_at"),
        fetched_at: r.get("fetched_at"),
        files: r.get("files"),
        input_bytes: r.get("input_bytes"),
        parts: r.get("parts"),
        parts_done: r.get("parts_done"),
        error: r.get("error"),
        warnings: r.get("warnings"),
        tools: r.get("tools"),
        adapter: r.get("adapter"),
    }
}

/// Write `j` and its files, `running`.
pub async fn insert_job(pool: &SqlitePool, j: &NewJob<'_>, files: &[NewJobFile]) -> DbResult<()> {
    let mut tx = super::begin_write(pool).await?;
    sqlx::query(
        "INSERT INTO convert_jobs (id, key_id, origin, state, created_at, files, input_bytes,
             warnings, adapter)
         VALUES (?1, ?2, ?3, 'running', ?4, ?5, ?6, ?7, ?8)",
    )
    .bind(j.id)
    .bind(j.key_id)
    .bind(j.origin)
    .bind(stamp(j.created_at))
    .bind(i64::try_from(files.len()).unwrap_or(i64::MAX))
    .bind(j.input_bytes)
    .bind(j.warnings)
    .bind(j.adapter)
    .execute(&mut *tx)
    .await?;
    for f in files {
        sqlx::query(
            "INSERT INTO convert_job_files (job_id, idx, name, bytes, state)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .bind(j.id)
        .bind(f.idx)
        .bind(&f.name)
        .bind(f.bytes)
        .bind(f.state)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// File `idx` of a running job has its outcome: its state, parts and error
/// code, and the job's sums with it. `false` when the job is not running
/// any more (cancelled meanwhile).
pub async fn file_done(
    pool: &SqlitePool,
    id: &str,
    idx: i64,
    state: &str,
    parts: i64,
    error: Option<&str>,
) -> DbResult<bool> {
    let one = FileDone {
        idx,
        state: state.to_string(),
        parts,
        error: error.map(str::to_string),
    };
    files_done(pool, id, std::slice::from_ref(&one)).await
}

/// One file's outcome for [`files_done`].
#[derive(Debug, Clone)]
pub struct FileDone {
    pub idx: i64,
    pub state: String,
    pub parts: i64,
    pub error: Option<String>,
}

/// [`file_done`] for several files of job `id` in one transaction.
pub async fn files_done(pool: &SqlitePool, id: &str, done: &[FileDone]) -> DbResult<bool> {
    let mut tx = super::begin_write(pool).await?;
    let running = sqlx::query("SELECT 1 FROM convert_jobs WHERE id = ?1 AND state = 'running'")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .is_some();
    if !running {
        return Ok(false);
    }
    let mut parts_sum = 0i64;
    for d in done {
        sqlx::query(
            "UPDATE convert_job_files SET state = ?3, parts = ?4, parts_done = ?4, error = ?5
             WHERE job_id = ?1 AND idx = ?2 AND state = 'pending'",
        )
        .bind(id)
        .bind(d.idx)
        .bind(&d.state)
        .bind(d.parts)
        // Bounded as every stored error is (`request_logs::error_msg`).
        .bind(
            d.error
                .as_deref()
                .map(|e| super::error_msg::bounded(e).into_owned()),
        )
        .execute(&mut *tx)
        .await?;
        parts_sum = parts_sum.saturating_add(d.parts);
    }
    sqlx::query(
        "UPDATE convert_jobs SET parts = parts + ?2, parts_done = parts_done + ?2 WHERE id = ?1",
    )
    .bind(id)
    .bind(parts_sum)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(true)
}

/// End running job `id` as `state` (`succeeded`, `failed`, `cancelled`),
/// its files still pending as `pending_as`, with its tools (JSON) and its
/// error. `false` when it had ended already.
pub async fn finish_job(
    pool: &SqlitePool,
    id: &str,
    state: &str,
    pending_as: &str,
    tools: &str,
    error: Option<&str>,
) -> DbResult<bool> {
    let mut tx = super::begin_write(pool).await?;
    let ended = sqlx::query(
        "UPDATE convert_jobs SET state = ?2, finished_at = ?3, tools = ?4, error = ?5
         WHERE id = ?1 AND state = 'running'",
    )
    .bind(id)
    .bind(state)
    .bind(stamp(Utc::now()))
    .bind(tools)
    .bind(error.map(|e| super::error_msg::bounded(e).into_owned()))
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if ended == 0 {
        return Ok(false);
    }
    sqlx::query("UPDATE convert_job_files SET state = ?2 WHERE job_id = ?1 AND state = 'pending'")
        .bind(id)
        .bind(pending_as)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(true)
}

/// Every job still `running` at start: the process that ran it is gone.
/// Each becomes `interrupted`, its pending files with it; files with an
/// outcome keep it. The ids, so their results can be kept or swept.
pub async fn interrupt_running_jobs(pool: &SqlitePool) -> DbResult<Vec<String>> {
    let mut tx = super::begin_write(pool).await?;
    let ids: Vec<String> = sqlx::query("SELECT id FROM convert_jobs WHERE state = 'running'")
        .fetch_all(&mut *tx)
        .await?
        .iter()
        .map(|r| r.get("id"))
        .collect();
    let error = "the gateway stopped while this job ran; the files that had finished keep \
                 their results";
    for id in &ids {
        sqlx::query(
            "UPDATE convert_jobs SET state = 'interrupted', finished_at = ?2, error = ?3
             WHERE id = ?1",
        )
        .bind(id)
        .bind(stamp(Utc::now()))
        .bind(error)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE convert_job_files SET state = 'interrupted'
             WHERE job_id = ?1 AND state = 'pending'",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(ids)
}

/// Job `id`.
pub async fn get_job(pool: &SqlitePool, id: &str) -> DbResult<Option<JobRow>> {
    Ok(sqlx::query("SELECT * FROM convert_jobs WHERE id = ?1")
        .bind(id)
        .fetch_optional(pool)
        .await?
        .as_ref()
        .map(job))
}

/// Job `id`'s files, in request order.
pub async fn job_files(pool: &SqlitePool, id: &str) -> DbResult<Vec<JobFileRow>> {
    Ok(sqlx::query(
        "SELECT idx, name, bytes, state, parts, parts_done, error FROM convert_job_files
         WHERE job_id = ?1 ORDER BY idx",
    )
    .bind(id)
    .fetch_all(pool)
    .await?
    .iter()
    .map(|r| JobFileRow {
        idx: r.get("idx"),
        name: r.get("name"),
        bytes: r.get("bytes"),
        state: r.get("state"),
        parts: r.get("parts"),
        parts_done: r.get("parts_done"),
        error: r.get("error"),
    })
    .collect())
}

/// A docling client's first result fetch of job `id`: the stamp it has (the
/// first one's, on a later fetch), and whether this call set it.
pub async fn mark_fetched(pool: &SqlitePool, id: &str) -> DbResult<(Option<String>, bool)> {
    let mut tx = super::begin_write(pool).await?;
    let first =
        sqlx::query("UPDATE convert_jobs SET fetched_at = ?2 WHERE id = ?1 AND fetched_at IS NULL")
            .bind(id)
            .bind(stamp(Utc::now()))
            .execute(&mut *tx)
            .await?
            .rows_affected()
            > 0;
    let at = sqlx::query("SELECT fetched_at FROM convert_jobs WHERE id = ?1")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .and_then(|r| r.get::<Option<String>, _>("fetched_at"));
    tx.commit().await?;
    Ok((at, first))
}

/// The ended jobs to delete now: ended before `ended_before` (`None`: the
/// retention keeps them), or fetched by a docling client before
/// `fetched_before`.
pub async fn jobs_to_delete(
    pool: &SqlitePool,
    ended_before: Option<DateTime<Utc>>,
    fetched_before: DateTime<Utc>,
) -> DbResult<Vec<String>> {
    let ended = ended_before.map(stamp);
    Ok(sqlx::query(
        "SELECT id FROM convert_jobs
         WHERE state != 'running'
           AND ((?1 IS NOT NULL AND finished_at < ?1) OR (fetched_at IS NOT NULL AND fetched_at < ?2))",
    )
    .bind(ended)
    .bind(stamp(fetched_before))
    .fetch_all(pool)
    .await?
    .iter()
    .map(|r| r.get("id"))
    .collect())
}

/// Every job id (the sweep's list of directories to keep).
pub async fn job_ids(pool: &SqlitePool) -> DbResult<Vec<String>> {
    Ok(sqlx::query("SELECT id FROM convert_jobs")
        .fetch_all(pool)
        .await?
        .iter()
        .map(|r| r.get("id"))
        .collect())
}

/// Delete job `id` and its files' rows.
pub async fn delete_job(pool: &SqlitePool, id: &str) -> DbResult<()> {
    let mut tx = super::begin_write(pool).await?;
    sqlx::query("DELETE FROM convert_jobs WHERE id = ?1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// Jobs running now, and the files of those that have no outcome yet.
pub async fn queue(pool: &SqlitePool) -> DbResult<(u64, u64)> {
    let jobs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM convert_jobs WHERE state = 'running'")
        .fetch_one(pool)
        .await?;
    let files: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM convert_job_files f JOIN convert_jobs j ON j.id = f.job_id
         WHERE j.state = 'running' AND f.state = 'pending'",
    )
    .fetch_one(pool)
    .await?;
    Ok((
        u64::try_from(jobs).unwrap_or(0),
        u64::try_from(files).unwrap_or(0),
    ))
}
