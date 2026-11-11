//! The copy before a container start migrates (headless server design
//! ruling 14, §6.4, §12 "Migrations copy"), through `start::prepare` on a
//! data dir whose database this build's migrations left one step behind;
//! and the command line's offline path refusing a database a newer build
//! migrated, naming that copy (D108).

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use lmgw_core::state::fresh_install::DB_FILE;
use lmgw_core::store;
use lmgw_server::account::{self, Passwords};
use lmgw_server::cli::Account;
use lmgw_server::start;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use tokio::sync::mpsc;

use crate::support::{container_env, host, recorder, EmptyPodman};

/// This build's newest migration.
fn newest() -> i64 {
    sqlx::migrate!("../lmgw-core/migrations")
        .migrations
        .iter()
        .map(|m| m.version)
        .max()
        .unwrap()
}

/// `data`'s database, migrated to one before this build's newest, closed.
async fn one_behind(data: &Path) -> PathBuf {
    let path = data.join(DB_FILE);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))
                .unwrap()
                .create_if_missing(true)
                .journal_mode(SqliteJournalMode::Wal),
        )
        .await
        .unwrap();
    let mut m = sqlx::migrate!("../lmgw-core/migrations");
    let behind = newest() - 1;
    m.migrations = m
        .migrations
        .iter()
        .filter(|x| x.version <= behind)
        .cloned()
        .collect::<Vec<_>>()
        .into();
    m.run(&pool).await.unwrap();
    pool.close().await;
    path
}

/// A container-mode start on `dir`, ready to serve, the lock held until
/// the answer drops; or why it did not start.
async fn prepare(dir: &Path, dev: bool) -> anyhow::Result<start::Ready> {
    let (exit, _) = recorder();
    let (_signals, mut rx) = mpsc::channel(2);
    let ready = start::prepare(
        &container_env(dir, dev),
        host(&Arc::new(EmptyPodman::default())),
        &mut rx,
        &exit,
    )
    .await?;
    Ok(ready.expect("no signal: ready"))
}

struct NoPassword;

impl Passwords for NoPassword {
    fn read(&mut self, prompt: &str) -> Result<String, String> {
        panic!("asked for a password ({prompt})")
    }
}

#[tokio::test]
async fn a_container_start_with_a_pending_migration_copies_the_database_first() {
    let dir = tempfile::tempdir().unwrap();
    one_behind(dir.path()).await;
    let ready = prepare(dir.path(), false).await.unwrap();
    let file = format!("lmgw-{:04}-{:04}.sqlite", newest() - 1, newest());
    let backups = store::backups_dir(dir.path());
    let listed = store::migration_copies(&backups).copies;
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0].file, file);
    let copy = store::open_read_only(&backups.join(&file)).await.unwrap();
    let copied: i64 = sqlx::query_scalar("SELECT MAX(version) FROM _sqlx_migrations")
        .fetch_one(&copy)
        .await
        .unwrap();
    assert_eq!(
        copied,
        newest() - 1,
        "the database as the older image left it"
    );
    copy.close().await;
    let migrated: i64 = sqlx::query_scalar("SELECT MAX(version) FROM _sqlx_migrations")
        .fetch_one(&ready.state.db)
        .await
        .unwrap();
    assert_eq!(migrated, newest());
    ready.state.db.close().await;
    drop(ready);

    // The next start has nothing pending: no second copy, the first kept.
    let again = prepare(dir.path(), false).await.unwrap();
    assert_eq!(store::migration_copies(&backups).copies, listed);
    again.state.db.close().await;
}

/// The dev instance in container mode copies too: container mode is the
/// gate, not production (D25).
#[tokio::test]
async fn a_dev_instance_in_container_mode_copies_as_well() {
    let dir = tempfile::tempdir().unwrap();
    one_behind(dir.path()).await;
    let ready = prepare(dir.path(), true).await.unwrap();
    assert_eq!(
        store::migration_copies(&store::backups_dir(dir.path()))
            .copies
            .len(),
        1
    );
    ready.state.db.close().await;
}

/// The offline command line refuses a database a newer build migrated, as
/// the start does, naming the copy made before that migration.
#[tokio::test]
async fn offline_a_newer_database_is_refused_naming_the_copy() {
    let dir = tempfile::tempdir().unwrap();
    one_behind(dir.path()).await;
    let ready = prepare(dir.path(), false).await.unwrap();
    sqlx::query(
        "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time)
         VALUES (9999, 'a newer build', 1, x'00', 0)",
    )
    .execute(&ready.state.db)
    .await
    .unwrap();
    ready.state.db.close().await;
    drop(ready);
    let copy = store::backups_dir(dir.path()).join(format!(
        "lmgw-{:04}-{:04}.sqlite",
        newest() - 1,
        newest()
    ));
    let e = account::run(Account::List, dir.path(), &mut NoPassword)
        .await
        .unwrap_err();
    assert!(
        e.contains("migrated by a newer lmgw: it records migration 9999")
            && e.contains(&format!("{} (", copy.display()))
            && e.contains("loses everything written to the database since it was made"),
        "{e}"
    );

    // And the start refuses it the same way, before it writes anything.
    let e = prepare(dir.path(), false).await.err().unwrap();
    assert!(
        format!("{e:#}").contains("migrated by a newer lmgw"),
        "{e:#}"
    );
    assert_eq!(
        store::migration_copies(&store::backups_dir(dir.path()))
            .copies
            .len(),
        1,
        "no copy of a database it refuses"
    );
}
