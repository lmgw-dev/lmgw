//! `lab_batches` (labs-store design §3.1, migration 0084): set runs, and
//! their items, which are `lab_groups` rows with `batch_id` set. A batch is
//! written in several short write transactions, so a large file does not
//! hold the write lock past the busy timeout for every other writer: its
//! row first, not ready, then its items in chunks (each chunk one set-based
//! `INSERT … SELECT … FROM json_each`), and the last chunk marks it ready.
//! Until then nothing runs, lists or counts it; a boot deletes what a crash
//! left of one. Their runs come one by one through `labs::record`, as a lab
//! cell's do.

use std::collections::HashMap;

use lmgw_api_types::lab_store::LabKind;
use sqlx::{Row, SqliteConnection};

use super::*;

/// One stored set run, its JSON columns as the text they were written as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabBatchRow {
    pub id: i64,
    pub lab: String,
    pub preset_id: Option<i64>,
    pub preset_snapshot: String,
    pub model: String,
    pub source_name: String,
    pub mapping: String,
    pub item_count: i64,
    pub concurrency: i64,
    pub job_id: Option<i64>,
    pub created_at: String,
}

/// A set run to write.
#[derive(Debug, Clone)]
pub struct NewLabBatch<'a> {
    pub lab: LabKind,
    pub preset_id: Option<i64>,
    pub preset_snapshot: &'a str,
    pub model: &'a str,
    pub source_name: &'a str,
    pub mapping: &'a str,
    pub concurrency: i64,
    /// Every item's plan: its one cell.
    pub plan: &'a str,
}

/// One item of a new set run: a group with one cell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewLabItem {
    pub title: String,
    pub form: String,
    /// `{question id: label}`, `None` when the item has no label.
    pub expected: Option<String>,
    /// The mapping's key column's value, `None` without one.
    pub key: Option<String>,
}

/// What a set run's items say so far.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LabBatchProgress {
    /// Items with a run.
    pub done: u64,
    /// Of those, the runs that are no 2xx.
    pub failed: u64,
    /// Of those, the 2xx runs judged not to read as an answer to every
    /// question of the set (`lab_runs.readable` 0).
    pub unreadable: u64,
    /// Items with an expected answer.
    pub labelled: u64,
}

/// How a chunked write went: how many write transactions it took, and the
/// longest of them (from `BEGIN IMMEDIATE` returning to the commit's end),
/// which every other writer may have waited for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChunkedWrite {
    pub transactions: u32,
    pub longest: std::time::Duration,
}

/// How many items one write transaction takes at most, and how many bytes
/// of item text (forms, titles, labels, keys): an internal batching
/// parameter that keeps each transaction short, not a limit on the input,
/// which is written in as many chunks as it needs. Measured on 100,000
/// items (labs-store design §3.5).
const CHUNK_ITEMS: usize = 2_000;
const CHUNK_BYTES: usize = 4 << 20;

/// One item and its run's answer, what the metrics read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabBatchAnswer {
    pub item_ix: i64,
    pub expected: Option<String>,
    /// `None`: no run yet.
    pub http_status: Option<i64>,
    pub response: Option<String>,
}

macro_rules! columns {
    () => {
        "id, lab, preset_id, preset_snapshot, model, source_name, mapping, item_count, \
         concurrency, job_id, created_at"
    };
}

fn row(r: &sqlx::sqlite::SqliteRow) -> LabBatchRow {
    LabBatchRow {
        id: r.get("id"),
        lab: r.get("lab"),
        preset_id: r.get("preset_id"),
        preset_snapshot: r.get("preset_snapshot"),
        model: r.get("model"),
        source_name: r.get("source_name"),
        mapping: r.get("mapping"),
        item_count: r.get("item_count"),
        concurrency: r.get("concurrency"),
        job_id: r.get("job_id"),
        created_at: r.get("created_at"),
    }
}

/// Write a set run and its items in item order, in several short write
/// transactions: the run's row not ready, the items in chunks, the last
/// chunk marking it ready. The run's id and how the write went; `None` when
/// the run was deleted while it was written (a Clear in between). A failure
/// half way deletes what was written, and a boot deletes what a crash left.
pub async fn insert_lab_batch(
    pool: &SqlitePool,
    b: &NewLabBatch<'_>,
    items: &[NewLabItem],
) -> DbResult<Option<(i64, ChunkedWrite)>> {
    let mut how = ChunkedWrite::default();
    let mut timed = |started: std::time::Instant| {
        how.transactions += 1;
        how.longest = how.longest.max(started.elapsed());
    };
    let mut tx = begin_write(pool).await?;
    let started = std::time::Instant::now();
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO lab_batches (lab, preset_id, preset_snapshot, model, source_name, mapping,
            item_count, concurrency, ready) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0)
         RETURNING id",
    )
    .bind(b.lab.as_str())
    .bind(b.preset_id)
    .bind(b.preset_snapshot)
    .bind(b.model)
    .bind(b.source_name)
    .bind(b.mapping)
    .bind(items.len() as i64)
    .bind(b.concurrency)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    timed(started);

    let written = async {
        let mut first = 0usize;
        loop {
            let mut end = first;
            let mut bytes = 0usize;
            while end < items.len() && end - first < CHUNK_ITEMS && bytes < CHUNK_BYTES {
                let i = &items[end];
                bytes += i.title.len()
                    + i.form.len()
                    + i.expected.as_ref().map_or(0, String::len)
                    + i.key.as_ref().map_or(0, String::len);
                end += 1;
            }
            let last = end == items.len();
            let (kept, started) =
                insert_items(pool, b, id, first, &items[first..end], last).await?;
            timed(started);
            if !kept {
                return Ok::<bool, GatewayError>(false);
            }
            if last {
                return Ok(true);
            }
            first = end;
        }
    };
    match written.await {
        Ok(true) => Ok(Some((id, how))),
        Ok(false) => Ok(None),
        Err(e) => {
            if let Err(again) = delete_unready_lab_batch(pool, id).await {
                tracing::warn!(
                    "lab store: set run {id} half written is left for the next boot: {again}"
                );
            }
            Err(e)
        }
    }
}

/// Items `items` of set run `id`, from index `first`, in one write
/// transaction and one set-based insert; with `last`, the run is marked
/// ready in it. Whether the run is still there, and when the write
/// transaction began (the chunk's time is the transaction's, not the
/// serialization or the wait for the writer).
async fn insert_items(
    pool: &SqlitePool,
    b: &NewLabBatch<'_>,
    id: i64,
    first: usize,
    items: &[NewLabItem],
    last: bool,
) -> DbResult<(bool, std::time::Instant)> {
    let rows: Vec<(&str, &str, Option<&str>, Option<&str>)> = items
        .iter()
        .map(|i| {
            (
                i.title.as_str(),
                i.form.as_str(),
                i.expected.as_deref(),
                i.key.as_deref(),
            )
        })
        .collect();
    let rows = serde_json::to_string(&rows).map_err(|e| GatewayError::Internal(e.to_string()))?;
    let mut tx = begin_write(pool).await?;
    let started = std::time::Instant::now();
    let there: Option<i64> =
        sqlx::query_scalar("SELECT id FROM lab_batches WHERE id = ?1 AND ready = 0")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    if there.is_none() {
        return Ok((false, started));
    }
    sqlx::query(
        "INSERT INTO lab_groups (lab, title, form, plan, batch_id, item_ix, expected, item_key)
         SELECT ?1, json_extract(value, '$[0]'), json_extract(value, '$[1]'), ?2, ?3,
                ?4 + key, json_extract(value, '$[2]'), json_extract(value, '$[3]')
         FROM json_each(?5) ORDER BY key",
    )
    .bind(b.lab.as_str())
    .bind(b.plan)
    .bind(id)
    .bind(first as i64)
    .bind(rows)
    .execute(&mut *tx)
    .await?;
    if last {
        sqlx::query("UPDATE lab_batches SET ready = 1 WHERE id = ?1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok((true, started))
}

/// Delete set run `id` if it is not ready, with what was written of its
/// items.
async fn delete_unready_lab_batch(pool: &SqlitePool, id: i64) -> DbResult<()> {
    let mut tx = begin_write(pool).await?;
    sqlx::query("DELETE FROM lab_batches WHERE id = ?1 AND ready = 0")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// Delete every set run a crash left half written (not ready), with its
/// items: at boot, before anything can write one. How many went. Their
/// items have no runs and no artifact rows, so no file is left unnamed.
pub async fn delete_unready_lab_batches(pool: &SqlitePool) -> DbResult<u64> {
    let mut tx = begin_write(pool).await?;
    let n = sqlx::query("DELETE FROM lab_batches WHERE ready = 0")
        .execute(&mut *tx)
        .await?
        .rows_affected();
    tx.commit().await?;
    Ok(n)
}

/// Name the job that runs set run `id` now, and its concurrency when given.
pub async fn set_lab_batch_job(
    pool: &SqlitePool,
    id: i64,
    job_id: i64,
    concurrency: Option<i64>,
) -> DbResult<()> {
    let mut tx = begin_write(pool).await?;
    sqlx::query(
        "UPDATE lab_batches SET job_id = ?2, concurrency = IFNULL(?3, concurrency) WHERE id = ?1",
    )
    .bind(id)
    .bind(job_id)
    .bind(concurrency)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Set run `id`, of `lab` when given; `None` while it is not ready.
pub async fn lab_batch(
    pool: &SqlitePool,
    lab: Option<LabKind>,
    id: i64,
) -> DbResult<Option<LabBatchRow>> {
    let r = sqlx::query(concat!(
        "SELECT ",
        columns!(),
        " FROM lab_batches WHERE id = ?1 AND (?2 IS NULL OR lab = ?2) AND ready = 1"
    ))
    .bind(id)
    .bind(lab.map(LabKind::as_str))
    .fetch_optional(pool)
    .await?;
    Ok(r.as_ref().map(row))
}

/// `lab`'s set runs that are ready, newest first.
pub async fn lab_batches(pool: &SqlitePool, lab: LabKind) -> DbResult<Vec<LabBatchRow>> {
    let rows = sqlx::query(concat!(
        "SELECT ",
        columns!(),
        " FROM lab_batches WHERE lab = ?1 AND ready = 1 ORDER BY id DESC"
    ))
    .bind(lab.as_str())
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(row).collect())
}

/// [`LabBatchProgress`] of each of the set runs `ids`.
pub async fn lab_batch_progress(
    pool: &SqlitePool,
    ids: &[i64],
) -> DbResult<HashMap<i64, LabBatchProgress>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let list = serde_json::to_string(ids).unwrap_or_else(|_| "[]".into());
    let rows = sqlx::query(
        "SELECT g.batch_id AS batch_id,
                COUNT(r.id) AS done,
                IFNULL(SUM(CASE WHEN r.id IS NOT NULL
                                 AND (r.http_status < 200 OR r.http_status > 299)
                           THEN 1 ELSE 0 END), 0) AS failed,
                IFNULL(SUM(CASE WHEN r.readable = 0 THEN 1 ELSE 0 END), 0) AS unreadable,
                IFNULL(SUM(CASE WHEN g.expected IS NOT NULL THEN 1 ELSE 0 END), 0) AS labelled
         FROM lab_groups g LEFT JOIN lab_runs r ON r.group_id = g.id
         WHERE g.batch_id IN (SELECT value FROM json_each(?1))
         GROUP BY g.batch_id",
    )
    .bind(list)
    .fetch_all(pool)
    .await?;
    let n = |r: &sqlx::sqlite::SqliteRow, col: &str| r.get::<i64, _>(col).max(0) as u64;
    Ok(rows
        .iter()
        .map(|r| {
            (
                r.get::<i64, _>("batch_id"),
                LabBatchProgress {
                    done: n(r, "done"),
                    failed: n(r, "failed"),
                    unreadable: n(r, "unreadable"),
                    labelled: n(r, "labelled"),
                },
            )
        })
        .collect())
}

/// The ids of set run `id`'s items that have no run, and with
/// `retry_failed` those whose run failed (no 2xx) too, in item order: each
/// with whether it has a failed run.
pub async fn lab_batch_pending(
    pool: &SqlitePool,
    id: i64,
    retry_failed: bool,
) -> DbResult<Vec<(i64, bool)>> {
    let rows = sqlx::query(
        "SELECT g.id AS id, EXISTS (SELECT 1 FROM lab_runs r WHERE r.group_id = g.id) AS ran
         FROM lab_groups g WHERE g.batch_id = ?1
           AND NOT EXISTS (SELECT 1 FROM lab_runs r WHERE r.group_id = g.id
                             AND (?2 = 0 OR r.http_status BETWEEN 200 AND 299))
         ORDER BY g.item_ix",
    )
    .bind(id)
    .bind(retry_failed)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|r| (r.get::<i64, _>("id"), r.get::<bool, _>("ran")))
        .collect())
}

/// Set run `id`'s items after index `after` when given, in item order, at
/// most `limit` of them when given (no limit is every one).
pub async fn lab_batch_items(
    pool: &SqlitePool,
    id: i64,
    after: Option<i64>,
    limit: Option<u32>,
) -> DbResult<Vec<LabGroupRow>> {
    let rows = sqlx::query(concat!(
        "SELECT ",
        lab_group_columns!(),
        " FROM lab_groups WHERE batch_id = ?1 AND (?2 IS NULL OR item_ix > ?2) \
         ORDER BY item_ix LIMIT ?3"
    ))
    .bind(id)
    .bind(after)
    .bind(limit.map_or(-1, i64::from))
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(lab_group_row).collect())
}

/// How many items set run `id` has.
pub async fn count_lab_batch_items(pool: &SqlitePool, id: i64) -> DbResult<u64> {
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM lab_groups WHERE batch_id = ?1")
        .bind(id)
        .fetch_one(pool)
        .await?;
    Ok(n.max(0) as u64)
}

/// Whether set run `id` has an item after index `after`.
pub async fn lab_batch_item_after(pool: &SqlitePool, id: i64, after: i64) -> DbResult<bool> {
    let n: Option<i64> =
        sqlx::query_scalar("SELECT 1 FROM lab_groups WHERE batch_id = ?1 AND item_ix > ?2 LIMIT 1")
            .bind(id)
            .bind(after)
            .fetch_optional(pool)
            .await?;
    Ok(n.is_some())
}

/// Every item of set run `id` with its run's status and answer, in item
/// order.
pub async fn lab_batch_answers(pool: &SqlitePool, id: i64) -> DbResult<Vec<LabBatchAnswer>> {
    let rows = sqlx::query(
        "SELECT g.item_ix AS item_ix, g.expected AS expected, r.http_status AS http_status,
                r.response AS response
         FROM lab_groups g LEFT JOIN lab_runs r ON r.group_id = g.id AND r.cell = 0
         WHERE g.batch_id = ?1 ORDER BY g.item_ix",
    )
    .bind(id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|r| LabBatchAnswer {
            item_ix: r.get::<Option<i64>, _>("item_ix").unwrap_or_default(),
            expected: r.get("expected"),
            http_status: r.get("http_status"),
            response: r.get("response"),
        })
        .collect())
}

/// The distinct files the artifact rows of set run `id`'s items name: what
/// its delete may leave unnamed.
pub async fn lab_batch_artifact_shas(
    conn: &mut SqliteConnection,
    id: i64,
) -> DbResult<Vec<String>> {
    let shas = sqlx::query_scalar(
        "SELECT DISTINCT a.sha256 FROM lab_artifacts a JOIN lab_groups g ON g.id = a.group_id
         WHERE g.batch_id = ?1",
    )
    .bind(id)
    .fetch_all(&mut *conn)
    .await?;
    Ok(shas)
}

/// Delete set run `batch` of `lab`, or every set run of `lab` when `None`
/// (one being written too): their items go with them, and the items' runs
/// and artifact rows with those (`ON DELETE CASCADE`). How many set runs
/// went.
pub async fn delete_lab_batches(
    conn: &mut SqliteConnection,
    lab: LabKind,
    batch: Option<i64>,
) -> DbResult<u64> {
    let res = sqlx::query("DELETE FROM lab_batches WHERE lab = ?1 AND (?2 IS NULL OR id = ?2)")
        .bind(lab.as_str())
        .bind(batch)
        .execute(&mut *conn)
        .await?;
    Ok(res.rows_affected())
}
