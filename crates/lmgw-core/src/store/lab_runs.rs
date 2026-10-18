//! `lab_runs` (labs-store design §1.2, migration 0083): one dispatch to one
//! `/v1` route, its request and answer kept as text.

use sqlx::{Row, SqliteConnection};

use super::*;

/// What a run records, the columns but its id and time.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LabRunData {
    pub group_id: i64,
    pub cell: i64,
    pub variant: String,
    pub endpoint: String,
    pub model_asked: String,
    pub answered_by: String,
    pub fallback_reason: Option<String>,
    pub upstream_model: Option<String>,
    pub http_status: i64,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    pub request: String,
    pub response: Option<String>,
    pub response_type: Option<String>,
    /// The `x-lmgw-*` headers as a JSON object's text.
    pub headers: String,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub latency_ms: i64,
    /// A set run item's 2xx run: whether it reads as an answer to every
    /// question of the item's form. `None` for a failed run and any run not
    /// of a set run (migration 0084).
    pub readable: Option<bool>,
}

/// One stored run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabRunRow {
    pub id: i64,
    pub created_at: String,
    pub data: LabRunData,
}

fn row(r: &sqlx::sqlite::SqliteRow) -> LabRunRow {
    LabRunRow {
        id: r.get("id"),
        created_at: r.get("created_at"),
        data: LabRunData {
            group_id: r.get("group_id"),
            cell: r.get("cell"),
            variant: r.get("variant"),
            endpoint: r.get("endpoint"),
            model_asked: r.get("model_asked"),
            answered_by: r.get("answered_by"),
            fallback_reason: r.get("fallback_reason"),
            upstream_model: r.get("upstream_model"),
            http_status: r.get("http_status"),
            error_code: r.get("error_code"),
            error_message: r.get("error_message"),
            request: r.get("request"),
            response: r.get("response"),
            response_type: r.get("response_type"),
            headers: r.get("headers"),
            input_tokens: r.get("input_tokens"),
            output_tokens: r.get("output_tokens"),
            latency_ms: r.get("latency_ms"),
            readable: r.get::<Option<i64>, _>("readable").map(|v| v != 0),
        },
    }
}

/// Insert a run; its id and time, `None` when the cell has a run already
/// (`UNIQUE (group_id, cell)`). A run of a deleted group is refused by its
/// foreign key.
pub async fn insert_lab_run(
    conn: &mut SqliteConnection,
    d: &LabRunData,
) -> DbResult<Option<(i64, String)>> {
    let r = sqlx::query(
        "INSERT INTO lab_runs (group_id, cell, variant, endpoint, model_asked, answered_by,
            fallback_reason, upstream_model, http_status, error_code, error_message, request,
            response, response_type, headers, input_tokens, output_tokens, latency_ms, readable)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18,
            ?19)
         ON CONFLICT (group_id, cell) DO NOTHING
         RETURNING id, created_at",
    )
    .bind(d.group_id)
    .bind(d.cell)
    .bind(&d.variant)
    .bind(&d.endpoint)
    .bind(&d.model_asked)
    .bind(&d.answered_by)
    .bind(&d.fallback_reason)
    .bind(&d.upstream_model)
    .bind(d.http_status)
    .bind(&d.error_code)
    .bind(&d.error_message)
    .bind(&d.request)
    .bind(&d.response)
    .bind(&d.response_type)
    .bind(&d.headers)
    .bind(d.input_tokens)
    .bind(d.output_tokens)
    .bind(d.latency_ms)
    .bind(d.readable.map(i64::from))
    .fetch_optional(&mut *conn)
    .await?;
    Ok(r.map(|r| (r.get("id"), r.get("created_at"))))
}

/// Rewrite run `id`'s stored request and response, in the transaction that
/// recorded it: the texts naming its new artifacts by id once they have
/// them (`lab-artifact:<id>` where the bytes were).
pub async fn set_lab_run_texts(
    conn: &mut SqliteConnection,
    id: i64,
    request: &str,
    response: Option<&str>,
) -> DbResult<()> {
    sqlx::query("UPDATE lab_runs SET request = ?2, response = ?3 WHERE id = ?1")
        .bind(id)
        .bind(request)
        .bind(response)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Delete cell `cell` of group `group_id`'s run when it failed (no 2xx),
/// in the transaction that records its replacement: the files its output
/// rows named, which may now be unnamed.
pub async fn delete_failed_lab_run(
    conn: &mut SqliteConnection,
    group_id: i64,
    cell: i64,
) -> DbResult<Vec<String>> {
    let shas: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT a.sha256 FROM lab_artifacts a JOIN lab_runs r ON r.id = a.run_id
         WHERE r.group_id = ?1 AND r.cell = ?2 AND (r.http_status < 200 OR r.http_status > 299)",
    )
    .bind(group_id)
    .bind(cell)
    .fetch_all(&mut *conn)
    .await?;
    sqlx::query(
        "DELETE FROM lab_runs WHERE group_id = ?1 AND cell = ?2
           AND (http_status < 200 OR http_status > 299)",
    )
    .bind(group_id)
    .bind(cell)
    .execute(&mut *conn)
    .await?;
    Ok(shas)
}

/// Whether cell `cell` of group `group_id` has a run; with `but_failed`, a
/// run that is a 2xx (a failed one is to be replaced).
pub async fn lab_run_exists(
    pool: &SqlitePool,
    group_id: i64,
    cell: i64,
    but_failed: bool,
) -> DbResult<bool> {
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM lab_runs WHERE group_id = ?1 AND cell = ?2
           AND (?3 = 0 OR http_status BETWEEN 200 AND 299)",
    )
    .bind(group_id)
    .bind(cell)
    .bind(but_failed)
    .fetch_one(pool)
    .await?;
    Ok(n > 0)
}

/// Every run of the groups `ids`, by group and cell.
pub async fn lab_runs_of(pool: &SqlitePool, ids: &[i64]) -> DbResult<Vec<LabRunRow>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let ids = serde_json::to_string(ids).unwrap_or_else(|_| "[]".into());
    let rows = sqlx::query(
        "SELECT * FROM lab_runs WHERE group_id IN (SELECT value FROM json_each(?1)) \
         ORDER BY group_id, cell",
    )
    .bind(ids)
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(row).collect())
}
