//! The server image's update notice (headless server design §6.3, ruling
//! 13): the image has no updater (ruling 1), so in container mode lmgw
//! polls the same feed the desktop's updater polls, on the desktop poll's
//! schedule, and says what it found in one line on Settings → About and one
//! in the log. It downloads nothing. The switch is the existing
//! `update_check_enabled`: the save that switches it off clears the notice
//! ([`switched`]), and while it is off the background check asks nothing
//! and leaves the notice alone. An explicit `update_check` is the
//! administrator's own request and still asks the feed and keeps what it
//! found, off or on (design §6.3). With no registry (ruling
//! 11) the notice compares versions alone and names the archive's path:
//! load it, restart the service.

use std::time::Duration;

use lmgw_api_types::UpdateNotice;

use crate::state::SharedState;

/// How long after the start the first check runs, as the desktop's poll
/// waits (`src-tauri/src/updater.rs`): the gateway settles first.
pub const STARTUP_DELAY: Duration = Duration::from_secs(20);

/// How often the check runs after that, as the desktop's poll does.
pub const POLL_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

/// The line About and the log carry.
pub fn sentence(latest: &str, running: &str) -> String {
    format!(
        "lmgw {latest} is out; this server runs {running}. Updates come as a new image archive: \
         load it and restart the service."
    )
}

/// What one check of the feed did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Checked {
    /// `update_check_enabled` is off: nothing was asked.
    Off,
    /// This lmgw is the feed's newest.
    UpToDate,
    /// The feed has a newer version.
    Newer(UpdateNotice),
    /// The feed could not be read: why. The last notice stays.
    Failed(String),
}

/// `latest` against `running`, as the notice says it now.
fn notice(latest: &str, running: &str) -> UpdateNotice {
    UpdateNotice {
        latest: latest.to_string(),
        running: running.to_string(),
        checked_at: chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        sentence: sentence(latest, running),
    }
}

/// Keep `latest` as the notice, and log its line when the version is new to
/// this process; the same version found again is a debug line.
pub(crate) fn record(state: &SharedState, latest: &str, running: &str) -> UpdateNotice {
    let n = notice(latest, running);
    let was = state.set_update_notice(Some(n.clone()));
    if was.is_some_and(|w| w.latest == n.latest) {
        tracing::debug!("update check: {}", n.sentence);
    } else {
        tracing::info!("{}", n.sentence);
    }
    n
}

/// One background check of the feed (module doc). Off, it asks nothing
/// and leaves the notice as it is; up to date clears it; a feed that cannot
/// be read keeps it and logs why.
pub async fn check_once(state: &SharedState) -> Checked {
    if !state.snapshot().settings.update_check_enabled {
        return Checked::Off;
    }
    let running = super::current_version();
    match super::check(state, running).await {
        Ok(Some(info)) => Checked::Newer(record(state, &info.manifest.version, running)),
        Ok(None) => {
            state.set_update_notice(None);
            tracing::debug!("update check: up to date ({running})");
            Checked::UpToDate
        }
        Err(e) => {
            tracing::warn!("update check failed: {e}");
            Checked::Failed(e.to_string())
        }
    }
}

/// A settings save from `was` to `now`: switching `update_check_enabled`
/// off clears the notice in that save, so none stays shown for a check
/// that is off.
pub(crate) fn switched(state: &crate::state::AppState, was: bool, now: bool) {
    if was && !now && state.set_update_notice(None).is_some() {
        tracing::info!("update check switched off: the update notice is cleared");
    }
}

/// The poll (module doc), until the task is aborted or the process ends.
pub fn spawn(state: SharedState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        tokio::time::sleep(STARTUP_DELAY).await;
        let mut tick = tokio::time::interval(POLL_INTERVAL);
        loop {
            tick.tick().await;
            check_once(&state).await;
        }
    })
}
