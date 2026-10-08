//! What `open_import` does with a corpus file that is not this build's schema.
//!
//! A corpus database is a plain file people move between machines (§4), so the
//! two builds at either end are routinely not the same one. Older is an upgrade
//! — the migrator runs and the data comes with it. Newer is a refusal *by
//! version*, before anything reads a column that does not exist yet, because
//! "no such column: eval_best" tells the owner nothing about what to do.

use std::str::FromStr;

use quickdoc_core::store as qstore;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::SqlitePool;

/// A corpus file written by a build whose migrations stopped at `version`,
/// holding one corpus with one chunk — enough to prove the upgrade carried the
/// data and not just the schema.
async fn corpus_file_at_version(path: &std::path::Path, version: i64) {
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))
        .unwrap()
        .create_if_missing(true)
        .foreign_keys(true)
        .busy_timeout(quickdoc_core::store::BUSY_TIMEOUT);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
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

    // Raw SQL, not the crate's own writers: those bind columns later migrations
    // added, which is exactly what an older file does not have.
    sqlx::query(
        "INSERT INTO corpus (library, version, status, embed_upstream, embed_model, embed_dims,
                             ingest_model, chunk_count)
         VALUES ('axum','0.8','ready','up','embed-tgt',4,'ingest-model',1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO source (corpus_id, root, kind) VALUES (1,'https://docs.rs/axum','markdown')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO document (source_id, url, content_hash) VALUES (1,'u','h')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO chunk (id, corpus_id, document_id, span_start, span_end, payload)
         VALUES ('c1',1,1,0,10,'an extractor pulls typed data out of the request')",
    )
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;
}

async fn max_version(pool: &SqlitePool) -> i64 {
    sqlx::query_scalar("SELECT MAX(version) FROM _sqlx_migrations")
        .fetch_one(pool)
        .await
        .unwrap()
}

/// An older file is migrated on the way in, and says which version it arrived
/// at so the import report can show it.
#[tokio::test]
async fn a_file_from_an_older_build_is_migrated_on_import() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("old.sqlite");
    corpus_file_at_version(&path, 1).await;

    let (pool, file_version) = qstore::open_import(&path)
        .await
        .expect("an older corpus file is an upgrade, not an error");
    assert_eq!(file_version, 1, "the report says where it came from");
    assert_eq!(
        max_version(&pool).await,
        qstore::schema_version(),
        "and it arrives at this build's schema"
    );

    // The data came with the schema, and the columns the upgrade added read as
    // their defaults rather than as missing.
    let corpora = qstore::list_corpora(&pool).await.unwrap();
    assert_eq!(corpora.len(), 1);
    assert_eq!(corpora[0].corpus_id(), "axum@0.8");
    assert_eq!(corpora[0].chunk_count, 1);
    assert_eq!(corpora[0].eval_best, None);
    assert!(!corpora[0].eval_regression);
    assert!(qstore::list_eval_runs(&pool, corpora[0].id, 0)
        .await
        .unwrap()
        .is_empty());
}

/// A file from a *newer* build is refused by version, naming both sides and the
/// way out. Letting it through would fail later on a missing column, which says
/// nothing about what to do — and `open_import` writes, so "try it and see"
/// would leave a half-migrated file behind.
#[tokio::test]
async fn a_file_from_a_newer_build_is_refused_by_version() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("future.sqlite");
    corpus_file_at_version(&path, qstore::schema_version()).await;

    // The same file, plus a migration this build has never heard of.
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))
        .unwrap()
        .foreign_keys(true)
        .busy_timeout(quickdoc_core::store::BUSY_TIMEOUT);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .unwrap();
    let future = qstore::schema_version() + 7;
    sqlx::query(
        "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time)
         VALUES (?1, 'from a later lmgw', 1, X'00', 0)",
    )
    .bind(future)
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;

    let err = qstore::open_import(&path)
        .await
        .expect_err("a file this build cannot read must not be opened")
        .to_string();
    assert!(
        err.contains(&format!("v{future}"))
            && err.contains(&format!("v{}", qstore::schema_version())),
        "the refusal names both schema versions: {err}"
    );
    assert!(err.contains("upgrade lmgw"), "and the way out: {err}");
}
