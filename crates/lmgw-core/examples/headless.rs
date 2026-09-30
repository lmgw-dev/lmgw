//! Headless dev runner: the full gateway without the Tauri shell.
//!
//! LMGW_DATA_DIR=<scratch dir> cargo run -p lmgw-core --example headless [-- <bind addr>]
//!
//! Dev-instance safety (chat-archive-pin-attachments review finding 12): a
//! fresh `LMGW_DATA_DIR` loads settings whose `container_prefix` is still the
//! production default — the same label the real tray app's model containers
//! carry. Boot reconciliation (`runtime::registry::Registry::reconcile`,
//! driven by `runtime::lifecycle::boot`, spawned from `server::run` below)
//! trusts that prefix to mean "every container under it belongs to *this*
//! gateway" and force-removes whatever it does not recognize — so a dev
//! instance sharing the production prefix can stop the real app's model
//! containers the moment it boots. That is exactly what bit a live-check
//! agent on 2026-09-25, which had to hand-apply migrations and forge sqlx's
//! bookkeeping table just to pre-seed settings without risking it.
//!
//! `ensure_dev_instance_safety` below steers a fresh data dir off that
//! default before `server::run` gets a chance to reconcile anything — this is
//! the only entry point that needs it: the Tauri app has its own
//! `src-tauri/src/main.rs`, and cargo examples are never included in an RPM
//! build, so nothing else can reach this binary in production.
//! `scripts/dev-instance.sh` wraps this with a scratch data dir.
//!
//! It is always a **dev instance** (container-builds §10), `LMGW_DEV` or
//! not: `AppState::init_with(…, true)` below. A dev instance shares
//! production's podman image store and `/var/tmp`, so it refuses image
//! deletion and keep_runs pruning, tags builds into
//! `localhost/lmgw-dev-<engine>:…`, and keeps its builds under
//! `~/.cache/lmgw-dev/builds` — a guard that must not hang on whether the
//! runner happened to be started through the script.

use lmgw_core::config::dev_instance_override;
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::{server, store};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "headless=info,lmgw_core=debug,tower_http=info".into()),
        )
        .init();

    // Never the installed app's data dir: this build's migrations are one-way
    // (the RPM refuses to start on a dir a newer build migrated), and the
    // prefix override below would rewrite the real app's container_prefix and
    // bind_addr. A dev run names its own dir; scripts/dev-instance.sh makes one.
    let data_dir = std::env::var("LMGW_DATA_DIR")
        .map(std::path::PathBuf::from)
        .map_err(|_| {
            anyhow::anyhow!(
            "LMGW_DATA_DIR is not set; the headless runner does not fall back to the installed \
             app's data dir. Use scripts/dev-instance.sh, or set LMGW_DATA_DIR to a scratch copy."
        )
        })?;
    let prod_dir = lmgw_core::state::default_data_dir();
    let same = |a: &std::path::Path, b: &std::path::Path| match (a.canonicalize(), b.canonicalize())
    {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    };
    if same(&data_dir, &prod_dir) {
        anyhow::bail!(
            "LMGW_DATA_DIR points at the installed app's data dir ({}); use a scratch copy",
            prod_dir.display()
        );
    }
    // Opens and migrates the store as its first steps. Always a dev
    // instance — see the module docs.
    if !lmgw_core::state::dev_from_env() {
        tracing::info!(
            "LMGW_DEV is not set; the headless runner is a dev instance regardless (its own tag \
             namespace and builds dir, no image deletion or pruning)"
        );
    }
    let state = AppState::init_with(data_dir, true).await?;

    let cli_addr = std::env::args().nth(1);
    let addr: std::net::SocketAddr = cli_addr
        .clone()
        .unwrap_or_else(|| state.snapshot().settings.bind_addr.clone())
        .parse()?;

    // The store is open and migrated; nothing has reconciled a container yet
    // — `server::run` below is what spawns that. Last chance to steer this
    // data dir away from the production `container_prefix` before it does.
    ensure_dev_instance_safety(&state, cli_addr.as_deref()).await?;

    server::run(state.clone(), addr, async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await?;

    // Ctrl-C is this binary's "quit", so it owes the same graceful teardown the
    // tray app's exit handler does (per-model-containers §3.4): stop every
    // managed container rather than leaving the GPU full behind a process that
    // is gone. A kill -9 skips this, deliberately — boot reconciliation adopts
    // what a crash leaves behind.
    lmgw_core::runtime::lifecycle::shutdown(&state).await;
    Ok(())
}

/// If this data dir's settings still carry the production default
/// `container_prefix`, move it to `lmgw-dev` (or `LMGW_CONTAINER_PREFIX`) and
/// pin `bind_addr` to the address this run was actually given, persist both,
/// and log one line saying so. A data dir that was already customized — an
/// owner-configured one, or one an earlier run of this same check already
/// fixed — is left alone entirely (see [`dev_instance_override`], which makes
/// that call and is unit-tested on its own since this I/O wrapper cannot be
/// exercised from `cargo test -p lmgw-core`).
async fn ensure_dev_instance_safety(
    state: &SharedState,
    cli_addr: Option<&str>,
) -> anyhow::Result<()> {
    let settings = state.snapshot().settings.clone();
    let env_prefix = std::env::var("LMGW_CONTAINER_PREFIX").ok();
    let Some((new_prefix, new_bind_addr)) =
        dev_instance_override(&settings.container_prefix, env_prefix.as_deref(), cli_addr)
    else {
        return Ok(());
    };

    let mut settings = settings;
    settings.container_prefix = new_prefix;
    if let Some(addr) = new_bind_addr {
        settings.bind_addr = addr;
    }
    tracing::info!(
        "dev instance safety: container_prefix -> '{}', bind_addr -> '{}' — a fresh data dir \
         must never boot under the production container_prefix, or boot reconciliation could \
         force-remove the real app's model containers (see scripts/dev-instance.sh)",
        settings.container_prefix,
        settings.bind_addr,
    );
    store::save_settings(&state.db, &settings).await?;
    state.reload_snapshot().await?;
    Ok(())
}
