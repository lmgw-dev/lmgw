//! The `convert_calls` table (document-conversion design §7.8, D17;
//! migration 0114): what one conversion request sent, beside its
//! request-log row and deleted with it. Routes, counts, option names with
//! their enum-like values and outcomes, and the hosts of `http` sources;
//! never file names, content, URLs, headers or prompts (the caller builds
//! the record from those rules: `extract::convert::calls`).

use sqlx::{Row, SqlitePool};

use super::DbResult;

/// One request's record.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NewConvertCall {
    /// The route, as called (`/v1/convert/file`, `/v1/documents/convert`).
    pub route: String,
    pub files: i64,
    /// docling's target kind (`inbody`, `zip`); `None` on lmgw's own route.
    pub target: Option<String>,
    /// `[{"name", "value", "outcome"}]`.
    pub options: String,
    /// `[{"host", "outcome"}]`.
    pub hosts: String,
}

/// Write `c` beside request-log row `log_id`.
pub async fn insert_convert_call(
    pool: &SqlitePool,
    log_id: i64,
    c: &NewConvertCall,
) -> DbResult<()> {
    let mut tx = super::begin_write(pool).await?;
    sqlx::query(
        "INSERT INTO convert_calls (log_id, route, files, target, options, hosts)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )
    .bind(log_id)
    .bind(&c.route)
    .bind(c.files)
    .bind(&c.target)
    .bind(&c.options)
    .bind(&c.hosts)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// A record with its request's key and time.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConvertCallRow {
    pub key_id: Option<i64>,
    pub client_key: Option<String>,
    pub ts: String,
    pub route: String,
    pub files: i64,
    pub target: Option<String>,
    pub options: String,
    pub hosts: String,
}

/// Every record whose route starts with `prefix`, oldest first.
pub async fn convert_calls(pool: &SqlitePool, prefix: &str) -> DbResult<Vec<ConvertCallRow>> {
    Ok(sqlx::query(
        "SELECT r.key_id, r.client_key, r.ts, c.route, c.files, c.target, c.options, c.hosts
         FROM convert_calls c JOIN request_logs r ON r.id = c.log_id
         WHERE substr(c.route, 1, length(?1)) = ?1
         ORDER BY c.log_id",
    )
    .bind(prefix)
    .fetch_all(pool)
    .await?
    .iter()
    .map(|r| ConvertCallRow {
        key_id: r.get("key_id"),
        client_key: r.get("client_key"),
        ts: r.get("ts"),
        route: r.get("route"),
        files: r.get("files"),
        target: r.get("target"),
        options: r.get("options"),
        hosts: r.get("hosts"),
    })
    .collect())
}
