//! Self-update from a release feed: an app polls a small JSON manifest
//! (`latest.json`) that its CI publishes with every release, compares it to
//! the running build's version, and — on a newer one — downloads the
//! installer it names for this platform ([`ReleaseManifest::installer`]):
//! on Linux the RPM, handed to `pkexec dnf install` (both by absolute path,
//! see [`install`]); on Windows the per-user NSIS installer, started silently
//! for the current user ([`nsis`]).
//!
//! - [`manifest`]: the manifest's types and the version order ([`is_newer`]);
//! - [`feed`]: where the manifest lives and how to authenticate there
//!   ([`Feed`]), the fetch, the check and the sha256-verified download;
//! - [`install`]: whether this host can install an RPM at all
//!   ([`can_self_install`]) and the install step ([`install_rpm`]);
//! - [`nsis`]: whether the running program is the per-user installer's copy
//!   and the installer's start, on Windows.
//!
//! **Product-neutral.** Nothing here names an app: the feed URL, its token,
//! the version to compare against and every word shown to the user are the
//! caller's. Errors are typed ([`Error`], [`InstallError`]) and display as
//! the underlying failure, so a caller can show them as they are or word
//! them its own way.
//!
//! **The token stays on the feed's origin.** The feed's auth header goes only
//! on requests whose URL has the feed's scheme, host and port; an installer's
//! URL in the manifest is feed data and cannot steer it elsewhere. The crate cannot
//! change the caller's client, though, and reqwest strips only the standard
//! auth headers on a cross-host redirect, not `PRIVATE-TOKEN` or
//! `Deploy-Token`. A caller whose feed needs auth should therefore build its
//! client so it does not follow cross-origin redirects with the token (a
//! custom `redirect::Policy` that stops or refuses them).
//!
//! **The download path is the caller's.** [`download`] writes where it is
//! told, and [`install_rpm`] and [`nsis::spawn_installer`] run what they are given. Pass a fixed basename
//! under a directory only the user can write ([`private_dir`]: `<app>` under
//! `$XDG_RUNTIME_DIR`, 0700; on Windows under the user's own temp dir, with a
//! private descriptor) — not a fixed name in a shared `/tmp`, and never a
//! path derived from the manifest, whose `file` names come from the feed.
//! [`download`] creates the file fresh (`O_EXCL | O_NOFOLLOW`, 0600; on
//! Windows `CREATE_NEW`, no link followed, a private descriptor).

pub mod feed;
pub mod install;
pub mod manifest;
pub mod nsis;
pub mod private;

pub use feed::{check, download, fetch_manifest, Error, Feed, FeedAuth};
pub use install::{can_self_install, install_rpm, InstallError};
pub use manifest::{is_newer, Artifact, ReleaseManifest, UpdateInfo};
pub use private::private_dir;
