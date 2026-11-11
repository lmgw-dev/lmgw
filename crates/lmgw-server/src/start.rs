//! The production server start (headless server design §1): the steps after
//! the command line and the logging, from the refusal outside a container
//! to the quit's teardown.

use std::future::Future;
use std::path::Path;
use std::sync::Arc;

use lmgw_core::config::deployment::Start;
use lmgw_core::server;
use lmgw_core::state::data_dir_lock::DataDirLock;
use lmgw_core::state::{AppState, Host, SharedState};
use tokio::sync::mpsc;

use crate::env::ServerEnv;

/// What the start says about where it runs.
mod lines;
/// The quit signals, installed before the start.
mod signals;

pub use lines::{deployment_lines, Line, CONTAINERENV};
pub use signals::quit_signals;

/// Why this start must not go on (D1), when it must not: a production start
/// outside the server image would share the user's Podman, its image store
/// and its containers under the production prefix with the tray app.
pub fn refusal(env: &ServerEnv) -> Option<String> {
    (!env.deployment.is_container() && !env.dev).then(|| {
        "lmgw-server starts only in the server image (LMGW_DEPLOYMENT=container) or as a dev \
         instance (LMGW_DEV=1): a production start on a desktop would share this user's Podman, \
         its image store and its containers under the production prefix with the tray app"
            .to_string()
    })
}

/// The warning a start as `pid` 1 logs (D5), when it is one: Podman's
/// helpers that lmgw's inner containers leave behind are reaped by nobody.
pub fn pid1_warning(pid: u32) -> Option<&'static str> {
    (pid == 1).then_some(
        "lmgw-server is PID 1: the processes podman leaves behind (conmon, pasta) are never \
         reaped and stay zombies; start it under an init (the image's catatonit, or podman run \
         --init)",
    )
}

/// How the start ends the process at once: [`std::process::exit`] in the
/// binary, a recorder in a test.
pub type Exit = Arc<dyn Fn(i32) + Send + Sync>;

/// A start that is ready to serve: its gateway, its bound listeners (the
/// TLS one when the settings configure it), and the data-dir lock it holds
/// until it is dropped.
pub struct Ready {
    pub state: SharedState,
    pub listener: tokio::net::TcpListener,
    pub tls: Option<server::BoundTls>,
    pub lock: DataDirLock,
}

/// The start, from the data dir to the teardown (§1, steps 4–8), on `host`
/// (the machine's podman, GPU and facts in the binary): [`prepare`], then
/// [`serve_until_quit`] with the containers' teardown. `env` has passed
/// [`refusal`]; `signals` is [`quit_signals`], installed before anything
/// else, so a quit during the start is never the default disposition's
/// silent kill.
pub async fn serve(
    env: ServerEnv,
    host: Host,
    mut signals: mpsc::Receiver<&'static str>,
    exit: Exit,
) -> anyhow::Result<()> {
    let Some(Ready {
        state,
        listener,
        tls,
        lock,
    }) = prepare(&env, host, &mut signals, &exit).await?
    else {
        return Ok(());
    };
    // The update notice (§6.3, ruling 13): the image has no updater, so it
    // polls the feed and says what is out.
    let notice = env
        .deployment
        .is_container()
        .then(|| lmgw_core::update::notice::spawn(state.clone()));
    serve_until_quit(
        state,
        listener,
        tls,
        signals,
        move |code| exit(code),
        |st| async move { lmgw_core::runtime::lifecycle::shutdown(&st).await },
    )
    .await?;
    if let Some(n) = notice {
        n.abort();
    }
    drop(lock);
    Ok(())
}

/// The start up to its listener (§1, steps 4–6): the data dir and its lock,
/// the gateway with its environment's pinned and seeded settings, a dev
/// instance's prefix step, the bind. `None` when a quit signal came during
/// it: the start finishes opening the gateway (a database step cut short is
/// worse than a start that takes a moment longer), says so and serves
/// nothing; a second signal calls `exit` at once.
pub async fn prepare(
    env: &ServerEnv,
    host: Host,
    signals: &mut mpsc::Receiver<&'static str>,
    exit: &Exit,
) -> anyhow::Result<Option<Ready>> {
    // 4. The data dir, required, and its lock (D28).
    let data_dir = env.data_dir.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "LMGW_DATA_DIR is not set (or empty); lmgw-server has no default data dir (the \
             image sets /data)"
        )
    })?;
    if env.dev {
        refuse_installed_data_dir(&data_dir)?;
    }
    let lock = DataDirLock::acquire(&data_dir, "lmgw-server").map_err(anyhow::Error::msg)?;
    tracing::info!("holding the data dir: {}", lock.path().display());

    // 5. The gateway, with what its environment pins and seeds applied
    // before its snapshot is first read.
    for line in deployment_lines(env, Path::new(CONTAINERENV).exists()) {
        line.log();
    }
    let start = Start {
        deployment: env.deployment,
        env: env.container.clone(),
    };
    let init = AppState::init_started_on(data_dir, env.dev, start, host);
    tokio::pin!(init);
    let mut stopping = None;
    let state = loop {
        tokio::select! {
            r = &mut init => break r?,
            Some(signal) = signals.recv() => match stopping {
                None => {
                    tracing::info!(
                        "{signal}: stopping during start; the gateway finishes opening its data \
                         dir and exits without serving (again to exit at once)"
                    );
                    stopping = Some(signal);
                }
                Some(_) => {
                    tracing::warn!("{signal} again: exiting now, during the start");
                    exit(server::signal_exit_code(signal));
                }
            },
        }
    };
    if stopped(stopping, signals) {
        return Ok(None);
    }
    // A dev instance outside a container chooses its address with
    // LMGW_BIND_ADDR, as the headless runner does with its argument; in
    // container mode the variable is pinned and applied already.
    let dev_addr = (!env.deployment.is_container())
        .then_some(env.container.bind_addr)
        .flatten()
        .map(|a| a.to_string());
    if env.dev {
        // A dev instance's step before anything lists a container by
        // prefix: a fresh dir leaves the production prefix.
        lmgw_core::state::dev_instance::ensure_dev_instance_safety(
            &state,
            env.dev_container_prefix.as_deref(),
            dev_addr.as_deref(),
        )
        .await?;
    }
    if let Some(budget) = state.stop_budget() {
        if budget.over {
            tracing::warn!("stop budget: {}", budget.sentence);
        } else {
            tracing::info!("stop budget: {}", budget.sentence);
        }
    }

    // 6. The listener; a failed bind ends the start (no tray to keep alive).
    let bind_addr = dev_addr.unwrap_or_else(|| state.snapshot().settings.bind_addr.clone());
    let addr: std::net::SocketAddr = bind_addr
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("bind_addr '{bind_addr}' is not an address and port"))?;
    if stopped(None, signals) {
        return Ok(None);
    }
    let listener = server::bind(&state, addr).await?;
    // The TLS listener (§4): a pair that does not load or an address that
    // does not bind ends the start, as the plain bind does.
    let tls = server::bind_tls(&state).await?;
    if tls.is_none() && state.snapshot().settings.tls.has_pair() {
        tracing::warn!(
            "a TLS pair is set but tls.listen is empty: no TLS listener (set LMGW_TLS_LISTEN)"
        );
    }
    Ok(Some(Ready {
        state,
        listener,
        tls,
        lock,
    }))
}

/// Whether a quit signal came during the start (`stopping`, or one waiting
/// in `signals` now), said when it did.
fn stopped(stopping: Option<&'static str>, signals: &mut mpsc::Receiver<&'static str>) -> bool {
    match stopping.or_else(|| signals.try_recv().ok()) {
        Some(signal) => {
            tracing::info!("{signal}: stopped during start; the gateway did not serve");
            true
        }
        None => false,
    }
}

/// Serve on `listener`, and `tls` beside it, until the first of `signals` (SIGTERM from `podman
/// stop`, Ctrl-C), then `teardown` once the server returned (§1 steps 7–8):
/// the inner model containers stop before the outer one does, and no
/// session or turn still running has its model stopped under it. A second
/// signal during the stop calls `exit` at once with
/// [`server::signal_exit_code`], as the headless runner does: a stop that is
/// taking its time is never the only way out but SIGKILL.
pub async fn serve_until_quit<T, F>(
    state: SharedState,
    listener: tokio::net::TcpListener,
    tls: Option<server::BoundTls>,
    mut signals: mpsc::Receiver<&'static str>,
    exit: impl FnOnce(i32) + Send + 'static,
    teardown: T,
) -> anyhow::Result<()>
where
    T: FnOnce(SharedState) -> F,
    F: Future<Output = ()>,
{
    let shutdown = async move {
        let Some(first) = signals.recv().await else {
            // No signal source left: nothing will ever ask for a stop.
            return std::future::pending().await;
        };
        tracing::info!("{first}: stopping (again to exit at once)");
        tokio::spawn(async move {
            if let Some(second) = signals.recv().await {
                tracing::warn!(
                    "{second} again: exiting now, without waiting for the stop to finish or the \
                     model containers to stop (the next start adopts them)"
                );
                exit(server::signal_exit_code(second));
            }
        });
    };
    server::serve_with(state.clone(), listener, tls, shutdown, None).await?;
    teardown(state).await;
    Ok(())
}

/// A dev instance never runs on the installed app's data dir: this build's
/// migrations are one-way, and the dev step rewrites the dir's prefix.
pub(crate) fn refuse_installed_data_dir(data_dir: &Path) -> anyhow::Result<()> {
    let installed = lmgw_core::state::default_data_dir();
    let home_default = lmgw_core::state::home_default_data_dir();
    if lmgw_core::state::is_installed_data_dir(data_dir, &installed, home_default.as_deref()) {
        anyhow::bail!(
            "LMGW_DATA_DIR ({}) is the installed app's data dir; a dev instance uses a scratch \
             copy",
            data_dir.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lmgw_core::config::deployment::Deployment;

    #[test]
    fn only_a_container_or_a_dev_instance_starts() {
        let mut env = ServerEnv::default();
        let why = refusal(&env).expect("a desktop production start is refused");
        assert!(
            why.contains("LMGW_DEPLOYMENT=container") && why.contains("tray app"),
            "{why}"
        );
        env.dev = true;
        assert_eq!(refusal(&env), None);
        env.dev = false;
        env.deployment = Deployment::Container;
        assert_eq!(refusal(&env), None);
    }

    #[test]
    fn pid_1_warns() {
        assert!(pid1_warning(1).is_some_and(|w| w.contains("zombies")));
        assert_eq!(pid1_warning(2), None);
    }
}
