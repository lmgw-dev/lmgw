//! The rpm database (machine-check design §1.2, §3.2): who owns a file, what
//! an installed package is and who signed its header, the keys rpm trusts,
//! and when the database last changed.
//!
//! Every query runs `rpm` by its absolute path in the C locale ([`Facts::run`]
//! sets it), so the signature line reads the same on a German desktop.

use std::path::{Path, PathBuf};

use crate::facts::{exists, CmdOut, Facts};

pub const RPM: &str = "/usr/bin/rpm";
pub const RPMKEYS: &str = "/usr/bin/rpmkeys";

/// Where the rpm database lives: Fedora 36 and later, then the older place
/// (a link to the first on current Fedora).
pub const RPMDB_DIRS: [&str; 2] = ["/usr/lib/sysimage/rpm", "/var/lib/rpm"];

/// The files whose modification time says a transaction wrote the database:
/// sqlite's main file and its write-ahead log, and the older backends' files.
/// Not `rpmdb.sqlite-shm`: sqlite's shared-memory index changes with reads
/// too, such as the check's own `rpm -q` (machine-check design §3.6.1).
pub const RPMDB_WRITE_FILES: [&str; 4] = [
    "rpmdb.sqlite",
    "rpmdb.sqlite-wal",
    "Packages",
    "Packages.db",
];

/// The query format every package query prints, one line per package, tab
/// separated: name, epoch, version, release, arch, and the header signature
/// rpm reports through both of its signature tags.
const QF: &str = "%{NAME}\t%{EPOCH}\t%{VERSION}\t%{RELEASE}\t%{ARCH}\t%{RSAHEADER:pgpsig}\t%{DSAHEADER:pgpsig}\n";

/// One installed package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Package {
    pub name: String,
    pub epoch: Option<String>,
    pub version: String,
    pub release: String,
    pub arch: String,
    /// The key ID (16 hex digits, lowercase) that signed the header, `None`
    /// for an unsigned package.
    pub sig_key_id: Option<String>,
}

impl Package {
    /// `name-version-release.arch`, as rpm prints it.
    pub fn nvra(&self) -> String {
        format!(
            "{}-{}-{}.{}",
            self.name, self.version, self.release, self.arch
        )
    }
}

/// The key ID a `pgpsig` rendering names: `RSA/SHA512, <date>, Key ID
/// ddcae044f796ecb0` → `ddcae044f796ecb0`. `(none)` and anything without a
/// key ID give `None`.
pub fn key_id_of(sig: &str) -> Option<String> {
    let at = sig.find("Key ID ")?;
    let id: String = sig[at + "Key ID ".len()..]
        .chars()
        .take_while(|c| c.is_ascii_hexdigit())
        .collect();
    (!id.is_empty()).then(|| id.to_ascii_lowercase())
}

/// Parse the lines [`QF`] prints.
pub fn parse_packages(stdout: &str) -> Vec<Package> {
    stdout
        .lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split('\t').collect();
            if f.len() < 7 {
                return None;
            }
            let sig = key_id_of(f[5]).or_else(|| key_id_of(f[6]));
            Some(Package {
                name: f[0].to_string(),
                epoch: (f[1] != "(none)").then(|| f[1].to_string()),
                version: f[2].to_string(),
                release: f[3].to_string(),
                arch: f[4].to_string(),
                sig_key_id: sig,
            })
        })
        .collect()
}

/// Who owns a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Owner {
    /// The packages that own it (usually one).
    Packages(Vec<Package>),
    /// The file is there and no package owns it: put there by hand.
    NotOwned,
    /// The file is not there.
    Absent,
    /// There is no rpm on this machine to ask.
    NoRpm,
}

fn failed(what: &str, out: &CmdOut) -> String {
    let said = out
        .stderr
        .lines()
        .chain(out.stdout.lines())
        .find(|l| !l.trim().is_empty())
        .unwrap_or("no message")
        .trim()
        .to_string();
    format!("{what} exited {}: {said}", out.status_text())
}

/// `rpm -qf` on `path`.
pub fn owner_of(f: &dyn Facts, path: &Path) -> Result<Owner, String> {
    if !exists(f, path) && f.lstat(path).is_err() {
        return Ok(Owner::Absent);
    }
    if !exists(f, Path::new(RPM)) {
        return Ok(Owner::NoRpm);
    }
    let p = path.to_string_lossy();
    let out = f
        .run(Path::new(RPM), &["-qf", "--qf", QF, &p])
        .map_err(|e| format!("{RPM} could not be run: {e}"))?;
    if out.ok() {
        return Ok(Owner::Packages(parse_packages(&out.stdout)));
    }
    let text = format!("{}\n{}", out.stdout, out.stderr);
    if text.contains("is not owned by any package") {
        return Ok(Owner::NotOwned);
    }
    if text.contains("No such file or directory") {
        return Ok(Owner::Absent);
    }
    Err(failed(&format!("rpm -qf {p}"), &out))
}

/// `rpm -q` on a package name: every installed package of that name (empty
/// when none is).
pub fn query(f: &dyn Facts, name: &str) -> Result<Vec<Package>, String> {
    if !exists(f, Path::new(RPM)) {
        return Ok(Vec::new());
    }
    let out = f
        .run(Path::new(RPM), &["-q", "--qf", QF, name])
        .map_err(|e| format!("{RPM} could not be run: {e}"))?;
    if out.ok() {
        return Ok(parse_packages(&out.stdout));
    }
    if format!("{}{}", out.stdout, out.stderr).contains("is not installed") {
        return Ok(Vec::new());
    }
    Err(failed(&format!("rpm -q {name}"), &out))
}

/// The keys rpm trusts, as `rpmkeys --list` prints them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportedKeys {
    /// Full fingerprints (40 hex digits, lowercase), as rpm 6 lists them.
    pub fingerprints: Vec<String>,
    /// Short key IDs (8 hex digits, lowercase) from `gpg-pubkey-<id>-<date>`
    /// names, as older rpm lists them.
    pub short_ids: Vec<String>,
}

impl ImportedKeys {
    /// Is the key with this fingerprint (any case, spaces allowed) imported?
    /// A short ID matches its fingerprint's last eight digits, the only form
    /// an older rpm gives.
    pub fn contains(&self, fingerprint: &str) -> bool {
        let fp: String = fingerprint
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect::<String>()
            .to_ascii_lowercase();
        if self.fingerprints.contains(&fp) {
            return true;
        }
        let short = &fp[fp.len().saturating_sub(8)..];
        self.short_ids.iter().any(|k| k == short)
    }
}

/// Parse `rpmkeys --list`: each line starts with a fingerprint (rpm 6) or a
/// `gpg-pubkey-<id>-<date>` name (older rpm).
pub fn parse_imported_keys(stdout: &str) -> ImportedKeys {
    let mut keys = ImportedKeys::default();
    for line in stdout.lines() {
        let Some(first) = line.split_whitespace().next() else {
            continue;
        };
        let first = first.trim_end_matches(':').to_ascii_lowercase();
        if first.len() == 40 && first.bytes().all(|b| b.is_ascii_hexdigit()) {
            keys.fingerprints.push(first);
        } else if let Some(rest) = first.strip_prefix("gpg-pubkey-") {
            if let Some(id) = rest.split('-').next() {
                if !id.is_empty() && id.bytes().all(|b| b.is_ascii_hexdigit()) {
                    keys.short_ids.push(id.to_string());
                }
            }
        }
    }
    keys
}

/// `rpmkeys --list`.
pub fn imported_keys(f: &dyn Facts) -> Result<ImportedKeys, String> {
    if !exists(f, Path::new(RPMKEYS)) {
        return Ok(ImportedKeys::default());
    }
    let out = f
        .run(Path::new(RPMKEYS), &["--list"])
        .map_err(|e| format!("{RPMKEYS} could not be run: {e}"))?;
    if !out.ok() {
        return Err(failed("rpmkeys --list", &out));
    }
    Ok(parse_imported_keys(&out.stdout))
}

/// When the rpm database last changed: the newest modification time of
/// [`RPMDB_WRITE_FILES`] in the first of [`RPMDB_DIRS`] that holds any of
/// them, with the file it came from. `None` on a machine without an rpm
/// database.
pub fn database_changed(f: &dyn Facts) -> Option<(PathBuf, i128)> {
    for dir in RPMDB_DIRS {
        let newest = RPMDB_WRITE_FILES
            .iter()
            .filter_map(|name| {
                let p = Path::new(dir).join(name);
                f.stat(&p).ok().map(|m| (p, m.mtime_ns))
            })
            .max_by_key(|(_, t)| *t);
        if newest.is_some() {
            return newest;
        }
    }
    None
}
