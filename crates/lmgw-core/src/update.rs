//! Background update check (§12): the app polls a small JSON manifest that CI
//! attaches to every GitHub release, compares it to the running build's
//! version, and — in the Tauri shell — prompts to download + `dnf install` the
//! new RPM, or on Windows to run the per-user installer (Windows design §4.4). The server image has no updater: [`notice`] polls the same feed
//! there and only says what it found (headless server design §6.3).
//!
//! The mechanism — manifest, version order, sha256-verified download, the
//! install step — is the `lmgw-update` crate's, shared with the desktop
//! clients. What is lmgw's lives here (not in the Tauri crate): which feed
//! this build polls, its auth (build token, env, the persisted Settings
//! field), and the shared `reqwest` client the requests go through.

use std::path::Path;

pub mod notice;

pub use lmgw_update::{is_newer, Artifact, Feed, FeedAuth, ReleaseManifest, UpdateInfo};

use crate::state::SharedState;

/// The public feed: `latest.json` as an asset of the newest GitHub release,
/// which GitHub redirects to the file itself.
pub const PUBLIC_MANIFEST_URL: &str =
    "https://github.com/lmgw-dev/lmgw/releases/latest/download/latest.json";

/// The feed this build polls: the public one, unless the build was told
/// otherwise through `LMGW_UPDATE_MANIFEST_URL` at compile time (a private CI
/// publishing its own test builds sets it). `LMGW_UPDATE_ENDPOINT` overrides
/// it at run time for forks/testing — surfaced via an env var, never a hidden
/// constant swap.
pub const DEFAULT_MANIFEST_URL: &str = match option_env!("LMGW_UPDATE_MANIFEST_URL") {
    Some(url) if !url.is_empty() => url,
    _ => PUBLIC_MANIFEST_URL,
};

/// A read-only token for a private feed, baked in at compile time from
/// `LMGW_UPDATE_DEPLOY_TOKEN` (sent as `Deploy-Token`, a GitLab deploy token).
/// Absent from public builds: the public feed needs none, and the value lives
/// only in the private CI's masked variables, never in the source.
const BUILD_DEPLOY_TOKEN: Option<&str> = match option_env!("LMGW_UPDATE_DEPLOY_TOKEN") {
    Some(token) if !token.is_empty() => Some(token),
    _ => None,
};

/// The effective manifest URL (env override or the built-in default).
pub fn manifest_url() -> String {
    std::env::var("LMGW_UPDATE_ENDPOINT")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_MANIFEST_URL.to_string())
}

/// Version baked into this build. All workspace crates share
/// `version.workspace`, so the core's `CARGO_PKG_VERSION` equals the app's.
pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// A user-supplied token for a private feed (sent as `PRIVATE-TOKEN`, the
/// header a GitLab package registry takes), if any: env override first
/// (headless), then the persisted Settings field. Wins over the build's
/// deploy token. The public feed needs neither.
fn user_token(state: &SharedState) -> Option<String> {
    if let Ok(t) = std::env::var("LMGW_REGISTRY_TOKEN") {
        let t = t.trim().to_string();
        if !t.is_empty() {
            return Some(t);
        }
    }
    let t = state.snapshot().settings.update_token.trim().to_string();
    (!t.is_empty()).then_some(t)
}

/// The feed this build polls, with its auth: the user token via
/// `PRIVATE-TOKEN`, else the build's deploy token via `Deploy-Token`, else
/// anonymous. Built per request, so a token saved in Settings applies to the
/// next one.
pub fn feed(state: &SharedState) -> Feed {
    let auth = match (user_token(state), BUILD_DEPLOY_TOKEN) {
        (Some(token), _) => FeedAuth::PrivateToken(token),
        (None, Some(token)) => FeedAuth::DeployToken(token.to_string()),
        (None, None) => FeedAuth::None,
    };
    Feed {
        url: manifest_url(),
        auth,
    }
}

/// Fetch and parse the published manifest. Sends a `PRIVATE-TOKEN` header when a
/// feed token is configured (a private feed); anonymous otherwise.
pub async fn fetch_manifest(state: &SharedState) -> anyhow::Result<ReleaseManifest> {
    Ok(lmgw_update::fetch_manifest(&state.http, &feed(state)).await?)
}

/// Check for a newer build. `Ok(None)` means up to date.
pub async fn check(state: &SharedState, current: &str) -> anyhow::Result<Option<UpdateInfo>> {
    Ok(lmgw_update::check(&state.http, &feed(state), current).await?)
}

/// Stream an installer (the RPM, or the Windows installer) to `dest`, verifying
/// the manifest sha256 when present. On a hash mismatch the partial file is
/// removed so a retry re-downloads cleanly.
pub async fn download(state: &SharedState, artifact: &Artifact, dest: &Path) -> anyhow::Result<()> {
    Ok(lmgw_update::download(&state.http, &feed(state), artifact, dest).await?)
}

/// The installer's product name (`tauri.conf.json`'s `productName`): the
/// per-user uninstall key the Windows installer writes is named after it.
pub const PRODUCT: &str = "lmgw";

/// Why this lmgw can't install `info` itself, as one or two sentences the
/// tray's dialog and the dashboard's check both show; `None` when it can
/// (Windows design §4.4). On Windows: a release without a Windows installer,
/// an `nsis` entry the updater refuses (`lmgw_update::nsis::check_entry`),
/// or a copy that is not the installer's own (`can_self_install`). On Linux:
/// a host without `pkexec` and `dnf`.
pub fn cannot_install(info: &UpdateInfo) -> Option<String> {
    #[cfg(windows)]
    {
        let Some(entry) = info.manifest.installer() else {
            return Some(
                "This release has no Windows installer, so there is nothing to install yet."
                    .to_string(),
            );
        };
        if let Err(e) = lmgw_update::nsis::check_entry(entry) {
            return Some(format!(
                "This release's Windows installer is refused: {e}. Install the new version with \
                 the lmgw installer from the release."
            ));
        }
        let why = lmgw_update::nsis::can_self_install(PRODUCT).err()?;
        Some(format!(
            "This lmgw can't update itself: {why}. Install the new version with the lmgw \
             installer from the release."
        ))
    }
    #[cfg(not(windows))]
    {
        let _ = info;
        if lmgw_update::can_self_install() {
            return None;
        }
        Some(
            "This host is not RPM-based, so lmgw can't install it for you — self-install is \
             RPM-only on Linux. Update the way you installed it: rebuild from source (`git pull \
             && cargo tauri build`), or replace the AppImage."
                .to_string(),
        )
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    /// 0.6.0+2 offered to 0.6.0+1, with a Windows installer of this sha256 or none.
    fn info(nsis_sha256: Option<&str>) -> UpdateInfo {
        let artifact = |file: &str, sha256: &str| Artifact {
            file: file.into(),
            url: format!("https://example.org/{file}"),
            sha256: sha256.into(),
        };
        UpdateInfo {
            current: "0.6.0+1".into(),
            manifest: ReleaseManifest {
                version: "0.6.0+2".into(),
                notes: String::new(),
                pub_date: String::new(),
                rpm: artifact("lmgw.rpm", ""),
                nsis: nsis_sha256.map(|sha| artifact("lmgw_0.6.0+2_x64-setup.exe", sha)),
            },
        }
    }

    #[test]
    fn a_windows_copy_says_why_it_cannot_install() {
        assert_eq!(
            cannot_install(&info(None)).unwrap(),
            "This release has no Windows installer, so there is nothing to install yet."
        );
        let why = cannot_install(&info(Some(""))).unwrap();
        assert!(
            why.starts_with("This release's Windows installer is refused: its sha256 is empty."),
            "{why}"
        );
        // This test binary is no installer's copy.
        let why = cannot_install(&info(Some(&"ab".repeat(32)))).unwrap();
        assert!(why.starts_with("This lmgw can't update itself: "), "{why}");
    }
}
