//! The start of a server (headless server design §4.4): [`bind`], the TLS
//! listener's [`bind_tls`], and [`serve`] / [`serve_with`], which run the
//! plain listener, the TLS listener beside it and the device listener until
//! the stop, within one bound.

use super::{
    build_router, conn, main_tls, serve_app, spawn_background_tasks, stop, Via, STOP_WITHIN,
};
use crate::state::SharedState;

/// Serve until `shutdown` resolves: [`bind`], then [`serve`].
pub async fn run(
    state: SharedState,
    addr: std::net::SocketAddr,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    let listener = bind(&state, addr).await?;
    serve(state, listener, shutdown, None).await
}

/// Refuse a dev instance on the production prefix, then bind `addr`.
///
/// Its own step so an entry point can act on the outcome before it serves:
/// the shell builds its window only once this process holds the port, and
/// never points one at whoever else does (chat-voice WP11 review m3).
pub async fn bind(
    state: &SharedState,
    addr: std::net::SocketAddr,
) -> anyhow::Result<tokio::net::TcpListener> {
    // Before anything lists, removes or starts a container by prefix.
    if let Some(why) =
        crate::config::dev_prefix_refusal(state.dev(), &state.snapshot().settings.container_prefix)
    {
        anyhow::bail!(why);
    }
    tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| anyhow::anyhow!("binding {addr}: {e}"))
}

/// The TLS listener a start binds beside the plain one (headless server
/// design §4.4): its socket, the pair it serves and what it started with.
pub struct BoundTls {
    pub listener: tokio::net::TcpListener,
    pub tls: std::sync::Arc<main_tls::MainTls>,
    pub config: crate::config::TlsListenerConfig,
}

/// Load the configured pair and bind the TLS listener's address, when the
/// settings configure one (`tls.cert_path`, `tls.key_path`, `tls.listen`);
/// `None` when they do not. A pair that does not load and an address that
/// does not bind are errors naming the file or the address: the server's
/// start ends on them (§1 step 6), the desktop's says them and serves
/// without TLS ([`serve`]).
pub async fn bind_tls(state: &SharedState) -> anyhow::Result<Option<BoundTls>> {
    bind_tls_for(state.snapshot().settings.tls.listener()).await
}

/// [`bind_tls`] for the listener config the caller read from the settings,
/// so it can decide on that same value afterwards.
async fn bind_tls_for(
    config: Option<crate::config::TlsListenerConfig>,
) -> anyhow::Result<Option<BoundTls>> {
    let Some(config) = config else {
        return Ok(None);
    };
    let (cert, key) = (config.cert.clone(), config.key.clone());
    let tls = tokio::task::spawn_blocking(move || main_tls::MainTls::open(&cert, &key))
        .await
        .map_err(|e| anyhow::anyhow!("loading the TLS pair: {e}"))?
        .map_err(|e| anyhow::anyhow!("the TLS listener's pair: {e}"))?;
    let listener = tokio::net::TcpListener::bind(config.addr)
        .await
        .map_err(|e| anyhow::anyhow!("binding the TLS listener on {}: {e}", config.addr))?;
    Ok(Some(BoundTls {
        listener,
        tls: std::sync::Arc::new(tls),
        config,
    }))
}

/// Serve on `listener` (from [`bind`]) until `shutdown` resolves, with the
/// TLS listener beside it when the settings configure one ([`bind_tls`]):
/// one that cannot start is said in the log and on Settings, and the plain
/// listener serves on (the desktop's start; the server's binds first and
/// ends on the error, [`serve_with`]).
///
/// `released`, when given, fires as the listeners close: right after
/// `shutdown`, before the open connections have drained. A restart that binds
/// the same ports again waits for it instead of racing the old listeners.
pub async fn serve(
    state: SharedState,
    listener: tokio::net::TcpListener,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
    released: Option<tokio::sync::oneshot::Sender<()>>,
) -> anyhow::Result<()> {
    // Before this start's TLS listener (or its failure) replaces the last.
    let origin_before = agent_origin_now(&state);
    // Read once: the value `bind_tls_for` binds from is the one the None
    // branch of `serve_from` decides on.
    let wanted = state.snapshot().settings.tls.listener();
    let no_tls_read = wanted.is_none();
    let tls = match bind_tls_for(wanted).await {
        Ok(tls) => tls,
        Err(e) => {
            tracing::warn!("{e:#}; serving without the TLS listener until the next start");
            state.main_tls.failed(&state, format!("{e:#}"));
            None
        }
    };
    serve_from(
        state,
        listener,
        tls,
        no_tls_read,
        shutdown,
        released,
        origin_before,
    )
    .await
}

/// [`serve`] with the TLS listener bound already (or none).
pub async fn serve_with(
    state: SharedState,
    listener: tokio::net::TcpListener,
    tls: Option<BoundTls>,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
    released: Option<tokio::sync::oneshot::Sender<()>>,
) -> anyhow::Result<()> {
    let origin_before = agent_origin_now(&state);
    // The caller's `bind_tls` bound nothing: it read no TLS (a failure ends
    // its start instead of reaching here).
    let no_tls_read = tls.is_none();
    serve_from(
        state,
        listener,
        tls,
        no_tls_read,
        shutdown,
        released,
        origin_before,
    )
    .await
}

/// The agents' origin scheme and port on the TLS listener the last start
/// served, kept past that start's stop: what the running app containers
/// were started with.
fn agent_origin_now(state: &SharedState) -> (crate::net::Scheme, Option<u16>) {
    crate::agents::service::origin_scheme_port(
        &state.snapshot().settings,
        state.main_tls.last_served().as_ref(),
    )
}

/// [`serve_with`], with the agents' origin as the app containers were
/// started with it (`origin_before`, taken before this start touched the
/// TLS listener's slot).
async fn serve_from(
    state: SharedState,
    listener: tokio::net::TcpListener,
    tls: Option<BoundTls>,
    no_tls_read: bool,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
    released: Option<tokio::sync::oneshot::Sender<()>>,
    origin_before: (crate::net::Scheme, Option<u16>),
) -> anyhow::Result<()> {
    // `bind` asked already; a caller with a listener of its own has not.
    if let Some(why) =
        crate::config::dev_prefix_refusal(state.dev(), &state.snapshot().settings.container_prefix)
    {
        anyhow::bail!(why);
    }
    let addr = listener.local_addr()?;
    // Both listeners' "closed", joined: `released` fires once both are.
    let (plain_released, tls_released) = match (&tls, released) {
        (Some(_), Some(released)) => {
            let (plain_tx, plain_rx) = tokio::sync::oneshot::channel::<()>();
            let (tls_tx, tls_rx) = tokio::sync::oneshot::channel::<()>();
            tokio::spawn(async move {
                let _ = plain_rx.await;
                let _ = tls_rx.await;
                let _ = released.send(());
            });
            (Some(plain_tx), Some(tls_tx))
        }
        (_, released) => (released, None),
    };
    // WebRTC calls' plain socket group (calls design §3.1): bound beside the
    // listener, closed with it before `released` fires. In container mode
    // behind TLS a plain listener on loopback is the container's own, which
    // no UDP socket outside it reaches: its group is not bound (D22). An
    // address the administrator pinned off loopback keeps its group.
    let realtime = state.snapshot().settings.realtime.webrtc;
    let plain_released =
        if tls.is_some() && state.deployment().is_container() && addr.ip().is_loopback() {
            state.calls.plain_not_bound(
                "in the server image behind TLS the plain listener is the container's loopback; \
             calls use the TLS listener's group",
            );
            plain_released
        } else {
            state
                .calls
                .serve_plain(&realtime, addr, plain_released)
                .await
        };
    let app = build_router(state.clone());
    // axum drops the listener when the shutdown signal ends its accept loop,
    // which fires `released`.
    let listener = conn::PlainListener::new(listener, plain_released);
    tracing::info!("lmgw listening on http://{addr}");
    let tls = match tls {
        Some(BoundTls {
            listener: socket,
            tls,
            config,
        }) => {
            let tls_addr = socket.local_addr()?;
            let tls_released = state
                .calls
                .serve_tls(&realtime, tls_addr, tls_released)
                .await;
            let info = tls.info();
            tracing::info!(
                "lmgw listening on https://{tls_addr} (TLS 1.2 and 1.3; certificate serial {}, \
                 names {}, valid until {})",
                info.serial,
                if info.names.is_empty() {
                    "none".to_string()
                } else {
                    info.names.join(", ")
                },
                info.not_after.format("%Y-%m-%d %H:%M:%S UTC")
            );
            let id = state
                .main_tls
                .serving(&state, tls.clone(), tls_addr, config.clone());
            Some((
                id,
                main_tls::MainTlsListener::new(socket, tls, config.handshake_timeout, tls_released),
            ))
        }
        None => {
            // Decided on the settings the bind read, not a fresh read: a
            // save that adds TLS during a restart must not skip `none()`,
            // or the origin comparison below misses the move. A failed bind
            // already said so (`failed`) and keeps its state.
            if no_tls_read {
                state.main_tls.none(&state);
            }
            None
        }
    };
    // What the TLS card warns of, in the start's log too: the names, the
    // leaf's end, the key's mode, a public URL of the other scheme.
    for w in state.main_tls.status(&state.snapshot().settings).warnings {
        tracing::warn!("TLS listener: {w}");
    }
    // Proxy mode (§4.5): which peers' forwarded headers this start reads,
    // and what the TLS card warns of it. The first peers are this start's
    // own from here on.
    state.proxy_seen.reset(super::forwarded::Since::Start);
    {
        let s = &state.snapshot().settings;
        if let Some(line) = super::forwarded::start_line(s) {
            tracing::info!("{line}");
        }
        for w in super::forwarded::status(s, &state.proxy_seen).warnings {
            tracing::warn!("proxy mode: {w}");
        }
    }
    origin_moved_at_start(&state, origin_before);
    if let Some(w) = crate::agents::service::remote_suffix_warning(
        &state.snapshot().settings,
        state.deployment().is_container(),
    ) {
        tracing::warn!("{w}");
    }
    // The headless owner's whole login procedure: click it (§3.4). One line
    // per start, beside the listening line, at the same level and target —
    // and a deliberate disclosure of a secret into the owner's own process
    // log, the only one lmgw makes. Built from the public origin (D9): the
    // address people open, else the TLS listener this start serves on, else
    // the plain address this start bound (a port of 0 is the one the OS
    // chose).
    let base = {
        let snap = state.snapshot();
        crate::net::public_base_url_on(&snap.settings, snap.serving_tls.as_ref(), &addr.to_string())
    };
    if state.snapshot().accounts_on {
        state.accounts.verifier.log_at_start();
    }
    if state.deployment().is_container() {
        // Ruling 15 (headless server design §5.3): the container's log never
        // prints a durable key.
        crate::accounts::start_lines(&state, &base).await;
    } else {
        match state
            .snapshot()
            .owner_key(crate::agents::token::OWNER_DASHBOARD)
        {
            Some(key) => tracing::info!("dashboard login: {base}/api/session/login?token={key}"),
            // Seeded by `AppState::init` and undeletable (§3.1), so this is the
            // unreachable arm — and it logs rather than panics because a gateway
            // that cannot print its login link still serves every other caller.
            None => tracing::info!(
                "dashboard login: unavailable — the '{}' key is missing or disabled; \
                 rotate it on Usage → Keys from a browser that still has a session",
                crate::agents::token::OWNER_DASHBOARD
            ),
        }
    }
    spawn_background_tasks(state.clone());
    // The device listener runs beside this one for as long as it serves
    // (device link design §4.1, §4.4): started from the `devices` settings
    // once this one is up — opening the CA may wait on the wallet's unlock
    // prompt, which must not hold the plain listener — and stopped by the
    // same stop, within the same bound, its own streams and sessions only.
    let shutdown = futures::FutureExt::shared(Box::pin(shutdown));
    let devices = {
        let (st, stopped) = (state.clone(), shutdown.clone());
        async move {
            let epoch = st.devices.listener.claim(&st).await;
            // Only the CA's opening is raced against the stop: the wallet's
            // prompt may wait for as long as nobody answers it, and a stop
            // must neither wait for it nor see the listener bind after it.
            // The start itself changes the listener's state and is never cut.
            let ca = tokio::select! {
                ca = st.devices.listener.open_ca(&st) => Some(ca),
                () = stopped.clone() => None,
            };
            if let Some(ca) = ca {
                st.devices.listener.start_claimed(&st, epoch, ca).await;
            }
            stopped.await;
            st.devices.listener.disarm(&st, epoch, STOP_WITHIN).await;
        }
    };
    // The TLS listener serves the plain one's router and stops with it,
    // within the same bound, by its own generation (§4.4).
    let tls_served = {
        let (st, stopped) = (state.clone(), shutdown.clone());
        async move {
            let Some((id, l)) = tls else {
                return Ok(());
            };
            let served = stop::serve_stopping(
                &st,
                Via::Tls,
                l,
                build_router(st.clone()),
                stopped,
                STOP_WITHIN,
            )
            .await;
            st.main_tls.stopped(&st, id);
            served
        }
    };
    let (served, tls_served, ()) = tokio::join!(
        serve_app(&state, listener, app, shutdown),
        tls_served,
        devices
    );
    if let Err(e) = tls_served {
        tracing::warn!("the TLS listener ended with an error: {e:#}");
    }
    served
}

/// The agents' origins follow the TLS listener this start serves, or that it
/// does not (headless server design §4.3): when the public scheme or port
/// moved against `before`, every running app container holds the old one in
/// `LMGW_APP_ORIGIN` (agent origins §4.10) and is stopped; the next request
/// to its origin starts it again. A save of the TLS settings stops none, as
/// it moves nothing until this start applies it.
fn origin_moved_at_start(state: &SharedState, before: (crate::net::Scheme, Option<u16>)) {
    if agent_origin_now(state) == before {
        return;
    }
    let st = state.clone();
    tokio::spawn(async move {
        let stopped = crate::agents::service::stop_all(
            &st,
            "the TLS listener this start serves moved the agents' origin",
        )
        .await;
        if !stopped.is_empty() {
            tracing::info!(
                "{} app container(s) were stopped because they were started under the old \
                 origin ({}); the next request to an agent's origin starts it again",
                stopped.len(),
                stopped.join(", ")
            );
        }
    });
}
