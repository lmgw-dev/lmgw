//! Background updater (§12): poll the update feed's manifest and, on a newer
//! build, prompt with a native dialog → download this platform's installer →
//! install it → run the new version. The fetch/compare/download is
//! `lmgw_core::update` over the `lmgw-update` crate, which also has the
//! install steps and their host checks; how a platform installs is its
//! module's:
//!
//! - `rpm` (Linux): `pkexec dnf install` of the RPM, then an offer to
//!   restart;
//! - `nsis` (Windows, Windows design §4.4): the quit sequence, then the
//!   per-user NSIS installer started silently, which replaces the files and
//!   starts lmgw again; the next start says whether it took.
//!
//! **Hosts that can't install.** The same binary built and run on Arch (or
//! from the AppImage) has no `dnf` to hand the package to, a Windows
//! `lmgw.exe` run from anywhere but the installer's directory would install a
//! copy beside itself, and a release whose Windows build failed has no
//! Windows installer (or one whose entry the updater refuses). The core says
//! why (`lmgw_core::update::cannot_install`, which the dashboard's check
//! shows too), and that gates the whole install path: the background poll
//! stops at a log line rather than opening a modal every six hours it cannot
//! act on, and an explicit "Check for Updates…" still reports the new
//! version — it just says why there is nothing to install instead of
//! offering a button that would end in an error.
//!
//! **One update at a time.** The poll's dialog and "Check for Updates…" can
//! both be open, and both download to the one fixed path. A confirmed
//! install takes [`UpdateRun`] from the download through the install (on
//! Windows until lmgw exits for the installer); a second confirmation while
//! it is held is told, in a dialog, that an update already runs, and starts
//! nothing.

use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use lmgw_core::state::SharedState;
use lmgw_core::update::{self, UpdateInfo};
use tauri::{AppHandle, Manager};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};

#[cfg(windows)]
mod nsis;
#[cfg(not(windows))]
mod rpm;

#[cfg(windows)]
use nsis as platform;
#[cfg(not(windows))]
use rpm as platform;

#[cfg(windows)]
pub(crate) use nsis::{start_installer, Installer};

/// Version baked into this binary — authoritative for the running process.
const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Let the app settle (and the gateway come up) before the first check.
const STARTUP_DELAY: Duration = update::notice::STARTUP_DELAY;
/// Re-check every 6 hours thereafter. The server image's notice polls on
/// the same schedule, from the same constants.
const POLL_INTERVAL: Duration = update::notice::POLL_INTERVAL;

/// Spawn the background poll loop. Each tick is a no-op while the user has
/// disabled background checks (Settings → Tokens & updates). On Windows it
/// first says how the last update went, when one was started (`nsis`).
pub fn spawn(app: AppHandle) {
    #[cfg(windows)]
    nsis::after_start(&app);
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(STARTUP_DELAY).await;
        // A downloaded installer the start could not remove (it was still running then).
        #[cfg(windows)]
        nsis::sweep();
        let mut tick = tokio::time::interval(POLL_INTERVAL);
        loop {
            tick.tick().await;
            let state = match app.try_state::<SharedState>() {
                Some(s) => s.inner().clone(),
                None => continue,
            };
            if !state.snapshot().settings.update_check_enabled {
                continue;
            }
            match update::check(&state, APP_VERSION).await {
                // A host that cannot install what it just found gets a log
                // line, not a dialog: the prompt's only button would fail.
                Ok(Some(info)) => match cannot_install(&info) {
                    Some(why) => tracing::info!("{}", why.replace("\n\n", " ")),
                    None => prompt_and_install(app.clone(), info),
                },
                Ok(None) => tracing::debug!("update check: up to date ({APP_VERSION})"),
                Err(e) => tracing::warn!("update check failed: {e}"),
            }
        }
    });
}

/// One-off check from the tray "Check for Updates…" item. Unlike the background
/// loop this always reports back — including "up to date" and errors — since the
/// user asked explicitly.
pub fn check_now(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let state = match app.try_state::<SharedState>() {
            Some(s) => s.inner().clone(),
            None => return,
        };
        match update::check(&state, APP_VERSION).await {
            Ok(Some(info)) => prompt_and_install(app.clone(), info),
            Ok(None) => {
                app.dialog()
                    .message(format!("lmgw {APP_VERSION} is the latest version."))
                    .title("No updates")
                    .kind(MessageDialogKind::Info)
                    .show(|_| {});
            }
            Err(e) => {
                app.dialog()
                    .message(format!("Couldn't check for updates:\n{e}"))
                    .title("Update check failed")
                    .kind(MessageDialogKind::Error)
                    .show(|_| {});
            }
        }
    });
}

/// The release notes as a paragraph of the dialog, or nothing.
fn notes_paragraph(info: &UpdateInfo) -> String {
    let notes = info.manifest.notes.trim();
    if notes.is_empty() {
        String::new()
    } else {
        format!("\n\n{notes}")
    }
}

/// Why this lmgw can't install `info`, as the dialog's text (the core's
/// reason after the version and the notes); `None` when it can.
fn cannot_install(info: &UpdateInfo) -> Option<String> {
    let why = update::cannot_install(info)?;
    Some(format!(
        "lmgw {} is available (you have {}).{}\n\n{why}",
        info.manifest.version,
        info.current,
        notes_paragraph(info)
    ))
}

/// The update that runs now, by the version it installs (module doc).
static RUNNING: Mutex<Option<String>> = Mutex::new(None);

/// One confirmed update, from its download through its install: taken
/// before the download, given back when it drops (a failed download, the
/// RPM's install done), or kept until lmgw exits ([`UpdateRun::keep`]).
pub(super) struct UpdateRun {
    slot: &'static Mutex<Option<String>>,
}

impl UpdateRun {
    /// Take it for `version`, or the version of the update that holds it.
    fn begin(version: &str) -> Result<Self, String> {
        Self::begin_in(&RUNNING, version)
    }

    fn begin_in(slot: &'static Mutex<Option<String>>, version: &str) -> Result<Self, String> {
        let mut held = slot.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(running) = held.as_ref() {
            return Err(running.clone());
        }
        *held = Some(version.to_string());
        Ok(Self { slot })
    }

    /// Hold it until lmgw exits: the quit for the installer has begun.
    #[cfg(windows)]
    pub(super) fn keep(self) {
        std::mem::forget(self);
    }
}

impl Drop for UpdateRun {
    fn drop(&mut self) {
        *self.slot.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }
}

/// What a second confirmation is told while the update to `running` runs.
fn already_running(running: &str) -> String {
    format!(
        "An update is already running: lmgw {running} is being downloaded and installed, and \
         lmgw says when it is done.\n\nThis Install does not start a second one."
    )
}

/// An error dialog titled "Update failed", and the same in the log.
fn failed(app: &AppHandle, message: String) {
    tracing::error!("update: {}", message.replace('\n', " "));
    app.dialog()
        .message(message)
        .title("Update failed")
        .kind(MessageDialogKind::Error)
        .show(|_| {});
}

/// Prompt the user about an available update; install on confirmation. Uses the
/// non-blocking `.show()` form so it is safe to call from the async runtime
/// (the dialog is dispatched to the main thread internally).
fn prompt_and_install(app: AppHandle, info: UpdateInfo) {
    // Nothing to offer where this host can't install it: report the version
    // and why, rather than a button whose only outcome is an error dialog.
    if let Some(why) = cannot_install(&info) {
        app.dialog()
            .message(why)
            .title("Update available")
            .kind(MessageDialogKind::Info)
            .show(|_| {});
        return;
    }
    let body = format!(
        "lmgw {} is available (you have {}).{}\n\nDownload and install it now? {}",
        info.manifest.version,
        info.current,
        notes_paragraph(&info),
        platform::WHAT_INSTALL_DOES
    );
    let app2 = app.clone();
    app.dialog()
        .message(body)
        .title("Update available")
        .kind(MessageDialogKind::Info)
        .buttons(MessageDialogButtons::OkCancelCustom(
            "Install & Restart".into(),
            "Later".into(),
        ))
        .show(move |install| {
            if install {
                do_install(app2.clone(), info.clone());
            }
        });
}

/// Download this platform's installer and hand it to the platform's install
/// step, holding [`UpdateRun`]; while another update holds it, say so.
fn do_install(app: AppHandle, info: UpdateInfo) {
    let run = match UpdateRun::begin(&info.manifest.version) {
        Ok(run) => run,
        Err(running) => {
            tracing::info!(
                "update: the update to {running} already runs; this Install starts nothing"
            );
            app.dialog()
                .message(already_running(&running))
                .title("Update already running")
                .kind(MessageDialogKind::Info)
                .show(|_| {});
            return;
        }
    };
    tauri::async_runtime::spawn(async move {
        let state = match app.try_state::<SharedState>() {
            Some(s) => s.inner().clone(),
            None => return,
        };
        // `cannot_install` found one before the prompt.
        let Some(artifact) = info.manifest.installer().cloned() else {
            return;
        };

        // A fixed basename in a directory only this user can write
        // ($XDG_RUNTIME_DIR/lmgw, 0700; without XDG_RUNTIME_DIR a checked
        // /tmp/lmgw-<uid>; on Windows %TEMP%\lmgw with a private descriptor) —
        // never a shared fixed /tmp path, never the manifest's name.
        let dest = match lmgw_update::private_dir("lmgw") {
            Ok(dir) => dir.join(platform::DOWNLOAD_NAME),
            Err(e) => return failed(&app, format!("No private place for the download:\n{e}")),
        };
        if let Err(e) = update::download(&state, &artifact, &dest).await {
            return failed(&app, format!("Download failed:\n{e}"));
        }
        platform::install(app, info, dest, run).await;
    });
}

/// Ask for the restart into the installed version after the quit sequence
/// (review P-2): the server stops and is waited for, the model containers
/// stop, and only then is the restart asked for. Tauri ignores
/// `prevent_exit` for a restart's exit, so the exit events cannot hold it
/// for the sequence (`gateway` module doc).
#[cfg(not(windows))]
fn restart_after_quit(app: &AppHandle, version: &str) {
    match crate::gateway::quit_then(app, crate::gateway::AfterQuit::Restart) {
        crate::gateway::QuitStep::Start => {}
        crate::gateway::QuitStep::Done => app.request_restart(),
        crate::gateway::QuitStep::Running => tracing::warn!(
            "update: lmgw is already quitting, so it ends without the restart; start it again \
             to run {version}"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_install_is_told_that_one_runs_until_the_first_ends() {
        static SLOT: Mutex<Option<String>> = Mutex::new(None);
        let first = UpdateRun::begin_in(&SLOT, "0.6.0+2").unwrap();
        let running = UpdateRun::begin_in(&SLOT, "0.6.0+3").err().unwrap();
        assert_eq!(running, "0.6.0+2", "the second is told the first's version");
        assert!(already_running(&running).starts_with(
            "An update is already running: lmgw 0.6.0+2 is being downloaded and installed"
        ));
        drop(first);
        let again = UpdateRun::begin_in(&SLOT, "0.6.0+3");
        assert!(again.is_ok(), "a failed download gives it back");
    }

    #[test]
    fn the_product_name_is_the_bundles() {
        let conf: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        assert_eq!(conf["productName"], update::PRODUCT);
    }
}
