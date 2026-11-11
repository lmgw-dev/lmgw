//! Whether a start reuses the copy of its step that is there instead of
//! copying again (D113, D118, D119), and whether a new copy is made from a
//! database a start left inside its migrations (D118).

use std::path::{Path, PathBuf};

use lmgw_api_types::MigrationCopy;
use sqlx::SqlitePool;

use super::names::{found, newest_of_step};
use super::source::{self, Match, Source};
use super::{Db, Step, MAIN};
use crate::store::check_pending;

/// The newest copy of `step` this start reuses, with its record's
/// `part_applied`; `None` when the start copies. `now` is the database's
/// state as [`source::read`] found it, or why it cannot be read. Each reuse
/// is logged with its reason.
pub(super) async fn reusable(
    pool: &SqlitePool,
    db: &Path,
    backups: &Path,
    which: &Db,
    step: Step,
    now: &Result<Source, String>,
) -> Option<(PathBuf, bool)> {
    let Some(c) = newest_of_step(backups, which, step.from, step.to) else {
        if let Err(e) = now {
            tracing::warn!(
                "{} cannot be compared with an earlier copy ({e}); no copy of this step is in {} \
                 yet, so it is copied",
                db.display(),
                backups.display(),
            );
        }
        return None;
    };
    let path = PathBuf::from(&c.path);
    let record = source::read_record(&path);
    let part_applied = record.as_ref().is_some_and(|r| r.marks.part_applied);
    let s = match now {
        Ok(s) => s,
        Err(e) => {
            // Copying at every start this keeps failing at would bring back
            // the restart loop D113 is about (D119).
            tracing::warn!(
                "before migrating {} from {:04} to {:04}: whether the copy {} (made {} UTC) still \
                 matches the database cannot be told, since {} {e}; that copy is reused rather \
                 than copying the database again at every start this fails at, and if something \
                 wrote to the database since the copy was made, the copy does not hold it",
                which.file,
                step.from,
                step.to,
                path.display(),
                made(&c),
                db.display(),
            );
            return Some((path, part_applied));
        }
    };
    let verdict = record.as_ref().map_or(Match::Differs, |r| {
        r.compare(c.bytes, db, s, (step.from, step.to))
    });
    match verdict {
        Match::Same => {
            tracing::info!(
                "before migrating {} from {:04} to {:04}: the copy {} (made {} UTC) still matches \
                 the database, which nothing wrote to since the last start that made or reused \
                 the copy left it; not copied again",
                which.file,
                step.from,
                step.to,
                path.display(),
                made(&c),
            );
            Some((path, part_applied))
        }
        Match::Entered => {
            let marked = if which.file == MAIN.file {
                match check_pending(pool).await {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::warn!(
                            "the foreign-key check's marker in {} cannot be read ({e}); the \
                             database is copied again",
                            db.display()
                        );
                        false
                    }
                }
            } else {
                false
            };
            if marked {
                tracing::info!(
                    "before migrating {} from {:04} to {:04}: the copy {} (made {} UTC) is still \
                     the database before this step: the last start went on into the step's \
                     migrations, one of which runs without a transaction, and stopped inside \
                     them (killed by TimeoutStartSec, say), so the statements it committed \
                     changed the file's size or time; it is the same file, nothing else writes a \
                     database with a pending migration, and the foreign-key check's marker that \
                     start set is still there; not copied again (the database may hold a \
                     migration part applied, which the copy does not)",
                    which.file,
                    step.from,
                    step.to,
                    path.display(),
                    made(&c),
                );
                Some((path, part_applied))
            } else {
                tracing::info!(
                    "before migrating {} from {:04} to {:04}: the last start that left the copy {} \
                     went on into the step's migrations, but the foreign-key check's marker it \
                     set is gone, so a start finished migrating the database since (the \
                     previous image, say) and may have written to it: it is copied again",
                    which.file,
                    step.from,
                    step.to,
                    path.display(),
                );
                None
            }
        }
        Match::Differs => None,
    }
}

/// Whether a new copy for `step` is made from a database a start left
/// inside migrations that include one without a transaction, so it may
/// hold that one part applied (D118): a record beside a copy of this file
/// says such a start entered the migrator towards the same `<to>` from
/// this `<from>` or below, or (`lmgw.sqlite`) the foreign-key check's
/// marker a start sets before the migrator is still there.
pub(super) async fn part_applied(
    pool: &SqlitePool,
    db: &Path,
    backups: &Path,
    which: &Db,
    step: Step,
) -> bool {
    // The migration a stopped start left part applied is not recorded, so
    // it is among this step's.
    if !step.no_tx {
        return false;
    }
    if let Some(inode) = source::inode_of(db) {
        let (copies, _) = found(backups);
        let entered = copies
            .iter()
            .filter(|f| f.which.file == which.file)
            .filter_map(|f| source::read_record(Path::new(&f.copy.path)))
            .any(|r| r.entered_towards(db, inode, step.from, step.to));
        if entered {
            return true;
        }
    }
    which.file == MAIN.file && check_pending(pool).await.unwrap_or(false)
}

/// When `c` was made, for a log line.
fn made(c: &MigrationCopy) -> &str {
    if c.made_at.is_empty() {
        "at an unknown time"
    } else {
        &c.made_at
    }
}
