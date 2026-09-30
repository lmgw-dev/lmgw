//! Background update check (§12): the app polls a small JSON manifest that CI
//! attaches to every GitHub release, compares it to the running build's
//! version, and — in the Tauri shell — prompts to download + `dnf install` the
//! new RPM.
//!
//! The fetch / compare / download logic lives here (not in the Tauri crate) so
//! it is unit-testable and reuses the shared `reqwest` client and the persisted
//! settings (feed token, enable flag).

use std::path::Path;

use futures::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

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

/// The RPM artifact a manifest points at.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpmArtifact {
    /// File name, e.g. `lmgw-0.1.42-1.x86_64.rpm`.
    pub file: String,
    /// Direct download URL (generic-registry API path).
    pub url: String,
    /// hex(sha256) of the RPM; verified after download when non-empty.
    #[serde(default)]
    pub sha256: String,
}

/// `latest.json` as published by CI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseManifest {
    pub version: String,
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub pub_date: String,
    pub rpm: RpmArtifact,
}

/// A pending update: the running version plus the newer manifest.
#[derive(Debug, Clone)]
pub struct UpdateInfo {
    pub current: String,
    pub manifest: ReleaseManifest,
}

/// Parse `MAJOR.MINOR.PATCH[+BUILD]` into comparable integers. `+BUILD` is a
/// private build made after release `MAJOR.MINOR.PATCH` (ci/build.sh stamps the
/// GitLab pipeline number there), so `0.3.0+7` sorts after `0.3.0` and before
/// `0.3.1`, as it does for rpm. A `-pre` suffix and non-numeric build metadata
/// are ignored. Unparseable input yields zeros so a malformed manifest can
/// never be reported as "newer".
fn parse_version(v: &str) -> (u64, u64, u64, u64) {
    let (core, build) = v.trim().split_once('+').unwrap_or((v.trim(), ""));
    let core = core.split('-').next().unwrap_or("");
    let mut it = core
        .split('.')
        .map(|p| p.trim().parse::<u64>().unwrap_or(0));
    (
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
        build.trim().parse::<u64>().unwrap_or(0),
    )
}

/// True iff `latest` is a strictly higher version than `current`.
pub fn is_newer(latest: &str, current: &str) -> bool {
    parse_version(latest) > parse_version(current)
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

/// Attach feed auth to a request: the user token via `PRIVATE-TOKEN`, else
/// the build's deploy token via `Deploy-Token`, else anonymous.
fn with_auth(state: &SharedState, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    match (user_token(state), BUILD_DEPLOY_TOKEN) {
        (Some(token), _) => req.header("PRIVATE-TOKEN", token),
        (None, Some(token)) => req.header("Deploy-Token", token),
        (None, None) => req,
    }
}

/// Fetch and parse the published manifest. Sends a `PRIVATE-TOKEN` header when a
/// feed token is configured (a private feed); anonymous otherwise.
pub async fn fetch_manifest(state: &SharedState) -> anyhow::Result<ReleaseManifest> {
    let req = with_auth(state, state.http.get(manifest_url()));
    let resp = req.send().await?.error_for_status()?;
    Ok(resp.json::<ReleaseManifest>().await?)
}

/// Check for a newer build. `Ok(None)` means up to date.
pub async fn check(state: &SharedState, current: &str) -> anyhow::Result<Option<UpdateInfo>> {
    let manifest = fetch_manifest(state).await?;
    Ok(is_newer(&manifest.version, current).then(|| UpdateInfo {
        current: current.to_string(),
        manifest,
    }))
}

/// Stream the RPM to `dest`, verifying the manifest sha256 when present. On a
/// hash mismatch the partial file is removed so a retry re-downloads cleanly.
pub async fn download_rpm(
    state: &SharedState,
    rpm: &RpmArtifact,
    dest: &Path,
) -> anyhow::Result<()> {
    let req = with_auth(state, state.http.get(&rpm.url));
    let resp = req.send().await?.error_for_status()?;
    let mut stream = resp.bytes_stream();

    let mut file = tokio::fs::File::create(dest).await?;
    let mut hasher = Sha256::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        hasher.update(&chunk);
        file.write_all(&chunk).await?;
    }
    file.flush().await?;

    if !rpm.sha256.is_empty() {
        let got = hex::encode(hasher.finalize());
        if !got.eq_ignore_ascii_case(rpm.sha256.trim()) {
            let _ = tokio::fs::remove_file(dest).await;
            anyhow::bail!("sha256 mismatch: expected {}, got {got}", rpm.sha256);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_compares_numerically_not_lexically() {
        assert!(is_newer("0.1.42", "0.1.0"));
        assert!(is_newer("0.1.10", "0.1.9")); // would fail under string compare
        assert!(is_newer("0.2.0", "0.1.99"));
        assert!(is_newer("1.0.0", "0.9.9"));
    }

    #[test]
    fn equal_or_older_is_not_newer() {
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.1.5"));
        assert!(!is_newer("0.1.0", "0.2.0"));
    }

    #[test]
    fn suffixes_are_ignored_and_bad_input_is_not_newer() {
        assert!(!is_newer("0.1.0-dev.3", "0.1.0"));
        assert!(!is_newer("garbage", "0.1.0"));
        assert!(is_newer("0.1.5+abc", "0.1.0"));
        assert!(!is_newer("0.1.0+abc", "0.1.0"));
    }

    #[test]
    fn private_builds_sort_between_releases() {
        assert!(is_newer("0.3.0+1", "0.3.0"));
        assert!(is_newer("0.3.0+107", "0.3.0+99")); // numeric, not lexical
        assert!(is_newer("0.3.1", "0.3.0+500"));
        assert!(!is_newer("0.3.0", "0.3.0+5"));
        assert!(!is_newer("0.3.0+5", "0.3.0+5"));
        assert!(is_newer("0.3.0+100", "0.2.99")); // the old counter scheme
    }

    #[test]
    fn manifest_parses_with_optional_fields() {
        let json = r#"{
            "version": "0.1.42",
            "rpm": { "file": "lmgw-0.1.42-1.x86_64.rpm",
                     "url": "https://example/lmgw-0.1.42-1.x86_64.rpm" }
        }"#;
        let m: ReleaseManifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.version, "0.1.42");
        assert_eq!(m.rpm.sha256, "");
        assert!(m.notes.is_empty());
    }
}
