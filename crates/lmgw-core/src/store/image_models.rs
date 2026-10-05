//! Image models CRUD (the sd-server pipelines)

use sqlx::{Row, SqlitePool};

use crate::config::{HoldFallbackMode, ImageModel};
use crate::error::GatewayError;

use super::*;

pub(super) fn image_model_from_row(row: &sqlx::sqlite::SqliteRow) -> ImageModel {
    ImageModel {
        id: row.get("id"),
        // The same tolerance the audio JSON columns take: a column that will
        // not parse reads as empty rather than failing the whole snapshot
        // load, because one hand-edited row must not stop the gateway.
        files: parse_json_or(row.get::<String, _>("files").as_str()),
        args: parse_json_or(row.get::<String, _>("args").as_str()),
        modes: parse_json_or(row.get::<String, _>("modes").as_str()),
        model_id: row.get("model_id"),
        edit: row.get::<i64, _>("edit") != 0,
        enabled: row.get::<i64, _>("enabled") != 0,
        image: row.get("image"),
        extra_run_args: parse_json_opt(row.get::<Option<String>, _>("extra_run_args")),
        warm_start: row.get::<i64, _>("warm_start") != 0,
        idle_seconds: row.get("idle_seconds"),
        hold_fallback_mode: hold_fallback_mode_from_row(row),
        hold_fallback: row.get("hold_fallback"),
        capabilities_override: capabilities_override_from_row(
            row.get::<Option<String>, _>("capabilities_override"),
        ),
        // Learned, not configured (0035): written only by `set_image_model_peak`
        // below, which is why `NewImageModel` has no field for it and neither
        // insert nor update names the column.
        peak_extra_bytes: row
            .get::<Option<i64>, _>("peak_extra_bytes")
            .and_then(|b| u64::try_from(b).ok()),
        peak_learned_at: row.get("peak_learned_at"),
    }
}

#[derive(Debug, Default)]
pub struct NewImageModel {
    pub model_id: String,
    pub files: serde_json::Map<String, serde_json::Value>,
    pub args: serde_json::Map<String, serde_json::Value>,
    pub modes: Vec<String>,
    pub edit: bool,
    pub enabled: bool,
    /// `None` inherits the image class settings' image/extra_run_args.
    pub image: Option<String>,
    pub extra_run_args: Option<Vec<String>>,
    pub warm_start: bool,
    pub idle_seconds: i64,
    pub hold_fallback_mode: HoldFallbackMode,
    /// Meaningful only when `hold_fallback_mode` is `Alias`.
    pub hold_fallback: Option<String>,
    pub capabilities_override: Option<serde_json::Value>,
}

pub async fn list_image_models(pool: &SqlitePool) -> DbResult<Vec<ImageModel>> {
    let rows = sqlx::query("SELECT * FROM image_models ORDER BY model_id")
        .fetch_all(pool)
        .await?;
    Ok(rows.iter().map(image_model_from_row).collect())
}

pub async fn get_image_model(pool: &SqlitePool, id: i64) -> DbResult<Option<ImageModel>> {
    let row = sqlx::query("SELECT * FROM image_models WHERE id = ?1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(image_model_from_row))
}

/// The three JSON columns of one row, in bind order.
fn image_model_json(m: &NewImageModel) -> DbResult<(String, String, String)> {
    Ok((to_json(&m.files)?, to_json(&m.args)?, to_json(&m.modes)?))
}

pub async fn insert_image_model(pool: &SqlitePool, m: &NewImageModel) -> DbResult<i64> {
    let (files, args, modes) = image_model_json(m)?;
    let extra_run_args = run_args_json(&m.extra_run_args)?;
    let capabilities_override = to_json_opt(&m.capabilities_override)?;
    let res = sqlx::query(
        "INSERT INTO image_models (model_id, files, args, modes, edit, enabled, image,
                                   extra_run_args, warm_start, idle_seconds, hold_fallback_mode,
                                   hold_fallback, capabilities_override)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
    )
    .bind(&m.model_id)
    .bind(files)
    .bind(args)
    .bind(modes)
    .bind(m.edit as i64)
    .bind(m.enabled as i64)
    .bind(&m.image)
    .bind(extra_run_args)
    .bind(m.warm_start as i64)
    .bind(m.idle_seconds)
    .bind(m.hold_fallback_mode.as_str())
    .bind(&m.hold_fallback)
    .bind(capabilities_override)
    .execute(pool)
    .await?;
    Ok(res.last_insert_rowid())
}

pub async fn update_image_model(pool: &SqlitePool, id: i64, m: &NewImageModel) -> DbResult<()> {
    let (files, args, modes) = image_model_json(m)?;
    let extra_run_args = run_args_json(&m.extra_run_args)?;
    let capabilities_override = to_json_opt(&m.capabilities_override)?;
    sqlx::query(
        "UPDATE image_models SET model_id=?2, files=?3, args=?4, modes=?5, edit=?6, enabled=?7,
         image=?8, extra_run_args=?9, warm_start=?10, idle_seconds=?11, hold_fallback_mode=?12,
         hold_fallback=?13, capabilities_override=?14,
         updated_at=datetime('now') WHERE id=?1",
    )
    .bind(id)
    .bind(&m.model_id)
    .bind(files)
    .bind(args)
    .bind(modes)
    .bind(m.edit as i64)
    .bind(m.enabled as i64)
    .bind(&m.image)
    .bind(extra_run_args)
    .bind(m.warm_start as i64)
    .bind(m.idle_seconds)
    .bind(m.hold_fallback_mode.as_str())
    .bind(&m.hold_fallback)
    .bind(capabilities_override)
    .execute(pool)
    .await?;
    Ok(())
}

/// Record what one generation cost above this pipeline's idle residency
/// (image-generation §9, migration 0035).
///
/// Its own statement rather than a field on [`NewImageModel`], because this is
/// the one value in the row lmgw writes by itself: `update_image_model` above
/// names every *configured* column and deliberately not this one, so an owner
/// saving a row never overwrites a measurement, and a measurement never
/// rewrites an owner's row. `updated_at` is left alone for the same reason —
/// it means "the configuration changed", and learning a peak is not that.
///
/// `None` clears the figure back to unlearned, which is what a `files` or
/// `args` change does to it.
pub async fn set_image_model_peak(
    pool: &SqlitePool,
    id: i64,
    peak_extra_bytes: Option<u64>,
) -> DbResult<()> {
    sqlx::query(
        "UPDATE image_models
            SET peak_extra_bytes = ?2,
                peak_learned_at  = CASE WHEN ?2 IS NULL THEN NULL ELSE datetime('now') END
          WHERE id = ?1",
    )
    .bind(id)
    // SQLite INTEGER is signed. Nothing a GPU can report comes near the edge,
    // so this is a conversion that cannot fail in practice — and it *says* so
    // rather than silently storing NULL (which would read as "never learned")
    // if it ever did.
    .bind(match peak_extra_bytes {
        Some(b) => Some(i64::try_from(b).map_err(|_| {
            GatewayError::Internal(format!(
                "a learned peak of {b} bytes does not fit an INTEGER"
            ))
        })?),
        None => None,
    })
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn delete_image_model(pool: &SqlitePool, id: i64) -> DbResult<()> {
    sqlx::query("DELETE FROM image_models WHERE id = ?1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}
