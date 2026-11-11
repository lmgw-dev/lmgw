//! Unpack a Windows Installer package without installing it: eSpeak NG's
//! only Windows asset is an MSI (Windows design §4.2), and `msiexec /a` (an
//! administrative install) answers 1601 ("the Windows Installer service could
//! not be accessed") from a non-administrator's network logon, which is how
//! the CI runner and an SSH session reach Windows.
//!
//! The package's tables say where each file goes: the `File` table names a
//! file's key, its component and its name (`short|long`); the `Component`
//! table the component's directory; the `Directory` table each directory's
//! parent and name (`DefaultDir`, `target[:source]`, `.` for "the parent
//! itself"), from `TARGETDIR`, which is `dir` here. The bytes are in the
//! cabinets the `Media` table names, embedded as streams (`#cab1.cab`),
//! where each file is stored under its key.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Every file of the package at `msi` into `dir`, laid out by its Directory
/// table.
pub fn extract(msi: &Path, dir: &Path) -> Result<(), String> {
    let at = |e: std::io::Error| format!("reading {}: {e}", msi.display());
    let mut pkg = msi::open(msi).map_err(at)?;
    let dirs = rows(
        &mut pkg,
        "Directory",
        &["Directory", "Directory_Parent", "DefaultDir"],
    )
    .map_err(at)?;
    let comps = rows(&mut pkg, "Component", &["Component", "Directory_"]).map_err(at)?;
    let files = rows(&mut pkg, "File", &["File", "Component_", "FileName"]).map_err(at)?;
    let media = rows(&mut pkg, "Media", &["Cabinet"]).map_err(at)?;

    let parent_and_name: HashMap<String, (Option<String>, String)> = dirs
        .into_iter()
        .map(|r| {
            let [id, parent, default] = <[Option<String>; 3]>::try_from(r).unwrap_or_default();
            (
                id.unwrap_or_default(),
                (
                    parent.filter(|p| !p.is_empty()),
                    target_name(&default.unwrap_or_default()),
                ),
            )
        })
        .collect();
    let component_dir: HashMap<String, String> = comps
        .into_iter()
        .filter_map(|r| {
            let [c, d] = <[Option<String>; 2]>::try_from(r).ok()?;
            Some((c?, d?))
        })
        .collect();
    let mut target: HashMap<String, PathBuf> = HashMap::new();
    for r in files {
        let [key, comp, name] = <[Option<String>; 3]>::try_from(r).unwrap_or_default();
        let (Some(key), Some(comp), Some(name)) = (key, comp, name) else {
            continue;
        };
        let d = component_dir
            .get(&comp)
            .ok_or_else(|| format!("{}: component {comp} has no directory", msi.display()))?;
        let rel = dir_path(&parent_and_name, d)?.join(safe(&long_name(&name))?);
        target.insert(key, rel);
    }

    let mut written = 0usize;
    for r in media {
        let Some(cab) = r.into_iter().next().flatten() else {
            continue;
        };
        let Some(stream) = cab.strip_prefix('#') else {
            return Err(format!(
                "{} keeps its files in an external cabinet ({cab}), which is not unpacked yet",
                msi.display()
            ));
        };
        let mut bytes = Vec::new();
        pkg.read_stream(stream)
            .and_then(|mut s| s.read_to_end(&mut bytes))
            .map_err(at)?;
        let files = super::cab::files(&bytes)
            .map_err(|e| format!("{}: cabinet {stream}: {e}", msi.display()))?;
        for f in files {
            let rel = target.get(&f.name).ok_or_else(|| {
                format!(
                    "{}: cabinet {stream} holds {}, which its File table does not name",
                    msi.display(),
                    f.name
                )
            })?;
            let out = dir.join(rel);
            if let Some(p) = out.parent() {
                std::fs::create_dir_all(p).map_err(|e| format!("creating {}: {e}", p.display()))?;
            }
            std::fs::write(&out, &f.data).map_err(|e| format!("writing {}: {e}", out.display()))?;
            written += 1;
        }
    }
    if written == 0 {
        return Err(format!("{} held no file to unpack", msi.display()));
    }
    Ok(())
}

/// `columns` of every row of `table`, as strings.
fn rows<F: std::io::Read + std::io::Seek>(
    pkg: &mut msi::Package<F>,
    table: &str,
    columns: &[&str],
) -> std::io::Result<Vec<Vec<Option<String>>>> {
    let rows = pkg.select_rows(msi::Select::table(table))?;
    Ok(rows
        .map(|row| {
            columns
                .iter()
                .map(|c| {
                    row.has_column(c)
                        .then(|| row[*c].as_str().map(str::to_string))
                        .flatten()
                })
                .collect()
        })
        .collect())
}

/// `DefaultDir`'s target part, long name: `"PFiles:Program Files"` →
/// `"PFiles"`, `"ESPEAK~1|eSpeak NG"` → `"eSpeak NG"`.
fn target_name(default_dir: &str) -> String {
    long_name(default_dir.split(':').next().unwrap_or_default())
}

/// `short|long` → long; a name without `|` is itself.
fn long_name(s: &str) -> String {
    s.rsplit('|').next().unwrap_or(s).to_string()
}

/// One path component, refused when it could leave its directory.
fn safe(name: &str) -> Result<&str, String> {
    if name.is_empty()
        || name == ".."
        || name.contains(['/', '\\', ':'])
        || name.chars().any(char::is_control)
    {
        return Err(format!("the package names a file or directory {name:?}"));
    }
    Ok(name)
}

/// The path of directory `id` below `TARGETDIR`.
fn dir_path(dirs: &HashMap<String, (Option<String>, String)>, id: &str) -> Result<PathBuf, String> {
    let mut parts = Vec::new();
    let mut at = id.to_string();
    // A directory chain longer than the table is a cycle.
    for _ in 0..=dirs.len() {
        let Some((parent, name)) = dirs.get(&at) else {
            return Err(format!(
                "the package's directory {at} is not in its Directory table"
            ));
        };
        let Some(parent) = parent else {
            // The root (TARGETDIR, "SourceDir") is the extraction dir.
            return Ok(parts.iter().rev().collect());
        };
        if name != "." {
            parts.push(safe(name)?.to_string());
        }
        at = parent.clone();
    }
    Err(format!("the package's directory {id} has a parent cycle"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_paths() {
        assert_eq!(target_name("ESPEAK~1|eSpeak NG"), "eSpeak NG");
        assert_eq!(target_name("PFiles:Program Files"), "PFiles");
        assert_eq!(
            long_name("libesp~1.dll|libespeak-ng.dll"),
            "libespeak-ng.dll"
        );
        assert!(safe("..").is_err());
        assert!(safe("a\\b").is_err());
        let mut d = HashMap::new();
        d.insert("TARGETDIR".to_string(), (None, "SourceDir".to_string()));
        d.insert(
            "PF".to_string(),
            (Some("TARGETDIR".to_string()), "PFiles64".to_string()),
        );
        d.insert(
            "APP".to_string(),
            (Some("PF".to_string()), "eSpeak NG".to_string()),
        );
        d.insert(
            "SAME".to_string(),
            (Some("APP".to_string()), ".".to_string()),
        );
        assert_eq!(
            dir_path(&d, "SAME").unwrap(),
            PathBuf::from("PFiles64/eSpeak NG")
        );
        d.insert(
            "LOOP".to_string(),
            (Some("LOOP".to_string()), "x".to_string()),
        );
        assert!(dir_path(&d, "LOOP").is_err());
    }
}
