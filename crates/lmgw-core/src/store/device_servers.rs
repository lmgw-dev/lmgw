//! A paired device's hosted-tools row (client-apps design §5.2): the
//! `mcp_servers` row of transport `device` its hosting grant owns.
//!
//! Written only here, and only inside the transaction that writes the key or
//! its grant, so the row and the grant never disagree: set, the row exists
//! with the label as its prefix and the key's name as its name; cleared, or
//! the key deleted, it is gone (its tool overrides with it, by the FK).

use sqlx::SqliteConnection;

use super::*;

/// The row's default call timeout (§5.2): editable on the MCP page.
pub const DEVICE_TIMEOUT_MS: i64 = 60_000;

/// Why device key `key`'s hosted-tools row cannot be written: server row
/// `id`, not the device's, already has its name (an older row named
/// `device:<name>`, from before the prefix was the grant's alone).
pub fn blocked_grant(key: &str, id: i64) -> String {
    format!(
        "device '{}''s hosting grant has no MCP server row: the name it needs, '{key}', is \
         taken by MCP server {id}, which is not the device's — rename or delete that server \
         on the MCP page, then set the grant again (Usage → Keys)",
        crate::devices::short_name(key)
    )
}

/// The server row, not key `key_id`'s own, that has the name `name`.
pub(super) async fn name_taken(
    conn: &mut SqliteConnection,
    key_id: i64,
    name: &str,
) -> DbResult<Option<i64>> {
    Ok(sqlx::query_scalar(
        "SELECT id FROM mcp_servers
          WHERE name = ?1 AND (device_key_id IS NULL OR device_key_id != ?2)",
    )
    .bind(name)
    .bind(key_id)
    .fetch_optional(&mut *conn)
    .await?)
}

/// Bring key `key_id`'s hosted-tools row in step with its grant: `label`
/// `None` deletes it, `Some` creates it or renames it (`name` is the key's
/// own, `device:<name>`).
///
/// A row not the device's that has the name already (an older one, see
/// migration 0072) keeps the device from having its own. A write that sets
/// or changes the grant (`granting`) is refused naming that row; any other
/// write of the key — disabling it, say — goes through and leaves the grant
/// without a row, as it was (its host link says why, `mcp::host`).
pub(super) async fn sync_device_server(
    tx: &mut SqliteConnection,
    key_id: i64,
    name: &str,
    (label, granting): (Option<&str>, bool),
) -> DbResult<()> {
    let Some(label) = label else {
        // Its MCP tasks end with it (MCP Tasks design §1.6). Only a key's
        // delete passes no grant change (`granting` false) with no label;
        // a write that clears a grant the row stood for passes `true`.
        let why = if granting {
            "its device's hosting grant was cleared"
        } else {
            "its device's key was deleted"
        };
        let row: Option<(i64, String)> =
            sqlx::query_as("SELECT id, name FROM mcp_servers WHERE device_key_id = ?1")
                .bind(key_id)
                .fetch_optional(&mut *tx)
                .await?;
        if let Some((id, name)) = row {
            super::mcp_tasks::server_gone(tx, id, |r| {
                crate::mcp::tasks::removed_result(&r.task_id, &r.tool, &name, why)
            })
            .await?;
        }
        sqlx::query("DELETE FROM mcp_servers WHERE device_key_id = ?1")
            .bind(key_id)
            .execute(&mut *tx)
            .await?;
        return Ok(());
    };
    // Said by name, not as the UNIQUE constraint's bare error: nothing is
    // written (the caller's transaction rolls back).
    if let Some(id) = name_taken(tx, key_id, name).await? {
        if !granting {
            return Ok(());
        }
        return Err(GatewayError::BadRequest(blocked_grant(name, id)));
    }
    let moved = sqlx::query(
        "UPDATE mcp_servers SET name = ?2, tool_prefix = ?3, transport = 'device',
                updated_at = datetime('now')
          WHERE device_key_id = ?1",
    )
    .bind(key_id)
    .bind(name)
    .bind(label)
    .execute(&mut *tx)
    .await?;
    if moved.rows_affected() == 0 {
        sqlx::query(
            "INSERT INTO mcp_servers
                    (name, enabled, transport, tool_prefix, timeout_ms, autostart,
                     idle_seconds, allow_sampling, device_key_id)
             VALUES (?1, 1, 'device', ?2, ?3, 0, 0, 0, ?4)",
        )
        .bind(name)
        .bind(label)
        .bind(DEVICE_TIMEOUT_MS)
        .bind(key_id)
        .execute(&mut *tx)
        .await?;
    }
    Ok(())
}

/// A device row's editable fields (§5.2): its `enabled` flag and its call
/// timeout. Every other field is the grant's or unused, and stays.
pub async fn update_device_server(
    pool: &SqlitePool,
    id: i64,
    enabled: bool,
    timeout_ms: u64,
) -> DbResult<u64> {
    let mut tx = begin_write(pool).await?;
    let res = sqlx::query(
        "UPDATE mcp_servers SET enabled = ?2, timeout_ms = ?3, updated_at = datetime('now')
          WHERE id = ?1 AND device_key_id IS NOT NULL",
    )
    .bind(id)
    .bind(i64::from(enabled))
    .bind(i64::try_from(timeout_ms).unwrap_or(i64::MAX))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(res.rows_affected())
}
