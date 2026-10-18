//! Aux models CRUD (the aux router's preset sections: embed, rerank, decision)

use sqlx::{Row, SqlitePool};

use crate::config::{AuxBackend, AuxKind, AuxModel, HoldFallbackMode};
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
        ubatch_size: row.get("ubatch_size"),
        batch_size: row.get("batch_size"),
        mmproj_path: row.get("mmproj_path"),
        // The column's CHECK admits 'cpu' and NULL only.
        backend: row
            .get::<Option<String>, _>("backend")
            .as_deref()
            .and_then(AuxBackend::parse),
        threads: row.get("threads"),
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
    /// The five fields of decision-models design §3.3; `None` everywhere is
    /// the row as it rendered before they existed.
    pub ubatch_size: Option<i64>,
    pub batch_size: Option<i64>,
    pub mmproj_path: Option<String>,
    pub backend: Option<AuxBackend>,
    pub threads: Option<i64>,
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
                                  hold_fallback, ubatch_size, batch_size, mmproj_path, backend,
                                  threads)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
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
    .bind(m.ubatch_size)
    .bind(m.batch_size)
    .bind(&m.mmproj_path)
    .bind(m.backend.map(|b| b.as_str()))
    .bind(m.threads)
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
         hold_fallback_mode=?13, hold_fallback=?14, ubatch_size=?15, batch_size=?16,
         mmproj_path=?17, backend=?18, threads=?19,
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
    .bind(m.ubatch_size)
    .bind(m.batch_size)
    .bind(&m.mmproj_path)
    .bind(m.backend.map(|b| b.as_str()))
    .bind(m.threads)
    .execute(pool)
    .await?;
    Ok(())
}

/// Write back what the boot fold hoisted for one row (decision-models design
/// §3.3, WP2b; `runtime::lifecycle::hoist_aux_args`): `now`'s args and its
/// five fields, only while the stored row still holds `was`'s — those, and
/// the image and run args the fold judged them by (`--n-gpu-layers 0`
/// becomes `backend: cpu` by the effective image). A save that landed after
/// the fold read its snapshot wins, and the next start hoists what that save
/// left. `false` when the row moved on or is gone.
pub async fn store_aux_hoist(pool: &SqlitePool, was: &AuxModel, now: &AuxModel) -> DbResult<bool> {
    let fields = |m: &AuxModel| {
        (
            m.args.clone(),
            m.ubatch_size,
            m.batch_size,
            m.mmproj_path.clone(),
            m.backend,
            m.threads,
            m.image.clone(),
            m.extra_run_args.clone(),
        )
    };
    let args =
        serde_json::to_string(&now.args).map_err(|e| GatewayError::Internal(e.to_string()))?;
    // Reads before it writes: the write lock first.
    let mut tx = begin_write(pool).await?;
    let row = sqlx::query("SELECT * FROM aux_models WHERE id = ?1")
        .bind(was.id)
        .fetch_optional(&mut *tx)
        .await?;
    if row.as_ref().map(aux_model_from_row).map(|m| fields(&m)) != Some(fields(was)) {
        return Ok(false);
    }
    sqlx::query(
        "UPDATE aux_models SET args=?2, ubatch_size=?3, batch_size=?4, mmproj_path=?5,
         backend=?6, threads=?7, updated_at=datetime('now') WHERE id=?1",
    )
    .bind(was.id)
    .bind(args)
    .bind(now.ubatch_size)
    .bind(now.batch_size)
    .bind(&now.mmproj_path)
    .bind(now.backend.map(|b| b.as_str()))
    .bind(now.threads)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(true)
}

pub async fn delete_aux_model(pool: &SqlitePool, id: i64) -> DbResult<()> {
    sqlx::query("DELETE FROM aux_models WHERE id = ?1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}
