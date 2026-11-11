//! The process's quit signals (headless server design §1 step 7), installed
//! before the start's first step that can take a while, so a `podman stop`
//! during the gateway's initialisation is a quit the log says, not the
//! default disposition's silent kill.

use tokio::signal::unix::{signal, Signal, SignalKind};
use tokio::sync::mpsc;

/// The process's quit signals, in order, for [`super::serve`]: SIGTERM
/// (`podman stop`) and Ctrl-C (SIGINT). The handlers are installed before
/// this returns, so a signal from then on is a quit. A handler that cannot
/// be installed leaves that signal's default, and says so. Called inside the
/// runtime.
pub fn quit_signals() -> mpsc::Receiver<&'static str> {
    let mut term = install(SignalKind::terminate(), "SIGTERM");
    let mut int = install(SignalKind::interrupt(), "Ctrl-C");
    let (tx, rx) = mpsc::channel(2);
    tokio::spawn(async move {
        loop {
            let signal = tokio::select! {
                Some(()) = next(&mut term) => "SIGTERM",
                Some(()) = next(&mut int) => "Ctrl-C",
                else => break,
            };
            if tx.send(signal).await.is_err() {
                break;
            }
        }
    });
    rx
}

fn install(kind: SignalKind, name: &str) -> Option<Signal> {
    match signal(kind) {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::warn!("{name} handler not installed ({e}): {name} ends the process at once");
            None
        }
    }
}

/// The next signal of `s`; never, without a handler.
async fn next(s: &mut Option<Signal>) -> Option<()> {
    match s {
        Some(s) => s.recv().await,
        None => std::future::pending().await,
    }
}
