//! The migration copy's second review round (headless server design D118–
//! D120): a start stopped inside a migration that runs without a
//! transaction reuses the copy made before it although the file moved on,
//! a copy made from such a database is never offered ahead of a whole one
//! of the same `<from>`, a database whose state cannot be read reuses the
//! copy that is there, and a linked or setgid backups dir.

use std::path::Path;
use std::time::{Duration, SystemTime};

use lmgw_core::store;

use super::{alias_refused_db, copies, db_at, files, newest, plain};
use crate::common::captured_log::capture_log;

/// The foreign-key check's marker a start sets ahead of the migrator.
const MARKER: &str = "store:foreign_key_check_pending";

fn record(backups: &Path, copy: &str) -> String {
    std::fs::read_to_string(backups.join(format!("{copy}.source"))).unwrap()
}

/// 0086 runs without a transaction and rebuilds `hf_models`: a view on
/// the old table makes its rename fail after it dropped that table (SQLite
/// checks the views at a rename), so the database is left part applied, as a
/// start killed in there leaves it. The statements such a start commits
/// after its record are stood in for by a write; the next start with the
/// same step reuses the copy. Once the marker is gone (a start finished
/// migrating it since, the previous image say) it copies again, and that
/// copy, marked part applied, is not the one a newer build's refusal
/// offers (D118).
#[tokio::test]
async fn a_start_stopped_inside_a_migration_without_a_transaction_reuses_its_copy() {
    let dir = tempfile::tempdir().unwrap();
    let backups = store::backups_dir(dir.path());
    let db = db_at(dir.path(), 85).await;
    let pool = plain(&db).await;
    sqlx::query("CREATE VIEW test_reads_hf_models AS SELECT id FROM hf_models")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    let base = format!("lmgw-0085-{:04}", newest());
    let first = format!("{base}.sqlite");
    let entered = format!("migrator_entered 0085-{:04}", newest());

    let (log, _guard) = capture_log();
    let e = store::open_for_start(&db, Some(&backups))
        .await
        .unwrap_err();
    assert!(format!("{e:#}").contains("hf_models"), "{e:#}");
    assert_eq!(copies(&backups), std::slice::from_ref(&first));
    let r = record(&backups, &first);
    assert!(r.contains(&entered) && !r.contains("part_applied"), "{r}");
    let pool = plain(&db).await;
    let old_table: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'hf_models'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(old_table, 0, "0086 stopped part applied");
    sqlx::query("INSERT INTO settings (key, value) VALUES ('test:after-the-record', '1')")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;

    // The same file at the same step, changed since the record: reused.
    store::open_for_start(&db, Some(&backups))
        .await
        .unwrap_err();
    assert_eq!(copies(&backups), std::slice::from_ref(&first));
    assert_eq!(
        log.text()
            .matches("is still the database before this step")
            .count(),
        1,
        "{}",
        log.text()
    );
    // The copy is the database before the first start.
    let copied = store::open_read_only(&backups.join(&first)).await.unwrap();
    let whole: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'hf_models'",
    )
    .fetch_one(&copied)
    .await
    .unwrap();
    assert_eq!(whole, 1);
    copied.close().await;

    // A start finished migrating it since and cleared the marker: copied
    // again, marked part applied.
    let pool = plain(&db).await;
    sqlx::query("DELETE FROM settings WHERE key = ?1")
        .bind(MARKER)
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    store::open_for_start(&db, Some(&backups))
        .await
        .unwrap_err();
    let second = format!("{base}-2.sqlite");
    assert_eq!(copies(&backups), [second.clone(), first.clone()]);
    let text = log.text();
    assert!(
        text.contains("the foreign-key check's marker it set is gone")
            && text.contains("so it may hold one of them part applied"),
        "{text}"
    );
    assert!(
        record(&backups, &second).contains("part_applied yes"),
        "{}",
        record(&backups, &second)
    );

    // A newer build's database: the whole copy is named, though the part
    // applied one of the same <from> is newer.
    let pool = plain(&db).await;
    sqlx::query(
        "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time)
         VALUES (9999, 'a newer build', 1, x'00', 0)",
    )
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;
    let e = store::open_for_start(&db, Some(&backups))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        e.contains(&format!(
            "restore the copy made before its migration: {} (",
            backups.join(&first).display()
        )) && !e.contains("part applied"),
        "{e}"
    );
}

/// Set `db`'s modification time before 1970, which the reuse check cannot
/// read as a state.
fn unreadable_time(db: &Path) {
    let before = SystemTime::UNIX_EPOCH
        .checked_sub(Duration::from_secs(60))
        .unwrap();
    std::fs::File::options()
        .write(true)
        .open(db)
        .unwrap()
        .set_modified(before)
        .unwrap();
}

/// A database whose state cannot be read reuses the copy of its step that
/// is there, with a warn line saying why, rather than copying at every
/// start this fails at; with no copy of the step yet it is copied (D119).
#[tokio::test]
async fn a_database_that_cannot_be_read_reuses_the_copy_that_is_there() {
    let dir = tempfile::tempdir().unwrap();
    let backups = store::backups_dir(dir.path());
    let (db, _) = alias_refused_db(dir.path()).await;
    let first = format!("lmgw-0017-{:04}.sqlite", newest());
    let (log, _guard) = capture_log();
    store::open_for_start(&db, Some(&backups))
        .await
        .unwrap_err();
    assert_eq!(copies(&backups), std::slice::from_ref(&first));

    unreadable_time(&db);
    store::open_for_start(&db, Some(&backups))
        .await
        .unwrap_err();
    assert_eq!(
        copies(&backups),
        std::slice::from_ref(&first),
        "not copied again"
    );
    let text = log.text();
    assert!(
        text.contains(&format!(
            "whether the copy {} (made ",
            backups.join(&first).display()
        )) && text.contains("its modification time is before 1970")
            && text.contains("that copy is reused rather than copying the database again"),
        "{text}"
    );

    // No copy of the step yet: copied.
    let other = tempfile::tempdir().unwrap();
    let backups = store::backups_dir(other.path());
    let db = db_at(other.path(), newest() - 1).await;
    unreadable_time(&db);
    store::open_for_start(&db, Some(&backups))
        .await
        .unwrap()
        .close()
        .await;
    assert_eq!(
        copies(&backups),
        [format!("lmgw-{:04}-{:04}.sqlite", newest() - 1, newest())]
    );
    assert!(
        log.text().contains("no copy of this step is in"),
        "{}",
        log.text()
    );
}

fn full_mode(p: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).unwrap().permissions().mode() & 0o7777
}

/// A backups path that is a link is used, its target's mode left as it is
/// with a warn line; a setgid bit an administrator set stays when lmgw
/// tightens the dir (D120).
#[tokio::test]
async fn a_linked_backups_dir_keeps_its_mode_and_a_setgid_bit_stays() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let target = elsewhere.path().join("lmgw-copies");
    std::fs::create_dir(&target).unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
    let backups = store::backups_dir(dir.path());
    std::os::unix::fs::symlink(&target, &backups).unwrap();
    let db = db_at(dir.path(), newest() - 1).await;
    let (log, _guard) = capture_log();
    store::open_for_start(&db, Some(&backups))
        .await
        .unwrap()
        .close()
        .await;
    let base = format!("lmgw-{:04}-{:04}.sqlite", newest() - 1, newest());
    assert_eq!(files(&target), [base.clone(), format!("{base}.source")]);
    assert_eq!(full_mode(&target), 0o755);
    assert!(
        log.text().contains(&format!(
            "the backups dir {} is a link to {}, which is 0755, open to others",
            backups.display(),
            target.display()
        )),
        "{}",
        log.text()
    );

    let other = tempfile::tempdir().unwrap();
    let backups = store::backups_dir(other.path());
    std::fs::create_dir(&backups).unwrap();
    std::fs::set_permissions(&backups, std::fs::Permissions::from_mode(0o2775)).unwrap();
    let db = db_at(other.path(), newest() - 1).await;
    store::open_for_start(&db, Some(&backups))
        .await
        .unwrap()
        .close()
        .await;
    assert_eq!(full_mode(&backups), 0o2700);
    assert!(
        log.text()
            .contains("was 2775, open to others; it is 2700 now"),
        "{}",
        log.text()
    );
}
