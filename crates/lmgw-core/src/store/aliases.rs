//! Model aliases CRUD

use sqlx::SqlitePool;

use crate::config::ModelAlias;
use crate::error::GatewayError;
use crate::ir::Params;

use super::*;

pub struct NewAlias {
    pub alias: String,
    pub upstream_id: i64,
    pub upstream_model_id: String,
    pub param_overrides: Params,
    pub enabled: bool,
    /// Owner override of the derived `/v1/models` capability facts
    /// (model-capabilities design §7). `None` = no override.
    pub capabilities_override: Option<serde_json::Value>,
}

pub async fn list_aliases(pool: &SqlitePool) -> DbResult<Vec<ModelAlias>> {
    let rows = sqlx::query("SELECT * FROM models ORDER BY alias")
        .fetch_all(pool)
        .await?;
    Ok(rows.iter().map(alias_from_row).collect())
}

pub async fn insert_alias(pool: &SqlitePool, a: &NewAlias) -> DbResult<i64> {
    let overrides = serde_json::to_string(&a.param_overrides)
        .map_err(|e| GatewayError::Internal(e.to_string()))?;
    let capabilities_override = to_json_opt(&a.capabilities_override)?;
    let res = sqlx::query(
        "INSERT INTO models (alias, upstream_id, upstream_model_id, param_overrides, enabled,
                              capabilities_override)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )
    .bind(&a.alias)
    .bind(a.upstream_id)
    .bind(&a.upstream_model_id)
    .bind(overrides)
    .bind(a.enabled as i64)
    .bind(capabilities_override)
    .execute(pool)
    .await?;
    Ok(res.last_insert_rowid())
}

pub async fn update_alias(pool: &SqlitePool, id: i64, a: &NewAlias) -> DbResult<()> {
    let overrides = serde_json::to_string(&a.param_overrides)
        .map_err(|e| GatewayError::Internal(e.to_string()))?;
    let capabilities_override = to_json_opt(&a.capabilities_override)?;
    sqlx::query(
        "UPDATE models SET alias=?2, upstream_id=?3, upstream_model_id=?4, param_overrides=?5,
         enabled=?6, capabilities_override=?7, updated_at=datetime('now') WHERE id=?1",
    )
    .bind(id)
    .bind(&a.alias)
    .bind(a.upstream_id)
    .bind(&a.upstream_model_id)
    .bind(overrides)
    .bind(a.enabled as i64)
    .bind(capabilities_override)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn delete_alias(pool: &SqlitePool, id: i64) -> DbResult<()> {
    sqlx::query("DELETE FROM models WHERE id = ?1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}
