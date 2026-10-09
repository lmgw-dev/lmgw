//! The feed: where the manifest lives, how to authenticate there, and the
//! fetch / check / download against it.

use std::fmt;
use std::path::Path;

use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::manifest::{is_newer, ReleaseManifest, RpmArtifact, UpdateInfo};

/// An update feed: the manifest's URL and the auth every request to its
/// origin (the manifest, and the RPM it names when that is on the same
/// origin) carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Feed {
    pub url: String,
    pub auth: FeedAuth,
}

/// Auth for a private feed, in the headers a GitLab package registry takes.
/// A public feed needs none.
#[derive(Clone, Default, PartialEq, Eq)]
pub enum FeedAuth {
    #[default]
    None,
    /// A user's token, sent as `PRIVATE-TOKEN`.
    PrivateToken(String),
    /// A read-only deploy token, sent as `Deploy-Token`.
    DeployToken(String),
}

// By hand, so a token never reaches a log line through `{:?}`.
impl fmt::Debug for FeedAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            FeedAuth::None => "None",
            FeedAuth::PrivateToken(_) => "PrivateToken(..)",
            FeedAuth::DeployToken(_) => "DeployToken(..)",
        })
    }
}

impl FeedAuth {
    /// Attach the auth header, if any, to a GET of `target`, but only when
    /// `target` has the same origin (scheme, host, port) as the feed's own
    /// `feed_url`. The RPM URL is data from the feed and must not steer the
    /// token to another host. An unparseable URL gets no header.
    fn apply(
        &self,
        req: reqwest::RequestBuilder,
        feed_url: &str,
        target: &str,
    ) -> reqwest::RequestBuilder {
        if !same_origin(feed_url, target) {
            return req;
        }
        match self {
            FeedAuth::None => req,
            FeedAuth::PrivateToken(token) => req.header("PRIVATE-TOKEN", token),
            FeedAuth::DeployToken(token) => req.header("Deploy-Token", token),
        }
    }
}

fn same_origin(a: &str, b: &str) -> bool {
    let (Ok(a), Ok(b)) = (reqwest::Url::parse(a), reqwest::Url::parse(b)) else {
        return false;
    };
    a.scheme() == b.scheme()
        && a.host_str().is_some()
        && a.host_str() == b.host_str()
        && a.port_or_known_default() == b.port_or_known_default()
}

/// Why a fetch, check or download failed. Each variant displays as the
/// failure itself (an HTTP error displays as reqwest's message), so `{e}`
/// reads the same as the error underneath.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// The request, its status, or the manifest's JSON.
    Http(reqwest::Error),
    /// Writing the download.
    Io(std::io::Error),
    /// The downloaded RPM's hash is not the manifest's; the file is removed.
    Sha256Mismatch { expected: String, got: String },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Http(e) => e.fmt(f),
            Error::Io(e) => e.fmt(f),
            Error::Sha256Mismatch { expected, got } => {
                write!(f, "sha256 mismatch: expected {expected}, got {got}")
            }
        }
    }
}

// Transparent: the source is the wrapped error's own, so a chain printed
// with its sources does not show the same message twice.
impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Http(e) => e.source(),
            Error::Io(e) => e.source(),
            Error::Sha256Mismatch { .. } => None,
        }
    }
}

impl From<reqwest::Error> for Error {
    fn from(e: reqwest::Error) -> Self {
        Error::Http(e)
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

/// Fetch and parse the published manifest, with the feed's auth.
pub async fn fetch_manifest(http: &reqwest::Client, feed: &Feed) -> Result<ReleaseManifest, Error> {
    let req = feed.auth.apply(http.get(&feed.url), &feed.url, &feed.url);
    let resp = req.send().await?.error_for_status()?;
    Ok(resp.json::<ReleaseManifest>().await?)
}

/// Check for a newer build than `current`. `Ok(None)` means up to date.
pub async fn check(
    http: &reqwest::Client,
    feed: &Feed,
    current: &str,
) -> Result<Option<UpdateInfo>, Error> {
    let manifest = fetch_manifest(http, feed).await?;
    Ok(is_newer(&manifest.version, current).then(|| UpdateInfo {
        current: current.to_string(),
        manifest,
    }))
}

/// Stream the RPM to `dest` (with the feed's auth when `rpm.url` is on the
/// feed's origin, without it otherwise), verifying the manifest
/// sha256 when present. On a hash mismatch the partial file is removed so a
/// retry re-downloads cleanly.
///
/// `dest` is the caller's fixed path, never one built from `rpm.file` (see
/// the crate docs).
pub async fn download_rpm(
    http: &reqwest::Client,
    feed: &Feed,
    rpm: &RpmArtifact,
    dest: &Path,
) -> Result<(), Error> {
    let req = feed.auth.apply(http.get(&rpm.url), &feed.url, &rpm.url);
    let mut resp = req.send().await?.error_for_status()?;

    let mut file = tokio::fs::File::create(dest).await?;
    let mut hasher = Sha256::new();
    while let Some(chunk) = resp.chunk().await? {
        hasher.update(&chunk);
        file.write_all(&chunk).await?;
    }
    file.flush().await?;

    if !rpm.sha256.is_empty() {
        let got = hex::encode(hasher.finalize());
        if !got.eq_ignore_ascii_case(rpm.sha256.trim()) {
            let _ = tokio::fs::remove_file(dest).await;
            return Err(Error::Sha256Mismatch {
                expected: rpm.sha256.clone(),
                got,
            });
        }
    }
    Ok(())
}
