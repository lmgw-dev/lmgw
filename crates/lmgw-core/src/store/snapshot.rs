//! Snapshot

use sqlx::{Row, SqlitePool};

use crate::config::{DisabledTool, McpToolOverride, Settings, Snapshot};
use crate::error::GatewayError;

use super::*;

pub async fn load_snapshot(pool: &SqlitePool) -> DbResult<Snapshot> {
    let upstream_rows = sqlx::query("SELECT * FROM upstreams")
        .fetch_all(pool)
        .await?;
    let alias_rows = sqlx::query("SELECT * FROM models").fetch_all(pool).await?;
    let candidate_alias_rows = sqlx::query("SELECT * FROM candidate_aliases")
        .fetch_all(pool)
        .await?;
    let local_rows = sqlx::query("SELECT * FROM local_models ORDER BY model_id")
        .fetch_all(pool)
        .await?;
    let aux_rows = sqlx::query("SELECT * FROM aux_models ORDER BY model_id")
        .fetch_all(pool)
        .await?;
    let audio_rows = sqlx::query("SELECT * FROM audio_models ORDER BY model_id")
        .fetch_all(pool)
        .await?;
    let image_rows = sqlx::query("SELECT * FROM image_models ORDER BY model_id")
        .fetch_all(pool)
        .await?;
    let key_rows = sqlx::query("SELECT * FROM api_keys")
        .fetch_all(pool)
        .await?;
    let price_rows = sqlx::query("SELECT * FROM prices").fetch_all(pool).await?;
    let hidden_rows = sqlx::query("SELECT upstream_id, model_id FROM hidden_passthrough_models")
        .fetch_all(pool)
        .await?;
    let mcp_rows = sqlx::query("SELECT * FROM mcp_servers")
        .fetch_all(pool)
        .await?;
    let mcp_override_rows =
        sqlx::query("SELECT server_id, tool_name, hidden, rename FROM mcp_tool_overrides")
            .fetch_all(pool)
            .await?;
    let disabled_tool_rows =
        sqlx::query("SELECT tool_name, source, disabled_at FROM tool_disabled")
            .fetch_all(pool)
            .await?;
    let settings = load_settings(pool).await?;

    let mut snap = Snapshot {
        settings,
        ..Default::default()
    };
    for r in &upstream_rows {
        let u = upstream_from_row(r);
        snap.upstreams.insert(u.id, u);
    }
    for r in &alias_rows {
        let a = alias_from_row(r);
        snap.aliases.insert(a.alias.clone(), a);
    }
    for r in &candidate_alias_rows {
        let c = candidate_alias_from_row(r);
        snap.candidate_aliases.insert(c.alias.to_lowercase(), c);
    }
    snap.local_models = local_rows.iter().map(local_model_from_row).collect();
    snap.aux_models = aux_rows.iter().map(aux_model_from_row).collect();
    snap.audio_models = audio_rows.iter().map(audio_model_from_row).collect();
    snap.image_models = image_rows.iter().map(image_model_from_row).collect();
    snap.api_keys = key_rows.iter().map(api_key_from_row).collect();
    snap.prices = price_rows.iter().map(price_from_row).collect();
    snap.hidden_passthrough = hidden_rows
        .iter()
        .map(|r| {
            (
                r.get::<i64, _>("upstream_id"),
                r.get::<String, _>("model_id"),
            )
        })
        .collect();
    for r in &mcp_rows {
        let s = mcp_server_from_row(r);
        snap.mcp_servers.insert(s.id, s);
    }
    snap.mcp_tool_overrides = mcp_override_rows
        .iter()
        .map(|r| {
            let key = (
                r.get::<i64, _>("server_id"),
                r.get::<String, _>("tool_name"),
            );
            let ov = McpToolOverride {
                hidden: r.get::<i64, _>("hidden") != 0,
                rename: r.get("rename"),
            };
            (key, ov)
        })
        .collect();
    snap.disabled_tools = disabled_tool_rows
        .iter()
        .map(|r| {
            (
                r.get::<String, _>("tool_name"),
                DisabledTool {
                    source: r.get("source"),
                    disabled_at: r.get("disabled_at"),
                },
            )
        })
        .collect();
    Ok(snap)
}

pub async fn load_settings(pool: &SqlitePool) -> DbResult<Settings> {
    let row = sqlx::query("SELECT value FROM settings WHERE key = 'settings'")
        .fetch_optional(pool)
        .await?;
    let Some(r) = row else {
        return Ok(Settings::default());
    };
    let raw: String = r.get("value");
    // `migrations/0018` rewrites the key, so seeing the old one means this blob
    // came from somewhere else (restored backup, hand edit). `Settings`'
    // serde alias reads it either way; say so rather than let a settings save
    // quietly drop the container config into the default.
    if raw.contains("\"embed_router\"") {
        tracing::info!(
            "settings carry the pre-rename `embed_router` key — reading it as `aux_router`; \
             the next settings save stores the new spelling"
        );
    }
    let mut settings: Settings = parse_json_or(raw.as_str());
    capture_legacy_container_names(&raw, &mut settings);
    if !crate::config::CHAT_PDF_MODES.contains(&settings.chat_pdf_mode.as_str()) {
        tracing::warn!(
            "stored setting chat_pdf_mode = {:?} is not one of {}; using the default until it is saved again",
            settings.chat_pdf_mode,
            crate::config::CHAT_PDF_MODES.join(", ")
        );
        settings.chat_pdf_mode = "text".to_string();
    }
    Ok(settings)
}

/// The settings-shape migration of the per-model-containers design (§6).
///
/// The removed router-mode fields (`container_name`, `listen_port`,
/// `models_max`, `auto_start`) need no rewrite of their own: `Settings` has no
/// `deny_unknown_fields`, so a pre-upgrade blob deserializes cleanly and the
/// next save drops the dead keys. One of them is not dead on arrival, though —
/// `container_name` is the *only* way to find the shared containers a
/// pre-upgrade install left running, because they carry none of the labels
/// reconciliation filters on (§3.3). So it is lifted here, at load, into
/// [`Settings::legacy_container_names`], which is exactly the ordering §6
/// demands: read the old values *before* the shape is rewritten.
///
/// Idempotent by construction — it re-reads the same names on every load until
/// a save drops the old keys, and dedupes into whatever the field already
/// holds. [`crate::runtime::lifecycle::boot`] is what empties it, once the
/// sweep has actually run.
fn capture_legacy_container_names(raw: &str, settings: &mut Settings) {
    // Cheap negative: every post-migration blob takes this branch, and this
    // function runs on every snapshot reload.
    if !raw.contains("\"container_name\"") {
        return;
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) else {
        return;
    };
    let mut found = Vec::new();
    // `embed_router` alongside `aux_router` for the same reason the note above
    // exists: a blob restored from before migration 0018 spells it that way.
    for section in ["router", "aux_router", "embed_router", "audio"] {
        let Some(name) = v
            .get(section)
            .and_then(|s| s.get("container_name"))
            .and_then(|n| n.as_str())
            .map(str::trim)
            .filter(|n| !n.is_empty())
        else {
            continue;
        };
        if !settings.legacy_container_names.iter().any(|k| k == name)
            && !found.iter().any(|k: &String| k == name)
        {
            found.push(name.to_string());
        }
    }
    if found.is_empty() {
        return;
    }
    tracing::info!(
        "settings still carry router-mode container names ({}) — kept for the one-time \
         container sweep; the next settings save drops the old keys",
        found.join(", ")
    );
    settings.legacy_container_names.extend(found);
}

pub async fn save_settings(pool: &SqlitePool, s: &Settings) -> DbResult<()> {
    let json = serde_json::to_string(s).map_err(|e| GatewayError::Internal(e.to_string()))?;
    sqlx::query(
        "INSERT INTO settings (key, value) VALUES ('settings', ?1)
         ON CONFLICT(key) DO UPDATE SET value = ?1",
    )
    .bind(json)
    .execute(pool)
    .await?;
    Ok(())
}

/// Generic key/value access over the `settings` table, for per-feature blobs
/// (e.g. a workflow's saved config) that don't belong in the hot-path
/// `Settings` snapshot. Keys are namespaced by the caller, e.g. `workflow:mail`.
pub async fn get_kv(pool: &SqlitePool, key: &str) -> DbResult<Option<String>> {
    let row = sqlx::query("SELECT value FROM settings WHERE key = ?1")
        .bind(key)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|r| r.get::<String, _>("value")))
}

pub async fn set_kv(pool: &SqlitePool, key: &str, value: &str) -> DbResult<()> {
    sqlx::query(
        "INSERT INTO settings (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = ?2",
    )
    .bind(key)
    .bind(value)
    .execute(pool)
    .await?;
    Ok(())
}

/// Drop a per-feature blob. `true` when a row was actually removed, which is
/// what makes a one-shot migration safe to run on every start: the key's
/// presence is the gate, and deleting it closes it (agent-catalog design §7.5).
pub async fn delete_kv(pool: &SqlitePool, key: &str) -> DbResult<bool> {
    let res = sqlx::query("DELETE FROM settings WHERE key = ?1")
        .bind(key)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}
