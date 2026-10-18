//! `lab_groups` (labs-store design §1.2, migration 0083): one click in a lab,
//! its form, its plan and its override. Writes take a connection inside the
//! caller's `begin_write` (`labs::record`, `labs::delete`); reads take the
//! pool.

use lmgw_api_types::lab_store::LabKind;
use sqlx::{Row, SqliteConnection};

use super::*;

/// One stored group, its JSON columns as the text they were written as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabGroupRow {
    pub id: i64,
    pub lab: String,
    pub title: String,
    pub form: String,
    pub plan: String,
    pub override_text: Option<String>,
    pub created_at: String,
    /// The set run it is an item of (labs-store design §3.1).
    pub batch_id: Option<i64>,
    /// Its place in the set run's file, from 0.
    pub item_ix: Option<i64>,
    /// Its labels, `{question id: label}`, when it has any.
    pub expected: Option<String>,
    /// Its key, the mapping's key column's value, when it has one.
    pub item_key: Option<String>,
}

/// Which groups a read takes by their set run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LabGroupScope {
    /// The history and every set run's items: what usage and an export
    /// count.
    All,
    /// The history alone: the groups of no set run.
    History,
    /// One set run's items.
    Batch(i64),
}

/// Which groups of a lab a read takes: one by id, those whose plan asks a
/// model, those created in `[from, to)` (stored timestamps, UTC), and by
/// their set run. Every part left `None` takes them all.
#[derive(Debug, Clone, Copy)]
pub struct LabGroupFilter<'a> {
    pub lab: LabKind,
    pub group: Option<i64>,
    pub model: Option<&'a str>,
    pub from: Option<&'a str>,
    pub to: Option<&'a str>,
    pub scope: LabGroupScope,
}

impl<'a> LabGroupFilter<'a> {
    /// Every group of `lab`, the set runs' items included.
    pub fn lab(lab: LabKind) -> Self {
        Self {
            lab,
            group: None,
            model: None,
            from: None,
            to: None,
            scope: LabGroupScope::All,
        }
    }
}

/// The condition on `lab_groups` that leaves out the items of a set run
/// that is not ready yet (written in several transactions, migration
/// 0084): a group of the history, or an item of a ready run.
macro_rules! lab_group_ready {
    () => {
        "NOT EXISTS (SELECT 1 FROM lab_batches nb \
          WHERE nb.id = lab_groups.batch_id AND nb.ready = 0)"
    };
}

/// The filter's condition on `lab_groups`, its binds `?1` to `?7` (`?6` 1
/// for the history alone, `?7` one set run), and `?8` a keyset bound
/// (`id < ?8` or `id > ?8`, by the query). A set run that is not ready yet
/// has no items here.
macro_rules! lab_group_filter {
    () => {
        concat!(
            "lab = ?1 AND (?2 IS NULL OR id = ?2) \
             AND (?3 IS NULL OR EXISTS (SELECT 1 FROM json_each(plan) \
                  WHERE json_extract(value, '$.model') = ?3)) \
             AND (?4 IS NULL OR created_at >= ?4) AND (?5 IS NULL OR created_at < ?5) \
             AND (?6 = 0 OR batch_id IS NULL) AND (?7 IS NULL OR batch_id = ?7) AND ",
            lab_group_ready!()
        )
    };
}

/// The columns a [`LabGroupRow`] reads.
macro_rules! lab_group_columns {
    () => {
        "id, lab, title, form, plan, override, created_at, batch_id, item_ix, expected, item_key"
    };
}
pub(crate) use lab_group_columns;

pub(crate) fn lab_group_row(r: &sqlx::sqlite::SqliteRow) -> LabGroupRow {
    LabGroupRow {
        id: r.get("id"),
        lab: r.get("lab"),
        title: r.get("title"),
        form: r.get("form"),
        plan: r.get("plan"),
        override_text: r.get("override"),
        created_at: r.get("created_at"),
        batch_id: r.get("batch_id"),
        item_ix: r.get("item_ix"),
        expected: r.get("expected"),
        item_key: r.get("item_key"),
    }
}

use lab_group_row as row;

/// Insert a group; its id.
pub async fn insert_lab_group(
    conn: &mut SqliteConnection,
    lab: LabKind,
    title: &str,
    form: &str,
    plan: &str,
    override_text: Option<&str>,
) -> DbResult<i64> {
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO lab_groups (lab, title, form, plan, override) VALUES (?1, ?2, ?3, ?4, ?5) \
         RETURNING id",
    )
    .bind(lab.as_str())
    .bind(title)
    .bind(form)
    .bind(plan)
    .bind(override_text)
    .fetch_one(&mut *conn)
    .await?;
    Ok(id)
}

/// Rewrite a new group's form and override once its input artifacts have
/// ids, which they name.
pub async fn set_lab_group_texts(
    conn: &mut SqliteConnection,
    id: i64,
    form: &str,
    override_text: Option<&str>,
) -> DbResult<()> {
    sqlx::query("UPDATE lab_groups SET form = ?2, override = ?3 WHERE id = ?1")
        .bind(id)
        .bind(form)
        .bind(override_text)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Is group `id` there, inside the caller's transaction? A run's record
/// asks first, so a group deleted while its run was on its way is named as
/// that rather than as the foreign key it breaks.
pub async fn lab_group_in(conn: &mut SqliteConnection, id: i64) -> DbResult<bool> {
    let r = sqlx::query("SELECT 1 FROM lab_groups WHERE id = ?1")
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?;
    Ok(r.is_some())
}

/// Group `id` of `lab`; `None` when there is none, it is another lab's, or
/// it is an item of a set run that is not ready yet.
pub async fn lab_group(pool: &SqlitePool, lab: LabKind, id: i64) -> DbResult<Option<LabGroupRow>> {
    let r = sqlx::query(concat!(
        "SELECT ",
        lab_group_columns!(),
        " FROM lab_groups WHERE id = ?1 AND lab = ?2 AND ",
        lab_group_ready!()
    ))
    .bind(id)
    .bind(lab.as_str())
    .fetch_optional(pool)
    .await?;
    Ok(r.as_ref().map(row))
}

fn bind_filter<'q>(
    q: sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments>,
    f: &LabGroupFilter<'_>,
) -> sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments> {
    let (history, batch) = match f.scope {
        LabGroupScope::All => (0, None),
        LabGroupScope::History => (1, None),
        LabGroupScope::Batch(id) => (0, Some(id)),
    };
    q.bind(f.lab.as_str())
        .bind(f.group)
        .bind(f.model)
        .bind(f.from)
        .bind(f.to)
        .bind(history)
        .bind(batch)
}

/// The filter's groups newest first, older than `before` when given, at most
/// `limit` of them when given (no limit is every one).
pub async fn lab_groups_newest(
    pool: &SqlitePool,
    f: &LabGroupFilter<'_>,
    before: Option<i64>,
    limit: Option<u32>,
) -> DbResult<Vec<LabGroupRow>> {
    let q = sqlx::query(concat!(
        "SELECT ",
        lab_group_columns!(),
        " FROM lab_groups WHERE ",
        lab_group_filter!(),
        " AND (?8 IS NULL OR id < ?8) ORDER BY id DESC LIMIT ?9"
    ));
    let rows = bind_filter(q, f)
        .bind(before)
        .bind(limit.map_or(-1, i64::from))
        .fetch_all(pool)
        .await?;
    Ok(rows.iter().map(row).collect())
}

/// The filter's groups oldest first, newer than `after` when given, at most
/// `limit`: an export reads a lab in these batches.
pub async fn lab_groups_oldest(
    pool: &SqlitePool,
    f: &LabGroupFilter<'_>,
    after: Option<i64>,
    limit: u32,
) -> DbResult<Vec<LabGroupRow>> {
    let q = sqlx::query(concat!(
        "SELECT ",
        lab_group_columns!(),
        " FROM lab_groups WHERE ",
        lab_group_filter!(),
        " AND (?8 IS NULL OR id > ?8) ORDER BY id ASC LIMIT ?9"
    ));
    let rows = bind_filter(q, f)
        .bind(after)
        .bind(i64::from(limit))
        .fetch_all(pool)
        .await?;
    Ok(rows.iter().map(row).collect())
}

/// How many groups the filter takes, older than `before` when given.
pub async fn count_lab_groups(
    pool: &SqlitePool,
    f: &LabGroupFilter<'_>,
    before: Option<i64>,
) -> DbResult<u64> {
    let q = sqlx::query(concat!(
        "SELECT COUNT(*) AS n FROM lab_groups WHERE ",
        lab_group_filter!(),
        " AND (?8 IS NULL OR id < ?8)"
    ));
    let n: i64 = bind_filter(q, f)
        .bind(before)
        .fetch_one(pool)
        .await?
        .get("n");
    Ok(n.max(0) as u64)
}

/// How many artifact rows the filter's groups have: whether an export of
/// them is a zip.
pub async fn count_lab_group_artifacts(pool: &SqlitePool, f: &LabGroupFilter<'_>) -> DbResult<u64> {
    let q = sqlx::query(concat!(
        "SELECT COUNT(*) AS n FROM lab_artifacts WHERE group_id IN \
         (SELECT id FROM lab_groups WHERE ",
        lab_group_filter!(),
        ")"
    ));
    let n: i64 = bind_filter(q, f).fetch_one(pool).await?.get("n");
    Ok(n.max(0) as u64)
}

/// Delete group `group` of `lab`, or every group of `lab` when `None`: their
/// runs and artifact rows go with them (`ON DELETE CASCADE`). How many
/// groups went.
pub async fn delete_lab_groups(
    conn: &mut SqliteConnection,
    lab: LabKind,
    group: Option<i64>,
) -> DbResult<u64> {
    let res = sqlx::query("DELETE FROM lab_groups WHERE lab = ?1 AND (?2 IS NULL OR id = ?2)")
        .bind(lab.as_str())
        .bind(group)
        .execute(&mut *conn)
        .await?;
    Ok(res.rows_affected())
}

/// What a lab keeps, counted in the database (labs-store design §1.5); the
/// files no row names are the artifacts directory's to count.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LabCounts {
    pub groups: u64,
    pub batches: u64,
    pub runs: u64,
    pub artifacts: u64,
    pub artifact_files: u64,
    pub text_bytes: u64,
    pub artifact_bytes: u64,
}

/// [`LabCounts`] of `lab`, the set runs' items among its groups; a set run
/// that is not ready yet is not counted, nor are its items. Text is counted
/// in bytes (`CAST … AS BLOB`), not characters.
pub async fn lab_counts(pool: &SqlitePool, lab: LabKind) -> DbResult<LabCounts> {
    let r = sqlx::query(concat!(
        "SELECT
           (SELECT COUNT(*) FROM lab_groups WHERE lab = ?1 AND ",
        lab_group_ready!(),
        ") AS groups,
           (SELECT COUNT(*) FROM lab_batches WHERE lab = ?1 AND ready = 1) AS batches,
           (SELECT COUNT(*) FROM lab_runs r JOIN lab_groups g ON g.id = r.group_id
             WHERE g.lab = ?1) AS runs,
           (SELECT COUNT(*) FROM lab_artifacts a JOIN lab_groups g ON g.id = a.group_id
             WHERE g.lab = ?1) AS artifacts,
           (SELECT IFNULL(SUM(length(CAST(title AS BLOB)) + length(CAST(form AS BLOB))
                   + length(CAST(plan AS BLOB)) + IFNULL(length(CAST(override AS BLOB)), 0)
                   + IFNULL(length(CAST(expected AS BLOB)), 0)
                   + IFNULL(length(CAST(item_key AS BLOB)), 0)), 0)
             FROM lab_groups WHERE lab = ?1 AND ",
        lab_group_ready!(),
        ") AS group_text,
           (SELECT IFNULL(SUM(length(CAST(preset_snapshot AS BLOB))
                   + length(CAST(mapping AS BLOB)) + length(CAST(source_name AS BLOB))), 0)
             FROM lab_batches WHERE lab = ?1 AND ready = 1) AS batch_text,
           (SELECT IFNULL(SUM(length(CAST(r.request AS BLOB))
                   + IFNULL(length(CAST(r.response AS BLOB)), 0)
                   + length(CAST(r.headers AS BLOB))), 0)
             FROM lab_runs r JOIN lab_groups g ON g.id = r.group_id WHERE g.lab = ?1) AS run_text,
           (SELECT COUNT(*) FROM (SELECT DISTINCT a.sha256 FROM lab_artifacts a
             JOIN lab_groups g ON g.id = a.group_id WHERE g.lab = ?1)) AS files,
           (SELECT IFNULL(SUM(size), 0) FROM (SELECT a.sha256, MAX(a.size) AS size
             FROM lab_artifacts a JOIN lab_groups g ON g.id = a.group_id WHERE g.lab = ?1
             GROUP BY a.sha256)) AS file_bytes",
    ))
    .bind(lab.as_str())
    .fetch_one(pool)
    .await?;
    let n = |col: &str| -> u64 { r.get::<i64, _>(col).max(0) as u64 };
    Ok(LabCounts {
        groups: n("groups"),
        batches: n("batches"),
        runs: n("runs"),
        artifacts: n("artifacts"),
        artifact_files: n("files"),
        text_bytes: n("group_text") + n("run_text") + n("batch_text"),
        artifact_bytes: n("file_bytes"),
    })
}
