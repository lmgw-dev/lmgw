//! MCP server CRUD (the config plane; live connections live in McpManager),
//! plus per-tool disable (`tool_disabled`)

use sqlx::SqlitePool;

use crate::config::{run_only_refusal, McpServer, McpTransport};

use super::*;

// ---------------------------------------------------------------------------
// MCP servers CRUD (the config plane; live connections live in McpManager)
// ---------------------------------------------------------------------------

pub struct NewMcpServer {
    pub name: String,
    pub enabled: bool,
    pub transport: McpTransport,
    pub command: Option<String>,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: Option<String>,
    pub container_image: Option<String>,
    pub extra_run_args: Vec<String>,
    pub url: Option<String>,
    pub headers: Vec<(String, String)>,
    pub tool_prefix: String,
    pub timeout_ms: u64,
    pub autostart: bool,
    pub idle_seconds: i64,
    pub allow_sampling: bool,
    pub sampling_alias: Option<String>,
    /// The agent this row belongs to (container-runtime §3.3). `None` for an
    /// owner-created server.
    pub agent_id: Option<String>,
}

pub async fn list_mcp_servers(pool: &SqlitePool) -> DbResult<Vec<McpServer>> {
    let rows = sqlx::query("SELECT * FROM mcp_servers ORDER BY name")
        .fetch_all(pool)
        .await?;
    Ok(rows.iter().map(mcp_server_from_row).collect())
}

pub async fn get_mcp_server(pool: &SqlitePool, id: i64) -> DbResult<Option<McpServer>> {
    let row = sqlx::query("SELECT * FROM mcp_servers WHERE id = ?1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(mcp_server_from_row))
}

/// Serialize the four JSON list columns (args, env, extra_run_args, headers)
/// shared by insert/update.
fn mcp_json_cols(s: &NewMcpServer) -> DbResult<(String, String, String, String)> {
    Ok((
        to_json(&s.args)?,
        to_json(&s.env)?,
        to_json(&s.extra_run_args)?,
        to_json(&s.headers)?,
    ))
}

/// A Podman-isolated row is not saved with a run flag its connect cannot
/// pass ([`run_only_refusal`]: `--rmi`, `-d`, ...), and nothing of it is
/// stored. Here, under every writer: the dashboard's op, the
/// `lmgw__mcp_server_set` tool and whatever else saves one.
fn refuse_run_only_flags(s: &NewMcpServer) -> DbResult<()> {
    let isolated = s.transport.is_stdio()
        && s.container_image
            .as_deref()
            .is_some_and(|i| !i.trim().is_empty());
    match isolated.then(|| run_only_refusal(&s.extra_run_args)) {
        Some(Some(why)) => Err(GatewayError::BadRequest(why)),
        _ => Ok(()),
    }
}

pub async fn insert_mcp_server(pool: &SqlitePool, s: &NewMcpServer) -> DbResult<i64> {
    refuse_run_only_flags(s)?;
    let (args, env, extra, headers) = mcp_json_cols(s)?;
    let res = sqlx::query(
        "INSERT INTO mcp_servers (name, enabled, transport, command, args, env, cwd,
            container_image, extra_run_args, url, headers, tool_prefix, timeout_ms,
            autostart, idle_seconds, allow_sampling, sampling_alias, agent_id)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)",
    )
    .bind(&s.name)
    .bind(s.enabled as i64)
    .bind(s.transport.as_str())
    .bind(&s.command)
    .bind(args)
    .bind(env)
    .bind(&s.cwd)
    .bind(&s.container_image)
    .bind(extra)
    .bind(&s.url)
    .bind(headers)
    .bind(&s.tool_prefix)
    .bind(s.timeout_ms as i64)
    .bind(s.autostart as i64)
    .bind(s.idle_seconds)
    .bind(s.allow_sampling as i64)
    .bind(&s.sampling_alias)
    .bind(&s.agent_id)
    .execute(pool)
    .await?;
    Ok(res.last_insert_rowid())
}

pub async fn update_mcp_server(pool: &SqlitePool, id: i64, s: &NewMcpServer) -> DbResult<()> {
    refuse_run_only_flags(s)?;
    let (args, env, extra, headers) = mcp_json_cols(s)?;
    sqlx::query(
        "UPDATE mcp_servers SET name=?2, enabled=?3, transport=?4, command=?5, args=?6, env=?7,
            cwd=?8, container_image=?9, extra_run_args=?10, url=?11, headers=?12, tool_prefix=?13,
            timeout_ms=?14, autostart=?15, idle_seconds=?16, allow_sampling=?17, sampling_alias=?18,
            agent_id=?19, updated_at=datetime('now') WHERE id=?1",
    )
    .bind(id)
    .bind(&s.name)
    .bind(s.enabled as i64)
    .bind(s.transport.as_str())
    .bind(&s.command)
    .bind(args)
    .bind(env)
    .bind(&s.cwd)
    .bind(&s.container_image)
    .bind(extra)
    .bind(&s.url)
    .bind(headers)
    .bind(&s.tool_prefix)
    .bind(s.timeout_ms as i64)
    .bind(s.autostart as i64)
    .bind(s.idle_seconds)
    .bind(s.allow_sampling as i64)
    .bind(&s.sampling_alias)
    .bind(&s.agent_id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn delete_mcp_server(pool: &SqlitePool, id: i64) -> DbResult<()> {
    // mcp_tool_overrides rows cascade via the FK.
    sqlx::query("DELETE FROM mcp_servers WHERE id = ?1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Per-tool disable (`tool_disabled`)
// ---------------------------------------------------------------------------

/// Switch one fully-qualified tool off, recording the source it belonged to.
/// Idempotent: re-disabling refreshes the recorded source rather than failing,
/// so a tool that moved between servers carries the current provenance.
pub async fn disable_tool(pool: &SqlitePool, name: &str, source: &str) -> DbResult<()> {
    sqlx::query(
        "INSERT INTO tool_disabled (tool_name, source) VALUES (?1, ?2)
         ON CONFLICT(tool_name) DO UPDATE SET source = excluded.source",
    )
    .bind(name)
    .bind(source)
    .execute(pool)
    .await?;
    Ok(())
}

/// Switch one tool back on. Returns how many rows went away, so the caller can
/// tell "re-enabled" from "was never disabled" — the same call clears a stale
/// entry whose tool no longer exists.
pub async fn enable_tool(pool: &SqlitePool, name: &str) -> DbResult<u64> {
    let res = sqlx::query("DELETE FROM tool_disabled WHERE tool_name = ?1")
        .bind(name)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}
