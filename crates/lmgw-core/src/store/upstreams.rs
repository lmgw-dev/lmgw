//! Upstreams CRUD

use sqlx::SqlitePool;

use crate::config::{Protocol, Upstream, UpstreamKind};
use crate::error::GatewayError;

use super::*;

pub struct NewUpstream {
    pub name: String,
    pub protocol: Protocol,
    pub kind: UpstreamKind,
    pub base_url: String,
    pub api_key: Option<String>,
    pub extra_headers: Vec<(String, String)>,
    pub timeout_ms: u64,
    pub enabled: bool,
    pub expose_all: bool,
    pub expose_prefix: String,
    pub supports_responses: bool,
}

pub async fn list_upstreams(pool: &SqlitePool) -> DbResult<Vec<Upstream>> {
    let rows = sqlx::query("SELECT * FROM upstreams ORDER BY name")
        .fetch_all(pool)
        .await?;
    Ok(rows.iter().map(upstream_from_row).collect())
}

pub async fn get_upstream(pool: &SqlitePool, id: i64) -> DbResult<Option<Upstream>> {
    let row = sqlx::query("SELECT * FROM upstreams WHERE id = ?1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(upstream_from_row))
}

/// Look up an upstream by its unique name (used to find/sync the managed
/// aux-router upstream row).
pub async fn get_upstream_by_name(pool: &SqlitePool, name: &str) -> DbResult<Option<Upstream>> {
    let row = sqlx::query("SELECT * FROM upstreams WHERE name = ?1")
        .bind(name)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(upstream_from_row))
}

pub async fn insert_upstream(pool: &SqlitePool, u: &NewUpstream) -> DbResult<i64> {
    let headers = serde_json::to_string(&u.extra_headers)
        .map_err(|e| GatewayError::Internal(e.to_string()))?;
    let res = sqlx::query(
        "INSERT INTO upstreams (name, protocol, kind, base_url, api_key, extra_headers, timeout_ms, enabled, expose_all, expose_prefix, supports_responses)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
    )
    .bind(&u.name)
    .bind(u.protocol.as_str())
    .bind(u.kind.as_str())
    .bind(&u.base_url)
    .bind(&u.api_key)
    .bind(headers)
    .bind(u.timeout_ms as i64)
    .bind(u.enabled as i64)
    .bind(u.expose_all as i64)
    .bind(&u.expose_prefix)
    .bind(u.supports_responses as i64)
    .execute(pool)
    .await?;
    Ok(res.last_insert_rowid())
}

/// `api_key: None` keeps the stored key (UI sends masked value back).
pub async fn update_upstream(
    pool: &SqlitePool,
    id: i64,
    u: &NewUpstream,
    update_key: bool,
) -> DbResult<()> {
    let headers = serde_json::to_string(&u.extra_headers)
        .map_err(|e| GatewayError::Internal(e.to_string()))?;
    if update_key {
        sqlx::query(
            "UPDATE upstreams SET name=?2, protocol=?3, kind=?4, base_url=?5, api_key=?6,
             extra_headers=?7, timeout_ms=?8, enabled=?9, expose_all=?10, expose_prefix=?11,
             supports_responses=?12, updated_at=datetime('now') WHERE id=?1",
        )
        .bind(id)
        .bind(&u.name)
        .bind(u.protocol.as_str())
        .bind(u.kind.as_str())
        .bind(&u.base_url)
        .bind(&u.api_key)
        .bind(headers)
        .bind(u.timeout_ms as i64)
        .bind(u.enabled as i64)
        .bind(u.expose_all as i64)
        .bind(&u.expose_prefix)
        .bind(u.supports_responses as i64)
        .execute(pool)
        .await?;
    } else {
        sqlx::query(
            "UPDATE upstreams SET name=?2, protocol=?3, kind=?4, base_url=?5,
             extra_headers=?6, timeout_ms=?7, enabled=?8, expose_all=?9, expose_prefix=?10,
             supports_responses=?11, updated_at=datetime('now') WHERE id=?1",
        )
        .bind(id)
        .bind(&u.name)
        .bind(u.protocol.as_str())
        .bind(u.kind.as_str())
        .bind(&u.base_url)
        .bind(headers)
        .bind(u.timeout_ms as i64)
        .bind(u.enabled as i64)
        .bind(u.expose_all as i64)
        .bind(&u.expose_prefix)
        .bind(u.supports_responses as i64)
        .execute(pool)
        .await?;
    }
    Ok(())
}

pub async fn delete_upstream(pool: &SqlitePool, id: i64) -> DbResult<()> {
    sqlx::query("DELETE FROM upstreams WHERE id = ?1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Hide (or unhide) several models of one upstream's catalog at once, as one
/// transaction: "hide the 17 shown" either lands whole or not at all, rather
/// than leaving the owner to find out which of the 17 went through.
pub async fn set_passthrough_hidden(
    pool: &SqlitePool,
    upstream_id: i64,
    model_ids: &[String],
    hidden: bool,
) -> DbResult<()> {
    let sql = if hidden {
        "INSERT OR IGNORE INTO hidden_passthrough_models (upstream_id, model_id) VALUES (?1, ?2)"
    } else {
        "DELETE FROM hidden_passthrough_models WHERE upstream_id = ?1 AND model_id = ?2"
    };
    let mut tx = pool.begin().await?;
    for model_id in model_ids {
        sqlx::query(sql)
            .bind(upstream_id)
            .bind(model_id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}
