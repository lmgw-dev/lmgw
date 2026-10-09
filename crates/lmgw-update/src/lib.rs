//! Self-update from a release feed: an app polls a small JSON manifest
//! (`latest.json`) that its CI publishes with every release, compares it to
//! the running build's version, and — on a newer one — downloads the RPM it
//! names and hands it to `pkexec dnf install`.
//!
//! - [`manifest`]: the manifest's types and the version order ([`is_newer`]);
//! - [`feed`]: where the manifest lives and how to authenticate there
//!   ([`Feed`]), the fetch, the check and the sha256-verified download;
//! - [`install`]: whether this host can install an RPM at all
//!   ([`can_self_install`]) and the install step ([`install_rpm`]).
//!
//! **Product-neutral.** Nothing here names an app: the feed URL, its token,
//! the version to compare against and every word shown to the user are the
//! caller's. Errors are typed ([`Error`], [`InstallError`]) and display as
//! the underlying failure, so a caller can show them as they are or word
//! them its own way.
//!
//! **The token stays on the feed's origin.** The feed's auth header goes only
//! on requests whose URL has the feed's scheme, host and port; the RPM URL in
//! the manifest is feed data and cannot steer it elsewhere. The crate cannot
//! change the caller's client, though, and reqwest strips only the standard
//! auth headers on a cross-host redirect, not `PRIVATE-TOKEN` or
//! `Deploy-Token`. A caller whose feed needs auth should therefore build its
//! client so it does not follow cross-origin redirects with the token (a
//! custom `redirect::Policy` that stops or refuses them).
//!
//! **The download path is the caller's.** [`download_rpm`] writes where it is
//! told and [`install_rpm`] installs what it is given. Pass a fixed basename
//! under a directory the app owns (e.g. `std::env::temp_dir()` joined with
//! `"<app>-update.rpm"`) — never a path derived from the manifest, whose
//! `rpm.file` comes from the feed.

pub mod feed;
pub mod install;
pub mod manifest;

pub use feed::{check, download_rpm, fetch_manifest, Error, Feed, FeedAuth};
pub use install::{can_self_install, install_rpm, InstallError};
pub use manifest::{is_newer, ReleaseManifest, RpmArtifact, UpdateInfo};
