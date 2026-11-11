//! Whether a database is unchanged since a copy of it (D113): the record
//! beside each copy (`<copy>.source`) of the database as the last start
//! that made or reused the copy left it.
//!
//! The record is the database file's inode, size and modification time
//! (to the nanosecond), taken right after `PRAGMA wal_checkpoint(TRUNCATE)`
//! folded the write-ahead log into it and emptied the log. In WAL mode a
//! commit reaches the database file only through a checkpoint, which
//! writes the file (its time moves), so with the log empty the file is the
//! whole database and any write since shows; a file put in its place (a
//! restore, a move) has another inode. A checkpoint changes no content: it
//! is not a write in the copy's sense. A record that does not match makes
//! a new copy; a record matches only the state it holds, so one that could
//! not be updated needs no removing: the database moved on from it.
//!
//! Two cases reuse a copy without a match (D118, D119). A start killed
//! inside a migration that runs without a transaction (only `lmgw.sqlite`
//! has them) committed statements after its record, so the record carries
//! `migrator_entered <from>-<to>`, and the same file at the same step is
//! still the database the copy was made before: nothing else writes a
//! database with a pending migration (the command line's offline open
//! refuses one), and the previous image, which would, clears the
//! foreign-key check's marker the reuse also asks for. And a database
//! whose state cannot be read at all reuses the step's copy that is there
//! rather than copying at every restart. A copy made from a database a
//! start left inside such a migrator is marked `part_applied` and never
//! offered ahead of a whole copy of the same `<from>`.
//!
//! A content hash would also catch a write that keeps the size and lands in
//! the same nanosecond; it reads the whole database at every refused start,
//! which on a large database is the time the unit's `TimeoutStartSec` is
//! short of already.

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use sqlx::SqlitePool;

/// A database file as a start left it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Source {
    inode: u64,
    bytes: u64,
    modified_ns: u128,
}

/// The database `db` (open as `pool`) now: the log folded in first; why
/// not, in words.
pub(super) async fn read(pool: &SqlitePool, db: &Path) -> Result<Source, String> {
    let (busy, _, _): (i64, i64, i64) = sqlx::query_as("PRAGMA wal_checkpoint(TRUNCATE)")
        .fetch_one(pool)
        .await
        .map_err(|e| format!("its write-ahead log cannot be folded in: {e}"))?;
    if busy != 0 {
        return Err("its write-ahead log cannot be folded in while another reader holds it".into());
    }
    let mut wal = db.as_os_str().to_owned();
    wal.push("-wal");
    match std::fs::metadata(PathBuf::from(wal)) {
        Ok(m) if m.len() > 0 => {
            return Err(format!(
                "its write-ahead log still holds {} bytes after the checkpoint",
                m.len()
            ))
        }
        _ => {}
    }
    let meta =
        std::fs::metadata(db).map_err(|e| format!("{} cannot be read: {e}", db.display()))?;
    let modified_ns = meta
        .modified()
        .map_err(|e| format!("its modification time cannot be read: {e}"))?
        .duration_since(UNIX_EPOCH)
        .map_err(|e| format!("its modification time is before 1970: {e}"))?
        .as_nanos();
    Ok(Source {
        inode: inode(&meta),
        bytes: meta.len(),
        modified_ns,
    })
}

#[cfg(unix)]
fn inode(meta: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.ino()
}

#[cfg(not(unix))]
fn inode(_meta: &std::fs::Metadata) -> u64 {
    0
}

/// The record's path beside `copy`.
fn record(copy: &Path) -> PathBuf {
    let mut p = copy.as_os_str().to_owned();
    p.push(".source");
    PathBuf::from(p)
}

/// What a record says beyond the database's state (D118).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Marks {
    /// The start that left the database as recorded went on into the
    /// migrator of this step (`<from>`, `<to>`), which holds a migration
    /// that runs without a transaction: when it was killed in there, the
    /// statements it committed since moved the file on from the record.
    pub(super) entered: Option<(i64, i64)>,
    /// The copy was made from a database a start had left inside such a
    /// migrator, so it may hold a migration part applied; it is never
    /// offered ahead of a whole copy of the same `<from>`.
    pub(super) part_applied: bool,
}

/// A record as read back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Record {
    database: String,
    inode: Option<u128>,
    bytes: Option<u128>,
    modified_ns: Option<u128>,
    copy_bytes: Option<u128>,
    pub(super) marks: Marks,
}

/// How a record compares with the database now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Match {
    /// The database is as recorded.
    Same,
    /// The same file (database path and inode, and the copy unchanged), on
    /// which the recorded start entered this step's migrator; its size or
    /// time moved since (D118).
    Entered,
    /// Anything else.
    Differs,
}

/// The record beside `copy`; `None` when there is none or it cannot be read.
pub(super) fn read_record(copy: &Path) -> Option<Record> {
    let text = std::fs::read_to_string(record(copy)).ok()?;
    let field = |key: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix(' '))
            .map(str::to_string)
    };
    let num = |key: &str| field(key).and_then(|v| v.parse::<u128>().ok());
    let entered = field("migrator_entered").and_then(|v| {
        let (from, to) = v.split_once('-')?;
        Some((from.parse().ok()?, to.parse().ok()?))
    });
    Some(Record {
        database: field("database")?,
        inode: num("inode"),
        bytes: num("bytes"),
        modified_ns: num("modified_ns"),
        copy_bytes: num("copy_bytes"),
        marks: Marks {
            entered,
            part_applied: field("part_applied").as_deref() == Some("yes"),
        },
    })
}

impl Record {
    /// Whether the record says `db` (now `now`) is as recorded, beside a
    /// copy now `copy_bytes` long, for the start's step `from` → `to`.
    pub(super) fn compare(
        &self,
        copy_bytes: u64,
        db: &Path,
        now: &Source,
        (from, to): (i64, i64),
    ) -> Match {
        let same_file = self.database == db.display().to_string()
            && self.inode == Some(u128::from(now.inode))
            && self.copy_bytes == Some(u128::from(copy_bytes));
        if !same_file {
            return Match::Differs;
        }
        if self.bytes == Some(u128::from(now.bytes)) && self.modified_ns == Some(now.modified_ns) {
            Match::Same
        } else if self.marks.entered == Some((from, to)) {
            Match::Entered
        } else {
            Match::Differs
        }
    }

    /// Whether this record is of the file `db` with `inode` and holds a
    /// start that entered the migrator towards `to` from `from` or below:
    /// the database it was left in has not finished that step since.
    pub(super) fn entered_towards(&self, db: &Path, inode: u64, from: i64, to: i64) -> bool {
        self.database == db.display().to_string()
            && self.inode == Some(u128::from(inode))
            && self
                .marks
                .entered
                .is_some_and(|(f, t)| t == to && f <= from)
    }
}

/// The inode of the database file `db`, read on its own (the reuse check's
/// [`read`] may have failed).
pub(super) fn inode_of(db: &Path) -> Option<u64> {
    std::fs::metadata(db).ok().map(|m| inode(&m))
}

/// Record `s` as the state of `db` that `copy` still matches, with `marks`;
/// the copy's own size goes with it, so a copy changed since is not reused.
pub(super) fn write(copy: &Path, db: &Path, s: &Source, marks: Marks) -> std::io::Result<()> {
    let copy_bytes = std::fs::metadata(copy)?.len();
    let mut text = format!(
        "# lmgw: the database this copy still matches, as the last start that made or reused \
         it left it (headless server design D113, D118). Removing this file makes the next \
         start copy the database again.\n\
         database {}\ninode {}\nbytes {}\nmodified_ns {}\ncopy_bytes {copy_bytes}\n",
        db.display(),
        s.inode,
        s.bytes,
        s.modified_ns,
    );
    if let Some((from, to)) = marks.entered {
        text.push_str(&format!("migrator_entered {from:04}-{to:04}\n"));
    }
    if marks.part_applied {
        text.push_str("part_applied yes\n");
    }
    let path = record(copy);
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    {
        let mut o = std::fs::OpenOptions::new();
        o.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            o.mode(0o600);
        }
        use std::io::Write;
        let mut f = o.open(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, &path)?;
    // The new name, durable.
    if let Some(dir) = path.parent() {
        if let Err(e) = std::fs::File::open(dir).and_then(|d| d.sync_all()) {
            tracing::warn!(
                "{} is written, but its dir cannot be synced ({e}); after a power loss the \
                 next start may find the earlier record, and then copies the database again",
                path.display()
            );
        }
    }
    Ok(())
}
