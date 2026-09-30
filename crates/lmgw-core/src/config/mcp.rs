//! MCP gateway — server definitions (the config plane; live rmcp connections
//! live in `AppState.mcp::McpManager`, the same split as a model row and the
//! container runtime entry that serves it).

use serde::{Deserialize, Serialize};

/// MCP server transport. Persisted to the `transport TEXT` column via
/// hand-written `as_str`/`parse` (lowercase) — matching the `Protocol` /
/// `UpstreamKind` convention (row mapper `.unwrap_or(default)`). The `serde`
/// derive below is *not* the persistence path; it exists only so a `Snapshot`
/// can be JSON-dumped for debugging, and `rename_all = "lowercase"` keeps that
/// dump symmetric with the on-disk `as_str` spelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpTransport {
    Stdio,
    Http,
    Sse,
}

impl McpTransport {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Stdio => "stdio",
            Self::Http => "http",
            Self::Sse => "sse",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "stdio" => Some(Self::Stdio),
            "http" => Some(Self::Http),
            "sse" => Some(Self::Sse),
            _ => None,
        }
    }

    /// stdio servers run a subprocess; http/sse connect to a URL.
    pub fn is_stdio(&self) -> bool {
        matches!(self, Self::Stdio)
    }
}

/// A per-server tool override: hide a discovered tool from the aggregate, or
/// rename its exposed name. Keyed by `(server_id, upstream tool name)` in the
/// [`Snapshot`](crate::config::Snapshot) — the tool-plane analogue of `hidden_passthrough`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolOverride {
    pub hidden: bool,
    pub rename: Option<String>,
}

/// A registered MCP server lmgw aggregates behind its northbound `/mcp`
/// endpoint — the tool-plane analogue of [`Upstream`](crate::config::Upstream). Definitions live in the
/// [`Snapshot`](crate::config::Snapshot); live `rmcp` connections live in `AppState.mcp` (Milestone 2+).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServer {
    pub id: i64,
    pub name: String,
    pub enabled: bool,
    pub transport: McpTransport,
    // stdio transport
    pub command: Option<String>,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: Option<String>,
    /// `Some(non-empty)` ⇒ Podman-isolated (the default path); the `podman run
    /// -i` argv is synthesized from the image + env + extra args.
    pub container_image: Option<String>,
    pub extra_run_args: Vec<String>,
    // http/sse transport
    pub url: Option<String>,
    pub headers: Vec<(String, String)>,
    // common
    pub tool_prefix: String,
    pub timeout_ms: u64,
    pub autostart: bool,
    pub idle_seconds: i64,
    pub allow_sampling: bool,
    pub sampling_alias: Option<String>,
    /// Set when this row is an **agent's own** registration (`agent:<id>`,
    /// container-runtime §3.3): lmgw creates it from `run.provides.mcp`, keeps
    /// it in step with the manifest and deletes it with the agent. `None` for
    /// every row an owner created, which is every row that existed before.
    pub agent_id: Option<String>,
}

impl McpServer {
    /// Whether this (stdio) server runs Podman-isolated vs. a bare subprocess.
    pub fn is_isolated(&self) -> bool {
        self.container_image
            .as_deref()
            .is_some_and(|i| !i.trim().is_empty())
    }

    /// Exposed (client-facing) name for an upstream tool: `<prefix>__<tool>`
    /// when a prefix is set, else the bare tool name (§7).
    pub fn exposed_name(&self, tool: &str) -> String {
        match self.tool_prefix.trim() {
            "" => tool.to_string(),
            p => format!("{p}__{tool}"),
        }
    }

    /// Final `(program, args)` to spawn a stdio server. Podman-isolated when
    /// [`container_image`](Self::container_image) is set (the default): env is
    /// passed as `-e K=V` flags and `extra_run_args` (GPU/CDI/`:Z`) precede the
    /// image. A bare subprocess returns `command`/`args` verbatim — its env is
    /// applied by the spawner, not the argv.
    pub fn stdio_argv(&self) -> (String, Vec<String>) {
        match &self.container_image {
            Some(image) if !image.trim().is_empty() => {
                let mut argv = vec![
                    "run".to_string(),
                    "--rm".to_string(),
                    "-i".to_string(),
                    "--quiet".to_string(),
                ];
                argv.extend(self.extra_run_args.iter().cloned());
                for (k, v) in &self.env {
                    argv.push("-e".to_string());
                    argv.push(format!("{k}={v}"));
                }
                argv.push(image.trim().to_string());
                if let Some(cmd) = self.command.as_deref().filter(|c| !c.trim().is_empty()) {
                    argv.push(cmd.to_string());
                }
                argv.extend(self.args.iter().cloned());
                ("podman".to_string(), argv)
            }
            _ => (self.command.clone().unwrap_or_default(), self.args.clone()),
        }
    }
}
