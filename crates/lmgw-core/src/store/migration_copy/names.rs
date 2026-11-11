//! The copies' names, and the list of the copies in a backups dir
//! (Settings → About, the refusal of a newer database, the reuse check).

use std::path::Path;
use std::time::SystemTime;

use lmgw_api_types::MigrationCopy;

use super::{Db, DATABASES};

/// The `n`th copy's name for `which`'s step from `from` to `to`.
pub(super) fn name(which: &Db, from: i64, to: i64, n: u64) -> String {
    if n == 1 {
        format!("{}-{from:04}-{to:04}.{}", which.stem, which.ext)
    } else {
        format!("{}-{from:04}-{to:04}-{n}.{}", which.stem, which.ext)
    }
}

/// The hidden file a copy is made in before it is published (D114): no
/// copy's pattern matches it, so nothing lists it or offers it.
pub(super) fn partial_name(which: &Db, from: i64, to: i64) -> String {
    format!(".{}.partial", name(which, from, to, 1))
}

/// Whether `file` is a partial of `which`'s copies.
pub(super) fn is_partial(which: &Db, file: &str) -> bool {
    file.strip_prefix('.')
        .and_then(|f| f.strip_suffix(".partial"))
        .is_some_and(|f| parse(which, f).is_some())
}

/// `(from, to, n)` from a name of `which`'s copies; `None` for any other
/// file.
pub(super) fn parse(which: &Db, file: &str) -> Option<(i64, i64, u64)> {
    let stem = file
        .strip_prefix(which.stem)?
        .strip_prefix('-')?
        .strip_suffix(which.ext)?
        .strip_suffix('.')?;
    let mut parts = stem.split('-');
    let from = parts.next()?.parse().ok()?;
    let to = parts.next()?.parse().ok()?;
    let n = match parts.next() {
        None => 1,
        Some(n) => n.parse().ok().filter(|n| *n >= 2)?,
    };
    if parts.next().is_some() {
        return None;
    }
    Some((from, to, n))
}

/// The copies in a backups dir, and what of it cannot be read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CopyList {
    /// Newest first: by the file's time, then by the name's number.
    pub copies: Vec<MigrationCopy>,
    /// The dir or an entry in it that cannot be read, in words; `None`
    /// when everything was read (a dir that is not there lists none).
    pub error: Option<String>,
}

/// A copy with what the list sorts by.
pub(super) struct Found {
    pub(super) copy: MigrationCopy,
    pub(super) which: &'static Db,
    pub(super) n: u64,
    pub(super) modified: Option<SystemTime>,
}

/// Every copy in `backups`, unsorted, and what cannot be read.
pub(super) fn found(backups: &Path) -> (Vec<Found>, Option<String>) {
    let entries = match std::fs::read_dir(backups) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (Vec::new(), None),
        Err(e) => {
            return (
                Vec::new(),
                Some(format!(
                    "the backups folder {} cannot be read: {e}",
                    backups.display()
                )),
            )
        }
    };
    let mut out = Vec::new();
    let mut unreadable = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                unreadable.push(format!(
                    "an entry of {} cannot be read: {e}",
                    backups.display()
                ));
                continue;
            }
        };
        let Some(file) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let Some((which, (from, to, n))) = DATABASES
            .iter()
            .find_map(|db| parse(db, &file).map(|p| (*db, p)))
        else {
            continue;
        };
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(e) => {
                unreadable.push(format!("{file} cannot be read: {e}"));
                continue;
            }
        };
        // A dir or a link under a copy's name is not a copy.
        if !meta.is_file() {
            continue;
        }
        let modified = meta.modified().ok();
        out.push(Found {
            copy: MigrationCopy {
                file,
                path: entry.path().display().to_string(),
                database: which.file.to_string(),
                from,
                to,
                bytes: meta.len(),
                made_at: modified.map(utc).unwrap_or_default(),
            },
            which,
            n,
            modified,
        });
    }
    let error = (!unreadable.is_empty()).then(|| {
        format!(
            "in the backups folder {}: {}",
            backups.display(),
            unreadable.join("; ")
        )
    });
    (out, error)
}

/// The copies in `backups`, newest first (by the file's time, then the
/// name's number), with what cannot be read: what Settings → About lists.
pub fn migration_copies(backups: &Path) -> CopyList {
    let (mut found, error) = found(backups);
    found.sort_by_key(|f| std::cmp::Reverse((f.modified, f.n)));
    CopyList {
        copies: found.into_iter().map(|f| f.copy).collect(),
        error,
    }
}

/// The newest copy of `which`'s step from `from` to `to` (by the file's
/// time, then the name's number): the one a start may reuse (D113).
pub(super) fn newest_of_step(
    backups: &Path,
    which: &Db,
    from: i64,
    to: i64,
) -> Option<MigrationCopy> {
    found(backups)
        .0
        .into_iter()
        .filter(|f| f.which.file == which.file && f.copy.from == from && f.copy.to == to)
        .max_by_key(|f| (f.modified, f.n))
        .map(|f| f.copy)
}

fn utc(t: SystemTime) -> String {
    chrono::DateTime::<chrono::Utc>::from(t)
        .format("%Y-%m-%d %H:%M:%S")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::super::{CORPUS, KNOWLEDGE, MAIN};
    use super::*;

    #[test]
    fn a_copy_name_reads_back_and_nothing_else_does() {
        assert_eq!(name(&MAIN, 101, 102, 1), "lmgw-0101-0102.sqlite");
        assert_eq!(name(&MAIN, 101, 102, 3), "lmgw-0101-0102-3.sqlite");
        assert_eq!(name(&KNOWLEDGE, 2, 3, 1), "knowledge-0002-0003.db");
        assert_eq!(name(&CORPUS, 2, 3, 2), "quickdoc-0002-0003-2.db");
        assert_eq!(parse(&MAIN, "lmgw-0101-0102.sqlite"), Some((101, 102, 1)));
        assert_eq!(parse(&MAIN, "lmgw-0101-0102-3.sqlite"), Some((101, 102, 3)));
        assert_eq!(parse(&KNOWLEDGE, "knowledge-0002-0003.db"), Some((2, 3, 1)));
        for other in [
            "lmgw.sqlite",
            "lmgw-0101-0102.sqlite-wal",
            "lmgw-0101-0102.sqlite.source",
            "lmgw-0101-0102-1.sqlite",
            "lmgw-0101.sqlite",
            "lmgw-0101-0102-2-2.sqlite",
            "lmgw-0101-0102.db",
            ".lmgw-0101-0102.sqlite.partial",
            "notes.txt",
        ] {
            assert_eq!(parse(&MAIN, other), None, "{other}");
        }
        assert_eq!(parse(&KNOWLEDGE, "lmgw-0101-0102.sqlite"), None);
    }

    #[test]
    fn a_partial_is_known_by_its_dot_and_suffix() {
        assert_eq!(
            partial_name(&MAIN, 101, 102),
            ".lmgw-0101-0102.sqlite.partial"
        );
        assert!(is_partial(&MAIN, ".lmgw-0101-0102.sqlite.partial"));
        assert!(!is_partial(&KNOWLEDGE, ".lmgw-0101-0102.sqlite.partial"));
        assert!(!is_partial(&MAIN, "lmgw-0101-0102.sqlite"));
        assert!(!is_partial(&MAIN, ".notes.partial"));
    }
}
