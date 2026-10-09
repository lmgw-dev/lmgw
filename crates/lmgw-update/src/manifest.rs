//! `latest.json` and the version order it is compared in.

use serde::{Deserialize, Serialize};

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
/// private build made after release `MAJOR.MINOR.PATCH` (a private CI stamps
/// its pipeline number there), so `0.3.0+7` sorts after `0.3.0` and before
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
