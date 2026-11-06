//! A watch on the connection (find-a-model design D133): one message each
//! time NetworkManager reports a change, from `nmcli monitor`, which runs
//! only while someone holds the watch. The downloads the boot paused on a
//! metered connection hold one and ask [`super::connection_metered`] again
//! at each message, so they go on soon after the connection stops being
//! metered, with no interval to choose and nothing asked while nothing
//! changes.

use std::process::Stdio;
use std::sync::Arc;

use tokio::io::AsyncBufReadExt;

/// One message per change NetworkManager reported; closed when its monitor
/// ended. Messages that arrive while one waits are folded into it.
pub type NetChanges = tokio::sync::mpsc::Receiver<()>;

/// How a gateway begins a watch: NetworkManager's monitor on the real
/// machine, none on a test state's blank one, a channel a test feeds
/// ([`super::MachineChecks::set_net_watch`]).
pub type NetWatch = Arc<dyn Fn() -> Result<NetChanges, String> + Send + Sync>;

/// No watch: why, for the log.
pub(super) fn none(why: &'static str) -> NetWatch {
    Arc::new(move || Err(why.to_string()))
}

/// `nmcli monitor`, a line per change, until the watch is dropped; then the
/// monitor is killed. Its own process group is not needed: nmcli starts
/// nothing.
pub(super) fn nmcli() -> NetWatch {
    Arc::new(|| {
        let mut child = tokio::process::Command::new(super::prefetch::NMCLI)
            .arg("monitor")
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("{} monitor could not start: {e}", super::prefetch::NMCLI))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| format!("{} monitor has no output", super::prefetch::NMCLI))?;
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        tokio::spawn(async move {
            let mut lines = tokio::io::BufReader::new(stdout).lines();
            loop {
                tokio::select! {
                    // The watch was dropped: the monitor goes with `child`.
                    () = tx.closed() => break,
                    line = lines.next_line() => match line {
                        Ok(Some(_)) => {
                            // Full: a message waits already, which says it.
                            let _ = tx.try_send(());
                        }
                        Ok(None) | Err(_) => break,
                    },
                }
            }
            drop(child);
        });
        Ok(rx)
    })
}
