//! Pre-migration notice (migration 0072): name the hosting grants 0072 gives
//! no row.
//!
//! 0072 backfills a device-hosted `mcp_servers` row for every grant made
//! before it, named after the device key (`device:<name>`), and skips a grant
//! whose name an older server row already has — SQL cannot say so, and the
//! names are unique. Such a device's host link is then refused, naming the
//! row in the way (`mcp::host`), and setting its grant again is refused the
//! same way (`store::sync_device_server`) until the owner renames or deletes
//! that row; lmgw changes neither row itself. This says so at the start that
//! runs 0072, once per grant.

use sqlx::{Row, SqlitePool};

use super::{has_column, migration_applied, table_exists};

/// Version of `0072_mcp_device_rows.sql`.
const DEVICE_ROWS_MIGRATION: i64 = 72;

/// One `warn` line per grant 0072 will give no row. Nothing on a new
/// database, one that ran 0072 already, or one from before the grants.
pub(in crate::store) async fn device_rows_notice(pool: &SqlitePool) -> anyhow::Result<()> {
    if !table_exists(pool, "_sqlx_migrations").await?
        || migration_applied(pool, DEVICE_ROWS_MIGRATION).await?
        || !has_column(pool, "api_keys", "hosts_label").await?
    {
        return Ok(());
    }
    let rows = sqlx::query(
        "SELECT k.name AS key, s.id AS id FROM api_keys k JOIN mcp_servers s ON s.name = k.name
          WHERE k.kind = 'device' AND k.hosts_label IS NOT NULL ORDER BY k.id",
    )
    .fetch_all(pool)
    .await?;
    for row in rows {
        let key: String = row.try_get("key")?;
        let id: i64 = row.try_get("id")?;
        tracing::warn!("migration 0072: {}", super::super::blocked_grant(&key, id));
    }
    Ok(())
}
