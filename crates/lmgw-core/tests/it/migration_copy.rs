//! The copy before a container start migrates (headless server design
//! ruling 14, §6.4, §12 "Migrations copy"): a start with a pending
//! migration copies the database into the data dir's `backups/` before the
//! repairs and logs its size and time; none without one; none on the
//! desktop, with accounts on or off; no copy deleted or restored by lmgw;
//! a copy that cannot be made refuses the migration; and a database a newer
//! build migrated is refused naming the newest copy this build opens. The
//! review round's (D113–D117): no copy for a start a read-only guard
//! refuses, one copy for a start refused after it until the database
//! changes, a partial removed, ties broken by time, what cannot be read
//! said so, and `knowledge.db` and `quickdoc.db` copied as well.
//!
//! The old databases are real ones: this build's migrations run up to a
//! version and no further, as `migrations::db_at_version` does, on a file.

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use lmgw_core::config::deployment::{ContainerEnv, Deployment, Start};
use lmgw_core::state::{fresh_install, AppState, SharedState};
use lmgw_core::store;
use lmgw_core::vram::nvml::NoTelemetry;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::SqlitePool;

use crate::common::captured_log::capture_log;
use crate::common::serve;
use crate::support::decision_world::RecordingPodman;

/// This build's newest migration.
pub(crate) fn newest() -> i64 {
    sqlx::migrate!("./migrations")
        .migrations
        .iter()
        .map(|m| m.version)
        .max()
        .unwrap()
}

/// `data`'s database, migrated up to `version` and no further, closed.
pub(crate) async fn db_at(data: &Path, version: i64) -> PathBuf {
    std::fs::create_dir_all(data).unwrap();
    let path = data.join(fresh_install::DB_FILE);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))
                .unwrap()
                .create_if_missing(true)
                .journal_mode(SqliteJournalMode::Wal)
                .foreign_keys(true),
        )
        .await
        .unwrap();
    let mut m = sqlx::migrate!("./migrations");
    m.migrations = m
        .migrations
        .iter()
        .filter(|x| x.version <= version)
        .cloned()
        .collect::<Vec<_>>()
        .into();
    m.run(&pool).await.unwrap();
    pool.close().await;
    path
}

async fn newest_recorded(pool: &SqlitePool) -> i64 {
    sqlx::query_scalar("SELECT MAX(version) FROM _sqlx_migrations")
        .fetch_one(pool)
        .await
        .unwrap()
}

/// The files in `dir`, sorted; none when it is not there.
pub(crate) fn files(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

pub(crate) fn mode(p: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).unwrap().permissions().mode() & 0o777
}

async fn insert_upstream(pool: &SqlitePool, name: &str, port: u16) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO upstreams (name, protocol, kind, base_url, expose_all)
         VALUES (?1, 'openai', 'llama_server', ?2, 1) RETURNING id",
    )
    .bind(name)
    .bind(format!("http://127.0.0.1:{port}/v1"))
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn name_of(pool: &SqlitePool, id: i64) -> String {
    sqlx::query_scalar("SELECT name FROM upstreams WHERE id = ?1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// A start through the real initialisation, on fake host seams.
async fn start(data: &Path, deployment: Deployment) -> anyhow::Result<SharedState> {
    AppState::init_started_with_host(
        data.to_path_buf(),
        false,
        Start {
            deployment,
            env: ContainerEnv::default(),
        },
        Arc::new(RecordingPodman::default()),
        Arc::new(NoTelemetry("test: no GPU".into())),
        Arc::new(lmgw_machine::fake::FakeFacts::new()),
    )
    .await
}

/// The copy holds the database as it was before the start touched it: the
/// 0018 repair (which renames an owner's `llama-aux` row before the
/// migrator) has not run on it, while the migrated database has it.
#[tokio::test]
async fn a_pending_migration_is_copied_before_the_repairs_and_logged() {
    let dir = tempfile::tempdir().unwrap();
    let db = db_at(dir.path(), 17).await;
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect(&format!("sqlite://{}", db.display()))
        .await
        .unwrap();
    insert_upstream(&pool, "llama-embed", 18081).await;
    let owners = insert_upstream(&pool, "llama-aux", 9999).await;
    pool.close().await;

    let (log, _guard) = capture_log();
    let backups = store::backups_dir(dir.path());
    let pool = store::open_for_start(&db, Some(&backups)).await.unwrap();
    let file = format!("lmgw-0017-{:04}.sqlite", newest());
    assert_eq!(files(&backups), [file.clone(), format!("{file}.source")]);
    let copy = backups.join(&file);
    assert_eq!(mode(&copy), 0o600, "the copy holds what the database holds");
    assert_eq!(mode(&backups), 0o700);

    // The migrated database, repaired; the copy, untouched by the repair.
    assert_eq!(newest_recorded(&pool).await, newest());
    assert_eq!(name_of(&pool, owners).await, "llama-aux-conflict");
    let copied = store::open_read_only(&copy).await.unwrap();
    assert_eq!(newest_recorded(&copied).await, 17);
    assert_eq!(
        name_of(&copied, owners).await,
        "llama-aux",
        "before the repair"
    );
    copied.close().await;

    let text = log.text();
    assert!(
        text.contains(&format!(
            "before migrating lmgw.sqlite from 0017 to {:04}: copied it to {}",
            newest(),
            copy.display()
        )) && text.contains(" KiB) in ")
            && text.contains("never deleted or restored by lmgw"),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "migrated lmgw.sqlite from 0017 to {:04} (",
            newest()
        )) && text.contains("TimeoutStartSec"),
        "{text}"
    );
    pool.close().await;
}

/// A fresh database has nothing to restore, and one with nothing pending
/// gets no copy; the copy an earlier start made stays as it was.
#[tokio::test]
async fn no_copy_without_a_pending_migration_and_none_ever_deleted() {
    let dir = tempfile::tempdir().unwrap();
    let backups = store::backups_dir(dir.path());
    let db = dir.path().join(fresh_install::DB_FILE);
    store::open_for_start(&db, Some(&backups))
        .await
        .unwrap()
        .close()
        .await;
    assert!(!backups.exists(), "a database this start created: no copy");

    let dir = tempfile::tempdir().unwrap();
    let backups = store::backups_dir(dir.path());
    let db = db_at(dir.path(), newest() - 1).await;
    store::open_for_start(&db, Some(&backups))
        .await
        .unwrap()
        .close()
        .await;
    let made = files(&backups);
    assert_eq!(made.len(), 2, "the copy and its record: {made:?}");
    let copy = backups.join(&made[0]);
    let bytes = std::fs::read(&copy).unwrap();
    for _ in 0..2 {
        store::open_for_start(&db, Some(&backups))
            .await
            .unwrap()
            .close()
            .await;
    }
    assert_eq!(files(&backups), made, "nothing pending: no copy");
    assert_eq!(std::fs::read(&copy).unwrap(), bytes, "kept as it was made");
}

/// An earlier start's copy under the same name (a migration that failed
/// after it, then the old image again and this one once more) is never
/// overwritten: the next copy takes `-2`.
#[tokio::test]
async fn a_taken_name_moves_to_the_next_number() {
    let dir = tempfile::tempdir().unwrap();
    let backups = store::backups_dir(dir.path());
    let db = db_at(dir.path(), newest() - 1).await;
    std::fs::create_dir_all(&backups).unwrap();
    let base = format!("lmgw-{:04}-{:04}", newest() - 1, newest());
    std::fs::write(backups.join(format!("{base}.sqlite")), "an earlier copy").unwrap();
    store::open_for_start(&db, Some(&backups))
        .await
        .unwrap()
        .close()
        .await;
    assert_eq!(
        files(&backups),
        [
            format!("{base}-2.sqlite"),
            format!("{base}-2.sqlite.source"),
            format!("{base}.sqlite")
        ]
    );
    assert_eq!(
        std::fs::read_to_string(backups.join(format!("{base}.sqlite"))).unwrap(),
        "an earlier copy"
    );
    let listed: Vec<String> = store::migration_copies(&backups)
        .copies
        .into_iter()
        .map(|c| c.file)
        .collect();
    assert_eq!(
        listed,
        [format!("{base}-2.sqlite"), format!("{base}.sqlite")],
        "newest first"
    );
}

/// A copy that cannot be made refuses the start before anything writes:
/// the cause, the path, the free space, the database's size.
#[tokio::test]
async fn a_copy_that_cannot_be_made_refuses_the_migration() {
    let dir = tempfile::tempdir().unwrap();
    let backups = store::backups_dir(dir.path());
    let db = db_at(dir.path(), 17).await;
    // A file where the dir goes: refused for root too.
    std::fs::write(&backups, "not a dir").unwrap();
    let e = store::open_for_start(&db, Some(&backups))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        e.contains(&format!(
            "the backups dir {} cannot be created",
            backups.display()
        )) && e.contains(&format!("Free space at {}: ", dir.path().display()))
            && e.contains("the copy needs about the database's size")
            && e.contains("does not migrate without the copy")
            && e.contains("nothing was written to lmgw.sqlite"),
        "{e}"
    );
    let pool = store::open_read_only(&db).await.unwrap();
    assert_eq!(newest_recorded(&pool).await, 17, "not migrated");
    pool.close().await;
}

/// The desktop makes no copy, with accounts off (the default) or on.
#[tokio::test]
async fn the_desktop_makes_no_copy_with_accounts_on_or_off() {
    for accounts in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let db = db_at(dir.path(), newest() - 1).await;
        if accounts {
            let pool = SqlitePoolOptions::new()
                .max_connections(1)
                .connect(&format!("sqlite://{}", db.display()))
                .await
                .unwrap();
            let mut s = store::load_settings(&pool).await.unwrap();
            s.accounts.enabled = true;
            store::save_settings(&pool, &s).await.unwrap();
            pool.close().await;
        }
        let state = start(dir.path(), Deployment::Desktop).await.unwrap();
        assert_eq!(state.snapshot().settings.accounts.enabled, accounts);
        assert_eq!(newest_recorded(&state.db).await, newest(), "migrated");
        assert!(
            !store::backups_dir(dir.path()).exists(),
            "accounts {accounts}: no copy"
        );
        state.db.close().await;
    }
}

/// A container start makes the copy through the real initialisation, and
/// the settings answer lists it; the desktop's lists none.
#[tokio::test]
async fn the_settings_answer_lists_the_copies_in_container_mode() {
    let dir = tempfile::tempdir().unwrap();
    db_at(dir.path(), newest() - 1).await;
    let state = start(dir.path(), Deployment::Container).await.unwrap();
    let gw = serve(state.clone()).await;
    let full: lmgw_api_types::SettingsFull = gw
        .client()
        .get(format!("{gw}/api/settings-full"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let [copy] = full.migration_copies.as_slice() else {
        panic!("{:?}", full.migration_copies);
    };
    let backups = store::backups_dir(&state.data_dir);
    assert_eq!(
        copy.file,
        format!("lmgw-{:04}-{:04}.sqlite", newest() - 1, newest())
    );
    assert_eq!(copy.path, backups.join(&copy.file).display().to_string());
    assert_eq!((copy.from, copy.to), (newest() - 1, newest()));
    assert_eq!(copy.database, "lmgw.sqlite");
    assert_eq!(full.migration_copies_error, None);
    assert_eq!(
        copy.bytes,
        std::fs::metadata(backups.join(&copy.file)).unwrap().len()
    );
    assert!(copy.bytes > 0 && copy.made_at.len() == 19, "{copy:?}");

    let desktop = AppState::init_for_tests().await.unwrap();
    let full: lmgw_api_types::SettingsFull = {
        let gw = serve(desktop).await;
        gw.client()
            .get(format!("{gw}/api/settings-full"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    };
    assert!(full.migration_copies.is_empty());
}

/// A database a newer build migrated: refused before anything writes, the
/// newest copy this build opens named with what restoring it loses; the
/// desktop's refusal names the migrations; the command line's offline open
/// refuses as the start does.
#[tokio::test]
async fn a_newer_database_is_refused_naming_the_newest_copy() {
    let dir = tempfile::tempdir().unwrap();
    let backups = store::backups_dir(dir.path());
    let db = db_at(dir.path(), newest() - 1).await;
    let pool = store::open_for_start(&db, Some(&backups)).await.unwrap();
    // A newer build's migration, recorded as sqlx records one.
    sqlx::query(
        "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time)
         VALUES (9999, 'a newer build', 1, x'00', 0)",
    )
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;
    // An older copy too: the newest this build opens is the one named.
    std::fs::write(backups.join("lmgw-0050-0060.sqlite"), "older").unwrap();
    let before = files(&backups);
    let copy = backups.join(format!("lmgw-{:04}-{:04}.sqlite", newest() - 1, newest()));

    let e = store::open_for_start(&db, Some(&backups))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("lmgw.sqlite was migrated by a newer lmgw: it records migration 9999")
            && e.contains(&format!("newest migration {:04}", newest()))
            && e.contains(&format!(
                "restore the copy made before its migration: {} (",
                copy.display()
            ))
            && e.contains("which this build migrates forward at its start")
            && e.contains("loses everything written to the database since it was made")
            && e.contains("lmgw never restores a copy by itself"),
        "{e}"
    );
    assert_eq!(files(&backups), before, "no copy of a database it refuses");

    let desktop = store::open(&db).await.unwrap_err().to_string();
    assert!(
        desktop.contains("records migration 9999")
            && desktop.ends_with("Run that lmgw on it again"),
        "{desktop}"
    );
    let offline = store::open_current(&db, Some(&backups))
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(offline, e, "the offline path refuses in the same words");

    // Its own schema's copy is named as this build's.
    std::fs::rename(
        &copy,
        backups.join(format!("lmgw-{:04}-9999.sqlite", newest())),
    )
    .unwrap();
    let e = store::open_current(&db, Some(&backups))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        e.contains(&format!("lmgw-{:04}-9999.sqlite (", newest()))
            && e.contains(&format!("migration {:04}'s schema, this build's", newest())),
        "{e}"
    );

    // Two copies of one step: the newer by its time is named, not the one
    // with the higher `to` (D108).
    let later = backups.join(format!("lmgw-{:04}-9998.sqlite", newest()));
    std::fs::write(&later, "a later copy").unwrap();
    std::fs::File::options()
        .write(true)
        .open(&later)
        .unwrap()
        .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(60))
        .unwrap();
    let e = store::open_current(&db, Some(&backups))
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains(&format!("{} (", later.display())), "{e}");

    // A backups dir that cannot be read: said so, not "no copy".
    let not_a_dir = dir.path().join("not-a-dir");
    std::fs::write(&not_a_dir, "a file").unwrap();
    let e = store::open_current(&db, Some(&not_a_dir))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("no copy this build can open can be named, since the backups folder")
            && e.contains("cannot be read"),
        "{e}"
    );
    let listed = store::migration_copies(&not_a_dir);
    assert!(listed.copies.is_empty());
    assert!(
        listed.error.as_deref().is_some_and(|e| e.contains(&format!(
            "the backups folder {} cannot be read",
            not_a_dir.display()
        ))),
        "{listed:?}"
    );

    // No copy it can open: said so, and where they would be.
    let empty = tempfile::tempdir().unwrap();
    let e = store::open_current(&db, Some(empty.path()))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        e.contains(&format!(
            "no copy this build can open is in {}",
            empty.path().display()
        )),
        "{e}"
    );
}

/// A plain pool on an existing database, no migrator.
pub(crate) async fn plain(db: &Path) -> SqlitePool {
    SqlitePoolOptions::new()
        .max_connections(1)
        .connect(&format!("sqlite://{}", db.display()))
        .await
        .unwrap()
}

/// The copies' files in `dir` (their records left out).
pub(crate) fn copies(dir: &Path) -> Vec<String> {
    files(dir)
        .into_iter()
        .filter(|f| !f.ends_with(".source"))
        .collect()
}

/// `data`'s database at 0017 with an owner's `llama-aux` row (which the 0018
/// repair renames) and an alias on the managed audio upstream, which the
/// 0023 guard refuses after the copy and the repair ("'speak' → audiocpp"):
/// the database and the owner's row.
pub(crate) async fn alias_refused_db(data: &Path) -> (PathBuf, i64) {
    let db = db_at(data, 17).await;
    let pool = plain(&db).await;
    insert_upstream(&pool, "llama-embed", 18081).await;
    let owners = insert_upstream(&pool, "llama-aux", 9999).await;
    let audio = insert_upstream(&pool, "audiocpp", 18090).await;
    sqlx::query(
        "INSERT INTO models (alias, upstream_id, upstream_model_id) VALUES ('speak', ?1, 'tts')",
    )
    .bind(audio)
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;
    (db, owners)
}

/// A guard that only reads refuses ahead of the copy (D113): a start it
/// refuses makes none, however often the unit restarts it.
#[tokio::test]
async fn a_start_a_read_only_guard_refuses_makes_no_copy() {
    let dir = tempfile::tempdir().unwrap();
    let backups = store::backups_dir(dir.path());
    let db = db_at(dir.path(), 69).await;
    let pool = plain(&db).await;
    sqlx::query(
        "INSERT INTO prices (id, scope_kind, scope_key, unit, price_in, price_out, source)
         VALUES (2, 'alias', 'whisper', 'per_second', 0.0001, NULL, 'manual')",
    )
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;
    for _ in 0..2 {
        let e = store::open_for_start(&db, Some(&backups))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("id 2 (alias 'whisper', manual, per_second)"),
            "{e}"
        );
        assert_eq!(files(&backups), Vec::<String>::new(), "no copy");
    }
}

/// A start refused after its copy (here the 0023 alias guard, which on a
/// database from before 0018 waits for the repair, so it comes after the
/// copy and after the repair wrote) makes one copy, and the starts the unit
/// restarts reuse it while the database is as the last one left it; a
/// write between makes the next copy (D113).
#[tokio::test]
async fn a_refused_start_reuses_its_copy_until_the_database_changes() {
    let dir = tempfile::tempdir().unwrap();
    let backups = store::backups_dir(dir.path());
    let (db, owners) = alias_refused_db(dir.path()).await;
    let base = format!("lmgw-0017-{:04}", newest());

    let (log, _guard) = capture_log();
    for start in 1..=3 {
        let e = store::open_for_start(&db, Some(&backups))
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("'speak' → audiocpp"), "start {start}: {e}");
        assert_eq!(
            copies(&backups),
            [format!("{base}.sqlite")],
            "start {start}: one copy"
        );
    }
    assert_eq!(
        log.text().matches("still matches the database").count(),
        2,
        "{}",
        log.text()
    );
    // The copy is the database before the first start's repair.
    let copied = store::open_read_only(&backups.join(format!("{base}.sqlite")))
        .await
        .unwrap();
    assert_eq!(name_of(&copied, owners).await, "llama-aux");
    copied.close().await;

    // Something else wrote to it (the previous image, say): a new copy.
    let pool = plain(&db).await;
    insert_upstream(&pool, "written-between", 18100).await;
    pool.close().await;
    store::open_for_start(&db, Some(&backups))
        .await
        .unwrap_err();
    assert_eq!(
        copies(&backups),
        [format!("{base}-2.sqlite"), format!("{base}.sqlite")]
    );
    let copied = store::open_read_only(&backups.join(format!("{base}-2.sqlite")))
        .await
        .unwrap();
    let between: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM upstreams WHERE name = 'written-between'")
            .fetch_one(&copied)
            .await
            .unwrap();
    assert_eq!(between, 1);
    copied.close().await;
}

/// A copy is made under a hidden partial name and published whole; the
/// partial a killed start left is removed and logged, and never listed
/// (D114). An existing backups dir open to others is made 0700 (D109).
#[tokio::test]
async fn a_partial_from_a_killed_start_is_removed_and_never_listed() {
    let dir = tempfile::tempdir().unwrap();
    let backups = store::backups_dir(dir.path());
    let db = db_at(dir.path(), newest() - 1).await;
    std::fs::create_dir_all(&backups).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&backups, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let base = format!("lmgw-{:04}-{:04}.sqlite", newest() - 1, newest());
    let partial = backups.join(format!(".{base}.partial"));
    std::fs::write(&partial, "half a database").unwrap();
    assert!(store::migration_copies(&backups).copies.is_empty());

    let (log, _guard) = capture_log();
    store::open_for_start(&db, Some(&backups))
        .await
        .unwrap()
        .close()
        .await;
    assert_eq!(files(&backups), [base.clone(), format!("{base}.source")]);
    assert_eq!(mode(&backups), 0o700);
    let text = log.text();
    assert!(
        text.contains(&format!(
            "removed {}, the unfinished copy of lmgw.sqlite an earlier start left",
            partial.display()
        )) && text.contains(&format!(
            "the server unit's TimeoutStartSec is likely shorter than the copy of the database \
             takes ({} is ",
            db.display()
        )) && text.contains("was 0755, open to others; it is 0700 now"),
        "{text}"
    );
}

/// `knowledge.db` and `quickdoc.db` run their own migrations at the start,
/// so a container start copies them as well, under their own stems, and
/// the list holds them (D117).
#[tokio::test]
async fn the_knowledge_and_corpus_databases_are_copied_too() {
    let dir = tempfile::tempdir().unwrap();
    db_at(dir.path(), newest()).await;
    let mut behind = Vec::new();
    for (file, mut m, stem) in [
        (
            "knowledge.db",
            lmgw_core::knowledge::store::migrator(),
            "knowledge",
        ),
        ("quickdoc.db", quickdoc_core::store::migrator(), "quickdoc"),
    ] {
        let top = m.migrations.iter().map(|x| x.version).max().unwrap();
        m.migrations = m
            .migrations
            .iter()
            .filter(|x| x.version < top)
            .cloned()
            .collect::<Vec<_>>()
            .into();
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::from_str(&format!(
                    "sqlite://{}",
                    dir.path().join(file).display()
                ))
                .unwrap()
                .create_if_missing(true)
                .journal_mode(SqliteJournalMode::Wal),
            )
            .await
            .unwrap();
        m.run(&pool).await.unwrap();
        pool.close().await;
        let from = m.migrations.iter().map(|x| x.version).max().unwrap();
        behind.push(format!("{stem}-{from:04}-{top:04}.db"));
    }
    let (log, _guard) = capture_log();
    let state = start(dir.path(), Deployment::Container).await.unwrap();
    let backups = store::backups_dir(dir.path());
    let mut listed: Vec<(String, String)> = store::migration_copies(&backups)
        .copies
        .into_iter()
        .map(|c| (c.database, c.file))
        .collect();
    listed.sort();
    assert_eq!(
        listed,
        [
            ("knowledge.db".to_string(), behind[0].clone()),
            ("quickdoc.db".to_string(), behind[1].clone()),
        ],
        "lmgw.sqlite had nothing pending"
    );
    let text = log.text();
    for f in ["knowledge.db", "quickdoc.db"] {
        assert!(
            text.contains(&format!("before migrating {f} from "))
                && text.contains(&format!("migrated {f} from ")),
            "{f}: {text}"
        );
    }
    state.db.close().await;
}

/// The second review round (D118–D120).
mod stopped;
