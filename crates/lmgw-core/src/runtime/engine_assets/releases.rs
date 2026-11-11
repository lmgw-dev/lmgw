//! The upstream release check (Windows design §4.2): whether an engine's
//! project has published a newer Windows release than lmgw's pin. It runs
//! in the same check as the registry images' (`backends::updates::check_all`,
//! the schedule and **Check now**), and only where this platform has pins.
//!
//! One `GET /repos/<repo>/releases` per pinned project. The newest release
//! that is not a draft and carries a Windows asset is the one compared:
//! llama.cpp publishes its builds (`b12244`) as pre-releases and its
//! `v0.9.0` releases without binaries, so "latest release" is not the
//! answer there. An update is a different tag published on the pin's day or
//! later. lmgw runs its pins only; a newer release becomes runnable with the
//! lmgw release that pins it (installing an unpinned release is not built
//! yet).

use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::pins::PlatformPins;

/// GitHub's API.
pub const API: &str = "https://api.github.com";

/// What the check saw of one project, stored with the image observations
/// (`backends:update_check`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReleaseObservation {
    pub checked_at: String,
    pub repo: String,
    /// The pin it was compared with.
    pub pinned_tag: String,
    pub latest_tag: Option<String>,
    /// RFC 3339.
    pub latest_published: Option<String>,
    pub update_available: bool,
    pub error: Option<String>,
}

/// One release as GitHub's list gives it.
#[derive(Debug, Clone, Deserialize)]
pub struct GhRelease {
    pub tag_name: String,
    #[serde(default)]
    pub published_at: Option<String>,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub assets: Vec<GhAsset>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GhAsset {
    pub name: String,
}

/// An asset of a Windows build: `…-bin-win-…`, `…-windows-…`, or an MSI.
fn windows_asset(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.contains("-win-") || n.contains("windows") || n.ends_with(".msi")
}

/// The newest non-draft release with a Windows asset (GitHub lists newest
/// first).
pub fn newest_windows(list: &[GhRelease]) -> Option<&GhRelease> {
    list.iter()
        .filter(|r| !r.draft && r.assets.iter().any(|a| windows_asset(&a.name)))
        .max_by(|a, b| a.published_at.cmp(&b.published_at))
}

/// The observation of `repo` against the pin `(tag, published day)`.
pub fn evaluate(
    repo: &str,
    pinned_tag: &str,
    pinned_day: &str,
    list: &[GhRelease],
    checked_at: &str,
) -> ReleaseObservation {
    let mut obs = ReleaseObservation {
        checked_at: checked_at.to_string(),
        repo: repo.to_string(),
        pinned_tag: pinned_tag.to_string(),
        ..ReleaseObservation::default()
    };
    match newest_windows(list) {
        Some(r) => {
            let day = r
                .published_at
                .as_deref()
                .and_then(|p| p.get(..10))
                .unwrap_or("");
            obs.update_available = r.tag_name != pinned_tag && day >= pinned_day;
            obs.latest_tag = Some(r.tag_name.clone());
            obs.latest_published = r.published_at.clone();
        }
        None => {
            obs.error = Some(format!(
                "none of {repo}'s {} newest releases has a Windows asset",
                list.len()
            ))
        }
    }
    obs
}

/// Ask GitHub about every pinned project of `pins`, each within `timeout`.
/// Keyed by engine (`llama`, `sdcpp`, `audio`, `espeak`).
pub async fn observe(
    api: &str,
    pins: &PlatformPins,
    timeout: Duration,
    checked_at: &str,
) -> BTreeMap<String, ReleaseObservation> {
    let http = reqwest::Client::builder()
        .user_agent(concat!("lmgw/", env!("CARGO_PKG_VERSION")))
        .timeout(timeout)
        .build()
        .unwrap_or_default();
    let mut targets: Vec<(&str, &str, &str, &str)> = pins
        .engines()
        .iter()
        .map(|(e, p)| (*e, p.repo.as_str(), p.tag.as_str(), p.published.as_str()))
        .collect();
    targets.push((
        "espeak",
        &pins.espeak.repo,
        &pins.espeak.tag,
        &pins.espeak.published,
    ));
    let mut out = BTreeMap::new();
    for (engine, repo, tag, day) in targets {
        let url = format!(
            "{}/repos/{repo}/releases?per_page=20",
            api.trim_end_matches('/')
        );
        let answer = async {
            let resp = http
                .get(&url)
                .header("Accept", "application/vnd.github+json")
                .send()
                .await
                .map_err(|e| e.to_string())?;
            if !resp.status().is_success() {
                return Err(format!("{url} answered {}", resp.status()));
            }
            resp.json::<Vec<GhRelease>>()
                .await
                .map_err(|e| format!("{url}: {e}"))
        }
        .await;
        let obs = match answer {
            Ok(list) => evaluate(repo, tag, day, &list, checked_at),
            Err(e) => ReleaseObservation {
                checked_at: checked_at.to_string(),
                repo: repo.to_string(),
                pinned_tag: tag.to_string(),
                error: Some(format!("check failed: {e}")),
                ..ReleaseObservation::default()
            },
        };
        out.insert(engine.to_string(), obs);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rel(tag: &str, at: &str, assets: &[&str]) -> GhRelease {
        GhRelease {
            tag_name: tag.into(),
            published_at: Some(at.into()),
            draft: false,
            assets: assets
                .iter()
                .map(|n| GhAsset { name: (*n).into() })
                .collect(),
        }
    }

    #[test]
    fn the_newest_windows_release_is_compared_with_the_pin() {
        let list = vec![
            rel("v0.9.0", "2026-11-12T00:00:00Z", &["nightly-tag.txt"]),
            rel(
                "b12250",
                "2026-11-11T08:00:00Z",
                &["llama-b12250-bin-win-cpu-x64.zip"],
            ),
            rel(
                "b12244",
                "2026-11-10T07:25:25Z",
                &["llama-b12244-bin-win-cpu-x64.zip"],
            ),
        ];
        let o = evaluate("ggml-org/llama.cpp", "b12244", "2026-11-10", &list, "t");
        assert!(o.update_available);
        assert_eq!(o.latest_tag.as_deref(), Some("b12250"));
        let o = evaluate("ggml-org/llama.cpp", "b12250", "2026-11-11", &list, "t");
        assert!(!o.update_available);
        let o = evaluate("x/y", "1", "2026-01-01", &list[..1], "t");
        assert!(o.error.is_some() && !o.update_available);
        let msi = vec![rel("1.52.0", "2024-12-12T13:47:27Z", &["espeak-ng.msi"])];
        assert!(
            !evaluate("espeak-ng/espeak-ng", "1.52.0", "2024-12-12", &msi, "t").update_available
        );
    }
}
