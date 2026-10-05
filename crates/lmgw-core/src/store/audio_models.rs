//! Audio models CRUD (the audiocpp_server container's server.json entries)

use sqlx::{Row, SqlitePool};

use crate::config::{AudioModel, HoldFallbackMode, LearnedResidency};
use crate::error::GatewayError;

use super::*;

pub(super) fn audio_model_from_row(row: &sqlx::sqlite::SqliteRow) -> AudioModel {
    AudioModel {
        id: row.get("id"),
        model_id: row.get("model_id"),
        family: row.get("family"),
        path: row.get("path"),
        task: row.get("task"),
        mode: row.get("mode"),
        lazy: row.get::<Option<i64>, _>("lazy").map(|v| v != 0),
        busy_timeout_ms: row.get("busy_timeout_ms"),
        backend: row.get("backend"),
        threads: row.get("threads"),
        load_options: parse_json_or(row.get::<String, _>("load_options").as_str()),
        session_options: parse_json_or(row.get::<String, _>("session_options").as_str()),
        default_request_options: parse_json_or(
            row.get::<String, _>("default_request_options").as_str(),
        ),
        model_spec_override: row.get("model_spec_override"),
        config_id: row.get("config_id"),
        weight_id: row.get("weight_id"),
        voice_presets: parse_json_or(row.get::<String, _>("voice_presets").as_str()),
        // Stored as the JSON it renders to ("" = unset), because audio.cpp
        // accepts either a preset name or an inline preset object there.
        default_voice_preset: serde_json::from_str(
            row.get::<String, _>("default_voice_preset").as_str(),
        )
        .ok(),
        enabled: row.get::<i64, _>("enabled") != 0,
        image: row.get("image"),
        extra_run_args: parse_json_opt(row.get::<Option<String>, _>("extra_run_args")),
        warm_start: row.get::<i64, _>("warm_start") != 0,
        hold_fallback_mode: hold_fallback_mode_from_row(row),
        hold_fallback: row.get("hold_fallback"),
        // Learned, not configured (0053): written only by
        // `set_audio_model_residency` below, which is why `NewAudioModel` has
        // no field for it and neither insert nor update names the columns. A
        // row missing any of the three (or holding a figure no `u64` can
        // carry) reads as unlearned.
        residency: match (
            row.get::<Option<i64>, _>("resident_bytes")
                .and_then(|b| u64::try_from(b).ok()),
            row.get::<Option<String>, _>("resident_learned_at"),
            row.get::<Option<String>, _>("resident_key"),
        ) {
            (Some(bytes), Some(learned_at), Some(key)) => Some(LearnedResidency {
                bytes,
                learned_at,
                key,
            }),
            _ => None,
        },
    }
}

#[derive(Debug)]
pub struct NewAudioModel {
    pub model_id: String,
    pub family: String,
    pub path: String,
    pub task: String,
    pub mode: String,
    /// `None` inherits the class's `lazy_load`.
    pub lazy: Option<bool>,
    /// `None` inherits the class's `busy_timeout_ms`.
    pub busy_timeout_ms: Option<i64>,
    /// `Some("cpu")` runs the row on the CPU; `None` inherits the class's
    /// backend (0056).
    pub backend: Option<String>,
    /// `None` inherits (0056).
    pub threads: Option<i64>,
    pub load_options: serde_json::Map<String, serde_json::Value>,
    pub session_options: serde_json::Map<String, serde_json::Value>,
    /// Request-option defaults for every call to this model.
    pub default_request_options: serde_json::Map<String, serde_json::Value>,
    /// A `<family>.json` spec (or a directory of them) under the audio models
    /// dir, replacing the image's own catalog lookup for this row.
    pub model_spec_override: Option<String>,
    /// Named config/weights asset ids, for a model directory holding several.
    pub config_id: Option<String>,
    pub weight_id: Option<String>,
    pub voice_presets: serde_json::Map<String, serde_json::Value>,
    pub default_voice_preset: Option<serde_json::Value>,
    pub enabled: bool,
    /// `None` inherits the audio class settings' image/extra_run_args (§3.1).
    pub image: Option<String>,
    pub extra_run_args: Option<Vec<String>>,
    pub warm_start: bool,
    /// GPU-hold fallback mode (gpu-hold design §2/§3.2).
    pub hold_fallback_mode: HoldFallbackMode,
    /// Meaningful only when `hold_fallback_mode` is `Alias`.
    pub hold_fallback: Option<String>,
}

pub async fn list_audio_models(pool: &SqlitePool) -> DbResult<Vec<AudioModel>> {
    let rows = sqlx::query("SELECT * FROM audio_models ORDER BY model_id")
        .fetch_all(pool)
        .await?;
    Ok(rows.iter().map(audio_model_from_row).collect())
}

pub async fn get_audio_model(pool: &SqlitePool, id: i64) -> DbResult<Option<AudioModel>> {
    let row = sqlx::query("SELECT * FROM audio_models WHERE id = ?1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(audio_model_from_row))
}

/// The five JSON columns of one entry, in bind order.
fn audio_model_json(m: &NewAudioModel) -> DbResult<(String, String, String, String, String)> {
    Ok((
        to_json(&m.load_options)?,
        to_json(&m.session_options)?,
        to_json(&m.default_request_options)?,
        to_json(&m.voice_presets)?,
        // "" rather than "null" so the column reads as unset, not as a JSON null.
        match &m.default_voice_preset {
            Some(v) => to_json(v)?,
            None => String::new(),
        },
    ))
}

pub async fn insert_audio_model(pool: &SqlitePool, m: &NewAudioModel) -> DbResult<i64> {
    let (load, session, request_defaults, presets, default_preset) = audio_model_json(m)?;
    let extra_run_args = run_args_json(&m.extra_run_args)?;
    let res = sqlx::query(
        "INSERT INTO audio_models (model_id, family, path, task, mode, load_options, session_options,
                                   voice_presets, default_voice_preset, enabled, image, extra_run_args,
                                   warm_start, hold_fallback_mode, hold_fallback,
                                   lazy, busy_timeout_ms, default_request_options,
                                   model_spec_override, config_id, weight_id,
                                   backend, threads)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15,
                 ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23)",
    )
    .bind(&m.model_id)
    .bind(&m.family)
    .bind(&m.path)
    .bind(&m.task)
    .bind(&m.mode)
    .bind(load)
    .bind(session)
    .bind(presets)
    .bind(default_preset)
    .bind(m.enabled as i64)
    .bind(&m.image)
    .bind(extra_run_args)
    .bind(m.warm_start as i64)
    .bind(m.hold_fallback_mode.as_str())
    .bind(&m.hold_fallback)
    .bind(m.lazy.map(|v| v as i64))
    .bind(m.busy_timeout_ms)
    .bind(request_defaults)
    .bind(&m.model_spec_override)
    .bind(&m.config_id)
    .bind(&m.weight_id)
    .bind(&m.backend)
    .bind(m.threads)
    .execute(pool)
    .await?;
    Ok(res.last_insert_rowid())
}

pub async fn update_audio_model(pool: &SqlitePool, id: i64, m: &NewAudioModel) -> DbResult<()> {
    let (load, session, request_defaults, presets, default_preset) = audio_model_json(m)?;
    let extra_run_args = run_args_json(&m.extra_run_args)?;
    sqlx::query(
        "UPDATE audio_models SET model_id=?2, family=?3, path=?4, task=?5, mode=?6,
         load_options=?7, session_options=?8, voice_presets=?9, default_voice_preset=?10,
         enabled=?11, image=?12, extra_run_args=?13, warm_start=?14,
         hold_fallback_mode=?15, hold_fallback=?16,
         lazy=?17, busy_timeout_ms=?18, default_request_options=?19,
         model_spec_override=?20, config_id=?21, weight_id=?22,
         backend=?23, threads=?24,
         updated_at=datetime('now') WHERE id=?1",
    )
    .bind(id)
    .bind(&m.model_id)
    .bind(&m.family)
    .bind(&m.path)
    .bind(&m.task)
    .bind(&m.mode)
    .bind(load)
    .bind(session)
    .bind(presets)
    .bind(default_preset)
    .bind(m.enabled as i64)
    .bind(&m.image)
    .bind(extra_run_args)
    .bind(m.warm_start as i64)
    .bind(m.hold_fallback_mode.as_str())
    .bind(&m.hold_fallback)
    .bind(m.lazy.map(|v| v as i64))
    .bind(m.busy_timeout_ms)
    .bind(request_defaults)
    .bind(&m.model_spec_override)
    .bind(&m.config_id)
    .bind(&m.weight_id)
    .bind(&m.backend)
    .bind(m.threads)
    .execute(pool)
    .await?;
    Ok(())
}

/// Record what one audio model's container was measured to hold
/// (realtime design §9.4, migration 0053): `Some((bytes, key))` stores the
/// figure, the configuration key it was read under and the time; `None`
/// clears all three back to unlearned — the owner's reset.
///
/// Its own statement rather than a field on [`NewAudioModel`], for the reason
/// `set_image_model_peak` gives: this is a value lmgw writes by itself, and
/// the owner's insert/update never name these columns. `updated_at` is left
/// alone too — learning a figure is not a configuration change. Whether the
/// new figure should replace the stored one (larger, or another key) is the
/// caller's rule ([`crate::vram::residency`]); this only writes.
pub async fn set_audio_model_residency(
    pool: &SqlitePool,
    id: i64,
    residency: Option<(u64, &str)>,
) -> DbResult<()> {
    let (bytes, key) = match residency {
        Some((b, k)) => (
            // SQLite INTEGER is signed; a GPU never comes near the edge, and
            // the conversion says so rather than storing NULL ("unlearned").
            Some(i64::try_from(b).map_err(|_| {
                GatewayError::Internal(format!(
                    "a learned residency of {b} bytes does not fit an INTEGER"
                ))
            })?),
            Some(k),
        ),
        None => (None, None),
    };
    sqlx::query(
        "UPDATE audio_models
            SET resident_bytes      = ?2,
                resident_key        = ?3,
                resident_learned_at = CASE WHEN ?2 IS NULL THEN NULL ELSE datetime('now') END
          WHERE id = ?1",
    )
    .bind(id)
    .bind(bytes)
    .bind(key)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn delete_audio_model(pool: &SqlitePool, id: i64) -> DbResult<()> {
    sqlx::query("DELETE FROM audio_models WHERE id = ?1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}
