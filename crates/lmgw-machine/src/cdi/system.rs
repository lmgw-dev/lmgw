//! The system's CDI spec dirs and what they resolve to (machine-check design
//! §1.2), read the way Podman reads them.
//!
//! **The dirs.** containers.conf's `[engine] cdi_spec_dirs`, default
//! `/etc/cdi` then `/var/run/cdi`, through the files Podman loads for a
//! rootless user, in its order (containers.conf(5)): the distribution's, the
//! administrator's and its drop-ins, the rootless ones, the user's and its
//! drop-ins; `CONTAINERS_CONF` replaces all of them, and
//! `CONTAINERS_CONF_OVERRIDE` comes last. A later file's value replaces an
//! earlier one, unless an `{append=true}` entry switched to appending.
//!
//! **The resolution** mirrors the CDI library's cache refresh
//! (`container-device-interface` `pkg/cdi/cache.go`): the dirs are read in
//! order, each one's `*.yaml` and `*.json` files in name order and no
//! subdirectory; a device name a later dir defines replaces an earlier one;
//! two files in the *same* dir defining one name are a conflict, and a
//! conflicting name is dropped at the end even when a later dir defines it
//! too. A file that does not parse is an error and defines nothing.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::spec::{Format, Spec};
use crate::facts::{read_text, Facts, Kind};

/// Podman's default spec dirs, in its order.
pub const DEFAULT_SPEC_DIRS: [&str; 2] = ["/etc/cdi", "/var/run/cdi"];

/// Podman's configured spec dirs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpecDirs {
    pub dirs: Vec<PathBuf>,
    /// The last file that set them, `None` for Podman's default.
    pub set_by: Option<PathBuf>,
    /// containers.conf files that did not parse, and why.
    pub errors: Vec<(PathBuf, String)>,
}

/// The `.conf` files in `dir`, sorted.
fn drop_ins(f: &dyn Facts, dir: &Path) -> Vec<PathBuf> {
    f.list(dir)
        .unwrap_or_default()
        .into_iter()
        .filter(|n| n.ends_with(".conf"))
        .map(|n| dir.join(n))
        .collect()
}

/// The containers.conf files Podman loads for this (rootless) user, in its
/// order, whether or not each is there.
pub fn containers_conf_files(f: &dyn Facts) -> Vec<PathBuf> {
    let mut out = Vec::new();
    match f.env("CONTAINERS_CONF").filter(|v| !v.is_empty()) {
        Some(only) => out.push(PathBuf::from(only)),
        None => {
            for base in ["/usr/share/containers", "/etc/containers"] {
                let base = Path::new(base);
                out.push(base.join("containers.conf"));
                out.extend(drop_ins(f, &base.join("containers.conf.d")));
            }
            if let Some(uid) = f.uid().filter(|&uid| uid != 0) {
                let etc = Path::new("/etc/containers");
                out.push(etc.join("containers.rootless.conf"));
                // containers.conf(5) spells the drop-in dir `containers.rootless.d`;
                // both spellings are read, a file in either counts.
                for d in ["containers.rootless.d", "containers.rootless.conf.d"] {
                    out.extend(drop_ins(f, &etc.join(d)));
                    out.extend(drop_ins(f, &etc.join(d).join(uid.to_string())));
                }
            }
            let config = f
                .env("XDG_CONFIG_HOME")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .or_else(|| f.env("HOME").map(|h| Path::new(&h).join(".config")));
            if let Some(config) = config {
                let user = config.join("containers");
                out.push(user.join("containers.conf"));
                out.extend(drop_ins(f, &user.join("containers.conf.d")));
            }
        }
    }
    if let Some(over) = f.env("CONTAINERS_CONF_OVERRIDE").filter(|v| !v.is_empty()) {
        out.push(PathBuf::from(over));
    }
    out
}

/// One containers.conf file's `[engine] cdi_spec_dirs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfDirs {
    pub dirs: Vec<String>,
    /// The append switch an `{append = …}` entry sets, when one does.
    pub append: Option<bool>,
}

/// One file's `[engine] cdi_spec_dirs`; `None` when the file does not set
/// the key.
pub fn cdi_dirs_in(text: &str) -> Result<Option<ConfDirs>, String> {
    let table: toml::Table = toml::from_str(text).map_err(|e| e.to_string())?;
    let Some(value) = table
        .get("engine")
        .and_then(|e| e.as_table())
        .and_then(|e| e.get("cdi_spec_dirs"))
    else {
        return Ok(None);
    };
    let array = value
        .as_array()
        .ok_or_else(|| "engine.cdi_spec_dirs is not an array".to_string())?;
    let mut dirs = Vec::new();
    let mut append = None;
    for item in array {
        match item {
            toml::Value::String(s) => dirs.push(s.clone()),
            toml::Value::Table(t) => {
                if let Some(b) = t.get("append").and_then(|v| v.as_bool()) {
                    append = Some(b);
                }
            }
            other => {
                return Err(format!(
                    "engine.cdi_spec_dirs holds {other}, neither a path nor {{append=…}}"
                ))
            }
        }
    }
    Ok(Some(ConfDirs { dirs, append }))
}

/// Podman's spec dirs for this user.
pub fn configured_spec_dirs(f: &dyn Facts) -> SpecDirs {
    let mut dirs: Vec<String> = DEFAULT_SPEC_DIRS.iter().map(|d| d.to_string()).collect();
    let mut set_by = None;
    let mut append = false;
    let mut errors = Vec::new();
    for path in containers_conf_files(f) {
        let Ok(text) = read_text(f, &path) else {
            continue;
        };
        match cdi_dirs_in(&text) {
            Ok(None) => {}
            Ok(Some(conf)) => {
                if let Some(flag) = conf.append {
                    append = flag;
                }
                if append {
                    dirs.extend(conf.dirs);
                } else {
                    dirs = conf.dirs;
                }
                set_by = Some(path);
            }
            Err(e) => errors.push((path, e)),
        }
    }
    SpecDirs {
        dirs: dirs.into_iter().map(PathBuf::from).collect(),
        set_by,
        errors,
    }
}

/// What a list of spec dirs resolves to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Resolution {
    /// Every device name that resolves (`kind=name`), with the spec file
    /// that defines it.
    pub devices: BTreeMap<String, PathBuf>,
    /// Every name dropped for a same-dir conflict, with the files that
    /// define it.
    pub conflicts: BTreeMap<String, Vec<PathBuf>>,
    /// Spec files that could not be read or parsed, and why.
    pub errors: Vec<(PathBuf, String)>,
}

/// Resolve `dirs` as the CDI library does (see the module doc).
pub fn resolve(f: &dyn Facts, dirs: &[PathBuf]) -> Resolution {
    let mut res = Resolution::default();
    // name → (dir priority, file)
    let mut seen: BTreeMap<String, (usize, PathBuf)> = BTreeMap::new();
    for (priority, dir) in dirs.iter().enumerate() {
        let names = match f.list(dir) {
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                res.errors.push((dir.clone(), e.to_string()));
                continue;
            }
        };
        for name in names {
            let path = dir.join(&name);
            if matches!(f.stat(&path), Ok(m) if m.kind == Kind::Dir) {
                continue;
            }
            let Some(format) = Format::of(&path) else {
                continue;
            };
            let spec = match f.read(&path) {
                Ok(bytes) => Spec::parse(&bytes, format).map_err(|e| e.to_string()),
                Err(e) => Err(e.to_string()),
            };
            let spec = match spec {
                Ok(s) => s,
                Err(e) => {
                    res.errors.push((path, e));
                    continue;
                }
            };
            for qualified in spec.qualified_names() {
                match seen.get(&qualified) {
                    Some((p, _)) if *p < priority => {
                        seen.insert(qualified, (priority, path.clone()));
                    }
                    Some((p, other)) if *p == priority => {
                        let files = res.conflicts.entry(qualified).or_default();
                        for file in [other, &path] {
                            if !files.contains(file) {
                                files.push(file.clone());
                            }
                        }
                    }
                    Some(_) => {}
                    None => {
                        seen.insert(qualified, (priority, path.clone()));
                    }
                }
            }
        }
    }
    for (name, (_, file)) in seen {
        if !res.conflicts.contains_key(&name) {
            res.devices.insert(name, file);
        }
    }
    res
}
