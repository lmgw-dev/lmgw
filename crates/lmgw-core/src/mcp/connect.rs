//! Connecting one southbound server (§9): the claim that marks it
//! `Connecting`, the handshake and its bound, and the outcome that settles
//! the claim as `Ready` or `Error`; and the reconcile that drives them after
//! a reload. Each runs to its end on a task of its own, whoever waits for it.

use rmcp::model::Tool;
use rmcp::service::RunningService;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{RoleClient, ServiceExt};

use super::container::{self, Container};
use super::*;

/// A server's live session and the tools it listed, as the handshake ends.
type Handshake = (RunningService<RoleClient, GatewayClientHandler>, Vec<Tool>);

/// A connected server: its live session, the container it runs in (a
/// Podman-isolated stdio server's) and the tools it listed.
pub(super) struct Connected {
    pub(super) running: RunningService<RoleClient, GatewayClientHandler>,
    pub(super) container: Option<Container>,
    pub(super) tools: Vec<Tool>,
}

impl Connected {
    /// Close the session, and with it the child process or the container
    /// ([`container::close`]).
    pub(super) async fn close(self) {
        container::close(Some(self.running), self.container).await;
    }
}

impl McpManager {
    /// Connect one server and store the resulting conn (used by reconcile +
    /// test-connection). Marks `Connecting` first so the badge reflects a cold
    /// Podman pull, then `Ready`/`Error`. Honors the backoff window (§14): a
    /// recently-failed conn within its backoff is left `Error` untouched.
    pub(super) async fn start_one(&self, server: &McpServer) {
        // A device row is never dialled: it waits for its device's link
        // (client-apps design §5.2, `host`).
        if server.is_device() {
            return;
        }
        let hash = connection_config_hash(server);

        // Atomically *claim* the connect under one write lock so concurrent
        // callers (lazy `tools/list`, the 5s tick, reconcile) can't each spawn a
        // duplicate `podman run` for the same server:
        // - already `Connecting` ⇒ an attempt is in flight, bail;
        // - within the backoff window after an `Error` ⇒ bail (§14, no hot-loop);
        // - otherwise mark `Connecting` and proceed (preserving the failure
        //   counter for backoff).
        let claim = {
            let mut conns = self.conns.write().await;
            let entry = conns
                .entry(server.id)
                .or_insert_with(|| McpConn::stopped(hash));
            if matches!(entry.status, McpStatus::Connecting) {
                return;
            }
            if let Some(last) = entry.last_attempt {
                if matches!(entry.status, McpStatus::Error(_))
                    && last.elapsed() < backoff_delay(entry.consecutive_failures)
                {
                    return;
                }
            }
            self.claim(entry, hash)
        };

        // From the claim to its outcome on a task of its own, which the
        // caller waits for: a caller dropped mid-connect — a request whose
        // client hung up, a reconcile inside it — used to leave the server
        // `Connecting`, and every later attempt bailed on that claim until
        // the gateway restarted.
        match self.app() {
            Some(app) => {
                let server = server.clone();
                let connect =
                    tokio::spawn(async move { app.mcp.finish_start(server, claim).await });
                if let Err(e) = connect.await {
                    if e.is_panic() {
                        std::panic::resume_unwind(e.into_panic());
                    }
                }
            }
            None => self.finish_start(server.clone(), claim).await,
        }
    }

    /// Mark `entry` `Connecting` for a connect of the configuration that
    /// hashes to `hash`, under a claim number of its own: that number.
    fn claim(&self, entry: &mut McpConn, hash: u64) -> u64 {
        let claim = self.claims.fetch_add(1, Ordering::SeqCst).wrapping_add(1);
        entry.status = McpStatus::Connecting;
        entry.config_hash = hash;
        entry.claim = Some(claim);
        claim
    }

    /// [`start_one`](Self::start_one) past its claim: connect, and settle
    /// the claim as `Ready` or `Error` ([`settle`](Self::settle)). A
    /// configuration changed meanwhile is connected again, under a claim of
    /// its own; a connect whose claim was withdrawn closes its session.
    async fn finish_start(&self, mut server: McpServer, mut claim: u64) {
        loop {
            let hash = connection_config_hash(&server);
            let result = self.connect(&server).await;
            match self.settle(&server, claim, hash, result).await {
                Settled::Stored { ready } => {
                    // A newly-`Ready` server changed the aggregate composition
                    // (its tools now appear) — nudge every open `GET /mcp`
                    // subscriber to re-list (§8/§9). The signal is sent outside
                    // the conns lock.
                    if ready {
                        self.notify_tools_changed();
                    }
                    return;
                }
                Settled::Withdrawn(connected) => {
                    // Closed outside the lock, as `stop_one` closes one: the
                    // transport goes, and with it the child process, the
                    // container or the HTTP session.
                    if let Some(connected) = connected {
                        connected.close().await;
                    }
                    return;
                }
                Settled::Changed {
                    server: current,
                    claim: next,
                    connected,
                } => {
                    if let Some(connected) = connected {
                        connected.close().await;
                    }
                    server = *current;
                    claim = next;
                }
            }
        }
    }

    /// Settle claim `claim` on `server` with a connect's `result`, under the
    /// conns lock, against the snapshot published now (the begin-write
    /// review's B-2). A connect runs to its end whoever waited for it, so
    /// its outcome can arrive after the server was stopped, deleted,
    /// disabled or edited; one stored then was a live session, a child
    /// process or a container for a server that is not there, or not
    /// enabled, until some later reload stopped it.
    /// - The entry is gone, or its claim is not this one (a stop, a restart,
    ///   a delete's reconcile): the outcome is withdrawn, and the entry stays
    ///   as that left it.
    /// - The server is gone from the snapshot: withdrawn, and the entry goes
    ///   too. Disabled: withdrawn, and the entry is `Stopped`.
    /// - Its connection config changed: connected again with the current one,
    ///   under a new claim.
    /// - Otherwise `Ready` or `Error`, as ever.
    async fn settle(
        &self,
        server: &McpServer,
        claim: u64,
        hash: u64,
        result: Result<Connected, String>,
    ) -> Settled {
        let mut conns = self.conns.write().await;
        // Read under the lock: a delete or a disable publishes before its
        // reconcile takes the lock, so this sees the publish, or the
        // reconcile that follows it sees what is stored here.
        let current = self
            .app()
            .map(|app| app.snapshot().mcp_servers.get(&server.id).cloned());
        let Some(entry) = conns.get_mut(&server.id) else {
            return Settled::withdrawn(result);
        };
        if entry.claim != Some(claim) {
            return Settled::withdrawn(result);
        }
        match current {
            Some(None) => {
                conns.remove(&server.id);
                return Settled::withdrawn(result);
            }
            Some(Some(row)) if !row.enabled => {
                entry.claim = None;
                entry.status = McpStatus::Stopped;
                entry.tools.clear();
                return Settled::withdrawn(result);
            }
            Some(Some(row)) if connection_config_hash(&row) != hash => {
                let claim = self.claim(entry, connection_config_hash(&row));
                return Settled::Changed {
                    server: Box::new(row),
                    claim,
                    connected: result.ok(),
                };
            }
            _ => {}
        }
        entry.claim = None;
        entry.last_attempt = Some(Instant::now());
        entry.config_hash = hash;
        match result {
            Ok(connected) => {
                entry.running = Some(connected.running);
                entry.container = connected.container;
                entry.tools = connected.tools;
                entry.status = McpStatus::Ready;
                entry.last_used = Instant::now();
                entry.consecutive_failures = 0;
                entry.idle_reaped = false; // alive again; clear the reaped marker
                Settled::Stored { ready: true }
            }
            Err(e) => {
                entry.running = None;
                entry.container = None;
                entry.tools.clear();
                entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
                entry.status = McpStatus::Error(e);
                Settled::Stored { ready: false }
            }
        }
    }

    /// [`handshake`](Self::handshake), bounded by the server's own
    /// `timeout_ms`, as each of its tool calls is (§9). A server that takes
    /// the connection, or starts, and never answers used to hold its
    /// connect, and the reconcile waiting on it, for good.
    ///
    /// What comes before the handshake is not bounded: a Podman-isolated
    /// stdio server's container is created first ([`Container::create`]),
    /// with the pull of a cold image when its run flags ask for one, and
    /// cutting that would kill the pull mid-way, so the next attempt starts
    /// it over (the lazy list's reason for detaching). The container's start
    /// counts, from `podman start` on, and a container whose handshake fails
    /// is removed; so is one whose connect is dropped before it ends (the
    /// [`Container`]'s drop). A bare stdio server's handshake is not bounded:
    /// a command such as `npx` installs its package inside the process it
    /// starts, and that cannot be told apart from the server's own start.
    pub(super) async fn connect(&self, server: &McpServer) -> Result<Connected, String> {
        let (running, tools, container) = match server.transport {
            McpTransport::Stdio if server.is_isolated() => {
                let container = Container::create(server, &self.instance()).await?;
                match bounded(server, self.handshake(server, Some(container.id()))).await {
                    Ok((running, tools)) => (running, tools, Some(container)),
                    Err(e) => {
                        container.discard().await;
                        return Err(e);
                    }
                }
            }
            McpTransport::Stdio => {
                let (running, tools) = self.handshake(server, None).await?;
                (running, tools, None)
            }
            McpTransport::Http | McpTransport::Sse => {
                let (running, tools) = bounded(server, self.handshake(server, None)).await?;
                (running, tools, None)
            }
            McpTransport::Device => return Err(self.offline_words(server).await),
        };
        Ok(Connected {
            running,
            container,
            tools,
        })
    }

    /// The instance this gateway's containers are labelled with: its
    /// `container_prefix` ([`container::LABEL_MCP`]).
    fn instance(&self) -> String {
        self.app()
            .map(|app| app.snapshot().settings.container_prefix.clone())
            .unwrap_or_else(crate::config::default_container_prefix)
    }

    /// Build the right `rmcp` transport for a server and `serve` it into a
    /// running peer, then `list_all_tools` (paged internally — no cap, §7).
    /// Returns the live service + discovered tools, or a human error string.
    /// `container` is a Podman-isolated server's container, created already
    /// ([`connect`](Self::connect)): it is started here, attached to its
    /// stdio.
    ///
    /// **Spawn asymmetry (M1 review):** for the bare stdio case `stdio_argv`
    /// returns just `(command, args)` and env/cwd are the spawner's job; for
    /// the isolated case env is already in the argv as `-e` flags and cwd is
    /// intentionally bare-only — so we apply `env`/`cwd` to the `Command` only
    /// when `!is_isolated()`.
    async fn handshake(
        &self,
        server: &McpServer,
        container: Option<&str>,
    ) -> Result<Handshake, String> {
        let handler = self.handler_for(server);
        let running = match server.transport {
            McpTransport::Stdio => {
                let (program, args) = match container {
                    Some(id) => ("podman".to_string(), server.container_start_argv(id)),
                    None => server.stdio_argv(),
                };
                if program.trim().is_empty() {
                    return Err("stdio server has no command/container image".into());
                }
                let mut cmd = tokio::process::Command::new(&program);
                cmd.args(&args);
                if !server.is_isolated() {
                    // Bare subprocess: env + cwd are the spawner's responsibility.
                    if !server.env.is_empty() {
                        cmd.envs(server.env.iter().map(|(k, v)| (k.clone(), v.clone())));
                    }
                    if let Some(cwd) = server.cwd.as_deref().filter(|c| !c.trim().is_empty()) {
                        cmd.current_dir(cwd);
                    }
                }
                let transport = TokioChildProcess::new(cmd)
                    .map_err(|e| format!("spawning `{program}`: {e}"))?;
                handler
                    .serve(transport)
                    .await
                    .map_err(|e| format!("stdio handshake: {e}"))?
            }
            // rmcp 2.0 has no standalone legacy SSE client transport; the
            // streamable-HTTP client handles servers that reply with
            // `text/event-stream` too, so both `http` and `sse` route here.
            McpTransport::Http | McpTransport::Sse => {
                let url = server
                    .url
                    .as_deref()
                    .filter(|u| !u.trim().is_empty())
                    .ok_or_else(|| "http/sse server has no URL".to_string())?;
                let transport = build_http_transport(url, &self.dial_headers(server))?;
                handler
                    .serve(transport)
                    .await
                    .map_err(|e| format!("http handshake: {e}"))?
            }
            McpTransport::Device => return Err(self.offline_words(server).await),
        };
        let tools = running
            .peer()
            .list_all_tools()
            .await
            .map_err(|e| format!("list_tools: {e}"))?;
        Ok((running, tools))
    }

    /// Reconcile live connections against a snapshot (§9). Called after every
    /// `reload_snapshot()` and once on boot. Diff is the pure
    /// [`plan_reconcile`]; this method only performs the resulting IO.
    ///
    /// On a task of its own, which the caller waits for, as a Chat write
    /// runs to its end (`chat_live::to_its_end`): the reconcile after a
    /// reload runs inside the request that saved, and a client that hung up
    /// on it while a connect was slow used to cut it there, its stops and
    /// starts half done. A connect it waits on is bounded as
    /// [`connect`](Self::connect) bounds it.
    pub async fn reconcile(&self, snap: &Arc<Snapshot>) {
        let Some(app) = self.app() else {
            return self.reconcile_now(snap).await;
        };
        let snap = snap.clone();
        let reconcile = tokio::spawn(async move { app.mcp.reconcile_now(&snap).await });
        if let Err(e) = reconcile.await {
            if e.is_panic() {
                std::panic::resume_unwind(e.into_panic());
            }
        }
    }
}

/// `handshake`, failed with the sentence that says so when `server`'s
/// `timeout_ms` passes first.
async fn bounded(
    server: &McpServer,
    handshake: impl std::future::Future<Output = Result<Handshake, String>>,
) -> Result<Handshake, String> {
    tokio::time::timeout(Duration::from_millis(server.timeout_ms), handshake)
        .await
        .unwrap_or_else(|_| {
            Err(format!(
                "no answer to the MCP handshake within {} ms (the server's timeout_ms)",
                server.timeout_ms
            ))
        })
}

/// How [`McpManager::settle`] settled a connect's outcome.
enum Settled {
    /// Stored as the entry's outcome: `Ready` (`ready`) or `Error`.
    Stored { ready: bool },
    /// Not stored: the claim was withdrawn. The session, if the connect
    /// made one, is the caller's to close.
    Withdrawn(Option<Connected>),
    /// The server's connection config changed while it connected: connect
    /// `server`, its current row, under `claim`, after closing `connected`.
    Changed {
        server: Box<McpServer>,
        claim: u64,
        connected: Option<Connected>,
    },
}

impl Settled {
    fn withdrawn(result: Result<Connected, String>) -> Self {
        Self::Withdrawn(result.ok())
    }
}

/// Build the streamable-HTTP client transport for a remote server, injecting
/// the per-server headers (§5). Confined here so the `http`/`rmcp` header types
/// never leak past the `mcp` boundary.
fn build_http_transport(
    url: &str,
    headers: &[(String, String)],
) -> Result<StreamableHttpClientTransport<reqwest::Client>, String> {
    use reqwest::header::{HeaderName, HeaderValue};
    let mut map: HashMap<HeaderName, HeaderValue> = HashMap::new();
    for (name, value) in headers {
        let hn = HeaderName::from_bytes(name.trim().as_bytes())
            .map_err(|e| format!("invalid header name `{name}`: {e}"))?;
        let hv = HeaderValue::from_str(value.trim())
            .map_err(|e| format!("invalid value for header `{name}`: {e}"))?;
        map.insert(hn, hv);
    }
    let config =
        StreamableHttpClientTransportConfig::with_uri(url.trim().to_string()).custom_headers(map);
    Ok(StreamableHttpClientTransport::from_config(config))
}
