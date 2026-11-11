//! The Linux install step: `pkexec dnf install` of the downloaded RPM, then
//! an offer to restart into it. Self-install needs `pkexec` and `dnf`
//! (`lmgw_update::can_self_install`); a host without them is told how to
//! update instead (`lmgw_core::update::cannot_install`).

use std::path::PathBuf;

use lmgw_core::update::UpdateInfo;
use tauri::AppHandle;
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};

/// The download's basename in the private dir.
pub(super) const DOWNLOAD_NAME: &str = "lmgw-update.rpm";

/// What the prompt's install button does, after "Download and install it now?".
pub(super) const WHAT_INSTALL_DOES: &str =
    "You'll be asked for your password to install the package.";

/// Install the RPM at `rpm` via polkit; on success offer to restart. The file
/// is removed whatever the outcome, and `run` given back once dnf is done.
pub(super) async fn install(app: AppHandle, info: UpdateInfo, rpm: PathBuf, run: super::UpdateRun) {
    // Privileged install via polkit; `dnf install` of a newer RPM upgrades
    // the package in place. `InstallError` words its own two cases
    // ("dnf exited with …", "could not run pkexec/dnf: …").
    let install_dest = rpm.clone();
    let install = move || lmgw_update::install_rpm(&install_dest);
    let installed = match tokio::task::spawn_blocking(install).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(e.to_string()),
        Err(e) => Err(format!("install task error: {e}")),
    };
    let _ = tokio::fs::remove_file(&rpm).await;
    drop(run);

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
                    if restart {
                        super::restart_after_quit(&app2, &v);
                    }
                });
        }
        Err(detail) => super::failed(&app, format!("Install failed:\n{detail}")),
    }
}
