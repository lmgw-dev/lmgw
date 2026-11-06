//! Where the toolkit comes from (machine-check design §3.2): the provider
//! table, the repository sections that count, lmgw's own two files, and the
//! one section the plan's dnf steps are scoped to.

use std::path::Path;

use crate::facts::{Facts, Kind};
use crate::os::OsRelease;
use crate::repos::{glob_match, RepoSection, Repos};

use super::embedded::{NVIDIA_STABLE_PREFIX, OWN_KEYS, REPO_ID, REPO_INCLUDEPKGS, REPO_MARKER};

/// A repository a distribution ships the toolkit in: on the system `os_id`
/// at `version_id`, the section `repo_id` serves `package`, which owns
/// `/usr/bin/nvidia-ctk`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Provider {
    pub os_id: &'static str,
    pub version_id: &'static str,
    pub repo_id: &'static str,
    pub package: &'static str,
}

impl Provider {
    /// Does this row apply to the system `os` describes?
    pub fn serves(&self, os: &OsRelease) -> bool {
        os.id == self.os_id && os.version_id.as_deref() == Some(self.version_id)
    }
}

/// The provider table: empty. No distribution repository on Fedora 44 ships
/// the toolkit (checked 2026-11-03: not `fedora`, `updates` or any
/// `rpmfusion*` repo; Fedora's own `nvidia-container-toolkit` is in package
/// review, bug 2450007). When one lands, a reviewed lmgw change adds its row,
/// and the plan installs from it without NVIDIA's repository or key.
pub const PROVIDERS: &[Provider] = &[];

/// One of lmgw's own two files (its repository file, NVIDIA's key), as it is
/// on the machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnFile {
    /// Not there: lmgw may create it.
    Absent,
    /// lmgw's, with the text this lmgw writes.
    Current,
    /// lmgw's, with another text (an older lmgw's): lmgw replaces it.
    OlderOwn,
    /// Not lmgw's (or not one lmgw can tell is its own): never overwritten.
    NotOwn(String),
}

impl OwnFile {
    /// lmgw may write the file: it is not there, or it is lmgw's own.
    pub fn writable(&self) -> bool {
        !matches!(self, Self::NotOwn(_))
    }
}

/// Read `path` as root and the user both can: a root-owned, world-readable
/// regular file, judged lmgw's by `is_own` and current by `current`.
fn own_file(
    f: &dyn Facts,
    path: &str,
    is_own: impl Fn(&str) -> bool,
    current: impl Fn(&str) -> bool,
) -> OwnFile {
    let meta = match f.lstat(Path::new(path)) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return OwnFile::Absent,
        Err(e) => return OwnFile::NotOwn(format!("{path} could not be read: {e}")),
    };
    if meta.kind == Kind::Symlink {
        return OwnFile::NotOwn(format!("{path} is a symbolic link"));
    }
    if meta.kind != Kind::File {
        return OwnFile::NotOwn(format!("{path} is not a regular file"));
    }
    if meta.uid != 0 {
        return OwnFile::NotOwn(format!("{path} belongs to user {}, not to root", meta.uid));
    }
    if meta.mode & 0o004 == 0 {
        return OwnFile::NotOwn(format!("{path} is not readable by every user"));
    }
    let text = match f.read(Path::new(path)) {
        Ok(b) => String::from_utf8_lossy(&b).into_owned(),
        Err(e) => return OwnFile::NotOwn(format!("{path} could not be read: {e}")),
    };
    if !is_own(&text) {
        return OwnFile::NotOwn(format!("{path} was not written by lmgw"));
    }
    if current(&text) {
        OwnFile::Current
    } else {
        OwnFile::OlderOwn
    }
}

/// lmgw's repository file ([`super::embedded::REPO_FILE`]): lmgw's when its
/// first line is [`REPO_MARKER`].
pub fn repo_file(f: &dyn Facts) -> OwnFile {
    own_file(
        f,
        super::embedded::REPO_FILE,
        |t| t.lines().next().is_some_and(|l| l.starts_with(REPO_MARKER)),
        |t| t == super::embedded::REPO_TEXT,
    )
}

/// lmgw's copy of NVIDIA's key ([`super::embedded::KEY_FILE`]): lmgw's when
/// it holds one of [`OWN_KEYS`].
pub fn key_file(f: &dyn Facts) -> OwnFile {
    own_file(
        f,
        super::embedded::KEY_FILE,
        |t| OWN_KEYS.contains(&t),
        |t| t == super::embedded::NVIDIA_KEY,
    )
}

/// Who wrote a section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// lmgw's own section ([`REPO_ID`]).
    Lmgw,
    /// The person's NVIDIA section (they followed NVIDIA's docs).
    Person,
    /// A distribution repository from the provider table.
    Provider,
}

/// The section the plan's dnf steps are scoped to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    pub id: String,
    pub origin: Origin,
    /// After dnf5's overrides.
    pub enabled: bool,
    /// The package the plan installs from it.
    pub package: String,
    /// Its `includepkgs` once the plan's steps ran (empty: no filter): the
    /// one read after the overrides, and for lmgw's own section the one its
    /// text has, which S1 writes when an older one is there.
    pub includepkgs: Vec<String>,
}

impl Section {
    /// Does its `includepkgs` let `package` through?
    pub fn lets_through(&self, package: &str) -> bool {
        self.includepkgs.is_empty() || self.includepkgs.iter().any(|g| glob_match(g, package))
    }
}

/// lmgw's own section as [`super::embedded::REPO_TEXT`] defines it, for a
/// plan that adds it.
pub fn own_section() -> Section {
    Section {
        id: REPO_ID.into(),
        origin: Origin::Lmgw,
        enabled: true,
        package: super::embedded::TOOLKIT_BASE.into(),
        includepkgs: REPO_INCLUDEPKGS.iter().map(|p| p.to_string()).collect(),
    }
}

/// Is `s` a section of NVIDIA's stable channel that can serve `package`
/// (its `includepkgs`, when it has one, lets it through)?
fn nvidia_stable(s: &RepoSection, package: &str) -> bool {
    s.baseurl
        .iter()
        .any(|u| u.starts_with(NVIDIA_STABLE_PREFIX))
        && (s.includepkgs.is_empty() || s.includepkgs.iter().any(|g| glob_match(g, package)))
}

/// The section the toolkit comes from, in this order: an enabled repository
/// of the provider table; the person's enabled NVIDIA section; lmgw's own
/// enabled section; the person's switched-off NVIDIA section; lmgw's own
/// switched-off one. `None`: there is no NVIDIA section yet, and the plan adds
/// lmgw's own.
pub fn choose(
    repos: &Repos,
    os: Option<&OsRelease>,
    providers: &[Provider],
    package: &str,
) -> Option<Section> {
    if let Some(os) = os {
        for p in providers.iter().filter(|p| p.serves(os)) {
            if let Some(s) = repos.get(p.repo_id).filter(|s| s.enabled) {
                return Some(Section {
                    id: s.id.clone(),
                    origin: Origin::Provider,
                    enabled: true,
                    package: p.package.to_string(),
                    includepkgs: s.includepkgs.clone(),
                });
            }
        }
    }
    let nvidia: Vec<&RepoSection> = repos
        .sections
        .iter()
        .filter(|s| nvidia_stable(s, package))
        .collect();
    let pick = |enabled: bool, own: bool| {
        nvidia
            .iter()
            .find(|s| s.enabled == enabled && (s.id == REPO_ID) == own)
    };
    let (s, origin) = [(true, false), (true, true), (false, false), (false, true)]
        .into_iter()
        .find_map(|(enabled, own)| {
            pick(enabled, own).map(|s| (*s, if own { Origin::Lmgw } else { Origin::Person }))
        })?;
    Some(Section {
        id: s.id.clone(),
        origin,
        enabled: s.enabled,
        package: package.to_string(),
        includepkgs: match origin {
            Origin::Lmgw => REPO_INCLUDEPKGS.iter().map(|p| p.to_string()).collect(),
            _ => s.includepkgs.clone(),
        },
    })
}
