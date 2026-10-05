//! Aux models CRUD (the aux router's preset sections: embed + rerank)

use sqlx::{Row, SqlitePool};

use crate::config::{AuxKind, AuxModel, HoldFallbackMode};
use crate::error::GatewayError;

use super::*;

pub(super) fn aux_model_from_row(row: &sqlx::sqlite::SqliteRow) -> AuxModel {
    AuxModel {
        id: row.get("id"),
        model_id: row.get("model_id"),
        gguf_path: row.get("gguf_path"),
        kind: AuxKind::parse(row.get::<String, _>("kind").as_str()).unwrap_or(AuxKind::Embed),
        pooling: row.get("pooling"),
        ctx_size: row.get("ctx_size"),
        args: parse_json_or(row.get::<String, _>("args").as_str()),
        idle_seconds: row.get("idle_seconds"),
        enabled: row.get::<i64, _>("enabled") != 0,
        image: row.get("image"),
        extra_run_args: parse_json_opt(row.get::<Option<String>, _>("extra_run_args")),
        warm_start: row.get::<i64, _>("warm_start") != 0,
        hold_fallback_mode: hold_fallback_mode_from_row(row),
        hold_fallback: row.get("hold_fallback"),
    }
}

pub struct NewAuxModel {
    pub model_id: String,
    pub gguf_path: String,
    pub kind: AuxKind,
    pub pooling: Option<String>,
    pub ctx_size: Option<i64>,
    pub args: Vec<String>,
    pub idle_seconds: i64,
    pub enabled: bool,
    /// `None` inherits the aux class settings' image/extra_run_args (§3.1).
    pub image: Option<String>,
    pub extra_run_args: Option<Vec<String>>,
    pub warm_start: bool,
    /// GPU-hold fallback mode (gpu-hold design §2/§3.2).
    pub hold_fallback_mode: HoldFallbackMode,
    /// Meaningful only when `hold_fallback_mode` is `Alias`.
    pub hold_fallback: Option<String>,
}

pub async fn list_aux_models(pool: &SqlitePool) -> DbResult<Vec<AuxModel>> {
    let rows = sqlx::query("SELECT * FROM aux_models ORDER BY model_id")
        .fetch_all(pool)
        .await?;
    Ok(rows.iter().map(aux_model_from_row).collect())
}

pub async fn get_aux_model(pool: &SqlitePool, id: i64) -> DbResult<Option<AuxModel>> {
    let row = sqlx::query("SELECT * FROM aux_models WHERE id = ?1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(aux_model_from_row))
}

pub async fn insert_aux_model(pool: &SqlitePool, m: &NewAuxModel) -> DbResult<i64> {
    let args = serde_json::to_string(&m.args).map_err(|e| GatewayError::Internal(e.to_string()))?;
    let extra_run_args = run_args_json(&m.extra_run_args)?;
    let res = sqlx::query(
        "INSERT INTO aux_models (model_id, gguf_path, kind, pooling, ctx_size, args, idle_seconds,
                                  enabled, image, extra_run_args, warm_start, hold_fallback_mode,
                                  hold_fallback)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
    )
    .bind(&m.model_id)
    .bind(&m.gguf_path)
    .bind(m.kind.as_str())
    .bind(&m.pooling)
    .bind(m.ctx_size)
    .bind(args)
    .bind(m.idle_seconds)
    .bind(m.enabled as i64)
    .bind(&m.image)
    .bind(extra_run_args)
    .bind(m.warm_start as i64)
    .bind(m.hold_fallback_mode.as_str())
    .bind(&m.hold_fallback)
    .execute(pool)
    .await?;
    Ok(res.last_insert_rowid())
}

pub async fn update_aux_model(pool: &SqlitePool, id: i64, m: &NewAuxModel) -> DbResult<()> {
    let args = serde_json::to_string(&m.args).map_err(|e| GatewayError::Internal(e.to_string()))?;
    let extra_run_args = run_args_json(&m.extra_run_args)?;
    sqlx::query(
        "UPDATE aux_models SET model_id=?2, gguf_path=?3, kind=?4, pooling=?5, ctx_size=?6, args=?7,
         idle_seconds=?8, enabled=?9, image=?10, extra_run_args=?11, warm_start=?12,
         hold_fallback_mode=?13, hold_fallback=?14,
         updated_at=datetime('now') WHERE id=?1",
    )
    .bind(id)
    .bind(&m.model_id)
    .bind(&m.gguf_path)
    .bind(m.kind.as_str())
    .bind(&m.pooling)
    .bind(m.ctx_size)
    .bind(args)
    .bind(m.idle_seconds)
    .bind(m.enabled as i64)
    .bind(&m.image)
    .bind(extra_run_args)
    .bind(m.warm_start as i64)
    .bind(m.hold_fallback_mode.as_str())
    .bind(&m.hold_fallback)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn delete_aux_model(pool: &SqlitePool, id: i64) -> DbResult<()> {
    sqlx::query("DELETE FROM aux_models WHERE id = ?1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}
