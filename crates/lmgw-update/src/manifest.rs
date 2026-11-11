//! `latest.json` and the version order it is compared in.

use serde::{Deserialize, Serialize};

/// A file a manifest points at: the RPM, or the Windows installer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Artifact {
    /// File name, e.g. `lmgw-0.1.42-1.x86_64.rpm` or `lmgw_0.1.42_x64-setup.exe`.
    pub file: String,
    /// Direct download URL (generic-registry API path, or a release asset).
    pub url: String,
    /// hex(sha256) of the file; verified after download when non-empty.
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
    /// The RPM, which every release has.
    pub rpm: Artifact,
    /// The per-user NSIS installer for Windows x86-64, when the release has one. A release
    /// whose Windows build failed has none, and a client that does not know the key ignores it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nsis: Option<Artifact>,
}

impl ReleaseManifest {
    /// The installer this platform runs: the NSIS installer on Windows, `None` when the release
    /// has none; the RPM everywhere else.
    pub fn installer(&self) -> Option<&Artifact> {
        if cfg!(windows) {
            self.nsis.as_ref()
        } else {
            Some(&self.rpm)
        }
    }
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
