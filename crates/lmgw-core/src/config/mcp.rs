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
    /// A paired device's hosted tools (client-apps design §5.2): the device
    /// dials lmgw (`GET /mcp/host`) and lmgw is the MCP client on its link.
    /// The row's URL and command fields are unused.
    Device,
}

impl McpTransport {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Stdio => "stdio",
            Self::Http => "http",
            Self::Sse => "sse",
            Self::Device => "device",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "stdio" => Some(Self::Stdio),
            "http" => Some(Self::Http),
            "sse" => Some(Self::Sse),
            "device" => Some(Self::Device),
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
    /// Set when this row is a **paired device's** hosted tools (client-apps
    /// design §5.2): the `api_keys.id` of the device whose hosting grant
    /// created it. lmgw keeps it in step with the grant (prefix = the
    /// device's `hosts_label`, name = the device key's name) and deletes it
    /// with the grant or the key. `None` for every other row.
    pub device_key_id: Option<i64>,
}

impl McpServer {
    /// Whether this row is a paired device's hosted tools (§5.2).
    pub fn is_device(&self) -> bool {
        self.transport == McpTransport::Device || self.device_key_id.is_some()
    }

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
        match self.podman_argv("run", &[], &self.extra_run_args) {
            Some(argv) => ("podman".to_string(), argv),
            None => (self.command.clone().unwrap_or_default(), self.args.clone()),
        }
    }

    /// The `podman create` argv of a Podman-isolated server: what
    /// [`stdio_argv`](Self::stdio_argv)'s `podman run` does before the
    /// container starts, the image pull when the run flags ask for one
    /// included, with `labels` as `--label k=v`. The connect creates the
    /// container with it, then starts it with
    /// [`container_start_argv`](Self::container_start_argv), so that only
    /// the start and the MCP handshake count against `timeout_ms` (MCP
    /// gateway design §9). The run flags only `podman start` takes
    /// (`--sig-proxy`, `--detach-keys`) go there instead. `None` for a bare
    /// subprocess.
    pub fn container_create_argv(&self, labels: &[(String, String)]) -> Option<Vec<String>> {
        let (create, _) = split_start_flags(&self.extra_run_args);
        self.podman_argv("create", labels, &create)
    }

    /// The `podman start` argv of the container
    /// [`container_create_argv`](Self::container_create_argv) made, `id`:
    /// attached to its stdio, with the run flags `podman create` refuses and
    /// `podman start` takes.
    pub fn container_start_argv(&self, id: &str) -> Vec<String> {
        let (_, start) = split_start_flags(&self.extra_run_args);
        let mut argv: Vec<String> = ["start", "--attach", "--interactive"]
            .map(String::from)
            .to_vec();
        argv.extend(start);
        argv.push(id.to_string());
        argv
    }

    /// `podman <verb>` of a Podman-isolated server (`run` or `create`), its
    /// labels, flags, image and command; `None` for a bare subprocess.
    fn podman_argv(
        &self,
        verb: &str,
        labels: &[(String, String)],
        extra_run_args: &[String],
    ) -> Option<Vec<String>> {
        let image = self.container_image.as_deref()?.trim();
        if image.is_empty() {
            return None;
        }
        let mut argv = vec![
            verb.to_string(),
            "--rm".to_string(),
            "-i".to_string(),
            "--quiet".to_string(),
        ];
        for (k, v) in labels {
            argv.push("--label".to_string());
            argv.push(format!("{k}={v}"));
        }
        argv.extend(extra_run_args.iter().cloned());
        for (k, v) in &self.env {
            argv.push("-e".to_string());
            argv.push(format!("{k}={v}"));
        }
        argv.push(image.to_string());
        if let Some(cmd) = self.command.as_deref().filter(|c| !c.trim().is_empty()) {
            argv.push(cmd.to_string());
        }
        argv.extend(self.args.iter().cloned());
        Some(argv)
    }
}

/// One token of an isolated server's `extra_run_args`, read as `podman run`
/// reads it, against what `podman create` and `podman start` take (podman
/// 5.8.7: `podman create --help` against `podman run --help`).
#[derive(Debug, PartialEq, Eq)]
enum RunFlag {
    /// Taken by `podman create` as by `podman run`, or not a flag at all.
    Create,
    /// Refused by `podman create`, taken by `podman start`; `takes_next`
    /// when its value is the next token.
    Start { takes_next: bool },
    /// Taken by neither: the flag, and why it cannot apply.
    Refused { flag: String, why: &'static str },
}

const DETACHED: &str = "the server speaks MCP over the container's stdio, so lmgw always \
                        attaches to it";

fn run_flag(token: &str) -> RunFlag {
    if let Some(long) = token.strip_prefix("--") {
        let (name, inline_value) = match long.split_once('=') {
            Some((name, _)) => (name, true),
            None => (long, false),
        };
        let why = match name {
            "sig-proxy" => return RunFlag::Start { takes_next: false },
            "detach-keys" => {
                return RunFlag::Start {
                    takes_next: !inline_value,
                }
            }
            "detach" => DETACHED,
            "rmi" => {
                "the container is created --rm and goes when it ends; its image stays for the \
                 next connect"
            }
            "preserve-fd" | "preserve-fds" => {
                "lmgw hands the container its stdio and no other file descriptor"
            }
            "passwd" => {
                "a created container always gets the /etc/passwd entries --passwd=false would \
                 leave out"
            }
            _ => return RunFlag::Create,
        };
        return RunFlag::Refused {
            flag: format!("--{name}"),
            why,
        };
    }
    // A cluster of shorthands (`-it`): each letter is a flag, until one that
    // takes a value, which takes the rest of the token (`-v/a:/b`). `podman
    // run`'s boolean shorthands are d, i, P, q and t.
    if let Some(short) = token.strip_prefix('-') {
        for c in short.chars() {
            match c {
                'd' => {
                    return RunFlag::Refused {
                        flag: "-d".into(),
                        why: DETACHED,
                    }
                }
                'i' | 'P' | 'q' | 't' => {}
                _ => break,
            }
        }
    }
    RunFlag::Create
}

/// `extra_run_args` split into the flags `podman create` takes and the ones
/// that go to `podman start` instead (`--sig-proxy`, `--detach-keys`). A
/// refused flag stays with `create`, which says so in podman's own words;
/// a row is not saved with one ([`run_only_refusal`]).
fn split_start_flags(extra_run_args: &[String]) -> (Vec<String>, Vec<String>) {
    let (mut create, mut start) = (Vec::new(), Vec::new());
    let mut tokens = extra_run_args.iter();
    while let Some(token) = tokens.next() {
        match run_flag(token) {
            RunFlag::Start { takes_next } => {
                start.push(token.clone());
                if takes_next {
                    start.extend(tokens.next().cloned());
                }
            }
            RunFlag::Create | RunFlag::Refused { .. } => create.push(token.clone()),
        }
    }
    (create, start)
}

/// Why `extra_run_args` cannot be a Podman-isolated server's, when they
/// cannot: they hold a flag only `podman run` takes, which neither the
/// `podman create` nor the `podman start` of its connect accepts (MCP
/// gateway design §9). The flags `podman start` takes are moved there and
/// are fine. Checked when a row is saved, so that the owner hears it then,
/// not as podman's "unknown flag" at every connect.
pub fn run_only_refusal(extra_run_args: &[String]) -> Option<String> {
    let mut refused: Vec<String> = Vec::new();
    for token in extra_run_args {
        if let RunFlag::Refused { flag, why } = run_flag(token) {
            let said = format!("`{flag}`: {why}");
            if !refused.contains(&said) {
                refused.push(said);
            }
        }
    }
    let (flags, them) = match refused.len() {
        0 => return None,
        1 => ("a flag", "it"),
        _ => ("flags", "them"),
    };
    Some(format!(
        "extra_run_args holds {flags} only `podman run` takes ({}). lmgw creates an isolated \
         server's container with `podman create` and starts it with `podman start --attach \
         --interactive` (MCP gateway design §9), and neither accepts {them}: remove {them} from \
         extra_run_args.",
        refused.join("; ")
    ))
}

/// The device MCP host link's limits (client-apps design §5.1), Settings →
/// MCP: set explicitly on the link's WebSocket, defaulting to realtime's
/// (realtime design §10.4), each named by the close an overrun causes; and
/// the MCP Tasks poll interval (MCP Tasks design §5.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct McpSettings {
    /// The largest message a device may send over its host link, in MiB.
    /// **0 = no bound of its own**: a frame is then bounded by
    /// `host_max_frame_mb`. Not 0 together with it (refused by a save, and
    /// at load the defaults are used instead). A full-desktop screenshot is
    /// the expected large message.
    pub host_max_message_mb: u32,
    /// The largest single frame, in MiB. **0 = bounded by
    /// `host_max_message_mb`**: a frame is never unbounded, because
    /// tungstenite reserves a frame's declared length before reading it.
    pub host_max_frame_mb: u32,
    /// Seconds between the link's pings. A ping with no pong for a whole
    /// interval closes the link with a reason naming this setting. **0 = no
    /// pings**: a device that vanished without a FIN keeps its link (and its
    /// tools listed) until TCP gives up.
    pub host_ping_interval_s: u32,
    /// Seconds between two `tasks/get` of a task whose server suggests no
    /// `pollInterval` of its own (MCP Tasks design T8): a task's own
    /// `pollInterval` wins, and a status notification acts at once. At
    /// least 1 (a save refuses 0; at load a stored 0 reads as the default).
    /// Applies from each task's next poll.
    pub task_poll_interval_s: u32,
}

/// [`McpSettings::task_poll_interval_s`]'s default.
pub const TASK_POLL_INTERVAL_DEFAULT_S: u32 = 5;

impl Default for McpSettings {
    fn default() -> Self {
        // Realtime's (§10.4): tungstenite's own defaults, made explicit, and
        // twice what a browser allows a pong.
        Self {
            host_max_message_mb: 64,
            host_max_frame_mb: 16,
            host_ping_interval_s: 20,
            task_poll_interval_s: TASK_POLL_INTERVAL_DEFAULT_S,
        }
    }
}
