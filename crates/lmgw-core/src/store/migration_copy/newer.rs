//! The refusal of a database a newer (or a different) build migrated
//! (D108): before anything writes, naming the newest copy this build can
//! open in container mode and on the command line's offline path.

use std::collections::HashSet;
use std::path::Path;

use lmgw_api_types::units::fmt_bytes;
use sqlx::SqlitePool;

use super::names::found;
use super::{source, table_exists, Db};

/// Refuse `which`'s database (open as `pool`) when it records a migration
/// this build does not have, before anything writes to it. With `backups`
/// (container mode, the command line's offline path) the refusal names
/// the newest copy this build can open and what restoring it loses.
pub(crate) async fn refuse_newer(
    pool: &SqlitePool,
    which: &Db,
    backups: Option<&Path>,
) -> anyhow::Result<()> {
    if !table_exists(pool, "_sqlx_migrations").await? {
        return Ok(());
    }
    let recorded: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations ORDER BY version")
            .fetch_all(pool)
            .await?;
    let known: HashSet<i64> = which
        .migrator()
        .migrations
        .iter()
        .filter(|m| m.migration_type.is_up_migration())
        .map(|m| m.version)
        .collect();
    let unknown: Vec<i64> = recorded
        .into_iter()
        .filter(|v| !known.contains(v))
        .collect();
    if unknown.is_empty() {
        return Ok(());
    }
    anyhow::bail!("{}", sentence(which, &unknown, which.newest(), backups))
}

/// The refusal's words: which migrations, this build's newest, and the
/// ways back.
fn sentence(which: &Db, unknown: &[i64], newest: i64, backups: Option<&Path>) -> String {
    let list = unknown
        .iter()
        .map(|v| format!("{v:04}"))
        .collect::<Vec<_>>()
        .join(", ");
    // A migration below this build's newest that it does not hold comes
    // from another line of builds, not a later one.
    let by = if unknown.iter().all(|v| *v > newest) {
        "a newer lmgw"
    } else {
        "a newer or different build of lmgw"
    };
    let head = format!(
        "{} was migrated by {by}: it records migration{} {list}, which this build ({}, newest \
         migration {newest:04}) does not have, so it cannot open it",
        which.file,
        if unknown.len() == 1 { "" } else { "s" },
        env!("CARGO_PKG_VERSION"),
    );
    let Some(dir) = backups else {
        return format!("{head}. Run that lmgw on it again");
    };
    // The newest copy this build opens: made from its newest migration or
    // an older one (which its start migrates forward); among those of one
    // `<from>`, a whole one ahead of one that may hold a migration part
    // applied (D118), then the newest by the file's time, then by the
    // name's number.
    let (copies, error) = found(dir);
    let copy = copies
        .into_iter()
        .filter(|f| f.which.file == which.file && f.copy.from <= newest)
        .map(|f| {
            let part =
                source::read_record(Path::new(&f.copy.path)).is_some_and(|r| r.marks.part_applied);
            (f, part)
        })
        .max_by_key(|(f, part)| (f.copy.from, !*part, f.modified, f.n))
        .map(|(f, part)| (f.copy, part));
    let unread = error
        .as_deref()
        .map(|e| format!(" ({e})"))
        .unwrap_or_default();
    match copy {
        Some((c, part)) => format!(
            "{head}. Start that image again, or restore the copy made before its migration: {} \
             ({}, made {} UTC, migration {:04}'s schema{}{}). Restoring it loses everything \
             written to the database since it was made, the other image's migrations included: \
             with the service stopped, put it in place of {} and remove the -wal and -shm files \
             beside it. lmgw never restores a copy by itself{unread}",
            c.path,
            fmt_bytes(c.bytes),
            if c.made_at.is_empty() {
                "at an unknown time"
            } else {
                &c.made_at
            },
            c.from,
            if c.from < newest {
                ", which this build migrates forward at its start"
            } else {
                ", this build's"
            },
            if part {
                "; made from a database a start had stopped migrating part way, so it may hold \
                 a migration part applied, and no whole copy of this schema is there"
            } else {
                ""
            },
            which.file,
        ),
        None => match error {
            Some(e) => format!(
                "{head}. Start that image again; no copy this build can open can be named, \
                 since {e}"
            ),
            None => format!(
                "{head}. Start that image again; no copy this build can open is in {} (a \
                 container start copies the database there before it migrates)",
                dir.display()
            ),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::super::MAIN;
    use super::*;

    #[test]
    fn the_refusal_without_a_backups_dir_names_the_migrations() {
        let s = sentence(&MAIN, &[103, 104], 102, None);
        assert!(
            s.contains(
                "lmgw.sqlite was migrated by a newer lmgw: it records migrations 0103, 0104"
            ) && s.contains("newest migration 0102")
                && s.ends_with("Run that lmgw on it again"),
            "{s}"
        );
    }

    /// A migration below this build's newest that it does not hold: not
    /// a newer build's, another line's.
    #[test]
    fn an_unknown_migration_below_the_newest_is_a_different_build() {
        let s = sentence(&MAIN, &[90], 102, None);
        assert!(
            s.contains(
                "was migrated by a newer or different build of lmgw: it records migration 0090"
            ),
            "{s}"
        );
    }
}
