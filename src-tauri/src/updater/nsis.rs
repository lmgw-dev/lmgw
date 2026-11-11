//! The Windows install step (Windows design §4.4, WP7): the per-user NSIS
//! installer, which replaces lmgw's own files and so cannot run beside it.
//!
//! After the download and its sha256 check, lmgw writes what it went to
//! install (`update-pending.json` beside the download, in the private dir),
//! runs the quit sequence (the server stops, the engines stop), starts the
//! installer silently for this account (`lmgw_update::nsis`: `/S /UPDATE
//! /R`, no administrator rights) and exits. The installer overwrites the
//! files and starts lmgw again. It reports nothing back, so the next start
//! reads the note ([`after_start`]): its own version at or past the one it
//! went to install is the update done (a log line); an older one is the
//! update failed, said in a dialog, with the reason when lmgw knows it (the
//! installer did not start, or its file was no longer the one downloaded:
//! then lmgw restarts itself on the old version; lmgw was already quitting
//! when the update was confirmed). Either way the note and the downloaded
//! installer are removed then.
//!
//! Right before the start the installer is hashed again against the
//! manifest's sha256 (`lmgw_update::nsis::spawn_installer`); a file changed
//! since the download is removed and not run. The update's [`UpdateRun`]
//! is held from the download until lmgw exits.

use std::path::PathBuf;

use lmgw_core::update::UpdateInfo;
use serde::{Deserialize, Serialize};
use tauri::AppHandle;

use super::UpdateRun;
use crate::gateway::{quit_then, AfterQuit, QuitStep};

/// The download's basename in the private dir.
pub(super) const DOWNLOAD_NAME: &str = "lmgw-update-setup.exe";

/// What the prompt's install button does, after "Download and install it now?".
pub(super) const WHAT_INSTALL_DOES: &str =
    "lmgw closes (running models stop), the installer runs for your account without \
     administrator rights, and lmgw starts again when it is done.";

/// The note an update leaves for the next start, beside the download.
const PENDING: &str = "update-pending.json";

/// The downloaded installer the quit sequence ends with
/// ([`AfterQuit::Install`]): its path and the manifest's sha256, which it is
/// hashed against again before it starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Installer {
    pub(crate) path: PathBuf,
    pub(crate) sha256: String,
}

/// What an update went to install ([`PENDING`]).
#[derive(Debug, Serialize, Deserialize, PartialEq)]
struct Pending {
    /// The version the installer carries.
    version: String,
    /// The version that started it.
    from: String,
    /// Why the installer did not start, when it did not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

fn pending_path() -> std::io::Result<PathBuf> {
    Ok(lmgw_update::private_dir("lmgw")?.join(PENDING))
}

fn write_pending(pending: &Pending) -> std::io::Result<()> {
    let text = serde_json::to_string(pending).map_err(std::io::Error::other)?;
    std::fs::write(pending_path()?, text)
}

/// Note what is being installed, then quit and start the installer
/// (`gateway::quit_then` with [`AfterQuit::Install`], which calls
/// [`start_installer`] once the quit sequence has run). `run` is held until
/// lmgw exits.
pub(super) async fn install(app: AppHandle, info: UpdateInfo, installer: PathBuf, run: UpdateRun) {
    let mut pending = Pending {
        version: info.manifest.version.clone(),
        from: info.current.clone(),
        error: None,
    };
    if let Err(e) = write_pending(&pending) {
        tracing::warn!(
            "update: the note for the next start could not be written ({e}); the update goes \
             on, and the next start cannot say whether it took"
        );
    }
    let sha256 = info
        .manifest
        .installer()
        .map(|entry| entry.sha256.clone())
        .unwrap_or_default();
    let then = AfterQuit::Install(Installer {
        path: installer,
        sha256,
    });
    match quit_then(&app, then) {
        QuitStep::Start => tracing::info!(
            "update: lmgw quits, then the installer for {} starts",
            pending.version
        ),
        // The note stays, with why, so the next start says the update did
        // not install rather than nothing.
        QuitStep::Running | QuitStep::Done => {
            pending.error = Some(
                "lmgw was already quitting when the update was confirmed, so the installer did \
                 not start. Check for updates again to install it."
                    .to_string(),
            );
            let _ = write_pending(&pending);
            tracing::warn!(
                "update: lmgw is already quitting, so the installer for {} does not start",
                pending.version
            );
        }
    }
    run.keep();
}

/// The end of the quit sequence for an update: start the installer and exit;
/// when it does not start (its file no longer the one downloaded among the
/// reasons), note why for the next start and restart into this version.
pub(crate) fn start_installer(app: &AppHandle, installer: &Installer) {
    let path = &installer.path;
    match lmgw_update::nsis::spawn_installer(path, &installer.sha256) {
        Ok(()) => {
            tracing::info!(
                "update: the installer {} runs; lmgw exits for it",
                path.display()
            );
            app.exit(0);
        }
        Err(e) => {
            let why = format!("the installer {} did not start: {e}", path.display());
            tracing::error!("update: {why}; lmgw starts again on this version");
            let noted = pending_path()
                .and_then(std::fs::read_to_string)
                .ok()
                .and_then(|t| serde_json::from_str::<Pending>(&t).ok());
            if let Some(mut pending) = noted {
                pending.error = Some(why);
                let _ = write_pending(&pending);
            }
            app.request_restart();
        }
    }
}

/// What the start after an update says about it ([`Pending`]), from the
/// running version and the note.
fn outcome(pending: &Pending, running: &str) -> Result<String, String> {
    if lmgw_update::is_newer(&pending.version, running) {
        let why = pending.error.clone().unwrap_or_else(|| {
            "The installer ended without replacing lmgw; it reports nothing back. Run it from \
             the release by hand to see why."
                .to_string()
        });
        Err(format!(
            "The update to lmgw {} did not install; this is still lmgw {running}.\n\n{why}",
            pending.version
        ))
    } else {
        Ok(format!(
            "update: lmgw {running} runs, updated from {}",
            pending.from
        ))
    }
}

/// At the start: say how the last update went, when one was started, and
/// remove its note and the downloaded installer.
pub(super) fn after_start(app: &AppHandle) {
    let Ok(dir) = lmgw_update::private_dir("lmgw") else {
        return;
    };
    let note = dir.join(PENDING);
    let text = match std::fs::read_to_string(&note) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            return tracing::warn!("update: the last update's note {}: {e}", note.display());
        }
    };
    let _ = std::fs::remove_file(&note);
    let installer = dir.join(DOWNLOAD_NAME);
    match std::fs::remove_file(&installer) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        // The installer that started this lmgw may still be running: the
        // first poll tick tries again (`sweep`).
        Err(e) => tracing::info!(
            "update: the downloaded installer stays at {} for now ({e}); lmgw tries again when \
             its update checks begin",
            installer.display()
        ),
    }
    let pending: Pending = match serde_json::from_str(&text) {
        Ok(p) => p,
        Err(e) => {
            return tracing::warn!("update: the last update's note is not readable: {e}");
        }
    };
    match outcome(&pending, super::APP_VERSION) {
        Ok(line) => tracing::info!("{line}"),
        Err(message) => super::failed(app, message),
    }
}

/// At the first poll tick: remove a downloaded installer that no update is
/// pending for (the start could not remove it while the installer that
/// started lmgw still ran). One that cannot be removed now stays for the
/// next download, which replaces it.
pub(super) fn sweep() {
    let Ok(dir) = lmgw_update::private_dir("lmgw") else {
        return;
    };
    let installer = dir.join(DOWNLOAD_NAME);
    if dir.join(PENDING).exists() || !installer.exists() {
        return;
    }
    match std::fs::remove_file(&installer) {
        Ok(()) => tracing::info!(
            "update: removed the downloaded installer {}",
            installer.display()
        ),
        Err(e) => tracing::info!(
            "update: the downloaded installer stays at {} ({e}); the next download replaces it",
            installer.display()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_next_start_tells_an_update_that_took_from_one_that_did_not() {
        let p = |version: &str, error: Option<&str>| Pending {
            version: version.into(),
            from: "0.6.0+1".into(),
            error: error.map(str::to_string),
        };
        assert_eq!(
            outcome(&p("0.6.0+2", None), "0.6.0+2").unwrap(),
            "update: lmgw 0.6.0+2 runs, updated from 0.6.0+1"
        );
        assert!(outcome(&p("0.6.0+2", None), "0.6.1").is_ok(), "past it");
        let e = outcome(&p("0.6.0+2", None), "0.6.0+1").unwrap_err();
        assert!(
            e.starts_with(
                "The update to lmgw 0.6.0+2 did not install; this is still lmgw 0.6.0+1."
            ),
            "{e}"
        );
        assert!(e.contains("reports nothing back"), "{e}");
        let e = outcome(
            &p("0.6.0+2", Some("the installer x did not start: denied")),
            "0.6.0+1",
        )
        .unwrap_err();
        assert!(e.ends_with("the installer x did not start: denied"), "{e}");
        // The note's wire, as an older start reads it.
        let json = serde_json::to_string(&p("0.6.0+2", None)).unwrap();
        assert_eq!(json, r#"{"version":"0.6.0+2","from":"0.6.0+1"}"#);
        assert_eq!(
            serde_json::from_str::<Pending>(&json).unwrap(),
            p("0.6.0+2", None)
        );
    }
}
