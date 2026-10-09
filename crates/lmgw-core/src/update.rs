//! Background update check (§12): the app polls a small JSON manifest that CI
//! attaches to every GitHub release, compares it to the running build's
//! version, and — in the Tauri shell — prompts to download + `dnf install` the
//! new RPM.
//!
//! The mechanism — manifest, version order, sha256-verified download, the
//! install step — is the `lmgw-update` crate's, shared with the desktop
//! clients. What is lmgw's lives here (not in the Tauri crate): which feed
//! this build polls, its auth (build token, env, the persisted Settings
//! field), and the shared `reqwest` client the requests go through.

use std::path::Path;

pub use lmgw_update::{is_newer, Feed, FeedAuth, ReleaseManifest, RpmArtifact, UpdateInfo};

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

/// Stream the RPM to `dest`, verifying the manifest sha256 when present. On a
/// hash mismatch the partial file is removed so a retry re-downloads cleanly.
pub async fn download_rpm(
    state: &SharedState,
    rpm: &RpmArtifact,
    dest: &Path,
) -> anyhow::Result<()> {
    Ok(lmgw_update::download_rpm(&state.http, &feed(state), rpm, dest).await?)
}
