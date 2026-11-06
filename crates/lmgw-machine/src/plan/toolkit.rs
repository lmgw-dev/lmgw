//! Whose `nvidia-ctk` this is (machine-check design §1.2, D24): every
//! toolkit decision keys on who owns `/usr/bin/nvidia-ctk` (`rpm -qf`),
//! never on a file's presence alone or on `rpm -q` of a package name.

use crate::os::OsRelease;
use crate::rpm::{Owner, Package};

use super::embedded::{NVIDIA_KEY_ID, RETIRED_TOOLKIT, TOOLKIT_BASE};
use super::sections::Provider;

/// The toolkit, by the owner of `/usr/bin/nvidia-ctk`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Toolkit {
    /// The file is not there: the toolkit is missing.
    Absent,
    /// NVIDIA's `nvidia-container-toolkit-base`, its header signed by
    /// NVIDIA's key.
    Nvidia(Package),
    /// A package the provider table names for this system: updates come with
    /// the system's.
    Distribution(Package),
    /// Fedora's retired Go package, which no longer gets updates.
    Retired(Package),
    /// Any other package, or NVIDIA's package name signed by another key or
    /// not at all: used as it is, never replaced.
    Other(Vec<Package>),
    /// The file is there and no package owns it: put there by hand.
    NotOwned,
    /// The file is there and this system has no rpm to ask.
    NoRpm,
    /// rpm could not answer; why.
    Unknown(String),
}

impl Toolkit {
    /// Judge `owner` (`rpm -qf /usr/bin/nvidia-ctk`) on the system `os`
    /// describes, with `providers` as the provider table.
    pub fn of(
        owner: Result<Owner, String>,
        os: Option<&OsRelease>,
        providers: &[Provider],
    ) -> Self {
        let pkgs = match owner {
            Ok(Owner::Absent) => return Self::Absent,
            Ok(Owner::NotOwned) => return Self::NotOwned,
            Ok(Owner::NoRpm) => return Self::NoRpm,
            Ok(Owner::Packages(p)) if p.is_empty() => {
                return Self::Unknown("rpm -qf named no package".into())
            }
            Ok(Owner::Packages(p)) => p,
            Err(e) => return Self::Unknown(e),
        };
        // One owner is the normal case; several (a file two packages share)
        // are judged by the first that lmgw knows, else as "other".
        for p in &pkgs {
            if p.name == TOOLKIT_BASE && p.sig_key_id.as_deref() == Some(NVIDIA_KEY_ID) {
                return Self::Nvidia(p.clone());
            }
            if p.name == RETIRED_TOOLKIT {
                return Self::Retired(p.clone());
            }
            if os.is_some_and(|os| {
                providers
                    .iter()
                    .any(|r| r.serves(os) && r.package == p.name)
            }) {
                return Self::Distribution(p.clone());
            }
        }
        Self::Other(pkgs)
    }

    /// The package that owns the file, when one does.
    pub fn package(&self) -> Option<&Package> {
        match self {
            Self::Nvidia(p) | Self::Distribution(p) | Self::Retired(p) => Some(p),
            Self::Other(p) => p.first(),
            _ => None,
        }
    }

    /// The owner as the detail names it: `nvidia-container-toolkit-base-1.20.1-1.x86_64`.
    pub fn owner_text(&self) -> String {
        match self {
            Self::Absent => "none (/usr/bin/nvidia-ctk is not there)".into(),
            Self::NotOwned => "no package (/usr/bin/nvidia-ctk was put there by hand)".into(),
            Self::NoRpm => "unknown (this system has no rpm to ask)".into(),
            Self::Unknown(why) => format!("unknown ({why})"),
            Self::Other(pkgs) => pkgs
                .iter()
                .map(|p| match &p.sig_key_id {
                    Some(id) => format!("{} (signed by key {id})", p.nvra()),
                    None => format!("{} (unsigned)", p.nvra()),
                })
                .collect::<Vec<_>>()
                .join(", "),
            Self::Nvidia(p) | Self::Distribution(p) | Self::Retired(p) => p.nvra(),
        }
    }
}
