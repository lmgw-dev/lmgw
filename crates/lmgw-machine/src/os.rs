//! Which system this is (machine-check design §3.7): `/etc/os-release`, and
//! whether it is image-based (`/run/ostree-booted`).

use std::path::Path;

use crate::facts::{exists, read_text, Facts};

/// Where os-release is, in the order the spec says to look.
pub const OS_RELEASE: [&str; 2] = ["/etc/os-release", "/usr/lib/os-release"];

/// Present on an rpm-ostree system (Silverblue, Kinoite, Bazzite).
pub const OSTREE_BOOTED: &str = "/run/ostree-booted";

/// The fields of os-release the check reads.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OsRelease {
    /// `ID`, e.g. `fedora`.
    pub id: String,
    /// `ID_LIKE`, split on whitespace.
    pub id_like: Vec<String>,
    /// `VERSION_ID`, e.g. `44`.
    pub version_id: Option<String>,
    /// `PRETTY_NAME`, for the detail.
    pub pretty_name: Option<String>,
}

impl OsRelease {
    /// Is this the system `id`, or one that names it in `ID_LIKE`?
    pub fn is_like(&self, id: &str) -> bool {
        self.id == id || self.id_like.iter().any(|l| l == id)
    }
}

/// One os-release value: unquoted, or in single or double quotes with the
/// shell escapes os-release allows (`\"`, `\\`, `` \` ``, `\$`).
fn unquote(v: &str) -> String {
    let v = v.trim();
    let quoted = (v.starts_with('"') && v.ends_with('"') && v.len() >= 2)
        || (v.starts_with('\'') && v.ends_with('\'') && v.len() >= 2);
    if !quoted {
        return v.to_string();
    }
    let inner = &v[1..v.len() - 1];
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(n) = chars.next() {
                out.push(n);
            }
        } else {
            out.push(c);
        }
    }
    out
}

pub fn parse_os_release(text: &str) -> OsRelease {
    let mut os = OsRelease::default();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let v = unquote(v);
        match k.trim() {
            "ID" => os.id = v,
            "ID_LIKE" => os.id_like = v.split_whitespace().map(str::to_string).collect(),
            "VERSION_ID" => os.version_id = Some(v),
            "PRETTY_NAME" => os.pretty_name = Some(v),
            _ => {}
        }
    }
    os
}

/// os-release from the first of [`OS_RELEASE`] that is there; `None` when
/// neither is.
pub fn os_release(f: &dyn Facts) -> Option<OsRelease> {
    OS_RELEASE
        .iter()
        .find_map(|p| read_text(f, Path::new(p)).ok())
        .map(|t| parse_os_release(&t))
}

/// Is this an image-based (rpm-ostree) system?
pub fn ostree_booted(f: &dyn Facts) -> bool {
    exists(f, Path::new(OSTREE_BOOTED))
}
