//! Making a copy: the backups dir, the partial file, `VACUUM INTO` it, the
//! publish under the first free name; and the refusal when it cannot be
//! made (D106).

use std::path::{Path, PathBuf};

use lmgw_api_types::units::fmt_bytes;
use sqlx::SqlitePool;

use super::names::{is_partial, name, partial_name};
use super::{Db, Step};

/// The backups dir ready for a copy of `which` (the database `db`): made
/// `0700` when it is not there, tightened to `0700` when others may read it
/// (D109, D120), and the partials an earlier start left removed (D114).
/// What failed, in words.
pub(super) fn prepare_dir(backups: &Path, which: &Db, db: &Path) -> Result<(), String> {
    make_dir(backups).map_err(|e| {
        format!(
            "the backups dir {} cannot be created: {e}",
            backups.display()
        )
    })?;
    clear_partials(backups, which, db);
    Ok(())
}

fn make_dir(dir: &Path) -> std::io::Result<()> {
    if dir.is_dir() {
        tighten(dir);
        return Ok(());
    }
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(0o700);
    }
    b.create(dir)
}

/// An existing backups dir others may enter or read is set to `0700`:
/// every copy holds what the database holds, the provider keys in
/// cleartext included, and the dir is lmgw's own. The setgid and sticky
/// bits an administrator set stay (D120): `0700` is about who reads, they
/// are about the group new files get and who may remove them. A backups
/// path that is a symbolic link is used as it is, its target's mode never
/// changed (D120): an administrator who put the copies on another volume
/// owns that dir, and a chmod would go through the link onto it.
#[cfg(unix)]
fn tighten(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let Ok(meta) = std::fs::metadata(dir) else {
        return;
    };
    let mode = meta.permissions().mode() & 0o7777;
    if mode & 0o077 == 0 {
        return;
    }
    if std::fs::symlink_metadata(dir).is_ok_and(|m| m.file_type().is_symlink()) {
        let target = std::fs::read_link(dir).map_or_else(
            |e| format!("a target that cannot be read: {e}"),
            |t| t.display().to_string(),
        );
        tracing::warn!(
            "the backups dir {} is a link to {target}, which is {mode:04o}, open to others; lmgw \
             does not change a linked dir's mode, so make it 0700 yourself: every copy holds \
             what the database holds, the provider keys in cleartext included (each copy is \
             0600 itself)",
            dir.display()
        );
        return;
    }
    let tight = (mode & 0o7000) | 0o700;
    match std::fs::set_permissions(dir, std::fs::Permissions::from_mode(tight)) {
        Ok(()) => tracing::warn!(
            "the backups dir {} was {mode:04o}, open to others; it is {tight:04o} now, since \
             every copy holds what the database holds, the provider keys in cleartext included",
            dir.display()
        ),
        Err(e) => tracing::warn!(
            "the backups dir {} is {mode:04o}, open to others, and cannot be made 0700 ({e}); \
             each copy in it is 0600 itself",
            dir.display()
        ),
    }
}

#[cfg(not(unix))]
fn tighten(_dir: &Path) {}

/// Remove the partial copies of `which` (the database `db`) an earlier
/// start left: a start killed or stopped while it copied (the unit's
/// `TimeoutStartSec` on a large database). Such a file never held a whole
/// database, and kept, a start killed at every try would fill the volume
/// with them.
fn clear_partials(backups: &Path, which: &Db, db: &Path) {
    let Ok(entries) = std::fs::read_dir(backups) else {
        return;
    };
    for entry in entries.flatten() {
        let file = entry.file_name();
        if !file.to_str().is_some_and(|f| is_partial(which, f)) {
            continue;
        }
        let path = entry.path();
        match std::fs::remove_file(&path) {
            Ok(()) => tracing::warn!(
                "removed {}, the unfinished copy of {} an earlier start left (stopped while it \
                 copied; it never held a whole database). When every start stops while it \
                 copies, the server unit's TimeoutStartSec is likely shorter than the copy of the \
                 database takes ({} is {}): raise it in the unit",
                path.display(),
                which.file,
                db.display(),
                std::fs::metadata(db).map_or_else(
                    |e| format!("of a size that cannot be read: {e}"),
                    |m| fmt_bytes(m.len())
                ),
            ),
            Err(e) => tracing::warn!(
                "{}, the unfinished copy of {} an earlier start left, cannot be removed: {e}",
                path.display(),
                which.file,
            ),
        }
    }
}

/// `VACUUM INTO` a partial file in `backups`, synced, then published under
/// the first free name by `link()`, which never replaces a file: its path,
/// or what failed in words.
pub(super) async fn copy(
    pool: &SqlitePool,
    backups: &Path,
    which: &Db,
    from: i64,
    to: i64,
) -> Result<PathBuf, String> {
    let partial = backups.join(partial_name(which, from, to));
    // Created empty, `0600` from the start: `VACUUM INTO` fills an empty
    // file and keeps its mode.
    create_empty(&partial).map_err(|e| format!("{} cannot be created: {e}", partial.display()))?;
    let into = partial.to_string_lossy().into_owned();
    let filled = async {
        sqlx::query("VACUUM INTO ?1")
            .bind(&into)
            .execute(pool)
            .await
            .map_err(|e| format!("VACUUM INTO {into} failed: {e}"))?;
        super::super::clamp_0600(&partial)
            .map_err(|e| format!("{into} cannot be made 0600: {e}"))?;
        std::fs::File::open(&partial)
            .and_then(|f| f.sync_all())
            .map_err(|e| format!("{into} cannot be synced to disk: {e}"))
    }
    .await;
    if let Err(what) = filled {
        let left = match std::fs::remove_file(&partial) {
            Ok(()) => String::new(),
            Err(r) => format!(" (the unfinished file {into} is left: {r})"),
        };
        return Err(format!("{what}{left}"));
    }
    let path = publish(&partial, backups, which, from, to).map_err(|e| {
        let _ = std::fs::remove_file(&partial);
        format!(
            "the copy cannot be put under its name in {}: {e}",
            backups.display()
        )
    })?;
    if let Err(e) = std::fs::remove_file(&partial) {
        // The copy holds the same file; the next start removes the name.
        tracing::warn!(
            "{} cannot be removed after publishing it as {}: {e}",
            partial.display(),
            path.display()
        );
    }
    // The new name, durable.
    if let Err(e) = std::fs::File::open(backups).and_then(|d| d.sync_all()) {
        tracing::warn!(
            "the copy {} is made, but {} cannot be synced ({e}); after a power loss the copy's \
             name may be gone, and the next start copies the database again",
            path.display(),
            backups.display()
        );
    }
    Ok(path)
}

fn create_empty(path: &Path) -> std::io::Result<()> {
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    o.open(path).map(drop)
}

/// The partial under the first free name: `<stem>-<from>-<to>.<ext>`, then
/// `-2`, `-3` … (D105). `link()` fails on a name that is there, so no copy
/// is overwritten.
fn publish(partial: &Path, dir: &Path, which: &Db, from: i64, to: i64) -> std::io::Result<PathBuf> {
    let mut n = 1u64;
    loop {
        let path = dir.join(name(which, from, to, n));
        match std::fs::hard_link(partial, &path) {
            Ok(()) => return Ok(path),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => n += 1,
            Err(e) => return Err(e),
        }
    }
}

/// The start's refusal when the copy cannot be made (D106).
pub(super) async fn refusal(
    pool: &SqlitePool,
    db: &Path,
    backups: &Path,
    which: &Db,
    step: Step,
    what: String,
) -> anyhow::Error {
    // The backups dir when it is there, else the data dir it would be in.
    let place = if backups.is_dir() {
        backups
    } else {
        backups.parent().unwrap_or(backups)
    };
    let free = crate::backends::paths::free_bytes(place)
        .map_or_else(|| "cannot be read".to_string(), fmt_bytes);
    anyhow::anyhow!(
        "{} has a migration to apply ({:04} → {:04}), and a container start copies it first: \
         {what}. The start does not migrate without the copy, the only way back to the schema \
         the previous image opens. Free space at {}: {free}; the copy needs about the \
         database's size, {}. Free space on the data volume or fix the backups dir {}, then \
         start again; nothing was written to {}",
        db.display(),
        step.from,
        step.to,
        place.display(),
        database_size(pool).await,
        backups.display(),
        which.file,
    )
}

/// The database's size as SQLite counts it (its pages), in words.
async fn database_size(pool: &SqlitePool) -> String {
    let pages: Result<(i64, i64), sqlx::Error> = async {
        let count: i64 = sqlx::query_scalar("PRAGMA page_count")
            .fetch_one(pool)
            .await?;
        let size: i64 = sqlx::query_scalar("PRAGMA page_size")
            .fetch_one(pool)
            .await?;
        Ok((count, size))
    }
    .await;
    match pages {
        Ok((count, size)) => fmt_bytes(u64::try_from(count * size).unwrap_or(0)),
        Err(e) => format!("unknown ({e})"),
    }
}
