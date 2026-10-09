//! `mcp_known_tools`: the tools each MCP server listed when it was last
//! connected (`mcp::known`, migration 0078).

use std::collections::HashMap;

use super::*;

/// Every server's last listing, by server id.
pub async fn known_tools(pool: &SqlitePool) -> DbResult<HashMap<i64, Vec<String>>> {
    let rows: Vec<(i64, String)> = sqlx::query_as(
        "SELECT server_id, tool_name FROM mcp_known_tools ORDER BY server_id, tool_name",
    )
    .fetch_all(pool)
    .await?;
    let mut out: HashMap<i64, Vec<String>> = HashMap::new();
    for (id, name) in rows {
        out.entry(id).or_default().push(name);
    }
    Ok(out)
}

/// Server `server_id` listed `names` (its whole list): they replace what it
/// listed before. A server deleted meanwhile has no row to write for: the
/// foreign key refuses the insert and nothing is stored.
pub async fn set_known_tools(pool: &SqlitePool, server_id: i64, names: &[String]) -> DbResult<()> {
    let mut tx = begin_write(pool).await?;
    sqlx::query("DELETE FROM mcp_known_tools WHERE server_id = ?1")
        .bind(server_id)
        .execute(&mut *tx)
        .await?;
    for name in names {
        sqlx::query("INSERT OR IGNORE INTO mcp_known_tools (server_id, tool_name) VALUES (?1, ?2)")
            .bind(server_id)
            .bind(name)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}
