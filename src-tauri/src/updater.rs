//! Background updater (§12): poll the update feed's manifest and, on
//! a newer build, prompt with a native dialog → download the RPM → `pkexec dnf
//! install` it → offer to restart. Self-install is RPM-only (how this app
//! ships); the fetch/compare/download is `lmgw_core::update` over the
//! `lmgw-update` crate, which also has the install step and its host check.
//!
//! **Non-RPM hosts.** The same binary built and run on Arch (or from the
//! AppImage) has no `dnf` to hand the package to, so [`can_self_install`]
//! gates the whole install path: the background poll stops at a log line
//! rather than opening a modal every six hours it cannot act on, and an
//! explicit "Check for Updates…" still reports the new version — it just says
//! where to get it instead of offering a button that would end in
//! "could not run pkexec/dnf".

use std::time::Duration;

use lmgw_core::state::SharedState;
use lmgw_core::update::{self, UpdateInfo};
use lmgw_update::can_self_install;
use tauri::{AppHandle, Manager};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};

/// Version baked into this binary — authoritative for the running process.
const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Let the app settle (and the gateway come up) before the first check.
const STARTUP_DELAY: Duration = Duration::from_secs(20);
/// Re-check every 6 hours thereafter.
const POLL_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

/// Spawn the background poll loop. Each tick is a no-op while the user has
/// disabled background checks (Settings → Tokens & updates).
pub fn spawn(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(STARTUP_DELAY).await;
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
                Ok(Some(info)) if !can_self_install() => tracing::info!(
                    "lmgw {} is available (running {APP_VERSION}), but this host is not \
                     RPM-based — self-install is RPM-only; update from source or the AppImage",
                    info.manifest.version
                ),
                Ok(Some(info)) => prompt_and_install(app.clone(), info),
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

/// Prompt the user about an available update; install on confirmation. Uses the
/// non-blocking `.show()` form so it is safe to call from the async runtime
/// (the dialog is dispatched to the main thread internally).
fn prompt_and_install(app: AppHandle, info: UpdateInfo) {
    let v = info.manifest.version.clone();
    let notes = info.manifest.notes.trim().to_string();
    // Nothing to offer on a host with no `dnf`: report the version and what it
    // would take, rather than a button whose only outcome is an error dialog.
    if !can_self_install() {
        let notes = if notes.is_empty() {
            String::new()
        } else {
            format!("\n\n{notes}")
        };
        app.dialog()
            .message(format!(
                "lmgw {v} is available (you have {}).{notes}\n\nThis host is not RPM-based, \
                 so lmgw can't install it for you — self-install is RPM-only. Update the way \
                 you installed it: rebuild from source (`git pull && cargo tauri build`), or \
                 replace the AppImage.",
                info.current
            ))
            .title("Update available")
            .kind(MessageDialogKind::Info)
            .show(|_| {});
        return;
    }
    let body = if notes.is_empty() {
        format!(
            "lmgw {v} is available (you have {}).\n\nDownload and install it now? \
             You'll be asked for your password to install the package.",
            info.current
        )
    } else {
        format!(
            "lmgw {v} is available (you have {}).\n\n{notes}\n\nDownload and install it now?",
            info.current
        )
    };
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

/// Download the RPM and install it via polkit; on success offer to restart.
fn do_install(app: AppHandle, info: UpdateInfo) {
    tauri::async_runtime::spawn(async move {
        let state = match app.try_state::<SharedState>() {
            Some(s) => s.inner().clone(),
            None => return,
        };

        // Fixed basename under the temp dir — never trust the manifest's path.
        let dest = std::env::temp_dir().join("lmgw-update.rpm");
        if let Err(e) = update::download_rpm(&state, &info.manifest.rpm, &dest).await {
            tracing::error!("update download failed: {e}");
            app.dialog()
                .message(format!("Download failed:\n{e}"))
                .title("Update failed")
                .kind(MessageDialogKind::Error)
                .show(|_| {});
            return;
        }

        // Privileged install via polkit; `dnf install` of a newer RPM upgrades
        // the package in place. `InstallError` words its own two cases
        // ("dnf exited with …", "could not run pkexec/dnf: …").
        let install_dest = dest.clone();
        let install = move || lmgw_update::install_rpm(&install_dest);
        let installed = match tokio::task::spawn_blocking(install).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(e.to_string()),
            Err(e) => Err(format!("install task error: {e}")),
        };
        let _ = tokio::fs::remove_file(&dest).await;

        match installed {
            Ok(()) => {
                let app2 = app.clone();
                let v = info.manifest.version.clone();
                app.dialog()
                    .message(format!(
                        "Updated to lmgw {v}. Restart now to use the new version?"
                    ))
                    .title("Update installed")
                    .kind(MessageDialogKind::Info)
                    .buttons(MessageDialogButtons::OkCancelCustom(
                        "Restart".into(),
                        "Later".into(),
                    ))
                    .show(move |restart| {
                        // The quit sequence first, then the restart (review
                        // P-2): the server stops and is waited for, the
                        // model containers stop, and only then is the
                        // restart asked for. Tauri ignores `prevent_exit`
                        // for a restart's exit, so the exit events cannot
                        // hold it for the sequence (`gateway` module doc).
                        if restart {
                            match crate::gateway::quit_then(
                                &app2,
                                crate::gateway::AfterQuit::Restart,
                            ) {
                                crate::gateway::QuitStep::Start => {}
                                crate::gateway::QuitStep::Done => app2.request_restart(),
                                crate::gateway::QuitStep::Running => tracing::warn!(
                                    "update: lmgw is already quitting, so it ends without the \
                                     restart; start it again to run {v}"
                                ),
                            }
                        }
                    });
            }
            Err(detail) => {
                tracing::error!("update install failed: {detail}");
                app.dialog()
                    .message(format!("Install failed:\n{detail}"))
                    .title("Update failed")
                    .kind(MessageDialogKind::Error)
                    .show(|_| {});
            }
        }
    });
}
