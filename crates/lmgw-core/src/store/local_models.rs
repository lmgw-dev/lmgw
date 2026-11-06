//! Local models CRUD

use sqlx::SqlitePool;

use crate::config::{LlamaParams, LocalModel};
use crate::error::GatewayError;

use super::*;

pub struct NewLocalModel {
    pub model_id: String,
    pub gguf_path: String,
    pub params: LlamaParams,
    pub args: Vec<String>,
    pub idle_seconds: i64,
    pub enabled: bool,
    pub public: bool,
    /// `None` inherits the chat class settings' image/extra_run_args (§3.1).
    pub image: Option<String>,
    pub extra_run_args: Option<Vec<String>>,
    pub warm_start: bool,
    /// The row's one fallback (openai egress design §14.2); `None` = none.
    pub fallback: Option<String>,
    /// Owner override of the derived `/v1/models` capability facts
    /// (model-capabilities design §7). `None` = no override.
    pub capabilities_override: Option<serde_json::Value>,
    /// Ladder rungs above the base (ladder design §4.1). Empty = not a
    /// ladder.
    pub ladder: Vec<crate::ladder::Rung>,
}

pub async fn list_local_models(pool: &SqlitePool) -> DbResult<Vec<LocalModel>> {
    let rows = sqlx::query("SELECT * FROM local_models ORDER BY model_id")
        .fetch_all(pool)
        .await?;
    Ok(rows.iter().map(local_model_from_row).collect())
}

pub async fn insert_local_model(pool: &SqlitePool, m: &NewLocalModel) -> DbResult<i64> {
    insert_local_model_in(&mut *pool.acquire().await?, m).await
}

/// [`insert_local_model`] inside a caller's transaction: the fallback save checks
/// write it in a dry run first (`ops::fallback::referrers`).
pub async fn insert_local_model_in(
    conn: &mut sqlx::SqliteConnection,
    m: &NewLocalModel,
) -> DbResult<i64> {
    let params =
        serde_json::to_string(&m.params).map_err(|e| GatewayError::Internal(e.to_string()))?;
    let args = serde_json::to_string(&m.args).map_err(|e| GatewayError::Internal(e.to_string()))?;
    let extra_run_args = run_args_json(&m.extra_run_args)?;
    let capabilities_override = to_json_opt(&m.capabilities_override)?;
    let ladder = to_json(&m.ladder)?;
    let res = sqlx::query(
        "INSERT INTO local_models (model_id, gguf_path, params, args, idle_seconds, enabled, public,
                                    image, extra_run_args, warm_start, fallback,
                                    capabilities_override, ladder)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
    )
    .bind(&m.model_id)
    .bind(&m.gguf_path)
    .bind(params)
    .bind(args)
    .bind(m.idle_seconds)
    .bind(m.enabled as i64)
    .bind(m.public as i64)
    .bind(&m.image)
    .bind(extra_run_args)
    .bind(m.warm_start as i64)
    .bind(&m.fallback)
    .bind(capabilities_override)
    .bind(ladder)
    .execute(&mut *conn)
    .await?;
    Ok(res.last_insert_rowid())
}

pub async fn update_local_model(pool: &SqlitePool, id: i64, m: &NewLocalModel) -> DbResult<()> {
    update_local_model_in(&mut *pool.acquire().await?, id, m).await
}

/// [`update_local_model`] inside a caller's transaction: a rename rewrites the
/// fallbacks that name the row in the same one ([`rewrite_fallbacks_in`]).
pub async fn update_local_model_in(
    conn: &mut sqlx::SqliteConnection,
    id: i64,
    m: &NewLocalModel,
) -> DbResult<()> {
    let params =
        serde_json::to_string(&m.params).map_err(|e| GatewayError::Internal(e.to_string()))?;
    let args = serde_json::to_string(&m.args).map_err(|e| GatewayError::Internal(e.to_string()))?;
    let extra_run_args = run_args_json(&m.extra_run_args)?;
    let capabilities_override = to_json_opt(&m.capabilities_override)?;
    let ladder = to_json(&m.ladder)?;
    sqlx::query(
        "UPDATE local_models SET model_id=?2, gguf_path=?3, params=?4, args=?5, idle_seconds=?6,
         enabled=?7, public=?8, image=?9, extra_run_args=?10, warm_start=?11,
         fallback=?12, capabilities_override=?13, ladder=?14,
         updated_at=datetime('now') WHERE id=?1",
    )
    .bind(id)
    .bind(&m.model_id)
    .bind(&m.gguf_path)
    .bind(params)
    .bind(args)
    .bind(m.idle_seconds)
    .bind(m.enabled as i64)
    .bind(m.public as i64)
    .bind(&m.image)
    .bind(extra_run_args)
    .bind(m.warm_start as i64)
    .bind(&m.fallback)
    .bind(capabilities_override)
    .bind(ladder)
    .execute(conn)
    .await?;
    Ok(())
}

pub async fn delete_local_model(pool: &SqlitePool, id: i64) -> DbResult<()> {
    sqlx::query("DELETE FROM local_models WHERE id = ?1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}
