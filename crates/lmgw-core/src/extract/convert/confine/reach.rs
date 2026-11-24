//! What a confined tool's rules may reach beyond the system's files
//! (performance round review S1, S2): the directories a tool installed
//! outside `/usr` runs from, and tesseract's language data outside `/usr`.
//! Each is checked against what no rule may ever cover ([`too_wide`]): the
//! root directory, a home directory or one that holds a home, and any
//! directory that holds lmgw's data, settings or cache. A tool whose needs
//! cannot be met that way runs with its limits alone (not confined), and
//! Settings › Documents names it so with the reason (`verdict.rs`); a rule
//! is never widened past this to make it run.
//!
//! **Where a tool is installed** ([`install_dirs`]): the binary's own
//! directory and, when that is a `bin` directory, the `lib`, `lib64`,
//! `libexec` and `share` beside it (each only when it passes
//! [`too_wide`]), never the prefix itself: `~/.local/bin/pdftotext` gets
//! `~/.local/bin` and `~/.local/lib`, not `~/.local`, whose `share` holds
//! lmgw's data dir. A `bin` directly in a home (`~/bin`) gets no siblings:
//! the home's own folders are not an install. A binary directly in a home
//! directory cannot be confined. `/home`, `/var/home` and what is above
//! them never pass, whatever `HOME` says; neither does anything inside
//! lmgw's own directories. A binary in a Nix-style store (`/nix/store/…`) gets the store
//! read and executable: its libraries and its loader are other paths of the
//! same store, which holds packages only, readable by every user.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// The system's programs and libraries, which every confined tool reads
/// and executes (`super::SYSTEM`).
const SYSTEM: &str = "/usr";

/// A Nix-style package store (module doc).
const STORE: &str = "/nix/store";

/// The directories under which a home directory is one level down.
const HOMES_UNDER: &[&str] = &["/home", "/var/home"];

/// The data dirs the gateway runs with (the default one is always counted).
static DATA_DIRS: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// The same dirs as the gateway was given them (a tool is given paths under
/// these): what a tool's stderr is scrubbed of (`run::scrub::known`).
static GIVEN: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// Count `data_dir` among lmgw's own directories, which no rule may hold
/// ([`too_wide`]): the gateway's start calls it with the dir it runs with.
pub fn protect(data_dir: &Path) {
    let dir = canonical(data_dir);
    let mut g = DATA_DIRS.lock().unwrap_or_else(|e| e.into_inner());
    if !g.contains(&dir) {
        g.push(dir);
    }
    drop(g);
    let mut g = GIVEN.lock().unwrap_or_else(|e| e.into_inner());
    if !g.iter().any(|d| d == data_dir) {
        g.push(data_dir.to_path_buf());
    }
}

/// lmgw's data dirs, as given and resolved, the default one among them.
pub fn own_data_dirs() -> Vec<PathBuf> {
    let mut out = GIVEN.lock().unwrap_or_else(|e| e.into_inner()).clone();
    out.extend(
        DATA_DIRS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned(),
    );
    out.push(crate::state::data_dirs::default_data_dir());
    out
}

fn canonical(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

fn env_dir(var: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

fn home() -> Option<PathBuf> {
    env_dir("HOME")
}

/// lmgw's own directories: the data dirs it runs with, the installed app's
/// data dir, and its settings and cache directories (`lmgw` and `lmgw-dev`
/// under `$XDG_CONFIG_HOME` and `$XDG_CACHE_HOME`, or `~/.config` and
/// `~/.cache`).
fn own_dirs(home: Option<&PathBuf>) -> Vec<PathBuf> {
    let mut out = DATA_DIRS.lock().unwrap_or_else(|e| e.into_inner()).clone();
    out.push(canonical(&crate::state::data_dirs::default_data_dir()));
    let base = |var: &str, under: &str| env_dir(var).or_else(|| home.map(|h| h.join(under)));
    for b in [
        base("XDG_CONFIG_HOME", ".config"),
        base("XDG_CACHE_HOME", ".cache"),
    ]
    .into_iter()
    .flatten()
    {
        for name in ["lmgw", "lmgw-dev"] {
            out.push(canonical(&b.join(name)));
        }
    }
    out
}

/// Where the home directory and lmgw's own directories are: the
/// process's environment ([`Places::now`]), or given (a test, which so
/// never changes `HOME` for the whole test binary).
#[derive(Debug, Clone, Default)]
pub struct Places {
    /// `$HOME`, when it is set and absolute.
    pub home: Option<PathBuf>,
    /// lmgw's own directories ([`own_dirs`]), canonical.
    pub own: Vec<PathBuf>,
}

impl Places {
    /// As the process has them now.
    pub fn now() -> Self {
        let home = home();
        let own = own_dirs(home.as_ref());
        Self { home, own }
    }

    /// The home directories known by name: `$HOME` and `/root`.
    fn homes(&self) -> Vec<PathBuf> {
        self.home
            .iter()
            .map(|h| canonical(h))
            .chain([PathBuf::from("/root")])
            .collect()
    }

    /// Whether `p` (canonical) is a home directory: `$HOME`, `/root`, or
    /// one level under `/home` or `/var/home`.
    fn is_home(&self, p: &Path) -> bool {
        self.homes().iter().any(|h| h == p)
            || p.parent()
                .is_some_and(|up| HOMES_UNDER.iter().any(|h| up == Path::new(h)))
    }
}

/// Why `p` may not be in a confined tool's rules (module doc), or `None`
/// when it may.
pub fn too_wide(p: &Path) -> Option<&'static str> {
    too_wide_in(p, &Places::now())
}

/// [`too_wide`] with the home and lmgw's directories at `places`.
pub fn too_wide_in(p: &Path, places: &Places) -> Option<&'static str> {
    let p = canonical(p);
    if p.parent().is_none() {
        return Some("it is the root directory");
    }
    // `/home` and `/var/home` themselves and what is above them, whatever
    // `HOME` says (root, a service user, none: verification SF3).
    if places.is_home(&p)
        || places.homes().iter().any(|h| h.starts_with(&p))
        || HOMES_UNDER.iter().any(|h| Path::new(h).starts_with(&p))
    {
        return Some("it is a home directory or holds one");
    }
    // lmgw's own directories, what holds them and what is inside them
    // (verification N-g).
    if places
        .own
        .iter()
        .any(|d| d.starts_with(&p) || p.starts_with(d))
    {
        return Some("it holds lmgw's data, settings or cache, or is inside them");
    }
    None
}

/// The directories a tool at `bin` reads and executes from beside the
/// system's (module doc): none under `/usr`; `Err` says why it cannot be
/// confined.
pub fn install_dirs(bin: &Path) -> Result<Vec<PathBuf>, String> {
    install_dirs_in(bin, &Places::now())
}

/// [`install_dirs`] with the home and lmgw's directories at `places`.
pub fn install_dirs_in(bin: &Path, places: &Places) -> Result<Vec<PathBuf>, String> {
    let Some(abs) = super::super::run::start::absolute(bin) else {
        // Not found: no run starts it either.
        return Ok(Vec::new());
    };
    let bin = canonical(&abs);
    if bin.starts_with(SYSTEM) {
        return Ok(Vec::new());
    }
    if bin.starts_with(STORE) {
        return Ok(vec![PathBuf::from(STORE)]);
    }
    let Some(dir) = bin.parent().filter(|d| d.is_absolute()) else {
        return Ok(Vec::new());
    };
    if let Some(why) = too_wide_in(dir, places) {
        return Err(format!(
            "it is installed in {} and {why}, which a confined program may not read",
            dir.display()
        ));
    }
    let mut out = vec![dir.to_path_buf()];
    if dir.file_name().is_some_and(|n| n == "bin") {
        if let Some(prefix) = dir.parent() {
            out.extend(siblings_in(prefix, places));
        }
    }
    Ok(out)
}

/// The `lib`, `lib64`, `libexec` and `share` of the install prefix
/// `prefix` that exist and pass [`too_wide`]; none when `prefix` is a
/// home (`~/bin`): a home's own folders are not an install (SF2).
#[cfg(all(test, unix))]
pub(super) fn siblings(prefix: &Path) -> Vec<PathBuf> {
    siblings_in(prefix, &Places::now())
}

/// [`siblings`] with the home and lmgw's directories at `places`.
pub(super) fn siblings_in(prefix: &Path, places: &Places) -> Vec<PathBuf> {
    if places.is_home(&canonical(prefix)) {
        return Vec::new();
    }
    ["lib", "lib64", "libexec", "share"]
        .iter()
        .map(|sub| prefix.join(sub))
        .filter(|p| p.is_dir() && too_wide_in(p, places).is_none())
        .collect()
}

/// LibreOffice's install directory for the `soffice` at `bin`: the top of
/// the directory its resolved path is in (`/usr/lib64/libreoffice` for
/// `…/program/soffice`), `/usr` for one that is itself in `/usr/bin`;
/// `None` when `bin` cannot be resolved. `Err` when that directory may not
/// be in a confined program's rules ([`too_wide_in`]): a `soffice` wrapper
/// in `~/.local/bin` makes it `~/.local`, which holds lmgw's data, one in
/// `~/bin` the whole home (the review of round B, S5). LibreOffice is then
/// refused, never run with that directory or without its confinement.
pub fn libreoffice_install(bin: &Path, places: &Places) -> Result<Option<PathBuf>, String> {
    let Some(top) = std::fs::canonicalize(bin)
        .ok()
        .and_then(|p| p.parent().and_then(Path::parent).map(Path::to_path_buf))
    else {
        return Ok(None);
    };
    // A `soffice` that is itself in `/usr/bin` (not a link into its
    // install) gives no install directory narrower than `/usr`.
    if top == Path::new("/") {
        return Ok(Some(PathBuf::from(SYSTEM)));
    }
    if let Some(why) = too_wide_in(&top, places) {
        return Err(format!(
            "LibreOffice's soffice at {} makes {} its install directory, and {why}, which a \
             confined program may not read and execute; LibreOffice is not used from there \
             (install it from the system's packages, or put its own install first on PATH)",
            bin.display(),
            top.display()
        ));
    }
    Ok(Some(top))
}

/// tesseract's language data at `dir` as a rule (S2): `None` under `/usr`
/// (the system's files have it); `Err` when it is no language data
/// directory (no `*.traineddata` in it) or one no rule may hold.
pub fn tessdata(dir: &Path) -> Result<Option<PathBuf>, String> {
    let dir = canonical(dir);
    if dir.starts_with(SYSTEM) {
        return Ok(None);
    }
    if let Some(why) = too_wide(&dir) {
        return Err(format!(
            "its language data directory {} is one a confined program may not read: {why}",
            dir.display()
        ));
    }
    let has_data = std::fs::read_dir(&dir).is_ok_and(|entries| {
        entries
            .flatten()
            .any(|e| e.path().extension().is_some_and(|x| x == "traineddata"))
    });
    if !has_data {
        return Err(format!(
            "its language data directory {} holds no *.traineddata, so lmgw does not let a \
             confined program read it",
            dir.display()
        ));
    }
    Ok(Some(dir))
}
