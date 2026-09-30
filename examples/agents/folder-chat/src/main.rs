//! `folder-chat` — the container's entrypoint in service mode.
//!
//! 1. Read `LMGW_PORT` and `LMGW_APP_ORIGIN` (lmgw sets both in service mode,
//!    docs/agents.md §5) and open the agent ([`FolderChat::from_env`]: the
//!    config from `/lmgw/input.json`, the token from `/lmgw/secrets.json`, the
//!    index inside the bound folder). No gateway call yet.
//! 2. Bind `0.0.0.0:$LMGW_PORT` and serve. `/healthz` answers from here on,
//!    before any model is touched, so lmgw's start probe is not held up by a
//!    local embedding model loading its weights.
//! 3. Start **one** sync right away, with the index on disk loaded first
//!    ([`FolderChat::load_existing`]) so questions are answered from it while
//!    the sync runs. The owner re-points the folder on the Run tab, lmgw
//!    restarts the service, and by the time the page opens it is syncing.
//! 4. On SIGTERM or SIGINT: stop accepting connections and end every SSE
//!    stream; stop the sync **first**, where it stands ([`Server::stop_sync`]
//!    says why the index stays consistent), so it does not go on embedding
//!    while requests drain; then give in-flight requests up to
//!    [`SHUTDOWN_DRAIN`]; close the index; exit — well inside the manifest's
//!    `stop_grace_seconds` (default 10), after which podman would SIGKILL.
//!
//! The process is meant to be the container's PID 1 (an exec-form
//! `ENTRYPOINT`): as PID 1 the kernel delivers no default-action signals, so
//! SIGTERM reaches only the handler installed here.

use std::future::IntoFuture;
use std::net::SocketAddr;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use folder_chat::server::{Server, SyncStop};
use folder_chat::FolderChat;
use tokio_util::sync::CancellationToken;

/// How long in-flight requests may take to finish after a stop signal before
/// the server stops waiting for them. SSE streams end at once; this is for a
/// request mid-way through a gateway call (an MCP `search` embedding its
/// query). The sync is stopped before this starts, so it cannot spend it
/// embedding; five seconds leaves the other half of lmgw's default
/// `stop_grace_seconds` (10) for closing the index.
pub const SHUTDOWN_DRAIN: Duration = Duration::from_secs(5);

/// How long closing the index may take (SQLite checkpoints its WAL on the
/// last connection's close). Past it the process exits anyway: the WAL is
/// replayed on the next open, so nothing is lost. Drain plus this stays under
/// the default `stop_grace_seconds` of 10.
pub const INDEX_CLOSE_WAIT: Duration = Duration::from_secs(2);

fn required_env(name: &str) -> Result<String, String> {
    std::env::var(name)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| {
            format!(
                "{name} is not set; lmgw sets it when it starts the service (run.service in \
                 agent.json). To run this outside lmgw, set LMGW_PORT, LMGW_APP_ORIGIN, \
                 LMGW_API_BASE and LMGW_INPUT yourself"
            )
        })
}

#[tokio::main]
async fn main() -> ExitCode {
    // stderr is the service log lmgw keeps (`agent_service_log`); stdout is
    // for a phase run's JSONL, which a service has none of.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_ansi(false)
        .init();
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("{e}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), String> {
    let port: u16 = required_env("LMGW_PORT")?
        .trim()
        .parse()
        .map_err(|e| format!("LMGW_PORT is not a port number: {e}"))?;
    let origin = required_env("LMGW_APP_ORIGIN")?;

    let app = FolderChat::from_env()
        .await
        .map_err(|e| format!("cannot start: {e}"))?;
    let app = Arc::new(app);
    let c = app.config();
    tracing::info!(
        embed_model = %c.embed_model,
        chat_model = %c.chat_model,
        rerank_model = c.rerank_model.as_deref().unwrap_or("(none)"),
        chunk_tokens = c.chunk_tokens,
        allow_remote = c.allow_remote,
        index = %app.index().db_path().display(),
        "folder-chat {} starting",
        env!("CARGO_PKG_VERSION")
    );

    let shutdown = CancellationToken::new();
    let server = Server::new(app.clone(), &origin, shutdown.clone()).map_err(|e| e.to_string())?;
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("cannot listen on {addr}: {e}"))?;
    tracing::info!(
        "listening on {addr}; requests are accepted only through lmgw, with \
         X-Forwarded-Host {}; the app answers {}",
        server.authority(),
        if app.config().allow_remote {
            "every machine that reaches lmgw (allow_remote is on)"
        } else {
            "only a browser on the machine lmgw runs on (allow_remote is off)"
        }
    );

    tokio::spawn(signals(shutdown.clone()));

    if let Some(run) = server.start_sync(true) {
        tracing::info!(run, "start-up sync started");
    }

    let serve = axum::serve(listener, server.router())
        .with_graceful_shutdown(shutdown.clone().cancelled_owned())
        .into_future();
    tokio::pin!(serve);
    // `biased`: once the signal is in, the shutdown below runs in its order
    // even if the server finished draining in the same instant.
    let served = tokio::select! {
        biased;
        () = shutdown.cancelled() => false,
        // axum::serve ends by itself only on an error.
        r = &mut serve => {
            r.map_err(|e| format!("the server stopped: {e}"))?;
            true
        }
    };

    // 1. The sync, first: left running, it would go on embedding through the
    //    drain below.
    if server.stop_sync(SyncStop::Shutdown).await {
        tracing::info!(
            "the sync was stopped mid-way; every file it had not finished is indexed again on \
             the next sync"
        );
    }
    // 2. In-flight requests.
    if !served {
        tracing::info!(
            "draining in-flight requests for up to {}s (SHUTDOWN_DRAIN)",
            SHUTDOWN_DRAIN.as_secs()
        );
        match tokio::time::timeout(SHUTDOWN_DRAIN, &mut serve).await {
            Ok(r) => r.map_err(|e| format!("the server stopped: {e}"))?,
            Err(_) => tracing::warn!(
                "requests still in flight after {}s (SHUTDOWN_DRAIN); closing them",
                SHUTDOWN_DRAIN.as_secs()
            ),
        }
    }
    // 3. The index.
    if tokio::time::timeout(INDEX_CLOSE_WAIT, app.index().pool().close())
        .await
        .is_err()
    {
        tracing::warn!(
            "the index did not close within {}s (INDEX_CLOSE_WAIT); exiting anyway, its WAL is \
             replayed on the next open",
            INDEX_CLOSE_WAIT.as_secs()
        );
    }
    tracing::info!("stopped");
    Ok(())
}

/// Cancel `shutdown` on the first SIGTERM (podman stop) or SIGINT (Ctrl-C).
async fn signals(shutdown: CancellationToken) {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("cannot listen for SIGTERM: {e}");
            return;
        }
    };
    let mut int = match signal(SignalKind::interrupt()) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("cannot listen for SIGINT: {e}");
            return;
        }
    };
    let which = tokio::select! {
        _ = term.recv() => "SIGTERM",
        _ = int.recv() => "SIGINT",
    };
    tracing::info!(
        "{which}: shutting down: the sync stops first, then in-flight requests get up to {}s \
         (SHUTDOWN_DRAIN)",
        SHUTDOWN_DRAIN.as_secs()
    );
    shutdown.cancel();
}
