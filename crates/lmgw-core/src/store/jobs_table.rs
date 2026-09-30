//! Background jobs (§9c)

use sqlx::{Row, SqlitePool};

use super::*;

/// One row of the `jobs` table — the durable half of a background job. The
/// live half (byte counters that move many times a second) lives in
/// [`crate::jobs::JobManager`] and is written back here on a throttle.
#[derive(Debug, Clone, serde::Serialize)]
pub struct JobRow {
    pub id: i64,
    /// Job kind name; see [`crate::jobs::JobKind`]. Text, not an enum: a row
    /// written by a newer build stays readable.
    pub kind: String,
    /// Dedup key scoped to the kind, unique among non-terminal rows.
    pub key: Option<String>,
    pub label: String,
    /// `queued | running | done | failed | canceled`.
    pub status: String,
    /// Kind-specific request payload (JSON text).
    pub input: String,
    /// Last progress snapshot (JSON text of [`crate::jobs::JobProgress`]).
    pub progress: String,
    /// Terminal payload (JSON text), kind-specific.
    pub result: Option<String>,
    pub error: Option<String>,
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}

pub(super) fn job_from_row(row: &sqlx::sqlite::SqliteRow) -> JobRow {
    JobRow {
        id: row.get("id"),
        kind: row.get("kind"),
        key: row.get("key"),
        label: row.get("label"),
        status: row.get("status"),
        input: row.get("input"),
        progress: row.get("progress"),
        result: row.get("result"),
        error: row.get("error"),
        created_at: row.get("created_at"),
        started_at: row.get("started_at"),
        finished_at: row.get("finished_at"),
    }
}

/// Insert a `queued` job, unless `key` already names a non-terminal job of the
/// same kind — the duplicate guard, enforced by the partial unique index.
/// `Ok(None)` means "one is already in flight", not an error.
pub async fn claim_job(
    pool: &SqlitePool,
    kind: &str,
    key: Option<&str>,
    label: &str,
    input: &str,
) -> DbResult<Option<i64>> {
    let row = sqlx::query(
        "INSERT INTO jobs (kind, key, label, input) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT DO NOTHING RETURNING id",
    )
    .bind(kind)
    .bind(key)
    .bind(label)
    .bind(input)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| r.get("id")))
}

pub async fn get_job(pool: &SqlitePool, id: i64) -> DbResult<Option<JobRow>> {
    let row = sqlx::query("SELECT * FROM jobs WHERE id = ?1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(job_from_row))
}

/// The non-terminal job holding `(kind, key)`, if any.
pub async fn active_job_by_key(
    pool: &SqlitePool,
    kind: &str,
    key: &str,
) -> DbResult<Option<JobRow>> {
    let row = sqlx::query(
        "SELECT * FROM jobs WHERE kind = ?1 AND key = ?2
           AND status IN ('queued','running') LIMIT 1",
    )
    .bind(kind)
    .bind(key)
    .fetch_optional(pool)
    .await?;
    Ok(row.as_ref().map(job_from_row))
}

/// Newest first. `limit <= 0` means every row the retention rules have kept —
/// the table is already bounded by a visible setting, so there is no second,
/// invisible cap here.
pub async fn list_jobs(
    pool: &SqlitePool,
    kind: Option<&str>,
    active_only: bool,
    limit: i64,
) -> DbResult<Vec<JobRow>> {
    let rows = sqlx::query(
        "SELECT * FROM jobs
         WHERE (?1 IS NULL OR kind = ?1)
           AND (?2 = 0 OR status IN ('queued','running'))
         ORDER BY id DESC
         LIMIT (CASE WHEN ?3 > 0 THEN ?3 ELSE -1 END)",
    )
    .bind(kind)
    .bind(i64::from(active_only))
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(job_from_row).collect())
}

pub async fn start_job(pool: &SqlitePool, id: i64) -> DbResult<()> {
    sqlx::query("UPDATE jobs SET status='running', started_at=datetime('now') WHERE id=?1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn set_job_progress(pool: &SqlitePool, id: i64, progress: &str) -> DbResult<()> {
    sqlx::query("UPDATE jobs SET progress=?2 WHERE id=?1")
        .bind(id)
        .bind(progress)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn finish_job(
    pool: &SqlitePool,
    id: i64,
    status: &str,
    result: Option<&str>,
    error: Option<&str>,
) -> DbResult<()> {
    sqlx::query(
        "UPDATE jobs SET status=?2, result=?3, error=?4, finished_at=datetime('now') WHERE id=?1",
    )
    .bind(id)
    .bind(status)
    .bind(result)
    .bind(error)
    .execute(pool)
    .await?;
    Ok(())
}

/// Fail every non-terminal row. Called once at startup: nothing can be running
/// in a process that has just opened the DB, so a `running` row is the residue
/// of a crash or a shutdown and must not linger as a phantom live job (it would
/// also hold the `(kind, key)` guard against a legitimate re-run). Returns the
/// number of rows closed.
pub async fn fail_orphaned_jobs(pool: &SqlitePool) -> DbResult<u64> {
    let res = sqlx::query(
        "UPDATE jobs SET status='failed', error='interrupted by shutdown',
             finished_at=datetime('now')
         WHERE status IN ('queued','running')",
    )
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Trim finished job rows by age and by count, mirroring [`prune_logs`]. Both
/// bounds come from visible settings; `0` disables that rule. Live jobs are
/// never touched.
pub async fn prune_jobs(pool: &SqlitePool, days: i64, max_rows: i64) -> DbResult<u64> {
    let mut removed = 0u64;
    if days > 0 {
        let res = sqlx::query(
            "DELETE FROM jobs WHERE status NOT IN ('queued','running')
               AND COALESCE(finished_at, created_at) < datetime('now', ?1)",
        )
        .bind(format!("-{days} days"))
        .execute(pool)
        .await?;
        removed += res.rows_affected();
    }
    if max_rows > 0 {
        let res = sqlx::query(
            "DELETE FROM jobs WHERE status NOT IN ('queued','running') AND id <= (
                 SELECT id FROM jobs WHERE status NOT IN ('queued','running')
                 ORDER BY id DESC LIMIT 1 OFFSET ?1
             )",
        )
        .bind(max_rows)
        .execute(pool)
        .await?;
        removed += res.rows_affected();
    }
    Ok(removed)
}
