//! What else a probe read, beyond the tool's own binary (design §4.3): the
//! files and lookups whose change makes the probe stale. A tool's version
//! run is stamped by the binary alone, but a tesseract language pack is a
//! file in the tessdata directory, and LibreOffice's parts are launchers
//! beside `soffice`: installing or removing one changes the answer while
//! the main binary stays what it was. The probe stores these, and
//! [`Watched::now`] reads them again for the comparison
//! ([`super::tools`]), without starting a process.

use std::ffi::OsStr;
use std::path::PathBuf;

use super::probe::{lookup, stamp, Located, Stamp};

/// One thing a probe's extras depend on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Watched {
    /// A file or directory: its stamp, `None` when it is not there
    /// (a directory's mtime moves when a file is added or removed in it).
    File { path: PathBuf, stamp: Option<Stamp> },
    /// A binary looked up by name on `PATH` and the tool's install
    /// directories (poppler's `pdftoppm` beside `pdftotext`), `None` when
    /// it is on none.
    Binary { name: String, at: Option<Located> },
}

impl Watched {
    pub fn file(path: PathBuf) -> Self {
        let stamp = stamp(&path);
        Self::File { path, stamp }
    }

    pub fn binary(name: &str, search_path: &OsStr, dirs: &[&str]) -> Self {
        Self::Binary {
            name: name.to_string(),
            at: lookup(name, search_path, dirs),
        }
    }

    /// The same thing as it is now.
    pub fn now(&self, search_path: &OsStr, dirs: &[&str]) -> Self {
        match self {
            Self::File { path, .. } => Self::file(path.clone()),
            Self::Binary { name, .. } => Self::binary(name, search_path, dirs),
        }
    }
}
