//! dnf's repository sections, as dnf5 sees them (machine-check design §3.2,
//! "Which sections count"): the `.repo` files in dnf5's repository dirs, then
//! dnf5's override files, later ones winning. And the times in dnf's system
//! metadata cache, for the frozen-section reading.
//!
//! Only world-readable `.repo` files are read, override files included:
//! root's helper skips the others the same way, so the user's and root's
//! plans agree, and the skipped ones are named.

use std::path::{Path, PathBuf};

use crate::facts::{read_text, Facts};

/// dnf5's default `reposdir` (`dnf5 --dump-main-config` on Fedora 44), read
/// in this order. A `reposdir` set in dnf's own configuration is not read
/// yet.
pub const REPO_DIRS: [&str; 3] = [
    "/etc/yum.repos.d",
    "/etc/distro.repos.d",
    "/usr/share/dnf5/repos.d",
];

/// dnf5's override dirs, the distribution's first, the administrator's (and
/// `dnf config-manager setopt`'s) second.
pub const OVERRIDE_DIRS: [&str; 2] = [
    "/usr/share/dnf5/repos.override.d",
    "/etc/dnf/repos.override.d",
];

/// dnf5's system metadata cache: one `<repo id>-<16 hex>` dir per section.
pub const DNF_CACHE: &str = "/var/cache/libdnf5";

/// One repository section, after the overrides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoSection {
    pub id: String,
    /// The `.repo` file it is in.
    pub file: PathBuf,
    pub name: Option<String>,
    /// `enabled` as the `.repo` file says (absent means on).
    pub enabled_in_file: bool,
    /// `enabled` after every override.
    pub enabled: bool,
    pub baseurl: Vec<String>,
    pub includepkgs: Vec<String>,
    pub gpgcheck: Option<bool>,
    pub repo_gpgcheck: Option<bool>,
    pub metadata_expire: Option<String>,
    /// The override files that changed a key of this section, in order.
    pub overridden_by: Vec<PathBuf>,
}

/// A `.repo` file that was not read, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    pub path: PathBuf,
    pub why: String,
}

/// Every section dnf5 would see.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Repos {
    pub sections: Vec<RepoSection>,
    pub skipped: Vec<Skipped>,
}

impl Repos {
    pub fn get(&self, id: &str) -> Option<&RepoSection> {
        self.sections.iter().find(|s| s.id == id)
    }
}

/// One `[section]` of an INI file: its header and its `key=value` lines, a
/// value continued on indented lines joined with a space.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IniSection {
    pub header: String,
    pub entries: Vec<(String, String)>,
}

/// The INI dialect dnf reads: `#` and `;` start a comment line, `key=value`
/// with whitespace around `=` ignored, and a line starting with whitespace
/// continues the previous value.
pub fn parse_ini(text: &str) -> Vec<IniSection> {
    let mut out: Vec<IniSection> = Vec::new();
    for raw in text.lines() {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
            continue;
        }
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            out.push(IniSection {
                header: trimmed[1..trimmed.len() - 1].trim().to_string(),
                entries: Vec::new(),
            });
            continue;
        }
        let Some(section) = out.last_mut() else {
            continue;
        };
        if raw.starts_with(char::is_whitespace) && !trimmed.contains('=') {
            if let Some((_, v)) = section.entries.last_mut() {
                v.push(' ');
                v.push_str(trimmed);
            }
            continue;
        }
        if let Some((k, v)) = trimmed.split_once('=') {
            section
                .entries
                .push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    out
}

/// A dnf boolean: `1`, `yes`, `true`, `on` (any case) are on, `0`, `no`,
/// `false`, `off` are off.
pub fn parse_bool(v: &str) -> Option<bool> {
    match v.trim().to_ascii_lowercase().as_str() {
        "1" | "yes" | "true" | "on" => Some(true),
        "0" | "no" | "false" | "off" => Some(false),
        _ => None,
    }
}

/// A list value: split on commas and whitespace.
pub fn parse_list(v: &str) -> Vec<String> {
    v.split(|c: char| c == ',' || c.is_whitespace())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// fnmatch's `*` and `?`, which is what dnf5 accepts in an override's
/// section name.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    fn go(p: &[u8], t: &[u8]) -> bool {
        match (p.first(), t.first()) {
            (None, None) => true,
            (Some(b'*'), _) => go(&p[1..], t) || (!t.is_empty() && go(p, &t[1..])),
            (Some(b'?'), Some(_)) => go(&p[1..], &t[1..]),
            (Some(a), Some(b)) if a == b => go(&p[1..], &t[1..]),
            _ => false,
        }
    }
    go(pattern.as_bytes(), text.as_bytes())
}

/// The `.repo` files in `dir`, sorted, each with its path.
fn repo_files(f: &dyn Facts, dir: &str) -> Vec<PathBuf> {
    f.list(Path::new(dir))
        .unwrap_or_default()
        .into_iter()
        .filter(|n| n.ends_with(".repo"))
        .map(|n| Path::new(dir).join(n))
        .collect()
}

/// `path`'s text when every user may read it, as root and the user both
/// read it; otherwise why not, for [`Repos::skipped`]. Root could read a
/// 0600 file the user cannot, and the two plans would then differ on what
/// it says (an `enabled` the override sets), so neither reads it.
fn readable_text(f: &dyn Facts, path: &Path) -> Result<String, String> {
    match f.stat(path) {
        Ok(m) if !m.posix.is_some_and(|p| p.mode & 0o004 != 0) => {
            return Err("not readable by every user".into())
        }
        Ok(_) => {}
        Err(e) => return Err(e.to_string()),
    }
    read_text(f, path).map_err(|e| e.to_string())
}

fn apply(section: &mut RepoSection, key: &str, value: &str) {
    match key {
        "name" => section.name = Some(value.to_string()),
        "enabled" => {
            if let Some(b) = parse_bool(value) {
                section.enabled = b;
            }
        }
        "baseurl" => section.baseurl = parse_list(value),
        "includepkgs" => section.includepkgs = parse_list(value),
        "gpgcheck" | "pkg_gpgcheck" => section.gpgcheck = parse_bool(value),
        "repo_gpgcheck" => section.repo_gpgcheck = parse_bool(value),
        "metadata_expire" => section.metadata_expire = Some(value.to_string()),
        _ => {}
    }
}

/// Every section in [`REPO_DIRS`], with [`OVERRIDE_DIRS`] applied.
pub fn read_repos(f: &dyn Facts) -> Repos {
    let mut repos = Repos::default();
    for dir in REPO_DIRS {
        for path in repo_files(f, dir) {
            let text = match readable_text(f, &path) {
                Ok(t) => t,
                Err(why) => {
                    repos.skipped.push(Skipped { path, why });
                    continue;
                }
            };
            for ini in parse_ini(&text) {
                // A section id seen in an earlier file wins, as in dnf: the
                // later one is a duplicate dnf warns about and ignores.
                if repos.sections.iter().any(|s| s.id == ini.header) {
                    continue;
                }
                let mut s = RepoSection {
                    id: ini.header.clone(),
                    file: path.clone(),
                    name: None,
                    enabled_in_file: true,
                    enabled: true,
                    baseurl: Vec::new(),
                    includepkgs: Vec::new(),
                    gpgcheck: None,
                    repo_gpgcheck: None,
                    metadata_expire: None,
                    overridden_by: Vec::new(),
                };
                for (k, v) in &ini.entries {
                    apply(&mut s, k, v);
                }
                s.enabled_in_file = s.enabled;
                repos.sections.push(s);
            }
        }
    }
    for dir in OVERRIDE_DIRS {
        for path in repo_files(f, dir) {
            let text = match readable_text(f, &path) {
                Ok(t) => t,
                Err(why) => {
                    repos.skipped.push(Skipped { path, why });
                    continue;
                }
            };
            for ini in parse_ini(&text) {
                for s in repos
                    .sections
                    .iter_mut()
                    .filter(|s| glob_match(&ini.header, &s.id))
                {
                    for (k, v) in &ini.entries {
                        apply(s, k, v);
                    }
                    if !ini.entries.is_empty() && !s.overridden_by.contains(&path) {
                        s.overridden_by.push(path.clone());
                    }
                }
            }
        }
    }
    repos
}

/// A section's times in the system cache, from its newest cache dir.
///
/// `checked_ns` is the modification time of the section's primary metadata
/// file (the `<data type="primary">` location `repomd.xml` names), which is
/// dnf5's own clock for its cache: `Repo::get_age` (libdnf5
/// `repo/repo.cpp`) is now minus that file's mtime, and when expired
/// metadata is found still in sync with its source, the reuse branch of
/// `RepoSack::Impl::update_and_load_repos` (`repo/repo_sack.cpp`, its
/// `load_downloaded_repo`) sets the file's times to now with
/// `utimes(primary, nullptr)` — so it moves only when dnf loaded the section,
/// fresh or confirmed (read on dnf5's `main`, 2026-11-03). Neither
/// `repomd.xml`'s time (when a source last published: a release
/// repository's keeps its release day while its primary file moves on every
/// load) nor the cache dir's (it moves on any entry made or removed, so many
/// dirs share one time) says that.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheTime {
    pub id: String,
    pub dir: PathBuf,
    pub repomd_mtime_ns: i128,
    /// The primary metadata file whose time `checked_ns` is.
    pub primary: PathBuf,
    pub checked_ns: i128,
}

/// The `href` of `repomd.xml`'s primary metadata location
/// (`repodata/<checksum>-primary.xml.gz`); `None` without one, or for one
/// that is absolute or leaves the cache dir.
pub fn primary_location(repomd: &str) -> Option<&str> {
    let start = repomd
        .find(r#"<data type="primary">"#)
        .or_else(|| repomd.find("<data type='primary'>"))?;
    let block = &repomd[start..];
    let block = &block[..block.find("</data>")?];
    let loc = &block[block.find("<location")?..];
    let at = loc.find("href=")? + "href=".len();
    let quote = loc[at..]
        .chars()
        .next()
        .filter(|c| *c == '"' || *c == '\'')?;
    let value = &loc[at + 1..];
    let href = &value[..value.find(quote)?];
    let inside = !href.is_empty()
        && !href.starts_with('/')
        && Path::new(href)
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)));
    inside.then_some(href)
}

/// The section id a cache dir is for: `<id>-<16 hex>` → `<id>`.
pub fn cache_dir_id(name: &str) -> Option<&str> {
    let (id, hash) = name.rsplit_once('-')?;
    (hash.len() == 16 && hash.bytes().all(|b| b.is_ascii_hexdigit()) && !id.is_empty())
        .then_some(id)
}

/// Every section's newest cache time in [`DNF_CACHE`], by id. A cache dir
/// without a primary metadata file `repomd.xml` names is one dnf does not
/// load either, and is left out.
pub fn cache_times(f: &dyn Facts) -> Vec<CacheTime> {
    let mut out: Vec<CacheTime> = Vec::new();
    for name in f.list(Path::new(DNF_CACHE)).unwrap_or_default() {
        let Some(id) = cache_dir_id(&name) else {
            continue;
        };
        let dir = Path::new(DNF_CACHE).join(&name);
        let repomd = dir.join("repodata").join("repomd.xml");
        let (Ok(m), Ok(text)) = (f.stat(&repomd), read_text(f, &repomd)) else {
            continue;
        };
        let Some(primary) = primary_location(&text).map(|href| dir.join(href)) else {
            continue;
        };
        let Ok(p) = f.stat(&primary) else {
            continue;
        };
        let time = CacheTime {
            id: id.to_string(),
            dir,
            repomd_mtime_ns: m.mtime_ns,
            primary,
            checked_ns: p.mtime_ns,
        };
        match out.iter_mut().find(|c| c.id == id) {
            Some(c) if c.checked_ns >= time.checked_ns => {}
            Some(c) => *c = time,
            None => out.push(time),
        }
    }
    out
}
