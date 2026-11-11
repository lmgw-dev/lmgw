//! The database copies a container start makes before it migrates
//! (headless server design ruling 14, §6.4), and the refusal of a database
//! a newer build migrated.
//!
//! **What is copied.** Each of the data dir's three databases that runs
//! migrations of its own at the start ([`DATABASES`]): `lmgw.sqlite`, and
//! `quickdoc.db` and `knowledge.db` beside it (D117). In container mode, a
//! start that finds a migration this build holds and a database has not
//! recorded copies that database with `VACUUM INTO` into the data dir's
//! `backups/` before anything writes to it: for `lmgw.sqlite` ahead of the
//! repairs (`repair_before_migrations` writes too), the notices and the
//! migrator, for the other two ahead of their migrator. A database with
//! nothing pending gets no copy, and neither does one this start creates
//! (no `_sqlx_migrations` table: nothing in it to restore, D107). The
//! desktop makes none, with accounts on or off (D25).
//!
//! **The name.** `<stem>-<from>-<to>.<ext>` (`lmgw-0101-0102.sqlite`,
//! `knowledge-0002-0003.db`): `<to>` is this build's newest migration of
//! that database, `<from>` the newest one the database records below the
//! first one this start applies (D105, D116). A taken name moves to `-2`,
//! `-3` … and no copy is ever overwritten.
//!
//! **How.** The read-only refusals come first, so a start they refuse makes
//! no copy (D113). The copy goes to a hidden partial file
//! (`.lmgw-0101-0102.sqlite.partial`), is synced, and is then published
//! under the first free name by `link()`, which never replaces a file; a
//! partial an earlier start left (killed while it copied) is removed and
//! logged (D114). Beside each copy a small `.source` file records the
//! database as the start that made or reused it left it (its inode, size
//! and modification time after the write-ahead log is folded in); the next
//! start with the same step reuses the newest copy while that still
//! matches, instead of copying again (D113), and also when the record says
//! the last start went on into a migration without a transaction and
//! stopped in there (D118) or the database cannot be read (D119). A copy
//! made from a database a start left in there may hold a migration part
//! applied and is never offered ahead of a whole one (D118).
//!
//! The copy is the only way back to the schema the previous image opens, so
//! a start whose copy fails does not migrate: it refuses, naming the cause,
//! the path, the free space there and the database's size (D106). lmgw
//! never deletes a copy and never restores one.
//!
//! **The refusal.** A database that records a migration this build does
//! not have was migrated by a newer (or a different) build; sqlx would
//! refuse it with `VersionMissing`, for `lmgw.sqlite` only after the
//! repairs ran. [`refuse_newer`] refuses it first, before anything writes,
//! and in container mode names the newest copy this build can open and
//! what restoring it loses (§6.4, D108).

use std::future::Future;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use lmgw_api_types::units::fmt_bytes;
use sqlx::migrate::Migrator;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::SqlitePool;

use super::{pending_versions, refuse_before_copy, table_exists, BeforeCopy};

mod make;
mod names;
mod newer;
mod reuse;
mod source;

pub use names::{migration_copies, CopyList};
pub(crate) use newer::refuse_newer;

/// The data dir's subdirectory the copies go to (`/data/backups` in the
/// image).
pub const BACKUPS_SUBDIR: &str = "backups";

/// Where `data_dir`'s copies go.
pub fn backups_dir(data_dir: &Path) -> PathBuf {
    data_dir.join(BACKUPS_SUBDIR)
}

/// A database a start migrates, and its copies' name.
#[derive(Debug)]
pub struct Db {
    /// Its file in the data dir.
    pub file: &'static str,
    /// The copies' `<stem>-<from>-<to>.<ext>`.
    stem: &'static str,
    ext: &'static str,
    migrator: fn() -> Migrator,
}

impl Db {
    fn migrator(&self) -> Migrator {
        (self.migrator)()
    }

    /// This build's newest migration of it.
    fn newest(&self) -> i64 {
        newest_of(&self.migrator())
    }
}

fn newest_of(m: &Migrator) -> i64 {
    m.migrations
        .iter()
        .filter(|m| m.migration_type.is_up_migration())
        .map(|m| m.version)
        .max()
        .unwrap_or(0)
}

fn main_migrator() -> Migrator {
    sqlx::migrate!("./migrations")
}

/// `lmgw.sqlite`: the gateway's database.
pub const MAIN: Db = Db {
    file: crate::state::fresh_install::DB_FILE,
    stem: "lmgw",
    ext: "sqlite",
    migrator: main_migrator,
};

/// `quickdoc.db`: the document corpora.
pub const CORPUS: Db = Db {
    file: crate::quickdoc::CORPUS_DB_FILE,
    stem: "quickdoc",
    ext: "db",
    migrator: quickdoc_core::store::migrator,
};

/// `knowledge.db`: the knowledge bases (chat-complete design §9.1).
pub const KNOWLEDGE: Db = Db {
    file: crate::knowledge::KNOWLEDGE_DB_FILE,
    stem: "knowledge",
    ext: "db",
    migrator: crate::knowledge::store::migrator,
};

/// Every database a start migrates, in the order it opens them.
pub const DATABASES: [&Db; 3] = [&MAIN, &CORPUS, &KNOWLEDGE];

/// A start's migration step on one database.
#[derive(Debug, Clone, Copy)]
struct Step {
    from: i64,
    to: i64,
    pending: usize,
    /// One of the pending migrations runs without a transaction (only
    /// `lmgw.sqlite` has such: 0002, 0008, 0013, 0018, 0034, 0058, 0086,
    /// 0090), so a start stopped inside the migrator may leave it part
    /// applied (D118).
    no_tx: bool,
}

/// The step `pool`'s database is to take; `None` with nothing pending, or
/// for a database this start creates (no bookkeeping table, D107).
async fn step(pool: &SqlitePool, which: &Db) -> anyhow::Result<Option<Step>> {
    if !table_exists(pool, "_sqlx_migrations").await? {
        return Ok(None);
    }
    let m = which.migrator();
    let pending = pending_versions(pool, &m).await?;
    let Some(&first) = pending.first() else {
        return Ok(None);
    };
    // Below the first gap: a database that recorded a later migration out
    // of order (a different build) is named by what it has whole (D116).
    let from: Option<i64> =
        sqlx::query_scalar("SELECT MAX(version) FROM _sqlx_migrations WHERE version < ?1")
            .bind(first)
            .fetch_one(pool)
            .await?;
    let no_tx = m
        .migrations
        .iter()
        .any(|x| x.no_tx && pending.contains(&x.version));
    Ok(Some(Step {
        from: from.unwrap_or(0),
        to: newest_of(&m),
        pending: pending.len(),
        no_tx,
    }))
}

/// What a container start's copy step did on `lmgw.sqlite`, for the
/// migration after it.
pub(super) struct Prepared {
    /// The refusals that ran ahead of the copy.
    pub(super) checked: BeforeCopy,
    /// The copy made or reused; `None` with nothing pending.
    pub(super) copied: Option<Copied>,
}

/// Container mode, `lmgw.sqlite`, before the repairs: refuse a database a
/// newer build migrated ([`refuse_newer`]), ask the refusals that only
/// read, then copy one with a pending migration into `backups` (or reuse
/// the copy that still matches it, D113).
pub(super) async fn before_migrating(
    pool: &SqlitePool,
    db: &Path,
    backups: &Path,
) -> anyhow::Result<Prepared> {
    refuse_newer(pool, &MAIN, Some(backups)).await?;
    let checked = refuse_before_copy(pool).await?;
    let copied = match step(pool, &MAIN).await? {
        Some(s) => Some(copy_or_reuse(pool, db, backups, &MAIN, s).await?),
        None => None,
    };
    Ok(Prepared { checked, copied })
}

/// `open` (which migrates `which` in `data_dir`, as `knowledge::store::open`
/// and `quickdoc_core::store::open` do), with `backups` (container mode)
/// after the copy step on that database: refused when a newer build
/// migrated it, copied (or the matching copy reused) when it has a pending
/// migration. Without `backups` it is `open` alone.
pub async fn around_open<T>(
    data_dir: &Path,
    which: &'static Db,
    backups: Option<&Path>,
    open: impl Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    let Some(backups) = backups else {
        return open.await;
    };
    let path = data_dir.join(which.file);
    let copied = if path.try_exists().unwrap_or(true) {
        let pool = plain_pool(&path).await?;
        let r = async {
            refuse_newer(&pool, which, Some(backups)).await?;
            match step(&pool, which).await? {
                Some(s) => copy_or_reuse(&pool, &path, backups, which, s)
                    .await
                    .map(Some),
                None => Ok(None),
            }
        }
        .await;
        pool.close().await;
        r?
    } else {
        // This start creates it: nothing to restore (D107).
        None
    };
    let migrating = Instant::now();
    match open.await {
        Ok(v) => {
            if let Some(c) = &copied {
                c.log_migrated(migrating.elapsed());
            }
            Ok(v)
        }
        Err(e) => {
            if let Some(c) = &copied {
                c.leaving_path().await;
            }
            Err(e)
        }
    }
}

/// An existing database, opened for the copy step alone: no migrator, no
/// file created.
async fn plain_pool(path: &Path) -> anyhow::Result<SqlitePool> {
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))?
        .create_if_missing(false)
        .journal_mode(SqliteJournalMode::Wal)
        .busy_timeout(super::BUSY_TIMEOUT);
    Ok(SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await?)
}

/// A copy this start made or reused, for the lines after it.
pub(super) struct Copied {
    which: &'static Db,
    /// The database copied.
    db: PathBuf,
    /// The copy.
    path: PathBuf,
    step: Step,
    /// The copy step's time (the copy's, or the check that reused one).
    took: Duration,
    /// Made from a database a start had left inside a migrator with a
    /// no-transaction migration (D118), kept in every record written.
    part_applied: bool,
    /// This start entered the migrator ([`Self::entering`]).
    entered: AtomicBool,
}

impl Copied {
    fn new(
        which: &'static Db,
        db: &Path,
        path: PathBuf,
        step: Step,
        took: Duration,
        part_applied: bool,
    ) -> Self {
        Self {
            which,
            db: db.to_path_buf(),
            path,
            step,
            took,
            part_applied,
            entered: AtomicBool::new(false),
        }
    }

    /// [`Self::leaving`] just before the migrator: from here on the record
    /// says this start entered the step's migrations, when one of them runs
    /// without a transaction (D118). A start killed inside such a one has
    /// committed statements after this record, and the next start with the
    /// same step reuses the copy all the same.
    pub(super) async fn entering(&self, pool: &SqlitePool) {
        self.entered.store(true, Ordering::Relaxed);
        self.leaving(pool).await;
    }

    /// Record the database as this start leaves it beside the copy, so the
    /// next start with the same step reuses the copy while nothing else
    /// wrote to it (D113): called just before the migrator and when the
    /// start fails. When it cannot be recorded, the record still holds an
    /// earlier state, which the database no longer matches: the next start
    /// copies again (unless the record says this start entered a migration
    /// without a transaction, D118, or the database cannot be read then
    /// either, D119).
    pub(super) async fn leaving(&self, pool: &SqlitePool) {
        let marks = source::Marks {
            entered: (self.step.no_tx && self.entered.load(Ordering::Relaxed))
                .then_some((self.step.from, self.step.to)),
            part_applied: self.part_applied,
        };
        match source::read(pool, &self.db).await {
            Ok(s) => {
                if let Err(e) = source::write(&self.path, &self.db, &s, marks) {
                    tracing::warn!(
                        "the record beside {} of the database as this start leaves it cannot \
                         be written ({e}); the next start may copy {} again",
                        self.path.display(),
                        self.which.file,
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    "{} as this start leaves it cannot be told apart from a later change \
                     ({e}); the next start may copy it again",
                    self.db.display(),
                );
            }
        }
    }

    /// [`Self::leaving`] when the pool that migrated is gone.
    async fn leaving_path(&self) {
        match plain_pool(&self.db).await {
            Ok(pool) => {
                self.leaving(&pool).await;
                pool.close().await;
            }
            Err(e) => {
                tracing::warn!(
                    "{} cannot be opened to record it as this start leaves it ({e}); the next \
                     start copies it again",
                    self.db.display(),
                );
            }
        }
    }

    /// The migrations' line: their time, and the copy step's and theirs
    /// together against the unit's start timeout.
    pub(super) fn log_migrated(&self, took: Duration) {
        let Step {
            from, to, pending, ..
        } = self.step;
        tracing::info!(
            "migrated {} from {from:04} to {to:04} ({pending} migration{}) in {:.1} s; with the \
             copy step ({}) this start spent {:.1} s on it before serving, which the server \
             unit's TimeoutStartSec must cover",
            self.which.file,
            if pending == 1 { "" } else { "s" },
            took.as_secs_f64(),
            self.path.display(),
            (self.took + took).as_secs_f64(),
        );
    }
}

/// The copy step on one database: the backups dir ready, the newest copy
/// of the same step reused while it still matches the database (or the
/// database is the one a start stopped migrating, or cannot be read,
/// D118, D119), otherwise a new copy; or the refusal (D106).
async fn copy_or_reuse(
    pool: &SqlitePool,
    db: &Path,
    backups: &Path,
    which: &'static Db,
    step: Step,
) -> anyhow::Result<Copied> {
    let started = Instant::now();
    if let Err(what) = make::prepare_dir(backups, which, db) {
        return Err(make::refusal(pool, db, backups, which, step, what).await);
    }
    let now = source::read(pool, db).await;
    if let Some((path, part_applied)) = reuse::reusable(pool, db, backups, which, step, &now).await
    {
        return Ok(Copied::new(
            which,
            db,
            path,
            step,
            started.elapsed(),
            part_applied,
        ));
    }
    let part_applied = reuse::part_applied(pool, db, backups, which, step).await;
    let path = match make::copy(pool, backups, which, step.from, step.to).await {
        Ok(p) => p,
        Err(what) => return Err(make::refusal(pool, db, backups, which, step, what).await),
    };
    let took = started.elapsed();
    if let Ok(s) = &now {
        let marks = source::Marks {
            entered: None,
            part_applied,
        };
        if let Err(e) = source::write(&path, db, s, marks) {
            tracing::warn!(
                "the record beside {} of the database it was made from cannot be written ({e}); \
                 a start refused after this one may copy the database again",
                path.display()
            );
        }
    }
    let size = match std::fs::metadata(&path) {
        Ok(m) => fmt_bytes(m.len()),
        Err(e) => format!("its size cannot be read: {e}"),
    };
    tracing::info!(
        "before migrating {} from {:04} to {:04}: copied it to {} ({size}) in {:.1} s; a manual \
         restore source for the image whose newest migration of it is {:04} (restoring it loses \
         everything written after it), never deleted or restored by lmgw (Settings → About \
         lists the copies){}",
        which.file,
        step.from,
        step.to,
        path.display(),
        took.as_secs_f64(),
        step.from,
        if part_applied {
            "; a start stopped inside this step's migrations on the database before, so it may \
             hold one of them part applied, and a whole copy of the same schema is offered ahead \
             of it"
        } else {
            ""
        },
    );
    Ok(Copied::new(which, db, path, step, took, part_applied))
}
